//! Same-image standard host worker. Only explicitly lent stream endpoints enter
//! it after READY; the supervisor owns the child and private control socket.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::{
    FillError, MAX_STREAM_CHUNK_BYTES, StandardHost, StandardStream, TransferError, validate_fill,
};

mod image;
#[allow(unsafe_code)]
mod native;

pub const WORKER_ARGUMENT: &str = "--opaal-standard-host-worker";
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TERM_GRACE: Duration = Duration::from_millis(100);
const HEADER_BYTES: usize = 36;
const BIND: u16 = 1;
const READY: u16 = 2;
const FILL: u16 = 3;
const DATA: u16 = 4;
const ACK: u16 = 5;
const FAILED: u16 = 6;
const READ: u16 = 7;
const WRITE_OUT: u16 = 8;
const WRITE_ERR: u16 = 9;
const PROGRESS: u16 = 10;
const INTERRUPTED: u16 = 11;
const WOULD_BLOCK: u16 = 12;
const BROKEN_PIPE: u16 = 13;
const IO_FAILED: u16 = 14;

/// Explicit native CLI binding; an arbitrary embedding must supply its own
/// qualified worker entry point and child ownership instead of using this.
pub struct PosixStandardHost {
    evaluation: u64,
    image: image::Image,
    worker: Option<Worker>,
    request: u64,
    closed: bool,
    endpoints: [Option<native::Endpoint>; 3],
    terminal_lease: Option<native::TerminalLease>,
    signal_lease: Option<native::SignalLease>,
}

impl PosixStandardHost {
    pub fn for_cli(evaluation: u64) -> Result<Self, OperationalError> {
        if evaluation == 0 {
            return Err(protocol());
        }
        native::check_parent()?;
        Ok(Self {
            evaluation,
            image: image::Image::capture()?,
            worker: None,
            request: 0,
            closed: false,
            endpoints: [None, None, None],
            terminal_lease: None,
            signal_lease: None,
        })
    }

    /// Capture an explicitly lent pipe/file before evaluation starts. Terminals
    /// require the frontend's editor restoration lease and are not admitted here.
    pub fn bind_stream(
        &mut self,
        stream: StandardStream,
        descriptor: BorrowedFd<'_>,
    ) -> Result<(), OperationalError> {
        let slot = &mut self.endpoints[stream as usize];
        if self.closed || self.worker.is_some() || slot.is_some() {
            return Err(unsupported());
        }
        *slot = Some(native::Endpoint::capture(descriptor, stream, false)?);
        Ok(())
    }

    /// Lend a foreground terminal after the frontend has stopped reading and
    /// restored cooked mode. The parent retains restoration through worker reap.
    pub fn bind_terminal_stream(
        &mut self,
        stream: StandardStream,
        descriptor: BorrowedFd<'_>,
    ) -> Result<(), OperationalError> {
        if self.closed
            || self.worker.is_some()
            || self.endpoints[stream as usize].is_some()
            || !crate::terminal_mode::is_terminal(descriptor)
        {
            return Err(unsupported());
        }
        let endpoint = native::Endpoint::capture(descriptor, stream, true)?;
        if self.terminal_lease.is_none() {
            self.terminal_lease = Some(native::TerminalLease::acquire()?);
        }
        self.terminal_lease
            .as_mut()
            .expect("owned terminal lease")
            .capture(descriptor)?;
        self.endpoints[stream as usize] = Some(endpoint);
        Ok(())
    }

    /// Capture invocation termination signals before CLI evaluation starts.
    /// The host restores dispositions only after worker reap and terminal cleanup.
    pub fn capture_cancellation_signals(
        &mut self,
    ) -> Result<Box<dyn Fn() -> bool + Send + Sync>, OperationalError> {
        if self.closed || self.worker.is_some() || self.signal_lease.is_some() {
            return Err(unsupported());
        }
        let lease = native::SignalLease::capture()?;
        let cancelled = lease.cancellation();
        self.signal_lease = Some(lease);
        Ok(cancelled)
    }

