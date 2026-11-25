use super::super::*;
use opaal_syntax::{ParseOutcome, SourceId, parse_opaal};
use std::sync::atomic::{AtomicUsize, Ordering};

fn evaluate_slice(
    text: &str,
    scope: &mut ScopeStack,
    budget: &mut ResourceBudget,
    cancel: CancellationToken,
    retain_type: bool,
) -> Eval<Value> {
    let source = Arc::new(SourceFile::new(SourceId::new(713), "slice.opaal", text));
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("{text}");
    };
    let StatementKind::Declaration(binding) = script.statements()[0].kind() else {
        panic!("expected declaration");
    };
    let mut environment = Environment::new();
    let mut host = PureEvaluationHost {
        environment: &mut environment,
        policy: EvaluationPolicy::PureOpaal,
    };
    let mut evaluator = Evaluator {
        source,
        binding_types: Arc::new(RuntimeBindingTypes::default()),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects: None,
        cancel,
        budget,
        host: &mut host,
    };
    if retain_type {
        evaluator
            .operation_argument(&binding.value, scope, None)
            .map(|(value, _)| value)
    } else {
        evaluator.declaration(binding, scope)?;
        Ok(scope.get("selected").unwrap().clone())
    }
}

fn scope(input: &Value) -> ScopeStack {
    let mut scope = ScopeStack::new();
    scope
        .declare("input", BindingMutability::Immutable, input)
        .unwrap();
    scope
}

#[test]
fn fixed_windows_meter_only_the_selected_list_or_required_string_prefix() {
    let list = |len| Value::list((0..len).map(Value::Int).collect());
    let string = |suffix: &str| Value::string(format!("é🙂\0{suffix}"));
    for (short, long, expected, items, bytes, work) in [
        (
            list(3),
            list(4096),
            Value::list(vec![Value::Int(1), Value::Int(2)]),
            2,
            4 * size_of::<Value>() + 2 * size_of::<usize>(),
            2,
        ),
        (
            string(""),
            string(&"z".repeat(4096)),
            Value::string("🙂\0"),
            0,
            5 + 2 * size_of::<usize>(),
            3,
        ),
    ] {
        for retain_type in [false, true] {
            for value in [&short, &long] {
                let text = "let selected = input[1:3]";
                let run = |budget: &mut ResourceBudget| {
                    evaluate_slice(
                        text,
                        &mut scope(value),
                        budget,
                        CancellationToken::never(),
                        retain_type,
                    )
                };
                let mut measured = ResourceBudget::steps(u64::MAX)
                    .with_collection_items(u64::MAX)
                    .with_collection_bytes(u64::MAX);
                assert!(matches!(run(&mut measured), Ok(value) if value == expected));
                assert_eq!(measured.used(), 4 + work);
                assert_eq!(measured.collection_items(), items);
                assert_eq!(measured.collection_bytes(), bytes as u64);
                let mut exact = ResourceBudget::steps(measured.used())
                    .with_collection_items(items)
                    .with_collection_bytes(bytes as u64);
                assert!(matches!(run(&mut exact), Ok(value) if value == expected));
                let mut excess = vec![
                    ResourceBudget::steps(measured.used() - 1),
                    ResourceBudget::unlimited().with_collection_bytes(bytes as u64 - 1),
                ];
                if items > 0 {
                    excess.push(ResourceBudget::unlimited().with_collection_items(items - 1));
                }
                for mut budget in excess {
                    assert!(
                        matches!(run(&mut budget), Err(Abort::Error(error)) if *error.kind() == RuntimeErrorKind::ResourceBudgetExceeded)
                    );
                }
            }
        }
    }
}

#[test]
fn cancellation_at_each_slice_work_boundary_leaves_no_binding() {
    for input in [Value::string("é🙂q"), Value::list(vec![Value::Int(0); 3])] {
        let base = scope(&input);
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&polls);
        let text = "let selected = input[1:3]";
        assert!(
            evaluate_slice(
                text,
                &mut base.clone(),
                &mut ResourceBudget::unlimited(),
                CancellationToken::from_fn(move || {
                    seen.fetch_add(1, Ordering::SeqCst);
                    false
                }),
                false
            )
            .is_ok()
        );
        for at in 0..polls.load(Ordering::SeqCst) {
            let seen = AtomicUsize::new(0);
            let mut current = base.clone();
            let result = evaluate_slice(
                text,
                &mut current,
                &mut ResourceBudget::unlimited(),
                CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= at),
                false,
            );
            assert!(matches!(result, Err(Abort::Cancelled(_))), "poll {at}");
            assert_eq!(current, base);
        }
    }
}

