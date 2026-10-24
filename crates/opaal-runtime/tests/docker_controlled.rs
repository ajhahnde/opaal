#![forbid(unsafe_code)]
#[path = "support/docker.rs"]
mod docker_cases;
#[path = "support/random.rs"]
mod support;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use opaal_platform::operational::{
    FakeOperationalAdapter, OperationalCall, ProcessExit, ProcessOutput, UnixIdentity,
};
use opaal_platform::{AuthorityProfile, Capabilities, FakePlatform};
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, AuthorityVerdict, CapabilityRequest, EffectSet,
    EvaluationContextId, RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, FakeClock};
use opaal_runtime::module::ActionId;
use opaal_runtime::operational::docker::bind_target;
use opaal_runtime::operational::source::{
    ControlledSourceOperations, SourceActionOutcome, SourceEffectEvent, SourceEffectJournal,
    SourceEffectOutcome,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::project::{load_project_program, parse_project_manifest, parse_tool_lock};
use opaal_runtime::script::{ScriptExecutionOutcome, execute_project_task_outcome};
use opaal_runtime::{Environment, NativePath, NominalRecordValue, Value};
use serde_json::json;

const MANIFEST: &str = r#"schema_version = 1
[project]
name = "sample"
root_module = "tasks.opaal"
required_opaal = ">=1.4.0,<2.0.0"
[paths]
root = "."
evidence = "result.json"
[tools.docker]
adapter = "docker"
version = "=29.9.0"
[targets.ci]
adapter = "docker"
tool = "docker"
socket = { encoding = "base64url-nopad", platform = "unix", value = "L2ZpeHR1cmUvZG9ja2VyLnNvY2s" }
config_directory = "docker-empty"
api_version = "1.48"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#;

fn version() -> Vec<u8> {
    let arch = if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "arm64"
    } else {
        "amd64"
    };
    serde_json::to_vec(&json!({
        "Client": {"Version":"29.9.0","ApiVersion":"1.48","Os":"linux","Arch":arch},
        "Server": {"Version":"29.9.0","ApiVersion":"1.54","MinAPIVersion":"1.44","Os":"linux","Arch":arch}
    }))
    .unwrap()
}
fn outputs() -> Vec<(i32, Vec<u8>)> {
    vec![
        (0, b"Docker version 29.9.0, build abc123\n".to_vec()),
        (0, version()),
        (
            0,
            include_bytes!("../../../tests/golden/docker-image-checks/fixtures/image.json")
                .to_vec(),
        ),
        (0, version()),
    ]
}

