use super::super::*;
use crate::FiniteFloat;
use crate::module::{
    ModuleCanonicalizer, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use std::path::PathBuf;

struct Source(String);
impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Source {
    fn load(&self, _: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(self.0.as_bytes().to_vec())
    }
}

struct CountingHost {
    environment: Environment,
    reads: usize,
    operations: usize,
}
impl EvaluationHost for CountingHost {
    fn environment(&mut self) -> &mut Environment {
        self.reads += 1;
        self.environment.set("next", self.reads.to_string());
        &mut self.environment
    }
    fn current_status(&self) -> Option<&Status> {
        None
    }
    fn policy(&self) -> EvaluationPolicy {
        EvaluationPolicy::General
    }
    fn invoke_operational(
        &mut self,
        _: &ModuleId,
        _: &str,
        _: Vec<Value>,
        _: &mut ResourceBudget,
    ) -> Option<Result<Value, OperationalModuleError>> {
        self.operations += 1;
        panic!("math must not invoke an operational host")
    }
    fn read_directory(&mut self, _: &Path) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
        panic!("math must not read a directory")
    }
    fn execute_chain(
        &mut self,
        _: &ConditionalChain,
        _: &mut ScopeStack,
        _: EvaluationContext,
    ) -> Result<Status, Abort> {
        panic!("math must not execute a command")
    }
}

fn execute(body: &str) -> (Value, CountingHost) {
    let source = Source(format!("import std::math as math\n{body}"));
    let program = ModuleProgramLoader::new(&source, &source)
        .load(Path::new("/numeric/main.opaal"))
        .unwrap();
    let root = program.graph().root();
    let StatementKind::Job(job) = program
        .sources()
        .script(root)
        .unwrap()
        .statements()
        .last()
        .unwrap()
        .kind()
    else {
        panic!("expression fixture")
    };
    let expression = crate::module::single_value_expression(&job.chain).unwrap();
    let mut host = CountingHost {
        environment: Environment::new(),
        reads: 0,
        operations: 0,
    };
    let mut budget = ResourceBudget::default();
    let mut evaluator = Evaluator {
        source: Arc::new(program.sources().source(root).unwrap().clone()),
        binding_types: Arc::new(program.runtime_binding_types()),
        current_result_type: None,
        current_type_arguments: BTreeMap::new(),
        budgeted_callback: false,
        standard_effects_allowed: true,
        cancel: CancellationToken::never(),
        budget: &mut budget,
        host: &mut host,
    };
    let value = evaluator
        .expression(expression, &mut ScopeStack::new())
        .unwrap_or_else(|error| match error {
            Abort::Error(error) => panic!("numeric call failed: {error:?}"),
            _ => panic!("numeric call was interrupted"),
        });
    (value, host)
}

#[test]
fn tuple_selection_evaluates_each_argument_once_in_source_order() {
    let (value, host) = execute(
        "math::clamp(({'1': 1, '2': 2, '3': 3})[env('next')], ({'1': 1, '2': 2, '3': 3})[env('next')], ({'1': 1, '2': 2, '3': 3})[env('next')])",
    );
    assert_eq!(value, Value::Int(2));
    assert_eq!(host.reads, 3);
    assert_eq!(host.operations, 0);
    let (value, host) = execute(
        "math::min(({'1': 1, '2': 2, '3': 3})[env('next')], ({'1': 1, '2': 2, '3': 3})[env('next')])",
    );
    assert_eq!(value, Value::Int(1));
    assert_eq!(host.reads, 2);
}

#[test]
fn pure_numeric_bodies_have_zero_host_calls() {
    for body in [
        "math::abs(-3)",
        "math::min(1, 2)",
        "math::max(1.0, 2.0)",
        "math::clamp(1, 0, 2)",
        "math::floor(2.7)",
        "math::ceil(2.1)",
        "math::round(0.5)",
        "math::sqrt(81.0)",
    ] {
        let (_, host) = execute(body);
        assert_eq!(host.reads, 0);
        assert_eq!(host.operations, 0);
    }
}

