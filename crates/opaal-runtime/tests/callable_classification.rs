#![forbid(unsafe_code)]

use opaal_runtime::eval::{Completion, EvalLimits, apply_callable, evaluate_in_environment};
use opaal_runtime::{Environment, ScopeStack, Value};
use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};

fn construct(text: &str) -> (SourceFile, Value) {
    let source = SourceFile::new(SourceId::new(42), "same.opaal", text);
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("complete source required");
    };
    let mut scope = ScopeStack::new();
    evaluate_in_environment(
        &script,
        &source,
        &mut scope,
        &mut Environment::new(),
        &EvalLimits::default(),
    )
    .unwrap();
    (source, scope.get("candidate").unwrap().clone())
}

#[test]
fn raw_default_types_preserve_syntactic_effect_refusal_on_every_call() {
    for (text, refuses) in [
        ("def candidate() -> Int { return 7 }", false),
        ("let candidate = {|| 7}", false),
        ("action candidate() -> Int effects {} { return 7 }", false),
        (
            "action candidate() -> Int effects { clock.wall; } { return 7 }",
            true,
        ),
        (
            "action candidate() -> Int effects { clock.wall; clock.wall; } { return 7 }",
            true,
        ),
    ] {
        let (_, value) = construct(text);
        let caller = SourceFile::new(SourceId::new(43), "caller.opaal", "candidate()");
        for _ in 0..8 {
            let result = apply_callable(
                &value,
                Vec::new(),
                &caller,
                caller.span(0..11).unwrap(),
                &mut Environment::new(),
                &EvalLimits::default(),
            );
            if refuses {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("effectful action execution")
                );
            } else {
                assert!(matches!(result.unwrap(), Completion::Value(Value::Int(7))));
            }
        }
    }
}

#[test]
fn retained_callables_keep_defining_bytes_when_source_identity_is_reused() {
    let (_, old) = construct("action candidate() -> Int effects { clock.wall; } { return 7 }");
    let (_, new) = construct("action candidate() -> Int effects {} { return 9 }");
    let caller = SourceFile::new(SourceId::new(42), "same.opaal", "candidate()");
    for _ in 0..3 {
        let apply = |value| {
            apply_callable(
                value,
                Vec::new(),
                &caller,
                caller.span(0..11).unwrap(),
                &mut Environment::new(),
                &EvalLimits::default(),
            )
        };
        assert!(
            apply(&old)
                .unwrap_err()
                .to_string()
                .contains("effectful action execution")
        );
        assert!(matches!(
            apply(&new).unwrap(),
            Completion::Value(Value::Int(9))
        ));
    }
}

#[test]
fn warmed_errors_retain_defining_source_and_call_frames() {
    let (defining, value) = construct("def candidate() -> Int { throw 'failure' }");
    let caller = SourceFile::new(SourceId::new(43), "caller.opaal", "candidate()");
    let invoke = || {
        apply_callable(
            &value,
            Vec::new(),
            &caller,
            caller.span(0..11).unwrap(),
            &mut Environment::new(),
            &EvalLimits::default(),
        )
        .unwrap_err()
    };
    let cold = invoke();
    assert_eq!(cold.source().unwrap(), &defining);
    assert!(!cold.frames().is_empty());
    for _ in 0..7 {
        assert_eq!(invoke(), cold);
    }
}
