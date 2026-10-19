#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_runtime::workflow::{
    CheckArtifact, PlanArtifact, audit_journal, digest_bytes, native_path,
};
use serde_json::Value;

fn lock_native_path(path: &Path) -> String {
    format!(
        "{{ encoding = 'base64url-nopad', platform = 'unix', value = '{}' }}",
        native_path(path)["value"].as_str().unwrap(),
    )
}

struct Prepared(PathBuf);

impl Prepared {
    fn new(environment: &str, tool: Option<&Path>) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let destination = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "native-prepare-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        opaal_cli::project::init_repository_project(&destination, environment, tool).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_opaal"), destination.join("opaal")).unwrap();
        Self(destination)
    }

    fn run(&self, arguments: &[&str], input: &[u8]) -> Output {
        use std::io::Write as _;
        let mut child = Command::new(self.0.join("opaal"))
            .current_dir(&self.0)
            .env_clear()
            .args(arguments)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn preparation_offline_assets_policy_and_accepted_saved_routes() {
    let prepared = Prepared::new("offline", None);
    let owner = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/repository-check");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/controlled-external-tools/reference");
    for name in [
        "README.md",
        "policy.opaal",
        "policy-tests.opaal",
        "source.opaal",
        "saved-tasks.opaal",
        "tasks.opaal",
        "fixtures/check.sh",
        "fixtures/repository.toml",
        "fixtures/pass.json",
        "fixtures/findings.json",
        "fixtures/negative-manifest.json",
        "fixtures/invalid.json",
        "fixtures/acquisition.json",
        "offline-authority.toml",
    ] {
        let bytes = fs::read(owner.join(name)).unwrap();
        assert_eq!(fs::read(prepared.0.join(name)).unwrap(), bytes, "{name}");
        assert_eq!(fs::read(golden.join(name)).unwrap(), bytes, "{name}");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(prepared.0.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::read(prepared.0.join("opaal.toml")).unwrap(),
        fs::read(owner.join("offline.toml")).unwrap()
    );
    for name in [
        "offline.toml",
        "offline-tools-linux.toml",
        "offline-tools-macos.toml",
        "live.toml",
        "live-authority.toml",
        "live-tools.template.toml",
    ] {
        assert_eq!(
            fs::read(owner.join(name)).unwrap(),
            fs::read(golden.join(name)).unwrap(),
            "{name}"
        );
    }
    let doctor = prepared.run(
        &[
            "project",
            "doctor",
            "--project",
            "opaal.toml",
            "--task",
            "saved",
            "--environment",
            "offline",
            "--format",
            "json",
        ],
        b"",
    );
    assert_eq!(doctor.status.code(), Some(0), "{doctor:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&doctor.stdout).unwrap()["local_readiness"],
        "ready"
    );
    let tests = prepared.run(&["test", "policy-tests.opaal"], b"");
    assert!(tests.status.success(), "{tests:?}");
    let init = prepared.run(
        &[
            "project",
            "init",
            "--example",
            "repository-check",
            "--destination",
            "cli-offline",
            "--environment",
            "offline",
        ],
        b"",
    );
    assert!(init.status.success(), "{init:?}");
    for (index, (fixture, exit, report)) in [
        (
            "pass",
            0,
            r#"{"findings":0,"manifest_ok":true,"passed":true}"#,
        ),
        (
            "findings",
            1,
            r#"{"findings":1,"manifest_ok":true,"passed":false}"#,
        ),
        (
            "negative-manifest",
            1,
            r#"{"findings":0,"manifest_ok":false,"passed":false}"#,
        ),
        ("invalid", 2, ""),
        ("acquisition", 2, ""),
    ]
    .iter()
    .enumerate()
    {
        let input = fs::read(prepared.0.join(format!("fixtures/{fixture}.json"))).unwrap();
        let standalone = prepared.run(&["source.opaal"], &input);
        assert_eq!(
            standalone.status.code(),
            Some(if *exit == 2 { 1 } else { *exit }),
            "{fixture}: {standalone:?}"
        );
        assert_eq!(standalone.stdout, report.as_bytes(), "{fixture}");
        let plan_path = format!("saved{index}.plan.json");
        let plan = prepared.run(
            &[
                "plan",
                "--project",
                "opaal.toml",
                "--task",
                "saved",
                "--environment",
                "offline",
                "--expires-in",
                "900s",
                "--out",
                &plan_path,
                "--format",
                "digest",
            ],
            b"",
        );
        assert!(plan.status.success(), "{plan:?}");
        let accepted = String::from_utf8(plan.stdout).unwrap();
        let run = prepared.run(
            &[
                "task",
                "run",
                "--plan",
                &plan_path,
                "--accept",
                accepted.trim(),
                "--journal",
                &format!("saved{index}.run.jsonl"),
                "--receipt-out",
                &format!("saved{index}.receipt.json"),
                "--audit-out",
                &format!("saved{index}.audit.json"),
                "--require-true",
            ],
            &input,
        );
        assert_eq!(run.status.code(), Some(*exit), "{fixture}: {run:?}");
        assert_eq!(run.stdout, report.as_bytes(), "{fixture}");
        let explanation = prepared.run(
            &[
                "task",
                "explain",
                "--receipt",
                &format!("saved{index}.receipt.json"),
                "--audit",
                &format!("saved{index}.audit.json"),
                "--format",
                "json",
            ],
            b"",
        );
        assert!(explanation.status.success(), "{explanation:?}");
    }
}

#[test]
fn preparation_closed_options_and_destination_refusals_preserve_files() {
    use opaal_cli::project::init_repository_project;
    use std::os::unix::fs::symlink;
    let prepared = Prepared::new("offline", None);
    let destination = prepared.0.join("fresh");
    for (environment, tool) in [
        ("offline", Some(Path::new("/tool"))),
        ("ci", None),
        ("ci", Some(Path::new("relative"))),
        ("other", None),
    ] {
        assert!(init_repository_project(&destination, environment, tool).is_err());
        assert!(!destination.exists());
    }
    for options in [
        vec!["--environment", "offline", "--tool", "/tool"],
        vec!["--environment", "ci"],
        vec!["--environment", "ci", "--tool", "relative"],
        vec![
            "--environment",
            "ci",
            "--tool",
            "/tool",
            "--qualified-version",
            "1.0.0",
        ],
        vec!["--environment", "ci", "--tool", "/tool", "--tool", "/tool"],
    ] {
        let mut args = vec![
            "project",
            "init",
            "--example",
            "repository-check",
            "--destination",
            "fresh",
        ];
        args.extend(options);
        let output = prepared.run(&args, b"");
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(!destination.exists());
    }
    fs::create_dir(prepared.0.join("existing")).unwrap();
    fs::write(prepared.0.join("existing/canary"), b"untouched").unwrap();
    fs::write(prepared.0.join("file"), b"untouched").unwrap();
    symlink("existing", prepared.0.join("link")).unwrap();
    symlink("missing", prepared.0.join("dangling")).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(prepared.0.join("fifo"))
            .status()
            .unwrap()
            .success()
    );
    for name in [
        "existing",
        "file",
        "link",
        "dangling",
        "fifo",
        "link/child",
        "missing/child",
    ] {
        assert!(
            init_repository_project(&prepared.0.join(name), "offline", None).is_err(),
            "{name}"
        );
    }
    assert_eq!(
        fs::read(prepared.0.join("existing/canary")).unwrap(),
        b"untouched"
    );
    assert_eq!(fs::read(prepared.0.join("file")).unwrap(), b"untouched");
}

