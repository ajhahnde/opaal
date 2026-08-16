use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::module::ValueType;
use crate::{Environment, NativePath, Value};

/// Whether a lexical binding cell may be reassigned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingMutability {
    Immutable,
    Mutable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Binding {
    mutability: BindingMutability,
    value_type: Option<ValueType>,
    value: Value,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ScopeFrame {
    bindings: Vec<(Arc<str>, Binding)>,
}

/// A root scope and its active nested lexical frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeStack {
    frames: Vec<ScopeFrame>,
}

impl ScopeStack {
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames: vec![ScopeFrame::default()],
        }
    }

    pub(crate) fn from_environment(environment: &Environment) -> Self {
        let mut scope = Self::new();
        for (name, value) in environment.iter() {
            scope
                .declare(
                    name,
                    BindingMutability::Immutable,
                    Value::Path(NativePath::new(value.to_os_string())),
                )
                .expect("environment entry names are unique");
        }
        scope
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Returns a by-value capture of the visible scope with every binding frozen
    /// immutable, matching the closure-capture rule: a captured cell cannot be
    /// reassigned inside the callable even when its source binding was `mut`.
    #[must_use]
    pub fn captured_snapshot(&self) -> Self {
        let frames = self
            .frames
            .iter()
            .map(|frame| ScopeFrame {
                bindings: frame
                    .bindings
                    .iter()
                    .map(|(name, binding)| {
                        (
                            Arc::clone(name),
                            Binding {
                                mutability: BindingMutability::Immutable,
                                value_type: binding.value_type.clone(),
                                value: binding.value.clone(),
                            },
                        )
                    })
                    .collect(),
            })
            .collect();
        Self { frames }
    }

    pub fn push(&mut self) {
        self.frames.push(ScopeFrame::default());
    }

    pub fn pop(&mut self) -> Result<(), ScopeError> {
        if self.frames.len() == 1 {
            return Err(ScopeError::CannotPopRoot);
        }
        self.frames.pop();
        Ok(())
    }

    pub fn declare(
        &mut self,
        name: impl Into<String>,
        mutability: BindingMutability,
        value: Value,
    ) -> Result<(), ScopeError> {
        self.declare_typed(name, mutability, value, None)
    }

    pub(crate) fn declare_typed(
        &mut self,
        name: impl Into<String>,
        mutability: BindingMutability,
        value: Value,
        value_type: Option<ValueType>,
    ) -> Result<(), ScopeError> {
        let name = name.into();
        let current = self
            .frames
            .last_mut()
            .expect("scope stacks always retain their root frame");
        if current
            .bindings
            .iter()
            .any(|(existing, _)| existing.as_ref() == name)
        {
            return Err(ScopeError::DuplicateBinding(name));
        }
        if let Some(expected) = value_type.as_ref()
            && !expected.accepts(&value)
        {
            return Err(ScopeError::TypeMismatch {
                expected: expected.clone(),
                actual: value.family_name(),
            });
        }
        current.bindings.push((
            Arc::from(name),
            Binding {
                mutability,
                value_type,
                value,
            },
        ));
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.find(name).map(|binding| &binding.value)
    }

    #[must_use]
    pub fn mutability(&self, name: &str) -> Option<BindingMutability> {
        self.find(name).map(|binding| binding.mutability)
    }

    pub fn assign(&mut self, name: &str, value: Value) -> Result<(), ScopeError> {
        for frame in self.frames.iter_mut().rev() {
            let Some((_, binding)) = frame
                .bindings
                .iter_mut()
                .find(|(candidate, _)| candidate.as_ref() == name)
            else {
                continue;
            };
            if binding.mutability == BindingMutability::Immutable {
                return Err(ScopeError::ImmutableBinding(name.to_owned()));
            }
            if let Some(expected) = binding.value_type.as_ref()
                && !expected.accepts(&value)
            {
                return Err(ScopeError::TypeMismatch {
                    expected: expected.clone(),
                    actual: value.family_name(),
                });
            }
            binding.value = value;
            return Ok(());
        }
        Err(ScopeError::UnknownBinding(name.to_owned()))
    }

    /// Returns the visible bindings in name order, with inner frames shadowing
    /// bindings of the same name in outer frames.
    #[must_use]
    pub fn visible_bindings(&self) -> Vec<(&str, &Value)> {
        let mut visible = BTreeMap::new();
        for frame in &self.frames {
            for (name, binding) in &frame.bindings {
                visible.insert(name.as_ref(), &binding.value);
            }
        }
        visible.into_iter().collect()
    }

    fn find(&self, name: &str) -> Option<&Binding> {
        self.frames.iter().rev().find_map(|frame| {
            frame
                .bindings
                .iter()
                .find_map(|(candidate, binding)| (candidate.as_ref() == name).then_some(binding))
        })
    }
}

impl Default for ScopeStack {
    fn default() -> Self {
        Self::new()
    }
}

/// A source-independent lexical-scope operation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScopeError {
    DuplicateBinding(String),
    ReservedBinding(String),
    UnknownBinding(String),
    ImmutableBinding(String),
    TypeMismatch {
        expected: ValueType,
        actual: &'static str,
    },
    CannotPopRoot,
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBinding(name) => {
                write!(formatter, "binding {name:?} already exists in this scope")
            }
            Self::ReservedBinding(name) => {
                write!(formatter, "binding name {name:?} is reserved")
            }
            Self::UnknownBinding(name) => write!(formatter, "unknown binding {name:?}"),
            Self::ImmutableBinding(name) => {
                write!(formatter, "binding {name:?} is immutable")
            }
            Self::TypeMismatch { expected, actual } => {
                write!(formatter, "binding expects {expected}, found {actual}")
            }
            Self::CannotPopRoot => formatter.write_str("cannot leave the root scope"),
        }
    }
}

impl Error for ScopeError {}
