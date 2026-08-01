#![cfg(any(target_os = "macos", target_os = "linux"))]

//! Pseudoterminal acceptance coverage for the interactive `fsh` client.
//!
//! Each test drives the real built `fsh` binary over a pseudoterminal composed
//! directly from `rustix` — a controller/user pair, unlocked and sized — and a
//! reader thread that accumulates everything the shell renders. Assertions
//! observe the shell's own prompts, diagnostics, exit codes, and echoed edit
//! buffer; no host shell is used as a semantic oracle. Every session runs with
//! `--no-config` so the developer's real configuration is never consulted, and
//! history is isolated to a per-test state directory.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open};
use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

const FSH: &str = env!("CARGO_BIN_EXE_fsh");
const HANGUP_OBSERVER: &str = env!("CARGO_BIN_EXE_flashshell-e2e-hangup-observer-fixture");
const PROCESS_OBSERVER: &str = env!("CARGO_BIN_EXE_flashshell-e2e-process-observer-fixture");
const ENTER: &[u8] = b"\r";
const CTRL_C: &[u8] = b"\x03";
const CTRL_D: &[u8] = b"\x04";
const CTRL_Z: &[u8] = b"\x1a";
const UP_ARROW: &[u8] = b"\x1b[A";
const TAB: &[u8] = b"\t";
const TIMEOUT: Duration = Duration::from_secs(10);
/// A brief pause after a prompt is drawn, letting reedline finish its cursor
/// handshake before input is injected, so keystrokes are not swallowed.
const SETTLE: Duration = Duration::from_millis(150);

static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// A live `fsh` session attached to a pseudoterminal.
struct Pty {
    writer: File,
    // A retained user-side handle: on macOS the winsize ioctl targets the user
    // (slave) side, and it is closed at drop so the reader thread reaches EOF.
    control_user: Option<File>,
    output: Arc<Mutex<Vec<u8>>>,
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
}

impl Pty {
    fn spawn(args: &[&str], env: &[(&str, &str)], cwd: &Path) -> Self {
        let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open controller");
        grantpt(&controller).expect("grant");
        unlockpt(&controller).expect("unlock");
        let name = ptsname(&controller, Vec::new()).expect("ptsname");

        let control_user = File::from(
            open(
                name.as_c_str(),
                OFlags::RDWR | OFlags::NOCTTY,
                Mode::empty(),
            )
            .expect("open user side of the pty"),
        );
        tcsetwinsize(&control_user, winsize(24, 80)).expect("initial winsize");

        let mut command = Command::new(FSH);
        command
            .args(args)
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(control_user.try_clone().expect("clone stdin")))
            .stdout(Stdio::from(control_user.try_clone().expect("clone stdout")))
            .stderr(Stdio::from(control_user.try_clone().expect("clone stderr")));
        for (key, value) in env {
            command.env(key, value);
        }
        // Give the child its own session with the pty as controlling terminal,
        // so terminal raw mode, key events, and SIGWINCH all reach it. setsid
        // and TIOCSCTTY are async-signal-safe, as pre_exec requires.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid().map_err(std::io::Error::from)?;
                let stdin = rustix::fd::BorrowedFd::borrow_raw(0);
                rustix::process::ioctl_tiocsctty(stdin).map_err(std::io::Error::from)?;
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn fsh");

