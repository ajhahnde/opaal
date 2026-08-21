//! Acceptance tests for canonical module identity and import-cycle analysis.
//!
//! Module analysis is host-free: these tests inject a fixed canonicalizer, so
//! aliases and failures are deterministic and no real filesystem is touched.

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flash_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, FakePlatform, FileActionError, FileIoEndpoint,
    FileOpenRequest, PipeEndpoints, PipeError, Platform, ProcessGroup, ProcessGroupId,
    ProcessStatus, RecordingPlatform, SpawnError, SpawnRequest, TerminateError, WaitError,
    WorkingDirectoryError, WorkingDirectoryRequest,
};
use flash_platform_posix::PosixPlatform;
use flash_runtime::builtin::standard_registry;
use flash_runtime::command::{
    Carrier, CommandLifecycle, CommandNamespaceEntry, CommandRegistry, CommandSignature,
};
use flash_runtime::eval::{FakeClock, RuntimeError, RuntimeErrorKind};
use flash_runtime::module::{
    AnalysisControl, ModuleAnalysisOutcome, ModuleCanonicalizer, ModuleEffect, ModuleGraph,
    ModuleGraphError, ModulePathError, ModuleProgramLoader, ModuleReferenceTarget, ModuleResolver,
    ModuleSourceError, ModuleSourceLoader, ValueType,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::query::{NameKind, SemanticHover};
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_module_program as execute_module_program_with_output;
use flash_runtime::{
    ByteSize, Callable, Duration, Environment, FiniteFloat, NativePath, Range, Record, Status,
    Table, Value,
};
use flash_syntax::{LabelStyle, Severity, SourceFile, SourceId, Span};

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

struct OneExecutable(PathBuf);

impl ExecutableProbe for OneExecutable {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.0.as_os_str() == path
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "flash-modules-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("the unique test directory is created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("the test directory is removed");
    }
}

struct CountingPosix {
    spawns: AtomicUsize,
}

impl CountingPosix {
    fn new() -> Self {
        Self {
            spawns: AtomicUsize::new(0),
        }
    }

    fn spawns(&self) -> usize {
        self.spawns.load(Ordering::Relaxed)
    }
}

impl Platform for CountingPosix {
    fn capabilities(&self) -> Capabilities {
        PosixPlatform.capabilities()
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        PosixPlatform.resolve_working_directory(request)
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        PosixPlatform.pipe()
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        PosixPlatform.open_file(request)
    }

    fn open_file_io(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn FileIoEndpoint>, FileActionError> {
        PosixPlatform.open_file_io(request)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        PosixPlatform.inherit_descriptor(descriptor)
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        self.spawns.fetch_add(1, Ordering::Relaxed);
        PosixPlatform.spawn(request)
    }
}

#[derive(Default)]
struct PrefixThenFailWriter {
    bytes: Vec<u8>,
    remaining: usize,
}

impl PrefixThenFailWriter {
    fn new(remaining: usize) -> Self {
        Self {
            bytes: Vec::new(),
            remaining,
        }
    }
}

impl Write for PrefixThenFailWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "scripted sink failure",
            ));
        }
        let written = self.remaining.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..written]);
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct StatusChild {
    id: u64,
    group: Option<ProcessGroupId>,
    code: i32,
    waits: Arc<AtomicUsize>,
}

impl ChildProcess for StatusChild {
    fn id(&self) -> u64 {
        self.id
    }

    fn process_group(&self) -> Option<ProcessGroupId> {
        self.group
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        self.waits.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessStatus::Exited(self.code))
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

struct StatusPlatform {
    inner: FakePlatform,
    code: i32,
    waits: Arc<AtomicUsize>,
    spawns: Mutex<Vec<StatusSpawn>>,
}

#[derive(Clone)]
struct StatusSpawn {
    argv: Vec<OsString>,
    cwd: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl StatusPlatform {
    fn new(code: i32) -> Self {
        Self {
            inner: FakePlatform::full(),
            code,
            waits: Arc::new(AtomicUsize::new(0)),
            spawns: Mutex::new(Vec::new()),
        }
    }

    fn waits(&self) -> usize {
        self.waits.load(Ordering::Relaxed)
    }

    fn spawns(&self) -> Vec<StatusSpawn> {
        self.spawns.lock().expect("spawn log lock").clone()
    }
}

impl Platform for StatusPlatform {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        self.inner.pipe()
    }

    fn resolve_working_directory(
        &self,
        request: WorkingDirectoryRequest<'_>,
    ) -> Result<PathBuf, WorkingDirectoryError> {
        self.inner.resolve_working_directory(request)
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.inner.open_file(request)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        self.inner.inherit_descriptor(descriptor)
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(10_000);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let group = match request.process_group() {
            ProcessGroup::Inherit => None,
            ProcessGroup::New => ProcessGroupId::new(id),
            ProcessGroup::Join(group) => Some(group),
        };
        self.spawns
            .lock()
            .expect("spawn log lock")
            .push(StatusSpawn {
                argv: request.argv().to_vec(),
                cwd: request.cwd().to_path_buf(),
                environment: request.environment().to_vec(),
            });
        Ok(Box::new(StatusChild {
            id,
            group,
            code: self.code,
            waits: Arc::clone(&self.waits),
        }))
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_module_program(
    program: &flash_runtime::module::ModuleProgram,
    script_arguments: &[String],
    cwd: &Path,
    environment: &mut Environment,
    registry: &flash_runtime::command::CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn flash_runtime::eval::Clock>,
) -> Result<flash_runtime::script::ScriptCompletion, flash_runtime::script::ScriptError> {
    let mut output = Vec::new();
    execute_module_program_with_output(
        program,
        script_arguments,
        cwd,
        environment,
        registry,
        probe,
        options,
        platform,
        clock,
        &mut output,
    )
}

#[derive(Debug)]
struct FamilyCallable(&'static str);

impl Callable for FamilyCallable {
    fn family(&self) -> &'static str {
        self.0
    }

    fn display(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }

    fn as_any(&self) -> &dyn Any {
        self
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

struct CancellingSourceLoader {
    cancelled: Arc<AtomicBool>,
    bytes: Vec<u8>,
    loads: AtomicUsize,
}

struct ArmingSourceLoader {
    armed: Arc<AtomicBool>,
    sources: BTreeMap<PathBuf, Vec<u8>>,
}

impl ModuleSourceLoader for ArmingSourceLoader {
    fn load(&self, module: &flash_runtime::module::ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        let bytes = self
            .sources
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("source was not mapped by the test"))?;
        if module.path() == Path::new("/project/arm.fsh") {
            self.armed.store(true, Ordering::Release);
        }
        Ok(bytes)
    }
}

impl ModuleSourceLoader for CancellingSourceLoader {
    fn load(
        &self,
        _module: &flash_runtime::module::ModuleId,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        self.loads.fetch_add(1, Ordering::Relaxed);
        self.cancelled.store(true, Ordering::Release);
        Ok(self.bytes.clone())
    }
}

#[test]
fn controlled_analysis_cancels_after_source_loading_without_a_partial_report() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let cancelled = Arc::new(AtomicBool::new(false));
    let source_loader = CancellingSourceLoader {
        cancelled: Arc::clone(&cancelled),
        bytes: b"let answer = 42\n".to_vec(),
        loads: AtomicUsize::new(0),
    };
    let control = AnalysisControl::cooperative({
        let cancelled = Arc::clone(&cancelled);
        move || cancelled.load(Ordering::Acquire)
    });

    let outcome = ModuleProgramLoader::new(&paths, &source_loader)
        .analyze_controlled(Path::new("/project/main.fsh"), &control);

    assert_eq!(outcome, ModuleAnalysisOutcome::Cancelled);
    assert_eq!(source_loader.loads.load(Ordering::Relaxed), 1);
}

#[test]
fn controlled_analysis_can_cancel_during_recursive_syntax_work() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = (0..512)
        .map(|index| format!("let value_{index}: List[Int] = [{index}, {index}]\n"))
        .collect::<String>();
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", &text);
    let polls = Arc::new(AtomicUsize::new(0));
    let control = AnalysisControl::cooperative({
        let polls = Arc::clone(&polls);
        move || polls.fetch_add(1, Ordering::Relaxed) >= 256
    });

    let outcome = ModuleProgramLoader::new(&paths, &sources).analyze_with_commands_controlled(
        Path::new("/project/main.fsh"),
        &standard_registry(),
        &control,
    );

    assert_eq!(outcome, ModuleAnalysisOutcome::Cancelled);
    assert_eq!(sources.load_count("/project/main.fsh"), 1);
    assert!(polls.load(Ordering::Relaxed) >= 257);
}

#[test]
fn controlled_analysis_polls_during_recursive_semantic_traversal() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/arm.fsh", "/project/arm.fsh");
    let root = format!(
        "import './arm.fsh'\n{}",
        (0..512)
            .map(|index| format!("let value_{index}: List[Int] = [{index}, {index}]\n"))
            .collect::<String>()
    );
    let armed = Arc::new(AtomicBool::new(false));
    let sources = ArmingSourceLoader {
        armed: Arc::clone(&armed),
        sources: BTreeMap::from([
            (PathBuf::from("/project/main.fsh"), root.into_bytes()),
            (PathBuf::from("/project/arm.fsh"), Vec::new()),
        ]),
    };
    let semantic_polls = Arc::new(AtomicUsize::new(0));
    let control = AnalysisControl::cooperative({
        let armed = Arc::clone(&armed);
        let semantic_polls = Arc::clone(&semantic_polls);
        move || {
            armed.load(Ordering::Acquire) && semantic_polls.fetch_add(1, Ordering::Relaxed) >= 32
        }
    });

    let outcome = ModuleProgramLoader::new(&paths, &sources).analyze_with_commands_controlled(
        Path::new("/project/main.fsh"),
        &standard_registry(),
        &control,
    );