#[test]
fn preparation_native_doctor_unknown_and_invalid_images_never_launch() {
    use opaal_cli::project::{
        CheckProjectRequest, ProjectInputBinding, doctor_project, init_repository_project,
    };
    use opaal_runtime::module::AnalysisControl;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let parent = Prepared::new("offline", None);
    let tool = parent.0.join("checker");
    // Structurally valid ELF, intentionally not a runnable tool: inspection is not execution proof.
    let mut image = vec![0; 120];
    image[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    image[18..20].copy_from_slice(&62_u16.to_le_bytes());
    image[20..24].copy_from_slice(&1_u32.to_le_bytes());
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[52..54].copy_from_slice(&64_u16.to_le_bytes());
    image[54..56].copy_from_slice(&56_u16.to_le_bytes());
    image[56..58].copy_from_slice(&1_u16.to_le_bytes());
    image[64..68].copy_from_slice(&1_u32.to_le_bytes());
    fs::write(&tool, &image).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    let live = parent.0.join("live");
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        assert!(init_repository_project(&live, "ci", Some(&tool)).is_err());
        assert!(!live.exists());
        // Construct the same controls to diagnose unsupported native execution on this host.
        fs::create_dir(&live).unwrap();
        let owner = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/repository-check");
        for name in ["tasks.opaal", "policy.opaal", "live-authority.toml"] {
            fs::copy(owner.join(name), live.join(name)).unwrap();
        }
        fs::copy(owner.join("live.toml"), live.join("opaal.toml")).unwrap();
        let lock = fs::read_to_string(owner.join("live-tools.template.toml"))
            .unwrap()
            .replace("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")
            .replace("@TOOL@", native_path(&tool)["value"].as_str().unwrap())
            .replace("@DIGEST@", &digest_bytes(&image));
        fs::write(live.join("live-tools.toml"), lock).unwrap();
    } else {
        init_repository_project(&live, "ci", Some(&tool)).unwrap();
    }
    fs::write(live.join("check.sh"), b"#!/bin/sh\ntrue\n").unwrap();
    fs::write(
        live.join("repository.toml"),
        b"[repository]\nlicense = 'MIT'\nprivate = false\n",
    )
    .unwrap();
    let request = CheckProjectRequest::with_bindings(
        live.join("opaal.toml"),
        "repository".to_owned(),
        "ci".to_owned(),
        vec![
            ProjectInputBinding::file("script".to_owned(), live.join("check.sh")),
            ProjectInputBinding::file("manifest".to_owned(), live.join("repository.toml")),
        ],
    )
    .with_json();
    let run = doctor_project(&request, &AnalysisControl::never()).unwrap();
    let view: Value = serde_json::from_slice(&run.output).unwrap();
    let findings = view["findings"].as_array().unwrap();
    assert!(findings.iter().any(|f| f["code"] == "DOCTOR017"
        && f["severity"] == "unknown"
        && f["message"]
            == "Native tool version and behavior are unverified; child effects are unenforced."));
    assert!(!findings.iter().any(|f| f["code"] == "DOCTOR007"));
    if cfg!(target_os = "macos") {
        assert!(findings.iter().any(|f| f["code"] == "DOCTOR006"));
        assert_eq!(view["local_readiness"], "blocked");
    } else {
        assert_eq!(view["local_readiness"], "unknown");
        assert_eq!(run.exit_code, 1);
        assert_eq!(findings.len(), 1);
    }
    let source = live.join("tasks.opaal");
    fs::write(
        &source,
        format!(
            "{}\naction no_tool() -> Bool effects {{}} {{ return true }}\ntask pure_check = no_tool\n",
            fs::read_to_string(&source).unwrap().replace("export { check_repository }", "export { check_repository, no_tool }")
        ),
    )
    .unwrap();
    let pure = doctor_project(
        &CheckProjectRequest::new(
            live.join("opaal.toml"),
            "pure_check".to_owned(),
            "ci".to_owned(),
            Vec::new(),
        )
        .with_json(),
        &AnalysisControl::never(),
    )
    .unwrap();
    let pure_view: Value = serde_json::from_slice(&pure.output).unwrap();
    assert_eq!(pure_view["local_readiness"], "unknown", "{pure_view}");
    assert_eq!(pure_view["findings"].as_array().unwrap().len(), 1);
    let canary = parent.0.join("launched");
    let script = format!("#!/bin/sh\ntouch '{}'\n", canary.display());
    let lock_path = live.join("live-tools.toml");
    let lock = fs::read_to_string(&lock_path).unwrap();
    for (index, bytes) in [script.as_bytes(), b"not an executable", &image[..64]]
        .iter()
        .enumerate()
    {
        fs::write(&tool, bytes).unwrap();
        fs::write(
            &lock_path,
            lock.replace(&digest_bytes(&image), &digest_bytes(bytes)),
        )
        .unwrap();
        let destination = parent.0.join(format!("bad{index}"));
        assert!(init_repository_project(&destination, "ci", Some(&tool)).is_err());
        assert!(!destination.exists());
        let run = doctor_project(&request, &AnalysisControl::never()).unwrap();
        assert!(String::from_utf8_lossy(&run.output).contains("DOCTOR007"));
        assert!(!canary.exists());
    }
    fs::write(&tool, &image).unwrap();
    fs::write(&lock_path, &lock).unwrap();
    let modes: &[u32] = if cfg!(target_os = "linux") {
        &[0o600, 0o4700, 0o2700]
    } else {
        &[0o600]
    };
    for &mode in modes {
        fs::set_permissions(&tool, fs::Permissions::from_mode(mode)).unwrap();
        let destination = parent.0.join(format!("mode{mode}"));
        assert!(init_repository_project(&destination, "ci", Some(&tool)).is_err());
        assert!(!destination.exists());
        let run = doctor_project(&request, &AnalysisControl::never()).unwrap();
        assert!(
            String::from_utf8_lossy(&run.output).contains("DOCTOR007"),
            "mode {mode:o}: {}",
            String::from_utf8_lossy(&run.output)
        );
    }
    symlink(&tool, parent.0.join("tool-link")).unwrap();
    assert!(
        init_repository_project(
            &parent.0.join("linked"),
            "ci",
            Some(&parent.0.join("tool-link"))
        )
        .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly supplied, independently qualified ShellCheck image"]
fn preparation_supplied_checker_reference_reports_and_policy_failures() {
    let tool = std::env::var_os("OPAAL_NATIVE_CHECKER").expect("supply the qualified image path");
    let prepared = Prepared::new("ci", Some(Path::new(&tool)));
    let lock = fs::read_to_string(prepared.0.join("live-tools.toml")).unwrap();
    assert!(!lock.contains("\nversion ="));
    assert!(lock.contains("inherit = []"));
    assert!(lock.contains("name = \"LC_ALL\""));
    assert!(lock.contains(&digest_bytes(&fs::read(&tool).unwrap())));
    let doctor = prepared.run(
        &[
            "project",
            "doctor",
            "--project",
            "opaal.toml",
            "--task",
            "repository",
            "--environment",
            "ci",
            "--input-file",
            "script=fixtures/check.sh",
            "--input-file",
            "manifest=fixtures/repository.toml",
            "--format",
            "json",
        ],
        b"",
    );
    assert_eq!(doctor.status.code(), Some(1), "{doctor:?}");
    let view: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(view["local_readiness"], "unknown");
    assert_eq!(view["findings"].as_array().unwrap().len(), 1);
    let clean = b"#!/bin/sh\nprintf '%s\\n' 'ready'\n";
    let finding = b"#!/bin/sh\nprintf '%s\\n' $1\n";
    let manifest = b"[repository]\nlicense = 'MIT'\nprivate = false\n";
    let negative = b"[repository]\nlicense = ''\nprivate = true\n";
    for (index, (script, document, exit, report)) in [
        (
            clean.as_slice(),
            manifest.as_slice(),
            0,
            r#"{"findings":0,"manifest_ok":true,"passed":true}"#,
        ),
        (
            finding.as_slice(),
            manifest.as_slice(),
            1,
            r#"{"findings":1,"manifest_ok":true,"passed":false}"#,
        ),
        (
            clean.as_slice(),
            negative.as_slice(),
            1,
            r#"{"findings":0,"manifest_ok":false,"passed":false}"#,
        ),
        (clean.as_slice(), b"[invalid".as_slice(), 2, ""),
    ]
    .iter()
    .enumerate()
    {
        fs::write(prepared.0.join("fixtures/check.sh"), script).unwrap();
        fs::write(prepared.0.join("fixtures/repository.toml"), document).unwrap();
        let plan = format!("live{index}.plan.json");
        let output = prepared.run(
            &[
                "plan",
                "--project",
                "opaal.toml",
                "--task",
                "repository",
                "--environment",
                "ci",
                "--input-file",
                "script=fixtures/check.sh",
                "--input-file",
                "manifest=fixtures/repository.toml",
                "--expires-in",
                "900s",
                "--out",
                &plan,
                "--format",
                "digest",
            ],
            b"",
        );
        assert!(output.status.success(), "{output:?}");
        let digest = String::from_utf8(output.stdout).unwrap();
        let run = prepared.run(
            &[
                "task",
                "run",
                "--plan",
                &plan,
                "--accept",
                digest.trim(),
                "--journal",
                &format!("live{index}.jsonl"),
                "--receipt-out",
                &format!("live{index}.receipt.json"),
                "--audit-out",
                &format!("live{index}.audit.json"),
                "--require-true",
            ],
            b"",
        );
        assert_eq!(run.status.code(), Some(*exit), "case {index}: {run:?}");
        assert_eq!(run.stdout, report.as_bytes(), "case {index}");
        let explanation = prepared.run(
            &[
                "task",
                "explain",
                "--receipt",
                &format!("live{index}.receipt.json"),
                "--audit",
                &format!("live{index}.audit.json"),
                "--format",
                "json",
            ],
            b"",
        );
        assert!(explanation.status.success(), "{explanation:?}");
    }
}

struct Project(PathBuf);

impl Project {
    fn new(source: &str) -> Self {
        Self::with_tool(source, Path::new("/usr/bin/printf"))
    }

    fn with_tool(source: &str, tool: &Path) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // Exercise the candidate outside the build filesystem, including its same-image worker.
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "native-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_opaal"), root.join("opaal")).unwrap();
        fs::write(
            root.join("opaal.toml"),
            r#"schema_version = 1
[project]
name = "native_test"
root_module = "tasks.opaal"
required_opaal = ">=1.6.0,<2.0.0"
[paths]
root = "."
evidence = "output"
[tools.checker]
adapter = "native"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
        )
        .unwrap();
        fs::write(root.join("tasks.opaal"), source).unwrap();
        fs::write(
            root.join("authority.toml"),
            r#"schema_version = 1
project = "native_test"
environment = "ci"
[[rules]]
effect = "process.run"
scope = "tool.checker"
decision = "grant"
required_enforcement = "acknowledge-unenforced"
[[rules]]
effect = "stdout.write"
scope = "evaluation"
decision = "grant"
required_enforcement = "enforced"
"#,
        )
        .unwrap();
        let tool = tool.canonicalize().unwrap();
        let platform = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            "x86_64-unknown-linux-gnu"
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "aarch64-apple-darwin"
        } else {
            "unsupported"
        };
        fs::write(
            root.join("tools.toml"),
            format!(
                r#"schema_version = 1
project = "native_test"
environment = "ci"
platform = "{platform}"
[child_environment]
inherit = []
[[tools]]
id = "checker"
adapter = "native"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
digest = "{}"
"#,
                native_path(&tool)["value"].as_str().unwrap(),
                digest_bytes(&fs::read(tool).unwrap())
            ),
        )
        .unwrap();
        Self(root)
    }

    fn command(&self, arguments: &[&str]) -> Output {
        Command::new(self.0.join("opaal"))
            .current_dir(&self.0)
            .env_clear()
            .args(arguments)
            .output()
            .unwrap()
    }

    fn pin_tool(&self, tool: &Path) {
        let lock = self.0.join("tools.toml");
        let document = fs::read_to_string(&lock)
            .unwrap()
            .lines()
            .map(|line| {
                if line.starts_with("path = ") {
                    format!("path = {}", lock_native_path(tool))
                } else if line.starts_with("digest = ") {
                    format!("digest = '{}'", digest_bytes(&fs::read(tool).unwrap()))
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(lock, document).unwrap();
    }

    fn plan(&self) -> PlanArtifact {
        self.plan_with_bindings(&[])
    }

    fn plan_with_bindings(&self, bindings: &[&str]) -> PlanArtifact {
        let mut arguments = vec![
            "check",
            "--project",
            "opaal.toml",
            "--task",
            "check",
            "--environment",
            "ci",
            "--format",
            "json",
        ];
        arguments.extend_from_slice(bindings);
        let check = self.command(&arguments);
        let check = CheckArtifact::parse(&check.stdout)
            .unwrap_or_else(|error| panic!("invalid check: {error}; {check:?}"));
        assert_eq!(check.value()["schema"], "opaal.check.v5");
        assert!(check.value()["tools"][0]["locked_version"].is_null());
        let mut arguments = vec![
            "plan",
            "--project",
            "opaal.toml",
            "--task",
            "check",
            "--environment",
            "ci",
            "--expires-in",
            "900s",
            "--out",
            "plan.json",
        ];
        arguments.extend_from_slice(bindings);
        let output = self.command(&arguments);
        assert!(self.0.join("plan.json").exists(), "{output:?}");
        let plan = PlanArtifact::parse(&fs::read(self.0.join("plan.json")).unwrap()).unwrap();
        assert_eq!(plan.value()["schema"], "opaal.plan.v5");
        let inspected = self.command(&["plan", "inspect", "plan.json"]);
        assert!(inspected.status.success(), "{inspected:?}");
        assert!(
            String::from_utf8(inspected.stdout)
                .unwrap()
                .contains("version and behavior are unverified")
        );
        plan
    }

    fn run(&self, plan: &PlanArtifact, accept: &str) -> Output {
        assert_eq!(plan.value()["schema_version"], 5);
        self.command(&[
            "task",
            "run",
            "--plan",
            "plan.json",
            "--accept",
            accept,
            "--require-true",
            "--run-id",
            "11111111111111111111111111111111",
            "--journal",
            "run.jsonl",
            "--receipt-out",
            "receipt.json",
            "--audit-out",
            "audit.json",
        ])
    }

    fn audit(&self) -> Value {
        let audit = audit_journal(&fs::read(self.0.join("run.jsonl")).unwrap()).unwrap();
        assert!(audit.is_complete());
        assert_eq!(audit.value()["schema"], "opaal.audit.v5");
        assert_eq!(fs::read(self.0.join("audit.json")).unwrap(), audit.bytes());
        for name in ["plan.json", "run.jsonl", "receipt.json", "audit.json"] {
            assert!(
                !fs::read_to_string(self.0.join(name))
                    .unwrap()
                    .contains("payload-canary"),
                "{name}"
            );
        }
        let explanation = self.command(&[
            "task",
            "explain",
            "--receipt",
            "receipt.json",
            "--audit",
            "audit.json",
            "--format",
            "json",
        ]);
        assert!(explanation.status.success(), "{explanation:?}");
        assert!(!String::from_utf8_lossy(&explanation.stdout).contains("payload-canary"));
        audit.value().clone()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("failed native CLI fixture retained: {}", self.0.display());
            return;
        }
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn record_evidence_case(name: &str, observed: Value) {
    let inventory_bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/controlled-external-tools/evidence-cases.json"
    ));
    let inventory: Value = serde_json::from_slice(inventory_bytes).unwrap();
    let host = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => panic!("unsupported evidence host"),
    };
    let row = inventory["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap();
    assert!(
        row["hosts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == host)
    );
    assert_eq!(observed, row["expected"], "{name}");
    println!(
        "OPAAL_EVIDENCE_CASE {}",
        serde_json::json!({
            "name": name, "observed": observed, "host": host,
            "inventory_digest": digest_bytes(inventory_bytes)
        })
    );
}

fn metadata_observation(
    project: &Project,
    run: &Output,
    audit: &Value,
    standard_io: bool,
) -> Value {
    let mut outcomes = vec![&audit["primary"]];
    let mut operations = Vec::new();
    for event in audit["events"].as_array().unwrap() {
        if let Some(outcome) = event["payload"].get("outcome") {
            outcomes.push(outcome);
        }
        if let Some(operation) = event["payload"].get("operation") {
            operations.push(operation);
        }
        assert!(
            event["payload"]
                .get("evidence_digest")
                .is_none_or(Value::is_null)
        );
    }
    for operation in &operations {
        if operation["kind"] == "process" {
            assert_eq!(operation["argv"], serde_json::json!([]));
            assert!(operation["argv_digest"].is_null());
        }
        assert!(operation.get("evidence_digest").is_none_or(Value::is_null));
    }
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome["value_digest"].is_null())
    );
    let raw_canaries = ["plan.json", "run.jsonl", "receipt.json", "audit.json"]
        .iter()
        .any(|name| {
            fs::read_to_string(project.0.join(name))
                .unwrap()
                .contains("payload-canary")
        })
        || String::from_utf8_lossy(&run.stderr).contains("payload-canary");
    serde_json::json!({
        "raw_canaries": raw_canaries, "argv": [], "argv_digest": null,
        "payload_digest": null, "value_digest": null,
        "canonical_outcomes": outcomes.iter().all(|outcome| outcome["message"] == "metadata-only outcome"),
        "fictitious_standard_host": audit.get("standard_host").is_some() != standard_io
    })
}