#[derive(Default)]
struct Journal {
    count: usize,
    fail_before: bool,
    fail_after: bool,
    clock: Option<Arc<FakeClock>>,
    advance_nanos: u64,
    adapter: Option<FakeOperationalAdapter>,
    mutate_path: Option<(&'static str, bool)>,
    fault_at: usize,
    events: Vec<SourceEffectOutcome>,
    requested_at: Option<usize>,
    requested: Arc<std::sync::atomic::AtomicBool>,
}
impl SourceEffectJournal for Journal {
    fn action_start(&mut self, _: &str, _: &ActionId) -> Result<(), String> {
        Ok(())
    }
    fn action_end(&mut self, _: &str, _: &SourceActionOutcome) -> Result<(), String> {
        Ok(())
    }
    fn before(&mut self, _: &SourceEffectEvent, _: AuthorityVerdict) -> Result<(), String> {
        if self.requested_at == Some(self.count) {
            self.requested
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(clock) = &self.clock {
            clock.advance(self.advance_nanos);
        }
        if self.fail_before && self.count == self.fault_at {
            Err("injected before failure".into())
        } else {
            Ok(())
        }
    }
    fn after(
        &mut self,
        _: &SourceEffectEvent,
        outcome: &SourceEffectOutcome,
    ) -> Result<(), String> {
        self.count += 1;
        self.events.push(outcome.clone());
        if let Some((path, directory)) = self.mutate_path {
            self.adapter.as_ref().unwrap().insert_local_path(
                Path::new("/fixture").join(path),
                directory,
                UnixIdentity {
                    device: 1,
                    inode: 4,
                    mode: if directory { 0o040700 } else { 0o140600 },
                    uid: 1000,
                    gid: 1000,
                },
            );
        }
        assert!(!format!("{outcome:?}").contains("unknown-label-canary"));
        if self.fail_after && self.count - 1 == self.fault_at {
            Err("injected after failure".into())
        } else {
            Ok(())
        }
    }
}

fn run(
    body: &str,
    responses: Vec<(i32, Vec<u8>)>,
    grant: bool,
    journal: &mut Journal,
) -> (bool, Vec<OperationalCall>) {
    let (outcome, calls) = run_outputs(
        body,
        responses
            .into_iter()
            .map(|(status, stdout)| {
                Ok(ProcessOutput::new(
                    ProcessExit::Exited(status),
                    stdout,
                    b"unknown-label-canary".to_vec(),
                    Duration::from_millis(1),
                ))
            })
            .collect(),
        grant,
        journal,
    );
    (
        matches!(outcome.primary(), PrimaryOutcome::Completed(value) if value.value() == &Value::Bool(true)),
        calls,
    )
}

fn run_outputs(
    body: &str,
    responses: Vec<Result<ProcessOutput, opaal_platform::operational::OperationalError>>,
    grant: bool,
    journal: &mut Journal,
) -> (ScriptExecutionOutcome, Vec<OperationalCall>) {
    run_task(body, None, "Bool", responses, grant, journal)
}

fn run_task(
    body: &str,
    input: Option<(&str, Value)>,
    result_type: &str,
    responses: Vec<Result<ProcessOutput, opaal_platform::operational::OperationalError>>,
    grant: bool,
    journal: &mut Journal,
) -> (ScriptExecutionOutcome, Vec<OperationalCall>) {
    let manifest =
        parse_project_manifest(Path::new("/fixture/opaal.toml"), MANIFEST.as_bytes()).unwrap();
    let input_type = input.as_ref().map_or("Bytes", |(kind, _)| *kind);
    let source = support::Source(format!(
        "import std::docker as docker\nimport std::data as data\nimport std::outcome as outcome\nimport project::targets as targets\nimport project::tools as tools\ntype Wrapper = {{ target: docker::TargetIdentity }}\naction check(input: {input_type}) -> {result_type} effects {{ process.run(tools::docker); }} {{ {body} }}\ntask check = check\n"
    ));
    let project = load_project_program(manifest, &source, &source).unwrap();
    let executable = b"retained Docker test executable";
    let path = std::env::temp_dir().join(format!(
        "opaal-docker-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    file.write_all(executable).unwrap();
    let lock = format!(
        "schema_version = 1\nproject = 'sample'\nenvironment = 'ci'\nplatform = 'x86_64-unknown-linux-gnu'\n[child_environment]\ninherit = []\n[[tools]]\nid = 'docker'\nadapter = 'docker'\npath = {{ encoding = 'base64url-nopad', platform = 'unix', value = 'L3Rvb2wvZG9ja2Vy' }}\nversion = '29.9.0'\ndigest = '{}'\n",
        opaal_runtime::workflow::digest_bytes(executable)
    );
    let tools = parse_tool_lock(project.manifest(), "ci", lock.as_bytes()).unwrap();
    let adapter = FakeOperationalAdapter::new();
    adapter.insert_file("/tool/docker", executable.to_vec());
    for path in ["docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"] {
        adapter.insert_local_path(
            Path::new("/fixture").join(path),
            true,
            UnixIdentity {
                device: 1,
                inode: 2,
                mode: 0o040700,
                uid: 1000,
                gid: 1000,
            },
        );
    }
    adapter.insert_local_path(
        "/fixture/docker.sock",
        false,
        UnixIdentity {
            device: 1,
            inode: 3,
            mode: 0o140600,
            uid: 1000,
            gid: 1000,
        },
    );
    let bound = bind_target(
        project.manifest(),
        &project.manifest().targets()["ci"],
        &adapter,
    )
    .unwrap();
    let input = input.map_or_else(
        || {
            Value::bytes(
                responses
                    .get(2)
                    .and_then(|response| response.as_ref().ok())
                    .map(|output| output.stdout().to_vec())
                    .unwrap_or_default(),
            )
        },
        |(_, value)| value,
    );
    for response in responses {
        adapter.push_process(response);
    }
    let request = CapabilityRequest::process_run("docker").unwrap();
    let clock = Arc::new(FakeClock::new());
    journal.clock = Some(clock.clone());
    journal.adapter = Some(adapter.clone());
    let requested = journal.requested.clone();
    let cancel =
        CancellationToken::from_fn(move || requested.load(std::sync::atomic::Ordering::Relaxed));
    let mut context = OperationalContext::new(
        AuthorityContext::new(
            EvaluationContextId::new(71).unwrap(),
            if grant {
                vec![AuthorityRule::grant(
                    request.clone(),
                    RequiredEnforcement::AcknowledgeUnenforced,
                )]
            } else {
                vec![]
            },
        )
        .unwrap(),
        cancel.clone(),
        clock.clone(),
        None,
    );
    let effects = EffectSet::new([request]);
    let platform =
        FakePlatform::with_authority_profile(Capabilities::full(), AuthorityProfile::enforced());
    let files = BTreeMap::<String, File>::from([("docker".into(), file)]);
    let tls = BTreeMap::new();
    let nodes = project
        .modules()
        .sources()
        .entries()
        .flat_map(|entry| project.modules().actions().actions(entry.module()).iter())
        .map(|action| (action.id().clone(), action.id().qualified_name()))
        .collect();
    let mut operations = ControlledSourceOperations::new(
        &mut context,
        &effects,
        project.manifest(),
        &tools,
        &files,
        &platform,
        &adapter,
        &tls,
        journal,
        nodes,
        BTreeMap::new(),
        Default::default(),
        None,
    )
    .with_docker_targets(BTreeMap::from([("ci".into(), bound)]));
    let outcome = execute_project_task_outcome(
        &project,
        project.task("check").unwrap(),
        vec![input],
        project.manifest().root(),
        &mut Environment::new(),
        &opaal_runtime::builtin::standard_registry(),
        &NoExecutables,
        &Default::default(),
        &platform,
        clock,
        cancel,
        &mut operations,
        &mut Vec::new(),
    );
    (outcome, adapter.calls())
}
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct NoExecutables;
impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("no ambient executable search")
    }
}
fn process_count(calls: &[OperationalCall]) -> usize {
    calls
        .iter()
        .filter(|call| matches!(call, OperationalCall::Process { .. }))
        .count()
}
const BODY: &str =
    "let image = docker::inspect_image(targets::ci, 'example:test')\nreturn image.os == 'linux'";

#[test]
fn four_fixed_children_share_target_environment_and_return_the_selected_image() {
    let mut journal = Journal::default();
    let (passed, calls) = run(BODY, outputs(), true, &mut journal);
    assert!(passed, "{calls:?}");
    assert_eq!(journal.count, 4);
    let processes = calls
        .iter()
        .filter_map(|call| {
            if let OperationalCall::Process {
                argv, environment, ..
            } = call
            {
                Some((argv, environment))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let vectors = [
        vec![
            "--config",
            "/fixture/docker-empty",
            "--host",
            "unix:///fixture/docker.sock",
            "--version",
        ],
        vec![
            "--config",
            "/fixture/docker-empty",
            "--host",
            "unix:///fixture/docker.sock",
            "version",
            "--format",
            "{{json .}}",
        ],
        vec![
            "--config",
            "/fixture/docker-empty",
            "--host",
            "unix:///fixture/docker.sock",
            "image",
            "inspect",
            "--format",
            "{{json .}}",
            "--",
            "example:test",
        ],
        vec![
            "--config",
            "/fixture/docker-empty",
            "--host",
            "unix:///fixture/docker.sock",
            "version",
            "--format",
            "{{json .}}",
        ],
    ];
    for ((argv, env), expected) in processes.iter().zip(vectors) {
        assert_eq!(argv[0], "/tool/docker");
        assert_eq!(
            argv[1..],
            expected
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            *env,
            &vec![
                ("LC_ALL".into(), "C".into()),
                ("TZ".into(), "UTC".into()),
                ("PATH".into(), "/fixture/empty-path".into()),
                ("HOME".into(), "/fixture/sandbox-home".into()),
                ("TMPDIR".into(), "/fixture/sandbox-tmp".into()),
                ("DOCKER_API_VERSION".into(), "1.48".into()),
            ]
        );
        assert_eq!(env, &processes[0].1);
    }
    assert_eq!(
        calls
            .iter()
            .filter(|call| matches!(call, OperationalCall::LocalObservation { .. }))
            .count(),
        45
    );
}

#[test]
fn failed_status_stops_before_decoding_even_when_stdout_is_complete() {
    for stage in 0..4 {
        let mut responses = outputs();
        responses[stage].0 = 1;
        let (passed, calls) = run(BODY, responses, true, &mut Journal::default());
        assert!(!passed);
        assert_eq!(process_count(&calls), stage + 1);
    }
}

#[test]
fn forged_sentinel_and_denied_calls_do_no_process_work() {
    for body in [
        "let image = docker::inspect_image(docker::TargetIdentity {}, 'example:test')\nreturn true",
        "let image = docker::inspect_image(targets::ci, '__complete')\nreturn true",
        "let image = docker::inspect_image(targets::ci, '__completeNoDesc')\nreturn true",
        "let image = docker::inspect_image(targets::ci, '-x')\nreturn true",
        "let image = docker::inspect_image(targets::ci, 'bad ref')\nreturn true",
    ] {
        let (passed, calls) = run(body, outputs(), true, &mut Journal::default());
        assert!(!passed);
        assert_eq!(process_count(&calls), 0);
    }
    let (passed, calls) = run(BODY, outputs(), false, &mut Journal::default());
    assert!(!passed);
    assert_eq!(process_count(&calls), 0);
}

#[test]
fn invalid_responses_and_changed_server_profile_never_admit_an_image() {
    for (stage, bytes, count) in [
        (0, b"Docker version 29.8.0, build abc\n".to_vec(), 1),
        (1, b"{}".to_vec(), 2),
        (2, b"[]".to_vec(), 3),
        (2, b"{\"Id\":1,\"Id\":2}".to_vec(), 3),
        (2, b"{\"Id\":false}".to_vec(), 4),
        (
            3,
            String::from_utf8(version())
                .unwrap()
                .replace("1.54", "1.55")
                .into_bytes(),
            4,
        ),
    ] {
        let mut responses = outputs();
        responses[stage].1 = bytes;
        let (passed, calls) = run(BODY, responses, true, &mut Journal::default());
        assert!(!passed);
        assert_eq!(process_count(&calls), count);
    }
}

#[test]
fn evidence_failures_stop_the_fixed_sequence() {
    for before in [true, false] {
        let mut journal = Journal {
            fail_before: before,
            fail_after: !before,
            ..Default::default()
        };
        let (passed, calls) = run(BODY, outputs(), true, &mut journal);
        assert!(!passed);
        assert_eq!(process_count(&calls), usize::from(!before));
    }
}

#[test]
fn admission_time_and_target_changes_cannot_extend_or_continue_acquisition() {
    for (advance, expected) in [(30_000_000_000, 0), (20_000_000_000, 2)] {
        let mut journal = Journal {
            advance_nanos: advance,
            ..Default::default()
        };
        let (passed, calls) = run(BODY, outputs(), true, &mut journal);
        assert!(!passed);
        assert_eq!(process_count(&calls), expected);
    }
    let mut journal = Journal {
        mutate_path: Some(("docker.sock", false)),
        ..Default::default()
    };
    let (passed, calls) = run(BODY, outputs(), true, &mut journal);
    assert!(!passed);
    assert_eq!(process_count(&calls), 1);
}

#[test]
fn named_operation_corpus_asserts_typed_outcomes_counts_and_redacted_evidence() {
    let manifest: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../tests/golden/docker-image-checks/operations/cases.json"
    ))
    .unwrap();
    assert_eq!(manifest["schema_version"], 1);
    let cases = manifest["cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    let mut names = std::collections::BTreeSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        assert!(names.insert(name), "duplicate case: {name}");
        let mut case = case.clone();
        if cfg!(all(target_os = "linux", target_arch = "aarch64")) && name.ends_with("-arch") {
            for change in case["changes"].as_array_mut().unwrap() {
                if change["path"][1] == "Arch" {
                    change["value"] = json!("amd64");
                }
            }
        }
        let mut responses = outputs();
        for stage in case["stages"].as_array().unwrap() {
            let stage = stage.as_u64().unwrap() as usize;
            if let Some(status) = case["status"].as_i64() {
                responses[stage].0 = status as i32;
            } else {
                responses[stage].1 =
                    docker_cases::response(serde_json::from_slice(&version()).unwrap(), &case);
            }
        }
        let mut journal = Journal::default();
        let (outcome, calls) = run_outputs(
            BODY,
            responses
                .into_iter()
                .map(|(status, stdout)| {
                    Ok(ProcessOutput::new(
                        ProcessExit::Exited(status),
                        stdout,
                        b"unknown-label-canary".to_vec(),
                        Duration::from_millis(1),
                    ))
                })
                .collect(),
            true,
            &mut journal,
        );
        assert_eq!(
            process_count(&calls),
            case["processes"].as_u64().unwrap() as usize,
            "{name}"
        );
        if case["expect"] == "true" {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Completed(value) if value.value() == &Value::Bool(true)),
                "{name}: {outcome:?}"
            );
        } else {
            assert_eq!(case["expect"], "error");
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Error(_)),
                "{name}: {outcome:?}"
            );
        }
        for canary in [
            "credential-canary",
            "raw-metadata-canary",
            "unknown-label-canary",
        ] {
            assert!(
                !format!("{outcome:?} {:?}", journal.events).contains(canary),
                "{name}: raw response in outcome/evidence"
            );
        }
    }
}

