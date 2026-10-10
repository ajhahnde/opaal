use super::super::*;
use opaal_syntax::{ParseOutcome, SourceId, parse_opaal};
use std::cell::{Cell, RefCell};
use std::sync::atomic::AtomicUsize;

struct ToggleHost {
    environment: Environment,
    allowed: Cell<bool>,
    events: RefCell<Vec<&'static str>>,
}
impl ToggleHost {
    fn new() -> Self {
        Self {
            environment: Environment::new(),
            allowed: Cell::new(false),
            events: RefCell::new(Vec::new()),
        }
    }
}
impl EvaluationHost for ToggleHost {
    fn environment(&mut self) -> &mut Environment {
        &mut self.environment
    }
    fn current_status(&self) -> Option<&Status> {
        None
    }
    fn policy(&self) -> EvaluationPolicy {
        EvaluationPolicy::ControlledAction
    }
    fn permits_controlled_action(&self) -> bool {
        self.events.borrow_mut().push("permission");
        self.allowed.get()
    }
    fn action_start(&mut self, _: &ActionId) -> Option<Result<(), OperationalModuleError>> {
        self.events.borrow_mut().push("start");
        Some(Ok(()))
    }
    fn action_end(
        &mut self,
        _: &ActionId,
        _: SourceActionOutcome,
    ) -> Option<Result<(), OperationalModuleError>> {
        self.events.borrow_mut().push("end");
        Some(Ok(()))
    }
    fn read_directory(&mut self, _: &Path) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
        panic!("no filesystem calls")
    }
    fn execute_chain(
        &mut self,
        _: &ConditionalChain,
        _: &mut ScopeStack,
        _: EvaluationContext,
    ) -> Result<Status, Abort> {
        panic!("no commands")
    }
}

fn parsed(source: &SourceFile) -> Script {
    let ParseOutcome::Complete(script) = parse_opaal(source) else {
        panic!("fixture parse: {}", source.text())
    };
    script
}
fn stored(function: &CallableValue) -> bool {
    function.effect_requirement()
}
fn definition(statement: &Statement) -> FunctionDefinition {
    match statement.kind() {
        StatementKind::Function(f) => f.clone(),
        StatementKind::Action(a) => a.as_function(),
        StatementKind::If(i) => definition(&i.then_block.statements[0]),
        _ => panic!("expected definition"),
    }
}
fn constructed(
    ast: &FunctionDefinition,
    retained: Arc<SourceFile>,
    analyzed: bool,
) -> Arc<dyn Callable> {
    let declared_name = retained.slice(ast.name.span()).unwrap().to_owned();
    let types = if analyzed {
        RuntimeBindingTypes::analyze_repl_source(
            &retained,
            &parsed(&retained),
            &crate::module::ModuleAliasRegistry::default(),
        )
        .unwrap()
    } else {
        RuntimeBindingTypes::default()
    };
    let mut host = ToggleHost::new();
    let mut budget = ResourceBudget::unlimited();
    let evaluator = Evaluator {
        source: retained,
        binding_types: Arc::new(types),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects: None,
        cancel: CancellationToken::never(),
        budget: &mut budget,
        host: &mut host,
    };
    let mut scope = ScopeStack::new();
    assert!(evaluator.function_definition(ast, &mut scope).is_ok());
    let Value::Callable(callable) = scope.get(&declared_name).unwrap() else {
        panic!("callable")
    };
    Arc::clone(callable)
}
#[test]
fn direct_capsule_wire_and_restored_permission_are_preserved() {
    for text in [
        "def candidate() -> Int { return 7 }",
        "action candidate() -> Int effects {} { return 7 }",
        "action candidate() -> Int effects { clock.wall; clock.wall; } { return 7 }",
    ]
    .iter()
    {
        let source = Arc::new(SourceFile::new(SourceId::new(11), "capsule.opaal", *text));
        let ast = definition(&parsed(&source).statements()[0]);
        let callable = constructed(&ast, source, true);
        let mut scope = ScopeStack::new();
        scope
            .declare(
                "candidate",
                BindingMutability::Immutable,
                Value::Callable(Arc::clone(&callable)),
            )
            .unwrap();
        let wire = crate::capsule::encode_background_capsule(
            "capsule.opaal",
            text,
            Path::new("/fixture"),
            &Environment::new(),
            None,
            &scope,
            crate::plan::SessionOptions::default(),
        )
        .unwrap();
        let decoded = crate::capsule::decode_background_capsule(&wire).unwrap();
        let value = decoded.scope().get("candidate").unwrap();
        let Value::Callable(restored) = value else {
            panic!("callable")
        };
        let c = function(restored);
        assert_eq!(
            stored(c),
            action_has_declared_effects(&c.family.source, c.family.origin_span)
        );
        let result = apply_callable(
            value,
            vec![],
            &c.family.source,
            c.family.origin_span,
            &mut Environment::new(),
            &EvalLimits::default(),
        );
        if text.contains("clock.wall") {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("effectful action execution")
            );
        } else {
            assert!(matches!(result.unwrap(), Completion::Value(Value::Int(7))));
        }
        assert!(crate::capsule::decode_background_capsule(&wire[..wire.len() - 1]).is_err());
        let mut future = wire.clone();
        future[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(crate::capsule::decode_background_capsule(&future).is_err());
    }
}

fn function(callable: &Arc<dyn Callable>) -> &CallableValue {
    callable.as_any().downcast_ref::<CallableValue>().unwrap()
}

