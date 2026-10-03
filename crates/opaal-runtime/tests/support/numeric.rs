#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgram, ModuleProgramError,
    ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::{ScriptExecutionOutcome, execute_module_program_outcome_with_limits};
use opaal_runtime::session::{Session, SubmitError, SubmitOutcome};
use opaal_runtime::{BindingMutability, Environment, FiniteFloat, ScopeStack, Value};

pub struct Source(pub String);
impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(match module.path().file_name().unwrap().to_str().unwrap() {
            "api.opaal" => b"import std::math as math\nexport { math }\n".to_vec(),
            "report.opaal" => {
                include_bytes!("../../../../examples/numeric-report/report.opaal").to_vec()
            }
            _ => self.0.as_bytes().to_vec(),
        })
    }
}
pub struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("pure math must not probe a host executable")
    }
}
pub fn try_load(body: &str) -> Result<ModuleProgram, ModuleProgramError> {
    let source = Source(format!("import std::math as math\n{body}"));
    ModuleProgramLoader::new(&source, &source).load(Path::new("/project/main.opaal"))
}
pub fn load(body: &str) -> ModuleProgram {
    try_load(body).unwrap_or_else(|error| panic!("{error:?}\n{body}"))
}
pub fn run(
    program: &ModuleProgram,
    budget: ResourceBudget,
    cancel: CancellationToken,
) -> ScriptExecutionOutcome {
    execute_module_program_outcome_with_limits(
        program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::full(),
        Arc::new(FakeClock::new()),
        &mut Vec::new(),
        &EvalLimits::pure_opaal(cancel, budget),
    )
}
pub fn completed(outcome: &ScriptExecutionOutcome) -> Value {
    match outcome.primary() {
        PrimaryOutcome::Completed(completion) => completion.value().clone(),
        other => panic!("expected value: {other:?}"),
    }
}
pub fn value(body: &str) -> Value {
    completed(&run(
        &load(body),
        ResourceBudget::default(),
        CancellationToken::never(),
    ))
}
pub fn float(value: f64) -> Value {
    Value::Float(FiniteFloat::new(value).unwrap())
}
pub fn session_run(body: &str, input: Value) -> Result<Value, SubmitError> {
    let mut scope = ScopeStack::new();
    scope
        .declare("input", BindingMutability::Immutable, input)
        .unwrap();
    let mut session = Session::with_scope(
        scope,
        "/project",
        Environment::new(),
        SessionOptions::default(),
    );
    let source = Source(format!("import std::math as math\n{body}"));
    let (outcome, value) = session.submit_with_source_loader_and_limits(
        "main.opaal",
        &source.0,
        &source,
        &source,
        &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
        &NoExecutables,
        &FakePlatform::full(),
        &FakeClock::new(),
        &mut Vec::new(),
    )?;
    assert_eq!(outcome, SubmitOutcome::Continued);
    Ok(value)
}
