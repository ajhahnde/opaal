#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use opaal_cli::check::{CheckRequest, check_source};
use opaal_cli::cli::FormatOperation;
use opaal_cli::format::{FileInspection, FormatFilesystem, FormatRequest, format_files};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};

#[derive(Default)]
struct CheckSources {
    sources: BTreeMap<PathBuf, Vec<u8>>,
    loads: RefCell<Vec<PathBuf>>,
}

impl CheckSources {
    fn with(mut self, path: &str, text: &str) -> Self {
        self.sources
            .insert(PathBuf::from(path), text.as_bytes().to_vec());
        self
    }
}

impl ModuleCanonicalizer for CheckSources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.sources
            .contains_key(candidate)
            .then(|| candidate.to_path_buf())
            .ok_or_else(|| ModulePathError::new("source was not mapped"))
    }
}

impl ModuleSourceLoader for CheckSources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.loads.borrow_mut().push(module.path().to_path_buf());
        self.sources
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("source was not mapped"))
    }
}

#[test]
fn opaal_checking_accepts_a_directive_free_module_graph() {
    let sources = CheckSources::default()
        .with(
            "/project/main.opaal",
            "import './library.opaal' as library\n",
        )
        .with("/project/library.opaal", "let value = 1\n");
    let opaal = check_source(
        &CheckRequest::new(PathBuf::from("/project/main.opaal")),
        &sources,
    );

    assert!(!opaal.has_errors());
    assert!(opaal.rendered_issues().is_empty());
}

#[derive(Default)]
struct FormatSources(BTreeMap<PathBuf, Vec<u8>>);

impl FormatFilesystem for FormatSources {
    fn inspect(&mut self, path: &Path) -> io::Result<FileInspection> {
        self.0
            .contains_key(path)
            .then(|| FileInspection::new(path.to_path_buf(), 0o644))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "source was not mapped"))
    }

    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "source was not mapped"))
    }

    fn replace_atomically(
        &mut self,
        path: &Path,
        _expected: &[u8],
        replacement: &[u8],
        _permissions: u32,
    ) -> io::Result<()> {
        self.0.insert(path.to_path_buf(), replacement.to_vec());
        Ok(())
    }
}

#[test]
fn opaal_formatting_never_requires_or_inserts_a_file_directive() {
    let path = PathBuf::from("source.opaal");
    let mut canonical = FormatSources(BTreeMap::from([(
        path.clone(),
        b"let value = 1\n".to_vec(),
    )]));
    let run = format_files(
        &FormatRequest::new(FormatOperation::Check, [path.clone()]),
        &mut canonical,
    );
    assert!(run.is_success());
    assert_eq!(run.changed_count(), 0);

    let mut unformatted = FormatSources(BTreeMap::from([(
        path.clone(),
        b"let value =  { item:1 }\n".to_vec(),
    )]));
    let run = format_files(
        &FormatRequest::new(FormatOperation::Write, [path.clone()]),
        &mut unformatted,
    );
    assert!(run.is_success());
    assert_eq!(run.changed_count(), 1);
    assert_eq!(unformatted.0[&path], b"let value = { item:1 }\n");
}