    assert_eq!(outcome, ModuleAnalysisOutcome::Cancelled);
    assert!(semantic_polls.load(Ordering::Relaxed) >= 33);
}

#[test]
fn never_cancelled_control_preserves_the_legacy_analysis_report_exactly() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "let answer: Int = 42\nexport { answer }\n",
    );
    let loader = ModuleProgramLoader::new(&paths, &sources);
    let expected =
        loader.analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());

    assert_eq!(
        loader.analyze_with_commands_controlled(
            Path::new("/project/main.fsh"),
            &standard_registry(),
            &AnalysisControl::never(),
        ),
        ModuleAnalysisOutcome::Complete(Box::new(expected))
    );
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
fn typed_command_capture_drives_analysis_hover_and_word_diagnostics() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "def accept_bytes(value: Bytes) -> Bytes { $value }\n",
        "def accept_text(value: String) -> String { $value }\n",
        "let binary = $(bytes: ^tool)\n",
        "let text = $(text: ^tool)\n",
        "accept_bytes($binary)\n",
        "accept_text($text)\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("capture result types should satisfy matching signatures");
    let root = program.graph().root();
    let binary_reference = text.rfind("$binary").expect("binary reference is present") + 1;
    let text_reference = text.rfind("$text").expect("text reference is present") + 1;
    let registry = standard_registry();
    let queries = program.semantic_queries(&registry);

    let SemanticHover::Binding(binary) = queries
        .hover_at(root, binary_reference)
        .expect("byte capture binding has hover data")
    else {
        panic!("expected binding hover");
    };
    assert_eq!(binary.value_type(), &ValueType::Bytes);
    let SemanticHover::Binding(text_hover) = queries
        .hover_at(root, text_reference)
        .expect("text capture binding has hover data")
    else {
        panic!("expected binding hover");
    };
    assert_eq!(text_hover.value_type(), &ValueType::String);

    for (invalid, code) in [
        (
            concat!(
                "def accept_text(value: String) -> String { $value }\n",
                "accept_text($(bytes: ^tool))\n",
            ),
            "SIG004",
        ),
        ("^tool $(bytes: ^tool)\n", "SIG006"),
    ] {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", invalid);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("an incompatible byte capture must fail static analysis");
        assert_eq!(error.diagnostics()[0].code(), code, "{invalid}");
    }
}

#[test]
fn structured_error_bindings_types_hover_and_throw_diagnostics_agree() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "mut observed = ''\n",
        "try {\n",
        "    throw \"boom\"\n",
        "} catch error {\n",
        "    let typed: Error = $error\n",
        "    $observed = $error.message\n",
        "}\n",
        "$observed\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("structured error bindings should analyze");
    let root = program.graph().root();
    let reference = text
        .find("$error.message")
        .expect("error reference is present")
        + 1;
    let registry = standard_registry();
    let queries = program.semantic_queries(&registry);
    let SemanticHover::Binding(hover) = queries
        .hover_at(root, reference)
        .expect("catch binding has hover data")
    else {
        panic!("expected catch-binding hover");
    };
    assert_eq!(hover.value_type(), &ValueType::Error);

    let invalid = FakeSourceLoader::default().contains("/project/main.fsh", "throw 42\n");
    let error = ModuleProgramLoader::new(&paths, &invalid)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a statically known invalid throw operand must fail analysis");
    assert_eq!(error.diagnostics()[0].code(), "SIG007");
}

#[test]
fn function_documentation_is_normalized_beside_resolved_module_signatures_without_activation() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/library.fsh", "/project/library.fsh");
    let root_text = concat!(
        "import { imported } from './library.fsh'\n",
        "## Local summary π\n",
        "##\n",
        "## Local detail 🚀\n",
        "def local(value: String) -> String { $value }\n",
    );
    let imported_text = concat!(
        "export { imported }\n",
        "## Imported summary\n",
        "def imported(value: Int) -> Int {\n",
        "    ^must-not-run\n",
        "    $value\n",
        "}\n",
    );
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", root_text)
        .contains("/project/library.fsh", imported_text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("documentation is analyzed without activating the imported source");

    let root = program.graph().root();
    let imported = program.graph().imports()[0].target();
    let local = &program.types().functions(root)[0];
    let imported = &program.types().functions(imported)[0];

    assert_eq!(local.name(), "local");
    assert_eq!(local.parameters()[0].value_type(), &ValueType::String);
    assert_eq!(local.result(), &ValueType::String);
    assert_eq!(local.documentation().unwrap().summary(), "Local summary π");
    assert_eq!(
        local.documentation().unwrap().text(),
        "Local summary π\n\nLocal detail 🚀"
    );

    assert_eq!(imported.name(), "imported");
    assert_eq!(imported.parameters()[0].value_type(), &ValueType::Int);
    assert_eq!(imported.result(), &ValueType::Int);
    assert_eq!(imported.documentation().unwrap().text(), "Imported summary");
}

#[test]
fn every_builtin_type_spelling_resolves_in_source_annotations() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "def all_types(any_value: Any, null_value: Null, bool_value: Bool, int_value: Int, ",
        "float_value: Float, string_value: String, bytes_value: Bytes, path_value: Path, ",
        "duration_value: Duration, size_value: ByteSize, strings: List[String], ",
        "nested: List[List[Int]], record_value: Record, table_value: Table, range_value: Range, ",
        "status_value: Status, error_value: Error, function_value: Function, ",
        "closure_value: Closure) -> Any { null }\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("every built-in type spelling resolves");
    let root = program.graph().root();
    let source = program
        .sources()
        .source(root)
        .expect("the root source is registered");
    let name_start = text.find("all_types").expect("function name is present");
    let signature = program
        .types()
        .function(
            root,
            span(source, name_start..name_start + "all_types".len()),
        )
        .expect("the resolved signature is inspectable");

    let expected = vec![
        ValueType::Any,
        ValueType::Null,
        ValueType::Bool,
        ValueType::Int,
        ValueType::Float,
        ValueType::String,
        ValueType::Bytes,
        ValueType::Path,
        ValueType::Duration,
        ValueType::ByteSize,
        ValueType::List(Box::new(ValueType::String)),
        ValueType::List(Box::new(ValueType::List(Box::new(ValueType::Int)))),
        ValueType::Record,
        ValueType::Table,
        ValueType::Range,
        ValueType::Status,
        ValueType::Error,
        ValueType::Function,
        ValueType::Closure,
    ];
    assert_eq!(
        signature
            .parameters()
            .iter()
            .map(|parameter| parameter.value_type().clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(signature.result(), &ValueType::Any);
}

#[test]
fn expression_intrinsics_and_double_quoted_values_reach_assembled_scripts() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "let product = \"FlashOS\"\n",
            "let whole: Int = int(3.9)\n",
            "let decimal: Float = float(7)\n",
            "export PRODUCT = \"$product\"\n",
            "export WHOLE = $whole\n",
            "export DECIMAL = $decimal\n",
        ),
    );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the documented expressions pass assembled static analysis");
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
    .expect("the documented expressions execute through the script session");

    assert_eq!(environment.get("PRODUCT"), Some(OsStr::new("FlashOS")));
    assert_eq!(environment.get("WHOLE"), Some(OsStr::new("3")));
    assert_eq!(environment.get("DECIMAL"), Some(OsStr::new("7.0")));
}

#[test]
fn expression_intrinsic_analysis_reports_types_arity_and_shadowing() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let text = concat!(
        "let whole = int(3.9)\n",
        "let decimal = float($whole)\n",
        "let home = env('HOME')\n",
        "let files = glob('*.fsh')\n",
        "let latest = $status\n",
    );
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("unshadowed intrinsics resolve during static analysis");
    let root = program.graph().root();
    let commands = standard_registry();
    let queries = program.semantic_queries(&commands);
    let visible = queries.visible_names(root, text.len()).unwrap();
    assert!(
        visible
            .iter()
            .any(|name| name.name() == "int" && name.kind() == NameKind::Intrinsic)
    );
    assert!(
        visible
            .iter()
            .any(|name| name.name() == "float" && name.kind() == NameKind::Intrinsic)
    );
    assert!(
        visible
            .iter()
            .any(|name| name.name() == "env" && name.kind() == NameKind::Intrinsic)
    );
    assert!(
        visible
            .iter()
            .any(|name| name.name() == "glob" && name.kind() == NameKind::Intrinsic)
    );
    assert!(visible.iter().any(|name| {
        name.name() == "status"
            && name.kind() == NameKind::DynamicBinding
            && name.value_type() == &ValueType::Any
    }));
    let int_callee = text.find("int(3.9)").unwrap() + 1;
    let SemanticHover::Intrinsic(hover) = queries.hover_at(root, int_callee).unwrap() else {
        panic!("an unshadowed intrinsic has shared hover metadata");
    };
    assert_eq!(hover.intrinsic().name(), "int");
    assert_eq!(hover.intrinsic().result_type(), ValueType::Int);
    let signature = queries
        .intrinsic_signature_at(root, text.find("3.9").unwrap() + 1)
        .expect("an intrinsic call has shared signature metadata");
    assert_eq!(signature.intrinsic().name(), "int");
    assert_eq!(signature.active_parameter(), 0);
    let glob_callee = text.find("glob('*.fsh')").unwrap() + 1;
    let SemanticHover::Intrinsic(glob_hover) = queries.hover_at(root, glob_callee).unwrap() else {
        panic!("glob has shared intrinsic hover metadata");
    };
    assert_eq!(
        glob_hover.intrinsic().result_type(),
        ValueType::List(Box::new(ValueType::Path))
    );
    let effects = program.effects().direct(root).occurrences();
    assert!(
        !effects
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::OpaqueExternal),
        "known dynamic reads do not gain an opaque external effect"
    );
    assert!(
        effects
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::ChildEnvironment)
    );
    assert!(
        effects
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::FilesystemRead)
    );
    assert!(
        effects
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::Status)
    );
    let status_read = text.find("$status").unwrap() + 1;
    let reference = program
        .names()
        .references(root)
        .iter()
        .find(|reference| reference.name() == "status")
        .expect("the current status read is resolved");
    assert_eq!(reference.target(), &ModuleReferenceTarget::DynamicStatus);
    assert!(matches!(
        queries.hover_at(root, status_read),
        Some(SemanticHover::DynamicBinding(_))
    ));

    for (source, code) in [
        ("int()\n", "SIG003"),
        ("float('text')\n", "SIG004"),
        ("env(1)\n", "SIG004"),
        ("glob(true)\n", "SIG004"),
    ] {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", source);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("an invalid intrinsic call fails static analysis");
        assert_eq!(error.diagnostics()[0].code(), code, "{source}");
        assert_eq!(error.diagnostics()[0].labels().len(), 1, "{source}");
    }
    for source in [
        concat!(
            "def needs_float(value: Float) -> Null { null }\n",
            "needs_float(int(3.9))\n",
        ),
        concat!(
            "def needs_int(value: Int) -> Null { null }\n",
            "needs_int(float(7))\n",
        ),
    ] {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", source);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("an intrinsic result keeps its exact static type");
        assert_eq!(error.diagnostics()[0].code(), "SIG004", "{source}");
    }

    let shadowed_text = concat!(
        "def int(value: String) -> String { $value }\n",
        "let result = int('shadowed')\n",
        "export RESULT = $result\n",
    );
    let shadowed_sources = FakeSourceLoader::default().contains("/project/main.fsh", shadowed_text);
    let shadowed = ModuleProgramLoader::new(&paths, &shadowed_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("a lexical callable shadows the intrinsic");
    let root = shadowed.graph().root();
    let call = shadowed_text.rfind("int('shadowed')").unwrap() + 1;
    assert!(matches!(
        shadowed
            .semantic_queries(&standard_registry())
            .hover_at(root, call),
        Some(SemanticHover::Function(_))
    ));
    let mut environment = Environment::new();
    execute_module_program(
        &shadowed,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect("the lexical callable executes instead of the intrinsic");
    assert_eq!(environment.get("RESULT"), Some(OsStr::new("shadowed")));
}

#[test]
fn current_status_name_is_reserved_across_static_binding_forms() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    for text in [
        "let status = 1\n",
        "mut status = 1\n",
        "def status() { null }\n",
        "def callable(status) { $status }\n",
        "let callable = {|status| $status}\n",
        "for status in [1] { null }\n",
        "match 1 { status => { null } }\n",
    ] {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("the dynamic status name cannot become lexical");
        assert_eq!(error.diagnostics()[0].code(), "MOD011", "{text}");
    }

    let import_paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let import_sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import { status } from './lib.fsh'\n")
        .contains("/project/lib.fsh", "let value = 1\nexport { value }\n");
    let error = ModuleProgramLoader::new(&import_paths, &import_sources)
        .load(Path::new("/project/main.fsh"))
        .expect_err("an import cannot occupy the dynamic status name");
    assert_eq!(error.diagnostics()[0].code(), "MOD011");
}