#[test]
fn complete_source_oracle_and_mismatched_ast_are_equal() {
    for (text, expected) in [
        ("def candidate() -> Int { return 7 }", false),
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
        let source = Arc::new(SourceFile::new(SourceId::new(1), "same.opaal", text));
        let ast = definition(&parsed(&source).statements()[0]);
        let value = constructed(&ast, Arc::clone(&source), false);
        let c = function(&value);
        assert_eq!(
            action_has_declared_effects(&source, ast.name.span()),
            expected
        );
        assert_eq!(stored(c), expected);
        assert_eq!(stored(&c.clone()), expected);
        assert!(!action_has_declared_effects(
            &source,
            source.span(0..1).unwrap()
        ));
    }
    let valid = "action candidate() -> Int effects { clock.wall; } { return 7 }";
    let original = SourceFile::new(SourceId::new(1), "same.opaal", valid);
    let ast = definition(&parsed(&original).statements()[0]);
    let nested = Arc::new(SourceFile::new(
        SourceId::new(1),
        "same.opaal",
        format!("if true {{ {valid} }}"),
    ));
    assert!(!matches!(parse_opaal(&nested), ParseOutcome::Complete(_)));
    let mut nested_ast = ast.clone();
    let offset = nested.text().find("candidate").unwrap();
    nested_ast.name = opaal_syntax::Identifier::new(nested.span(offset..offset + 9).unwrap());
    let nested_callable = constructed(&nested_ast, Arc::clone(&nested), false);
    assert!(!action_has_declared_effects(
        &nested,
        nested_ast.name.span()
    ));
    assert!(!stored(function(&nested_callable)));
    for text in [
        format!("{valid}\nlet ="),
        "action candidate() -> Int effects { clock.wall; } {".to_owned(),
        valid.replace("clock.wall;", "           "),
    ] {
        let retained = Arc::new(SourceFile::new(SourceId::new(1), "same.opaal", text));
        let value = constructed(&ast, Arc::clone(&retained), false);
        assert!(!action_has_declared_effects(&retained, ast.name.span()));
        assert!(!stored(function(&value)));
    }
    // Different bytes with the same source label/id/span must not alias.
    let allowed = constructed(&ast, Arc::new(original), false);
    assert!(stored(function(&allowed)));
}

#[test]
fn closures_and_repeated_calls_keep_fact_and_parse_count() {
    for text in [
        "def candidate() -> Int { return 7 }\ncandidate",
        "let candidate = {|| 7}\ncandidate",
    ] {
        let source = SourceFile::new(SourceId::new(2), "pure.opaal", text);
        let script = parsed(&source);
        let value = evaluate(&script, &source, &mut ScopeStack::new()).unwrap();
        let Value::Callable(_callable) = &value else {
            panic!("callable")
        };
        CLASSIFICATION_PARSES.with(|c| c.set(0));
        for _ in 0..7 {
            let result = apply_callable(
                &value,
                vec![],
                &source,
                script.span(),
                &mut Environment::new(),
                &EvalLimits::default(),
            )
            .unwrap();
            assert!(matches!(result, Completion::Value(Value::Int(7))));
        }
        assert_eq!(CLASSIFICATION_PARSES.with(Cell::get), 1);
    }
}

#[test]
fn live_permission_denial_precedes_type_depth_and_body() {
    let source = Arc::new(SourceFile::new(
        SourceId::new(3),
        "effect.opaal",
        "action candidate(value: Int) -> Int effects { clock.wall; } { return value }",
    ));
    let ast = definition(&parsed(&source).statements()[0]);
    for analyzed in [false, true] {
        let callable = constructed(&ast, Arc::clone(&source), analyzed);
        let c = function(&callable);
        let mut host = ToggleHost::new();
        let mut budget = ResourceBudget::unlimited().with_call_depth(0);
        let mut evaluator = Evaluator {
            source: Arc::clone(&source),
            binding_types: Arc::clone(&c.binding_types),
            current_result_type: None,
            current_type_arguments: BTreeMap::new(),
            budgeted_callback: false,
            standard_effects: None,
            cancel: CancellationToken::never(),
            budget: &mut budget,
            host: &mut host,
        };
        let result = evaluator.run_call(
            &callable,
            c,
            vec![RuntimeArgument {
                value: Value::String(Arc::from("wrong")),
                span: ast.name.span(),
            }],
            ast.name.span(),
            None,
            None,
        );
        assert!(
            matches!(result, Err(Abort::Refused(refusal)) if refusal.operation() == "effectful action execution")
        );
        assert_eq!(budget.call_depth, 0);
        assert_eq!(budget.used_steps, 0);
        assert_eq!(&*host.events.borrow(), &["permission"]);
        for allowed in [false, true, false] {
            host.allowed.set(allowed);
            host.events.borrow_mut().clear();
            let mut budget = ResourceBudget::steps(100).with_call_depth(10);
            let mut evaluator = Evaluator {
                source: Arc::clone(&source),
                binding_types: Arc::clone(&c.binding_types),
                current_result_type: None,
                current_type_arguments: BTreeMap::new(),
                budgeted_callback: false,
                standard_effects: None,
                cancel: CancellationToken::never(),
                budget: &mut budget,
                host: &mut host,
            };
            let result = evaluator.run_call(
                &callable,
                c,
                vec![RuntimeArgument {
                    value: Value::Int(7),
                    span: ast.name.span(),
                }],
                ast.name.span(),
                None,
                None,
            );
            if allowed {
                assert!(matches!(result, Ok(Value::Int(7))));
                let has_action = c
                    .binding_types
                    .function_signature(c.family.source.id(), c.family.origin_span)
                    .and_then(|s| s.downstream().action())
                    .is_some();
                assert_eq!(
                    &*host.events.borrow(),
                    if has_action {
                        &["permission", "start", "end"][..]
                    } else {
                        &["permission"][..]
                    }
                );
            } else {
                assert!(matches!(result, Err(Abort::Refused(_))));
                assert_eq!(budget.used_steps, 0);
                assert_eq!(&*host.events.borrow(), &["permission"]);
            }
            assert_eq!(budget.call_depth, 0);
        }
    }
}

