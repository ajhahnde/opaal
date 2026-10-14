#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use opaal_runtime::Value;
use opaal_runtime::eval::{CancellationToken, Completion, ResourceBudget, RuntimeErrorKind};
use opaal_runtime::module::{
    AnalysisControl, ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgram,
    ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::script::invoke_pure_function;

struct Source(String);
impl ModuleCanonicalizer for Source {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}
impl ModuleSourceLoader for Source {
    fn load(&self, _: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(self.0.as_bytes().to_vec())
    }
}

fn program(source: &str) -> ModuleProgram {
    let source = Source(source.to_owned());
    ModuleProgramLoader::new(&source, &source)
        .load_for_frontend(Path::new("/pure.opaal"))
        .unwrap_or_else(|error| panic!("{}", error.render()))
}

#[test]
fn initializer_and_named_call_share_the_exact_step_budget() {
    let program = program(
        "export { test_one }\nlet initial = [1, 2]\ndef test_one() -> Bool { return initial[0] == 1 }\n",
    );
    let (module, signature) = program.exported_function("test_one").unwrap();
    let mut budget = ResourceBudget::opaal();
    assert!(matches!(
        invoke_pure_function(
            &program,
            &module,
            signature,
            CancellationToken::never(),
            &mut budget
        ),
        Ok(Completion::Value(Value::Bool(true)))
    ));
    let exact = budget.used();
    assert!(exact > 1);
    for _ in 0..2 {
        let mut budget = ResourceBudget::steps(exact).with_collection_items(2);
        assert!(matches!(
            invoke_pure_function(
                &program,
                &module,
                signature,
                CancellationToken::never(),
                &mut budget
            ),
            Ok(Completion::Value(Value::Bool(true)))
        ));
        assert_eq!(budget.used(), exact);
        assert_eq!(budget.collection_items(), 2);
    }
    let mut budget = ResourceBudget::steps(exact - 1);
    let error = invoke_pure_function(
        &program,
        &module,
        signature,
        CancellationToken::never(),
        &mut budget,
    )
    .unwrap_err();
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::ResourceBudgetExceeded
    ));
}

#[test]
fn runtime_denial_remains_when_the_static_frontend_is_bypassed() {
    for operation in [
        "env('PRIVATE')",
        "glob('*')",
        "random::bytes(0)",
        "io::read_stdin(0)",
    ] {
        let program = program(&format!(
            "import std::random as random\nimport std::io as io\nexport {{ test_one }}\ndef test_one() -> Bool {{ let denied = {operation}\nreturn true }}\n"
        ));
        let (module, signature) = program.exported_function("test_one").unwrap();
        assert!(
            !program
                .function_effects(&module, signature, &AnalysisControl::never())
                .occurrences()
                .is_empty()
        );
        let error = invoke_pure_function(
            &program,
            &module,
            signature,
            CancellationToken::never(),
            &mut ResourceBudget::opaal(),
        )
        .unwrap_err();
        assert!(
            matches!(error.kind(), RuntimeErrorKind::PolicyRefused { .. }),
            "{operation}: {error}"
        );
    }
}

#[test]
fn initializer_denial_and_cancellation_do_not_enter_the_case() {
    let program = program(
        "export { test_one }\nlet denied = env('PRIVATE')\ndef test_one() -> Bool { throw 'must-not-enter' }\n",
    );
    let (module, signature) = program.exported_function("test_one").unwrap();
    let error = invoke_pure_function(
        &program,
        &module,
        signature,
        CancellationToken::never(),
        &mut ResourceBudget::opaal(),
    )
    .unwrap_err();
    assert!(matches!(
        error.kind(),
        RuntimeErrorKind::PolicyRefused { .. }
    ));
    assert!(matches!(
        invoke_pure_function(
            &program,
            &module,
            signature,
            CancellationToken::from_fn(|| true),
            &mut ResourceBudget::opaal()
        ),
        Ok(Completion::Cancelled(_))
    ));
}

#[test]
fn caught_depth_error_retains_ordinary_error_semantics() {
    let program = program(
        "export { test_one }\ndef recurse() -> Bool { return recurse() }\ndef test_one() -> Bool { try { let value = recurse() } catch error { return error.category == 'resource' }\nreturn false }\n",
    );
    let (module, signature) = program.exported_function("test_one").unwrap();
    let mut budget = ResourceBudget::steps(1000).with_call_depth(8);
    assert!(matches!(
        invoke_pure_function(
            &program,
            &module,
            signature,
            CancellationToken::never(),
            &mut budget
        ),
        Ok(Completion::Value(Value::Bool(true)))
    ));
    assert!(budget.used() > 8);
    assert_eq!(budget.peak_call_depth(), 8);
}