#[test]
fn unknown_type_names_and_invalid_type_arity_report_stable_diagnostics() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let cases = [
        (
            "let value: Mystery = null\n",
            "SIG001",
            "unknown value type `Mystery`",
        ),
        (
            "let value: string = 'text'\n",
            "SIG001",
            "unknown value type `string`",
        ),
        (
            "let value: List = []\n",
            "SIG002",
            "expected 1 type arguments, found 0",
        ),
        (
            "let value: List[String, Int] = []\n",
            "SIG002",
            "expected 1 type arguments, found 2",
        ),
        (
            "let value: String[Int] = 'text'\n",
            "SIG002",
            "expected 0 type arguments, found 1",
        ),
    ];

    for (text, code, label) in cases {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("an invalid type reference fails during analysis");
        let diagnostic = &error.diagnostics()[0];

        assert_eq!(diagnostic.code(), code, "{text}");
        assert_eq!(diagnostic.labels()[0].message(), label, "{text}");
    }
}

#[test]
fn conservative_call_validation_uses_annotations_functions_results_and_operators() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let local_cases = [
        concat!(
            "def needs_text(value: String) -> String { $value }\n",
            "let count: Int = 1\n",
            "needs_text($count)\n",
        ),
        concat!(
            "def needs_closure(value: Closure) -> Null { null }\n",
            "def candidate() -> Int { 1 }\n",
            "needs_closure($candidate)\n",
        ),
        concat!(
            "def needs_text(value: String) -> String { $value }\n",
            "needs_text(1 < 2)\n",
        ),
        concat!(
            "def number() -> Int { 1 }\n",
            "def needs_text(value: String) -> String { $value }\n",
            "needs_text(number())\n",
        ),
    ];

    for text in local_cases {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("a conservatively known incompatible argument fails");
        assert_eq!(error.diagnostics()[0].code(), "SIG004", "{text}");
    }

    let imported = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { count } from './lib.fsh'\n",
                "def needs_text(value: String) -> String { $value }\n",
                "needs_text($count)\n",
            ),
        )
        .contains("/project/lib.fsh", "let count: Int = 1\nexport { count }\n");
    let error = ModuleProgramLoader::new(&paths, &imported)
        .load(Path::new("/project/main.fsh"))
        .expect_err("an imported annotated binding retains its known type");
    assert_eq!(error.diagnostics()[0].code(), "SIG004");

    let imported_function = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { needs_text } from './lib.fsh'\nneeds_text(42)\n",
        )
        .contains(
            "/project/lib.fsh",
            concat!(
                "def needs_text(value: String) -> String { $value }\n",
                "export { needs_text }\n",
            ),
        );
    let error = ModuleProgramLoader::new(&paths, &imported_function)
        .load(Path::new("/project/main.fsh"))
        .expect_err("an imported known function validates its argument types");
    let diagnostic = &error.diagnostics()[0];
    assert_eq!(diagnostic.code(), "SIG004");
    assert_eq!(diagnostic.labels()[0].span().source_id(), SourceId::new(0));
    assert_eq!(diagnostic.labels()[1].span().source_id(), SourceId::new(1));

    let unknown = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "def dynamic(value) -> String { $value }\n",
    );
    ModuleProgramLoader::new(&paths, &unknown)
        .load(Path::new("/project/main.fsh"))
        .expect("an unannotated data-dependent result remains unknown");
}

#[test]
fn known_local_and_imported_calls_validate_arity_with_multi_source_diagnostics() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let local = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "def echo(value: String) -> String { $value }\necho()\n",
    );
    let local_error = ModuleProgramLoader::new(&paths, &local)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a known local call always validates arity");
    let local_diagnostic = &local_error.diagnostics()[0];

    assert_eq!(local_diagnostic.code(), "SIG003");
    assert_eq!(local_diagnostic.labels().len(), 2);
    assert!(
        local_diagnostic
            .labels()
            .iter()
            .all(|label| label.span().source_id() == SourceId::new(0))
    );

    let imported = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { echo } from './lib.fsh'\necho('first', 'second')\n",
        )
        .contains(
            "/project/lib.fsh",
            "def echo(value: String) -> String { $value }\nexport { echo }\n",
        );
    let imported_error = ModuleProgramLoader::new(&paths, &imported)
        .load_for_frontend(Path::new("/project/main.fsh"))
        .expect_err("a known imported call always validates arity");
    let imported_diagnostics = imported_error.error().diagnostics();
    let imported_diagnostic = &imported_diagnostics[0];
    let rendered = imported_error.render();

    assert_eq!(imported_diagnostic.code(), "SIG003");
    assert_eq!(
        imported_diagnostic.labels()[0].span().source_id(),
        SourceId::new(0)
    );
    assert_eq!(
        imported_diagnostic.labels()[1].span().source_id(),
        SourceId::new(1)
    );
    assert!(rendered.contains("/project/main.fsh"), "{rendered}");
    assert!(rendered.contains("/project/lib.fsh"), "{rendered}");
    assert!(
        rendered.contains("expected 1 arguments, found 2"),
        "{rendered}"
    );
    assert!(rendered.contains("function declared here"), "{rendered}");
}

#[test]
fn dynamic_function_and_closure_callees_enforce_runtime_contracts() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let cases = [
        (
            "function arity",
            concat!(
                "def accept(value: Int) { 1 / 0 }\n",
                "let callable = $accept\n",
                "let result = $callable()\n",
            ),
            "expected 1 argument(s), found 0",
        ),
        (
            "function parameter",
            concat!(
                "let dynamic = $args[0]\n",
                "def accept(value: Int) { 1 / 0 }\n",
                "let callable = $accept\n",
                "let result = $callable($dynamic)\n",
            ),
            "parameter \"value\" expects Int, found string",
        ),
        (
            "closure parameter",
            concat!(
                "let dynamic = $args[0]\n",
                "let callable = {|value: Int| 1 / 0}\n",
                "let result = $callable($dynamic)\n",
            ),
            "parameter \"value\" expects Int, found string",
        ),
    ];

    for (case, text, expected) in cases {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
        let program = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .unwrap_or_else(|error| panic!("{case} remains dynamic during analysis: {error}"));
        let error = execute_module_program(
            &program,
            &["not-an-int".to_owned()],
            Path::new("/project"),
            &mut Environment::new(),
            &standard_registry(),
            &NoExecutables,
            &SessionOptions::default(),
            &FakePlatform::none(),
            Arc::new(FakeClock::new()),
        )
        .err()
        .unwrap_or_else(|| panic!("{case} must fail before entering the callable body"));
        let rendered = error.render();

        assert!(rendered.contains(expected), "{case}: {rendered}");
        assert!(!rendered.contains("division by zero"), "{case}: {rendered}");
    }
}

#[test]
fn annotations_and_closure_types_are_registered_in_dormant_modules() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/dormant.fsh", "/project/dormant.fsh");
    let root_text = "import './dormant.fsh'\nlet ready: Bool = true\nexport ROOT = $ready\n";
    let dormant_text = concat!(
        "let count: Int = 1\n",
        "def format(values: List[String]) -> String {\n",
        "    let mapper = {|value: String| $value}\n",
        "    'ready'\n",
        "}\n",
        "export DORMANT = true\n",
    );
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", root_text)
        .contains("/project/dormant.fsh", dormant_text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("every dormant annotation is resolved without activation");
    let dormant = program.graph().imports()[0].target();
    let dormant_source = program
        .sources()
        .source(dormant)
        .expect("the dormant source is registered");
    let annotations = program.types().annotations(dormant);
    let spellings = annotations
        .iter()
        .map(|annotation| {
            dormant_source
                .slice(annotation.span())
                .expect("the annotation span belongs to the dormant source")
        })
        .collect::<Vec<_>>();

    assert_eq!(spellings, ["Int", "List[String]", "String", "String"]);
    assert_eq!(
        annotations
            .iter()
            .map(|annotation| annotation.value_type().clone())
            .collect::<Vec<_>>(),
        [
            ValueType::Int,
            ValueType::List(Box::new(ValueType::String)),
            ValueType::String,
            ValueType::String,
        ]
    );
    for annotation in annotations {
        assert_eq!(
            program.types().annotation(dormant, annotation.span()),
            Some(annotation)
        );
    }

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
    .expect("resolved dormant metadata does not activate the module");
    assert_eq!(environment.get("ROOT"), Some(OsStr::new("true")));
    assert!(!environment.contains("DORMANT"));

    let invalid = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import './dormant.fsh'\n")
        .contains("/project/dormant.fsh", "let value: Missing = null\n");
    let error = ModuleProgramLoader::new(&paths, &invalid)
        .load(Path::new("/project/main.fsh"))
        .expect_err("invalid annotations fail even in a load-only module");

    assert_eq!(error.diagnostics()[0].code(), "SIG001");
    assert_eq!(
        error.module().expect("the failing module").path(),
        Path::new("/project/dormant.fsh")
    );
}

