//! Controlled source bridge for the maintained operational modules.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opaal_platform::Platform;
use opaal_platform::operational::{HttpHeader, OperationalAdapter};

use crate::authority::{AuthorityVerdict, CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::module::{ActionId, ModuleId, ModuleOrigin, NominalTypeId};
use crate::project::{MaintainedAdapter, ProjectManifest, ToolLock};
use crate::security::{MAX_SECRET_BYTES, Secret, SecretId};
use crate::{NativePath, NominalRecordValue, Record, Status, Value};

use super::{ModuleError, data, filesystem, http, integrity, path, process, time, url, version};

const MAX_JOURNAL_MESSAGE_BYTES: usize = 4 * 1024;

/// One effect boundary projected into the durable run journal.
#[derive(Clone, Debug)]
pub struct SourceEffectEvent {
    action_node_id: String,
    request: CapabilityRequest,
    operation: SourceOperation,
    attempt: u64,
}

impl SourceEffectEvent {
    #[must_use]
    pub fn action_node_id(&self) -> &str {
        &self.action_node_id
    }

    #[must_use]
    pub const fn request(&self) -> &CapabilityRequest {
        &self.request
    }

    #[must_use]
    pub const fn operation(&self) -> &SourceOperation {
        &self.operation
    }

    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.attempt
    }
}

/// Closed, redaction-safe identity for one controlled source operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceOperation {
    Filesystem {
        operation: String,
        relative_path: PathBuf,
    },
    Process {
        operation: String,
        tool: String,
        argv_count: u64,
        argv_bytes: u64,
        argv_digest: String,
        argv: Vec<SourceProcessArgument>,
    },
    Http {
        operation: String,
        endpoint: String,
        method: String,
    },
    Clock {
        operation: String,
        clock: SourceClock,
    },
    SecretReveal {
        operation: String,
        endpoint: String,
        header: String,
    },
}

/// One allowlisted process argument representation in operation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceProcessArgument {
    Public(String),
    Redacted { bytes: u64, digest: String },
}

/// The clock class exposed by a redacted clock operation descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceClock {
    Wall,
    Monotonic,
}

/// Result metadata which may safely complete an operation descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceEffectResult {
    Filesystem {
        bytes: Option<u64>,
        evidence_digest: Option<String>,
    },
    Process {
        status: Option<i64>,
        stdout_bytes: Option<u64>,
        stderr_bytes: Option<u64>,
        evidence_digest: Option<String>,
    },
    Http {
        status: Option<u16>,
        body_bytes: Option<u64>,
        evidence_digest: Option<String>,
    },
    Clock {
        evidence_digest: Option<String>,
    },
    SecretReveal {
        evidence_digest: Option<String>,
    },
}

impl SourceOperation {
    fn empty_result(&self) -> SourceEffectResult {
        match self {
            Self::Filesystem { .. } => SourceEffectResult::Filesystem {
                bytes: None,
                evidence_digest: None,
            },
            Self::Process { .. } => SourceEffectResult::Process {
                status: None,
                stdout_bytes: None,
                stderr_bytes: None,
                evidence_digest: None,
            },
            Self::Http { .. } => SourceEffectResult::Http {
                status: None,
                body_bytes: None,
                evidence_digest: None,
            },
            Self::Clock { .. } => SourceEffectResult::Clock {
                evidence_digest: None,
            },
            Self::SecretReveal { .. } => SourceEffectResult::SecretReveal {
                evidence_digest: None,
            },
        }
    }
}

impl SourceEffectResult {
    fn evidence_digest(&self) -> Option<&str> {
        match self {
            Self::Filesystem {
                evidence_digest, ..
            }
            | Self::Process {
                evidence_digest, ..
            }
            | Self::Http {
                evidence_digest, ..
            }
            | Self::Clock { evidence_digest }
            | Self::SecretReveal { evidence_digest } => evidence_digest.as_deref(),
        }
    }
}

/// Closed effect outcome passed to a journal owner.
#[derive(Clone, Debug)]
pub struct SourceEffectOutcome {
    class: &'static str,
    code: String,
    message: String,
    status: Option<i64>,
    value_digest: Option<String>,
    result: Option<SourceEffectResult>,
    partial: bool,
}

/// One action-boundary outcome projected into the durable run journal.
#[derive(Clone, Debug)]
pub struct SourceActionOutcome(SourceEffectOutcome);

impl SourceActionOutcome {
    pub(crate) fn new(
        class: &'static str,
        code: impl Into<String>,
        message: impl Into<String>,
        value_digest: Option<String>,
        partial: bool,
    ) -> Self {
        Self(SourceEffectOutcome {
            class,
            code: code.into(),
            message: message.into(),
            status: None,
            value_digest,
            result: None,
            partial,
        })
    }

    #[must_use]
    pub const fn outcome(&self) -> &SourceEffectOutcome {
        &self.0
    }
}

impl SourceEffectOutcome {
    #[must_use]
    pub const fn class(&self) -> &'static str {
        self.class
    }
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
    #[must_use]
    pub const fn status(&self) -> Option<i64> {
        self.status
    }
    #[must_use]
    pub fn value_digest(&self) -> Option<&str> {
        self.value_digest.as_deref()
    }
    #[must_use]
    pub const fn result(&self) -> Option<&SourceEffectResult> {
        self.result.as_ref()
    }
    #[must_use]
    pub const fn partial(&self) -> bool {
        self.partial
    }
}

/// Durable sink owned by the accepted-execution boundary.
pub trait SourceEffectJournal {
    fn action_start(&mut self, action_node_id: &str, action: &ActionId) -> Result<(), String>;

    fn action_end(
        &mut self,
        action_node_id: &str,
        outcome: &SourceActionOutcome,
    ) -> Result<(), String>;

