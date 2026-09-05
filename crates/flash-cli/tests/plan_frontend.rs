#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use flash_cli::plan::{PlanHost, PlanRequest, inspect_host_source, inspect_source};
use flash_runtime::Environment;
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use flash_runtime::resolve::ExecutableProbe;

#[derive(Default)]
struct FakeFilesystem {
    canonical: BTreeMap<PathBuf, Result<PathBuf, String>>,
    sources: BTreeMap<PathBuf, Result<Vec<u8>, String>>,
    canonical_calls: AtomicUsize,
    source_calls: AtomicUsize,
}

impl FakeFilesystem {
    fn source(mut self, requested: impl Into<PathBuf>, contents: impl Into<Vec<u8>>) -> Self {
        let requested = requested.into();
        self.canonical
            .insert(requested.clone(), Ok(requested.clone()));
        self.sources.insert(requested, Ok(contents.into()));
        self
    }

    fn unreadable(mut self, requested: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        let requested = requested.into();
        self.canonical
            .insert(requested.clone(), Ok(requested.clone()));
        self.sources.insert(requested, Err(message.into()));
        self
    }
}

impl ModuleCanonicalizer for FakeFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.canonical_calls.fetch_add(1, Ordering::SeqCst);
        self.canonical
            .get(candidate)
            .cloned()
            .unwrap_or_else(|| Err("no canonical mapping".to_owned()))
            .map_err(ModulePathError::new)
    }
}

impl ModuleSourceLoader for FakeFilesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.source_calls.fetch_add(1, Ordering::SeqCst);
        self.sources
            .get(module.path())
            .cloned()
            .unwrap_or_else(|| Err("no source mapping".to_owned()))
            .map_err(ModuleSourceError::new)
    }
}

struct TransientReadFilesystem {
    source_calls: AtomicUsize,
}

impl ModuleCanonicalizer for TransientReadFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for TransientReadFilesystem {
    fn load(&self, _module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        if self.source_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(ModuleSourceError::new("transient read failure"))
        } else {
            Ok(b"language 2\n".to_vec())
        }
    }
}

#[derive(Default)]
struct FakeProbe {
    executable: Vec<OsString>,
    calls: Mutex<Vec<OsString>>,
    cwd_calls: AtomicUsize,
    environment_calls: AtomicUsize,
}

impl FakeProbe {
    fn with(paths: &[&str]) -> Self {
        Self {
            executable: paths.iter().map(OsString::from).collect(),
            calls: Mutex::new(Vec::new()),
            cwd_calls: AtomicUsize::new(0),
            environment_calls: AtomicUsize::new(0),
        }
    }
}

impl PlanHost for FakeProbe {
    fn current_dir(&self) -> Result<PathBuf, String> {
        self.cwd_calls.fetch_add(1, Ordering::SeqCst);
        Ok(PathBuf::from("/work"))
    }

    fn environment(&self) -> Environment {
        self.environment_calls.fetch_add(1, Ordering::SeqCst);
        Environment::from_snapshot([("PATH", "/bin"), ("VALUE", "inherited")])
    }
}

impl ExecutableProbe for FakeProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.calls.lock().unwrap().push(path.to_os_string());
        self.executable.iter().any(|candidate| candidate == path)
    }
}

fn request(source: &str) -> PlanRequest {
    PlanRequest::new(
        PathBuf::from(source),
        PathBuf::from("/work"),
        Environment::from_snapshot([("PATH", "/bin"), ("VALUE", "inherited")]),
    )
}

#[test]
fn one_pipeline_is_analyzed_resolved_preflighted_and_rendered() {
    let filesystem = FakeFilesystem::default().source(
        "/project/command.fsh",
        "^echo 'hello world' | ^cat > output.txt\n",
    );
    let probe = FakeProbe::with(&["/bin/echo", "/bin/cat"]);

    let run = inspect_source(&request("/project/command.fsh"), &filesystem, &probe);

    assert!(run.is_success(), "{:?}", run.rendered_issues());
    assert!(run.rendered_issues().is_empty());
    let plan = run
        .rendered_plan()
        .expect("successful inspection has a plan");
    assert!(plan.starts_with("plan span 0..39\ncwd [/work]\nenv\n"));
    assert!(plan.contains("  [PATH]=[/bin]\n  [VALUE]=[inherited]\n"));
    assert!(plan.contains("external [/bin/echo]"));
    assert!(plan.contains("external [/bin/cat]"));
    assert!(plan.contains("[hello world]"));
    assert!(plan.contains("[output.txt]"));
    assert!(plan.contains("pipefail false\ncapture-limit 8388608\n"));
    assert_eq!(
        probe.calls.lock().unwrap().as_slice(),
        [OsString::from("/bin/echo"), OsString::from("/bin/cat")]
    );
}

