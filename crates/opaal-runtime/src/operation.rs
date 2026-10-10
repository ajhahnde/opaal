//! Source-independent pure operations over [`Value`].
//!
//! These functions implement the postfix, unary, and binary expression
//! operators. They never touch source spans: every failure is an
//! [`OperationError`] kind that the evaluator later anchors to a span and stack
//! frame.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::eval::{FrameCallee, RuntimeError};
use crate::module::{ModuleId, ModuleOrigin, NominalTypeId, ValueType, substitute_type};
use crate::seam::DownstreamCallMetadata;
use crate::stream::{
    CheckedStreamPull, StreamCleanupFailure, StreamContractViolation, ValueStream,
};
use crate::{FiniteFloat, Range, Record, Value};

/// The stable identity of one compiled, qualified OPAAL operation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationId {
    module: ModuleId,
    name: String,
}

impl OperationId {
    fn new(module: ModuleId, name: impl Into<String>) -> Self {
        Self {
            module,
            name: name.into(),
        }
    }

    /// The canonical compiled module exporting this operation.
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    /// The exported operation name within its module.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The canonical display spelling, independent of a caller's local alias.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::{}", self.module.path().display(), self.name)
    }
}

/// The carrier accepted by one operation overload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationInputType {
    /// An ordinary immutable OPAAL value.
    Value(ValueType),
    /// A lazy, single-consumer stream whose items have the declared type.
    ValueStream(ValueType),
}

/// One member of a descriptor's validated overload set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationOverload {
    input: OperationInputType,
    parameters: Vec<OperationParameter>,
    result: ValueType,
}

/// One named argument in a compiled operation's ordinary call signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationParameter {
    name: String,
    input: OperationInputType,
    callback: Option<OperationCallback>,
}

/// A pure callback's parameter/result relation over descriptor type parameters.
/// This is metadata over Function and Closure, not a source callable type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationCallback {
    parameters: Vec<ValueType>,
    result: ValueType,
}

impl OperationCallback {
    #[must_use]
    pub fn parameters(&self) -> &[ValueType] {
        &self.parameters
    }

    #[must_use]
    pub const fn result(&self) -> &ValueType {
        &self.result
    }
}

/// Available binding metadata, shared by source checking and runtime preflight.
#[derive(Clone)]
pub(crate) struct CallbackShape {
    pub(crate) action: bool,
    pub(crate) generics: Vec<crate::module::ResolvedTypeParameter>,
    pub(crate) parameters: Vec<ValueType>,
    pub(crate) result: ValueType,
}

impl CallbackShape {
    pub(crate) fn infer_operation(
        &self,
        relation: &OperationCallback,
        substitutions: &mut std::collections::BTreeMap<String, ValueType>,
    ) -> Result<(), String> {
        if self.action {
            return Err(
                "callbacks require a function or closure; actions are not accepted".to_owned(),
            );
        }
        if self.parameters.len() != relation.parameters.len() {
            return Err(format!(
                "callback expects {} arguments; operation supplies {}",
                self.parameters.len(),
                relation.parameters.len()
            ));
        }
        for (expected, actual) in relation
            .parameters
            .iter()
            .zip(&self.parameters)
            .chain(std::iter::once((&relation.result, &self.result)))
        {
            if !self.contains_own_generic(actual)
                && !crate::module::unify_type(expected, actual, substitutions)
            {
                return Err(format!(
                    "callback type `{actual}` conflicts with `{expected}`"
                ));
            }
        }
        Ok(())
    }

    fn contains_own_generic(&self, value_type: &ValueType) -> bool {
        match value_type {
            ValueType::TypeParameter(name) => self
                .generics
                .iter()
                .any(|parameter| parameter.name() == name),
            ValueType::List(element) => self.contains_own_generic(element),
            ValueType::Nominal { arguments, .. } => arguments
                .iter()
                .any(|argument| self.contains_own_generic(argument)),
            _ => false,
        }
    }

    pub(crate) fn instantiate(
        &self,
        relation: &OperationCallback,
        operation_types: &std::collections::BTreeMap<String, ValueType>,
        satisfies: impl Fn(&ValueType, opaal_syntax::TypeConstraint) -> bool,
    ) -> Result<Vec<ValueType>, String> {
        if self.action {
            return Err(
                "callbacks require a function or closure; actions are not accepted".to_owned(),
            );
        }
        if self.parameters.len() != relation.parameters.len() {
            return Err(format!(
                "callback expects {} arguments; operation supplies {}",
                self.parameters.len(),
                relation.parameters.len()
            ));
        }
        let inputs = relation
            .parameters
            .iter()
            .map(|input| substitute_type(input, operation_types))
            .collect::<Vec<_>>();
        let output = substitute_type(&relation.result, operation_types);
        let mut own_types = std::collections::BTreeMap::new();
        for (parameter, supplied) in self
            .parameters
            .iter()
            .zip(&inputs)
            .chain(std::iter::once((&self.result, &output)))
        {
            if !crate::module::unify_type(parameter, supplied, &mut own_types) {
                return Err(format!(
                    "callback type `{parameter}` conflicts with operation type `{supplied}`"
                ));
            }
        }
        let mut arguments = Vec::new();
        for generic in &self.generics {
            let Some(actual) = own_types.get(generic.name()) else {
                return Err(format!(
                    "cannot infer callback type `{}`; use a concrete typed wrapper",
                    generic.name()
                ));
            };
            if generic
                .constraints()
                .iter()
                .any(|constraint| !satisfies(actual, *constraint))
            {
                return Err(format!(
                    "callback type `{actual}` fails constraints for `{}`",
                    generic.name()
                ));
            }
            arguments.push(actual.clone());
        }
        for (parameter, supplied) in self.parameters.iter().zip(&inputs) {
            let parameter = substitute_type(parameter, &own_types);
            if !parameter.accepts_type(supplied) {
                return Err(format!(
                    "callback parameter `{parameter}` cannot accept `{supplied}`"
                ));
            }
        }
        let result = substitute_type(&self.result, &own_types);
        if result != ValueType::Any && !output.accepts_type(&result) {
            return Err(format!(
                "callback result `{result}` does not fit `{output}`"
            ));
        }
        Ok(arguments)
    }
}

impl OperationParameter {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn input(&self) -> &OperationInputType {
        &self.input
    }

    #[must_use]
    pub const fn callback(&self) -> Option<&OperationCallback> {
        self.callback.as_ref()
    }
}

impl OperationOverload {
    fn new(input: OperationInputType, result: ValueType) -> Self {
        Self {
            parameters: vec![OperationParameter {
                name: "input".to_owned(),
                input: input.clone(),
                callback: None,
            }],
            input,
            result,
        }
    }

    fn values(parameters: Vec<(&str, ValueType)>, result: ValueType) -> Self {
        let parameters = parameters
            .into_iter()
            .map(|(name, value_type)| OperationParameter {
                name: name.to_owned(),
                input: OperationInputType::Value(value_type),
                callback: None,
            })
            .collect::<Vec<_>>();
        Self {
            input: parameters
                .first()
                .map_or(OperationInputType::Value(ValueType::Null), |parameter| {
                    parameter.input.clone()
                }),
            parameters,
            result,
        }
    }

    /// Every argument, in source call order, including the first carrier.
    #[must_use]
    pub fn parameters(&self) -> &[OperationParameter] {
        &self.parameters
    }

    /// The first-parameter carrier and type accepted by this overload.
    #[must_use]
    pub const fn input(&self) -> &OperationInputType {
        &self.input
    }

    /// The value type produced by this overload.
    #[must_use]
    pub const fn result(&self) -> &ValueType {
        &self.result
    }
}

/// A compiled standard operation's complete host-free semantic descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationDescriptor {
    id: OperationId,
    type_parameters: Vec<String>,
    overloads: Vec<OperationOverload>,
    documentation: String,
    purity: OperationPurity,
    downstream: DownstreamCallMetadata,
    implementation: StandardOperation,
}

/// Whether a compiled operation can run without a later authority contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPurity {
    /// The operation uses only language-owned data and its explicit budget.
    Pure,
    /// The operation must refuse until an explicit authority owner is present.
    RequiresAuthorityContract,
}