#[test]
fn named_decoder_corpus_has_identical_pure_and_controlled_image_admission() {
    for case in docker_cases::cases() {
        let name = case["name"].as_str().unwrap();
        let input = docker_cases::payload(&case);
        let body = if case["expect"] == "image" {
            "let image = docker::inspect_image(targets::ci, 'example:test')\nlet saved = docker::decode_image(data::json_decode(input))\nreturn image == saved"
        } else {
            "let image = docker::inspect_image(targets::ci, 'example:test')\nreturn true"
        };
        let mut responses = outputs();
        responses[2].1 = input;
        let mut journal = Journal::default();
        let (passed, calls) = run(body, responses, true, &mut journal);
        assert_eq!(passed, case["expect"] == "image", "{name}");
        let count = if !case["hex"].is_null()
            || name == "document-depth-65"
            || name == "document-bytes-1048577"
        {
            3
        } else {
            4
        };
        assert_eq!(process_count(&calls), count, "{name}");
    }
}

#[test]
fn journal_failure_at_every_stage_prevents_further_children() {
    for stage in 0..4 {
        for before in [true, false] {
            let mut journal = Journal {
                fail_before: before,
                fail_after: !before,
                fault_at: stage,
                ..Default::default()
            };
            let (passed, calls) = run(BODY, outputs(), true, &mut journal);
            assert!(!passed, "stage {stage}, before {before}");
            assert_eq!(process_count(&calls), stage + usize::from(!before));
        }
    }
}