    fn before(
        &mut self,
        event: &SourceEffectEvent,
        verdict: AuthorityVerdict,
    ) -> Result<(), String>;

    fn after(
        &mut self,
        event: &SourceEffectEvent,
        outcome: &SourceEffectOutcome,
    ) -> Result<(), String>;
}

/// Secondary evidence retained beside the source evaluation primary.
#[derive(Clone, Debug)]
pub(crate) enum SourceOperationalEvidence {
    Completed {
        operation: String,
        status: Option<Status>,
    },
    Partial {
        operation: String,
        detail: String,
    },
}

/// Evaluator-facing interface. Only the accepted-plan executor constructs an
/// implementation; every ordinary evaluator host returns no bridge.
pub(crate) trait SourceOperationalHost {
    fn action_start(&mut self, action: &ActionId) -> Result<(), ModuleError>;

    fn action_end(
        &mut self,
        action: &ActionId,
        outcome: SourceActionOutcome,
    ) -> Result<(), ModuleError>;

    fn invoke(
        &mut self,
        module: &ModuleId,
        operation: &str,
        arguments: Vec<Value>,
    ) -> Result<Value, ModuleError>;

    fn take_evidence(&mut self) -> Vec<SourceOperationalEvidence>;
}

/// Whether a value contains a control-only project identity or secret handle.
pub(crate) fn contains_control_carrier(value: &Value) -> bool {
    match value {
        Value::List(values) => values.iter().any(contains_control_carrier),
        Value::Record(record) => record
            .entries()
            .iter()
            .any(|(_, value)| contains_control_carrier(value)),
        Value::NominalRecord(record) => {
            let opaque = match record.id().module().origin() {
                ModuleOrigin::Standard { namespace, module } if namespace == "project" => {
                    matches!(module.as_str(), "tools" | "endpoints" | "secrets")
                }
                ModuleOrigin::Standard { namespace, module } if namespace == "std" => {
                    module == "http" && record.id().name() == "SecretHeader"
                }
                _ => false,
            };
            opaque
                || record
                    .fields()
                    .iter()
                    .any(|(_, value)| contains_control_carrier(value))
        }
        Value::Variant(variant) => variant.payload().iter().any(contains_control_carrier),
        Value::Table(table) => table
            .rows()
            .iter()
            .flat_map(|row| row.iter())
            .any(contains_control_carrier),
        Value::Callable(callable) => crate::eval::callable_contains_control_carrier(callable),
        _ => false,
    }
}

struct ZeroingBytes(Vec<u8>);

impl Drop for ZeroingBytes {
    fn drop(&mut self) {
        self.0.fill(0);
        let _ = std::hint::black_box(&mut self.0);
    }
}

/// Exact identities and bounded adapter state for one accepted action run.
pub struct ControlledSourceOperations<'a> {
    context: &'a mut OperationalContext,
    effects: &'a EffectSet,
    manifest: &'a ProjectManifest,
    tools: &'a ToolLock,
    executable_files: &'a BTreeMap<String, File>,
    platform: &'a dyn Platform,
    adapter: &'a dyn OperationalAdapter,
    tls_ca: &'a BTreeMap<String, Vec<u8>>,
    journal: &'a mut dyn SourceEffectJournal,
    action_nodes: BTreeMap<ActionId, String>,
    action_stack: Vec<(ActionId, bool)>,
    planned_inputs: BTreeMap<PathBuf, String>,
    read_budget: filesystem::ReadBudget,
    process_budget: process::ProcessBudget,
    http_budget: http::HttpBudget,
    next_attempt: u64,
    next_secret_header: u64,
    secret_headers: BTreeMap<u64, (String, String)>,
    secret_input_id: Option<String>,
    secret_input: Option<&'a mut dyn Read>,
    evidence: Vec<SourceOperationalEvidence>,
}

