#![cfg(any(target_os = "macos", target_os = "linux"))]
#![deny(unsafe_code)]

#[path = "support/data_processing.rs"]
mod support;

use opaal_cli::project::{
    ExecuteProjectRequest, PlanProjectRequest, execute_explicit_plan, plan_explicit_project,
};
use opaal_platform_posix::standard_host::{WORKER_ARGUMENT, worker_entry};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

static RECEIPT: OnceLock<PathBuf> = OnceLock::new();
static FAULT: OnceLock<String> = OnceLock::new();
static KERNEL_ERROR: AtomicI32 = AtomicI32::new(0);
const CANARY: &str = "native-receipt-payload-canary";

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.get(1).is_some_and(|arg| arg == WORKER_ARGUMENT) {
        std::process::exit(worker_entry(&arguments).unwrap());
    }
    if arguments.get(1).is_some_and(|arg| arg == "case") {
        qualify(
            arguments[2].to_str().unwrap(),
            arguments[3].to_str().unwrap() == "error",
        );
        return;
    }
    for fault in ["write-limit", "file-sync", "parent-sync"] {
        for primary in ["success", "error"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["case", fault, primary])
                .output()
                .unwrap();
            assert!(result.status.success(), "{fault}/{primary}: {result:?}");
            assert_eq!(result.stdout, CANARY.as_bytes(), "output replayed or lost");
            assert!(result.stderr.is_empty(), "administrative diagnostic leaked");
        }
    }
    println!("6 native receipt kernel write/sync failure cases passed");
}

fn qualify(fault: &str, error: bool) {
    let work = support::ReportDir::new();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/standard-input-output");
    for name in ["opaal.toml", "authority.toml"] {
        let text = std::fs::read_to_string(fixtures.join(name)).unwrap();
        work.write(
            name,
            text.replace(">=1.3.0", &format!(">={}", env!("CARGO_PKG_VERSION"))),
        );
    }
    let target = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    work.write(
        "tools.toml",
        std::fs::read(fixtures.join(format!("tools-{target}.toml"))).unwrap(),
    );
    work.write("tasks.opaal", format!(
        "import std::io as io\naction run() -> Null effects {{ stdout.write(); }} {{ io::print('{CANARY}')\n{} }}\ntask sample = run\n",
        if error { "throw 'error-payload-canary'" } else { "return null" }
    ));
    let plan = work.0.join("plan.json");
    plan_explicit_project(&PlanProjectRequest::new(
        work.0.join("opaal.toml"),
        "sample".into(),
        "ci".into(),
        Vec::new(),
        900,
        plan.clone(),
    ))
    .unwrap();
    let document: Value = serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    let digest = document["digest"].as_str().unwrap();
    let journal = work.0.join("run.jsonl");
    let receipt = work.0.join("receipt.json");
    RECEIPT.set(receipt.clone()).unwrap();
    FAULT.set(fault.to_owned()).unwrap();
    let run = execute_explicit_plan(
        &ExecuteProjectRequest::new(
            plan,
            digest.into(),
            Some("1".repeat(32)),
            None,
            journal.clone(),
        )
        .with_receipt_out(Some(receipt.clone())),
        false,
        &mut std::io::empty(),
    )
    .unwrap();
    native::assert_reaped();
    assert!(!run.is_successful());
    assert!(run.output().is_empty());
    assert_eq!(
        KERNEL_ERROR.load(Ordering::Relaxed),
        if fault == "write-limit" {
            libc::EFBIG
        } else {
            libc::EINVAL
        }
    );
    let metadata = run.receipt().unwrap();
    assert_eq!(
        metadata["primary"]["class"],
        if error { "error" } else { "success" }
    );
    assert!(metadata["primary"]["value_digest"].is_null());
    assert_eq!(metadata["primary"]["partial"], true);
    assert_eq!(
        metadata["secondary"],
        serde_json::json!([{"category":"receipt", "code":"JOURNAL005"}])
    );
    assert_eq!(metadata["journal_state"], "complete");
    assert_eq!(
        metadata["progress"]["stdout.write"]["confirmed_bytes"],
        CANARY.len()
    );
    assert_eq!(
        metadata["progress"]["stdout.write"]["uncertain_bytes_upper_bound"],
        0
    );
    let bytes = std::fs::read(&journal).unwrap();
    let audit = opaal_runtime::workflow::audit_journal(&bytes).unwrap();
    assert!(audit.is_complete());
    assert_eq!(
        audit.value()["primary"]["class"],
        metadata["primary"]["class"]
    );
    assert_eq!(audit.value()["primary"]["value_digest"], Value::Null);
    let durable = std::fs::read(&receipt).unwrap();
    if fault == "write-limit" {
        assert!(durable.is_empty());
    } else {
        let written: Value = serde_json::from_slice(&durable).unwrap();
        assert_eq!(written["primary"]["class"], metadata["primary"]["class"]);
        assert_eq!(written["secondary"], serde_json::json!([]));
    }
    for evidence in [bytes, durable, serde_json::to_vec(metadata).unwrap()] {
        let text = String::from_utf8(evidence).unwrap();
        assert!(!text.contains("canary"));
        let hash = opaal_runtime::workflow::digest_bytes(CANARY.as_bytes());
        assert!(!text.contains(&hash));
    }
}

