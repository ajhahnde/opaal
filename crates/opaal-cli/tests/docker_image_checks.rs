#![forbid(unsafe_code)]
#[path = "support/data_processing.rs"]
mod support;

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use opaal_runtime::workflow::{
    PlanArtifact, audit_journal, digest_bytes, native_path, parse_execution_receipt,
};
use serde_json::{Value, json};
use support::{ReportDir, success};

#[test]
fn saved_image_reference_binds_version_policy_report_and_native_assessment() {
    let owner =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/docker-image-checks");
    let original = fs::read(owner.join("fixtures/image.json")).unwrap();
    for (case, revision, input, exit, assessment) in [
        ("passed", "abc123", original.clone(), 0, Some(true)),
        ("negative", "wrong", original.clone(), 1, Some(false)),
        ("invalid", "abc123", b"{\"Id\":false}".to_vec(), 2, None),
    ] {
        let work = ReportDir::new();
        for name in [
            "policy.opaal",
            "saved-tasks.opaal",
            "offline-authority.toml",
        ] {
            work.write(name, fs::read(owner.join(name)).unwrap());
        }
        work.write("opaal.toml", fs::read(owner.join("offline.toml")).unwrap());
        work.write(
            "offline-tools.toml",
            fs::read(owner.join(if cfg!(target_os = "macos") {
                "offline-tools-macos.toml"
            } else if cfg!(target_arch = "aarch64") {
                "offline-tools-linux-aarch64.toml"
            } else {
                "offline-tools-linux.toml"
            }))
            .unwrap(),
        );
        let id = format!("expected_id=sha256:{}", "a".repeat(64));
        let expected_revision = format!("expected_revision={revision}");
        let planned = work.run(&[
            "plan",
            "--project",
            "opaal.toml",
            "--task",
            "saved",
            "--environment",
            "offline",
            "--input",
            &id,
            "--input",
            "expected_platform=linux/amd64",
            "--input",
            &expected_revision,
            "--expires-in",
            "900s",
            "--out",
            "plan.json",
            "--format",
            "digest",
        ]);
        success(&planned);
        let digest = String::from_utf8(planned.stdout).unwrap();
        let plan: Value =
            serde_json::from_slice(&fs::read(work.0.join("plan.json")).unwrap()).unwrap();
        assert!(plan.to_string().contains(env!("CARGO_PKG_VERSION")));
        assert_eq!(plan["digest"], digest.trim());
        let mut child = work
            .command()
            .args([
                "execute",
                "--plan",
                "plan.json",
                "--accept",
                digest.trim(),
                "--journal",
                "run.jsonl",
                "--receipt-out",
                "receipt.json",
                "--require-true",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(exit), "{case}: {output:?}");
        if let Some(passed) = assessment {
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["passed"], passed);
            assert_eq!(report["checks"].as_array().unwrap().len(), 3);
            assert_eq!(report["checks"][2]["expected"], revision);
            assert_eq!(report["checks"][2]["actual"], "abc123");
            assert_eq!(report["checks"][2]["missing"], false);
        } else {
            assert!(output.stdout.is_empty());
        }
        let receipt =
            parse_execution_receipt(&fs::read(work.0.join("receipt.json")).unwrap()).unwrap();
        let audit = audit_journal(&fs::read(work.0.join("run.jsonl")).unwrap()).unwrap();
        assert_eq!(receipt["plan_digest"], digest.trim());
        assert_eq!(receipt["run_id"], audit.value()["run_id"]);
        assert_eq!(
            receipt["primary"]["class"],
            if assessment.is_some() {
                "success"
            } else {
                "error"
            }
        );
        assert_eq!(
            receipt["assessment"],
            assessment.map_or(Value::Null, |value| json!({"kind":"bool","value":value}))
        );
        assert_eq!(receipt["assessment"], audit.value()["assessment"]);
        assert_eq!(receipt["secondary"], json!([]));
    }
}

