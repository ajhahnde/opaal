//! External-command resolution and `PATH` lookup.
//!
//! [`resolve_external`] turns an already-expanded native command name into a
//! concrete executable path using the environment's `PATH` and an injected
//! [`ExecutableProbe`]. It performs no filesystem access of its own and never
//! routes a command through `/bin/sh`: the probe is the only capability, so
//! runtime tests supply a fixed set of executable paths. Resolution is
//! span-independent; a command span is attached when it is wired into command
//! planning later.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::Environment;
use crate::command::{CommandRegistry, CommandSignature};

/// Answers whether a native path names an executable file.
///
/// The runtime supplies only this capability to resolution; the real
/// executable, regular-file, and permission checks live in the platform adapter.
pub trait ExecutableProbe {
    /// Whether `path` names a file that can be executed.
    fn is_executable(&self, path: &OsStr) -> bool;
}

/// An `^external` name resolved to a concrete executable path.
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

/// A failure to resolve an `^external` name to an executable.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResolutionError {
    /// No `PATH` element (or the path-like name itself) named an executable.
    /// `name` is the searched command name.
    NotFound {
        /// The command name that could not be resolved.
        name: std::ffi::OsString,
    },
}

/// A resolved command: an internal command's signature or an external executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution<'a> {
    /// A bare name matched a registered internal command.
    Internal(&'a CommandSignature),
    /// A name resolved to an external executable.
    External(ResolvedCommand),
}

/// Resolves a command name against the registry and, on a miss, `PATH`.
///
/// A bare name (`force_external` is `false`) is looked up in `registry` first;
/// only a miss falls back to [`resolve_external`], giving internal commands
/// precedence. An `^external` name (`force_external` is `true`) never consults the
/// registry and resolves externally only. A name whose native bytes are not valid
/// UTF-8 cannot equal an identifier and always resolves externally.
pub fn resolve_command<'a>(
    name: &OsStr,
    force_external: bool,
    registry: &'a CommandRegistry,
    environment: &Environment,
    probe: &dyn ExecutableProbe,
) -> Result<Resolution<'a>, ResolutionError> {
    if !force_external && let Some(signature) = name.to_str().and_then(|name| registry.lookup(name))
    {
        return Ok(Resolution::Internal(signature));
    }
    resolve_external(name, environment, probe).map(Resolution::External)
}

/// Resolves an `^external` command name to an executable path.
///
/// A name containing `/` is path-like and probed unchanged, resolving to itself
/// without consulting `PATH`. A name with no `/` is searched in `PATH` (read as
/// native bytes, split on `:`, with empty elements dropped) in order, and the
/// first candidate the probe accepts wins. A name that resolves nowhere is a
/// [`ResolutionError::NotFound`] carrying the searched name.
pub fn resolve_external(
    name: &OsStr,
    environment: &Environment,
    probe: &dyn ExecutableProbe,
) -> Result<ResolvedCommand, ResolutionError> {
    let not_found = || ResolutionError::NotFound {
        name: name.to_os_string(),
    };

    if name.as_bytes().contains(&b'/') {
        // Path-like: probe the name itself; PATH is never consulted. The
        // working-directory join for a relative name is the spawn adapter's job.
        return if probe.is_executable(name) {
            Ok(ResolvedCommand {
                path: PathBuf::from(name),
            })
        } else {
            Err(not_found())
        };
    }

    let Some(path_value) = environment.get("PATH") else {
        return Err(not_found());
    };

    // Split on `:` over native bytes and drop empty elements; an empty element
    // never denotes the working directory.
    for element in path_value.as_bytes().split(|byte| *byte == b':') {
        if element.is_empty() {
            continue;
        }
        let candidate = Path::new(OsStr::from_bytes(element)).join(name);
        if probe.is_executable(candidate.as_os_str()) {
            return Ok(ResolvedCommand { path: candidate });
        }
    }

    Err(not_found())
}
