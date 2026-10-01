#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use opaal_cli::check::{CheckRequest, CheckRun, check_source, check_source_controlled};
use opaal_cli::report::{HostExit, HostReport, write_report};
use opaal_runtime::module::{
    AnalysisControl, AnalysisLimitKind, AnalysisLimits, ModuleCanonicalizer, ModuleId,
    ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use serde_json::{Value, json};

const COMPLETE: &[u8] = b"{\"schema_version\":1,\"outcome\":\"complete\",\"diagnostics\":[]}\n";
const ROOT: &str = "/project/main.opaal";

fn document(bytes: &[u8], outcome: &str) -> Value {
    assert!(bytes.ends_with(b"\n"));
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
    let value: Value = serde_json::from_slice(bytes).unwrap();
    let fields = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(fields, ["diagnostics", "outcome", "schema_version"]);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], outcome);
    for diagnostic in value["diagnostics"].as_array().unwrap() {
        assert_eq!(
            diagnostic
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "code",
                "message",
                "notes",
                "primary_message",
                "range",
                "related",
                "severity",
                "source"
            ]
        );
        assert!(!diagnostic["code"].as_str().unwrap().is_empty());
        assert!(matches!(
            diagnostic["severity"].as_str().unwrap(),
            "error" | "warning" | "note"
        ));
        assert!(diagnostic["message"].is_string());
        assert!(
            diagnostic["primary_message"].is_null() || diagnostic["primary_message"].is_string()
        );
        assert!(
            diagnostic["notes"]
                .as_array()
                .unwrap()
                .iter()
                .all(Value::is_string)
        );
        location(diagnostic);
        for related in diagnostic["related"].as_array().unwrap() {
            assert_eq!(related.as_object().unwrap().len(), 3);
            assert!(related["message"].is_string());
            location(related);
        }
    }
    value
}

fn location(value: &Value) {
    assert!(value["source"].is_null() || value["source"].is_string());
    if !value["range"].is_null() {
        assert_eq!(value["range"].as_object().unwrap().len(), 2);
        assert!(value["source"].is_string());
        assert!(
            value["range"]["start"].as_u64().unwrap() <= value["range"]["end"].as_u64().unwrap()
        );
    }
}

fn cli(arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(arguments)
        .output()
        .unwrap()
}

fn json_output(output: &Output, outcome: &str) -> Value {
    assert_eq!(
        output.status.code(),
        Some(i32::from(outcome != "complete")),
        "{output:?}"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    document(&output.stdout, outcome)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "opaal-check-json-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn source(&self, name: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[derive(Default)]
struct Filesystem {
    sources: BTreeMap<PathBuf, Result<Vec<u8>, &'static str>>,
    calls: RefCell<Vec<PathBuf>>,
    cancel_on: Option<(PathBuf, Arc<AtomicBool>)>,
}

impl Filesystem {
    fn source(mut self, path: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.sources.insert(PathBuf::from(path), Ok(bytes.into()));
        self
    }

    fn check(&self) -> CheckRun {
        check_source(&CheckRequest::new(PathBuf::from(ROOT)), self)
    }
}

impl ModuleCanonicalizer for Filesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.calls.borrow_mut().push(candidate.to_path_buf());
        if self.sources.contains_key(candidate) {
            Ok(candidate.to_path_buf())
        } else {
            Err(ModulePathError::new("missing explicit source"))
        }
    }
}

impl ModuleSourceLoader for Filesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.calls.borrow_mut().push(module.path().to_path_buf());
        if let Some((path, cancelled)) = &self.cancel_on
            && path == module.path()
        {
            cancelled.store(true, Ordering::SeqCst);
        }
        self.sources
            .get(module.path())
            .unwrap()
            .clone()
            .map_err(ModuleSourceError::new)
    }
}