#[test]
fn output_caps_and_child_timeout_apply_at_every_stage() {
    for stage in 0..4 {
        for (stdout_extra, stderr_extra, elapsed) in [
            (true, false, Duration::from_millis(1)),
            (false, true, Duration::from_millis(1)),
            (false, false, Duration::from_secs(31)),
        ] {
            let mut responses = outputs()
                .into_iter()
                .map(|(status, stdout)| {
                    Ok(ProcessOutput::new(
                        ProcessExit::Exited(status),
                        stdout,
                        Vec::new(),
                        Duration::from_millis(1),
                    ))
                })
                .collect::<Vec<_>>();
            responses[stage] = Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                if stdout_extra {
                    vec![b'a'; 1_048_577]
                } else {
                    outputs()[stage].1.clone()
                },
                if stderr_extra {
                    vec![b'a'; 65_537]
                } else {
                    Vec::new()
                },
                elapsed,
            ));
            let (outcome, calls) = run_outputs(BODY, responses, true, &mut Journal::default());
            assert_eq!(process_count(&calls), stage + 1);
            if elapsed > Duration::from_secs(30) {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Cancelled(cancel) if cancel.reason() == opaal_runtime::eval::CancelReason::Timeout),
                    "{outcome:?}"
                );
            } else {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Error(_)),
                    "{outcome:?}"
                );
            }
        }
    }
}

