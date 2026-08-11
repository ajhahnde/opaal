#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use flash_cli::check::{CheckRequest, check_source};
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Canonicalize(PathBuf),
    Load(PathBuf),
}

#[derive(Default)]
struct FakeFilesystem {
    canonical: BTreeMap<PathBuf, Result<PathBuf, String>>,
    sources: BTreeMap<PathBuf, Result<Vec<u8>, String>>,
    calls: RefCell<Vec<Call>>,
}

impl FakeFilesystem {
    fn resolves(mut self, candidate: impl Into<PathBuf>, canonical: impl Into<PathBuf>) -> Self {
        self.canonical
            .insert(candidate.into(), Ok(canonical.into()));
        self
    }

    fn rejects_path(mut self, candidate: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        self.canonical.insert(candidate.into(), Err(message.into()));
        self
    }

    fn contains(mut self, module: impl Into<PathBuf>, source: impl Into<Vec<u8>>) -> Self {
        self.sources.insert(module.into(), Ok(source.into()));
        self
    }

    fn rejects_read(mut self, module: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        self.sources.insert(module.into(), Err(message.into()));
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

impl ModuleCanonicalizer for FakeFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.calls
            .borrow_mut()
            .push(Call::Canonicalize(candidate.to_path_buf()));
        self.canonical
            .get(candidate)
            .cloned()
            .unwrap_or_else(|| Err("no canonical mapping".to_owned()))
            .map_err(ModulePathError::new)
    }
}

impl ModuleSourceLoader for FakeFilesystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.calls
            .borrow_mut()
            .push(Call::Load(module.path().to_path_buf()));
        self.sources
            .get(module.path())
            .cloned()
            .unwrap_or_else(|| Err("no source mapping".to_owned()))
            .map_err(ModuleSourceError::new)
    }
}

#[test]
fn renders_ordered_multi_source_diagnostics_with_the_standard_registry() {
    let filesystem = FakeFilesystem::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh")
        .contains(
            "/project/main.fsh",
            "import { private } from './lib.fsh'\neach\n",
        )
        .contains("/project/lib.fsh", "let private = 1\nls | ^cat\n");

    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.fsh")),
        &filesystem,
    );

    assert!(run.has_errors());
    assert!(!run.is_success());
    assert_eq!(run.rendered_issues().len(), 3);
    assert!(run.rendered_issues()[0].starts_with("error[MOD007]"));
    assert!(
        run.rendered_issues()[0].contains(" --> /project/main.fsh:1:10"),
        "the importing source owns the primary label"
    );
    assert!(
        run.rendered_issues()[0].contains(" ::: /project/lib.fsh:1:5"),
        "the private declaration keeps its cross-source label"
    );
    assert!(run.rendered_issues()[1].starts_with("error[PIP001]"));
    assert!(run.rendered_issues()[1].contains("/project/main.fsh"));
    assert!(run.rendered_issues()[2].starts_with("error[PIP003]"));
    assert!(run.rendered_issues()[2].contains("/project/lib.fsh"));
    assert_eq!(
        filesystem.calls(),
        vec![
            Call::Canonicalize(PathBuf::from("/project/main.fsh")),
            Call::Load(PathBuf::from("/project/main.fsh")),
            Call::Canonicalize(PathBuf::from("/project/lib.fsh")),
            Call::Load(PathBuf::from("/project/lib.fsh")),
        ]
    );
}

#[test]
fn a_clean_program_is_silent_with_only_module_filesystem_capabilities() {
    let filesystem = FakeFilesystem::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .contains("/project/main.fsh", "echo ready\n");

    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.fsh")),
        &filesystem,
    );

    assert!(run.is_success());
    assert!(!run.has_errors());
    assert!(run.rendered_issues().is_empty());
    assert_eq!(
        filesystem.calls(),
        vec![
            Call::Canonicalize(PathBuf::from("/project/main.fsh")),
            Call::Load(PathBuf::from("/project/main.fsh")),
        ]
    );
}

#[test]
fn path_qualifies_unspanned_root_resolution_and_source_failures() {
    let unresolved = FakeFilesystem::default().rejects_path("missing.fsh", "permission denied");
    let run = check_source(
        &CheckRequest::new(PathBuf::from("missing.fsh")),
        &unresolved,
    );

    assert_eq!(
        run.rendered_issues(),
        ["fsh check: missing.fsh: permission denied\n"]
    );

    let unreadable = FakeFilesystem::default()
        .resolves("source.fsh", "/project/source.fsh")
        .rejects_read("/project/source.fsh", "input/output error");
    let run = check_source(&CheckRequest::new(PathBuf::from("source.fsh")), &unreadable);

    assert_eq!(
        run.rendered_issues(),
        ["fsh check: /project/source.fsh: input/output error\n"]
    );
}

#[test]
fn path_qualifies_an_unspanned_root_utf8_failure() {
    let filesystem = FakeFilesystem::default()
        .resolves("source.fsh", "/project/source.fsh")
        .contains("/project/source.fsh", vec![b'l', b'e', b't', b' ', 0xff]);

    let run = check_source(&CheckRequest::new(PathBuf::from("source.fsh")), &filesystem);

    assert_eq!(
        run.rendered_issues(),
        ["fsh check: /project/source.fsh: invalid UTF-8 at byte 4\n"]
    );
}
