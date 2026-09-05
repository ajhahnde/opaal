//! Non-executing formatter orchestration and host filesystem adaptation.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_syntax::{
    Diagnostic, FormatOutcome, LanguageDetection, LanguageMajor, Severity, SourceFile, SourceId,
    detect_source_language, format_source, format_source_v2, render_diagnostic,
};

use crate::cli::FormatOperation;

const TEMP_ATTEMPTS: u64 = 128;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// An explicit ordered formatter request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatRequest {
    operation: FormatOperation,
    paths: Vec<PathBuf>,
    language: LanguageMajor,
    detect_language: bool,
}

impl FormatRequest {
    pub fn new(operation: FormatOperation, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            operation,
            paths: paths.into_iter().collect(),
            language: LanguageMajor::V1,
            detect_language: false,
        }
    }

    /// Creates a formatter request for one explicitly selected language major.
    pub fn for_language(
        operation: FormatOperation,
        paths: impl IntoIterator<Item = PathBuf>,
        language: LanguageMajor,
    ) -> Self {
        Self {
            operation,
            paths: paths.into_iter().collect(),
            language,
            detect_language: false,
        }
    }

    /// Creates a CLI request that selects each source's declared language.
    #[must_use]
    pub fn detecting_language(
        operation: FormatOperation,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        Self {
            operation,
            paths: paths.into_iter().collect(),
            language: LanguageMajor::V1,
            detect_language: true,
        }
    }

    #[must_use]
    pub const fn operation(&self) -> FormatOperation {
        self.operation
    }

    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    #[must_use]
    pub const fn language(&self) -> LanguageMajor {
        self.language
    }

    const fn selected_language(&self) -> Option<LanguageMajor> {
        if self.detect_language {
            None
        } else {
            Some(self.language)
        }
    }
}

/// Stable identity and supported metadata captured before source reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileInspection {
    canonical_identity: PathBuf,
    permissions: u32,
}

impl FileInspection {
    #[must_use]
    pub const fn new(canonical_identity: PathBuf, permissions: u32) -> Self {
        Self {
            canonical_identity,
            permissions,
        }
    }

    #[must_use]
    pub fn canonical_identity(&self) -> &Path {
        &self.canonical_identity
    }

    #[must_use]
    pub const fn permissions(&self) -> u32 {
        self.permissions
    }
}

/// Filesystem capabilities required by host-free formatter orchestration.
pub trait FormatFilesystem {
    fn inspect(&mut self, path: &Path) -> io::Result<FileInspection>;
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>>;
    fn replace_atomically(
        &mut self,
        path: &Path,
        expected: &[u8],
        replacement: &[u8],
        permissions: u32,
    ) -> io::Result<()>;
}

/// One operand-owned formatter failure ready for stderr.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatFailure {
    path: PathBuf,
    rendered: String,
}

impl FormatFailure {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

/// Completed formatter orchestration without process-status policy.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FormatRun {
    failures: Vec<FormatFailure>,
    changed_count: usize,
}

impl FormatRun {
    #[must_use]
    pub fn failures(&self) -> &[FormatFailure] {
        &self.failures
    }

    #[must_use]
    pub const fn changed_count(&self) -> usize {
        self.changed_count
    }

    #[must_use]
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Debug)]
struct PreparedSource {
    path: PathBuf,
    original: Vec<u8>,
    permissions: u32,
    source: SourceFile,
    canonical: String,
}

impl PreparedSource {
    fn changed(&self) -> bool {
        self.original != self.canonical.as_bytes()
    }
}

/// Formats an ordered request without accessing any capability beyond `filesystem`.
pub fn format_files(request: &FormatRequest, filesystem: &mut dyn FormatFilesystem) -> FormatRun {
    let mut identities = HashMap::<PathBuf, PathBuf>::new();
    let mut preflight = Vec::with_capacity(request.paths.len());

    for (index, path) in request.paths.iter().enumerate() {
        let source_id = SourceId::new(u32::try_from(index + 1).unwrap_or(u32::MAX));
        preflight.push(prepare_source(
            filesystem,
            path,
            source_id,
            request.selected_language(),
            &mut identities,
        ));
    }

    match request.operation {
        FormatOperation::Check => check_preflight(preflight),
        FormatOperation::Write => write_preflight(preflight, filesystem),
    }
}

