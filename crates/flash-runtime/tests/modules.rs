//! Acceptance tests for canonical module identity and import-cycle analysis.
//!
//! Module analysis is host-free: these tests inject a fixed canonicalizer, so
//! aliases and failures are deterministic and no real filesystem is touched.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flash_platform::{FakePlatform, RecordingPlatform};
use flash_runtime::Environment;
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::FakeClock;
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleGraph, ModuleGraphError, ModulePathError, ModuleProgramLoader,
    ModuleReferenceTarget, ModuleResolver, ModuleSourceError, ModuleSourceLoader, ValueType,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_module_program;
use flash_syntax::{LabelStyle, SourceFile, SourceId, Span};

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

struct MarkExecutable;

impl ExecutableProbe for MarkExecutable {
    fn is_executable(&self, path: &OsStr) -> bool {
        path == OsStr::new("/bin/mark")
    }
}

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
fn typed_function_signatures_are_resolved_before_known_calls_are_validated() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let valid_text = "def echo(value: String) -> String { $value }\n";
    let valid_sources = FakeSourceLoader::default().contains("/project/main.fsh", valid_text);
    let program = ModuleProgramLoader::new(&paths, &valid_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the typed function is valid");
    let root = program.graph().root();
    let source = program
        .sources()
        .source(root)
        .expect("the root source is registered");
    let name_start = valid_text.find("echo").expect("function name is present");
    let signature = program
        .types()
        .function(root, span(source, name_start..name_start + "echo".len()))
        .expect("the resolved signature is inspectable by declaration span");

    assert_eq!(signature.name(), "echo");
    assert_eq!(signature.parameters().len(), 1);
    assert_eq!(signature.parameters()[0].name(), "value");
    assert_eq!(signature.parameters()[0].value_type(), &ValueType::String);
    assert_eq!(signature.result(), &ValueType::String);

    let invalid_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "def echo(value: String) -> String { $value }\n",
            "echo(42)\n",
        ),
    );
    let error = ModuleProgramLoader::new(&paths, &invalid_sources)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a known incompatible literal call fails before execution");

    assert_eq!(error.diagnostics()[0].code(), "SIG004");
}

#[test]
fn root_script_arguments_have_a_distinct_typed_analysis_target() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = "let captured = $args\n";
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the root script-argument input resolves");
    let root = program.graph().root();
    let reference = &program.names().references(root)[0];

    assert_eq!(reference.name(), "args");
    assert_eq!(reference.target(), &ModuleReferenceTarget::ScriptArguments);

    let incompatible = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "def accepts(value: Int) -> Int { $value }\naccepts($args)\n",
    );
    let error = ModuleProgramLoader::new(&paths, &incompatible)
        .load(Path::new("/project/main.fsh"))
        .expect_err("script arguments have the known List[String] analysis type");
    assert_eq!(error.diagnostics()[0].code(), "SIG004");
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
fn named_import_references_retain_local_and_cross_file_provenance() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { answer } from './lib.fsh'\nexport RESULT = $answer\n",
        )
        .contains("/project/lib.fsh", "let answer = 42\nexport { answer }\n");

    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the named import reference resolves");
    let root = program.graph().root();
    let references = program.names().references(root);

    assert_eq!(references.len(), 1);
    let reference = &references[0];
    assert_eq!(reference.name(), "answer");
    assert_eq!(
        program
            .sources()
            .source(root)
            .expect("the root source is registered")
            .slice(reference.reference_span())
            .expect("the reference span belongs to the root source"),
        "$answer"
    );
    assert_eq!(
        program.names().reference(root, reference.reference_span()),
        Some(reference)
    );

    let ModuleReferenceTarget::Imported {
        import_span,
        target_module,
        declaration_span,
        export_span,
    } = reference.target()
    else {
        panic!("the reference should resolve through an imported binding");
    };
    assert_eq!(*import_span, program.names().imports(root)[0].name_span());
    assert_eq!(target_module.path(), Path::new("/project/lib.fsh"));
    let target_source = program
        .sources()
        .source(target_module)
        .expect("the target source is registered");
    assert_eq!(
        target_source
            .slice(*declaration_span)
            .expect("the declaration span belongs to the target source"),
        "answer"
    );
    assert_eq!(
        target_source
            .slice(*export_span)
            .expect("the export span belongs to the target source"),
        "answer"
    );
}

