#![forbid(unsafe_code)]

//! Acceptance coverage for concurrent, allocation-independent stdout draining.

use std::any::Any;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flashshell_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, DescriptorReadError, FileActionError,
    FileOpenRequest, PipeEndpoints, PipeError, Platform, ProcessStatus, SpawnError, SpawnRequest,
    TerminateError, WaitError,
};
use flashshell_platform_posix::PosixPlatform;
use flashshell_runtime::command::CommandRegistry;
use flashshell_runtime::eval::{FakeClock, RuntimeErrorKind, SystemClock};
use flashshell_runtime::execute::{
    capture_command_substitution_bytes, capture_command_substitution_text,
    capture_foreground_bytes, capture_foreground_text, execute_foreground_with_stdout_drain,
};
use flashshell_runtime::plan::{ExecutionPlan, SessionOptions, plan_pipeline_with_options};
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::{Environment, ScopeStack};
use flashshell_syntax::{
    ConditionalChain, ParseOutcome, Pipeline, SourceFile, SourceId, StatementKind, parse,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(1), "test.fsh", text)
}

fn pipeline(file: &SourceFile) -> Pipeline {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected a job statement");
    };
    job.chain.or_terms()[0].and_terms()[0].clone()
}

fn chain(file: &SourceFile) -> ConditionalChain {
    let script = match parse(file) {
        ParseOutcome::Complete(script) => script,
        other => panic!("source did not parse: {other:?}"),
    };
    let StatementKind::Job(job) = script.statements()[0].kind() else {
        panic!("expected a job statement");
    };
    job.chain.clone()
}

struct BinProbe;

impl ExecutableProbe for BinProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        matches!(path.to_str(), Some("/bin/tool" | "/bin/other"))
    }
}

fn build(text: &str) -> ExecutionPlan {
    build_with_options(text, &SessionOptions::default())
}

fn build_with_options(text: &str, options: &SessionOptions) -> ExecutionPlan {
    let file = source(text);
    plan_pipeline_with_options(
        &pipeline(&file),
        "/work",
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        options,
    )
    .expect("pipeline plan should build")
}

#[derive(Debug)]
struct TestEndpoint {
    id: usize,
}

impl DescriptorEndpoint for TestEndpoint {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type DescriptorMappings = Arc<Mutex<Vec<Vec<(u32, usize)>>>>;

#[derive(Debug)]
struct DrainPlatform {
    statuses: Vec<ProcessStatus>,
    pipe_count: AtomicUsize,
    spawn_count: Arc<AtomicUsize>,
    wait_count: Arc<AtomicUsize>,
    wait_after_spawns: Option<usize>,
    read_started: Arc<AtomicBool>,
    chunks: Mutex<Vec<Vec<u8>>>,
    read_error: bool,
    mappings: DescriptorMappings,
}

impl DrainPlatform {
    fn new(statuses: Vec<ProcessStatus>, chunks: Vec<Vec<u8>>) -> Self {
        let wait_after_spawns = statuses.len();
        Self {
            statuses,
            pipe_count: AtomicUsize::new(0),
            spawn_count: Arc::new(AtomicUsize::new(0)),
            wait_count: Arc::new(AtomicUsize::new(0)),
            wait_after_spawns: Some(wait_after_spawns),
            read_started: Arc::new(AtomicBool::new(false)),
            chunks: Mutex::new(chunks.into_iter().rev().collect()),
            read_error: false,
            mappings: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn sequential(statuses: Vec<ProcessStatus>, chunks: Vec<Vec<u8>>) -> Self {
        Self {
            wait_after_spawns: None,
            ..Self::new(statuses, chunks)
        }
    }
}

impl Platform for DrainPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
        let index = self.pipe_count.fetch_add(1, Ordering::SeqCst);
        Ok(PipeEndpoints::new(
            Box::new(TestEndpoint { id: index * 2 }),
            Box::new(TestEndpoint { id: index * 2 + 1 }),
        ))
    }

    fn open_file(
        &self,
        _request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(TestEndpoint { id: 10_000 }))
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        Ok(Box::new(TestEndpoint {
            id: descriptor as usize,
        }))
    }