#[test]
fn a_child_closure_can_shadow_an_imported_typed_function() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { transform } from './lib.fsh'\n",
                "if true {\n",
                "    let transform = {|value: String| $value}\n",
                "    export RESULT = transform('shadowed')\n",
                "}\n",
            ),
        )
        .contains(
            "/project/lib.fsh",
            concat!(
                "def transform(value: Int) -> Int { $value }\n",
                "export { transform }\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the child closure, not the imported signature, owns the call");
    let root = program.graph().root();
    let call_reference = program
        .names()
        .references(root)
        .iter()
        .find(|reference| reference.name() == "transform")
        .expect("the shadowed call has a lexical reference");

    assert!(matches!(
        call_reference.target(),
        ModuleReferenceTarget::Local { .. }
    ));

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
    .expect("the shadowing closure accepts its own declared String parameter");

    assert_eq!(environment.get("RESULT"), Some(OsStr::new("shadowed")));
}

#[test]
fn conservatively_known_function_result_mismatches_fail_without_execution() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    for text in [
        "def explicit() -> String { return 1 }\n",
        "def empty_return() -> String { return }\n",
        "def nested() -> String { if true { return 1 }\n'ok' }\n",
        "def implicit() -> String { 1 }\n",
    ] {
        let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
        let error = ModuleProgramLoader::new(&paths, &sources)
            .load(Path::new("/project/main.fsh"))
            .expect_err("a known function result mismatch fails");
        let diagnostic = &error.diagnostics()[0];
        assert_eq!(diagnostic.code(), "SIG005", "{text}");
        assert_eq!(diagnostic.labels().len(), 2, "{text}");
    }

    let imported = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import { broken } from './lib.fsh'\n")
        .contains(
            "/project/lib.fsh",
            "def broken() -> String { return 1 }\nexport { broken }\n",
        );
    let error = ModuleProgramLoader::new(&paths, &imported)
        .load(Path::new("/project/main.fsh"))
        .expect_err("a dependency function result is validated without activation");
    assert_eq!(error.diagnostics()[0].code(), "SIG005");
    assert_eq!(
        error.diagnostics()[0].labels()[0].span().source_id(),
        SourceId::new(1)
    );
}

#[test]
fn value_types_match_exact_runtime_families_and_recursive_lists() {
    let error_source = SourceFile::new(SourceId::new(99), "error.fsh", "throw 'x'");
    let error_span = error_source.span(6..9).expect("test error span is valid");
    let cases = vec![
        (ValueType::Null, Value::Null),
        (ValueType::Bool, Value::Bool(true)),
        (ValueType::Int, Value::Int(1)),
        (
            ValueType::Float,
            Value::from(FiniteFloat::new(1.0).expect("finite test value")),
        ),
        (ValueType::String, Value::string("one")),
        (ValueType::Bytes, Value::bytes(vec![1])),
        (ValueType::Path, Value::from(NativePath::new("one"))),
        (ValueType::Duration, Value::from(Duration::ZERO)),
        (ValueType::ByteSize, Value::from(ByteSize::new(1))),
        (
            ValueType::List(Box::new(ValueType::String)),
            Value::list(vec![Value::string("one")]),
        ),
        (
            ValueType::Record,
            Value::from(Record::new(Vec::new()).expect("empty record")),
        ),
        (
            ValueType::Table,
            Value::from(Table::new(Vec::new(), Vec::new()).expect("empty table")),
        ),
        (ValueType::Range, Value::from(Range::new(0, 1, false))),
        (
            ValueType::Status,
            Value::from(Status::exit(0, Duration::ZERO).expect("valid status")),
        ),
        (
            ValueType::Error,
            Value::Error(Arc::new(RuntimeError::new(
                RuntimeErrorKind::UserThrown {
                    message: "x".to_owned(),
                },
                error_span,
            ))),
        ),
        (
            ValueType::Function,
            Value::Callable(Arc::new(FamilyCallable("function"))),
        ),
        (
            ValueType::Closure,
            Value::Callable(Arc::new(FamilyCallable("closure"))),
        ),
    ];

    for (expected_index, (expected_type, expected_value)) in cases.iter().enumerate() {
        assert!(
            expected_type.accepts(expected_value),
            "{expected_type} should accept its runtime family"
        );
        assert!(ValueType::Any.accepts(expected_value));
        for (actual_index, (_, actual_value)) in cases.iter().enumerate() {
            if expected_index != actual_index {
                assert!(
                    !expected_type.accepts(actual_value),
                    "{expected_type} must not accept {}",
                    actual_value.family_name()
                );
            }
        }
    }

    let strings = ValueType::List(Box::new(ValueType::String));
    assert!(strings.accepts(&Value::list(Vec::new())));
    assert!(strings.accepts(&Value::list(vec![
        Value::string("first"),
        Value::string("second"),
    ])));
    assert!(!strings.accepts(&Value::list(vec![Value::string("first"), Value::Int(2),])));

    let nested_strings = ValueType::List(Box::new(strings));
    assert!(
        nested_strings.accepts(&Value::list(vec![Value::list(vec![Value::string(
            "nested",
        )])]))
    );
    let wrong_nested_value = Value::list(vec![Value::list(vec![Value::Bool(true)])]);
    assert!(!nested_strings.accepts(&wrong_nested_value));
}

#[test]
fn dynamic_lists_enforce_any_and_recursive_element_types_at_runtime() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let valid_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "let nested = [['nested']]\n",
            "let mixed = [1, 'two', true]\n",
            "def accept_nested(values: List[List[String]]) -> Null { null }\n",
            "def accept_any(values: List[Any]) -> Null { null }\n",
            "let nested_result = accept_nested($nested)\n",
            "let any_result = accept_any($mixed)\n",
        ),
    );
    let valid_program = ModuleProgramLoader::new(&paths, &valid_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("untyped list bindings remain conservative during analysis");
    execute_module_program(
        &valid_program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect("Any and recursively matching list elements are accepted");

    let invalid_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "let nested = [[true]]\n",
            "def accept(values: List[List[String]]) -> Null { null }\n",
            "let result = accept($nested)\n",
        ),
    );
    let invalid_program = ModuleProgramLoader::new(&paths, &invalid_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("an untyped list binding remains unknown during analysis");
    let error = execute_module_program(
        &invalid_program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("a nested runtime element mismatch is rejected");
    let rendered = error.render();

    assert!(
        rendered.contains("parameter \"values\" expects List[List[String]], found list"),
        "{rendered}"
    );
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
            ModuleReferenceTarget::DynamicStatus => panic!("fixture has no dynamic reads"),
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
fn annotated_declarations_and_assignments_reject_dynamic_runtime_mismatches() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");

    let initializer_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "let count: Int = $args[0]\nexport AFTER = true\n",
    );
    let initializer_program = ModuleProgramLoader::new(&paths, &initializer_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the dynamic initializer cannot be rejected before execution");
    let mut initializer_environment = Environment::new();
    let initializer_error = execute_module_program(
        &initializer_program,
        &["not-an-int".to_owned()],
        Path::new("/project"),
        &mut initializer_environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("an annotated initializer must match before binding insertion");
    let initializer_rendered = initializer_error.render();
    assert!(
        initializer_rendered.contains("binding expects Int, found string"),
        "{initializer_rendered}"
    );
    assert!(
        initializer_rendered.contains(" --> /project/main.fsh:1:18"),
        "{initializer_rendered}"
    );
    assert!(!initializer_environment.contains("AFTER"));

    let assignment_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "mut count: Int = 1\n$count = $args[0]\nexport AFTER = $count\n",
    );
    let assignment_program = ModuleProgramLoader::new(&paths, &assignment_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the dynamic assignment cannot be rejected before execution");
    let mut assignment_environment = Environment::new();
    let assignment_error = execute_module_program(
        &assignment_program,
        &["still-not-an-int".to_owned()],
        Path::new("/project"),
        &mut assignment_environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("a later assignment must preserve the binding's declared type");
    let assignment_rendered = assignment_error.render();
    assert!(
        assignment_rendered.contains("binding expects Int, found string"),
        "{assignment_rendered}"
    );
    assert!(
        assignment_rendered.contains(" --> /project/main.fsh:2:10"),
        "{assignment_rendered}"
    );
    assert!(!assignment_environment.contains("AFTER"));
}

#[test]
fn ordinary_calls_reject_dynamic_parameter_mismatches_before_body_entry() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");

    let call_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "let dynamic = $args[0]\n",
            "def accept(value: Int) {\n",
            "    export ENTERED = true\n",
            "}\n",
            "let result = accept($dynamic)\n",
        ),
    );
    let call_program = ModuleProgramLoader::new(&paths, &call_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the unannotated argument remains unknown during analysis");
    let mut call_environment = Environment::new();
    let call_error = execute_module_program(
        &call_program,
        &["not-an-int".to_owned()],
        Path::new("/project"),
        &mut call_environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("an ordinary call must enforce its resolved parameter type");
    let call_rendered = call_error.render();
    assert!(
        call_rendered.contains("parameter \"value\" expects Int, found string"),
        "{call_rendered}"
    );
    assert!(
        call_rendered.contains(" --> /project/main.fsh:5:21"),
        "{call_rendered}"
    );
    assert!(!call_environment.contains("ENTERED"));
}

#[test]
fn apply_callable_rejects_dynamic_parameter_mismatches_before_body_entry() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");

    let applied_sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "which pwd | each {|row: String| 1 / 0} | first 1 | to json\n",
    );
    let applied_program = ModuleProgramLoader::new(&paths, &applied_sources)
        .load(Path::new("/project/main.fsh"))
        .expect("internal-command closure parameters are resolved without execution");
    let mut applied_environment = Environment::new();
    let applied_error = execute_module_program(
        &applied_program,
        &[],
        Path::new("/project"),
        &mut applied_environment,
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("apply_callable must enforce the same resolved parameter type");
    let applied_rendered = applied_error.render();
    assert!(
        applied_rendered.contains("parameter \"row\" expects String, found record"),
        "{applied_rendered}"
    );
    assert!(
        applied_rendered.contains(" --> /project/main.fsh:1:13"),
        "{applied_rendered}"
    );
}

#[test]
fn dynamic_function_results_enforce_explicit_implicit_and_null_fallthrough_values() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let cases = [
        (
            "explicit",
            concat!(
                "let dynamic = $args[0]\n",
                "def produce() -> Int {\n",
                "    return $dynamic\n",
                "}\n",
                "let callable = $produce\n",
                "let result = $callable()\n",
            ),
            "string",
            " --> /project/main.fsh:3:12",
            "6 | let result = $callable()",
        ),
        (
            "implicit final",
            concat!(
                "let dynamic = $args[0]\n",
                "def produce() -> Int {\n",
                "    $dynamic\n",
                "}\n",
                "let result = produce()\n",
            ),
            "string",
            " --> /project/main.fsh:3:5",
            "5 | let result = produce()",
        ),
        (
            "null fallthrough",
            concat!(
                "def produce() -> Int {\n",
                "    let touched = true\n",
                "}\n",
                "let result = produce()\n",
            ),
            "null",
            " --> /project/main.fsh:1:22",
            "4 | let result = produce()",
        ),
    ];

    let outcomes = cases
        .iter()
        .map(|(_, text, _, _, _)| {
            let sources = FakeSourceLoader::default().contains("/project/main.fsh", text);
            let program = ModuleProgramLoader::new(&paths, &sources)
                .load(Path::new("/project/main.fsh"))
                .expect("the dynamic result cannot be rejected before execution");
            execute_module_program(
                &program,
                &["not-an-int".to_owned()],
                Path::new("/project"),
                &mut Environment::new(),
                &standard_registry(),
                &NoExecutables,
                &SessionOptions::default(),
                &FakePlatform::none(),
                Arc::new(FakeClock::new()),
            )
            .err()
            .map(|error| error.render().to_owned())
        })
        .collect::<Vec<_>>();

    for ((case, _, actual, primary, frame), rendered) in cases.iter().zip(outcomes) {
        let rendered = rendered.unwrap_or_else(|| panic!("{case} result was not enforced"));
        assert!(
            rendered.contains(&format!("function result expects Int, found {actual}")),
            "{case}: {rendered}"
        );
        assert!(rendered.contains(primary), "{case}: {rendered}");
        assert!(rendered.contains(frame), "{case}: {rendered}");
        assert!(rendered.contains("called from here"), "{case}: {rendered}");
    }
}