impl<'a> ControlledSourceOperations<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: &'a mut OperationalContext,
        effects: &'a EffectSet,
        manifest: &'a ProjectManifest,
        tools: &'a ToolLock,
        executable_files: &'a BTreeMap<String, File>,
        platform: &'a dyn Platform,
        adapter: &'a dyn OperationalAdapter,
        tls_ca: &'a BTreeMap<String, Vec<u8>>,
        journal: &'a mut dyn SourceEffectJournal,
        action_nodes: BTreeMap<ActionId, String>,
        planned_inputs: BTreeMap<PathBuf, String>,
        process_budget: process::ProcessBudget,
        secret_input: Option<(String, &'a mut dyn Read)>,
    ) -> Self {
        let next_attempt = process_budget.attempts().try_into().unwrap_or(u64::MAX);
        let (secret_input_id, secret_input) =
            secret_input.map_or((None, None), |(id, input)| (Some(id), Some(input)));
        Self {
            context,
            effects,
            manifest,
            tools,
            executable_files,
            platform,
            adapter,
            tls_ca,
            journal,
            action_nodes,
            action_stack: Vec::new(),
            planned_inputs,
            read_budget: filesystem::ReadBudget::default(),
            process_budget,
            http_budget: http::HttpBudget::default(),
            next_attempt,
            next_secret_header: 0,
            secret_headers: BTreeMap::new(),
            secret_input_id,
            secret_input,
            evidence: Vec::new(),
        }
    }

    fn materialize_secret(&mut self, secret_id: &SecretId) -> Result<(), ModuleError> {
        if self.secret_input_id.as_deref() != Some(secret_id.as_str()) {
            return Err(ModuleError::invalid(
                "EXECUTE008",
                "secret input identity differs from the requested sink",
            ));
        }
        let input = self.secret_input.take().ok_or_else(|| {
            ModuleError::invalid("EXECUTE008", "secret input was already consumed")
        })?;
        let mut bytes = ZeroingBytes(Vec::with_capacity(4096));
        input
            .take((MAX_SECRET_BYTES + 1) as u64)
            .read_to_end(&mut bytes.0)
            .map_err(|error| ModuleError::invalid("EXECUTE008", error.to_string()))?;
        if bytes.0.is_empty() || bytes.0.len() > MAX_SECRET_BYTES {
            return Err(ModuleError::invalid(
                "EXECUTE008",
                "secret stdin must contain 1 through 65536 bytes",
            ));
        }
        let payload = std::mem::take(&mut bytes.0);
        self.context
            .insert_secret(
                Secret::new(secret_id.clone(), payload)
                    .map_err(|error| ModuleError::invalid("EXECUTE008", error.to_string()))?,
            )
            .map_err(|error| ModuleError::invalid("EXECUTE008", error.to_string()))
    }

    fn effect<T>(
        &mut self,
        request: CapabilityRequest,
        operation: &str,
        descriptor: SourceOperation,
        invoke: impl FnOnce(&mut Self) -> Result<(T, Option<Status>, SourceEffectResult), ModuleError>,
    ) -> Result<T, ModuleError> {
        if let Some(reason) = self.context.poll_cancellation() {
            return Err(ModuleError::Cancelled(reason));
        }
        let verdict = self.context.verdict(self.effects, &request, self.platform);
        if !verdict.is_executable() {
            return Err(ModuleError::Authority { verdict });
        }
        let action_node_id = self
            .action_stack
            .last()
            .and_then(|(action, _)| self.action_nodes.get(action))
            .cloned()
            .ok_or_else(|| {
                ModuleError::invalid("JOURNAL006", "effect occurred outside an action boundary")
            })?;
        for (_, attempted) in &mut self.action_stack {
            *attempted = true;
        }
        let attempt = self.next_attempt;
        self.next_attempt = self
            .next_attempt
            .checked_add(1)
            .ok_or_else(|| ModuleError::invalid("OPERATION005", "effect attempt overflow"))?;
        let event = SourceEffectEvent {
            action_node_id,
            request,
            operation: descriptor,
            attempt,
        };
        self.journal
            .before(&event, verdict)
            .map_err(|message| ModuleError::invalid("JOURNAL005", message))?;

        match invoke(self) {
            Ok((value, status, result)) => {
                let outcome = SourceEffectOutcome {
                    class: "success",
                    code: "EFFECT000".to_owned(),
                    message: "effect completed".to_owned(),
                    status: status.as_ref().and_then(Status::code),
                    value_digest: result.evidence_digest().map(str::to_owned),
                    result: Some(result),
                    partial: false,
                };
                if let Err(message) = self.journal.after(&event, &outcome) {
                    self.evidence.push(SourceOperationalEvidence::Partial {
                        operation: operation.to_owned(),
                        detail: "effect completed but its journal record failed".to_owned(),
                    });
                    return Err(ModuleError::invalid("JOURNAL005", message));
                }
                self.evidence.push(SourceOperationalEvidence::Completed {
                    operation: operation.to_owned(),
                    status,
                });
                Ok(value)
            }
            Err(error) => {
                let partial = matches!(error, ModuleError::Adapter(_));
                let class = match &error {
                    ModuleError::Authority { .. } => "refused",
                    ModuleError::Invalid {
                        code: "EXECUTE_STALE",
                        ..
                    } => "refused",
                    ModuleError::Cancelled(_) => "cancelled",
                    ModuleError::Invalid { .. } | ModuleError::Adapter(_) => "error",
                };
                let outcome = SourceEffectOutcome {
                    class,
                    code: error.code().to_owned(),
                    message: bounded_message(&self.context.redact_text(&error.to_string())),
                    status: None,
                    value_digest: None,
                    result: Some(event.operation.empty_result()),
                    partial,
                };
                self.journal
                    .after(&event, &outcome)
                    .map_err(|message| ModuleError::invalid("JOURNAL005", message))?;
                if partial {
                    self.evidence.push(SourceOperationalEvidence::Partial {
                        operation: operation.to_owned(),
                        detail: outcome.message.clone(),
                    });
                }
                Err(error)
            }
        }
    }

    fn invoke_inner(
        &mut self,
        module: &str,
        operation: &str,
        arguments: Vec<Value>,
    ) -> Result<Value, ModuleError> {
        match (module, operation) {
            ("data", "toml_decode") => Ok(data::toml_decode(bytes(&arguments, 0)?)?),
            ("data", "get") => {
                let keys = string_list(&arguments, 1)?;
                Ok(data::get(
                    argument(&arguments, 0)?,
                    &keys.iter().map(String::as_str).collect::<Vec<_>>(),
                )?
                .clone())
            }
            ("data", "json_encode") => {
                Ok(Value::bytes(data::json_encode(argument(&arguments, 0)?)?))
            }
            ("path", "normalize") => Ok(path_value(path::normalize(value_path(argument(
                &arguments, 0,
            )?)?)?)),
            ("path", "join") => Ok(path_value(path::join(
                value_path(argument(&arguments, 0)?)?,
                [string(&arguments, 1)?],
            )?)),
            ("path", "contained") => Ok(path_value(path::contained(
                value_path(argument(&arguments, 0)?)?,
                value_path(argument(&arguments, 1)?)?,
            )?)),
            ("filesystem", "read") => {
                let target = value_path(argument(&arguments, 0)?)?.to_path_buf();
                let maximum = usize_value(&arguments, 1)?;
                let request = CapabilityRequest::filesystem_read(self.manifest.root())
                    .map_err(invalid_request)?;
                let descriptor =
                    filesystem_descriptor("std::filesystem::read", self.manifest.root(), &target)?;
                self.effect(request, "std::filesystem::read", descriptor, |this| {
                    let value = filesystem::read(
                        this.context,
                        this.effects,
                        this.platform,
                        this.adapter,
                        this.manifest.root(),
                        this.manifest.root(),
                        &target,
                        maximum,
                        &mut this.read_budget,
                    )?;
                    let digest = integrity::sha256(&value);
                    let resolved = if target.is_absolute() {
                        path::normalize(&target)
                    } else {
                        path::normalize(&this.manifest.root().join(&target))
                    }?;
                    if let Some(expected) = this.planned_inputs.get(&resolved)
                        && expected != &digest
                    {
                        return Err(ModuleError::invalid(
                            "EXECUTE_STALE",
                            "bound input bytes changed before use",
                        ));
                    }
                    let bytes = u64::try_from(value.len()).map_err(|_| {
                        ModuleError::invalid("OPERATION005", "filesystem byte count overflow")
                    })?;
                    Ok((
                        Value::bytes(value),
                        None,
                        SourceEffectResult::Filesystem {
                            bytes: Some(bytes),
                            evidence_digest: Some(digest),
                        },
                    ))
                })
            }
            ("filesystem", "write_atomic") => {
                let target = value_path(argument(&arguments, 0)?)?.to_path_buf();
                let payload = bytes(&arguments, 1)?.to_vec();
                let request = CapabilityRequest::filesystem_write(self.manifest.evidence())
                    .map_err(invalid_request)?;
                let descriptor = filesystem_descriptor(
                    "std::filesystem::write_atomic",
                    self.manifest.root(),
                    &target,
                )?;
                self.effect(
                    request,
                    "std::filesystem::write_atomic",
                    descriptor,
                    |this| {
                        filesystem::write_atomic(
                            this.context,
                            this.effects,
                            this.platform,
                            this.adapter,
                            this.manifest.evidence(),
                            this.manifest.root(),
                            &target,
                            &payload,
                        )?;
                        let bytes = u64::try_from(payload.len()).map_err(|_| {
                            ModuleError::invalid("OPERATION005", "filesystem byte count overflow")
                        })?;
                        Ok((
                            Value::Null,
                            None,
                            SourceEffectResult::Filesystem {
                                bytes: Some(bytes),
                                evidence_digest: Some(integrity::sha256(&payload)),
                            },
                        ))
                    },
                )
            }
            ("time", "wall_now") => {
                let request = CapabilityRequest::clock_wall();
                let descriptor = SourceOperation::Clock {
                    operation: "std::time::wall_now".to_owned(),
                    clock: SourceClock::Wall,
                };
                self.effect(request, "std::time::wall_now", descriptor, |this| {
                    let value =
                        time::wall_now(this.context, this.effects, this.platform, this.adapter)?
                            .to_string();
                    let digest = integrity::sha256(value.as_bytes());
                    Ok((
                        nominal_record("time", "Timestamp", vec![("_value", Value::string(value))]),
                        None,
                        SourceEffectResult::Clock {
                            evidence_digest: Some(digest),
                        },
                    ))
                })
            }
            ("time", "monotonic_now") => {
                let request = CapabilityRequest::clock_monotonic();
                let descriptor = SourceOperation::Clock {
                    operation: "std::time::monotonic_now".to_owned(),
                    clock: SourceClock::Monotonic,
                };
                self.effect(request, "std::time::monotonic_now", descriptor, |this| {
                    let value = time::monotonic_now(
                        this.context,
                        this.effects,
                        this.platform,
                        this.adapter,
                    )?
                    .as_nanos();
                    let value = i64::try_from(value).map_err(|_| {
                        ModuleError::invalid("TIME002", "monotonic instant exceeds Int")
                    })?;
                    Ok((
                        Value::Int(value),
                        None,
                        SourceEffectResult::Clock {
                            evidence_digest: None,
                        },
                    ))
                })
            }
            ("version", "parse") => version::Version::parse(string(&arguments, 0)?)
                .map(|value| {
                    nominal_record(
                        "version",
                        "Version",
                        vec![("_value", Value::string(value.render()))],
                    )
                })
                .map_err(|error| ModuleError::invalid("VERSION001", error.to_string())),
            ("version", "render") => {
                version::Version::parse(nominal_string(&arguments, 0, "version", "Version")?)
                    .map(|value| Value::string(value.render()))
                    .map_err(|error| ModuleError::invalid("VERSION001", error.to_string()))
            }
            ("version", "matches") => {
                let range = version::VersionRange::parse(string(&arguments, 0)?)
                    .map_err(|error| ModuleError::invalid("VERSION002", error.to_string()))?;
                let value =
                    version::Version::parse(nominal_string(&arguments, 1, "version", "Version")?)
                        .map_err(|error| ModuleError::invalid("VERSION001", error.to_string()))?;
                Ok(Value::Bool(range.matches(&value)))
            }
            ("integrity", "sha256") => Ok(Value::string(integrity::sha256(bytes(&arguments, 0)?))),
            ("url", "parse") => url::Url::parse(string(&arguments, 0)?)
                .map(|value| {
                    nominal_record(
                        "url",
                        "Url",
                        vec![("_value", Value::string(value.render()))],
                    )
                })
                .map_err(|error| ModuleError::invalid("URL001", error.to_string())),
            ("url", "render") => url::Url::parse(nominal_string(&arguments, 0, "url", "Url")?)
                .map(|value| Value::string(value.render()))
                .map_err(|error| ModuleError::invalid("URL001", error.to_string())),
            ("process", "run") => {
                let tool = project_identity(&arguments, 0, "tools", "ToolIdentity")?.to_owned();
                let argv = string_list(&arguments, 1)?;
                let request =
                    CapabilityRequest::process_run(tool.clone()).map_err(invalid_request)?;
                let descriptor = process_descriptor(self.tools, &tool, &argv)?;
                self.effect(request, "std::process::run", descriptor, |this| {
                    let refs = argv.iter().map(String::as_str).collect::<Vec<_>>();
                    let executable_file = this.executable_files.get(&tool).ok_or_else(|| {
                        ModuleError::invalid(
                            "EXECUTE_STALE",
                            "accepted tool executable identity is absent",
                        )
                    })?;
                    let result = process::run_retained(
                        this.context,
                        this.effects,
                        this.platform,
                        this.adapter,
                        this.tools,
                        &tool,
                        &refs,
                        this.manifest.root(),
                        super::MAX_PROCESS_OUTPUT_BYTES,
                        super::MAX_PROCESS_OUTPUT_BYTES,
                        process::MAX_PROCESS_DURATION,
                        &mut this.process_budget,
                        executable_file,
                    )?;
                    let status = result.status().clone();
                    let digest = integrity::sha256(&[result.stdout(), result.stderr()].concat());
                    let stdout_bytes = u64::try_from(result.stdout().len()).map_err(|_| {
                        ModuleError::invalid("OPERATION005", "process stdout byte count overflow")
                    })?;
                    let stderr_bytes = u64::try_from(result.stderr().len()).map_err(|_| {
                        ModuleError::invalid("OPERATION005", "process stderr byte count overflow")
                    })?;
                    let value = nominal_record(
                        "process",
                        "ToolResult",
                        vec![
                            ("status", Value::Status(status.clone())),
                            ("stdout", Value::bytes(result.stdout().to_vec())),
                            ("stderr", Value::bytes(result.stderr().to_vec())),
                        ],
                    );
                    Ok((
                        value,
                        Some(status.clone()),
                        SourceEffectResult::Process {
                            status: status.code(),
                            stdout_bytes: Some(stdout_bytes),
                            stderr_bytes: Some(stderr_bytes),
                            evidence_digest: Some(digest),
                        },
                    ))
                })
            }
            ("http", "secret_header") => {
                let name = string(&arguments, 0)?.to_owned();
                let secret =
                    project_identity(&arguments, 1, "secrets", "SecretIdentity")?.to_owned();
                if !self.manifest.secrets().contains_key(&secret) {
                    return Err(ModuleError::invalid("HTTP003", "unknown project secret"));
                }
                if self
                    .secret_headers
                    .values()
                    .any(|(_, existing)| existing == &secret)
                {
                    return Err(ModuleError::invalid(
                        "EXECUTE008",
                        "a secret header for this injected secret already exists",
                    ));
                }
                if self.secret_headers.len() == crate::security::MAX_SECRETS {
                    return Err(ModuleError::invalid(
                        "EXECUTE008",
                        "secret-header handles exceed the secret limit",
                    ));
                }
                self.next_secret_header =
                    self.next_secret_header.checked_add(1).ok_or_else(|| {
                        ModuleError::invalid("HTTP003", "secret-header identity overflow")
                    })?;
                let handle = self.next_secret_header;
                self.secret_headers.insert(handle, (name, secret));
                Ok(nominal_record(
                    "http",
                    "SecretHeader",
                    vec![(
                        "_handle",
                        Value::Int(i64::try_from(handle).unwrap_or(i64::MAX)),
                    )],
                ))
            }
            ("http", "request") => {
                let endpoint_id =
                    project_identity(&arguments, 0, "endpoints", "EndpointIdentity")?.to_owned();
                let method = string(&arguments, 1)?.to_owned();
                let endpoint =
                    self.manifest.endpoints().get(&endpoint_id).ok_or_else(|| {
                        ModuleError::invalid("HTTP012", "unknown project endpoint")
                    })?;
                let ordinary_headers = http_headers(argument(&arguments, 2)?)?;
                let secret = match argument(&arguments, 3)? {
                    Value::Null => None,
                    value => {
                        let handle = secret_header_handle(value)?;
                        let (name, secret) =
                            self.secret_headers.remove(&handle).ok_or_else(|| {
                                ModuleError::invalid(
                                    "HTTP014",
                                    "secret header is missing or was already consumed",
                                )
                            })?;
                        let secret_id = crate::security::SecretId::new(secret)
                            .map_err(|error| ModuleError::invalid("HTTP003", error.to_string()))?;
                        let reveal = CapabilityRequest::secret_reveal(
                            secret_id.as_str(),
                            &endpoint_id,
                            &name,
                        )
                        .map_err(invalid_request)?;
                        Some((
                            http::secret_header(endpoint, &name, secret_id.clone())?,
                            reveal,
                            secret_id,
                        ))
                    }
                };
                let body = optional_bytes(argument(&arguments, 4)?)?;
                let maximum = usize_value(&arguments, 5)?;
                let request = CapabilityRequest::network_http(endpoint_id.clone(), method.clone())
                    .map_err(invalid_request)?;
                let descriptor = SourceOperation::Http {
                    operation: "std::http::request".to_owned(),
                    endpoint: endpoint_id.clone(),
                    method: method.clone(),
                };
                let ca = self.tls_ca.get(&endpoint_id).map(Vec::as_slice);
                self.effect(request, "std::http::request", descriptor, |this| {
                    let response = match secret {
                        Some((secret, reveal, secret_id)) => {
                            let (sink_endpoint, header) = match reveal.scope() {
                                crate::authority::CapabilityScope::SecretSink {
                                    endpoint,
                                    header,
                                    ..
                                } => (endpoint.clone(), header.clone()),
                                _ => unreachable!("secret reveal has one closed scope"),
                            };
                            let descriptor = SourceOperation::SecretReveal {
                                operation: "std::http::secret_reveal".to_owned(),
                                endpoint: sink_endpoint,
                                header,
                            };
                            this.effect(reveal, "std::http::secret_reveal", descriptor, |this| {
                                this.materialize_secret(&secret_id)?;
                                let response = http::request(
                                    this.context,
                                    this.effects,
                                    this.platform,
                                    this.adapter,
                                    endpoint,
                                    &method,
                                    &ordinary_headers,
                                    Some(secret),
                                    &body,
                                    maximum,
                                    ca,
                                    http::MAX_HTTP_DURATION,
                                    &mut this.http_budget,
                                )?;
                                let digest = integrity::sha256(response.body());
                                Ok((
                                    response,
                                    None,
                                    SourceEffectResult::SecretReveal {
                                        evidence_digest: Some(digest),
                                    },
                                ))
                            })?
                        }
                        None => http::request(
                            this.context,
                            this.effects,
                            this.platform,
                            this.adapter,
                            endpoint,
                            &method,
                            &ordinary_headers,
                            None,
                            &body,
                            maximum,
                            ca,
                            http::MAX_HTTP_DURATION,
                            &mut this.http_budget,
                        )?,
                    };
                    let digest = integrity::sha256(response.body());
                    let status = response.status();
                    let body_bytes = u64::try_from(response.body().len()).map_err(|_| {
                        ModuleError::invalid("OPERATION005", "HTTP body byte count overflow")
                    })?;
                    let headers = Record::new(
                        response
                            .headers()
                            .iter()
                            .map(|header| {
                                (
                                    header.name().to_owned(),
                                    Value::bytes(header.value().to_vec()),
                                )
                            })
                            .collect(),
                    )
                    .map_err(|error| ModuleError::invalid("HTTP015", error.to_string()))?;
                    let value = nominal_record(
                        "http",
                        "HttpResponse",
                        vec![
                            ("status", Value::Int(i64::from(response.status()))),
                            ("headers", Value::Record(headers)),
                            ("body", Value::bytes(response.body().to_vec())),
                        ],
                    );
                    Ok((
                        value,
                        None,
                        SourceEffectResult::Http {
                            status: Some(status),
                            body_bytes: Some(body_bytes),
                            evidence_digest: Some(digest),
                        },
                    ))
                })
            }
            _ => Err(ModuleError::invalid(
                "OPERATION001",
                format!("unknown controlled operation `std::{module}::{operation}`"),
            )),
        }
    }
}

