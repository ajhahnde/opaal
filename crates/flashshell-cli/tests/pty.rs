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

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

const FSH: &str = env!("CARGO_BIN_EXE_fsh");
const ENTER: &[u8] = b"\r";
const CTRL_C: &[u8] = b"\x03";
const CTRL_D: &[u8] = b"\x04";
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

    fn rendered(&self) -> String {
        let bytes = self.output.lock().unwrap().clone();
        strip_ansi(&String::from_utf8_lossy(&bytes))
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
