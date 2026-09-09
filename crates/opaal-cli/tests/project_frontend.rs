#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fs;
#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use opaal_platform::operational::supports_exact_process_execution;

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

fn opaal_with_stdin(
    arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
    input: &[u8],
) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_opaal"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn host_platform_triple() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "unsupported"
    }
}

#[cfg(target_os = "linux")]
fn encoded_native(value: &Path) -> String {
    opaal_runtime::workflow::native_path(value)["value"]
        .as_str()
        .unwrap()
        .to_owned()
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
action normalize(candidate: String) -> String
effects {}
{
    return $candidate
}
## Check one candidate without executing it.
action ready(candidate: String) -> String
effects {
    filesystem.read(project::root);
    process.run(tools::git);
}
{
    return normalize($candidate)
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
        &format!(
            r#"schema_version = 1
project = "demo"
environment = "ci"
platform = "{}"
[child_environment]
inherit = []
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvZ2l0" }}
version = "2.50.0"
digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
"#,
            host_platform_triple()
        ),
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
    assert!(
        stdout.contains("documentation\n  Check one candidate without executing it.\n"),
        "{stdout:?}"
    );
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
    let process_supported = supports_exact_process_execution(host_platform_triple());
    assert_eq!(output.status.success(), process_supported, "{output:?}");
    if process_supported {
        assert!(output.stderr.is_empty(), "{output:?}");
    } else {
        assert!(String::from_utf8_lossy(&output.stderr).contains("CHECK008"));
    }
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!marker.exists());
}

#[test]
fn project_check_json_is_canonical_and_preserves_non_path_input_text() {
    let (project, manifest, authority, tools) = fixture();
    project.write("artifact.tar", "candidate-bytes");
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
        OsStr::new("--format"),
        OsStr::new("json"),
    ]);
    assert_eq!(
        output.status.success(),
        supports_exact_process_execution(host_platform_triple()),
        "{output:?}"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    let artifact = opaal_runtime::workflow::CheckArtifact::parse(&output.stdout).unwrap();
    assert_eq!(artifact.value()["schema"], "opaal.check.v1");
    assert_eq!(artifact.value()["inputs"][0]["name"], "candidate");
    assert_eq!(artifact.value()["inputs"][0]["type"], "String");
    assert_eq!(artifact.value()["inputs"][0]["value"], "artifact.tar");
    assert!(artifact.value()["inputs"][0]["path"].is_null());
    assert!(artifact.value()["inputs"][0]["digest"].is_null());
    assert!(artifact.value()["inputs"][0]["size"].is_null());
    assert_eq!(artifact.value()["tools"][0]["version_verified"], false);
    assert_eq!(
        artifact.value()["authority"]["requests"][1]["verdict"],
        if supports_exact_process_execution(host_platform_triple()) {
            "granted-unenforced"
        } else {
            "unsupported"
        }
    );
}

#[test]
fn project_check_exposes_unknown_enforcement_for_an_unrecognized_target() {
    let (_project, manifest, authority, tools) = fixture();
    let lock = fs::read_to_string(&tools)
        .unwrap()
        .replace(host_platform_triple(), "future-unknown-platform");
    fs::write(&tools, lock).unwrap();
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
        OsStr::new("--format"),
        OsStr::new("json"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let artifact = opaal_runtime::workflow::CheckArtifact::parse(&output.stdout).unwrap();
    assert_eq!(
        artifact.value()["authority"]["requests"][1]["verdict"],
        "unknown"
    );
    assert!(
        artifact.value()["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|finding| {
                finding["code"] == "CHECK009"
                    && finding["message"]
                        == "maintained process enforcement is unknown on this platform"
            })
    );
    assert!(!artifact.is_executable());
}