#[allow(unsafe_code)]
mod native {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub fn assert_reaped() {
        // SAFETY: this isolated case owns every child; WNOHANG cannot block.
        unsafe {
            let mut status = 0;
            assert_eq!(libc::waitpid(-1, &mut status, libc::WNOHANG), -1);
            assert_eq!(errno(), libc::ECHILD);
        }
    }

    #[allow(clippy::unnecessary_cast)]
    fn matches(fd: libc::c_int, parent: bool) -> bool {
        let Some(receipt) = RECEIPT.get() else {
            return false;
        };
        let Ok(metadata) = std::fs::metadata(if parent {
            receipt.parent().unwrap()
        } else {
            receipt
        }) else {
            return false;
        };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat initializes this buffer only on success; the FD is borrowed.
        unsafe {
            libc::fstat(fd, stat.as_mut_ptr()) == 0 && {
                let stat = stat.assume_init();
                stat.st_dev as u64 == metadata.dev() && stat.st_ino == metadata.ino()
            }
        }
    }

    fn errno() -> i32 {
        // SAFETY: libc supplies this thread's errno storage on the selected host.
        unsafe {
            #[cfg(target_os = "macos")]
            {
                *libc::__error()
            }
            #[cfg(target_os = "linux")]
            {
                *libc::__errno_location()
            }
        }
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn write(
        fd: libc::c_int,
        bytes: *const libc::c_void,
        count: usize,
    ) -> libc::ssize_t {
        // SAFETY: RTLD_NEXT supplies the exact ABI; the original owns buffer checks.
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_NEXT, c"write".as_ptr());
            assert!(!symbol.is_null());
            let original: unsafe extern "C" fn(
                libc::c_int,
                *const libc::c_void,
                usize,
            ) -> libc::ssize_t = std::mem::transmute(symbol);
            if FAULT.get().is_some_and(|fault| fault == "write-limit") && matches(fd, false) {
                let mut previous = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                assert_eq!(
                    libc::getrlimit(libc::RLIMIT_FSIZE, previous.as_mut_ptr()),
                    0
                );
                let previous = previous.assume_init();
                let limited = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: previous.rlim_max,
                };
                let handler = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limited), 0);
                let result = original(fd, bytes, count);
                let error = errno();
                KERNEL_ERROR.store(error, Ordering::Relaxed);
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &previous), 0);
                libc::signal(libc::SIGXFSZ, handler);
                #[cfg(target_os = "macos")]
                {
                    *libc::__error() = error;
                }
                #[cfg(target_os = "linux")]
                {
                    *libc::__errno_location() = error;
                }
                return result;
            }
            let result = original(fd, bytes, count);
            if result == 1 && count == 1 && *bytes.cast::<u8>() == b'\n' && matches(fd, false) {
                let fault = FAULT.get().unwrap();
                if fault == "file-sync" {
                    substitute_pipe(fd);
                } else if fault == "parent-sync" {
                    // All matching directories belong to this isolated test's work directory.
                    let parents = (3..1024)
                        .filter(|fd| matches(*fd, true))
                        .collect::<Vec<_>>();
                    assert!(!parents.is_empty());
                    for parent in parents {
                        substitute_pipe(parent);
                    }
                }
            }
            result
        }
    }

    unsafe fn substitute_pipe(fd: libc::c_int) {
        // SAFETY: the caller selects an owned descriptor after the final receipt write.
        unsafe {
            let mut pipe = [-1; 2];
            assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
            assert_eq!(libc::fsync(pipe[0]), -1);
            KERNEL_ERROR.store(errno(), Ordering::Relaxed);
            assert_eq!(libc::dup2(pipe[0], fd), fd);
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }
}
