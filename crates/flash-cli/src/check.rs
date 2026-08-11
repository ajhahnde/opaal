//! Host-free orchestration and rendering for non-executing source checks.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use flash_runtime::builtin::standard_registry;
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramError, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use flash_syntax::render_diagnostic_sources;

/// One explicit source-check request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckRequest {
    source: PathBuf,
}

impl CheckRequest {
    /// Creates a request for one root source and its canonical import closure.
    #[must_use]
    pub const fn new(source: PathBuf) -> Self {
        Self { source }
    }

    /// The native root path supplied by the caller.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
}

/// The complete host capability surface available to source checking.
///
/// This composition deliberately contains only canonicalization and finite
/// source loading. Runtime sessions, environments, executable probes,
/// platforms, terminals, configuration, and history cannot enter the checker
/// orchestration API.
pub trait CheckFilesystem: ModuleCanonicalizer + ModuleSourceLoader {}

impl<T> CheckFilesystem for T where T: ModuleCanonicalizer + ModuleSourceLoader {}

/// Read-only host adapter for canonical regular source files.
///
/// Symbolic links are accepted because canonical module identity owns alias
/// collapse. Directories and special files are rejected before source loading.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostCheckFilesystem;

impl ModuleCanonicalizer for HostCheckFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let canonical =
            fs::canonicalize(candidate).map_err(|error| ModulePathError::new(error.to_string()))?;
        let metadata =
            fs::metadata(&canonical).map_err(|error| ModulePathError::new(error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(ModulePathError::new("path is not a regular file"));
        }
        Ok(canonical)
    }
}

impl ModuleSourceLoader for HostCheckFilesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        let mut file =
            File::open(module.path()).map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(ModuleSourceError::new("path is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        Ok(bytes)
    }
}

/// A completed source check without executable output or process-status policy.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CheckRun {
    rendered_issues: Vec<String>,
    has_errors: bool,
}

impl CheckRun {
    /// Ordered diagnostics and path-qualified unspanned source failures.
    #[must_use]
    pub fn rendered_issues(&self) -> &[String] {
        &self.rendered_issues
    }

    /// Whether the analysis found any error-classified issue.
    #[must_use]
    pub const fn has_errors(&self) -> bool {
        self.has_errors
    }

    /// Whether checking completed without an error-classified issue.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        !self.has_errors
    }
}

/// Checks one source and its canonical imports without initializing or
/// executing a runtime session.
#[must_use]
pub fn check_source<F>(request: &CheckRequest, filesystem: &F) -> CheckRun
where
    F: CheckFilesystem,
{
    let commands = standard_registry();
    let report = ModuleProgramLoader::new(filesystem, filesystem)
        .analyze_with_commands(request.source(), &commands);
    let sources = report
        .sources()
        .iter()
        .map(|entry| entry.source())
        .collect::<Vec<_>>();
    let mut rendered_issues = Vec::new();

    for issue in report.issues() {
        let diagnostics = issue.error().diagnostics();
        if diagnostics.is_empty() {
            rendered_issues.push(render_unspanned(request.source(), issue.error()));
            continue;
        }
        rendered_issues.extend(diagnostics.iter().map(|diagnostic| {
            render_diagnostic_sources(sources.iter().copied(), diagnostic)
                .expect("module analysis diagnostics address retained sources")
        }));
    }

    CheckRun {
        rendered_issues,
        has_errors: report.has_errors(),
    }
}

fn render_unspanned(requested: &Path, error: &ModuleProgramError) -> String {
    let (path, cause) = match error {
        ModuleProgramError::Resolution(error) => (error.requested(), error.cause().to_string()),
        ModuleProgramError::SourceRead { module, cause, .. } => (module.path(), cause.to_string()),
        ModuleProgramError::InvalidUtf8 {
            module,
            valid_up_to,
            ..
        } => (
            module.path(),
            format!("invalid UTF-8 at byte {valid_up_to}"),
        ),
        _ => (
            error.module().map_or(requested, |module| module.path()),
            error.to_string(),
        ),
    };
    format!("fsh check: {}: {cause}\n", path.display())
}