#[test]
fn broader_or_status_dependent_source_shapes_are_rejected_before_path_probing() {
    for (label, source) in [
        ("empty", ""),
        ("declaration", "let value = 1\n"),
        ("multiple", "^one\n^two\n"),
        ("conditional", "^one && ^two\n"),
        ("background", "^one &\n"),
        ("expression-stage", "(1 + 2)\n"),
    ] {
        let path = format!("/project/{label}.fsh");
        let filesystem = FakeFilesystem::default().source(&path, source);
        let probe = FakeProbe::with(&["/bin/one", "/bin/two"]);

        let run = inspect_source(&request(&path), &filesystem, &probe);

        assert!(!run.is_success(), "{label}");
        assert!(run.rendered_plan().is_none(), "{label}");
        assert_eq!(run.rendered_issues().len(), 1, "{label}");
        assert!(run.rendered_issues()[0].starts_with("error[PLAN001]"));
        assert!(probe.calls.lock().unwrap().is_empty(), "{label}");
    }
}

#[test]
fn command_substitution_is_diagnosed_instead_of_evaluated() {
    let filesystem =
        FakeFilesystem::default().source("/project/substitution.fsh", "^echo $(^side-effect)\n");
    let probe = FakeProbe::with(&["/bin/echo", "/bin/side-effect"]);

    let run = inspect_source(&request("/project/substitution.fsh"), &filesystem, &probe);

    assert!(!run.is_success());
    assert!(run.rendered_plan().is_none());
    assert_eq!(run.rendered_issues().len(), 1);
    assert!(run.rendered_issues()[0].starts_with("error[PLAN002]"));
    assert!(run.rendered_issues()[0].contains("command substitution in a word"));
    assert_eq!(
        probe.calls.lock().unwrap().as_slice(),
        [OsString::from("/bin/echo")],
        "the nested command is never resolved or executed"
    );
}

#[test]
fn source_and_analysis_failures_are_deterministic_and_emit_no_plan() {
    let unreadable =
        FakeFilesystem::default().unreadable("/project/missing.fsh", "permission denied");
    let run = inspect_source(
        &request("/project/missing.fsh"),
        &unreadable,
        &FakeProbe::default(),
    );
    assert_eq!(
        run.rendered_issues(),
        ["fsh plan: /project/missing.fsh: permission denied\n"]
    );
    assert!(run.rendered_plan().is_none());
    assert_eq!(unreadable.source_calls.load(Ordering::SeqCst), 1);

    let invalid = FakeFilesystem::default().source("/project/invalid.fsh", "^echo |\n");
    let run = inspect_source(
        &request("/project/invalid.fsh"),
        &invalid,
        &FakeProbe::default(),
    );
    assert!(!run.is_success());
    assert!(
        run.rendered_issues()[0].starts_with("error[SYN002]"),
        "{:?}",
        run.rendered_issues()
    );
}

#[test]
fn root_read_failure_is_terminal_before_v1_host_fallback() {
    let filesystem = TransientReadFilesystem {
        source_calls: AtomicUsize::new(0),
    };
    let host = FakeProbe::with(&["/bin/language"]);

    let run = inspect_host_source(Path::new("/project/transient.fsh"), &filesystem, &host);

    assert!(!run.is_success());
    assert!(run.refusal().is_none());
    assert_eq!(
        run.rendered_issues(),
        ["fsh plan: /project/transient.fsh: transient read failure\n"]
    );
    assert_eq!(filesystem.source_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.cwd_calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.environment_calls.load(Ordering::SeqCst), 0);
    assert!(host.calls.lock().unwrap().is_empty());
}

#[test]
fn flash_2_planning_refuses_before_any_ambient_host_observation() {
    let filesystem =
        FakeFilesystem::default().source("/project/v2.fsh", "language 2\n\n^tool must-not-run\n");
    let host = FakeProbe::with(&["/bin/tool"]);

    let run = inspect_host_source(Path::new("/project/v2.fsh"), &filesystem, &host);

    assert!(!run.is_success());
    assert!(run.rendered_plan().is_none());
    assert_eq!(run.rendered_issues().len(), 1);
    assert!(run.rendered_issues()[0].starts_with("error[PLAN004]"));
    assert!(run.rendered_issues()[0].contains("explicit authority and controlled-planning"));
    let refusal = run
        .refusal()
        .expect("Flash 2 planning is a structured refusal");
    assert_eq!(
        refusal.reason(),
        flash_runtime::outcome::RefusalReason::Unsupported
    );
    assert_eq!(refusal.operation(), "Flash 2 execution planning");
    assert_eq!(refusal.span().start(), 0);
    assert_eq!(refusal.span().end(), "language 2".len());
    assert_eq!(host.cwd_calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.environment_calls.load(Ordering::SeqCst), 0);
    assert!(host.calls.lock().unwrap().is_empty());
    assert_eq!(filesystem.canonical_calls.load(Ordering::SeqCst), 1);
    assert_eq!(filesystem.source_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn frozen_flash_1_planning_takes_one_lazy_host_snapshot() {
    let filesystem = FakeFilesystem::default().source("/project/v1.fsh", "^tool\n");
    let host = FakeProbe::with(&["/bin/tool"]);

    let run = inspect_host_source(Path::new("/project/v1.fsh"), &filesystem, &host);

    assert!(run.is_success(), "{:?}", run.rendered_issues());
    assert!(run.refusal().is_none());
    assert_eq!(host.cwd_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.environment_calls.load(Ordering::SeqCst), 1);
    assert_eq!(filesystem.canonical_calls.load(Ordering::SeqCst), 1);
    assert_eq!(filesystem.source_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.calls.lock().unwrap().as_slice(),
        [OsString::from("/bin/tool")]
    );
}