    fn read_descriptor(
        &self,
        _endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        self.read_started.store(true, Ordering::SeqCst);
        if self.read_error {
            return Err(DescriptorReadError::Operation {
                kind: io::ErrorKind::BrokenPipe,
                message: "scripted drain failure".to_owned(),
            });
        }
        let Some(chunk) = self.chunks.lock().expect("chunk lock").pop() else {
            return Ok(0);
        };
        assert!(chunk.len() <= buffer.len());
        buffer[..chunk.len()].copy_from_slice(&chunk);
        Ok(chunk.len())
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        let index = self.spawn_count.fetch_add(1, Ordering::SeqCst);
        let mappings = request
            .descriptors()
            .iter()
            .map(|mapping| {
                let endpoint = mapping
                    .endpoint()
                    .as_any()
                    .downcast_ref::<TestEndpoint>()
                    .expect("recording platform receives its own endpoint");
                (mapping.target(), endpoint.id)
            })
            .collect();
        self.mappings.lock().expect("mapping lock").push(mappings);
        Ok(Box::new(DrainChild {
            status: self.statuses[index],
            wait_after_spawns: self.wait_after_spawns,
            spawn_count: Arc::clone(&self.spawn_count),
            wait_count: Arc::clone(&self.wait_count),
            read_started: Arc::clone(&self.read_started),
        }))
    }
}

#[derive(Debug)]
struct DrainChild {
    status: ProcessStatus,
    wait_after_spawns: Option<usize>,
    spawn_count: Arc<AtomicUsize>,
    wait_count: Arc<AtomicUsize>,
    read_started: Arc<AtomicBool>,
}

impl ChildProcess for DrainChild {
    fn id(&self) -> u64 {
        1
    }

    fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
        if let Some(expected) = self.wait_after_spawns {
            assert_eq!(self.spawn_count.load(Ordering::SeqCst), expected);
        }
        assert!(
            self.read_started.load(Ordering::SeqCst),
            "the drain must start before the first child wait",
        );
        self.wait_count.fetch_add(1, Ordering::SeqCst);
        Ok(self.status)
    }

    fn terminate(&mut self) -> Result<(), TerminateError> {
        Ok(())
    }
}

#[test]
fn drain_runs_concurrently_with_wait_and_streams_chunks_in_order() {
    let plan = build("^tool | ^other");
    let platform = DrainPlatform::new(
        vec![ProcessStatus::Exited(7), ProcessStatus::Exited(0)],
        vec![b"first".to_vec(), b"second".to_vec()],
    );
    let mut chunks = Vec::new();
    let mut drain = |chunk: &[u8]| chunks.push(chunk.to_vec());

    let status =
        execute_foreground_with_stdout_drain(&plan, &platform, &FakeClock::new(), &mut drain)
            .expect("drained pipeline should complete");

    assert_eq!(chunks, [b"first".as_slice(), b"second".as_slice()]);
    assert_eq!(status.code(), Some(0));
    assert_eq!(status.stages().len(), 2);
    assert_eq!(platform.wait_count.load(Ordering::SeqCst), 2);
    let mappings = platform.mappings.lock().expect("mapping lock");
    assert_eq!(mappings[0], [(1, 3)]);
    assert_eq!(mappings[1], [(0, 2), (1, 1)]);
}

#[test]
fn descriptor_read_failure_is_a_runtime_error_at_the_producing_stage() {
    let plan = build("^tool | ^other");
    let platform = DrainPlatform {
        read_error: true,
        ..DrainPlatform::new(
            vec![ProcessStatus::Exited(0), ProcessStatus::Exited(0)],
            Vec::new(),
        )
    };
    let mut drain = |_chunk: &[u8]| {};

    let error =
        execute_foreground_with_stdout_drain(&plan, &platform, &FakeClock::new(), &mut drain)
            .expect_err("read failure should remain a runtime error");

    assert!(matches!(error.kind(), RuntimeErrorKind::CaptureRead(_)));
    assert_eq!(error.span(), plan.stages()[1].span());
    assert_eq!(platform.wait_count.load(Ordering::SeqCst), 2);
}

#[test]
fn a_local_stdout_redirection_overrides_the_capture_pipe() {
    let plan = build("^tool > output");
    let platform = DrainPlatform::new(vec![ProcessStatus::Exited(0)], Vec::new());
    let mut bytes = 0usize;
    let mut drain = |chunk: &[u8]| bytes += chunk.len();

    execute_foreground_with_stdout_drain(&plan, &platform, &FakeClock::new(), &mut drain)
        .expect("redirected command should complete");

    assert_eq!(bytes, 0);
    assert_eq!(
        platform.mappings.lock().expect("mapping lock")[0],
        [(1, 10_000)]
    );
}

