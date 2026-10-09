#![cfg(any(target_os = "macos", target_os = "linux"))]
#![deny(unsafe_code)]

#[allow(dead_code)]
#[path = "../../opaal-platform-posix/tests/fixtures/standard_host.rs"]
mod native_fixture;
#[path = "support/data_processing.rs"]
mod support;

use opaal_cli::project::{
    AuditRequest, ExecuteProjectRequest, PlanProjectRequest, audit_explicit_journal,
    execute_explicit_plan, plan_explicit_project,
};
use opaal_platform_posix::standard_host::{WORKER_ARGUMENT, worker_entry};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static FAIL_CLEANUP: AtomicBool = AtomicBool::new(false);
static CLEANUP_FAILURES: AtomicUsize = AtomicUsize::new(0);
static WORKER: AtomicBool = AtomicBool::new(false);

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if arguments.get(1).is_some_and(|arg| arg == WORKER_ARGUMENT) {
        WORKER.store(true, Ordering::Relaxed);
        native_fixture::blocked_library::enable(73);
        native_fixture::blocked_library::enable(72);
        std::process::exit(worker_entry(&arguments).unwrap());
    }
    use opaal_platform::standard_host::StandardHost;
    let mut host = opaal_platform_posix::standard_host::PosixStandardHost::for_cli(71).unwrap();
    host.fill(&mut [0; 8], &|| false).unwrap();
    host.close().unwrap();
    for cancelled in [false, true] {
        for cleanup_failed in [false, true] {
            qualify(cancelled, cleanup_failed);
        }
    }
    println!("8 native backend/cancellation/cleanup and journal persistence cases passed");
}

