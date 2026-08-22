#![cfg(any(target_os = "macos", target_os = "linux"))]

//! Pseudoterminal acceptance for the raw-mode editor.
//!
//! These drive the real fixture binary over a pty, so raw-mode acquisition,
//! escape decoding, redrawing and history all run against a real terminal
//! rather than a scripted byte slice and a `Vec<u8>`.
//!
//! The harness is a deliberate copy of the one in `tests/pty.rs` rather than a
//! shared module: that suite qualifies the reedline host editor and this one
//! qualifies the FlashOS editor, and coupling them would make either free to
//! break the other. The copy drops reedline's cursor-position responder, which
//! this editor never provokes.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

const FIXTURE: &str = env!("CARGO_BIN_EXE_flash-terminal-editor-fixture");
const TIMEOUT: Duration = Duration::from_secs(10);
static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// A live fixture session attached to a pseudoterminal.
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
    fn spawn(binary: &str) -> Self {
        Self::spawn_with_env(binary, &[])
    }

    fn spawn_with_env(binary: &str, environment: &[(&str, &str)]) -> Self {
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

        let mut command = Command::new(binary);
        command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(control_user.try_clone().expect("clone stdin")))
            .stdout(Stdio::from(control_user.try_clone().expect("clone stdout")))
            .stderr(Stdio::from(control_user.try_clone().expect("clone stderr")));
        for (name, value) in environment {
            command.env(name, value);
        }
        // Give the child its own session with the pty as controlling terminal,
        // so terminal raw mode and key events reach it. setsid and TIOCSCTTY
        // are async-signal-safe, as pre_exec requires.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid().map_err(std::io::Error::from)?;
                let stdin = rustix::fd::BorrowedFd::borrow_raw(0);
                rustix::process::ioctl_tiocsctty(stdin).map_err(std::io::Error::from)?;
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn the fixture");

        let writer = File::from(controller);
        let reader_handle = writer.try_clone().expect("clone controller");
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let reader = thread::spawn(move || {
            let mut handle = reader_handle;
            let mut buffer = [0u8; 4096];
            loop {
                match handle.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => sink.lock().unwrap().extend_from_slice(&buffer[..read]),
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

    fn resize(&self, rows: u16, columns: u16) {
        tcsetwinsize(
            self.control_user.as_ref().expect("pty user side is open"),
            winsize(rows, columns),
        )
        .expect("resize pty");
    }

    /// The current raw output length, used as a synchronization point so a
    /// later wait matches output produced *after* this call rather than
    /// something already on screen.
    fn mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    /// Block until the rendered output contains `needle`, ANSI stripped.
    fn wait_for(&self, needle: &str) -> String {
        self.wait_for_from(0, needle)
    }

    /// Block until output produced after raw offset `start` contains `needle`.
    fn wait_for_from(&self, start: usize, needle: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let text = self.rendered_from(start);
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

    /// Block until `needle` appears after `anchor` in output produced after
    /// `start`. Waiting for both in one captured snapshot matters when the pty
    /// reader receives an external notice and its redraw in separate reads.
    fn wait_for_after_from(&self, start: usize, anchor: &str, needle: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let text = self.rendered_from(start);
            if let Some(index) = text.rfind(anchor)
                && text[index + anchor.len()..].contains(needle)
            {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} after {anchor:?}; rendered since mark:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Block until `needle` has appeared at least `count` times in all output.
    fn wait_for_count(&self, needle: &str, count: usize) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let text = self.rendered_from(0);
            if text.matches(needle).count() >= count {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} occurrences of {needle:?}; rendered:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait for a prompt drawn after `mark`.
    ///
    /// The editor acquires raw mode before it draws anything, so a prompt that
    /// appears after `mark` also proves the terminal is raw again and the next
    /// keystroke will not be swallowed by the line discipline.
    fn await_prompt(&self, mark: usize) -> String {
        self.wait_for_from(mark, ">> ")
    }

    /// Wait for a prompt drawn after the last occurrence of `needle`.
    ///
    /// A byte offset cannot synchronize on this: the editor redraws the whole
    /// row on every keystroke, so prompts are scattered through the transcript
    /// and the one following an evaluation is identified by its position
    /// relative to that evaluation's output, not by when the test looked.
    fn await_prompt_after(&self, needle: &str) -> String {
        self.await_prompt_text_after(needle, ">> ")
    }

    fn await_prompt_text_after(&self, needle: &str, prompt: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let text = self.rendered_from(0);
            if let Some(index) = text.rfind(needle)
                && text[index + needle.len()..].contains(prompt)
            {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {prompt:?} after {needle:?}; rendered:\n{text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Block until the child exits, returning its exit code.
    fn wait_exit(&mut self) -> i32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("wait for the child") {
                return status.code().unwrap_or(-1);
            }
            assert!(
                Instant::now() < deadline,
                "child did not exit; rendered so far:\n{}",
                self.rendered_from(0)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn rendered_from(&self, start: usize) -> String {
        let raw = self.output.lock().unwrap().clone();
        let tail = raw.get(start..).unwrap_or(&[]);
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

/// Ask the fixture which of its two ends are terminals, giving each end a
/// different kind: stdin is the pty's user side, stdout is an ordinary pipe.
///
/// This is the only construction that can tell the two apart. Every other test
/// in this file wires all three stdio to the same pty, so an adapter that read
/// the wrong descriptor would answer `true` either way and go unnoticed.
fn report_terminal_ends() -> String {
    let controller = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open controller");
    grantpt(&controller).expect("grant");
    unlockpt(&controller).expect("unlock");
    let name = ptsname(&controller, Vec::new()).expect("ptsname");
    let user = File::from(
        open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .expect("open user side of the pty"),
    );

    let mut child = Command::new(FIXTURE)
        .arg("--report-terminal-ends")
        .stdin(Stdio::from(user.try_clone().expect("clone stdin")))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(user.try_clone().expect("clone stderr")))
        .spawn()
        .expect("spawn the fixture");

    // Read on a thread against the same deadline every other wait here uses.
    // Blocking on the pipe until EOF would turn a fixture that never reports
    // into a hanging test and an orphaned child, and a hang tells whoever runs
    // this far less than a failure does.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut report = String::new();
        let _ = stdout.read_to_string(&mut report);
        let _ = sender.send(report);
    });
    let report = match receiver.recv_timeout(TIMEOUT) {
        Ok(report) => report,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the fixture did not report its terminal ends within {TIMEOUT:?}");
        }
    };

    let _ = child.wait();
    drop((user, controller));
    report
}

#[test]
fn the_two_terminal_ends_are_answered_from_their_own_descriptors() {
    // A keyboard session with its output redirected: stdin is a terminal and
    // stdout is not. The shipped editor is selected only when both are, so an
    // adapter that answered the output end from stdin would put cursor escapes
    // into the redirect target.
    let report = report_terminal_ends();

    assert_eq!(report.trim(), "stdin=true stdout=false");
}

/// Leave the session through the fixture's own exit path.
///
/// The caller synchronizes on a freshly drawn prompt first, so `exit` is typed
/// into a raw-mode editor rather than into the line discipline.
fn exit_cleanly(pty: &mut Pty) {
    pty.send(b"exit\r");
    assert_eq!(pty.wait_exit(), 0);
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let id = UNIQUE.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "flash-terminal-editor-{tag}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create terminal-editor test directory");
    path
}

#[test]
fn backspace_edits_the_line_before_submission() {
    let mut pty = Pty::spawn(FIXTURE);
    pty.wait_for(">> ");

    // The text is sent and awaited before the erasures, and the awaited string
    // is a whole editor-drawn row. Waiting only on the submitted text would
    // prove nothing: a cooked terminal applies its own ERASE to the two 0x7f
    // bytes and hands the same edited line to an editor that did nothing, so
    // the assertion would hold with raw mode off and the backspace arm gutted.
    // A prompt with the text on the same row is drawn by this editor alone.
    pty.send(b"echo hallo");
    pty.wait_for(">> echo hallo");

    pty.send(b"\x7f\x7fx\r");

    pty.wait_for("submitted: echo halx");
    pty.await_prompt_after("submitted: echo halx");
    exit_cleanly(&mut pty);
}

#[test]
fn the_up_arrow_recalls_the_previous_submission() {
    let mut pty = Pty::spawn(FIXTURE);
    pty.wait_for(">> ");
    pty.send(b"echo one\r");
    pty.await_prompt_after("submitted: echo one");

    // Same reasoning as the backspace test: the recalled row has to be seen
    // being drawn. A cooked terminal echoes the arrow as a literal control
    // sequence and never redraws the prompt, so this wait fails unless the
    // editor really recalled the entry into its own buffer.
    let mark = pty.mark();
    pty.send(b"\x1b[A");
    pty.wait_for_from(mark, ">> echo one");

    pty.send(b"\r");

    pty.wait_for_count("submitted: echo one", 2);
    pty.await_prompt_after("submitted: echo one");
    exit_cleanly(&mut pty);
}

#[test]
fn ctrl_c_abandons_the_line_and_reprompts() {
    let mut pty = Pty::spawn(FIXTURE);
    pty.wait_for(">> ");
    pty.send(b"echo hallo");
    pty.wait_for("echo hallo");

    // The mark matters: the editor redraws the whole row on every keystroke,
    // so counting prompts over the entire transcript would already be
    // satisfied by typing. Only a prompt drawn after this point can say
    // anything about the cancel.
    let mark = pty.mark();
    pty.send(b"\x03");

    // A prompt alone is still not enough — the redraw at the top of the read
    // loop produces one whether or not the cancel took effect. The row break
    // the cancel writes before it returns is what distinguishes "abandoned the
    // line and started a new one" from "kept reading the same one".
    let reprompt = pty.await_prompt(mark);
    assert!(
        reprompt.contains('\n'),
        "the cancel ends the row before the fresh prompt; got {reprompt:?}"
    );

    // And the abandoned text is gone rather than merely off screen.
    pty.send(b"x\r");
    pty.wait_for("submitted: x");
    assert!(!pty.rendered_from(0).contains("submitted: echo hallo"));
    pty.await_prompt_after("submitted: x");
    exit_cleanly(&mut pty);
}

#[test]
fn incomplete_source_draws_the_continuation_prompt() {
    let mut pty = Pty::spawn(FIXTURE);
    pty.wait_for(">> ");

    // Raw mode is held across the whole block, so the continuation line needs
    // no resynchronization before it is typed — but each row still has to be
    // seen drawn by the editor rather than echoed by the line discipline.
    pty.send(b"if true {");
    pty.wait_for(">> if true {");
    pty.send(b"\r");

    pty.wait_for("...> ");
    let mark = pty.mark();
    pty.send(b"}");
    pty.wait_for_from(mark, "...> }");
    pty.send(b"\r");

    // The joined form matters, not just the first line: the block is submitted
    // as one source with the continuation newline the editor inserted, and a
    // prefix match would survive that newline turning into anything else.
    //
    // The `\r` is the tty's, not the editor's: evaluation runs cooked because
    // the raw-mode guard is scoped to `read_line`, so ONLCR expands the stored
    // `\n` on the way out. Scoping that guard to the whole session would make
    // this a bare `\n` and this wait would time out on correct behaviour.
    pty.wait_for("submitted: if true {\r\n}");
    pty.await_prompt_after("submitted: if true {");
    exit_cleanly(&mut pty);
}

#[test]
fn unicode_graphemes_and_multiline_movement_reach_the_real_portable_editor() {
    let mut pty = Pty::spawn(FIXTURE);
    pty.wait_for(">> ");

    pty.send("a\u{301}界".as_bytes());
    pty.send(b"\x1b[D\x7fx\r");
    pty.wait_for("submitted: x界");
    pty.await_prompt_after("submitted: x界");

    pty.send(b"if true {\recho no\x1b[A\x05 # yes\x1b[B\r}\r");
    pty.wait_for("submitted: if true { # yes\r\necho no\r\n}");
    pty.await_prompt_after("submitted: if true {");
    exit_cleanly(&mut pty);
}

#[test]
fn completion_highlighting_hints_and_resize_are_live_in_the_portable_editor() {
    let mut pty = Pty::spawn(FIXTURE);
    pty.wait_for(">> ");

    pty.send(b"pw\t");
    pty.wait_for(">> pwd ");
    pty.send(b"\r");
    pty.await_prompt_after("submitted: pwd ");

    pty.send(b"echo history-value\r");
    pty.await_prompt_after("submitted: echo history-value");
    let hint_mark = pty.mark();
    pty.send(b"echo ");
    pty.wait_for_from(hint_mark, ">> echo history-value");
    pty.send(b"\x1b[C\r");
    pty.await_prompt_after("submitted: echo history-value");

    let resize_mark = pty.mark();
    pty.send(b"abcdefghijklmnopqrstuvwxyz");
    pty.wait_for_from(resize_mark, ">> abcdefghijklmnopqrstuvwxyz");
    let redraw_mark = pty.mark();
    pty.resize(40, 12);
    pty.wait_for_from(redraw_mark, ">> stuvwxyz");
    pty.send(b"\x03");
    pty.await_prompt(redraw_mark);
    exit_cleanly(&mut pty);
}

#[test]
fn external_output_redraws_a_live_portable_edit_buffer() {
    let mut pty = Pty::spawn_with_env(FIXTURE, &[("FLASH_TEST_EXTERNAL_NOTICE", "1")]);
    pty.wait_for(">> ");
    let mark = pty.mark();
    pty.send(b"preserved-buffer");

    pty.wait_for_after_from(mark, "[1] Done     external worker", ">> preserved-buffer");

    pty.send(b"\r");
    pty.wait_for("submitted: preserved-buffer");
    pty.await_prompt_after("submitted: preserved-buffer");
    exit_cleanly(&mut pty);
}

#[test]
fn persistent_history_survives_portable_editor_processes() {
    let state = unique_dir("history");
    let state = state.to_str().expect("history state path is UTF-8");
    let environment = [
        ("FLASH_TEST_PERSISTENT_HISTORY", "1"),
        ("XDG_STATE_HOME", state),
    ];

    {
        let mut first = Pty::spawn_with_env(FIXTURE, &environment);
        first.wait_for(">> ");
        first.send(b"persisted-portable-entry\r");
        first.await_prompt_after("submitted: persisted-portable-entry");
    }

    let mut second = Pty::spawn_with_env(FIXTURE, &environment);
    second.wait_for(">> ");
    let mark = second.mark();
    second.send(b"\x1b[A");
    second.wait_for_from(mark, ">> persisted-portable-entry");
    second.send(b"\x03");
    second.await_prompt(mark);
    exit_cleanly(&mut second);
}

#[test]
fn configured_and_safe_prompts_reach_the_portable_editor() {
    let mut configured = Pty::spawn_with_env(
        FIXTURE,
        &[
            ("FLASH_TEST_PROMPT", "flash> "),
            ("FLASH_TEST_CONTINUATION_PROMPT", "more> "),
        ],
    );
    configured.wait_for("flash> ");
    configured.send(b"if true {\r");
    configured.wait_for("more> ");
    configured.send(b"}\r");
    configured.await_prompt_text_after("submitted: if true {", "flash> ");
    configured.send(b"exit\r");
    assert_eq!(configured.wait_exit(), 0);

    let mut safe = Pty::spawn_with_env(
        FIXTURE,
        &[
            ("FLASH_TEST_SAFE_MODE", "1"),
            ("FLASH_TEST_PROMPT", "must-not-render> "),
            ("FLASH_TEST_CONTINUATION_PROMPT", "must-not-render> "),
        ],
    );
    safe.wait_for("[SAFE] >> ");
    assert!(!safe.rendered_from(0).contains("must-not-render> "));
    safe.send(b"exit\r");
    assert_eq!(safe.wait_exit(), 0);
}

#[test]
fn portable_editor_restores_terminal_mode_after_each_read() {
    let mut pty = Pty::spawn_with_env(FIXTURE, &[("FLASH_TEST_TERMINAL_RESTORE", "1")]);
    pty.wait_for(">> ");
    pty.send(b"one edit\r");
    pty.wait_for("terminal-restored=true");
    assert_eq!(pty.wait_exit(), 0);
}
