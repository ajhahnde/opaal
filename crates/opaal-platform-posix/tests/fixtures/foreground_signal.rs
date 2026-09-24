#![deny(unsafe_code)]

//! Process-isolated proof of scoped foreground signal forwarding.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use opaal_platform::{
    ChildProcess, JobSignal, Platform, ProcessGroup, ProcessStatus, ProcessTransition, SpawnRequest,
};
use opaal_platform_posix::PosixPlatform;

fn main() {
    if let Some(workspace) = env::var_os("OPAAL_PTY_WORKSPACE") {
        run_pty_case(Path::new(&workspace));
        return;
    }
    if let Some(mode) = env::var_os("OPAAL_SIGNAL_TARGET_MODE") {
        run_signal_target(&mode);
        return;
    }

    let report =
        PathBuf::from(env::var_os("OPAAL_SIGNAL_REPORT").expect("report path is required"));
    let observer =
        PathBuf::from(env::var_os("OPAAL_SIGNAL_OBSERVER").expect("observer path is required"));
    let workspace =
        PathBuf::from(env::var_os("OPAAL_SIGNAL_WORKSPACE").expect("workspace is required"));
    let mut findings = Vec::new();
    signal_probe::install_prior_dispositions();

    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        findings.push(run_case(&observer, &workspace, signal));
        findings.push(run_behavior_case(&workspace, signal, "caught"));
        findings.push(run_behavior_case(&workspace, signal, "ignored"));
    }
    findings.push(run_preactivation_case(&observer, &workspace));
    findings.push(run_no_signal_case(&workspace));
    signal_probe::verify_prior_dispositions();
    findings.push("prior-dispositions-restored".to_owned());
    findings.push(run_owned_group_case(&workspace, false));
    findings.push(run_owned_group_case(&workspace, true));
    findings.push(run_concurrent_release_case());

    fs::write(report, findings.join("\n")).expect("signal report should be written");
}