#[test]
fn reconstructed_numeric_callbacks_preserve_aliases_and_generic_dispatch() {
    for (body, expected) in [
        (
            "def callback[T](x: T) -> T { return math::abs(x) }\nlist::map[Int, Int]([-2, 3], callback)",
            Value::list(vec![Value::Int(2), Value::Int(3)]),
        ),
        (
            "let callback = {|x: Float| -> Float math::sqrt(x)}\nlist::map[Float, Float]([4.0, 9.0], callback)",
            Value::list(vec![
                Value::Float(FiniteFloat::new(2.0).unwrap()),
                Value::Float(FiniteFloat::new(3.0).unwrap()),
            ]),
        ),
    ] {
        let source = Source(format!(
            "import std::math as math\nimport std::list as list\n{body}"
        ));
        let program = ModuleProgramLoader::new(&source, &source)
            .load(Path::new("/numeric/main.opaal"))
            .unwrap();
        let root = program.graph().root();
        let script = program.sources().script(root).unwrap();
        let source = Arc::new(program.sources().source(root).unwrap().clone());
        let types = Arc::new(program.runtime_binding_types());
        let mut scope = ScopeStack::new();
        assert!(matches!(
            evaluate_in_environment_owned_with_binding_types(
                script,
                Arc::clone(&source),
                &mut scope,
                &mut Environment::new(),
                &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
                Arc::clone(&types),
            )
            .unwrap(),
            Completion::Value(value) if value == expected
        ));
        let wire = crate::capsule::encode_background_capsule(
            "next.opaal",
            "",
            Path::new("/numeric"),
            &Environment::new(),
            None,
            &scope,
            crate::plan::SessionOptions::default(),
        )
        .unwrap();
        let restored = crate::capsule::decode_background_capsule(&wire).unwrap();
        let StatementKind::Job(job) = script.statements().last().unwrap().kind() else {
            panic!("callback expression")
        };
        let mut host = CountingHost {
            environment: Environment::new(),
            reads: 0,
            operations: 0,
        };
        let mut budget = ResourceBudget::default();
        let mut evaluator = Evaluator {
            source,
            binding_types: types,
            current_result_type: None,
            current_type_arguments: BTreeMap::new(),
            budgeted_callback: false,
            standard_effects_allowed: true,
            cancel: CancellationToken::never(),
            budget: &mut budget,
            host: &mut host,
        };
        let result = evaluator.expression(
            crate::module::single_value_expression(&job.chain).unwrap(),
            &mut restored.scope().clone(),
        );
        assert!(matches!(result, Ok(value) if value == expected), "{body}");
        // The existing list callback host obtains the caller's environment view.
        assert_eq!(host.operations, 0);
    }
}

#[test]
fn scalar_admission_charges_call_and_candidates_before_domain_evaluation() {
    let source = Source("import std::math as math".to_owned());
    let program = ModuleProgramLoader::new(&source, &source)
        .load(Path::new("/numeric/main.opaal"))
        .unwrap();
    let root = program.graph().root();
    let source = Arc::new(program.sources().source(root).unwrap().clone());
    let types = Arc::new(program.runtime_binding_types());
    let float = |value| Value::Float(FiniteFloat::new(value).unwrap());
    for (name, arguments, expected, steps) in [
        ("abs", vec![Value::Int(-1)], Some(Value::Int(1)), 3),
        (
            "min",
            vec![Value::Int(1), Value::Int(2)],
            Some(Value::Int(1)),
            3,
        ),
        (
            "max",
            vec![Value::Int(1), Value::Int(2)],
            Some(Value::Int(2)),
            3,
        ),
        (
            "clamp",
            vec![Value::Int(3), Value::Int(0), Value::Int(2)],
            Some(Value::Int(2)),
            3,
        ),
        ("floor", vec![float(-0.5)], Some(float(-1.0)), 2),
        ("ceil", vec![float(-0.5)], Some(float(0.0)), 2),
        ("round", vec![float(-0.5)], Some(float(-1.0)), 2),
        ("sqrt", vec![float(4.0)], Some(float(2.0)), 2),
        ("sqrt", vec![float(-1.0)], None, 2),
    ] {
        let operation = types
            .qualified_operation(source.id(), &["math", name])
            .unwrap();
        for limit in 0..=steps {
            let mut host = CountingHost {
                environment: Environment::new(),
                reads: 0,
                operations: 0,
            };
            let mut budget = ResourceBudget::steps(limit)
                .with_collection_items(0)
                .with_collection_bytes(0);
            let mut evaluator = Evaluator {
                source: Arc::clone(&source),
                binding_types: Arc::clone(&types),
                current_result_type: None,
                current_type_arguments: BTreeMap::new(),
                budgeted_callback: false,
                standard_effects_allowed: true,
                cancel: CancellationToken::never(),
                budget: &mut budget,
                host: &mut host,
            };
            let result = evaluator.execute_operation(
                &operation,
                arguments.clone(),
                &[],
                source.span(0..source.text().len()).unwrap(),
            );
            if limit < steps {
                assert!(matches!(result, Err(Abort::Error(error))
                    if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded)));
            } else if let Some(expected) = &expected {
                assert!(matches!(result, Ok(value) if &value == expected));
            } else {
                assert!(matches!(result, Err(Abort::Error(error))
                    if matches!(error.kind(), RuntimeErrorKind::Operation(OperationError::InvalidArgument { .. }))));
            }
            assert_eq!(budget.used(), limit);
            assert_eq!(budget.collection_items(), 0);
            assert_eq!(budget.collection_bytes(), 0);
            assert_eq!(host.reads, 0);
            assert_eq!(host.operations, 0);
        }
    }
}