impl OperationDescriptor {
    /// The stable operation identity shared by analysis, help, and execution.
    #[must_use]
    pub const fn id(&self) -> &OperationId {
        &self.id
    }

    /// Invariant generic parameters used by the overload set.
    #[must_use]
    pub fn type_parameters(&self) -> &[String] {
        &self.type_parameters
    }

    /// Constraints shared by checking, execution and signature observers.
    #[must_use]
    pub fn type_parameter_constraints(&self, name: &str) -> &[opaal_syntax::TypeConstraint] {
        if matches!(
            (self.implementation, name),
            (StandardOperation::Sort, "T") | (StandardOperation::SortBy, "K")
        ) {
            &[opaal_syntax::TypeConstraint::Ordered]
        } else {
            &[]
        }
    }

    #[must_use]
    pub fn type_parameter_labels(&self) -> Vec<String> {
        self.type_parameters
            .iter()
            .map(|name| {
                if self.type_parameter_constraints(name).is_empty() {
                    name.clone()
                } else {
                    format!("{name}: Ordered")
                }
            })
            .collect()
    }

    /// The complete, construction-validated overload set.
    #[must_use]
    pub fn overloads(&self) -> &[OperationOverload] {
        &self.overloads
    }

    /// Stable help text shipped with the compiled descriptor.
    #[must_use]
    pub fn documentation(&self) -> &str {
        &self.documentation
    }

    /// The descriptor-owned purity classification.
    #[must_use]
    pub const fn purity(&self) -> OperationPurity {
        self.purity
    }

    /// Empty operational metadata plus later-owned execution/project slots.
    #[must_use]
    pub const fn downstream(&self) -> &DownstreamCallMetadata {
        &self.downstream
    }

    pub(crate) const fn implementation(&self) -> StandardOperation {
        self.implementation
    }

    /// A contextual inference template exists only for a single value overload.
    /// Multiple scalar overloads must wait for argument evidence.
    pub(crate) fn inference_overload(&self) -> Option<&OperationOverload> {
        if self.implementation.is_math() {
            return None;
        }
        let mut values = self
            .overloads
            .iter()
            .filter(|overload| matches!(overload.input(), OperationInputType::Value(_)));
        let first = values.next()?;
        values.next().is_none().then_some(first)
    }

    pub(crate) fn call_arity(&self) -> usize {
        self.overloads[0].parameters.len()
    }

    /// Filter complete tuples, retaining candidates where static evidence is
    /// unavailable. Expected results never select a numeric input family.
    pub(crate) fn resolve_call<'a>(
        &'a self,
        arguments: &[Option<OperationInputType>],
        substitutions: &BTreeMap<String, ValueType>,
    ) -> Result<Vec<&'a OperationOverload>, (usize, OperationInputType)> {
        let mut candidates = self
            .overloads
            .iter()
            .filter(|overload| overload.parameters.len() == arguments.len())
            .collect::<Vec<_>>();
        for (index, actual) in arguments.iter().enumerate() {
            let Some(actual) = actual else { continue };
            let expected = candidates
                .first()
                .map(|overload| overload.parameters[index].input.clone());
            candidates.retain(|overload| {
                let expected = &overload.parameters[index].input;
                match (expected, actual) {
                    (OperationInputType::Value(expected), OperationInputType::Value(actual))
                    | (
                        OperationInputType::ValueStream(expected),
                        OperationInputType::ValueStream(actual),
                    ) => call_types_compatible(&substitute_type(expected, substitutions), actual),
                    _ => false,
                }
            });
            if candidates.is_empty() {
                return Err((index, expected.unwrap_or_else(|| actual.clone())));
            }
        }
        Ok(candidates)
    }

    pub(crate) fn call_result(
        candidates: &[&OperationOverload],
        substitutions: &BTreeMap<String, ValueType>,
    ) -> ValueType {
        let Some(first) = candidates.first() else {
            return ValueType::Any;
        };
        let result = substitute_type(first.result(), substitutions);
        if candidates
            .iter()
            .all(|candidate| substitute_type(candidate.result(), substitutions) == result)
        {
            result
        } else {
            ValueType::Any
        }
    }

    /// Only existing unary operations admit the implicit value-pipeline form.
    #[must_use]
    pub fn supports_value_pipeline(&self) -> bool {
        self.implementation == StandardOperation::Length
    }

    /// Canonical callable labels for every carrier overload in descriptor order.
    #[must_use]
    pub fn signature_labels(&self) -> Vec<String> {
        self.overloads
            .iter()
            .map(|overload| {
                let generics = if self.type_parameters.is_empty() {
                    String::new()
                } else {
                    format!("[{}]", self.type_parameter_labels().join(", "))
                };
                format!(
                    "{}{}({}) -> {}",
                    self.id.qualified_name(),
                    generics,
                    overload
                        .parameters()
                        .iter()
                        .map(|parameter| {
                            if let Some(callback) = parameter.callback() {
                                format!(
                                    "{}: Callable({}) -> {}",
                                    parameter.name(),
                                    callback
                                        .parameters()
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    callback.result()
                                )
                            } else {
                                format!("{}: {}", parameter.name(), parameter.input())
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    overload.result(),
                )
            })
            .collect()
    }

    /// Validates the descriptor as one indivisible overload set.
    pub fn validate(&self) -> Result<(), OperationDescriptorError> {
        if self.overloads.is_empty() {
            return Err(OperationDescriptorError::EmptyOverloadSet);
        }
        let mut parameters = std::collections::BTreeSet::new();
        for parameter in &self.type_parameters {
            if !parameters.insert(parameter.as_str()) {
                return Err(OperationDescriptorError::DuplicateTypeParameter {
                    name: parameter.clone(),
                });
            }
        }
        for overload in &self.overloads {
            for parameter in overload.parameters() {
                let input = match parameter.input() {
                    OperationInputType::Value(input) | OperationInputType::ValueStream(input) => {
                        input
                    }
                };
                validate_type_parameters(input, &parameters)?;
                if let Some(callback) = parameter.callback() {
                    for input in callback.parameters() {
                        validate_type_parameters(input, &parameters)?;
                    }
                    validate_type_parameters(callback.result(), &parameters)?;
                }
            }
            validate_type_parameters(&overload.result, &parameters)?;
        }
        for (index, left) in self.overloads.iter().enumerate() {
            for right in &self.overloads[index + 1..] {
                let overlap = left.parameters.len() == right.parameters.len()
                    && left
                        .parameters
                        .iter()
                        .zip(&right.parameters)
                        .all(|(left, right)| match (&left.input, &right.input) {
                            (OperationInputType::Value(left), OperationInputType::Value(right))
                            | (
                                OperationInputType::ValueStream(left),
                                OperationInputType::ValueStream(right),
                            ) => types_overlap(left, right),
                            _ => false,
                        });
                if overlap {
                    return Err(OperationDescriptorError::OverlappingOverloads);
                }
            }
        }
        Ok(())
    }

    /// Calls the value overload without mapping or carrier conversion.
    pub fn execute_value(&self, value: Value) -> Result<Value, OperationError> {
        self.execute_value_with_types(value, &[])
    }

    /// Calls the value overload with an exact explicit generic instantiation.
    pub fn execute_value_with_types(
        &self,
        value: Value,
        type_arguments: &[ValueType],
    ) -> Result<Value, OperationError> {
        if !type_arguments.is_empty() && type_arguments.len() != self.type_parameters.len() {
            return Err(OperationError::GenericArity {
                operation: self.id.qualified_name(),
                expected: self.type_parameters.len(),
                actual: type_arguments.len(),
            });
        }
        let substitutions = self
            .type_parameters
            .iter()
            .zip(type_arguments)
            .map(|(parameter, argument)| (parameter.clone(), argument.clone()))
            .collect();
        let Some(overload) = self.overloads.iter().find(|overload| {
            matches!(&overload.input, OperationInputType::Value(expected)
                if substitute_type(expected, &substitutions).accepts(&value))
        }) else {
            return Err(OperationError::NoMatchingOverload {
                operation: self.id.qualified_name(),
                input: format!("Value({})", value.family_name()),
            });
        };
        if self.implementation != StandardOperation::Length {
            return Err(OperationError::HostContextRequired {
                operation: "budgeted pure operation",
            });
        }
        debug_assert_eq!(overload.result, ValueType::Int);
        match self.implementation {
            StandardOperation::Length => match value {
                Value::List(values) => i64::try_from(values.len()).map(Value::Int).map_err(|_| {
                    OperationError::LengthOverflow {
                        operation: self.id.qualified_name(),
                    }
                }),
                _ => unreachable!("the selected value overload accepts only lists"),
            },
            _ => unreachable!("budgeted operations use the shared evaluator"),
        }
    }

    /// Calls the stream overload under an exact item budget.
    #[must_use]
    pub fn execute_value_stream(
        &self,
        mut stream: ValueStream,
        item_limit: usize,
    ) -> OperationStreamOutcome {
        if !self
            .overloads
            .iter()
            .any(|overload| matches!(overload.input, OperationInputType::ValueStream(_)))
        {
            return finish_stream_operation(
                &mut stream,
                OperationStreamPrimary::Rejected(OperationError::NoMatchingOverload {
                    operation: self.id.qualified_name(),
                    input: "ValueStream".to_owned(),
                }),
                0,
            );
        }
        match self.implementation {
            StandardOperation::Length => {
                let mut delivered_items = 0_usize;
                let primary = loop {
                    match stream.pull_checked() {
                        CheckedStreamPull::Item(_) if delivered_items == item_limit => {
                            break OperationStreamPrimary::LimitExceeded { limit: item_limit };
                        }
                        CheckedStreamPull::Item(_) => delivered_items += 1,
                        CheckedStreamPull::End => {
                            let value =
                                i64::try_from(delivered_items).map(Value::Int).map_err(|_| {
                                    OperationError::LengthOverflow {
                                        operation: self.id.qualified_name(),
                                    }
                                });
                            break match value {
                                Ok(value) => OperationStreamPrimary::Value(value),
                                Err(error) => OperationStreamPrimary::Rejected(error),
                            };
                        }
                        CheckedStreamPull::Failed(error) => {
                            break OperationStreamPrimary::Failed(error);
                        }
                        CheckedStreamPull::Cancelled(reason) => {
                            break OperationStreamPrimary::Cancelled(reason);
                        }
                        CheckedStreamPull::ContractViolation(violation) => {
                            break OperationStreamPrimary::ContractViolation(violation);
                        }
                    }
                };
                finish_stream_operation(&mut stream, primary, delivered_items)
            }
            _ => unreachable!("only length has a stream overload"),
        }
    }
}

fn call_types_compatible(expected: &ValueType, actual: &ValueType) -> bool {
    match (expected, actual) {
        (ValueType::Any | ValueType::TypeParameter(_), _)
        | (_, ValueType::Any | ValueType::TypeParameter(_)) => true,
        (ValueType::List(expected), ValueType::List(actual)) => {
            call_types_compatible(expected, actual)
        }
        _ => expected.accepts_type(actual),
    }
}

impl fmt::Display for OperationInputType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(value_type) => value_type.fmt(formatter),
            Self::ValueStream(value_type) => write!(formatter, "ValueStream[{value_type}]"),
        }
    }
}

