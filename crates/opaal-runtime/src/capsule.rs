//! Private typed transport for re-executed job supervisors.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::ops::Range as ByteRange;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opaal_syntax::{SourceFile, SourceId};

use crate::builtin::SessionState;
use crate::eval::{
    CallFrame, CallableSnapshot, ErrorCategory, ErrorLabel, FrameCallee, RuntimeErrorSnapshot,
    callable_binding_types, restore_callable, restore_runtime_error, snapshot_callable,
    snapshot_runtime_error,
};
use crate::module::{
    AnalysisLimitKind, AnalysisLimits, ModuleCanonicalizer, ModuleId, ModuleOrigin,
    ModulePathError, ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader, NominalTypeId,
    ReplTypeSource, RuntimeBindingTypes, RuntimeTypeContext, ValueType,
};
use crate::plan::SessionOptions;
use crate::scope::CapsuleBinding;
use crate::{
    BindingMutability, ByteSize, Duration, Environment, FiniteFloat, NativePath,
    NominalRecordValue, Range, Record, ScopeStack, Signal, Status, Table, Value, VariantValue,
};

const MAGIC: &[u8; 8] = b"OPAALCAP";
const COMPLETION_MAGIC: &[u8; 8] = b"OPAALEND";
const VERSION: u16 = 4;
const MAX_CAPSULE_DEPTH: usize = 64;
const MAX_CAPSULE_ITEMS: usize = 1_000_000;
pub const MAX_CAPSULE_BYTES: usize = 16 * 1024 * 1024;
pub const CAPSULE_DESCRIPTOR: u32 = 3;
pub const COMPLETION_DESCRIPTOR: u32 = 4;

/// One decoded private supervisor invocation.
#[doc(hidden)]
#[cfg(test)]
pub struct BackgroundCapsule {
    name: String,
    text: String,
    cwd: PathBuf,
    environment: Environment,
    current_status: Option<Status>,
    scope: ScopeStack,
    options: SessionOptions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorOutcome {
    Continued,
    Exit(u8),
}

#[doc(hidden)]
pub struct SupervisorCompletion {
    outcome: SupervisorOutcome,
    status: Option<Status>,
    base_scope: ScopeStack,
    updated_scope: ScopeStack,
    base_state: SessionState,
    updated_state: SessionState,
}

impl SupervisorCompletion {
    pub const fn outcome(&self) -> SupervisorOutcome {
        self.outcome
    }

    pub const fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    pub(crate) fn apply(
        self,
        scope: &mut ScopeStack,
        state: &mut SessionState,
    ) -> Result<(), CapsuleError> {
        scope
            .apply_capsule_delta(&self.base_scope, &self.updated_scope)
            .map_err(|error| {
                CapsuleError::new(format!("invalid supervisor scope delta: {error}"))
            })?;
        state.apply_delta(&self.base_state, &self.updated_state);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn supervisor_completion(
    outcome: SupervisorOutcome,
    status: Option<Status>,
    base_scope: ScopeStack,
    updated_scope: ScopeStack,
    base_state: SessionState,
    updated_state: SessionState,
) -> SupervisorCompletion {
    SupervisorCompletion {
        outcome,
        status,
        base_scope,
        updated_scope,
        base_state,
        updated_state,
    }
}

#[cfg(test)]
impl BackgroundCapsule {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub const fn environment(&self) -> &Environment {
        &self.environment
    }

    pub const fn current_status(&self) -> Option<&Status> {
        self.current_status.as_ref()
    }

    pub const fn scope(&self) -> &ScopeStack {
        &self.scope
    }

    pub const fn options(&self) -> SessionOptions {
        self.options
    }
}

/// A malformed, unsupported, or over-budget supervisor capsule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapsuleError {
    message: String,
}

impl CapsuleError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CapsuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CapsuleError {}

/// Encode one finite launch snapshot into the private versioned wire format.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn encode_background_capsule(
    name: &str,
    text: &str,
    cwd: &Path,
    environment: &Environment,
    current_status: Option<&Status>,
    scope: &ScopeStack,
    options: SessionOptions,
) -> Result<Vec<u8>, CapsuleError> {
    let mut callables = CallableTable::default();
    callables.collect_scope(scope)?;
    validate_nominal_scopes(&[scope], &callables.types, &callables.snapshots)?;

    let mut payload = Encoder::default();
    payload.string(name)?;
    payload.string(text)?;
    payload.native(cwd.as_os_str())?;
    encode_environment(&mut payload, environment)?;
    payload.bool(current_status.is_some());
    if let Some(status) = current_status {
        encode_status(&mut payload, status)?;
    }
    payload.bool(options.pipefail());
    payload.u64(
        u64::try_from(options.capture_limit())
            .map_err(|_| CapsuleError::new("capture limit exceeds the capsule integer range"))?,
    );
    payload.usize(callables.snapshots.len())?;
    for snapshot in &callables.snapshots {
        encode_callable(&mut payload, snapshot, &callables.ids)?;
    }
    encode_scope(&mut payload, scope, &callables.ids)?;

    if payload.bytes.len() > MAX_CAPSULE_BYTES {
        return Err(CapsuleError::new(format!(
            "execution capsule exceeds the {MAX_CAPSULE_BYTES}-byte limit"
        )));
    }
    let mut wire = Vec::with_capacity(18 + payload.bytes.len());
    wire.extend_from_slice(MAGIC);
    wire.extend_from_slice(&VERSION.to_le_bytes());
    wire.extend_from_slice(&(payload.bytes.len() as u64).to_le_bytes());
    wire.extend_from_slice(&payload.bytes);
    Ok(wire)
}

/// Decode one complete private supervisor capsule.
#[doc(hidden)]
#[cfg(test)]
pub fn decode_background_capsule(bytes: &[u8]) -> Result<BackgroundCapsule, CapsuleError> {
    if bytes.len() > MAX_CAPSULE_BYTES + 18 {
        return Err(CapsuleError::new(
            "execution capsule exceeds its byte limit",
        ));
    }
    let mut wire = Decoder::new(bytes);
    if wire.take(MAGIC.len())? != MAGIC {
        return Err(CapsuleError::new(
            "execution capsule has an invalid magic header",
        ));
    }
    let version = wire.u16()?;
    if version != VERSION {
        return Err(CapsuleError::new(format!(
            "execution capsule version {version} is unsupported"
        )));
    }
    let payload_len = wire.u64()?;
    let payload_len = usize::try_from(payload_len)
        .map_err(|_| CapsuleError::new("execution capsule length exceeds this platform"))?;
    if payload_len > MAX_CAPSULE_BYTES || payload_len != wire.remaining() {
        return Err(CapsuleError::new("execution capsule length is invalid"));
    }

    let name = wire.string()?;
    let text = wire.string()?;
    let cwd = PathBuf::from(wire.native()?);
    let environment = decode_environment(&mut wire)?;
    let current_status = wire
        .bool()?
        .then(|| decode_status(&mut wire, 0))
        .transpose()?;
    let options = SessionOptions::default()
        .with_pipefail(wire.bool()?)
        .with_capture_limit(
            usize::try_from(wire.u64()?)
                .map_err(|_| CapsuleError::new("capture limit exceeds this platform"))?,
        );
    let callable_count = wire.collection_len()?;
    let mut callables = Vec::with_capacity(callable_count);
    for _ in 0..callable_count {
        let snapshot = decode_callable(&mut wire, &callables)?;
        let callable = restore_callable(snapshot, &mut wire.analysis_work_remaining)
            .map_err(CapsuleError::new)?;
        callables.push(callable);
    }
    let scope = decode_scope(&mut wire, &callables)?;
    validate_decoded_nominals(&[&scope], &callables)?;
    if wire.remaining() != 0 {
        return Err(CapsuleError::new("execution capsule has trailing bytes"));
    }
    Ok(BackgroundCapsule {
        name,
        text,
        cwd,
        environment,
        current_status,
        scope,
        options,
    })
}

#[doc(hidden)]
#[cfg(test)]
pub fn encode_supervisor_completion(
    completion: &SupervisorCompletion,
) -> Result<Vec<u8>, CapsuleError> {
    let mut callables = CallableTable::default();
    callables.collect_scope(&completion.base_scope)?;
    callables.collect_scope(&completion.updated_scope)?;
    validate_nominal_scopes(
        &[&completion.base_scope, &completion.updated_scope],
        &callables.types,
        &callables.snapshots,
    )?;
    let mut payload = Encoder::default();
    match completion.outcome {
        SupervisorOutcome::Continued => payload.u8(0),
        SupervisorOutcome::Exit(code) => {
            payload.u8(1);
            payload.u8(code);
        }
    }
    payload.bool(completion.status.is_some());
    if let Some(status) = &completion.status {
        encode_status(&mut payload, status)?;
    }
    payload.usize(callables.snapshots.len())?;
    for snapshot in &callables.snapshots {
        encode_callable(&mut payload, snapshot, &callables.ids)?;
    }
    encode_scope(&mut payload, &completion.base_scope, &callables.ids)?;
    encode_scope(&mut payload, &completion.updated_scope, &callables.ids)?;
    encode_session_state(&mut payload, &completion.base_state)?;
    encode_session_state(&mut payload, &completion.updated_state)?;
    finish_wire(COMPLETION_MAGIC, payload)
}

#[doc(hidden)]
pub fn decode_supervisor_completion(bytes: &[u8]) -> Result<SupervisorCompletion, CapsuleError> {
    let mut wire = begin_wire(bytes, COMPLETION_MAGIC)?;
    let outcome = match wire.u8()? {
        0 => SupervisorOutcome::Continued,
        1 => SupervisorOutcome::Exit(wire.u8()?),
        _ => {
            return Err(CapsuleError::new(
                "supervisor completion outcome is invalid",
            ));
        }
    };
    let status = wire
        .bool()?
        .then(|| decode_status(&mut wire, 0))
        .transpose()?;
    let callable_count = wire.collection_len()?;
    let mut callables = Vec::with_capacity(callable_count);
    for _ in 0..callable_count {
        let snapshot = decode_callable(&mut wire, &callables)?;
        callables.push(
            restore_callable(snapshot, &mut wire.analysis_work_remaining)
                .map_err(CapsuleError::new)?,
        );
    }
    let base_scope = decode_scope(&mut wire, &callables)?;
    let updated_scope = decode_scope(&mut wire, &callables)?;
    validate_decoded_nominals(&[&base_scope, &updated_scope], &callables)?;
    let base_state = decode_session_state(&mut wire)?;
    let updated_state = decode_session_state(&mut wire)?;
    if wire.remaining() != 0 {
        return Err(CapsuleError::new(
            "supervisor completion has trailing bytes",
        ));
    }
    Ok(SupervisorCompletion {
        outcome,
        status,
        base_scope,
        updated_scope,
        base_state,
        updated_state,
    })
}

#[cfg(test)]
fn encode_session_state(encoder: &mut Encoder, state: &SessionState) -> Result<(), CapsuleError> {
    encoder.native(state.cwd().as_os_str())?;
    encode_environment(encoder, state.environment())?;
    encoder.bool(state.current_status().is_some());
    if let Some(status) = state.current_status() {
        encode_status(encoder, status)?;
    }
    Ok(())
}

fn decode_session_state(decoder: &mut Decoder<'_>) -> Result<SessionState, CapsuleError> {
    let cwd = PathBuf::from(decoder.native()?);
    let environment = decode_environment(decoder)?;
    let current_status = decoder
        .bool()?
        .then(|| decode_status(decoder, 0))
        .transpose()?;
    let mut state = SessionState::new(cwd, environment);
    state.set_current_status(current_status);
    Ok(state)
}

#[cfg(test)]
fn finish_wire(magic: &[u8; 8], payload: Encoder) -> Result<Vec<u8>, CapsuleError> {
    if payload.bytes.len() > MAX_CAPSULE_BYTES {
        return Err(CapsuleError::new(format!(
            "execution capsule exceeds the {MAX_CAPSULE_BYTES}-byte limit"
        )));
    }
    let mut wire = Vec::with_capacity(18 + payload.bytes.len());
    wire.extend_from_slice(magic);
    wire.extend_from_slice(&VERSION.to_le_bytes());
    wire.extend_from_slice(&(payload.bytes.len() as u64).to_le_bytes());
    wire.extend_from_slice(&payload.bytes);
    Ok(wire)
}

fn begin_wire<'a>(bytes: &'a [u8], magic: &[u8; 8]) -> Result<Decoder<'a>, CapsuleError> {
    if bytes.len() > MAX_CAPSULE_BYTES + 18 {
        return Err(CapsuleError::new(
            "execution capsule exceeds its byte limit",
        ));
    }
    let mut wire = Decoder::new(bytes);
    if wire.take(magic.len())? != magic {
        return Err(CapsuleError::new(
            "execution capsule has an invalid magic header",
        ));
    }
    let version = wire.u16()?;
    if version != VERSION {
        return Err(CapsuleError::new(format!(
            "execution capsule version {version} is unsupported"
        )));
    }
    let payload_len = usize::try_from(wire.u64()?)
        .map_err(|_| CapsuleError::new("execution capsule length exceeds this platform"))?;
    if payload_len > MAX_CAPSULE_BYTES || payload_len != wire.remaining() {
        return Err(CapsuleError::new("execution capsule length is invalid"));
    }
    Ok(wire)
}

