//! Maintained Git/Cargo probes and bounded execution.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use opaal_platform::Platform;
use opaal_platform::operational::{
    OperationalAdapter, ProcessExit, ProcessRequest, ReadFileRequest, validate_child_environment,
};

use crate::authority::{CapabilityRequest, EffectSet};
use crate::context::OperationalContext;
use crate::project::{LockedTool, MaintainedAdapter, ToolLock};
use crate::{Duration, Signal, Status};

use super::{MAX_PROCESS_OUTPUT_BYTES, ModuleError, adapter_error, authorize, integrity};

pub const MAX_TOOL_EXECUTABLE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PROCESS_ATTEMPTS: usize = 8;
pub const MAX_PROCESS_DURATION: StdDuration = opaal_platform::operational::MAX_PROCESS_DURATION;
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
const REQUIRED_ENVIRONMENT: [&str; 11] = [
    "HOME",
    "TMPDIR",
    "PATH",
    "CARGO_HOME",
    "RUSTC",
    "RUSTDOC",
    "LC_ALL",
    "TZ",
    "CARGO_NET_OFFLINE",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_GLOBAL",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessBudget {
    attempts: usize,
}

impl ProcessBudget {
    #[must_use]
    pub const fn attempts(&self) -> usize {
        self.attempts
    }

    fn ensure_available(&self) -> Result<(), ModuleError> {
        if self.attempts == MAX_PROCESS_ATTEMPTS {
            return Err(ModuleError::invalid(
                "PROCESS001",
                "process attempts exceed eight",
            ));
        }
        Ok(())
    }