#[test]
fn text_capture_is_strict_trims_line_endings_and_retains_status() {
    let plan = build("^tool");
    let platform = DrainPlatform::new(
        vec![ProcessStatus::Exited(23)],
        vec![b"Flash \xF0\x9F\x9A\x80\r\nline\n\r\n\r".to_vec()],
    );

    let capture = capture_foreground_text(&plan, &platform, &FakeClock::new())
        .expect("valid UTF-8 capture should succeed despite nonzero status");

    assert_eq!(capture.output(), "Flash \u{1F680}\r\nline\n\r\n\r");
    assert_eq!(capture.status().code(), Some(23));
}

#[test]
fn byte_capture_is_exact_and_retains_aggregate_status() {
    let plan = build("^tool | ^other");
    let platform = DrainPlatform::new(
        vec![ProcessStatus::Exited(7), ProcessStatus::Exited(0)],
        vec![vec![0xFF, b'\n', 0, b'\r', b'\n']],
    );

    let capture = capture_foreground_bytes(&plan, &platform, &FakeClock::new())
        .expect("arbitrary bytes should be captured exactly");

    assert_eq!(capture.output(), &[0xFF, b'\n', 0, b'\r', b'\n']);
    assert_eq!(capture.status().code(), Some(0));
    assert_eq!(capture.status().stages().len(), 2);
}

#[test]
fn invalid_utf8_is_a_source_anchored_capture_error() {
    let plan = build("^tool");
    let platform = DrainPlatform::new(
        vec![ProcessStatus::Exited(0)],
        vec![b"valid\xFFtail".to_vec()],
    );

    let error = capture_foreground_text(&plan, &platform, &FakeClock::new())
        .expect_err("text capture must not replace invalid bytes");

    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::CaptureInvalidUtf8 {
            valid_up_to: 5,
            error_len: Some(1),
        }
    ));
    assert_eq!(error.span(), plan.span());
    assert!(error.to_string().contains("capture bytes"));
}

#[test]
fn capture_limit_allows_exact_size_and_rejects_the_next_byte_after_drain() {
    let options = SessionOptions::default().with_capture_limit(5);
    let exact_plan = build_with_options("^tool", &options);
    assert_eq!(exact_plan.capture_limit(), 5);
    let exact_platform = DrainPlatform::new(
        vec![ProcessStatus::Exited(0)],
        vec![b"12".to_vec(), b"345".to_vec()],
    );
    let exact = capture_foreground_bytes(&exact_plan, &exact_platform, &FakeClock::new())
        .expect("the exact configured limit should succeed");
    assert_eq!(exact.output(), b"12345");

    let overflow_plan = build_with_options("^tool", &options);
    let overflow_platform = DrainPlatform::new(
        vec![ProcessStatus::Exited(0)],
        vec![b"123".to_vec(), b"456".to_vec(), b"discarded".to_vec()],
    );
    let error = capture_foreground_bytes(&overflow_plan, &overflow_platform, &FakeClock::new())
        .expect_err("one byte beyond the configured limit must fail");

    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::CaptureLimitExceeded { limit: 5 }
    );
    assert_eq!(error.span(), overflow_plan.span());
    assert_eq!(overflow_platform.wait_count.load(Ordering::SeqCst), 1);
    assert!(
        overflow_platform
            .chunks
            .lock()
            .expect("chunk lock")
            .is_empty(),
        "overflow must continue draining every later chunk",
    );
}

#[test]
fn zero_capture_limit_accepts_empty_output_only() {
    let options = SessionOptions::default().with_capture_limit(0);
    let empty_plan = build_with_options("^tool", &options);
    let empty_platform = DrainPlatform::new(vec![ProcessStatus::Exited(0)], Vec::new());
    let empty = capture_foreground_text(&empty_plan, &empty_platform, &FakeClock::new())
        .expect("empty output fits a zero-byte limit");
    assert_eq!(empty.output(), "");

    let byte_plan = build_with_options("^tool", &options);
    let byte_platform = DrainPlatform::new(vec![ProcessStatus::Exited(0)], vec![b"x".to_vec()]);
    let error = capture_foreground_bytes(&byte_plan, &byte_platform, &FakeClock::new())
        .expect_err("any output exceeds a zero-byte limit");
    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::CaptureLimitExceeded { limit: 0 }
    );
}

