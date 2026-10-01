#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opaal_cli::check::{CheckRequest, check_source};
use opaal_runtime::module::{
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
fn a_clean_program_is_silent_with_only_module_filesystem_capabilities() {
    let filesystem = FakeFilesystem::default()
        .resolves("/project/main.opaal", "/project/main.opaal")
        .contains("/project/main.opaal", "let ready = true\nready\n");

    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.opaal")),
        &filesystem,
    );

    assert!(run.is_success(), "{:?}", run.rendered_issues());
    assert!(!run.has_errors());
    assert!(run.rendered_issues().is_empty());
    assert_eq!(
        filesystem.calls(),
        vec![
            Call::Canonicalize(PathBuf::from("/project/main.opaal")),
            Call::Load(PathBuf::from("/project/main.opaal")),
        ]
    );
}

#[test]
fn explicit_external_programs_are_checked_with_only_module_filesystem_capabilities() {
    let filesystem = FakeFilesystem::default()
        .resolves("/project/main.opaal", "/project/main.opaal")
        .contains("/project/main.opaal", "^tool\n");

    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.opaal")),
        &filesystem,
    );

    assert!(run.is_success(), "{:?}", run.rendered_issues());
    assert_eq!(
        filesystem.calls(),
        vec![
            Call::Canonicalize(PathBuf::from("/project/main.opaal")),
            Call::Load(PathBuf::from("/project/main.opaal")),
        ]
    );
}

#[test]
fn path_qualifies_unspanned_root_resolution_and_source_failures() {
    let unresolved = FakeFilesystem::default().rejects_path("missing.opaal", "permission denied");
    let run = check_source(
        &CheckRequest::new(PathBuf::from("missing.opaal")),
        &unresolved,
    );

    assert_eq!(
        run.rendered_issues(),
        ["opaal check: missing.opaal: permission denied\n"]
    );

    let unreadable = FakeFilesystem::default()
        .resolves("source.opaal", "/project/source.opaal")
        .rejects_read("/project/source.opaal", "input/output error");
    let run = check_source(
        &CheckRequest::new(PathBuf::from("source.opaal")),
        &unreadable,
    );

    assert_eq!(
        run.rendered_issues(),
        ["opaal check: /project/source.opaal: input/output error\n"]
    );
}

#[test]
fn path_qualifies_an_unspanned_root_utf8_failure() {
    let filesystem = FakeFilesystem::default()
        .resolves("source.opaal", "/project/source.opaal")
        .contains("/project/source.opaal", vec![b'l', b'e', b't', b' ', 0xff]);

    let run = check_source(
        &CheckRequest::new(PathBuf::from("source.opaal")),
        &filesystem,
    );

    assert_eq!(
        run.rendered_issues(),
        ["opaal check: /project/source.opaal: invalid UTF-8 at byte 4\n"]
    );
}

#[test]
fn opaal_checker_shares_alias_reexport_and_nominal_identity_without_loading_std() {
    let filesystem = FakeFilesystem::default()
        .resolves("/project/main.opaal", "/project/main.opaal")
        .resolves("/project/model.opaal", "/project/model.opaal")
        .contains(
            "/project/main.opaal",
            concat!(
                "\n",
                "import './model.opaal' as model\n",
                "import std::value as values\n",
                "export { model, values }\n",
            ),
        )
        .contains("/project/model.opaal", "\ntype Item = { value: Int, }\n");

    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.opaal")),
        &filesystem,
    );

    assert!(run.is_success(), "{:?}", run.rendered_issues());
    assert_eq!(
        filesystem.calls(),
        vec![
            Call::Canonicalize(PathBuf::from("/project/main.opaal")),
            Call::Load(PathBuf::from("/project/main.opaal")),
            Call::Canonicalize(PathBuf::from("/project/model.opaal")),
            Call::Load(PathBuf::from("/project/model.opaal")),
        ]
    );
}