#[derive(Default)]
struct CallableTable {
    ids: BTreeMap<usize, u32>,
    visiting: BTreeSet<usize>,
    snapshots: Vec<CallableSnapshot>,
    types: Vec<Arc<RuntimeBindingTypes>>,
    items: usize,
}

impl CallableTable {
    fn collect_scope(&mut self, scope: &ScopeStack) -> Result<(), CapsuleError> {
        for binding in scope.capsule_bindings() {
            self.collect_value(&binding.value, 0)?;
        }
        Ok(())
    }

    fn collect_value(&mut self, value: &Value, depth: usize) -> Result<(), CapsuleError> {
        if depth > MAX_CAPSULE_DEPTH {
            return Err(CapsuleError::new("execution capsule nesting is too deep"));
        }
        self.items = self
            .items
            .checked_add(1)
            .filter(|items| *items <= MAX_CAPSULE_ITEMS)
            .ok_or_else(|| CapsuleError::new("execution capsule has too many values"))?;
        match value {
            Value::List(values) => {
                for value in values.iter() {
                    self.collect_value(value, depth + 1)?;
                }
            }
            Value::Record(record) => {
                for (_, value) in record.entries() {
                    self.collect_value(value, depth + 1)?;
                }
            }
            Value::NominalRecord(record) => {
                for (_, value) in record.fields() {
                    self.collect_value(value, depth + 1)?;
                }
            }
            Value::Variant(variant) => {
                for value in variant.payload() {
                    self.collect_value(value, depth + 1)?;
                }
            }
            Value::Table(table) => {
                for row in table.rows() {
                    for value in row.iter() {
                        self.collect_value(value, depth + 1)?;
                    }
                }
            }
            Value::Callable(callable) => self.collect_callable(callable)?,
            Value::Error(_) => {}
            _ => {}
        }
        Ok(())
    }

    fn collect_callable(
        &mut self,
        callable: &Arc<dyn crate::Callable>,
    ) -> Result<(), CapsuleError> {
        let key = Arc::as_ptr(callable) as *const () as usize;
        if self.ids.contains_key(&key) {
            return Ok(());
        }
        if !self.visiting.insert(key) {
            return Err(CapsuleError::new("callable capture graph contains a cycle"));
        }
        let snapshot = snapshot_callable(callable).ok_or_else(|| {
            CapsuleError::new("an opaque host callable cannot cross the execution capsule")
        })?;
        self.collect_scope(&snapshot.captured)?;
        self.visiting.remove(&key);
        let id = u32::try_from(self.snapshots.len())
            .map_err(|_| CapsuleError::new("execution capsule has too many callables"))?;
        self.ids.insert(key, id);
        self.types.push(
            callable_binding_types(callable)
                .ok_or_else(|| CapsuleError::new("opaque callable type context"))?,
        );
        self.snapshots.push(snapshot);
        Ok(())
    }
}

fn validate_decoded_nominals(
    scopes: &[&ScopeStack],
    callables: &[Arc<dyn crate::Callable>],
) -> Result<(), CapsuleError> {
    let types = callables
        .iter()
        .filter_map(callable_binding_types)
        .collect::<Vec<_>>();
    let snapshots = callables
        .iter()
        .filter_map(snapshot_callable)
        .collect::<Vec<_>>();
    validate_nominal_scopes(scopes, &types, &snapshots)
}