#[test]
fn reconstructed_direct_actions_recompute_fact_and_reject_bad_signatures() {
    for text in [
        "def candidate() -> Int { return 7 }",
        "action candidate() -> Int effects {} { return 7 }",
        "action candidate() -> Int effects { clock.wall; } { return 7 }",
    ] {
        let source = Arc::new(SourceFile::new(SourceId::new(4), "restore.opaal", text));
        let ast = definition(&parsed(&source).statements()[0]);
        let callable = constructed(&ast, source, true);
        let snapshot = snapshot_callable(&callable).unwrap();
        let restored = restore_callable(snapshot.clone(), &mut 1_000_000).unwrap();
        let c = function(&restored);
        assert_eq!(
            stored(c),
            action_has_declared_effects(&c.family.source, c.family.origin_span)
        );
        let mut bad = snapshot.clone();
        bad.result_type = Some(ValueType::String);
        assert!(restore_callable(bad, &mut 1_000_000).is_err());
        let mut bad = snapshot.clone();
        bad.origin_span = bad.source.span(0..1).unwrap();
        assert!(restore_callable(bad, &mut 1_000_000).is_err());
        let mut bad = snapshot.clone();
        bad.source = SourceFile::new(SourceId::new(4), "restore.opaal", format!("{text}\nlet ="));
        assert!(restore_callable(bad, &mut 1_000_000).is_err());
        let caller = SourceFile::new(SourceId::new(5), "caller.opaal", "candidate()");
        let result = apply_callable(
            &Value::Callable(restored),
            vec![],
            &caller,
            caller.span(0..9).unwrap(),
            &mut Environment::new(),
            &EvalLimits::default(),
        );
        if text.contains("clock.wall") {
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

fn count() -> usize {
    CLASSIFICATION_PARSES.with(Cell::get)
}
fn reset() {
    CLASSIFICATION_PARSES.with(|c| c.set(0));
}
fn state(callable: &Arc<dyn Callable>) -> u8 {
    function(callable)
        .family
        .classification
        .load(Ordering::Acquire)
}
fn fresh(text: &str, analyzed: bool) -> Arc<dyn Callable> {
    let source = Arc::new(SourceFile::new(SourceId::new(51), "same.opaal", text));
    let ast = definition(&parsed(&source).statements()[0]);
    constructed(&ast, source, analyzed)
}
fn invoke(
    callable: &Arc<dyn Callable>,
    arguments: Vec<Value>,
    host: &mut ToggleHost,
    budget: &mut ResourceBudget,
) -> Eval<Value> {
    let c = function(callable);
    let mut evaluator = Evaluator {
        source: Arc::clone(&c.family.source),
        binding_types: Arc::clone(&c.binding_types),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects: None,
        cancel: CancellationToken::never(),
        budget,
        host,
    };
    evaluator.run_call(
        callable,
        c,
        arguments
            .into_iter()
            .map(|value| RuntimeArgument {
                value,
                span: c.family.origin_span,
            })
            .collect(),
        c.family.origin_span,
        None,
        None,
    )
}
fn map(callable: &Arc<dyn Callable>, items: Vec<Value>) -> Eval<Value> {
    let c = function(callable);
    let mut host = ToggleHost::new();
    let mut budget = ResourceBudget::unlimited();
    let mut evaluator = Evaluator {
        source: Arc::clone(&c.family.source),
        binding_types: Arc::clone(&c.binding_types),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects: None,
        cancel: CancellationToken::never(),
        budget: &mut budget,
        host: &mut host,
    };
    evaluator.list_operation(
        &crate::operation::standard_operation(
            &crate::module::ModuleId::standard("std", "list"),
            "map",
        )
        .unwrap(),
        &[Value::list(items), Value::Callable(Arc::clone(callable))],
        &BTreeMap::from([
            ("T".to_owned(), ValueType::Int),
            ("U".to_owned(), ValueType::Int),
        ]),
        c.family.origin_span,
    )
}
fn install_probe(callable: &Arc<dyn Callable>, callback: impl Fn() + 'static) {
    let identity = Arc::as_ptr(&function(callable).family) as usize;
    COLD_OBSERVER.with(|p| *p.borrow_mut() = Some((identity, Box::new(callback))));
}
fn clear_probe() {
    COLD_OBSERVER.with(|p| *p.borrow_mut() = None);
}

#[test]
fn first_use_sequential_truth_table() {
    let valid = "action candidate() -> Int effects { clock.wall; clock.wall; } { return 7 }";
    let ast_source = SourceFile::new(SourceId::new(51), "same.opaal", valid);
    let action_ast = definition(&parsed(&ast_source).statements()[0]);
    let fixtures = [
        "def candidate() -> Int { return 7 }".to_owned(),
        "action candidate() -> Int effects {} { return 7 }".to_owned(),
        valid.to_owned(),
        valid.replace("clock.wall; clock.wall;", "clock.wall;"),
        format!("{valid}\nlet ="),
        "action candidate() -> Int effects { clock.wall; } {".to_owned(),
        valid.replace("clock.wall; clock.wall;", "                       "),
        format!("if true {{ {valid} }}"),
    ];
    for text in fixtures {
        let source = Arc::new(SourceFile::new(SourceId::new(51), "same.opaal", text));
        let ast = if matches!(parse_opaal(&source), ParseOutcome::Complete(_)) {
            definition(&parsed(&source).statements()[0])
        } else {
            action_ast.clone()
        };
        let expected = action_has_declared_effects(&source, ast.name.span());
        reset();
        let callable = constructed(&ast, Arc::clone(&source), false);
        assert_eq!(count(), 0);
        assert_eq!(state(&callable), 0);
        assert_eq!(function(&callable).effect_requirement(), expected);
        assert_eq!(count(), 1);
        for _ in 0..7 {
            assert_eq!(function(&callable).effect_requirement(), expected);
        }
        assert_eq!(count(), 1);
        assert_eq!(state(&callable), if expected { 2 } else { 1 });
        let mut mismatch = ast.clone();
        mismatch.name = opaal_syntax::Identifier::new(source.span(0..1).unwrap());
        let c = constructed(&mismatch, source, false);
        assert_eq!(state(&c), 0);
        assert!(!function(&c).effect_requirement());
    }
}

#[test]
fn first_use_pre_guard_paths_stay_cold() {
    let c = fresh("def candidate(value: Int) -> Int { return value }", true);
    reset();
    let f = function(&c);
    assert!(
        apply_callable(
            &Value::Callable(Arc::clone(&c)),
            vec![],
            &f.family.source,
            f.family.origin_span,
            &mut Environment::new(),
            &EvalLimits::default()
        )
        .is_err()
    );
    let limits = EvalLimits::new(
        CancellationToken::from_fn(|| true),
        ResourceBudget::unlimited(),
    );
    assert!(matches!(
        apply_callable(
            &Value::Callable(Arc::clone(&c)),
            vec![Value::Int(7)],
            &f.family.source,
            f.family.origin_span,
            &mut Environment::new(),
            &limits
        )
        .unwrap(),
        Completion::Cancelled(_)
    ));
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "candidate",
            BindingMutability::Immutable,
            Value::Callable(Arc::clone(&c)),
        )
        .unwrap();
    let caller = SourceFile::new(SourceId::new(52), "arguments.opaal", "candidate(missing)");
    assert!(
        evaluate_in_environment(
            &parsed(&caller),
            &caller,
            &mut scope,
            &mut Environment::new(),
            &EvalLimits::default()
        )
        .is_err()
    );
    assert!(matches!(map(&c, vec![]), Ok(Value::List(v)) if v.is_empty()));
    assert_eq!(count(), 0);
    assert_eq!(state(&c), 0);
    let bad = fresh("def candidate(a: Int, b: Int) -> Int { return a }", true);
    assert!(map(&bad, vec![]).is_err());
    assert_eq!(count(), 0);
    assert_eq!(state(&bad), 0);
}

#[test]
fn first_use_live_permission_and_failed_calls() {
    for analyzed in [false, true] {
        let c = fresh(
            "action candidate(value: Int) -> Int effects { clock.wall; } { return value }",
            analyzed,
        );
        let mut host = ToggleHost::new();
        reset();
        for allowed in [false, true, false] {
            host.allowed.set(allowed);
            host.events.borrow_mut().clear();
            let mut budget = ResourceBudget::steps(100).with_call_depth(10);
            let result = invoke(&c, vec![Value::Int(7)], &mut host, &mut budget);
            if allowed {
                assert!(matches!(result, Ok(Value::Int(7))));
            } else {
                assert!(matches!(result, Err(Abort::Refused(_))));
                assert_eq!(budget.used_steps, 0);
            }
            let has_action = function(&c)
                .binding_types
                .function_signature(
                    function(&c).family.source.id(),
                    function(&c).family.origin_span,
                )
                .and_then(|signature| signature.downstream().action())
                .is_some();
            let expected: &[&str] = if allowed && has_action {
                &["permission", "start", "end"]
            } else {
                &["permission"]
            };
            assert_eq!(&*host.events.borrow(), expected);
            assert_eq!(budget.call_depth, 0);
            assert_eq!(count(), 1);
        }
        host.allowed.set(true);
        let wrong = invoke(
            &c,
            vec![Value::String(Arc::from("wrong"))],
            &mut host,
            &mut ResourceBudget::unlimited(),
        );
        if analyzed {
            assert!(wrong.is_err());
        } else {
            assert!(matches!(wrong, Ok(Value::String(_))));
        }
        assert!(
            invoke(
                &c,
                vec![Value::Int(7)],
                &mut host,
                &mut ResourceBudget::unlimited().with_call_depth(0)
            )
            .is_err()
        );
        assert_eq!(count(), 1);
        assert_eq!(state(&c), 2);
    }
    for text in [
        "def candidate(value: Int) -> Int { return value }",
        "def candidate(value: Int) -> Int { throw 'body' }",
        "def candidate[X](value: Int) -> Int { return value }",
    ] {
        let c = fresh(text, true);
        let mut host = ToggleHost::new();
        reset();
        let _ = invoke(
            &c,
            vec![Value::String(Arc::from("wrong"))],
            &mut host,
            &mut ResourceBudget::unlimited(),
        );
        let _ = invoke(
            &c,
            vec![Value::Int(7)],
            &mut host,
            &mut ResourceBudget::unlimited().with_call_depth(0),
        );
        let _ = invoke(
            &c,
            vec![Value::Int(7)],
            &mut host,
            &mut ResourceBudget::unlimited(),
        );
        assert_eq!(count(), 1);
        assert_eq!(state(&c), 1);
        assert!(host.events.borrow().is_empty());
    }
}

#[test]
fn first_use_clone_family_and_collisions() {
    let text = "def candidate(value: Int) -> Int { return value }";
    let original = fresh(text, true);
    reset();
    let cold_clone = function(&original).clone();
    assert!(Arc::ptr_eq(&cold_clone.family, &function(&original).family));
    assert_eq!(count(), 0);
    assert_eq!(state(&original), 0);
    for _ in 0..7 {
        assert!(
            matches!(map(&original, vec![Value::Int(7)]), Ok(Value::List(v)) if v.as_ref()==[Value::Int(7)])
        );
    }
    assert_eq!(count(), 1);
    assert_eq!(state(&original), 1);
    assert!(!cold_clone.effect_requirement());
    let mut adjusted = cold_clone.clone();
    adjusted
        .captured_type_arguments
        .insert("X".to_owned(), ValueType::Int);
    adjusted.parameters[0].value_type = ValueType::Any;
    adjusted.result_type = Some(ValueType::Any);
    assert!(!adjusted.effect_requirement());
    assert_eq!(count(), 1);
    let identical = fresh(text, true);
    assert!(!Arc::ptr_eq(
        &function(&identical).family,
        &function(&original).family
    ));
    assert_eq!(state(&identical), 0);
    assert!(!function(&identical).effect_requirement());
    assert_eq!(count(), 2);
    let effect = fresh(
        "action candidate(value: Int) -> Int effects { clock.wall; } { return value }",
        false,
    );
    let ast = definition(&parsed(&function(&effect).family.source).statements()[0]);
    let false_bytes = Arc::new(SourceFile::new(
        SourceId::new(51),
        "same.opaal",
        function(&effect)
            .family
            .source
            .text()
            .replace("clock.wall;", "           "),
    ));
    let different = constructed(&ast, false_bytes, false);
    assert_eq!(
        function(&effect).family.origin_span,
        function(&different).family.origin_span
    );
    assert!(function(&effect).effect_requirement());
    assert!(!function(&different).effect_requirement());
    assert!(!Arc::ptr_eq(
        &function(&effect).family,
        &function(&different).family
    ));
}

#[test]
fn first_use_concurrent_cold_publication() {
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;
    for (text, expected) in [
        ("def candidate() -> Int { return 7 }", false),
        (
            "action candidate() -> Int effects { clock.wall; } { return 7 }",
            true,
        ),
    ] {
        let c = fresh(text, false);
        let total = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let c = Arc::clone(&c);
                    let total = Arc::clone(&total);
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        reset();
                        install_probe(&c, move || {
                            total.fetch_add(1, Ordering::SeqCst);
                            barrier.wait();
                        });
                        let result = function(&c).effect_requirement();
                        clear_probe();
                        assert_eq!(count(), 1);
                        result
                    })
                })
                .collect();
            for h in handles {
                assert_eq!(h.join().unwrap(), expected);
            }
        });
        assert_eq!(total.load(Ordering::SeqCst), 2);
        assert_eq!(state(&c), if expected { 2 } else { 1 });
        reset();
        for _ in 0..7 {
            assert_eq!(function(&c).effect_requirement(), expected);
        }
        assert_eq!(count(), 0);
        // One cold derivation is paused; another publishes and a warmed third reader completes.
        let c = fresh(text, false);
        let total = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let cold = Arc::clone(&c);
            let counter = Arc::clone(&total);
            let paused = scope.spawn(move || {
                install_probe(&cold, move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
                let result = function(&cold).effect_requirement();
                clear_probe();
                result
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            install_probe(&c, {
                let total = Arc::clone(&total);
                move || {
                    total.fetch_add(1, Ordering::SeqCst);
                }
            });
            assert_eq!(function(&c).effect_requirement(), expected);
            clear_probe();
            let warm = Arc::clone(&c);
            let (done_tx, done_rx) = mpsc::channel();
            scope.spawn(move || {
                reset();
                done_tx
                    .send((function(&warm).effect_requirement(), count()))
                    .unwrap();
            });
            let result = done_rx.recv_timeout(Duration::from_secs(5));
            release_tx.send(()).unwrap();
            assert_eq!(result.unwrap(), (expected, 0));
            assert_eq!(paused.join().unwrap(), expected);
        });
        assert_eq!(total.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn first_use_reentrant_and_unwind() {
    let c = fresh(
        "def candidate(value: Int) -> Int { if value == 0 { return 7 }\nreturn candidate(value - 1) }",
        true,
    );
    reset();
    assert!(matches!(
        invoke(
            &c,
            vec![Value::Int(4)],
            &mut ToggleHost::new(),
            &mut ResourceBudget::unlimited()
        ),
        Ok(Value::Int(7))
    ));
    assert_eq!(count(), 1);
    let c = fresh(
        "action candidate() -> Int effects { clock.wall; } { return 7 }",
        false,
    );
    install_probe(&c, || panic!("pre-publication test panic"));
    reset();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || function(&c).effect_requirement()
        ))
        .is_err()
    );
    clear_probe();
    assert_eq!(state(&c), 0);
    assert_eq!(count(), 0);
    assert!(function(&c).effect_requirement());
    assert_eq!(state(&c), 2);
    assert_eq!(count(), 1);
}

