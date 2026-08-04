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

use crate::{FiniteFloat, Range, Value};

/// A pure-operation failure, reported without a source span.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum OperationError {
    /// An operator received operand families it is not defined for.
    UnsupportedOperands {
        operator: &'static str,
        operands: Vec<&'static str>,
    },
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
        _ => Err(unsupported(".", [target])),
    }
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