#[test]
fn successful_json_checks_are_one_document_and_never_execute_or_discover_tools() {
    let temp = TempDir::new();
    let marker = temp.0.join("marker");
    let source = temp.source(
        "main.opaal",
        format!("^touch '{}'\n^not_on_path\n", marker.display()),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args([
            OsString::from("check"),
            OsString::from("--format"),
            OsString::from("json"),
            source.into_os_string(),
        ])
        .env_clear()
        .env("PATH", temp.0.join("absent"))
        .output()
        .unwrap();
    json_output(&output, "complete");
    assert_eq!(output.stdout, COMPLETE);
    assert!(!marker.exists());
}

#[test]
fn source_format_options_preserve_usage_and_native_path_operands() {
    let temp = TempDir::new();
    let source = temp.source("main.opaal", "let ready = true\nready\n");
    for args in [
        vec![
            OsString::from("check"),
            OsString::from("--format"),
            OsString::from("json"),
            source.clone().into_os_string(),
        ],
        vec![
            OsString::from("check"),
            source.clone().into_os_string(),
            OsString::from("--format"),
            OsString::from("json"),
        ],
    ] {
        assert_eq!(
            json_output(&cli(args), "complete")["diagnostics"],
            json!([])
        );
    }
    temp.source("--format.opaal", "");
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(["check", "--format", "json", "--", "--format.opaal"])
        .current_dir(&temp.0)
        .output()
        .unwrap();
    json_output(&output, "complete");
    for args in [
        vec!["check", "--format"],
        vec!["check", "--format", "yaml", "main.opaal"],
        vec!["check", "--format", "json"],
        vec![
            "check",
            "--format",
            "json",
            "--format",
            "json",
            "main.opaal",
        ],
        vec!["check", "--format", "json", "-"],
        vec!["check", "--format", "json", "--", "-"],
        vec!["check", "--format", "json", "main.opaal", "other.opaal"],
        vec!["check", "--format", "json", "--help"],
        vec!["check", "--unknown", "--format", "json", "main.opaal"],
        vec!["plan", "--format", "json", "main.opaal"],
    ] {
        let output = cli(args);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    let output = cli(["check", "--format", "json", "wrong.txt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    let help = cli(["check", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for contract in [
        "opaal check --format json",
        "schema_version 1",
        "UTF-8 byte",
        "Project --format json",
    ] {
        assert!(help.contains(contract), "{help}");
    }
}

#[test]
fn utf8_byte_ranges_and_escaping_are_deterministic_without_source_text() {
    let temp = TempDir::new();
    let content = "let text = 'é🙂 KEEP_SOURCE_PRIVATE'\nmissing\n";
    let source = temp.source("quote\"-line\n-é.opaal", content);
    let output = cli([
        OsString::from("check"),
        OsString::from("--format"),
        OsString::from("json"),
        source.clone().into_os_string(),
    ]);
    let value = json_output(&output, "invalid");
    assert_eq!(value["diagnostics"].as_array().unwrap().len(), 1);
    let diagnostic = &value["diagnostics"][0];
    assert_eq!(diagnostic["code"], "CMD007");
    let start = content.find("missing").unwrap();
    assert_eq!(
        diagnostic["range"],
        json!({"start": start, "end": start + 7})
    );
    assert_eq!(
        diagnostic["source"],
        source.to_str().unwrap().replace('\n', "\\n")
    );
    assert!(
        !String::from_utf8(output.stdout.clone())
            .unwrap()
            .contains("KEEP_SOURCE_PRIVATE")
    );
    let repeat = cli([
        OsString::from("check"),
        source.into_os_string(),
        OsString::from("--format"),
        OsString::from("json"),
    ]);
    assert_eq!(repeat.stdout, output.stdout);
}

#[test]
fn non_utf8_filenames_have_distinct_escaped_display_identities() {
    // Some macOS filesystems reject invalid UTF-8 names. Inject native paths
    // so both qualified hosts exercise the display and argument contract.
    for byte in [0xfe, 0xff] {
        let name = OsString::from_vec([vec![byte], b".opaal".to_vec()].concat());
        let source = Path::new("/project").join(name);
        let mut filesystem = Filesystem::default();
        filesystem
            .sources
            .insert(source.clone(), Ok(b"missing\n".to_vec()));
        let arguments = [
            OsString::from("check"),
            OsString::from("--format"),
            OsString::from("json"),
            source.clone().into_os_string(),
        ];
        assert_eq!(
            opaal_cli::cli::parse_args(arguments).unwrap().mode,
            opaal_cli::cli::Mode::Check {
                source: source.clone(),
                format_json: true
            }
        );
        let run = check_source(&CheckRequest::new(source), &filesystem);
        let value = document(&run.render_json(), "invalid");
        assert_eq!(
            value["diagnostics"][0]["source"],
            format!("/project/\\x{byte:02x}.opaal")
        );
    }
}

#[test]
fn unspanned_root_failures_use_explicit_codes_paths_and_null_ranges() {
    let missing = Filesystem::default().check();
    let mut unreadable = Filesystem::default();
    unreadable
        .sources
        .insert(PathBuf::from(ROOT), Err("read failed \"quoted\"\nline"));
    let utf8 = Filesystem::default()
        .source(ROOT, vec![b'l', b'e', b't', b' ', 0xff])
        .check();
    for (run, outcome, code) in [
        (missing, "failed", "MOD001"),
        (unreadable.check(), "failed", "MOD003"),
        (utf8, "invalid", "MOD004"),
    ] {
        assert!(run.has_errors());
        let value = document(&run.render_json(), outcome);
        let diagnostic = &value["diagnostics"][0];
        assert_eq!(diagnostic["code"], code);
        assert_eq!(diagnostic["source"], ROOT);
        assert!(diagnostic["range"].is_null());
        assert!(diagnostic["primary_message"].is_null());
        assert_eq!(diagnostic["related"], json!([]));
        assert_eq!(diagnostic["notes"], json!([]));
        if code == "MOD004" {
            assert!(diagnostic["message"].as_str().unwrap().contains("byte 4"));
        }
    }
}

#[test]
fn imported_source_failures_keep_shared_codes_and_import_locations() {
    for (bytes, outcome, code) in [
        (Err("read failure"), "failed", "MOD003"),
        (Ok(vec![0xff]), "invalid", "MOD004"),
    ] {
        let mut filesystem = Filesystem::default().source(ROOT, "import './bad.opaal' as bad\n");
        filesystem
            .sources
            .insert(PathBuf::from("/project/bad.opaal"), bytes);
        let run = filesystem.check();
        let value = document(&run.render_json(), outcome);
        let diagnostic = &value["diagnostics"][0];
        assert_eq!(diagnostic["code"], code);
        assert_eq!(diagnostic["source"], ROOT);
        assert!(diagnostic["range"].is_object());
        assert!(diagnostic["primary_message"].is_string());
    }
}

#[test]
fn completed_report_preserves_all_findings_and_failed_precedes_invalid() {
    let filesystem = Filesystem::default()
        .source(
            ROOT,
            "import './bad.opaal' as bad\nimport './absent.opaal' as absent\n",
        )
        .source("/project/bad.opaal", "let = 1\n");
    let run = filesystem.check();
    let value = document(&run.render_json(), "failed");
    let diagnostics = value["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2, "{value}");
    assert_eq!(diagnostics[0]["code"], "OP1000");
    assert_eq!(diagnostics[0]["source"], "/project/bad.opaal");
    assert_eq!(diagnostics[1]["code"], "MOD001");
    assert_eq!(diagnostics[1]["source"], ROOT);
    assert_eq!(run.render_json(), filesystem.check().render_json());
}

#[test]
fn module_cycle_related_labels_are_preserved_in_analysis_order() {
    let filesystem = Filesystem::default()
        .source(ROOT, "import './a.opaal' as a\n")
        .source("/project/a.opaal", "import './main.opaal' as main\n");
    let value = document(&filesystem.check().render_json(), "invalid");
    let diagnostic = &value["diagnostics"][0];
    assert_eq!(diagnostic["code"], "MOD002");
    assert_eq!(diagnostic["source"], "/project/a.opaal");
    let related = diagnostic["related"].as_array().unwrap();
    assert_eq!(related.len(), 1, "{diagnostic}");
    assert_eq!(related[0]["source"], ROOT);
}

#[test]
fn cancellation_discards_partial_findings_and_can_stop_before_host_access() {
    let request = CheckRequest::new(PathBuf::from(ROOT));
    let filesystem = Filesystem::default().source(ROOT, "missing\n");
    let run = check_source_controlled(
        &request,
        &filesystem,
        &AnalysisControl::cooperative(|| true),
        AnalysisLimits::default(),
    );
    assert!(filesystem.calls.borrow().is_empty());
    assert!(run.has_errors());
    let value = document(&run.render_json(), "cancelled");
    assert_eq!(
        value["diagnostics"],
        json!([{
            "code": "CHECKJSON003", "severity": "error", "message": "source analysis cancelled",
            "primary_message": null, "notes": [], "source": null, "range": null, "related": []
        }])
    );

    let cancelled = Arc::new(AtomicBool::new(false));
    let mut filesystem = Filesystem::default()
        .source(
            ROOT,
            "import './bad.opaal' as bad\nimport './stop.opaal' as stop\n",
        )
        .source("/project/bad.opaal", "let = 1\n")
        .source("/project/stop.opaal", "");
    filesystem.cancel_on = Some((PathBuf::from("/project/stop.opaal"), Arc::clone(&cancelled)));
    let run = check_source_controlled(
        &request,
        &filesystem,
        &AnalysisControl::cooperative(move || cancelled.load(Ordering::SeqCst)),
        AnalysisLimits::default(),
    );
    assert_eq!(document(&run.render_json(), "cancelled"), value);
}

#[test]
fn budget_refusal_discards_partial_report_and_preserves_human_failure_text() {
    let request = CheckRequest::new(PathBuf::from(ROOT));
    let filesystem = Filesystem::default()
        .source(
            ROOT,
            "import './bad.opaal' as bad\nimport './extra.opaal' as extra\n",
        )
        .source("/project/bad.opaal", "let = 1\n")
        .source("/project/extra.opaal", "let extra = true\n");
    for limits in [
        AnalysisLimits::default().with_limit(AnalysisLimitKind::WorkUnits, 0),
        AnalysisLimits::default().with_limit(AnalysisLimitKind::SourceBytes, 65),
    ] {
        let run = check_source_controlled(&request, &filesystem, &AnalysisControl::never(), limits);
        let value = document(&run.render_json(), "refused");
        assert!(run.has_errors());
        assert_eq!(value["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(value["diagnostics"][0]["code"], "CHECKJSON004");
        assert!(value["diagnostics"][0]["range"].is_null());
        assert!(
            run.rendered_issues()[0]
                .starts_with("opaal check: /project/main.opaal: OPAAL analysis exceeded the ")
        );
    }
}

#[test]
fn report_fixtures_check_using_only_the_explicit_source_graph() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/data-processing");
    for name in [
        "json-report.opaal",
        "text-report.opaal",
        "policy-operations.opaal",
    ] {
        let root = format!("/project/{name}");
        let filesystem = Filesystem::default()
            .source(&root, fs::read(fixtures.join(name)).unwrap())
            .source(
                "/project/report.opaal",
                fs::read(fixtures.join("report.opaal")).unwrap(),
            );
        let run = check_source(&CheckRequest::new(PathBuf::from(&root)), &filesystem);
        assert_eq!(run.render_json(), COMPLETE);
        let mut calls = vec![PathBuf::from(&root), PathBuf::from(&root)];
        if name != "policy-operations.opaal" {
            calls.extend([
                PathBuf::from("/project/report.opaal"),
                PathBuf::from("/project/report.opaal"),
            ]);
        }
        assert_eq!(*filesystem.calls.borrow(), calls);
    }
}

#[test]
fn human_checks_retain_their_existing_streams_and_diagnostic_bytes() {
    let temp = TempDir::new();
    let valid = temp.source("valid.opaal", "let ready = true\n");
    let output = cli([OsString::from("check"), valid.into_os_string()]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let invalid = temp.source("invalid.opaal", "missing\n");
    let output = cli([OsString::from("check"), invalid.clone().into_os_string()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "error[CMD007]: unknown bare command or callable `missing`\n --> {}:1:1\n  |\n1 | missing\n  | ^^^^^^^ this name cannot fall back to external process lookup\n  = note: use `^missing` for a literal external command or call a declared value\n",
            invalid.display()
        )
    );
    let missing = temp.0.join("absent.opaal");
    let human = cli([OsString::from("check"), missing.clone().into_os_string()]);
    assert_eq!(human.status.code(), Some(1));
    assert!(human.stdout.is_empty());
    assert!(
        String::from_utf8(human.stderr)
            .unwrap()
            .starts_with(&format!("opaal check: {}: ", missing.display()))
    );
    let machine = cli([
        OsString::from("check"),
        OsString::from("--format"),
        OsString::from("json"),
        missing.into_os_string(),
    ]);
    json_output(&machine, "failed");
}

#[test]
fn write_and_flush_failures_cannot_report_success_for_either_outcome() {
    struct Broken {
        flush: bool,
    }
    impl Write for Broken {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.flush {
                Ok(bytes.len())
            } else {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
    for failed in [false, true] {
        for flush in [false, true] {
            let mut output = Broken { flush };
            let mut stderr = Vec::new();
            let report = if failed {
                HostReport::failure_with_output(COMPLETE, b"")
            } else {
                HostReport::success(COMPLETE)
            };
            assert_eq!(
                write_report(report, &mut output, &mut stderr),
                HostExit::Failure
            );
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .starts_with("opaal: cannot write standard output: ")
            );
        }
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    assert_eq!(
        write_report(
            HostReport::failure_with_output(b"{\"outcome\":\"invalid\"}\n", b""),
            &mut stdout,
            &mut stderr
        ),
        HostExit::Failure
    );
    assert_eq!(stdout, b"{\"outcome\":\"invalid\"}\n");
    assert!(stderr.is_empty());
}

#[test]
fn closed_stdout_is_a_diagnosed_nonzero_cli_failure() {
    let temp = TempDir::new();
    for contents in ["let valid = true\n", "missing\n"] {
        let source = temp.source("main.opaal", contents);
        let (output, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
            .args([
                OsString::from("check"),
                OsString::from("--format"),
                OsString::from("json"),
                source.into_os_string(),
            ])
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(output)))
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("opaal: cannot write standard output: ")
        );
    }
}
