//! Audited native launch, descriptor, signal and wait primitives.
use super::*;
#[cfg(target_os = "macos")]
use std::ffi::CString;
use std::os::fd::{AsFd, OwnedFd};
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

pub(super) struct Endpoint {
    descriptor: OwnedFd,
    stream: StandardStream,
    identity: (libc::dev_t, libc::ino_t, libc::mode_t, i32),
    allow_terminal: bool,
}

impl Endpoint {
    pub(super) fn capture(
        fd: BorrowedFd<'_>,
        stream: StandardStream,
        allow_terminal: bool,
    ) -> Result<Self, OperationalError> {
        let descriptor = fd.try_clone_to_owned().map_err(io_error)?;
        let identity = endpoint_identity(descriptor.as_fd(), stream, allow_terminal)?;
        Ok(Self {
            descriptor,
            stream,
            identity,
            allow_terminal,
        })
    }
    pub(super) fn revalidate(&self) -> Result<(), OperationalError> {
        if endpoint_identity(self.descriptor.as_fd(), self.stream, self.allow_terminal)?
            != self.identity
        {
            return Err(unsupported());
        }
        Ok(())
    }
    pub(super) fn as_fd(&self) -> BorrowedFd<'_> {
        self.descriptor.as_fd()
    }
}

fn endpoint_identity(
    fd: BorrowedFd<'_>,
    stream: StandardStream,
    allow_terminal: bool,
) -> Result<(libc::dev_t, libc::ino_t, libc::mode_t, i32), OperationalError> {
    // SAFETY: fstat and F_GETFL inspect the borrowed live descriptor without
    // changing its shared open-file-description flags or offset.
    unsafe {
        let mut status: libc::stat = std::mem::zeroed();
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fstat(fd.as_raw_fd(), &raw mut status) != 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        let flags = stable_endpoint_flags(flags);
        let kind = status.st_mode & libc::S_IFMT;
        let mode = flags & libc::O_ACCMODE;
        let terminal =
            allow_terminal && kind == libc::S_IFCHR && crate::terminal_mode::is_terminal(fd);
        if terminal {
            terminal_attributes(fd)?;
        }
        if (!matches!(kind, libc::S_IFREG | libc::S_IFIFO) && !terminal)
            || (stream == StandardStream::Stdin && mode == libc::O_WRONLY)
            || (stream != StandardStream::Stdin && mode == libc::O_RDONLY)
        {
            return Err(unsupported());
        }
        #[cfg(target_os = "linux")]
        if flags & (libc::O_DIRECT | libc::O_PATH) != 0 {
            return Err(unsupported());
        }
        #[cfg(target_os = "macos")]
        if flags & (libc::O_EVTONLY | libc::O_SYMLINK | libc::O_EXEC | 0x0004_0000 | 0x0080_0000)
            != 0
        {
            return Err(unsupported());
        }
        Ok((status.st_dev, status.st_ino, status.st_mode, flags))
    }
}

fn stable_endpoint_flags(flags: i32) -> i32 {
    #[cfg(target_os = "macos")]
    // Darwin exposes transient GC marks and write history through F_GETFL.
    let flags = flags & !0x0001_3000; // FMARK, FDEFER, FWASWRITTEN
    flags
}

fn terminal_attributes(fd: BorrowedFd<'_>) -> Result<libc::termios, OperationalError> {
    let attributes = crate::terminal_mode::current_attributes(fd).map_err(io_error)?;
    // SAFETY: these scalar queries inspect the live terminal and this process;
    // the worker inherits the invocation's foreground process group.
    let foreground = unsafe { libc::tcgetpgrp(fd.as_raw_fd()) == libc::getpgrp() };
    if !foreground || attributes.c_lflag & libc::ICANON == 0 {
        return Err(unsupported());
    }
    Ok(attributes)
}

static TERMINAL_LENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) struct TerminalLease {
    saved: Vec<(OwnedFd, libc::termios)>,
}

