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
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags, open};
use rustix::process::{Pid, Signal, kill_process, kill_process_group, test_kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{Winsize, tcgetpgrp, tcsetwinsize};

const FSH: &str = env!("CARGO_BIN_EXE_fsh");
const HANGUP_OBSERVER: &str = env!("CARGO_BIN_EXE_flash-e2e-hangup-observer-fixture");
const PROCESS_OBSERVER: &str = env!("CARGO_BIN_EXE_flash-e2e-process-observer-fixture");
const STATUS_FIXTURE: &str = env!("CARGO_BIN_EXE_flash-e2e-status-fixture");
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

    fn shell_group(&self) -> Pid {
        let process = i32::try_from(self.child.id()).expect("the shell pid fits in i32");
        Pid::from_raw(process).expect("a live shell has a nonzero process group")
    }

    fn await_terminal_owner(&self, expected: Pid, context: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let observed = tcgetpgrp(&self.writer).expect("query the pty foreground process group");
            if observed == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "terminal owner did not become {expected} during {context}; \
                 last owner was {observed}; rendered so far:\n{}",
                self.rendered()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait for a freshly drawn prompt after `mark`, then let it settle.
    fn await_prompt(&self, mark: usize) {
        self.expect_from(mark, ">> ");
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

fn configured(cwd: &Path, source: &str, extra_env: &[(&str, &str)]) -> Pty {
    let home = unique_dir("config-home");
    let config_root = home.join(".config");
    let state_root = home.join(".local/state");
    let config_directory = config_root.join("flash");
    fs::create_dir_all(&config_directory).expect("create config directory");
    let mut directory_permissions = fs::metadata(&config_directory).unwrap().permissions();
    directory_permissions.set_mode(0o700);
    fs::set_permissions(&config_directory, directory_permissions)
        .expect("secure config and history directory");
    let config = config_directory.join("config.fsh");
    fs::write(&config, source).expect("write startup config");
    let mut permissions = fs::metadata(&config).unwrap().permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&config, permissions).expect("secure startup config");
    let home = home.to_str().expect("config home is UTF-8");
    let config_root = config_root.to_str().expect("config root is UTF-8");
    let state_root = state_root.to_str().expect("state root is UTF-8");
    // Runner/user XDG overrides must not redirect config or history outside
    // this fixture. A test-specific override in `extra_env` still wins below.
    let mut environment = vec![
        ("HOME", home),
        ("XDG_CONFIG_HOME", config_root),
        ("XDG_STATE_HOME", state_root),
    ];
    environment.extend_from_slice(extra_env);
    Pty::spawn(&[], &environment, cwd)
}

fn await_process_group_reports(directory: &Path, count: usize) -> Vec<(Pid, Pid)> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut reports: Vec<_> = fs::read_dir(directory)
            .expect("read the group report directory")
            .map(|entry| entry.expect("read one group report entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "group")
            })
            .map(|path| {
                let process = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.parse::<i32>().ok())
                    .and_then(Pid::from_raw)
                    .expect("the group report name contains a live process id");
                let group = fs::read_to_string(path)
                    .expect("read one group report")
                    .parse::<i32>()
                    .ok()
                    .and_then(Pid::from_raw)
                    .expect("the process group report contains a valid id");
                (process, group)
            })
            .collect();
        if reports.len() == count {
            reports.sort_unstable_by_key(|(process, _)| process.as_raw_nonzero());
            return reports;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} process-group reports in {}; found {}",
            directory.display(),
            reports.len()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn await_group_reports(directory: &Path, count: usize) -> Vec<u64> {
    await_process_group_reports(directory, count)
        .into_iter()
        .map(|(_, group)| {
            u64::try_from(group.as_raw_nonzero().get()).expect("a process group is positive")
        })
        .collect()
}

fn await_one_process_group_report(directory: &Path) -> (Pid, Pid) {
    await_process_group_reports(directory, 1)
        .pop()
        .expect("one process-group report was returned")
}

fn await_process_exit(process: Pid) {
    await_process_exit_during(process, "fixture cleanup");
}

fn await_process_exit_during(process: Pid, context: &str) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match test_kill_process(process) {
            Err(rustix::io::Errno::SRCH) => return,
            Ok(()) => {}
            Err(error) => panic!("cannot inspect process {process} during {context}: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "process {} remained alive during {context}",
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

const DEFAULT_STRESS_SEEDS: &[u64] = &[
    0x1020_3040_5060_7080,
    0x1357_9bdf_2468_ace1,
    0x5eed_fade_cafe_beef,
    0xfedc_ba98_7654_3210,
];

fn stress_seeds() -> Vec<u64> {
    let Some(configured) = std::env::var_os("FLASH_PTY_STRESS_SEEDS") else {
        return DEFAULT_STRESS_SEEDS.to_vec();
    };
    let configured = configured
        .into_string()
        .expect("FLASH_PTY_STRESS_SEEDS must be UTF-8");
    assert!(
        !configured.trim().is_empty(),
        "FLASH_PTY_STRESS_SEEDS must name at least one nonzero seed"
    );

    configured
        .split(',')
        .map(|raw| {
            let token = raw.trim();
            assert!(
                !token.is_empty(),
                "FLASH_PTY_STRESS_SEEDS contains an empty seed"
            );
            let parsed = token.strip_prefix("0x").map_or_else(
                || token.parse::<u64>(),
                |digits| u64::from_str_radix(digits, 16),
            );
            let seed =
                parsed.unwrap_or_else(|error| panic!("invalid PTY stress seed {token:?}: {error}"));
            assert_ne!(seed, 0, "PTY stress seeds must be nonzero");
            seed
        })
        .collect()
}

struct SeededSchedule(u64);

impl SeededSchedule {
    fn new(seed: u64) -> Self {
        assert_ne!(seed, 0, "PTY stress seeds must be nonzero");
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn choose(&mut self, upper: usize) -> usize {
        assert!(upper > 0, "a schedule choice needs a nonempty range");
        usize::try_from(self.next() % u64::try_from(upper).unwrap()).unwrap()
    }

    fn coin(&mut self) -> bool {
        self.next() & 1 == 1
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for upper in (1..values.len()).rev() {
            let other = self.choose(upper + 1);
            values.swap(upper, other);
        }
    }
}

fn export_path(session: &mut Pty, name: &str, path: &Path) {
    let value = path.to_str().expect("the test path is UTF-8");
    assert!(
        !value.contains(['\'', '\n']),
        "the PTY fixture path must fit one single-quoted word"
    );
    let mark = session.mark();
    let source = format!("export {name} = '{value}'");
    session.send(source.as_bytes());
    session.send(ENTER);
    session.await_prompt(mark);
}

fn assert_notice_precedes_prompt(rendered: &str, notice: &str) {
    let notice_start = rendered
        .find(notice)
        .unwrap_or_else(|| panic!("missing notice {notice:?} in:\n{rendered}"));
    assert!(
        rendered[notice_start + notice.len()..].contains(">> "),
        "the next prompt must follow {notice:?}; rendered:\n{rendered}"
    );
}

fn await_completion_notice(session: &mut Pty, start: usize, notice: &str) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let prompt_mark = session.mark();
        session.send(CTRL_C);
        session.await_prompt(prompt_mark);

        let rendered = session.rendered_from(start);
        if rendered.contains(notice) {
            return rendered;
        }
        assert!(
            Instant::now() < deadline,
            "the background job completed without a prompt-boundary notice:\n{rendered}"
        );
    }
}

#[test]
fn draws_the_primary_prompt_and_runs_a_command() {
    let cwd = unique_dir("prompt");
    let mut session = interactive(&cwd);
    session.expect(">> ");

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
    session.expect(">> ");

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
    session.expect(">> ");

    session.send(CTRL_D);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn the_exit_builtin_propagates_its_status() {
    let cwd = unique_dir("exit");
    let mut session = interactive(&cwd);
    session.expect(">> ");

    session.send(b"exit 7");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 7);
}

#[test]
fn a_runtime_error_is_reported_and_the_session_recovers() {
    let cwd = unique_dir("recovery");
    let mut session = interactive(&cwd);
    session.expect(">> ");

    session.send(b"$missing");
    session.send(ENTER);
    // The recoverable diagnostic anchors on the offending source.
    session.expect("missing");

    session.send(b"exit 2");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 2);
}

#[test]
fn structured_errors_are_caught_in_the_real_interactive_session() {
    let cwd = unique_dir("interactive-structured-error");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let mark = session.mark();
    session.send(b"try { throw \"interactive\" } catch error { echo ${$error.message} }");
    session.send(ENTER);
    session.await_prompt(mark);
    let rendered = session.rendered_from(mark);
    assert!(rendered.contains("interactive"), "{rendered}");
    assert!(!rendered.contains("error[RUN001]"), "{rendered}");

    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
}

#[test]
fn the_session_survives_a_terminal_resize() {
    let cwd = unique_dir("resize");
    let mut session = interactive(&cwd);
    session.expect(">> ");

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
fn config_commits_pipefail_and_capture_limit_into_the_real_session() {
    let cwd = unique_dir("config-options");
    let mut session = configured(
        &cwd,
        "$pipefail = true\n$capture_limit = 3\n$history = false\n",
        &[("XDG_STATE_HOME", "/dev/null")],
    );
    session.await_prompt(0);

    let exact_mark = session.mark();
    session.send(b"let exact = $(^printf abc)");
    session.send(ENTER);
    session.await_prompt(exact_mark);

    let overflow_mark = session.mark();
    session.send(b"let overflow = $(^printf abcd)");
    session.send(ENTER);
    session.expect_from(overflow_mark, "capture limit");
    session.await_prompt(overflow_mark);

    let pipeline_mark = session.mark();
    session.send(b"^false | ^true");
    session.send(ENTER);
    session.await_prompt(pipeline_mark);
    session.send(b"exit");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 1);
}

#[test]
fn config_capture_limit_zero_accepts_empty_and_rejects_one_byte() {
    let cwd = unique_dir("config-zero-capture");
    let mut session = configured(&cwd, "$capture_limit = 0\n", &[]);
    session.await_prompt(0);

    let empty_mark = session.mark();
    session.send(b"let empty = $(^true)");
    session.send(ENTER);
    session.await_prompt(empty_mark);

    let overflow_mark = session.mark();
    session.send(b"let overflow = $(^printf x)");
    session.send(ENTER);
    session.expect_from(overflow_mark, "capture limit");
}

#[test]
fn disabled_and_failed_config_use_clean_session_defaults() {
    let cwd = unique_dir("config-defaults");
    let home = unique_dir("disabled-config-home");
    let config_directory = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/flash")
    } else {
        home.join(".config/flash")
    };
    fs::create_dir_all(&config_directory).unwrap();
    fs::write(config_directory.join("config.fsh"), "$pipefail = true\n").unwrap();
    let home_text = home.to_str().unwrap();
    let mut disabled = Pty::spawn(
        &["--no-config", "--no-history"],
        &[("HOME", home_text)],
        &cwd,
    );
    disabled.await_prompt(0);
    let mark = disabled.mark();
    disabled.send(b"^false | ^true");
    disabled.send(ENTER);
    disabled.await_prompt(mark);
    disabled.send(b"exit");
    disabled.send(ENTER);
    assert_eq!(disabled.wait_code(), 0);

    let mut failed = configured(&cwd, "$pipefail = 'yes'\n", &[]);
    failed.expect("ConfigEvaluation");
    failed.expect("[SAFE] >> ");
    let mark = failed.mark();
    failed.send(b"^false | ^true");
    failed.send(ENTER);
    failed.await_prompt(mark);
    failed.send(b"exit");
    failed.send(ENTER);
    assert_eq!(failed.wait_code(), 0);
}

