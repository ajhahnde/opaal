//! Command-namespace resolution and external `PATH` lookup.
//!
//! [`resolve_external`] turns an already-expanded native command name into a
//! concrete executable path using the retained session working directory, the
//! environment's `PATH`, and an injected [`ExecutableProbe`]. It performs no
//! filesystem access of its own and never routes a command through `/bin/sh`:
//! the probe is the only capability, so runtime tests supply a fixed set of
//! executable paths. Resolution is span-independent; a command span is attached
//! when it is wired into command planning later.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::Environment;
use crate::command::{CommandClassification, CommandRegistry, CommandSignature};

pub use opaal_platform::ExecutableProbe;

/// Maximum native bytes accepted from `PATH` for one executable resolution.
pub const MAX_PATH_SEARCH_BYTES: usize = 1024 * 1024;
/// Maximum ordered `PATH` elements accepted for one executable resolution.
pub const MAX_PATH_SEARCH_ELEMENTS: usize = 4_096;

/// The bounded `PATH` measurement that prevented executable resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathSearchLimitKind {
    /// Native bytes in the complete `PATH` value.
    Bytes,
    /// Ordered elements in the colon-separated `PATH` value.
    Elements,
}

impl PathSearchLimitKind {
    /// Stable machine-readable name for this measurement unit.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bytes => "native-bytes",
            Self::Elements => "elements",
        }
    }
}

/// An external name resolved to a concrete executable path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCommand {
    path: PathBuf,
}

impl ResolvedCommand {
    /// The resolved native executable path. For a path-like name this is the name
    /// itself; for a bare name it is the accepted `PATH` element joined with the
    /// name.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A failure to resolve a command name.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResolutionError {
    /// No `PATH` element (or the path-like name itself) named an executable.
    /// `name` is the searched command name.
    NotFound {
        /// The command name that could not be resolved.
        name: std::ffi::OsString,
    },
    /// A namespace reservation prevented implicit external fallback.
    Reserved {
        /// The reserved UTF-8 source spelling.
        name: String,
        /// Stable reason the spelling is unavailable.
        purpose: String,
        /// Optional canonical replacement target.
        replacement: Option<String>,
    },
    /// The native `PATH` value exceeded a deterministic search ceiling.
    PathSearchLimitExceeded {
        /// The exhausted resource dimension.
        kind: PathSearchLimitKind,
        /// The exact inclusive ceiling.
        limit: usize,
    },
}

/// A resolved command: an internal source/canonical identity or an external executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution<'a> {
    /// A bare name matched a registered internal command.
    Internal {
        /// The expanded source spelling used by the caller.
        source_name: String,
        /// The canonical executor identity.
        canonical_name: &'a str,
        /// The canonical command signature.
        signature: &'a CommandSignature,
    },
    /// A name resolved to an external executable.
    External(ResolvedCommand),
}

/// Resolves a command name against the registry or the explicit external seam.
///
/// A bare name (`force_external` is `false`) must be a registered internal
/// command. Unknown names never reach `PATH`. An `^external` name
/// (`force_external` is `true`) resolves externally only.
pub fn resolve_command<'a>(
    name: &OsStr,
    force_external: bool,
    registry: &'a CommandRegistry,
    cwd: &Path,
    environment: &Environment,
    probe: &dyn ExecutableProbe,
) -> Result<Resolution<'a>, ResolutionError> {
    if !force_external && let Some(name) = name.to_str() {
        match registry.classify(name) {
            CommandClassification::Unknown => {
                return Err(ResolutionError::Reserved {
                    name: name.to_owned(),
                    purpose: "unknown bare name cannot start an external process".to_owned(),
                    replacement: Some(format!("^{name}")),
                });
            }
            CommandClassification::Core { signature, .. } => {
                return Ok(Resolution::Internal {
                    source_name: name.to_owned(),
                    canonical_name: signature.name(),
                    signature,
                });
            }
            CommandClassification::Alias {
                canonical_name,
                signature,
                ..
            } => {
                return Ok(Resolution::Internal {
                    source_name: name.to_owned(),
                    canonical_name,
                    signature,
                });
            }
            CommandClassification::Reserved {
                purpose,
                replacement,
                ..
            } => {
                return Err(ResolutionError::Reserved {
                    name: name.to_owned(),
                    purpose: purpose.to_owned(),
                    replacement: replacement.map(str::to_owned),
                });
            }
        }
    }
    if force_external {
        resolve_external(name, cwd, environment, probe).map(Resolution::External)
    } else {
        Err(ResolutionError::Reserved {
            name: name.to_string_lossy().into_owned(),
            purpose: "a bare command head must be a UTF-8 registered name".to_owned(),
            replacement: None,
        })
    }
}