impl TerminalLease {
    pub(super) fn acquire() -> Result<Self, OperationalError> {
        TERMINAL_LENT
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| unsupported())?;
        Ok(Self { saved: Vec::new() })
    }

    pub(super) fn capture(&mut self, fd: BorrowedFd<'_>) -> Result<(), OperationalError> {
        let attributes = terminal_attributes(fd)?;
        self.saved
            .push((fd.try_clone_to_owned().map_err(io_error)?, attributes));
        Ok(())
    }

    pub(super) fn restore(&mut self) -> Result<(), OperationalError> {
        let mut result = Ok(());
        for (fd, attributes) in &self.saved {
            let restored = crate::terminal_mode::apply(fd.as_fd(), attributes).map_err(io_error);
            result = result.and(restored);
        }
        if result.is_ok() {
            self.saved.clear();
        }
        result
    }
}

impl Drop for TerminalLease {
    fn drop(&mut self) {
        let _ = self.restore();
        TERMINAL_LENT.store(false, std::sync::atomic::Ordering::Release);
    }
}

static SIGNAL_LENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SIGNAL_RECEIVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SIGNAL_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const CANCELLATION_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn cancel_signal(_: libc::c_int) {
    SIGNAL_RECEIVED.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(super) struct SignalLease {
    previous: Vec<(libc::c_int, libc::sigaction)>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    generation: u64,
}

impl SignalLease {
    pub(super) fn capture() -> Result<Self, OperationalError> {
        SIGNAL_LENT
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| unsupported())?;
        SIGNAL_RECEIVED.store(false, std::sync::atomic::Ordering::SeqCst);
        let generation = match SIGNAL_GENERATION.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |id| id.checked_add(1),
        ) {
            Ok(previous) => previous + 1,
            Err(_) => {
                SIGNAL_LENT.store(false, std::sync::atomic::Ordering::Release);
                return Err(unsupported());
            }
        };
        let mut lease = Self {
            previous: Vec::new(),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            generation,
        };
        // SAFETY: each action is initialized before installation; previous
        // dispositions are saved in the owned guard, including partial setup.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = cancel_signal as *const () as usize;
            if libc::sigemptyset(&raw mut action.sa_mask) != 0 {
                return Err(io_error(io::Error::last_os_error()));
            }
            for signal in CANCELLATION_SIGNALS {
                let mut previous = std::mem::zeroed();
                if libc::sigaction(signal, &raw const action, &raw mut previous) != 0 {
                    return Err(io_error(io::Error::last_os_error()));
                }
                lease.previous.push((signal, previous));
            }
        }
        Ok(lease)
    }

    pub(super) fn cancellation(&self) -> Box<dyn Fn() -> bool + Send + Sync> {
        let flag = self.cancelled.clone();
        let generation = self.generation;
        Box::new(move || {
            if SIGNAL_GENERATION.load(std::sync::atomic::Ordering::SeqCst) == generation
                && SIGNAL_LENT.load(std::sync::atomic::Ordering::Acquire)
                && SIGNAL_RECEIVED.load(std::sync::atomic::Ordering::SeqCst)
            {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            flag.load(std::sync::atomic::Ordering::SeqCst)
        })
    }

    pub(super) fn poll(&self) {
        if SIGNAL_RECEIVED.load(std::sync::atomic::Ordering::SeqCst) {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub(super) fn restore(&mut self) -> Result<(), OperationalError> {
        self.poll();
        let mut result = Ok(());
        self.previous.retain(|(signal, previous)| {
            // SAFETY: each saved disposition came from successful sigaction;
            // restoring it does not access any released host or worker memory.
            if unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) } != 0 {
                result = Err(io_error(io::Error::last_os_error()));
                true
            } else {
                false
            }
        });
        result
    }
}

