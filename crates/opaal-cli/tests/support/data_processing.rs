#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

pub const OPAAL: &str = env!("CARGO_BIN_EXE_opaal");
pub const VALID: &[&str] = &[
    "empty",
    "pending",
    "conclusions",
    "stable-prefix",
    "escaped",
    "producer-shaped",
];
pub const INVALID: &[&str] = &[
    "malformed",
    "duplicate",
    "null-jobs",
    "missing-jobs",
    "root-list",
    "unknown-status",
    "unknown-conclusion",
    "null-conclusion",
    "missing-completed-conclusion",
    "wrong-name",
    "wrong-job",
    "utf8",
];
static NEXT: AtomicU64 = AtomicU64::new(0);

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/data-processing")
}

pub fn fixture(name: &str) -> Vec<u8> {
    fs::read(fixtures().join(name)).unwrap()
}

pub struct ReportDir(pub PathBuf);

impl ReportDir {
    pub fn new() -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "opaal-report-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let result = Self(path.canonicalize().unwrap());
        for name in [
            "report.opaal",
            "json-report.opaal",
            "text-report.opaal",
            "policy-operations.opaal",
            "tasks.opaal",
            "authority.toml",
            "jobs.json",
            "expected-report.json",
        ] {
            result.write(name, fixture(name));
        }
        let manifest = String::from_utf8(fixture("opaal.toml")).unwrap();
        result.write(
            "opaal.toml",
            manifest.replace(
                ">=1.2.0,<2.0.0",
                &format!(">={},<2.0.0", env!("CARGO_PKG_VERSION")),
            ),
        );
        let platform = if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        };
        result.write("tools.toml", fixture(&format!("tools-{platform}.toml")));
        fs::create_dir(result.0.join("bin")).unwrap();
        symlink("/bin/cat", result.0.join("bin/cat")).unwrap();
        result
    }

    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) {
        fs::write(self.0.join(name), contents).unwrap();
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(OPAAL);
        command
            .current_dir(&self.0)
            .env_clear()
            .env("PATH", self.0.join("bin"));
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    pub fn plan(&self) -> opaal_runtime::workflow::PlanArtifact {
        let output = self.run(&[
            "plan",
            "--project",
            "opaal.toml",
            "--task",
            "summarize",
            "--environment",
            "ci",
            "--input-file",
            "input=jobs.json",
            "--expires-in",
            "900s",
            "--out",
            "report.plan.json",
        ]);
        success(&output);
        let plan = opaal_runtime::workflow::PlanArtifact::parse(
            &fs::read(self.0.join("report.plan.json")).unwrap(),
        )
        .unwrap();
        assert!(plan.is_executable());
        assert_eq!(
            output.stdout,
            format!("plan {}\n", plan.digest()).as_bytes()
        );
        plan
    }

    pub fn execute(&self, digest: &str, journal: &str) -> Output {
        self.run(&[
            "execute",
            "--plan",
            "report.plan.json",
            "--accept",
            digest,
            "--journal",
            journal,
        ])
    }

    pub fn audit(&self, journal: &str) -> opaal_runtime::workflow::AuditArtifact {
        opaal_runtime::workflow::audit_journal(&fs::read(self.0.join(journal)).unwrap()).unwrap()
    }
}

impl Drop for ReportDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

pub fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{output:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

pub fn failure(output: &Output) {
    assert!(!output.status.success(), "{output:?}");
}

pub fn effects(audit: &opaal_runtime::workflow::AuditArtifact) -> Vec<&str> {
    audit.value()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "effect-before")
        .map(|event| event["payload"]["effect"].as_str().unwrap())
        .collect()
}