    fn transfer(
        &mut self,
        stream: StandardStream,
        bytes: &mut [u8],
        output: Option<&[u8]>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        let length = output.map_or(bytes.len(), <[u8]>::len);
        if length == 0 || length > MAX_STREAM_CHUNK_BYTES || !self.stream_available(stream) {
            return Err(TransferError::not_started(unsupported()));
        }
        let mut progress = 0;
        let mut admitted = false;
        let mut eof = None;
        let mut acknowledged = false;
        let result = (|| {
            self.launch(cancelled)?;
            let endpoint = self.endpoints[stream as usize]
                .as_ref()
                .ok_or_else(unsupported)?;
            endpoint.revalidate()?;
            self.request = self.request.checked_add(1).ok_or_else(protocol)?;
            let tag = match stream {
                StandardStream::Stdin => READ,
                StandardStream::Stdout => WRITE_OUT,
                StandardStream::Stderr => WRITE_ERR,
            };
            let worker = self.worker.as_mut().expect("owned READY worker");
            // Until an accepted response, the one lent syscall may consume its
            // entire admission, including cancellation during control delivery.
            send_endpoint(
                worker.socket(),
                Frame::new(tag, self.evaluation, self.request, length as u32),
                endpoint,
                cancelled,
            )?;
            admitted = true;
            if let Some(output) = output {
                send_payload(worker.socket(), output, cancelled)?;
            }
            let frame = worker.receive(cancelled)?;
            if frame.evaluation != self.evaluation || frame.request != self.request {
                return Err(protocol());
            }
            let transfer_error = match frame.tag {
                PROGRESS if frame.bytes as usize <= length => None,
                INTERRUPTED if frame.bytes == 0 => Some(io::ErrorKind::Interrupted),
                WOULD_BLOCK if frame.bytes == 0 => Some(io::ErrorKind::WouldBlock),
                BROKEN_PIPE if frame.bytes == 0 => Some(io::ErrorKind::BrokenPipe),
                IO_FAILED if frame.bytes == 0 => Some(io::ErrorKind::Other),
                _ => return Err(protocol()),
            };
            progress = frame.bytes as usize;
            admitted = false;
            if output.is_none() && frame.tag == PROGRESS && frame.bytes == 0 {
                eof = Some(true);
            }
            if output.is_none() && frame.bytes > 0 {
                worker.receive_payload(&mut bytes[..frame.bytes as usize], cancelled)?;
            }
            worker.send(Frame::new(ACK, self.evaluation, self.request, 0), cancelled)?;
            acknowledged = true;
            match transfer_error {
                Some(kind) => Err(OperationalError::new(
                    OperationalErrorKind::Io(kind),
                    "standard stream syscall failed",
                )),
                None => Ok(progress),
            }
        })();
        result.map_err(|error| {
            let cleanup_error = if acknowledged {
                None
            } else {
                self.close().err()
            };
            bytes.fill(0);
            TransferError {
                error,
                confirmed_bytes: progress,
                uncertain_bytes_upper_bound: if admitted { length } else { 0 },
                eof,
                cleanup_error,
            }
        })
    }

    fn launch(&mut self, cancelled: &dyn Fn() -> bool) -> Result<(), OperationalError> {
        if self.worker.is_some() {
            return Ok(());
        }
        poll_cancel(cancelled)?;
        let (parent, child) = UnixDatagram::pair().map_err(io_error)?;
        native::protect_socket(&parent)?;
        native::protect_socket(&child)?;
        parent.set_nonblocking(true).map_err(io_error)?;
        let pid = native::spawn(&self.image, &child)?;
        drop(child);
        // Store ownership before any fallible identity/control operation.
        self.worker = Some(Worker {
            socket: Some(parent),
            pid: Some(pid),
        });
        native::admit(pid)?;
        poll_cancel(cancelled)?;
        native::resume(pid)?;
        let worker = self.worker.as_mut().expect("spawned worker is owned");
        worker.send(Frame::new(BIND, self.evaluation, 0, 0), cancelled)?;
        let frame = worker.receive(cancelled)?;
        if frame != Frame::new(READY, self.evaluation, 0, 0) {
            return Err(protocol());
        }
        Ok(())
    }

    fn fill_started(
        &mut self,
        destination: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), FillError> {
        self.request = self
            .request
            .checked_add(1)
            .ok_or_else(protocol)
            .map_err(FillError::not_started)?;
        let worker = self.worker.as_mut().expect("worker READY before request");
        worker
            .send(
                Frame::new(
                    FILL,
                    self.evaluation,
                    self.request,
                    destination.len() as u32,
                ),
                cancelled,
            )
            .map_err(FillError::attempted)?;
        let frame = worker.receive(cancelled).map_err(FillError::attempted)?;
        if frame == Frame::new(FAILED, self.evaluation, self.request, 0) {
            return Err(FillError::attempted(OperationalError::new(
                OperationalErrorKind::Io(io::ErrorKind::Other),
                "system entropy fill failed",
            )));
        }
        if frame
            != Frame::new(
                DATA,
                self.evaluation,
                self.request,
                destination.len() as u32,
            )
        {
            return Err(FillError::attempted(protocol()));
        }
        worker
            .receive_bytes(destination, cancelled)
            .map_err(FillError::attempted)?;
        // The full success response is accepted. Failure to acknowledge the
        // next request cannot make that already confirmed fill uncertain.
        worker
            .send(Frame::new(ACK, self.evaluation, self.request, 0), cancelled)
            .map_err(FillError::confirmed)
    }
}