#[test]
fn live_completion_refreshes_config_repl_path_and_cwd_candidates() {
    let cwd = unique_dir("live-completion");
    let bin = unique_dir("live-completion-bin");
    let later_bin = unique_dir("live-completion-later-bin");
    let executable = bin.join("flash-path-candidate");
    fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).unwrap();
    let later_executable = later_bin.join("flash-later-candidate");
    fs::write(&later_executable, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&later_executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&later_executable, permissions).unwrap();
    fs::write(cwd.join("output-candidate"), b"").unwrap();
    fs::create_dir(cwd.join("nested")).unwrap();
    fs::write(cwd.join("nested/nested-candidate"), b"").unwrap();
    let path = bin.to_str().unwrap();
    let mut session = configured(
        &cwd,
        "def configcandidate() {\n    return 1\n}\n",
        &[("PATH", path)],
    );
    session.await_prompt(0);

    for (source, candidate) in [
        ("configcan", "configcandidate"),
        ("^flash-path", "flash-path-candidate"),
        ("pwd > output-c", "output-candidate"),
    ] {
        session.send(source.as_bytes());
        let mark = session.mark();
        session.send(TAB);
        session.expect_from(mark, candidate);
        let prompt_mark = session.mark();
        session.send(CTRL_C);
        session.await_prompt(prompt_mark);
    }

    let define_mark = session.mark();
    session.send(b"def latercandidate() { return 1 }");
    session.send(ENTER);
    session.await_prompt(define_mark);
    session.send(b"latercan");
    let mark = session.mark();
    session.send(TAB);
    session.expect_from(mark, "latercandidate");
    let prompt_mark = session.mark();
    session.send(CTRL_C);
    session.await_prompt(prompt_mark);

    export_path(&mut session, "PATH", &later_bin);
    session.send(b"^flash-later");
    let mark = session.mark();
    session.send(TAB);
    session.expect_from(mark, "flash-later-candidate");
    let prompt_mark = session.mark();
    session.send(CTRL_C);
    session.await_prompt(prompt_mark);

    let cd_mark = session.mark();
    session.send(b"cd nested");
    session.send(ENTER);
    session.await_prompt(cd_mark);
    session.send(b"pwd > nested-c");
    let mark = session.mark();
    session.send(TAB);
    session.expect_from(mark, "nested-candidate");
}