#[test]
fn command_substitution_captures_every_reached_chain_output_and_last_status() {
    let file = source("^tool && ^other || ^tool");
    let substitution = chain(&file);
    let platform = DrainPlatform::sequential(
        vec![
            ProcessStatus::Exited(0),
            ProcessStatus::Exited(7),
            ProcessStatus::Exited(9),
        ],
        vec![
            b"first\n".to_vec(),
            Vec::new(),
            b"second\n".to_vec(),
            Vec::new(),
            b"third\n".to_vec(),
        ],
    );
    let mut scope = ScopeStack::new();

    let capture = capture_command_substitution_text(
        &substitution,
        "/work".as_ref(),
        &file,
        &mut scope,
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        &SessionOptions::default(),
        &platform,
        &FakeClock::new(),
    )
    .expect("reached conditional terms should capture normally");

    assert_eq!(capture.output(), "first\nsecond\nthird");
    assert_eq!(capture.status().code(), Some(9));
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 3);
}

#[test]
fn command_substitution_limit_spans_the_complete_conditional_chain() {
    let file = source("^tool && ^other");
    let substitution = chain(&file);
    let platform = DrainPlatform::sequential(
        vec![ProcessStatus::Exited(0), ProcessStatus::Exited(0)],
        vec![b"ab".to_vec(), Vec::new(), b"c".to_vec()],
    );
    let options = SessionOptions::default().with_capture_limit(2);
    let mut scope = ScopeStack::new();

    let error = capture_command_substitution_bytes(
        &substitution,
        "/work".as_ref(),
        &file,
        &mut scope,
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        &options,
        &platform,
        &FakeClock::new(),
    )
    .expect_err("the limit must not reset for a later reached pipeline");

    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::CaptureLimitExceeded { limit: 2 }
    );
    assert_eq!(error.span(), substitution.span());
    assert_eq!(platform.wait_count.load(Ordering::SeqCst), 2);
}

#[test]
fn capture_overflow_aborts_unreached_conditional_terms_after_reaping() {
    let file = source("^tool && ^other");
    let substitution = chain(&file);
    let platform = DrainPlatform::sequential(
        vec![ProcessStatus::Exited(0), ProcessStatus::Exited(0)],
        vec![b"overflow".to_vec(), Vec::new(), b"must not run".to_vec()],
    );
    let options = SessionOptions::default().with_capture_limit(1);
    let mut scope = ScopeStack::new();

    let error = capture_command_substitution_bytes(
        &substitution,
        "/work".as_ref(),
        &file,
        &mut scope,
        &Environment::from_snapshot([("PATH", "/bin")]),
        &CommandRegistry::new(),
        &BinProbe,
        &options,
        &platform,
        &FakeClock::new(),
    )
    .expect_err("overflow should abort before a later conditional term starts");

    assert_eq!(
        error.kind(),
        &RuntimeErrorKind::CaptureLimitExceeded { limit: 1 }
    );
    assert_eq!(platform.spawn_count.load(Ordering::SeqCst), 1);
    assert_eq!(platform.wait_count.load(Ordering::SeqCst), 1);
}

#[test]
fn session_capture_limit_defaults_to_eight_mib() {
    assert_eq!(SessionOptions::default().capture_limit(), 8 * 1024 * 1024);
}

struct ExactProbe(PathBuf);

impl ExecutableProbe for ExactProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        path == self.0.as_os_str()
    }
}

#[test]
fn real_posix_pipeline_drains_64_mib_without_executor_side_capture() {
    const LENGTH: usize = 64 * 1024 * 1024;
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flashshell-stream-fixture"));
    let directory = fixture.parent().expect("fixture has a parent").to_owned();
    let name = fixture.file_name().expect("fixture has a name");
    let text = format!(
        "^{0} source {LENGTH} 7 | ^{0} relay 11 | ^{0} relay 13",
        name.to_string_lossy(),
    );
    let file = source(&text);
    let plan = plan_pipeline_with_options(
        &pipeline(&file),
        &directory,
        &file,
        &mut ScopeStack::new(),
        &Environment::from_snapshot([("PATH", directory.as_os_str().to_os_string())]),
        &CommandRegistry::new(),
        &ExactProbe(fixture),
        &SessionOptions::default(),
    )
    .expect("real source plan should build");
    let mut length = 0usize;
    let mut all_x = true;
    let mut drain = |chunk: &[u8]| {
        length += chunk.len();
        all_x &= chunk.iter().all(|byte| *byte == b'x');
    };

    let status = execute_foreground_with_stdout_drain(
        &plan,
        &PosixPlatform,
        &SystemClock::new(),
        &mut drain,
    )
    .expect("64 MiB drain should complete without deadlock");

    assert_eq!(status.code(), Some(13));
    assert_eq!(status.stages().len(), 3);
    assert_eq!(length, LENGTH);
    assert!(all_x);
}