#[test]
fn imported_function_result_failures_keep_defining_source_and_call_frame() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { produce } from './lib.fsh'\n",
                "let dynamic = $args[0]\n",
                "let result = produce($dynamic)\n",
            ),
        )
        .contains(
            "/project/lib.fsh",
            "def produce(value) -> Int { return $value }\nexport { produce }\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the imported dynamic result cannot be rejected before execution");
    let error = execute_module_program(
        &program,
        &["not-an-int".to_owned()],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("the imported function's result type must be enforced");
    let rendered = error.render();

    assert!(
        rendered.contains("function result expects Int, found string"),
        "{rendered}"
    );
    assert!(
        rendered.contains(" --> /project/lib.fsh:1:36"),
        "{rendered}"
    );
    assert!(
        rendered.contains(" ::: /project/main.fsh:3:14"),
        "{rendered}"
    );
}

#[test]
fn annotated_binding_failures_inside_imported_callables_use_the_defining_source() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { store } from './lib.fsh'\nlet result = store($args[0])\n",
        )
        .contains(
            "/project/lib.fsh",
            "def store(value) {\n    let count: Int = $value\n}\nexport { store }\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the dynamic imported call cannot be rejected before execution");
    let error = execute_module_program(
        &program,
        &["not-an-int".to_owned()],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
    )
    .expect_err("the defining module's annotated local must be enforced");
    let rendered = error.render();

    assert!(
        rendered.contains("binding expects Int, found string"),
        "{rendered}"
    );
    assert!(
        rendered.contains(" --> /project/lib.fsh:2:22"),
        "{rendered}"
    );
    assert!(
        rendered.contains(" ::: /project/main.fsh:2:14"),
        "{rendered}"
    );
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
fn imported_module_initializers_preserve_typed_byte_captures() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { binary } from './lib.fsh'\n",
                "def accept(value: Bytes) -> String { 'captured' }\n",
                "export RESULT = accept($binary)\n",
            ),
        )
        .contains(
            "/project/lib.fsh",
            "let binary: Bytes = $(bytes: pwd)\nexport { binary }\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the typed imported capture should analyze");
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
    .expect("the imported initializer should produce a Bytes value");

    assert_eq!(environment.get("RESULT"), Some(OsStr::new("captured")));
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
fn initializer_failure_keeps_crossed_file_and_process_effects_without_environment_commit() {
    let directory = TestDirectory::new("failure-effects");
    let root_path = directory.path().join("main.fsh");
    let failing_path = directory.path().join("failing.fsh");
    let later_path = directory.path().join("later.fsh");
    let root_name = root_path.to_string_lossy();
    let failing_name = failing_path.to_string_lossy();
    let later_name = later_path.to_string_lossy();
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flash-status-fixture"));
    let fixture_name = fixture.to_string_lossy();
    let paths = FakeCanonicalizer::default()
        .resolves(&root_name, &root_name)
        .resolves(&failing_name, &failing_name)
        .resolves(&later_name, &later_name);
    let sources = FakeSourceLoader::default()
        .contains(
            &root_name,
            concat!(
                "import { failing } from './failing.fsh'\n",
                "import { later } from './later.fsh'\n",
                "which pwd | get kind | encode utf8 | save root-ran.txt\n",
            ),
        )
        .contains(
            &failing_name,
            &format!(
                concat!(
                    "let failing = 1\n",
                    "export {{ failing }}\n",
                    "which pwd | get kind | encode utf8 | save crossed.txt\n",
                    "^{} exit 0\n",
                    "^{} exit 0 &\n",
                    "export SESSION_ONLY = 'changed'\n",
                    "export BROKEN = 1 + true\n",
                ),
                fixture_name, fixture_name
            ),
        )
        .contains(
            &later_name,
            concat!(
                "let later = 1\n",
                "export { later }\n",
                "which pwd | get kind | encode utf8 | save later-ran.txt\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(&root_path)
        .expect("the failure-effects module program loads");
    let platform = CountingPosix::new();
    let mut environment = Environment::from_snapshot([("SESSION_ONLY", "caller")]);

    let error = execute_module_program(
        &program,
        &[],
        directory.path(),
        &mut environment,
        &standard_registry(),
        &OneExecutable(fixture),
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
    )
    .expect_err("the initializer fails after crossing file and process boundaries");

    assert!(
        error
            .render()
            .contains("operator `+` is not defined for int, bool"),
        "{}",
        error.render()
    );
    assert_eq!(
        fs::read(directory.path().join("crossed.txt")).unwrap(),
        b"internal"
    );
    assert!(!directory.path().join("later-ran.txt").exists());
    assert!(!directory.path().join("root-ran.txt").exists());
    assert_eq!(platform.spawns(), 2);
    assert_eq!(environment.get("SESSION_ONLY"), Some(OsStr::new("caller")));
    assert!(!environment.contains("BROKEN"));
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
fn dependency_order_shares_cwd_environment_status_and_output_until_root_exit() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/state.fsh", "/project/state.fsh")
        .resolves("/project/observer.fsh", "/project/observer.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { observed } from './observer.fsh'\n",
                "help cd\n",
                "^/bin/mark root\n",
                "exit\n",
            ),
        )
        .contains(
            "/project/observer.fsh",
            concat!(
                "import { ready } from './state.fsh'\n",
                "help pwd\n",
                "^/bin/mark observer\n",
                "let observed = $ready\n",
                "export { observed }\n",
            ),
        )
        .contains(
            "/project/state.fsh",
            concat!(
                "cd './nested'\n",
                "export TOKEN = 'dependency'\n",
                "which exit | get kind | encode utf8\n",
                "^/bin/mark status\n",
                "export STATUS_CODE = $status.code\n",
                "export SEEN_PWD = env('PWD')\n",
                "let ready = true\n",
                "export { ready }\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the shared-state module program loads");
    let platform = StatusPlatform::new(7);
    let mut environment = Environment::from_snapshot([("PWD", "/project")]);
    let mut output = Vec::new();

    let completion = execute_module_program_with_output(
        &program,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &MarkExecutable,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .expect("the root bare exit observes the dependency status");

    assert_eq!(completion.status().and_then(Status::code), Some(7));
    assert_eq!(environment.get("TOKEN"), Some(OsStr::new("dependency")));
    assert_eq!(environment.get("STATUS_CODE"), Some(OsStr::new("7")));
    assert_eq!(
        environment.get("SEEN_PWD"),
        Some(OsStr::new("/project/nested"))
    );
    assert_eq!(environment.get("PWD"), Some(OsStr::new("/project/nested")));
    let spawns = platform.spawns();
    assert_eq!(
        spawns
            .iter()
            .map(|spawn| spawn.argv[1].clone())
            .collect::<Vec<_>>(),
        ["status", "observer", "root"].map(OsString::from)
    );
    assert!(
        spawns
            .iter()
            .all(|spawn| spawn.cwd == Path::new("/project/nested"))
    );
    assert!(spawns.iter().all(|spawn| {
        spawn
            .environment
            .iter()
            .any(|(name, value)| name == "TOKEN" && value == "dependency")
    }));
    let rendered = String::from_utf8(output).expect("built-in output is UTF-8");
    let cwd = rendered.find("internal").expect("state output");
    let observer = rendered.find("pwd").expect("observer help output");
    let root = rendered.rfind("cd").expect("root help output");
    assert!(cwd < observer && observer < root, "{rendered}");
}

#[test]
fn initializer_exit_commits_environment_joins_jobs_and_keeps_failure_precedence() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/dependency.fsh", "/project/dependency.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { value } from './dependency.fsh'\nexport ROOT_RAN = true\n",
        )
        .contains(
            "/project/dependency.fsh",
            concat!(
                "let value = 1\n",
                "export { value }\n",
                "export COMMITTED = 'yes'\n",
                "^/bin/mark background &\n",
                "exit 7\n",
                "export AFTER_EXIT = 'no'\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the explicit-exit module program loads");
    let platform = StatusPlatform::new(9);
    let mut environment = Environment::new();

    let completion = execute_module_program(
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
    .expect("explicit exit is normal completion even when a joined job fails");

    assert_eq!(completion.status().and_then(Status::code), Some(9));
    assert_eq!(completion.background_failures().len(), 1);
    assert_eq!(platform.waits(), 1);
    assert_eq!(environment.get("COMMITTED"), Some(OsStr::new("yes")));
    assert!(!environment.contains("AFTER_EXIT"));
    assert!(!environment.contains("ROOT_RAN"));
}

#[test]
fn initializer_output_failure_keeps_prefix_joins_jobs_and_discards_environment() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/dependency.fsh", "/project/dependency.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            "import { value } from './dependency.fsh'\nexport ROOT_RAN = true\n",
        )
        .contains(
            "/project/dependency.fsh",
            concat!(
                "let value = 1\n",
                "export { value }\n",
                "export CHANGED = 'session-only'\n",
                "^/bin/mark background &\n",
                "which pwd | get kind | encode utf8\n",
                "export AFTER_OUTPUT = 'no'\n",
            ),
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the output-failure module program loads");
    let platform = StatusPlatform::new(9);
    let mut environment = Environment::from_snapshot([("CHANGED", "caller")]);
    let mut output = PrefixThenFailWriter::new(3);

    let error = execute_module_program_with_output(
        &program,
        &[],
        Path::new("/project"),
        &mut environment,
        &standard_registry(),
        &MarkExecutable,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .expect_err("the injected sink failure is fatal");

    assert_eq!(output.bytes, b"int", "{}", error.render());
    assert!(error.render().contains("output write failed"));
    assert_eq!(error.background_failures().len(), 1);
    assert_eq!(platform.waits(), 1);
    assert_eq!(environment.get("CHANGED"), Some(OsStr::new("caller")));
    assert!(!environment.contains("AFTER_OUTPUT"));
    assert!(!environment.contains("ROOT_RAN"));
}

#[test]
fn module_program_output_uses_only_the_injected_sink() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "which pwd | get kind | encode utf8\n");
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the output program loads");
    let mut output = Vec::new();

    execute_module_program_with_output(
        &program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .expect("module output should use the supplied sink");

    assert_eq!(output, b"internal");
}

#[test]
fn module_effect_summaries_fold_named_dependencies_and_exclude_load_only_modules() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/dependency.fsh", "/project/dependency.fsh")
        .resolves("/project/dormant.fsh", "/project/dormant.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { reveal } from './dependency.fsh'\n",
                "import './dormant.fsh'\n",
                "let observed = reveal()\n",
                "let action = $reveal\n",
                "let dynamic = action()\n",
                "export ROOT = 'ready'\n",
                "open input.txt | save output.txt\n",
                "exit 0\n",
            ),
        )
        .contains(
            "/project/dependency.fsh",
            concat!(
                "def reveal() {\n",
                "    pwd | encode utf8\n",
                "}\n",
                "export { reveal }\n",
                "cd '/work'\n",
                "export TOKEN = 'set'\n",
                "^tool &\n",
            ),
        )
        .contains(
            "/project/dormant.fsh",
            "pwd | encode utf8 | save dormant.txt\n",
        );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the effectful module program loads without execution");
    let root = program.graph().root();
    let dependency = program
        .graph()
        .imports()
        .iter()
        .find(|import| import.target().path() == Path::new("/project/dependency.fsh"))
        .expect("the named dependency is graphed")
        .target();
    let dormant = program
        .graph()
        .imports()
        .iter()
        .find(|import| import.target().path() == Path::new("/project/dormant.fsh"))
        .expect("the load-only dependency is graphed")
        .target();

    let dependency_kinds = program
        .effects()
        .direct(dependency)
        .occurrences()
        .iter()
        .map(|occurrence| occurrence.effect())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(dependency_kinds.contains(&ModuleEffect::WorkingDirectory));
    assert!(dependency_kinds.contains(&ModuleEffect::ChildEnvironment));
    assert!(dependency_kinds.contains(&ModuleEffect::Status));
    assert!(dependency_kinds.contains(&ModuleEffect::Process));
    assert!(dependency_kinds.contains(&ModuleEffect::Job));
    assert!(dependency_kinds.contains(&ModuleEffect::OpaqueExternal));
    let dependency_source = program
        .sources()
        .source(dependency)
        .expect("dependency source");
    assert!(
        !program
            .effects()
            .direct(dependency)
            .occurrences()
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::Output
                && dependency_source.slice(occurrence.span()) == Ok("encode")),
        "a dormant function body is not a direct initializer effect"
    );

    let dormant_kinds = program
        .effects()
        .direct(dormant)
        .occurrences()
        .iter()
        .map(|occurrence| occurrence.effect())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(dormant_kinds.contains(&ModuleEffect::FilesystemWrite));

    let root_direct = program.effects().direct(root);
    assert!(
        root_direct
            .occurrences()
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::Output
                && occurrence.span().source_id()
                    == program
                        .sources()
                        .source(dependency)
                        .expect("dependency source")
                        .id()),
        "the statically known imported callable folds its defining body"
    );
    let root_kinds = root_direct
        .occurrences()
        .iter()
        .map(|occurrence| occurrence.effect())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(root_kinds.contains(&ModuleEffect::FilesystemRead));
    assert!(root_kinds.contains(&ModuleEffect::FilesystemWrite));
    assert!(root_kinds.contains(&ModuleEffect::ProgramExit));
    assert!(
        root_kinds.contains(&ModuleEffect::OpaqueExternal),
        "an indirectly held callable remains conservative"
    );

    let root_transitive = program.effects().transitive(root);
    let transitive_sources = root_transitive
        .occurrences()
        .iter()
        .map(|occurrence| occurrence.span().source_id())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        transitive_sources.contains(
            &program
                .sources()
                .source(dependency)
                .expect("dependency source")
                .id()
        )
    );
    assert!(
        transitive_sources.contains(&program.sources().source(root).expect("root source").id())
    );
    assert!(
        !transitive_sources.contains(
            &program
                .sources()
                .source(dormant)
                .expect("dormant source")
                .id()
        ),
        "load-only modules are analyzed directly but remain dormant transitively"
    );
}