#[test]
fn config_can_disable_completion_before_the_first_prompt() {
    let cwd = unique_dir("completion-disabled");
    let mut session = configured(&cwd, "$completion = false\n$history = false\n", &[]);
    session.await_prompt(0);

    session.send(b"pw");
    let mark = session.mark();
    session.send(TAB);
    thread::sleep(SETTLE);
    assert!(
        !session.rendered_from(mark).contains("pwd"),
        "a disabled completer must not expose the standard command catalog"
    );
    let prompt_mark = session.mark();
    session.send(CTRL_C);
    session.await_prompt(prompt_mark);
    session.send(b"exit 0");
    session.send(ENTER);
    assert_eq!(session.wait_code(), 0);
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
fn a_complex_foreground_chain_commits_its_supervisor_state_after_completion() {
    let cwd = unique_dir("foreground-chain-completion");
    let destination = cwd.join("transported");
    fs::create_dir(&destination).expect("create the transported directory");
    let destination_text = destination
        .to_str()
        .expect("the test directory path is UTF-8");
    let mut session = interactive(&cwd);
    session.await_prompt(0);

    let completion_mark = session.mark();
    let command = format!("^{STATUS_FIXTURE} exit 0 && cd {destination_text}");
    session.send(command.as_bytes());
    session.send(ENTER);
    session.await_prompt(completion_mark);

    let state_mark = session.mark();
    session.send(b"pwd");
    session.send(ENTER);
    session.expect_from(state_mark, destination_text);
    session.await_prompt(state_mark);

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
    session.await_terminal_owner(session.shell_group(), "the initial job lifecycle prompt");

    let command = format!("^{PROCESS_OBSERVER}");
    let stop_mark = session.mark();
    session.send(command.as_bytes());
    session.send(ENTER);
    let (process, group) = await_one_process_group_report(&cwd);
    let mut cleanup = ProcessGroupCleanup::new(group);
    session.await_terminal_owner(group, "the initial foreground job");

    session.send(CTRL_Z);
    session.expect_from(stop_mark, "[1] Stopped");
    session.await_prompt(stop_mark);
    session.await_terminal_owner(session.shell_group(), "the prompt after Ctrl-Z");
    let stopped = session.rendered_from(stop_mark);
    assert_eq!(
        stopped.matches("[1] Stopped").count(),
        1,
        "one terminal stop must publish one notice:\n{stopped}"
    );

    let stopped_jobs_mark = session.mark();
    session.send(b"jobs");
    session.send(ENTER);
    session.expect_from(stopped_jobs_mark, "job | state");
    session.expect_from(stopped_jobs_mark, "%1  | stopped | foreground");
    session.await_prompt(stopped_jobs_mark);

    let bg_mark = session.mark();
    session.send(b"bg %1");
    session.send(ENTER);
    session.await_prompt(bg_mark);
    session.await_terminal_owner(session.shell_group(), "the prompt after bg");

    let running_jobs_mark = session.mark();
    session.send(b"jobs");
    session.send(ENTER);
    session.expect_from(running_jobs_mark, "%1  | running | background");
    session.await_prompt(running_jobs_mark);

    let fg_mark = session.mark();
    session.send(b"fg %1");
    session.send(ENTER);
    session.await_terminal_owner(group, "the job resumed through fg");
    assert!(
        test_kill_process(process).is_ok(),
        "the held fixture must still be live while fg waits"
    );
    fs::write(&release, b"complete").expect("release the foreground fixture");
    session.await_prompt(fg_mark);
    session.await_terminal_owner(
        session.shell_group(),
        "the prompt after foreground completion",
    );
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
fn an_externally_continued_job_reports_running_at_a_real_terminal() {
    let cwd = unique_dir("job-external-continue");
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
    assert_eq!(
        session
            .rendered_from(stop_mark)
            .matches("[1] Stopped")
            .count(),
        1,
        "the terminal stop must publish exactly one notice"
    );

    let continued_mark = session.mark();
    kill_process_group(group, Signal::CONT).expect("continue the fixture from the controller");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let jobs_mark = session.mark();
        session.send(b"jobs");
        session.send(ENTER);
        session.await_prompt(jobs_mark);
        if session
            .rendered_from(jobs_mark)
            .contains("%1  | running | foreground")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the externally continued job never became running:\n{}",
            session.rendered_from(continued_mark)
        );
    }
    assert_eq!(
        session
            .rendered_from(continued_mark)
            .matches("[1] Stopped")
            .count(),
        0,
        "an observed continuation must not republish a stale stop notice"
    );

    let completion_mark = session.mark();
    fs::write(&release, b"complete").expect("release the continued fixture");
    await_process_exit(process);

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let prompt_mark = session.mark();
        session.send(b"jobs");
        session.send(ENTER);
        session.await_prompt(prompt_mark);
        let completion = session.rendered_from(completion_mark);
        if completion.contains("[1] Done") {
            assert_notice_precedes_prompt(&completion, "[1] Done");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the continued fixture completed without a prompt-boundary notice:\n{completion}"
        );
    }
    cleanup.disarm();

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
    session.expect_from(completion_mark, ">> ");
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
    session.expect_from(launch_mark, ">> ");
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

    let completion_mark = session.mark();
    fs::write(&release, b"complete").expect("release both pipeline members");
    let completion = await_completion_notice(&mut session, completion_mark, "[1] Done");
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

    let completion_mark = session.mark();
    let completion = await_completion_notice(&mut session, completion_mark, "[1] Done");
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
    let completion_mark = session.mark();
    fs::write(&release, b"complete").expect("release the successful left side");
    let completion = await_completion_notice(&mut session, completion_mark, "[1] Done");
    let result = session.rendered_from(result_mark);
    let component = cwd.file_name().unwrap().to_string_lossy();
    assert!(
        !result.contains(component.as_ref()),
        "the successful left side must skip the internal right side:\n{result}"
    );
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
    session.expect_from(completion_mark, ">> ");
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
    session.expect_from(completion_mark, ">> ");
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

