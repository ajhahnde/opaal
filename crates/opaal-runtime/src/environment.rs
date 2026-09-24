//! Native session cwd/environment snapshots and the child-process environment.
//!
//! Names and values are platform-native (`OsString`) because a child process
//! receives name→bytes. A value crosses into an entry through the single
//! canonical word encoding used for command words. The map is seeded from an
//! injected snapshot and mutated in place across statements. Iteration is
//! native-name-sorted so display and planning are deterministic.

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Exact resource ceilings for one native host environment snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostEnvironmentLimits {
    entries: usize,
    bytes: usize,
}

impl HostEnvironmentLimits {
    /// OPAAL's native host environment ceilings.
    pub const OPAAL: Self = Self::new(1_000_000, 16 * 1024 * 1024);

    /// Build explicit limits. This is primarily useful to exercise exact
    /// boundary behavior without constructing a production-sized fixture.
    #[must_use]
    pub const fn new(entries: usize, bytes: usize) -> Self {
        Self { entries, bytes }
    }

    /// The maximum accepted number of snapshot entries.
    #[must_use]
    pub const fn entries(self) -> usize {
        self.entries
    }

    /// The maximum accepted combined native name/value byte count.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }
}

impl Default for HostEnvironmentLimits {
    fn default() -> Self {
        Self::OPAAL
    }
}

/// One bounded host-environment measurement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostEnvironmentLimitKind {
    /// Number of name/value entries encountered in the snapshot.
    Entries,
    /// Combined native bytes in every encountered name and value.
    Bytes,
}

impl HostEnvironmentLimitKind {
    /// Stable machine-readable name for this measurement unit.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::Bytes => "native-bytes",
        }
    }
}

/// A native environment component that cannot cross the process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidEnvironmentComponent {
    /// The native variable name.
    Name,
    /// The native variable value.
    Value,
}

/// A deterministic native environment snapshot refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentSnapshotError {
    /// The first entry beyond one inclusive ceiling.
    LimitExceeded {
        /// The exhausted resource dimension.
        kind: HostEnvironmentLimitKind,
        /// The exact inclusive ceiling.
        limit: usize,
        /// The one-based ordinal of the entry that first exceeded it.
        entry: usize,
    },
    /// An entry contains bytes the POSIX process environment cannot represent.
    InvalidEntry {
        /// The one-based ordinal of the invalid entry.
        entry: usize,
        /// Whether the invalid bytes occur in its name or value.
        component: InvalidEnvironmentComponent,
    },
    /// A native name occurred twice and cannot be represented losslessly.
    DuplicateName {
        /// The one-based ordinal of the repeated entry.
        entry: usize,
    },
}

impl fmt::Display for EnvironmentSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded { kind, limit, entry } => write!(
                formatter,
                "host environment entry {entry} exceeded the {} limit of {limit}",
                kind.name()
            ),
            Self::InvalidEntry { entry, component } => {
                let component = match component {
                    InvalidEnvironmentComponent::Name => "name",
                    InvalidEnvironmentComponent::Value => "value",
                };
                write!(
                    formatter,
                    "host environment entry {entry} has an invalid native {component}"
                )
            }
            Self::DuplicateName { entry } => {
                write!(
                    formatter,
                    "host environment entry {entry} repeats a native name"
                )
            }
        }
    }
}

impl Error for EnvironmentSnapshotError {}

/// A failure to acquire a complete native session cwd/environment snapshot.
#[derive(Debug)]
pub enum NativeSessionSnapshotError {
    /// The host working directory could not be acquired.
    CurrentDirectory(io::Error),
    /// The acquired working directory is not a usable absolute native path.
    InvalidCurrentDirectory,
    /// The environment failed its native representation or resource contract.
    Environment(EnvironmentSnapshotError),
}

impl fmt::Display for NativeSessionSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectory(error) => {
                write!(
                    formatter,
                    "failed to acquire the host working directory: {error}"
                )
            }
            Self::InvalidCurrentDirectory => {
                formatter.write_str("the host working directory is not a valid absolute path")
            }
            Self::Environment(error) => error.fmt(formatter),
        }
    }
}