fn validate_nominal_scopes(
    scopes: &[&ScopeStack],
    types: &[Arc<RuntimeBindingTypes>],
    snapshots: &[CallableSnapshot],
) -> Result<(), CapsuleError> {
    fn concrete_type(
        value_type: &ValueType,
        types: &[Arc<RuntimeBindingTypes>],
        remaining: &mut usize,
        depth: usize,
    ) -> Result<(), CapsuleError> {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| CapsuleError::new("nominal validation exceeds its type limit"))?;
        if depth > MAX_CAPSULE_DEPTH {
            return Err(CapsuleError::new("nominal type nesting is too deep"));
        }
        match value_type {
            ValueType::TypeParameter(_) => {
                return Err(CapsuleError::new(
                    "nominal value has an unresolved type argument",
                ));
            }
            ValueType::List(element) => concrete_type(element, types, remaining, depth + 1)?,
            ValueType::Nominal { id, arguments } => {
                *remaining = remaining.checked_sub(types.len()).ok_or_else(|| {
                    CapsuleError::new("nominal validation exceeds its work limit")
                })?;
                if crate::module::is_opaque_operational_nominal(id)
                    || !types.iter().any(|types| {
                        types.nominal_by_id(id).is_some_and(|nominal| {
                            nominal.type_parameters().len() == arguments.len()
                        })
                    })
                {
                    return Err(CapsuleError::new(
                        "nominal type argument has no retained schema",
                    ));
                }
                for argument in arguments {
                    concrete_type(argument, types, remaining, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn value(
        value: &Value,
        types: &[Arc<RuntimeBindingTypes>],
        remaining: &mut usize,
        depth: usize,
    ) -> Result<(), CapsuleError> {
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(|| CapsuleError::new("nominal validation exceeds its value limit"))?;
        if depth > MAX_CAPSULE_DEPTH {
            return Err(CapsuleError::new("nominal validation nesting is too deep"));
        }
        let nominal = match value {
            Value::NominalRecord(record) => Some((record.id(), record.type_arguments())),
            Value::Variant(variant) => Some((variant.id(), variant.type_arguments())),
            _ => None,
        };
        if let Some((id, arguments)) = nominal {
            if crate::module::is_opaque_operational_nominal(id) {
                return Err(CapsuleError::new(
                    "control carriers cannot cross the execution capsule",
                ));
            }
            for argument in arguments {
                concrete_type(argument, types, remaining, 0)?;
            }
            *remaining = remaining
                .checked_sub(types.len())
                .ok_or_else(|| CapsuleError::new("nominal validation exceeds its work limit"))?;
            let schemas = types
                .iter()
                .filter_map(|types| types.nominal_by_id(id))
                .collect::<Vec<_>>();
            if schemas.is_empty() {
                return Err(CapsuleError::new("nominal value has no retained schema"));
            }
            for schema in schemas {
                *remaining = remaining
                    .checked_sub(
                        schema.fields().len()
                            + schema.variants().len()
                            + schema.type_parameters().len(),
                    )
                    .ok_or_else(|| {
                        CapsuleError::new("nominal validation exceeds its work limit")
                    })?;
                if schema.type_parameters().len() != arguments.len() {
                    return Err(CapsuleError::new("invalid nominal type argument count"));
                }
                let substitutions = schema
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect::<BTreeMap<_, _>>();
                for (parameter, argument) in schema.type_parameters().iter().zip(arguments) {
                    for constraint in parameter.constraints() {
                        if !types
                            .iter()
                            .any(|types| types.type_satisfies_constraint(argument, *constraint))
                        {
                            return Err(CapsuleError::new(
                                "invalid nominal type argument constraint",
                            ));
                        }
                    }
                }
                let valid = match value {
                    Value::NominalRecord(record) => {
                        let fields = record
                            .fields()
                            .iter()
                            .map(|(name, value)| (name.as_ref(), value))
                            .collect::<BTreeMap<_, _>>();
                        schema.kind() == crate::module::NominalTypeKind::Record
                            && record.fields().len() == schema.fields().len()
                            && fields.len() == record.fields().len()
                            && schema.fields().iter().all(|field| {
                                fields.get(field.name()).is_some_and(|value| {
                                    crate::module::substitute_type(
                                        field.value_type(),
                                        &substitutions,
                                    )
                                    .accepts(value)
                                })
                            })
                    }
                    Value::Variant(variant) => {
                        schema.kind() == crate::module::NominalTypeKind::Variant
                            && schema
                                .variants()
                                .iter()
                                .find(|constructor| constructor.name() == variant.constructor())
                                .is_some_and(|constructor| {
                                    constructor.payload().len() == variant.payload().len()
                                        && constructor.payload().iter().zip(variant.payload()).all(
                                            |(expected, value)| {
                                                crate::module::substitute_type(
                                                    expected,
                                                    &substitutions,
                                                )
                                                .accepts(value)
                                            },
                                        )
                                })
                    }
                    _ => unreachable!(),
                };
                if !valid {
                    return Err(CapsuleError::new(
                        "nominal value does not match its retained schema",
                    ));
                }
            }
        }
        match value {
            Value::List(values) => {
                for child in values.iter() {
                    value_child(child, types, remaining, depth)?;
                }
            }
            Value::Record(record) => {
                for (_, child) in record.entries() {
                    value_child(child, types, remaining, depth)?;
                }
            }
            Value::Table(table) => {
                for row in table.rows() {
                    for child in row.iter() {
                        value_child(child, types, remaining, depth)?;
                    }
                }
            }
            Value::NominalRecord(record) => {
                for (_, child) in record.fields() {
                    value_child(child, types, remaining, depth)?;
                }
            }
            Value::Variant(variant) => {
                for child in variant.payload() {
                    value_child(child, types, remaining, depth)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn value_child(
        child: &Value,
        types: &[Arc<RuntimeBindingTypes>],
        remaining: &mut usize,
        depth: usize,
    ) -> Result<(), CapsuleError> {
        value(child, types, remaining, depth + 1)
    }
    let mut remaining = MAX_CAPSULE_ITEMS;
    for scope in scopes
        .iter()
        .copied()
        .chain(snapshots.iter().map(|snapshot| &snapshot.captured))
    {
        for child in scope.values() {
            value(child, types, &mut remaining, 0)?;
        }
    }
    Ok(())
}

fn encode_callable(
    encoder: &mut Encoder,
    snapshot: &CallableSnapshot,
    callable_ids: &BTreeMap<usize, u32>,
) -> Result<(), CapsuleError> {
    encoder.optional_string(snapshot.name.as_deref())?;
    encoder.usize(snapshot.parameters.len())?;
    for (name, value_type) in &snapshot.parameters {
        encoder.string(name)?;
        encode_value_type(encoder, value_type)?;
    }
    encode_scope(encoder, &snapshot.captured, callable_ids)?;
    encoder.u32(snapshot.source.id().get());
    encoder.string(snapshot.source.name())?;
    encoder.string(snapshot.source.text())?;
    match &snapshot.result_type {
        Some(value_type) => {
            encoder.bool(true);
            encode_value_type(encoder, value_type)?;
        }
        None => encoder.bool(false),
    }
    encoder.string(&snapshot.location)?;
    encoder.u32(snapshot.origin_span.source_id().get());
    encoder.usize(snapshot.origin_span.start())?;
    encoder.usize(snapshot.origin_span.end())?;
    encoder.usize(snapshot.captured_type_arguments.len())?;
    for (name, value_type) in &snapshot.captured_type_arguments {
        encoder.string(name)?;
        encode_value_type(encoder, value_type)?;
    }
    encode_type_context(encoder, &snapshot.type_context)?;
    Ok(())
}

fn decode_callable(
    decoder: &mut Decoder<'_>,
    callables: &[Arc<dyn crate::Callable>],
) -> Result<CallableSnapshot, CapsuleError> {
    let name = decoder.optional_string()?;
    let parameter_count = decoder.collection_len()?;
    let mut parameters = Vec::with_capacity(parameter_count);
    for _ in 0..parameter_count {
        parameters.push((decoder.string()?, decode_value_type(decoder, 0)?));
    }
    let captured = decode_scope(decoder, callables)?;
    let source_id = SourceId::new(decoder.u32()?);
    let source_name = decoder.string()?;
    let source_text = decoder.string()?;
    let source = SourceFile::new(source_id, source_name, source_text);
    let result_type = decoder
        .bool()?
        .then(|| decode_value_type(decoder, 0))
        .transpose()?;
    let location = decoder.string()?;
    let origin_source = SourceId::new(decoder.u32()?);
    if origin_source != source.id() {
        return Err(CapsuleError::new(
            "callable origin source does not match its source",
        ));
    }
    let start = decoder.usize()?;
    let end = decoder.usize()?;
    let origin_span = source
        .span(ByteRange { start, end })
        .map_err(|error| CapsuleError::new(format!("invalid callable origin span: {error}")))?;
    let type_argument_count = decoder.collection_len()?;
    let mut captured_type_arguments = BTreeMap::new();
    for _ in 0..type_argument_count {
        let name = decoder.string()?;
        let value_type = decode_value_type(decoder, 0)?;
        if captured_type_arguments.insert(name, value_type).is_some() {
            return Err(CapsuleError::new("duplicate captured type parameter"));
        }
    }
    Ok(CallableSnapshot {
        name,
        parameters,
        captured,
        source,
        result_type,
        location,
        origin_span,
        captured_type_arguments,
        type_context: decode_type_context(decoder)?,
    })
}

fn encode_module_id(encoder: &mut Encoder, module: &ModuleId) -> Result<(), CapsuleError> {
    match module.origin() {
        ModuleOrigin::Local => {
            encoder.u8(0);
            encoder.native(module.path().as_os_str())?;
        }
        ModuleOrigin::Standard { namespace, module } => {
            encoder.u8(1);
            encoder.string(namespace)?;
            encoder.string(module)?;
        }
    }
    Ok(())
}

fn decode_module_id(decoder: &mut Decoder<'_>) -> Result<ModuleId, CapsuleError> {
    match decoder.u8()? {
        0 => Ok(ModuleId::local(PathBuf::from(decoder.native()?))),
        1 => Ok(ModuleId::standard(&decoder.string()?, &decoder.string()?)),
        _ => Err(CapsuleError::new("invalid module origin")),
    }
}

fn encode_type_context(
    encoder: &mut Encoder,
    context: &RuntimeTypeContext,
) -> Result<(), CapsuleError> {
    let mut sources = BTreeMap::new();
    let mut resolutions = BTreeMap::new();
    for program in &context.programs {
        for entry in program.sources().entries() {
            if let Some(previous) = sources.insert(entry.module().clone(), entry.source())
                && previous != entry.source()
            {
                return Err(CapsuleError::new("conflicting retained module sources"));
            }
        }
        for import in program.graph().imports() {
            if import.target().origin() == &ModuleOrigin::Local {
                let candidate = import
                    .importer()
                    .path()
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(import.requested())
                    .components()
                    .collect::<PathBuf>();
                resolutions.insert(candidate, import.target().path().to_path_buf());
            }
        }
    }
    encoder.usize(sources.len())?;
    for (module, source) in sources {
        encode_module_id(encoder, &module)?;
        encode_source(encoder, source)?;
    }
    encoder.usize(resolutions.len())?;
    for (requested, target) in resolutions {
        encoder.native(requested.as_os_str())?;
        encoder.native(target.as_os_str())?;
    }
    encoder.usize(context.programs.len())?;
    for program in &context.programs {
        encode_module_id(encoder, program.graph().root())?;
    }
    encoder.usize(context.cells.len())?;
    for cell in &context.cells {
        encode_source(encoder, &cell.source)?;
        encoder.usize(cell.statement_count)?;
        encoder.usize(cell.nominal_offsets.len())?;
        for offset in &cell.nominal_offsets {
            encoder.usize(*offset)?;
        }
        encoder.usize(cell.local_imports.len())?;
        for (offset, target) in &cell.local_imports {
            encoder.usize(*offset)?;
            encode_module_id(encoder, target)?;
        }
    }
    Ok(())
}

struct RetainedSources {
    sources: BTreeMap<ModuleId, SourceFile>,
    resolutions: BTreeMap<PathBuf, PathBuf>,
}

impl ModuleCanonicalizer for RetainedSources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let candidate = candidate.components().collect::<PathBuf>();
        self.resolutions
            .get(&candidate)
            .cloned()
            .or_else(|| {
                self.sources
                    .keys()
                    .find(|module| module.path() == candidate)
                    .map(|module| module.path().to_path_buf())
            })
            .ok_or_else(|| ModulePathError::new("path is absent from retained source context"))
    }
}

impl ModuleSourceLoader for RetainedSources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.sources
            .get(module)
            .map(|source| source.text().as_bytes().to_vec())
            .ok_or_else(|| ModuleSourceError::new("source is absent from retained source context"))
    }
}

fn decode_type_context(decoder: &mut Decoder<'_>) -> Result<RuntimeTypeContext, CapsuleError> {
    let count = decoder.collection_len()?;
    if count > 256 {
        return Err(CapsuleError::new("too many retained sources"));
    }
    let mut sources = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for _ in 0..count {
        let module = decode_module_id(decoder)?;
        let source = decode_source(decoder)?;
        if !ids.insert(source.id()) || sources.insert(module, source).is_some() {
            return Err(CapsuleError::new("duplicate retained source identity"));
        }
    }
    let count = decoder.collection_len()?;
    let mut resolutions = BTreeMap::new();
    for _ in 0..count {
        let requested = PathBuf::from(decoder.native()?);
        let target = PathBuf::from(decoder.native()?);
        if resolutions.insert(requested, target).is_some() {
            return Err(CapsuleError::new("duplicate retained path resolution"));
        }
    }
    let retained = RetainedSources {
        sources,
        resolutions,
    };
    let count = decoder.collection_len()?;
    if count > 256 {
        return Err(CapsuleError::new("too many retained programs"));
    }
    let mut programs = Vec::new();
    for _ in 0..count {
        let root = decode_module_id(decoder)?;
        if root.origin() != &ModuleOrigin::Local {
            return Err(CapsuleError::new("retained program root must be local"));
        }
        let ids = retained
            .sources
            .iter()
            .map(|(module, source)| (module.clone(), source.id()))
            .collect();
        let limits = AnalysisLimits::OPAAL.with_limit(
            AnalysisLimitKind::WorkUnits,
            decoder.analysis_work_remaining,
        );
        let report = ModuleProgramLoader::new(&retained, &retained)
            .with_source_ids(ids, 0)
            .analyze_with_limits(root.path(), limits);
        decoder.analysis_work_remaining = decoder
            .analysis_work_remaining
            .saturating_sub(report.usage().get(AnalysisLimitKind::WorkUnits));
        let program = report
            .program()
            .cloned()
            .ok_or_else(|| CapsuleError::new("retained module context cannot be analyzed"))?;
        if program
            .sources()
            .entries()
            .any(|entry| retained.sources.get(entry.module()) != Some(entry.source()))
        {
            return Err(CapsuleError::new(
                "retained module source context is inconsistent",
            ));
        }
        programs.push(Arc::new(program));
    }
    let count = decoder.collection_len()?;
    if count + retained.sources.len() > 256 {
        return Err(CapsuleError::new("too many retained sources"));
    }
    let mut cells = Vec::new();
    for _ in 0..count {
        let source = decode_source(decoder)?;
        if !ids.insert(source.id()) {
            return Err(CapsuleError::new("duplicate retained source identity"));
        }
        let statement_count = decoder.usize()?;
        let nominal_count = decoder.collection_len()?;
        let mut nominal_offsets = BTreeSet::new();
        for _ in 0..nominal_count {
            let offset = decoder.usize()?;
            if offset >= source.len() || !nominal_offsets.insert(offset) {
                return Err(CapsuleError::new("invalid retained nominal declaration"));
            }
        }
        let import_count = decoder.collection_len()?;
        let mut local_imports = BTreeMap::new();
        for _ in 0..import_count {
            let offset = decoder.usize()?;
            let target = decode_module_id(decoder)?;
            if !retained.sources.contains_key(&target)
                || local_imports.insert(offset, target).is_some()
            {
                return Err(CapsuleError::new("invalid retained interactive import"));
            }
        }
        cells.push(ReplTypeSource {
            source: Arc::new(source),
            local_imports,
            statement_count,
            nominal_offsets,
        });
    }
    Ok(RuntimeTypeContext { programs, cells })
}

fn encode_scope(
    encoder: &mut Encoder,
    scope: &ScopeStack,
    callable_ids: &BTreeMap<usize, u32>,
) -> Result<(), CapsuleError> {
    let bindings = scope.capsule_bindings();
    encoder.usize(bindings.len())?;
    for binding in bindings {
        encoder.string(&binding.name)?;
        encoder.u8(match binding.mutability {
            BindingMutability::Immutable => 0,
            BindingMutability::Mutable => 1,
        });
        match &binding.value_type {
            Some(value_type) => {
                encoder.bool(true);
                encode_value_type(encoder, value_type)?;
            }
            None => encoder.bool(false),
        }
        encode_value(encoder, &binding.value, callable_ids)?;
    }
    Ok(())
}

fn decode_scope(
    decoder: &mut Decoder<'_>,
    callables: &[Arc<dyn crate::Callable>],
) -> Result<ScopeStack, CapsuleError> {
    let count = decoder.collection_len()?;
    let mut bindings = Vec::with_capacity(count);
    for _ in 0..count {
        let name = decoder.string()?;
        let mutability = match decoder.u8()? {
            0 => BindingMutability::Immutable,
            1 => BindingMutability::Mutable,
            _ => return Err(CapsuleError::new("capsule binding mutability is invalid")),
        };
        let value_type = decoder
            .bool()?
            .then(|| decode_value_type(decoder, 0))
            .transpose()?;
        let value = decode_value(decoder, callables, 0)?;
        bindings.push(CapsuleBinding {
            name,
            mutability,
            value_type,
            value,
        });
    }
    ScopeStack::from_capsule_bindings(bindings)
        .map_err(|error| CapsuleError::new(format!("invalid capsule scope: {error}")))
}

fn encode_environment(
    encoder: &mut Encoder,
    environment: &Environment,
) -> Result<(), CapsuleError> {
    encoder.usize(environment.len())?;
    for (name, value) in environment.iter() {
        encoder.native(name)?;
        encoder.native(value)?;
    }
    Ok(())
}

fn decode_environment(decoder: &mut Decoder<'_>) -> Result<Environment, CapsuleError> {
    let count = decoder.collection_len()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((decoder.native()?, decoder.native()?));
    }
    Ok(Environment::from_snapshot(entries))
}

fn encode_value(
    encoder: &mut Encoder,
    value: &Value,
    callable_ids: &BTreeMap<usize, u32>,
) -> Result<(), CapsuleError> {
    encode_value_at(encoder, value, callable_ids, 0)
}

fn encode_value_at(
    encoder: &mut Encoder,
    value: &Value,
    callable_ids: &BTreeMap<usize, u32>,
    depth: usize,
) -> Result<(), CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new("execution capsule nesting is too deep"));
    }
    match value {
        Value::Null => encoder.u8(0),
        Value::Bool(value) => {
            encoder.u8(1);
            encoder.bool(*value);
        }
        Value::Int(value) => {
            encoder.u8(2);
            encoder.i64(*value);
        }
        Value::Float(value) => {
            encoder.u8(3);
            encoder.u64(value.get().to_bits());
        }
        Value::String(value) => {
            encoder.u8(4);
            encoder.string(value)?;
        }
        Value::Bytes(value) => {
            encoder.u8(5);
            encoder.byte_string(value)?;
        }
        Value::Path(value) => {
            encoder.u8(6);
            encoder.native(value.as_os_str())?;
        }
        Value::Duration(value) => {
            encoder.u8(7);
            encoder.i128(value.as_nanos());
        }
        Value::ByteSize(value) => {
            encoder.u8(8);
            encoder.u64(value.bytes());
        }
        Value::List(values) => {
            encoder.u8(9);
            encoder.usize(values.len())?;
            for value in values.iter() {
                encode_value_at(encoder, value, callable_ids, depth + 1)?;
            }
        }
        Value::Record(record) => {
            encoder.u8(10);
            encoder.usize(record.entries().len())?;
            for (key, value) in record.entries() {
                encoder.string(key)?;
                encode_value_at(encoder, value, callable_ids, depth + 1)?;
            }
        }
        Value::NominalRecord(record) => {
            encoder.u8(16);
            encode_nominal_identity(encoder, record.id(), record.type_arguments())?;
            encoder.usize(record.fields().len())?;
            for (name, value) in record.fields() {
                encoder.string(name)?;
                encode_value_at(encoder, value, callable_ids, depth + 1)?;
            }
        }
        Value::Variant(variant) => {
            encoder.u8(17);
            encode_nominal_identity(encoder, variant.id(), variant.type_arguments())?;
            encoder.string(variant.constructor())?;
            encoder.usize(variant.payload().len())?;
            for value in variant.payload() {
                encode_value_at(encoder, value, callable_ids, depth + 1)?;
            }
        }
        Value::Table(table) => {
            encoder.u8(11);
            encoder.usize(table.columns().len())?;
            for column in table.columns() {
                encoder.string(column)?;
            }
            encoder.usize(table.rows().len())?;
            for row in table.rows() {
                for value in row.iter() {
                    encode_value_at(encoder, value, callable_ids, depth + 1)?;
                }
            }
        }
        Value::Range(value) => {
            encoder.u8(12);
            encoder.i64(value.start());
            encoder.i64(value.end());
            encoder.bool(value.includes_end());
        }
        Value::Status(value) => {
            encoder.u8(13);
            encode_status(encoder, value)?;
        }
        Value::Callable(callable) => {
            encoder.u8(14);
            let key = Arc::as_ptr(callable) as *const () as usize;
            encoder.u32(*callable_ids.get(&key).ok_or_else(|| {
                CapsuleError::new("callable was not registered in the capsule graph")
            })?);
        }
        Value::Error(error) => {
            encoder.u8(15);
            encode_runtime_error(encoder, &snapshot_runtime_error(error), depth + 1)?;
        }
    }
    Ok(())
}