#[test]
fn first_use_restore_and_wire() {
    for text in [
        "def candidate() -> Int { return 7 }",
        "action candidate() -> Int effects {} { return 7 }",
        "action candidate() -> Int effects { clock.wall; } { return 7 }",
        "let candidate = {|| 7}",
        "def candidate[X](value: X) -> X { return value }",
        "import std::list as list\ndef identity(x: Int) -> Int { x }\ndef candidate() -> List[Int] { list::map[Int, Int]([1], identity) }",
    ] {
        let source = Arc::new(SourceFile::new(
            SourceId::new(55),
            "restore-all.opaal",
            text,
        ));
        let script = parsed(&source);
        let types = RuntimeBindingTypes::analyze_repl_source(
            &source,
            &script,
            &crate::module::ModuleAliasRegistry::default(),
        )
        .unwrap();
        let mut scope = ScopeStack::new();
        evaluate_in_environment_owned_with_binding_types(
            &script,
            Arc::clone(&source),
            &mut scope,
            &mut Environment::new(),
            &EvalLimits::default(),
            Arc::new(types),
        )
        .unwrap();
        let encode = |scope: &ScopeStack| {
            crate::capsule::encode_background_capsule(
                "next.opaal",
                "",
                Path::new("/fixture"),
                &Environment::new(),
                None,
                scope,
                crate::plan::SessionOptions::default(),
            )
            .unwrap()
        };
        reset();
        let cold = encode(&scope);
        assert_eq!(count(), 0);
        for (_, value) in scope.visible_bindings() {
            if let Value::Callable(c) = value {
                let _ = function(c).effect_requirement();
            }
        }
        assert_eq!(encode(&scope), cold);
        let Value::Callable(original) = scope.get("candidate").unwrap() else {
            panic!()
        };
        reset();
        let decoded = crate::capsule::decode_background_capsule(&cold).unwrap();
        assert_eq!(count(), 0);
        let Value::Callable(c) = decoded.scope().get("candidate").unwrap() else {
            panic!()
        };
        assert_ne!(state(c), 0);
        assert!(!Arc::ptr_eq(
            &function(c).family,
            &function(original).family
        ));
        let expected =
            action_has_declared_effects(&function(c).family.source, function(c).family.origin_span);
        reset();
        for _ in 0..8 {
            assert_eq!(function(c).effect_requirement(), expected);
            let args = if text.contains("value: X") {
                vec![Value::Int(7)]
            } else {
                vec![]
            };
            let _ = invoke(
                c,
                args,
                &mut ToggleHost::new(),
                &mut ResourceBudget::unlimited(),
            );
        }
        assert_eq!(count(), 0);
        let mut bad = snapshot_callable(original).unwrap();
        bad.result_type = Some(ValueType::String);
        assert!(restore_callable(bad, &mut 1_000_000).is_err());
        assert!(crate::capsule::decode_background_capsule(&cold[..cold.len() - 1]).is_err());
    }
}

