//! Native stream observations run in the isolated same-image fixture.
#![allow(unsafe_code)]
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use opaal_platform::operational::OperationalErrorKind;
use opaal_platform::standard_host::{MAX_STREAM_CHUNK_BYTES, StandardHost, StandardStream};
use opaal_platform_posix::standard_host::PosixStandardHost;

static SYSCALL_FAULT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn enable_syscall_fault(evaluation: u64) {
    if (200..=204).contains(&evaluation) {
        SYSCALL_FAULT.store(evaluation, std::sync::atomic::Ordering::Relaxed);
    }
}

unsafe fn syscall(fd: i32, bytes: *mut libc::c_void, count: usize, read: bool) -> libc::ssize_t {
    if fd >= 3 && fd != 8 && count == 61 {
        let mode = SYSCALL_FAULT.swap(0, std::sync::atomic::Ordering::Relaxed);
        if (200..=203).contains(&mode) {
            let error = [libc::EINTR, libc::EAGAIN, libc::EPIPE, libc::EIO][(mode - 200) as usize];
            // SAFETY: errno is this worker thread's native writable slot.
            unsafe {
                #[cfg(target_os = "macos")]
                {
                    *libc::__error() = error;
                }
                #[cfg(target_os = "linux")]
                {
                    *libc::__errno_location() = error;
                }
            }
            return -1;
        }
        if mode == 204 {
            return 0;
        }
    }
    // SAFETY: RTLD_NEXT supplies the OS function with the exact libc ABI;
    // the original caller keeps the descriptor and buffer live for the call.
    unsafe {
        let symbol = libc::dlsym(
            libc::RTLD_NEXT,
            if read { c"read" } else { c"write" }.as_ptr(),
        );
        if symbol.is_null() {
            std::process::abort();
        }
        let original: unsafe extern "C" fn(i32, *mut libc::c_void, usize) -> libc::ssize_t =
            std::mem::transmute(symbol);
        original(fd, bytes, count)
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn read(fd: i32, bytes: *mut libc::c_void, count: usize) -> libc::ssize_t {
    // SAFETY: preserve the caller's read ABI and writable buffer contract.
    unsafe { syscall(fd, bytes, count, true) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn write(fd: i32, bytes: *const libc::c_void, count: usize) -> libc::ssize_t {
    // SAFETY: the write path never mutates this caller's read-only buffer.
    unsafe { syscall(fd, bytes.cast_mut(), count, false) }
}

pub(super) fn pipe() -> (OwnedFd, OwnedFd) {
    let mut descriptors = [0; 2];
    // SAFETY: this fixture creates and adopts each owned pipe end exactly once.
    unsafe {
        assert_eq!(libc::pipe(descriptors.as_mut_ptr()), 0);
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    }
}

fn flags(fd: &impl AsRawFd) -> i32 {
    // SAFETY: only the live borrowed test endpoint's flags are inspected.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    #[cfg(target_os = "macos")]
    // Darwin's GC marks and write history do not change the endpoint's I/O mode.
    let flags = flags & !0x0001_3000;
    flags
}

pub fn run() {
    let (reader, writer) = pipe();
    let drain = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        File::from(reader).read_to_end(&mut bytes).unwrap();
        bytes
    });
    let mut host = PosixStandardHost::for_cli(71).unwrap();
    host.bind_stream(StandardStream::Stdout, writer.as_fd())
        .unwrap();
    for _ in 0..128 {
        assert_eq!(
            host.write(StandardStream::Stdout, b"x", &|| false).unwrap(),
            1
        );
    }
    host.close().unwrap();
    drop(writer);
    assert_eq!(drain.join().unwrap(), vec![b'x'; 128]);
    let directory = std::env::temp_dir().join(format!("opaal-streams-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let mut input = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.join("input"))
        .unwrap();
    let mut output = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.join("output"))
        .unwrap();
    let mut errors = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.join("errors"))
        .unwrap();
    let payload = (0..MAX_STREAM_CHUNK_BYTES)
        .map(|index| index as u8)
        .collect::<Vec<_>>();
    input.write_all(&payload).unwrap();
    input.seek(SeekFrom::Start(0)).unwrap();
    for stream in [
        StandardStream::Stdin,
        StandardStream::Stdout,
        StandardStream::Stderr,
    ] {
        for evaluation in 200..=204 {
            input.seek(SeekFrom::Start(0)).unwrap();
            let mut host = PosixStandardHost::for_cli(evaluation).unwrap();
            host.bind_stream(
                stream,
                if stream == StandardStream::Stdin {
                    input.as_fd()
                } else {
                    output.as_fd()
                },
            )
            .unwrap();
            let mut bytes = [0xa5; 61];
            let result = if stream == StandardStream::Stdin {
                host.read(&mut bytes, &|| false)
            } else {
                host.write(stream, &[0x47; 61], &|| false)
            };
            if evaluation == 204 {
                assert_eq!(result.unwrap(), 0);
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    error.error.kind(),
                    OperationalErrorKind::Io(
                        [
                            std::io::ErrorKind::Interrupted,
                            std::io::ErrorKind::WouldBlock,
                            std::io::ErrorKind::BrokenPipe,
                            std::io::ErrorKind::Other,
                        ][(evaluation - 200) as usize]
                    )
                );
                assert_eq!(
                    (error.confirmed_bytes, error.uncertain_bytes_upper_bound),
                    (0, 0)
                );
                assert_eq!(error.eof, None);
                assert!(error.cleanup_error.is_none());
                assert_eq!(
                    bytes,
                    if stream == StandardStream::Stdin {
                        [0; 61]
                    } else {
                        [0xa5; 61]
                    }
                );
            }
            assert!(host.available());
            if stream == StandardStream::Stdin {
                assert_eq!(host.read(&mut bytes, &|| false).unwrap(), 61);
                assert_eq!(bytes.as_slice(), &payload[..61]);
            } else {
                assert_eq!(host.write(stream, &[0x47; 61], &|| false).unwrap(), 61);
            }
            host.fill(&mut [0; 8], &|| false).unwrap();
            host.close().unwrap();
            super::checks::assert_reaped();
        }
    }
    output.set_len(0).unwrap();
    output.seek(SeekFrom::Start(0)).unwrap();
    input.seek(SeekFrom::Start(0)).unwrap();
    let original_flags = [flags(&input), flags(&output), flags(&errors)];
    let mut host = PosixStandardHost::for_cli(71).unwrap();
    host.bind_stream(StandardStream::Stdin, input.as_fd())
        .unwrap();
    host.bind_stream(StandardStream::Stdout, output.as_fd())
        .unwrap();
    host.bind_stream(StandardStream::Stderr, errors.as_fd())
        .unwrap();
    assert!(
        host.bind_stream(StandardStream::Stdout, errors.as_fd())
            .is_err()
    );
    assert_eq!(
        host.write(StandardStream::Stdout, &payload, &|| false)
            .unwrap(),
        payload.len()
    );
    assert_eq!(
        host.write(StandardStream::Stderr, b"\0\xff\n", &|| false)
            .unwrap(),
        3
    );
    assert_eq!(
        host.write(StandardStream::Stdout, b"!", &|| false).unwrap(),
        1
    );
    let mut bytes = vec![0; MAX_STREAM_CHUNK_BYTES];
    assert_eq!(host.read(&mut bytes, &|| false).unwrap(), payload.len());
    assert_eq!(bytes, payload);
    assert_eq!(host.read(&mut [0; 1], &|| false).unwrap(), 0);
    host.fill(&mut [0; 8], &|| false).unwrap();
    host.close().unwrap();
    assert!(!host.stream_available(StandardStream::Stdout));
    assert_eq!(
        [flags(&input), flags(&output), flags(&errors)],
        original_flags
    );
    output.seek(SeekFrom::Start(0)).unwrap();
    let mut result = Vec::new();
    output.read_to_end(&mut result).unwrap();
    assert_eq!(result, [payload.as_slice(), b"!"].concat());
    errors.seek(SeekFrom::Start(0)).unwrap();
    result.clear();
    errors.read_to_end(&mut result).unwrap();
    assert_eq!(result, b"\0\xff\n");

    let (reader, writer) = pipe();
    let mut host = PosixStandardHost::for_cli(71).unwrap();
    assert!(
        host.bind_stream(StandardStream::Stdout, reader.as_fd())
            .is_err()
    );
    assert!(
        host.bind_stream(StandardStream::Stdin, writer.as_fd())
            .is_err()
    );
    let (socket, _) = std::os::unix::net::UnixDatagram::pair().unwrap();
    assert!(
        host.bind_stream(StandardStream::Stdin, socket.as_fd())
            .is_err()
    );
    host.bind_stream(StandardStream::Stdout, writer.as_fd())
        .unwrap();
    host.bind_stream(StandardStream::Stderr, errors.as_fd())
        .unwrap();
    drop(reader);
    let error = host
        .write(StandardStream::Stdout, b"broken", &|| false)
        .unwrap_err();
    assert_eq!(
        error.error.kind(),
        OperationalErrorKind::Io(std::io::ErrorKind::BrokenPipe)
    );
    assert_eq!(error.confirmed_bytes, 0);
    assert_eq!(error.uncertain_bytes_upper_bound, 0);
    assert!(error.cleanup_error.is_none());
    assert_eq!(
        host.write(StandardStream::Stderr, b"caught", &|| false)
            .unwrap(),
        6
    );
    host.fill(&mut [0; 8], &|| false).unwrap();
    host.close().unwrap();

    let (reader, writer) = pipe();
    let original = flags(&reader);
    let mut host = PosixStandardHost::for_cli(71).unwrap();
    host.bind_stream(StandardStream::Stdin, reader.as_fd())
        .unwrap();
    let started = Instant::now();
    let error = host
        .read(&mut [0; 64], &|| {
            started.elapsed() >= Duration::from_millis(50)
        })
        .unwrap_err();
    assert_eq!(error.error.kind(), OperationalErrorKind::Cancelled);
    assert_eq!(error.confirmed_bytes, 0);
    assert_eq!(error.uncertain_bytes_upper_bound, 64);
    assert!(error.cleanup_error.is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(flags(&reader), original);
    assert!(!host.available());
    drop(writer);

    let (reader, writer) = pipe();
    let original = flags(&writer);
    assert_eq!(
        // SAFETY: only this freshly created test pipe is made nonblocking;
        // production must preserve that inherited flag throughout transfer.
        unsafe {
            libc::fcntl(
                writer.as_raw_fd(),
                libc::F_SETFL,
                original | libc::O_NONBLOCK,
            )
        },
        0
    );
    let mut writer = File::from(writer);
    loop {
        match writer.write(&[0; 4096]) {
            Ok(count) => assert!(count > 0),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            result => panic!("{result:?}"),
        }
    }
    let mut host = PosixStandardHost::for_cli(71).unwrap();
    let mut reader = File::from(reader);
    reader.read_exact(&mut [0; 4096]).unwrap();
    host.bind_stream(StandardStream::Stdout, writer.as_fd())
        .unwrap();
    let started = Instant::now();
    let count = host
        .write(StandardStream::Stdout, &payload, &|| {
            started.elapsed() >= Duration::from_secs(2)
        })
        .unwrap();
    assert!(count > 0 && count < payload.len());
    let started = Instant::now();
    let error = host
        .write(StandardStream::Stdout, b"full", &|| {
            started.elapsed() >= Duration::from_millis(50)
        })
        .unwrap_err();
    assert_eq!(error.error.kind(), OperationalErrorKind::Cancelled);
    assert_eq!(error.uncertain_bytes_upper_bound, 4);
    assert!(error.cleanup_error.is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(flags(&writer), original | libc::O_NONBLOCK);
    // The same full pipe must also cancel while blocked in write, before an ACK.
    assert_eq!(
        // SAFETY: restore only this fixture's owned pipe before binding a new host.
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, original) },
        0
    );
    let mut host = PosixStandardHost::for_cli(71).unwrap();
    host.bind_stream(StandardStream::Stdout, writer.as_fd())
        .unwrap();
    let started = Instant::now();
    let error = host
        .write(StandardStream::Stdout, b"full", &|| {
            started.elapsed() >= Duration::from_millis(50)
        })
        .unwrap_err();
    assert_eq!(error.error.kind(), OperationalErrorKind::Cancelled);
    assert_eq!(error.confirmed_bytes, 0);
    assert_eq!(error.uncertain_bytes_upper_bound, 4);
    assert!(error.cleanup_error.is_none());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(flags(&writer), original);
    super::checks::assert_reaped();
    drop(reader);
    std::fs::remove_dir_all(directory).unwrap();
}