#[test]
fn project_plan_binds_observations_without_probing_or_overwriting() {
    let (project, manifest, authority, tools) = fixture();
    project.write("artifact.tar", "candidate-bytes");
    let executable = project.write("locked-git", "not an executable probe");
    let encoded = opaal_runtime::workflow::native_path(&executable)["value"]
        .as_str()
        .unwrap()
        .to_owned();
    let executable_digest = opaal_runtime::workflow::digest_bytes(&fs::read(&executable).unwrap());
    let platform = if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "unsupported"
    };
    fs::write(
        &tools,
        format!(
            r#"schema_version = 1
project = "demo"
environment = "ci"
platform = "{platform}"
[child_environment]
inherit = []
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{encoded}" }}
version = "2.50.0"
digest = "{executable_digest}"
"#
        ),
    )
    .unwrap();
    let out = project.0.join("release.plan.json");
    let arguments = [
        OsStr::new("plan"),
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
        OsStr::new("--expires-in"),
        OsStr::new("900s"),
        OsStr::new("--out"),
        out.as_os_str(),
    ];
    let output = opaal(arguments);
    let process_supported = supports_exact_process_execution(platform);
    assert_eq!(output.status.success(), process_supported, "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let plan = opaal_runtime::workflow::PlanArtifact::parse(&fs::read(&out).unwrap()).unwrap();
    assert_eq!(plan.is_executable(), process_supported);
    assert_eq!(plan.value()["platform"]["triple"], platform);
    assert_eq!(plan.value()["tools"][0]["version_verified"], false);
    let kinds = plan.value()["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "manifest",
            "source",
            "authority",
            "tool-lock",
            "tool-executable",
            "child-environment",
            "wall-clock"
        ]
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "plan {}{}\n",
            plan.digest(),
            if process_supported { "" } else { " refused" }
        )
    );

    if !process_supported {
        let journal = project.0.join("unsupported.run.jsonl");
        let execute = opaal_with_stdin(
            [
                OsStr::new("execute"),
                OsStr::new("--plan"),
                out.as_os_str(),
                OsStr::new("--accept"),
                OsStr::new(plan.digest()),
                OsStr::new("--run-id"),
                OsStr::new("0000000000000000000000000000000a"),
                OsStr::new("--authority"),
                authority.as_os_str(),
                OsStr::new("--secret-stdin"),
                OsStr::new("unused"),
                OsStr::new("--journal"),
                journal.as_os_str(),
            ],
            b"",
        );
        assert_eq!(execute.status.code(), Some(1), "{execute:?}");
        assert!(
            String::from_utf8(execute.stderr)
                .unwrap()
                .contains("EXECUTE003")
        );
        assert!(!journal.exists());
    }

    let second = opaal(arguments);
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    assert!(
        String::from_utf8(second.stderr)
            .unwrap()
            .contains("PLAN010")
    );
    assert_eq!(
        opaal_runtime::workflow::PlanArtifact::parse(&fs::read(&out).unwrap()).unwrap(),
        plan
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
    let refused_out = project.0.join("release.refused.plan.json");
    let refused = opaal([
        OsStr::new("plan"),
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
        OsStr::new("--expires-in"),
        OsStr::new("900s"),
        OsStr::new("--out"),
        refused_out.as_os_str(),
    ]);
    assert_eq!(refused.status.code(), Some(1), "{refused:?}");
    assert!(refused.stderr.is_empty(), "{refused:?}");
    let refused_plan =
        opaal_runtime::workflow::PlanArtifact::parse(&fs::read(refused_out).unwrap()).unwrap();
    assert!(!refused_plan.is_executable());
    assert!(
        String::from_utf8(refused.stdout)
            .unwrap()
            .contains(" refused\n")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn accepted_execution_revalidates_probes_refuses_unused_secret_and_audits() {
    let project = TempProject::new();
    let manifest = project.write(
        "opaal.toml",
        r#"schema_version = 1
[project]
name = "execute_demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[tools.git]
adapter = "git"
version = ">=2.39.0,<3.0.0"
[endpoints.readiness]
url = "http://127.0.0.1:43119/readiness"
methods = ["GET", "POST"]
secret_headers = ["authorization", "x-readiness-token"]
tls = false
[secrets.token]
kind = "injected"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    );
    project.write(
        "tasks.opaal",
        r#"import project::context as project
import project::tools as tools
import project::endpoints as endpoints
import project::secrets as secrets
action normalize(candidate: String) -> String
effects {}
{
    return $candidate
}
action ready(candidate: String) -> String
effects {
    filesystem.read(project::root);
    process.run(tools::git);
    network.http(endpoints::readiness);
    secret.reveal(secrets::token, endpoints::readiness);
}


{
    return normalize($candidate)
}
task release = ready
"#,
    );
    let authority = project.write(
        "authority.toml",
        r#"schema_version = 1
project = "execute_demo"
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
[[rules]]
decision = "grant"
effect = "network.http"
scope = "endpoint.readiness"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "secret.reveal"
scope = "secret.token@endpoint.readiness"
required_enforcement = "enforced"
"#,
    );
    let marker = project.0.join("probe.marker");
    let tool = project.write(
        "locked-git",
        &format!(
            "#!/bin/sh\nprintf 'probe\\n' >> '{}'\nprintf 'git version 2.50.0\\n'\n",
            marker.display()
        ),
    );
    let mut permissions = fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&tool, permissions).unwrap();
    let empty_git_config = project.write("empty.gitconfig", "");
    let tool_digest = opaal_runtime::workflow::digest_bytes(&fs::read(&tool).unwrap());
    let tools = project.0.join("tools.toml");
    fs::write(
        &tools,
        format!(
            r#"schema_version = 1
project = "execute_demo"
environment = "ci"
platform = "{}"
[child_environment]
inherit = []
[[child_environment.variables]]
name = "HOME"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "TMPDIR"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "PATH"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "CARGO_HOME"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "RUSTC"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "RUSTDOC"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "LC_ALL"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "Qw" }}
[[child_environment.variables]]
name = "TZ"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "VVRD" }}
[[child_environment.variables]]
name = "CARGO_NET_OFFLINE"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "dHJ1ZQ" }}
[[child_environment.variables]]
name = "GIT_CONFIG_NOSYSTEM"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "MQ" }}
[[child_environment.variables]]
name = "GIT_CONFIG_GLOBAL"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
version = "2.50.0"
digest = "{}"
"#,
            host_platform_triple(),
            encoded_native(&project.0),
            encoded_native(&project.0),
            encoded_native(Path::new("/usr/bin:/bin")),
            encoded_native(&project.0),
            encoded_native(Path::new("/usr/bin/false")),
            encoded_native(Path::new("/usr/bin/false")),
            encoded_native(&empty_git_config),
            encoded_native(&tool),
            tool_digest,
        ),
    )
    .unwrap();
    project.write("artifact.tar", "candidate-bytes");
    let plan_path = project.0.join("release.plan.json");
    let plan_run = opaal([
        OsStr::new("plan"),
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
        OsStr::new("--expires-in"),
        OsStr::new("900s"),
        OsStr::new("--out"),
        plan_path.as_os_str(),
    ]);
    assert!(plan_run.status.success(), "{plan_run:?}");
    let plan =
        opaal_runtime::workflow::PlanArtifact::parse(&fs::read(&plan_path).unwrap()).unwrap();
    assert_eq!(plan.value()["actions"].as_array().unwrap().len(), 2);
    let requests = plan.value()["authority"]["requests"].as_array().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request["scope"]["method"] == "GET")
    );
    assert!(
        requests
            .iter()
            .any(|request| request["scope"]["method"] == "POST")
    );
    assert!(
        requests
            .iter()
            .any(|request| request["scope"]["header"] == "authorization")
    );
    assert!(
        requests
            .iter()
            .any(|request| request["scope"]["header"] == "x-readiness-token")
    );

    let substituted_authority = project.0.join("substituted-authority.toml");
    fs::copy(&authority, &substituted_authority).unwrap();
    let mut substituted_document = plan.value().clone();
    substituted_document
        .as_object_mut()
        .unwrap()
        .remove("digest");
    let substituted_path = opaal_runtime::workflow::native_path(&substituted_authority);
    substituted_document["authority"]["path"] = substituted_path.clone();
    substituted_document["observations"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|observation| observation["kind"] == "authority")
        .unwrap()["path"] = substituted_path;
    let substituted_plan =
        opaal_runtime::workflow::PlanArtifact::seal(substituted_document).unwrap();
    let substituted_plan_path = project.0.join("substituted.plan.json");
    fs::write(&substituted_plan_path, substituted_plan.bytes()).unwrap();
    let substituted_journal = project.0.join("substituted.run.jsonl");
    let substituted = opaal_with_stdin(
        [
            OsStr::new("execute"),
            OsStr::new("--plan"),
            substituted_plan_path.as_os_str(),
            OsStr::new("--accept"),
            OsStr::new(substituted_plan.digest()),
            OsStr::new("--run-id"),
            OsStr::new("00000000000000000000000000000000"),
            OsStr::new("--authority"),
            substituted_authority.as_os_str(),
            OsStr::new("--secret-stdin"),
            OsStr::new("token"),
            OsStr::new("--journal"),
            substituted_journal.as_os_str(),
        ],
        b"canary-execute-secret",
    );
    assert_eq!(substituted.status.code(), Some(1), "{substituted:?}");
    assert!(
        String::from_utf8(substituted.stderr)
            .unwrap()
            .contains("EXECUTE_STALE")
    );
    assert!(!marker.exists());
    assert!(!substituted_journal.exists());

    let journal = project.0.join("release.run.jsonl");
    let execute_arguments = vec![
        OsStr::new("execute").to_os_string(),
        OsStr::new("--plan").to_os_string(),
        plan_path.as_os_str().to_os_string(),
        OsStr::new("--accept").to_os_string(),
        OsStr::new(plan.digest()).to_os_string(),
        OsStr::new("--run-id").to_os_string(),
        OsStr::new("00000000000000000000000000000001").to_os_string(),
        OsStr::new("--authority").to_os_string(),
        authority.as_os_str().to_os_string(),
        OsStr::new("--secret-stdin").to_os_string(),
        OsStr::new("token").to_os_string(),
        OsStr::new("--journal").to_os_string(),
        journal.as_os_str().to_os_string(),
    ];
    let run = opaal_with_stdin(&execute_arguments, b"canary-execute-secret");
    assert_eq!(
        run.status.code(),
        Some(1),
        "{run:?}\n{}",
        fs::read_to_string(&journal).unwrap_or_default()
    );
    assert!(run.stderr.is_empty(), "{run:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "probe\n");
    let journal_bytes = fs::read(&journal).unwrap();
    assert!(
        !journal_bytes
            .windows(b"canary-execute-secret".len())
            .any(|window| window == b"canary-execute-secret")
    );
    let audit = opaal_runtime::workflow::audit_journal(&journal_bytes).unwrap();
    assert!(audit.is_complete());
    let kinds = audit.value()["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "effect-before",
            "effect-after",
            "action-start",
            "action-start",
            "action-end",
            "action-end",
            "terminal"
        ]
    );
    assert_eq!(audit.value()["primary"]["class"], "refused");
    assert_eq!(
        audit.value()["events"][5]["payload"]["outcome"]["code"],
        "EXECUTE_UNUSED_SECRET"
    );

    let duplicate = opaal_with_stdin(&execute_arguments, b"canary-execute-secret");
    assert_eq!(duplicate.status.code(), Some(1), "{duplicate:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "probe\n");

    let second_journal = project.0.join("release-second.run.jsonl");
    let mut same_label_different_journal = execute_arguments.clone();
    *same_label_different_journal
        .last_mut()
        .expect("journal path is the final execute argument") =
        second_journal.as_os_str().to_os_string();
    let second = opaal_with_stdin(&same_label_different_journal, b"canary-execute-secret");
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "probe\nprobe\n");
    let second_audit =
        opaal_runtime::workflow::audit_journal(&fs::read(second_journal).unwrap()).unwrap();
    assert_eq!(second_audit.value()["run_id"], audit.value()["run_id"]);

    let audit_path = project.0.join("release.audit.json");
    let audit_run = opaal([
        OsStr::new("audit"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--journal"),
        journal.as_os_str(),
        OsStr::new("--out"),
        audit_path.as_os_str(),
    ]);
    assert!(audit_run.status.success(), "{audit_run:?}");
    assert!(
        opaal_runtime::workflow::AuditArtifact::parse(&fs::read(audit_path).unwrap())
            .unwrap()
            .is_complete()
    );
}