/// The single primary selected by a bounded stream operation.
#[derive(Debug)]
pub enum OperationStreamPrimary {
    Value(Value),
    LimitExceeded { limit: usize },
    Failed(RuntimeError),
    Cancelled(crate::eval::CancelReason),
    ContractViolation(StreamContractViolation),
    Rejected(OperationError),
    CleanupFailed(StreamCleanupFailure),
}

/// One stream-operation primary plus delivered-prefix and cleanup evidence.
#[derive(Debug)]
pub struct OperationStreamOutcome {
    primary: OperationStreamPrimary,
    delivered_items: usize,
    cleanup_failure: Option<StreamCleanupFailure>,
}

impl OperationStreamOutcome {
    /// The sole terminal result of the stream operation.
    #[must_use]
    pub const fn primary(&self) -> &OperationStreamPrimary {
        &self.primary
    }

    /// Items accepted before the terminal result was established.
    #[must_use]
    pub const fn delivered_items(&self) -> usize {
        self.delivered_items
    }

    /// Cleanup evidence retained beside a pre-existing primary.
    #[must_use]
    pub const fn cleanup_failure(&self) -> Option<&StreamCleanupFailure> {
        self.cleanup_failure.as_ref()
    }
}

fn finish_stream_operation(
    stream: &mut ValueStream,
    mut primary: OperationStreamPrimary,
    delivered_items: usize,
) -> OperationStreamOutcome {
    let mut cleanup_failure = stream.close().err();
    if matches!(primary, OperationStreamPrimary::Value(_))
        && let Some(failure) = cleanup_failure.take()
    {
        primary = OperationStreamPrimary::CleanupFailed(failure);
    }
    OperationStreamOutcome {
        primary,
        delivered_items,
        cleanup_failure,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StandardOperation {
    RandomInt,
    RandomFloat,
    RandomBytes,
    Io,
    Abs,
    Min,
    Max,
    Clamp,
    Floor,
    Ceil,
    Round,
    Sqrt,
    Length,
    Trim,
    Split,
    Join,
    Contains,
    StartsWith,
    EndsWith,
    Replace,
    DecodeUtf8,
    Map,
    Filter,
    Fold,
    Any,
    All,
    Count,
    Find,
    Sort,
    SortBy,
    Take,
    Drop,
    Reverse,
    RecordKeys,
    RecordHas,
    RecordGetOr,
    RecordSelect,
    RecordSet,
    RecordMerge,
    TomlDecode,
    DataGet,
    JsonEncode,
    JsonDecode,
}

impl StandardOperation {
    pub(crate) fn is_math(self) -> bool {
        matches!(
            self,
            Self::Abs
                | Self::Min
                | Self::Max
                | Self::Clamp
                | Self::Floor
                | Self::Ceil
                | Self::Round
                | Self::Sqrt
        )
    }
}

/// A compiled operation descriptor that cannot enter the standard manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationDescriptorError {
    EmptyOverloadSet,
    DuplicateTypeParameter { name: String },
    UnknownTypeParameter { name: String },
    OverlappingOverloads,
}

impl fmt::Display for OperationDescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyOverloadSet => formatter.write_str("operation has no overloads"),
            Self::DuplicateTypeParameter { name } => {
                write!(formatter, "operation repeats type parameter `{name}`")
            }
            Self::UnknownTypeParameter { name } => {
                write!(
                    formatter,
                    "operation references undeclared type parameter `{name}`"
                )
            }
            Self::OverlappingOverloads => {
                formatter.write_str("operation has overlapping unifiable overloads")
            }
        }
    }
}

impl Error for OperationDescriptorError {}