#[test]
fn native_frozen_cli_evidence_cases() {
    println!(
        "OPAAL_EVIDENCE_CLI {}",
        digest_bytes(&fs::read(env!("CARGO_BIN_EXE_opaal")).unwrap())
    );
    unused_native_tool_selects_v5_for_pure_tasks_and_caught_errors_without_host_roles();
    accepted_native_process_omits_payload_and_preserves_explicit_report();
    evidence_mixed_tools_redact_probes_calls_and_errors_without_fictitious_host_roles();
    acceptance_bound_input_is_checked_at_use_after_execution_has_begun();
}

#[test]
fn unused_native_tool_selects_v5_for_pure_tasks_and_caught_errors_without_host_roles() {
    for (body, exit) in [
        ("return true", 0),
        ("return false", 1),
        (
            "try { throw 'payload-canary' } catch error { return false }",
            1,
        ),
        ("throw 'payload-canary'", 2),
    ] {
        let project = Project::new(&format!(
            "action check() -> Bool effects {{}} {{ {body} }}\ntask check = check\n"
        ));
        let plan = project.plan();
        assert!(plan.is_executable());
        assert!(plan.value().get("standard_host").is_none());
        let run = project.run(&plan, plan.digest());
        assert_eq!(run.status.code(), Some(exit), "{run:?}");
        assert!(run.stdout.is_empty());
        let audit = project.audit();
        assert!(audit.get("standard_host").is_none());
        assert_eq!(audit["primary"]["message"], "metadata-only outcome");
        assert!(audit["primary"]["value_digest"].is_null());
        if body != "return false" {
            record_evidence_case(
                match exit {
                    0 => "metadata-pure-unused-native",
                    1 => "metadata-caught-throw",
                    _ => "metadata-unhandled-throw",
                },
                metadata_observation(&project, &run, &audit, false),
            );
        }
        let mut receipt: Value =
            serde_json::from_slice(&fs::read(project.0.join("receipt.json")).unwrap()).unwrap();
        receipt["progress"]["stdout.write"]["confirmed_bytes"] = 1.into();
        let mut bytes = serde_json::to_vec(&receipt).unwrap();
        bytes.push(b'\n');
        opaal_runtime::workflow::parse_execution_receipt(&bytes).unwrap();
        fs::write(project.0.join("receipt.json"), bytes).unwrap();
        let explanation = project.command(&[
            "task",
            "explain",
            "--receipt",
            "receipt.json",
            "--audit",
            "audit.json",
            "--format",
            "json",
        ]);
        assert!(!explanation.status.success(), "forged progress must refuse");
    }
}

