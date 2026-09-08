//! Bounded host operations used by OPAAL's maintained operational modules.
//!
//! This boundary is deliberately separate from [`crate::Platform`]. The
//! latter owns the general shell host, while this module admits only the
//! bounded file, clock, HTTP, and maintained-process operations needed by the
//! operational standard-module contract. Every request carries its complete
//! byte and time ceilings and an explicit cancellation poll.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Maximum ordinary HTTP header count for one request or response.
pub const MAX_HTTP_HEADERS: usize = 128;
/// Maximum aggregate encoded HTTP header bytes.
pub const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
/// Maximum HTTP request or response body bytes.
pub const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Maximum duration of one maintained HTTP request.
pub const MAX_HTTP_DURATION: Duration = Duration::from_secs(30);
/// Maximum retained stdout or stderr bytes for one process attempt.
pub const MAX_PROCESS_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum duration of one maintained process attempt.
pub const MAX_PROCESS_DURATION: Duration = Duration::from_secs(10 * 60);

/// A stable class for a bounded adapter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalErrorKind {
    InvalidInput,
    NotFound,
    NotRegular,
    Symlink,
    OutsideRoot,
    AlreadyExists,
    LimitExceeded,
    Cancelled,
    TimedOut,
    Unsupported,
    Network,
    Tls,
    Protocol,
    Io(io::ErrorKind),
}

/// A redaction-safe failure from a bounded adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationalError {
    kind: OperationalErrorKind,
    message: String,
}

impl OperationalError {
    pub fn new(kind: OperationalErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> OperationalErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for OperationalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OperationalError {}

impl From<io::Error> for OperationalError {
    fn from(error: io::Error) -> Self {
        Self::new(OperationalErrorKind::Io(error.kind()), error.to_string())
    }
}

/// Read one exact, lexically contained regular file without following links.
#[derive(Clone, Copy, Debug)]
pub struct ReadFileRequest<'a> {
    pub root: &'a Path,
    pub path: &'a Path,
    pub max_bytes: usize,
}

/// Atomically replace one exact, lexically contained regular evidence file.
#[derive(Clone, Copy, Debug)]
pub struct AtomicWriteRequest<'a> {
    pub root: &'a Path,
    pub path: &'a Path,
    pub bytes: &'a [u8],
    pub max_bytes: usize,
}

/// One validated ordinary HTTP header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpHeader {
    name: String,
    value: Vec<u8>,
}

impl HttpHeader {
    pub fn new(name: impl Into<String>, value: Vec<u8>) -> Result<Self, OperationalError> {
        let name = name.into();
        if name.is_empty()
            || !name.bytes().all(is_lowercase_header_token_byte)
            || value
                .iter()
                .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
        {
            return Err(OperationalError::new(
                OperationalErrorKind::InvalidInput,
                "HTTP headers require a lowercase token name and a value without line breaks",
            ));
        }
        Ok(Self { name, value })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// The sole plaintext-bearing header slot. Debug output never includes bytes.
pub struct MaterializedSecretHeader<'a> {
    pub name: &'a str,
    pub value: &'a [u8],
}

impl fmt::Debug for MaterializedSecretHeader<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MaterializedSecretHeader")
            .field("name", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

/// One normalized, bounded HTTP request.
pub struct HttpRequest<'a> {
    pub connect_host: &'a str,
    pub port: u16,
    pub tls_server_name: Option<&'a str>,
    pub ca_pem: Option<&'a [u8]>,
    pub method: &'a str,
    pub path_and_query: &'a str,
    pub headers: &'a [HttpHeader],
    pub secret_header: Option<MaterializedSecretHeader<'a>>,
    pub body: &'a [u8],
    pub max_response_bytes: usize,
    pub timeout: Duration,
}

impl fmt::Debug for HttpRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("connect_host", &self.connect_host)
            .field("port", &self.port)
            .field("tls_server_name", &self.tls_server_name)
            .field("method", &self.method)
            .field("path_and_query", &self.path_and_query)
            .field("headers", &self.headers)
            .field("secret_header", &self.secret_header)
            .field("body_bytes", &self.body.len())
            .field("max_response_bytes", &self.max_response_bytes)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A bounded response whose status is ordinary data, not an adapter error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    status: u16,
    headers: Vec<HttpHeader>,
    body: Vec<u8>,
}