impl SourceOperationalHost for ControlledSourceOperations<'_> {
    fn action_start(&mut self, action: &ActionId) -> Result<(), ModuleError> {
        let node = self.action_nodes.get(action).cloned().ok_or_else(|| {
            ModuleError::invalid("JOURNAL006", "action is absent from the accepted plan")
        })?;
        self.journal
            .action_start(&node, action)
            .map_err(|message| ModuleError::invalid("JOURNAL005", message))?;
        self.action_stack.push((action.clone(), false));
        Ok(())
    }

    fn action_end(
        &mut self,
        action: &ActionId,
        mut outcome: SourceActionOutcome,
    ) -> Result<(), ModuleError> {
        let Some((active, attempted)) = self.action_stack.pop() else {
            return Err(ModuleError::invalid(
                "JOURNAL006",
                "action end has no matching start",
            ));
        };
        if &active != action {
            return Err(ModuleError::invalid(
                "JOURNAL006",
                "action boundaries are not properly nested",
            ));
        }
        if attempted && outcome.0.class != "success" {
            outcome.0.partial = true;
        }
        outcome.0.message = bounded_message(&self.context.redact_text(&outcome.0.message));
        let unused_secret = self.action_stack.is_empty()
            && (self.secret_input.is_some() || !self.secret_headers.is_empty())
            && outcome.0.class == "success";
        if unused_secret {
            outcome = SourceActionOutcome::new(
                "refused",
                "EXECUTE_UNUSED_SECRET",
                "the injected secret was not consumed by its declared sink",
                None,
                attempted,
            );
        }
        let node = self.action_nodes.get(action).ok_or_else(|| {
            ModuleError::invalid("JOURNAL006", "action is absent from the accepted plan")
        })?;
        self.journal
            .action_end(node, &outcome)
            .map_err(|message| ModuleError::invalid("JOURNAL005", message))?;
        if unused_secret {
            return Err(ModuleError::invalid(
                "EXECUTE_UNUSED_SECRET",
                "the injected secret was not consumed by its declared sink",
            ));
        }
        Ok(())
    }

    fn invoke(
        &mut self,
        module: &ModuleId,
        operation: &str,
        arguments: Vec<Value>,
    ) -> Result<Value, ModuleError> {
        let ModuleOrigin::Standard { namespace, module } = module.origin() else {
            return Err(ModuleError::invalid(
                "OPERATION001",
                "operation is not standard",
            ));
        };
        if namespace != "std" {
            return Err(ModuleError::invalid(
                "OPERATION001",
                "operation is not in std",
            ));
        }
        self.invoke_inner(module, operation, arguments)
    }

    fn take_evidence(&mut self) -> Vec<SourceOperationalEvidence> {
        std::mem::take(&mut self.evidence)
    }
}

