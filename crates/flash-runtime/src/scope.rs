use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapsuleBinding {
    pub(crate) name: String,
    pub(crate) mutability: BindingMutability,
    pub(crate) value_type: Option<ValueType>,
    pub(crate) value: Value,
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

    /// Removes the innermost visible binding named `name` and returns its value.
    ///
    /// Host startup integration uses this to consume temporary configuration
    /// bindings before installing the committed lexical scope.
    pub fn remove(&mut self, name: &str) -> Option<Value> {
        for frame in self.frames.iter_mut().rev() {
            let Some(index) = frame
                .bindings
                .iter()
                .position(|(candidate, _)| candidate.as_ref() == name)
            else {
                continue;
            };
            return Some(frame.bindings.remove(index).1.value);
        }
        None
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

    pub(crate) fn capsule_bindings(&self) -> Vec<CapsuleBinding> {
        let mut visible = BTreeMap::new();
        for frame in &self.frames {
            for (name, binding) in &frame.bindings {
                visible.insert(name.as_ref(), binding);
            }
        }
        visible
            .into_iter()
            .map(|(name, binding)| CapsuleBinding {
                name: name.to_owned(),
                mutability: binding.mutability,
                value_type: binding.value_type.clone(),
                value: binding.value.clone(),
            })
            .collect()
    }

    pub(crate) fn from_capsule_bindings(bindings: Vec<CapsuleBinding>) -> Result<Self, ScopeError> {
        let mut scope = Self::new();
        for binding in bindings {
            scope.declare_typed(
                binding.name,
                binding.mutability,
                binding.value,
                binding.value_type,
            )?;
        }
        Ok(scope)
    }

    pub(crate) fn apply_capsule_delta(
        &mut self,
        base: &Self,
        updated: &Self,
    ) -> Result<(), ScopeError> {
        let base: BTreeMap<_, _> = base
            .capsule_bindings()
            .into_iter()
            .map(|binding| (binding.name.clone(), binding))
            .collect();
        let updated: BTreeMap<_, _> = updated
            .capsule_bindings()
            .into_iter()
            .map(|binding| (binding.name.clone(), binding))
            .collect();
        let changed_names: BTreeSet<_> = base.keys().chain(updated.keys()).cloned().collect();
        for name in changed_names {
            let base_binding = base.get(&name);
            let updated_binding = updated.get(&name);
            if base_binding == updated_binding {
                continue;
            }
            if let (Some(base_binding), Some(updated_binding)) = (base_binding, updated_binding)
                && base_binding.mutability == updated_binding.mutability
                && base_binding.value_type == updated_binding.value_type
            {
                self.assign(&name, updated_binding.value.clone())?;
                continue;
            }
            self.remove(&name);
            if let Some(binding) = updated_binding {
                self.declare_typed(
                    binding.name.clone(),
                    binding.mutability,
                    binding.value.clone(),
                    binding.value_type.clone(),
                )?;
            }
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_consumes_the_innermost_visible_binding() {
        let mut scope = ScopeStack::new();
        scope
            .declare("setting", BindingMutability::Mutable, Value::Int(1))
            .unwrap();
        scope.push();
        scope
            .declare("setting", BindingMutability::Immutable, Value::Int(2))
            .unwrap();

        assert_eq!(scope.remove("setting"), Some(Value::Int(2)));
        assert_eq!(scope.get("setting"), Some(&Value::Int(1)));
        assert_eq!(scope.remove("setting"), Some(Value::Int(1)));
        assert_eq!(scope.remove("setting"), None);
    }
}