        let writer = File::from(controller);
        let reader_handle = writer.try_clone().expect("clone controller");
        let mut responder = writer.try_clone().expect("clone responder");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let reader = thread::spawn(move || {
            // A minimal terminal emulator: accumulate output and answer the
            // cursor-position query (DSR, ESC[6n) that reedline blocks on, since
            // no real terminal is present to reply.
            const DSR_QUERY: &[u8] = b"\x1b[6n";
            let mut handle = reader_handle;
            let mut buffer = [0u8; 4096];
            let mut tail: Vec<u8> = Vec::new();
            loop {
                match handle.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let chunk = &buffer[..read];
                        sink.lock().unwrap().extend_from_slice(chunk);

                        let mut scan = tail.clone();
                        scan.extend_from_slice(chunk);
                        let queries = scan
                            .windows(DSR_QUERY.len())
                            .filter(|window| *window == DSR_QUERY)
                            .count();
                        for _ in 0..queries {
                            let _ = responder.write_all(b"\x1b[1;1R");
                        }
                        if queries > 0 {
                            let _ = responder.flush();
                        }
                        let keep = scan.len().saturating_sub(DSR_QUERY.len() - 1);
                        tail = scan.split_off(keep);
                    }
                }
            }
        });

        Self {
            writer,
            control_user: Some(control_user),
            output,
            child,
            reader: Some(reader),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to the pty");
        self.writer.flush().expect("flush the pty");
    }

    fn resize(&self, rows: u16, cols: u16) {
        let user = self.control_user.as_ref().expect("user side is open");
        tcsetwinsize(user, winsize(rows, cols)).expect("resize");
    }

    /// Wait for a freshly drawn prompt after `mark`, then let it settle.
    fn await_prompt(&self, mark: usize) {
        self.expect_from(mark, "fsh> ");
        thread::sleep(SETTLE);
    }

    /// Block until the rendered output contains `needle`, ANSI stripped.
    fn expect(&self, needle: &str) -> String {
        self.expect_from(0, needle)
    }

    /// The current raw output length, used as a synchronization point so a later
    /// `expect_from` waits for output produced *after* this call rather than
    /// matching a prompt already on screen.
    fn mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Block until output produced after raw offset `start` contains `needle`.
    fn expect_from(&self, start: usize, needle: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let raw = self.output.lock().unwrap().clone();
            let tail = raw.get(start..).unwrap_or(&[]);
            let text = strip_ansi(&String::from_utf8_lossy(tail));
            if text.contains(needle) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}; rendered since mark:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Block until the child exits, returning its exit code.
    fn wait_code(&mut self) -> i32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait for the child") {
                return status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < deadline,
                "child did not exit; rendered so far:\n{}",
                self.rendered()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("inspect the shell process")
            .is_none()
    }

    fn rendered(&self) -> String {
        let bytes = self.output.lock().unwrap().clone();
        strip_ansi(&String::from_utf8_lossy(&bytes))
    }

    fn rendered_from(&self, start: usize) -> String {
        let bytes = self.output.lock().unwrap().clone();
        let tail = bytes.get(start..).unwrap_or(&[]);
        strip_ansi(&String::from_utf8_lossy(tail))
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Close the retained user side so the controller reader reaches EOF.
        self.control_user = None;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn winsize(rows: u16, cols: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Remove CSI/escape sequences so assertions match the visible characters.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // Operating System Command: skip to the string terminator.
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\u{7}' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(character);
        }
    }
    out
}

fn unique_dir(tag: &str) -> PathBuf {
    let id = UNIQUE.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("fsh-pty-{tag}-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create the test directory");
    path
}

fn interactive(cwd: &Path) -> Pty {
    Pty::spawn(&["--no-config", "--no-history"], &[], cwd)
}

