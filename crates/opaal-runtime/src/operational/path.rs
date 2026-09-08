//! Native-byte lexical path operations.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::ModuleError;

/// Normalize `path` lexically without accessing the host.
pub fn normalize(path: &Path) -> Result<PathBuf, ModuleError> {
    if path.as_os_str().is_empty() {
        return Err(ModuleError::invalid("PATH001", "a path cannot be empty"));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                return Err(ModuleError::invalid(
                    "PATH002",
                    "non-Unix path prefixes are unsupported",
                ));
            }
            Component::RootDir => output.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() || output.as_os_str().is_empty() && path.is_absolute() {
                    return Err(ModuleError::invalid(
                        "PATH003",
                        "a path escapes its lexical root",
                    ));
                }
            }
            Component::Normal(value) => output.push(value),
        }
    }
    if output.as_os_str().is_empty() {
        output.push(".");
    }
    Ok(output)
}

/// Join only relative components and require the result to remain beneath root.
pub fn join(
    root: &Path,
    components: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<PathBuf, ModuleError> {
    let root = normalize(root)?;
    if !root.is_absolute() {
        return Err(ModuleError::invalid(
            "PATH004",
            "the containment root must be absolute",
        ));
    }
    let mut result = root.clone();
    for component in components {
        let component = Path::new(component.as_ref());
        if component.is_absolute() {
            return Err(ModuleError::invalid(
                "PATH005",
                "joined components must be relative",
            ));
        }
        result.push(component);
        result = normalize(&result)?;
        if !result.starts_with(&root) {
            return Err(ModuleError::invalid(
                "PATH003",
                "the joined path escapes its lexical root",
            ));
        }
    }
    Ok(result)
}

/// Require an absolute target to be lexically contained by an absolute root.
pub fn contained(root: &Path, target: &Path) -> Result<PathBuf, ModuleError> {
    let root = normalize(root)?;
    let target = normalize(target)?;
    if !root.is_absolute() || !target.is_absolute() || !target.starts_with(&root) {
        return Err(ModuleError::invalid(
            "PATH006",
            "target is outside the lexical root",
        ));
    }
    Ok(target)
}