#[test]
fn local_references_follow_source_order_shadowing_and_callable_capture() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "let outer = 1\n",
        "if true {\n",
        "    let outer = 2\n",
        "    let inner = $outer\n",
        "}\n",
        "let after = $outer\n",
        "def recurse(value) {\n",
        "    if $value > 0 {\n",
        "        recurse($value - 1)\n",
        "    }\n",
        "    $outer\n",
        "}\n",
        "let closure = {|outer| $outer}\n",
        "recurse(1)\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);

    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("source-ordered scopes resolve");
    let root = program.graph().root();
    let source = program.sources().source(root).expect("root source");
    let references = program.names().references(root);
    let spellings = references
        .iter()
        .map(|reference| source.slice(reference.reference_span()).expect("root span"))
        .collect::<Vec<_>>();

    assert_eq!(
        spellings,
        [
            "$outer", "$outer", "$value", "recurse", "$value", "$outer", "$outer", "recurse"
        ]
    );
    let declaration_starts = references
        .iter()
        .map(|reference| match reference.target() {
            ModuleReferenceTarget::ScriptArguments => panic!("all bindings are source locals"),
            ModuleReferenceTarget::Local {
                declaration_span, ..
            } => declaration_span.start(),
            ModuleReferenceTarget::Imported { .. } => panic!("all bindings are local"),
        })
        .collect::<Vec<_>>();
    assert_ne!(declaration_starts[0], declaration_starts[1]);
    assert_eq!(declaration_starts[1], declaration_starts[5]);
    assert_ne!(declaration_starts[5], declaration_starts[6]);
    assert_eq!(declaration_starts[3], declaration_starts[7]);

    let forward = FakeSourceLoader::default()
        .contains("/project/main.fsh", "let early = $later\nlet later = 1\n");
    let error = ModuleProgramLoader::new(&paths, &forward)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a forward lexical read is unresolved");
    assert_eq!(error.diagnostics()[0].code(), "MOD009");
}

#[test]
fn loop_and_match_bindings_share_their_evaluator_frames() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "let items = [1]\n",
        "for item in $items {\n",
        "    let copy = $item\n",
        "}\n",
        "match 1 {\n",
        "    arm if $arm > 0 => { let copy = $arm }\n",
        "}\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("loop and arm bindings resolve");
    let root = program.graph().root();

    assert_eq!(
        program
            .names()
            .references(root)
            .iter()
            .map(|reference| reference.name())
            .collect::<Vec<_>>(),
        ["items", "item", "arm", "arm"]
    );
}

#[test]
fn expression_word_spread_call_substitution_and_redirection_reads_are_resolved() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "let value = 1\n",
        "mut assigned = 0\n",
        "let items = [$value]\n",
        "let callable = {|| $value}\n",
        "$assigned = $value\n",
        "callable($value)\n",
        "echo \"$value\" ...$items $(echo $value) >\"$value\"\n",
        "let indexed = $items[0].size\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("all lexical read forms resolve");
    let root = program.graph().root();
    let source = program.sources().source(root).expect("root source");

    assert_eq!(
        program
            .names()
            .references(root)
            .iter()
            .map(|reference| source.slice(reference.reference_span()).expect("root span"))
            .collect::<Vec<_>>(),
        [
            "$value",
            "$value",
            "$assigned",
            "$value",
            "callable",
            "$value",
            "$value",
            "...$items",
            "$value",
            "$value",
            "$items",
        ]
    );
}