#[test]
fn first_use_cancel_and_exact_limits() {
    let text =
        "def candidate(value: Int) -> Int { if value < 0 { throw 'negative' }\nreturn value + 1 }";
    let warm = fresh(text, true);
    function(&warm).effect_requirement();
    let clone: Arc<dyn Callable> = Arc::new(function(&warm).clone());
    let restored = restore_callable(snapshot_callable(&warm).unwrap(), &mut 1_000_000).unwrap();
    let caller = SourceFile::new(SourceId::new(56), "caller.opaal", "candidate(7)");
    let span = caller.span(0..3).unwrap();
    for after in 0..32 {
        let mut observations = Vec::new();
        let cold = fresh(text, true);
        for c in [&cold, &warm, &clone, &restored] {
            let checks = Arc::new(AtomicUsize::new(0));
            let inner = Arc::clone(&checks);
            let limits = EvalLimits::new(
                CancellationToken::from_fn(move || inner.fetch_add(1, Ordering::SeqCst) >= after),
                ResourceBudget::steps(100).with_call_depth(5),
            );
            let result = apply_callable(
                &Value::Callable(Arc::clone(c)),
                vec![Value::Int(7)],
                &caller,
                span,
                &mut Environment::new(),
                &limits,
            );
            observations.push((format!("{result:?}"), checks.load(Ordering::SeqCst)));
        }
        assert!(observations.windows(2).all(|pair| pair[0] == pair[1]));
    }
    for steps in 0..24 {
        for depth in 0..3 {
            for value in [
                Value::Int(7),
                Value::Int(-1),
                Value::String(Arc::from("wrong")),
            ] {
                let mut observations = Vec::new();
                let cold = fresh(text, true);
                for c in [&cold, &warm, &clone, &restored] {
                    let mut budget = ResourceBudget::steps(steps).with_call_depth(depth);
                    let limits = EvalLimits::default();
                    let result = apply_callable_with_budget(
                        &Value::Callable(Arc::clone(c)),
                        vec![value.clone()],
                        &caller,
                        span,
                        &mut Environment::new(),
                        &limits,
                        &mut budget,
                    );
                    observations.push((
                        format!("{result:?}"),
                        budget.used_steps,
                        budget.call_depth,
                        budget.peak_call_depth,
                    ));
                }
                assert!(
                    observations.windows(2).all(|pair| pair[0] == pair[1]),
                    "{observations:?}"
                );
            }
        }
    }
}

