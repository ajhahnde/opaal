#![forbid(unsafe_code)]

use std::fs::File;

use opaal_platform::operational::{OperationalAdapter, OperationalErrorKind};
use opaal_platform_posix::operational::PosixOperationalAdapter;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
static SNAPSHOT_OBSERVER: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn observe_native_image(kind: &str, path: &std::path::Path, snapshot: &File) {
    use std::os::unix::fs::FileExt;

    let original = std::fs::read(path).unwrap();
    let mut retained = vec![0; original.len()];
    snapshot.read_exact_at(&mut retained, 0).unwrap();
    assert_eq!(retained, original);
    assert_eq!(snapshot.metadata().unwrap().len(), original.len() as u64);
    let hashed = std::process::Command::new("/usr/bin/sha256sum")
        .arg(path)
        .output()
        .unwrap();
    assert!(hashed.status.success() && hashed.stderr.is_empty());
    let digest = std::str::from_utf8(&hashed.stdout[..64]).unwrap();
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    println!("OPAAL_NATIVE_IMAGE {{\"kind\":\"{kind}\",\"digest\":\"sha256:{digest}\"}}");
}

fn observe_image_case(name: &str, observed: &str) {
    let host = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "aarch64-apple-darwin"
    };
    // Frozen inventory identity prevents reuse against a different corpus.
    println!(
        "OPAAL_IMAGE_CASE {{\"name\":\"{name}\",\"observed\":{observed},\"host\":\"{host}\",\"inventory_digest\":\"sha256:03bb3cc4f8ea719206db79f4475dd12d530b5df572e2a1179d77f0aabc1c5686\"}}"
    );
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn snapshot_handles() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.unwrap().path()).ok())
        .filter(|path| {
            path.as_os_str()
                .as_encoded_bytes()
                .starts_with(b"/memfd:opaal-native")
        })
        .count()
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
#[ignore = "requires the qualifier's attached syscall fault observer"]
fn image_snapshot_syscall_fault() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let _observer = SNAPSHOT_OBSERVER.lock().unwrap();
    let root = std::path::PathBuf::from(std::env::var_os("OPAAL_NATIVE_FAULT_ROOT").unwrap());
    let name = std::env::var("OPAAL_NATIVE_FAULT_CASE").unwrap();
    let path = root.join("source");
    std::fs::write(&path, vec![1; 128 * 1024]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = File::open(path).unwrap();
    assert_eq!(snapshot_handles(), 0);
    std::fs::write(root.join("ready"), b"ready").unwrap();
    let start = Instant::now();
    while !root.join("release").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "tracer did not attach"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let error = PosixOperationalAdapter::new()
        .snapshot_executable(&source, 128 * 1024, &|| false)
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::Unsupported);
    assert_eq!(snapshot_handles(), 0);
    observe_image_case(
        &name,
        &format!(
            "{{\"error\":\"unsupported\",\"attempts\":0,\"fallback_attempts\":0,\"remaining_snapshot_handles\":{}}}",
            snapshot_handles()
        ),
    );
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn image_maintained_script_with_seal_support_preserves_interpreter_descriptor() {
    use std::ffi::OsString;
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    use std::path::Path;
    use std::time::Duration;

    use opaal_platform::operational::{ProcessExit, ProcessRequest};
    use rustix::fs::{MemfdFlags, Mode, SealFlags};

    // Without ALLOW_SEALING, memfd has the same SEAL-only state as an ordinary tmpfs file.
    let mut storage = File::from(
        rustix::fs::memfd_create(c"maintained-script", MemfdFlags::EXEC | MemfdFlags::CLOEXEC)
            .unwrap(),
    );
    storage
        .write_all(b"#!/bin/sh\nprintf 'descriptor'\n")
        .unwrap();
    rustix::fs::fchmod(&storage, Mode::RWXU).unwrap();
    let retained = File::open(format!("/proc/self/fd/{}", storage.as_raw_fd())).unwrap();
    drop(storage);
    assert_eq!(
        rustix::fs::fcntl_get_seals(&retained).unwrap(),
        SealFlags::SEAL
    );
    let argv = [OsString::from("maintained-script")];
    let output = PosixOperationalAdapter::new()
        .run_process(
            ProcessRequest {
                executable: Path::new("/usr/bin/false"),
                executable_file: Some(&retained),
                argv: &argv,
                environment: &[],
                cwd: Path::new("/tmp"),
                stdout_limit: 10,
                stderr_limit: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap();
    assert_eq!(output.status(), ProcessExit::Exited(0));
    assert_eq!(output.stdout(), b"descriptor");
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
#[test]
fn native_frozen_image_snapshots_refuse_unsupported_hosts() {
    let source = File::open(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(
        PosixOperationalAdapter::new()
            .snapshot_executable(&source, 64 * 1024 * 1024, &|| false)
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Unsupported
    );
    observe_image_case(
        "macos-live-refusal",
        r#"{"error":"unsupported","attempts":0,"fallback_attempts":0}"#,
    );
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn native_frozen_image_seals_permissions_cancellation_and_descriptor_launch() {
    use std::ffi::OsString;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{FileExt, PermissionsExt};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use opaal_platform::operational::{ProcessExit, ProcessRequest};
    use rustix::fs::SealFlags;

    let _observer = SNAPSHOT_OBSERVER.lock().unwrap();
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let root = Scratch(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("native-image-{}", std::process::id())),
    );
    fs::create_dir(&root.0).unwrap();
    let adapter = PosixOperationalAdapter::new();
    assert_eq!(snapshot_handles(), 0);
    for (kind, fixture) in [
        ("static", include_str!("fixtures/native-static.c")),
        ("dynamic", include_str!("fixtures/native-dynamic.c")),
    ] {
        let source_path = root.0.join(format!("{kind}.c"));
        let path = root.0.join(kind);
        fs::write(&source_path, fixture).unwrap();
        let mut cc = Command::new("/usr/bin/cc");
        cc.args(["-O2", "-Wall", "-Wextra", "-Werror"]);
        if kind == "static" {
            cc.args([
                "-ffreestanding",
                "-nostdlib",
                "-static",
                "-no-pie",
                "-fno-stack-protector",
            ]);
        }
        let built = cc.arg(&source_path).arg("-o").arg(&path).output().unwrap();
        assert!(
            built.status.success(),
            "{kind}: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let source = File::open(&path).unwrap();
        let original = fs::read(&path).unwrap();
        let snapshot = adapter
            .snapshot_executable(&source, original.len(), &|| false)
            .unwrap();
        observe_native_image(kind, &path, &snapshot);
        let seals = SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL;
        assert!(
            rustix::fs::fcntl_get_seals(&snapshot)
                .unwrap()
                .contains(seals)
        );
        assert!(
            rustix::io::fcntl_getfd(&snapshot)
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        assert!(snapshot.as_raw_fd() < 1024);
        assert!(snapshot.write_at(b"changed", 0).is_err());
        assert!(snapshot.set_len(0).is_err());
        assert!(snapshot.set_len(original.len() as u64 + 1).is_err());
        for mutation in ["none", "in-place", "path", "empty-args"] {
            if kind == "static" && mutation != "none" {
                continue;
            }
            if mutation == "in-place" {
                fs::write(&path, b"changed original inode").unwrap();
            } else if mutation == "path" {
                fs::write(&path, &original).unwrap();
                fs::rename(&path, path.with_extension("replaced")).unwrap();
                fs::write(&path, b"replacement pathname").unwrap();
            }
            let mut argv = vec![OsString::from("original argv zero")];
            if mutation != "empty-args" {
                argv.push(OsString::from("literal ; $() * unicode-ä"));
            }
            let output = adapter
                .run_process(
                    ProcessRequest {
                        executable: &path,
                        executable_file: Some(&snapshot),
                        argv: &argv,
                        environment: &[],
                        cwd: &root.0,
                        stdout_limit: 1024,
                        stderr_limit: 1024,
                        timeout: Duration::from_secs(2),
                    },
                    &|| false,
                )
                .unwrap();
            assert_eq!(output.status(), ProcessExit::Exited(0));
            assert_eq!(
                output.stdout(),
                if mutation == "empty-args" {
                    b"original argv zero\n".as_slice()
                } else {
                    b"original argv zero\nliteral ; $() * unicode-\xc3\xa4\n"
                }
            );
            assert!(output.stderr().is_empty());
            assert_eq!(snapshot_handles(), 1, "launch duplicate leaked");
            let name = match (kind, mutation) {
                ("static", "none") => "static-sealed-execution",
                ("dynamic", "none") => "dynamic-sealed-execution",
                (_, "in-place") => "post-seal-source-inplace-mutation",
                (_, "path") => "post-seal-path-replacement",
                (_, "empty-args") => "empty-argument-list",
                _ => unreachable!(),
            };
            observe_image_case(name, r#"{"exit":0,"attempts":1,"fallback_attempts":0}"#);
            if kind == "dynamic" && mutation == "none" {
                observe_image_case(
                    "snapshot-descriptors-close-on-exec",
                    r#"{"exit":0,"attempts":1,"fallback_attempts":0}"#,
                );
            }
        }
    }
    assert_eq!(snapshot_handles(), 0);
    let path = root.0.join("admission");
    fs::write(&path, [1; 128 * 1024]).unwrap();
    let source = File::open(&path).unwrap();
    for (mode, name) in [
        (0o600, "original-nonexec"),
        (0o4700, "original-setuid"),
        (0o2700, "original-setgid"),
    ] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            adapter
                .snapshot_executable(&source, 128 * 1024, &|| false)
                .unwrap_err()
                .kind(),
            OperationalErrorKind::Unsupported
        );
        assert_eq!(snapshot_handles(), 0);
        observe_image_case(
            name,
            &format!(
                "{{\"error\":\"unsupported\",\"attempts\":0,\"fallback_attempts\":0,\"remaining_snapshot_handles\":{}}}",
                snapshot_handles()
            ),
        );
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    for boundary in 2..7 {
        fs::write(&path, [1; 128 * 1024]).unwrap();
        let polls = AtomicUsize::new(0);
        let snapshot = adapter
            .snapshot_executable(&source, 128 * 1024, &|| {
                if polls.fetch_add(1, Ordering::SeqCst) == boundary {
                    fs::write(&path, [2; 128 * 1024]).unwrap();
                }
                false
            })
            .unwrap();
        let mut actual = vec![0; 128 * 1024];
        snapshot.read_exact_at(&mut actual, 0).unwrap();
        let mut expected = vec![1; 128 * 1024];
        if boundary == 2 {
            expected.fill(2);
        }
        if boundary == 3 {
            expected[64 * 1024..].fill(2);
        }
        assert_eq!(actual, expected, "mutation at acquisition poll {boundary}");
        assert_eq!(fs::read(&path).unwrap(), vec![2; 128 * 1024]);
        assert!(snapshot.write_at(b"post-seal change", 0).is_err());
    }
    assert_eq!(
        adapter
            .snapshot_executable(&source, 128 * 1024 - 1, &|| false)
            .unwrap_err()
            .kind(),
        OperationalErrorKind::LimitExceeded
    );
    for boundary in 0..7 {
        let polls = AtomicUsize::new(0);
        let result = adapter.snapshot_executable(&source, 128 * 1024, &|| {
            polls.fetch_add(1, Ordering::SeqCst) >= boundary
        });
        assert_eq!(
            result.unwrap_err().kind(),
            OperationalErrorKind::Cancelled,
            "poll {boundary}"
        );
        assert_eq!(snapshot_handles(), 0, "cancelled acquisition leaked");
    }
    for (name, size, grow) in [
        ("image-first-excess", 64 * 1024 * 1024 + 1, false),
        ("image-growth-during-copy", 64 * 1024 * 1024, true),
    ] {
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(size)
            .unwrap();
        let polls = AtomicUsize::new(0);
        let result = adapter.snapshot_executable(&source, 64 * 1024 * 1024, &|| {
            if grow && polls.fetch_add(1, Ordering::SeqCst) == 1 {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(size + 1)
                    .unwrap();
            }
            false
        });
        assert_eq!(
            result.unwrap_err().kind(),
            OperationalErrorKind::LimitExceeded
        );
        assert_eq!(snapshot_handles(), 0);
        observe_image_case(
            name,
            &format!(
                "{{\"error\":\"limit\",\"attempts\":0,\"fallback_attempts\":0,\"remaining_snapshot_handles\":{}}}",
                snapshot_handles()
            ),
        );
    }
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn native_frozen_lifetime_output_deadline_cancellation_and_owned_group() {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use opaal_platform::operational::{ProcessExit, ProcessRequest};

    let _observer = SNAPSHOT_OBSERVER.lock().unwrap();
    let root = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("native-lifetime-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let path = root.join("observer");
    let source_path = root.join("observer.c");
    fs::write(&source_path, include_str!("fixtures/native-dynamic.c")).unwrap();
    let built = Command::new("/usr/bin/cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(&source_path)
        .arg("-o")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let adapter = PosixOperationalAdapter::new();
    let source = File::open(&path).unwrap();
    let environment = [(
        OsString::from("EXPLICIT"),
        OsString::from("literal ; $() ä"),
    )];
    for (mode, limit, timeout, cancel, expected) in [
        ("exit", 1024, 2000, false, None),
        ("signal", 1024, 2000, false, None),
        ("context", 4096, 2000, false, None),
        ("output", 1024, 2000, false, None),
        (
            "output",
            1023,
            2000,
            false,
            Some(OperationalErrorKind::LimitExceeded),
        ),
        ("stdout-exact", 8 * 1024 * 1024, 5000, false, None),
        (
            "stdout-first-excess",
            8 * 1024 * 1024,
            5000,
            false,
            Some(OperationalErrorKind::LimitExceeded),
        ),
        ("stderr-exact", 8 * 1024 * 1024, 5000, false, None),
        (
            "stderr-first-excess",
            8 * 1024 * 1024,
            5000,
            false,
            Some(OperationalErrorKind::LimitExceeded),
        ),
        ("sleep", 0, 100, false, Some(OperationalErrorKind::TimedOut)),
        (
            "sleep",
            0,
            2000,
            true,
            Some(OperationalErrorKind::Cancelled),
        ),
        (
            "descendant",
            0,
            100,
            false,
            Some(OperationalErrorKind::TimedOut),
        ),
    ] {
        let snapshot = adapter
            .snapshot_executable(&source, 1024 * 1024, &|| false)
            .unwrap();
        if mode == "exit" {
            observe_native_image("lifetime", &path, &snapshot);
        }
        assert_eq!(snapshot_handles(), 1);
        let marker = root.join(format!("{mode}-{limit}-{cancel}.pids"));
        let argv = [
            path.as_os_str().to_owned(),
            "--lifetime".into(),
            mode.into(),
            marker.as_os_str().to_owned(),
        ];
        let start = Instant::now();
        let result = adapter.run_process(
            ProcessRequest {
                executable: &path,
                executable_file: Some(&snapshot),
                argv: &argv,
                environment: &environment,
                cwd: &root,
                stdout_limit: limit,
                stderr_limit: limit,
                timeout: Duration::from_millis(timeout),
            },
            &|| cancel && start.elapsed() >= Duration::from_millis(100),
        );
        assert!(
            start.elapsed() < Duration::from_secs(11),
            "{mode} cleanup was unbounded"
        );
        if let Some(kind) = expected {
            assert_eq!(result.unwrap_err().kind(), kind, "{mode}");
        } else {
            let output = result.unwrap();
            match mode {
                "exit" => assert_eq!(output.status(), ProcessExit::Exited(17)),
                "signal" => assert_eq!(output.status(), ProcessExit::Signaled(10)),
                "context" => {
                    assert_eq!(output.status(), ProcessExit::Exited(0));
                    assert_eq!(
                        output.stdout(),
                        format!("{}\nliteral ; $() ä\n", root.display()).as_bytes()
                    );
                }
                "output" => {
                    assert_eq!(output.status(), ProcessExit::Exited(0));
                    assert_eq!(output.stdout(), vec![b'o'; 1024]);
                    assert_eq!(output.stderr(), vec![b'e'; 1024]);
                }
                "stdout-exact" | "stderr-exact" => {
                    assert_eq!(output.status(), ProcessExit::Exited(0));
                    let (full, empty) = if mode == "stdout-exact" {
                        (output.stdout(), output.stderr())
                    } else {
                        (output.stderr(), output.stdout())
                    };
                    assert_eq!(full, vec![b'x'; 8 * 1024 * 1024]);
                    assert!(empty.is_empty());
                }
                _ => unreachable!(),
            }
        }
        let pids = fs::read_to_string(marker).unwrap();
        assert_eq!(
            pids.lines().count(),
            if mode == "descendant" { 2 } else { 1 },
            "no retry"
        );
        let mut running = 0;
        for pid in pids.lines() {
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                running += usize::from(!stat.split_once(") ").unwrap().1.starts_with('Z'));
            }
        }
        assert_eq!(running, 0, "owned child still runs");
        assert_eq!(snapshot_handles(), 1, "launch duplicate leaked");
        drop(snapshot);
        assert_eq!(snapshot_handles(), 0);
        match mode {
            "context" => observe_image_case(
                "literal-argv-environment-cwd",
                r#"{"exit":0,"attempts":1,"fallback_attempts":0}"#,
            ),
            "exit" => observe_image_case(
                "nonzero-status-is-data",
                r#"{"status_code":17,"completed":true}"#,
            ),
            "signal" => {
                observe_image_case("signal-status-is-data", r#"{"signal":10,"completed":true}"#)
            }
            "stdout-exact" | "stderr-exact" => observe_image_case(mode, r#"{"completed":true}"#),
            "stdout-first-excess" | "stderr-first-excess" => {
                observe_image_case(mode, r#"{"error":"limit"}"#)
            }
            "sleep" => observe_image_case(
                if cancel {
                    "running-child-requested"
                } else {
                    "running-child-deadline"
                },
                &format!(
                    "{{\"exit\":{},\"attempts\":1,\"remaining_snapshot_handles\":{}}}",
                    if cancel { 130 } else { 2 },
                    snapshot_handles()
                ),
            ),
            "descendant" => observe_image_case(
                "term-resistant-group",
                &format!(
                    "{{\"exit\":2,\"owned_running_processes\":{running},\"cleanup_ms_at_most\":11000}}"
                ),
            ),
            _ => {}
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
#[ignore = "requires an isolated noexec mount; set OPAAL_NATIVE_NOEXEC_ROOT"]
fn image_original_noexec_mount_refuses() {
    use std::os::unix::fs::PermissionsExt;
    let _observer = SNAPSHOT_OBSERVER.lock().unwrap();
    let root = std::env::var_os("OPAAL_NATIVE_NOEXEC_ROOT").expect("noexec test mount");
    let path = std::path::Path::new(&root).join("native-noexec");
    std::fs::write(&path, b"admission only").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = File::open(&path).unwrap();
    assert_eq!(
        PosixOperationalAdapter::new()
            .snapshot_executable(&source, 1024, &|| false)
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Unsupported
    );
    assert_eq!(snapshot_handles(), 0);
    observe_image_case(
        "original-noexec-mount",
        r#"{"error":"unsupported","attempts":0,"fallback_attempts":0,"remaining_snapshot_handles":0}"#,
    );
    std::fs::remove_file(path).unwrap();
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
#[ignore = "requires setfacl without DAC override"]
fn image_original_acl_refuses() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let _observer = SNAPSHOT_OBSERVER.lock().unwrap();
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("native-security-{}", std::process::id()));
    std::fs::write(&path, b"admission only").unwrap();
    let source = File::open(&path).unwrap();
    let adapter = PosixOperationalAdapter::new();
    assert!(
        Command::new("setfacl")
            .args(["-m", "u::rw-,g::r-x,m::r-x,o::---"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert_ne!(source.metadata().unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(
        adapter
            .snapshot_executable(&source, 1024, &|| false)
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Unsupported
    );
    assert!(
        Command::new("setfacl")
            .arg("-b")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        adapter
            .snapshot_executable(&source, 1024, &|| false)
            .is_ok()
    );
    assert_eq!(snapshot_handles(), 0);
    observe_image_case(
        "original-acl-denies-effective-user",
        r#"{"error":"unsupported","attempts":0,"fallback_attempts":0,"remaining_snapshot_handles":0}"#,
    );
    std::fs::remove_file(path).unwrap();
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
#[ignore = "requires setcap in an isolated user namespace"]
fn image_original_capabilities_refuse() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let _observer = SNAPSHOT_OBSERVER.lock().unwrap();
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("native-capabilities-{}", std::process::id()));
    std::fs::write(&path, b"admission only").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = File::open(&path).unwrap();
    let adapter = PosixOperationalAdapter::new();
    assert!(
        adapter
            .snapshot_executable(&source, 1024, &|| false)
            .is_ok()
    );
    assert!(
        Command::new("setcap")
            .arg("cap_net_bind_service=ep")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        adapter
            .snapshot_executable(&source, 1024, &|| false)
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Unsupported
    );
    assert!(
        Command::new("setcap")
            .arg("-r")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        adapter
            .snapshot_executable(&source, 1024, &|| false)
            .is_ok()
    );
    assert_eq!(snapshot_handles(), 0);
    observe_image_case(
        "original-file-capability",
        r#"{"error":"unsupported","attempts":0,"fallback_attempts":0,"remaining_snapshot_handles":0}"#,
    );
    std::fs::remove_file(path).unwrap();
}
