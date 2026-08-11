#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use flash_cli::cli::FormatOperation;
use flash_cli::format::{FileInspection, FormatFilesystem, FormatRequest, format_files};
use flash_syntax::{
    Diagnostic, FormatOutcome, Severity, SourceFile, SourceId, format_source, render_diagnostic,
};

const DEFAULT_PERMISSIONS: u32 = 0o644;

#[derive(Clone, Debug)]
struct Entry {
    canonical_identity: PathBuf,
    bytes: Vec<u8>,
    permissions: u32,
    inspect_error: Option<String>,
    read_error: Option<String>,
}

impl Entry {
    fn regular(path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            canonical_identity: path.into(),
            bytes: bytes.into(),
            permissions: DEFAULT_PERMISSIONS,
            inspect_error: None,
            read_error: None,
        }
    }

    fn with_identity(mut self, identity: impl Into<PathBuf>) -> Self {
        self.canonical_identity = identity.into();
        self
    }

    fn with_permissions(mut self, permissions: u32) -> Self {
        self.permissions = permissions;
        self
    }

    fn with_inspect_error(mut self, message: impl Into<String>) -> Self {
        self.inspect_error = Some(message.into());
        self
    }

    fn with_read_error(mut self, message: impl Into<String>) -> Self {
        self.read_error = Some(message.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Inspect(PathBuf),
    Read(PathBuf),
    Replace {
        path: PathBuf,
        expected: Vec<u8>,
        replacement: Vec<u8>,
        permissions: u32,
    },
}

#[derive(Default)]
struct FakeFilesystem {
    entries: BTreeMap<PathBuf, Entry>,
    replacement_errors: BTreeMap<PathBuf, String>,
    calls: Vec<Call>,
}

impl FakeFilesystem {
    fn insert(&mut self, path: impl Into<PathBuf>, entry: Entry) {
        self.entries.insert(path.into(), entry);
    }

    fn fail_replacement(&mut self, path: impl Into<PathBuf>, message: impl Into<String>) {
        self.replacement_errors.insert(path.into(), message.into());
    }

    fn replacements(&self) -> Vec<&Call> {
        self.calls
            .iter()
            .filter(|call| matches!(call, Call::Replace { .. }))
            .collect()
    }
}

impl FormatFilesystem for FakeFilesystem {
    fn inspect(&mut self, path: &Path) -> io::Result<FileInspection> {
        self.calls.push(Call::Inspect(path.to_path_buf()));
        let entry = self
            .entries
            .get(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))?;
        if let Some(message) = &entry.inspect_error {
            return Err(io::Error::other(message.clone()));
        }
        Ok(FileInspection::new(
            entry.canonical_identity.clone(),
            entry.permissions,
        ))
    }

    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.calls.push(Call::Read(path.to_path_buf()));
        let entry = self
            .entries
            .get(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))?;
        if let Some(message) = &entry.read_error {
            return Err(io::Error::other(message.clone()));
        }
        Ok(entry.bytes.clone())
    }

    fn replace_atomically(
        &mut self,
        path: &Path,
        expected: &[u8],
        replacement: &[u8],
        permissions: u32,
    ) -> io::Result<()> {
        self.calls.push(Call::Replace {
            path: path.to_path_buf(),
            expected: expected.to_vec(),
            replacement: replacement.to_vec(),
            permissions,
        });
        if let Some(message) = self.replacement_errors.get(path) {
            return Err(io::Error::other(message.clone()));
        }
        self.entries
            .get_mut(path)
            .expect("replacement target was inspected")
            .bytes = replacement.to_vec();
        Ok(())
    }
}