#[test]
fn family_storage_and_generic_views() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<CallableValue>();
    send_sync::<CallableFamily>();
    let text = "def candidate[X: Equal, Y: Ordered](value: X) -> X { return value }";
    let original = fresh(text, true);
    let f = function(&original);
    assert_eq!(f.family.type_parameters.len(), 2);
    assert_eq!(f.family.type_parameters[0].name(), "X");
    assert_eq!(f.family.type_parameters[1].name(), "Y");
    assert_eq!(f.family.type_parameters[0].constraints().len(), 1);
    assert_eq!(f.family.type_parameters[1].constraints().len(), 1);
    let mut view = f.clone();
    assert!(Arc::ptr_eq(&view.family, &f.family));
    assert_eq!(
        view.family.type_parameters.as_ptr(),
        f.family.type_parameters.as_ptr()
    );
    assert!(Arc::ptr_eq(&view.family.source, &f.family.source));
    view.parameters[0].value_type = ValueType::Int;
    view.result_type = Some(ValueType::Int);
    view.captured_type_arguments
        .insert("X".to_owned(), ValueType::String);
    assert_ne!(view.parameters[0].value_type, f.parameters[0].value_type);
    assert_ne!(view.result_type, f.result_type);
    assert!(f.captured_type_arguments.is_empty());
    let fresh_same = fresh(text, true);
    assert!(!Arc::ptr_eq(&function(&fresh_same).family, &f.family));
    assert_eq!(
        function(&fresh_same).family.origin_span,
        f.family.origin_span
    );
    assert_eq!(
        function(&fresh_same).family.type_parameters,
        f.family.type_parameters
    );
    let alias = Value::Callable(Arc::clone(&original));
    assert_eq!(alias, alias.clone());
    let clone: Arc<dyn Callable> = Arc::new(f.clone());
    assert_ne!(alias, Value::Callable(clone));

    // The actual callback_shape route substitutes captured local evidence,
    // keeps declared generics unchanged, and builds an independently owned shape.
    let one = fresh(
        "def candidate[X: Equal](value: X) -> X { return value }",
        true,
    );
    let mut local = function(&one).clone();
    local.parameters[0].value_type = ValueType::TypeParameter("Outer".to_owned());
    local.result_type = Some(ValueType::TypeParameter("Outer".to_owned()));
    local
        .captured_type_arguments
        .insert("Outer".to_owned(), ValueType::Int);
    local
        .captured_type_arguments
        .insert("X".to_owned(), ValueType::String);
    let local: Arc<dyn Callable> = Arc::new(local);
    let mut host = ToggleHost::new();
    let mut budget = ResourceBudget::unlimited();
    let evaluator = Evaluator {
        source: Arc::clone(&function(&local).family.source),
        binding_types: Arc::clone(&function(&local).binding_types),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects: None,
        cancel: CancellationToken::never(),
        budget: &mut budget,
        host: &mut host,
    };
    let descriptor = crate::operation::standard_operation(
        &crate::module::ModuleId::standard("std", "list"),
        "map",
    )
    .unwrap();
    reset();
    let Ok((adjusted, mut shape)) = evaluator.test_callback_shape(
        &Value::Callable(Arc::clone(&local)),
        &descriptor,
        function(&local).family.origin_span,
    ) else {
        panic!("callback shape fixture")
    };
    assert_eq!(count(), 0);
    assert!(Arc::ptr_eq(&adjusted.family, &function(&local).family));
    assert_eq!(adjusted.parameters[0].value_type, ValueType::Int);
    assert_eq!(adjusted.result_type, Some(ValueType::Int));
    assert_eq!(
        function(&local).parameters[0].value_type,
        ValueType::TypeParameter("Outer".to_owned())
    );
    assert_eq!(
        function(&one).parameters[0].value_type,
        ValueType::TypeParameter("X".to_owned())
    );
    assert_eq!(shape.generics, adjusted.family.type_parameters);
    assert_ne!(
        shape.generics.as_ptr(),
        adjusted.family.type_parameters.as_ptr()
    );
    shape.generics.clear();
    assert_eq!(adjusted.family.type_parameters.len(), 1);
    assert_eq!(
        adjusted.captured_type_arguments,
        function(&local).captured_type_arguments
    );
}

