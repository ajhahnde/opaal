#![forbid(unsafe_code)]
#[path = "support/random.rs"]
mod support;

use std::path::Path;
use std::sync::Arc;

use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EvaluationContextId, RequiredEnforcement,
};
use opaal_runtime::eval::{CancelReason, CancellationToken, FakeClock};
use opaal_runtime::operational::standard_source::StandardBinding;
use opaal_runtime::outcome::{OutcomeEvidence, PrimaryOutcome};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::script::{
    ScriptExecutionOutcome, execute_ambient_module_program_outcome_with_standard_host,
};
use opaal_runtime::session::{Session, SubmitOutcome};
use opaal_runtime::{HostEnvironmentLimits, NativeSessionSnapshot, Value};
use support::Harness;

struct NoExecutables;
impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
        panic!("these sources must not probe executables")
    }
}

fn snapshot() -> NativeSessionSnapshot {
    NativeSessionSnapshot::from_snapshot(
        "/random",
        std::iter::empty::<(&str, &str)>(),
        HostEnvironmentLimits::OPAAL,
    )
    .unwrap()
}

fn binding(harness: Harness) -> StandardBinding {
    StandardBinding::new(
        AuthorityContext::new(
            EvaluationContextId::new(71).unwrap(),
            [AuthorityRule::grant(
                CapabilityRequest::entropy_system(),
                RequiredEnforcement::Enforced,
            )],
        )
        .unwrap(),
        CancellationToken::never(),
        harness.clock,
        None,
        harness.state,
    )
}

fn run(body: &str, harness: Harness) -> ScriptExecutionOutcome {
    let program = support::load(body);
    let platform = harness.platform;
    let clock = harness.clock.clone();
    execute_ambient_module_program_outcome_with_standard_host(
        &program,
        &[],
        snapshot(),
        &opaal_runtime::builtin::standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &platform,
        clock,
        &mut Vec::new(),
        &mut binding(harness),
    )
}

#[test]
fn root_source_has_exact_samples_and_closes_before_return() {
    let harness = Harness::new(&[0, 5, u64::MAX], &[0, 1, 2]);
    let script = harness.script.clone();
    let outcome = run(
        "let shard = random::int(0, 3)\nlet fraction = random::float()\nlet bytes = random::bytes(3)\n[shard, fraction, bytes, random::bytes(0), random::int(7, 8)]",
        harness,
    );
    let PrimaryOutcome::Completed(completed) = outcome.primary() else {
        panic!("{outcome:?}")
    };
    assert_eq!(
        completed.value(),
        &Value::list(vec![
            Value::Int(2),
            Value::Float(opaal_runtime::FiniteFloat::new(1.0 - 2.0_f64.powi(-53)).unwrap()),
            Value::bytes(vec![0, 1, 2]),
            Value::bytes(vec![]),
            Value::Int(7)
        ])
    );
    assert!(outcome.evidence().is_empty());
    let script = script.lock().unwrap();
    assert_eq!(script.fills, [8, 8, 8, 3]);
    assert_eq!(script.closed, 1);
}

#[test]
fn imported_initializers_and_pure_bodies_cannot_acquire_the_root_binding() {
    for body in [
        "def pure() -> Int { return random::int(7, 8) }\npure()",
        "let callback = {|| random::bytes(0)}\ncallback()",
        "import std::list as list\nlist::map[Int, Int]([1], {|x| random::int(7, 8)})",
        "action pure() -> Int effects {} { return random::int(7, 8) }\npure()",
    ] {
        let harness = Harness::new(&[], &[]);
        let script = harness.script.clone();
        assert!(
            matches!(run(body, harness).primary(), PrimaryOutcome::Refused(_)),
            "{body}"
        );
        let script = script.lock().unwrap();
        assert!(script.fills.is_empty());
        assert_eq!(script.closed, 1);
    }
    // An imported initializer never receives the root's binding.
    struct Dependency;
    impl opaal_runtime::module::ModuleSourceLoader for Dependency {
        fn load(
            &self,
            module: &opaal_runtime::module::ModuleId,
        ) -> Result<Vec<u8>, opaal_runtime::module::ModuleSourceError> {
            Ok(if module.path().file_name().unwrap() == "main.opaal" {
                b"import './dependency.opaal' as dependency\n42\n".to_vec()
            } else {
                b"import std::random as random\nlet sample = random::bytes(0)\nexport { sample }\n"
                    .to_vec()
            })
        }
    }
    let harness = Harness::new(&[], &[]);
    let script = harness.script.clone();
    let source = support::Source(String::new());
    let program = opaal_runtime::module::ModuleProgramLoader::new(&source, &Dependency)
        .load(Path::new("/random/main.opaal"))
        .unwrap();
    let platform = harness.platform;
    let outcome = execute_ambient_module_program_outcome_with_standard_host(
        &program,
        &[],
        snapshot(),
        &opaal_runtime::builtin::standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &platform,
        Arc::new(FakeClock::new()),
        &mut Vec::new(),
        &mut binding(harness),
    );
    assert!(matches!(outcome.primary(), PrimaryOutcome::Refused(_)));
    assert!(script.lock().unwrap().fills.is_empty());
    assert_eq!(script.lock().unwrap().closed, 1);
}