#[test]
fn opaal_checker_rejects_unknown_standard_modules_and_alias_conflicts() {
    let unknown = FakeFilesystem::default()
        .resolves("/project/main.opaal", "/project/main.opaal")
        .contains("/project/main.opaal", "\nimport std::missing as missing\n");
    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.opaal")),
        &unknown,
    );
    assert!(run.has_errors());
    assert!(run.rendered_issues()[0].starts_with("error[MOD010]"));

    let conflict = FakeFilesystem::default()
        .resolves("/project/main.opaal", "/project/main.opaal")
        .resolves("/project/model.opaal", "/project/model.opaal")
        .contains(
            "/project/main.opaal",
            concat!(
                "\n",
                "import './model.opaal' as duplicate\n",
                "import std::value as duplicate\n",
            ),
        )
        .contains("/project/model.opaal", "");
    let run = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.opaal")),
        &conflict,
    );
    assert!(run.has_errors());
    assert!(run.rendered_issues()[0].starts_with("error[MOD011]"));
}

#[test]
fn complete_report_and_policy_checks_load_only_the_explicit_source_graph() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/data-processing");
    let report = std::fs::read(fixtures.join("report.opaal")).unwrap();
    for name in [
        "json-report.opaal",
        "text-report.opaal",
        "policy-operations.opaal",
    ] {
        let root = PathBuf::from("/project").join(name);
        let filesystem = FakeFilesystem::default()
            .resolves(&root, &root)
            .resolves("/project/report.opaal", "/project/report.opaal")
            .contains(&root, std::fs::read(fixtures.join(name)).unwrap())
            .contains("/project/report.opaal", report.clone());
        let run = check_source(&CheckRequest::new(root.clone()), &filesystem);
        assert!(run.is_success(), "{name}: {:?}", run.rendered_issues());
        let mut expected = vec![Call::Canonicalize(root.clone()), Call::Load(root)];
        if name != "policy-operations.opaal" {
            expected.extend([
                Call::Canonicalize(PathBuf::from("/project/report.opaal")),
                Call::Load(PathBuf::from("/project/report.opaal")),
            ]);
        }
        assert_eq!(filesystem.calls(), expected);
    }
}

#[test]
fn complete_report_compiled_help_matches_the_resolved_operation_descriptors() {
    use opaal_runtime::help::{ModuleHelpCatalog, ModuleHelpKind, render_module_operation_help};
    use opaal_runtime::module::ModuleProgramLoader;

    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/data-processing");
    let root = PathBuf::from("/project/report.opaal");
    let filesystem = FakeFilesystem::default()
        .resolves(&root, &root)
        .contains(&root, std::fs::read(fixtures.join("report.opaal")).unwrap());
    let program = ModuleProgramLoader::new(&filesystem, &filesystem)
        .load(&root)
        .unwrap();
    let module = program.graph().root();
    let help = ModuleHelpCatalog::snapshot(&program);
    for name in [
        "string::trim",
        "string::split",
        "string::join",
        "string::decode_utf8",
        "list::map",
        "list::filter",
        "list::fold",
        "list::sort_by",
        "list::take",
        "record::get_or",
        "record::select",
        "record::set",
        "data::json_encode",
    ] {
        let descriptor = program
            .resolve_operation(module, &name.split("::").collect::<Vec<_>>())
            .unwrap();
        let entry = help.query(module, name).unwrap();
        assert_eq!(entry.kind(), ModuleHelpKind::Operation);
        assert_eq!(entry.operation().unwrap().id(), descriptor.id());
        let rendered =
            String::from_utf8(render_module_operation_help(entry.operation().unwrap())).unwrap();
        for signature in descriptor.signature_labels() {
            assert!(rendered.contains(&signature), "{name}: {rendered}");
        }
        assert!(rendered.contains(descriptor.documentation()));
    }
    assert_eq!(
        filesystem.calls(),
        [Call::Canonicalize(root.clone()), Call::Load(root)]
    );
}