#[test]
fn dynamic_external_command_has_the_external_process_effect_contract() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "let selected = 'tool'\ncommand $selected argument\n",
    );
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the dynamic external source loads without execution");
    let effects = program
        .effects()
        .direct(program.graph().root())
        .occurrences()
        .iter()
        .map(|occurrence| occurrence.effect())
        .collect::<std::collections::BTreeSet<_>>();

    assert!(effects.contains(&ModuleEffect::Process));
    assert!(effects.contains(&ModuleEffect::OpaqueExternal));
    assert!(effects.contains(&ModuleEffect::Output));
    assert!(effects.contains(&ModuleEffect::Status));
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
fn analysis_report_collapses_canonical_aliases_and_exposes_a_complete_program() {
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

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert!(report.issues().is_empty());
    assert!(report.program().is_some());
    assert_eq!(report.sources().len(), 2);
    assert_eq!(sources.load_count("/project/lib/math.fsh"), 1);
    assert_eq!(
        report
            .sources()
            .iter()
            .map(|entry| (entry.module().path(), entry.source().id()))
            .collect::<Vec<_>>(),
        [
            (Path::new("/project/main.fsh"), SourceId::new(0)),
            (Path::new("/project/lib/math.fsh"), SourceId::new(1)),
        ]
    );
}

#[test]
fn analysis_report_accumulates_discovery_failures_in_depth_first_source_order() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .rejects("/project/unresolved.fsh", "missing mapping")
        .resolves("/project/unreadable.fsh", "/project/unreadable.fsh")
        .resolves("/project/bytes.fsh", "/project/bytes.fsh")
        .resolves("/project/syntax.fsh", "/project/syntax.fsh")
        .resolves("/project/a.fsh", "/project/a.fsh")
        .resolves("/project/b.fsh", "/project/b.fsh")
        .resolves("/project/valid.fsh", "/project/valid.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import './unresolved.fsh'\n",
                "import './unreadable.fsh'\n",
                "import './bytes.fsh'\n",
                "import './syntax.fsh'\n",
                "import './a.fsh'\n",
                "import './valid.fsh'\n",
            ),
        )
        .rejects("/project/unreadable.fsh", "read refused")
        .contains_bytes("/project/bytes.fsh", vec![b'l', b'e', b't', b' ', 0xff])
        .contains(
            "/project/syntax.fsh",
            "let broken = ;\nimport './not-scanned.fsh'\n",
        )
        .contains("/project/a.fsh", "import './b.fsh'\n")
        .contains("/project/b.fsh", "import './a.fsh'\n")
        .contains("/project/valid.fsh", "let value = 1\n");

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert!(
        report.program().is_none(),
        "a partial program must not escape"
    );
    assert_eq!(
        report
            .issues()
            .iter()
            .map(|issue| {
                issue.error().diagnostics().first().map_or_else(
                    || "unspanned".to_owned(),
                    |diagnostic| diagnostic.code().to_owned(),
                )
            })
            .collect::<Vec<_>>(),
        ["MOD001", "MOD003", "MOD004", "FS1000", "MOD002"]
    );
    assert_eq!(
        report
            .sources()
            .iter()
            .map(|entry| (
                entry.module().path(),
                entry.source().id(),
                entry.script().is_some()
            ))
            .collect::<Vec<_>>(),
        [
            (Path::new("/project/main.fsh"), SourceId::new(0), true),
            (Path::new("/project/syntax.fsh"), SourceId::new(1), false),
            (Path::new("/project/a.fsh"), SourceId::new(2), true),
            (Path::new("/project/b.fsh"), SourceId::new(3), true),
            (Path::new("/project/valid.fsh"), SourceId::new(4), true),
        ]
    );
    assert_eq!(sources.load_count("/project/valid.fsh"), 1);
    assert_eq!(sources.load_count("/project/not-scanned.fsh"), 0);
}