fn decode_value(
    decoder: &mut Decoder<'_>,
    callables: &[Arc<dyn crate::Callable>],
    depth: usize,
) -> Result<Value, CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new("execution capsule nesting is too deep"));
    }
    Ok(match decoder.u8()? {
        0 => Value::Null,
        1 => Value::Bool(decoder.bool()?),
        2 => Value::Int(decoder.i64()?),
        3 => Value::Float(
            FiniteFloat::new(f64::from_bits(decoder.u64()?))
                .map_err(|error| CapsuleError::new(error.to_string()))?,
        ),
        4 => Value::string(decoder.string()?),
        5 => Value::bytes(decoder.byte_string()?),
        6 => Value::Path(NativePath::new(decoder.native()?)),
        7 => Value::Duration(Duration::from_nanos(decoder.i128()?)),
        8 => Value::ByteSize(ByteSize::new(decoder.u64()?)),
        9 => {
            let count = decoder.collection_len()?;
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(decode_value(decoder, callables, depth + 1)?);
            }
            Value::list(values)
        }
        10 => {
            let count = decoder.collection_len()?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push((
                    decoder.string()?,
                    decode_value(decoder, callables, depth + 1)?,
                ));
            }
            Value::Record(
                Record::new(entries)
                    .map_err(|error| CapsuleError::new(format!("invalid record: {error}")))?,
            )
        }
        11 => {
            let width = decoder.collection_len()?;
            let mut columns = Vec::with_capacity(width);
            for _ in 0..width {
                columns.push(decoder.string()?);
            }
            let height = decoder.collection_len()?;
            let cells = width
                .checked_mul(height)
                .ok_or_else(|| CapsuleError::new("capsule table dimensions overflow"))?;
            decoder.charge_items(cells)?;
            let mut rows = Vec::with_capacity(height);
            for _ in 0..height {
                let mut row = Vec::with_capacity(width);
                for _ in 0..width {
                    row.push(decode_value(decoder, callables, depth + 1)?);
                }
                rows.push(row);
            }
            Value::Table(
                Table::new(columns, rows)
                    .map_err(|error| CapsuleError::new(format!("invalid table: {error}")))?,
            )
        }
        12 => Value::Range(Range::new(decoder.i64()?, decoder.i64()?, decoder.bool()?)),
        13 => Value::Status(decode_status(decoder, depth + 1)?),
        14 => {
            let id = usize::try_from(decoder.u32()?)
                .map_err(|_| CapsuleError::new("callable identity exceeds this platform"))?;
            Value::Callable(
                callables
                    .get(id)
                    .cloned()
                    .ok_or_else(|| CapsuleError::new("callable identity is not yet defined"))?,
            )
        }
        15 => Value::Error(Arc::new(restore_runtime_error(decode_runtime_error(
            decoder,
            depth + 1,
        )?))),
        16 => {
            let (id, arguments) = decode_nominal_identity(decoder, depth)?;
            let count = decoder.collection_len()?;
            let mut fields = Vec::with_capacity(count);
            let mut names = BTreeSet::new();
            for _ in 0..count {
                let name = decoder.string()?;
                if !names.insert(name.clone()) {
                    return Err(CapsuleError::new("duplicate nominal field"));
                }
                fields.push((
                    Arc::from(name),
                    decode_value(decoder, callables, depth + 1)?,
                ));
            }
            Value::NominalRecord(Box::new(NominalRecordValue::new(id, arguments, fields)))
        }
        17 => {
            let (id, arguments) = decode_nominal_identity(decoder, depth)?;
            let constructor = decoder.string()?;
            let count = decoder.collection_len()?;
            let mut payload = Vec::with_capacity(count);
            for _ in 0..count {
                payload.push(decode_value(decoder, callables, depth + 1)?);
            }
            Value::Variant(Box::new(VariantValue::new(
                id,
                arguments,
                constructor,
                payload,
            )))
        }
        tag => {
            return Err(CapsuleError::new(format!(
                "unknown capsule value tag {tag}"
            )));
        }
    })
}