impl Drop for SignalLease {
    fn drop(&mut self) {
        let _ = self.restore();
        SIGNAL_LENT.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(super) fn stream_syscall(
    fd: BorrowedFd<'_>,
    read: bool,
    bytes: &mut [u8],
) -> io::Result<usize> {
    // SAFETY: the owned, admitted endpoint and bounded live buffer remain
    // borrowed through exactly one direct syscall; no flags are changed.
    let result = unsafe {
        if read {
            libc::read(fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len())
        } else {
            libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
        }
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

pub(super) fn wait_endpoint(fd: BorrowedFd<'_>, read: bool) -> Result<(), OperationalError> {
    loop {
        let events = if read { libc::POLLIN } else { libc::POLLOUT };
        let mut descriptor = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: only the admitted live endpoint is polled in the owned
        // worker. The parent can terminate/reap it throughout this wait.
        let result =
            unsafe { libc::poll(&raw mut descriptor, 1, POLL_INTERVAL.as_millis() as i32) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io_error(io::Error::last_os_error()));
        }
    }
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
                for signal in CANCELLATION_SIGNALS {
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
                libc::sigaddset(&raw mut defaults, libc::SIGHUP);
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
pub(super) fn wait_child(pid: libc::pid_t, nohang: bool) -> Result<bool, OperationalError> {
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
    receive_record(socket, bytes, false).map(|(count, _)| count)
}

pub(super) fn receive_endpoint(
    socket: &UnixDatagram,
    bytes: &mut [u8],
) -> io::Result<Option<OwnedFd>> {
    let (count, endpoint) = loop {
        match receive_record(socket, bytes, true) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => break result?,
        }
    };
    if count != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid standard host record",
        ));
    }
    Ok(endpoint)
}

pub(super) fn send_endpoint(
    socket: &UnixDatagram,
    bytes: &[u8],
    endpoint: BorrowedFd<'_>,
) -> io::Result<usize> {
    // SAFETY: aligned control storage contains exactly one SCM_RIGHTS int;
    // all borrowed payload/descriptor storage lives through atomic sendmsg.
    unsafe {
        let mut control = [0_usize; 4];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut message: libc::msghdr = std::mem::zeroed();
        message.msg_iov = &raw mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) as _;
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<i32>(), endpoint.as_raw_fd());
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_NOSIGNAL;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let result = libc::sendmsg(socket.as_raw_fd(), &raw const message, flags);
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
}

fn receive_record(
    socket: &UnixDatagram,
    bytes: &mut [u8],
    allow_endpoint: bool,
) -> io::Result<(usize, Option<OwnedFd>)> {
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
        let mut descriptors = Vec::new();
        let mut records = 0;
        while !ancillary.is_null() {
            records += 1;
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
                    let descriptor = OwnedFd::from_raw_fd(fd);
                    if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                        unexpected = true;
                    }
                    descriptors.push(descriptor);
                }
            } else {
                unexpected = true;
            }
            ancillary = libc::CMSG_NXTHDR(&raw const message, ancillary);
        }
        if unexpected
            || (!allow_endpoint && records != 0)
            || records > 1
            || descriptors.len() > 1
            || (records == 1 && descriptors.len() != 1)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected standard host descriptors",
            ));
        }
        Ok((result as usize, descriptors.pop()))
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

#[cfg(test)]
mod lease_tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn endpoint_identity_ignores_gc_history_but_retains_io_and_forbidden_modes() {
        for mode in [
            libc::O_WRONLY,
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_APPEND,
            libc::O_EVTONLY,
            libc::O_EXEC,
            0x0004_0000,
            0x0080_0000,
        ] {
            for bookkeeping in [0x0000_1000, 0x0000_2000, 0x0001_0000, 0x0001_3000] {
                assert_eq!(stable_endpoint_flags(mode | bookkeeping), mode);
            }
        }
    }

    #[test]
    fn terminal_and_signal_leases_refuse_overlap_and_recover_after_drop() {
        let terminal = TerminalLease::acquire().unwrap();
        assert!(TerminalLease::acquire().is_err());
        drop(terminal);
        drop(TerminalLease::acquire().unwrap());

        let signals = SignalLease::capture().unwrap();
        let previous_token = signals.cancellation();
        assert!(!previous_token());
        assert!(SignalLease::capture().is_err());
        drop(signals);
        let signals = SignalLease::capture().unwrap();
        let cancelled_token = signals.cancellation();
        cancel_signal(libc::SIGINT);
        assert!(cancelled_token());
        assert!(!previous_token());
        drop(signals);
        let signals = SignalLease::capture().unwrap();
        assert!(!signals.cancellation()());
        assert!(cancelled_token());
        assert!(!previous_token());
    }
}