#[test]
fn accepted_native_process_omits_payload_and_preserves_explicit_report() {
    for standard_io in [false, true] {
        let report = if standard_io {
            "io::write_stdout(result.stdout)"
        } else {
            ""
        };
        let project = Project::new(&format!(
            r#"import project::tools as tools
import std::process as process
import std::io as io
action check() -> Bool effects {{ process.run(tools::checker); {} }} {{
    let result = process::run(tools::checker, ["%s", "payload-canary ; $() ä"])
    {report}
    return (result.status.code == 0)
}}
task check = check
"#,
            if standard_io { "stdout.write();" } else { "" }
        ));
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            fs::write(
                project.0.join("redaction.c"),
                r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
    const char *value = getenv("NATIVE_CANARY");
    if (argc != 3 || !value || strcmp(value, "payload-canary-env")) return 91;
    FILE *marker = fopen("native.launches", "a");
    if (!marker || fputs("launch\n", marker) < 0 || fclose(marker)) return 92;
    if (fputs(argv[2], stdout) < 0 || fputs("payload-canary-stderr", stderr) < 0) return 93;
    return 0;
}
"#,
            )
            .unwrap();
            let built = Command::new("/usr/bin/cc")
                .current_dir(&project.0)
                .env("PATH", "/usr/bin:/bin")
                .args([
                    "-O2",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                    "redaction.c",
                    "-o",
                    "checker",
                ])
                .output()
                .unwrap();
            assert!(built.status.success(), "{built:?}");
            project.pin_tool(&project.0.join("checker"));
            use std::io::Write as _;
            fs::OpenOptions::new()
                .append(true)
                .open(project.0.join("tools.toml"))
                .unwrap()
                .write_all(
                    format!(
                        "\n[[child_environment.variables]]\nname = 'NATIVE_CANARY'\nvalue = {}\n",
                        lock_native_path(Path::new("payload-canary-env"))
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
        let plan = project.plan();
        let supported = cfg!(all(target_os = "linux", target_arch = "x86_64"));
        assert_eq!(plan.is_executable(), supported);
        let wrong = project.run(&plan, &format!("sha256:{}", "0".repeat(64)));
        assert!(!wrong.status.success());
        assert!(!project.0.join("run.jsonl").exists());
        let run = project.run(&plan, plan.digest());
        if !supported {
            assert!(!run.status.success());
            assert!(!project.0.join("run.jsonl").exists());
            continue;
        }
        assert!(run.status.success(), "{run:?}");
        assert_eq!(
            run.stdout,
            if standard_io {
                "payload-canary ; $() ä".as_bytes()
            } else {
                b""
            }
        );
        let audit = project.audit();
        assert_eq!(audit.get("standard_host").is_some(), standard_io);
        let effects = audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| {
                event["kind"] == "effect-after" && event["payload"]["effect"] == "process.run"
            })
            .collect::<Vec<_>>();
        assert_eq!(effects.len(), 1, "no native probe or retry");
        let operation = &effects[0]["payload"]["operation"];
        assert_eq!(operation["operation"], "std::process::run");
        assert_eq!(operation["argv_count"], 3);
        assert_eq!(operation["argv"], serde_json::json!([]));
        assert!(operation["argv_digest"].is_null());
        assert!(operation["evidence_digest"].is_null());
        assert_eq!(
            fs::read(project.0.join("native.launches")).unwrap(),
            b"launch\n"
        );
        assert_eq!(operation["stdout_bytes"], "payload-canary ; $() ä".len());
        assert_eq!(operation["stderr_bytes"], "payload-canary-stderr".len());
        if !standard_io {
            record_evidence_case(
                "metadata-native-call",
                metadata_observation(&project, &run, &audit, false),
            );
        }
    }
}