#[test]
fn check_accumulates_all_failures_and_noncanonical_sources_in_operand_order() {
    let mut filesystem = FakeFilesystem::default();
    filesystem.insert(
        "canonical.fsh",
        Entry::regular("identity/canonical", b"echo ready\n".to_vec()),
    );
    filesystem.insert(
        "noncanonical.fsh",
        Entry::regular("identity/noncanonical", b"echo   spaced".to_vec()),
    );
    filesystem.insert("empty.fsh", Entry::regular("identity/empty", Vec::new()));
    filesystem.insert(
        "unicode-docs.fsh",
        Entry::regular(
            "identity/unicode-docs",
            "## Café docs\ndef   greet() { echo   'Grüße' }\n"
                .as_bytes()
                .to_vec(),
        ),
    );
    filesystem.insert(
        "incomplete.fsh",
        Entry::regular("identity/incomplete", b"echo \"".to_vec()),
    );
    filesystem.insert(
        "invalid.fsh",
        Entry::regular("identity/invalid", b"| broken\n".to_vec()),
    );
    filesystem.insert(
        "non-utf8.fsh",
        Entry::regular("identity/non-utf8", vec![0xff, b'!']),
    );
    filesystem.insert(
        "unreadable.fsh",
        Entry::regular("identity/unreadable", Vec::new()).with_read_error("permission denied"),
    );
    filesystem.insert(
        "directory.fsh",
        Entry::regular("identity/directory", Vec::new())
            .with_inspect_error("not a regular file: directory"),
    );
    filesystem.insert(
        "symlink.fsh",
        Entry::regular("identity/symlink", Vec::new())
            .with_inspect_error("final path component is a symlink"),
    );
    filesystem.insert(
        "late-noncanonical.fsh",
        Entry::regular(
            "identity/late-noncanonical",
            b"echo   still-visited".to_vec(),
        ),
    );
    filesystem.insert(
        "alias.fsh",
        Entry::regular("identity/alias-placeholder", b"echo ready\n".to_vec())
            .with_identity("identity/canonical"),
    );

    let paths = [
        "canonical.fsh",
        "noncanonical.fsh",
        "empty.fsh",
        "unicode-docs.fsh",
        "incomplete.fsh",
        "invalid.fsh",
        "non-utf8.fsh",
        "unreadable.fsh",
        "directory.fsh",
        "symlink.fsh",
        "late-noncanonical.fsh",
        "alias.fsh",
    ];
    let run = format_files(
        &FormatRequest::new(FormatOperation::Check, paths.map(PathBuf::from)),
        &mut filesystem,
    );

    assert!(!run.is_success());
    assert_eq!(run.changed_count(), 3);
    assert_eq!(
        failure_paths(&run),
        vec![
            PathBuf::from("noncanonical.fsh"),
            PathBuf::from("unicode-docs.fsh"),
            PathBuf::from("incomplete.fsh"),
            PathBuf::from("invalid.fsh"),
            PathBuf::from("non-utf8.fsh"),
            PathBuf::from("unreadable.fsh"),
            PathBuf::from("directory.fsh"),
            PathBuf::from("symlink.fsh"),
            PathBuf::from("late-noncanonical.fsh"),
            PathBuf::from("alias.fsh"),
        ]
    );
    assert!(run.failures()[0].rendered().contains("error[FMT001]"));
    assert!(run.failures()[1].rendered().contains("error[FMT001]"));
    assert!(run.failures()[2].rendered().contains("error[SYN002]"));
    assert!(
        run.failures()[3]
            .rendered()
            .contains("pipeline operator cannot begin a stage")
    );
    assert!(run.failures()[4].rendered().contains("byte 0"));
    assert_eq!(
        run.failures()[5].rendered(),
        "fsh format: unreadable.fsh: read: permission denied\n"
    );
    assert!(run.failures()[6].rendered().contains("not a regular file"));
    assert!(run.failures()[7].rendered().contains("symlink"));
    assert!(run.failures()[8].rendered().contains("error[FMT001]"));
    assert!(run.failures()[9].rendered().contains("canonical.fsh"));
    assert!(filesystem.replacements().is_empty());
}

