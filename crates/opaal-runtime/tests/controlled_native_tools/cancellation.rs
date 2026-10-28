use super::*;
use std::collections::BTreeMap;

use opaal_runtime::authority::AuthorityVerdict;
use opaal_runtime::module::{
    ActionId, ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::operational::source::{
    ControlledSourceOperations, SourceActionOutcome, SourceEffectEvent, SourceEffectJournal,
    SourceEffectOutcome,
};
use opaal_runtime::outcome::{OutcomeEvidence, PrimaryOutcome};

struct Source<'a>(&'a str);
impl ModuleCanonicalizer for Source<'_> {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Source<'_> {
    fn load(&self, _: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(format!("import std::process as process\nimport project::tools as tools\naction check() -> Bool {{ effects {{ process.run(tools::checker), }} \n{}\n}}\ntask check = check\n", self.0).into_bytes())
    }
}
impl opaal_runtime::resolve::ExecutableProbe for Source<'_> {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("no ambient executable lookup")
    }
}

#[derive(Default)]
struct Journal {
    effects: Vec<SourceEffectOutcome>,
    actions: Vec<SourceActionOutcome>,
    fail: Option<&'static str>,
    cancel: Option<(&'static str, Arc<FakeClock>)>,
}
impl Journal {
    fn checkpoint(&self, phase: &str) -> Result<(), String> {
        if let Some((at, clock)) = &self.cancel
            && *at == phase
        {
            clock.advance(1);
        }
        if self.fail == Some(phase) {
            Err(format!("injected {phase} failure"))
        } else {
            Ok(())
        }
    }
}
impl SourceEffectJournal for Journal {
    fn action_start(&mut self, _: &str, _: &ActionId) -> Result<(), String> {
        self.checkpoint("action-start")
    }
    fn action_end(&mut self, _: &str, outcome: &SourceActionOutcome) -> Result<(), String> {
        self.actions.push(outcome.clone());
        self.checkpoint("action-end")
    }
    fn before(&mut self, _: &SourceEffectEvent, verdict: AuthorityVerdict) -> Result<(), String> {
        assert_eq!(verdict, AuthorityVerdict::GrantedUnenforced);
        self.checkpoint("effect-before")
    }
    fn after(
        &mut self,
        _: &SourceEffectEvent,
        outcome: &SourceEffectOutcome,
    ) -> Result<(), String> {
        self.effects.push(outcome.clone());
        self.checkpoint("effect-after")
    }
}

fn run_source(
    fixture: &mut Fixture,
    token: CancellationToken,
    body: &str,
    journal: &mut Journal,
) -> opaal_runtime::script::ScriptExecutionOutcome {
    let source = Source(body);
    let manifest =
        parse_project_manifest(Path::new("/project/opaal.toml"), NATIVE_MANIFEST).unwrap();
    let project = opaal_runtime::project::load_project_program(manifest, &source, &source).unwrap();
    let nodes = project
        .modules()
        .sources()
        .entries()
        .flat_map(|entry| project.modules().actions().actions(entry.module()).iter())
        .map(|action| (action.id().clone(), action.id().qualified_name()))
        .collect();
    let files = BTreeMap::from([("checker".to_owned(), fixture.source.try_clone().unwrap())]);
    let tls = BTreeMap::new();
    let mut operations = ControlledSourceOperations::new(
        &mut fixture.context,
        &fixture.effects,
        project.manifest(),
        &fixture.lock,
        &files,
        &fixture.platform,
        &fixture.adapter,
        &tls,
        journal,
        nodes,
        BTreeMap::new(),
        process::ProcessBudget::default(),
        None,
    )
    .with_metadata_only_evidence();
    opaal_runtime::script::execute_project_task_outcome(
        &project,
        project.task("check").unwrap(),
        Vec::new(),
        Path::new("/project"),
        &mut opaal_runtime::Environment::new(),
        &opaal_runtime::builtin::standard_registry(),
        &source,
        &opaal_runtime::plan::SessionOptions::default(),
        &fixture.platform,
        fixture.adapter.clock.clone(),
        token,
        &mut operations,
        &mut Vec::new(),
    )
}

#[test]
fn source_cancellation_preserves_partial_only_after_a_process_attempt() {
    for launched in [false, true] {
        let cancelled = Arc::new(AtomicBool::new(!launched));
        let predicate = cancelled.clone();
        let token = CancellationToken::from_fn(move || predicate.load(Ordering::SeqCst));
        let original = image();
        let mut fixture = Fixture::with_token(&original, &original, token.clone());
        fixture.adapter.cancel_on_launch = Some(cancelled);
        fixture
            .adapter
            .processes
            .push_process(Err(OperationalError::new(
                OperationalErrorKind::Cancelled,
                "cancelled after launch",
            )));
        let mut journal = Journal::default();
        let outcome = run_source(
            &mut fixture,
            token,
            "let result = process::run(tools::checker, [])\nreturn true",
            &mut journal,
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(cause) if cause.reason() == CancelReason::Requested)
        );
        assert_eq!(
            fixture.adapter.launches.load(Ordering::SeqCst),
            usize::from(launched)
        );
        assert_eq!(journal.effects.len(), usize::from(launched));
        if launched {
            assert!(journal.effects[0].partial());
        }
        assert_eq!(
            outcome
                .evidence()
                .iter()
                .any(|item| matches!(item, OutcomeEvidence::PartialEffect(_))),
            launched
        );
    }
}