fn filesystem_descriptor(
    operation: &str,
    root: &Path,
    target: &Path,
) -> Result<SourceOperation, ModuleError> {
    let root = path::normalize(root)?;
    let target = path::contained(&root, target)?;
    let relative_path = target
        .strip_prefix(&root)
        .map_err(|_| ModuleError::invalid("FS004", "filesystem target escapes project root"))?
        .to_path_buf();
    Ok(SourceOperation::Filesystem {
        operation: operation.to_owned(),
        relative_path,
    })
}

fn process_descriptor(
    tools: &ToolLock,
    tool_id: &str,
    arguments: &[String],
) -> Result<SourceOperation, ModuleError> {
    let tool = tools.tools().get(tool_id).ok_or_else(|| {
        ModuleError::invalid("PROCESS004", format!("unknown locked tool `{tool_id}`"))
    })?;
    let mut native_argv = Vec::with_capacity(arguments.len() + 1);
    native_argv.push(process::locked_path(tool).into_os_string());
    native_argv.extend(arguments.iter().map(OsString::from));
    let argv_count = u64::try_from(native_argv.len())
        .map_err(|_| ModuleError::invalid("OPERATION005", "process argv count overflow"))?;
    let argv_bytes = native_argv.iter().try_fold(0_u64, |total, argument| {
        let bytes = u64::try_from(argument.as_os_str().as_bytes().len())
            .map_err(|_| ModuleError::invalid("OPERATION005", "process argv byte overflow"))?;
        total
            .checked_add(bytes)
            .ok_or_else(|| ModuleError::invalid("OPERATION005", "process argv byte overflow"))
    })?;
    let argv_digest = digest_native_argv(&native_argv)?;
    let argv = native_argv
        .iter()
        .map(|argument| {
            let bytes = argument.as_os_str().as_bytes();
            Ok(SourceProcessArgument::Redacted {
                bytes: u64::try_from(bytes.len()).map_err(|_| {
                    ModuleError::invalid("OPERATION005", "process argument byte overflow")
                })?,
                digest: integrity::sha256(bytes),
            })
        })
        .collect::<Result<Vec<_>, ModuleError>>()?;
    Ok(SourceOperation::Process {
        operation: "std::process::run".to_owned(),
        tool: tool_id.to_owned(),
        argv_count,
        argv_bytes,
        argv_digest,
        argv,
    })
}

