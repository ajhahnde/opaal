//! Acceptance tests for canonical module identity and import-cycle analysis.
//!
//! Module analysis is host-free: these tests inject a fixed canonicalizer, so
//! aliases and failures are deterministic and no real filesystem is touched.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use flash_runtime::module::{
    ModuleCanonicalizer, ModuleGraph, ModuleGraphError, ModulePathError, ModuleProgramLoader,
    ModuleResolver, ModuleSourceError, ModuleSourceLoader,
};
use flash_syntax::{LabelStyle, SourceFile, SourceId, Span};

#[derive(Default)]
struct FakeCanonicalizer {
    paths: BTreeMap<PathBuf, Result<PathBuf, ModulePathError>>,
}

impl FakeCanonicalizer {
    fn resolves(mut self, candidate: &str, canonical: &str) -> Self {
        self.paths
            .insert(PathBuf::from(candidate), Ok(PathBuf::from(canonical)));
        self
    }

    fn rejects(mut self, candidate: &str, message: &str) -> Self {
        self.paths
            .insert(PathBuf::from(candidate), Err(ModulePathError::new(message)));
        self
    }
}

impl ModuleCanonicalizer for FakeCanonicalizer {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.paths
            .get(candidate)
            .cloned()
            .unwrap_or_else(|| Err(ModulePathError::new("path was not mapped by the test")))
    }
}

#[derive(Default)]
struct FakeSourceLoader {
    sources: BTreeMap<PathBuf, Result<Vec<u8>, ModuleSourceError>>,
    loads: RefCell<BTreeMap<PathBuf, usize>>,
}

impl FakeSourceLoader {
    fn contains(mut self, path: &str, text: &str) -> Self {
        self.sources
            .insert(PathBuf::from(path), Ok(text.as_bytes().to_vec()));
        self
    }

    fn contains_bytes(mut self, path: &str, bytes: Vec<u8>) -> Self {
        self.sources.insert(PathBuf::from(path), Ok(bytes));
        self
    }

    fn rejects(mut self, path: &str, message: &str) -> Self {
        self.sources
            .insert(PathBuf::from(path), Err(ModuleSourceError::new(message)));
        self
    }

    fn load_count(&self, path: &str) -> usize {
        self.loads
            .borrow()
            .get(Path::new(path))
            .copied()
            .unwrap_or(0)
    }
}

impl ModuleSourceLoader for FakeSourceLoader {
    fn load(&self, module: &flash_runtime::module::ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        *self
            .loads
            .borrow_mut()
            .entry(module.path().to_path_buf())
            .or_default() += 1;
        self.sources
            .get(module.path())
            .cloned()
            .unwrap_or_else(|| Err(ModuleSourceError::new("source was not mapped by the test")))
    }
}

fn source(id: u32, name: &str, text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(id), name, text)
}

fn span(source: &SourceFile, range: std::ops::Range<usize>) -> Span {
    source.span(range).expect("valid test span")
}

#[test]
fn static_imports_recursively_load_a_registered_module_program() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib/math.fsh", "/project/lib/math.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import './lib/math.fsh'\n")
        .contains("/project/lib/math.fsh", "let answer = 42\n");

    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the module program loads");

    assert_eq!(program.graph().modules().count(), 2);
    assert_eq!(program.graph().imports().len(), 1);
    assert_eq!(program.sources().len(), 2);
    assert_eq!(
        program.sources().module(SourceId::new(0)),
        Some(program.graph().root())
    );
    assert_eq!(
        program
            .sources()
            .source(program.graph().root())
            .expect("root source is registered")
            .id(),
        SourceId::new(0)
    );
    assert_eq!(
        program
            .sources()
            .source(program.graph().imports()[0].target())
            .expect("imported source is registered")
            .id(),
        SourceId::new(1)
    );
    assert_eq!(
        program
            .sources()
            .script(program.graph().imports()[0].target())
            .expect("imported syntax is registered")
            .statements()
            .len(),
        1
    );
}

#[test]
fn named_imports_resolve_through_explicit_target_exports() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import { answer } from './lib.fsh'\n")
        .contains("/project/lib.fsh", "let answer = 42\nexport { answer }\n");

    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the named import resolves");
    let root = program.graph().root();
    let import = &program.names().imports(root)[0];
    let target = import.target();
    let export = program
        .names()
        .export(target, "answer")
        .expect("the target exports answer");

    assert_eq!(import.name(), "answer");
    assert_eq!(import.name_span().source_id(), SourceId::new(0));
    assert_eq!(export.name(), "answer");
    assert_eq!(export.declaration_span().source_id(), SourceId::new(1));
    assert_eq!(export.export_span().source_id(), SourceId::new(1));
}