#[test]
fn family_lifetime_and_observers() {
    for text in [
        "def candidate() -> Int { return 7 }",
        "def candidate[X: Equal, Y: Ordered](value: X) -> X { return value }",
        "def candidate(value: Int) -> Int { if value == 0 { return 7 }\nreturn candidate(value - 1) }",
    ] {
        let c = fresh(text, true);
        let weak = Arc::downgrade(&function(&c).family);
        let source = Arc::downgrade(&function(&c).family.source);
        let clone: Arc<dyn Callable> = Arc::new(function(&c).clone());
        let display = format!("{}", Value::Callable(Arc::clone(&c)));
        let debug = format!("{c:?}");
        let inspection = format!("{:?}", c.inspection());
        let snapshot = snapshot_callable(&c).unwrap();
        let binding_types = callable_binding_types(&c).unwrap();
        let carriers = callable_contains_control_carrier(&c);
        function(&c).effect_requirement();
        assert_eq!(format!("{}", Value::Callable(Arc::clone(&c))), display);
        assert_eq!(format!("{c:?}"), debug);
        assert_eq!(format!("{:?}", c.inspection()), inspection);
        assert_eq!(snapshot_callable(&c).unwrap().source, snapshot.source);
        assert_eq!(
            snapshot_callable(&c).unwrap().origin_span,
            snapshot.origin_span
        );
        assert_eq!(callable_contains_control_carrier(&c), carriers);
        assert!(Arc::ptr_eq(
            &binding_types,
            &callable_binding_types(&c).unwrap()
        ));
        drop(binding_types);
        drop(snapshot);
        drop(c);
        assert!(weak.upgrade().is_some());
        assert!(source.upgrade().is_some());
        drop(clone);
        assert!(weak.upgrade().is_none());
        assert!(source.upgrade().is_none());
    }
    // Successful closure/restored families also have no lifetime root.
    let source = SourceFile::new(SourceId::new(77), "closure.opaal", "{|| 7}");
    let value = evaluate(&parsed(&source), &source, &mut ScopeStack::new()).unwrap();
    let Value::Callable(c) = value else { panic!() };
    assert!(function(&c).family.type_parameters.is_empty());
    let closure = Arc::downgrade(&function(&c).family);
    let restored = restore_callable(snapshot_callable(&c).unwrap(), &mut 1_000_000).unwrap();
    let restored_weak = Arc::downgrade(&function(&restored).family);
    assert!(!Arc::ptr_eq(
        &function(&restored).family,
        &function(&c).family
    ));
    drop(restored);
    drop(c);
    assert!(closure.upgrade().is_none());
    assert!(restored_weak.upgrade().is_none());

    // Observe real temporary restored families, then fail the later whole-
    // capsule trailing-byte check while preserving the existing decoder order.
    let c = fresh(
        "def candidate[X: Equal](value: X) -> X { return value }",
        true,
    );
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "candidate",
            BindingMutability::Immutable,
            Value::Callable(Arc::clone(&c)),
        )
        .unwrap();
    let wire = crate::capsule::encode_background_capsule(
        "next.opaal",
        "",
        Path::new("/fixture"),
        &Environment::new(),
        None,
        &scope,
        crate::plan::SessionOptions::default(),
    )
    .unwrap();
    RESTORED_FAMILIES.with(|r| *r.borrow_mut() = Some(Vec::new()));
    let mut bad = wire.clone();
    bad.push(0);
    let length = (bad.len() - 18) as u64;
    bad[10..18].copy_from_slice(&length.to_le_bytes());
    let result = crate::capsule::decode_background_capsule(&bad);
    assert!(result.is_err());
    assert!(result.err().unwrap().to_string().contains("trailing bytes"));
    RESTORED_FAMILIES.with(|r| {
        let observed = r.borrow_mut().take().unwrap();
        assert!(!observed.is_empty());
        assert!(observed.iter().all(|weak| weak.upgrade().is_none()));
    });
    RESTORED_FAMILIES.with(|r| *r.borrow_mut() = Some(Vec::new()));
    let mut bad = snapshot_callable(&c).unwrap();
    bad.result_type = Some(ValueType::String);
    assert!(restore_callable(bad, &mut 1_000_000).is_err());
    RESTORED_FAMILIES.with(|r| assert!(r.borrow_mut().take().unwrap().is_empty()));
}