impl HttpResponse {
    #[must_use]
    pub fn new(status: u16, headers: Vec<HttpHeader>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn headers(&self) -> &[HttpHeader] {
        &self.headers
    }

    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// One exact maintained-process request with a complete child environment.
#[derive(Debug)]
pub struct ProcessRequest<'a> {
    pub executable: &'a Path,
    pub argv: &'a [OsString],
    pub environment: &'a [(OsString, OsString)],
    pub cwd: &'a Path,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
    pub timeout: Duration,
}

/// A completed maintained process. Nonzero status remains ordinary data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutput {
    status: ProcessExit,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
}

impl ProcessOutput {
    #[must_use]
    pub fn new(status: ProcessExit, stdout: Vec<u8>, stderr: Vec<u8>, elapsed: Duration) -> Self {
        Self {
            status,
            stdout,
            stderr,
            elapsed,
        }
    }

    #[must_use]
    pub const fn status(&self) -> ProcessExit {
        self.status
    }

    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// The exact terminal state of a maintained process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessExit {
    Exited(i32),
    Signaled(i32),
}

/// The complete bounded operational adapter boundary.
pub trait OperationalAdapter: Send + Sync {
    fn read_file(&self, request: ReadFileRequest<'_>) -> Result<Vec<u8>, OperationalError>;
    fn write_atomic(&self, request: AtomicWriteRequest<'_>) -> Result<(), OperationalError>;
    fn wall_time_unix_nanos(&self) -> Result<i128, OperationalError>;
    fn monotonic_nanos(&self) -> Result<u128, OperationalError>;
    fn http_request(
        &self,
        request: HttpRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<HttpResponse, OperationalError>;
    fn run_process(
        &self,
        request: ProcessRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ProcessOutput, OperationalError>;
}

/// One observable fake call without secret or body payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationalCall {
    Read {
        root: PathBuf,
        path: PathBuf,
        max_bytes: usize,
    },
    Write {
        root: PathBuf,
        path: PathBuf,
        bytes: usize,
        max_bytes: usize,
    },
    WallTime,
    MonotonicTime,
    Http {
        host: String,
        port: u16,
        method: String,
        path: String,
        secret_header: Option<String>,
    },
    Process {
        executable: PathBuf,
        argv: Vec<OsString>,
        environment: Vec<(OsString, OsString)>,
    },
}

#[derive(Default)]
struct FakeState {
    files: BTreeMap<PathBuf, Vec<u8>>,
    calls: Vec<OperationalCall>,
    http: VecDeque<Result<HttpResponse, OperationalError>>,
    processes: VecDeque<Result<ProcessOutput, OperationalError>>,
    wall_nanos: i128,
    monotonic_nanos: u128,
}

/// A deterministic, host-free adapter with a payload-free call log.
#[derive(Clone, Default)]
pub struct FakeOperationalAdapter {
    state: Arc<Mutex<FakeState>>,
}

impl fmt::Debug for FakeOperationalAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FakeOperationalAdapter")
            .finish_non_exhaustive()
    }
}

impl FakeOperationalAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_file(&self, path: impl Into<PathBuf>, bytes: Vec<u8>) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .files
            .insert(path.into(), bytes);
    }

    pub fn push_http(&self, response: Result<HttpResponse, OperationalError>) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .http
            .push_back(response);
    }

    pub fn push_process(&self, output: Result<ProcessOutput, OperationalError>) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .processes
            .push_back(output);
    }

    pub fn set_times(&self, wall_nanos: i128, monotonic_nanos: u128) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.wall_nanos = wall_nanos;
        state.monotonic_nanos = monotonic_nanos;
    }

    #[must_use]
    pub fn calls(&self) -> Vec<OperationalCall> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .clone()
    }

    #[must_use]
    pub fn file(&self, path: &Path) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .files
            .get(path)
            .cloned()
    }
}