fn prepare_source(
    filesystem: &mut dyn FormatFilesystem,
    path: &Path,
    source_id: SourceId,
    language: Option<LanguageMajor>,
    identities: &mut HashMap<PathBuf, PathBuf>,
) -> Result<PreparedSource, FormatFailure> {
    let inspection = filesystem
        .inspect(path)
        .map_err(|error| operation_failure(path, "inspect", &error))?;
    if let Some(earlier) = identities.get(inspection.canonical_identity()) {
        return Err(message_failure(
            path,
            "duplicate",
            format!(
                "target duplicates earlier operand {}",
                earlier.to_string_lossy()
            ),
        ));
    }
    identities.insert(
        inspection.canonical_identity().to_path_buf(),
        path.to_path_buf(),
    );

    let original = filesystem
        .read(path)
        .map_err(|error| operation_failure(path, "read", &error))?;
    let source_name = path.to_string_lossy().into_owned();
    let source =
        SourceFile::from_bytes(source_id, source_name, original.clone()).map_err(|error| {
            message_failure(
                path,
                "decode UTF-8",
                format!("invalid UTF-8 at byte {}", error.utf8_error().valid_up_to()),
            )
        })?;
    let language = language.unwrap_or_else(|| detected_language(&source));
    let outcome = match language {
        LanguageMajor::V1 => format_source(&source),
        LanguageMajor::V2 => format_source_v2(&source),
    };
    let canonical = match outcome {
        FormatOutcome::Complete(canonical) => canonical,
        FormatOutcome::Incomplete(incomplete) => {
            let diagnostic = Diagnostic::new(
                Severity::Error,
                "SYN002",
                format!("incomplete input: {}", incomplete.reason()),
            )
            .with_primary(
                incomplete.span(),
                "input ends before this construct is complete",
            );
            return Err(diagnostic_failure(path, &source, &[diagnostic]));
        }
        FormatOutcome::Invalid(diagnostics) => {
            return Err(diagnostic_failure(path, &source, &diagnostics));
        }
    };

    Ok(PreparedSource {
        path: path.to_path_buf(),
        original,
        permissions: inspection.permissions(),
        source,
        canonical,
    })
}

fn detected_language(source: &SourceFile) -> LanguageMajor {
    match detect_source_language(source) {
        LanguageDetection::Complete(directive) => directive.major(),
        LanguageDetection::Invalid(diagnostics)
            if diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() != "FS2001") =>
        {
            LanguageMajor::V2
        }
        LanguageDetection::Invalid(_) => LanguageMajor::V1,
    }
}

fn check_preflight(preflight: Vec<Result<PreparedSource, FormatFailure>>) -> FormatRun {
    let mut run = FormatRun::default();
    for entry in preflight {
        match entry {
            Ok(prepared) if prepared.changed() => {
                run.changed_count += 1;
                run.failures.push(noncanonical_failure(&prepared));
            }
            Ok(_) => {}
            Err(failure) => run.failures.push(failure),
        }
    }
    run
}

fn write_preflight(
    preflight: Vec<Result<PreparedSource, FormatFailure>>,
    filesystem: &mut dyn FormatFilesystem,
) -> FormatRun {
    let failures = preflight
        .iter()
        .filter_map(|entry| entry.as_ref().err().cloned())
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        return FormatRun {
            failures,
            changed_count: 0,
        };
    }

    let mut run = FormatRun::default();
    for prepared in preflight.into_iter().filter_map(Result::ok) {
        if !prepared.changed() {
            continue;
        }
        if let Err(error) = filesystem.replace_atomically(
            &prepared.path,
            &prepared.original,
            prepared.canonical.as_bytes(),
            prepared.permissions,
        ) {
            run.failures
                .push(operation_failure(&prepared.path, "replace", &error));
            break;
        }
        run.changed_count += 1;
    }
    run
}

fn noncanonical_failure(prepared: &PreparedSource) -> FormatFailure {
    let difference = first_difference(prepared.source.text(), &prepared.canonical);
    let end = prepared.source.text()[difference..]
        .chars()
        .next()
        .map_or(difference, |scalar| difference + scalar.len_utf8());
    let diagnostic = Diagnostic::new(
        Severity::Error,
        "FMT001",
        "source is not canonically formatted",
    )
    .with_primary(
        prepared
            .source
            .span(difference..end)
            .expect("first divergence is a source scalar boundary"),
        "formatting first differs here",
    )
    .with_note(format!(
        "run `fsh format --write -- {}` to rewrite this source",
        prepared.path.to_string_lossy()
    ));
    diagnostic_failure(&prepared.path, &prepared.source, &[diagnostic])
}