#[test]
fn explicitly_declared_entropy_actions_use_the_bound_root() {
    let harness = Harness::new(&[], &[]);
    let script = harness.script.clone();
    let outcome = run(
        "action sample() -> Int effects { entropy.system(); } { return random::int(7, 8) }\nsample()",
        harness,
    );
    let PrimaryOutcome::Completed(completed) = outcome.primary() else {
        panic!("{outcome:?}")
    };
    assert_eq!(completed.value(), &Value::Int(7));
    assert!(script.lock().unwrap().fills.is_empty());
}

#[test]
fn each_retained_cell_owns_a_fresh_binding_without_ambient_inheritance() {
    let mut session = Session::from_ambient_snapshot(snapshot(), SessionOptions::default());
    let source = support::Source(String::new());
    let fraction = Value::Float(opaal_runtime::FiniteFloat::new(1.0 - 2.0_f64.powi(-53)).unwrap());
    for (body, words, tail, expected) in [
        ("import std::random as random", vec![], vec![], Value::Null),
        (
            "let shard = random::int(0, 3)",
            vec![0, 5],
            vec![],
            Value::Null,
        ),
        ("shard", vec![], vec![], Value::Int(2)),
        (
            "let fraction = random::float()",
            vec![u64::MAX],
            vec![],
            Value::Null,
        ),
        ("fraction", vec![], vec![], fraction),
        (
            "let bytes = random::bytes(3)",
            vec![],
            vec![0, 1, 2],
            Value::Null,
        ),
        ("bytes", vec![], vec![], Value::bytes(vec![0, 1, 2])),
        ("random::bytes(0)", vec![], vec![], Value::bytes(vec![])),
        ("random::int(7, 8)", vec![], vec![], Value::Int(7)),
    ] {
        let harness = Harness::new(&words, &tail);
        let script = harness.script.clone();
        let platform = harness.platform;
        let result = session
            .submit_with_source_loader_and_standard_host(
                "cell",
                body,
                &source,
                &source,
                &mut binding(harness),
                &NoExecutables,
                &platform,
                &FakeClock::new(),
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(result, (SubmitOutcome::Continued, expected));
        let script = script.lock().unwrap();
        let mut fills = vec![8; words.len()];
        if !tail.is_empty() {
            fills.push(tail.len());
        }
        assert_eq!(script.fills, fills);
        assert!(script.bytes.is_empty());
        assert_eq!(script.closed, 1);
    }
    let result = session
        .submit_with_source_loader(
            "cell",
            "random::bytes(0)",
            &source,
            &source,
            &NoExecutables,
            &opaal_platform::FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap();
    assert!(matches!(result.0, SubmitOutcome::Refused(_)));
}

#[test]
fn timeout_and_secondary_cleanup_preserve_the_primary_without_raw_diagnostics() {
    let harness = Harness::new(&[], &[]);
    let script = harness.script.clone();
    {
        let mut settings = script.lock().unwrap();
        settings.fail = true;
        settings.cleanup_fail = true;
        settings.error_message = Some("PRIVATE_CLEANUP_SENTINEL");
        settings.advance = Some(harness.clock.clone());
    }
    let outcome = run(
        "try { let ignored = random::bytes(2) } catch error { 0 }\n42",
        harness,
    );
    assert!(
        matches!(outcome.primary(), PrimaryOutcome::Cancelled(cancellation) if cancellation.reason() == CancelReason::Timeout)
    );
    assert!(outcome.evidence().iter().all(|item| matches!(item, OutcomeEvidence::CleanupFailure(error) if !error.render().contains("PRIVATE_CLEANUP_SENTINEL"))));
    assert_eq!(outcome.evidence().len(), 1);
    let script = script.lock().unwrap();
    assert_eq!(script.fills, [2]);
    assert_eq!(script.closed, 1);
}

#[test]
fn ambient_help_renders_each_entropy_signature_without_drawing() {
    let mut session = Session::from_ambient_snapshot(snapshot(), SessionOptions::default());
    let source = support::Source(String::new());
    for cell in [
        "import std::random as draw",
        "help draw::int",
        "help draw::float",
        "help draw::bytes",
    ] {
        let harness = Harness::new(&[], &[]);
        let script = harness.script.clone();
        let platform = harness.platform;
        let mut output = Vec::new();
        let result = session
            .submit_with_source_loader_and_standard_host(
                "cell",
                cell,
                &source,
                &source,
                &mut binding(harness),
                &NoExecutables,
                &platform,
                &FakeClock::new(),
                &mut output,
            )
            .unwrap();
        assert_eq!(result.0, SubmitOutcome::Continued);
        if cell.starts_with("help") {
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains(&format!(
                "std::random::{}",
                cell.strip_prefix("help draw::").unwrap()
            )));
            assert!(text.contains("effect: entropy.system (evaluation)"));
        }
        assert!(script.lock().unwrap().fills.is_empty());
        assert_eq!(script.lock().unwrap().closed, 1);
    }
}
