//! The child-process environment: a name→native-string map distinct from the
//! lexical scope.
//!
//! Entries are platform-native (`OsString`) because a child
//! process receives name→bytes; a value crosses into an entry through the single
//! canonical word encoding used for command words. The map is seeded from an
//! injected snapshot and mutated in place across statements, with no real
//! `std::env`, process, or platform-trait dependency. Iteration is name-sorted so
//! display and planning are deterministic.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

/// An ordered environment mapping variable names to native string values.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Environment {
    entries: BTreeMap<String, OsString>,
}

impl Environment {
    /// An empty environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds an environment from an inherited snapshot of name/value pairs. A
    /// later pair overwrites an earlier one with the same name.
    pub fn from_snapshot<I, N, V>(entries: I) -> Self
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: Into<OsString>,
    {
        Self {
            entries: entries
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        }
    }

    /// The native value bound to `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.entries.get(name).map(OsString::as_os_str)
    }

    /// Whether `name` has an entry.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Inserts or overwrites `name` with `value`.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<OsString>) {
        self.entries.insert(name.into(), value.into());
    }

    /// Removes `name`, returning whether an entry was present. Removing an absent
    /// name is a no-op.
    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the environment has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// The entries in sorted name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &OsStr)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_os_str()))
    }
}