impl Error for NativeSessionSnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentDirectory(error) => Some(error),
            Self::Environment(error) => Some(error),
            Self::InvalidCurrentDirectory => None,
        }
    }
}

impl From<EnvironmentSnapshotError> for NativeSessionSnapshotError {
    fn from(error: EnvironmentSnapshotError) -> Self {
        Self::Environment(error)
    }
}

/// One complete bounded native cwd/environment snapshot for a CLI session.
#[derive(Clone, Eq, PartialEq)]
pub struct NativeSessionSnapshot {
    cwd: PathBuf,
    environment: Environment,
}

impl fmt::Debug for NativeSessionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSessionSnapshot")
            .field("cwd", &"<native>")
            .field("environment", &self.environment)
            .finish()
    }
}

impl NativeSessionSnapshot {
    /// Acquire the current process cwd and environment once, without lossy
    /// conversion, using OPAAL's exact production ceilings.
    pub fn capture() -> Result<Self, NativeSessionSnapshotError> {
        let cwd = std::env::current_dir().map_err(NativeSessionSnapshotError::CurrentDirectory)?;
        Self::from_snapshot(cwd, std::env::vars_os(), HostEnvironmentLimits::OPAAL)
    }

    /// Build and validate one injected native snapshot.
    pub fn from_snapshot<I, N, V>(
        cwd: impl Into<PathBuf>,
        entries: I,
        limits: HostEnvironmentLimits,
    ) -> Result<Self, NativeSessionSnapshotError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<OsString>,
        V: Into<OsString>,
    {
        let cwd = cwd.into();
        if !cwd.is_absolute() || cwd.as_os_str().as_bytes().contains(&0) {
            return Err(NativeSessionSnapshotError::InvalidCurrentDirectory);
        }
        let environment = Environment::try_from_snapshot(entries, limits)?;
        Ok(Self { cwd, environment })
    }

    /// The native working directory retained for the session lifetime.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The complete native child environment retained for the session lifetime.
    #[must_use]
    pub const fn environment(&self) -> &Environment {
        &self.environment
    }

    /// Split the snapshot into the two values consumed by session construction.
    #[must_use]
    pub fn into_parts(self) -> (PathBuf, Environment) {
        (self.cwd, self.environment)
    }
}

/// An ordered environment mapping variable names to native string values.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct Environment {
    entries: BTreeMap<OsString, OsString>,
}

