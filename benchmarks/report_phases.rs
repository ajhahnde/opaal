
mod callable_classification {
    use super::*;
    fn parsed(source: &SourceFile) -> Script {
        let ParseOutcome::Complete(script) = parse_opaal(source) else {
            panic!("fixture")
        };
        script
    }
    fn constructed(
        source: Arc<SourceFile>,
        ast: &FunctionDefinition,
        types: Arc<RuntimeBindingTypes>,
    ) -> Arc<dyn Callable> {
        let mut env = Environment::new();
        let mut host = PureEvaluationHost {
            environment: &mut env,
            policy: EvaluationPolicy::General,
        };
        let mut budget = ResourceBudget::unlimited();
        let evaluator = Evaluator {
            source,
            binding_types: types,
            current_result_type: None,
            current_type_arguments: BTreeMap::new(),
            budgeted_callback: false,
            cancel: CancellationToken::never(),
            budget: &mut budget,
            host: &mut host,
        };
        let mut scope = ScopeStack::new();
        assert!(evaluator.function_definition(ast, &mut scope).is_ok());
        let Value::Callable(c) = scope.get("candidate").unwrap() else {
            panic!()
        };
        Arc::clone(c)
    }
    fn fixture(
        padding: usize,
        parameter: bool,
    ) -> (
        Arc<SourceFile>,
        FunctionDefinition,
        Arc<RuntimeBindingTypes>,
    ) {
        let text = format!(
            "def candidate({}) -> Int {{ return {} }}\n#{}\n",
            if parameter { "value: Int" } else { "" },
            if parameter { "value" } else { "7" },
            "x".repeat(padding)
        );
        let source = Arc::new(SourceFile::new(
            SourceId::new(12),
            "restore-phase.opaal",
            text,
        ));
        let script = parsed(&source);
        let StatementKind::Function(ast) = script.statements()[0].kind() else {
            panic!()
        };
        let types = RuntimeBindingTypes::analyze_repl_source(
            &source,
            &script,
            &crate::module::ModuleAliasRegistry::default(),
        )
        .unwrap();
        (source, ast.clone(), Arc::new(types))
    }
    #[test]
    #[ignore = "optimized uninstrumented restore measurement"]
    fn measure_restore_phase() {
        let padding: usize = std::env::var("REPORT_PADDING").unwrap().parse().unwrap();
        let repeats: usize = std::env::var("REPORT_REPEATS").unwrap().parse().unwrap();
        let (source, ast, types) = fixture(padding, false);
        let callable = constructed(source, &ast, types);
        let snapshot = snapshot_callable(&callable).unwrap();
        let mut retained = Vec::with_capacity(repeats);
        for _ in 0..repeats {
            retained.push(restore_callable(snapshot.clone(), &mut 1_000_000).unwrap());
        }
        assert_eq!(retained.len(), repeats);
        std::hint::black_box(retained);
        println!("PHASE restore padding={padding} repeats={repeats}");
    }
    #[test]
    #[ignore = "optimized uninstrumented clone/retention measurement"]
    fn measure_first_use_phase() {
        let mode = std::env::var("REPORT_MODE").unwrap();
        let repeats: usize = std::env::var("REPORT_REPEATS").unwrap().parse().unwrap();
        let (source, ast, types) = fixture(0, true);
        let callable = constructed(Arc::clone(&source), &ast, Arc::clone(&types));
        if mode == "originals" || mode == "clones" {
            let mut retained = Vec::with_capacity(repeats);
            for _ in 0..repeats {
                let value: Arc<dyn Callable> = if mode == "originals" {
                    constructed(Arc::clone(&source), &ast, Arc::clone(&types))
                } else {
                    Arc::new(
                        callable
                            .as_any()
                            .downcast_ref::<CallableValue>()
                            .unwrap()
                            .clone(),
                    )
                };
                retained.push(value);
            }
            assert_eq!(retained.len(), repeats);
            std::hint::black_box(retained);
        } else {
            let mut env = Environment::new();
            let mut host = PureEvaluationHost {
                environment: &mut env,
                policy: EvaluationPolicy::General,
            };
            let mut budget = ResourceBudget::unlimited();
            let mut evaluator = Evaluator {
                source,
                binding_types: types,
                current_result_type: None,
                current_type_arguments: BTreeMap::new(),
                budgeted_callback: false,
                cancel: CancellationToken::never(),
                budget: &mut budget,
                host: &mut host,
            };
            let descriptor = crate::operation::standard_operation(
                &crate::module::ModuleId::standard("std", "list"),
                "map",
            )
            .unwrap();
            let substitutions = BTreeMap::from([
                ("T".to_owned(), ValueType::Int),
                ("U".to_owned(), ValueType::Int),
            ]);
            for _ in 0..repeats {
                let items = if mode == "empty-preparation" {
                    vec![]
                } else {
                    vec![Value::Int(7)]
                };
                let expected = Value::list(items.clone());
                let result = evaluator.list_operation(
                    &descriptor,
                    &[Value::list(items), Value::Callable(Arc::clone(&callable))],
                    &substitutions,
                    ast.name.span(),
                );
                assert!(matches!(result,Ok(value) if value==expected));
            }
            std::hint::black_box(budget);
        }
        println!("PHASE first-use mode={mode} repeats={repeats}");
    }
}
