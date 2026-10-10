use opaal_platform::standard_host::StandardHost;
use opaal_platform_posix::standard_host::{PosixStandardHost, worker_entry};

mod standard_host_faults;
mod standard_streams;

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|arg| arg == opaal_platform_posix::standard_host::WORKER_ARGUMENT)
    {
        checks::worker_boundary();
        standard_host_faults::worker();
    } else {
        checks::parent_sentinels_and_mask();
    }
    if let Some(code) = worker_entry(&arguments) {
        std::process::exit(code);
    }
    if arguments.get(1).is_some_and(|arg| arg == "streams") {
        standard_streams::run();
        checks::assert_reaped();
        println!("standard streams bytes, failures and reap passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "stream-protocol") {
        standard_host_faults::stream_parent();
        println!("standard stream progress, acknowledgement and reap passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "protocol") {
        standard_host_faults::parent();
        println!("standard host protocol refusal and reap passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "worker-input") {
        standard_host_faults::worker_inputs(None);
        standard_host_faults::valid_worker_inputs();
        println!("standard host worker input refusal and reap passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "protocol-fuzz") {
        use std::io::Read;
        let mut input = Vec::new();
        std::io::stdin().take(4097).read_to_end(&mut input).unwrap();
        assert!(input.len() <= 4096);
        standard_host_faults::worker_inputs(Some(&input));
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "loader") {
        checks::load_library(&arguments[2]);
        assert!(PosixStandardHost::for_cli(71).is_err());
        checks::assert_reaped();
        println!("standard host live loader refusal passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "policy") {
        #[cfg(target_os = "macos")]
        checks::mark_debugged();
        assert!(PosixStandardHost::for_cli(71).is_err());
        checks::assert_reaped();
        println!("standard host signing policy refusal passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "race") {
        let executable = std::env::current_exe().unwrap();
        let original = executable.with_extension("original");
        std::fs::hard_link(&executable, &original).unwrap();
        let replacement = std::path::PathBuf::from(&arguments[2]);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let racing = stop.clone();
        let swaps = std::thread::spawn(move || {
            let temporary = executable.with_extension("swap");
            let mut swaps = 0;
            while !racing.load(std::sync::atomic::Ordering::Relaxed) {
                for template in [&replacement, &original] {
                    std::fs::hard_link(template, &temporary).unwrap();
                    std::fs::rename(&temporary, &executable).unwrap();
                    swaps += 1;
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            }
            swaps
        });
        for _ in 0..32 {
            let mut host = PosixStandardHost::for_cli(71).unwrap();
            let started = std::time::Instant::now();
            let result = host.fill(&mut [0; 8], &|| {
                started.elapsed() >= std::time::Duration::from_secs(5)
            });
            #[cfg(target_os = "linux")]
            result.expect("retained image survives racing pathname replacements");
            #[cfg(target_os = "macos")]
            if let Err(error) = result {
                assert_eq!(
                    error.progress,
                    opaal_platform::standard_host::FillProgress::NotStarted
                );
                assert!(error.cleanup_error.is_none());
            }
            host.close().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(swaps.join().unwrap() > 1);
        checks::assert_failed_spawn_released();
        println!("standard host racing image admission and reap passed");
        return;
    }
    let thread = std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_millis(80)));
    let evaluation = if arguments.get(1).is_some_and(|arg| arg == "failure") {
        73
    } else {
        71
    };
    let mut host = PosixStandardHost::for_cli(evaluation).expect("qualified fixture image");
    if arguments.get(1).is_some_and(|arg| arg == "failure") {
        let mut bytes = [0xa5; 250];
        let error = host.fill(&mut bytes, &|| false).unwrap_err();
        assert_eq!(
            error.progress,
            opaal_platform::standard_host::FillProgress::Uncertain
        );
        assert_eq!(
            error.error.kind(),
            opaal_platform::operational::OperationalErrorKind::Io(std::io::ErrorKind::Other)
        );
        assert_eq!(bytes, [0; 250]);
        assert!(error.cleanup_error.is_none());
        assert!(!host.available());
        host.close().unwrap();
        checks::assert_reaped();
        thread.join().unwrap();
        println!("standard host failed fill and reap passed");
        return;
    }
    if arguments
        .get(1)
        .is_some_and(|arg| arg == "replace" || arg == "delete")
    {
        let executable = std::env::current_exe().unwrap();
        std::fs::remove_file(&executable).unwrap();
        if arguments[1] == "replace" {
            std::fs::copy("/usr/bin/false", &executable).unwrap();
        }
        let result = host.fill(&mut [0; 8], &|| false);
        #[cfg(target_os = "linux")]
        result.expect("retained running image survives replacement or deletion");
        #[cfg(target_os = "macos")]
        {
            let error = result.unwrap_err();
            assert_eq!(
                error.progress,
                opaal_platform::standard_host::FillProgress::NotStarted
            );
            assert!(error.cleanup_error.is_none());
        }
        host.close().unwrap();
        if arguments[1] == "delete" && cfg!(target_os = "macos") {
            checks::assert_failed_spawn_released();
        } else {
            checks::assert_reaped();
        }
        thread.join().unwrap();
        println!("standard host image identity and reap passed");
        return;
    }
    if arguments.get(1).is_some_and(|arg| arg == "availability") {
        checks::set_auto_reaping(true);
        assert!(!host.available());
        let error = host.fill(&mut [0; 8], &|| false).unwrap_err();
        assert_eq!(
            error.progress,
            opaal_platform::standard_host::FillProgress::NotStarted
        );
        assert_eq!(
            error.error.kind(),
            opaal_platform::operational::OperationalErrorKind::Unsupported
        );
        checks::set_auto_reaping(false);
        checks::assert_reaped();
        thread.join().unwrap();
        println!("standard host availability refusal passed");
        return;
    }
    let mut bytes = [0; 256];
    for _ in 0..4 {
        host.fill(&mut bytes, &|| false)
            .expect("system entropy fill");
    }
    if arguments.get(1).is_some_and(|arg| arg == "maximum") {
        let direct_started = std::time::Instant::now();
        for _ in 0..4096 {
            getrandom::fill(&mut bytes).unwrap();
        }
        let direct_nanos = direct_started.elapsed().as_nanos();
        let started = std::time::Instant::now();
        for _ in 0..4096 {
            host.fill(&mut bytes, &|| {
                started.elapsed() >= std::time::Duration::from_secs(30)
            })
            .expect("maximum Bytes fills within one operation interval");
        }
        eprintln!(
            "maximum_bytes=1048576 fills=4096 elapsed_ms={} worker_ns={} direct_ns={direct_nanos}",
            started.elapsed().as_millis(),
            started.elapsed().as_nanos()
        );
    }
    host.close().expect("worker reaped");
    if arguments.get(1).is_some_and(|arg| arg == "maximum") {
        let (parent, worker) = checks::peak_rss_bytes();
        eprintln!("parent_peak_rss_bytes={parent} worker_peak_rss_bytes={worker}");
    }
    host.close().expect("idempotent close");
    assert!(!host.available());
    assert!(host.fill(&mut bytes, &|| false).is_err());
    thread.join().unwrap();
    checks::assert_reaped();
    if arguments
        .get(1)
        .is_some_and(|arg| arg == "cancel" || arg == "interrupt" || arg == "retry")
    {
        let interrupter = arguments
            .get(1)
            .filter(|arg| *arg == "interrupt")
            .map(|_| checks::interrupt_wait());
        let retry = arguments.get(1).is_some_and(|arg| arg == "retry");
        let mut host = PosixStandardHost::for_cli(if retry { 74 } else { 72 }).unwrap();
        host.fill(&mut [0; 8], &|| false).unwrap();
        let started = std::time::Instant::now();
        let mut blocked = vec![0; if retry { 252 } else { 251 }];
        let error = host
            .fill(&mut blocked, &|| {
                started.elapsed() >= std::time::Duration::from_millis(40)
            })
            .unwrap_err();
        assert_eq!(
            error.error.kind(),
            opaal_platform::operational::OperationalErrorKind::Cancelled
        );
        assert_eq!(
            error.progress,
            opaal_platform::standard_host::FillProgress::Uncertain
        );
        assert!(error.cleanup_error.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        host.close().unwrap();
        checks::assert_reaped();
        if let Some(interrupter) = interrupter {
            interrupter.join().unwrap();
            checks::assert_interrupted();
        }
    }
    println!("standard host fill and reap passed");
}

#[allow(unsafe_code)]
pub(crate) mod checks {
    pub fn peak_rss_bytes() -> (u64, u64) {
        // SAFETY: the isolated fixture supplies exact rusage buffers; its only
        // reaped child is the entropy worker, so CHILDREN measures that owner.
        unsafe {
            let mut parent: libc::rusage = std::mem::zeroed();
            let mut worker: libc::rusage = std::mem::zeroed();
            assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &raw mut parent), 0);
            assert_eq!(libc::getrusage(libc::RUSAGE_CHILDREN, &raw mut worker), 0);
            let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
            (
                parent.ru_maxrss as u64 * unit,
                worker.ru_maxrss as u64 * unit,
            )
        }
    }
    #[cfg(target_os = "macos")]
    pub fn mark_debugged() {
        // SAFETY: this isolated fixture asks its own parent to trace it; no
        // other process or signing state is modified.
        let result = unsafe { libc::ptrace(libc::PT_TRACE_ME, 0, std::ptr::null_mut(), 0) };
        assert_eq!(result, 0);
    }
    static ANCILLARY_TRUNCATED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static TRUNCATE_ANCILLARY: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static CANCEL_AFTER_RECORD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static RECORD_CANCELLED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static CANCEL_POLLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    pub fn cancel_after_record(evaluation: u64) {
        RECORD_CANCELLED.store(false, std::sync::atomic::Ordering::Relaxed);
        CANCEL_POLLS.store(0, std::sync::atomic::Ordering::Relaxed);
        CANCEL_AFTER_RECORD.store(evaluation, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn record_cancelled() -> bool {
        // Let the received record pass its trailing poll; cancel the next ACK poll.
        RECORD_CANCELLED.load(std::sync::atomic::Ordering::Relaxed)
            && CANCEL_POLLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn recvmsg(
        fd: libc::c_int,
        message: *mut libc::msghdr,
        flags: libc::c_int,
    ) -> libc::ssize_t {
        // SAFETY: RTLD_NEXT resolves the platform recvmsg ABI; the production
        // caller owns its live buffers. Linux also tests a smaller ancillary
        // capacity; Darwin requires full capacity to retain every installed fd.
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_NEXT, c"recvmsg".as_ptr());
            if symbol.is_null() {
                std::process::abort();
            }
            let original: unsafe extern "C" fn(
                libc::c_int,
                *mut libc::msghdr,
                libc::c_int,
            ) -> libc::ssize_t = std::mem::transmute(symbol);
            if TRUNCATE_ANCILLARY.load(std::sync::atomic::Ordering::Relaxed) {
                (*message).msg_controllen = (*message).msg_controllen.min(24);
            }
            let result = original(fd, message, flags);
            if result >= 0 && (*message).msg_flags & libc::MSG_CTRUNC != 0 {
                ANCILLARY_TRUNCATED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let evaluation = CANCEL_AFTER_RECORD.load(std::sync::atomic::Ordering::Relaxed);
            if result > 0 && evaluation != 0 && (*message).msg_iovlen == 1 {
                let iov = &*(*message).msg_iov;
                let bytes = std::slice::from_raw_parts(
                    iov.iov_base.cast::<u8>(),
                    (result as usize).min(iov.iov_len),
                );
                if (evaluation == 146 && bytes.len() == 3)
                    || (matches!(evaluation, 145 | 147)
                        && bytes.len() == 36
                        && &bytes[..8] == b"OPAALSH\0"
                        && bytes[10..12] == 10_u16.to_be_bytes()
                        && bytes[12..20] == evaluation.to_be_bytes())
                {
                    RECORD_CANCELLED.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            result
        }
    }

    pub fn truncate_ancillary(enabled: bool) {
        TRUNCATE_ANCILLARY.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn assert_ancillary_truncated() {
        assert!(ANCILLARY_TRUNCATED.load(std::sync::atomic::Ordering::Relaxed));
    }

    pub fn load_library(path: &std::ffi::OsStr) {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_bytes()).unwrap();
        // SAFETY: this standalone fixture loads only its compiled inert library;
        // it deliberately keeps that mapping alive through host admission.
        let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(!library.is_null());
    }
    static INTERRUPTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    extern "C" fn interrupted(_: libc::c_int) {
        INTERRUPTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn interrupt_wait() -> std::thread::JoinHandle<()> {
        // SAFETY: only this isolated fixture installs a signal handler; it uses
        // one lock-free native atomic and targets the still-live parent thread.
        let thread = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interrupted as *const () as usize;
            libc::sigemptyset(&raw mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR1, &raw const action, std::ptr::null_mut()),
                0
            );
            libc::pthread_self()
        };
        std::thread::spawn(move || {
            for _ in 0..100 {
                // SAFETY: the main thread remains alive until this thread joins.
                assert_eq!(unsafe { libc::pthread_kill(thread, libc::SIGUSR1) }, 0);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    }
    pub fn assert_interrupted() {
        assert!(INTERRUPTIONS.load(std::sync::atomic::Ordering::Relaxed) >= 10);
    }
    pub fn parent_sentinels_and_mask() {
        // SAFETY: this standalone test fixture owns its descriptor table and
        // changes only its own thread signal mask; no product caller is touched.
        unsafe {
            let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            assert!(fd >= 0);
            for target in [3, 4, 5, 6, 7, 8, 9, 50, 200, 900] {
                assert_eq!(libc::dup2(fd, target), target);
            }
            if ![3, 4, 5, 6, 7, 8, 9, 50, 200, 900].contains(&fd) {
                libc::close(fd);
            }
            let mut mask: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut mask);
            libc::sigaddset(&raw mut mask, libc::SIGTERM);
            libc::sigaddset(&raw mut mask, libc::SIGINT);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &raw const mask, std::ptr::null_mut()),
                0
            );
        }
    }
    pub fn worker_boundary() {
        // SAFETY: fcntl/signal queries do not mutate descriptor or signal state;
        // all output buffers have the native ABI's exact size.
        unsafe {
            for fd in (3..=1023).filter(|fd| *fd != 8) {
                assert_eq!(libc::fcntl(fd, libc::F_GETFD), -1, "unexpected fd {fd}");
            }
            let mut mask: libc::sigset_t = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &raw mut mask),
                0
            );
            for signal in [libc::SIGTERM, libc::SIGINT] {
                assert_eq!(libc::sigismember(&raw const mask, signal), 0);
                let mut action: libc::sigaction = std::mem::zeroed();
                assert_eq!(
                    libc::sigaction(signal, std::ptr::null(), &raw mut action),
                    0
                );
                assert_eq!(action.sa_sigaction, libc::SIG_DFL);
            }
            let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            let mut expected: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd, &raw mut expected), 0);
            for fd in 0..3 {
                let mut actual: libc::stat = std::mem::zeroed();
                assert_eq!(libc::fstat(fd, &raw mut actual), 0);
                assert_eq!(
                    (actual.st_dev, actual.st_ino, actual.st_rdev),
                    (expected.st_dev, expected.st_ino, expected.st_rdev)
                );
            }
            libc::close(fd);
        }
        assert!(std::env::vars_os().next().is_none());
    }
    pub fn set_auto_reaping(enabled: bool) {
        // SAFETY: this isolated fixture has no live children when changing its
        // own SIGCHLD disposition; it restores the original default afterward.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = if enabled {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            };
            libc::sigemptyset(&raw mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, &raw const action, std::ptr::null_mut()),
                0
            );
        }
    }
    pub fn assert_reaped() {
        let mut status = 0;
        // SAFETY: the fixture has closed its only child owners. This assertion
        // asks whether any unconsumed child remains after explicit teardown.
        let result = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    pub fn assert_failed_spawn_released() {
        let started = std::time::Instant::now();
        loop {
            let mut status = 0;
            // SAFETY: this fixture has no other children. Darwin internally
            // reaps failed spawn tasks without returning their PID to callers.
            let result = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
            if result == -1 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                return;
            }
            assert_eq!(result, 0, "failed spawn left a caller-reapable child");
            assert!(started.elapsed() < std::time::Duration::from_secs(1));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) mod blocked_library {
    static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub fn enable(evaluation: u64) {
        ENABLED.store(
            matches!(evaluation, 72 | 73),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    /// Interpose the actual maintained macOS backend's libc call. Only the
    /// selected test length blocks; ordinary calls invoke the real OS function.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn getentropy(destination: *mut libc::c_void, count: usize) -> libc::c_int {
        if ENABLED.load(std::sync::atomic::Ordering::Relaxed) && count == 250 {
            // SAFETY: the maintained caller supplied this live writable buffer;
            // errno is thread-local. A failed backend must expose no partial fill.
            unsafe {
                std::ptr::write_bytes(destination.cast::<u8>(), 0x47, count);
                *libc::__error() = libc::EIO;
            }
            return -1;
        }
        if ENABLED.load(std::sync::atomic::Ordering::Relaxed) && count == 251 {
            loop {
                // SAFETY: pause takes no arguments and waits for a signal;
                // worker TERM/INT have their native default disposition.
                unsafe {
                    libc::pause();
                }
            }
        }
        // SAFETY: RTLD_NEXT retrieves the OS implementation with this exact
        // libc ABI. The maintained caller supplies its writable buffer/count.
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_NEXT, c"getentropy".as_ptr());
            if symbol.is_null() {
                std::process::abort();
            }
            let original: unsafe extern "C" fn(*mut libc::c_void, usize) -> libc::c_int =
                std::mem::transmute(symbol);
            original(destination, count)
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) mod blocked_library {
    use std::os::fd::{FromRawFd, OwnedFd};
    static LISTENER: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();

    pub fn enable(evaluation: u64) {
        let (count, action, flags) = match evaluation {
            72 => (
                251,
                libc::SECCOMP_RET_USER_NOTIF,
                libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
            ),
            73 => (250, libc::SECCOMP_RET_ERRNO | libc::EIO as u32, 0),
            74 => (252, libc::SECCOMP_RET_ERRNO | libc::EINTR as u32, 0),
            _ => return,
        };
        let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
        let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        let equal = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        let ret = (libc::BPF_RET | libc::BPF_K) as u16;
        // seccomp_data.nr and the low word of args[1] on the supported x86_64 host.
        let mut instructions = [
            instruction(load, 0, 0, 0),
            instruction(equal, 0, 3, libc::SYS_getrandom as u32),
            instruction(load, 0, 0, 24),
            instruction(equal, 0, 1, count),
            instruction(ret, 0, 0, action),
            instruction(ret, 0, 0, libc::SECCOMP_RET_ALLOW),
        ];
        let program = libc::sock_fprog {
            len: instructions.len() as u16,
            filter: instructions.as_mut_ptr(),
        };
        // SAFETY: only the fresh fixture worker installs this filter. The exact
        // BPF buffers stay live through seccomp; ordinary calls reach the OS.
        let descriptor = unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                flags,
                &raw const program,
            )
        };
        assert!(descriptor >= 0, "seccomp fixture admission failed");
        if flags != 0 {
            // SAFETY: NEW_LISTENER transferred this descriptor to the worker;
            // leaving it unread blocks the maintained call until owned teardown.
            LISTENER
                .set(unsafe { OwnedFd::from_raw_fd(descriptor as i32) })
                .unwrap();
        }
    }
}