#[test]
fn distinct_identifier_namespaces_are_not_lexical_references() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "let record = {key: 1}\n",
        "let member = $record.key\n",
        "let symbol = free_symbol\n",
        "let typed: Int = 1\n",
        "def typed_function(argument: String) -> String { $argument }\n",
        "export ENV_NAME = 1\n",
        "unset ENV_NAME\n",
        "literal-command literal-argument\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("non-lexical identifier positions remain excluded");
    let root = program.graph().root();

    assert_eq!(
        program
            .names()
            .references(root)
            .iter()
            .map(|reference| reference.name())
            .collect::<Vec<_>>(),
        ["record", "argument"]
    );
}

#[test]
fn unknown_references_fail_in_root_named_and_load_only_modules() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/named.fsh", "/project/named.fsh")
        .resolves("/project/dormant.fsh", "/project/dormant.fsh");
    let cases = [
        FakeSourceLoader::default()
            .contains("/project/main.fsh", "if false { let value = $missing }\n"),
        FakeSourceLoader::default()
            .contains("/project/main.fsh", "import { value } from './named.fsh'\n")
            .contains(
                "/project/named.fsh",
                "let value = $missing\nexport { value }\n",
            ),
        FakeSourceLoader::default()
            .contains("/project/main.fsh", "import './dormant.fsh'\n")
            .contains("/project/dormant.fsh", "let value = $missing\n"),
    ];

    for sources in cases {
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("every loaded module is statically resolved");
        assert_eq!(error.diagnostics()[0].code(), "MOD009");
        assert_eq!(
            error.diagnostics()[0].labels()[0].span().source_id(),
            error
                .module()
                .and_then(|module| match module.path().to_str() {
                    Some("/project/main.fsh") => Some(SourceId::new(0)),
                    Some(_) => Some(SourceId::new(1)),
                    None => None,
                })
                .expect("the name failure identifies its module")
        );
    }
}

#[test]
fn same_scope_duplicate_bindings_fail_while_child_shadowing_remains_valid() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    for text in [
        "let value = 1\nlet value = 2\n",
        "if true {\n    let value = 1\n    let value = 2\n}\n",
        "def duplicate(value, value) { $value }\n",
        "let duplicate = {|value, value| $value}\n",
        "for value in [1] { let value = 2 }\n",
    ] {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("a repeated binding in one scope fails");
        let diagnostic = &error.diagnostics()[0];
        assert_eq!(diagnostic.code(), "MOD010", "{text}");
        assert_eq!(diagnostic.labels().len(), 2, "{text}");
    }

    let shadowing = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "let value = 1\nif true { let value = 2\nlet copy = $value }\n",
    );
    ModuleProgramLoader::new(&paths, &shadowing)
        .load(Path::new("/project/main.fsh"))
        .expect("a child scope may shadow an outer binding");
}

#[test]
fn named_import_initializes_a_scalar_export_while_load_only_imports_stay_dormant() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh")
        .resolves("/project/dormant.fsh", "/project/dormant.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { answer } from './lib.fsh'\n",
                "import './dormant.fsh'\n",
                "export RESULT = $answer\n",
            ),
        )
        .contains("/project/lib.fsh", "let answer = 42\nexport { answer }\n")
        .contains("/project/dormant.fsh", "export BROKEN = 1\n");
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the module program loads");
    let mut environment = Environment::new();

    execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect("the named dependency initializes and the load-only sibling stays dormant");

    assert_eq!(environment.get("RESULT"), Some(OsStr::new("42")));
    assert!(!environment.contains("BROKEN"));
}

