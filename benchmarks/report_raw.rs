use opaal_runtime::eval::{
    CancellationToken, Completion, EvalLimits, ResourceBudget, apply_callable,
    evaluate_closure_argument, evaluate_in_environment,
};
use opaal_runtime::{Environment, ScopeStack, Value};
use opaal_syntax::{
    ExpressionKind, ParseOutcome, SourceFile, SourceId, StatementKind, parse_opaal,
};
use std::hint::black_box;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn parsed(source: &SourceFile) -> opaal_syntax::Script {
    let ParseOutcome::Complete(script) = parse_opaal(source) else {
        panic!("invalid fixture")
    };
    script
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("differential");
    if mode == "construct" {
        let count: usize = args[2].parse().unwrap();
        let padding: usize = args[3].parse().unwrap();
        let repeats: usize = args[4].parse().unwrap();
        let mut text = String::new();
        for i in 0..count {
            text += &format!("def f{i}() -> Int {{ return 7 }}\n");
        }
        if padding > 0 {
            text += &format!("#{}\n", "x".repeat(padding));
        }
        let source = SourceFile::new(SourceId::new(1), "raw.opaal", text);
        let script = parsed(&source);
        // Parsing fixture/admission is identical; each evaluation retains all unused callables.
        for _ in 0..repeats {
            let mut scope = ScopeStack::new();
            let result = evaluate_in_environment(
                &script,
                &source,
                &mut scope,
                &mut Environment::new(),
                &EvalLimits::default(),
            )
            .unwrap();
            assert!(matches!(result, Completion::Value(Value::Null)));
            assert!(scope.get(&format!("f{}", count - 1)).is_some());
            black_box(scope);
        }
        println!("constructed={count} padding={padding} repeats={repeats}");
    } else if mode == "call" {
        let warm: bool = args[2] == "warm";
        let padding: usize = args[3].parse().unwrap();
        let repeats: usize = args[4].parse().unwrap();
        let source = SourceFile::new(
            SourceId::new(1),
            "raw.opaal",
            format!(
                "def candidate() -> Int {{ return 7 }}\ncandidate\n#{}\n",
                "x".repeat(padding)
            ),
        );
        let script = parsed(&source);
        let create = || {
            let Completion::Value(value) = evaluate_in_environment(
                &script,
                &source,
                &mut ScopeStack::new(),
                &mut Environment::new(),
                &EvalLimits::default(),
            )
            .unwrap() else {
                panic!()
            };
            value
        };
        let apply = |value: &Value| {
            assert!(matches!(
                apply_callable(
                    value,
                    vec![],
                    &source,
                    script.span(),
                    &mut Environment::new(),
                    &EvalLimits::default()
                )
                .unwrap(),
                Completion::Value(Value::Int(7))
            ));
        };
        if warm {
            let value = create();
            apply(&value);
            for _ in 0..repeats {
                apply(black_box(&value));
            }
        } else {
            for _ in 0..repeats {
                let value = create();
                apply(black_box(&value));
            }
        }
        println!(
            "call {} padding={padding} repeats={repeats}",
            if warm { "warm" } else { "cold" }
        );
    } else if mode == "differential-first-use" {
        for stage in ["cold", "warm"] {
            for (label, text) in [
                (
                    "scalar",
                    "def candidate(value: Int) -> Int { if value < 0 { throw 'negative' }\nreturn value + 1 }\ncandidate",
                ),
                (
                    "collection",
                    "def candidate(value: Int) -> List[Int] { return [value, value] }\ncandidate",
                ),
            ] {
                let source = SourceFile::new(SourceId::new(70), "exact.opaal", text);
                let script = parsed(&source);
                let create = || {
                    let Completion::Value(value) = evaluate_in_environment(
                        &script,
                        &source,
                        &mut ScopeStack::new(),
                        &mut Environment::new(),
                        &EvalLimits::default(),
                    )
                    .unwrap() else {
                        panic!()
                    };
                    value
                };
                for after in 0..32 {
                    let value = create();
                    if stage == "warm" {
                        let _ = apply_callable(
                            &value,
                            vec![Value::Int(7)],
                            &source,
                            script.span(),
                            &mut Environment::new(),
                            &EvalLimits::default(),
                        );
                    }
                    let checks = Arc::new(AtomicUsize::new(0));
                    let inner = Arc::clone(&checks);
                    let limits = EvalLimits::new(
                        CancellationToken::from_fn(move || {
                            inner.fetch_add(1, Ordering::SeqCst) >= after
                        }),
                        ResourceBudget::steps(100).with_call_depth(5),
                    );
                    let result = apply_callable(
                        &value,
                        vec![Value::Int(7)],
                        &source,
                        script.span(),
                        &mut Environment::new(),
                        &limits,
                    );
                    println!(
                        "{stage} {label} cancel={after} polls={} result={result:?}",
                        checks.load(Ordering::SeqCst)
                    );
                }
                for steps in 0..20 {
                    for depth in 0..3 {
                        for (items, bytes) in [(0, 0), (1, 8), (2, 15), (2, 16), (3, 17)] {
                            let value = create();
                            if stage == "warm" {
                                let _ = apply_callable(
                                    &value,
                                    vec![Value::Int(7)],
                                    &source,
                                    script.span(),
                                    &mut Environment::new(),
                                    &EvalLimits::default(),
                                );
                            }
                            let limits = EvalLimits::new(
                                CancellationToken::never(),
                                ResourceBudget::steps(steps)
                                    .with_call_depth(depth)
                                    .with_collection_items(items)
                                    .with_collection_bytes(bytes),
                            );
                            let result = apply_callable(
                                &value,
                                vec![Value::Int(7)],
                                &source,
                                script.span(),
                                &mut Environment::new(),
                                &limits,
                            );
                            println!(
                                "{stage} {label} steps={steps} depth={depth} items={items} bytes={bytes} result={result:?}"
                            );
                        }
                    }
                }
            }
        }
    } else if mode == "closure" {
        let padding: usize = args[2].parse().unwrap();
        let repeats: usize = args[3].parse().unwrap();
        let source = SourceFile::new(
            SourceId::new(1),
            "raw.opaal",
            format!("let callback = {{|x| x}}\n#{}\n", "x".repeat(padding)),
        );
        let script = parsed(&source);
        let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
            panic!("declaration")
        };
        let ExpressionKind::Closure(closure) = declaration.value.kind() else {
            panic!("closure")
        };
        let scope = ScopeStack::new();
        for _ in 0..repeats {
            assert!(matches!(
                black_box(evaluate_closure_argument(closure, &source, &scope).unwrap()),
                Value::Callable(_)
            ));
        }
        println!("closure padding={padding} repeats={repeats}");
    } else {
        let text = "def candidate(value: Int) -> Int { if value < 0 { throw 'negative' }\nreturn value + 1 }\ncandidate";
        let source = SourceFile::new(SourceId::new(7), "raw.opaal", text);
        let script = parsed(&source);
        let Completion::Value(callable) = evaluate_in_environment(
            &script,
            &source,
            &mut ScopeStack::new(),
            &mut Environment::new(),
            &EvalLimits::default(),
        )
        .unwrap() else {
            panic!("callable")
        };
        for after in 0..32 {
            let checks = Arc::new(AtomicUsize::new(0));
            let inner = Arc::clone(&checks);
            let cancel =
                CancellationToken::from_fn(move || inner.fetch_add(1, Ordering::SeqCst) >= after);
            let result = apply_callable(
                &callable,
                vec![Value::Int(7)],
                &source,
                source.span(0..3).unwrap(),
                &mut Environment::new(),
                &EvalLimits::new(cancel, ResourceBudget::steps(100).with_call_depth(5)),
            );
            println!(
                "cancel after={after} polls={} result={result:?}",
                checks.load(Ordering::SeqCst)
            );
        }
        for steps in 0..24 {
            for depth in 0..3 {
                for value in [
                    Value::Int(7),
                    Value::Int(-1),
                    Value::String(Arc::from("wrong")),
                ] {
                    let result = apply_callable(
                        &callable,
                        vec![value.clone()],
                        &source,
                        source.span(0..3).unwrap(),
                        &mut Environment::new(),
                        &EvalLimits::new(
                            CancellationToken::never(),
                            ResourceBudget::steps(steps).with_call_depth(depth),
                        ),
                    );
                    println!("steps={steps} depth={depth} arg={value:?} result={result:?}");
                }
            }
        }
        // Already-cancelled raw construction must retain its previous poll boundary.
        for after in 0..8 {
            let checks = Arc::new(AtomicUsize::new(0));
            let inner = Arc::clone(&checks);
            let source = SourceFile::new(
                SourceId::new(8),
                "construct.opaal",
                "let f = {|| 7}\nlet g = {|| 8}",
            );
            let script = parsed(&source);
            let result = evaluate_in_environment(
                &script,
                &source,
                &mut ScopeStack::new(),
                &mut Environment::new(),
                &EvalLimits::new(
                    CancellationToken::from_fn(move || {
                        inner.fetch_add(1, Ordering::SeqCst) >= after
                    }),
                    ResourceBudget::steps(100),
                ),
            );
            println!(
                "construction after={after} polls={} result={result:?}",
                checks.load(Ordering::SeqCst)
            );
        }
    }
}