fn validate_type_parameters(
    value_type: &ValueType,
    declared: &std::collections::BTreeSet<&str>,
) -> Result<(), OperationDescriptorError> {
    match value_type {
        ValueType::TypeParameter(name) if !declared.contains(name.as_str()) => {
            Err(OperationDescriptorError::UnknownTypeParameter { name: name.clone() })
        }
        ValueType::List(element) => validate_type_parameters(element, declared),
        ValueType::Nominal { arguments, .. } => {
            for argument in arguments {
                validate_type_parameters(argument, declared)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn types_overlap(left: &ValueType, right: &ValueType) -> bool {
    match (left, right) {
        (ValueType::Any | ValueType::TypeParameter(_), _)
        | (_, ValueType::Any | ValueType::TypeParameter(_)) => true,
        (ValueType::List(left), ValueType::List(right)) => types_overlap(left, right),
        (
            ValueType::Nominal {
                id: left_id,
                arguments: left_arguments,
            },
            ValueType::Nominal {
                id: right_id,
                arguments: right_arguments,
            },
        ) => {
            left_id == right_id
                && left_arguments.len() == right_arguments.len()
                && left_arguments
                    .iter()
                    .zip(right_arguments)
                    .all(|(left, right)| types_overlap(left, right))
        }
        _ => left == right,
    }
}

/// Resolves one exported operation from the closed compiled-standard manifest.
#[must_use]
pub fn standard_operation(module: &ModuleId, name: &str) -> Option<OperationDescriptor> {
    let ModuleOrigin::Standard {
        namespace,
        module: standard,
    } = module.origin()
    else {
        return None;
    };
    if namespace != "std" {
        return None;
    }
    if standard == "random" {
        let (implementation, parameters, result, documentation) = match name {
            "int" => (
                StandardOperation::RandomInt,
                vec![("min", ValueType::Int), ("max", ValueType::Int)],
                ValueType::Int,
                "Return an unbiased Int in [min, max). The interval must be nonempty; width one draws no entropy.",
            ),
            "float" => (
                StandardOperation::RandomFloat,
                vec![],
                ValueType::Float,
                "Return a finite Float k / 2^53 in [0, 1), including zero and excluding one.",
            ),
            "bytes" => (
                StandardOperation::RandomBytes,
                vec![("count", ValueType::Int)],
                ValueType::Bytes,
                "Return exactly count system random bytes. Count must be nonnegative and at most 1048576; zero draws no entropy.",
            ),
            _ => return None,
        };
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: Vec::new(),
            overloads: vec![OperationOverload::values(parameters, result)],
            documentation: format!(
                "{documentation} Requires entropy.system in evaluation scope, including no-draw calls, and an explicitly bound cancellable host. Pure functions, callbacks, initializers and default embeddings refuse. Calls share evaluation work and byte limits; errors and cancellation return no partial value. No seed, generator state, fallback or implicit conversion; no pipeline form."
            ),
            purity: OperationPurity::RequiresAuthorityContract,
            downstream: DownstreamCallMetadata::foundation()
                .with_declared_request(crate::authority::CapabilityRequest::entropy_system()),
            implementation,
        };
        descriptor
            .validate()
            .expect("compiled random descriptors must be valid");
        return Some(descriptor);
    }
    if standard == "io" {
        use crate::authority::CapabilityRequest;
        let (parameter, argument_type, result, request, documentation) = match name {
            "read_stdin" => (
                "max_bytes",
                ValueType::Int,
                ValueType::Bytes,
                CapabilityRequest::stdin_read(),
                "Read raw stdin through EOF within an explicit cap from 0 through 1048576 bytes. Reserve cap+1 for the excess probe; excess raises IO002 without a truncated value.",
            ),
            "print" | "println" => (
                "text",
                ValueType::String,
                ValueType::Null,
                CapabilityRequest::stdout_write(),
                "Write exact UTF-8 text to stdout. println appends exactly one LF, even to empty or already LF-terminated text; print adds nothing.",
            ),
            "eprint" | "eprintln" => (
                "text",
                ValueType::String,
                ValueType::Null,
                CapabilityRequest::stderr_write(),
                "Write exact UTF-8 text to stderr. eprintln appends exactly one LF, even to empty or already LF-terminated text; eprint adds nothing.",
            ),
            "write_stdout" => (
                "bytes",
                ValueType::Bytes,
                ValueType::Null,
                CapabilityRequest::stdout_write(),
                "Write exact raw bytes to stdout, with no added newline. String is rejected before transfer.",
            ),
            "write_stderr" => (
                "bytes",
                ValueType::Bytes,
                ValueType::Null,
                CapabilityRequest::stderr_write(),
                "Write exact raw bytes to stderr, with no added newline. String is rejected before transfer.",
            ),
            _ => return None,
        };
        let effect = match request.effect() {
            opaal_platform::AuthorityEffect::StdinRead => "stdin.read",
            opaal_platform::AuthorityEffect::StdoutWrite => "stdout.write",
            opaal_platform::AuthorityEffect::StderrWrite => "stderr.write",
            _ => unreachable!("I/O descriptors request only standard streams"),
        };
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: Vec::new(),
            overloads: vec![OperationOverload::values(
                vec![(parameter, argument_type)],
                result,
            )],
            documentation: format!(
                "{documentation} Requires {effect} in evaluation scope, an exact grant and an explicitly bound cancellable endpoint, including empty calls. Pure functions, callbacks, initializers and default embeddings refuse. Writes preflight at most 1048576 encoded bytes including LF; calls share entropy/input/output byte and work limits. Broken Pipe raises IO003 and zero-write IO004. Failed or cancelled reads return no partial value; consumed/emitted or uncertain bytes cannot be rolled back or automatically replayed. No formatting, coercion, capture, handles or pipeline form."
            ),
            purity: OperationPurity::RequiresAuthorityContract,
            downstream: DownstreamCallMetadata::foundation().with_declared_request(request),
            implementation: StandardOperation::Io,
        };
        descriptor
            .validate()
            .expect("compiled I/O descriptors must be valid");
        return Some(descriptor);
    }
    if standard == "math" {
        use StandardOperation::{Abs, Ceil, Clamp, Floor, Max, Min, Round, Sqrt};
        let (implementation, names, homogeneous, documentation) = match name {
            "abs" => (
                Abs,
                vec!["value"],
                true,
                "Return the absolute value, preserving Int or Float. Int minimum raises integer overflow.",
            ),
            "min" => (
                Min,
                vec!["left", "right"],
                true,
                "Return the smaller value; equal inputs select left.",
            ),
            "max" => (
                Max,
                vec!["left", "right"],
                true,
                "Return the larger value; equal inputs select left.",
            ),
            "clamp" => (
                Clamp,
                vec!["value", "min", "max"],
                true,
                "Clamp to inclusive bounds; equal bounds are valid. Reversed bounds raise an operation Error.",
            ),
            "floor" => (
                Floor,
                vec!["value"],
                false,
                "Return the largest integral Float at or below the input; no Int conversion.",
            ),
            "ceil" => (
                Ceil,
                vec!["value"],
                false,
                "Return the smallest integral Float at or above the input; no Int conversion.",
            ),
            "round" => (
                Round,
                vec!["value"],
                false,
                "Round to the nearest integral Float, with ties away from zero.",
            ),
            "sqrt" => (
                Sqrt,
                vec!["value"],
                false,
                "Return the correctly rounded binary64 square root (nearest, ties to even). Negative inputs raise an operation Error.",
            ),
            _ => return None,
        };
        let families = if homogeneous {
            vec![ValueType::Int, ValueType::Float]
        } else {
            vec![ValueType::Float]
        };
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: Vec::new(),
            overloads: families
                .into_iter()
                .map(|family| {
                    OperationOverload::values(
                        names.iter().map(|name| (*name, family.clone())).collect(),
                        family,
                    )
                })
                .collect(),
            documentation: format!(
                "{documentation} All arguments must match one declared numeric family; no implicit conversion. Float results are finite with positive zero. Pure calls share caller work budgets and cancellation; no method or pipeline form."
            ),
            purity: OperationPurity::Pure,
            downstream: DownstreamCallMetadata::foundation(),
            implementation,
        };
        descriptor
            .validate()
            .expect("compiled math descriptors must be valid");
        return Some(descriptor);
    }
    if standard == "list" {
        use ValueType::{Any, Bool, Int, List, TypeParameter};
        let t = TypeParameter("T".to_owned());
        let (implementation, generics, parameters, callback, result, documentation) = match name {
            "map" => {
                let u = TypeParameter("U".to_owned());
                (
                    StandardOperation::Map,
                    vec!["T", "U"],
                    vec![("input", List(Box::new(t.clone()))), ("transform", Any)],
                    Some(OperationCallback {
                        parameters: vec![t],
                        result: u.clone(),
                    }),
                    List(Box::new(u)),
                    "Apply a pure callback once per item in source order; preserve result values without flattening.",
                )
            }
            "filter" => (
                StandardOperation::Filter,
                vec!["T"],
                vec![("input", List(Box::new(t.clone()))), ("predicate", Any)],
                Some(OperationCallback {
                    parameters: vec![t.clone()],
                    result: Bool,
                }),
                List(Box::new(t)),
                "Retain items in source order when a pure callback returns Bool true.",
            ),
            "fold" => {
                let a = TypeParameter("A".to_owned());
                (
                    StandardOperation::Fold,
                    vec!["T", "A"],
                    vec![
                        ("input", List(Box::new(t.clone()))),
                        ("initial", a.clone()),
                        ("combine", Any),
                    ],
                    Some(OperationCallback {
                        parameters: vec![a.clone(), t],
                        result: a.clone(),
                    }),
                    a,
                    "Left fold with a pure callback receiving accumulator then item; empty input returns initial.",
                )
            }
            "any" | "all" | "count" | "find" => {
                let (implementation, result, documentation) = match name {
                    "any" => (
                        StandardOperation::Any,
                        Bool,
                        "Return true at the first true predicate; empty input returns false.",
                    ),
                    "all" => (
                        StandardOperation::All,
                        Bool,
                        "Return false at the first false predicate; empty input returns true.",
                    ),
                    "count" => (
                        StandardOperation::Count,
                        Int,
                        "Count true predicate results with checked Int arithmetic; empty input returns zero.",
                    ),
                    _ => (
                        StandardOperation::Find,
                        ValueType::Nominal {
                            id: Box::new(NominalTypeId::standard("outcome", "Option")),
                            arguments: vec![t.clone()],
                        },
                        "Return the first matching original item as std::outcome::Option::Some; empty or unmatched input returns None, distinct from Some(null).",
                    ),
                };
                (
                    implementation,
                    vec!["T"],
                    vec![("input", List(Box::new(t.clone()))), ("predicate", Any)],
                    Some(OperationCallback {
                        parameters: vec![t],
                        result: Bool,
                    }),
                    result,
                    documentation,
                )
            }
            "sort_by" => {
                let k = TypeParameter("K".to_owned());
                (
                    StandardOperation::SortBy,
                    vec!["T", "K"],
                    vec![("input", List(Box::new(t.clone()))), ("key", Any)],
                    Some(OperationCallback {
                        parameters: vec![t.clone()],
                        result: k,
                    }),
                    List(Box::new(t)),
                    "Compute one Ordered key per item in source order, then sort stably in ascending order; equal keys retain source order.",
                )
            }
            "sort" | "reverse" => {
                let (implementation, documentation) = if name == "sort" {
                    (
                        StandardOperation::Sort,
                        "Sort homogeneous Ordered items stably in ascending order.",
                    )
                } else {
                    (
                        StandardOperation::Reverse,
                        "Return all items in reverse order, retaining duplicates; this reverses equal-key runs in a sorted input.",
                    )
                };
                (
                    implementation,
                    vec!["T"],
                    vec![("input", List(Box::new(t.clone())))],
                    None,
                    List(Box::new(t)),
                    documentation,
                )
            }
            "take" | "drop" => {
                let (implementation, documentation) = if name == "take" {
                    (
                        StandardOperation::Take,
                        "Return the prefix of min(count, length) items; negative count is an Error.",
                    )
                } else {
                    (
                        StandardOperation::Drop,
                        "Return the suffix after min(count, length) items; negative count is an Error.",
                    )
                };
                (
                    implementation,
                    vec!["T"],
                    vec![("input", List(Box::new(t.clone()))), ("count", Int)],
                    None,
                    List(Box::new(t)),
                    documentation,
                )
            }
            _ => return None,
        };
        let mut overload = OperationOverload::values(parameters, result);
        overload
            .parameters
            .last_mut()
            .expect("callback parameter")
            .callback = callback;
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: generics.into_iter().map(str::to_owned).collect(),
            overloads: vec![overload],
            documentation: format!(
                "{documentation} Validate types and any callback kind, arity and annotations even on empty input; share caller budgets and cancellation; stop on first failure."
            ),
            purity: OperationPurity::Pure,
            downstream: DownstreamCallMetadata::foundation(),
            implementation,
        };
        descriptor
            .validate()
            .expect("compiled list descriptors must be valid");
        return Some(descriptor);
    }
    if standard == "data" {
        use StandardOperation::{DataGet, JsonDecode, JsonEncode, TomlDecode};
        use ValueType::{Any, Bytes, List, String as Text};
        let (implementation, parameters, result, documentation) = match name {
            "toml_decode" => (
                TomlDecode,
                vec![("input", Bytes)],
                Any,
                "Decode UTF8 TOML with the existing 16 MiB/depth64 limits, errors and per-call accounting; datetime conversion remains explicit.",
            ),
            "get" => (
                DataGet,
                vec![("input", Any), ("keys", List(Box::new(Text)))],
                Any,
                "Strictly traverse a multi-key structural Record path; an empty path returns the input. Preserve existing errors and per-call accounting.",
            ),
            "json_encode" => (
                JsonEncode,
                vec![("input", Any)],
                Bytes,
                "Encode canonical compact JSON with sorted object keys, existing 16 MiB/depth64 limits, errors and per-call accounting; recursively refuse secret/control carriers and unsupported values.",
            ),
            "json_decode" => (
                JsonDecode,
                vec![("input", Bytes)],
                Any,
                "Decode one strict UTF8 JSON document, at most 8 MiB/depth64. Reject duplicate keys and nonfinite numbers; retain array and object source order. Integer tokens fitting i64 become Int; other finite numbers become Float with possible precision loss. Share caller allocation, work and cancellation budgets; errors contain bounded escaped keys or byte offsets, never the document. Ordinary values have no automatic secret taint.",
            ),
            _ => return None,
        };
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: Vec::new(),
            overloads: vec![OperationOverload::values(parameters, result)],
            documentation: documentation.to_owned(),
            purity: OperationPurity::Pure,
            downstream: DownstreamCallMetadata::foundation(),
            implementation,
        };
        descriptor
            .validate()
            .expect("compiled data descriptors must be valid");
        return Some(descriptor);
    }
    if standard == "record" {
        use StandardOperation::{
            RecordGetOr, RecordHas, RecordKeys, RecordMerge, RecordSelect, RecordSet,
        };
        use ValueType::{Any, Bool, List, Record, String as Text};
        let (implementation, parameters, result, documentation) = match name {
            "keys" => (
                RecordKeys,
                vec![("input", Record)],
                List(Box::new(Text)),
                "Return keys in retained field order.",
            ),
            "has" => (
                RecordHas,
                vec![("input", Record), ("key", Text)],
                Bool,
                "Test exact key presence, including fields whose value is null.",
            ),
            "get_or" => (
                RecordGetOr,
                vec![("input", Record), ("key", Text), ("default", Any)],
                Any,
                "Return a field's value or the default only for absence; the default is evaluated eagerly like every ordinary argument.",
            ),
            "select" => (
                RecordSelect,
                vec![("input", Record), ("keys", List(Box::new(Text)))],
                Record,
                "Return fields in requested order; missing or repeated requested keys are errors; an empty request returns an empty Record.",
            ),
            "set" => (
                RecordSet,
                vec![("input", Record), ("key", Text), ("value", Any)],
                Record,
                "Replace a field keeping its position, or append a new field; preserve the input.",
            ),
            "merge" => (
                RecordMerge,
                vec![("left", Record), ("right", Record)],
                Record,
                "Merge with right values winning; keep replaced left positions and append new right fields in right order; preserve both inputs.",
            ),
            _ => return None,
        };
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: Vec::new(),
            overloads: vec![OperationOverload::values(parameters, result)],
            documentation: format!(
                "{documentation} Accept structural Records only; nominal records retain their schemas. Share caller budgets and cancellation. Ordinary keys and values have no automatic secret taint."
            ),
            purity: OperationPurity::Pure,
            downstream: DownstreamCallMetadata::foundation(),
            implementation,
        };
        descriptor
            .validate()
            .expect("compiled Record descriptors must be valid");
        return Some(descriptor);
    }
    if standard == "string" {
        use StandardOperation::{
            Contains, DecodeUtf8, EndsWith, Join, Replace, Split, StartsWith, Trim,
        };
        use ValueType::{Bool, Bytes, List, String as Text};
        let (implementation, parameters, result, documentation) = match name {
            "trim" => (
                Trim,
                vec![("input", Text)],
                Text,
                "Remove leading and trailing Unicode whitespace, preserving interior text.",
            ),
            "split" => (
                Split,
                vec![("input", Text), ("separator", Text)],
                List(Box::new(Text)),
                "Split at a nonempty literal separator, retaining empty fields.",
            ),
            "join" => (
                Join,
                vec![("input", List(Box::new(Text))), ("separator", Text)],
                Text,
                "Join String items in order with a literal separator between items.",
            ),
            "contains" => (
                Contains,
                vec![("input", Text), ("pattern", Text)],
                Bool,
                "Test a literal substring; an empty pattern succeeds.",
            ),
            "starts_with" => (
                StartsWith,
                vec![("input", Text), ("prefix", Text)],
                Bool,
                "Test a literal anchored prefix; an empty prefix succeeds.",
            ),
            "ends_with" => (
                EndsWith,
                vec![("input", Text), ("suffix", Text)],
                Bool,
                "Test a literal anchored suffix; an empty suffix succeeds.",
            ),
            "replace" => (
                Replace,
                vec![("input", Text), ("pattern", Text), ("replacement", Text)],
                Text,
                "Replace nonoverlapping literal matches; an empty pattern is an error.",
            ),
            "decode_utf8" => (
                DecodeUtf8,
                vec![("input", Bytes)],
                Text,
                "Decode strict UTF8 bytes without trimming, BOM removal or lossy conversion.",
            ),
            _ => return None,
        };
        let descriptor = OperationDescriptor {
            id: OperationId::new(module.clone(), name),
            type_parameters: Vec::new(),
            overloads: vec![OperationOverload::values(parameters, result)],
            documentation: documentation.to_owned(),
            purity: OperationPurity::Pure,
            downstream: DownstreamCallMetadata::foundation(),
            implementation,
        };
        descriptor
            .validate()
            .expect("compiled String descriptors must be valid");
        return Some(descriptor);
    }
    if standard != "value" || name != "length" {
        return None;
    }
    let descriptor = OperationDescriptor {
        id: OperationId::new(module.clone(), "length"),
        type_parameters: vec!["T".to_owned()],
        overloads: vec![
            OperationOverload::new(
                OperationInputType::Value(ValueType::List(Box::new(ValueType::TypeParameter(
                    "T".to_owned(),
                )))),
                ValueType::Int,
            ),
            OperationOverload::new(
                OperationInputType::ValueStream(ValueType::TypeParameter("T".to_owned())),
                ValueType::Int,
            ),
        ],
        documentation: "Return the number of items in a list or bounded value stream.".to_owned(),
        purity: OperationPurity::Pure,
        downstream: DownstreamCallMetadata::foundation(),
        implementation: StandardOperation::Length,
    };
    descriptor
        .validate()
        .expect("the compiled std::value::length descriptor must be valid");
    Some(descriptor)
}

