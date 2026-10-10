//! Faults live only in the fixture image; production has no protocol override.
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

use opaal_platform::operational::OperationalErrorKind;
use opaal_platform::standard_host::{FillProgress, StandardHost, StandardStream};
use opaal_platform_posix::standard_host::PosixStandardHost;

fn frame(tag: u16, evaluation: u64, request: u64, count: u32) -> Vec<u8> {
    let mut bytes = vec![0; 36];
    bytes[..8].copy_from_slice(b"OPAALSH\0");
    bytes[8..10].copy_from_slice(&1_u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&tag.to_be_bytes());
    bytes[12..20].copy_from_slice(&evaluation.to_be_bytes());
    bytes[20..28].copy_from_slice(&request.to_be_bytes());
    bytes[28..32].copy_from_slice(&count.to_be_bytes());
    bytes
}

#[allow(unsafe_code)]
pub fn worker() {
    let mut binding = [0; 36];
    // SAFETY: the launcher owns fd8 and supplies a connected datagram socket.
    // Peeking leaves the ordinary production binding intact for other modes.
    let count = unsafe { libc::recv(8, binding.as_mut_ptr().cast(), 36, libc::MSG_PEEK) };
    if count != 36 {
        return;
    }
    let evaluation = u64::from_be_bytes(binding[12..20].try_into().unwrap());
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    super::blocked_library::enable(evaluation);
    super::standard_streams::enable_syscall_fault(evaluation);
    if (130..=149).contains(&evaluation) {
        stream_worker(evaluation, binding);
    }
    if !(100..=120).contains(&evaluation) || binding.as_slice() != frame(1, evaluation, 0, 0) {
        return;
    }
    // SAFETY: this fault worker adopts its sole control socket once and exits.
    let socket = unsafe { UnixDatagram::from_raw_fd(8) };
    assert_eq!(socket.recv(&mut binding).unwrap(), 36);
    let mut ready = frame(2, evaluation, 0, 0);
    match evaluation {
        100 => ready[0] = 0,
        101 => ready[9] = 2,
        102 => ready[11] = 99,
        103 => ready[19] ^= 1,
        104 => ready[27] = 1,
        105 => ready[31] = 1,
        106 => ready[32] = 1,
        107 => {
            ready.pop();
        }
        108 => ready.push(0),
        109 => {
            send_descriptor(&socket, &ready);
            wait_for_teardown();
        }
        110 => {
            // SAFETY: only this fault worker ignores TERM to exercise KILL.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            wait_for_teardown();
        }
        111 => std::process::exit(0),
        _ => {}
    }
    socket.send(&ready).unwrap();
    if evaluation <= 108 {
        wait_for_teardown();
    }
    let mut request = [0; 36];
    assert_eq!(socket.recv(&mut request).unwrap(), 36);
    assert_eq!(request.as_slice(), frame(3, evaluation, 1, 8));
    let mut data = frame(4, evaluation, 1, 8);
    match evaluation {
        112 => data[27] = 2,
        113 => {
            data[30] = 1;
            data[31] = 1;
        }
        114 => data[31] = 7,
        115 => {
            socket.send(&ready).unwrap();
            wait_for_teardown();
        }
        116 => {
            socket.send(&frame(6, evaluation, 1, 0)).unwrap();
            wait_for_teardown();
        }
        117 => {
            socket.send(&data).unwrap();
            socket.send(&[0x47; 7]).unwrap();
            wait_for_teardown();
        }
        118 => {
            send_descriptor(&socket, &data);
            wait_for_teardown();
        }
        119 => {
            socket.send(&data).unwrap();
            socket.send(&[0x47; 8]).unwrap();
            let mut ack = [0; 36];
            assert_eq!(socket.recv(&mut ack).unwrap(), 36);
            assert_eq!(ack.as_slice(), frame(5, evaluation, 1, 0));
        }
        120 => {
            send_descriptors(&socket, &data, 253);
            wait_for_teardown();
        }
        _ => unreachable!(),
    }
    socket.send(&data).unwrap();
    wait_for_teardown();
}