impl fmt::Debug for Environment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Environment")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
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
        N: Into<OsString>,
        V: Into<OsString>,
    {
        Self {
            entries: entries
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        }
    }

    /// Builds a complete bounded native snapshot or refuses the first invalid
    /// or over-limit entry. Error values contain only the entry ordinal and
    /// failure class, never inherited names or values.
    pub fn try_from_snapshot<I, N, V>(
        entries: I,
        limits: HostEnvironmentLimits,
    ) -> Result<Self, EnvironmentSnapshotError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<OsString>,
        V: Into<OsString>,
    {
        let mut snapshot = BTreeMap::new();
        let mut bytes = 0usize;
        for (index, (name, value)) in entries.into_iter().enumerate() {
            let entry = index.saturating_add(1);
            if entry > limits.entries() {
                return Err(EnvironmentSnapshotError::LimitExceeded {
                    kind: HostEnvironmentLimitKind::Entries,
                    limit: limits.entries(),
                    entry,
                });
            }

            let name = name.into();
            let value = value.into();
            if name.is_empty() || name.as_bytes().contains(&0) || name.as_bytes().contains(&b'=') {
                return Err(EnvironmentSnapshotError::InvalidEntry {
                    entry,
                    component: InvalidEnvironmentComponent::Name,
                });
            }
            if value.as_bytes().contains(&0) {
                return Err(EnvironmentSnapshotError::InvalidEntry {
                    entry,
                    component: InvalidEnvironmentComponent::Value,
                });
            }

            let entry_bytes = name.as_bytes().len().saturating_add(value.as_bytes().len());
            bytes = bytes.saturating_add(entry_bytes);
            if bytes > limits.bytes() {
                return Err(EnvironmentSnapshotError::LimitExceeded {
                    kind: HostEnvironmentLimitKind::Bytes,
                    limit: limits.bytes(),
                    entry,
                });
            }
            if snapshot.contains_key(&name) {
                return Err(EnvironmentSnapshotError::DuplicateName { entry });
            }
            snapshot.insert(name, value);
        }
        Ok(Self { entries: snapshot })
    }

    /// The native value bound to `name`, if any.
    #[must_use]
    pub fn get(&self, name: impl AsRef<OsStr>) -> Option<&OsStr> {
        self.entries.get(name.as_ref()).map(OsString::as_os_str)
    }

    /// Whether `name` has an entry.
    #[must_use]
    pub fn contains(&self, name: impl AsRef<OsStr>) -> bool {
        self.entries.contains_key(name.as_ref())
    }

    /// Inserts or overwrites `name` with `value`.
    pub fn set(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        self.entries.insert(name.into(), value.into());
    }

    /// Removes `name`, returning whether an entry was present. Removing an absent
    /// name is a no-op.
    pub fn remove(&mut self, name: impl AsRef<OsStr>) -> bool {
        self.entries.remove(name.as_ref()).is_some()
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
    pub fn names(&self) -> impl Iterator<Item = &OsStr> {
        self.entries.keys().map(OsString::as_os_str)
    }

    /// The entries in sorted name order.
    pub fn iter(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.entries
            .iter()
            .map(|(name, value)| (name.as_os_str(), value.as_os_str()))
    }

    /// Apply only entries changed between `base` and `updated` to this mapping.
    ///
    /// Lazy closure stages evaluate against their own transaction snapshot.
    /// Committing that snapshot wholesale would overwrite state changed by a
    /// built-in in the same pending pipeline, such as `cd` updating `PWD` and
    /// `OLDPWD`. A delta preserves both boundaries while still committing
    /// closure-side `export` and `unset` changes after successful rendering.
    pub(crate) fn apply_delta(&mut self, base: &Self, updated: &Self) {
        for name in base.entries.keys().chain(updated.entries.keys()) {
            if base.entries.get(name) == updated.entries.get(name) {
                continue;
            }
            match updated.entries.get(name) {
                Some(value) => {
                    self.entries.insert(name.clone(), value.clone());
                }
                None => {
                    self.entries.remove(name);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    use super::{
        Environment, EnvironmentSnapshotError, HostEnvironmentLimitKind, HostEnvironmentLimits,
        InvalidEnvironmentComponent, NativeSessionSnapshot, NativeSessionSnapshotError,
    };

    #[test]
    fn native_session_snapshot_preserves_non_utf8_cwd_names_and_values() {
        let cwd = OsString::from_vec(vec![b'/', b's', 0x80]);
        let name = OsString::from_vec(vec![b'N', 0x81]);
        let value = OsString::from_vec(vec![b'v', 0x82]);
        let snapshot = NativeSessionSnapshot::from_snapshot(
            &cwd,
            [(name.clone(), value.clone())],
            HostEnvironmentLimits::new(1, 4),
        )
        .expect("native snapshot should be preserved");

        assert_eq!(snapshot.cwd().as_os_str().as_bytes(), cwd.as_bytes());
        assert_eq!(snapshot.environment().get(&name), Some(value.as_os_str()));
    }

    #[test]
    fn native_session_snapshot_debug_does_not_dump_inherited_context() {
        let snapshot = NativeSessionSnapshot::from_snapshot(
            "/private/session",
            [("SECRET_NAME", "secret-value")],
            HostEnvironmentLimits::OPAAL,
        )
        .expect("fixture snapshot should be valid");
        let rendered = format!("{snapshot:?}");

        assert!(!rendered.contains("/private/session"));
        assert!(!rendered.contains("SECRET_NAME"));
        assert!(!rendered.contains("secret-value"));
        assert!(rendered.contains("entries: 1"));
    }

    #[test]
    fn native_snapshot_refuses_duplicate_names_without_exposing_their_values() {
        let name = OsString::from_vec(vec![b'N', 0x81]);
        let error = NativeSessionSnapshot::from_snapshot(
            "/session",
            [
                (name.clone(), OsString::from("first-secret")),
                (name, OsString::from("second-secret")),
            ],
            HostEnvironmentLimits::OPAAL,
        )
        .expect_err("a duplicate native name cannot be represented losslessly");

        assert!(matches!(
            error,
            NativeSessionSnapshotError::Environment(EnvironmentSnapshotError::DuplicateName {
                entry: 2
            })
        ));
        let rendered = error.to_string();
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("N"));
    }

    #[test]
    fn environment_entry_limit_accepts_the_boundary_and_refuses_the_first_excess() {
        let limits = HostEnvironmentLimits::new(2, usize::MAX);
        let accepted = Environment::try_from_snapshot([("A", "1"), ("B", "2")], limits)
            .expect("the exact entry boundary should be accepted");
        assert_eq!(accepted.len(), 2);

        assert_eq!(
            Environment::try_from_snapshot([("A", "1"), ("B", "2"), ("C", "3")], limits),
            Err(EnvironmentSnapshotError::LimitExceeded {
                kind: HostEnvironmentLimitKind::Entries,
                limit: 2,
                entry: 3,
            })
        );
    }

    #[test]
    fn environment_byte_limit_accepts_the_boundary_and_refuses_the_first_excess() {
        let exact = HostEnvironmentLimits::new(usize::MAX, 5);
        let accepted = Environment::try_from_snapshot([("A", "12"), ("B", "3")], exact)
            .expect("the exact byte boundary should be accepted");
        assert_eq!(accepted.len(), 2);

        assert_eq!(
            Environment::try_from_snapshot([("A", "12"), ("B", "3"), ("C", "")], exact,),
            Err(EnvironmentSnapshotError::LimitExceeded {
                kind: HostEnvironmentLimitKind::Bytes,
                limit: 5,
                entry: 3,
            })
        );
    }

    #[test]
    fn invalid_native_entries_refuse_without_rendering_inherited_bytes() {
        let secret = OsString::from_vec(b"SECRET=value\0hidden".to_vec());
        let error = Environment::try_from_snapshot(
            [(OsString::from("TOKEN"), secret)],
            HostEnvironmentLimits::OPAAL,
        )
        .expect_err("a native NUL cannot cross the process boundary");

        assert_eq!(
            error,
            EnvironmentSnapshotError::InvalidEntry {
                entry: 1,
                component: InvalidEnvironmentComponent::Value,
            }
        );
        assert!(!error.to_string().contains("SECRET"));
        assert!(!error.to_string().contains("hidden"));
    }

    #[test]
    fn invalid_environment_name_and_cwd_refuse_before_session_creation() {
        assert_eq!(
            Environment::try_from_snapshot(
                [(OsString::new(), OsString::from("value"))],
                HostEnvironmentLimits::OPAAL,
            ),
            Err(EnvironmentSnapshotError::InvalidEntry {
                entry: 1,
                component: InvalidEnvironmentComponent::Name,
            })
        );
        assert_eq!(
            Environment::try_from_snapshot(
                [(OsString::from("A=B"), OsString::new())],
                HostEnvironmentLimits::OPAAL,
            ),
            Err(EnvironmentSnapshotError::InvalidEntry {
                entry: 1,
                component: InvalidEnvironmentComponent::Name,
            })
        );
        assert!(matches!(
            NativeSessionSnapshot::from_snapshot(
                Path::new("relative"),
                [(OsStr::new("A"), OsStr::new("B"))],
                HostEnvironmentLimits::OPAAL,
            ),
            Err(NativeSessionSnapshotError::InvalidCurrentDirectory)
        ));
    }
}