#[test]
fn stress_replayable_job_control_cycles_restore_the_terminal_and_reap_the_child() {
    for seed in stress_seeds() {
        eprintln!("PTY lifecycle stress seed {seed:#018x}");
        let cwd = unique_dir(&format!("stress-cycle-{seed:016x}"));
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
        let mut schedule = SeededSchedule::new(seed);
        let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
        let _release_on_drop = ReleaseOnDrop(release.clone());
        session.await_prompt(0);
        session.await_terminal_owner(
            session.shell_group(),
            &format!("the initial lifecycle prompt for seed {seed:#018x}"),
        );

        let edit_mark = session.mark();
        session.send(b"exit 99");
        session.expect_from(edit_mark, "exit 99");
        session.send(CTRL_C);
        session.await_prompt(edit_mark);
        session.await_terminal_owner(
            session.shell_group(),
            &format!("editor Ctrl-C for seed {seed:#018x}"),
        );

        let command = format!("^{PROCESS_OBSERVER}");
        session.send(command.as_bytes());
        session.send(ENTER);
        let (process, group) = await_one_process_group_report(&cwd);
        let mut cleanup = ProcessGroupCleanup::new(group);
        session.await_terminal_owner(
            group,
            &format!("initial foreground ownership for seed {seed:#018x}"),
        );

        let stop_cycles = 1 + schedule.choose(3);
        let complete_in_background = schedule.coin();
        for cycle in 0..stop_cycles {
            let stop_mark = session.mark();
            session.send(CTRL_Z);
            session.expect_from(stop_mark, "[1] Stopped");
            session.await_prompt(stop_mark);
            session.await_terminal_owner(
                session.shell_group(),
                &format!("the prompt after stop {} for seed {seed:#018x}", cycle + 1),
            );
            assert_eq!(
                session
                    .rendered_from(stop_mark)
                    .matches("[1] Stopped")
                    .count(),
                1,
                "one stop notice is owed in cycle {} for seed {seed:#018x}:\n{}",
                cycle + 1,
                session.rendered_from(stop_mark)
            );

            let final_cycle = cycle + 1 == stop_cycles;
            let move_through_background = complete_in_background && final_cycle || schedule.coin();
            if move_through_background {
                let bg_mark = session.mark();
                session.send(b"bg %1");
                session.send(ENTER);
                session.await_prompt(bg_mark);
                session.await_terminal_owner(
                    session.shell_group(),
                    &format!("the prompt after bg for seed {seed:#018x}"),
                );

                if schedule.coin() {
                    let jobs_mark = session.mark();
                    session.send(b"jobs");
                    session.send(ENTER);
                    session.expect_from(jobs_mark, "%1  | running | background");
                    session.await_prompt(jobs_mark);
                    session.await_terminal_owner(
                        session.shell_group(),
                        &format!("the jobs prompt for seed {seed:#018x}"),
                    );
                }
            }

            if complete_in_background && final_cycle {
                let completion_mark = session.mark();
                fs::write(&release, b"complete").expect("release the background fixture");
                await_process_exit_during(
                    process,
                    &format!("background completion for seed {seed:#018x}"),
                );
                assert!(
                    !session.rendered_from(completion_mark).contains("[1] Done"),
                    "a completion notice entered the active prompt for seed {seed:#018x}:\n{}",
                    session.rendered_from(completion_mark)
                );
                session.send(CTRL_C);
                session.expect_from(completion_mark, "[1] Done");
                session.await_prompt(completion_mark);
                let completion = session.rendered_from(completion_mark);
                assert_eq!(
                    completion.matches("[1] Done").count(),
                    1,
                    "background completion was not exactly once for seed {seed:#018x}:\n{completion}"
                );
                assert_notice_precedes_prompt(&completion, "[1] Done");
                session.await_terminal_owner(
                    session.shell_group(),
                    &format!("background completion for seed {seed:#018x}"),
                );
                break;
            }

            let fg_mark = session.mark();
            session.send(b"fg %1");
            session.send(ENTER);
            session.await_terminal_owner(
                group,
                &format!("foreground resume {} for seed {seed:#018x}", cycle + 1),
            );

            if final_cycle {
                if schedule.coin() {
                    session.send(CTRL_C);
                } else {
                    fs::write(&release, b"complete").expect("release the foreground fixture");
                }
                session.await_prompt(fg_mark);
                session.await_terminal_owner(
                    session.shell_group(),
                    &format!("foreground completion for seed {seed:#018x}"),
                );
                await_process_exit_during(
                    process,
                    &format!("foreground completion for seed {seed:#018x}"),
                );
                assert!(
                    !session.rendered_from(fg_mark).contains("[1] Done"),
                    "fg did not consume completion for seed {seed:#018x}:\n{}",
                    session.rendered_from(fg_mark)
                );
            }
        }

        await_process_exit_during(
            process,
            &format!("final lifecycle cleanup for seed {seed:#018x}"),
        );
        cleanup.disarm();

        let recovery_mark = session.mark();
        session.send(b"echo stress-cycle-alive");
        session.send(ENTER);
        session.expect_from(recovery_mark, "stress-cycle-alive");
        session.await_prompt(recovery_mark);
        session.await_terminal_owner(
            session.shell_group(),
            &format!("the recovery prompt for seed {seed:#018x}"),
        );

        session.send(b"exit 0");
        session.send(ENTER);
        assert_eq!(
            session.wait_code(),
            0,
            "the lifecycle shell failed for seed {seed:#018x}"
        );
    }
}

