//! Shared metadata and execution for expression intrinsics.

use crate::Value;
use crate::module::ValueType;
use crate::operation::{self, OperationError};

/// A built-in callable that is resolved in expression position when no lexical
/// binding with the same name is visible.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ExpressionIntrinsic {
    Env,
    Float,
    Glob,
    Int,
}

impl ExpressionIntrinsic {
    /// Every expression intrinsic in deterministic name order.
    pub const ALL: [Self; 4] = [Self::Env, Self::Float, Self::Glob, Self::Int];

    /// Resolves one exact intrinsic spelling.
    #[must_use]
    pub fn lookup(name: &str) -> Option<Self> {
        match name {
            "env" => Some(Self::Env),
            "float" => Some(Self::Float),
            "glob" => Some(Self::Glob),
            "int" => Some(Self::Int),
            _ => None,
        }
    }

    /// The exact language spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Float => "float",
            Self::Glob => "glob",
            Self::Int => "int",
        }
    }

    /// The single parameter name used by query and protocol surfaces.
    #[must_use]
    pub const fn parameter_name(self) -> &'static str {
        match self {
            Self::Env => "name",
            Self::Glob => "pattern",
            Self::Int | Self::Float => "value",
        }
    }

    /// The number of arguments accepted by this intrinsic.
    #[must_use]
    pub const fn arity(self) -> usize {
        1
    }

    /// The statically known result family.
    #[must_use]
    pub fn result_type(self) -> ValueType {
        match self {
            Self::Env => ValueType::Any,
            Self::Float => ValueType::Float,
            Self::Glob => ValueType::List(Box::new(ValueType::Path)),
            Self::Int => ValueType::Int,
        }
    }

    /// Whether one statically known input family is accepted.
    #[must_use]
    pub const fn accepts_type(self, value_type: &ValueType) -> bool {
        match self {
            Self::Env => matches!(value_type, ValueType::Any | ValueType::String),
            Self::Glob => matches!(
                value_type,
                ValueType::Any | ValueType::String | ValueType::Path
            ),
            Self::Int | Self::Float => matches!(
                value_type,
                ValueType::Any | ValueType::Int | ValueType::Float
            ),
        }
    }

    /// The input-family text shared by static diagnostics and editor help.
    #[must_use]
    pub const fn parameter_type_label(self) -> &'static str {
        match self {
            Self::Env => "String",
            Self::Glob => "String | Path",
            Self::Int | Self::Float => "Int | Float",
        }
    }

    /// Concise language documentation for hover surfaces.
    #[must_use]
    pub const fn documentation(self) -> &'static str {
        match self {
            Self::Env => {
                "Reads a child-environment entry by name, returning String or Null when absent."
            }
            Self::Glob => {
                "Matches an explicit filesystem pattern and returns sorted native Path values."
            }
            Self::Int => "Converts an Int or Float value to Int by truncating toward zero.",
            Self::Float => "Converts an Int or Float value to Float.",
        }
    }

    /// Applies the intrinsic to one already evaluated argument. Host-backed
    /// intrinsics report that requirement for callers outside the evaluator.
    pub fn invoke(self, value: &Value) -> Result<Value, OperationError> {
        match self {
            Self::Env => Err(OperationError::HostContextRequired { operation: "env" }),
            Self::Glob => Err(OperationError::HostContextRequired { operation: "glob" }),
            Self::Int => operation::to_int(value),
            Self::Float => operation::to_float(value),
        }
    }
}

/// A reserved value resolved dynamically from the active evaluation host.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DynamicBinding {
    CurrentStatus,
}

impl DynamicBinding {
    /// Every dynamic binding in deterministic name order.
    pub const ALL: [Self; 1] = [Self::CurrentStatus];

    /// Resolves one exact dynamic-binding spelling.
    #[must_use]
    pub fn lookup(name: &str) -> Option<Self> {
        match name {
            "status" => Some(Self::CurrentStatus),
            _ => None,
        }
    }

    /// The exact language spelling, without the `$` read sigil.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CurrentStatus => "status",
        }
    }

    /// The statically known family. Dynamic status is `Null | Status`, whose
    /// v1 approximation is `Any` because the type language has no unions.
    #[must_use]
    pub const fn result_type(self) -> ValueType {
        match self {
            Self::CurrentStatus => ValueType::Any,
        }
    }

    /// Concise language documentation for hover surfaces.
    #[must_use]
    pub const fn documentation(self) -> &'static str {
        match self {
            Self::CurrentStatus => {
                "The live completed command status, or Null before any status exists."
            }
        }
    }
}