/// Returns the closed compiled operation catalog for one standard module.
#[must_use]
pub(crate) fn standard_operations(module: &ModuleId) -> Vec<OperationDescriptor> {
    [
        "int",
        "float",
        "bytes",
        "read_stdin",
        "print",
        "println",
        "eprint",
        "eprintln",
        "write_stdout",
        "write_stderr",
        "abs",
        "min",
        "max",
        "clamp",
        "floor",
        "ceil",
        "round",
        "sqrt",
        "length",
        "trim",
        "split",
        "join",
        "contains",
        "starts_with",
        "ends_with",
        "replace",
        "decode_utf8",
        "toml_decode",
        "get",
        "json_encode",
        "json_decode",
        "map",
        "filter",
        "fold",
        "any",
        "all",
        "count",
        "find",
        "sort",
        "sort_by",
        "take",
        "drop",
        "reverse",
        "keys",
        "has",
        "get_or",
        "select",
        "set",
        "merge",
    ]
    .into_iter()
    .filter_map(|name| standard_operation(module, name))
    .collect()
}

/// Scalar bodies run only after the descriptor's concrete tuple is selected.
pub(crate) fn numeric_operation(
    descriptor: &OperationDescriptor,
    arguments: &[Value],
) -> Result<Value, OperationError> {
    use StandardOperation::{Abs, Ceil, Clamp, Floor, Max, Min, Round, Sqrt};
    let invalid = |message: &str| OperationError::InvalidArgument {
        operation: descriptor.id().qualified_name(),
        message: message.to_owned(),
    };
    match arguments {
        [Value::Int(value)] if descriptor.implementation == Abs => value
            .checked_abs()
            .map(Value::Int)
            .ok_or(OperationError::IntegerOverflow { operator: "abs" }),
        [Value::Int(left), Value::Int(right)] => Ok(Value::Int(match descriptor.implementation {
            Min => {
                if left <= right {
                    *left
                } else {
                    *right
                }
            }
            Max => {
                if left >= right {
                    *left
                } else {
                    *right
                }
            }
            _ => unreachable!("numeric tuple validated"),
        })),
        [Value::Int(value), Value::Int(min), Value::Int(max)] => {
            if min > max {
                return Err(invalid("minimum exceeds maximum"));
            }
            Ok(Value::Int((*value).clamp(*min, *max)))
        }
        _ => {
            let mut values = [0.0; 3];
            for (slot, argument) in values.iter_mut().zip(arguments) {
                let Value::Float(value) = argument else {
                    unreachable!("numeric tuple validated")
                };
                *slot = value.get();
            }
            let result = match (descriptor.implementation, &values[..arguments.len()]) {
                (Abs, [value]) => value.abs(),
                (Min, [left, right]) => {
                    if left <= right {
                        *left
                    } else {
                        *right
                    }
                }
                (Max, [left, right]) => {
                    if left >= right {
                        *left
                    } else {
                        *right
                    }
                }
                (Clamp, [value, min, max]) => {
                    if min > max {
                        return Err(invalid("minimum exceeds maximum"));
                    }
                    value.clamp(*min, *max)
                }
                (Floor, [value]) => value.floor(),
                (Ceil, [value]) => value.ceil(),
                (Round, [value]) => value.round(),
                (Sqrt, [value]) => {
                    if *value < 0.0 {
                        return Err(invalid("square root requires a nonnegative value"));
                    }
                    value.sqrt()
                }
                _ => unreachable!("numeric tuple validated"),
            };
            FiniteFloat::new(result)
                .map(Value::Float)
                .map_err(|_| OperationError::NonFiniteFloat)
        }
    }
}