fn await_group_reports(directory: &Path, count: usize) -> Vec<u64> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut groups: Vec<_> = fs::read_dir(directory)
            .expect("read the group report directory")
            .map(|entry| entry.expect("read one group report entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "group")
            })
            .map(|path| {
                fs::read_to_string(path)
                    .expect("read one group report")
                    .parse::<u64>()
                    .expect("the process group report is numeric")
            })
            .collect();
        if groups.len() == count {
            groups.sort_unstable();
            return groups;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} process-group reports; found {}",
            groups.len()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_one_process_group_report(directory: &Path) -> (Pid, Pid) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reports: Vec<_> = fs::read_dir(directory)
            .expect("read the group report directory")
            .map(|entry| entry.expect("read one group report entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "group")
            })
            .collect();
        if reports.len() == 1 {
            let path = &reports[0];
            let process = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<i32>().ok())
                .and_then(Pid::from_raw)
                .expect("the group report name contains a live process id");
            let group = fs::read_to_string(path)
                .expect("read the process group report")
                .parse::<i32>()
                .ok()
                .and_then(Pid::from_raw)
                .expect("the process group report contains a valid id");
            return (process, group);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for one process-group report; found {}",
            reports.len()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_process_exit(process: Pid) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match test_kill_process(process) {
            Err(rustix::io::Errno::SRCH) => return,
            Ok(()) => {}
            Err(error) => panic!("cannot inspect process {process}: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "process {} remained alive after the shell exited",
            process.as_raw_nonzero()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct ProcessGroupCleanup(Option<Pid>);

impl ProcessGroupCleanup {
    fn new(group: Pid) -> Self {
        Self(Some(group))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroupCleanup {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

struct ReleaseOnDrop(PathBuf);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"release");
    }
}

fn assert_notice_precedes_prompt(rendered: &str, notice: &str) {
    let notice_start = rendered
        .find(notice)
        .unwrap_or_else(|| panic!("missing notice {notice:?} in:\n{rendered}"));
    assert!(
        rendered[notice_start + notice.len()..].contains("fsh> "),
        "the next prompt must follow {notice:?}; rendered:\n{rendered}"
    );
}

#[test]
fn draws_the_primary_prompt_and_runs_a_command() {
    let cwd = unique_dir("prompt");
    let mut session = interactive(&cwd);
    session.expect("fsh> ");

    session.send(b"pwd");
    session.send(ENTER);
    // `pwd` prints the retained logical cwd, whose unique component is stable
    // across any /private symlink canonicalization.
    let component = cwd.file_name().unwrap().to_string_lossy().into_owned();
    session.expect(&component);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn shows_the_continuation_prompt_for_incomplete_input() {
    let cwd = unique_dir("continuation");
    let mut session = interactive(&cwd);
    session.expect("fsh> ");

    session.send(b"if true {");
    session.send(ENTER);
    session.expect("...> ");

    // Completing the block returns to the primary prompt without an error.
    session.send(b"}");
    session.send(ENTER);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn ctrl_c_cancels_the_line_and_keeps_the_session_alive() {
    let cwd = unique_dir("ctrlc");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    // A partial buffer is abandoned, not evaluated; the session stays alive.
    session.send(b"exit 99");
    session.expect("exit 99");
    let mark = session.mark();
    session.send(CTRL_C);
    session.await_prompt(mark);

    // After the cancel, a fresh command still runs against the live session.
    let mark = session.mark();
    session.send(b"pwd");
    session.send(ENTER);
    let component = cwd.file_name().unwrap().to_string_lossy().into_owned();
    session.expect_from(mark, &component);

    session.await_prompt(mark);
    session.send(b"exit 5");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 5);
}

#[test]
fn ctrl_d_on_an_empty_buffer_exits_successfully() {
    let cwd = unique_dir("ctrld");
    let mut session = interactive(&cwd);
    session.expect("fsh> ");

    session.send(CTRL_D);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn the_exit_builtin_propagates_its_status() {
    let cwd = unique_dir("exit");
    let mut session = interactive(&cwd);
    session.expect("fsh> ");

    session.send(b"exit 7");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 7);
}

#[test]
fn a_runtime_error_is_reported_and_the_session_recovers() {
    let cwd = unique_dir("recovery");
    let mut session = interactive(&cwd);
    session.expect("fsh> ");

    session.send(b"$missing");
    session.send(ENTER);
    // The recoverable diagnostic anchors on the offending source.
    session.expect("missing");

    session.send(b"exit 2");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 2);
}

#[test]
fn the_session_survives_a_terminal_resize() {
    let cwd = unique_dir("resize");
    let mut session = interactive(&cwd);
    session.expect("fsh> ");

    session.resize(40, 100);
    session.send(b"exit 3");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 3);
}

#[test]
fn persistent_history_recalls_an_earlier_submission() {
    let cwd = unique_dir("history");
    let state = unique_dir("history-state");
    let state_env = [("XDG_STATE_HOME", state.to_str().unwrap())];

    // First session records one distinctive submission. The editor syncs
    // history inside read_line before the next prompt, so once that prompt is
    // drawn the entry is persisted; the harness then tears the session down
    // without submitting a newer entry that would shadow the recall.
    {
        let mut first = Pty::spawn(&["--no-config"], &state_env, &cwd);
        first.await_prompt(0);
        first.send(b"let historymarker = 41");
        let mark = first.mark();
        first.send(ENTER);
        first.await_prompt(mark);
    }

    // A fresh session recalls it with the up arrow.
    let mut second = Pty::spawn(&["--no-config"], &state_env, &cwd);
    second.await_prompt(0);
    let mark = second.mark();
    second.send(UP_ARROW);
    // The recalled buffer proves cross-session persistence; the harness tears
    // the session down afterward.
    second.expect_from(mark, "historymarker");
}

#[test]
fn tab_completes_a_command_name() {
    let cwd = unique_dir("completion");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    session.send(b"pw");
    session.expect("pw");
    let mark = session.mark();
    session.send(TAB);
    // The completion menu surfaces the standard `pwd` command name. The session
    // is torn down by the harness rather than through a menu-active exit path.
    session.expect_from(mark, "pwd");
}

#[test]
fn an_interrupt_kills_the_job_and_leaves_the_shell_at_a_prompt() {
    let cwd = unique_dir("interrupt");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let mark = session.mark();
    session.send(b"sleep 30");
    session.send(ENTER);
    thread::sleep(SETTLE);
    // The job owns the terminal by now, so the interrupt is delivered to it and
    // must actually end it: a job that had inherited the shell's ignored
    // disposition would outlive the interrupt, and no further prompt would be
    // drawn for the remaining thirty seconds.
    session.send(CTRL_C);
    session.await_prompt(mark);

    let mark = session.mark();
    session.send(b"echo alive");
    session.send(ENTER);
    session.expect_from(mark, "alive");

    session.await_prompt(mark);
    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_terminal_stop_retains_an_addressable_job_and_returns_the_prompt() {
    let cwd = unique_dir("stop");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let mark = session.mark();
    session.send(b"sleep 1");
    session.send(ENTER);
    thread::sleep(SETTLE);
    // The exact external foreground job stops into the coordinator and returns
    // the prompt without inventing a completion. The complete job remains
    // addressable for the job built-ins and the established exit policy.
    session.send(CTRL_Z);
    session.expect_from(mark, "[1] Stopped  sleep 1");
    session.await_prompt(mark);

    let mark = session.mark();
    session.send(b"echo retained");
    session.send(ENTER);
    session.expect_from(mark, "retained");

    session.await_prompt(mark);
    let refusal_mark = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(refusal_mark, "fsh: 1 live background job");
    session.await_prompt(refusal_mark);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn job_builtins_stop_list_bg_and_fg_a_real_foreground_job() {
    let cwd = unique_dir("job-lifecycle");
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER}");
    let stop_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    let (process, group) = await_one_process_group_report(&cwd);
    let mut cleanup = ProcessGroupCleanup::new(group);

    session.send(CTRL_Z);
    session.expect_from(stop_mark, "[1] Stopped");
    session.await_prompt(stop_mark);
    let stopped = session.rendered_from(stop_mark);
    assert_eq!(
        stopped.matches("[1] Stopped").count(),
        1,
        "one terminal stop must publish one notice:\n{stopped}"
    );

    let stopped_jobs_mark = session.mark();
    session.send(b"jobs");
    session.send(ENTER);
    session.expect_from(stopped_jobs_mark, "\"job\": \"%1\"");
    session.expect_from(stopped_jobs_mark, "\"state\": \"stopped\"");
    session.await_prompt(stopped_jobs_mark);

    let bg_mark = session.mark();
    session.send(b"bg %1");
    session.send(ENTER);
    session.await_prompt(bg_mark);

    let running_jobs_mark = session.mark();
    session.send(b"jobs");
    session.send(ENTER);
    session.expect_from(running_jobs_mark, "\"state\": \"running\"");
    session.expect_from(running_jobs_mark, "\"placement\": \"background\"");
    session.await_prompt(running_jobs_mark);

    let fg_mark = session.mark();
    session.send(b"fg %1");
    session.send(ENTER);
    thread::sleep(SETTLE);
    assert!(
        test_kill_process(process).is_ok(),
        "the held fixture must still be live while fg waits"
    );
    fs::write(&release, b"complete").expect("release the foreground fixture");
    session.await_prompt(fg_mark);
    await_process_exit(process);
    cleanup.disarm();
    let foreground = session.rendered_from(fg_mark);
    assert!(
        !foreground.contains("[1] Done"),
        "fg consumes the selected terminal aggregate:\n{foreground}"
    );

    let returned_mark = session.mark();
    session.send(b"echo terminal-returned");
    session.send(ENTER);
    session.expect_from(returned_mark, "terminal-returned");
    session.await_prompt(returned_mark);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn job_builtins_wait_consumes_an_exit_and_diagnostics_are_recoverable() {
    let cwd = unique_dir("job-wait");
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);
    let (process, group) = await_one_process_group_report(&cwd);
    let mut cleanup = ProcessGroupCleanup::new(group);

    let wait_mark = session.mark();
    session.send(b"wait %1");
    session.send(ENTER);
    thread::sleep(SETTLE);
    assert!(
        test_kill_process(process).is_ok(),
        "wait must remain blocked on the held fixture"
    );
    fs::write(&release, b"complete").expect("release the waited fixture");
    session.await_prompt(wait_mark);
    await_process_exit(process);
    cleanup.disarm();
    let waited = session.rendered_from(wait_mark);
    assert!(
        !waited.contains("[1] Done"),
        "wait consumes the completion instead of publishing a duplicate notice:\n{waited}"
    );

    let removed_mark = session.mark();
    session.send(b"wait %1");
    session.send(ENTER);
    session.expect_from(removed_mark, "wait: unknown job `%1`");
    session.await_prompt(removed_mark);

    let invalid_mark = session.mark();
    session.send(b"fg %0");
    session.send(ENTER);
    session.expect_from(
        invalid_mark,
        "job identity must be a nonzero decimal number",
    );
    session.await_prompt(invalid_mark);

    let recovery_mark = session.mark();
    session.send(b"echo job-errors-are-recoverable");
    session.send(ENTER);
    session.expect_from(recovery_mark, "job-errors-are-recoverable");
    session.await_prompt(recovery_mark);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

fn assert_kill_ends_held_fixture(tag: &str, selector: Option<&str>) {
    let cwd = unique_dir(tag);
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);
    let (process, group) = await_one_process_group_report(&cwd);
    let mut cleanup = ProcessGroupCleanup::new(group);

    let kill_mark = session.mark();
    let command = selector.map_or_else(|| "kill %1".to_owned(), |flag| format!("kill {flag} %1"));
    session.send(command.as_bytes());
    session.send(ENTER);
    session.await_prompt(kill_mark);
    await_process_exit(process);
    assert!(
        !release.exists(),
        "the signal must end the fixture without its cooperative release"
    );
    cleanup.disarm();

    if !session.rendered_from(kill_mark).contains("[1] Done") {
        let completion_mark = session.mark();
        session.send(CTRL_C);
        session.expect_from(completion_mark, "[1] Done");
        session.await_prompt(completion_mark);
    }
    let completion = session.rendered_from(kill_mark);
    assert_eq!(
        completion.matches("[1] Done").count(),
        1,
        "the terminated fixture has one acknowledged completion:\n{completion}"
    );

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn job_builtins_kill_defaults_to_terminate_for_a_real_group() {
    assert_kill_ends_held_fixture("job-terminate", None);
}

#[test]
fn job_builtins_kill_kill_ends_a_real_group() {
    assert_kill_ends_held_fixture("job-kill", Some("--kill"));
}

#[test]
fn an_interrupt_aimed_at_the_shell_does_not_end_the_session() {
    let cwd = unique_dir("shell-interrupt");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    // No keystroke can exercise this: while a job runs the terminal signals the
    // job's own group, and while the editor reads it suppresses signal
    // generation altogether. Signalling the shell process directly is therefore
    // the only way to observe the shell's own disposition, and an unarranged
    // shell is killed outright by it.
    let pid = Pid::from_raw(i32::try_from(session.child.id()).expect("the child pid fits in i32"))
        .expect("a live child has a valid pid");
    kill_process(pid, Signal::INT).expect("signal the shell");

    let mark = session.mark();
    session.send(b"echo intact");
    session.send(ENTER);
    session.expect_from(mark, "intact");

    session.await_prompt(mark);
    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_background_command_returns_a_prompt_and_completes_after_foreground_work() {
    let cwd = unique_dir("background-prompt");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let launched_at = Instant::now();
    let launch_mark = session.mark();
    session.send(b"sleep 1 &");
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);
    let launch = session.rendered_from(launch_mark);
    assert!(
        launched_at.elapsed() < Duration::from_secs(1),
        "the fresh prompt must return before sleep can complete"
    );
    assert!(
        !launch.contains("[1] Done"),
        "the launch and fresh prompt must precede completion:\n{launch}"
    );

    let foreground_mark = session.mark();
    session.send(b"echo foreground-while-background-runs");
    session.send(ENTER);
    session.expect_from(foreground_mark, "foreground-while-background-runs");
    session.await_prompt(foreground_mark);
    let foreground = session.rendered_from(foreground_mark);
    assert!(
        !foreground.contains("[1] Done"),
        "foreground work should finish while the background command remains live:\n{foreground}"
    );

    let blocked_editor_mark = session.mark();
    thread::sleep(Duration::from_millis(1200).saturating_sub(launched_at.elapsed()));
    assert!(
        !session
            .rendered_from(blocked_editor_mark)
            .contains("[1] Done"),
        "completion must remain queued while the editor owns the prompt"
    );
    let completion_mark = session.mark();
    session.send(CTRL_C);
    session.expect_from(completion_mark, "[1] Done     sleep 1");
    session.expect_from(completion_mark, "fsh> ");
    let completion = session.rendered_from(completion_mark);
    assert_notice_precedes_prompt(&completion, "[1] Done     sleep 1");
    assert_eq!(completion.matches("[1] Done").count(), 1);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_background_pipeline_has_one_job_identity_and_one_completion_notice() {
    let cwd = unique_dir("background-pipeline");
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} | ^{PROCESS_OBSERVER} &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.expect_from(launch_mark, "fsh> ");
    let launch = session.rendered_from(launch_mark);
    let groups = await_group_reports(&cwd, 2);
    assert_eq!(groups[0], groups[1], "both stages share one process group");
    assert_eq!(
        launch.matches("[1] ").count(),
        1,
        "one pipeline should publish one launch notice:\n{launch}"
    );
    assert!(
        !launch.contains("[2] "),
        "pipeline members must not receive separate job identities:\n{launch}"
    );

    fs::write(&release, b"complete").expect("release both pipeline members");
    thread::sleep(SETTLE);
    let completion_mark = session.mark();
    session.send(CTRL_C);
    session.expect_from(completion_mark, "[1] Done");
    session.expect_from(completion_mark, "fsh> ");
    let completion = session.rendered_from(completion_mark);
    assert_notice_precedes_prompt(&completion, "[1] Done");
    assert_eq!(
        completion.matches("[1] Done").count(),
        1,
        "one pipeline should publish one aggregate completion:\n{completion}"
    );
    assert!(
        !completion.contains("[2] Done"),
        "pipeline members must not publish separate completions:\n{completion}"
    );

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_background_and_chain_reaches_an_internal_command_before_completion() {
    let cwd = unique_dir("background-chain-and");
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} && pwd &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);
    assert_eq!(await_group_reports(&cwd, 1).len(), 1);

    let result_mark = session.mark();
    fs::write(&release, b"complete").expect("release the left side");
    let component = cwd.file_name().unwrap().to_string_lossy().into_owned();
    session.expect_from(result_mark, &component);

    thread::sleep(SETTLE);
    let completion_mark = session.mark();
    session.send(CTRL_C);
    session.expect_from(completion_mark, "[1] Done");
    session.expect_from(completion_mark, "fsh> ");
    let completion = session.rendered_from(completion_mark);
    assert_notice_precedes_prompt(&completion, "[1] Done");

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_background_or_chain_short_circuits_before_its_internal_command() {
    let cwd = unique_dir("background-chain-or");
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} || pwd &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);
    assert_eq!(await_group_reports(&cwd, 1).len(), 1);

    let result_mark = session.mark();
    fs::write(&release, b"complete").expect("release the successful left side");
    thread::sleep(SETTLE);
    let completion_mark = session.mark();
    session.send(CTRL_C);
    session.expect_from(completion_mark, "[1] Done");
    session.expect_from(completion_mark, "fsh> ");
    let result = session.rendered_from(result_mark);
    let component = cwd.file_name().unwrap().to_string_lossy();
    assert!(
        !result.contains(component.as_ref()),
        "the successful left side must skip the internal right side:\n{result}"
    );
    let completion = session.rendered_from(completion_mark);
    assert_notice_precedes_prompt(&completion, "[1] Done");

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_background_chain_supervisor_survives_hangup_to_wait_for_its_grandchild() {
    let cwd = unique_dir("background-chain-hangup");
    let release = cwd.join("never-release");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{HANGUP_OBSERVER} && pwd &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    let (grandchild, group) = await_one_process_group_report(&cwd);
    assert_ne!(
        grandchild, group,
        "the observer must be a grandchild of the supervising group leader"
    );
    let mut cleanup = ProcessGroupCleanup::new(group);

    let refusal_mark = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(refusal_mark, "fsh: 1 live background job");
    session.expect_from(refusal_mark, "fsh: exit again to hang up");
    session.await_prompt(refusal_mark);

    session.send(b"exit 0");
    session.send(ENTER);
    thread::sleep(SETTLE);
    assert!(
        session.is_running(),
        "the supervising child must survive hang-up and wait for its grandchild"
    );

    fs::write(&release, b"complete").expect("release the ignored grandchild");
    assert_eq!(session.wait_code(), 0);
    await_process_exit(grandchild);
    cleanup.disarm();
}

#[test]
fn hanging_up_a_background_chain_ends_its_external_grandchild() {
    let cwd = unique_dir("background-chain-grandchild-hangup");
    let report = cwd.join("report.bin");
    let release = cwd.join("never-release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    let _release_on_drop = ReleaseOnDrop(release.clone());
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} && pwd &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    let (grandchild, group) = await_one_process_group_report(&cwd);
    assert_ne!(
        grandchild, group,
        "the observer must be a grandchild of the supervising group leader"
    );
    let mut cleanup = ProcessGroupCleanup::new(group);

    let refusal_mark = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(refusal_mark, "fsh: 1 live background job");
    session.expect_from(refusal_mark, "fsh: exit again to hang up");
    session.await_prompt(refusal_mark);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
    await_process_exit(grandchild);
    cleanup.disarm();
}

#[test]
fn an_external_grandchild_restores_the_default_hangup_disposition() {
    let cwd = unique_dir("background-chain-default-hangup");
    let report = cwd.join("report.bin");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_RAISE", "1"),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} && pwd &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    let (grandchild, group) = await_one_process_group_report(&cwd);
    assert_ne!(
        grandchild, group,
        "the observer must be a grandchild of the supervising group leader"
    );
    let mut cleanup = ProcessGroupCleanup::new(group);

    thread::sleep(SETTLE);
    let completion_mark = session.mark();
    session.send(CTRL_C);
    session.expect_from(completion_mark, "[1] Done");
    session.expect_from(completion_mark, "fsh> ");
    let survived = cwd.join(format!("{}.survived", grandchild.as_raw_nonzero()));
    assert!(
        !survived.exists(),
        "an external grandchild must not inherit the supervisor's ignored hang-up"
    );
    await_process_exit(grandchild);
    cleanup.disarm();

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn a_completion_notice_never_appears_inside_an_active_edit_buffer() {
    let cwd = unique_dir("background-buffer");
    let report = cwd.join("report.bin");
    let release = cwd.join("release");
    let report_text = report.to_str().expect("test report path is UTF-8");
    let release_text = release.to_str().expect("test release path is UTF-8");
    let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
    let environment = [
        ("FLASH_PROBE_REPORT", report_text),
        ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ("FLASH_PROBE_HOLD_UNTIL", release_text),
    ];
    let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
    session.await_prompt(0);

    let command = format!("^{PROCESS_OBSERVER} &");
    let launch_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);
    let groups = await_group_reports(&cwd, 1);
    assert_ne!(groups[0], 0);

    let buffer_mark = session.mark();
    session.send(b"echo distinctive-active-buffer");
    session.expect_from(buffer_mark, "distinctive-active-buffer");
    fs::write(&release, b"complete").expect("release the background child");
    thread::sleep(SETTLE);
    let while_editing = session.rendered_from(buffer_mark);
    assert!(
        !while_editing.contains("[1] Done"),
        "a shell notice must not corrupt the active edit buffer:\n{while_editing}"
    );

    let completion_mark = session.mark();
    session.send(CTRL_C);
    session.expect_from(completion_mark, "[1] Done");
    session.expect_from(completion_mark, "fsh> ");
    let completion = session.rendered_from(completion_mark);
    assert_notice_precedes_prompt(&completion, "[1] Done");
    assert_eq!(completion.matches("[1] Done").count(), 1);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn exiting_with_a_live_job_is_refused_once_then_hangs_up() {
    let cwd = unique_dir("exit-refusal");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let launch_mark = session.mark();
    session.send(b"sleep 30 &");
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    let refusal_mark = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(refusal_mark, "fsh: 1 live background job");
    session.expect_from(refusal_mark, "[1] Running  sleep 30");
    session.expect_from(refusal_mark, "fsh: exit again to hang up");
    session.await_prompt(refusal_mark);

    let exit_mark = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
    let rendered = session.rendered_from(exit_mark);
    assert_eq!(
        rendered.matches("live background job").count(),
        0,
        "the second attempt must not warn again:\n{rendered}"
    );
}

#[test]
fn a_submission_between_two_exit_attempts_resets_the_refusal() {
    let cwd = unique_dir("exit-reset");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let launch_mark = session.mark();
    session.send(b"sleep 30 &");
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    let first = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(first, "fsh: exit again to hang up");
    session.await_prompt(first);

    let between = session.mark();
    session.send(b"echo still-here");
    session.send(ENTER);
    session.expect_from(between, "still-here");
    session.await_prompt(between);

    let second = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(second, "fsh: 1 live background job");
    session.await_prompt(second);

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn end_of_input_is_gated_exactly_like_the_exit_builtin() {
    let cwd = unique_dir("exit-eof");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let launch_mark = session.mark();
    session.send(b"sleep 30 &");
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    let refusal_mark = session.mark();
    session.send(CTRL_D);
    session.expect_from(refusal_mark, "fsh: 1 live background job");
    session.await_prompt(refusal_mark);

    session.send(CTRL_D);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn an_unacknowledged_completion_is_rendered_before_exit() {
    let cwd = unique_dir("exit-completion");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let launch_mark = session.mark();
    session.send(b"sleep 1 &");
    session.send(ENTER);
    session.expect_from(launch_mark, "[1] ");
    session.await_prompt(launch_mark);

    thread::sleep(Duration::from_millis(1400));
    let exit_mark = session.mark();
    session.send(b"exit 0");
    session.send(ENTER);
    session.expect_from(exit_mark, "[1] Done     sleep 1");
    assert_eq!(session.wait_code(), 0);
}
