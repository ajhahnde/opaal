use super::super::*;
use crate::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EffectSet, EvaluationContextId,
    RequiredEnforcement,
};
use crate::context::OperationalContext;
use crate::module::{
    ModuleCanonicalizer, ModulePathError, ModuleProgram, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use crate::operational::standard::{StandardLimits, StandardState};
use opaal_platform::standard_host::{FillError, StandardHost};
use opaal_platform::{AuthorityProfile, Capabilities, FakePlatform};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Source(String);
impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(if module.path().file_name().unwrap() == "api.opaal" {
            b"import std::random as rng\nexport { rng }\n".to_vec()
        } else {
            self.0.as_bytes().to_vec()
        })
    }
}
fn program(body: &str) -> ModuleProgram {
    let source = Source(format!("import std::random as random\n{body}"));
    ModuleProgramLoader::new(&source, &source)
        .load(Path::new("/random/main.opaal"))
        .unwrap()
}

struct Entropy {
    bytes: VecDeque<u8>,
    fills: Arc<AtomicUsize>,
}
impl StandardHost for Entropy {
    fn evaluation(&self) -> u64 {
        71
    }
    fn available(&self) -> bool {
        true
    }
    fn fill(
        &mut self,
        destination: &mut [u8],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), FillError> {
        assert!(!cancelled());
        assert!(destination.len() <= 256);
        self.fills.fetch_add(1, Ordering::Relaxed);
        for byte in destination {
            *byte = self.bytes.pop_front().expect("scripted entropy");
        }
        Ok(())
    }
    fn close(&mut self) -> Result<(), opaal_platform::operational::OperationalError> {
        Ok(())
    }
}
struct Host {
    environment: Environment,
    context: OperationalContext,
    random: StandardState,
    platform: FakePlatform,
    fills: Arc<AtomicUsize>,
    calls: usize,
    reads: usize,
    wrong_result: bool,
    policy: EvaluationPolicy,
}
impl Host {
    fn new(words: &[u64], tail: &[u8]) -> Self {
        let request = CapabilityRequest::entropy_system();
        let fills = Arc::new(AtomicUsize::new(0));
        Self {
            environment: Environment::new(),
            context: OperationalContext::new(
                AuthorityContext::new(
                    EvaluationContextId::new(71).unwrap(),
                    [AuthorityRule::grant(request, RequiredEnforcement::Enforced)],
                )
                .unwrap(),
                CancellationToken::never(),
                Arc::new(FakeClock::new()),
                None,
            ),
            random: StandardState::new(
                Some(Box::new(Entropy {
                    bytes: words
                        .iter()
                        .flat_map(|word| word.to_be_bytes())
                        .chain(tail.iter().copied())
                        .collect(),
                    fills: fills.clone(),
                })),
                StandardLimits::default(),
            )
            .unwrap(),
            platform: FakePlatform::with_authority_profile(
                Capabilities::full(),
                AuthorityProfile::enforced(),
            ),
            fills,
            calls: 0,
            reads: 0,
            wrong_result: false,
            policy: EvaluationPolicy::General,
        }
    }
}
impl EvaluationHost for Host {
    fn environment(&mut self) -> &mut Environment {
        self.reads += 1;
        self.environment.set("next", self.reads.to_string());
        &mut self.environment
    }
    fn current_status(&self) -> Option<&Status> {
        None
    }
    fn policy(&self) -> EvaluationPolicy {
        self.policy
    }
    fn permits_controlled_action(&self) -> bool {
        true
    }
    fn invoke_operational(
        &mut self,
        module: &ModuleId,
        operation: &str,
        arguments: Vec<Value>,
        budget: &mut ResourceBudget,
    ) -> Option<Result<Value, OperationalModuleError>> {
        assert_eq!(module.path(), Path::new("std::random"));
        self.calls += 1;
        if self.wrong_result {
            return Some(Ok(Value::string("invalid host result")));
        }
        Some(self.random.invoke(
            &self.context,
            &EffectSet::new([CapabilityRequest::entropy_system()]),
            &self.platform,
            budget,
            operation,
            &arguments,
        ))
    }
    fn read_directory(&mut self, _: &Path) -> Result<Box<dyn DirectoryStream>, RuntimeErrorKind> {
        panic!("unexpected directory read")
    }
    fn execute_chain(
        &mut self,
        _: &ConditionalChain,
        _: &mut ScopeStack,
        _: EvaluationContext,
    ) -> Result<Status, Abort> {
        panic!("unexpected process")
    }
}
fn run(
    body: &str,
    host: &mut Host,
    budget: &mut ResourceBudget,
) -> Result<HostedEvaluationOutcome, HostedEvaluationFailure> {
    let program = program(body);
    let root = program.graph().root();
    evaluate_with_host_and_budget(
        program.sources().script(root).unwrap(),
        Arc::new(program.sources().source(root).unwrap().clone()),
        &mut ScopeStack::new(),
        &EvalLimits::default(),
        budget,
        Arc::new(program.runtime_binding_types()),
        host,
    )
}
fn value(body: &str, host: &mut Host) -> Value {
    match run(body, host, &mut ResourceBudget::opaal()) {
        Ok(HostedEvaluationOutcome::Value(value)) => value,
        Err(HostedEvaluationFailure::Runtime(error)) => panic!("unexpected runtime error: {error}"),
        _ => panic!("unexpected outcome"),
    }
}