#[test]
fn slice_operand_evaluation_precedes_ordered_family_and_interval_checks() {
    for retain in [false, true] {
        for (input, bounds, operand, message) in [
            (Value::Bool(false), "0:missing", "missing", "missing"),
            (Value::Bool(false), "false:false", "input", "receiver"),
            (
                Value::string("private-text"),
                "-2:false",
                "false",
                "Int endpoint",
            ),
            (Value::string("é🙂"), "-2:-1", "-2", "negative"),
            (Value::string("é🙂"), "0:-1", "-1", "negative"),
            (
                Value::string("é🙂"),
                "9:8",
                "8",
                "precedes start endpoint 9",
            ),
            (Value::string("é🙂"), "3:4", "3", "outside length 2"),
            (
                Value::list(vec![Value::Int(1)]),
                "1:9223372036854775807",
                "9223372036854775807",
                "outside length 1",
            ),
        ] {
            let text = format!("let selected = input[{bounds}]");
            let mut scope = scope(&input);
            let result = evaluate_slice(
                &text,
                &mut scope,
                &mut ResourceBudget::unlimited(),
                CancellationToken::never(),
                retain,
            );
            let Err(Abort::Error(error)) = result else {
                panic!("expected ordered refusal for {text}");
            };
            assert!(error.to_string().contains(message), "{error}");
            assert!(!error.to_string().contains("private-text"));
            let span = error.span();
            assert_eq!(&text[span.start()..span.end()], operand);
            assert!(scope.get("selected").is_none());
        }
    }
}

#[test]
fn a_selected_nominal_slot_charges_its_identity_and_shares_its_fields() {
    use crate::module::NominalTypeId;
    let id = NominalTypeId::standard("fixture", "Item");
    let identity_bytes = id.name().len()
        + id.module().path().as_os_str().as_encoded_bytes().len()
        + "std".len()
        + "fixture".len();
    let record = Value::NominalRecord(Box::new(crate::NominalRecordValue::new(
        id,
        vec![],
        vec![(Arc::from("data"), Value::string("shared"))],
    )));
    let input = Value::list(vec![record.clone(), record]);
    let scope = scope(&input);
    let bytes = (2 * size_of::<Value>()
        + 2 * size_of::<usize>()
        + size_of::<crate::NominalRecordValue>()
        + identity_bytes) as u64;
    let mut exact = ResourceBudget::unlimited().with_collection_bytes(bytes);
    let text = "let selected = input[1:2]";
    let Ok(result) = evaluate_slice(
        text,
        &mut scope.clone(),
        &mut exact,
        CancellationToken::never(),
        false,
    ) else {
        panic!("nominal slice exceeded its exact budget");
    };
    assert_eq!(exact.collection_bytes(), bytes);
    let Value::NominalRecord(before) = &input.as_list().unwrap()[1] else {
        panic!("record")
    };
    let Value::NominalRecord(after) = &result.as_list().unwrap()[0] else {
        panic!("record")
    };
    assert_eq!(before.fields().as_ptr(), after.fields().as_ptr());
    let mut failed = scope.clone();
    assert!(matches!(
        evaluate_slice(text, &mut failed, &mut ResourceBudget::unlimited().with_collection_bytes(bytes - 1), CancellationToken::never(), false),
        Err(Abort::Error(error)) if *error.kind() == RuntimeErrorKind::ResourceBudgetExceeded
    ));
    assert_eq!(failed, scope);
}

#[test]
fn callable_defined_inside_a_slice_endpoint_can_be_restored() {
    let text = "let selected = [1][0:({|| -> Int 1})()]";
    let source = Arc::new(SourceFile::new(
        SourceId::new(714),
        "slice-callable.opaal",
        text,
    ));
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("source must parse")
    };
    let StatementKind::Declaration(binding) = script.statements()[0].kind() else {
        panic!("declaration")
    };
    let ExpressionKind::Slice(slice) = binding.value.kind() else {
        panic!("slice")
    };
    let ExpressionKind::Call(call) = slice.end.kind() else {
        panic!("endpoint call")
    };
    let mut environment = Environment::new();
    let mut host = PureEvaluationHost {
        environment: &mut environment,
        policy: EvaluationPolicy::PureOpaal,
    };
    let mut budget = ResourceBudget::unlimited();
    let mut evaluator = Evaluator {
        source: Arc::clone(&source),
        binding_types: Arc::new(
            RuntimeBindingTypes::analyze_repl_source(
                &source,
                &script,
                &crate::module::ModuleAliasRegistry::default(),
            )
            .unwrap(),
        ),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects: None,
        cancel: CancellationToken::never(),
        budget: &mut budget,
        host: &mut host,
    };
    let mut scope = ScopeStack::new();
    let callable = evaluator
        .expression(&call.callee, &mut scope)
        .unwrap_or_else(|_| panic!("endpoint callable"));
    scope
        .declare("last", BindingMutability::Immutable, &callable)
        .unwrap();
    let wire = crate::capsule::encode_background_capsule(
        "slice-callable.opaal",
        text,
        Path::new("/fixture"),
        &Environment::new(),
        None,
        &scope,
        crate::plan::SessionOptions::default(),
    )
    .unwrap();
    let decoded = crate::capsule::decode_background_capsule(&wire).unwrap();
    assert!(matches!(
        apply_callable(
            decoded.scope().get("last").unwrap(),
            &[],
            &source,
            slice.end.span(),
            &mut Environment::new(),
            &EvalLimits::default()
        )
        .unwrap(),
        Completion::Value(Value::Int(1))
    ));
}