#[test]
fn analysis_report_accumulates_name_failures_in_phase_and_source_order() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/alpha.fsh", "/project/alpha.fsh")
        .resolves("/project/beta.fsh", "/project/beta.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { private } from './alpha.fsh'\n",
                "import { collision } from './beta.fsh'\n",
                "let duplicate_export = 1\n",
                "let collision = 1\n",
                "let repeated = 1\n",
                "let repeated = 2\n",
                "let repeated = 3\n",
                "export { missing, duplicate_export, duplicate_export }\n",
                "let root_read = $unknown_root\n",
            ),
        )
        .contains(
            "/project/alpha.fsh",
            concat!(
                "let private = 1\n",
                "export { ghost }\n",
                "let alpha_read = $unknown_alpha\n",
            ),
        )
        .contains(
            "/project/beta.fsh",
            "let collision = 2\nexport { collision }\n",
        );

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert!(report.program().is_none());
    let diagnostics = report
        .issues()
        .iter()
        .map(|issue| issue.error().diagnostics().remove(0))
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code())
            .collect::<Vec<_>>(),
        [
            "MOD005", "MOD006", "MOD005", "MOD007", "MOD008", "MOD010", "MOD010", "MOD009",
            "MOD009",
        ]
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(diagnostics_source_ids)
            .collect::<Vec<_>>(),
        [
            vec![SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(1)],
            vec![SourceId::new(0), SourceId::new(1)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(0)],
            vec![SourceId::new(1)],
        ]
    );
}

#[test]
fn poisoned_name_owners_suppress_only_their_dependent_cascades() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let invalid_export = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { missing } from './lib.fsh'\n",
                "let dependent = $missing\n",
                "let independent = $unknown\n",
            ),
        )
        .contains("/project/lib.fsh", "export { missing }\n");

    let report =
        ModuleProgramLoader::new(&paths, &invalid_export).analyze(Path::new("/project/main.fsh"));
    assert_eq!(analysis_codes(&report), ["MOD005", "MOD009"]);

    let unavailable_import = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { private } from './lib.fsh'\n",
                "let dependent = $private\n",
                "let independent = $unknown\n",
            ),
        )
        .contains("/project/lib.fsh", "let private = 1\n");
    let report = ModuleProgramLoader::new(&paths, &unavailable_import)
        .analyze(Path::new("/project/main.fsh"));

    assert_eq!(analysis_codes(&report), ["MOD007", "MOD009"]);
}

#[test]
fn graph_issues_suppress_the_name_phase() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .rejects("/project/missing.fsh", "missing mapping");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "import './missing.fsh'\n",
            "export { missing }\n",
            "let value = $unknown\n",
        ),
    );

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert_eq!(analysis_codes(&report), ["MOD001"]);
}

#[test]
fn name_issues_suppress_the_signature_phase() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "export { missing }\nlet value: Mystery = null\n",
    );

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert_eq!(analysis_codes(&report), ["MOD005"]);
}

#[test]
fn analysis_report_accumulates_signature_failures_in_source_and_construct_order() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains(
            "/project/main.fsh",
            concat!(
                "import { imported } from './lib.fsh'\n",
                "let unknown: Mystery = null\n",
                "let invalid_arity: List = []\n",
                "def arity(value: Int) -> Int { $value }\n",
                "arity()\n",
                "def needs_text(value: String) -> String { $value }\n",
                "needs_text(42)\n",
                "def wrong_result() -> String { 42 }\n",
                "imported(42)\n",
            ),
        )
        .contains(
            "/project/lib.fsh",
            concat!(
                "def imported(value: String) -> String { $value }\n",
                "export { imported }\n",
                "let library_unknown: Missing = null\n",
            ),
        );

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert!(report.program().is_none());
    let diagnostics = analysis_diagnostics(&report);
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code())
            .collect::<Vec<_>>(),
        [
            "SIG001", "SIG002", "SIG003", "SIG004", "SIG005", "SIG004", "SIG001",
        ]
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(diagnostics_source_ids)
            .collect::<Vec<_>>(),
        [
            vec![SourceId::new(0)],
            vec![SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(0)],
            vec![SourceId::new(0), SourceId::new(1)],
            vec![SourceId::new(1)],
        ]
    );
}

#[test]
fn poisoned_signature_owners_suppress_only_dependent_mismatches() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "def invalid(value: Mystery) -> String { $value }\n",
            "invalid(42)\n",
            "let poisoned: Missing = 42\n",
            "def needs_text(value: String) -> String { $value }\n",
            "needs_text($poisoned)\n",
            "needs_text(42)\n",
            "def number(value: Int) -> Int { $value }\n",
            "needs_text(number())\n",
        ),
    );

    let report = ModuleProgramLoader::new(&paths, &sources).analyze(Path::new("/project/main.fsh"));

    assert_eq!(
        analysis_codes(&report),
        ["SIG001", "SIG001", "SIG004", "SIG003"]
    );
}

#[test]
fn legacy_loaders_select_and_render_the_first_accumulated_signature_issue() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        "let first: Mystery = null\nlet second: List = []\n",
    );
    let loader = ModuleProgramLoader::new(&paths, &sources);

    let report = loader.analyze(Path::new("/project/main.fsh"));
    assert_eq!(analysis_codes(&report), ["SIG001", "SIG002"]);

    let error = loader
        .load(Path::new("/project/main.fsh"))
        .expect_err("legacy execution loading selects the first signature issue");
    assert!(error.to_string().contains("`Mystery`"));

    let error = loader
        .load_for_frontend(Path::new("/project/main.fsh"))
        .expect_err("legacy frontend loading renders only the first signature issue");
    assert!(error.render().contains("`Mystery`"));
    assert!(!error.render().contains("type `List`"));
}

#[test]
fn legacy_loaders_select_and_render_the_first_accumulated_name_issue() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources =
        FakeSourceLoader::default().contains("/project/main.fsh", "export { first, second }\n");
    let loader = ModuleProgramLoader::new(&paths, &sources);

    let report = loader.analyze(Path::new("/project/main.fsh"));
    assert_eq!(analysis_codes(&report), ["MOD005", "MOD005"]);

    let error = loader
        .load(Path::new("/project/main.fsh"))
        .expect_err("legacy execution loading selects the first issue");
    assert!(error.to_string().contains("`first`"));

    let error = loader
        .load_for_frontend(Path::new("/project/main.fsh"))
        .expect_err("legacy frontend loading renders only the first issue");
    assert!(error.render().contains("`first`"));
    assert!(!error.render().contains("`second`"));
}

#[test]
fn static_pipeline_analysis_reports_all_four_fault_families_in_source_order() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!("each\n", "ls |& each\n", "ls | ^cat\n", "^cat | (1 + 2)\n",),
    );

    let report = ModuleProgramLoader::new(&paths, &sources)
        .analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());

    assert_eq!(
        analysis_codes(&report),
        ["PIP001", "PIP002", "PIP003", "PIP004"]
    );
    assert!(report.program().is_none());
    let diagnostics = analysis_diagnostics(&report);
    assert_eq!(diagnostics[0].labels()[0].span().start(), 0);
    assert_eq!(diagnostics[1].labels()[0].span().start(), 8);
    assert_eq!(diagnostics[2].labels()[0].span().start(), 19);
    assert_eq!(diagnostics[3].labels()[0].span().start(), 33);
    assert!(
        diagnostics[2]
            .notes()
            .iter()
            .any(|note| note.contains("encode") && note.contains("to")),
        "structured-to-byte faults retain explicit repair guidance"
    );
}

#[test]
fn forced_assumed_and_dynamic_heads_need_no_probe_or_expansion() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "^each\n",
            "not_installed\n",
            "let selected = 'each'\n",
            "cmd-${selected}\n",
            "ls | ^cat\n",
            "^cat | each\n",
            "ls | not_installed\n",
            "ls | cmd-${selected}\n",
        ),
    );

    let report = ModuleProgramLoader::new(&paths, &sources)
        .analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());

    assert_eq!(analysis_codes(&report), ["PIP003", "PIP003", "PIP003"]);
    let diagnostics = analysis_diagnostics(&report);
    assert!(diagnostics[0].notes()[0].contains("encode"));
    assert!(diagnostics[1].notes()[0].contains("decode"));
    assert!(diagnostics[2].notes()[0].contains("encode"));
}

fn namespace_checker_registry() -> CommandRegistry {
    CommandRegistry::try_from_entries(
        1,
        [
            CommandNamespaceEntry::core(
                CommandSignature::new("pwd", [Carrier::Empty], Carrier::Value),
                CommandLifecycle::introduced(1),
            ),
            CommandNamespaceEntry::core(
                CommandSignature::new("oldpwd", [Carrier::Empty], Carrier::Value),
                CommandLifecycle::introduced(1)
                    .deprecated_since("0.9.0")
                    .with_replacement("pwd"),
            ),
            CommandNamespaceEntry::alias(
                "cwd",
                "pwd",
                CommandLifecycle::introduced(1)
                    .deprecated_since("0.9.0")
                    .with_replacement("pwd"),
            ),
            CommandNamespaceEntry::reserved(
                "future",
                1,
                "reserved for a future structured command",
                Some("pwd"),
            ),
            CommandNamespaceEntry::core(
                CommandSignature::new("ls", [Carrier::Empty], Carrier::ValueStream),
                CommandLifecycle::introduced(1),
            ),
            CommandNamespaceEntry::core(
                CommandSignature::new("each", [Carrier::ValueStream], Carrier::ValueStream),
                CommandLifecycle::introduced(1),
            ),
        ],
    )
    .expect("valid checker namespace")
}

#[test]
fn deprecated_core_and_alias_are_ordered_warnings_with_replacement_guidance() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", "oldpwd\ncwd\npwd\n");

    let report = ModuleProgramLoader::new(&paths, &sources).analyze_with_commands(
        Path::new("/project/main.fsh"),
        &namespace_checker_registry(),
    );

    assert!(!report.has_errors());
    assert!(report.program().is_some());
    assert_eq!(analysis_codes(&report), ["CMD001", "CMD001"]);
    assert!(
        report
            .issues()
            .iter()
            .all(|issue| issue.severity() == Severity::Warning)
    );
    let diagnostics = analysis_diagnostics(&report);
    assert_eq!(diagnostics[0].severity(), Severity::Warning);
    let first_span = diagnostics[0].labels()[0].span();
    let second_span = diagnostics[1].labels()[0].span();
    assert_eq!(
        (first_span.source_id(), first_span.start(), first_span.end()),
        (SourceId::new(0), 0, 6)
    );
    assert_eq!(
        (
            second_span.source_id(),
            second_span.start(),
            second_span.end()
        ),
        (SourceId::new(0), 7, 10)
    );
    assert!(diagnostics[0].message().contains("`oldpwd` is deprecated"));
    assert!(diagnostics[1].message().contains("`cwd` is deprecated"));
    for diagnostic in diagnostics {
        assert!(diagnostic.message().contains("0.9.0"));
        assert!(
            diagnostic
                .notes()
                .iter()
                .any(|note| note == "use `pwd` instead")
        );
    }
}