#[test]
fn requested_cancellation_at_every_admission_stops_before_the_affected_child() {
    for stage in 0..4 {
        let mut journal = Journal {
            requested_at: Some(stage),
            ..Default::default()
        };
        let responses = outputs()
            .into_iter()
            .map(|(status, stdout)| {
                Ok(ProcessOutput::new(
                    ProcessExit::Exited(status),
                    stdout,
                    Vec::new(),
                    Duration::from_millis(1),
                ))
            })
            .collect();
        let (outcome, calls) = run_outputs(BODY, responses, true, &mut journal);
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(cancel) if cancel.reason() == opaal_runtime::eval::CancelReason::Requested),
            "stage {stage}: {outcome:?}"
        );
        assert_eq!(process_count(&calls), stage);
    }
}

#[test]
fn every_changed_safe_directory_stops_before_the_next_child() {
    for path in ["docker-empty", "empty-path", "sandbox-home", "sandbox-tmp"] {
        let mut journal = Journal {
            mutate_path: Some((path, true)),
            ..Default::default()
        };
        let (passed, calls) = run(BODY, outputs(), true, &mut journal);
        assert!(!passed, "{path}");
        assert_eq!(process_count(&calls), 1, "{path}");
    }
}

#[test]
fn hostile_manifest_and_lock_environment_are_rejected_before_acquisition() {
    for (name, text) in [
        (
            "api",
            MANIFEST.replace("api_version = \"1.48\"", "api_version = \"1.49\""),
        ),
        (
            "context",
            MANIFEST.replace(
                "api_version = \"1.48\"",
                "api_version = \"1.48\"\ncontext = 'account'",
            ),
        ),
        (
            "remote-socket",
            MANIFEST.replace("L2ZpeHR1cmUvZG9ja2VyLnNvY2s", "dGNwOi8vMTI3LjAuMC4xOjIzNzU"),
        ),
        (
            "absolute-config",
            MANIFEST.replace(
                "config_directory = \"docker-empty\"",
                "config_directory = '/account'",
            ),
        ),
        (
            "parent-config",
            MANIFEST.replace(
                "config_directory = \"docker-empty\"",
                "config_directory = '../account'",
            ),
        ),
        (
            "unknown-tool",
            MANIFEST.replace("tool = \"docker\"", "tool = 'other'"),
        ),
        (
            "mixed-tools",
            MANIFEST.to_owned() + "\n[tools.git]\nadapter='git'\nversion='=2.0.0'\n",
        ),
    ] {
        assert!(
            parse_project_manifest(Path::new("/fixture/opaal.toml"), text.as_bytes()).is_err(),
            "{name}"
        );
    }
    let manifest =
        parse_project_manifest(Path::new("/fixture/opaal.toml"), MANIFEST.as_bytes()).unwrap();
    let lock = format!(
        "schema_version=1\nproject='sample'\nenvironment='ci'\nplatform='x86_64-unknown-linux-gnu'\n[child_environment]\ninherit=[]\n[[tools]]\nid='docker'\nadapter='docker'\npath={{encoding='base64url-nopad',platform='unix',value='L3Rvb2wvZG9ja2Vy'}}\nversion='29.9.0'\ndigest='{}'\n",
        opaal_runtime::workflow::digest_bytes(b"test executable")
    );
    assert!(parse_tool_lock(&manifest, "ci", lock.as_bytes()).is_ok());
    assert!(
        parse_tool_lock(
            &manifest,
            "ci",
            lock.replace("inherit=[]", "inherit=['HOME']").as_bytes()
        )
        .is_err()
    );
    for name in [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_CONFIG",
        "DOCKER_API_VERSION",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "HOME",
        "PATH",
    ] {
        let hostile = lock.clone()
            + &format!(
                "\n[[child_environment.variables]]\nname='{name}'\nvalue={{encoding='base64url-nopad',platform='unix',value='Y2FuYXJ5'}}\n"
            );
        assert!(
            parse_tool_lock(&manifest, "ci", hostile.as_bytes()).is_err(),
            "{name}"
        );
    }
}

