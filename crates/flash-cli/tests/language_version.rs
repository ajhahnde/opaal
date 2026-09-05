#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use flash_cli::check::{CheckRequest, check_source};
use flash_cli::cli::FormatOperation;
use flash_cli::format::{FileInspection, FormatFilesystem, FormatRequest, format_files};
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use flash_syntax::LanguageMajor;

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
fn v2_checking_validates_each_file_while_v1_checking_stays_unversioned() {
    let sources = CheckSources::default()
        .with(
            "/project/main.fsh",
            "language 2\nimport './library.fsh' as library\n",
        )
        .with("/project/library.fsh", "let value = 1\n");
    let v2 = check_source(
        &CheckRequest::for_language(PathBuf::from("/project/main.fsh"), LanguageMajor::V2),
        &sources,
    );

    assert!(v2.has_errors());
    assert_eq!(v2.rendered_issues().len(), 1);
    assert!(v2.rendered_issues()[0].starts_with("error[FS2001]"));
    assert!(v2.rendered_issues()[0].contains("/project/library.fsh"));

    let v1_sources = CheckSources::default().with("/project/main.fsh", "let value = 1\n");
    assert!(
        check_source(
            &CheckRequest::new(PathBuf::from("/project/main.fsh")),
            &v1_sources,
        )
        .is_success()
    );
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
fn v2_formatting_requires_and_retains_the_file_directive() {
    let path = PathBuf::from("source.fsh");
    let mut missing = FormatSources(BTreeMap::from([(
        path.clone(),
        b"let value = 1\n".to_vec(),
    )]));
    let run = format_files(
        &FormatRequest::for_language(FormatOperation::Check, [path.clone()], LanguageMajor::V2),
        &mut missing,
    );
    assert_eq!(run.failures().len(), 1);
    assert!(run.failures()[0].rendered().starts_with("error[FS2001]"));

    let mut versioned = FormatSources(BTreeMap::from([(
        path.clone(),
        b"language   2\nlet value =  { item:1 }\n".to_vec(),
    )]));
    let run = format_files(
        &FormatRequest::for_language(FormatOperation::Write, [path.clone()], LanguageMajor::V2),
        &mut versioned,
    );
    assert!(run.is_success());
    assert_eq!(run.changed_count(), 1);
    assert_eq!(versioned.0[&path], b"language 2\nlet value = { item:1 }\n");
}
