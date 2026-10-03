#![forbid(unsafe_code)]
#[path = "support/numeric.rs"]
mod support;
use opaal_runtime::Value;
use opaal_runtime::eval::{CancellationToken, ResourceBudget};
use opaal_runtime::outcome::PrimaryOutcome;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::{completed, load, run, session_run, value};

#[test]
fn scalar_calls_admit_exact_shared_steps_without_retaining_collections() {
    for body in [
        "math::abs(-3)",
        "math::min(1.0, 2.0)",
        "math::max(1, 2)",
        "math::clamp(math::round(92.5), 0.0, 100.0)",
        "math::floor(2.7)",
        "math::ceil(2.1)",
        "math::sqrt(81.0)",
    ] {
        let program = load(body);
        let expected = completed(&run(
            &program,
            ResourceBudget::default(),
            CancellationToken::never(),
        ));
        let mut boundary = None;
        for limit in 0..128 {
            let budget = ResourceBudget::steps(limit)
                .with_collection_items(0)
                .with_collection_bytes(0);
            let outcome = run(&program, budget, CancellationToken::never());
            match outcome.primary() {
                PrimaryOutcome::Completed(completion) => {
                    assert_eq!(completion.value(), &expected);
                    boundary = Some(limit);
                    break;
                }
                PrimaryOutcome::Error(error) => {
                    assert!(error.to_string().contains("resource budget"), "{outcome:?}")
                }
                other => panic!("{other:?}"),
            }
        }
        let boundary = boundary.expect("bounded scalar work");
        assert!(boundary > 0);
        assert_eq!(
            completed(&run(
                &program,
                ResourceBudget::steps(boundary + 1),
                CancellationToken::never()
            )),
            expected
        );
    }
}

#[test]
fn cancellation_at_every_poll_remains_primary_through_catch() {
    let program = load("try { math::clamp(math::round(92.5), 0.0, 100.0) } catch error { 0.0 }");
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = polls.clone();
    completed(&run(
        &program,
        ResourceBudget::default(),
        CancellationToken::from_fn(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            false
        }),
    ));
    let count = polls.load(Ordering::SeqCst);
    assert!(count > 0);
    for stop in 0..count {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let outcome = run(
            &program,
            ResourceBudget::default(),
            CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= stop),
        );
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Cancelled(_)),
            "poll {stop}: {outcome:?}"
        );
    }
}

#[test]
fn domain_errors_keep_source_and_stack_and_can_be_rethrown() {
    for body in [
        "math::sqrt(-1.0)",
        "math::abs((-9223372036854775807 - 1))",
        "math::clamp(1, 2, 0)",
    ] {
        let source = format!(
            "def fail() {{ try {{ return {body} }} catch error {{ throw error }} }}\nfail()"
        );
        let program = load(&source);
        let outcome = run(
            &program,
            ResourceBudget::default(),
            CancellationToken::never(),
        );
        let PrimaryOutcome::Error(script_error) = outcome.primary() else {
            panic!("{outcome:?}")
        };
        assert!(script_error.render().contains("fail"));
        let opaal_runtime::session::SubmitError::Runtime { error, .. } =
            session_run(&source, Value::Null).unwrap_err()
        else {
            panic!("runtime operation error")
        };
        assert_eq!(
            error.category(),
            opaal_runtime::eval::ErrorCategory::Operation,
            "{body}: {error:?}"
        );
        assert!(error.span().start() > 0);
        assert!(!error.frames().is_empty());
        assert_eq!(
            value(&format!("try {{ {body} }} catch error {{ 0 }}")),
            Value::Int(0)
        );
    }
    assert!(
        session_run("math::sqrt(1.0 / 0.0)", Value::Null)
            .unwrap_err()
            .render()
            .contains("zero")
    );
}
