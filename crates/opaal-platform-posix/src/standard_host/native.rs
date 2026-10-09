//! Audited native launch, descriptor, signal and wait primitives.
use super::*;
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::os::fd::OwnedFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;

pub(super) fn check_parent() -> Result<(), OperationalError> {
    // SAFETY: sigaction writes one initialized, correctly sized action and
    // does not change this process's disposition when the new action is null.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &raw mut action) != 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            return Err(unsupported());
        }
    }
    #[cfg(target_os = "macos")]
    signing_identity(0)?;
    Ok(())
}

pub(super) fn protect_socket(socket: &UnixDatagram) -> Result<(), OperationalError> {
    #[cfg(target_os = "macos")]
    {
        let value: libc::c_int = 1;
        // SAFETY: the socket remains borrowed; value is a valid int of the
        // supplied size. Only this newly created control socket is modified.
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const value).cast(),
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io_error(io::Error::last_os_error()));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = socket;
    Ok(())
}

pub(super) fn spawn(
    image: &image::Image,
    socket: &UnixDatagram,
) -> Result<libc::pid_t, OperationalError> {
    check_parent()?;
    // SAFETY: fcntl duplicates a borrowed live descriptor; its successful
    // result transfers one new descriptor to OwnedFd, exactly once.
    let child_socket = unsafe {
        let fd = libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64);
        if fd < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        OwnedFd::from_raw_fd(fd)
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        // SAFETY: duplicate the live image away from all target descriptors,
        // including fd8. Both source owners survive through kernel exec.
        let executable = unsafe {
            let fd = libc::fcntl(image.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64);
            if fd < 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            OwnedFd::from_raw_fd(fd)
        };
        let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
        command
            .arg0("opaal")
            .arg(WORKER_ARGUMENT)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let fd = child_socket.as_raw_fd();
        // SAFETY: the hook captures one integer. It executes only native
        // descriptor and signal operations with stack storage; it never runs
        // allocator, source, loader inspection or worker Rust code after fork.
        unsafe {
            command.pre_exec(move || {
                // Mark all inherited descriptors close-on-exec, preserving
                // Command's error pipe and the retained executable until exec.
                if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(fd, 8) < 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut mask: libc::sigset_t = std::mem::zeroed();
                if libc::sigemptyset(&raw mut mask) != 0
                    || libc::sigprocmask(libc::SIG_SETMASK, &raw const mask, std::ptr::null_mut())
                        != 0
                {
                    return Err(io::Error::last_os_error());
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                if libc::sigemptyset(&raw mut action.sa_mask) != 0 {
                    return Err(io::Error::last_os_error());
                }
                for signal in [libc::SIGINT, libc::SIGTERM] {
                    if libc::sigaction(signal, &raw const action, std::ptr::null_mut()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(io_error)?;
        // Child::drop neither signals nor waits. The pid becomes exclusively
        // owned by the caller and is consumed only by terminate_and_reap.
        let pid = i32::try_from(child.id()).map_err(|_| unsupported())?;
        drop(child);
        Ok(pid)
    }
    #[cfg(target_os = "macos")]
    {
        signing_identity(0)?;
        let path = CString::new(image.path.as_os_str().as_bytes()).map_err(|_| unsupported())?;
        let argument = CString::new(WORKER_ARGUMENT).expect("fixed argument has no NUL");
        let argv = [
            path.as_ptr().cast_mut(),
            argument.as_ptr().cast_mut(),
            std::ptr::null_mut(),
        ];
        let environment = [std::ptr::null_mut()];
        let null = c"/dev/null";
        // SAFETY: spawn objects are initialized before use and destroyed on
        // every exit. All CString/argv/signal buffers live through spawn. The
        // child starts suspended with only explicit descriptor mappings.
        unsafe {
            let mut attributes: libc::posix_spawnattr_t = std::mem::zeroed();
            let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
            if libc::posix_spawnattr_init(&raw mut attributes) != 0 {
                return Err(unsupported());
            }
            if libc::posix_spawn_file_actions_init(&raw mut actions) != 0 {
                libc::posix_spawnattr_destroy(&raw mut attributes);
                return Err(unsupported());
            }
            let result = (|| {
                let mut mask: libc::sigset_t = std::mem::zeroed();
                let mut defaults: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut mask);
                libc::sigemptyset(&raw mut defaults);
                libc::sigaddset(&raw mut defaults, libc::SIGINT);
                libc::sigaddset(&raw mut defaults, libc::SIGTERM);
                let flags =
                    0x0080 | 0x4000 | libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF;
                if libc::posix_spawnattr_setflags(&raw mut attributes, flags as i16) != 0
                    || libc::posix_spawnattr_setsigmask(&raw mut attributes, &raw const mask) != 0
                    || libc::posix_spawnattr_setsigdefault(&raw mut attributes, &raw const defaults)
                        != 0
                {
                    return Err(unsupported());
                }
                for fd in 0..3 {
                    if libc::posix_spawn_file_actions_addopen(
                        &raw mut actions,
                        fd,
                        null.as_ptr(),
                        libc::O_RDWR,
                        0,
                    ) != 0
                    {
                        return Err(unsupported());
                    }
                }
                if libc::posix_spawn_file_actions_adddup2(
                    &raw mut actions,
                    child_socket.as_raw_fd(),
                    8,
                ) != 0
                {
                    return Err(unsupported());
                }
                let mut pid = 0;
                if libc::posix_spawn(
                    &raw mut pid,
                    path.as_ptr(),
                    &raw const actions,
                    &raw const attributes,
                    argv.as_ptr(),
                    environment.as_ptr(),
                ) != 0
                {
                    return Err(unsupported());
                }
                Ok(pid)
            })();
            libc::posix_spawn_file_actions_destroy(&raw mut actions);
            libc::posix_spawnattr_destroy(&raw mut attributes);
            result
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (image, child_socket);
        Err(unsupported())
    }
}

pub(super) fn admit(pid: libc::pid_t) -> Result<(), OperationalError> {
    #[cfg(target_os = "macos")]
    if signing_identity(0)? != signing_identity(pid)? {
        return Err(unsupported());
    }
    #[cfg(not(target_os = "macos"))]
    let _ = pid;
    Ok(())
}

#[cfg(target_os = "macos")]
fn signing_identity(pid: libc::pid_t) -> Result<[u8; 20], OperationalError> {
    // SAFETY: csops is the Darwin kernel ABI. Its two output buffers have the
    // sizes required for CS_OPS_STATUS and CS_OPS_CDHASH. pid is still owned.
    unsafe {
        unsafe extern "C" {
            fn csops(
                pid: libc::pid_t,
                operation: u32,
                output: *mut libc::c_void,
                bytes: usize,
            ) -> i32;
        }
        let mut flags = 0_u32;
        let mut hash = [0; 20];
        if csops(pid, 0, (&raw mut flags).cast(), 4) != 0
            || flags & 0x201 != 0x201
            || flags & (0x10000000 | 0x20) != 0
            || csops(pid, 5, hash.as_mut_ptr().cast(), hash.len()) != 0
        {
            return Err(unsupported());
        }
        Ok(hash)
    }
}

pub(super) fn resume(pid: libc::pid_t) -> Result<(), OperationalError> {
    #[cfg(target_os = "macos")]
    return signal(pid, libc::SIGCONT);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        Ok(())
    }
}

pub(super) fn terminate_and_reap(
    pid: libc::pid_t,
    grace: Duration,
) -> Result<(), OperationalError> {
    if wait_child(pid, true)? {
        return Ok(());
    }
    let term = signal(pid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    loop {
        if wait_child(pid, true)? {
            return term;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let kill = signal(pid, libc::SIGKILL);
    // Even a failed signal cannot abandon the owned child. Kernel wait may be
    // uninterruptible; the contract requires ownership through exact reap.
    wait_child(pid, false)?;
    term.and(kill)
}

fn signal(pid: libc::pid_t, signal: libc::c_int) -> Result<(), OperationalError> {
    // SAFETY: only the scalar pid of the exclusively owned unreaped child is
    // signalled. No process-group or unrelated recycled identity is used.
    if unsafe { libc::kill(pid, signal) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(io_error(error));
        }
    }
    Ok(())
}
fn wait_child(pid: libc::pid_t, nohang: bool) -> Result<bool, OperationalError> {
    loop {
        let mut status = 0;
        // SAFETY: waitpid is restricted to the owned child; status is one
        // valid int. A successful wait consumes this unreaped identity.
        let result =
            unsafe { libc::waitpid(pid, &raw mut status, if nohang { libc::WNOHANG } else { 0 }) };
        if result == pid {
            return Ok(true);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(io_error(error));
        }
    }
}

pub(super) fn wait(
    socket: &UnixDatagram,
    events: libc::c_short,
    interval: Duration,
) -> Result<(), OperationalError> {
    let mut descriptor = libc::pollfd {
        fd: socket.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: poll borrows one live descriptor and one correctly sized entry.
    let result = unsafe { libc::poll(&raw mut descriptor, 1, interval.as_millis() as i32) };
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
        return Err(io_error(io::Error::last_os_error()));
    }
    Ok(())
}

pub(super) fn send(socket: &UnixDatagram, bytes: &[u8]) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: bytes remains borrowed and its exact length is supplied. Linux
    // uses MSG_NOSIGNAL; Darwin's owned control socket has SO_NOSIGPIPE.
    let result = unsafe {
        libc::send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            flags,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

pub(super) fn receive(socket: &UnixDatagram, bytes: &mut [u8]) -> io::Result<usize> {
    // SAFETY: recvmsg writes only the supplied live byte/control buffers.
    // The kernel supplies aligned, bounded ancillary records. Every received
    // descriptor is closed before rejecting it; entropy lends no descriptors.
    unsafe {
        let mut control = [0_usize; 512];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = std::mem::size_of_val(&control) as _;
        let result = libc::recvmsg(socket.as_raw_fd(), &raw mut message, 0);
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ancillary = libc::CMSG_FIRSTHDR(&raw const message);
        let mut unexpected = message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0;
        while !ancillary.is_null() {
            unexpected = true;
            if (*ancillary).cmsg_level == libc::SOL_SOCKET
                && (*ancillary).cmsg_type == libc::SCM_RIGHTS
            {
                let header = libc::CMSG_LEN(0) as usize;
                // A truncated record can retain its original cmsg_len.
                let offset = ancillary as usize - control.as_ptr() as usize;
                let available = (message.msg_controllen as usize)
                    .min(std::mem::size_of_val(&control))
                    .saturating_sub(offset);
                let count = ((*ancillary).cmsg_len as usize)
                    .min(available)
                    .saturating_sub(header)
                    / std::mem::size_of::<i32>();
                for index in 0..count {
                    let fd = std::ptr::read_unaligned(
                        libc::CMSG_DATA(ancillary).cast::<i32>().add(index),
                    );
                    libc::close(fd);
                }
            }
            ancillary = libc::CMSG_NXTHDR(&raw const message, ancillary);
        }
        if unexpected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected standard host descriptors",
            ));
        }
        Ok(result as usize)
    }
}

pub(super) fn prepare_worker() -> Result<(), OperationalError> {
    // SAFETY: signal dispositions are changed only in the fresh worker, before
    // READY. The parent is never modified. sigaction has initialized storage.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_IGN;
        libc::sigemptyset(&raw mut action.sa_mask);
        if libc::sigaction(libc::SIGPIPE, &raw const action, std::ptr::null_mut()) != 0 {
            return Err(unsupported());
        }
    }
    #[cfg(target_os = "macos")]
    {
        // CoreFoundation initializes this OS encoding variable before main,
        // even when exec receives an empty environment. Remove only that fixed
        // synthetic setting in the fresh worker; every other variable refuses.
        // SAFETY: this private fresh-exec worker has not started any threads or
        // application code; no parent environment or inherited value is edited.
        if unsafe { libc::unsetenv(c"__CF_USER_TEXT_ENCODING".as_ptr()) } != 0 {
            return Err(unsupported());
        }
    }
    if std::env::vars_os().next().is_some() {
        return Err(protocol());
    }
    Ok(())
}

pub(super) fn control_socket() -> Result<UnixDatagram, OperationalError> {
    // SAFETY: the native launcher gives the worker exclusive ownership of fd8.
    // Validate a connected Unix datagram socket before adopting it once.
    unsafe {
        let mut address: libc::sockaddr_un = std::mem::zeroed();
        let mut size = std::mem::size_of_val(&address) as libc::socklen_t;
        if libc::getpeername(8, (&raw mut address).cast(), &raw mut size) != 0
            || address.sun_family != libc::AF_UNIX as libc::sa_family_t
        {
            return Err(protocol());
        }
        let mut kind = 0;
        let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
        if libc::getsockopt(
            8,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut kind).cast(),
            &raw mut length,
        ) != 0
            || kind != libc::SOCK_DGRAM
        {
            return Err(protocol());
        }
        Ok(UnixDatagram::from_raw_fd(8))
    }
}

#[cfg(target_os = "macos")]
pub(super) fn running_mach_libraries() -> Result<Vec<String>, OperationalError> {
    // SAFETY: dyld owns these NUL-terminated image names for the lifetime of
    // their mappings. The standalone CLI has no concurrent library unloading;
    // returned names are copied before the borrow ends.
    unsafe {
        unsafe extern "C" {
            fn _dyld_image_count() -> u32;
            fn _dyld_get_image_name(index: u32) -> *const libc::c_char;
        }
        let count = _dyld_image_count();
        if count == 0 || count > 4096 {
            return Err(unsupported());
        }
        let mut names = Vec::new();
        let mut bytes = 0;
        for index in 1..count {
            let pointer = _dyld_get_image_name(index);
            if pointer.is_null() {
                return Err(unsupported());
            }
            let name = std::ffi::CStr::from_ptr(pointer)
                .to_str()
                .map_err(|_| unsupported())?;
            if name.len() > 4096 {
                return Err(unsupported());
            }
            bytes += name.len();
            if bytes > 65536 {
                return Err(unsupported());
            }
            names.push(name.to_owned());
        }
        Ok(names)
    }
}

#[cfg(target_os = "macos")]
pub(super) fn running_mach_header() -> Result<Vec<u8>, OperationalError> {
    // SAFETY: dyld image zero is this running executable and remains mapped.
    // Its kernel-validated header gives the load-command size. Both slices
    // are bounded to the fixed header plus at most 64 KiB of load commands.
    unsafe {
        unsafe extern "C" {
            fn _dyld_get_image_header(index: u32) -> *const u8;
        }
        let pointer = _dyld_get_image_header(0);
        if pointer.is_null() {
            return Err(unsupported());
        }
        let header = std::slice::from_raw_parts(pointer, 32);
        if header[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
            return Err(unsupported());
        }
        let size = u32::from_le_bytes(header[20..24].try_into().expect("header size")) as usize;
        if size > 65536 {
            return Err(unsupported());
        }
        Ok(std::slice::from_raw_parts(pointer, 32 + size).to_vec())
    }
}