#[test]
fn source_cancellation_and_journal_faults_preserve_first_evaluator_outcome() {
    use opaal_runtime::eval::{Clock, Instant};
    use opaal_runtime::lifetime::CleanupStatus;

    for phase in [
        "pre-action",
        "action-start",
        "effect-before",
        "acquisition",
        "launch",
        "effect-after",
    ] {
        for reason in [CancelReason::Requested, CancelReason::Timeout] {
            for fail in [None, Some("effect-after"), Some("action-end")] {
                let clock = Arc::new(FakeClock::new());
                let token = if reason == CancelReason::Requested {
                    let clock = clock.clone();
                    CancellationToken::from_fn(move || clock.now() >= Instant::from_nanos(1))
                } else {
                    CancellationToken::deadline((*clock).clone(), Instant::from_nanos(1))
                };
                let original = image();
                let mut fixture =
                    Fixture::with_clock(&original, &original, token.clone(), clock.clone());
                fixture.adapter.acquisition_nanos = u64::from(phase == "acquisition");
                fixture.adapter.execution_nanos = u64::from(phase == "launch");
                fixture
                    .adapter
                    .processes
                    .push_process(Ok(ProcessOutput::new(
                        ProcessExit::Exited(0),
                        Vec::new(),
                        Vec::new(),
                        Duration::ZERO,
                    )));
                let cleanup_calls = Arc::new(AtomicUsize::new(0));
                let calls = cleanup_calls.clone();
                fixture
                    .context
                    .register_resource(move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err("injected cleanup failure".to_owned())
                    })
                    .unwrap();
                let mut journal = Journal {
                    fail,
                    cancel: Some((phase, clock.clone())),
                    ..Journal::default()
                };
                if phase == "pre-action" {
                    clock.advance(1);
                }
                let outcome = run_source(
                    &mut fixture,
                    token,
                    "try { let result = process::run(tools::checker, []) } catch error { return true }\nreturn false",
                    &mut journal,
                );
                let case = format!("{phase}/{reason:?}/{fail:?}");
                // A successful effect's failed journal aborts before the next cancellation poll.
                if phase == "effect-after" && fail == Some("effect-after") {
                    assert!(
                        matches!(outcome.primary(), PrimaryOutcome::FatalHostFailure(_)),
                        "{case}: {outcome:?}"
                    );
                } else {
                    assert!(
                        matches!(outcome.primary(), PrimaryOutcome::Cancelled(cause) if cause.reason() == reason),
                        "{case}: {outcome:?}"
                    );
                }
                let launched = matches!(phase, "launch" | "effect-after");
                let acquired = matches!(phase, "acquisition" | "launch" | "effect-after");
                assert_eq!(
                    fixture.adapter.snapshots.load(Ordering::SeqCst),
                    usize::from(acquired),
                    "{case}"
                );
                assert_eq!(
                    fixture.adapter.launches.load(Ordering::SeqCst),
                    usize::from(launched),
                    "{case}"
                );
                if let Some(effect) = journal.effects.first() {
                    assert_eq!(
                        effect.class(),
                        if phase == "effect-after" {
                            "success"
                        } else {
                            "cancelled"
                        },
                        "{case}"
                    );
                    assert_eq!(effect.partial(), phase == "launch", "{case}");
                }
                assert!(journal.effects.len() <= 1, "{case}");
                if launched {
                    assert!(!outcome.evidence().is_empty(), "{case}");
                }
                let primary = outcome.primary().clone();
                let (selected, evidence, _) = outcome.into_parts();
                let context = std::mem::replace(
                    &mut fixture.context,
                    OperationalContext::empty(EvaluationContextId::new(72).unwrap(), clock),
                );
                let finished = context.finish(selected, evidence);
                assert_eq!(finished.primary(), &primary, "{case}");
                assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1, "{case}");
                assert!(
                    matches!(
                        finished.downstream().cleanup()[0].status(),
                        CleanupStatus::Failed(_)
                    ),
                    "{case}"
                );
                println!(
                    "OPAAL_EVALUATOR_CASE {}",
                    serde_json::json!({
                        "phase": phase, "reason": format!("{reason:?}"), "journal_fault": fail,
                        "primary": if matches!(finished.primary(), PrimaryOutcome::Cancelled(_)) { "cancelled" } else { "host-failure" },
                        "snapshots": usize::from(acquired), "launches": usize::from(launched),
                        "cleanup_calls": cleanup_calls.load(Ordering::SeqCst),
                    })
                );
            }
        }
    }
}

