#![forbid(unsafe_code)]
#[path = "support/random.rs"]
mod support;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use opaal_platform::operational::FakeOperationalAdapter;
use opaal_runtime::Environment;
use opaal_runtime::authority::{AuthorityVerdict, EffectSet};
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::CancellationToken;
use opaal_runtime::module::ActionId;
use opaal_runtime::operational::ModuleError;
use opaal_runtime::operational::source::{
    ControlledSourceOperations, SourceActionOutcome, SourceEffectEvent, SourceEffectJournal,
    SourceEffectOutcome, SourceEffectResult,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::project::{load_project_program, parse_project_manifest, parse_tool_lock};
use opaal_runtime::script::{ScriptExecutionOutcome, execute_project_task_outcome};
use support::{Harness, Script};

struct NoExecutables;
impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("no process effect")
    }
}

#[derive(Default)]
struct Journal {
    before_fail: bool,
    after_fail: bool,
    action_end_fail: bool,
    events: Vec<(SourceEffectEvent, SourceEffectOutcome)>,
    actions: Vec<SourceActionOutcome>,
    admission_clock: Option<Arc<opaal_runtime::eval::FakeClock>>,
}
impl SourceEffectJournal for Journal {
    fn action_start(&mut self, _: &str, _: &ActionId) -> Result<(), String> {
        Ok(())
    }
    fn action_end(&mut self, _: &str, outcome: &SourceActionOutcome) -> Result<(), String> {
        self.actions.push(outcome.clone());
        if self.action_end_fail {
            Err("injected action evidence failure".into())
        } else {
            Ok(())
        }
    }
    fn before(&mut self, _: &SourceEffectEvent, _: AuthorityVerdict) -> Result<(), String> {
        if let Some(clock) = &self.admission_clock {
            clock.advance(30_000_000_000);
        }
        if self.before_fail {
            Err("injected before failure".into())
        } else {
            Ok(())
        }
    }
    fn after(
        &mut self,
        event: &SourceEffectEvent,
        outcome: &SourceEffectOutcome,
    ) -> Result<(), String> {
        self.events.push((event.clone(), outcome.clone()));
        if self.after_fail {
            Err("injected after failure".into())
        } else {
            Ok(())
        }
    }
}