fn encode_runtime_error(
    encoder: &mut Encoder,
    error: &RuntimeErrorSnapshot,
    depth: usize,
) -> Result<(), CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new(
            "execution capsule error nesting is too deep",
        ));
    }
    encoder.u8(error_category_tag(error.category));
    encoder.string(&error.message)?;
    let source = error.source.as_ref().ok_or_else(|| {
        CapsuleError::new("a source-free Error value cannot cross the execution capsule")
    })?;
    encode_source(encoder, source)?;
    encode_span(encoder, error.span)?;
    encoder.usize(error.labels.len())?;
    for label in &error.labels {
        encode_source(encoder, label.source())?;
        encode_span(encoder, label.span())?;
        encoder.string(label.message())?;
    }
    encoder.usize(error.frames.len())?;
    for frame in &error.frames {
        match frame.callee() {
            FrameCallee::Function(name) => {
                encoder.u8(0);
                encoder.string(name)?;
            }
            FrameCallee::Closure => encoder.u8(1),
        }
        encode_source(encoder, frame.source())?;
        encode_span(encoder, frame.call_site())?;
    }
    encoder.bool(error.cause.is_some());
    if let Some(cause) = &error.cause {
        encode_runtime_error(encoder, cause, depth + 1)?;
    }
    encoder.bool(error.status.is_some());
    if let Some(status) = &error.status {
        encode_status(encoder, status)?;
    }
    Ok(())
}

fn decode_runtime_error(
    decoder: &mut Decoder<'_>,
    depth: usize,
) -> Result<RuntimeErrorSnapshot, CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new(
            "execution capsule error nesting is too deep",
        ));
    }
    let category = decode_error_category(decoder.u8()?)?;
    let message = decoder.string()?;
    let source = decode_source(decoder)?;
    let span = decode_span(decoder, &source)?;
    let label_count = decoder.collection_len()?;
    let mut labels = Vec::with_capacity(label_count);
    for _ in 0..label_count {
        let label_source = Arc::new(decode_source(decoder)?);
        let label_span = decode_span(decoder, &label_source)?;
        labels.push(ErrorLabel::new(label_source, label_span, decoder.string()?));
    }
    let frame_count = decoder.collection_len()?;
    let mut frames = Vec::with_capacity(frame_count);
    for _ in 0..frame_count {
        let callee = match decoder.u8()? {
            0 => FrameCallee::Function(decoder.string()?),
            1 => FrameCallee::Closure,
            _ => return Err(CapsuleError::new("capsule error frame is invalid")),
        };
        let frame_source = Arc::new(decode_source(decoder)?);
        let call_site = decode_span(decoder, &frame_source)?;
        frames.push(CallFrame::restored(callee, call_site, frame_source));
    }
    let cause = decoder
        .bool()?
        .then(|| decode_runtime_error(decoder, depth + 1))
        .transpose()?
        .map(Box::new);
    let status = decoder
        .bool()?
        .then(|| decode_status(decoder, depth + 1))
        .transpose()?;
    Ok(RuntimeErrorSnapshot {
        category,
        message,
        span,
        labels,
        frames,
        source: Some(source),
        cause,
        status,
    })
}

const fn error_category_tag(category: ErrorCategory) -> u8 {
    match category {
        ErrorCategory::User => 0,
        ErrorCategory::Type => 1,
        ErrorCategory::Name => 2,
        ErrorCategory::Control => 3,
        ErrorCategory::Operation => 4,
        ErrorCategory::Command => 5,
        ErrorCategory::Io => 6,
        ErrorCategory::Process => 7,
        ErrorCategory::Job => 8,
        ErrorCategory::Resource => 9,
        ErrorCategory::Platform => 10,
        ErrorCategory::Internal => 11,
    }
}

fn decode_error_category(tag: u8) -> Result<ErrorCategory, CapsuleError> {
    match tag {
        0 => Ok(ErrorCategory::User),
        1 => Ok(ErrorCategory::Type),
        2 => Ok(ErrorCategory::Name),
        3 => Ok(ErrorCategory::Control),
        4 => Ok(ErrorCategory::Operation),
        5 => Ok(ErrorCategory::Command),
        6 => Ok(ErrorCategory::Io),
        7 => Ok(ErrorCategory::Process),
        8 => Ok(ErrorCategory::Job),
        9 => Ok(ErrorCategory::Resource),
        10 => Ok(ErrorCategory::Platform),
        11 => Ok(ErrorCategory::Internal),
        _ => Err(CapsuleError::new("capsule error category is invalid")),
    }
}

fn encode_source(encoder: &mut Encoder, source: &SourceFile) -> Result<(), CapsuleError> {
    encoder.u32(source.id().get());
    encoder.string(source.name())?;
    encoder.string(source.text())
}

fn decode_source(decoder: &mut Decoder<'_>) -> Result<SourceFile, CapsuleError> {
    Ok(SourceFile::new(
        SourceId::new(decoder.u32()?),
        decoder.string()?,
        decoder.string()?,
    ))
}

fn encode_span(encoder: &mut Encoder, span: opaal_syntax::Span) -> Result<(), CapsuleError> {
    encoder.u32(span.source_id().get());
    encoder.usize(span.start())?;
    encoder.usize(span.end())
}