/// A pure-operation failure, reported without a source span.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum OperationError {
    /// A well-typed argument violates the operation's value contract.
    InvalidArgument { operation: String, message: String },
    /// An operator received operand families it is not defined for.
    UnsupportedOperands {
        operator: &'static str,
        operands: Vec<&'static str>,
    },
    /// An operation in the shared catalog requires the active evaluator host.
    HostContextRequired { operation: &'static str },
    /// Checked integer arithmetic overflowed the `i64` range.
    IntegerOverflow { operator: &'static str },
    /// A float operation produced a non-finite result.
    NonFiniteFloat,
    /// Integer or float division or remainder by zero.
    DivisionByZero { operator: &'static str },
    /// An `Int` index outside the valid range of a list or string.
    IndexOutOfRange { index: i64, length: usize },
    /// A negative index, which is never valid.
    NegativeIndex { index: i64 },
    /// A record key absent from string indexing.
    MissingKey { key: String },
    /// A record field absent from member access.
    MissingField { name: String },
    /// A finite float truncation that falls outside the `Int` range.
    ConversionOutOfRange { value: f64 },
    /// No declared overload accepts the exact input carrier and value family.
    NoMatchingOverload { operation: String, input: String },
    /// A collection length cannot be represented by OPAAL's signed `Int`.
    LengthOverflow { operation: String },
    /// Explicit operation type arguments do not match the descriptor arity.
    GenericArity {
        operation: String,
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for OperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument { operation, message } => {
                write!(formatter, "{operation}: {message}")
            }
            Self::UnsupportedOperands { operator, operands } => {
                write!(formatter, "operator `{operator}` is not defined for ")?;
                for (index, family) in operands.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    formatter.write_str(family)?;
                }
                Ok(())
            }
            Self::HostContextRequired { operation } => {
                write!(
                    formatter,
                    "operation `{operation}` requires evaluator host context"
                )
            }
            Self::IntegerOverflow { operator } => {
                write!(formatter, "integer `{operator}` overflowed")
            }
            Self::NonFiniteFloat => {
                formatter.write_str("float operation produced a non-finite result")
            }
            Self::DivisionByZero { operator } => write!(formatter, "`{operator}` by zero"),
            Self::IndexOutOfRange { index, length } => {
                write!(formatter, "index {index} is outside length {length}")
            }
            Self::NegativeIndex { index } => write!(formatter, "index {index} is negative"),
            Self::MissingKey { key } => write!(formatter, "no record key {key:?}"),
            Self::MissingField { name } => write!(formatter, "no record field {name:?}"),
            Self::ConversionOutOfRange { value } => {
                write!(formatter, "{value} is outside the integer range")
            }
            Self::NoMatchingOverload { operation, input } => {
                write!(
                    formatter,
                    "operation `{operation}` does not accept carrier {input}"
                )
            }
            Self::LengthOverflow { operation } => {
                write!(formatter, "operation `{operation}` result exceeds Int")
            }
            Self::GenericArity {
                operation,
                expected,
                actual,
            } => write!(
                formatter,
                "operation `{operation}` expects {expected} type arguments, found {actual}"
            ),
        }
    }
}