struct StaticTarget {
    work: ReportDir,
    socket: std::path::PathBuf,
    _listener: std::os::unix::net::UnixListener,
}
impl StaticTarget {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let work = ReportDir::new();
        #[cfg(target_os = "macos")]
        let socket = std::path::PathBuf::from(format!(
            "/private/tmp/opaal-target-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        #[cfg(target_os = "linux")]
        let socket = std::path::PathBuf::from(format!(
            "/tmp/opaal-target-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        for name in ["docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"] {
            fs::create_dir(work.0.join(name)).unwrap();
        }
        work.write("docker", b"this static canary must never execute");
        let path = native_path(&work.0.join("docker"));
        let platform = support::host_platform_triple();
        work.write("opaal.toml",format!("schema_version=1\n[project]\nname='image_check'\nroot_module='tasks.opaal'\nrequired_opaal='>=1.9.0,<2.0.0'\n[paths]\nroot='.'\nevidence='output'\n[tools.docker]\nadapter='docker'\nversion='=29.9.0'\n[targets.ci]\nadapter='docker'\ntool='docker'\nsocket={{encoding='base64url-nopad',platform='unix',value='{}'}}\nconfig_directory='docker-empty'\napi_version='1.48'\n[environments.ci]\nauthority='authority.toml'\ntool_lock='tools.toml'\n",native_path(&socket)["value"].as_str().unwrap()));
        work.write("tasks.opaal","import std::docker as docker\nimport project::targets as targets\nimport project::tools as tools\naction verify() -> Bool { effects { process.run(tools::docker), } \nlet image = docker::inspect_image(targets::ci, 'example:test')\nreturn image.os == 'linux'\n}\ntask verify = verify\n");
        work.write("authority.toml","schema_version=1\nproject='image_check'\nenvironment='ci'\n[[rules]]\ndecision='grant'\neffect='process.run'\nscope='tool.docker'\nrequired_enforcement='acknowledge-unenforced'\n");
        work.write("tools.toml",format!("schema_version=1\nproject='image_check'\nenvironment='ci'\nplatform='{platform}'\n[child_environment]\ninherit=[]\n[[tools]]\nid='docker'\nadapter='docker'\npath={{encoding='base64url-nopad',platform='unix',value='{}'}}\nversion='29.9.0'\ndigest='{}'\n",path["value"].as_str().unwrap(),digest_bytes(b"this static canary must never execute")));
        Self {
            work,
            socket,
            _listener: listener,
        }
    }
    fn plan(&self) -> std::process::Output {
        self.work.run(&[
            "plan",
            "--project",
            "opaal.toml",
            "--task",
            "verify",
            "--environment",
            "ci",
            "--out",
            "plan.json",
            "--expires-in",
            "900s",
        ])
    }
}
impl Drop for StaticTarget {
    fn drop(&mut self) {
        fs::remove_file(&self.socket).unwrap();
    }
}

#[test]
fn static_target_plan_seals_native_facts_without_executing_the_canary() {
    let fixture = StaticTarget::new();
    let discovered = fixture.work.run(&[
        "task",
        "list",
        "--project",
        "opaal.toml",
        "--format",
        "json",
    ]);
    success(&discovered);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&discovered.stdout).unwrap()["targets"],
        json!([{
            "id": "image_check::targets::ci",
            "adapter": "docker",
            "tool": "docker",
            "socket": native_path(&fixture.socket),
            "config_directory": native_path(&fixture.work.0.join("docker-empty")),
            "api_version": "1.48"
        }])
    );
    let output = fixture.plan();
    assert_eq!(
        output.status.success(),
        !cfg!(target_os = "macos"),
        "{output:?}"
    );
    let plan = PlanArtifact::parse(&fs::read(fixture.work.0.join("plan.json")).unwrap()).unwrap();
    let document = plan.value();
    assert_eq!(document["schema"], "opaal.plan.v4");
    assert_eq!(document["project"]["targets"].as_array().unwrap().len(), 1);
    let target = &document["project"]["targets"][0];
    assert_eq!(target["id"], "image_check::targets::ci");
    assert_eq!(target["profile"], "docker-cli-image-inspect.v1");
    assert_eq!(target["socket"], native_path(&fixture.socket));
    assert_eq!(
        target["safe_directories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["config", "home", "path", "tmp"]
    );
    assert_eq!(
        target["socket_identity"]["mode"].as_u64().unwrap() & 0o170000,
        0o140000
    );
    let observation = document["observations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "target")
        .unwrap();
    assert_eq!(
        observation["digest"],
        opaal_runtime::workflow::digest_value(target).unwrap()
    );
    assert!(observation["size"].is_null());
    for (path, value) in [
        ("id", json!("image_check::targets::other")),
        ("adapter", json!("git")),
        ("api_version", json!("1.47")),
        ("profile", json!("unknown-profile")),
        (
            "profile_digest",
            json!(format!("sha256:{}", "0".repeat(64))),
        ),
        ("tool", json!("other")),
        (
            "socket_identity",
            json!({"device":0,"inode":0,"mode":0o040700,"uid":0,"gid":0}),
        ),
    ] {
        let mut changed = document.clone();
        changed["project"]["targets"][0][path] = value;
        assert!(PlanArtifact::seal(changed).is_err(), "{path}");
    }
    let mut old = document.clone();
    old["schema"] = json!("opaal.plan.v3");
    old["schema_version"] = json!(3);
    assert!(PlanArtifact::seal(old).is_err());
    let mut tampered = document.clone();
    tampered["project"]["targets"][0]["socket_identity"]["inode"] = json!(0);
    assert!(PlanArtifact::parse(&serde_json::to_vec(&tampered).unwrap()).is_err());
    let mut missing = document.clone();
    missing["project"]["targets"] = json!([]);
    assert!(PlanArtifact::seal(missing).is_err());
    for (pointer, value) in [
        (
            "/project/targets/0/config_directory",
            document["project"]["root"].clone(),
        ),
        (
            "/project/targets/0/safe_directories/0/path",
            native_path(Path::new("/")),
        ),
        (
            "/project/targets/0/safe_directories/1/role",
            json!("config"),
        ),
        (
            "/project/targets/0/safe_directories/0/mode",
            json!(0o140700),
        ),
        (
            "/project/targets/0/socket_identity/uid",
            json!(u64::from(u32::MAX) + 1),
        ),
        (
            "/project/targets/0/socket",
            native_path(Path::new("tcp://localhost:2375")),
        ),
        ("/tools/0/locked_version", json!("29.8.2")),
        ("/tools/0/adapter", json!("git")),
    ] {
        let mut changed = document.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(PlanArtifact::seal(changed).is_err(), "{pointer}");
    }
    for pointer in [
        "/project/targets/0",
        "/project/targets/0/socket_identity",
        "/project/targets/0/safe_directories/0",
    ] {
        let mut changed = document.clone();
        changed.pointer_mut(pointer).unwrap()["raw-metadata-canary"] = json!("credential-canary");
        assert!(PlanArtifact::seal(changed).is_err(), "{pointer}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn changed_bound_target_facts_refuse_before_process_or_evidence_creation() {
    for changed in [
        "opaal.toml",
        "tasks.opaal",
        "tools.toml",
        "authority.toml",
        "docker",
        "docker-empty",
        "empty-path",
        "sandbox-home",
        "sandbox-tmp",
        "socket",
    ] {
        let fixture = StaticTarget::new();
        success(&fixture.plan());
        let plan =
            PlanArtifact::parse(&fs::read(fixture.work.0.join("plan.json")).unwrap()).unwrap();
        if changed == "socket" {
            fs::remove_file(&fixture.socket).unwrap();
            let _replacement = std::os::unix::net::UnixListener::bind(&fixture.socket).unwrap();
        } else if ["docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"].contains(&changed) {
            fixture
                .work
                .write(&format!("{changed}/entry"), b"credential-canary");
        } else {
            let mut bytes = fs::read(fixture.work.0.join(changed)).unwrap();
            bytes.extend_from_slice(b"\n");
            fixture.work.write(changed, bytes);
        }
        let output = fixture.work.run(&[
            "execute",
            "--plan",
            "plan.json",
            "--accept",
            plan.digest(),
            "--journal",
            "run.jsonl",
            "--receipt-out",
            "receipt.json",
            "--require-true",
        ]);
        assert!(!output.status.success(), "{changed}: {output:?}");
        assert!(output.stdout.is_empty(), "{changed}: {output:?}");
        assert!(!fixture.work.0.join("run.jsonl").exists(), "{changed}");
        assert!(!fixture.work.0.join("receipt.json").exists(), "{changed}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("credential-canary"));
    }
}

#[test]
fn target_binding_refuses_nonempty_and_symlinked_directories_without_reading_contents() {
    for symlink in [false, true] {
        let fixture = StaticTarget::new();
        if symlink {
            fs::remove_dir(fixture.work.0.join("docker-empty")).unwrap();
            std::os::unix::fs::symlink(
                fixture.work.0.join("sandbox-home"),
                fixture.work.0.join("docker-empty"),
            )
            .unwrap();
        } else {
            fixture
                .work
                .write("docker-empty/entry", b"do-not-read-canary");
        }
        let output = fixture.plan();
        assert!(!output.status.success());
        assert!(!fixture.work.0.join("plan.json").exists());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("do-not-read-canary"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn stream_preflight_observes_the_socket_without_opening_it_as_a_control_file() {
    let fixture = StaticTarget::new();
    let tasks = fixture.work.0.join("tasks.opaal");
    fixture.work.write(
        "tasks.opaal",
        fs::read_to_string(&tasks).unwrap().replace(
            "process.run(tools::docker),",
            "process.run(tools::docker), stdout.write(), stderr.write(),",
        ),
    );
    fixture.work.write(
        "authority.toml",
        fs::read_to_string(fixture.work.0.join("authority.toml")).unwrap()
            + "\n[[rules]]\ndecision='grant'\neffect='stdout.write'\nscope='evaluation'\nrequired_enforcement='enforced'\n\n[[rules]]\ndecision='grant'\neffect='stderr.write'\nscope='evaluation'\nrequired_enforcement='enforced'\n",
    );
    success(&fixture.plan());
    let plan = PlanArtifact::parse(&fs::read(fixture.work.0.join("plan.json")).unwrap()).unwrap();
    let output = fixture.work.run(&[
        "execute",
        "--plan",
        "plan.json",
        "--accept",
        plan.digest(),
        "--journal",
        "run.jsonl",
        "--receipt-out",
        "receipt.json",
        "--require-true",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("PROJECT024"),
        "{output:?}"
    );
    let receipt =
        parse_execution_receipt(&fs::read(fixture.work.0.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["primary"]["class"], "error");
    assert_eq!(receipt["assessment"], Value::Null);
    let journal = fs::read(fixture.work.0.join("run.jsonl")).unwrap();
    let audit = audit_journal(&journal).unwrap();
    assert_eq!(audit.value()["completeness"], "complete");
    assert_eq!(audit.value()["primary"], receipt["primary"]);
    assert_eq!(
        std::str::from_utf8(&journal)
            .unwrap()
            .matches("\"effect-before\"")
            .count(),
        1
    );
}