#[test]
fn foreign_stale_and_absent_target_carriers_do_no_process_work() {
    let manifest =
        parse_project_manifest(Path::new("/fixture/opaal.toml"), MANIFEST.as_bytes()).unwrap();
    let Value::NominalRecord(target) =
        opaal_runtime::session::Session::new("/fixture", Environment::new(), Default::default())
            .submit_with_value(
                "target.opaal",
                "import std::docker as docker\ndocker::TargetIdentity {}",
                &NoExecutables,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap()
            .1
    else {
        panic!("expected TargetIdentity")
    };
    for (name, path, digest, calls) in [
        (
            "ci",
            "/fixture/opaal.toml",
            manifest.id().manifest_digest(),
            4,
        ),
        (
            "ci",
            "/foreign/opaal.toml",
            manifest.id().manifest_digest(),
            0,
        ),
        ("ci", "/fixture/opaal.toml", "sha256:stale-manifest", 0),
        (
            "absent",
            "/fixture/opaal.toml",
            manifest.id().manifest_digest(),
            0,
        ),
    ] {
        let carrier = Value::NominalRecord(Box::new(NominalRecordValue::new(
            target.id().clone(),
            Vec::new(),
            vec![
                (Arc::from("_id"), Value::string(name)),
                (Arc::from("_manifest"), Value::string(digest)),
                (Arc::from("_path"), Value::Path(NativePath::new(path))),
            ],
        )));
        let (outcome, observed) = run_task(
            "let image = docker::inspect_image(input, 'example:test')\nreturn image.os == 'linux'",
            Some(("docker::TargetIdentity", carrier)),
            "Bool",
            outputs()
                .into_iter()
                .map(|(status, stdout)| {
                    Ok(ProcessOutput::new(
                        ProcessExit::Exited(status),
                        stdout,
                        Vec::new(),
                        Duration::from_millis(1),
                    ))
                })
                .collect(),
            true,
            &mut Journal::default(),
        );
        assert_eq!(process_count(&observed), calls, "{name}, {path}, {digest}");
        if name == "absent" {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Refused(_)),
                "{outcome:?}"
            );
        } else if calls == 0 {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Error(_)),
                "{outcome:?}"
            );
        } else {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Completed(value) if value.value() == &Value::Bool(true))
            );
        }
    }
    let (passed, calls) = run(
        "let alias = targets::ci\nlet nested = { target: alias }\nlet image = docker::inspect_image(nested.target, 'example:test')\nreturn image.os == 'linux'",
        outputs(),
        true,
        &mut Journal::default(),
    );
    assert!(passed);
    assert_eq!(process_count(&calls), 4);
}

