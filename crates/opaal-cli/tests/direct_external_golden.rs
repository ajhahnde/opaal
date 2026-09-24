#![cfg(any(target_os = "macos", target_os = "linux"))]
#![forbid(unsafe_code)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process, test_kill_process};

const OPAAL: &str = env!("CARGO_BIN_EXE_opaal");
const PROBE: &str = env!("CARGO_BIN_EXE_opaal-e2e-direct-probe-fixture");
static NEXT: AtomicU64 = AtomicU64::new(1);

struct GoldenDir(PathBuf);

impl GoldenDir {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "opaal-direct-golden-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).expect("create golden work directory");
        symlink(PROBE, path.join("opaal-direct-probe")).expect("put probe on PATH");
        Self(
            path.canonicalize()
                .expect("canonical golden work directory"),
        )
    }
}

impl Drop for GoldenDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove owned golden work directory");
    }
}

#[test]
fn foreground_source_has_exact_cross_host_bytes_and_status() {
    let work = GoldenDir::new();
    let input = [0, 0xff, b'\n'];
    fs::write(work.0.join("golden-input.txt"), input).expect("write golden input");
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/direct-external-execution/foreground.opaal");

    let output = Command::new(OPAAL)
        .arg(source)
        .current_dir(&work.0)
        .env_clear()
        .env("PATH", &work.0)
        .env("OPAAL_GOLDEN_VALUE", "golden-env")
        .output()
        .expect("run golden source");

    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let expected_stdout = format!(
        "arg:74776f20776f726473\narg:\narg:cebb\ngolden-env\n{}\npipeline bytes\n",
        work.0.display()
    );
    assert_eq!(output.stdout, expected_stdout.as_bytes());
    assert_eq!(output.stderr, b"probe stderr\n");
    assert_eq!(
        fs::read(work.0.join("golden-output.txt")).expect("read redirected output"),
        input
    );
}

#[test]
fn background_source_refuses_before_starting_the_golden_probe() {
    let work = GoldenDir::new();
    let source = work.0.join("background.opaal");
    fs::write(&source, "^opaal-direct-probe mark should-not-exist &\n")
        .expect("write background source");

    let output = Command::new(OPAAL)
        .arg(source)
        .current_dir(&work.0)
        .env_clear()
        .env("PATH", &work.0)
        .output()
        .expect("run background refusal");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refused"),
        "{output:?}"
    );
    assert!(!work.0.join("should-not-exist").exists());
}

#[test]
fn inherited_output_streams_past_the_capture_ceiling() {
    let work = GoldenDir::new();
    let source = work.0.join("large-output.opaal");
    fs::write(&source, "^opaal-direct-probe emit-large\n").expect("write large-output source");

    let output = Command::new(OPAAL)
        .arg(source)
        .current_dir(&work.0)
        .env_clear()
        .env("PATH", &work.0)
        .output()
        .expect("run large-output source");

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout.len(), 8 * 1024 * 1024 + 1);
    assert!(output.stdout.iter().all(|byte| *byte == b'O'));
    assert_eq!(output.stderr.len(), 8 * 1024 * 1024 + 1);
    assert!(output.stderr.iter().all(|byte| *byte == b'E'));
}

struct RunningCli(Child);

impl Drop for RunningCli {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn termination_reaps_both_owned_pipeline_members() {
    let work = GoldenDir::new();
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/direct-external-execution/termination.opaal");
    let mut cli = RunningCli(
        Command::new(OPAAL)
            .arg(source)
            .current_dir(&work.0)
            .env_clear()
            .env("PATH", &work.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start golden pipeline"),
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let members = ["first.pid", "second.pid"].map(|name| {
        let marker = work.0.join(name);
        loop {
            if let Ok(raw) = fs::read_to_string(&marker) {
                break Pid::from_raw(raw.parse::<i32>().expect("probe PID is numeric"))
                    .expect("probe PID is positive");
            }
            assert!(Instant::now() < deadline, "{name} did not start");
            thread::sleep(Duration::from_millis(10));
        }
    });

    let cli_pid = Pid::from_raw(cli.0.id() as i32).expect("CLI PID is positive");
    kill_process(cli_pid, Signal::TERM).expect("send TERM to the CLI");
    loop {
        if let Some(status) = cli.0.try_wait().expect("wait for the CLI") {
            assert!(!status.success(), "TERM should interrupt the source");
            break;
        }
        assert!(Instant::now() < deadline, "CLI did not finish after TERM");
        thread::sleep(Duration::from_millis(10));
    }

    for member in members {
        loop {
            if test_kill_process(member) == Err(Errno::SRCH) {
                break;
            }
            if Instant::now() >= deadline {
                let _ = kill_process(member, Signal::KILL);
                panic!("owned pipeline member {member:?} survived TERM cleanup");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
