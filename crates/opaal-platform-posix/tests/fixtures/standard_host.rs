use opaal_platform::standard_host::StandardHost;
use opaal_platform_posix::standard_host::{PosixStandardHost, worker_entry};

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|arg| arg == opaal_platform_posix::standard_host::WORKER_ARGUMENT)
    {
        checks::worker_boundary();
    } else {
        checks::parent_sentinels_and_mask();
    }
    if let Some(code) = worker_entry(&arguments) {
        std::process::exit(code);
    }
    let thread = std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_millis(80)));
    let mut host = PosixStandardHost::for_cli(71).expect("qualified fixture image");
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
        let started = std::time::Instant::now();
        for _ in 0..4096 {
            host.fill(&mut bytes, &|| {
                started.elapsed() >= std::time::Duration::from_secs(30)
            })
            .expect("maximum Bytes fills within one operation interval");
        }
        eprintln!(
            "maximum_bytes=1048576 fills=4096 elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }
    host.close().expect("worker reaped");
    host.close().expect("idempotent close");
    assert!(!host.available());
    assert!(host.fill(&mut bytes, &|| false).is_err());
    thread.join().unwrap();
    checks::assert_reaped();
    if arguments.get(1).is_some_and(|arg| arg == "cancel") {
        let mut host = PosixStandardHost::for_cli(72).unwrap();
        let started = std::time::Instant::now();
        #[cfg(target_os = "macos")]
        let mut blocked = [0; 251];
        #[cfg(not(target_os = "macos"))]
        let mut blocked = [0; 8];
        #[cfg(target_os = "macos")]
        let error = host
            .fill(&mut blocked, &|| {
                started.elapsed() >= std::time::Duration::from_millis(40)
            })
            .unwrap_err();
        #[cfg(not(target_os = "macos"))]
        let error = host.fill(&mut blocked, &|| true).unwrap_err();
        assert_eq!(
            error.error.kind(),
            opaal_platform::operational::OperationalErrorKind::Cancelled
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            error.progress,
            opaal_platform::standard_host::FillProgress::Uncertain
        );
        assert!(error.cleanup_error.is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        host.close().unwrap();
        checks::assert_reaped();
    }
    println!("standard host fill and reap passed");
}

#[allow(unsafe_code)]
mod checks {
    pub fn parent_sentinels_and_mask() {
        // SAFETY: this standalone test fixture owns its descriptor table and
        // changes only its own thread signal mask; no product caller is touched.
        unsafe {
            let fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            assert!(fd >= 0);
            for target in [3, 4, 5, 6, 7, 8, 9, 50, 200] {
                assert_eq!(libc::dup2(fd, target), target);
            }
            if ![3, 4, 5, 6, 7, 8, 9, 50, 200].contains(&fd) {
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
            for fd in (3..=255).filter(|fd| *fd != 8) {
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
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod blocked_library {
    /// Interpose the actual maintained macOS backend's libc call. Only the
    /// selected test length blocks; ordinary calls invoke the real OS function.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn getentropy(destination: *mut libc::c_void, count: usize) -> libc::c_int {
        if count == 251 {
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