fn wait_for_teardown() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[allow(unsafe_code)]
fn stream_worker(evaluation: u64, mut binding: [u8; 36]) -> ! {
    // SAFETY: this selected fixture worker adopts its sole control socket once.
    let socket = unsafe { UnixDatagram::from_raw_fd(8) };
    assert_eq!(socket.recv(&mut binding).unwrap(), 36);
    socket.send(&frame(2, evaluation, 0, 0)).unwrap();
    let mut request = [0; 36];
    assert_eq!(socket.recv(&mut request).unwrap(), 36);
    let write = matches!(evaluation, 144 | 147);
    assert_eq!(
        request.as_slice(),
        frame(if write { 8 } else { 7 }, evaluation, 1, 8)
    );
    if write {
        assert_eq!(socket.recv(&mut [0; 8]).unwrap(), 8);
    }
    if evaluation == 148 {
        std::process::exit(0);
    }
    let mut response = frame(10, evaluation, 1, 8);
    match evaluation {
        130 => response[19] ^= 1,
        131 => response[27] = 2,
        132 => response[31] = 9,
        133 => response = frame(2, evaluation, 0, 0),
        134 => response = frame(11, evaluation, 1, 1),
        135 => {
            response.pop();
        }
        136 => {
            send_descriptor(&socket, &response);
            wait_for_teardown();
        }
        139 | 144 | 145 => response = frame(10, evaluation, 1, 0),
        146 | 147 => response = frame(10, evaluation, 1, 3),
        140..=143 => response = frame(evaluation as u16 - 129, evaluation, 1, 0),
        _ => {}
    }
    socket.send(&response).unwrap();
    if evaluation == 149 {
        std::process::exit(0);
    }
    match evaluation {
        137 => {
            socket.send(&[0x47; 7]).unwrap();
        }
        138 => {}
        146 => {
            socket.send(&[0x47; 3]).unwrap();
        }
        139..=144 => {
            let mut ack = [0; 36];
            assert_eq!(socket.recv(&mut ack).unwrap(), 36);
            assert_eq!(ack.as_slice(), frame(5, evaluation, 1, 0));
            assert_eq!(socket.recv(&mut request).unwrap(), 36);
            assert_eq!(request.as_slice(), frame(3, evaluation, 2, 8));
            socket.send(&frame(4, evaluation, 2, 8)).unwrap();
            socket.send(&[0x47; 8]).unwrap();
            assert_eq!(socket.recv(&mut ack).unwrap(), 36);
            assert_eq!(ack.as_slice(), frame(5, evaluation, 2, 0));
        }
        _ => {}
    }
    wait_for_teardown();
}

pub fn stream_parent() {
    use std::os::fd::AsFd;
    drop(PosixStandardHost::for_cli(71).unwrap());
    let before = descriptors();
    for evaluation in 130..=149 {
        super::checks::cancel_after_record(if (145..=147).contains(&evaluation) {
            evaluation
        } else {
            0
        });
        let mut host = PosixStandardHost::for_cli(evaluation).unwrap();
        // Bind an ordinary file: the replacement worker supplies only control faults.
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let stream = if matches!(evaluation, 144 | 147) {
            StandardStream::Stdout
        } else {
            StandardStream::Stdin
        };
        if stream == StandardStream::Stdin {
            host.bind_stream(stream, file.as_fd()).unwrap();
        } else {
            let (reader, writer) = super::standard_streams::pipe();
            host.bind_stream(stream, writer.as_fd()).unwrap();
            drop((reader, writer));
        }
        let started = Instant::now();
        let cancelled =
            || super::checks::record_cancelled() || started.elapsed() >= Duration::from_secs(1);
        let mut bytes = [0xa5; 8];
        let result = if stream == StandardStream::Stdin {
            host.read(&mut bytes, &cancelled)
        } else {
            host.write(stream, &[0x47; 8], &cancelled)
        };
        match evaluation {
            139 | 144 => assert_eq!(result.unwrap(), 0),
            140..=143 => {
                let error = result.unwrap_err();
                assert_eq!(
                    error.error.kind(),
                    OperationalErrorKind::Io(
                        [
                            std::io::ErrorKind::Interrupted,
                            std::io::ErrorKind::WouldBlock,
                            std::io::ErrorKind::BrokenPipe,
                            std::io::ErrorKind::Other,
                        ][(evaluation - 140) as usize]
                    )
                );
                assert_eq!(error.confirmed_bytes, 0);
                assert_eq!(error.uncertain_bytes_upper_bound, 0);
                assert_eq!(error.eof, None);
                assert!(error.cleanup_error.is_none());
            }
            _ => {
                let error = result.unwrap_err();
                assert_eq!(
                    error.error.kind(),
                    match evaluation {
                        136 => OperationalErrorKind::Io(std::io::ErrorKind::InvalidData),
                        138 | 145..=147 => OperationalErrorKind::Cancelled,
                        148 | 149 => error.error.kind(),
                        _ => OperationalErrorKind::Protocol,
                    },
                    "evaluation {evaluation}: {error:?}"
                );
                assert_eq!(
                    error.confirmed_bytes,
                    match evaluation {
                        137 | 138 | 149 => 8,
                        146 | 147 => 3,
                        _ => 0,
                    }
                );
                assert_eq!(
                    error.uncertain_bytes_upper_bound,
                    if evaluation >= 137 && evaluation != 148 {
                        0
                    } else {
                        8
                    }
                );
                if matches!(evaluation, 148 | 149) {
                    assert!(matches!(
                        error.error.kind(),
                        OperationalErrorKind::Protocol | OperationalErrorKind::Io(_)
                    ));
                }
                assert_eq!(error.eof, if evaluation == 145 { Some(true) } else { None });
                assert!(error.cleanup_error.is_none());
                assert_eq!(bytes, if evaluation == 147 { [0xa5; 8] } else { [0; 8] });
                assert!(!host.available());
            }
        }
        if (139..=144).contains(&evaluation) {
            assert!(host.available());
            host.fill(&mut bytes, &|| false).unwrap();
            assert_eq!(bytes, [0x47; 8]);
        }
        host.close().unwrap();
        drop((host, file));
        super::checks::cancel_after_record(0);
        super::checks::assert_reaped();
        assert_eq!(
            descriptors(),
            before,
            "descriptor leak at evaluation {evaluation}"
        );
    }
}