#[test]
fn script_arguments_are_exact_root_only_immutable_data_with_ordinary_shadowing() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "export EMPTY = $args[0]\n",
            "export UNICODE = $args[1]\n",
            "export OPTION = $args[2]\n",
            "let args = ['shadowed']\n",
            "export SHADOWED = $args[0]\n",
        ),
    );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the root input and its ordinary shadow resolve");
    let root = program.graph().root();
    assert!(matches!(
        program.names().references(root)[0].target(),
        ModuleReferenceTarget::ScriptArguments
    ));
    assert!(matches!(
        program.names().references(root)[3].target(),
        ModuleReferenceTarget::Local { .. }
    ));
    let mut environment = Environment::new();

    execute_module_program(
        &program,
        &[String::new(), "Grüße 🌍".to_owned(), "--flag".to_owned()],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect("the root script arguments execute as exact strings");

    assert_eq!(environment.get("EMPTY"), Some(OsStr::new("")));
    assert_eq!(environment.get("UNICODE"), Some(OsStr::new("Grüße 🌍")));
    assert_eq!(environment.get("OPTION"), Some(OsStr::new("--flag")));
    assert_eq!(environment.get("SHADOWED"), Some(OsStr::new("shadowed")));

    let immutable_sources =
        FakeSourceLoader::default().contains("/project/main.fsh", "$args = []\n");
    let immutable_program = ModuleProgramLoader::new(&paths, &immutable_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("assignment target resolution succeeds before runtime mutability checking");
    let error = execute_module_program(
        &immutable_program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("the script-argument input cannot be assigned");
    assert!(error.render().contains("binding \"args\" is immutable"));
}

#[test]
fn dependency_modules_cannot_resolve_the_root_script_arguments() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import { value } from './lib.fsh'\n")
        .contains("/project/lib.fsh", "let value = $args\nexport { value }\n");
    let error = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a dependency does not inherit the root program input");

    assert_eq!(error.diagnostics()[0].code(), "MOD009");
    assert_eq!(
        error.module().expect("failing module").path(),
        Path::new("/project/lib.fsh")
    );
}

#[test]
fn named_dependencies_initialize_once_in_source_edge_depth_first_postorder() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/left.fsh", "/project/left.fsh")
        .resolves("/project/right.fsh", "/project/right.fsh")
        .resolves("/project/shared.fsh", "/project/shared.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { left } from './left.fsh'\n",
                "import { right } from './right.fsh'\n",
                "^/bin/mark root\n",
                "export ROOT_RAN = true\n",
            ),
        )
        .contains(
            "/project/left.fsh",
            concat!(
                "import { shared } from './shared.fsh'\n",
                "^/bin/mark left\n",
                "let left = $shared\n",
                "export { left }\n",
                "export LEFT_RAN = true\n",
            ),
        )
        .contains(
            "/project/right.fsh",
            concat!(
                "import { shared } from './shared.fsh'\n",
                "^/bin/mark right\n",
                "let right = $shared\n",
                "export { right }\n",
                "export RIGHT_RAN = true\n",
            ),
        )
        .contains(
            "/project/shared.fsh",
            concat!(
                "^/bin/mark shared\n",
                "let shared = 42\n",
                "export { shared }\n",
                "export SHARED_RAN = true\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the diamond module program loads");
    let mut environment = Environment::new();
    let platform = RecordingPlatform::new(FakePlatform::full());

    execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &MarkExecutable,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
    )
    .expect("the diamond initializes successfully");

    let labels = platform
        .spawn_log()
        .records()
        .into_iter()
        .map(|record| record.argv()[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        ["shared", "left", "right", "root"].map(OsString::from)
    );
    assert_eq!(environment.get("SHARED_RAN"), Some(OsStr::new("true")));
    assert_eq!(environment.get("LEFT_RAN"), Some(OsStr::new("true")));
    assert_eq!(environment.get("RIGHT_RAN"), Some(OsStr::new("true")));
    assert_eq!(environment.get("ROOT_RAN"), Some(OsStr::new("true")));
}

#[test]
fn imported_mut_values_materialize_after_initialization_as_immutable_snapshots() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { answer } from './lib.fsh'\nexport RESULT = $answer\n",
        )
        .contains(
            "/project/lib.fsh",
            "mut answer = 41\n$answer = 42\nexport { answer }\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the module program loads");
    let mut environment = Environment::new();

    execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect("the completed mutable export is materialized");

    assert_eq!(environment.get("RESULT"), Some(OsStr::new("42")));

    let immutable_root = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { answer } from './lib.fsh'\n$answer = 7\n",
        )
        .contains("/project/lib.fsh", "mut answer = 42\nexport { answer }\n");
    let immutable_program = ModuleProgramLoader::new(&paths, &immutable_root)
        .load(Path::new("/project/main.fsh"))
        .expect("the immutable-import program loads");
    let error = execute_module_program(
        &immutable_program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("an importer cannot assign through the snapshot");

    assert!(error.render().contains("binding \"answer\" is immutable"));
}

