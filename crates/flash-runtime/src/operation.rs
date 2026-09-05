//! Source-independent pure operations over [`Value`].
//!
//! These functions implement the postfix, unary, and binary expression
//! operators. They never touch source spans: every failure is an
//! [`OperationError`] kind that the evaluator later anchors to a span and stack
//! frame.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::eval::{FrameCallee, RuntimeError};
use crate::module::{ModuleId, ModuleOrigin, ValueType, substitute_type};
use crate::seam::DownstreamCallMetadata;
use crate::stream::{
    CheckedStreamPull, StreamCleanupFailure, StreamContractViolation, ValueStream,
};
use crate::{FiniteFloat, Range, Record, Value};

/// The stable identity of one compiled, qualified Flash 2 operation.
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
    /// An ordinary immutable Flash value.
    Value(ValueType),
    /// A lazy, single-consumer stream whose items have the declared type.
    ValueStream(ValueType),
}

/// One member of a descriptor's validated overload set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationOverload {
    input: OperationInputType,
    result: ValueType,
}

impl OperationOverload {
    fn new(input: OperationInputType, result: ValueType) -> Self {
        Self { input, result }
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

    /// Opaque attachment points reserved for later execution/project owners.
    #[must_use]
    pub const fn downstream(&self) -> &DownstreamCallMetadata {
        &self.downstream
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
                    format!("[{}]", self.type_parameters.join(", "))
                };
                format!(
                    "{}{}(input: {}) -> {}",
                    self.id.qualified_name(),
                    generics,
                    overload.input(),
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
            let input = match &overload.input {
                OperationInputType::Value(input) | OperationInputType::ValueStream(input) => input,
            };
            validate_type_parameters(input, &parameters)?;
            validate_type_parameters(&overload.result, &parameters)?;
        }
        for (index, left) in self.overloads.iter().enumerate() {
            for right in &self.overloads[index + 1..] {
                let overlap = match (&left.input, &right.input) {
                    (OperationInputType::Value(left), OperationInputType::Value(right))
                    | (
                        OperationInputType::ValueStream(left),
                        OperationInputType::ValueStream(right),
                    ) => types_overlap(left, right),
                    _ => false,
                };
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
        }
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
enum StandardOperation {
    Length,
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
    if namespace != "std" || standard != "value" || name != "length" {
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
    ["length"]
        .into_iter()
        .filter_map(|name| standard_operation(module, name))
        .collect()
}

/// A pure-operation failure, reported without a source span.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum OperationError {
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
    /// A collection length cannot be represented by Flash's signed `Int`.
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

fn source_span_value(source: &str, span: flash_syntax::Span) -> Value {
    Value::Record(
        Record::new(source_span_fields(source, span)).expect("source location fields are distinct"),
    )
}

fn source_span_fields(source: &str, span: flash_syntax::Span) -> Vec<(String, Value)> {
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