#[test]
fn target_carriers_cannot_escape_actions_through_containers_or_captured_closures() {
    for (kind, expression) in [
        ("docker::TargetIdentity", "target"),
        ("List[docker::TargetIdentity]", "[target]"),
        ("Record", "{ nested: [{ target: target }] }"),
        ("Wrapper", "Wrapper { target: target }"),
        ("Record", "{ nested: [Wrapper { target: target }] }"),
        (
            "outcome::Option[docker::TargetIdentity]",
            "outcome::Option::Some(target)",
        ),
        ("Closure", "{|| target}"),
        ("Record", "{ callback: {|| target} }"),
        ("Closure", "{|| { nested: [Wrapper { target: target }] }}"),
        ("Closure", "{|| {|| target}}"),
    ] {
        let (outcome, calls) = run_task(
            &format!("let target = targets::ci\nlet escaped = {expression}\nreturn escaped"),
            None,
            kind,
            Vec::new(),
            true,
            &mut Journal::default(),
        );
        assert_eq!(process_count(&calls), 0, "{expression}");
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Error(_)),
            "{expression}: {outcome:?}"
        );
        let rendered = format!("{outcome:?}");
        assert!(
            rendered.contains("control-only value escaping an action"),
            "{rendered}"
        );
        assert!(!rendered.contains("_manifest"));
        assert!(!rendered.contains("_path"));
    }
}