/// Resolves an `^external` command name to an executable path.
///
/// A name containing `/` is path-like and resolves to itself without consulting
/// `PATH`. A name with no `/` is searched in `PATH` (read as native bytes, split
/// on `:`, with empty elements dropped) in order. Relative direct and `PATH`
/// candidates are probed against `cwd` but remain relative in the resolved
/// command so the spawn adapter interprets them against the same retained
/// working directory. The first candidate the probe accepts wins. A name that
/// resolves nowhere is a [`ResolutionError::NotFound`] carrying the searched
/// name.
pub fn resolve_external(
    name: &OsStr,
    cwd: &Path,
    environment: &Environment,
    probe: &dyn ExecutableProbe,
) -> Result<ResolvedCommand, ResolutionError> {
    let not_found = || ResolutionError::NotFound {
        name: name.to_os_string(),
    };

    if name.as_bytes().contains(&b'/') {
        let path = PathBuf::from(name);
        return if probe.is_executable(probe_path(&path, cwd).as_os_str()) {
            Ok(ResolvedCommand { path })
        } else {
            Err(not_found())
        };
    }

    let Some(path_value) = environment.get("PATH") else {
        return Err(not_found());
    };

    validate_path_search(path_value)?;

    // Split on `:` over native bytes and drop empty elements; an empty element
    // never denotes the working directory.
    for element in path_value.as_bytes().split(|byte| *byte == b':') {
        if element.is_empty() {
            continue;
        }
        let candidate = Path::new(OsStr::from_bytes(element)).join(name);
        if probe.is_executable(probe_path(&candidate, cwd).as_os_str()) {
            return Ok(ResolvedCommand { path: candidate });
        }
    }

    Err(not_found())
}

fn validate_path_search(path_value: &OsStr) -> Result<(), ResolutionError> {
    let bytes = path_value.as_bytes();
    if bytes.len() > MAX_PATH_SEARCH_BYTES {
        return Err(ResolutionError::PathSearchLimitExceeded {
            kind: PathSearchLimitKind::Bytes,
            limit: MAX_PATH_SEARCH_BYTES,
        });
    }

    for (index, _) in bytes.split(|byte| *byte == b':').enumerate() {
        if index >= MAX_PATH_SEARCH_ELEMENTS {
            return Err(ResolutionError::PathSearchLimitExceeded {
                kind: PathSearchLimitKind::Elements,
                limit: MAX_PATH_SEARCH_ELEMENTS,
            });
        }
    }
    Ok(())
}

