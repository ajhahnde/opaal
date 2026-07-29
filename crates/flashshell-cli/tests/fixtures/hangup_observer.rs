#![deny(unsafe_code)]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::Duration;

use flashshell_platform::Platform;
use flashshell_platform_posix::PosixPlatform;
use rustix::process::getpgrp;

fn main() {
    PosixPlatform
        .ignore_hangup()
        .expect("the observer should be able to ignore hang-up");

    let directory =
        PathBuf::from(env::var_os("FLASH_PROBE_GROUP_REPORT").expect("report path is required"));
    let release =
        PathBuf::from(env::var_os("FLASH_PROBE_HOLD_UNTIL").expect("release path is required"));
    let report = directory.join(format!("{}.group", process::id()));
    fs::write(report, getpgrp().as_raw_nonzero().to_string())
        .expect("process group report should be written");

    while !release.exists() {
        thread::sleep(Duration::from_millis(10));
    }
}