impl StandardHost for PosixStandardHost {
    fn evaluation(&self) -> u64 {
        self.evaluation
    }

    fn available(&self) -> bool {
        !self.closed && native::check_parent().is_ok()
    }

    fn stream_available(&self, stream: StandardStream) -> bool {
        self.available()
            && self.endpoints[stream as usize]
                .as_ref()
                .is_some_and(|endpoint| endpoint.revalidate().is_ok())
    }

    fn read(
        &mut self,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        self.transfer(StandardStream::Stdin, bytes, None, cancelled)
    }

    fn write(
        &mut self,
        stream: StandardStream,
        bytes: &[u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<usize, TransferError> {
        if stream == StandardStream::Stdin {
            return Err(TransferError::not_started(unsupported()));
        }
        self.transfer(stream, &mut [], Some(bytes), cancelled)
    }

    fn fill(
        &mut self,
        destination: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), FillError> {
        validate_fill(destination.len()).map_err(FillError::not_started)?;
        if self.closed {
            return Err(FillError::not_started(protocol()));
        }
        if let Err(error) = self.launch(cancelled) {
            let mut error = FillError::not_started(error);
            error.cleanup_error = self.close().err();
            return Err(error);
        }
        if let Err(mut error) = self.fill_started(destination, cancelled) {
            destination.fill(0);
            error.cleanup_error = self.close().err();
            return Err(error);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), OperationalError> {
        self.closed = true;
        let worker = self
            .worker
            .take()
            .map_or(Ok(()), |mut worker| worker.close());
        let terminal = self
            .terminal_lease
            .take()
            .map_or(Ok(()), |mut lease| lease.restore());
        let signals = self
            .signal_lease
            .take()
            .map_or(Ok(()), |mut lease| lease.restore());
        self.endpoints = [None, None, None];
        worker.and(terminal).and(signals)
    }
}

impl Drop for PosixStandardHost {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct Worker {
    socket: Option<UnixDatagram>,
    pid: Option<libc::pid_t>,
}

impl Worker {
    fn socket(&self) -> &UnixDatagram {
        self.socket.as_ref().expect("live worker control socket")
    }
    fn send(&mut self, frame: Frame, cancelled: &dyn Fn() -> bool) -> Result<(), OperationalError> {
        send_bytes(self.socket(), &frame.encode(), cancelled)
    }
    fn receive(&mut self, cancelled: &dyn Fn() -> bool) -> Result<Frame, OperationalError> {
        let mut bytes = [0; HEADER_BYTES];
        self.receive_bytes(&mut bytes, cancelled)?;
        Frame::decode(&bytes)
    }
    fn receive_bytes(
        &mut self,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), OperationalError> {
        if self.pid.is_none() {
            poll_cancel(cancelled)?;
            return Err(protocol());
        }
        receive_bytes(
            self.socket.as_ref().expect("live worker control socket"),
            bytes,
            cancelled,
            &mut self.pid,
        )
    }
    fn receive_payload(
        &mut self,
        bytes: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), OperationalError> {
        for chunk in bytes.chunks_mut(1024) {
            self.receive_bytes(chunk, cancelled)?;
        }
        Ok(())
    }
    fn close(&mut self) -> Result<(), OperationalError> {
        drop(self.socket.take());
        if let Some(pid) = self.pid.take() {
            native::terminate_and_reap(pid, TERM_GRACE)?;
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Frame {
    tag: u16,
    evaluation: u64,
    request: u64,
    bytes: u32,
}
impl Frame {
    const fn new(tag: u16, evaluation: u64, request: u64, bytes: u32) -> Self {
        Self {
            tag,
            evaluation,
            request,
            bytes,
        }
    }
    fn encode(self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..8].copy_from_slice(b"OPAALSH\0");
        bytes[8..10].copy_from_slice(&1_u16.to_be_bytes());
        bytes[10..12].copy_from_slice(&self.tag.to_be_bytes());
        bytes[12..20].copy_from_slice(&self.evaluation.to_be_bytes());
        bytes[20..28].copy_from_slice(&self.request.to_be_bytes());
        bytes[28..32].copy_from_slice(&self.bytes.to_be_bytes());
        bytes
    }
    fn decode(bytes: &[u8; HEADER_BYTES]) -> Result<Self, OperationalError> {
        if &bytes[..8] != b"OPAALSH\0"
            || bytes[8..10] != 1_u16.to_be_bytes()
            || bytes[32..36] != [0; 4]
        {
            return Err(protocol());
        }
        let frame = Self {
            tag: u16::from_be_bytes(bytes[10..12].try_into().expect("fixed slice")),
            evaluation: u64::from_be_bytes(bytes[12..20].try_into().expect("fixed slice")),
            request: u64::from_be_bytes(bytes[20..28].try_into().expect("fixed slice")),
            bytes: u32::from_be_bytes(bytes[28..32].try_into().expect("fixed slice")),
        };
        let ceiling = if matches!(frame.tag, READ | WRITE_OUT | WRITE_ERR | PROGRESS) {
            MAX_STREAM_CHUNK_BYTES
        } else {
            256
        };
        if !(BIND..=IO_FAILED).contains(&frame.tag)
            || frame.evaluation == 0
            || frame.bytes as usize > ceiling
        {
            return Err(protocol());
        }
        Ok(frame)
    }
}

/// Handle the private worker argument before CLI/source/startup initialization.
/// This entry never reads source or the parent's standard streams/environment.
pub fn worker_entry(arguments: &[std::ffi::OsString]) -> Option<i32> {
    if arguments.get(1).is_none_or(|arg| arg != WORKER_ARGUMENT) {
        return None;
    }
    Some(if arguments.len() == 2 && run_worker().is_ok() {
        0
    } else {
        125
    })
}

fn run_worker() -> Result<(), OperationalError> {
    native::prepare_worker()?;
    let socket = native::control_socket()?;
    let mut header = [0; HEADER_BYTES];
    receive_bytes(&socket, &mut header, &|| false, &mut None)?;
    let binding = Frame::decode(&header)?;
    if binding != Frame::new(BIND, binding.evaluation, 0, 0) {
        return Err(protocol());
    }
    send_bytes(
        &socket,
        &Frame::new(READY, binding.evaluation, 0, 0).encode(),
        &|| false,
    )?;
    let mut request = 0_u64;
    loop {
        let endpoint = native::receive_endpoint(&socket, &mut header).map_err(io_error)?;
        let frame = Frame::decode(&header)?;
        request = request.checked_add(1).ok_or_else(protocol)?;
        if frame.evaluation != binding.evaluation || frame.request != request {
            return Err(protocol());
        }
        if matches!(frame.tag, READ | WRITE_OUT | WRITE_ERR) {
            let endpoint = endpoint.ok_or_else(protocol)?;
            stream_request(&socket, frame, endpoint)?;
            continue;
        }
        if frame.tag != FILL || endpoint.is_some() {
            return Err(protocol());
        }
        validate_fill(frame.bytes as usize)?;
        let mut payload = [0; 256];
        let payload = &mut payload[..frame.bytes as usize];
        if getrandom::fill(payload).is_err() {
            payload.fill(0);
            send_bytes(
                &socket,
                &Frame::new(FAILED, binding.evaluation, request, 0).encode(),
                &|| false,
            )?;
            return Ok(());
        }
        send_bytes(
            &socket,
            &Frame::new(DATA, binding.evaluation, request, frame.bytes).encode(),
            &|| false,
        )?;
        send_bytes(&socket, payload, &|| false)?;
        payload.fill(0);
        receive_bytes(&socket, &mut header, &|| false, &mut None)?;
        if Frame::decode(&header)? != Frame::new(ACK, binding.evaluation, request, 0) {
            return Err(protocol());
        }
    }
}

fn stream_request(
    socket: &UnixDatagram,
    frame: Frame,
    descriptor: OwnedFd,
) -> Result<(), OperationalError> {
    if frame.bytes == 0 {
        return Err(protocol());
    }
    let stream = match frame.tag {
        READ => StandardStream::Stdin,
        WRITE_OUT => StandardStream::Stdout,
        WRITE_ERR => StandardStream::Stderr,
        _ => return Err(protocol()),
    };
    let endpoint = native::Endpoint::capture(descriptor.as_fd(), stream, true)?;
    drop(descriptor);
    let mut storage = [0; MAX_STREAM_CHUNK_BYTES];
    let payload = &mut storage[..frame.bytes as usize];
    if frame.tag != READ {
        receive_payload(socket, payload, &|| false)?;
    }
    endpoint.revalidate()?;
    let result = native::stream_syscall(endpoint.as_fd(), frame.tag == READ, payload);
    let (tag, count) = match result {
        Ok(count) => (PROGRESS, count),
        Err(error) => (
            match error.kind() {
                io::ErrorKind::Interrupted => INTERRUPTED,
                io::ErrorKind::WouldBlock => {
                    native::wait_endpoint(endpoint.as_fd(), frame.tag == READ)?;
                    WOULD_BLOCK
                }
                io::ErrorKind::BrokenPipe => BROKEN_PIPE,
                _ => IO_FAILED,
            },
            0,
        ),
    };
    drop(endpoint);
    send_bytes(
        socket,
        &Frame::new(tag, frame.evaluation, frame.request, count as u32).encode(),
        &|| false,
    )?;
    if frame.tag == READ && count > 0 {
        send_payload(socket, &payload[..count], &|| false)?;
    }
    payload.fill(0);
    let mut header = [0; HEADER_BYTES];
    receive_bytes(socket, &mut header, &|| false, &mut None)?;
    if Frame::decode(&header)? != Frame::new(ACK, frame.evaluation, frame.request, 0) {
        return Err(protocol());
    }
    Ok(())
}

fn send_endpoint(
    socket: &UnixDatagram,
    frame: Frame,
    endpoint: &native::Endpoint,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), OperationalError> {
    loop {
        poll_cancel(cancelled)?;
        match native::send_endpoint(socket, &frame.encode(), endpoint.as_fd()) {
            Ok(count) if count == HEADER_BYTES => return Ok(()),
            Ok(_) => return Err(protocol()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                native::wait(socket, libc::POLLOUT, POLL_INTERVAL)?
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                // Darwin can report a full datagram queue despite POLLOUT.
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}

fn send_payload(
    socket: &UnixDatagram,
    bytes: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), OperationalError> {
    // Darwin's datagram ceiling is smaller than a host syscall chunk.
    for chunk in bytes.chunks(1024) {
        send_bytes(socket, chunk, cancelled)?;
    }
    Ok(())
}

fn receive_payload(
    socket: &UnixDatagram,
    bytes: &mut [u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), OperationalError> {
    for chunk in bytes.chunks_mut(1024) {
        receive_bytes(socket, chunk, cancelled, &mut None)?;
    }
    Ok(())
}

fn send_bytes(
    socket: &UnixDatagram,
    bytes: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), OperationalError> {
    loop {
        poll_cancel(cancelled)?;
        match native::send(socket, bytes) {
            Ok(0) => return Err(protocol()),
            Ok(count) if count == bytes.len() => return poll_cancel(cancelled),
            Ok(_) => return Err(protocol()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                native::wait(socket, libc::POLLOUT, POLL_INTERVAL)?
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                // Darwin can report a full datagram queue despite POLLOUT.
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}

fn receive_bytes(
    socket: &UnixDatagram,
    bytes: &mut [u8],
    cancelled: &dyn Fn() -> bool,
    peer: &mut Option<libc::pid_t>,
) -> Result<(), OperationalError> {
    let monitor_peer = peer.is_some();
    loop {
        poll_cancel(cancelled)?;
        match native::receive(socket, bytes) {
            Ok(0) => return Err(protocol()),
            Ok(count) if count == bytes.len() => return poll_cancel(cancelled),
            Ok(_) => return Err(protocol()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // A closed Linux datagram peer does not make recv report EOF.
                if monitor_peer && peer.is_none() {
                    return Err(protocol());
                }
                if let Some(pid) = *peer
                    && native::wait_child(pid, true)?
                {
                    *peer = None;
                    // A final record can arrive between recv and waitpid.
                    continue;
                }
                native::wait(socket, libc::POLLIN, POLL_INTERVAL)?
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}
fn poll_cancel(cancelled: &dyn Fn() -> bool) -> Result<(), OperationalError> {
    if cancelled() {
        Err(OperationalError::new(
            OperationalErrorKind::Cancelled,
            "entropy operation cancelled",
        ))
    } else {
        Ok(())
    }
}
fn protocol() -> OperationalError {
    OperationalError::new(
        OperationalErrorKind::Protocol,
        "invalid standard host protocol",
    )
}
fn unsupported() -> OperationalError {
    OperationalError::new(
        OperationalErrorKind::Unsupported,
        "verified standard host is unavailable",
    )
}
fn io_error(error: io::Error) -> OperationalError {
    OperationalError::new(
        OperationalErrorKind::Io(error.kind()),
        "standard host transport failed",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reaped_worker_receives_preserve_cancellation_precedence() {
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let mut worker = Worker {
            socket: Some(socket),
            pid: None,
        };
        for (cancelled, expected) in [
            (false, OperationalErrorKind::Protocol),
            (true, OperationalErrorKind::Cancelled),
        ] {
            let error = worker
                .receive_bytes(&mut [0; HEADER_BYTES], &|| cancelled)
                .unwrap_err();
            assert_eq!(error.kind(), expected);
        }
    }

    #[test]
    fn live_transport_backpressure_cancels_and_resumes_without_replaying_datagrams() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        sender.set_nonblocking(true).unwrap();
        native::protect_socket(&sender).unwrap();
        let bytes = [0x47; 256];
        let mut queued = 0;
        let blocked = loop {
            match sender.send(&bytes) {
                Ok(256) => queued += 1,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(libc::ENOBUFS) =>
                {
                    break error.kind();
                }
                result => panic!("unexpected queue result: {result:?}"),
            }
            assert!(queued < 65536);
        };
        assert!(queued > 0);
        let started = Instant::now();
        let error = send_bytes(&sender, &bytes, &|| {
            started.elapsed() >= Duration::from_millis(40)
        })
        .unwrap_err();
        assert_eq!(error.kind(), OperationalErrorKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));

        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        if blocked != io::ErrorKind::WouldBlock {
            for _ in 0..queued {
                assert_eq!(receiver.recv(&mut [0; 256]).unwrap(), 256);
            }
            queued = 0;
        }
        let drain = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            for _ in 0..=queued {
                let mut received = [0; 256];
                assert_eq!(receiver.recv(&mut received).unwrap(), 256);
                assert_eq!(received, bytes);
            }
            receiver.set_nonblocking(true).unwrap();
            assert_eq!(
                receiver.recv(&mut [0; 256]).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        });
        let started = Instant::now();
        send_bytes(&sender, &bytes, &|| {
            started.elapsed() >= Duration::from_secs(1)
        })
        .unwrap();
        drain.join().unwrap();
    }

    #[test]
    fn deterministic_hostile_headers_preserve_closed_decode_invariants() {
        let mut state = 71_u64;
        for index in 0..1000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut bytes =
                Frame::new(1 + (index % 6) as u16, 71, state, (index % 257) as u32).encode();
            bytes[(state as usize) % HEADER_BYTES] ^= (state >> 32) as u8;
            if let Ok(frame) = Frame::decode(&bytes) {
                assert_eq!(frame.encode(), bytes);
                assert!((BIND..=IO_FAILED).contains(&frame.tag));
                assert_ne!(frame.evaluation, 0);
                assert!(frame.bytes <= MAX_STREAM_CHUNK_BYTES as u32);
            }
        }
    }
    #[test]
    fn frames_are_closed_bounded_and_identity_preserving() {
        let frame = Frame::new(FILL, 71, 1, 256);
        assert_eq!(Frame::decode(&frame.encode()).unwrap(), frame);
        for (offset, byte) in [(0, 0), (8, 2), (10, 1), (32, 1)] {
            let mut bytes = frame.encode();
            bytes[offset] = byte;
            assert!(Frame::decode(&bytes).is_err());
        }
        assert!(Frame::decode(&Frame::new(FILL, 0, 1, 8).encode()).is_err());
        assert!(Frame::decode(&Frame::new(FILL, 71, 1, 257).encode()).is_err());
    }

    #[test]
    fn atomic_control_records_reject_trailing_and_truncated_data() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let mut header = [0; HEADER_BYTES];
        let mut excess = Frame::new(FILL, 71, 1, 8).encode().to_vec();
        excess.push(0);
        sender.send(&excess).unwrap();
        assert!(receive_bytes(&receiver, &mut header, &|| false, &mut None).is_err());
        sender.send(&[0; HEADER_BYTES - 1]).unwrap();
        assert!(receive_bytes(&receiver, &mut header, &|| false, &mut None).is_err());
        sender.send(&Frame::new(FILL, 71, 1, 8).encode()).unwrap();
        receive_bytes(&receiver, &mut header, &|| false, &mut None).unwrap();
        assert_eq!(Frame::decode(&header).unwrap(), Frame::new(FILL, 71, 1, 8));
    }
}
