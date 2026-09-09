//! macOS/Linux implementation of the bounded operational adapter contract.
//! Exact retained-identity process execution is Linux-only; macOS refuses it
//! as unsupported before pathname execution.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opaal_platform::operational::{
    AtomicWriteRequest, HttpHeader, HttpRequest, HttpResponse, MAX_HTTP_HEADER_BYTES,
    MAX_HTTP_HEADERS, OperationalAdapter, OperationalError, OperationalErrorKind, ProcessExit,
    ProcessOutput, ProcessRequest, ReadFileRequest, validate_http_request,
    validate_process_request,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// One concrete operational adapter with a process-local monotonic origin.
#[derive(Debug)]
pub struct PosixOperationalAdapter {
    origin: Instant,
}

impl PosixOperationalAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for PosixOperationalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationalAdapter for PosixOperationalAdapter {
    fn read_file(&self, request: ReadFileRequest<'_>) -> Result<Vec<u8>, OperationalError> {
        let (parent, name) = open_parent(request.root, request.path)?;
        let mut file = openat_file(
            &parent,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        require_regular(&file)?;
        let read_limit = request
            .max_bytes
            .checked_add(1)
            .and_then(|limit| u64::try_from(limit).ok())
            .ok_or_else(|| {
                error(
                    OperationalErrorKind::LimitExceeded,
                    "file byte limit is outside the supported range",
                )
            })?;
        let mut bytes = Vec::with_capacity(request.max_bytes.min(64 * 1024));
        Read::by_ref(&mut file)
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > request.max_bytes {
            return Err(error(
                OperationalErrorKind::LimitExceeded,
                "file exceeds its byte limit",
            ));
        }
        Ok(bytes)
    }

    fn write_atomic(&self, request: AtomicWriteRequest<'_>) -> Result<(), OperationalError> {
        if request.bytes.len() > request.max_bytes {
            return Err(error(
                OperationalErrorKind::LimitExceeded,
                "write exceeds its byte limit",
            ));
        }
        let (parent, name) = open_parent(request.root, request.path)?;
        reject_unsafe_existing(&parent, name)?;
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
        let temp = OsString::from(format!(
            ".opaal-write-{}-{:016x}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = openat_file(
            &parent,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        let outcome = (|| {
            file.write_all(request.bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            renameat(&parent, &temp, name)?;
            fsync_directory(&parent)?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = unlinkat(&parent, &temp);
        }
        outcome
    }

    fn wall_time_unix_nanos(&self) -> Result<i128, OperationalError> {
        let duration = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| {
            error(
                OperationalErrorKind::Unsupported,
                "wall clock precedes the Unix epoch",
            )
        })?;
        i128::try_from(duration.as_nanos()).map_err(|_| {
            error(
                OperationalErrorKind::LimitExceeded,
                "wall timestamp exceeds the supported range",
            )
        })
    }

    fn monotonic_nanos(&self) -> Result<u128, OperationalError> {
        Ok(self.origin.elapsed().as_nanos())
    }

    fn http_request(
        &self,
        request: HttpRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<HttpResponse, OperationalError> {
        validate_http_request(&request)?;
        if cancelled() {
            return Err(error(
                OperationalErrorKind::Cancelled,
                "HTTP request was cancelled",
            ));
        }
        let started = Instant::now();
        let deadline = started.checked_add(request.timeout).ok_or_else(|| {
            error(
                OperationalErrorKind::InvalidInput,
                "HTTP deadline overflows",
            )
        })?;
        let address = request.connect_host.parse::<IpAddr>().map_err(|_| {
            error(
                OperationalErrorKind::Unsupported,
                "bounded POSIX HTTP requires a literal IP connect host",
            )
        })?;
        let candidate = SocketAddr::new(address, request.port);
        let socket = loop {
            if cancelled() {
                return Err(error(
                    OperationalErrorKind::Cancelled,
                    "HTTP request was cancelled",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(error(
                    OperationalErrorKind::TimedOut,
                    "HTTP request timed out",
                ));
            }
            match TcpStream::connect_timeout(&candidate, remaining.min(Duration::from_millis(250)))
            {
                Ok(stream) => break stream,
                Err(cause)
                    if matches!(
                        cause.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(cause) => {
                    return Err(error(
                        OperationalErrorKind::Network,
                        format!("endpoint connection failed: {cause}"),
                    ));
                }
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(io_error)?;
        socket
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(io_error)?;
        if let (Some(server_name), Some(ca)) = (request.tls_server_name, request.ca_pem) {
            let mut roots = RootCertStore::empty();
            let mut count = 0usize;
            for certificate in CertificateDer::pem_slice_iter(ca) {
                roots
                    .add(certificate.map_err(|cause| {
                        error(
                            OperationalErrorKind::Tls,
                            format!("invalid CA PEM: {cause}"),
                        )
                    })?)
                    .map_err(|cause| {
                        error(
                            OperationalErrorKind::Tls,
                            format!("invalid CA certificate: {cause}"),
                        )
                    })?;
                count += 1;
            }
            if count == 0 {
                return Err(error(
                    OperationalErrorKind::Tls,
                    "CA file contains no certificate",
                ));
            }
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = ServerName::try_from(server_name.to_owned())
                .map_err(|_| error(OperationalErrorKind::Tls, "invalid TLS server name"))?;
            let connection = ClientConnection::new(Arc::new(config), name).map_err(|cause| {
                error(
                    OperationalErrorKind::Tls,
                    format!("TLS setup failed: {cause}"),
                )
            })?;
            perform_http(
                StreamOwned::new(connection, socket),
                &request,
                deadline,
                cancelled,
            )
            .map_err(classify_tls_error)
        } else {
            perform_http(socket, &request, deadline, cancelled)
        }
    }

    fn run_process(
        &self,
        request: ProcessRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ProcessOutput, OperationalError> {
        validate_process_request(&request)?;
        if cfg!(target_os = "macos") {
            return Err(error(
                OperationalErrorKind::Unsupported,
                "maintained process execution is unsupported on macOS because exact executable identity cannot be preserved",
            ));
        }
        if cancelled() {
            return Err(error(
                OperationalErrorKind::Cancelled,
                "process was cancelled",
            ));
        }
        let started = Instant::now();
        let deadline = started.checked_add(request.timeout).ok_or_else(|| {
            error(
                OperationalErrorKind::InvalidInput,
                "process deadline overflows",
            )
        })?;
        #[cfg(target_os = "linux")]
        let executable_descriptor = request
            .executable_file
            .map(rustix::io::dup)
            .transpose()
            .map_err(|cause| io_error(cause.into()))?;
        #[cfg(target_os = "linux")]
        let retained_path = executable_descriptor.as_ref().map(|descriptor| {
            std::path::PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
        });
        #[cfg(target_os = "macos")]
        let retained_path: Option<std::path::PathBuf> = None;
        let executable = retained_path.as_deref().unwrap_or(request.executable);
        let mut command = Command::new(executable);
        command.arg0(&request.argv[0]);
        command.args(&request.argv[1..]);
        command.env_clear();
        command.envs(
            request
                .environment
                .iter()
                .map(|(name, value)| (name, value)),
        );
        command.current_dir(request.cwd);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.process_group(0);
        let mut child = command.spawn().map_err(io_error)?;
        let mut stdout = child.stdout.take().expect("piped stdout is present");
        let mut stderr = child.stderr.take().expect("piped stderr is present");
        if let Err(primary) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
            drop(stdout);
            drop(stderr);
            let mut empty_stdout = io::empty();
            let mut empty_stderr = io::empty();
            let cleanup = terminate_and_reap(
                &mut child,
                &mut empty_stdout,
                &mut empty_stderr,
                0,
                0,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut false,
            );
            return Err(primary_after_cleanup(primary, cleanup));
        }
        let mut retained_stdout = Vec::with_capacity(request.stdout_limit.min(64 * 1024));
        let mut retained_stderr = Vec::with_capacity(request.stderr_limit.min(64 * 1024));
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let mut exceeded = false;
        let status = loop {
            if !stdout_eof {
                stdout_eof = match drain_nonblocking(
                    &mut stdout,
                    request.stdout_limit,
                    &mut retained_stdout,
                    &mut exceeded,
                ) {
                    Ok(eof) => eof,
                    Err(primary) => {
                        let cleanup = terminate_and_reap(
                            &mut child,
                            &mut stdout,
                            &mut stderr,
                            request.stdout_limit,
                            request.stderr_limit,
                            &mut retained_stdout,
                            &mut retained_stderr,
                            &mut exceeded,
                        );
                        return Err(primary_after_cleanup(primary, cleanup));
                    }
                };
            }
            if !stderr_eof {
                stderr_eof = match drain_nonblocking(
                    &mut stderr,
                    request.stderr_limit,
                    &mut retained_stderr,
                    &mut exceeded,
                ) {
                    Ok(eof) => eof,
                    Err(primary) => {
                        let cleanup = terminate_and_reap(
                            &mut child,
                            &mut stdout,
                            &mut stderr,
                            request.stdout_limit,
                            request.stderr_limit,
                            &mut retained_stdout,
                            &mut retained_stderr,
                            &mut exceeded,
                        );
                        return Err(primary_after_cleanup(primary, cleanup));
                    }
                };
            }
            if cancelled() {
                let primary = error(OperationalErrorKind::Cancelled, "process was cancelled");
                let cleanup = terminate_and_reap(
                    &mut child,
                    &mut stdout,
                    &mut stderr,
                    request.stdout_limit,
                    request.stderr_limit,
                    &mut retained_stdout,
                    &mut retained_stderr,
                    &mut exceeded,
                );
                return Err(primary_after_cleanup(primary, cleanup));
            }
            if Instant::now() >= deadline {
                let primary = error(OperationalErrorKind::TimedOut, "process timed out");
                let cleanup = terminate_and_reap(
                    &mut child,
                    &mut stdout,
                    &mut stderr,
                    request.stdout_limit,
                    request.stderr_limit,
                    &mut retained_stdout,
                    &mut retained_stderr,
                    &mut exceeded,
                );
                return Err(primary_after_cleanup(primary, cleanup));
            }
            if exceeded {
                let primary = error(
                    OperationalErrorKind::LimitExceeded,
                    "process output exceeds its byte limit",
                );
                let cleanup = terminate_and_reap(
                    &mut child,
                    &mut stdout,
                    &mut stderr,
                    request.stdout_limit,
                    request.stderr_limit,
                    &mut retained_stdout,
                    &mut retained_stderr,
                    &mut exceeded,
                );
                return Err(primary_after_cleanup(primary, cleanup));
            }
            let status = match child.try_wait() {
                Ok(status) => status,
                Err(cause) => {
                    let primary = io_error(cause);
                    let cleanup = terminate_and_reap(
                        &mut child,
                        &mut stdout,
                        &mut stderr,
                        request.stdout_limit,
                        request.stderr_limit,
                        &mut retained_stdout,
                        &mut retained_stderr,
                        &mut exceeded,
                    );
                    return Err(primary_after_cleanup(primary, cleanup));
                }
            };
            if let Some(status) = status {
                let group_exists = match process_group_exists(child.id()) {
                    Ok(exists) => exists,
                    Err(primary) => {
                        let cleanup = terminate_and_reap_after_status(
                            child.id(),
                            &mut stdout,
                            &mut stderr,
                            request.stdout_limit,
                            request.stderr_limit,
                            &mut retained_stdout,
                            &mut retained_stderr,
                            &mut exceeded,
                        );
                        return Err(primary_after_cleanup(primary, cleanup));
                    }
                };
                if stdout_eof && stderr_eof && !group_exists {
                    break status;
                }
                if !group_exists {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let primary = error(
                    OperationalErrorKind::Protocol,
                    "process left an owned descendant after its leader exited",
                );
                let cleanup = terminate_and_reap_after_status(
                    child.id(),
                    &mut stdout,
                    &mut stderr,
                    request.stdout_limit,
                    request.stderr_limit,
                    &mut retained_stdout,
                    &mut retained_stderr,
                    &mut exceeded,
                );
                return Err(primary_after_cleanup(primary, cleanup));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let status = process_exit(status)?;
        Ok(ProcessOutput::new(
            status,
            retained_stdout,
            retained_stderr,
            started.elapsed(),
        ))
    }
}

fn drain_nonblocking(
    reader: &mut impl Read,
    limit: usize,
    retained: &mut Vec<u8>,
    exceeded: &mut bool,
) -> Result<bool, OperationalError> {
    let mut buffer = [0u8; 64 * 1024];
    match reader.read(&mut buffer) {
        Ok(0) => Ok(true),
        Ok(read) => {
            let remaining = limit.saturating_sub(retained.len());
            let admitted = remaining.min(read);
            retained.extend_from_slice(&buffer[..admitted]);
            if admitted != read {
                *exceeded = true;
            }
            Ok(false)
        }
        Err(cause)
            if matches!(
                cause.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(cause) => Err(io_error(cause)),
    }
}

#[allow(clippy::too_many_arguments)]
fn terminate_and_reap(
    child: &mut std::process::Child,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    stdout_limit: usize,
    stderr_limit: usize,
    retained_stdout: &mut Vec<u8>,
    retained_stderr: &mut Vec<u8>,
    exceeded: &mut bool,
) -> Result<(), OperationalError> {
    let pid = child.id();
    let mut cleanup_error = None;
    let mut reaped = match child.try_wait() {
        Ok(status) => status.is_some(),
        Err(cause) => {
            record_cleanup_error(&mut cleanup_error, io_error(cause));
            false
        }
    };
    if wait_for_group(
        pid,
        &mut reaped,
        Some(child),
        stdout,
        stderr,
        stdout_limit,
        stderr_limit,
        retained_stdout,
        retained_stderr,
        exceeded,
        Duration::ZERO,
        &mut cleanup_error,
    ) {
        return finish_cleanup(cleanup_error);
    }
    if let Err(error) = signal_group(pid, libc::SIGTERM) {
        record_cleanup_error(&mut cleanup_error, error);
    }
    if wait_for_group(
        pid,
        &mut reaped,
        Some(child),
        stdout,
        stderr,
        stdout_limit,
        stderr_limit,
        retained_stdout,
        retained_stderr,
        exceeded,
        Duration::from_secs(5),
        &mut cleanup_error,
    ) {
        return finish_cleanup(cleanup_error);
    }
    if let Err(error) = signal_group(pid, libc::SIGKILL) {
        record_cleanup_error(&mut cleanup_error, error);
    }
    if wait_for_group(
        pid,
        &mut reaped,
        Some(child),
        stdout,
        stderr,
        stdout_limit,
        stderr_limit,
        retained_stdout,
        retained_stderr,
        exceeded,
        Duration::from_secs(5),
        &mut cleanup_error,
    ) {
        finish_cleanup(cleanup_error)
    } else {
        record_cleanup_error(
            &mut cleanup_error,
            error(
                OperationalErrorKind::TimedOut,
                "process group was not killed, drained, and reaped within ten seconds",
            ),
        );
        finish_cleanup(cleanup_error)
    }
}

#[allow(clippy::too_many_arguments)]
fn terminate_and_reap_after_status(
    pid: u32,
    stdout: &mut impl Read,
    stderr: &mut impl Read,
    stdout_limit: usize,
    stderr_limit: usize,
    retained_stdout: &mut Vec<u8>,
    retained_stderr: &mut Vec<u8>,
    exceeded: &mut bool,
) -> Result<(), OperationalError> {
    let mut cleanup_error = None;
    if let Err(error) = signal_group(pid, libc::SIGTERM) {
        record_cleanup_error(&mut cleanup_error, error);
    }
    let mut reaped = true;
    if wait_for_group(
        pid,
        &mut reaped,
        None,
        stdout,
        stderr,
        stdout_limit,
        stderr_limit,
        retained_stdout,
        retained_stderr,
        exceeded,
        Duration::from_secs(5),
        &mut cleanup_error,
    ) {
        return finish_cleanup(cleanup_error);
    }
    if let Err(error) = signal_group(pid, libc::SIGKILL) {
        record_cleanup_error(&mut cleanup_error, error);
    }
    if wait_for_group(
        pid,
        &mut reaped,
        None,
        stdout,
        stderr,
        stdout_limit,
        stderr_limit,
        retained_stdout,
        retained_stderr,
        exceeded,
        Duration::from_secs(5),
        &mut cleanup_error,
    ) {
        finish_cleanup(cleanup_error)
    } else {
        record_cleanup_error(
            &mut cleanup_error,
            error(
                OperationalErrorKind::TimedOut,
                "owned descendants were not killed and drained within ten seconds",
            ),
        );
        finish_cleanup(cleanup_error)
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_for_group<O: Read, E: Read>(
    pid: u32,
    reaped: &mut bool,
    mut child: Option<&mut std::process::Child>,
    stdout: &mut O,
    stderr: &mut E,
    stdout_limit: usize,
    stderr_limit: usize,
    retained_stdout: &mut Vec<u8>,
    retained_stderr: &mut Vec<u8>,
    exceeded: &mut bool,
    duration: Duration,
    cleanup_error: &mut Option<OperationalError>,
) -> bool {
    let deadline = Instant::now() + duration;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    loop {
        if !stdout_eof {
            stdout_eof = match drain_nonblocking(stdout, stdout_limit, retained_stdout, exceeded) {
                Ok(eof) => eof,
                Err(error) => {
                    record_cleanup_error(cleanup_error, error);
                    true
                }
            };
        }
        if !stderr_eof {
            stderr_eof = match drain_nonblocking(stderr, stderr_limit, retained_stderr, exceeded) {
                Ok(eof) => eof,
                Err(error) => {
                    record_cleanup_error(cleanup_error, error);
                    true
                }
            };
        }
        if !*reaped && let Some(child) = child.as_deref_mut() {
            match child.try_wait() {
                Ok(status) => *reaped = status.is_some(),
                Err(cause) => record_cleanup_error(cleanup_error, io_error(cause)),
            }
        }
        let group_exists = match process_group_exists(pid) {
            Ok(exists) => exists,
            Err(error) => {
                record_cleanup_error(cleanup_error, error);
                true
            }
        };
        if *reaped && stdout_eof && stderr_eof && !group_exists {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn record_cleanup_error(slot: &mut Option<OperationalError>, error: OperationalError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

fn finish_cleanup(error: Option<OperationalError>) -> Result<(), OperationalError> {
    error.map_or(Ok(()), Err)
}

#[allow(unsafe_code)]
fn set_nonblocking(file: &impl AsRawFd) -> Result<(), OperationalError> {
    // SAFETY: fcntl reads and updates flags on one live owned descriptor.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io_error(io::Error::last_os_error()));
    }
    // SAFETY: the descriptor remains live and the existing flags are preserved.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io_error(io::Error::last_os_error()));
    }
    Ok(())
}

#[allow(unsafe_code)]
fn process_group_exists(pid: u32) -> Result<bool, OperationalError> {
    let pid = i32::try_from(pid).map_err(|_| {
        error(
            OperationalErrorKind::InvalidInput,
            "child pid exceeds POSIX range",
        )
    })?;
    // SAFETY: signal zero performs a read-only existence/permission check.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return Ok(true);
    }
    let cause = io::Error::last_os_error();
    match cause.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(io_error(cause)),
    }
}

#[allow(unsafe_code)]
fn signal_group(pid: u32, signal: libc::c_int) -> Result<(), OperationalError> {
    let pid = i32::try_from(pid).map_err(|_| {
        error(
            OperationalErrorKind::InvalidInput,
            "child pid exceeds POSIX range",
        )
    })?;
    // SAFETY: a negated positive child pid addresses the process group created
    // for this exact child; no pointer or borrowed memory crosses the call.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let cause = io::Error::last_os_error();
    if cause.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(io_error(cause))
    }
}

fn process_exit(status: ExitStatus) -> Result<ProcessExit, OperationalError> {
    if let Some(code) = status.code() {
        Ok(ProcessExit::Exited(code))
    } else if let Some(signal) = status.signal() {
        Ok(ProcessExit::Signaled(signal))
    } else {
        Err(error(
            OperationalErrorKind::Protocol,
            "process completed without exit code or signal",
        ))
    }
}

fn primary_after_cleanup(
    primary: OperationalError,
    cleanup: Result<(), OperationalError>,
) -> OperationalError {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => error(
            primary.kind(),
            format!(
                "{}; bounded cleanup also failed: {}",
                primary.message(),
                cleanup.message()
            ),
        ),
    }
}

fn perform_http<S: Read + Write>(
    mut stream: S,
    request: &HttpRequest<'_>,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<HttpResponse, OperationalError> {
    let host = if request.connect_host.contains(':') {
        format!("[{}]", request.connect_host)
    } else {
        request.connect_host.to_owned()
    };
    let mut encoded = Vec::new();
    let header_bytes = request.headers.iter().try_fold(0usize, |total, header| {
        total.checked_add(header.name().len() + header.value().len() + 4)
    });
    let header_bytes = request
        .secret_header
        .as_ref()
        .map_or(header_bytes, |header| {
            header_bytes
                .and_then(|total| total.checked_add(header.name.len() + header.value.len() + 4))
        });
    let capacity = header_bytes
        .and_then(|bytes| bytes.checked_add(request.body.len()))
        .and_then(|bytes| bytes.checked_add(request.method.len()))
        .and_then(|bytes| bytes.checked_add(request.path_and_query.len()))
        .and_then(|bytes| bytes.checked_add(host.len()))
        .and_then(|bytes| bytes.checked_add(96))
        .ok_or_else(|| {
            error(
                OperationalErrorKind::LimitExceeded,
                "HTTP request size overflow",
            )
        })?;
    encoded.try_reserve_exact(capacity).map_err(|_| {
        error(
            OperationalErrorKind::LimitExceeded,
            "HTTP request allocation failed within its byte limit",
        )
    })?;
    write!(
        &mut encoded,
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nContent-Length: {}\r\n",
        request.method,
        request.path_and_query,
        host,
        request.port,
        request.body.len()
    )
    .map_err(io_error)?;
    for header in request.headers {
        write_header(&mut encoded, header.name(), header.value())?;
    }
    if let Some(header) = &request.secret_header {
        write_header(&mut encoded, header.name, header.value)?;
    }
    encoded.extend_from_slice(b"\r\n");
    encoded.extend_from_slice(request.body);
    let write_result = write_all_polling(&mut stream, &encoded, deadline, cancelled);
    encoded.fill(0);
    let _ = std::hint::black_box(&mut encoded);
    write_result?;

    let mut received = Vec::new();
    let header_end = loop {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HTTP_HEADERS];
        let mut parsed = httparse::Response::new(&mut headers);
        match parsed.parse(&received).map_err(http_parse_error)? {
            httparse::Status::Complete(end) => {
                if end > MAX_HTTP_HEADER_BYTES {
                    return Err(error(
                        OperationalErrorKind::LimitExceeded,
                        "HTTP response headers exceed 64 KiB",
                    ));
                }
                break end;
            }
            httparse::Status::Partial => {
                if received.len() > MAX_HTTP_HEADER_BYTES {
                    return Err(error(
                        OperationalErrorKind::LimitExceeded,
                        "HTTP response headers exceed 64 KiB",
                    ));
                }
                if read_polling(
                    &mut stream,
                    &mut received,
                    deadline,
                    cancelled,
                    MAX_HTTP_HEADER_BYTES + 1,
                )? {
                    return Err(error(
                        OperationalErrorKind::Protocol,
                        "HTTP response ended before its headers completed",
                    ));
                }
            }
        }
    };
    let mut raw_headers = [httparse::EMPTY_HEADER; MAX_HTTP_HEADERS];
    let mut parsed = httparse::Response::new(&mut raw_headers);
    parsed
        .parse(&received[..header_end])
        .map_err(http_parse_error)?;
    let status = parsed.code.ok_or_else(|| {
        error(
            OperationalErrorKind::Protocol,
            "HTTP response has no status",
        )
    })?;
    if !(100..=599).contains(&status) {
        return Err(error(
            OperationalErrorKind::Protocol,
            "HTTP response status is outside 100 through 599",
        ));
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    for header in parsed.headers.iter() {
        let name = header.name.to_ascii_lowercase();
        if name == "transfer-encoding" && !header.value.eq_ignore_ascii_case(b"identity") {
            return Err(error(
                OperationalErrorKind::Protocol,
                "chunked or transformed HTTP responses are unsupported",
            ));
        }
        if name == "content-length" {
            let text = std::str::from_utf8(header.value)
                .map_err(|_| error(OperationalErrorKind::Protocol, "invalid content-length"))?;
            let length = text
                .parse::<usize>()
                .map_err(|_| error(OperationalErrorKind::Protocol, "invalid content-length"))?;
            if content_length.replace(length).is_some() {
                return Err(error(
                    OperationalErrorKind::Protocol,
                    "duplicate content-length",
                ));
            }
        }
        headers.push(HttpHeader::new(name, header.value.to_vec())?);
    }
    let mut body = received.split_off(header_end);
    if body.len() > request.max_response_bytes {
        return Err(error(
            OperationalErrorKind::LimitExceeded,
            "HTTP response body exceeds its byte limit",
        ));
    }
    if let Some(length) = content_length {
        if length > request.max_response_bytes {
            return Err(error(
                OperationalErrorKind::LimitExceeded,
                "HTTP response body exceeds its byte limit",
            ));
        }
        while body.len() < length {
            if read_polling(&mut stream, &mut body, deadline, cancelled, length)? {
                return Err(error(
                    OperationalErrorKind::Protocol,
                    "HTTP response ended before content-length",
                ));
            }
        }
        if body.len() != length {
            return Err(error(
                OperationalErrorKind::Protocol,
                "HTTP response length does not match content-length",
            ));
        }
    } else {
        loop {
            let eof = read_polling(
                &mut stream,
                &mut body,
                deadline,
                cancelled,
                request.max_response_bytes + 1,
            )?;
            if eof {
                break;
            }
            if body.len() > request.max_response_bytes {
                return Err(error(
                    OperationalErrorKind::LimitExceeded,
                    "HTTP response body exceeds its byte limit",
                ));
            }
        }
    }
    Ok(HttpResponse::new(status, headers, body))
}

fn write_header(output: &mut Vec<u8>, name: &str, value: &[u8]) -> Result<(), OperationalError> {
    output.extend_from_slice(name.as_bytes());
    output.extend_from_slice(b": ");
    output.extend_from_slice(value);
    output.extend_from_slice(b"\r\n");
    Ok(())
}

fn write_all_polling(
    stream: &mut impl Write,
    mut bytes: &[u8],
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), OperationalError> {
    while !bytes.is_empty() {
        check_live(deadline, cancelled)?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(error(
                    OperationalErrorKind::Network,
                    "HTTP connection closed while writing",
                ));
            }
            Ok(count) => bytes = &bytes[count..],
            Err(cause)
                if matches!(
                    cause.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(cause) => return Err(io_error(cause)),
        }
    }
    stream.flush().map_err(io_error)
}

fn read_polling(
    stream: &mut impl Read,
    output: &mut Vec<u8>,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
    limit: usize,
) -> Result<bool, OperationalError> {
    check_live(deadline, cancelled)?;
    let mut buffer = [0u8; 64 * 1024];
    let room = limit.saturating_sub(output.len()).min(buffer.len());
    if room == 0 {
        return Err(error(
            OperationalErrorKind::LimitExceeded,
            "HTTP response exceeds its byte limit",
        ));
    }
    match stream.read(&mut buffer[..room]) {
        Ok(0) => Ok(true),
        Ok(count) => {
            output.extend_from_slice(&buffer[..count]);
            Ok(false)
        }
        Err(cause)
            if matches!(
                cause.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(cause) => Err(io_error(cause)),
    }
}

fn check_live(deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<(), OperationalError> {
    if cancelled() {
        Err(error(
            OperationalErrorKind::Cancelled,
            "HTTP request was cancelled",
        ))
    } else if Instant::now() >= deadline {
        Err(error(
            OperationalErrorKind::TimedOut,
            "HTTP request timed out",
        ))
    } else {
        Ok(())
    }
}

fn open_parent<'a>(root: &Path, target: &'a Path) -> Result<(File, &'a OsStr), OperationalError> {
    if !root.is_absolute() || !target.is_absolute() || !target.starts_with(root) {
        return Err(error(
            OperationalErrorKind::OutsideRoot,
            "path is outside its lexical root",
        ));
    }
    let relative = target.strip_prefix(root).map_err(|_| {
        error(
            OperationalErrorKind::OutsideRoot,
            "path is outside its lexical root",
        )
    })?;
    let mut components = relative.components().peekable();
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(classify_path_error)?;
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = openat_file(
                    &directory,
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )?;
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(error(
                    OperationalErrorKind::InvalidInput,
                    "root path is not normalized",
                ));
            }
        }
    }
    let mut final_name = None;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(error(
                OperationalErrorKind::InvalidInput,
                "path is not normalized",
            ));
        };
        if components.peek().is_none() {
            final_name = Some(name);
            break;
        }
        directory = openat_file(
            &directory,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
    }
    Ok((
        directory,
        final_name.ok_or_else(|| {
            error(
                OperationalErrorKind::InvalidInput,
                "file path must name a child of the root",
            )
        })?,
    ))
}

#[allow(unsafe_code)]
fn openat_file(
    directory: &File,
    name: &OsStr,
    flags: libc::c_int,
    mode: u32,
) -> Result<File, OperationalError> {
    let name = c_name(name)?;
    // SAFETY: the directory fd and C string live across the call; mode is
    // supplied for the possible O_CREAT case and a successful fd is owned.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(classify_at_error(
            directory,
            &name,
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a successful openat transfers one unique descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[allow(unsafe_code)]
fn classify_at_error(directory: &File, name: &CString, cause: io::Error) -> OperationalError {
    if matches!(
        cause.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTDIR)
    ) {
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: status is writable and the retained descriptor/name are live.
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                status.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            // SAFETY: fstatat initialized status on success.
            let status = unsafe { status.assume_init() };
            return if status.st_mode & libc::S_IFMT == libc::S_IFLNK {
                error(OperationalErrorKind::Symlink, "path component is a symlink")
            } else {
                error(
                    OperationalErrorKind::NotRegular,
                    "path component is not a directory",
                )
            };
        }
    }
    classify_path_error(cause)
}

fn require_regular(file: &File) -> Result<(), OperationalError> {
    if file.metadata().map_err(io_error)?.is_file() {
        Ok(())
    } else {
        Err(error(
            OperationalErrorKind::NotRegular,
            "path is not a regular file",
        ))
    }
}

#[allow(unsafe_code)]
fn reject_unsafe_existing(parent: &File, name: &OsStr) -> Result<(), OperationalError> {
    let name = c_name(name)?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: status points to writable storage and both fd/name are live.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: fstatat initialized status on success.
        let status = unsafe { status.assume_init() };
        if status.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(error(OperationalErrorKind::Symlink, "target is a symlink"));
        }
        if status.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(error(
                OperationalErrorKind::NotRegular,
                "target is not a regular file",
            ));
        }
        Ok(())
    } else {
        let cause = io::Error::last_os_error();
        if cause.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(classify_path_error(cause))
        }
    }
}

#[allow(unsafe_code)]
fn renameat(parent: &File, source: &OsStr, target: &OsStr) -> Result<(), OperationalError> {
    let source = c_name(source)?;
    let target = c_name(target)?;
    // SAFETY: both names and the retained parent descriptor are valid.
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(io::Error::last_os_error()))
    }
}

#[allow(unsafe_code)]
fn unlinkat(parent: &File, name: &OsStr) -> Result<(), OperationalError> {
    let name = c_name(name)?;
    // SAFETY: name and parent descriptor are valid and no directory flag is used.
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(io::Error::last_os_error()))
    }
}

#[allow(unsafe_code)]
fn fsync_directory(directory: &File) -> Result<(), OperationalError> {
    // SAFETY: fsync borrows one live descriptor without taking ownership.
    let result = unsafe { libc::fsync(directory.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(io::Error::last_os_error()))
    }
}

fn c_name(name: &OsStr) -> Result<CString, OperationalError> {
    CString::new(name.as_bytes()).map_err(|_| {
        error(
            OperationalErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })
}

fn classify_path_error(cause: io::Error) -> OperationalError {
    let kind = if cause.raw_os_error() == Some(libc::ELOOP) {
        OperationalErrorKind::Symlink
    } else if cause.kind() == io::ErrorKind::NotFound {
        OperationalErrorKind::NotFound
    } else if cause.kind() == io::ErrorKind::AlreadyExists {
        OperationalErrorKind::AlreadyExists
    } else {
        OperationalErrorKind::Io(cause.kind())
    };
    error(kind, cause.to_string())
}

fn io_error(cause: io::Error) -> OperationalError {
    error(OperationalErrorKind::Io(cause.kind()), cause.to_string())
}
fn http_parse_error(cause: httparse::Error) -> OperationalError {
    if cause == httparse::Error::TooManyHeaders {
        error(
            OperationalErrorKind::LimitExceeded,
            "HTTP response header count exceeds 128",
        )
    } else {
        error(
            OperationalErrorKind::Protocol,
            format!("invalid HTTP response: {cause}"),
        )
    }
}
fn classify_tls_error(cause: OperationalError) -> OperationalError {
    if matches!(
        cause.kind(),
        OperationalErrorKind::Io(io::ErrorKind::InvalidData)
    ) {
        error(OperationalErrorKind::Tls, cause.message())
    } else {
        cause
    }
}
fn error(kind: OperationalErrorKind, message: impl Into<String>) -> OperationalError {
    OperationalError::new(kind, message)
}