#[test]
fn module_exports_must_name_locals_once() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let unknown = FakeSourceLoader::default().contains("/project/main.fsh", "export { missing }\n");
    let unknown_error = ModuleProgramLoader::new(&paths, &unknown)
        .load(Path::new("/project/main.fsh"))
        .expect_err("an unknown export fails");

    assert_eq!(unknown_error.diagnostics()[0].code(), "MOD005");
    assert_eq!(
        unknown_error.diagnostics()[0].labels()[0]
            .span()
            .source_id(),
        SourceId::new(0)
    );

    let duplicate = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "let answer = 42\nexport { answer, answer }\n",
    );
    let duplicate_error = ModuleProgramLoader::new(&paths, &duplicate)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a duplicate export fails");
    let diagnostic = &duplicate_error.diagnostics()[0];

    assert_eq!(diagnostic.code(), "MOD006");
    assert_eq!(diagnostic.labels().len(), 2);
    assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
    assert_eq!(diagnostic.labels()[1].style(), LabelStyle::Secondary);
}

#[test]
fn private_target_names_cannot_be_imported() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import { answer } from './lib.fsh'\n")
        .contains("/project/lib.fsh", "let answer = 42\n");

    let error = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a private target name is unavailable");
    let diagnostic = &error.diagnostics()[0];

    assert_eq!(diagnostic.code(), "MOD007");
    assert_eq!(
        diagnostic
            .labels()
            .iter()
            .map(|label| label.span().source_id())
            .collect::<Vec<_>>(),
        [SourceId::new(0), SourceId::new(1)]
    );
}

#[test]
fn imported_names_cannot_collide_with_local_or_imported_bindings() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let local_collision = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "let answer = 0\nimport { answer } from './lib.fsh'\n",
        )
        .contains("/project/lib.fsh", "let answer = 42\nexport { answer }\n");
    let local_error = ModuleProgramLoader::new(&paths, &local_collision)
        .load(Path::new("/project/main.fsh"))
        .expect_err("an imported name cannot replace a local");

    assert_eq!(local_error.diagnostics()[0].code(), "MOD008");

    let import_collision = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { answer } from './lib.fsh'\n",
                "import { answer } from './lib.fsh'\n",
            ),
        )
        .contains("/project/lib.fsh", "let answer = 42\nexport { answer }\n");
    let import_error = ModuleProgramLoader::new(&paths, &import_collision)
        .load(Path::new("/project/main.fsh"))
        .expect_err("an imported name cannot be bound twice");

    assert_eq!(import_error.diagnostics()[0].code(), "MOD008");
}

#[test]
fn canonical_aliases_share_one_loaded_and_parsed_source() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib/math.fsh", "/project/lib/math.fsh")
        .resolves("/project/math-alias.fsh", "/project/lib/math.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import './lib/math.fsh'\nimport './math-alias.fsh'\n",
        )
        .contains("/project/lib/math.fsh", "let answer = 42\n");

    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("both aliases load");

    assert_eq!(program.sources().len(), 2);
    assert_eq!(program.graph().modules().count(), 2);
    assert_eq!(program.graph().imports().len(), 2);
    assert_eq!(sources.load_count("/project/lib/math.fsh"), 1);
}

#[test]
fn an_imported_read_failure_is_anchored_to_the_static_path() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/missing.fsh", "/project/missing.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import './missing.fsh'\n")
        .rejects("/project/missing.fsh", "source does not exist");

    let error = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect_err("the imported read fails");
    let diagnostics = error.diagnostics();

    assert_eq!(
        error.module().expect("failed target is retained").path(),
        Path::new("/project/missing.fsh")
    );
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code(), "MOD003");
    assert_eq!(diagnostics[0].labels()[0].style(), LabelStyle::Primary);
    assert_eq!(
        diagnostics[0].labels()[0].span().source_id(),
        SourceId::new(0)
    );
    assert_eq!(diagnostics[0].labels()[0].span().start(), 7);
}

#[test]
fn invalid_utf8_and_parse_failures_identify_the_imported_source() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/bytes.fsh", "/project/bytes.fsh")
        .resolves("/project/syntax.fsh", "/project/syntax.fsh");
    let invalid_bytes = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import './bytes.fsh'\n")
        .contains_bytes("/project/bytes.fsh", vec![b'l', b'e', b't', b' ', 0xff]);
    let utf8_error = ModuleProgramLoader::new(&paths, &invalid_bytes)
        .load(Path::new("/project/main.fsh"))
        .expect_err("invalid UTF-8 fails");

    assert_eq!(utf8_error.diagnostics()[0].code(), "MOD004");
    assert_eq!(
        utf8_error.diagnostics()[0].labels()[0].span().source_id(),
        SourceId::new(0)
    );

    let invalid_syntax = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import './syntax.fsh'\n")
        .contains("/project/syntax.fsh", "let broken = ;\n");
    let syntax_error = ModuleProgramLoader::new(&paths, &invalid_syntax)
        .load(Path::new("/project/main.fsh"))
        .expect_err("invalid imported syntax fails");

    assert_eq!(syntax_error.diagnostics()[0].code(), "FS1000");
    assert_eq!(
        syntax_error.diagnostics()[0].message(),
        "expected an expression"
    );
    assert_eq!(
        syntax_error.diagnostics()[0].labels()[0].span().source_id(),
        SourceId::new(1)
    );
}