#[test]
fn write_preflight_failure_prevents_every_replacement() {
    let mut filesystem = FakeFilesystem::default();
    filesystem.insert(
        "first.fsh",
        Entry::regular("identity/first", b"echo   first".to_vec()),
    );
    filesystem.insert(
        "broken.fsh",
        Entry::regular("identity/broken", b"echo \"".to_vec()),
    );
    filesystem.insert(
        "last.fsh",
        Entry::regular("identity/last", b"echo   last".to_vec()),
    );

    let run = format_files(
        &FormatRequest::new(
            FormatOperation::Write,
            ["first.fsh", "broken.fsh", "last.fsh"].map(PathBuf::from),
        ),
        &mut filesystem,
    );

    assert!(!run.is_success());
    assert_eq!(failure_paths(&run), vec![PathBuf::from("broken.fsh")]);
    assert!(filesystem.replacements().is_empty());
}

#[test]
fn write_skips_unchanged_sources_and_replaces_changed_sources_in_order() {
    let mut filesystem = FakeFilesystem::default();
    filesystem.insert(
        "unchanged.fsh",
        Entry::regular("identity/unchanged", b"echo one\n".to_vec()).with_permissions(0o640),
    );
    filesystem.insert(
        "first.fsh",
        Entry::regular("identity/first", b"echo   two".to_vec()).with_permissions(0o600),
    );
    filesystem.insert(
        "second.fsh",
        Entry::regular(
            "identity/second",
            "## docs\necho   'Grüße'\n".as_bytes().to_vec(),
        )
        .with_permissions(0o755),
    );

    let run = format_files(
        &FormatRequest::new(
            FormatOperation::Write,
            ["unchanged.fsh", "first.fsh", "second.fsh"].map(PathBuf::from),
        ),
        &mut filesystem,
    );

    assert!(run.is_success());
    assert_eq!(run.changed_count(), 2);
    assert!(run.failures().is_empty());
    assert_eq!(
        filesystem.replacements(),
        vec![
            &Call::Replace {
                path: PathBuf::from("first.fsh"),
                expected: b"echo   two".to_vec(),
                replacement: b"echo two\n".to_vec(),
                permissions: 0o600,
            },
            &Call::Replace {
                path: PathBuf::from("second.fsh"),
                expected: "## docs\necho   'Grüße'\n".as_bytes().to_vec(),
                replacement: "## docs\necho 'Grüße'\n".as_bytes().to_vec(),
                permissions: 0o755,
            },
        ]
    );
}

