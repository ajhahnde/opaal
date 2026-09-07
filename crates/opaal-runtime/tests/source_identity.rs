#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_runtime::module::{
    AnalysisControl, ModuleAnalysisOutcome, ModuleCanonicalizer, ModuleId, ModulePathError,
    ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use opaal_syntax::StatementKind;

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
fn directive_free_loading_uses_one_identity_for_every_canonical_module() {
    let sources = Sources::default()
        .with(
            "/project/main.opaal",
            "import './library.opaal' as library\nlet answer = 42\n",
        )
        .with("/project/library.opaal", "let value = 1\n");

    let program = ModuleProgramLoader::new(&sources, &sources)
        .load(Path::new("/project/main.opaal"))
        .expect("the directive-free module closure is valid");

    assert_eq!(
        program.graph().root().path(),
        Path::new("/project/main.opaal")
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
fn a_former_header_in_an_import_is_ordinary_program_text() {
    let sources = Sources::default()
        .with(
            "/project/main.opaal",
            "import './library.opaal' as library\n",
        )
        .with("/project/library.opaal", "language 1\nlet value = 1\n");

    let program = ModuleProgramLoader::new(&sources, &sources)
        .load(Path::new("/project/main.opaal"))
        .expect("former header text must not activate compatibility diagnostics");
    let library = program
        .sources()
        .entries()
        .find(|entry| entry.module().path() == Path::new("/project/library.opaal"))
        .unwrap();

    assert_eq!(library.script().statements().len(), 2);
    assert!(matches!(
        library.script().statements()[0].kind(),
        StatementKind::Job(_)
    ));
}

#[test]
fn controlled_opaal_module_parsing_cancels_without_a_partial_report() {
    let text = (0..512)
        .map(|index| format!("let value_{index} = [{index}, {index}]\n"))
        .collect::<String>();
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