#[allow(unsafe_code)]
fn send_descriptor(socket: &UnixDatagram, bytes: &[u8]) {
    send_descriptors(socket, bytes, 1);
}

#[allow(unsafe_code)]
fn send_descriptors(socket: &UnixDatagram, bytes: &[u8], count: usize) {
    let file = std::fs::File::open("/dev/null").unwrap();
    send_file_descriptors(socket, bytes, &file, count);
}

#[allow(unsafe_code)]
fn send_file_descriptors(socket: &UnixDatagram, bytes: &[u8], file: &impl AsRawFd, count: usize) {
    // SAFETY: aligned control storage holds one SCM_RIGHTS record; the byte
    // buffer and borrowed file stay live through this single sendmsg call.
    unsafe {
        let mut control = [0_usize; 512];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE((count * 4) as u32) as _;
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((count * 4) as u32) as _;
        for index in 0..count {
            std::ptr::write_unaligned(
                libc::CMSG_DATA(header).cast::<i32>().add(index),
                file.as_raw_fd(),
            );
        }
        assert_eq!(
            libc::sendmsg(socket.as_raw_fd(), &raw const message, 0),
            bytes.len() as isize
        );
    }
}

#[allow(unsafe_code)]
fn descriptors() -> Vec<(i32, i32, i32)> {
    // SAFETY: querying this isolated fixture's descriptors changes no flags.
    (0..1024)
        .filter_map(|fd| unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            (flags >= 0).then(|| (fd, flags, libc::fcntl(fd, libc::F_GETFL)))
        })
        .collect()
}