#[test]
fn evidence_mixed_tools_redact_probes_calls_and_errors_without_fictitious_host_roles() {
    use std::os::unix::fs::PermissionsExt as _;
    for (native, ending, standard_io, exit) in [
        (false, "return true", false, 0),
        (
            false,
            "try { throw 'payload-canary-caught' } catch error { return false }",
            false,
            1,
        ),
        (false, "throw 'payload-canary-thrown'", false, 2),
        (true, "return true", false, 0),
        (true, "return true", true, 0),
        (
            true,
            "try { throw 'payload-canary-caught' } catch error { return false }",
            false,
            1,
        ),
        (true, "throw 'payload-canary-thrown'", false, 2),
    ] {
        let project = Project::new(&format!(
            r#"import project::tools as tools
import std::process as process
import std::io as io
import std::data as data
action check() -> Bool effects {{ process.run(tools::git); {} {} }} {{
    let maintained = process::run(tools::git, ["payload-canary-argv ; $() ä"])
    {}
    {}
    {ending}
}}
task check = check
"#,
            if native {
                "process.run(tools::checker);"
            } else {
                ""
            },
            if standard_io { "stdout.write();" } else { "" },
            if native {
                "let result = process::run(tools::checker, [\"%s\", \"payload-canary-native\"])"
            } else {
                ""
            },
            if standard_io {
                "io::write_stdout(data::json_encode({safe: true}))"
            } else {
                ""
            },
        ));
        let manifest = project.0.join("opaal.toml");
        let mut document = fs::read_to_string(&manifest).unwrap();
        document.push_str("\n[tools.git]\nadapter = 'git'\nversion = '>=2.0.0,<3.0.0'\n");
        fs::write(manifest, document).unwrap();
        let authority = project.0.join("authority.toml");
        let mut document = fs::read_to_string(&authority).unwrap();
        document.push_str("\n[[rules]]\neffect = 'process.run'\nscope = 'tool.git'\ndecision = 'grant'\nrequired_enforcement = 'acknowledge-unenforced'\n");
        fs::write(authority, document).unwrap();
        // A maintained adapter double records both its mandatory probe and its actual call.
        let tool = project.0.join("git-double");
        fs::write(
            &tool,
            "#!/bin/sh\n[ \"$NATIVE_CANARY\" = 'payload-canary-env' ] || exit 91\nprintf '%s\\n' \"$1\" >> maintained.launches\nif [ \"$1\" = '--version' ]; then\n  printf 'git version 2.50.0\\n'\nelse\n  printf 'payload-canary-stdout'\n  printf 'payload-canary-stderr' >&2\nfi\n",
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = project.0.join("tools.toml");
        let mut document = fs::read_to_string(&lock).unwrap();
        document.push_str(&format!(
            "\n[[tools]]\nid = 'git'\nadapter = 'git'\nversion = '2.50.0'\npath = {}\ndigest = '{}'\n",
            lock_native_path(&tool),
            digest_bytes(&fs::read(&tool).unwrap()),
        ));
        for name in [
            "HOME",
            "TMPDIR",
            "PATH",
            "CARGO_HOME",
            "RUSTC",
            "RUSTDOC",
            "LC_ALL",
            "TZ",
            "CARGO_NET_OFFLINE",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_GLOBAL",
            "NATIVE_CANARY",
        ] {
            let value = match name {
                "LC_ALL" => "C",
                "NATIVE_CANARY" => "payload-canary-env",
                _ => "/unused",
            };
            document.push_str(&format!(
                "\n[[child_environment.variables]]\nname = '{name}'\nvalue = {}\n",
                lock_native_path(Path::new(value)),
            ));
        }
        fs::write(lock, document).unwrap();
        let plan = project.plan();
        let supported = cfg!(all(target_os = "linux", target_arch = "x86_64"));
        assert_eq!(plan.is_executable(), supported);
        assert_eq!(plan.value().get("standard_host").is_some(), standard_io);
        assert!(
            !project.0.join("maintained.launches").exists(),
            "planning must not probe"
        );
        let run = project.run(&plan, plan.digest());
        if !supported {
            assert_eq!(run.status.code(), Some(2));
            assert!(!project.0.join("maintained.launches").exists());
            assert!(!project.0.join("run.jsonl").exists());
            continue;
        }
        assert_eq!(run.status.code(), Some(exit), "{run:?}");
        assert_eq!(
            run.stdout,
            if standard_io {
                br#"{"safe":true}"#.as_slice()
            } else {
                b""
            }
        );
        assert!(!String::from_utf8_lossy(&run.stderr).contains("payload-canary"));
        assert_eq!(
            fs::read(project.0.join("maintained.launches")).unwrap(),
            "--version\npayload-canary-argv ; $() ä\n".as_bytes(),
        );
        let audit = project.audit();
        assert_eq!(audit.get("standard_host").is_some(), standard_io);
        let calls = audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| {
                event["kind"] == "effect-after" && event["payload"]["effect"] == "process.run"
            })
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), if native { 3 } else { 2 });
        for (attempt, event) in calls.iter().enumerate() {
            let payload = &event["payload"];
            assert_eq!(payload["attempt"], attempt);
            assert_eq!(payload["outcome"]["message"], "metadata-only outcome");
            assert!(payload["outcome"]["value_digest"].is_null());
            assert!(payload["evidence_digest"].is_null());
            let operation = &payload["operation"];
            assert_eq!(operation["argv"], serde_json::json!([]));
            assert!(operation["argv_digest"].is_null());
            assert!(operation["evidence_digest"].is_null());
            assert_eq!(operation["argv_count"], if attempt == 2 { 3 } else { 2 });
            assert_eq!(operation["status"], 0);
            assert_eq!(
                operation["stdout_bytes"],
                match attempt {
                    0 => "git version 2.50.0\n".len(),
                    1 => "payload-canary-stdout".len(),
                    _ => "payload-canary-native".len(),
                }
            );
            assert_eq!(
                operation["stderr_bytes"],
                if attempt == 1 {
                    "payload-canary-stderr".len()
                } else {
                    0
                }
            );
        }
        if standard_io {
            record_evidence_case(
                "explicit-safe-report",
                serde_json::json!({
                    "report_only_explicit_fields": run.stdout == br#"{"safe":true}"#,
                    "raw_canaries": metadata_observation(&project, &run, &audit, standard_io)["raw_canaries"]
                }),
            );
        }
        let name = match (native, exit, standard_io) {
            (false, 0, false) => Some("metadata-maintained-only"),
            (true, 0, false) => Some("metadata-mixed-no-stdio"),
            (true, 0, true) => Some("metadata-mixed-native-maintained"),
            (true, 1, false) => Some("metadata-mixed-caught-throw"),
            (true, 2, false) => Some("metadata-mixed-unhandled-throw"),
            _ => None,
        };
        if let Some(name) = name {
            record_evidence_case(
                name,
                metadata_observation(&project, &run, &audit, standard_io),
            );
        }
    }
}

