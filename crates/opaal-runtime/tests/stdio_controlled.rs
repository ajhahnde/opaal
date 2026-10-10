#![forbid(unsafe_code)]
#[path = "support/stdio.rs"]
mod support;
use opaal_platform::operational::FakeOperationalAdapter;
use opaal_runtime::authority::AuthorityVerdict;
use opaal_runtime::eval::CancellationToken;
use opaal_runtime::module::ActionId;
use opaal_runtime::operational::source::{
    ControlledSourceOperations, SourceActionOutcome, SourceEffectEvent, SourceEffectJournal,
    SourceEffectOutcome, SourceEffectResult, SourceOperation,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::project::{load_project_program, parse_project_manifest, parse_tool_lock};
use opaal_runtime::script::{ScriptExecutionOutcome, execute_project_task_outcome};
use std::collections::BTreeMap;
use std::path::Path;
use support::Harness;

#[derive(Default)]
struct Journal {
    before_fail: bool,
    after_fail: bool,
    events: Vec<(SourceEffectEvent, SourceEffectOutcome)>,
    actions: Vec<SourceActionOutcome>,
}
impl SourceEffectJournal for Journal {
    fn action_start(&mut self, _: &str, _: &ActionId) -> Result<(), String> {
        Ok(())
    }
    fn action_end(&mut self, _: &str, outcome: &SourceActionOutcome) -> Result<(), String> {
        self.actions.push(outcome.clone());
        Ok(())
    }
    fn before(&mut self, _: &SourceEffectEvent, verdict: AuthorityVerdict) -> Result<(), String> {
        assert_eq!(verdict, AuthorityVerdict::GrantedEnforced);
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
struct NoExecutables;
impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("no process effect")
    }
}

fn run(h: Harness, journal: &mut Journal) -> ScriptExecutionOutcome {
    let manifest = parse_project_manifest(
        Path::new("/stdio/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "stdio_sample"
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
    let source = support::Source("import std::io as io\nexport { run }\naction run() -> Null effects { stdin.read(); stdout.write(); stderr.write(); } { let input = io::read_stdin(4)\nio::write_stdout(input)\nreturn io::eprintln('processed') }\ntask sample = run".into());
    let project = load_project_program(manifest, &source, &source).unwrap();
    let tools = parse_tool_lock(
        project.manifest(),
        "ci",
        br#"
schema_version = 1
project = "stdio_sample"
environment = "ci"
platform = "aarch64-apple-darwin"
tools = []
[child_environment]
inherit = []
"#,
    )
    .unwrap();
    let nodes = project
        .modules()
        .sources()
        .entries()
        .flat_map(|entry| project.modules().actions().actions(entry.module()).iter())
        .map(|action| (action.id().clone(), action.id().qualified_name()))
        .collect();
    let mut context = h.context;
    let adapter = FakeOperationalAdapter::new();
    let files = BTreeMap::new();
    let tls = BTreeMap::new();
    let mut operations = ControlledSourceOperations::new(
        &mut context,
        &h.effects,
        project.manifest(),
        &tools,
        &files,
        &h.platform,
        &adapter,
        &tls,
        journal,
        nodes,
        BTreeMap::new(),
        Default::default(),
        None,
    )
    .with_standard_host(h.state);
    let outcome = execute_project_task_outcome(
        &project,
        project.task("sample").unwrap(),
        Vec::new(),
        Path::new("/stdio"),
        &mut opaal_runtime::Environment::new(),
        &opaal_runtime::builtin::standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &h.platform,
        h.clock,
        CancellationToken::never(),
        &mut operations,
        &mut Vec::new(),
    );
    assert!(operations.finish_standard_host().is_empty());
    outcome
}

#[test]
fn declared_stream_actions_have_only_metadata_progress_and_no_value_digest() {
    let h = Harness::new(b"\0\xff");
    let script = h.script.clone();
    let mut journal = Journal::default();
    assert!(matches!(
        run(h, &mut journal).primary(),
        PrimaryOutcome::Completed(_)
    ));
    assert_eq!(journal.events.len(), 3);
    for (event, outcome) in &journal.events {
        assert!(matches!(
            event.operation(),
            SourceOperation::StandardStream { .. }
        ));
        assert!(outcome.value_digest().is_none());
        let Some(SourceEffectResult::StandardStream(progress)) = outcome.result() else {
            panic!("missing stream progress")
        };
        assert_eq!(progress.uncertain_bytes_upper_bound, 0);
    }
    assert!(
        journal
            .actions
            .iter()
            .all(|action| action.outcome().value_digest().is_none())
    );
    let script = script.lock().unwrap();
    assert_eq!(script.stdout, b"\0\xff");
    assert_eq!(script.stderr, b"processed\n");
}

#[test]
fn before_evidence_failure_transfers_nothing_and_after_failure_preserves_consumption() {
    for after in [false, true] {
        let h = Harness::new(b"ab");
        let script = h.script.clone();
        let mut journal = Journal {
            before_fail: !after,
            after_fail: after,
            ..Journal::default()
        };
        assert!(matches!(
            run(h, &mut journal).primary(),
            PrimaryOutcome::FatalHostFailure(_)
        ));
        if after {
            let Some(SourceEffectResult::StandardStream(progress)) = journal.events[0].1.result()
            else {
                panic!("missing progress")
            };
            assert_eq!(progress.confirmed_bytes, 2);
            assert_eq!(progress.eof, Some(true));
            assert!(script.lock().unwrap().input.is_empty());
        } else {
            assert!(script.lock().unwrap().calls.is_empty());
        }
        assert!(script.lock().unwrap().stdout.is_empty());
    }
}
