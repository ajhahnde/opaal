#![deny(unsafe_code)]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process;

fn main() {
    let report_path = env::var_os("FLASH_PROBE_REPORT").expect("report path is required");
    let value = env::var_os("FLASH_PROBE_VALUE").unwrap_or_default();
    let path = env::var_os("PATH").unwrap_or_default();
    let fd_open = env::var("FLASH_PROBE_FD")
        .ok()
        .is_some_and(|descriptor| descriptor_probe::is_open(&descriptor));
    let cwd = env::current_dir().expect("cwd should be readable");
    let argv: Vec<_> = env::args_os().collect();

    let mut report = Vec::new();
    write_field(&mut report, cwd.as_os_str());
    write_field(&mut report, &value);
    write_field(&mut report, &path);
    report.push(u8::from(fd_open));
    report.extend_from_slice(&(argv.len() as u32).to_le_bytes());
    for argument in &argv {
        write_field(&mut report, argument);
    }

    fs::write(report_path, report).expect("report should be written");

    // Written to a per-process file inside the named directory: the probe must
    // not disturb the byte layout the argv, environment, and descriptor tests
    // compare exactly, and two pipeline members sharing one environment must
    // still report separately.
    if let Some(group_directory) = env::var_os("FLASH_PROBE_GROUP_REPORT") {
        let report = PathBuf::from(group_directory).join(format!("{}.group", process::id()));
        fs::write(report, process_group::current().to_string())
            .expect("group report should be written");
    }

    // Raising a signal at itself is how the fixture reports its own disposition
    // and mask together: it survives only if the signal was ignored or blocked,
    // and the parent reads the difference straight off the exit status.
    if let Some(number) = env::var_os("FLASH_PROBE_RAISE") {
        let number: i32 = number
            .to_str()
            .and_then(|value| value.parse().ok())
            .expect("the raise probe takes a signal number");
        signal_probe::raise_signal(number);
        if let Some(directory) = env::var_os("FLASH_PROBE_GROUP_REPORT") {
            let survived = PathBuf::from(directory).join(format!("{}.survived", process::id()));
            fs::write(survived, b"survived").expect("survival report should be written");
        }
    }
}

fn write_field(output: &mut Vec<u8>, value: &OsStr) {
    let bytes = value.as_bytes();
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

#[allow(unsafe_code)]
mod process_group {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn getpgrp() -> c_int;
    }

    /// The group this process belongs to after the adapter's placement.
    pub(super) fn current() -> c_int {
        // SAFETY: getpgrp takes no argument, dereferences nothing, and is
        // specified never to fail.
        unsafe { getpgrp() }
    }
}

#[allow(unsafe_code)]
mod signal_probe {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn raise(signal: c_int) -> c_int;
    }

    /// Send `signal` to this process and return only if it survived it.
    pub(super) fn raise_signal(signal: c_int) {
        // SAFETY: raise takes one integer, dereferences nothing, and either
        // does not return or returns a status this probe does not need.
        unsafe {
            raise(signal);
        }
    }
}

#[allow(unsafe_code)]
mod descriptor_probe {
    use std::ffi::{c_int, c_void};

    unsafe extern "C" {
        fn write(descriptor: c_int, buffer: *const c_void, count: usize) -> isize;
    }

    pub(super) fn is_open(descriptor: &str) -> bool {
        let Ok(descriptor) = descriptor.parse::<c_int>() else {
            return false;
        };
        let probe = 0_u8;

        // SAFETY: `probe` remains valid for the one-byte write. POSIX defines
        // an invalid integer descriptor as an EBADF failure, not memory
        // unsafety.
        unsafe { write(descriptor, (&raw const probe).cast(), 1) == 1 }
    }
}