fn decode_span(
    decoder: &mut Decoder<'_>,
    source: &SourceFile,
) -> Result<opaal_syntax::Span, CapsuleError> {
    let source_id = SourceId::new(decoder.u32()?);
    if source_id != source.id() {
        return Err(CapsuleError::new("capsule span source is inconsistent"));
    }
    let start = decoder.usize()?;
    let end = decoder.usize()?;
    source
        .span(ByteRange { start, end })
        .map_err(|error| CapsuleError::new(format!("invalid capsule span: {error}")))
}

fn encode_status(encoder: &mut Encoder, status: &Status) -> Result<(), CapsuleError> {
    encode_status_at(encoder, status, 0)
}

fn encode_status_at(
    encoder: &mut Encoder,
    status: &Status,
    depth: usize,
) -> Result<(), CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new(
            "execution capsule status nesting is too deep",
        ));
    }
    match status.code() {
        Some(code) => {
            encoder.bool(true);
            encoder.i64(code);
        }
        None => encoder.bool(false),
    }
    match status.signal() {
        Some(signal) => {
            encoder.bool(true);
            match signal.number() {
                Some(number) => {
                    encoder.bool(true);
                    encoder.i64(number);
                }
                None => encoder.bool(false),
            }
            encoder.optional_string(signal.name())?;
        }
        None => encoder.bool(false),
    }
    encoder.i128(status.duration().as_nanos());
    encoder.usize(status.stages().len())?;
    for stage in status.stages() {
        encode_status_at(encoder, stage, depth + 1)?;
    }
    Ok(())
}

fn decode_status(decoder: &mut Decoder<'_>, depth: usize) -> Result<Status, CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new("execution capsule nesting is too deep"));
    }
    let code = decoder.bool()?.then(|| decoder.i64()).transpose()?;
    let signal = if decoder.bool()? {
        let number = decoder.bool()?.then(|| decoder.i64()).transpose()?;
        let name = decoder.optional_string()?;
        Some(Signal::new(number, name).map_err(|error| CapsuleError::new(error.to_string()))?)
    } else {
        None
    };
    let duration = Duration::from_nanos(decoder.i128()?);
    let count = decoder.collection_len()?;
    let mut stages = Vec::with_capacity(count);
    for _ in 0..count {
        stages.push(decode_status(decoder, depth + 1)?);
    }
    let status = if stages.is_empty() {
        match (code, signal) {
            (Some(code), None) => Status::exit(code, duration),
            (None, Some(signal)) => Status::signaled(signal, duration),
            _ => {
                return Err(CapsuleError::new(
                    "capsule status has invalid completion fields",
                ));
            }
        }
    } else {
        let selected = stages
            .iter()
            .position(|stage| stage.code() == code && stage.signal() == signal.as_ref())
            .ok_or_else(|| CapsuleError::new("aggregate status has no selected stage"))?;
        Status::aggregate(stages, selected, duration)
    };
    status.map_err(|error| CapsuleError::new(format!("invalid capsule status: {error}")))
}

fn encode_value_type(encoder: &mut Encoder, value_type: &ValueType) -> Result<(), CapsuleError> {
    encode_value_type_at(encoder, value_type, 0)
}

fn encode_nominal_identity(
    encoder: &mut Encoder,
    id: &NominalTypeId,
    arguments: &[ValueType],
) -> Result<(), CapsuleError> {
    if crate::module::is_opaque_operational_nominal(id) {
        return Err(CapsuleError::new(
            "control carriers cannot cross the execution capsule",
        ));
    }
    encode_module_id(encoder, id.module())?;
    encoder.string(id.name())?;
    encoder.usize(arguments.len())?;
    for argument in arguments {
        encode_value_type(encoder, argument)?;
    }
    Ok(())
}

fn decode_nominal_identity(
    decoder: &mut Decoder<'_>,
    depth: usize,
) -> Result<(NominalTypeId, Vec<ValueType>), CapsuleError> {
    let module = decode_module_id(decoder)?;
    let name = decoder.string()?;
    let count = decoder.collection_len()?;
    let mut arguments = Vec::with_capacity(count);
    for _ in 0..count {
        arguments.push(decode_value_type(decoder, depth + 1)?);
    }
    let id = NominalTypeId::new(module, name);
    if crate::module::is_opaque_operational_nominal(&id) {
        return Err(CapsuleError::new(
            "control carriers cannot cross the execution capsule",
        ));
    }
    Ok((id, arguments))
}

fn encode_value_type_at(
    encoder: &mut Encoder,
    value_type: &ValueType,
    depth: usize,
) -> Result<(), CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new(
            "execution capsule type nesting is too deep",
        ));
    }
    let tag = match value_type {
        ValueType::Any => 0,
        ValueType::Null => 1,
        ValueType::Bool => 2,
        ValueType::Int => 3,
        ValueType::Float => 4,
        ValueType::String => 5,
        ValueType::Bytes => 6,
        ValueType::Path => 7,
        ValueType::Duration => 8,
        ValueType::ByteSize => 9,
        ValueType::List(element) => {
            encoder.u8(10);
            return encode_value_type_at(encoder, element, depth + 1);
        }
        ValueType::Record => 11,
        ValueType::Table => 12,
        ValueType::Range => 13,
        ValueType::Status => 14,
        ValueType::Error => 15,
        ValueType::Function => 16,
        ValueType::Closure => 17,
        ValueType::TypeParameter(name) => {
            encoder.u8(18);
            return encoder.string(name);
        }
        ValueType::Nominal { id, arguments } => {
            if crate::module::is_opaque_operational_nominal(id) {
                return Err(CapsuleError::new(
                    "control carriers cannot cross the execution capsule",
                ));
            }
            encoder.u8(19);
            encode_module_id(encoder, id.module())?;
            encoder.string(id.name())?;
            encoder.usize(arguments.len())?;
            for argument in arguments {
                encode_value_type_at(encoder, argument, depth + 1)?;
            }
            return Ok(());
        }
    };
    encoder.u8(tag);
    Ok(())
}

fn decode_value_type(decoder: &mut Decoder<'_>, depth: usize) -> Result<ValueType, CapsuleError> {
    if depth > MAX_CAPSULE_DEPTH {
        return Err(CapsuleError::new(
            "execution capsule type nesting is too deep",
        ));
    }
    Ok(match decoder.u8()? {
        0 => ValueType::Any,
        1 => ValueType::Null,
        2 => ValueType::Bool,
        3 => ValueType::Int,
        4 => ValueType::Float,
        5 => ValueType::String,
        6 => ValueType::Bytes,
        7 => ValueType::Path,
        8 => ValueType::Duration,
        9 => ValueType::ByteSize,
        10 => ValueType::List(Box::new(decode_value_type(decoder, depth + 1)?)),
        11 => ValueType::Record,
        12 => ValueType::Table,
        13 => ValueType::Range,
        14 => ValueType::Status,
        15 => ValueType::Error,
        16 => ValueType::Function,
        17 => ValueType::Closure,
        18 => ValueType::TypeParameter(decoder.string()?),
        19 => {
            let (id, arguments) = decode_nominal_identity(decoder, depth)?;
            ValueType::Nominal {
                id: Box::new(id),
                arguments,
            }
        }
        tag => return Err(CapsuleError::new(format!("unknown capsule type tag {tag}"))),
    })
}

