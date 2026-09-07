#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_runtime::module::{
    AnalysisControl, ModuleAnalysisOutcome, ModuleCanonicalizer, ModuleId, ModulePathError,
    ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use opaal_syntax::LanguageIdentity;

#[derive(Default)]
struct Sources(BTreeMap<PathBuf, Vec<u8>>);

impl Sources {
    fn with(mut self, path: &str, text: &str) -> Self {
        self.0.insert(PathBuf::from(path), text.as_bytes().to_vec());
        self
    }
}

impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.0
            .contains_key(candidate)
            .then(|| candidate.to_path_buf())
            .ok_or_else(|| ModulePathError::new("source was not mapped"))
    }
}

impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.0
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("source was not mapped"))
    }
}

#[test]
fn explicit_opaal_loading_retains_the_major_on_every_canonical_module() {
    let sources = Sources::default()
        .with(
            "/project/main.opaal",
            "language 1\nimport './library.opaal' as library\nlet answer = 42\n",
        )
        .with("/project/library.opaal", "language 1\nlet value = 1\n");

    let program = ModuleProgramLoader::new(&sources, &sources)
        .load(Path::new("/project/main.opaal"))
        .expect("the explicitly versioned closure is valid");

    assert_eq!(program.graph().root().language(), LanguageIdentity::OpaalV1);
    assert!(
        program
            .sources()
            .entries()
            .all(|entry| entry.module().language() == LanguageIdentity::OpaalV1)
    );
    assert_eq!(
        program
            .sources()
            .entries()
            .map(|entry| entry.script().statements().len())
            .collect::<Vec<_>>(),
        [2, 1]
    );
}

#[test]
fn every_opaal_module_validates_its_own_directive_before_analysis() {
    for (library, code) in [
        ("let value = 1\n", "OP2001"),
        ("language 2\nlet value = 1\n", "OP2003"),
        ("language 3\nlet value = 1\n", "OP2003"),
        ("language 1\nlanguage 1\n", "OP2004"),
    ] {
        let sources = Sources::default()
            .with(
                "/project/main.opaal",
                "language 1\nimport './library.opaal' as library\n",
            )
            .with("/project/library.opaal", library);
        let report =
            ModuleProgramLoader::new(&sources, &sources).analyze(Path::new("/project/main.opaal"));

        assert!(
            report.program().is_none(),
            "invalid closure must not execute"
        );
        assert_eq!(report.issues().len(), 1);
        assert_eq!(report.issues()[0].error().diagnostics()[0].code(), code);
        assert_eq!(
            report.issues()[0].error().module().unwrap().path(),
            Path::new("/project/library.opaal")
        );
    }
}

#[test]
fn controlled_opaal_module_parsing_cancels_without_a_partial_report() {
    let text = format!(
        "language 1\n{}",
        (0..512)
            .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
            .collect::<String>()
    );
    let sources = Sources::default().with("/project/main.opaal", &text);
    let polls = Arc::new(AtomicUsize::new(0));
    let control = AnalysisControl::cooperative({
        let polls = Arc::clone(&polls);
        move || polls.fetch_add(1, Ordering::Relaxed) >= 64
    });

    let outcome = ModuleProgramLoader::new(&sources, &sources)
        .analyze_controlled(Path::new("/project/main.opaal"), &control);

    assert_eq!(outcome, ModuleAnalysisOutcome::Cancelled);
    assert!(polls.load(Ordering::Relaxed) >= 65);
}