#[test]
fn imported_callables_execute_against_their_defining_source() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { increment } from './lib.fsh'\nexport RESULT = increment(41)\n",
        )
        .contains(
            "/project/lib.fsh",
            "def increment(value) {\n    $value + 1\n}\nexport { increment }\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the callable module program loads");
    let mut environment = Environment::new();

    execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect("the imported callable uses its defining source");

    assert_eq!(environment.get("RESULT"), Some(OsStr::new("42")));
}

#[test]
fn imported_callable_failures_render_the_body_and_importing_call_site() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { boom } from './lib.fsh'\nexport RESULT = boom()\n",
        )
        .contains(
            "/project/lib.fsh",
            "def boom() {\n    1 + true\n}\nexport { boom }\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the callable module program loads");
    let error = execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("the imported callable body fails");
    let rendered = error.render();

    assert!(rendered.contains(" --> /project/lib.fsh:2:5"), "{rendered}");
    assert!(rendered.contains("1 + true"), "{rendered}");
    assert!(
        rendered.contains(" ::: /project/main.fsh:2:17"),
        "{rendered}"
    );
    assert!(rendered.contains("boom()"), "{rendered}");
}

#[test]
fn initializer_failure_stops_later_modules_after_preserving_externalized_effects() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/failing.fsh", "/project/failing.fsh")
        .resolves("/project/later.fsh", "/project/later.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { failing } from './failing.fsh'\n",
                "import { later } from './later.fsh'\n",
                "^/bin/mark root\n",
            ),
        )
        .contains(
            "/project/failing.fsh",
            concat!(
                "let failing = 1\n",
                "export { failing }\n",
                "^/bin/mark failing\n",
                "export BROKEN = 1 + true\n",
            ),
        )
        .contains(
            "/project/later.fsh",
            "let later = 2\nexport { later }\n^/bin/mark later\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the module program loads");
    let platform = RecordingPlatform::new(FakePlatform::full());
    let error = execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &MarkExecutable,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
    )
    .expect_err("the first initializer fails");

    let labels = platform
        .spawn_log()
        .records()
        .into_iter()
        .map(|record| record.argv()[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(labels, [OsString::from("failing")]);
    assert!(error.render().contains("/project/failing.fsh"));
}

#[test]
fn initializer_exit_terminates_the_whole_program_before_the_root() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/dependency.fsh", "/project/dependency.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { value } from './dependency.fsh'\n^/bin/mark root\n",
        )
        .contains(
            "/project/dependency.fsh",
            concat!(
                "let value = 1\n",
                "export { value }\n",
                "^/bin/mark dependency\n",
                "exit 7\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the module program loads");
    let platform = RecordingPlatform::new(FakePlatform::full());
    let completion = execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &MarkExecutable,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
    )
    .expect("the explicit exit is a normal program completion");

    assert_eq!(
        completion.status().and_then(|status| status.code()),
        Some(7)
    );
    let labels = platform
        .spawn_log()
        .records()
        .into_iter()
        .map(|record| record.argv()[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(labels, [OsString::from("dependency")]);
}

#[test]
fn dependency_background_jobs_live_until_the_whole_program_join() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/dependency.fsh", "/project/dependency.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { value } from './dependency.fsh'\n^/bin/mark root\n",
        )
        .contains(
            "/project/dependency.fsh",
            concat!(
                "let value = 1\n",
                "export { value }\n",
                "^/bin/mark background &\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the module program loads");
    let platform = RecordingPlatform::new(FakePlatform::full());

    execute_module_program(
        &program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &MarkExecutable,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
    )
    .expect("the dependency job is joined at whole-program completion");

    let labels = platform
        .spawn_log()
        .records()
        .into_iter()
        .map(|record| record.argv()[1].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        [OsString::from("background"), OsString::from("root")]
    );
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