#[test]
fn write_stops_at_the_first_replacement_failure() {
    let mut filesystem = FakeFilesystem::default();
    for name in ["first.fsh", "failing.fsh", "untouched.fsh"] {
        filesystem.insert(
            name,
            Entry::regular(
                format!("identity/{name}"),
                format!("echo   {name}").into_bytes(),
            ),
        );
    }
    filesystem.fail_replacement("failing.fsh", "atomic rename failed");

    let run = format_files(
        &FormatRequest::new(
            FormatOperation::Write,
            ["first.fsh", "failing.fsh", "untouched.fsh"].map(PathBuf::from),
        ),
        &mut filesystem,
    );

    assert!(!run.is_success());
    assert_eq!(failure_paths(&run), vec![PathBuf::from("failing.fsh")]);
    assert_eq!(
        run.failures()[0].rendered(),
        "fsh format: failing.fsh: replace: atomic rename failed\n"
    );
    assert_eq!(
        filesystem
            .replacements()
            .into_iter()
            .map(|call| match call {
                Call::Replace { path, .. } => path.clone(),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        vec![PathBuf::from("first.fsh"), PathBuf::from("failing.fsh")]
    );
    assert_eq!(
        filesystem.entries[Path::new("untouched.fsh")].bytes,
        b"echo   untouched.fsh"
    );
}

#[test]
fn fmt001_anchors_the_first_changed_scalar_and_an_end_insertion() {
    for (path, original) in [
        ("changed-scalar.fsh", "echo  value\n"),
        ("end-insertion.fsh", "echo value"),
    ] {
        let mut filesystem = FakeFilesystem::default();
        filesystem.insert(
            path,
            Entry::regular(format!("identity/{path}"), original.as_bytes().to_vec()),
        );

        let run = format_files(
            &FormatRequest::new(FormatOperation::Check, [PathBuf::from(path)]),
            &mut filesystem,
        );
        let expected = expected_fmt001(path, original);

        assert_eq!(run.failures().len(), 1);
        assert_eq!(run.failures()[0].rendered(), expected);
    }
}

#[test]
fn incomplete_and_invalid_sources_reuse_existing_syntax_diagnostics_exactly() {
    for (path, text, expected) in [
        (
            "incomplete.fsh",
            "echo \"",
            expected_incomplete("incomplete.fsh", "echo \""),
        ),
        (
            "invalid.fsh",
            "| broken\n",
            expected_invalid("invalid.fsh", "| broken\n"),
        ),
    ] {
        let mut filesystem = FakeFilesystem::default();
        filesystem.insert(
            path,
            Entry::regular(format!("identity/{path}"), text.as_bytes().to_vec()),
        );

        let run = format_files(
            &FormatRequest::new(FormatOperation::Check, [PathBuf::from(path)]),
            &mut filesystem,
        );

        assert_eq!(run.failures().len(), 1);
        assert_eq!(run.failures()[0].rendered(), expected);
    }
}

fn failure_paths(run: &flash_cli::format::FormatRun) -> Vec<PathBuf> {
    run.failures()
        .iter()
        .map(|failure| failure.path().to_path_buf())
        .collect()
}

fn expected_fmt001(path: &str, original: &str) -> String {
    let source = SourceFile::new(SourceId::new(1), path, original);
    let FormatOutcome::Complete(canonical) = format_source(&source) else {
        panic!("FMT001 fixture must be complete");
    };
    let difference = first_difference(original, &canonical);
    let end = original[difference..]
        .chars()
        .next()
        .map_or(difference, |scalar| difference + scalar.len_utf8());
    let diagnostic = Diagnostic::new(
        Severity::Error,
        "FMT001",
        "source is not canonically formatted",
    )
    .with_primary(
        source.span(difference..end).unwrap(),
        "formatting first differs here",
    )
    .with_note(format!(
        "run `fsh format --write -- {path}` to rewrite this source"
    ));
    render_diagnostic(&source, &diagnostic).unwrap()
}

fn expected_incomplete(path: &str, text: &str) -> String {
    let source = SourceFile::new(SourceId::new(1), path, text);
    let FormatOutcome::Incomplete(incomplete) = format_source(&source) else {
        panic!("incomplete fixture must stay incomplete");
    };
    let diagnostic = Diagnostic::new(
        Severity::Error,
        "SYN002",
        format!("incomplete input: {}", incomplete.reason()),
    )
    .with_primary(
        incomplete.span(),
        "input ends before this construct is complete",
    );
    render_diagnostic(&source, &diagnostic).unwrap()
}

fn expected_invalid(path: &str, text: &str) -> String {
    let source = SourceFile::new(SourceId::new(1), path, text);
    let FormatOutcome::Invalid(diagnostics) = format_source(&source) else {
        panic!("invalid fixture must stay invalid");
    };
    diagnostics
        .iter()
        .map(|diagnostic| render_diagnostic(&source, diagnostic).unwrap())
        .collect()
}

fn first_difference(original: &str, canonical: &str) -> usize {
    original
        .char_indices()
        .zip(canonical.char_indices())
        .find_map(|((offset, original), (_, canonical))| (original != canonical).then_some(offset))
        .unwrap_or_else(|| original.len().min(canonical.len()))
}