#[test]
fn acceptance_revalidates_native_project_controls_inputs_and_tool_before_evidence() {
    for changed in [
        "acceptance",
        "tasks.opaal",
        "opaal.toml",
        "authority.toml",
        "tools.toml",
        "input",
        "tool-bytes",
        "tool-path",
        "child-environment",
        "host",
        "expiry",
        "root",
    ] {
        let project = Project::new(
            "action check(input: Path) -> Bool effects {} { return true }\ntask check = check\n",
        );
        let tool = project.0.join("local-tool");
        fs::copy(Path::new("/usr/bin/printf").canonicalize().unwrap(), &tool).unwrap();
        let lock = project.0.join("tools.toml");
        project.pin_tool(&tool);
        let new_path = native_path(&tool);
        fs::write(project.0.join("input"), b"payload-canary-bound-input").unwrap();
        let plan = project.plan_with_bindings(&["--input-file", "input=input"]);
        assert!(plan.is_executable());
        let mut accepted = plan.digest().to_owned();
        match changed {
            "acceptance" => accepted = format!("sha256:{}", "0".repeat(64)),
            "input" | "tool-bytes" => {
                fs::write(
                    if changed == "input" {
                        project.0.join("input")
                    } else {
                        tool.clone()
                    },
                    b"changed",
                )
                .unwrap();
            }
            "tool-path" => {
                let replacement = project.0.join("replacement");
                fs::copy(&tool, &replacement).unwrap();
                fs::write(
                    &lock,
                    fs::read_to_string(&lock).unwrap().replace(
                        new_path["value"].as_str().unwrap(),
                        native_path(&replacement)["value"].as_str().unwrap(),
                    ),
                )
                .unwrap();
            }
            "child-environment" => {
                let mut document = fs::read_to_string(&lock).unwrap();
                document.push_str(&format!(
                    "\n[[child_environment.variables]]\nname = 'CANARY'\nvalue = {}\n",
                    lock_native_path(Path::new("changed"))
                ));
                fs::write(&lock, document).unwrap();
            }
            "host" | "expiry" | "root" => {
                let mut document = plan.value().clone();
                document.as_object_mut().unwrap().remove("digest");
                match changed {
                    "host" => {
                        document["platform"]["triple"] = "unsupported".into();
                        document["tools"][0]["platform"] = "unsupported".into();
                    }
                    "root" => document["project"]["root"] = native_path(&project.0.join("other")),
                    _ => {
                        document["created_at"] = "2026-01-01T00:00:00.000000000Z".into();
                        document["expires_at"] = "2026-01-01T00:15:00.000000000Z".into();
                        for observation in document["observations"].as_array_mut().unwrap() {
                            if observation["kind"] == "wall-clock" {
                                observation["observed_at"] =
                                    "2026-01-01T00:00:00.000000000Z".into();
                            }
                        }
                    }
                }
                let changed = PlanArtifact::seal(document).unwrap();
                accepted = changed.digest().to_owned();
                fs::write(project.0.join("plan.json"), changed.bytes()).unwrap();
            }
            name => {
                let path = project.0.join(name);
                let mut bytes = fs::read(&path).unwrap();
                bytes.extend_from_slice(b"\n# changed\n");
                fs::write(path, bytes).unwrap();
            }
        }
        let run = project.run(&plan, &accepted);
        assert_eq!(run.status.code(), Some(2), "{changed}: {run:?}");
        assert!(run.stdout.is_empty(), "{changed}");
        for name in ["run.jsonl", "receipt.json", "audit.json"] {
            assert!(!project.0.join(name).exists(), "{changed}: {name}");
        }
    }
}

