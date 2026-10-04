//! Same-image entropy worker. The supervisor owns the child and control socket;
//! no source, environment, standard stream or capsule descriptor enters it.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use opaal_platform::standard_host::{FillError, StandardHost, validate_fill};

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

/// Explicit native CLI binding; an arbitrary embedding must supply its own
/// qualified worker entry point and child ownership instead of using this.
pub struct PosixStandardHost {
    evaluation: u64,
    image: image::Image,
    worker: Option<Worker>,
    request: u64,
    closed: bool,
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
        receive_bytes(worker.socket(), destination, cancelled).map_err(FillError::attempted)?;
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
        if let Some(mut worker) = self.worker.take() {
            worker.close()?;
        }
        Ok(())
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
        receive_bytes(self.socket(), &mut bytes, cancelled)?;
        Frame::decode(&bytes)
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
        if !(BIND..=FAILED).contains(&frame.tag) || frame.evaluation == 0 || frame.bytes > 256 {
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
    receive_bytes(&socket, &mut header, &|| false)?;
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
        receive_bytes(&socket, &mut header, &|| false)?;
        let frame = Frame::decode(&header)?;
        request = request.checked_add(1).ok_or_else(protocol)?;
        if frame.tag != FILL || frame.evaluation != binding.evaluation || frame.request != request {
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
        receive_bytes(&socket, &mut header, &|| false)?;
        if Frame::decode(&header)? != Frame::new(ACK, binding.evaluation, request, 0) {
            return Err(protocol());
        }
    }
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
            Err(error) => return Err(io_error(error)),
        }
    }
}

fn receive_bytes(
    socket: &UnixDatagram,
    bytes: &mut [u8],
    cancelled: &dyn Fn() -> bool,
) -> Result<(), OperationalError> {
    loop {
        poll_cancel(cancelled)?;
        match native::receive(socket, bytes) {
            Ok(0) => return Err(protocol()),
            Ok(count) if count == bytes.len() => return poll_cancel(cancelled),
            Ok(_) => return Err(protocol()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
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
        assert!(receive_bytes(&receiver, &mut header, &|| false).is_err());
        sender.send(&[0; HEADER_BYTES - 1]).unwrap();
        assert!(receive_bytes(&receiver, &mut header, &|| false).is_err());
        sender.send(&Frame::new(FILL, 71, 1, 8).encode()).unwrap();
        receive_bytes(&receiver, &mut header, &|| false).unwrap();
        assert_eq!(Frame::decode(&header).unwrap(), Frame::new(FILL, 71, 1, 8));
    }
}