fn qualify(cancelled: bool, cleanup_failed: bool) {
    let work = support::ReportDir::new();
    let fixtures =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/random-values");
    for name in ["opaal.toml", "authority.toml"] {
        let text = std::fs::read_to_string(fixtures.join(name)).unwrap();
        work.write(name, text.replace(">=1.3.0", ">=1.2.0"));
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
    let count = if cancelled { 251 } else { 250 };
    work.write("tasks.opaal", format!(
        "import std::random as random\naction sample() -> Bytes effects {{ entropy.system(); }} {{ return random::bytes({count}) }}\ntask sample = sample\n"
    ));
    let manifest = work.0.join("opaal.toml");
    let plan = work.0.join("sample.plan.json");
    plan_explicit_project(&PlanProjectRequest::new(
        manifest.clone(),
        "sample".into(),
        "ci".into(),
        Vec::new(),
        900,
        plan.clone(),
    ))
    .unwrap();
    let document: Value = serde_json::from_slice(&std::fs::read(&plan).unwrap()).unwrap();
    assert_eq!(document["schema"], "opaal.plan.v3");
    let primary = if cancelled {
        "EXECUTE_CANCELLED"
    } else {
        "EXECUTE_LANGUAGE"
    };
    let mut sink_limit = None;
    for sink_failed in [false, true] {
        FAIL_CLEANUP.store(cleanup_failed, Ordering::Relaxed);
        let failures = CLEANUP_FAILURES.load(Ordering::Relaxed);
        let journal = work.0.join(if sink_failed {
            "failed.jsonl"
        } else {
            "baseline.jsonl"
        });
        let previous_limit = sink_limit.map(native::limit_sink);
        let started = Instant::now();
        let result = execute_explicit_plan(
            &ExecuteProjectRequest::new(
                plan.clone(),
                document["digest"].as_str().unwrap().into(),
                Some("0123456789abcdef0123456789abcdef".into()),
                None,
                journal.clone(),
            ),
            false,
            &mut std::io::empty(),
        );
        if let Some(previous) = previous_limit {
            native::restore_sink(previous);
        }
        assert!(started.elapsed() < Duration::from_secs(35));
        if cancelled {
            assert!(started.elapsed() >= Duration::from_secs(30));
        }
        native_fixture::checks::assert_reaped();
        assert_eq!(
            CLEANUP_FAILURES.load(Ordering::Relaxed) - failures,
            usize::from(cleanup_failed)
        );
        let bytes = std::fs::read(&journal).unwrap();
        let rows = bytes
            .split(|byte| *byte == b'\n')
            .filter(|row| !row.is_empty())
            .map(|row| serde_json::from_slice::<Value>(row).unwrap())
            .collect::<Vec<_>>();
        if sink_failed {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("JOURNAL005") && error.contains(primary),
                "{error}"
            );
            assert!(error.contains("incomplete or unavailable"));
            assert_eq!(
                rows.iter()
                    .map(|row| row["kind"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["header", "action-start", "effect-before"]
            );
            assert_eq!(bytes.len() as u64, sink_limit.unwrap());
        } else {
            assert!(!result.unwrap().is_successful());
            let outcome = &rows.last().unwrap()["payload"]["primary"];
            assert_eq!(outcome["code"], primary);
            assert!(outcome["value_digest"].is_null());
            let progress = &rows[3]["payload"]["operation"];
            assert_eq!(progress["confirmed_bytes"], 0);
            assert_eq!(progress["uncertain_bytes_upper_bound"], count, "{rows:#?}");
            assert_eq!(
                rows.iter()
                    .filter(|row| row["kind"] == "effect-before")
                    .count(),
                1
            );
            let cleanup = &rows.last().unwrap()["payload"]["cleanup"];
            assert_eq!(
                cleanup.as_array().unwrap().len(),
                usize::from(cleanup_failed)
            );
            if cleanup_failed {
                assert_eq!(cleanup[0]["class"], "cleanup-failed");
            }
            sink_limit = Some(
                bytes
                    .split_inclusive(|byte| *byte == b'\n')
                    .take(3)
                    .map(|line| line.len() as u64)
                    .sum(),
            );
        }
        let audit = work.0.join(if sink_failed {
            "failed-audit.json"
        } else {
            "baseline-audit.json"
        });
        let audited =
            audit_explicit_journal(&AuditRequest::new(manifest.clone(), journal, audit.clone()))
                .unwrap();
        assert_eq!(audited.is_complete(), !sink_failed);
        let audited: Value = serde_json::from_slice(&std::fs::read(audit).unwrap()).unwrap();
        assert_eq!(
            audited["completeness"],
            if sink_failed {
                "incomplete"
            } else {
                "complete"
            }
        );
        if sink_failed {
            assert!(audited["primary"].is_null());
        }
        println!(
            "native cancelled={cancelled} cleanup_failed={cleanup_failed} sink_failed={sink_failed} primary={primary} reaped=true elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }
    FAIL_CLEANUP.store(false, Ordering::Relaxed);
}

#[allow(unsafe_code)]
mod native {
    use super::*;

    #[unsafe(no_mangle)]
    unsafe extern "C" fn send(
        fd: libc::c_int,
        bytes: *const libc::c_void,
        count: usize,
        flags: libc::c_int,
    ) -> libc::ssize_t {
        // SAFETY: the caller owns the borrowed buffer; RTLD_NEXT has the exact
        // libc ABI. A reported backend failure waits for native TERM teardown
        // so cleanup fault assertions cannot race the worker's normal exit.
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_NEXT, c"send".as_ptr());
            assert!(!symbol.is_null());
            let original: unsafe extern "C" fn(
                libc::c_int,
                *const libc::c_void,
                usize,
                libc::c_int,
            ) -> libc::ssize_t = std::mem::transmute(symbol);
            let result = original(fd, bytes, count, flags);
            if result == 36 && count == 36 && WORKER.load(Ordering::Relaxed) {
                let header = std::slice::from_raw_parts(bytes.cast::<u8>(), count);
                if &header[..8] == b"OPAALSH\0" && header[10..12] == [0, 6] {
                    loop {
                        libc::pause();
                    }
                }
            }
            result
        }
    }

    // Report a signal error after real TERM delivery so the native owner must
    // preserve that secondary error and still reap its exact worker.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn kill(pid: libc::pid_t, signal: libc::c_int) -> libc::c_int {
        // SAFETY: RTLD_NEXT supplies the exact libc ABI; the production caller
        // owns this unreaped PID. Faults affect only this isolated test image.
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_NEXT, c"kill".as_ptr());
            assert!(!symbol.is_null());
            let original: unsafe extern "C" fn(libc::pid_t, libc::c_int) -> libc::c_int =
                std::mem::transmute(symbol);
            let result = original(pid, signal);
            if result == 0 && signal == libc::SIGTERM && FAIL_CLEANUP.load(Ordering::Relaxed) {
                CLEANUP_FAILURES.fetch_add(1, Ordering::Relaxed);
                #[cfg(target_os = "macos")]
                {
                    *libc::__error() = libc::EIO;
                }
                #[cfg(target_os = "linux")]
                {
                    *libc::__errno_location() = libc::EIO;
                }
                return -1;
            }
            result
        }
    }

    pub fn limit_sink(bytes: u64) -> libc::rlimit {
        // SAFETY: this single-threaded test changes only its own soft limit;
        // the original hard limit is retained and the soft limit is restored.
        unsafe {
            assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
            let mut previous: libc::rlimit = std::mem::zeroed();
            assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &raw mut previous), 0);
            let limited = libc::rlimit {
                rlim_cur: bytes as libc::rlim_t,
                rlim_max: previous.rlim_max,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limited), 0);
            previous
        }
    }

    pub fn restore_sink(previous: libc::rlimit) {
        // SAFETY: previous was read from this process immediately before work.
        let result = unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &raw const previous) };
        assert_eq!(result, 0);
    }
}
