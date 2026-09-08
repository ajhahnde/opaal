#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TempProject(PathBuf);

impl TempProject {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "opaal-project-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn opaal(arguments: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(arguments)
        .output()
        .unwrap()
}

fn fixture() -> (TempProject, PathBuf, PathBuf, PathBuf) {
    let project = TempProject::new();
    let manifest = project.write(
        "opaal.toml",
        r#"schema_version = 1
[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[tools.git]
adapter = "git"
version = ">=2.39.0,<3.0.0"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    );
    project.write(
        "tasks.opaal",
        r#"import project::context as project
import project::tools as tools
## Check one candidate without executing it.
action ready(candidate: String) -> String
effects {
    filesystem.read(project::root);
    process.run(tools::git);
}
{
    return candidate
}
task release = ready
"#,
    );
    let authority = project.write(
        "authority.toml",
        r#"schema_version = 1
project = "demo"
environment = "ci"
[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "process.run"
scope = "tool.git"
required_enforcement = "acknowledge-unenforced"
"#,
    );
    let tools = project.write(
        "tools.toml",
        r#"schema_version = 1
project = "demo"
environment = "ci"
platform = "test-host"
[child_environment]
inherit = []
[[tools]]
id = "git"
adapter = "git"
path = { encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvZ2l0" }
version = "2.50.0"
digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
"#,
    );
    (project, manifest, authority, tools)
}

#[test]
fn public_help_names_the_explicit_nonexecuting_project_forms() {
    let top = opaal([OsStr::new("--help")]);
    assert!(top.status.success(), "{top:?}");
    let top = String::from_utf8(top.stdout).unwrap();
    assert!(top.contains("opaal check --project opaal.toml"), "{top}");
    assert!(
        top.contains("opaal task inspect --project opaal.toml TASK"),
        "{top}"
    );

    let check = opaal([OsStr::new("check"), OsStr::new("--help")]);
    assert!(check.status.success(), "{check:?}");
    let check = String::from_utf8(check.stdout).unwrap();
    assert!(check.contains("without executing"), "{check}");

    let task = opaal([OsStr::new("task"), OsStr::new("--help")]);
    assert!(task.status.success(), "{task:?}");
    let task = String::from_utf8(task.stdout).unwrap();
    assert!(task.contains("exact manifest path is required"), "{task}");
}

#[test]
fn runtime_help_uses_the_shared_action_identity_and_effects() {
    let project = TempProject::new();
    let script = project.write(
        "help.opaal",
        "action observe(value: String) -> String effects { clock.wall; } { return value }\nhelp observe\n",
    );
    let output = opaal([script.as_os_str()]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("action observe\n"), "{stdout}");
    assert!(stdout.contains("action identity:"), "{stdout}");
    assert!(stdout.contains("contract digest: sha256:"), "{stdout}");
    assert!(stdout.contains("effects:\n    clock.wall\n"), "{stdout}");
}

#[test]
fn checked_in_project_case_inspects_and_checks_without_execution() {
    let project =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/operational-core/project-cases");
    let manifest = project.join("opaal.toml");
    let authority = project.join("authority-ci.toml");
    let tools = project.join("tools-host.toml");

    let inspect = opaal([
        OsStr::new("task"),
        OsStr::new("inspect"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("readiness"),
    ]);
    assert!(inspect.status.success(), "{inspect:?}");
    let inspect_stdout = String::from_utf8(inspect.stdout).unwrap();
    assert!(
        inspect_stdout.contains("task release_readiness::readiness"),
        "{inspect_stdout}"
    );

    let check = opaal([
        OsStr::new("check"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--task"),
        OsStr::new("readiness"),
        OsStr::new("--environment"),
        OsStr::new("ci"),
        OsStr::new("--authority"),
        authority.as_os_str(),
        OsStr::new("--tools"),
        tools.as_os_str(),
        OsStr::new("--input"),
        OsStr::new("candidate=artifact.tar"),
    ]);
    assert!(check.status.success(), "{check:?}");
    assert!(check.stdout.is_empty(), "{check:?}");
}

#[test]
fn task_inspect_reports_the_shared_action_contract() {
    let (_project, manifest, _, _) = fixture();
    let output = opaal([
        OsStr::new("task"),
        OsStr::new("inspect"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("release"),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("task demo::release\n"));
    assert!(stdout.contains("signature ready(candidate: String) -> String\n"));
    let digest = stdout
        .lines()
        .find_map(|line| line.strip_prefix("contract sha256:"))
        .expect("inspection exposes the shared action digest");
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert!(stdout.contains("documentation\n  Check one candidate without executing it.\n"));
    assert!(stdout.contains("filesystem.read project.root\n"));
    assert!(stdout.contains("process.run tool.git\n"));
    assert!(stdout.contains("demo::tool::git git >=2.39.0, <3.0.0\n"));
    assert!(stdout.contains("demo::environment::ci\n"));
}

#[test]
fn project_check_is_silent_and_performs_no_tool_or_action_execution() {
    let (project, manifest, authority, tools) = fixture();
    let marker = project.0.join("marker");
    let output = opaal([
        OsStr::new("check"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--task"),
        OsStr::new("release"),
        OsStr::new("--environment"),
        OsStr::new("ci"),
        OsStr::new("--authority"),
        authority.as_os_str(),
        OsStr::new("--tools"),
        tools.as_os_str(),
        OsStr::new("--input"),
        OsStr::new("candidate=artifact.tar"),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!marker.exists());
}

#[test]
fn project_discovery_is_explicit_and_fail_closed() {
    let (project, manifest, authority, tools) = fixture();
    let wrong_name = project.0.join("project.toml");
    fs::copy(&manifest, &wrong_name).unwrap();
    let output = opaal([
        OsStr::new("task"),
        OsStr::new("inspect"),
        OsStr::new("--project"),
        wrong_name.as_os_str(),
        OsStr::new("release"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("PROJECT023")
    );

    fs::write(
        &authority,
        fs::read_to_string(&authority)
            .unwrap()
            .replace("decision = \"grant\"", "decision = \"deny\"")
            .replace("required_enforcement = \"enforced\"\n", "")
            .replace("required_enforcement = \"acknowledge-unenforced\"\n", ""),
    )
    .unwrap();
    let output = opaal([
        OsStr::new("check"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--task"),
        OsStr::new("release"),
        OsStr::new("--environment"),
        OsStr::new("ci"),
        OsStr::new("--authority"),
        authority.as_os_str(),
        OsStr::new("--tools"),
        tools.as_os_str(),
        OsStr::new("--input"),
        OsStr::new("candidate=x"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("CHECK005")
    );
}

#[cfg(unix)]
#[test]
fn project_reads_never_follow_manifest_parent_or_source_symlinks() {
    use std::os::unix::fs::symlink;

    let (project, manifest, _, _) = fixture();
    let linked_parent = project.0.with_extension("linked");
    symlink(&project.0, &linked_parent).unwrap();
    let output = opaal([
        OsStr::new("task"),
        OsStr::new("inspect"),
        OsStr::new("--project"),
        linked_parent.join("opaal.toml").as_os_str(),
        OsStr::new("release"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let evidence_target = project.write("evidence-real.json", "{}");
    let evidence = project.0.join("evidence.json");
    symlink(&evidence_target, &evidence).unwrap();
    let output = opaal([
        OsStr::new("task"),
        OsStr::new("inspect"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("release"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    fs::remove_file(evidence).unwrap();

    let source = project.0.join("tasks.opaal");
    let real_source = project.0.join("tasks-real.opaal");
    fs::rename(&source, &real_source).unwrap();
    symlink(&real_source, &source).unwrap();
    let output = opaal([
        OsStr::new("task"),
        OsStr::new("inspect"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("release"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    fs::remove_file(linked_parent).unwrap();
}
