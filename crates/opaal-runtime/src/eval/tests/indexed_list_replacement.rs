use super::super::*;
use opaal_syntax::{ParseOutcome, SourceId, parse_opaal};
use std::sync::atomic::AtomicUsize;

fn ints(values: &[i64]) -> Value {
    Value::list(values.iter().copied().map(Value::Int).collect())
}

fn replace(
    text: &str,
    scope: &mut ScopeStack,
    budget: &mut ResourceBudget,
    cancel: CancellationToken,
) -> Eval<()> {
    let source = Arc::new(SourceFile::new(SourceId::new(711), "replace.opaal", text));
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("{text}")
    };
    let StatementKind::IndexedAssignment(assignment) = script.statements()[0].kind() else {
        panic!("statement")
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
    evaluator.indexed_assignment(assignment, scope)
}

fn scope(length: usize) -> ScopeStack {
    let mut scope = ScopeStack::new();
    let value = Value::list((0..length).map(|n| ints(&[n as i64, 2])).collect());
    scope
        .declare_typed(
            "items",
            BindingMutability::Mutable,
            value.clone(),
            Some(ValueType::List(Box::new(ValueType::List(Box::new(
                ValueType::Int,
            ))))),
        )
        .unwrap();
    scope
        .declare("saved", BindingMutability::Immutable, value)
        .unwrap();
    scope
}

#[test]
fn exact_copy_type_depth_budgets_and_first_excess_never_publish() {
    for length in [1, 2, 5, 64] {
        let base = scope(length);
        let mut success = base.clone();
        success
            .declare("replacement", BindingMutability::Immutable, ints(&[8, 9]))
            .unwrap();
        let mut budget = ResourceBudget::steps(u64::MAX);
        assert!(
            replace(
                "items[0] = replacement",
                &mut success,
                &mut budget,
                CancellationToken::never()
            )
            .is_ok()
        );
        assert_eq!(budget.collection_items(), length as u64);
        assert_eq!(
            budget.collection_bytes(),
            (2 * length * size_of::<Value>() + 2 * size_of::<usize>()) as u64
        );
        assert_eq!(budget.used(), (length + 4) as u64);
        assert_eq!(budget.peak_call_depth(), 1);
        for limit in [
            ResourceBudget::steps(budget.used() - 1),
            ResourceBudget::unlimited().with_collection_items(length as u64 - 1),
            ResourceBudget::unlimited().with_collection_bytes(budget.collection_bytes() - 1),
            ResourceBudget::unlimited().with_call_depth(0),
        ] {
            let mut original = base.clone();
            original
                .declare("replacement", BindingMutability::Immutable, ints(&[8, 9]))
                .unwrap();
            let before = original.clone();
            assert!(
                matches!(replace("items[0] = replacement", &mut original, &mut limit.clone(), CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
            );
            assert_eq!(original, before);
        }
        let mut exact = base.clone();
        exact
            .declare("replacement", BindingMutability::Immutable, ints(&[8, 9]))
            .unwrap();
        assert!(
            replace(
                "items[0] = replacement",
                &mut exact,
                &mut ResourceBudget::steps(budget.used())
                    .with_collection_items(length as u64)
                    .with_collection_bytes(budget.collection_bytes())
                    .with_call_depth(1),
                CancellationToken::never()
            )
            .is_ok()
        );
        assert_eq!(exact.declared_type("items"), base.declared_type("items"));
        assert_eq!(exact.get("saved"), base.get("saved"));
        assert_ne!(exact.get("items"), base.get("items"));
    }
}

#[test]
fn every_copy_and_type_validation_cancellation_preserves_target_metadata_and_alias() {
    let mut base = scope(5);
    base.declare("replacement", BindingMutability::Immutable, ints(&[8, 9]))
        .unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = polls.clone();
    assert!(
        replace(
            "items[0] = replacement",
            &mut base.clone(),
            &mut ResourceBudget::unlimited(),
            CancellationToken::from_fn(move || {
                seen.fetch_add(1, Ordering::SeqCst);
                false
            })
        )
        .is_ok()
    );
    for at in 0..polls.load(Ordering::SeqCst) {
        let seen = Arc::new(AtomicUsize::new(0));
        let mut current = base.clone();
        assert!(matches!(
            replace(
                "items[0] = replacement",
                &mut current,
                &mut ResourceBudget::unlimited(),
                CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= at)
            ),
            Err(Abort::Cancelled(_))
        ));
        assert_eq!(current, base, "poll {at}");
    }
}

#[test]
fn root_and_bounds_failures_suppress_later_expressions_and_payloads() {
    for (root, mutability, text, expected) in [
        (
            Value::string("private-root"),
            BindingMutability::Mutable,
            "items[missing_index] = missing_rhs",
            "indexed assignment",
        ),
        (
            ints(&[1]),
            BindingMutability::Immutable,
            "items[missing_index] = missing_rhs",
            "immutable",
        ),
        (
            ints(&[1]),
            BindingMutability::Mutable,
            "items[-1] = missing_rhs",
            "negative",
        ),
        (
            ints(&[1]),
            BindingMutability::Mutable,
            "items[1] = missing_rhs",
            "outside length",
        ),
        (
            ints(&[1]),
            BindingMutability::Mutable,
            "items['private-index'] = missing_rhs",
            "[]",
        ),
    ] {
        let mut scope = ScopeStack::new();
        scope.declare("items", mutability, root).unwrap();
        let before = scope.clone();
        let Err(Abort::Error(error)) = replace(
            text,
            &mut scope,
            &mut ResourceBudget::unlimited(),
            CancellationToken::never(),
        ) else {
            panic!("must fail")
        };
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("private-"));
        assert!(!error.to_string().contains("missing_"));
        assert_eq!(scope, before);
    }
}