#[test]
fn stress_concurrent_completions_remain_prompt_safe_and_exactly_once() {
    for seed in stress_seeds() {
        eprintln!("PTY concurrent-completion stress seed {seed:#018x}");
        let cwd = unique_dir(&format!("stress-completions-{seed:016x}"));
        let report = cwd.join("report.bin");
        let report_text = report.to_str().expect("test report path is UTF-8");
        let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
        let environment = [
            ("FLASH_PROBE_REPORT", report_text),
            ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ];
        let mut schedule = SeededSchedule::new(seed);
        let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
        session.await_prompt(0);
        session.await_terminal_owner(
            session.shell_group(),
            &format!("the initial completion prompt for seed {seed:#018x}"),
        );

        let releases: Vec<_> = (1..=3)
            .map(|job| cwd.join(format!("release-{job}")))
            .collect();
        let release_guards: Vec<_> = releases.iter().cloned().map(ReleaseOnDrop).collect();
        for (index, release) in releases.iter().enumerate() {
            export_path(&mut session, "FLASH_PROBE_HOLD_UNTIL", release);
            let launch_mark = session.mark();
            let command = format!("^{PROCESS_OBSERVER} &");
            session.send(command.as_bytes());
            session.send(ENTER);
            session.expect_from(launch_mark, &format!("[{}] ", index + 1));
            session.await_prompt(launch_mark);
            session.await_terminal_owner(
                session.shell_group(),
                &format!("background launch {} for seed {seed:#018x}", index + 1),
            );
        }

        let reports = await_process_group_reports(&cwd, 3);
        let mut cleanups: Vec<_> = reports
            .iter()
            .map(|(_, group)| ProcessGroupCleanup::new(*group))
            .collect();

        let buffer_mark = session.mark();
        session.send(b"echo concurrent-completion-buffer");
        session.expect_from(buffer_mark, "concurrent-completion-buffer");

        let mut release_order = releases.clone();
        schedule.shuffle(&mut release_order);
        let barrier = Arc::new(Barrier::new(release_order.len() + 1));
        let writers: Vec<_> = release_order
            .into_iter()
            .enumerate()
            .map(|(rank, release)| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..rank {
                        thread::yield_now();
                    }
                    fs::write(release, b"complete").expect("release one background fixture");
                })
            })
            .collect();
        barrier.wait();
        for writer in writers {
            writer.join().expect("join one release writer");
        }
        for (process, _) in &reports {
            await_process_exit_during(
                *process,
                &format!("concurrent completion for seed {seed:#018x}"),
            );
        }

        assert!(
            !session.rendered_from(buffer_mark).contains("Done"),
            "a concurrent completion entered the active buffer for seed {seed:#018x}:\n{}",
            session.rendered_from(buffer_mark)
        );

        let completion_mark = session.mark();
        session.send(CTRL_C);
        session.await_prompt(completion_mark);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let completion = session.rendered_from(completion_mark);
            if (1..=3).all(|job| completion.contains(&format!("[{job}] Done"))) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "concurrent completion notices did not converge for seed {seed:#018x}:\n{completion}"
            );
            let refresh_mark = session.mark();
            session.send(b"jobs");
            session.send(ENTER);
            session.await_prompt(refresh_mark);
        }

        let completion = session.rendered_from(completion_mark);
        for job in 1..=3 {
            let notice = format!("[{job}] Done");
            assert_eq!(
                completion.matches(&notice).count(),
                1,
                "completion {job} was not exactly once for seed {seed:#018x}:\n{completion}"
            );
            assert_notice_precedes_prompt(&completion, &notice);
        }
        session.await_terminal_owner(
            session.shell_group(),
            &format!("concurrent completion delivery for seed {seed:#018x}"),
        );
        for cleanup in &mut cleanups {
            cleanup.disarm();
        }
        drop(release_guards);

        session.send(b"exit 0");
        session.send(ENTER);
        assert_eq!(
            session.wait_code(),
            0,
            "the concurrent-completion shell failed for seed {seed:#018x}"
        );
    }
}