impl OperationalAdapter for FakeOperationalAdapter {
    fn read_file(&self, request: ReadFileRequest<'_>) -> Result<Vec<u8>, OperationalError> {
        validate_file_path(request.root, request.path)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(OperationalCall::Read {
            root: request.root.to_owned(),
            path: request.path.to_owned(),
            max_bytes: request.max_bytes,
        });
        let bytes = state.files.get(request.path).cloned().ok_or_else(|| {
            OperationalError::new(OperationalErrorKind::NotFound, "fake file is absent")
        })?;
        if bytes.len() > request.max_bytes {
            return Err(OperationalError::new(
                OperationalErrorKind::LimitExceeded,
                "file exceeds its byte limit",
            ));
        }
        Ok(bytes)
    }

    fn write_atomic(&self, request: AtomicWriteRequest<'_>) -> Result<(), OperationalError> {
        validate_file_path(request.root, request.path)?;
        if request.bytes.len() > request.max_bytes {
            return Err(OperationalError::new(
                OperationalErrorKind::LimitExceeded,
                "write exceeds its byte limit",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(OperationalCall::Write {
            root: request.root.to_owned(),
            path: request.path.to_owned(),
            bytes: request.bytes.len(),
            max_bytes: request.max_bytes,
        });
        state
            .files
            .insert(request.path.to_owned(), request.bytes.to_vec());
        Ok(())
    }

    fn wall_time_unix_nanos(&self) -> Result<i128, OperationalError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(OperationalCall::WallTime);
        Ok(state.wall_nanos)
    }

    fn monotonic_nanos(&self) -> Result<u128, OperationalError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(OperationalCall::MonotonicTime);
        Ok(state.monotonic_nanos)
    }

    fn http_request(
        &self,
        request: HttpRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<HttpResponse, OperationalError> {
        validate_http_request(&request)?;
        if cancelled() {
            return Err(OperationalError::new(
                OperationalErrorKind::Cancelled,
                "HTTP request was cancelled",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(OperationalCall::Http {
            host: request.connect_host.to_owned(),
            port: request.port,
            method: request.method.to_owned(),
            path: request.path_and_query.to_owned(),
            secret_header: request
                .secret_header
                .as_ref()
                .map(|header| header.name.to_owned()),
        });
        let response = state.http.pop_front().unwrap_or_else(|| {
            Err(OperationalError::new(
                OperationalErrorKind::Unsupported,
                "no fake HTTP response was scripted",
            ))
        })?;
        if response.body.len() > request.max_response_bytes {
            return Err(OperationalError::new(
                OperationalErrorKind::LimitExceeded,
                "HTTP response exceeds its byte limit",
            ));
        }
        validate_http_response(&response)?;
        if cancelled() {
            return Err(OperationalError::new(
                OperationalErrorKind::Cancelled,
                "HTTP request was cancelled",
            ));
        }
        Ok(response)
    }

    fn run_process(
        &self,
        request: ProcessRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ProcessOutput, OperationalError> {
        validate_process_request(&request)?;
        if cancelled() {
            return Err(OperationalError::new(
                OperationalErrorKind::Cancelled,
                "process was cancelled",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.calls.push(OperationalCall::Process {
            executable: request.executable.to_owned(),
            argv: request.argv.to_vec(),
            environment: request.environment.to_vec(),
        });
        let output = state.processes.pop_front().unwrap_or_else(|| {
            Err(OperationalError::new(
                OperationalErrorKind::Unsupported,
                "no fake process result was scripted",
            ))
        })?;
        if output.stdout.len() > request.stdout_limit || output.stderr.len() > request.stderr_limit
        {
            return Err(OperationalError::new(
                OperationalErrorKind::LimitExceeded,
                "process output exceeds its byte limit",
            ));
        }
        if output.elapsed > request.timeout {
            return Err(OperationalError::new(
                OperationalErrorKind::TimedOut,
                "process exceeded its deadline",
            ));
        }
        if cancelled() {
            return Err(OperationalError::new(
                OperationalErrorKind::Cancelled,
                "process was cancelled",
            ));
        }
        Ok(output)
    }
}

fn is_lowercase_header_token_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn validate_file_path(root: &Path, target: &Path) -> Result<(), OperationalError> {
    if !is_normalized_absolute(root) || !is_normalized_absolute(target) || !target.starts_with(root)
    {
        return Err(OperationalError::new(
            OperationalErrorKind::OutsideRoot,
            "path is outside its lexical root",
        ));
    }
    let relative = target.strip_prefix(root).map_err(|_| {
        OperationalError::new(
            OperationalErrorKind::OutsideRoot,
            "path is outside its lexical root",
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(OperationalError::new(
            OperationalErrorKind::InvalidInput,
            "file path must be a normalized child of its root",
        ));
    }
    Ok(())
}

fn is_normalized_absolute(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::CurDir
            | std::path::Component::ParentDir
            | std::path::Component::Prefix(_) => return false,
        }
    }
    normalized == path
}

/// Validate the host-independent shape and fixed ceilings of one HTTP request.
pub fn validate_http_request(request: &HttpRequest<'_>) -> Result<(), OperationalError> {
    let count = request
        .headers
        .len()
        .checked_add(usize::from(request.secret_header.is_some()))
        .ok_or_else(|| {
            OperationalError::new(
                OperationalErrorKind::LimitExceeded,
                "HTTP header count overflow",
            )
        })?;
    if request.connect_host.is_empty()
        || request.port == 0
        || request.timeout.is_zero()
        || request.method.is_empty()
        || !request.method.bytes().any(|byte| byte.is_ascii_uppercase())
        || !request
            .method
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'-')
        || !valid_connect_host(request.connect_host)
        || !valid_path_and_query(request.path_and_query)
        || count > MAX_HTTP_HEADERS
        || request.body.len() > MAX_HTTP_BODY_BYTES
        || request.max_response_bytes > MAX_HTTP_BODY_BYTES
        || request.timeout > MAX_HTTP_DURATION
    {
        return Err(OperationalError::new(
            OperationalErrorKind::InvalidInput,
            "invalid or over-budget HTTP request",
        ));
    }
    let mut bytes = 0usize;
    let mut names = BTreeSet::new();
    for header in request.headers {
        if !names.insert(header.name.as_str()) || is_transport_header(&header.name) {
            return Err(OperationalError::new(
                OperationalErrorKind::InvalidInput,
                "HTTP request contains a duplicate or adapter-owned header",
            ));
        }
        bytes = bytes
            .checked_add(header.name.len() + header.value.len() + 4)
            .ok_or_else(|| {
                OperationalError::new(
                    OperationalErrorKind::LimitExceeded,
                    "HTTP header size overflow",
                )
            })?;
    }
    if let Some(header) = &request.secret_header {
        if header.name.is_empty()
            || !header.name.bytes().all(is_lowercase_header_token_byte)
            || !names.insert(header.name)
            || is_transport_header(header.name)
            || header
                .value
                .iter()
                .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
        {
            return Err(OperationalError::new(
                OperationalErrorKind::InvalidInput,
                "invalid secret HTTP header",
            ));
        }
        bytes = bytes
            .checked_add(header.name.len() + header.value.len() + 4)
            .ok_or_else(|| {
                OperationalError::new(
                    OperationalErrorKind::LimitExceeded,
                    "HTTP header size overflow",
                )
            })?;
    }
    if bytes > MAX_HTTP_HEADER_BYTES {
        return Err(OperationalError::new(
            OperationalErrorKind::LimitExceeded,
            "HTTP headers exceed 64 KiB",
        ));
    }
    match (request.tls_server_name, request.ca_pem) {
        (Some(_), Some(ca)) if !ca.is_empty() && ca.len() <= MAX_HTTP_BODY_BYTES => Ok(()),
        (None, None) => Ok(()),
        _ => Err(OperationalError::new(
            OperationalErrorKind::InvalidInput,
            "TLS server name and CA must be supplied together",
        )),
    }
}

fn validate_http_response(response: &HttpResponse) -> Result<(), OperationalError> {
    if !(100..=599).contains(&response.status) {
        return Err(OperationalError::new(
            OperationalErrorKind::Protocol,
            "HTTP response status is outside 100 through 599",
        ));
    }
    if response.headers.len() > MAX_HTTP_HEADERS {
        return Err(OperationalError::new(
            OperationalErrorKind::LimitExceeded,
            "HTTP response header count exceeds 128",
        ));
    }
    let bytes = response.headers.iter().try_fold(0usize, |sum, header| {
        sum.checked_add(header.name.len() + header.value.len() + 4)
            .ok_or_else(|| {
                OperationalError::new(
                    OperationalErrorKind::LimitExceeded,
                    "HTTP response header size overflow",
                )
            })
    })?;
    if bytes > MAX_HTTP_HEADER_BYTES {
        return Err(OperationalError::new(
            OperationalErrorKind::LimitExceeded,
            "HTTP response headers exceed 64 KiB",
        ));
    }
    Ok(())
}

fn is_transport_header(name: &str) -> bool {
    matches!(
        name,
        "host" | "connection" | "content-length" | "transfer-encoding"
    )
}

/// Validate the host-independent shape and fixed ceilings of one process request.
pub fn validate_process_request(request: &ProcessRequest<'_>) -> Result<(), OperationalError> {
    if !request.executable.is_absolute()
        || request.argv.is_empty()
        || !request.cwd.is_absolute()
        || request.timeout.is_zero()
        || request.timeout > MAX_PROCESS_DURATION
        || request.stdout_limit > MAX_PROCESS_OUTPUT_BYTES
        || request.stderr_limit > MAX_PROCESS_OUTPUT_BYTES
        || request
            .argv
            .iter()
            .any(|argument| argument.as_encoded_bytes().contains(&0))
    {
        return Err(OperationalError::new(
            OperationalErrorKind::InvalidInput,
            "process requires absolute executable/cwd, argv zero, and a nonzero timeout",
        ));
    }
    validate_child_environment(request.environment)
}

fn valid_connect_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return false;
    }
    !host.is_empty()
        && host.len() <= 253
        && !host.ends_with('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

fn valid_path_and_query(value: &str) -> bool {
    value.starts_with('/')
        && value.bytes().all(|byte| {
            !byte.is_ascii_control() && !matches!(byte, b' ' | b'\\' | b'"' | b'<' | b'>' | b'`')
        })
}

/// Validate a complete child environment without consulting ambient state.
pub fn validate_child_environment(
    environment: &[(OsString, OsString)],
) -> Result<(), OperationalError> {
    let mut names = std::collections::BTreeSet::<&OsStr>::new();
    for (name, value) in environment {
        let name_bytes = name.as_encoded_bytes();
        if name_bytes.is_empty()
            || name_bytes.contains(&b'=')
            || name_bytes.contains(&0)
            || value.as_encoded_bytes().contains(&0)
            || !names.insert(name)
        {
            return Err(OperationalError::new(
                OperationalErrorKind::InvalidInput,
                "child environment contains an invalid or duplicate native entry",
            ));
        }
    }
    Ok(())
}