fn first_difference(original: &str, canonical: &str) -> usize {
    original
        .char_indices()
        .zip(canonical.char_indices())
        .find_map(|((offset, original), (_, canonical))| (original != canonical).then_some(offset))
        .unwrap_or_else(|| original.len().min(canonical.len()))
}

fn diagnostic_failure(
    path: &Path,
    source: &SourceFile,
    diagnostics: &[Diagnostic],
) -> FormatFailure {
    let rendered = diagnostics
        .iter()
        .map(|diagnostic| {
            render_diagnostic(source, diagnostic)
                .expect("formatter diagnostics address their retained source")
        })
        .collect();
    FormatFailure {
        path: path.to_path_buf(),
        rendered,
    }
}

fn operation_failure(path: &Path, operation: &str, error: &io::Error) -> FormatFailure {
    message_failure(path, operation, error.to_string())
}

fn message_failure(path: &Path, operation: &str, message: impl AsRef<str>) -> FormatFailure {
    FormatFailure {
        path: path.to_path_buf(),
        rendered: format!(
            "fsh format: {}: {operation}: {}\n",
            path.to_string_lossy(),
            message.as_ref()
        ),
    }
}

/// Real host adapter implementing explicit regular-file inspection and replacement.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostFormatFilesystem;

impl FormatFilesystem for HostFormatFilesystem {
    fn inspect(&mut self, path: &Path) -> io::Result<FileInspection> {
        inspect_regular_file(path)
    }

    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn replace_atomically(
        &mut self,
        path: &Path,
        expected: &[u8],
        replacement: &[u8],
        permissions: u32,
    ) -> io::Result<()> {
        replace_host_file(path, expected, replacement, permissions)
    }
}

fn inspect_regular_file(path: &Path) -> io::Result<FileInspection> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "final path component is a symlink",
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not a regular file",
        ));
    }
    let canonical_identity = fs::canonicalize(path)?;
    Ok(FileInspection::new(
        canonical_identity,
        metadata.permissions().mode() & 0o7777,
    ))
}

fn replace_host_file(
    path: &Path,
    expected: &[u8],
    replacement: &[u8],
    permissions: u32,
) -> io::Result<()> {
    let current = inspect_regular_file(path)?;
    if current.permissions() != permissions {
        return Err(io::Error::other(
            "target permissions changed since formatter preflight",
        ));
    }
    if fs::read(path)? != expected {
        return Err(io::Error::other(
            "target contents changed since formatter preflight",
        ));
    }

    let (temporary_path, mut temporary) = create_sibling_temporary(path)?;
    finish_replacement(
        path,
        &temporary_path,
        &mut temporary,
        replacement,
        permissions,
    )
}

fn finish_replacement(
    path: &Path,
    temporary_path: &Path,
    temporary: &mut File,
    replacement: &[u8],
    permissions: u32,
) -> io::Result<()> {
    let result = write_and_replace(path, temporary_path, temporary, replacement, permissions);
    if result.is_err() {
        let _ = fs::remove_file(temporary_path);
    }
    result
}

fn create_sibling_temporary(path: &Path) -> io::Result<(PathBuf, File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target path has no final file name",
        )
    })?;
    let start = NEXT_TEMPORARY.fetch_add(TEMP_ATTEMPTS, Ordering::Relaxed);

    for attempt in start..start + TEMP_ATTEMPTS {
        let mut temporary_name = OsString::from(file_name);
        temporary_name.push(format!(".fsh-format.{}.{attempt}.tmp", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot allocate a unique sibling temporary file",
    ))
}

fn write_and_replace(
    target_path: &Path,
    temporary_path: &Path,
    temporary: &mut File,
    replacement: &[u8],
    permissions: u32,
) -> io::Result<()> {
    temporary.set_permissions(fs::Permissions::from_mode(permissions))?;
    temporary.write_all(replacement)?;
    temporary.flush()?;
    temporary.sync_all()?;
    fs::rename(temporary_path, target_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_rename_removes_the_sibling_temporary() {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "flash-format-cleanup-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let temporary_path = root.join("temporary");
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .unwrap();

        let error = finish_replacement(
            &target,
            &temporary_path,
            &mut temporary,
            b"replacement",
            0o600,
        )
        .unwrap_err();

        assert!(matches!(
            error.kind(),
            io::ErrorKind::IsADirectory
                | io::ErrorKind::AlreadyExists
                | io::ErrorKind::PermissionDenied
        ));
        assert!(!temporary_path.exists());
        fs::remove_dir(&target).unwrap();
        fs::remove_dir(&root).unwrap();
    }
}