#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i128(&mut self, value: i128) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn usize(&mut self, value: usize) -> Result<(), CapsuleError> {
        let value = u32::try_from(value)
            .map_err(|_| CapsuleError::new("capsule collection exceeds the u32 limit"))?;
        self.u32(value);
        Ok(())
    }

    fn byte_string(&mut self, value: &[u8]) -> Result<(), CapsuleError> {
        if self
            .bytes
            .len()
            .checked_add(4)
            .and_then(|used| used.checked_add(value.len()))
            .is_none_or(|used| used > MAX_CAPSULE_BYTES)
        {
            return Err(CapsuleError::new(
                "execution capsule exceeds its byte limit",
            ));
        }
        self.usize(value.len())?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), CapsuleError> {
        self.byte_string(value.as_bytes())
    }

    fn optional_string(&mut self, value: Option<&str>) -> Result<(), CapsuleError> {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    fn native(&mut self, value: &OsStr) -> Result<(), CapsuleError> {
        self.byte_string(value.as_bytes())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
    items_remaining: usize,
    analysis_work_remaining: u64,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            items_remaining: MAX_CAPSULE_ITEMS,
            analysis_work_remaining: 5_000_000,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], CapsuleError> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| CapsuleError::new("execution capsule is truncated"))?;
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, CapsuleError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, CapsuleError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CapsuleError::new("capsule boolean is invalid")),
        }
    }

    fn u16(&mut self) -> Result<u16, CapsuleError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("exact length"),
        ))
    }

    fn u32(&mut self) -> Result<u32, CapsuleError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("exact length"),
        ))
    }

    fn u64(&mut self) -> Result<u64, CapsuleError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn i64(&mut self) -> Result<i64, CapsuleError> {
        Ok(i64::from_le_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn i128(&mut self) -> Result<i128, CapsuleError> {
        Ok(i128::from_le_bytes(
            self.take(16)?.try_into().expect("exact length"),
        ))
    }

    fn usize(&mut self) -> Result<usize, CapsuleError> {
        usize::try_from(self.u32()?)
            .map_err(|_| CapsuleError::new("capsule length exceeds this platform"))
    }

    fn collection_len(&mut self) -> Result<usize, CapsuleError> {
        let count = self.usize()?;
        self.charge_items(count)?;
        Ok(count)
    }

    fn charge_items(&mut self, count: usize) -> Result<(), CapsuleError> {
        self.items_remaining = self
            .items_remaining
            .checked_sub(count)
            .ok_or_else(|| CapsuleError::new("execution capsule has too many values"))?;
        Ok(())
    }

    fn byte_string(&mut self) -> Result<Vec<u8>, CapsuleError> {
        let count = self.usize()?;
        Ok(self.take(count)?.to_vec())
    }

    fn string(&mut self) -> Result<String, CapsuleError> {
        String::from_utf8(self.byte_string()?)
            .map_err(|_| CapsuleError::new("capsule string is not UTF-8"))
    }

    fn optional_string(&mut self) -> Result<Option<String>, CapsuleError> {
        self.bool()?.then(|| self.string()).transpose()
    }

    fn native(&mut self) -> Result<OsString, CapsuleError> {
        Ok(OsString::from_vec(self.byte_string()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{RuntimeError, RuntimeErrorKind};

    fn options() -> SessionOptions {
        SessionOptions::default()
            .with_pipefail(true)
            .with_capture_limit(12_345)
    }

    #[test]
    fn reconstructed_callbacks_keep_operation_aliases_and_callable_kind() {
        use crate::eval::{
            Completion, EvalLimits, evaluate_in_environment_owned_with_binding_types,
        };
        use crate::module::{ModuleAliasRegistry, RuntimeBindingTypes};
        use opaal_syntax::{ParseOutcome, parse_opaal};

        let apply = |callable: &Value, arguments| {
            let caller = SourceFile::new(SourceId::new(93), "next.opaal", "process()");
            crate::eval::apply_callable(
                callable,
                arguments,
                &caller,
                caller.span(0..9).unwrap(),
                &mut Environment::new(),
                &EvalLimits::default(),
            )
        };
        let round_trip = |text: &str| {
            let source = Arc::new(SourceFile::new(SourceId::new(92), "callbacks.opaal", text));
            let ParseOutcome::Complete(script) = parse_opaal(&source) else {
                panic!("fixture must parse");
            };
            let types = RuntimeBindingTypes::analyze_repl_source(
                &source,
                &script,
                &ModuleAliasRegistry::default(),
            )
            .unwrap();
            let mut scope = ScopeStack::new();
            evaluate_in_environment_owned_with_binding_types(
                &script,
                source,
                &mut scope,
                &mut Environment::new(),
                &EvalLimits::default(),
                Arc::new(types),
            )
            .unwrap();
            let wire = encode_background_capsule(
                "next.opaal",
                "",
                Path::new("/project"),
                &Environment::new(),
                None,
                &scope,
                options(),
            )
            .unwrap();
            decode_background_capsule(&wire).unwrap()
        };
        let decoded = round_trip(
            "import std::list as list\nlet amount = 3\ndef transform(x: Int) -> Int { x + amount }\ndef process(items: List[Int]) -> List[Int] { list::map[Int, Int](items, transform) }",
        );
        let process = decoded.scope().get("process").unwrap();
        assert!(matches!(
            apply(process, vec![Value::list(vec![Value::Int(2), Value::Int(1)])]).unwrap(),
            Completion::Value(value) if value == Value::list(vec![Value::Int(5), Value::Int(4)])
        ));

        let decoded = round_trip(
            "import std::list as list\ndef identity[X: Equal](x: Any) -> Any { x }\ndef make[X: Equal]() -> Closure { {|x| identity[X](x)} }\nlet callback = make[Int]()\ndef process() -> List[Int] { list::map[Int, Int]([1], callback) }",
        );
        assert!(matches!(
            apply(decoded.scope().get("process").unwrap(), vec![]).unwrap(),
            Completion::Value(value) if value == Value::list(vec![Value::Int(1)])
        ));

        for definition in [
            "action callback(x: Int) -> Int effects {} { x }",
            "action callback(x: Int) -> Int effects { clock.wall; } { x }",
            "def callback[X](x: Int) -> Int { x }",
        ] {
            let text = format!(
                "import std::list as list\n{definition}\ndef process() -> List[Int] {{ list::map[Int, Int]([], callback) }}"
            );
            let decoded = round_trip(&text);
            let process = decoded.scope().get("process").unwrap();
            assert!(apply(process, vec![]).is_err(), "{definition}");
        }
    }

    #[test]
    fn nominal_codec_keeps_nested_control_carriers_opaque() {
        for id in [
            NominalTypeId::project("tools", "ToolIdentity"),
            NominalTypeId::standard("http", "SecretHeader"),
        ] {
            let value = Value::NominalRecord(Box::new(NominalRecordValue::new(
                id,
                Vec::new(),
                vec![(Arc::from("_id"), Value::string("private-marker"))],
            )));
            let nested = Value::list(vec![Value::Record(
                Record::new(vec![("carrier".to_owned(), value)]).unwrap(),
            )]);
            let mut scope = ScopeStack::new();
            scope
                .declare("nested", BindingMutability::Immutable, nested)
                .unwrap();
            let error = encode_background_capsule(
                "next",
                "",
                Path::new("/project"),
                &Environment::new(),
                None,
                &scope,
                options(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("control carriers"));
            assert!(!error.to_string().contains("private-marker"));
        }
    }

    #[test]
    fn imported_callbacks_and_generic_nominals_restore_without_host_loading() {
        use crate::eval::{Completion, EvalLimits, FakeClock};
        use crate::module::{
            ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
        };
        use crate::session::Session;
        use opaal_platform::FakePlatform;

        struct Sources(BTreeMap<PathBuf, String>);
        impl ModuleCanonicalizer for Sources {
            fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
                let path = candidate.components().collect::<PathBuf>();
                self.0
                    .contains_key(&path)
                    .then_some(path)
                    .ok_or_else(|| ModulePathError::new("missing source"))
            }
        }
        impl ModuleSourceLoader for Sources {
            fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
                self.0
                    .get(module.path())
                    .map(|text| text.as_bytes().to_vec())
                    .ok_or_else(|| ModuleSourceError::new("missing source"))
            }
        }
        let sources = Sources(BTreeMap::from([
            (PathBuf::from("/project/model.opaal"), "type Box[T] = { value: T }\ndef get[T](item: Box[T]) -> T { item.value }\nexport { Box, get }\n".to_owned()),
            (PathBuf::from("/project/facade.opaal"), "import './model.opaal' as model\nexport { model }\n".to_owned()),
        ]));
        let mut session = Session::new("/project", Environment::new(), options());
        for text in [
            "import './facade.opaal' as api",
            "let item: api::model::Box[Int] = api::model::Box {value: 7}",
            "def process(item: api::model::Box[Int]) -> Int { api::model::get[Int](item) }",
            "let unpack = {|api::model::Box {value: x}: api::model::Box[Int]| x}",
        ] {
            session
                .submit_with_source_loader(
                    "cell",
                    text,
                    &sources,
                    &sources,
                    &opaal_platform_posix::PosixPlatform,
                    &FakePlatform::full(),
                    &FakeClock::new(),
                    &mut Vec::new(),
                )
                .unwrap();
        }
        let wire = encode_background_capsule(
            "next",
            "",
            Path::new("/project"),
            &Environment::new(),
            None,
            session.scope(),
            options(),
        )
        .unwrap();
        drop(sources);
        let decoded = decode_background_capsule(&wire).unwrap();
        let caller = SourceFile::new(SourceId::new(50_000), "next", "process(item)");
        let value = crate::eval::apply_callable(
            decoded.scope().get("process").unwrap(),
            vec![decoded.scope().get("item").unwrap().clone()],
            &caller,
            caller.span(0..caller.len()).unwrap(),
            &mut Environment::new(),
            &EvalLimits::default(),
        )
        .unwrap();
        assert!(matches!(value, Completion::Value(Value::Int(7))));
        assert_eq!(decoded.scope().get("item"), session.scope().get("item"));
        let unpacked = crate::eval::apply_callable(
            decoded.scope().get("unpack").unwrap(),
            vec![decoded.scope().get("item").unwrap().clone()],
            &caller,
            caller.span(0..caller.len()).unwrap(),
            &mut Environment::new(),
            &EvalLimits::default(),
        )
        .unwrap();
        assert!(matches!(unpacked, Completion::Value(Value::Int(7))));
        let Value::Callable(process) = session.scope().get("process").unwrap() else {
            panic!("callable")
        };
        let mut forged = snapshot_callable(process).unwrap();
        forged.parameters[0].1 = ValueType::Any;
        assert!(
            restore_callable(forged, &mut 5_000_000)
                .err()
                .unwrap()
                .contains("signature")
        );
        let Value::NominalRecord(item) = session.scope().get("item").unwrap() else {
            panic!("nominal record")
        };
        for fields in [
            Vec::new(),
            vec![(Arc::from("value"), Value::string("wrong type"))],
        ] {
            let mut malformed = session.scope().clone();
            malformed
                .declare(
                    "malformed",
                    BindingMutability::Immutable,
                    Value::NominalRecord(Box::new(NominalRecordValue::new(
                        item.id().clone(),
                        item.type_arguments().to_vec(),
                        fields,
                    ))),
                )
                .unwrap();
            let error = encode_background_capsule(
                "next",
                "",
                Path::new("/project"),
                &Environment::new(),
                None,
                &malformed,
                options(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("retained schema"), "{error}");
        }
        let mut malformed = session.scope().clone();
        malformed
            .declare(
                "unresolved",
                BindingMutability::Immutable,
                Value::NominalRecord(Box::new(NominalRecordValue::new(
                    item.id().clone(),
                    vec![ValueType::TypeParameter("T".to_owned())],
                    item.fields().to_vec(),
                ))),
            )
            .unwrap();
        let error = encode_background_capsule(
            "next",
            "",
            Path::new("/project"),
            &Environment::new(),
            None,
            &malformed,
            options(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unresolved type argument"));
        session
            .submit_with_source_loader(
                "cell",
                "type Forward = { item: later::Box[Int] }\n1 / 0\nimport './model.opaal' as later",
                &Sources(BTreeMap::new()),
                &Sources(BTreeMap::new()),
                &opaal_platform_posix::PosixPlatform,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap_err();
        // The initialized transitive snapshot supplies analysis metadata; the
        // unreached alias itself stays unpublished.
        assert!(session.scope().get("later::get").is_none());
        session
            .submit_with_value(
                "cell",
                "def forward(item: Forward) -> Int { item.item.value }",
                &opaal_platform_posix::PosixPlatform,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap();
        let wire = encode_background_capsule(
            "next",
            "",
            Path::new("/project"),
            &Environment::new(),
            None,
            session.scope(),
            options(),
        )
        .unwrap();
        decode_background_capsule(&wire).unwrap();
    }

    #[test]
    fn typed_capsule_round_trips_native_state_and_value_families() {
        let source = SourceFile::new(SourceId::new(91), "captured.opaal", "throw 'boom'\n");
        let span = source.span(0..12).expect("valid error span");
        let error = RuntimeError::new(
            RuntimeErrorKind::UserThrown {
                message: "boom".to_owned(),
            },
            span,
        )
        .with_source(Arc::new(source));
        let table = Table::new(
            vec!["name".to_owned()],
            vec![vec![Value::Path(NativePath::new(OsString::from_vec(
                vec![b'/', b'x', 0x80],
            )))]],
        )
        .expect("valid table");
        let mut scope = ScopeStack::new();
        scope
            .declare_typed(
                "items",
                BindingMutability::Mutable,
                Value::list(vec![
                    Value::Null,
                    Value::Bool(true),
                    Value::Int(-7),
                    Value::Float(FiniteFloat::new(1.5).expect("finite")),
                    Value::string("text"),
                    Value::bytes([0, 0xff]),
                    Value::Duration(Duration::from_nanos(-8)),
                    Value::ByteSize(ByteSize::new(9)),
                    Value::Range(Range::new(1, 3, true)),
                    Value::Status(Status::exit(4, Duration::ZERO).expect("valid status")),
                    Value::Table(table),
                    Value::Error(Arc::new(error)),
                ]),
                Some(ValueType::List(Box::new(ValueType::Any))),
            )
            .expect("valid binding");
        let environment = Environment::from_snapshot(vec![(
            OsString::from_vec(vec![b'N', 0x82]),
            OsString::from_vec(vec![b'a', 0x80]),
        )]);
        let cwd = PathBuf::from(OsString::from_vec(vec![b'/', b'w', 0x81]));
        let current_status = Status::exit(23, Duration::from_nanos(7)).expect("valid status");

        let wire = encode_background_capsule(
            "capsule.opaal",
            "^tool\n",
            &cwd,
            &environment,
            Some(&current_status),
            &scope,
            options(),
        )
        .expect("capsule should encode");
        let decoded = decode_background_capsule(&wire).expect("capsule should decode");

        assert_eq!(decoded.name(), "capsule.opaal");
        assert_eq!(decoded.text(), "^tool\n");
        assert_eq!(decoded.cwd(), cwd);
        assert_eq!(decoded.environment(), &environment);
        assert_eq!(decoded.current_status(), Some(&current_status));
        assert_eq!(decoded.options(), options());
        assert_eq!(
            decoded.scope().mutability("items"),
            Some(BindingMutability::Mutable)
        );
        let Value::List(items) = decoded.scope().get("items").expect("items binding") else {
            panic!("items should remain a list");
        };
        assert_eq!(items.len(), 12);
        let Value::Error(error) = &items[11] else {
            panic!("the Error family should survive transport");
        };
        assert_eq!(error.category(), ErrorCategory::User);
        assert_eq!(error.to_string(), "boom");
        assert_eq!(
            RuntimeError::source(error.as_ref()).map(SourceFile::name),
            Some("captured.opaal")
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn capsule_preserves_capture_limits_above_the_u32_range() {
        let capture_limit = usize::try_from(u64::from(u32::MAX) + 1)
            .expect("the test runs only on a 64-bit target");
        let wire = encode_background_capsule(
            "capsule.opaal",
            "^tool\n",
            Path::new("/work"),
            &Environment::new(),
            None,
            &ScopeStack::new(),
            SessionOptions::default().with_capture_limit(capture_limit),
        )
        .expect("a host-representable capture limit should encode");

        let decoded = decode_background_capsule(&wire).expect("the capsule should decode");
        assert_eq!(decoded.options().capture_limit(), capture_limit);
    }

    #[test]
    fn capsule_refuses_truncation_versions_lengths_and_allocation_bombs() {
        let wire = encode_background_capsule(
            "capsule.opaal",
            "^tool\n",
            Path::new("/work"),
            &Environment::new(),
            None,
            &ScopeStack::new(),
            options(),
        )
        .expect("capsule should encode");

        assert!(decode_background_capsule(&wire[..wire.len() - 1]).is_err());

        let mut wrong_version = wire.clone();
        wrong_version[MAGIC.len()..MAGIC.len() + 2].copy_from_slice(&1u16.to_le_bytes());
        assert!(decode_background_capsule(&wrong_version).is_err());

        let mut wrong_length = wire.clone();
        wrong_length[MAGIC.len() + 2..18].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_background_capsule(&wrong_length).is_err());

        let mut bomb = wire;
        let callable_count_offset = {
            let mut payload = Decoder::new(&bomb[18..]);
            let _ = payload.string().expect("name");
            let _ = payload.string().expect("text");
            let _ = payload.native().expect("cwd");
            let _ = decode_environment(&mut payload).expect("environment");
            let has_status = payload.bool().expect("current-status marker");
            assert!(!has_status);
            let _ = payload.bool().expect("pipefail");
            let _ = payload.u64().expect("capture limit");
            18 + payload.offset
        };
        bomb[callable_count_offset..callable_count_offset + 4]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_background_capsule(&bomb).is_err());

        let oversized_completion = vec![0_u8; MAX_CAPSULE_BYTES + 19];
        assert!(decode_supervisor_completion(&oversized_completion).is_err());
    }

    #[test]
    fn supervisor_completion_round_trips_and_applies_only_its_typed_delta() {
        let mut base_scope = ScopeStack::new();
        base_scope
            .declare_typed(
                "changed",
                BindingMutability::Mutable,
                Value::Int(1),
                Some(ValueType::Int),
            )
            .expect("base binding");
        base_scope
            .declare("removed", BindingMutability::Immutable, Value::Bool(true))
            .expect("removed binding");
        let mut updated_scope = base_scope.clone();
        updated_scope
            .assign("changed", Value::Int(2))
            .expect("updated binding");
        updated_scope.remove("removed");
        updated_scope
            .declare("added", BindingMutability::Immutable, Value::string("new"))
            .expect("added binding");

        let mut base_state = SessionState::new(
            "/base",
            Environment::from_snapshot([("A", OsString::from("old"))]),
        );
        base_state.set_current_status(Some(Status::exit(7, Duration::ZERO).expect("base status")));
        let mut updated_state = SessionState::new(
            "/updated",
            Environment::from_snapshot([
                ("A", OsString::from("new")),
                ("B", OsString::from_vec(vec![0x80])),
            ]),
        );
        updated_state.set_current_status(Some(
            Status::exit(0, Duration::from_nanos(9)).expect("updated status"),
        ));
        let completion = supervisor_completion(
            SupervisorOutcome::Continued,
            updated_state.current_status().cloned(),
            base_scope.clone(),
            updated_scope,
            base_state.clone(),
            updated_state,
        );
        let wire = encode_supervisor_completion(&completion).expect("completion should encode");
        let decoded = decode_supervisor_completion(&wire).expect("completion should decode");
        assert_eq!(decoded.outcome(), SupervisorOutcome::Continued);
        assert_eq!(decoded.status().and_then(Status::code), Some(0));

        let mut target_scope = base_scope;
        target_scope.push();
        target_scope
            .declare("unrelated", BindingMutability::Immutable, Value::Int(99))
            .expect("unrelated binding");
        let mut target_state = base_state;
        target_state
            .environment_mut()
            .set("C", OsString::from("kept"));
        decoded
            .apply(&mut target_scope, &mut target_state)
            .expect("the typed delta should apply");

        assert_eq!(target_scope.get("changed"), Some(&Value::Int(2)));
        assert!(target_scope.get("removed").is_none());
        assert_eq!(target_scope.get("added"), Some(&Value::string("new")));
        assert_eq!(target_scope.get("unrelated"), Some(&Value::Int(99)));
        assert_eq!(target_state.cwd(), Path::new("/updated"));
        assert_eq!(target_state.environment().get("A"), Some(OsStr::new("new")));
        assert_eq!(
            target_state.environment().get("B").map(OsStr::as_bytes),
            Some(&[0x80][..])
        );
        assert_eq!(
            target_state.environment().get("C"),
            Some(OsStr::new("kept"))
        );
        assert_eq!(
            target_state.current_status().and_then(Status::code),
            Some(0)
        );
        target_scope.pop().expect("pop the unrelated nested frame");
        assert_eq!(
            target_scope.get("changed"),
            Some(&Value::Int(2)),
            "an update keeps the parent binding in its original lexical frame"
        );

        assert!(decode_supervisor_completion(&wire[..wire.len() - 1]).is_err());
        let mut wrong_version = wire;
        wrong_version[COMPLETION_MAGIC.len()..COMPLETION_MAGIC.len() + 2]
            .copy_from_slice(&1u16.to_le_bytes());
        assert!(decode_supervisor_completion(&wrong_version).is_err());
    }
}