impl Error for OperationError {}

/// A binary numeric operand pair after promotion.
enum Numeric {
    Ints(i64, i64),
    Floats(f64, f64),
}

fn numeric_pair(
    operator: &'static str,
    left: &Value,
    right: &Value,
) -> Result<Numeric, OperationError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Ok(Numeric::Ints(*a, *b)),
        (Value::Int(a), Value::Float(b)) => Ok(Numeric::Floats(*a as f64, b.get())),
        (Value::Float(a), Value::Int(b)) => Ok(Numeric::Floats(a.get(), *b as f64)),
        (Value::Float(a), Value::Float(b)) => Ok(Numeric::Floats(a.get(), b.get())),
        _ => Err(unsupported(operator, [left, right])),
    }
}

fn unsupported<'a>(
    operator: &'static str,
    operands: impl IntoIterator<Item = &'a Value>,
) -> OperationError {
    OperationError::UnsupportedOperands {
        operator,
        operands: operands.into_iter().map(Value::family_name).collect(),
    }
}

fn float_value(value: f64) -> Result<Value, OperationError> {
    FiniteFloat::new(value)
        .map(Value::from)
        .map_err(|_| OperationError::NonFiniteFloat)
}

/// Adds two numeric values.
pub fn add(left: &Value, right: &Value) -> Result<Value, OperationError> {
    match numeric_pair("+", left, right)? {
        Numeric::Ints(a, b) => a
            .checked_add(b)
            .map(Value::Int)
            .ok_or(OperationError::IntegerOverflow { operator: "+" }),
        Numeric::Floats(a, b) => float_value(a + b),
    }
}

/// Subtracts the right numeric value from the left.
pub fn subtract(left: &Value, right: &Value) -> Result<Value, OperationError> {
    match numeric_pair("-", left, right)? {
        Numeric::Ints(a, b) => a
            .checked_sub(b)
            .map(Value::Int)
            .ok_or(OperationError::IntegerOverflow { operator: "-" }),
        Numeric::Floats(a, b) => float_value(a - b),
    }
}

/// Multiplies two numeric values.
pub fn multiply(left: &Value, right: &Value) -> Result<Value, OperationError> {
    match numeric_pair("*", left, right)? {
        Numeric::Ints(a, b) => a
            .checked_mul(b)
            .map(Value::Int)
            .ok_or(OperationError::IntegerOverflow { operator: "*" }),
        Numeric::Floats(a, b) => float_value(a * b),
    }
}

/// Divides the left numeric value by the right, flooring integer results.
pub fn divide(left: &Value, right: &Value) -> Result<Value, OperationError> {
    match numeric_pair("/", left, right)? {
        Numeric::Ints(a, b) => floored_div(a, b).map(Value::Int),
        Numeric::Floats(a, b) => {
            if b == 0.0 {
                return Err(OperationError::DivisionByZero { operator: "/" });
            }
            float_value(a / b)
        }
    }
}

/// Computes the floored remainder of the left value by the right.
pub fn remainder(left: &Value, right: &Value) -> Result<Value, OperationError> {
    match numeric_pair("%", left, right)? {
        Numeric::Ints(a, b) => floored_rem(a, b).map(Value::Int),
        Numeric::Floats(a, b) => {
            if b == 0.0 {
                return Err(OperationError::DivisionByZero { operator: "%" });
            }
            float_value(a - b * (a / b).floor())
        }
    }
}

/// Negates a numeric value.
pub fn negate(value: &Value) -> Result<Value, OperationError> {
    match value {
        Value::Int(a) => a
            .checked_neg()
            .map(Value::Int)
            .ok_or(OperationError::IntegerOverflow { operator: "-" }),
        Value::Float(a) => float_value(-a.get()),
        _ => Err(unsupported("-", [value])),
    }
}

/// Applies unary plus, which returns a numeric value unchanged.
pub fn plus(value: &Value) -> Result<Value, OperationError> {
    match value {
        Value::Int(_) | Value::Float(_) => Ok(value.clone()),
        _ => Err(unsupported("+", [value])),
    }
}

fn floored_div(a: i64, b: i64) -> Result<i64, OperationError> {
    if b == 0 {
        return Err(OperationError::DivisionByZero { operator: "/" });
    }
    let quotient = a
        .checked_div(b)
        .ok_or(OperationError::IntegerOverflow { operator: "/" })?;
    let remainder = a % b;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        quotient
            .checked_sub(1)
            .ok_or(OperationError::IntegerOverflow { operator: "/" })
    } else {
        Ok(quotient)
    }
}

fn floored_rem(a: i64, b: i64) -> Result<i64, OperationError> {
    if b == 0 {
        return Err(OperationError::DivisionByZero { operator: "%" });
    }
    // `i64::MIN % -1` is a defined `0` in Rust, so `checked_rem` only guards zero.
    let remainder = a % b;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        // |remainder| < |b|, so this addition stays within range.
        Ok(remainder + b)
    } else {
        Ok(remainder)
    }
}

/// Compares two values within the ratified ordering domains.
pub fn order(left: &Value, right: &Value) -> Result<Ordering, OperationError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::Int(a), Value::Float(b)) => Ok(compare_int_float(*a, b.get())),
        (Value::Float(a), Value::Int(b)) => Ok(compare_int_float(*b, a.get()).reverse()),
        (Value::Float(a), Value::Float(b)) => Ok(a
            .get()
            .partial_cmp(&b.get())
            .expect("finite floats are totally ordered")),
        (Value::String(a), Value::String(b)) => Ok(a.as_ref().cmp(b.as_ref())),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(a.as_ref().cmp(b.as_ref())),
        (Value::Path(a), Value::Path(b)) => Ok(a.as_os_str().cmp(b.as_os_str())),
        (Value::Duration(a), Value::Duration(b)) => Ok(a.cmp(b)),
        (Value::ByteSize(a), Value::ByteSize(b)) => Ok(a.cmp(b)),
        (Value::List(a), Value::List(b)) => order_lists(a, b),
        _ => Err(unsupported("<", [left, right])),
    }
}

fn order_lists(left: &[Value], right: &[Value]) -> Result<Ordering, OperationError> {
    for (a, b) in left.iter().zip(right.iter()) {
        match order(a, b)? {
            Ordering::Equal => {}
            other => return Ok(other),
        }
    }
    Ok(left.len().cmp(&right.len()))
}