#[test]
fn parsed_imports_reuse_multi_source_cycle_diagnostics() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/a.fsh", "/project/a.fsh")
        .resolves("/project/b.fsh", "/project/b.fsh")
        .resolves("/project/c.fsh", "/project/c.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/a.fsh", "import './b.fsh'\n")
        .contains("/project/b.fsh", "import './c.fsh'\n")
        .contains("/project/c.fsh", "import './a.fsh'\n");

    let error = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/a.fsh"))
        .expect_err("the parsed imports close a cycle");
    let diagnostics = error.diagnostics();

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code(), "MOD002");
    assert_eq!(
        diagnostics[0]
            .labels()
            .iter()
            .map(|label| label.span().source_id())
            .collect::<Vec<_>>(),
        [SourceId::new(2), SourceId::new(0), SourceId::new(1)]
    );
}

#[test]
fn relative_requests_are_resolved_from_the_importing_module() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib/math.fsh", "/project/lib/math.fsh");
    let resolver = ModuleResolver::new(&paths);
    let root = resolver
        .resolve_root(Path::new("/project/main.fsh"))
        .expect("root resolves");
    let importing_source = source(1, "/project/main.fsh", "import './lib/math.fsh'");

    let import = resolver
        .resolve_import(
            &root,
            Path::new("./lib/math.fsh"),
            span(&importing_source, 7..23),
        )
        .expect("import resolves");

    assert_eq!(import.importer(), &root);
    assert_eq!(import.requested(), Path::new("./lib/math.fsh"));
    assert_eq!(import.target().path(), Path::new("/project/lib/math.fsh"));
    assert_eq!(import.span(), span(&importing_source, 7..23));
}

#[test]
fn distinct_spellings_share_one_canonical_module_identity() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib/math.fsh", "/project/lib/math.fsh")
        .resolves("/project/alias.fsh", "/project/lib/math.fsh");
    let resolver = ModuleResolver::new(&paths);
    let root = resolver
        .resolve_root(Path::new("/project/main.fsh"))
        .expect("root resolves");
    let importing_source = source(1, "/project/main.fsh", "first second");
    let first = resolver
        .resolve_import(
            &root,
            Path::new("./lib/math.fsh"),
            span(&importing_source, 0..5),
        )
        .expect("first import resolves");
    let second = resolver
        .resolve_import(
            &root,
            Path::new("./alias.fsh"),
            span(&importing_source, 6..12),
        )
        .expect("second import resolves");

    assert_eq!(first.target(), second.target());

    let mut graph = ModuleGraph::new(root);
    graph.add_import(first).expect("first edge is acyclic");
    graph.add_import(second).expect("second edge is acyclic");

    assert_eq!(graph.modules().count(), 2);
    assert_eq!(graph.imports().len(), 2);
}

#[test]
fn resolution_failures_retain_the_importer_request_candidate_and_span() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .rejects("/project/missing.fsh", "source does not exist");
    let resolver = ModuleResolver::new(&paths);
    let root = resolver
        .resolve_root(Path::new("/project/main.fsh"))
        .expect("root resolves");
    let importing_source = source(1, "/project/main.fsh", "missing");
    let request_span = span(&importing_source, 0..7);

    let error = resolver
        .resolve_import(&root, Path::new("./missing.fsh"), request_span)
        .expect_err("missing import fails");

    assert_eq!(error.importer(), Some(&root));
    assert_eq!(error.requested(), Path::new("./missing.fsh"));
    assert_eq!(error.candidate(), Path::new("/project/missing.fsh"));
    assert_eq!(error.span(), Some(request_span));
    assert_eq!(error.cause().message(), "source does not exist");

    let diagnostic = error.diagnostic().expect("an import has a source span");
    assert_eq!(diagnostic.code(), "MOD001");
    assert_eq!(diagnostic.labels().len(), 1);
    assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
    assert_eq!(diagnostic.labels()[0].span(), request_span);
}