fn run_concurrent_release_case() -> String {
    let mut guard = PosixPlatform
        .prepare_foreground_signals()
        .expect("concurrent release prepares forwarding");
    let mut owner = PosixPlatform
        .create_process_group()
        .expect("concurrent release anchors a group");
    guard
        .forward_to(owner.id())
        .expect("concurrent release activates forwarding");
    let delivered = Arc::new(AtomicUsize::new(0));
    let progress = Arc::clone(&delivered);
    let sender = std::thread::spawn(move || {
        for _ in 0..20_000 {
            raise_probe::raise_signal(libc::SIGTERM);
            progress.fetch_add(1, Ordering::Release);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while delivered.load(Ordering::Acquire) < 100 {
        assert!(Instant::now() < deadline, "the signal sender should start");
        std::thread::yield_now();
    }
    owner
        .release()
        .expect("concurrent forwarding cannot retain a released group");
    sender
        .join()
        .expect("the concurrent signal sender completes");
    guard
        .restore()
        .expect("the original signal state returns after concurrent release");
    "concurrent-handler-release-safe".to_owned()
}

fn run_pty_case(workspace: &Path) {
    let ignored = env::var_os("OPAAL_PTY_IGNORE").is_some();
    let observer =
        PathBuf::from(env::var_os("OPAAL_SIGNAL_OBSERVER").expect("the PTY observer is required"));
    let shell = PosixPlatform
        .foreground_process_group()
        .expect("read the initial PTY owner")
        .expect("the fixture has a controlling terminal");
    let mut signals = PosixPlatform
        .prepare_foreground_signals()
        .expect("prepare the PTY signal guard");
    let mut owner = PosixPlatform
        .create_process_group()
        .expect("anchor the PTY foreground group");
    let group = owner.id();
    signals.forward_to(group).expect("activate PTY forwarding");
    let executable = if ignored {
        env::current_exe().expect("the PTY fixture path is available")
    } else {
        observer
    };
    let argv = [OsString::from("pty-observer")];
    let mut environment = vec![
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            workspace.join("pty-child-report").into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
            workspace.join("never-release-pty").into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_GROUP_REPORT"),
            workspace.as_os_str().to_os_string(),
        ),
    ];
    if ignored {
        environment.extend([
            (
                OsString::from("OPAAL_SIGNAL_TARGET_MODE"),
                OsString::from("ignored"),
            ),
            (
                OsString::from("OPAAL_SIGNAL_TARGET_NUMBER"),
                OsString::from(libc::SIGINT.to_string()),
            ),
            (
                OsString::from("OPAAL_SIGNAL_TARGET_READY"),
                workspace.join("pty-child-report").into_os_string(),
            ),
            (
                OsString::from("OPAAL_SIGNAL_TARGET_RELEASE"),
                workspace.join("never-release-pty").into_os_string(),
            ),
        ]);
    }
    let request = SpawnRequest::new(&executable, &argv, &environment, workspace)
        .expect("the PTY observer request is valid")
        .in_process_group(ProcessGroup::Join(group));
    let mut child = PosixPlatform
        .spawn(&request)
        .expect("start the PTY observer");
    assert_eq!(child.process_group(), Some(group));
    if ignored {
        fs::write(
            workspace.join("pty-observer.group"),
            group.get().to_string(),
        )
        .expect("record the ignored target group");
    }
    let mut terminal = PosixPlatform
        .enter_foreground(group)
        .expect("give the PTY to the observer group");
    assert_eq!(PosixPlatform.foreground_process_group(), Ok(Some(group)));

    let status = child
        .wait()
        .expect("reap the keyboard-interrupted observer");
    assert_eq!(
        status,
        ProcessStatus::Signaled(if ignored { libc::SIGKILL } else { libc::SIGINT })
    );
    if !ignored {
        PosixPlatform
            .signal_process_group(group, JobSignal::Kill)
            .expect("close the still-anchored group");
    }
    terminal.restore().expect("return the PTY to the shell");
    assert_eq!(PosixPlatform.foreground_process_group(), Ok(Some(shell)));
    owner
        .release()
        .expect("release the group after terminal restoration");
    signals.restore().expect("restore the parent signal state");
    fs::write(
        workspace.join("pty-result"),
        if ignored {
            b"ignored-interrupt-escalated-and-restored".as_slice()
        } else {
            b"interrupted-and-restored".as_slice()
        },
    )
    .expect("record the PTY result");
}

fn run_case(observer: &Path, workspace: &Path, signal: i32) -> String {
    let mut guard = PosixPlatform
        .prepare_foreground_signals()
        .expect("a POSIX host prepares foreground signal forwarding");
    let child_report = workspace.join(format!("child-{signal}.bin"));
    let held_until = workspace.join(format!("never-release-{signal}"));
    let argv = [OsString::from("signal-target")];
    let environment = [
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            child_report.into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
            held_until.into_os_string(),
        ),
    ];
    let request = SpawnRequest::new(observer, &argv, &environment, workspace)
        .expect("the child request is valid")
        .in_process_group(ProcessGroup::New);
    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the signal target should spawn");
    let group = child
        .process_group()
        .expect("the signal target should lead a group");
    guard
        .forward_to(group)
        .expect("the foreground group should activate");

    raise_probe::raise_signal(signal);
    let status = child.wait().expect("the forwarded child should be reaped");
    guard
        .restore()
        .expect("the foreground signal state should restore");
    assert_eq!(status, ProcessStatus::Signaled(signal));
    format!("{signal}:forwarded-and-reaped")
}

fn run_preactivation_case(observer: &Path, workspace: &Path) -> String {
    // Create this thread before preparation so it retains an unblocked mask.
    // A signal delivered there while the anchor has no user member must stay
    // pending until the first verified child joins the group.
    let (start, started) = mpsc::sync_channel::<()>(0);
    let sender = std::thread::spawn(move || {
        started.recv().expect("the startup signal is requested");
        raise_probe::raise_signal(libc::SIGTERM);
    });
    let mut guard = PosixPlatform
        .prepare_foreground_signals()
        .expect("prepare forwarding before the startup signal");
    let mut owner = PosixPlatform
        .create_process_group()
        .expect("anchor the startup group");
    start.send(()).expect("request the startup signal");
    sender.join().expect("the other thread handles the signal");

    let argv = [OsString::from("preactivation-target")];
    let environment = [
        (
            OsString::from("OPAAL_PROBE_REPORT"),
            workspace.join("preactivation-child.bin").into_os_string(),
        ),
        (
            OsString::from("OPAAL_PROBE_HOLD_UNTIL"),
            workspace
                .join("never-release-preactivation")
                .into_os_string(),
        ),
    ];
    let request = SpawnRequest::new(observer, &argv, &environment, workspace)
        .expect("the startup target request is valid")
        .in_process_group(ProcessGroup::Join(owner.id()));
    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the startup target joins the anchored group");
    assert_eq!(child.process_group(), Some(owner.id()));
    guard
        .forward_to(owner.id())
        .expect("activation forwards the pending startup signal");
    assert_eq!(
        child.wait().expect("the startup target is reaped"),
        ProcessStatus::Signaled(libc::SIGTERM)
    );
    PosixPlatform
        .signal_process_group(owner.id(), JobSignal::Kill)
        .expect("close the startup group");
    owner.release().expect("release the startup anchor");
    guard.restore().expect("restore the prior signal state");
    "preactivation-signal-forwarded".to_owned()
}

fn run_behavior_case(workspace: &Path, signal: i32, mode: &str) -> String {
    let mut guard = PosixPlatform
        .prepare_foreground_signals()
        .expect("a POSIX host prepares foreground signal forwarding");
    let executable = env::current_exe().expect("the fixture executable path is available");
    let ready = workspace.join(format!("{mode}-ready-{signal}"));
    let release = workspace.join(format!("{mode}-release-{signal}"));
    let argv = [OsString::from("signal-behavior-target")];
    let environment = [
        (
            OsString::from("OPAAL_SIGNAL_TARGET_MODE"),
            OsString::from(mode),
        ),
        (
            OsString::from("OPAAL_SIGNAL_TARGET_NUMBER"),
            OsString::from(signal.to_string()),
        ),
        (
            OsString::from("OPAAL_SIGNAL_TARGET_READY"),
            ready.clone().into_os_string(),
        ),
        (
            OsString::from("OPAAL_SIGNAL_TARGET_RELEASE"),
            release.clone().into_os_string(),
        ),
    ];
    let mut owner = PosixPlatform
        .create_process_group()
        .expect("the behavior target needs an owned group");
    let group = owner.id();
    let request = SpawnRequest::new(&executable, &argv, &environment, workspace)
        .expect("the behavior request is valid")
        .in_process_group(ProcessGroup::Join(group));
    let mut child = PosixPlatform
        .spawn(&request)
        .expect("the signal behavior target should spawn");
    assert_eq!(child.process_group(), Some(group));
    guard
        .forward_to(group)
        .expect("the foreground group should activate");
    wait_for_file(&ready, "the signal behavior target should become ready");

    let signaled_at = Instant::now();
    raise_probe::raise_signal(signal);
    let status = wait_for_completion(child.as_mut(), group);
    if mode == "ignored" {
        assert_eq!(status, ProcessStatus::Signaled(libc::SIGKILL));
        assert!(signaled_at.elapsed() >= Duration::from_millis(1800));
        assert!(signaled_at.elapsed() < Duration::from_secs(5));
    } else {
        assert_eq!(status, ProcessStatus::Exited(0));
    }
    owner.release().expect("the group anchor should be reaped");
    guard
        .restore()
        .expect("the foreground signal state should restore");
    format!("{signal}:{mode}-and-reaped")
}

fn run_no_signal_case(workspace: &Path) -> String {
    let executable = env::current_exe().expect("the fixture executable path is available");
    let ready = workspace.join("no-signal-ready");
    let release = workspace.join("no-signal-release");
    let argv = [OsString::from("signal-behavior-target")];
    let environment = [
        (
            OsString::from("OPAAL_SIGNAL_TARGET_MODE"),
            OsString::from("ignored"),
        ),
        (
            OsString::from("OPAAL_SIGNAL_TARGET_NUMBER"),
            OsString::from(libc::SIGTERM.to_string()),
        ),
        (
            OsString::from("OPAAL_SIGNAL_TARGET_READY"),
            ready.clone().into_os_string(),
        ),
        (
            OsString::from("OPAAL_SIGNAL_TARGET_RELEASE"),
            release.clone().into_os_string(),
        ),
    ];
    let mut guard = PosixPlatform
        .prepare_foreground_signals()
        .expect("prepare the no-signal group");
    let mut owner = PosixPlatform
        .create_process_group()
        .expect("own the no-signal group");
    let group = owner.id();
    let request = SpawnRequest::new(&executable, &argv, &environment, workspace)
        .expect("the no-signal request is valid")
        .in_process_group(ProcessGroup::Join(group));
    let mut child = PosixPlatform
        .spawn(&request)
        .expect("start the no-signal target");
    assert_eq!(child.process_group(), Some(group));
    guard
        .forward_to(group)
        .expect("activate no-signal forwarding");
    wait_for_file(&ready, "the no-signal target should become ready");
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(child.try_wait_for_transition(), Ok(None));
    fs::write(release, b"release").expect("the no-signal target should be released");
    assert_eq!(
        wait_for_completion(child.as_mut(), group),
        ProcessStatus::Exited(0)
    );
    owner.release().expect("reap the no-signal anchor");
    guard.restore().expect("restore the no-signal guard");
    "no-signal-has-no-deadline".to_owned()
}

fn run_signal_target(mode: &std::ffi::OsStr) {
    if mode == "descendant-parent" {
        run_descendant_parent();
        return;
    }
    if mode == "descendant-child" {
        run_descendant_child();
        return;
    }
    let signal: i32 = env::var("OPAAL_SIGNAL_TARGET_NUMBER")
        .expect("target signal is required")
        .parse()
        .expect("target signal is numeric");
    let ready = PathBuf::from(
        env::var_os("OPAAL_SIGNAL_TARGET_READY").expect("target ready path is required"),
    );
    let release = PathBuf::from(
        env::var_os("OPAAL_SIGNAL_TARGET_RELEASE").expect("target release path is required"),
    );
    signal_probe::install_target_disposition(signal, mode);
    fs::write(ready, b"ready").expect("target readiness should be writable");

    if mode == "caught" {
        let deadline = Instant::now() + Duration::from_secs(2);
        while signal_probe::caught_signal() != signal {
            assert!(
                Instant::now() < deadline,
                "the caught signal should be forwarded"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        assert_eq!(mode, "ignored");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !release.exists() {
            assert!(
                Instant::now() < deadline,
                "the ignored target should be released"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn run_owned_group_case(workspace: &Path, escalate: bool) -> String {
    let socket = workspace.join(if escalate { "d2.sock" } else { "d1.sock" });
    let listener = UnixListener::bind(&socket).expect("the descendant socket should bind");
    listener
        .set_nonblocking(true)
        .expect("the descendant socket should poll");
    let mut guard = PosixPlatform
        .prepare_foreground_signals()
        .expect("the descendant case should prepare forwarding");
    let mut owner = PosixPlatform
        .create_process_group()
        .expect("the foreground group should have a stable owner");
    let group = owner.id();
    guard
        .forward_to(group)
        .expect("the owned group should receive supported signals");
    let executable = env::current_exe().expect("the fixture executable path is available");
    let argv = [OsString::from("descendant-parent")];
    let environment = [
        (
            OsString::from("OPAAL_SIGNAL_TARGET_MODE"),
            OsString::from("descendant-parent"),
        ),
        (
            OsString::from("OPAAL_SIGNAL_SOCKET"),
            socket.into_os_string(),
        ),
    ];
    let request = SpawnRequest::new(&executable, &argv, &environment, workspace)
        .expect("the descendant parent request is valid")
        .in_process_group(ProcessGroup::Join(group));
    let mut direct_child = PosixPlatform
        .spawn(&request)
        .expect("the descendant parent should spawn");
    assert_eq!(direct_child.process_group(), Some(group));
    assert_eq!(direct_child.wait(), Ok(ProcessStatus::Exited(0)));

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "the descendant should connect");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accepting the descendant failed: {error}"),
        }
    };
    stream
        .set_nonblocking(false)
        .expect("the descendant stream should block for closure");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("the descendant stream should have a deadline");
    let mut reported_group = [0u8; 8];
    stream
        .read_exact(&mut reported_group)
        .expect("the descendant should report its group");
    assert_eq!(u64::from_le_bytes(reported_group), group.get());

    // The direct child is already reaped, while its same-group descendant
    // still holds the socket. The anchor keeps the group ID reserved here.
    let signaled_at = Instant::now();
    if escalate {
        raise_probe::raise_signal(libc::SIGTERM);
    } else {
        PosixPlatform
            .signal_process_group(group, JobSignal::Kill)
            .expect("normal completion should close the owned group");
    }
    let mut byte = [0u8; 1];
    assert_eq!(
        stream
            .read(&mut byte)
            .expect("the killed descendant should close"),
        0,
    );
    if escalate {
        assert!(signaled_at.elapsed() >= Duration::from_millis(1800));
        assert!(signaled_at.elapsed() < Duration::from_secs(5));
    }
    owner.release().expect("the group anchor should be reaped");
    owner
        .release()
        .expect("releasing the group twice is harmless");
    raise_probe::raise_signal(libc::SIGTERM);
    guard
        .restore()
        .expect("signal state should restore after the group is released");
    if escalate {
        "ignored-same-group-descendant-escalated".to_owned()
    } else {
        "owned-group-descendant-terminated".to_owned()
    }
}

// The direct child must exit without waiting: this creates the adversarial
// surviving same-group descendant that the foreground owner must close.
#[allow(clippy::zombie_processes)]
fn run_descendant_parent() {
    let executable = env::current_exe().expect("the fixture executable path is available");
    Command::new(executable)
        .env("OPAAL_SIGNAL_TARGET_MODE", "descendant-child")
        .spawn()
        .expect("the same-group descendant should spawn");
}

fn run_descendant_child() {
    signal_probe::install_target_disposition(libc::SIGTERM, std::ffi::OsStr::new("ignored"));
    let socket = env::var_os("OPAAL_SIGNAL_SOCKET").expect("the descendant socket is required");
    let mut stream =
        UnixStream::connect(PathBuf::from(socket)).expect("the descendant should connect");
    let group = descendant_group::current();
    stream
        .write_all(&group.to_le_bytes())
        .expect("the descendant should report its group");
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[allow(unsafe_code)]
mod descendant_group {
    pub(super) fn current() -> u64 {
        // SAFETY: getpgrp takes no arguments and returns the caller's group.
        u64::try_from(unsafe { libc::getpgrp() }).expect("a process group is positive")
    }
}

fn wait_for_file(path: &Path, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_completion(
    child: &mut dyn ChildProcess,
    group: opaal_platform::ProcessGroupId,
) -> ProcessStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child
            .try_wait_for_transition()
            .expect("the signal target remains observable")
        {
            Some(ProcessTransition::Completed(status)) => return status,
            Some(ProcessTransition::Stopped { .. } | ProcessTransition::Continued) | None => {}
        }
        if Instant::now() >= deadline {
            let _ = PosixPlatform.signal_process_group(group, opaal_platform::JobSignal::Kill);
            let _ = child.wait();
            panic!("the signal target should complete after forwarding");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[allow(unsafe_code)]
mod signal_probe {
    use std::ffi::{OsStr, c_int};
    use std::sync::atomic::{AtomicI32, Ordering};

    static CAUGHT: AtomicI32 = AtomicI32::new(0);
    static RESTORED: AtomicI32 = AtomicI32::new(0);

    extern "C" fn catch_target(signal: c_int) {
        CAUGHT.store(signal, Ordering::Relaxed);
    }

    extern "C" fn catch_prior(signal: c_int) {
        RESTORED.fetch_or(1 << signal, Ordering::Relaxed);
    }

    pub(super) fn install_prior_dispositions() {
        install(libc::SIGHUP, libc::SIG_IGN);
        install(libc::SIGINT, catch_prior as *const () as usize);
        install(libc::SIGTERM, catch_prior as *const () as usize);
    }

    pub(super) fn verify_prior_dispositions() {
        super::raise_probe::raise_signal(libc::SIGHUP);
        super::raise_probe::raise_signal(libc::SIGINT);
        super::raise_probe::raise_signal(libc::SIGTERM);
        let expected = (1 << libc::SIGINT) | (1 << libc::SIGTERM);
        assert_eq!(RESTORED.load(Ordering::Relaxed) & expected, expected);
    }

    pub(super) fn install_target_disposition(signal: c_int, mode: &OsStr) {
        CAUGHT.store(0, Ordering::Relaxed);
        if mode == "caught" {
            install(signal, catch_target as *const () as usize);
        } else {
            assert_eq!(mode, "ignored");
            install(signal, libc::SIG_IGN);
        }
    }

    pub(super) fn caught_signal() -> c_int {
        CAUGHT.load(Ordering::Relaxed)
    }

    fn install(signal: c_int, disposition: usize) {
        // SAFETY: the zero pattern is a valid base for sigaction; sigemptyset
        // initializes the complete mask, and `disposition` is SIG_IGN or one
        // of the two matching extern-C handlers above.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = disposition;
            assert_eq!(libc::sigemptyset(&raw mut action.sa_mask), 0);
            assert_eq!(
                libc::sigaction(signal, &raw const action, std::ptr::null_mut()),
                0,
            );
        }
    }
}

#[allow(unsafe_code)]
mod raise_probe {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn raise(signal: c_int) -> c_int;
    }

    pub(super) fn raise_signal(signal: c_int) {
        // SAFETY: raise takes one integer and dereferences nothing. The scoped
        // guard installed immediately before this call handles the signal.
        assert_eq!(unsafe { raise(signal) }, 0);
    }
}