#[test]
fn stress_replayable_live_job_exit_hangs_up_and_reaps_every_group() {
    for seed in stress_seeds() {
        eprintln!("PTY live-job exit stress seed {seed:#018x}");
        let cwd = unique_dir(&format!("stress-exit-{seed:016x}"));
        let report = cwd.join("report.bin");
        let report_text = report.to_str().expect("test report path is UTF-8");
        let cwd_text = cwd.to_str().expect("test directory path is UTF-8");
        let environment = [
            ("FLASH_PROBE_REPORT", report_text),
            ("FLASH_PROBE_GROUP_REPORT", cwd_text),
        ];
        let mut schedule = SeededSchedule::new(seed);
        let mut session = Pty::spawn(&["--no-config", "--no-history"], &environment, &cwd);
        session.await_prompt(0);

        let background_release = cwd.join("background-release");
        let stopped_release = cwd.join("stopped-release");
        let _background_release_on_drop = ReleaseOnDrop(background_release.clone());
        let _stopped_release_on_drop = ReleaseOnDrop(stopped_release.clone());

        export_path(&mut session, "FLASH_PROBE_HOLD_UNTIL", &background_release);
        let background_mark = session.mark();
        let command = format!("^{PROCESS_OBSERVER} &");
        session.send(command.as_bytes());
        session.send(ENTER);
        session.expect_from(background_mark, "[1] ");
        session.await_prompt(background_mark);
        let (background_process, background_group) = await_process_group_reports(&cwd, 1)[0];
        let mut background_cleanup = ProcessGroupCleanup::new(background_group);

        export_path(&mut session, "FLASH_PROBE_HOLD_UNTIL", &stopped_release);
        let command = format!("^{PROCESS_OBSERVER}");
        session.send(command.as_bytes());
        session.send(ENTER);
        let reports = await_process_group_reports(&cwd, 2);
        let (stopped_process, stopped_group) = reports
            .into_iter()
            .find(|(process, _)| *process != background_process)
            .expect("the foreground fixture has its own report");
        let mut stopped_cleanup = ProcessGroupCleanup::new(stopped_group);
        session.await_terminal_owner(
            stopped_group,
            &format!("the exit fixture foreground for seed {seed:#018x}"),
        );

        let stop_mark = session.mark();
        session.send(CTRL_Z);
        session.expect_from(stop_mark, "[2] Stopped");
        session.await_prompt(stop_mark);
        session.await_terminal_owner(
            session.shell_group(),
            &format!("the stopped-job prompt for seed {seed:#018x}"),
        );

        let first_refusal = session.mark();
        session.send(b"exit 0");
        session.send(ENTER);
        session.expect_from(first_refusal, "fsh: 2 live background jobs");
        session.expect_from(first_refusal, "fsh: exit again to hang up");
        session.await_prompt(first_refusal);
        session.await_terminal_owner(
            session.shell_group(),
            &format!("the first exit refusal for seed {seed:#018x}"),
        );

        if schedule.coin() {
            fs::write(&background_release, b"complete")
                .expect("release the independent background fixture");
            await_process_exit_during(
                background_process,
                &format!("independent completion for seed {seed:#018x}"),
            );
            background_cleanup.disarm();
        }

        if schedule.coin() {
            let reset_mark = session.mark();
            session.send(b"echo reset-exit-refusal");
            session.send(ENTER);
            session.expect_from(reset_mark, "reset-exit-refusal");
            session.await_prompt(reset_mark);
            session.await_terminal_owner(
                session.shell_group(),
                &format!("the refusal reset for seed {seed:#018x}"),
            );

            let second_refusal = session.mark();
            session.send(b"exit 0");
            session.send(ENTER);
            session.expect_from(second_refusal, "live background job");
            session.expect_from(second_refusal, "fsh: exit again to hang up");
            session.await_prompt(second_refusal);
            session.await_terminal_owner(
                session.shell_group(),
                &format!("the second exit refusal for seed {seed:#018x}"),
            );
        }

        session.send(b"exit 0");
        session.send(ENTER);
        assert_eq!(
            session.wait_code(),
            0,
            "the live-job exit failed for seed {seed:#018x}"
        );
        await_process_exit_during(
            background_process,
            &format!("background hang-up for seed {seed:#018x}"),
        );
        await_process_exit_during(
            stopped_process,
            &format!("stopped-job hang-up for seed {seed:#018x}"),
        );
        background_cleanup.disarm();
        stopped_cleanup.disarm();
    }
}