#[test]
fn reserved_static_heads_error_in_order_while_forced_and_dynamic_heads_are_suppressed() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "oldpwd\n",
            "future\n",
            "^future\n",
            "let selected = 'future'\n",
            "cmd-${selected}\n",
            "cwd\n",
        ),
    );

    let report = ModuleProgramLoader::new(&paths, &sources).analyze_with_commands(
        Path::new("/project/main.fsh"),
        &namespace_checker_registry(),
    );

    assert!(report.has_errors());
    assert!(report.program().is_none());
    assert_eq!(analysis_codes(&report), ["CMD001", "CMD002", "CMD001"]);
    let diagnostics = analysis_diagnostics(&report);
    assert_eq!(diagnostics[0].severity(), Severity::Warning);
    assert_eq!(diagnostics[2].severity(), Severity::Warning);
    let diagnostic = &diagnostics[1];
    assert_eq!(diagnostic.severity(), Severity::Error);
    let span = diagnostic.labels()[0].span();
    assert_eq!(
        (span.source_id(), span.start(), span.end()),
        (SourceId::new(0), 7, 13)
    );
    assert!(diagnostic.message().contains("`future` is reserved"));
    assert!(
        diagnostic
            .notes()
            .iter()
            .any(|note| note == "use `pwd` instead")
    );
    assert!(
        diagnostic
            .notes()
            .iter()
            .any(|note| { note.contains("`^future`") && note.contains("`command future`") })
    );
}

#[test]
fn a_reserved_stage_has_unknown_carriers_and_suppresses_dependent_cascades() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains("/project/main.fsh", "ls | future | each\n");

    let report = ModuleProgramLoader::new(&paths, &sources).analyze_with_commands(
        Path::new("/project/main.fsh"),
        &namespace_checker_registry(),
    );

    assert_eq!(analysis_codes(&report), ["CMD002"]);
}

#[test]
fn pipeline_walk_visits_functions_closures_and_command_substitutions() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "def nested() { each }\n",
            "let transform = {|| ls | ^cat}\n",
            "echo \"$(ls |& each)\"\n",
        ),
    );

    let report = ModuleProgramLoader::new(&paths, &sources)
        .analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());

    assert_eq!(analysis_codes(&report), ["PIP001", "PIP003", "PIP002"]);
}

#[test]
fn an_unknown_dynamic_head_suppresses_only_its_dependent_edges() {
    let paths = FakeCanonicalizer::default().resolves("/project/main.fsh", "/project/main.fsh");
    let sources = FakeSourceLoader::default().contains(
        "/project/main.fsh",
        concat!(
            "let selected = 'ls'\n",
            "cmd-${selected} | each\n",
            "ls | ^cat\n",
        ),
    );

    let report = ModuleProgramLoader::new(&paths, &sources)
        .analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());

    assert_eq!(analysis_codes(&report), ["PIP003"]);
}

#[test]
fn pipeline_issues_remain_available_beside_an_earlier_graph_phase() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .rejects("/project/missing.fsh", "missing mapping");
    let sources =
        FakeSourceLoader::default().contains("/project/main.fsh", "import './missing.fsh'\neach\n");

    let report = ModuleProgramLoader::new(&paths, &sources)
        .analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());

    assert_eq!(analysis_codes(&report), ["MOD001", "PIP001"]);
    assert!(report.program().is_none());
}

#[test]
fn pipeline_issues_follow_first_visit_source_order_across_imports() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/lib.fsh", "/project/lib.fsh");
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", "import './lib.fsh'\neach\n")
        .contains("/project/lib.fsh", "ls | ^cat\n");

    let report = ModuleProgramLoader::new(&paths, &sources)
        .analyze_with_commands(Path::new("/project/main.fsh"), &standard_registry());
    let diagnostics = analysis_diagnostics(&report);

    assert_eq!(analysis_codes(&report), ["PIP001", "PIP003"]);
    assert_eq!(
        diagnostics
            .iter()
            .map(diagnostics_source_ids)
            .collect::<Vec<_>>(),
        [vec![SourceId::new(0)], vec![SourceId::new(1)]]
    );
}

fn analysis_codes(report: &flash_runtime::module::ModuleAnalysisReport) -> Vec<String> {
    report
        .issues()
        .iter()
        .map(|issue| issue.error().diagnostics()[0].code().to_owned())
        .collect()
}

fn analysis_diagnostics(
    report: &flash_runtime::module::ModuleAnalysisReport,
) -> Vec<flash_syntax::Diagnostic> {
    report
        .issues()
        .iter()
        .map(|issue| issue.error().diagnostics().remove(0))
        .collect()
}

fn diagnostics_source_ids(diagnostic: &flash_syntax::Diagnostic) -> Vec<SourceId> {
    diagnostic
        .labels()
        .iter()
        .map(|label| label.span().source_id())
        .collect()
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

#[test]
fn semantic_queries_expose_visible_names_definitions_references_and_hover_without_a_host() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/library.fsh", "/project/library.fsh");
    let root_text = concat!(
        "import { greet } from './library.fsh'\n",
        "let top: Int = 1\n",
        "## Local helper\n",
        "def local(value: String) -> String {\n",
        "    let inside: String = $value\n",
        "    $inside\n",
        "}\n",
        "let message = greet('world')\n",
        "let copied = $top\n",
    );
    let library_text = concat!(
        "## Imported greeting\n",
        "def greet(name: String) -> String { $name }\n",
        "export { greet }\n",
        "cd '/tmp'\n",
    );
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", root_text)
        .contains("/project/library.fsh", library_text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the semantic fixture is valid");
    let commands = standard_registry();
    let queries = program.semantic_queries(&commands);
    let root = program.graph().root();
    let library = program.graph().imports()[0].target();

    let inside_read = root_text.find("$inside").unwrap() + 2;
    let visible = queries.visible_names(root, inside_read).unwrap();
    assert_eq!(
        visible
            .iter()
            .map(|candidate| (candidate.name(), candidate.kind()))
            .collect::<Vec<_>>(),
        [
            ("args", NameKind::ScriptArguments),
            ("env", NameKind::Intrinsic),
            ("float", NameKind::Intrinsic),
            ("glob", NameKind::Intrinsic),
            ("greet", NameKind::ImportedFunction),
            ("inside", NameKind::Binding),
            ("int", NameKind::Intrinsic),
            ("local", NameKind::Function),
            ("status", NameKind::DynamicBinding),
            ("top", NameKind::Binding),
            ("value", NameKind::Binding),
        ]
    );

    let call = root_text.find("greet('world')").unwrap() + 2;
    let definition = queries.definition_at(root, call).unwrap();
    assert_eq!(definition.module(), library);
    assert_eq!(
        program
            .sources()
            .source(library)
            .unwrap()
            .slice(definition.span())
            .unwrap(),
        "greet"
    );
    let references = queries.references_to(&definition, false);
    assert_eq!(
        references.len(),
        3,
        "the export, import, and call are references"
    );
    assert_eq!(
        references
            .iter()
            .filter(|location| location.module() == root)
            .count(),
        2
    );
    assert_eq!(queries.references_to(&definition, true).len(), 4);

    let inside_declaration = root_text.find("inside: String").unwrap() + 2;
    let local_definition = queries.definition_at(root, inside_declaration).unwrap();
    assert_eq!(local_definition.module(), root);
    assert_eq!(
        program
            .sources()
            .source(root)
            .unwrap()
            .slice(local_definition.span())
            .unwrap(),
        "inside"
    );

    let SemanticHover::Function(hover) = queries.hover_at(root, call).unwrap() else {
        panic!("an imported callable read has function hover data");
    };
    assert_eq!(hover.signature().name(), "greet");
    assert_eq!(
        hover.signature().documentation().unwrap().text(),
        "Imported greeting"
    );

    let top_read = root_text.rfind("$top").unwrap() + 1;
    let SemanticHover::Binding(hover) = queries.hover_at(root, top_read).unwrap() else {
        panic!("a typed lexical read has binding hover data");
    };
    assert_eq!(hover.name(), "top");
    assert_eq!(hover.value_type(), &ValueType::Int);
}

#[test]
fn semantic_queries_expose_signatures_commands_and_named_import_effects() {
    let paths = FakeCanonicalizer::default()
        .resolves("/project/main.fsh", "/project/main.fsh")
        .resolves("/project/library.fsh", "/project/library.fsh");
    let root_text = concat!(
        "import { greet } from './library.fsh'\n",
        "let message = greet('world')\n",
    );
    let library_text = concat!(
        "def greet(name: String) -> String { $name }\n",
        "export { greet }\n",
        "cd '/tmp'\n",
    );
    let sources = FakeSourceLoader::default()
        .contains("/project/main.fsh", root_text)
        .contains("/project/library.fsh", library_text);
    let program = ModuleProgramLoader::new(&paths, &sources)
        .load(Path::new("/project/main.fsh"))
        .expect("the semantic fixture is valid");
    let commands = standard_registry();
    let queries = program.semantic_queries(&commands);
    let root = program.graph().root();
    let library = program.graph().imports()[0].target();

    let argument = root_text.find("'world'").unwrap() + 2;
    let signature = queries.signature_at(root, argument).unwrap();
    assert_eq!(signature.signature().name(), "greet");
    assert_eq!(signature.active_parameter(), 0);
    assert_eq!(signature.definition().module(), library);

    let import_name = root_text.find("greet").unwrap() + 1;
    let effects = queries.import_effects_at(root, import_name).unwrap();
    assert_eq!(effects.target(), library);
    assert!(
        effects
            .direct()
            .occurrences()
            .iter()
            .any(|occurrence| occurrence.effect() == ModuleEffect::WorkingDirectory)
    );
    assert_eq!(effects.direct(), effects.transitive());

    let command_offset = library_text.rfind("cd").unwrap() + 1;
    let SemanticHover::Command(command) = queries.hover_at(library, command_offset).unwrap() else {
        panic!("a standard command head has registry-owned hover data");
    };
    assert_eq!(command.name(), "cd");
    assert_eq!(command.canonical_name(), "cd");
    assert_eq!(
        command.signature().documentation().invocation(),
        "cd [PATH]"
    );

    assert_eq!(
        queries
            .command_candidates("pw")
            .iter()
            .map(|command| command.name())
            .collect::<Vec<_>>(),
        ["pwd"]
    );
    assert_eq!(
        queries.command_flags("kill"),
        [
            "--continue",
            "--hangup",
            "--interrupt",
            "--kill",
            "--stop",
            "--terminate",
        ]
    );
}
