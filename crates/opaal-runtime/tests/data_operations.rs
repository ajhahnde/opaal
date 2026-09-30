#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::help::{ModuleHelpCatalog, ModuleHelpKind};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgram, ModuleProgramLoader,
    ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::{ScriptExecutionOutcome, execute_module_program_outcome_with_limits};
use opaal_runtime::{Environment, Value};

struct Sources(String);

impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(if module.path().file_name().unwrap() == "api.opaal" {
            b"import std::string as text\nexport { text }\n".to_vec()
        } else {
            self.0.as_bytes().to_vec()
        })
    }
}

struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("pure operations must not resolve executables")
    }
}

fn load(source: &str) -> ModuleProgram {
    let sources = Sources(source.to_owned());
    ModuleProgramLoader::new(&sources, &sources)
        .load(Path::new("/project/main.opaal"))
        .unwrap_or_else(|error| panic!("source must analyze: {error:?}\n{source}"))
}

fn execute(program: &ModuleProgram, budget: ResourceBudget) -> ScriptExecutionOutcome {
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
        &EvalLimits::pure_opaal(CancellationToken::never(), budget),
    )
}

fn value(source: &str) -> Value {
    let program = load(&format!("import std::string as string\n{source}"));
    match execute(&program, ResourceBudget::default()).primary() {
        PrimaryOutcome::Completed(completion) => completion.value().clone(),
        other => panic!("expected value: {other:?}"),
    }
}

#[test]
fn unicode_literal_operations_preserve_exact_text_and_empty_fields() {
    assert_eq!(
        value("string::trim(\"　 café\\r\\n \" )"),
        Value::string("café")
    );
    assert_eq!(value("string::trim(\" x  y \" )"), Value::string("x  y"));
    assert_eq!(value("string::trim(\"　 \" )"), Value::string(""));
    assert_eq!(
        value("string::split(\"/α//β/\", \"/\")"),
        Value::list(vec![
            Value::string(""),
            Value::string("α"),
            Value::string(""),
            Value::string("β"),
            Value::string("")
        ])
    );
    assert_eq!(
        value("string::split(\"\", \"::\")"),
        Value::list(vec![Value::string("")])
    );
    assert_eq!(
        value("string::join([\"\", \"é\", \"\"], \"::\")"),
        Value::string("::é::")
    );
    assert_eq!(value("string::join([], \"::\")"), Value::string(""));
    assert_eq!(
        value("string::replace(\"aaaaa\", \"aa\", \"$\\\\\")"),
        Value::string("$\\$\\a")
    );
    assert_eq!(
        value("string::replace(\"αβαβ\", \"αβ\", \"é\")"),
        Value::string("éé")
    );
    assert_eq!(
        value("string::replace(\"abc\", \"z\", \"long\")"),
        Value::string("abc")
    );
    for call in [
        "contains(\"abc\", \"\")",
        "starts_with(\"abc\", \"\")",
        "ends_with(\"abc\", \"\")",
        "contains(\"αβ\", \"β\")",
        "starts_with(\"αβ\", \"α\")",
        "ends_with(\"αβ\", \"β\")",
    ] {
        assert_eq!(value(&format!("string::{call}")), Value::Bool(true));
    }
    for call in [
        "contains(\"aaaaa\", \"aab\")",
        "starts_with(\"αβ\", \"β\")",
        "ends_with(\"αβ\", \"α\")",
        "ends_with(\"a\", \"long\")",
        "contains(\"é\", \"é\")",
    ] {
        assert_eq!(value(&format!("string::{call}")), Value::Bool(false));
    }
}

#[test]
fn complete_argument_metadata_resolves_through_aliases_and_help() {
    let program = load(
        "import std::string as string\nimport './api.opaal' as api\napi::text::replace(\"old\", \"old\", \"new\")",
    );
    let root = program.graph().root();
    let direct = program
        .resolve_operation(root, &["string", "replace"])
        .unwrap();
    let indirect = program
        .resolve_operation(root, &["api", "text", "replace"])
        .unwrap();
    assert_eq!(direct.id(), indirect.id());
    assert_eq!(direct.validate(), Ok(()));
    assert_eq!(direct.overloads()[0].parameters().len(), 3);
    assert_eq!(
        direct.signature_labels(),
        ["std::string::replace(input: String, pattern: String, replacement: String) -> String"]
    );
    assert!(!direct.supports_value_pipeline());
    let help = ModuleHelpCatalog::snapshot(&program)
        .query(root, "api::text::replace")
        .unwrap();
    assert_eq!(help.kind(), ModuleHelpKind::Operation);
    assert_eq!(
        help.operation().unwrap().signature_labels(),
        direct.signature_labels()
    );
    let source = program.sources().source(root).unwrap();
    let commands = standard_registry();
    let context = program
        .semantic_queries(&commands)
        .operation_signature_at(root, source.text().rfind("\"new\"").unwrap())
        .unwrap();
    assert_eq!(context.active_parameter(), 2);
    assert_eq!(
        context.operation().signature_labels(),
        direct.signature_labels()
    );
    assert!(
        matches!(execute(&program, ResourceBudget::default()).primary(), PrimaryOutcome::Completed(completion) if completion.value() == &Value::string("new"))
    );
}