    fn charge(&mut self) {
        self.attempts += 1;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    status: Status,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ToolResult {
    #[must_use]
    pub const fn status(&self) -> &Status {
        &self.status
    }
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

pub fn locked_path(tool: &LockedTool) -> PathBuf {
    PathBuf::from(OsString::from_vec(tool.path().bytes().to_vec()))
}

pub fn child_environment(lock: &ToolLock) -> Result<Vec<(OsString, OsString)>, ModuleError> {
    let actual = lock
        .variables()
        .iter()
        .map(|variable| variable.name())
        .collect::<BTreeSet<_>>();
    if REQUIRED_ENVIRONMENT
        .iter()
        .any(|required| !actual.contains(required))
    {
        return Err(ModuleError::invalid(
            "PROCESS002",
            "the sanitized child environment omits a required maintained-tool variable",
        ));
    }
    let environment = lock
        .variables()
        .iter()
        .map(|variable| {
            (
                OsString::from(variable.name()),
                OsString::from_vec(variable.value().bytes().to_vec()),
            )
        })
        .collect::<Vec<_>>();
    validate_child_environment(&environment)?;
    Ok(environment)
}

pub fn verify_executable(
    adapter: &dyn OperationalAdapter,
    tool: &LockedTool,
) -> Result<(), ModuleError> {
    let path = locked_path(tool);
    let bytes = adapter.read_file(ReadFileRequest {
        root: Path::new("/"),
        path: &path,
        max_bytes: MAX_TOOL_EXECUTABLE_BYTES,
    })?;
    if integrity::sha256(&bytes) != tool.digest() {
        return Err(ModuleError::invalid(
            "PROCESS003",
            "locked executable digest changed",
        ));
    }
    Ok(())
}

fn verify_retained_executable(file: &File, tool: &LockedTool) -> Result<(), ModuleError> {
    let metadata = file
        .metadata()
        .map_err(|error| ModuleError::invalid("PROCESS003", error.to_string()))?;
    if !metadata.is_file() {
        return Err(ModuleError::invalid(
            "PROCESS003",
            "retained executable is not a regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_TOOL_EXECUTABLE_BYTES)
            .min(MAX_TOOL_EXECUTABLE_BYTES),
    );
    let mut offset = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    while bytes.len() <= MAX_TOOL_EXECUTABLE_BYTES {
        let remaining = MAX_TOOL_EXECUTABLE_BYTES + 1 - bytes.len();
        let chunk = remaining.min(buffer.len());
        let read = file
            .read_at(&mut buffer[..chunk], offset)
            .map_err(|error| ModuleError::invalid("PROCESS003", error.to_string()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        offset = offset
            .checked_add(u64::try_from(read).expect("read size fits u64"))
            .ok_or_else(|| ModuleError::invalid("PROCESS003", "executable offset overflow"))?;
    }
    if bytes.len() > MAX_TOOL_EXECUTABLE_BYTES {
        return Err(ModuleError::invalid(
            "PROCESS003",
            "locked executable exceeds its byte limit",
        ));
    }
    if integrity::sha256(&bytes) != tool.digest() {
        return Err(ModuleError::invalid(
            "PROCESS003",
            "locked executable digest changed",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn probe(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    lock: &ToolLock,
    tool_id: &str,
    cwd: &Path,
    budget: &mut ProcessBudget,
) -> Result<ToolResult, ModuleError> {
    probe_inner(
        context, effects, platform, adapter, lock, tool_id, cwd, budget, None,
    )
}

/// On a supported host, probe the exact executable descriptor retained by
/// accepted-plan identity verification without resolving its pathname again.
#[allow(clippy::too_many_arguments)]
pub fn probe_retained(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    lock: &ToolLock,
    tool_id: &str,
    cwd: &Path,
    budget: &mut ProcessBudget,
    executable_file: &File,
) -> Result<ToolResult, ModuleError> {
    probe_inner(
        context,
        effects,
        platform,
        adapter,
        lock,
        tool_id,
        cwd,
        budget,
        Some(executable_file),
    )
}

#[allow(clippy::too_many_arguments)]
fn probe_inner(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    lock: &ToolLock,
    tool_id: &str,
    cwd: &Path,
    budget: &mut ProcessBudget,
    executable_file: Option<&File>,
) -> Result<ToolResult, ModuleError> {
    let tool = lock.tools().get(tool_id).ok_or_else(|| {
        ModuleError::invalid("PROCESS004", format!("unknown locked tool `{tool_id}`"))
    })?;
    budget.ensure_available()?;
    let fixed = match tool.adapter() {
        MaintainedAdapter::Git => &["--version"][..],
        MaintainedAdapter::Cargo => &["--version", "--verbose"][..],
    };
    let result = run_inner(
        context,
        effects,
        platform,
        adapter,
        lock,
        tool_id,
        fixed,
        cwd,
        MAX_PROBE_OUTPUT_BYTES,
        MAX_PROBE_OUTPUT_BYTES,
        MAX_PROCESS_DURATION,
        budget,
        executable_file,
    )?;
    if !result.status.is_ok() {
        return Err(ModuleError::invalid(
            "PROCESS005",
            "maintained version probe returned nonzero status",
        ));
    }
    let observed = parse_probe(tool.adapter(), &result.stdout)?;
    if observed != tool.version().to_string() {
        return Err(ModuleError::invalid(
            "PROCESS006",
            format!(
                "maintained probe reported {observed}, lock requires {}",
                tool.version()
            ),
        ));
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    lock: &ToolLock,
    tool_id: &str,
    arguments: &[&str],
    cwd: &Path,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: StdDuration,
    budget: &mut ProcessBudget,
) -> Result<ToolResult, ModuleError> {
    run_inner(
        context,
        effects,
        platform,
        adapter,
        lock,
        tool_id,
        arguments,
        cwd,
        stdout_limit,
        stderr_limit,
        timeout,
        budget,
        None,
    )
}

/// On a supported host, run the exact executable descriptor retained by
/// accepted-plan identity verification without resolving its pathname again.
#[allow(clippy::too_many_arguments)]
pub fn run_retained(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    lock: &ToolLock,
    tool_id: &str,
    arguments: &[&str],
    cwd: &Path,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: StdDuration,
    budget: &mut ProcessBudget,
    executable_file: &File,
) -> Result<ToolResult, ModuleError> {
    run_inner(
        context,
        effects,
        platform,
        adapter,
        lock,
        tool_id,
        arguments,
        cwd,
        stdout_limit,
        stderr_limit,
        timeout,
        budget,
        Some(executable_file),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    context: &OperationalContext,
    effects: &EffectSet,
    platform: &dyn Platform,
    adapter: &dyn OperationalAdapter,
    lock: &ToolLock,
    tool_id: &str,
    arguments: &[&str],
    cwd: &Path,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: StdDuration,
    budget: &mut ProcessBudget,
    executable_file: Option<&File>,
) -> Result<ToolResult, ModuleError> {
    if stdout_limit > MAX_PROCESS_OUTPUT_BYTES
        || stderr_limit > MAX_PROCESS_OUTPUT_BYTES
        || timeout > MAX_PROCESS_DURATION
    {
        return Err(ModuleError::invalid(
            "PROCESS007",
            "process output or deadline exceeds its fixed ceiling",
        ));
    }
    let tool = lock.tools().get(tool_id).ok_or_else(|| {
        ModuleError::invalid("PROCESS004", format!("unknown locked tool `{tool_id}`"))
    })?;
    budget.ensure_available()?;
    let authority = CapabilityRequest::process_run(tool_id.to_owned())
        .map_err(|error| ModuleError::invalid("PROCESS008", error.to_string()))?;
    authorize(context, effects, &authority, platform)?;
    match executable_file {
        Some(file) if cfg!(target_os = "linux") => verify_retained_executable(file, tool)?,
        _ => verify_executable(adapter, tool)?,
    }
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    let environment = child_environment(lock)?;
    budget.charge();
    let executable = locked_path(tool);
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(executable.as_os_str().to_owned());
    argv.extend(arguments.iter().map(OsString::from));
    let output = adapter
        .run_process(
            ProcessRequest {
                executable: &executable,
                executable_file,
                argv: &argv,
                environment: &environment,
                cwd,
                stdout_limit,
                stderr_limit,
                timeout,
            },
            &|| context.poll_cancellation().is_some(),
        )
        .map_err(|error| adapter_error(context, error))?;
    if let Some(reason) = context.poll_cancellation() {
        return Err(ModuleError::Cancelled(reason));
    }
    let elapsed = i128::try_from(output.elapsed().as_nanos()).map_err(|_| {
        ModuleError::invalid("PROCESS009", "process duration exceeds the runtime range")
    })?;
    let duration = Duration::from_nanos(elapsed);
    let status = match output.status() {
        ProcessExit::Exited(code) => Status::exit(i64::from(code), duration),
        ProcessExit::Signaled(number) => Status::signaled(
            Signal::new(Some(i64::from(number)), None).expect("a signal number is an identity"),
            duration,
        ),
    }
    .map_err(|error| ModuleError::invalid("PROCESS010", error.to_string()))?;
    Ok(ToolResult {
        status,
        stdout: context.redact_bytes(output.stdout()),
        stderr: context.redact_bytes(output.stderr()),
    })
}

fn parse_probe(adapter: MaintainedAdapter, bytes: &[u8]) -> Result<String, ModuleError> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        ModuleError::invalid("PROCESS011", format!("probe output is not UTF-8: {error}"))
    })?;
    let first = text.lines().next().unwrap_or_default();
    let prefix = match adapter {
        MaintainedAdapter::Git => "git version ",
        MaintainedAdapter::Cargo => "cargo ",
    };
    let version = first
        .strip_prefix(prefix)
        .and_then(|rest| rest.split_ascii_whitespace().next())
        .ok_or_else(|| {
            ModuleError::invalid("PROCESS012", "probe output has an unsupported shape")
        })?;
    let parsed = semver::Version::parse(version).map_err(|error| {
        ModuleError::invalid("PROCESS012", format!("invalid probe version: {error}"))
    })?;
    Ok(parsed.to_string())
}
