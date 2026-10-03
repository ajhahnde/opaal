#![no_main]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use libfuzzer_sys::fuzz_target;
use opaal_platform::FakePlatform;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::session::{Session, SubmitError, SubmitOutcome};
use opaal_runtime::{BindingMutability, Environment, ScopeStack, Value};

struct Source(String);

impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        (path == Path::new("/fuzz/main.opaal"))
            .then(|| path.to_path_buf())
            .ok_or_else(|| ModulePathError::new("local imports are unavailable"))
    }
}

impl ModuleSourceLoader for Source {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        (module.path() == Path::new("/fuzz/main.opaal"))
            .then(|| self.0.as_bytes().to_vec())
            .ok_or_else(|| ModuleSourceError::new("local imports are unavailable"))
    }
}

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("pure numeric operations must not resolve executables")
    }
}

fn execute(source: &Source, input: Value, limits: EvalLimits) -> PrimaryOutcome<Value, String> {
    let mut scope = ScopeStack::new();
    scope
        .declare(
            "input",
            BindingMutability::Immutable,
            input,
        )
        .unwrap();
    let mut session = Session::with_scope(
        scope,
        "/fuzz",
        Environment::new(),
        SessionOptions::default(),
    );
    match session.submit_with_source_loader_and_limits(
        "main.opaal",
        &source.0,
        source,
        source,
        &limits,
        &NoExecutables,
        &FakePlatform::full(),
        &FakeClock::new(),
        &mut Vec::new(),
    ) {
        Ok((SubmitOutcome::Continued, value)) => PrimaryOutcome::Completed(value),
        Ok((SubmitOutcome::Cancelled(reason), _)) => PrimaryOutcome::Cancelled(reason),
        Err(SubmitError::Runtime { rendered, .. } | SubmitError::Diagnostic(rendered)) => PrimaryOutcome::Error(rendered),
        other => panic!(
            "generated pure source must analyze without host effects: {other:?}\n{}",
            source.0
        ),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut bytes = [0_u8; 32];
    let count = data.len().min(bytes.len());
    bytes[..count].copy_from_slice(&data[..count]);
    let mut values = Vec::new();
    for index in 0..3 {
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
        let bits = u64::from_le_bytes(raw);
        let value = match bytes[24 + index] % 3 {
            0 => Value::Int(bits as i64),
            1 => match opaal_runtime::FiniteFloat::new(f64::from_bits(bits)) {
                Ok(value) => Value::Float(value),
                Err(_) => { assert!(!f64::from_bits(bits).is_finite()); Value::Null }
            },
            _ => Value::string("wrong numeric family"),
        };
        values.push(value);
    }
    let names = ["abs", "min", "max", "clamp", "floor", "ceil", "round", "sqrt"];
    let operation = usize::from(bytes[27] % 8);
    let arity = match operation { 1 | 2 => 2, 3 => 3, _ => 1 };
    let args = (0..arity).map(|index| format!("input[{index}]")).collect::<Vec<_>>().join(", ");
    let source = Source(format!("import std::math as math\nmath::{}({args})", names[operation]));
    let input = Value::list(values.clone());
    let generous = execute(&source, input.clone(), EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()));
    if let PrimaryOutcome::Completed(Value::Float(value)) = &generous {
        assert!(value.get().is_finite());
        if value.get() == 0.0 { assert_eq!(value.get().to_bits(), 0); }
    }
    if let PrimaryOutcome::Completed(_) = &generous {
        let family = if operation >= 4 { "float" } else { values[0].family_name() };
        assert!(family == "int" || family == "float");
        assert!(values[..arity].iter().all(|value| value.family_name() == family));
    }
    let limited = execute(&source, input.clone(), EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::steps(u64::from(bytes[28])).with_collection_items(0).with_collection_bytes(0)));
    if let PrimaryOutcome::Completed(value) = limited {
        let PrimaryOutcome::Completed(expected) = generous else { panic!("limited budget fabricated success") };
        assert_eq!(value, expected);
    }
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = polls.clone();
    let stop = usize::from(bytes[29] % 16);
    let cancel = CancellationToken::from_fn(move || seen.fetch_add(1, Ordering::SeqCst) >= stop);
    let _ = execute(&source, input, EvalLimits::pure_opaal(cancel, ResourceBudget::default()));
});