#[test]
fn callable_occurring_only_in_index_survives_capsule_roundtrip() {
    let text = "mut items = [1]\nitems[({|| -> Int 0})()] = 8";
    let source = Arc::new(SourceFile::new(
        SourceId::new(712),
        "index-callable.opaal",
        text,
    ));
    let ParseOutcome::Complete(script) = parse_opaal(&source) else {
        panic!("parse")
    };
    let StatementKind::IndexedAssignment(assignment) = script.statements()[1].kind() else {
        panic!("statement")
    };
    let ExpressionKind::Call(call) = assignment.index.kind() else {
        panic!("index")
    };
    let mut environment = Environment::new();
    let mut host = PureEvaluationHost {
        environment: &mut environment,
        policy: EvaluationPolicy::PureOpaal,
    };
    let mut budget = ResourceBudget::steps(u64::MAX);
    let mut evaluator = Evaluator {
        source: source.clone(),
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
        .unwrap_or_else(|_| panic!("closure"));
    scope
        .declare("position", BindingMutability::Immutable, callable)
        .unwrap();
    let wire = crate::capsule::encode_background_capsule(
        "index-callable.opaal",
        text,
        Path::new("/fixture"),
        &Environment::new(),
        None,
        &scope,
        crate::plan::SessionOptions::default(),
    )
    .unwrap();
    let decoded = crate::capsule::decode_background_capsule(&wire).unwrap();
    let restored = decoded.scope().get("position").unwrap();
    assert!(matches!(
        apply_callable(
            restored,
            vec![],
            &source,
            assignment.index.span(),
            &mut Environment::new(),
            &EvalLimits::default()
        )
        .unwrap(),
        Completion::Value(Value::Int(0))
    ));
}

#[test]
fn boxed_nominal_slots_charge_identity_storage_and_share_nested_payloads() {
    use crate::module::NominalTypeId;
    use crate::value::{NominalRecordValue, VariantValue};
    let id = NominalTypeId::standard("fixture", "Item");
    let identity_bytes = id.name().len()
        + id.module().path().as_os_str().as_encoded_bytes().len()
        + "std".len()
        + "fixture".len();
    let record = Value::NominalRecord(Box::new(NominalRecordValue::new(
        id.clone(),
        vec![],
        vec![(Arc::from("data"), ints(&[1, 2, 3]))],
    )));
    let variant = Value::Variant(Box::new(VariantValue::new(
        id,
        vec![],
        "Some",
        vec![ints(&[4, 5, 6])],
    )));
    let mut base = ScopeStack::new();
    let original = Value::list(vec![Value::Int(0), record, variant]);
    base.declare("items", BindingMutability::Mutable, original.clone())
        .unwrap();
    base.declare("saved", BindingMutability::Immutable, original)
        .unwrap();
    let bytes = (6 * size_of::<Value>()
        + 2 * size_of::<usize>()
        + size_of::<NominalRecordValue>()
        + size_of::<VariantValue>()
        + 2 * identity_bytes) as u64;
    let mut short = base.clone();
    assert!(matches!(replace("items[0] = null", &mut short,
        &mut ResourceBudget::unlimited().with_collection_bytes(bytes-1), CancellationToken::never()), Err(Abort::Error(error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)));
    assert_eq!(short, base);
    let mut exact = base.clone();
    let mut budget = ResourceBudget::unlimited().with_collection_bytes(bytes);
    assert!(
        replace(
            "items[0] = null",
            &mut exact,
            &mut budget,
            CancellationToken::never()
        )
        .is_ok()
    );
    assert_eq!(budget.collection_bytes(), bytes);
    let output = exact.get("items").unwrap().as_list().unwrap();
    let input = base.get("saved").unwrap().as_list().unwrap();
    let (Value::NominalRecord(before), Value::NominalRecord(after)) = (&input[1], &output[1])
    else {
        panic!("record")
    };
    assert_eq!(before.fields().as_ptr(), after.fields().as_ptr());
    let (Value::Variant(before), Value::Variant(after)) = (&input[2], &output[2]) else {
        panic!("variant")
    };
    assert_eq!(before.payload().as_ptr(), after.payload().as_ptr());
    assert_eq!(exact.get("saved"), base.get("saved"));
}