#[test]
fn an_indirect_cycle_reports_every_import_edge_in_cycle_order() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/a.fsh", "/project/a.fsh")
        .resolves("/project/b.fsh", "/project/b.fsh")
        .resolves("/project/c.fsh", "/project/c.fsh");
    let resolver = ModuleResolver::new(&paths);
    let a = resolver
        .resolve_root(Path::new("/project/a.fsh"))
        .expect("root resolves");
    let a_source = source(1, "/project/a.fsh", "b");
    let b_source = source(2, "/project/b.fsh", "c");
    let c_source = source(3, "/project/c.fsh", "a");
    let a_to_b = resolver
        .resolve_import(&a, Path::new("./b.fsh"), span(&a_source, 0..1))
        .expect("a -> b resolves");
    let b = a_to_b.target().clone();
    let b_to_c = resolver
        .resolve_import(&b, Path::new("./c.fsh"), span(&b_source, 0..1))
        .expect("b -> c resolves");
    let c = b_to_c.target().clone();
    let c_to_a = resolver
        .resolve_import(&c, Path::new("./a.fsh"), span(&c_source, 0..1))
        .expect("c -> a resolves");

    let mut graph = ModuleGraph::new(a.clone());
    graph.add_import(a_to_b).expect("a -> b is acyclic");
    graph.add_import(b_to_c).expect("b -> c is acyclic");
    let error = graph.add_import(c_to_a).expect_err("c -> a closes a cycle");
    let ModuleGraphError::Cycle(cycle) = error else {
        panic!("expected a cycle error");
    };

    let paths = cycle
        .imports()
        .iter()
        .map(|import| {
            (
                import.importer().path().to_path_buf(),
                import.target().path().to_path_buf(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            (
                PathBuf::from("/project/c.fsh"),
                PathBuf::from("/project/a.fsh")
            ),
            (
                PathBuf::from("/project/a.fsh"),
                PathBuf::from("/project/b.fsh")
            ),
            (
                PathBuf::from("/project/b.fsh"),
                PathBuf::from("/project/c.fsh")
            ),
        ]
    );

    let diagnostic = cycle.diagnostic();
    assert_eq!(diagnostic.code(), "MOD002");
    assert_eq!(diagnostic.labels().len(), 3);
    assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
    assert!(
        diagnostic.labels()[1..]
            .iter()
            .all(|label| label.style() == LabelStyle::Secondary)
    );
}

#[test]
fn a_direct_self_import_is_a_one_edge_cycle() {
    let paths = FakeCanonicalizer::default().resolves("/project/a.fsh", "/project/a.fsh");
    let resolver = ModuleResolver::new(&paths);
    let a = resolver
        .resolve_root(Path::new("/project/a.fsh"))
        .expect("root resolves");
    let a_source = source(1, "/project/a.fsh", "self");
    let self_import = resolver
        .resolve_import(&a, Path::new("./a.fsh"), span(&a_source, 0..4))
        .expect("self path resolves");
    let mut graph = ModuleGraph::new(a);

    let error = graph
        .add_import(self_import)
        .expect_err("a self import is cyclic");
    let ModuleGraphError::Cycle(cycle) = error else {
        panic!("expected a cycle error");
    };

    assert_eq!(cycle.imports().len(), 1);
    assert_eq!(cycle.diagnostic().labels().len(), 1);
    assert!(graph.imports().is_empty());
    assert_eq!(graph.modules().count(), 1);
}

#[test]
fn rejecting_a_cycle_leaves_the_graph_unchanged() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/a.fsh", "/project/a.fsh")
        .resolves("/project/b.fsh", "/project/b.fsh");
    let resolver = ModuleResolver::new(&paths);
    let a = resolver
        .resolve_root(Path::new("/project/a.fsh"))
        .expect("root resolves");
    let a_source = source(1, "/project/a.fsh", "b");
    let b_source = source(2, "/project/b.fsh", "a");
    let a_to_b = resolver
        .resolve_import(&a, Path::new("./b.fsh"), span(&a_source, 0..1))
        .expect("a -> b resolves");
    let b = a_to_b.target().clone();
    let b_to_a = resolver
        .resolve_import(&b, Path::new("./a.fsh"), span(&b_source, 0..1))
        .expect("b -> a resolves");

    let mut graph = ModuleGraph::new(a);
    graph.add_import(a_to_b).expect("a -> b is acyclic");
    let modules_before = graph.modules().cloned().collect::<Vec<_>>();
    let imports_before = graph.imports().to_vec();

    assert!(matches!(
        graph.add_import(b_to_a),
        Err(ModuleGraphError::Cycle(_))
    ));
    assert_eq!(graph.modules().cloned().collect::<Vec<_>>(), modules_before);
    assert_eq!(graph.imports(), imports_before);
}
