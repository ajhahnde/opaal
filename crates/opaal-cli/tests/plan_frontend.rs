#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use opaal_cli::plan::inspect_source;
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};

#[derive(Default)]
struct Sources(BTreeMap<PathBuf, Vec<u8>>);

impl Sources {
    fn with(mut self, path: &str, source: &str) -> Self {
        self.0
            .insert(PathBuf::from(path), source.as_bytes().to_vec());
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
fn valid_opaal_is_refused_before_any_host_capability_can_be_supplied() {
    let sources = Sources::default().with("/project/main.opaal", "language 1\nlet value = 1\n");
    let run = inspect_source(Path::new("/project/main.opaal"), &sources);

    assert!(!run.is_success());
    assert!(run.refusal().is_some());
    assert_eq!(run.rendered_issues().len(), 1);
    assert!(run.rendered_issues()[0].contains("error[PLAN004]"));
    assert!(run.rendered_issues()[0].contains("OPAAL 1 execution planning"));
}

#[test]
fn missing_directive_never_falls_back_to_flash_analysis() {
    let sources = Sources::default().with("/project/main.opaal", "let value = 1\n");
    let run = inspect_source(Path::new("/project/main.opaal"), &sources);

    assert!(run.refusal().is_none());
    assert_eq!(run.rendered_issues().len(), 1);
    assert!(run.rendered_issues()[0].contains("error[OP2001]"));
}