/// Compares an integer to a finite float without a lossy cast.
fn compare_int_float(integer: i64, float: f64) -> Ordering {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if float >= TWO_POW_63 {
        return Ordering::Less; // integer < float
    }
    if float < -TWO_POW_63 {
        return Ordering::Greater; // integer > float
    }
    let truncated = float.trunc();
    let floor_int = truncated as i128;
    match i128::from(integer).cmp(&floor_int) {
        Ordering::Equal => {
            let fraction = float - truncated;
            if fraction > 0.0 {
                Ordering::Less
            } else if fraction < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    }
}

/// Returns whether the left value orders strictly before the right.
pub fn less(left: &Value, right: &Value) -> Result<Value, OperationError> {
    order(left, right).map(|ordering| Value::Bool(ordering == Ordering::Less))
}

/// Returns whether the left value orders at or before the right.
pub fn less_equal(left: &Value, right: &Value) -> Result<Value, OperationError> {
    order(left, right).map(|ordering| Value::Bool(ordering != Ordering::Greater))
}

/// Returns whether the left value orders strictly after the right.
pub fn greater(left: &Value, right: &Value) -> Result<Value, OperationError> {
    order(left, right).map(|ordering| Value::Bool(ordering == Ordering::Greater))
}

/// Returns whether the left value orders at or after the right.
pub fn greater_equal(left: &Value, right: &Value) -> Result<Value, OperationError> {
    order(left, right).map(|ordering| Value::Bool(ordering != Ordering::Less))
}

/// Returns the total equality of two values as a `Bool`.
#[must_use]
pub fn equal(left: &Value, right: &Value) -> Value {
    Value::Bool(left == right)
}

/// Returns the total inequality of two values as a `Bool`.
#[must_use]
pub fn not_equal(left: &Value, right: &Value) -> Value {
    Value::Bool(left != right)
}

/// Evaluates `element in container` membership.
pub fn member(element: &Value, container: &Value) -> Result<Value, OperationError> {
    let present = match container {
        Value::Range(span) => match element {
            Value::Int(value) => span.contains(*value),
            _ => return Err(unsupported("in", [element, container])),
        },
        Value::List(items) => items.iter().any(|item| item == element),
        Value::String(text) => match element {
            Value::String(substring) => text.contains(substring.as_ref()),
            _ => return Err(unsupported("in", [element, container])),
        },
        Value::Record(record) => match element {
            Value::String(key) => record.get(key).is_some(),
            _ => return Err(unsupported("in", [element, container])),
        },
        _ => return Err(unsupported("in", [element, container])),
    };
    Ok(Value::Bool(present))
}

/// Evaluates `target[index]`.
pub fn index(target: &Value, index: &Value) -> Result<Value, OperationError> {
    match (target, index) {
        (Value::List(items), Value::Int(position)) => {
            let position = checked_position(*position, items.len())?;
            Ok(items[position].clone())
        }
        (Value::String(text), Value::Int(position)) => {
            let count = text.chars().count();
            let position = checked_position(*position, count)?;
            let character = text
                .chars()
                .nth(position)
                .expect("checked position is within the scalar count");
            Ok(Value::String(Arc::from(character.to_string())))
        }
        (Value::Record(record), Value::String(key)) => {
            record
                .get(key)
                .cloned()
                .ok_or_else(|| OperationError::MissingKey {
                    key: key.as_ref().to_owned(),
                })
        }
        _ => Err(unsupported("[]", [target, index])),
    }
}

fn checked_position(index: i64, length: usize) -> Result<usize, OperationError> {
    if index < 0 {
        return Err(OperationError::NegativeIndex { index });
    }
    let position =
        usize::try_from(index).map_err(|_| OperationError::IndexOutOfRange { index, length })?;
    if position >= length {
        return Err(OperationError::IndexOutOfRange { index, length });
    }
    Ok(position)
}

/// Evaluates `target.name` record member access.
pub fn field(target: &Value, name: &str) -> Result<Value, OperationError> {
    match target {
        Value::Record(record) => {
            record
                .get(name)
                .cloned()
                .ok_or_else(|| OperationError::MissingField {
                    name: name.to_owned(),
                })
        }
        Value::NominalRecord(record) => {
            record
                .get(name)
                .cloned()
                .ok_or_else(|| OperationError::MissingField {
                    name: name.to_owned(),
                })
        }
        Value::Status(status) => match name {
            "code" => Ok(status.code().map_or(Value::Null, Value::Int)),
            "signal" => Ok(status.signal().map_or(Value::Null, |signal| {
                Value::Record(
                    crate::Record::new(vec![
                        (
                            "number".to_owned(),
                            signal.number().map_or(Value::Null, Value::Int),
                        ),
                        (
                            "name".to_owned(),
                            signal.name().map_or(Value::Null, Value::string),
                        ),
                    ])
                    .expect("signal field names are distinct"),
                )
            })),
            "ok" => Ok(Value::Bool(status.is_ok())),
            "stages" => Ok(Value::list(
                status.stages().iter().cloned().map(Value::Status).collect(),
            )),
            "duration" => Ok(Value::Duration(status.duration())),
            _ => Err(OperationError::MissingField {
                name: name.to_owned(),
            }),
        },
        Value::Error(error) => error_field(error, name),
        _ => Err(unsupported(".", [target])),
    }
}

fn error_field(error: &RuntimeError, name: &str) -> Result<Value, OperationError> {
    match name {
        "category" => Ok(Value::string(error.category().name())),
        "message" => Ok(Value::string(error.to_string())),
        "source" => Ok(error.source().map_or(Value::Null, |source| {
            source_span_value(source.name(), error.span())
        })),
        "labels" => Ok(Value::list(
            error
                .labels()
                .iter()
                .map(|label| {
                    let mut fields = source_span_fields(label.source().name(), label.span());
                    fields.push(("message".to_owned(), Value::string(label.message())));
                    Value::Record(Record::new(fields).expect("error label fields are distinct"))
                })
                .collect(),
        )),
        "frames" => Ok(Value::list(
            error
                .frames()
                .iter()
                .map(|frame| {
                    let mut fields = source_span_fields(frame.source().name(), frame.call_site());
                    let callee = match frame.callee() {
                        FrameCallee::Function(name) => name.as_str(),
                        FrameCallee::Closure => "<closure>",
                    };
                    fields.push(("callee".to_owned(), Value::string(callee)));
                    Value::Record(Record::new(fields).expect("error frame fields are distinct"))
                })
                .collect(),
        )),
        "cause" => Ok(error
            .cause()
            .map_or(Value::Null, |cause| Value::Error(Arc::new(cause.clone())))),
        "status" => Ok(error
            .status()
            .map_or(Value::Null, |status| Value::Status(status.clone()))),
        _ => Err(OperationError::MissingField {
            name: name.to_owned(),
        }),
    }
}

fn source_span_value(source: &str, span: opaal_syntax::Span) -> Value {
    Value::Record(
        Record::new(source_span_fields(source, span)).expect("source location fields are distinct"),
    )
}

fn source_span_fields(source: &str, span: opaal_syntax::Span) -> Vec<(String, Value)> {
    vec![
        ("name".to_owned(), Value::string(source)),
        (
            "start".to_owned(),
            Value::Int(i64::try_from(span.start()).unwrap_or(i64::MAX)),
        ),
        (
            "end".to_owned(),
            Value::Int(i64::try_from(span.end()).unwrap_or(i64::MAX)),
        ),
    ]
}

/// Builds a `Range` value from `Int` endpoints.
pub fn range(start: &Value, end: &Value, inclusive_end: bool) -> Result<Value, OperationError> {
    match (start, end) {
        (Value::Int(start), Value::Int(end)) => {
            Ok(Value::from(Range::new(*start, *end, inclusive_end)))
        }
        _ => Err(unsupported("..", [start, end])),
    }
}

/// Converts a numeric value to `Int`, truncating a finite float toward zero.
pub fn to_int(value: &Value) -> Result<Value, OperationError> {
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    match value {
        Value::Int(integer) => Ok(Value::Int(*integer)),
        Value::Float(float) => {
            let truncated = float.get().trunc();
            if !(-TWO_POW_63..TWO_POW_63).contains(&truncated) {
                Err(OperationError::ConversionOutOfRange { value: float.get() })
            } else {
                Ok(Value::Int(truncated as i64))
            }
        }
        _ => Err(unsupported("int", [value])),
    }
}

/// Converts a numeric value to `Float`, widening an integer without error.
pub fn to_float(value: &Value) -> Result<Value, OperationError> {
    match value {
        Value::Int(integer) => Ok(Value::from(
            FiniteFloat::new(*integer as f64).expect("every i64 has a finite binary64 image"),
        )),
        Value::Float(float) => Ok(Value::Float(*float)),
        _ => Err(unsupported("float", [value])),
    }
}