fn run(
    source: &str,
    h: Harness,
    journal: &mut Journal,
) -> (ScriptExecutionOutcome, Arc<Mutex<Script>>, Vec<ModuleError>) {
    let manifest = parse_project_manifest(
        Path::new("/random/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "sample"
root_module = "tasks.opaal"
required_opaal = ">=1.2.0,<2.0.0"
[paths]
root = "."
evidence = "result.json"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    )
    .unwrap();
    let sources = support::Source(format!(
        "import std::random as random\n{source}\ntask sample = sample\n"
    ));
    let project = load_project_program(manifest, &sources, &sources).unwrap();
    let tools = parse_tool_lock(
        project.manifest(),
        "ci",
        br#"
schema_version = 1
project = "sample"
environment = "ci"
platform = "aarch64-apple-darwin"
tools = []
[child_environment]
inherit = []
"#,
    )
    .unwrap();
    execute(
        &project,
        &tools,
        h,
        journal,
        &FakeOperationalAdapter::new(),
        &BTreeMap::new(),
    )
}

fn execute(
    project: &opaal_runtime::project::ProjectProgram,
    tools: &opaal_runtime::project::ToolLock,
    mut h: Harness,
    journal: &mut Journal,
    adapter: &FakeOperationalAdapter,
    files: &BTreeMap<String, std::fs::File>,
) -> (ScriptExecutionOutcome, Arc<Mutex<Script>>, Vec<ModuleError>) {
    let task = project.task("sample").unwrap();
    let nodes = project
        .modules()
        .sources()
        .entries()
        .flat_map(|entry| project.modules().actions().actions(entry.module()).iter())
        .map(|action| (action.id().clone(), action.id().qualified_name()))
        .collect();
    let tls = BTreeMap::new();
    let mut operations = ControlledSourceOperations::new(
        &mut h.context,
        &h.effects,
        project.manifest(),
        tools,
        files,
        &h.platform,
        adapter,
        &tls,
        journal,
        nodes,
        BTreeMap::new(),
        Default::default(),
        None,
    )
    .with_random(h.state);
    let outcome = execute_project_task_outcome(
        project,
        task,
        Vec::new(),
        Path::new("/random"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &h.platform,
        h.clock,
        CancellationToken::never(),
        &mut operations,
        &mut Vec::new(),
    );
    let cleanup = operations.finish_standard_host();
    (outcome, h.script, cleanup)
}

#[test]
fn nested_calls_share_one_host_and_settle_candidate_reservations() {
    let mut journal = Journal::default();
    let (outcome, script, cleanup) = run(
        r#"
action draw() -> Int effects { entropy.system(); } { return random::int(0, 3) }
action sibling() -> Int effects {} { return 987654321 }
action sample() -> Int effects { entropy.system(); } {
    let ignored = sibling()
    let first = draw()
    let second = draw()
    let empty = random::bytes(0)
    return first + second
}
"#,
        Harness::new(&[0, 5, 2], &[]),
        &mut journal,
    );
    assert!(
        matches!(outcome.primary(), PrimaryOutcome::Completed(value) if value.value() == &opaal_runtime::Value::Int(4))
    );
    assert!(cleanup.is_empty());
    let script = script.lock().unwrap();
    assert_eq!(script.fills, [8, 8, 8]);
    assert_eq!(script.closed, 1);
    for (index, (_, outcome)) in journal.events.iter().enumerate() {
        let Some(SourceEffectResult::Entropy(progress)) = outcome.result() else {
            panic!("entropy progress")
        };
        assert_eq!(progress.admitted_bytes, [16, 8, 0][index]);
        assert_eq!(progress.confirmed_bytes, progress.admitted_bytes);
        assert_eq!(progress.uncertain_bytes_upper_bound, 0);
        assert!(outcome.value_digest().is_none());
    }
    assert_eq!(journal.actions.len(), 4);
    assert!(
        journal
            .actions
            .iter()
            .all(|action| action.outcome().value_digest().is_none())
    );
}

#[test]
fn before_failure_does_no_host_work_and_after_failure_preserves_primary() {
    for before in [true, false] {
        let h = Harness::new(&[], &[]);
        h.script.lock().unwrap().fail = true;
        let mut journal = Journal {
            before_fail: before,
            after_fail: !before,
            action_end_fail: true,
            ..Journal::default()
        };
        let (outcome, script, _) = run(
            "action sample() -> Float effects { entropy.system(); } { return random::float() }",
            h,
            &mut journal,
        );
        if before {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::FatalHostFailure(failure) if failure.to_string().contains("JOURNAL005"))
            );
        } else {
            let PrimaryOutcome::Error(error) = outcome.primary() else {
                panic!("{outcome:?}")
            };
            assert!(
                error.render().contains("injected fill failure"),
                "{}",
                error.render()
            );
        }
        assert_eq!(script.lock().unwrap().fills.len(), usize::from(!before));
        if !before {
            assert!(!outcome.evidence().is_empty());
            let Some(SourceEffectResult::Entropy(progress)) = journal.events[0].1.result() else {
                panic!()
            };
            assert_eq!(progress.admitted_bytes, 8);
            assert_eq!(progress.uncertain_bytes_upper_bound, 8);
        }
    }
}

#[test]
fn admission_persistence_consumes_the_original_operation_deadline() {
    let h = Harness::new(&[1], &[]);
    let mut journal = Journal {
        admission_clock: Some(h.clock.clone()),
        ..Default::default()
    };
    let (outcome, script, _) = run(
        "action sample() -> Float effects { entropy.system(); } { return random::float() }",
        h,
        &mut journal,
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)));
    assert!(script.lock().unwrap().fills.is_empty());
    let Some(SourceEffectResult::Entropy(progress)) = journal.events[0].1.result() else {
        panic!("entropy progress")
    };
    assert_eq!(progress.admitted_bytes, 0);
}

#[test]
fn no_draw_calls_still_require_declared_grant_and_exact_host_binding() {
    for case in 0..3 {
        let mut h = Harness::new(&[], &[]);
        match case {
            0 => h.effects = EffectSet::default(),
            1 => {
                h.context = opaal_runtime::context::OperationalContext::new(
                    opaal_runtime::authority::AuthorityContext::empty(
                        h.context.authority().evaluation(),
                    ),
                    CancellationToken::never(),
                    h.clock.clone(),
                    None,
                )
            }
            _ => {
                h.state = opaal_runtime::operational::random::RandomState::new(
                    Some(Box::new(support::Host {
                        script: h.script.clone(),
                        evaluation: 72,
                    })),
                    Default::default(),
                )
                .unwrap()
            }
        }
        let (outcome, script, _) = run(
            "action sample() -> Int effects { entropy.system(); } { return random::int(7, 8) }",
            h,
            &mut Journal::default(),
        );
        assert!(!matches!(outcome.primary(), PrimaryOutcome::Completed(_)));
        assert!(script.lock().unwrap().fills.is_empty());
    }
}