#[test]
fn checker_rejects_wrong_arity_types_generics_and_implicit_pipeline_arguments() {
    for expression in [
        "string::trim()",
        "string::trim(\"a\", \"b\")",
        "string::split(\"a\")",
        "string::replace(\"a\", \"b\")",
        "string::join([1], \"/\")",
        "string::contains(\"x\", 1)",
        "string::decode_utf8(\"x\")",
        "string::trim[Int](\"x\")",
        "string::unknown(\"x\")",
        "\"x\" | string::trim",
        "\"x\" | string::split",
    ] {
        let sources = Sources(format!("import std::string as string\n{expression}"));
        let report =
            ModuleProgramLoader::new(&sources, &sources).analyze(Path::new("/project/main.opaal"));
        assert!(report.program().is_none(), "checker accepted {expression}");
        assert!(
            !report.issues().is_empty(),
            "missing diagnostic for {expression}"
        );
    }
}

#[test]
fn empty_separator_and_pattern_errors_are_catchable() {
    for expression in [
        "string::split(\"x\", \"\")",
        "string::replace(\"x\", \"\", \"y\")",
    ] {
        assert_eq!(
            value(&format!(
                "mut caught = false\ntry {{\n let result = {expression}\n}} catch error {{\n caught = true\n}}\ncaught"
            )),
            Value::Bool(true)
        );
    }
}

#[test]
fn dynamic_values_are_checked_in_every_argument_position_with_call_spans() {
    for expression in [
        "string::trim(dynamic(1))",
        "string::split(dynamic(1), \"/\")",
        "string::split(\"x\", dynamic(1))",
        "string::join(dynamic(1), \"/\")",
        "string::join(dynamic([\"x\", 1]), \"/\")",
        "string::join([], dynamic(1))",
        "string::contains(dynamic(1), \"x\")",
        "string::contains(\"x\", dynamic(1))",
        "string::starts_with(\"x\", dynamic(1))",
        "string::ends_with(\"x\", dynamic(1))",
        "string::replace(dynamic(1), \"x\", \"y\")",
        "string::replace(\"x\", dynamic(1), \"y\")",
        "string::replace(\"x\", \"x\", dynamic(1))",
        "string::decode_utf8(dynamic(1))",
    ] {
        let source = format!(
            "import std::string as string\ndef dynamic(value: Any) -> Any {{ return value }}\n{expression}"
        );
        let program = load(&source);
        let outcome = execute(&program, ResourceBudget::default());
        let PrimaryOutcome::Error(error) = outcome.primary() else {
            panic!("dynamic mismatch must fail: {expression}")
        };
        assert!(error.render().contains("main.opaal"));
        let caught_source = format!(
            "import std::string as string\ndef dynamic(value: Any) -> Any {{ return value }}\nmut location = []\ntry {{ let result = {expression} }} catch error {{ location = [error.source.start, error.source.end] }}\nlocation"
        );
        let start = caught_source.find(expression).unwrap();
        assert_eq!(
            match execute(&load(&caught_source), ResourceBudget::default()).primary() {
                PrimaryOutcome::Completed(completion) => completion.value().clone(),
                other => panic!("dynamic mismatch must be catchable: {other:?}"),
            },
            Value::list(vec![
                Value::Int(start as i64),
                Value::Int((start + expression.len()) as i64)
            ])
        );
    }
}

#[test]
fn argument_expressions_run_eagerly_in_source_order_before_type_validation() {
    for (expression, expected) in [
        (
            "string::replace(fail(\"first\"), fail(\"second\"), fail(\"third\"))",
            "first",
        ),
        (
            "string::replace(\"x\", fail(\"second\"), fail(\"third\"))",
            "second",
        ),
        (
            "string::replace(dynamic(1), \"x\", fail(\"third\"))",
            "third",
        ),
    ] {
        assert_eq!(
            value(&format!(
                "def dynamic(value: Any) -> Any {{ return value }}\ndef fail(message: String) -> String {{ throw message }}\nmut message = \"\"\ntry {{ let result = {expression} }} catch error {{ message = error.message }}\nmessage"
            )),
            Value::string(expected)
        );
    }
}

#[test]
fn growth_and_split_retention_share_the_caller_budget() {
    let program =
        load("import std::string as string\nstring::replace(\"aaa\", \"a\", \"0123456789\")");
    // The search prefix table is charged once (one usize), then 30 output bytes.
    let baseline = size_of::<usize>() as u64 + 30;
    let minimum = (baseline..baseline + 100)
        .find(|bytes| {
            matches!(
                execute(
                    &program,
                    ResourceBudget::default().with_collection_bytes(*bytes)
                )
                .primary(),
                PrimaryOutcome::Completed(_)
            )
        })
        .unwrap();
    assert!(matches!(
        execute(
            &program,
            ResourceBudget::default().with_collection_bytes(minimum - 1)
        )
        .primary(),
        PrimaryOutcome::Error(_)
    ));
    let program = load("import std::string as string\nstring::split(\"///\", \"/\")");
    assert!(matches!(
        execute(&program, ResourceBudget::default().with_collection_items(4)).primary(),
        PrimaryOutcome::Completed(_)
    ));
    assert!(matches!(
        execute(&program, ResourceBudget::default().with_collection_items(3)).primary(),
        PrimaryOutcome::Error(_)
    ));
}