#[test]
fn source_journal_failure_bypasses_catch_and_does_not_replace_language_error() {
    for body in [
        "try { let result = process::run(tools::checker, []) } catch error { return true }\nreturn false",
        "let result = process::run(tools::checker, [])\nthrow 'primary-error-canary'",
    ] {
        for fail in [
            None,
            Some("action-start"),
            Some("effect-before"),
            Some("effect-after"),
            Some("action-end"),
        ] {
            let original = image();
            let mut fixture = Fixture::new(&original, &original);
            fixture
                .adapter
                .processes
                .push_process(Ok(ProcessOutput::new(
                    ProcessExit::Exited(7),
                    Vec::new(),
                    Vec::new(),
                    Duration::ZERO,
                )));
            let mut journal = Journal {
                fail,
                ..Journal::default()
            };
            let outcome = run_source(&mut fixture, CancellationToken::never(), body, &mut journal);
            let throws = body.contains("throw");
            let case = format!("throws={throws}/{fail:?}");
            if throws && matches!(fail, None | Some("action-end")) {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Error(error) if error.render().contains("primary-error-canary")),
                    "{case}: {outcome:?}"
                );
            } else if fail.is_some() {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::FatalHostFailure(_)),
                    "{case}: {outcome:?}"
                );
            } else {
                assert!(
                    matches!(outcome.primary(), PrimaryOutcome::Completed(completion) if completion.value() == &opaal_runtime::Value::Bool(false)),
                    "{case}: {outcome:?}"
                );
            }
            let launched = !matches!(fail, Some("action-start" | "effect-before"));
            assert_eq!(
                fixture.adapter.launches.load(Ordering::SeqCst),
                usize::from(launched),
                "{case}"
            );
            if launched {
                assert_eq!(journal.effects[0].status(), Some(7), "{case}");
                assert!(!outcome.evidence().is_empty(), "{case}");
            }
            println!(
                "OPAAL_EVALUATOR_CASE {}",
                serde_json::json!({
                    "body": if throws { "throw" } else { "catch" }, "journal_fault": fail,
                    "primary": match outcome.primary() {
                        PrimaryOutcome::Completed(_) => "completed-false",
                        PrimaryOutcome::Error(_) => "error",
                        PrimaryOutcome::FatalHostFailure(_) => "host-failure",
                        _ => unreachable!(),
                    },
                    "launches": usize::from(launched),
                })
            );
        }
    }
}

#[test]
fn source_native_calls_share_attempt_budget_and_keep_nonzero_status_as_data() {
    for count in [8, 9] {
        let original = image();
        let mut fixture = Fixture::new(&original, &original);
        for code in 0..8 {
            fixture
                .adapter
                .processes
                .push_process(Ok(ProcessOutput::new(
                    ProcessExit::Exited(code),
                    Vec::new(),
                    Vec::new(),
                    Duration::ZERO,
                )));
        }
        let body = "process::run(tools::checker, [])\n".repeat(count) + "return true";
        let mut journal = Journal::default();
        let outcome = run_source(
            &mut fixture,
            CancellationToken::never(),
            &body,
            &mut journal,
        );
        if count == 8 {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Completed(completion) if completion.value() == &opaal_runtime::Value::Bool(true))
            );
        } else {
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Error(error) if error.render().contains("PROCESS001"))
            );
            assert_eq!(journal.effects[8].class(), "error");
            assert!(!journal.effects[8].partial());
        }
        assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 8);
        assert_eq!(fixture.adapter.launches.load(Ordering::SeqCst), 8);
        assert_eq!(journal.effects.len(), count);
        for (code, effect) in journal.effects.iter().take(8).enumerate() {
            assert_eq!(effect.class(), "success");
            assert_eq!(effect.status(), Some(code as i64));
            assert!(effect.value_digest().is_none());
        }
        assert_eq!(outcome.evidence().len(), 8);
        assert!(
            outcome
                .evidence()
                .iter()
                .all(|item| matches!(item, OutcomeEvidence::Completed(_)))
        );
        println!(
            "OPAAL_EVALUATOR_CASE {}",
            serde_json::json!({
                "calls": count, "primary": if count == 8 { "completed-true" } else { "error" },
                "snapshots": 8, "launches": 8, "implicit_probes": 0,
            })
        );
    }
}
