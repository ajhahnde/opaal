#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use opaal_cli::project::{InitProjectRequest, init_project};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "prepare-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn request(&self, name: impl AsRef<Path>) -> InitProjectRequest {
        InitProjectRequest {
            destination: self.0.join(name),
            environment: "offline".to_owned(),
            docker: None,
            socket: None,
            qualified_version: None,
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn fixed_offline_assets_match_maintained_sources_and_private_modes() {
    let directory = Directory::new();
    let request = directory.request("image-check");
    init_project(&request).unwrap();
    let owner = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/image-check");
    for name in [
        "README.md",
        "policy.opaal",
        "source.opaal",
        "saved-tasks.opaal",
        "tasks.opaal",
        "fixtures/image.json",
        "fixtures/invalid.json",
        "decode/cases.json",
        "operations/cases.json",
        "live/cases.json",
        "docker-cli-profile.json",
        "offline-authority.toml",
    ] {
        assert_eq!(
            fs::read(request.destination.join(name)).unwrap(),
            fs::read(owner.join(name)).unwrap(),
            "{name}"
        );
        assert_eq!(
            fs::metadata(request.destination.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::read(request.destination.join("opaal.toml")).unwrap(),
        fs::read(owner.join("offline.toml")).unwrap()
    );
    let host = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    assert_eq!(
        fs::read(request.destination.join("offline-tools.toml")).unwrap(),
        fs::read(owner.join(format!("offline-tools-{host}.toml"))).unwrap()
    );
    assert_eq!(
        fs::metadata(&request.destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        fs::read_to_string(request.destination.join("opaal.toml"))
            .unwrap()
            .contains(&format!(">={},<2.0.0", env!("CARGO_PKG_VERSION")))
    );
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .current_dir(&request.destination)
        .env_clear()
        .args([
            "task",
            "inspect",
            "--project",
            "opaal.toml",
            "saved",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        Command::new(env!("CARGO_BIN_EXE_opaal"))
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        format!("opaal {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
    );
}

#[test]
fn existing_directory_file_symlink_and_fifo_are_preserved() {
    let directory = Directory::new();
    fs::create_dir(directory.0.join("existing")).unwrap();
    fs::write(directory.0.join("existing/canary"), b"untouched").unwrap();
    fs::write(directory.0.join("file"), b"untouched").unwrap();
    symlink("existing", directory.0.join("link")).unwrap();
    symlink("missing", directory.0.join("dangling")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(directory.0.join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    for name in ["existing", "file", "link", "dangling", "fifo"] {
        assert!(init_project(&directory.request(name)).is_err(), "{name}");
        let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
            .args([
                "project",
                "init",
                "--example",
                "image-check",
                "--destination",
            ])
            .arg(directory.0.join(name))
            .args(["--environment", "offline"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(output.stdout.is_empty());
    }
    assert_eq!(
        fs::read(directory.0.join("existing/canary")).unwrap(),
        b"untouched"
    );
    assert_eq!(fs::read(directory.0.join("file")).unwrap(), b"untouched");
    assert_eq!(
        fs::read_link(directory.0.join("dangling")).unwrap(),
        Path::new("missing")
    );
    assert_eq!(
        fs::read_dir(directory.0.join("existing")).unwrap().count(),
        1
    );
}

#[test]
fn completed_setup_with_broken_output_returns_two_and_keeps_files() {
    let directory = Directory::new();
    let destination = directory.0.join("broken-output");
    let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(reader);
    let descriptor: std::os::fd::OwnedFd = writer.into();
    let output = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args([
            "project",
            "init",
            "--example",
            "image-check",
            "--destination",
        ])
        .arg(&destination)
        .args(["--environment", "offline"])
        .stdout(std::process::Stdio::from(descriptor))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(destination.join("opaal.toml").is_file());
    assert!(output.stdout.is_empty());
}

#[test]
fn native_destination_and_parent_refusals() {
    let directory = Directory::new();
    let request = directory.request(OsString::from_vec(b"native-\xff".to_vec()));
    let manifest = opaal_runtime::project::parse_project_manifest(
        &request.destination.join("opaal.toml"),
        include_bytes!("../../../examples/image-check/offline.toml"),
    )
    .unwrap();
    opaal_runtime::project::parse_authority_document(
        &manifest,
        "offline",
        include_bytes!("../../../examples/image-check/offline-authority.toml"),
    )
    .unwrap();
    opaal_runtime::project::parse_tool_lock(
        &manifest,
        "offline",
        if cfg!(target_os = "macos") {
            include_bytes!("../../../examples/image-check/offline-tools-macos.toml").as_slice()
        } else {
            include_bytes!("../../../examples/image-check/offline-tools-linux.toml").as_slice()
        },
    )
    .unwrap();
    if cfg!(target_os = "linux") {
        init_project(&request).unwrap();
        assert!(request.destination.join("opaal.toml").is_file());
    } else {
        assert!(init_project(&request).is_err());
        assert!(!request.destination.exists());
    }
    symlink(&directory.0, directory.0.join("parent-link")).unwrap();
    assert!(init_project(&directory.request("parent-link/project")).is_err());
    assert!(init_project(&directory.request("missing/project")).is_err());
    assert!(init_project(&directory.request("parent-link/../project")).is_err());
    assert!(!directory.0.join("project").exists());
}

#[test]
fn concurrent_initializers_have_exactly_one_winner() {
    let directory = Directory::new();
    let request = directory.request("race");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let children = (0..2)
        .map(|_| {
            let request = request.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                init_project(&request).is_ok()
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        children
            .into_iter()
            .filter_map(|child| child.join().unwrap().then_some(()))
            .count(),
        1
    );
    assert!(request.destination.join("opaal.toml").is_file());
}

#[test]
fn invalid_qualified_facts_refuse_before_creation() {
    let directory = Directory::new();
    let mut request = directory.request("live");
    request.environment = "ci".to_owned();
    request.docker = Some(directory.0.join("missing-docker"));
    request.socket = Some(directory.0.join("missing.sock"));
    request.qualified_version = Some("wrong-profile".to_owned());
    assert!(init_project(&request).is_err());
    assert!(!request.destination.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn live_setup_binds_explicit_bytes_without_launching_or_connecting() {
    use opaal_cli::project::{CheckProjectRequest, doctor_project};
    use opaal_runtime::module::AnalysisControl;
    use opaal_runtime::workflow::{digest_bytes, native_path};
    use std::os::unix::net::UnixListener;
    let directory = Directory::new();
    let executable = directory
        .0
        .join(OsString::from_vec(b"docker-\xff".to_vec()));
    let canary = directory.0.join("launched");
    let bytes = format!("#!/bin/sh\ntouch {}\n", canary.display());
    fs::write(&executable, &bytes).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.0.join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut request = directory.request("live");
    request.environment = "ci".to_owned();
    request.docker = Some(executable.clone());
    request.socket = Some(socket);
    request.qualified_version = Some("29.9.0".to_owned());
    init_project(&request).unwrap();
    assert!(!canary.exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let lock = fs::read_to_string(request.destination.join("live-tools.toml")).unwrap();
    assert!(lock.contains(&digest_bytes(bytes.as_bytes())));
    assert!(lock.contains(native_path(&executable)["value"].as_str().unwrap()));
    for name in ["docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"] {
        assert_eq!(
            fs::read_dir(request.destination.join(name))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::metadata(request.destination.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let diagnose = || {
        doctor_project(
            &CheckProjectRequest::new(
                request.destination.join("opaal.toml"),
                "image".to_owned(),
                "ci".to_owned(),
                vec![
                    ("image".to_owned(), "example:test".to_owned()),
                    ("expected_id".to_owned(), "x".to_owned()),
                    ("expected_platform".to_owned(), "x".to_owned()),
                    ("expected_revision".to_owned(), "x".to_owned()),
                ],
            )
            .with_json(),
            &AnalysisControl::never(),
        )
        .unwrap()
    };
    let run = diagnose();
    assert_eq!(run.exit_code, 0);
    let view: serde_json::Value = serde_json::from_slice(&run.output).unwrap();
    assert_eq!(view["live_readiness"], "unknown");
    assert!(!canary.exists());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    fs::write(&executable, b"changed executable").unwrap();
    let run = diagnose();
    assert_eq!(run.exit_code, 1);
    assert!(String::from_utf8(run.output).unwrap().contains("DOCTOR007"));
    fs::write(
        request.destination.join("docker-empty/canary"),
        b"unread account canary",
    )
    .unwrap();
    let run = diagnose();
    assert_eq!(run.exit_code, 1);
    assert!(String::from_utf8(run.output).unwrap().contains("DOCTOR009"));
}
