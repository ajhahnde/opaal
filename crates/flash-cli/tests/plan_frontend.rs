#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use flash_cli::plan::{PlanRequest, inspect_source};
use flash_runtime::Environment;
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use flash_runtime::resolve::ExecutableProbe;

#[derive(Default)]
struct FakeFilesystem {
    canonical: BTreeMap<PathBuf, Result<PathBuf, String>>,
    sources: BTreeMap<PathBuf, Result<Vec<u8>, String>>,
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
        self.canonical
            .get(candidate)
            .cloned()
            .unwrap_or_else(|| Err("no canonical mapping".to_owned()))
            .map_err(ModulePathError::new)
    }
}

impl ModuleSourceLoader for FakeFilesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.sources
            .get(module.path())
            .cloned()
            .unwrap_or_else(|| Err("no source mapping".to_owned()))
            .map_err(ModuleSourceError::new)
    }
}

#[derive(Default)]
struct FakeProbe {
    executable: Vec<OsString>,
    calls: Mutex<Vec<OsString>>,
}

impl FakeProbe {
    fn with(paths: &[&str]) -> Self {
        Self {
            executable: paths.iter().map(OsString::from).collect(),
            calls: Mutex::new(Vec::new()),
        }
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