#[test]
fn scripted_source_calls_preserve_typed_results_and_exact_sampling() {
    let mut host = Host::new(&[0, 5, u64::MAX], &[0, 1, 2]);
    let result = value(
        "def keep[T](x: T) -> T { return x }\nlet shard = random::int(0, 3)\nlet fraction = keep(random::float())\nlet identifier = random::bytes(3)\nlet empty = random::bytes(0)\nlet single = random::int(7, 8)\n[shard, fraction, identifier, empty, single]",
        &mut host,
    );
    assert_eq!(
        result,
        Value::list(vec![
            Value::Int(2),
            Value::Float(crate::FiniteFloat::new(1.0 - 2.0_f64.powi(-53)).unwrap()),
            Value::bytes(vec![0, 1, 2]),
            Value::bytes(vec![]),
            Value::Int(7)
        ])
    );
    assert_eq!(host.calls, 5);
    assert_eq!(host.fills.load(Ordering::Relaxed), 4);
}

#[test]
fn sampler_uses_the_live_evaluator_budget_and_preserves_resource_errors() {
    let mut host = Host::new(&[], &[1, 2, 3]);
    let mut budget = ResourceBudget::opaal();
    value("random::bytes(0)", &mut host);
    assert!(run("random::bytes(3)", &mut host, &mut budget).is_ok());
    assert_eq!(budget.collection_bytes(), 3);
    let consumed = budget.used();
    assert!(consumed > 3);
    let mut host = Host::new(&[], &[]);
    let outcome = run(
        "random::bytes(3)",
        &mut host,
        &mut ResourceBudget::opaal().with_collection_bytes(0),
    );
    assert!(
        matches!(outcome, Err(HostedEvaluationFailure::Runtime(ref error)) if matches!(error.kind(), RuntimeErrorKind::ResourceBudgetExceeded))
    );
    assert_eq!(host.fills.load(Ordering::Relaxed), 0);
    assert_eq!(host.calls, 1);
}

#[test]
fn pure_functions_callbacks_empty_actions_aliases_and_startup_never_invoke_the_host() {
    for body in [
        "def pure() -> Int { return random::int(7, 8) }\npure()",
        "let callback = {|| random::bytes(0)}\ncallback()",
        "action empty() -> Int effects {} { return random::int(7, 8) }\nempty()",
        "let alias = random::int\nalias(7, 8)",
        "import std::list as list\nlist::map[Int, Int]([1], {|x| random::int(7, 8)})",
    ] {
        let mut host = Host::new(&[], &[]);
        assert!(
            matches!(
                run(body, &mut host, &mut ResourceBudget::opaal()),
                Ok(HostedEvaluationOutcome::Refused(_)) | Err(HostedEvaluationFailure::Runtime(_))
            ),
            "{body}"
        );
        assert_eq!(host.calls, 0, "{body}");
        assert_eq!(host.fills.load(Ordering::Relaxed), 0);
    }
    for policy in [EvaluationPolicy::Startup, EvaluationPolicy::PureOpaal] {
        let mut host = Host::new(&[], &[]);
        host.policy = policy;
        assert!(matches!(
            run(
                "let empty = random::bytes(0)",
                &mut host,
                &mut ResourceBudget::opaal()
            ),
            Ok(HostedEvaluationOutcome::Refused(_))
        ));
        assert_eq!(host.calls, 0);
    }
}

#[test]
fn declared_action_uses_the_same_operation_and_rejects_a_wrong_host_result() {
    let mut host = Host::new(&[], &[]);
    assert_eq!(
        value(
            "action sample() -> Int effects { entropy.system(); } { return random::int(7, 8) }\nsample()",
            &mut host
        ),
        Value::Int(7)
    );
    host.wrong_result = true;
    let outcome = run("random::float()", &mut host, &mut ResourceBudget::opaal());
    assert!(
        matches!(outcome, Err(HostedEvaluationFailure::Runtime(ref error)) if matches!(error.kind(), RuntimeErrorKind::FunctionResultTypeMismatch { expected: ValueType::Float, .. }))
    );
    assert_eq!(host.fills.load(Ordering::Relaxed), 0);
}

#[test]
fn whole_tuple_validation_evaluates_arguments_once_in_order_before_host_dispatch() {
    let mut host = Host::new(&[], &[]);
    assert_eq!(
        value(
            "random::int(({'1': 1, '2': 2})[env('next')], ({'1': 1, '2': 2})[env('next')])",
            &mut host
        ),
        Value::Int(1)
    );
    assert_eq!(host.reads, 2);
    assert_eq!(host.calls, 1);
    let mut host = Host::new(&[], &[]);
    let outcome = run(
        "mut count: Any = '3'\nrandom::bytes(count)",
        &mut host,
        &mut ResourceBudget::opaal(),
    );
    assert!(
        matches!(outcome, Err(HostedEvaluationFailure::Runtime(ref error)) if matches!(error.kind(), RuntimeErrorKind::Operation(OperationError::NoMatchingOverload { .. })))
    );
    assert_eq!(host.calls, 0);
}

#[test]
fn reexports_preserve_host_identity_and_language_catches_do_not_restore_entropy() {
    let mut host = Host::new(&[], &[0, 1]);
    let result = value(
        "import './api.opaal' as api\ntry { let discarded = api::rng::bytes(1); throw 'domain' } catch error { 0 }\napi::rng::bytes(1)",
        &mut host,
    );
    assert_eq!(result, Value::bytes(vec![1]));
    assert_eq!(host.random.consumed_bytes(), 2);
    assert_eq!(host.fills.load(Ordering::Relaxed), 2);
}
