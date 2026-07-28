#![deny(unsafe_code)]

//! Process-isolated proof of the shell's job-control signal arrangement.
//!
//! Signal dispositions are process-wide, so this cannot be a thread of the test
//! binary. The fixture installs the arrangement, proves the shell survives its
//! own interrupt, proves a child spawned underneath it does not inherit that
//! survival, restores the arrangement, and finally lets an interrupt kill it —
//! which is the exit status its parent asserts.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use flashshell_platform::{Platform, ProcessStatus, SpawnRequest};
use flashshell_platform_posix::PosixPlatform;

const SIGINT: i32 = 2;

fn main() {
    let report = PathBuf::from(env::var_os("FLASH_GUARD_REPORT").expect("report path is required"));
    let observer =
        PathBuf::from(env::var_os("FLASH_GUARD_OBSERVER").expect("observer path is required"));
    let workspace =
        PathBuf::from(env::var_os("FLASH_GUARD_WORKSPACE").expect("workspace is required"));
    let mut findings = Vec::new();

    let mut guard = PosixPlatform
        .install_job_control_signals()
        .expect("a POSIX host arranges job-control signals");

    raise_probe::raise_signal(SIGINT);
    findings.push("shell-survived-interrupt".to_owned());

    findings.push(format!(
        "child-status:{:?}",
        spawn_raising_child(&observer, &workspace)
    ));

    guard.restore().expect("the arrangement is restorable");
    fs::write(&report, findings.join("\n")).expect("report should be written");

    // With the arrangement restored the default disposition is back, so this
    // does not return and the parent reads SIGINT off the exit status.
    raise_probe::raise_signal(SIGINT);
    unreachable!("a restored default disposition must not survive an interrupt");
}

fn spawn_raising_child(observer: &Path, workspace: &Path) -> ProcessStatus {
    let argv = [OsString::from("raiser")];
    let environment = [
        (
            OsString::from("FLASH_PROBE_REPORT"),
            workspace.join("child-report.bin").into_os_string(),
        ),
        (OsString::from("FLASH_PROBE_RAISE"), OsString::from("2")),
    ];
    let request = SpawnRequest::new(observer, &argv, &environment, workspace)
        .expect("the spawn request is valid");
    PosixPlatform
        .spawn(&request)
        .expect("the observer spawns")
        .wait()
        .expect("the observer is waitable")
}

#[allow(unsafe_code)]
mod raise_probe {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn raise(signal: c_int) -> c_int;
    }

    pub(super) fn raise_signal(signal: c_int) {
        // SAFETY: raise takes one integer and dereferences nothing.
        unsafe {
            raise(signal);
        }
    }
}