/// Build the descriptor for one maintained adapter-owned version probe.
pub fn maintained_probe_operation(
    tools: &ToolLock,
    tool_id: &str,
) -> Result<SourceOperation, ModuleError> {
    let tool = tools.tools().get(tool_id).ok_or_else(|| {
        ModuleError::invalid("PROCESS004", format!("unknown locked tool `{tool_id}`"))
    })?;
    let fixed = match tool.adapter() {
        MaintainedAdapter::Git => &["--version"][..],
        MaintainedAdapter::Cargo => &["--version", "--verbose"][..],
    };
    let mut native_argv = Vec::with_capacity(fixed.len() + 1);
    native_argv.push(process::locked_path(tool).into_os_string());
    native_argv.extend(fixed.iter().map(OsString::from));
    let argv_count = u64::try_from(native_argv.len())
        .map_err(|_| ModuleError::invalid("OPERATION005", "process argv count overflow"))?;
    let argv_bytes = native_argv.iter().try_fold(0_u64, |total, argument| {
        let bytes = u64::try_from(argument.as_os_str().as_bytes().len())
            .map_err(|_| ModuleError::invalid("OPERATION005", "process argv byte overflow"))?;
        total
            .checked_add(bytes)
            .ok_or_else(|| ModuleError::invalid("OPERATION005", "process argv byte overflow"))
    })?;
    let executable = native_argv[0].as_os_str().as_bytes();
    let mut argv = Vec::with_capacity(native_argv.len());
    argv.push(SourceProcessArgument::Redacted {
        bytes: u64::try_from(executable.len())
            .map_err(|_| ModuleError::invalid("OPERATION005", "process argument overflow"))?,
        digest: integrity::sha256(executable),
    });
    argv.extend(
        fixed
            .iter()
            .map(|argument| SourceProcessArgument::Public((*argument).to_owned())),
    );
    Ok(SourceOperation::Process {
        operation: "std::process::probe".to_owned(),
        tool: tool_id.to_owned(),
        argv_count,
        argv_bytes,
        argv_digest: digest_native_argv(&native_argv)?,
        argv,
    })
}