#[test]
fn catches_do_not_restore_consumed_entropy_budget() {
    let mut h = Harness::new(&[], &[]);
    h.limits(opaal_runtime::operational::random::RandomLimits {
        max_host_bytes: 16,
        ..Default::default()
    });
    h.script.lock().unwrap().fail = true;
    let mut journal = Journal::default();
    let (outcome, script, _) = run(
        r#"
action sample() -> Float effects { entropy.system(); } {
    try { let ignored = random::float() } catch error {}
    try { let ignored = random::float() } catch error {}
    return random::float()
}
"#,
        h,
        &mut journal,
    );
    assert!(!matches!(outcome.primary(), PrimaryOutcome::Completed(_)));
    assert_eq!(script.lock().unwrap().fills, [8, 8]);
    assert_eq!(journal.events.len(), 2);
    assert!(journal.events.iter().all(|(_, outcome)| outcome.partial()));
}

#[test]
fn cumulative_limits_survive_calls_and_cleanup_stays_secondary_to_cancellation() {
    let mut h = Harness::new(&[1, 2], &[]);
    h.limits(opaal_runtime::operational::random::RandomLimits {
        max_host_bytes: 16,
        ..Default::default()
    });
    let mut journal = Journal::default();
    let (outcome, script, _) = run(
        r#"
action sample() -> Int effects { entropy.system(); } {
    let first = random::int(0, 4)
    let second = random::int(0, 4)
    return random::int(0, 4)
}

"#,
        h,
        &mut journal,
    );
    assert!(!matches!(outcome.primary(), PrimaryOutcome::Completed(_)));
    assert_eq!(script.lock().unwrap().fills, [8, 8]);
    assert_eq!(journal.events.len(), 2);

    let h = Harness::new(&[1], &[]);
    {
        let mut script = h.script.lock().unwrap();
        script.advance = Some(h.clock.clone());
        script.cleanup_fail = true;
    }
    let mut journal = Journal {
        after_fail: true,
        action_end_fail: true,
        ..Default::default()
    };
    let (outcome, script, cleanup) = run(
        "action sample() -> Float effects { entropy.system(); } { return random::float() }",
        h,
        &mut journal,
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)));
    assert_eq!(script.lock().unwrap().closed, 1);
    assert_eq!(cleanup.len(), 1);
    assert!(!outcome.evidence().is_empty());
}

#[test]
fn random_derived_process_arguments_and_failed_payloads_are_projected_before_hashing() {
    use opaal_platform::operational::{
        OperationalCall, OperationalError, OperationalErrorKind, ProcessExit, ProcessOutput,
    };
    use opaal_runtime::authority::{
        AuthorityContext, AuthorityRule, CapabilityRequest, RequiredEnforcement,
    };
    use opaal_runtime::operational::source::SourceOperation;

    for fail in [false, true] {
        let mut h = Harness::new(&[], &[]);
        let requests = [
            CapabilityRequest::entropy_system(),
            CapabilityRequest::process_run("git").unwrap(),
        ];
        h.effects = EffectSet::new(requests.clone());
        h.context = opaal_runtime::context::OperationalContext::new(
            AuthorityContext::new(
                h.context.authority().evaluation(),
                requests.into_iter().map(|request| {
                    let enforcement =
                        if request.effect() == opaal_platform::AuthorityEffect::EntropySystem {
                            RequiredEnforcement::Enforced
                        } else {
                            RequiredEnforcement::AcknowledgeUnenforced
                        };
                    AuthorityRule::grant(request, enforcement)
                }),
            )
            .unwrap(),
            CancellationToken::never(),
            h.clock.clone(),
            None,
        );
        let manifest = parse_project_manifest(
            Path::new("/random/opaal.toml"),
            br#"
schema_version = 1
[project]
name = "sample"
root_module = "tasks.opaal"
required_opaal = ">=1.2.0,<2.0.0"
[paths]
root = "."
evidence = "result.json"
[tools.git]
adapter = "git"
version = ">=2.50.0,<3.0.0"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
        )
        .unwrap();
        let source = support::Source(
            r#"import std::random as random
import std::data as data
import std::string as string
import std::process as process
import project::tools as tools
action draw() -> Int effects { entropy.system(); } { return random::int(987654321, 987654322) }
action sibling(argument: String) -> Bytes effects { process.run(tools::git); } {
    let result = process::run(tools::git, [argument])
    return result.stdout
}
action sample() -> Bytes effects { entropy.system(); process.run(tools::git); } {
    let value = draw()
    return sibling(string::decode_utf8(data::json_encode(value)))
}
task sample = sample
"#
            .to_owned(),
        );
        let project = load_project_program(manifest, &source, &source).unwrap();
        let path =
            std::env::temp_dir().join(format!("opaal-random-tool-{}-{fail}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let executable = b"retained test executable";
        (&file).write_all(executable).unwrap();
        let digest = opaal_runtime::workflow::digest_bytes(executable);
        let variables = ["HOME", "TMPDIR", "PATH", "CARGO_HOME", "RUSTC", "RUSTDOC", "LC_ALL", "TZ", "CARGO_NET_OFFLINE", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL"].into_iter().map(|name| format!("[[child_environment.variables]]\nname = \"{name}\"\nvalue = {{ encoding = \"base64url-nopad\", platform = \"unix\", value = \"eA\" }}\n")).collect::<String>();
        let tools = parse_tool_lock(
            project.manifest(),
            "ci",
            format!(
                r#"schema_version = 1
project = "sample"
environment = "ci"
platform = "fixture"
[child_environment]
inherit = []
{variables}
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvZ2l0" }}
version = "2.50.0"
digest = "{digest}"
"#
            )
            .as_bytes(),
        )
        .unwrap();
        let adapter = FakeOperationalAdapter::new();
        adapter.insert_file("/tool/git", executable.to_vec());
        adapter.push_process(if fail {
            Err(OperationalError::new(
                OperationalErrorKind::Io(std::io::ErrorKind::Other),
                "process-error-canary",
            ))
        } else {
            Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                b"process-output-canary".to_vec(),
                b"process-stderr-canary".to_vec(),
                std::time::Duration::from_millis(1),
            ))
        });
        let files = BTreeMap::from([("git".to_owned(), file)]);
        let mut journal = Journal::default();
        let (outcome, script, cleanup) =
            execute(&project, &tools, h, &mut journal, &adapter, &files);
        assert!(cleanup.is_empty());
        assert!(script.lock().unwrap().fills.is_empty());
        assert_eq!(
            matches!(outcome.primary(), PrimaryOutcome::Completed(_)),
            !fail,
            "{outcome:?}"
        );
        assert!(adapter.calls().iter().any(
            |call| matches!(call, OperationalCall::Process { argv, .. } if argv[1] == "987654321")
        ));
        let (event, outcome) = journal
            .events
            .iter()
            .find(|(event, _)| matches!(event.operation(), SourceOperation::Process { .. }))
            .unwrap();
        let SourceOperation::Process {
            argv_count,
            argv_digest,
            argv,
            ..
        } = event.operation()
        else {
            unreachable!()
        };
        assert_eq!(*argv_count, 2);
        assert!(argv_digest.is_none());
        assert!(argv.is_empty());
        assert!(matches!(
            outcome.result(),
            Some(SourceEffectResult::Process {
                evidence_digest: None,
                ..
            })
        ));
        for outcome in journal
            .events
            .iter()
            .map(|(_, outcome)| outcome)
            .chain(journal.actions.iter().map(SourceActionOutcome::outcome))
        {
            assert!(outcome.value_digest().is_none());
            assert_eq!(outcome.message(), "metadata-only outcome");
        }
    }
}