#[test]
fn caught_adapter_failure_finishes_successfully_with_partial_evidence() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
    });
    let project = TempProject::new();
    let manifest = project.write(
        "opaal.toml",
        &format!(
            r#"schema_version = 1
[project]
name = "caught_failure"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[endpoints.failing]
url = "http://127.0.0.1:{port}/failure"
methods = ["GET"]
secret_headers = ["authorization"]
tls = false
[secrets.token]
kind = "injected"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#
        ),
    );
    project.write(
        "tasks.opaal",
        r#"import project::endpoints as endpoints
import project::secrets as secrets
import std::http as http

action recover() -> String
effects {
    network.http(endpoints::failing);
    secret.reveal(secrets::token, endpoints::failing);
}
{
    let authorization = http::secret_header("authorization", secrets::token)
    try {
        http::request(endpoints::failing, "GET", {}, $authorization, null, 1024)
        return "unexpected"
    } catch error {
        return "recovered"
    }
}

task recover = recover
"#,
    );
    let authority = project.write(
        "authority.toml",
        r#"schema_version = 1
project = "caught_failure"
environment = "ci"
[[rules]]
decision = "grant"
effect = "network.http"
scope = "endpoint.failing"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "secret.reveal"
scope = "secret.token@endpoint.failing"
required_enforcement = "enforced"
"#,
    );
    let tools = project.write(
        "tools.toml",
        &format!(
            r#"schema_version = 1
project = "caught_failure"
environment = "ci"
platform = "{}"
tools = []
[child_environment]
inherit = []
"#,
            host_platform_triple()
        ),
    );
    let plan_path = project.0.join("caught.plan.json");
    let planned = opaal([
        OsStr::new("plan"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--task"),
        OsStr::new("recover"),
        OsStr::new("--environment"),
        OsStr::new("ci"),
        OsStr::new("--authority"),
        authority.as_os_str(),
        OsStr::new("--tools"),
        tools.as_os_str(),
        OsStr::new("--expires-in"),
        OsStr::new("900s"),
        OsStr::new("--out"),
        plan_path.as_os_str(),
    ]);
    assert!(planned.status.success(), "{planned:?}");
    let plan =
        opaal_runtime::workflow::PlanArtifact::parse(&fs::read(&plan_path).unwrap()).unwrap();
    let journal = project.0.join("caught.run.jsonl");
    let run = opaal_with_stdin(
        [
            OsStr::new("execute"),
            OsStr::new("--plan"),
            plan_path.as_os_str(),
            OsStr::new("--accept"),
            OsStr::new(plan.digest()),
            OsStr::new("--run-id"),
            OsStr::new("00000000000000000000000000000013"),
            OsStr::new("--authority"),
            authority.as_os_str(),
            OsStr::new("--secret-stdin"),
            OsStr::new("token"),
            OsStr::new("--journal"),
            journal.as_os_str(),
        ],
        b"canary-caught-secret",
    );
    assert!(
        run.status.success(),
        "{run:?}\n{}",
        fs::read_to_string(&journal).unwrap_or_default()
    );
    server.join().unwrap();

    let audit = opaal_runtime::workflow::audit_journal(&fs::read(journal).unwrap()).unwrap();
    assert!(audit.is_complete());
    assert_eq!(audit.value()["primary"]["class"], "success");
    assert_eq!(audit.value()["primary"]["partial"], true);
    let failed_effects = audit.value()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "effect-after")
        .map(|event| &event["payload"]["outcome"])
        .collect::<Vec<_>>();
    assert_eq!(failed_effects.len(), 2);
    assert!(
        failed_effects
            .iter()
            .all(|outcome| outcome["class"] == "error" && outcome["partial"] == true)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn accepted_execution_runs_the_maintained_readiness_modules() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        for status in [200, 503] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request.starts_with("GET /readiness HTTP/1.1\r\n"),
                "{request}"
            );
            assert!(
                request.contains("authorization: canary-readiness-secret\r\n"),
                "{request}"
            );
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
    });

    let project = TempProject::new();
    let manifest = project.write(
        "opaal.toml",
        &format!(
            r#"schema_version = 1
[project]
name = "readiness_demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[tools.git]
adapter = "git"
version = ">=2.39.0,<3.0.0"
[endpoints.readiness]
url = "http://127.0.0.1:{port}/readiness"
methods = ["GET"]
secret_headers = ["authorization"]
tls = false
[secrets.readiness_token]
kind = "injected"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#
        ),
    );
    project.write(
        "tasks.opaal",
        r#"import project::context as project
import project::endpoints as endpoints
import project::secrets as secrets
import project::tools as tools
import std::data as data
import std::filesystem as fs
import std::http as http
import std::integrity as integrity
import std::path as path
import std::process as process
import std::time as time
import std::version as version

type Readiness = {
    candidate: Path,
    candidate_digest: String,
    manifest_digest: String,
    lock_digest: String,
    git_status: Status,
    service_status: Int,
    checked_at: time::Timestamp,
}

action release_readiness(candidate: Path) -> Readiness
effects {
    filesystem.read(project::root);
    filesystem.write(project::evidence);
    process.run(tools::git);
    network.http(endpoints::readiness);
    secret.reveal(secrets::readiness_token, endpoints::readiness);
    clock.wall;
}
{
    let manifest_path = path::join(project::root, "Cargo.toml")
    let lock_path = path::join(project::root, "Cargo.lock")
    let candidate_bytes = fs::read($candidate, 16777216)
    let manifest_bytes = fs::read($manifest_path, 1048576)
    let lock_bytes = fs::read($lock_path, 8388608)
    let manifest = data::toml_decode($manifest_bytes)
    let package_version = data::get($manifest, ["workspace", "package", "version"])
    let expected = version::parse("1.0.0-alpha.1")
    let rendered_expected = version::render($expected)
    if $package_version != $rendered_expected {
        throw "workspace version differs from the readiness contract"
    }
    let git_result = process::run(tools::git, ["diff", "--quiet", "--", "Cargo.toml", "Cargo.lock"])
    let authorization = http::secret_header("authorization", secrets::readiness_token)
    let response = http::request(endpoints::readiness, "GET", {}, $authorization, null, 8388608)
    if !$git_result.status.ok {
        throw "tracked readiness inputs are dirty"
    }
    if $response.status != 200 {
        throw "readiness endpoint returned an unsuccessful status"
    }
    let report = Readiness {
        candidate: $candidate,
        candidate_digest: integrity::sha256($candidate_bytes),
        manifest_digest: integrity::sha256($manifest_bytes),
        lock_digest: integrity::sha256($lock_bytes),
        git_status: $git_result.status,
        service_status: $response.status,
        checked_at: time::wall_now(),
    }
    fs::write_atomic(project::evidence, data::json_encode($report))
    return $report
}

task release_readiness = release_readiness
"#,
    );
    let authority = project.write(
        "authority.toml",
        r#"schema_version = 1
project = "readiness_demo"
environment = "ci"
[[rules]]
decision = "grant"
effect = "filesystem.read"
scope = "project.root"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "filesystem.write"
scope = "project.evidence"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "process.run"
scope = "tool.git"
required_enforcement = "acknowledge-unenforced"
[[rules]]
decision = "grant"
effect = "network.http"
scope = "endpoint.readiness"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "secret.reveal"
scope = "secret.readiness_token@endpoint.readiness"
required_enforcement = "enforced"
[[rules]]
decision = "grant"
effect = "clock.wall"
scope = "evaluation"
required_enforcement = "enforced"
"#,
    );
    let marker = project.0.join("tool.marker");
    let tool = project.write(
        "locked-git",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'probe\\n' >> '{}'; printf 'git version 2.50.0\\n'; else printf 'run\\n' >> '{}'; fi\n",
            marker.display(),
            marker.display(),
        ),
    );
    let mut permissions = fs::metadata(&tool).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&tool, permissions).unwrap();
    let empty_git_config = project.write("empty.gitconfig", "");
    let tool_digest = opaal_runtime::workflow::digest_bytes(&fs::read(&tool).unwrap());
    let tools = project.0.join("tools.toml");
    fs::write(
        &tools,
        format!(
            r#"schema_version = 1
project = "readiness_demo"
environment = "ci"
platform = "{}"
[child_environment]
inherit = []
[[child_environment.variables]]
name = "HOME"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "TMPDIR"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "PATH"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "CARGO_HOME"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "RUSTC"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "RUSTDOC"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[child_environment.variables]]
name = "LC_ALL"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "Qw" }}
[[child_environment.variables]]
name = "TZ"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "VVRD" }}
[[child_environment.variables]]
name = "CARGO_NET_OFFLINE"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "dHJ1ZQ" }}
[[child_environment.variables]]
name = "GIT_CONFIG_NOSYSTEM"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "MQ" }}
[[child_environment.variables]]
name = "GIT_CONFIG_GLOBAL"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{}" }}
version = "2.50.0"
digest = "{}"
"#,
            host_platform_triple(),
            encoded_native(&project.0),
            encoded_native(&project.0),
            encoded_native(Path::new("/usr/bin:/bin")),
            encoded_native(&project.0),
            encoded_native(Path::new("/usr/bin/false")),
            encoded_native(Path::new("/usr/bin/false")),
            encoded_native(&empty_git_config),
            encoded_native(&tool),
            tool_digest,
        ),
    )
    .unwrap();
    project.write(
        "Cargo.toml",
        "[workspace.package]\nversion = \"1.0.0-alpha.1\"\n",
    );
    project.write("Cargo.lock", "version = 4\n");
    project.write("artifact.tar", "candidate-bytes");

    let plan_path = project.0.join("readiness.plan.json");
    let plan_run = opaal([
        OsStr::new("plan"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--task"),
        OsStr::new("release_readiness"),
        OsStr::new("--environment"),
        OsStr::new("ci"),
        OsStr::new("--authority"),
        authority.as_os_str(),
        OsStr::new("--tools"),
        tools.as_os_str(),
        OsStr::new("--input"),
        OsStr::new("candidate=artifact.tar"),
        OsStr::new("--expires-in"),
        OsStr::new("900s"),
        OsStr::new("--out"),
        plan_path.as_os_str(),
    ]);
    assert!(plan_run.status.success(), "{plan_run:?}");
    let plan =
        opaal_runtime::workflow::PlanArtifact::parse(&fs::read(&plan_path).unwrap()).unwrap();
    let journal = project.0.join("readiness.run.jsonl");
    let run = opaal_with_stdin(
        [
            OsStr::new("execute"),
            OsStr::new("--plan"),
            plan_path.as_os_str(),
            OsStr::new("--accept"),
            OsStr::new(plan.digest()),
            OsStr::new("--run-id"),
            OsStr::new("00000000000000000000000000000011"),
            OsStr::new("--authority"),
            authority.as_os_str(),
            OsStr::new("--secret-stdin"),
            OsStr::new("readiness_token"),
            OsStr::new("--journal"),
            journal.as_os_str(),
        ],
        b"canary-readiness-secret",
    );
    assert!(
        run.status.success(),
        "{run:?}\n{}\n{}",
        String::from_utf8_lossy(&run.stderr),
        fs::read_to_string(&journal).unwrap_or_default()
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "probe\nrun\n");
    let evidence = fs::read_to_string(project.0.join("evidence.json")).unwrap();
    assert!(
        evidence.contains("\"candidate_digest\":\"sha256:"),
        "{evidence}"
    );
    assert!(!evidence.contains("canary-readiness-secret"), "{evidence}");
    let audit = opaal_runtime::workflow::audit_journal(&fs::read(journal).unwrap()).unwrap();
    assert!(audit.is_complete());
    assert_eq!(audit.value()["primary"]["class"], "success");
    let effects = audit.value()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "effect-before")
        .map(|event| event["payload"]["effect"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(effects.contains(&"filesystem.read"), "{effects:?}");
    assert!(effects.contains(&"process.run"), "{effects:?}");
    assert!(effects.contains(&"network.http"), "{effects:?}");
    assert!(effects.contains(&"secret.reveal"), "{effects:?}");
    assert!(effects.contains(&"clock.wall"), "{effects:?}");
    assert!(effects.contains(&"filesystem.write"), "{effects:?}");
    let http_events = audit.value()["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(
                event["payload"]["effect"].as_str(),
                Some("network.http" | "secret.reveal")
            )
        })
        .map(|event| {
            (
                event["kind"].as_str().unwrap(),
                event["payload"]["effect"].as_str().unwrap(),
                event["payload"]["scope"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        http_events,
        [
            (
                "effect-before",
                "network.http",
                serde_json::json!({"kind":"endpoint","endpoint":"readiness","method":"GET"}),
            ),
            (
                "effect-before",
                "secret.reveal",
                serde_json::json!({
                    "kind":"secret-sink",
                    "secret":"readiness_token",
                    "endpoint":"readiness",
                    "header":"authorization"
                }),
            ),
            (
                "effect-after",
                "secret.reveal",
                serde_json::json!({
                    "kind":"secret-sink",
                    "secret":"readiness_token",
                    "endpoint":"readiness",
                    "header":"authorization"
                }),
            ),
            (
                "effect-after",
                "network.http",
                serde_json::json!({"kind":"endpoint","endpoint":"readiness","method":"GET"}),
            ),
        ]
    );

    let source_path = project.0.join("tasks.opaal");
    let candidate_path = project.0.join("artifact.tar");
    let originals = [
        (
            "input",
            candidate_path.clone(),
            fs::read(&candidate_path).unwrap(),
        ),
        (
            "source",
            source_path.clone(),
            fs::read(&source_path).unwrap(),
        ),
        ("manifest", manifest.clone(), fs::read(&manifest).unwrap()),
        (
            "authority",
            authority.clone(),
            fs::read(&authority).unwrap(),
        ),
        ("tool-lock", tools.clone(), fs::read(&tools).unwrap()),
        ("executable", tool.clone(), fs::read(&tool).unwrap()),
    ];
    for (index, (identity, path, original)) in originals.iter().enumerate() {
        let mut changed = original.clone();
        changed.extend_from_slice(b"\n");
        fs::write(path, changed).unwrap();
        let stale_journal = project.0.join(format!("stale-{identity}.jsonl"));
        let run_id = format!("0000000000000000000000000000002{index}");
        let stale = opaal_with_stdin(
            [
                OsStr::new("execute").to_os_string(),
                OsStr::new("--plan").to_os_string(),
                plan_path.as_os_str().to_os_string(),
                OsStr::new("--accept").to_os_string(),
                OsStr::new(plan.digest()).to_os_string(),
                OsStr::new("--run-id").to_os_string(),
                OsStr::new(&run_id).to_os_string(),
                OsStr::new("--authority").to_os_string(),
                authority.as_os_str().to_os_string(),
                OsStr::new("--secret-stdin").to_os_string(),
                OsStr::new("readiness_token").to_os_string(),
                OsStr::new("--journal").to_os_string(),
                stale_journal.as_os_str().to_os_string(),
            ],
            b"must-not-be-read-on-stale",
        );
        assert_eq!(stale.status.code(), Some(1), "{identity}: {stale:?}");
        assert!(
            String::from_utf8(stale.stderr)
                .unwrap()
                .contains("EXECUTE_STALE"),
            "{identity}"
        );
        assert!(!stale_journal.exists(), "{identity}");
        assert_eq!(fs::read_to_string(&marker).unwrap(), "probe\nrun\n");
        fs::write(path, original).unwrap();
    }

    let partial_journal = project.0.join("readiness-partial.run.jsonl");
    let partial = opaal_with_stdin(
        [
            OsStr::new("execute"),
            OsStr::new("--plan"),
            plan_path.as_os_str(),
            OsStr::new("--accept"),
            OsStr::new(plan.digest()),
            OsStr::new("--run-id"),
            OsStr::new("00000000000000000000000000000012"),
            OsStr::new("--authority"),
            authority.as_os_str(),
            OsStr::new("--secret-stdin"),
            OsStr::new("readiness_token"),
            OsStr::new("--journal"),
            partial_journal.as_os_str(),
        ],
        b"canary-readiness-secret",
    );
    assert_eq!(partial.status.code(), Some(1), "{partial:?}");
    let partial_audit =
        opaal_runtime::workflow::audit_journal(&fs::read(partial_journal).unwrap()).unwrap();
    assert!(partial_audit.is_complete());
    assert_eq!(partial_audit.value()["primary"]["class"], "error");
    assert_eq!(partial_audit.value()["primary"]["partial"], true);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "probe\nrun\nprobe\nrun\n"
    );
    server.join().unwrap();
}

#[test]
fn ordinary_source_cannot_activate_operational_standard_modules() {
    let project = TempProject::new();
    let script = project.write(
        "pure.opaal",
        "import std::version as version\nversion::parse(\"1.0.0\")\n",
    );
    let output = opaal([script.as_os_str()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("operational standard module")
    );
}

#[test]
fn audit_publishes_an_incomplete_artifact_and_refuses_overwrite() {
    let (project, manifest, _, _) = fixture();
    let (_chain, journal) = opaal_runtime::workflow::JournalChain::begin(
        "00000000000000000000000000000001",
        serde_json::json!({
            "plan_digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "accepted_plan_digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "authority_digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222",
            "project_digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333",
            "environment_digest":"sha256:4444444444444444444444444444444444444444444444444444444444444444",
            "tool_lock_digest":"sha256:5555555555555555555555555555555555555555555555555555555555555555",
            "child_environment_digest":"sha256:6666666666666666666666666666666666666666666666666666666666666666",
            "started_at":"2026-09-09T08:01:00.000000000Z"
        }),
    )
    .unwrap();
    let journal_path = project.0.join("run.jsonl");
    fs::write(&journal_path, journal).unwrap();
    let audit_path = project.0.join("audit.json");
    let arguments = [
        OsStr::new("audit"),
        OsStr::new("--project"),
        manifest.as_os_str(),
        OsStr::new("--journal"),
        journal_path.as_os_str(),
        OsStr::new("--out"),
        audit_path.as_os_str(),
    ];

    let output = opaal(arguments);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let artifact =
        opaal_runtime::workflow::AuditArtifact::parse(&fs::read(&audit_path).unwrap()).unwrap();
    assert!(!artifact.is_complete());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("audit {} incomplete\n", artifact.digest())
    );

    let original = fs::read(&audit_path).unwrap();
    let second = opaal(arguments);
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    assert!(
        String::from_utf8(second.stderr)
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(fs::read(&audit_path).unwrap(), original);
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
        OsStr::new("--format"),
        OsStr::new("json"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let artifact = opaal_runtime::workflow::CheckArtifact::parse(&output.stdout).unwrap();
    assert_eq!(artifact.value()["outcome"]["class"], "refused");
    assert_eq!(
        artifact.value()["findings"].as_array().unwrap().len(),
        if supports_exact_process_execution(host_platform_triple()) {
            2
        } else {
            3
        }
    );
    assert!(
        artifact.value()["authority"]["requests"]
            .as_array()
            .unwrap()
            .iter()
            .all(|request| request["verdict"] == "denied")
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