#[test]
fn acceptance_bound_input_is_checked_at_use_after_execution_has_begun() {
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::fs::symlink;
    use std::process::Stdio;

    for change in ["bytes", "symlink", "same-bytes-replacement"] {
        let project = Project::new(
            "import std::io as io\nimport std::filesystem as fs\nimport project::context as project\n\
             action check(input: Path) -> Bool\n\
             effects { stdin.read(); stdout.write(); filesystem.read(project::root); }\n\
             {\n\
                 io::println('ready')\n\
                 io::read_stdin(1)\n\
                 let value = fs::read(input, 64)\n\
                 io::println('read')\n\
                 return true\n\
             }\ntask check = check\n",
        );
        let authority = project.0.join("authority.toml");
        let mut file = fs::OpenOptions::new().append(true).open(authority).unwrap();
        file.write_all(
            b"\n[[rules]]\neffect = 'stdin.read'\nscope = 'evaluation'\ndecision = 'grant'\nrequired_enforcement = 'enforced'\n\
              [[rules]]\neffect = 'filesystem.read'\nscope = 'project.root'\ndecision = 'grant'\nrequired_enforcement = 'enforced'\n",
        )
        .unwrap();
        let input = project.0.join("input");
        fs::write(&input, b"payload-canary-original").unwrap();
        let plan = project.plan_with_bindings(&["--input-file", "input=input"]);
        assert!(plan.is_executable());
        let mut child = Command::new(project.0.join("opaal"))
            .current_dir(&project.0)
            .env_clear()
            .args([
                "task",
                "run",
                "--plan",
                "plan.json",
                "--accept",
                plan.digest(),
                "--require-true",
                "--journal",
                "run.jsonl",
                "--receipt-out",
                "receipt.json",
                "--audit-out",
                "audit.json",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n", "{change}");
        match change {
            "bytes" => fs::write(&input, b"payload-canary-changed").unwrap(),
            "symlink" => {
                fs::rename(&input, project.0.join("original")).unwrap();
                symlink("original", &input).unwrap();
            }
            "same-bytes-replacement" => {
                fs::rename(&input, project.0.join("original")).unwrap();
                fs::write(&input, b"payload-canary-original").unwrap();
            }
            _ => unreachable!(),
        }
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        let mut remaining = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut remaining).unwrap();
        let output = child.wait_with_output().unwrap();
        let unchanged = change == "same-bytes-replacement";
        assert_eq!(
            output.status.code(),
            Some(if unchanged { 0 } else { 2 }),
            "{change}: {output:?}"
        );
        assert_eq!(remaining, if unchanged { "read\n" } else { "" }, "{change}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("payload-canary"));
        let audit = project.audit();
        assert_eq!(
            audit["primary"]["class"],
            match change {
                "bytes" => "refused",
                "symlink" => "error",
                _ => "success",
            },
            "{change}"
        );
        assert!(audit["primary"]["value_digest"].is_null());
        record_evidence_case(
            &format!(
                "bound-input-before-use-{}",
                if change == "bytes" {
                    "changed-bytes"
                } else {
                    change
                }
            ),
            serde_json::json!({"exit": output.status.code(), "primary": audit["primary"]["class"],
                "value_returned": remaining == "read\n"}),
        );
    }
}

#[test]
fn observers_native_project_never_evaluate_initializers_actions_or_tools() {
    use std::os::unix::fs::PermissionsExt as _;
    let project = Project::new(
        r#"import project::tools as tools
import std::process as process
^printf "payload-canary-initializer"
action check() -> Bool effects { process.run(tools::checker); } {
    let result = process::run(tools::checker, ["payload-canary-argv"])
    throw "payload-canary-action"
}
task check = check
"#,
    );
    let tool = project.0.join("observer-tool");
    fs::write(&tool, "#!/bin/sh\nprintf launched >> observer.launches\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    project.pin_tool(&tool);
    for arguments in [
        vec![
            "task",
            "list",
            "--project",
            "opaal.toml",
            "--format",
            "json",
        ],
        vec![
            "task",
            "inspect",
            "check",
            "--project",
            "opaal.toml",
            "--format",
            "json",
        ],
        vec![
            "check",
            "--project",
            "opaal.toml",
            "--task",
            "check",
            "--environment",
            "ci",
            "--format",
            "json",
        ],
        vec![
            "project",
            "doctor",
            "--project",
            "opaal.toml",
            "--task",
            "check",
            "--environment",
            "ci",
            "--format",
            "json",
        ],
        vec![
            "plan",
            "--project",
            "opaal.toml",
            "--task",
            "check",
            "--environment",
            "ci",
            "--expires-in",
            "900s",
            "--out",
            "observer.plan.json",
        ],
        vec!["plan", "inspect", "observer.plan.json"],
    ] {
        let output = project.command(&arguments);
        assert!(
            !project.0.join("observer.launches").exists(),
            "{arguments:?}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("payload-canary"),
            "{arguments:?}: {output:?}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("payload-canary"),
            "{arguments:?}: {output:?}"
        );
        if arguments[0] == "plan" {
            let plan =
                PlanArtifact::parse(&fs::read(project.0.join("observer.plan.json")).unwrap())
                    .unwrap();
            assert_eq!(plan.value()["schema_version"], 5);
            if arguments[1] == "inspect" {
                assert!(output.status.success(), "{output:?}");
            }
            continue;
        }
        let view: Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{arguments:?}: {error}; {output:?}"));
        if arguments[0] == "task" {
            assert!(output.status.success(), "{output:?}");
            assert_eq!(view["schema_version"], 2);
        }
    }
    // Static discovery does not need a lock or any executable at all.
    fs::remove_file(project.0.join("tools.toml")).unwrap();
    fs::remove_file(tool).unwrap();
    let output = project.command(&[
        "task",
        "list",
        "--project",
        "opaal.toml",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(!project.0.join("observer.launches").exists());
}

#[test]
fn evidence_corrupt_partial_and_missing_native_artifacts_never_prove_pass() {
    use opaal_runtime::workflow::digest_value;
    let project =
        Project::new("action check() -> Bool effects {} { return true }\ntask check = check\n");
    let plan = project.plan();
    assert_eq!(project.run(&plan, plan.digest()).status.code(), Some(0));
    let audit = project.audit();
    let receipt: Value =
        serde_json::from_slice(&fs::read(project.0.join("receipt.json")).unwrap()).unwrap();
    let journal = fs::read(project.0.join("run.jsonl")).unwrap();
    fs::remove_file(project.0.join("tasks.opaal")).unwrap();
    fs::remove_file(project.0.join("tools.toml")).unwrap();
    fs::remove_file(project.0.join("run.jsonl")).unwrap();
    for fault in [
        "none",
        "future-receipt",
        "receipt-run-id",
        "receipt-partial",
        "receipt-progress",
        "receipt-secondary",
        "audit-message",
        "audit-value-digest",
        "future-audit",
        "audit-extra",
        "audit-wrong-digest",
        "audit-run-id",
        "missing-audit",
        "journal-prefix",
        "journal-truncated",
    ] {
        let mut changed_receipt = receipt.clone();
        let mut changed_audit = audit.clone();
        let mut conclusion = None;
        match fault {
            "none" => conclusion = Some("passed"),
            "future-receipt" => changed_receipt["schema_version"] = 99.into(),
            "receipt-run-id" => changed_receipt["run_id"] = "2".repeat(32).into(),
            "receipt-partial" => changed_receipt["primary"]["partial"] = true.into(),
            "receipt-progress" => {
                changed_receipt["progress"]["stdout.write"]["confirmed_bytes"] = 1.into()
            }
            "receipt-secondary" => {
                changed_receipt["secondary"] =
                    serde_json::json!([{"category":"evidence","code":"payload-canary"}]);
                conclusion = Some("failed");
            }
            "audit-message" => changed_audit["primary"]["message"] = "payload-canary".into(),
            "audit-value-digest" => {
                changed_audit["primary"]["value_digest"] = digest_bytes(b"payload-canary").into()
            }
            "future-audit" => changed_audit["schema_version"] = 99.into(),
            "audit-extra" => changed_audit["extra"] = "payload-canary".into(),
            "audit-wrong-digest" => {}
            "audit-run-id" => changed_audit["run_id"] = "2".repeat(32).into(),
            "missing-audit" | "journal-prefix" | "journal-truncated" => {
                conclusion = Some("unproven")
            }
            _ => unreachable!(),
        }
        changed_audit.as_object_mut().unwrap().remove("digest");
        changed_audit["digest"] = if fault == "audit-wrong-digest" {
            format!("sha256:{}", "0".repeat(64)).into()
        } else {
            digest_value(&changed_audit).unwrap().into()
        };
        let mut bytes = serde_json::to_vec(&changed_receipt).unwrap();
        bytes.push(b'\n');
        fs::write(project.0.join("receipt.json"), bytes).unwrap();
        fs::write(
            project.0.join("audit.json"),
            serde_json::to_vec(&changed_audit).unwrap(),
        )
        .unwrap();
        if fault == "missing-audit" {
            fs::remove_file(project.0.join("audit.json")).unwrap();
        } else if matches!(fault, "journal-prefix" | "journal-truncated") {
            let end = if fault == "journal-prefix" {
                journal.iter().position(|byte| *byte == b'\n').unwrap() + 1
            } else {
                journal.len() - 4
            };
            let incomplete = audit_journal(&journal[..end]).unwrap();
            assert_eq!(incomplete.value()["schema_version"], 5);
            assert!(!incomplete.is_complete());
            fs::write(project.0.join("audit.json"), incomplete.bytes()).unwrap();
        }
        let output = project.command(&[
            "task",
            "explain",
            "--receipt",
            "receipt.json",
            "--audit",
            "audit.json",
            "--format",
            "json",
        ]);
        assert_eq!(
            output.status.code(),
            Some(if conclusion.is_some() { 0 } else { 2 }),
            "{fault}: {output:?}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("payload-canary"),
            "{fault}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("payload-canary"),
            "{fault}"
        );
        if let Some(expected) = conclusion {
            let view: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(view["conclusion"], expected, "{fault}");
        } else {
            assert!(output.stdout.is_empty(), "{fault}");
        }
    }
    let mut corrupted = journal;
    corrupted[0] = b'!';
    assert!(audit_journal(&corrupted).is_err());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore = "requires an explicitly supplied and independently identified ShellCheck image"]
fn supplied_checker_snapshot_compatibility() {
    let tool = std::env::var_os("OPAAL_NATIVE_CHECKER").expect("explicit ShellCheck path");
    let project = Project::with_tool(
        r#"import project::tools as tools
import std::process as process
action check() -> Bool effects { process.run(tools::checker); } {
    let result = process::run(tools::checker, ["--norc", "--format=json1", "--shell=sh", "--severity=style", "--", "check.sh"])
    return (result.status.code == 0)
}
task check = check
"#,
        Path::new(&tool),
    );
    fs::write(
        project.0.join("check.sh"),
        "#!/bin/sh\nprintf '%s\\n' 'ready'\n",
    )
    .unwrap();
    let plan = project.plan();
    assert!(plan.is_executable());
    let run = project.run(&plan, plan.digest());
    assert!(run.status.success(), "{run:?}");
    let audit = project.audit();
    let effects = audit["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "effect-after")
        .collect::<Vec<_>>();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["payload"]["operation"]["status"], 0);
    assert!(
        effects[0]["payload"]["operation"]["stdout_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(effects[0]["payload"]["operation"]["stderr_bytes"], 0);
}