fn digest_native_argv(arguments: &[OsString]) -> Result<String, ModuleError> {
    let mut canonical = Vec::new();
    canonical.extend_from_slice(
        &u64::try_from(arguments.len())
            .map_err(|_| ModuleError::invalid("OPERATION005", "process argv count overflow"))?
            .to_be_bytes(),
    );
    for argument in arguments {
        let bytes = argument.as_os_str().as_bytes();
        canonical.extend_from_slice(
            &u64::try_from(bytes.len())
                .map_err(|_| ModuleError::invalid("OPERATION005", "process argv byte overflow"))?
                .to_be_bytes(),
        );
        canonical.extend_from_slice(bytes);
    }
    Ok(integrity::sha256(&canonical))
}

fn argument(arguments: &[Value], index: usize) -> Result<&Value, ModuleError> {
    arguments.get(index).ok_or_else(|| {
        ModuleError::invalid("OPERATION001", "controlled operation has invalid arity")
    })
}

fn string(arguments: &[Value], index: usize) -> Result<&str, ModuleError> {
    match argument(arguments, index)? {
        Value::String(value) => Ok(value),
        value => Err(type_error(index, "String", value)),
    }
}

fn project_identity<'a>(
    arguments: &'a [Value],
    index: usize,
    module: &str,
    type_name: &str,
) -> Result<&'a str, ModuleError> {
    let value = argument(arguments, index)?;
    let Value::NominalRecord(identity) = value else {
        return Err(type_error(index, "declarative project identity", value));
    };
    if identity.id() != &NominalTypeId::project(module, type_name) {
        return Err(type_error(index, "declarative project identity", value));
    }
    match identity.get("_id") {
        Some(Value::String(value)) => Ok(value),
        _ => Err(ModuleError::invalid(
            "OPERATION001",
            "invalid declarative project identity carrier",
        )),
    }
}

fn nominal_string<'a>(
    arguments: &'a [Value],
    index: usize,
    module: &str,
    type_name: &str,
) -> Result<&'a str, ModuleError> {
    let value = argument(arguments, index)?;
    let Value::NominalRecord(record) = value else {
        return Err(type_error(index, type_name, value));
    };
    if record.id() != &NominalTypeId::standard(module, type_name) {
        return Err(type_error(index, type_name, value));
    }
    match record.get("_value") {
        Some(Value::String(value)) => Ok(value),
        _ => Err(ModuleError::invalid(
            "OPERATION001",
            format!("invalid {type_name} carrier"),
        )),
    }
}