fn probe_path(candidate: &Path, cwd: &Path) -> PathBuf {
    if candidate.is_absolute() {
        candidate.to_owned()
    } else {
        cwd.join(candidate)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::{
        ExecutableProbe, MAX_PATH_SEARCH_BYTES, MAX_PATH_SEARCH_ELEMENTS, PathSearchLimitKind,
        ResolutionError, resolve_external,
    };
    use crate::Environment;

    #[derive(Default)]
    struct RecordingProbe {
        accepted: Vec<PathBuf>,
        observed: Mutex<Vec<PathBuf>>,
    }

    impl RecordingProbe {
        fn accepting(paths: impl IntoIterator<Item = PathBuf>) -> Self {
            Self {
                accepted: paths.into_iter().collect(),
                observed: Mutex::new(Vec::new()),
            }
        }

        fn observed(&self) -> Vec<PathBuf> {
            self.observed.lock().expect("probe log lock").clone()
        }
    }

    impl ExecutableProbe for RecordingProbe {
        fn is_executable(&self, path: &OsStr) -> bool {
            let path = PathBuf::from(path);
            self.observed
                .lock()
                .expect("probe log lock")
                .push(path.clone());
            self.accepted.contains(&path)
        }
    }

    #[test]
    fn relative_direct_candidate_is_probed_against_session_cwd() {
        let probe = RecordingProbe::accepting([PathBuf::from("/session/tools/probe")]);
        let resolved = resolve_external(
            OsStr::new("tools/probe"),
            Path::new("/session"),
            &Environment::new(),
            &probe,
        )
        .expect("relative executable resolves");

        assert_eq!(resolved.path(), Path::new("tools/probe"));
        assert_eq!(probe.observed(), [PathBuf::from("/session/tools/probe")]);
    }

    #[test]
    fn relative_path_elements_are_probed_against_session_cwd_in_order() {
        let environment =
            Environment::from_snapshot([("PATH", OsString::from("missing:/absolute/bin::tools"))]);
        let probe = RecordingProbe::accepting([PathBuf::from("/session/tools/probe")]);
        let resolved = resolve_external(
            OsStr::new("probe"),
            Path::new("/session"),
            &environment,
            &probe,
        )
        .expect("PATH executable resolves");

        assert_eq!(resolved.path(), Path::new("tools/probe"));
        assert_eq!(
            probe.observed(),
            [
                PathBuf::from("/session/missing/probe"),
                PathBuf::from("/absolute/bin/probe"),
                PathBuf::from("/session/tools/probe"),
            ]
        );
    }

    #[test]
    fn path_byte_limit_accepts_the_boundary_and_refuses_first_excess_before_probe() {
        let exact_path = OsString::from_vec(vec![b'x'; MAX_PATH_SEARCH_BYTES]);
        let exact_environment = Environment::from_snapshot([("PATH", exact_path)]);
        let exact_probe = RecordingProbe::default();
        assert!(matches!(
            resolve_external(
                OsStr::new("probe"),
                Path::new("/session"),
                &exact_environment,
                &exact_probe,
            ),
            Err(ResolutionError::NotFound { .. })
        ));
        assert_eq!(exact_probe.observed().len(), 1);

        let excess_path = OsString::from_vec(vec![b'x'; MAX_PATH_SEARCH_BYTES + 1]);
        let excess_environment = Environment::from_snapshot([("PATH", excess_path)]);
        let excess_probe = RecordingProbe::accepting([PathBuf::from("/session/x/probe")]);
        assert_eq!(
            resolve_external(
                OsStr::new("probe"),
                Path::new("/session"),
                &excess_environment,
                &excess_probe,
            ),
            Err(ResolutionError::PathSearchLimitExceeded {
                kind: PathSearchLimitKind::Bytes,
                limit: MAX_PATH_SEARCH_BYTES,
            })
        );
        assert!(excess_probe.observed().is_empty());
    }

    #[test]
    fn path_element_limit_accepts_the_boundary_and_refuses_first_excess_before_probe() {
        let exact_path = vec!["x"; MAX_PATH_SEARCH_ELEMENTS].join(":");
        let exact_environment = Environment::from_snapshot([("PATH", exact_path)]);
        let exact_probe = RecordingProbe::default();
        assert!(matches!(
            resolve_external(
                OsStr::new("probe"),
                Path::new("/session"),
                &exact_environment,
                &exact_probe,
            ),
            Err(ResolutionError::NotFound { .. })
        ));
        assert_eq!(exact_probe.observed().len(), MAX_PATH_SEARCH_ELEMENTS);

        let excess_path = vec!["x"; MAX_PATH_SEARCH_ELEMENTS + 1].join(":");
        let excess_environment = Environment::from_snapshot([("PATH", excess_path)]);
        let excess_probe = RecordingProbe::accepting([PathBuf::from("/session/x/probe")]);
        assert_eq!(
            resolve_external(
                OsStr::new("probe"),
                Path::new("/session"),
                &excess_environment,
                &excess_probe,
            ),
            Err(ResolutionError::PathSearchLimitExceeded {
                kind: PathSearchLimitKind::Elements,
                limit: MAX_PATH_SEARCH_ELEMENTS,
            })
        );
        assert!(excess_probe.observed().is_empty());

        let empty_excess = ":".repeat(MAX_PATH_SEARCH_ELEMENTS);
        let empty_environment = Environment::from_snapshot([("PATH", empty_excess)]);
        let empty_probe = RecordingProbe::default();
        assert_eq!(
            resolve_external(
                OsStr::new("probe"),
                Path::new("/session"),
                &empty_environment,
                &empty_probe,
            ),
            Err(ResolutionError::PathSearchLimitExceeded {
                kind: PathSearchLimitKind::Elements,
                limit: MAX_PATH_SEARCH_ELEMENTS,
            })
        );
        assert!(empty_probe.observed().is_empty());
    }
}