#[test]
fn generic_errors_stay_local_to_the_callable_view() {
    for (types, expected) in [
        (vec![], "expected 1 type arguments, found 0"),
        (
            vec![ValueType::Int, ValueType::Int],
            "expected 1 type arguments, found 2",
        ),
        (vec![ValueType::Any], "does not satisfy"),
    ] {
        let original = fresh(
            "def candidate[X: Equal](value: X) -> X { return value }",
            true,
        );
        let view: Arc<dyn Callable> = Arc::new(function(&original).clone());
        let mut errors = Vec::new();
        reset();
        for callable in [&original, &view, &original] {
            let c = function(callable);
            let mut host = ToggleHost::new();
            let mut budget = ResourceBudget::unlimited();
            let mut evaluator = Evaluator {
                source: Arc::clone(&c.family.source),
                binding_types: Arc::clone(&c.binding_types),
                current_result_type: None,
                current_type_arguments: BTreeMap::new(),
                budgeted_callback: false,
                standard_effects: None,
                cancel: CancellationToken::never(),
                budget: &mut budget,
                host: &mut host,
            };
            let error = evaluator
                .run_call(
                    callable,
                    c,
                    vec![RuntimeArgument {
                        value: Value::Int(7),
                        span: c.family.origin_span,
                    }],
                    c.family.origin_span,
                    Some(types.clone()),
                    None,
                )
                .unwrap_err();
            let Abort::Error(error) = error else {
                panic!("expected generic error")
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(error.span(), c.family.origin_span);
            assert_eq!(budget.used_steps, 0);
            assert!(host.events.borrow().is_empty());
            errors.push(error);
        }
        assert!(errors.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(count(), 1);
        assert_eq!(function(&original).family.type_parameters.len(), 1);
        assert!(function(&original).captured_type_arguments.is_empty());
    }
}

#[test]
fn restoration_refuses_forged_source_identity_and_context_before_family_creation() {
    let c = fresh("def candidate(value: Int) -> Int { return value }", true);
    let snapshot = snapshot_callable(&c).unwrap();
    let mut failures = Vec::new();
    let mut bad = snapshot.clone();
    bad.origin_span = SourceFile::new(SourceId::new(99), "same.opaal", bad.source.text())
        .span(4..13)
        .unwrap();
    failures.push(bad);
    let mut bad = snapshot.clone();
    bad.parameters[0].1 = ValueType::String;
    failures.push(bad);
    let mut bad = snapshot.clone();
    bad.type_context.cells[0].source = Arc::new(SourceFile::new(
        SourceId::new(51),
        "same.opaal",
        "def candidate(value: String) -> String { return value }",
    ));
    failures.push(bad);
    for bad in failures {
        RESTORED_FAMILIES.with(|families| *families.borrow_mut() = Some(Vec::new()));
        assert!(restore_callable(bad, &mut 1_000_000).is_err());
        RESTORED_FAMILIES
            .with(|families| assert!(families.borrow_mut().take().unwrap().is_empty()));
    }
}