#[test]
fn target_carriers_cannot_escape_actions_through_table_cells() {
    let Value::NominalRecord(target) =
        opaal_runtime::session::Session::new("/fixture", Environment::new(), Default::default())
            .submit_with_value(
                "target.opaal",
                "import std::docker as docker\ndocker::TargetIdentity {}",
                &NoExecutables,
                &FakePlatform::full(),
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap()
            .1
    else {
        panic!("expected TargetIdentity")
    };
    let target = Value::NominalRecord(target);
    for cell in [target.clone(), Value::list(vec![target])] {
        let table = Value::from(
            opaal_runtime::Table::new(vec!["target".into()], vec![vec![cell]]).unwrap(),
        );
        let (outcome, calls) = run_task(
            "return input",
            Some(("Table", table)),
            "Table",
            Vec::new(),
            true,
            &mut Journal::default(),
        );
        assert_eq!(process_count(&calls), 0);
        assert!(matches!(outcome.primary(), PrimaryOutcome::Error(_)));
        assert!(format!("{outcome:?}").contains("control-only value escaping an action"));
    }
}

#[test]
fn process_faults_at_every_child_stop_and_redact_adapter_diagnostics() {
    use opaal_platform::operational::{OperationalError, OperationalErrorKind};
    for stage in 0..4 {
        for kind in [
            OperationalErrorKind::Unsupported,
            OperationalErrorKind::Protocol,
            OperationalErrorKind::Io(std::io::ErrorKind::PermissionDenied),
        ] {
            let mut responses = outputs()
                .into_iter()
                .map(|(status, stdout)| {
                    Ok(ProcessOutput::new(
                        ProcessExit::Exited(status),
                        stdout,
                        Vec::new(),
                        Duration::from_millis(1),
                    ))
                })
                .collect::<Vec<_>>();
            responses[stage] = Err(OperationalError::new(kind, "credential-canary"));
            let mut journal = Journal::default();
            let (outcome, calls) = run_outputs(BODY, responses, true, &mut journal);
            assert_eq!(process_count(&calls), stage + 1);
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Error(_)),
                "{outcome:?}"
            );
            assert!(!format!("{outcome:?} {:?}", journal.events).contains("credential-canary"));
        }
    }
}