pub fn parent() {
    drop(PosixStandardHost::for_cli(71).unwrap());
    let before = descriptors();
    for evaluation in 100..=120 {
        super::checks::truncate_ancillary(cfg!(target_os = "linux") && evaluation == 120);
        let mut host = PosixStandardHost::for_cli(evaluation).unwrap();
        let started = Instant::now();
        let timeout = if matches!(evaluation, 110 | 111) {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(5)
        };
        let cancelled = || started.elapsed() >= timeout;
        let mut bytes = [0xa5; 8];
        if evaluation == 119 {
            host.fill(&mut bytes, &cancelled).unwrap();
            assert_eq!(bytes, [0x47; 8]);
        }
        let error = host.fill(&mut bytes, &cancelled).unwrap_err();
        let kind = match evaluation {
            108 | 109 | 118 | 120 => OperationalErrorKind::Io(std::io::ErrorKind::InvalidData),
            110 => OperationalErrorKind::Cancelled,
            111 => error.error.kind(),
            116 => OperationalErrorKind::Io(std::io::ErrorKind::Other),
            _ => OperationalErrorKind::Protocol,
        };
        assert_eq!(error.error.kind(), kind, "evaluation {evaluation}");
        if evaluation == 111 {
            assert!(matches!(
                kind,
                OperationalErrorKind::Protocol | OperationalErrorKind::Io(_)
            ));
        }
        assert_eq!(
            error.progress,
            if evaluation <= 111 {
                FillProgress::NotStarted
            } else {
                FillProgress::Uncertain
            }
        );
        assert_eq!(bytes, if evaluation <= 111 { [0xa5; 8] } else { [0; 8] });
        assert!(error.cleanup_error.is_none(), "{error:?}");
        assert!(started.elapsed() < timeout + Duration::from_secs(1));
        assert!(!host.available());
        assert_eq!(
            host.fill(&mut bytes, &|| false).unwrap_err().progress,
            FillProgress::NotStarted
        );
        host.close().unwrap();
        drop(host);
        super::checks::truncate_ancillary(false);
        if cfg!(target_os = "linux") && evaluation == 120 {
            super::checks::assert_ancillary_truncated();
        }
        super::checks::assert_reaped();
        assert_eq!(
            descriptors(),
            before,
            "descriptor leak at evaluation {evaluation}"
        );
    }
    {
        let mut host = PosixStandardHost::for_cli(71).unwrap();
        assert_eq!(
            host.fill(&mut [0; 257], &|| false).unwrap_err().progress,
            FillProgress::NotStarted
        );
        super::checks::assert_reaped();
        host.fill(&mut [0; 8], &|| false).unwrap();
    }
    super::checks::assert_reaped();
    assert_eq!(descriptors(), before);
}

