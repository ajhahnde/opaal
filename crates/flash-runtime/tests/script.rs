#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use flash_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, DescriptorReadError, FakePlatform,
    FileActionError, FileOpenRequest, PipeEndpoints, PipeError, Platform, ProcessGroup,
    RecordingPlatform, SpawnError, SpawnRequest,
};
use flash_platform_posix::PosixPlatform;
use flash_runtime::Environment;
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::{Clock, FakeClock};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_chain_subshell;

struct Probe {
    paths: Vec<PathBuf>,
}

impl Probe {
    fn new(paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            paths: paths.into_iter().map(Into::into).collect(),
        }
    }
}

impl ExecutableProbe for Probe {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.paths
            .iter()
            .any(|candidate| candidate.as_os_str() == path)
    }
}

struct RecordingPosix {
    requested: Mutex<Vec<ProcessGroup>>,
}

impl RecordingPosix {
    fn new() -> Self {
        Self {
            requested: Mutex::new(Vec::new()),
        }
    }

    fn requested(&self) -> Vec<ProcessGroup> {
        self.requested.lock().expect("requested-group lock").clone()
    }
}

impl Platform for RecordingPosix {
    fn capabilities(&self) -> Capabilities {
        PosixPlatform.capabilities()
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

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        PosixPlatform.inherit_descriptor(descriptor)
    }

    fn read_descriptor(
        &self,
        endpoint: &dyn DescriptorEndpoint,
        buffer: &mut [u8],
    ) -> Result<usize, DescriptorReadError> {
        PosixPlatform.read_descriptor(endpoint, buffer)
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        let child = PosixPlatform.spawn(request)?;
        self.requested
            .lock()
            .expect("requested-group lock")
            .push(request.process_group());
        Ok(child)
    }
}

fn environment() -> Environment {
    Environment::from_snapshot([("PATH", "/bin"), ("NAME", "value")])
}

#[test]
fn isolated_chain_external_processes_inherit_the_callers_group() {
    let mut environment = environment();
    let platform = RecordingPlatform::new(FakePlatform::new(Capabilities::full()));
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
    let mut output = Vec::new();

    let completion = execute_chain_subshell(
        "<chain>",
        "^tool $NAME && ^other",
        std::path::Path::new("/work"),
        &mut environment,
        &standard_registry(),
        &Probe::new(["/bin/tool", "/bin/other"]),
        &SessionOptions::default(),
        &platform,
        clock,
        &mut output,
    )
    .expect("the isolated chain should complete");

    assert!(completion.status().is_some_and(|status| status.is_ok()));
    let records = platform.spawn_log().records();
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.requested() == ProcessGroup::Inherit)
    );
}

#[test]
fn isolated_chain_mixed_pipeline_processes_inherit_the_callers_group() {
    let fixture = PathBuf::from(env!("CARGO_BIN_EXE_flash-status-fixture"));
    let fixture_directory = fixture
        .parent()
        .expect("the fixture executable should have a parent");
    let mut environment =
        Environment::from_snapshot([("PATH", fixture_directory.as_os_str().to_os_string())]);
    let platform = RecordingPosix::new();
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
    let cwd = std::env::current_dir().expect("the test current directory should be readable");
    let mut output = Vec::new();

    execute_chain_subshell(
        "<chain>",
        "^flash-status-fixture exit 0 | decode bytes | encode bytes",
        &cwd,
        &mut environment,
        &standard_registry(),
        &Probe::new([fixture]),
        &SessionOptions::default(),
        &platform,
        clock,
        &mut output,
    )
    .expect("the isolated mixed pipeline should complete");

    assert_eq!(platform.requested(), [ProcessGroup::Inherit]);
}

#[test]
fn isolated_chain_internal_output_uses_the_injected_sink() {
    let mut environment = environment();
    let mut output = Vec::new();

    execute_chain_subshell(
        "<chain>",
        "which pwd | get kind | encode utf8",
        std::path::Path::new("/work"),
        &mut environment,
        &standard_registry(),
        &Probe::new(std::iter::empty::<PathBuf>()),
        &SessionOptions::default(),
        &FakePlatform::none(),
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .expect("the isolated internal chain should complete");

    assert_eq!(output, b"internal");
}