#[test]
fn simultaneous_entropy_cleanup_and_journal_failures_keep_fixed_secondary_evidence() {
    let h = Harness::new(&[], &[]);
    {
        let mut script = h.script.lock().unwrap();
        script.fail = true;
        script.cleanup_fail = true;
        script.error_message = Some("primary-secondary-canary");
    }
    let mut journal = Journal {
        after_fail: true,
        action_end_fail: true,
        ..Default::default()
    };
    let (outcome, script, cleanup) = run(
        "action sample() -> Float effects { entropy.system(); } { return random::float() }",
        h,
        &mut journal,
    );
    let PrimaryOutcome::Error(primary) = outcome.primary() else {
        panic!("{outcome:?}")
    };
    assert!(primary.render().contains("primary-secondary-canary"));
    assert_eq!(script.lock().unwrap().fills, [8]);
    assert_eq!(script.lock().unwrap().closed, 1);
    assert_eq!(
        cleanup,
        vec![ModuleError::invalid("OPERATION004", "standard host cleanup failed"); 2]
    );
    let details = outcome
        .evidence()
        .iter()
        .map(|evidence| {
            let opaal_runtime::outcome::OutcomeEvidence::PartialEffect(partial) = evidence else {
                panic!("{evidence:?}")
            };
            partial.detail()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        details,
        [
            "effect failed and its journal record failed",
            "metadata-only outcome",
            "action outcome journal record failed",
        ]
    );
    assert_eq!(journal.events.len(), 1);
    let Some(SourceEffectResult::Entropy(progress)) = journal.events[0].1.result() else {
        panic!("entropy progress")
    };
    assert_eq!(progress.admitted_bytes, 8);
    assert_eq!(progress.confirmed_bytes, 0);
    assert_eq!(progress.uncertain_bytes_upper_bound, 8);
    assert_eq!(journal.actions.len(), 1);
    assert!(!format!("{:?}", outcome.evidence()).contains("primary-secondary-canary"));
    assert!(!format!("{cleanup:?}").contains("primary-secondary-canary"));
    for outcome in journal
        .events
        .iter()
        .map(|(_, outcome)| outcome)
        .chain(journal.actions.iter().map(SourceActionOutcome::outcome))
    {
        assert_eq!(outcome.message(), "metadata-only outcome");
        assert!(outcome.value_digest().is_none());
    }
}