/// Drive the production worker with hostile parent records, not a replacement worker.
#[allow(unsafe_code)]
pub fn worker_inputs(input: Option<&[u8]>) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    drop(PosixStandardHost::for_cli(71).unwrap());
    let before = descriptors();
    let cases = if let Some(input) = input {
        vec![(usize::from(input.first().copied().unwrap_or(0) % 7), 15)]
    } else {
        (0..7)
            .flat_map(|stage| (0..15).map(move |mutation| (stage, mutation)))
            .collect()
    };
    for (stage, mutation) in cases {
        let (socket, child_socket) = UnixDatagram::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // SAFETY: duplicate the borrowed socket above all target descriptors;
        // its owner stays live until spawn completes.
        let fd = unsafe { libc::fcntl(child_socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
        assert!(fd >= 64);
        // SAFETY: successful fcntl transferred exactly one descriptor.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg(opaal_platform_posix::standard_host::WORKER_ARGUMENT)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: this isolated fixture hook uses only native descriptor and
        // signal operations after fork, with no allocator or Rust worker code.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 8) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for other in (3..1024).filter(|other| *other != 8) {
                    libc::close(other);
                }
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut mask);
                if libc::sigprocmask(libc::SIG_SETMASK, &raw const mask, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(owned);
        drop(child_socket);
        let result = (|| -> std::io::Result<()> {
            let mut header = [0; 36];
            if stage > 0 {
                socket.send(&frame(1, 71, 0, 0))?;
                assert_eq!(socket.recv(&mut header)?, 36);
                assert_eq!(header.as_slice(), frame(2, 71, 0, 0));
            }
            if stage == 2 {
                socket.send(&frame(3, 71, 1, 8))?;
                assert_eq!(socket.recv(&mut header)?, 36);
                assert_eq!(header.as_slice(), frame(4, 71, 1, 8));
                assert_eq!(socket.recv(&mut [0; 8])?, 8);
            }
            if matches!(stage, 4 | 6) {
                use std::io::Write;
                let (reader, writer) = super::standard_streams::pipe();
                if stage == 4 {
                    let mut writer = std::fs::File::from(writer);
                    writer.write_all(&[0x47; 8])?;
                    send_file_descriptors(&socket, &frame(7, 71, 1, 8), &reader, 1);
                    assert_eq!(socket.recv(&mut header)?, 36);
                    assert_eq!(header.as_slice(), frame(10, 71, 1, 8));
                    let mut payload = [0; 8];
                    assert_eq!(socket.recv(&mut payload)?, 8);
                    assert_eq!(payload, [0x47; 8]);
                } else {
                    send_file_descriptors(&socket, &frame(8, 71, 1, 8), &writer, 1);
                    socket.send(&[0x47; 8])?;
                    assert_eq!(socket.recv(&mut header)?, 36);
                    assert_eq!(header.as_slice(), frame(10, 71, 1, 8));
                }
            }
            let (tag, request, count) = [
                (1, 0, 0),
                (3, 1, 8),
                (5, 1, 0),
                (7, 1, 8),
                (5, 1, 0),
                (8, 1, 8),
                (5, 1, 0),
            ][stage];
            let mut bytes = if let Some(input) = input {
                input.get(2..).unwrap_or_default().to_vec()
            } else {
                frame(tag, 71, request, count)
            };
            if input.is_some()
                && stage == 0
                && bytes.len() >= 20
                && matches!(u64::from_be_bytes(bytes[12..20].try_into().unwrap()), 100..=120 | 130..=147 | 200..=204)
            {
                // Keep fuzzed bindings out of this image's supervisor fault modes.
                bytes[12..20].copy_from_slice(&71_u64.to_be_bytes());
            }
            match mutation {
                0 => bytes[0] ^= 1,
                1 => bytes[9] = 2,
                2 => bytes[11] = 99,
                3 => bytes[12..20].fill(0),
                4 => {
                    if stage == 0 {
                        bytes[27] = 1
                    } else {
                        bytes[19] = 72
                    }
                }
                5 => bytes[27] = 2,
                6 => bytes[28..32].copy_from_slice(&257_u32.to_be_bytes()),
                7 => bytes[32] = 1,
                8 => {
                    bytes.pop();
                }
                9 => bytes.push(0),
                10 => bytes[11] = 2,
                11 => bytes[31] = if stage == 1 { 0 } else { 8 },
                12 => {
                    send_descriptors(&socket, &bytes, 1);
                    return Ok(());
                }
                13 => {
                    send_descriptors(&socket, &bytes, 32);
                    return Ok(());
                }
                14 => {
                    send_descriptors(&socket, &bytes, 253);
                    return Ok(());
                }
                15 => {
                    let count = [0, 1, 32, 253]
                        [usize::from(input.unwrap().get(1).copied().unwrap_or(0) % 4)];
                    if count > 0 {
                        send_descriptors(&socket, &bytes, count);
                        return Ok(());
                    }
                }
                _ => unreachable!(),
            }
            socket.send(&bytes)?;
            if mutation == 15 && bytes.len() == 36 {
                let evaluation = u64::from_be_bytes(bytes[12..20].try_into().unwrap());
                let count = u32::from_be_bytes(bytes[28..32].try_into().unwrap());
                if stage == 0 && evaluation > 0 && bytes == frame(1, evaluation, 0, 0) {
                    assert_eq!(socket.recv(&mut header)?, 36);
                    assert_eq!(header.as_slice(), frame(2, evaluation, 0, 0));
                    socket.send(b"invalid")?;
                } else if stage == 1
                    && (1..=256).contains(&count)
                    && bytes == frame(3, 71, 1, count)
                {
                    assert_eq!(socket.recv(&mut header)?, 36);
                    assert_eq!(header.as_slice(), frame(4, 71, 1, count));
                    assert_eq!(socket.recv(&mut vec![0; count as usize])?, count as usize);
                    socket.send(&frame(5, 71, 1, 0))?;
                    socket.send(b"invalid")?;
                } else if matches!(stage, 2 | 4 | 6) && bytes == frame(5, 71, 1, 0) {
                    socket.send(b"invalid")?;
                }
            }
            Ok(())
        })();
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if started.elapsed() > Duration::from_secs(5) {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        drop(socket);
        assert!(
            result.is_ok(),
            "stage {stage} mutation {mutation}: {result:?}"
        );
        assert_eq!(
            status.and_then(|status| status.code()),
            Some(125),
            "stage {stage} mutation {mutation}"
        );
        super::checks::assert_reaped();
        assert_eq!(descriptors(), before, "stage {stage} mutation {mutation}");
    }
}

pub fn valid_worker_inputs() {
    for (stage, (tag, request, count)) in
        [(1, 0, 0), (3, 1, 256), (5, 1, 0)].into_iter().enumerate()
    {
        let mut input = vec![stage as u8, 0];
        input.extend(frame(tag, 71, request, count));
        worker_inputs(Some(&input));
    }
    for stage in [4, 6] {
        let mut input = vec![stage, 0];
        input.extend(frame(5, 71, 1, 0));
        worker_inputs(Some(&input));
    }
}