fn bytes(arguments: &[Value], index: usize) -> Result<&[u8], ModuleError> {
    match argument(arguments, index)? {
        Value::Bytes(value) => Ok(value),
        value => Err(type_error(index, "Bytes", value)),
    }
}

fn optional_bytes(value: &Value) -> Result<Vec<u8>, ModuleError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Bytes(value) => Ok(value.to_vec()),
        other => Err(type_error(4, "Null or Bytes", other)),
    }
}

fn usize_value(arguments: &[Value], index: usize) -> Result<usize, ModuleError> {
    match argument(arguments, index)? {
        Value::Int(value) => usize::try_from(*value)
            .map_err(|_| ModuleError::invalid("OPERATION001", "size must be non-negative")),
        value => Err(type_error(index, "Int", value)),
    }
}

fn string_list(arguments: &[Value], index: usize) -> Result<Vec<String>, ModuleError> {
    let Value::List(values) = argument(arguments, index)? else {
        return Err(type_error(
            index,
            "List[String]",
            argument(arguments, index)?,
        ));
    };
    values
        .iter()
        .enumerate()
        .map(|(item, value)| match value {
            Value::String(value) => Ok(value.to_string()),
            other => Err(type_error(item, "String", other)),
        })
        .collect()
}

fn value_path(value: &Value) -> Result<&Path, ModuleError> {
    match value {
        Value::Path(value) => Ok(Path::new(value.as_os_str())),
        other => Err(type_error(0, "Path", other)),
    }
}

fn bounded_message(message: &str) -> String {
    if message.len() <= MAX_JOURNAL_MESSAGE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_JOURNAL_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = message[..end].to_owned();
    bounded.push('…');
    bounded
}

fn path_value(value: PathBuf) -> Value {
    Value::Path(NativePath::new(value.into_os_string()))
}

fn http_headers(value: &Value) -> Result<Vec<HttpHeader>, ModuleError> {
    let Value::Record(record) = value else {
        return Err(type_error(2, "Record", value));
    };
    record
        .entries()
        .iter()
        .map(|(name, value)| {
            let bytes = match value {
                Value::String(value) => value.as_bytes().to_vec(),
                Value::Bytes(value) => value.to_vec(),
                other => return Err(type_error(2, "String or Bytes header", other)),
            };
            HttpHeader::new(name.to_string(), bytes).map_err(ModuleError::from)
        })
        .collect()
}

fn secret_header_handle(value: &Value) -> Result<u64, ModuleError> {
    let Value::NominalRecord(value) = value else {
        return Err(type_error(3, "SecretHeader", value));
    };
    if value.id() != &NominalTypeId::standard("http", "SecretHeader") {
        return Err(type_error(
            3,
            "SecretHeader",
            &Value::NominalRecord(value.clone()),
        ));
    }
    match value.get("_handle") {
        Some(Value::Int(value)) => u64::try_from(*value)
            .map_err(|_| ModuleError::invalid("HTTP014", "invalid secret-header handle")),
        _ => Err(ModuleError::invalid(
            "HTTP014",
            "invalid secret-header handle",
        )),
    }
}

fn nominal_record(module: &str, name: &str, fields: Vec<(&str, Value)>) -> Value {
    Value::NominalRecord(Box::new(NominalRecordValue::new(
        NominalTypeId::standard(module, name),
        Vec::new(),
        fields
            .into_iter()
            .map(|(name, value)| (Arc::from(name), value))
            .collect(),
    )))
}

fn invalid_request(error: crate::authority::CapabilityRequestError) -> ModuleError {
    ModuleError::invalid("OPERATION001", error.to_string())
}

fn type_error(index: usize, expected: &str, actual: &Value) -> ModuleError {
    ModuleError::invalid(
        "OPERATION001",
        format!(
            "controlled argument {index} requires {expected}, found {}",
            actual.family_name()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Record, Table, VariantValue};

    fn project_identity() -> Value {
        Value::NominalRecord(Box::new(NominalRecordValue::new(
            NominalTypeId::project("tools", "ToolIdentity"),
            Vec::new(),
            vec![(Arc::from("_id"), Value::String(Arc::from("git")))],
        )))
    }

    #[test]
    fn control_carriers_remain_opaque_through_every_structured_container() {
        let identity = project_identity();
        assert!(contains_control_carrier(&identity));
        assert!(contains_control_carrier(&Value::List(Arc::from([
            identity.clone()
        ]))));
        assert!(contains_control_carrier(&Value::Record(
            Record::new(vec![("identity".to_owned(), identity.clone())]).unwrap()
        )));
        assert!(contains_control_carrier(&Value::NominalRecord(Box::new(
            NominalRecordValue::new(
                NominalTypeId::standard("version", "Version"),
                Vec::new(),
                vec![(Arc::from("nested"), identity.clone())],
            )
        ))));
        assert!(contains_control_carrier(&Value::Variant(Box::new(
            VariantValue::new(
                NominalTypeId::standard("test", "Wrapper"),
                Vec::new(),
                "Some",
                vec![identity.clone()],
            )
        ))));
        assert!(contains_control_carrier(&Value::from(
            Table::new(vec!["identity".to_owned()], vec![vec![identity]]).unwrap()
        )));
    }

    #[test]
    fn ordinary_operational_nominals_are_not_control_carriers() {
        let timestamp = nominal_record(
            "time",
            "Timestamp",
            vec![("value", Value::String(Arc::from("1970-01-01T00:00:00Z")))],
        );
        assert!(!contains_control_carrier(&timestamp));
    }
}
