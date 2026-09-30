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
            b"import std::string as text\nimport std::record as records\nexport { text, records }\n"
                .to_vec()
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

fn record_value(source: &str) -> Value {
    value(&format!("import std::record as record\n{source}"))
}

#[test]
fn record_keys_presence_and_defaults_preserve_order_null_and_exact_unicode() {
    assert_eq!(
        record_value("record::keys({ z: 1, '': 2, 'é': null, 'é': 3 })"),
        Value::list(vec![
            Value::string("z"),
            Value::string(""),
            Value::string("é"),
            Value::string("é")
        ])
    );
    assert_eq!(record_value("record::keys({})"), Value::list(vec![]));
    for (source, expected) in [
        ("record::has({ x: null }, 'x')", Value::Bool(true)),
        ("record::has({ x: null }, 'X')", Value::Bool(false)),
        ("record::has({}, '')", Value::Bool(false)),
        ("record::has({ 'é': 1 }, 'é')", Value::Bool(false)),
        ("record::get_or({ x: null }, 'x', 7)", Value::Null),
        ("record::get_or({}, 'x', 7)", Value::Int(7)),
        ("record::get_or({ '': 9 }, '', null)", Value::Int(9)),
        (
            "record::get_or({}, '', [1, 2])",
            Value::list(vec![Value::Int(1), Value::Int(2)]),
        ),
    ] {
        assert_eq!(record_value(source), expected, "{source}");
    }
}

#[test]
fn record_selection_update_and_merge_keep_exact_order_and_immutable_inputs() {
    for source in [
        "let input = { a: 1, b: 2, c: 3 }\nlet output = record::select(input, ['c', 'a'])\n[record::keys(output), output.c, output.a, record::keys(input), input.b]",
        "let input = { c: 3, a: 1 }\nlet output = record::set(input, 'a', 9)\n[record::keys(output), output.a, input.a]",
        "let input = { c: 3 }\nlet output = record::set(input, 'a', 9)\n[record::keys(output), output.a, record::keys(input)]",
        "let left = { z: 1, a: 2 }\nlet right = { a: null, n: 3, z: 4, m: 5 }\nlet output = record::merge(left, right)\n[record::keys(output), output.z, output.a, output.n, output.m, left.z, left.a, record::keys(right)]",
    ] {
        let expected = if source.contains("select") {
            Value::list(vec![
                Value::list(vec![Value::string("c"), Value::string("a")]),
                Value::Int(3),
                Value::Int(1),
                Value::list(vec![
                    Value::string("a"),
                    Value::string("b"),
                    Value::string("c"),
                ]),
                Value::Int(2),
            ])
        } else if source.contains("merge") {
            Value::list(vec![
                Value::list(vec![
                    Value::string("z"),
                    Value::string("a"),
                    Value::string("n"),
                    Value::string("m"),
                ]),
                Value::Int(4),
                Value::Null,
                Value::Int(3),
                Value::Int(5),
                Value::Int(1),
                Value::Int(2),
                Value::list(vec![
                    Value::string("a"),
                    Value::string("n"),
                    Value::string("z"),
                    Value::string("m"),
                ]),
            ])
        } else if source.contains("input.a") {
            Value::list(vec![
                Value::list(vec![Value::string("c"), Value::string("a")]),
                Value::Int(9),
                Value::Int(1),
            ])
        } else {
            Value::list(vec![
                Value::list(vec![Value::string("c"), Value::string("a")]),
                Value::Int(9),
                Value::list(vec![Value::string("c")]),
            ])
        };
        assert_eq!(record_value(source), expected);
    }
    for source in [
        "record::select({ a: 1 }, [])",
        "record::select({}, [])",
        "record::merge({}, {})",
    ] {
        assert_eq!(
            record_value(source),
            opaal_runtime::Record::new(vec![]).unwrap().into()
        );
    }
    for source in [
        "record::merge({ a: 1 }, {})",
        "record::merge({}, { a: 1 })",
        "record::set({}, 'a', 1)",
    ] {
        assert_eq!(
            record_value(source),
            opaal_runtime::Record::new(vec![("a".to_owned(), Value::Int(1))])
                .unwrap()
                .into()
        );
    }
    assert_eq!(
        record_value(
            "import std::list as list\nlet input = { z: 2, a: 1 }\nlist::map[String, Any](record::keys(input), {|key: String| -> Any input[key]})"
        ),
        Value::list(vec![Value::Int(2), Value::Int(1)])
    );
    assert_eq!(
        record_value(
            "import std::list as list\nlist::map[Record, Record]([{ a: 1 }, { a: 2 }], {|row: Record| -> Record record::merge(row, { b: 3 })})"
        ),
        Value::list(vec![
            opaal_runtime::Record::new(vec![
                ("a".to_owned(), Value::Int(1)),
                ("b".to_owned(), Value::Int(3))
            ])
            .unwrap()
            .into(),
            opaal_runtime::Record::new(vec![
                ("a".to_owned(), Value::Int(2)),
                ("b".to_owned(), Value::Int(3))
            ])
            .unwrap()
            .into()
        ])
    );
}

#[test]
fn record_errors_are_catchable_spanned_and_show_bounded_escaped_keys() {
    for (call, expected) in [
        (
            "record::select({ a: 1 }, ['b'])",
            "std::record::select: missing requested key \"b\"",
        ),
        (
            "record::select({ a: 1 }, ['a', 'a'])",
            "std::record::select: repeated requested key \"a\"",
        ),
        (
            "record::select({}, [\"line\n\t\u{1b}\"])",
            "std::record::select: missing requested key \"line\\n\\t\\u{1b}\"",
        ),
    ] {
        let source = format!(
            "import std::record as record\nmut found = []\ntry {{ let result = {call} }} catch error {{ found = [error.message, error.source.start, error.source.end] }}\nfound"
        );
        let start = source.find(call).unwrap();
        let outcome = execute(&load(&source), ResourceBudget::default());
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Completed(completion) if completion.value() == &Value::list(vec![Value::string(expected), Value::Int(start as i64), Value::Int((start + call.len()) as i64)])),
            "{outcome:?}"
        );
    }
    let sensitive = "secret".repeat(1000);
    let output = record_value(&format!(
        "mut message = ''\ntry {{ let result = record::select({{}}, ['{sensitive}']) }} catch error {{ message = error.message }}\nmessage"
    ));
    let Value::String(message) = output else {
        panic!("error message expected")
    };
    assert!(message.len() < 200);
    assert!(!message.contains(&sensitive));
}

#[test]
fn record_arguments_and_defaults_are_eager_even_for_present_keys_and_bad_inputs() {
    for call in [
        "record::get_or({ a: 1 }, 'a', fail())",
        "record::get_or(dynamic(1), 'a', fail())",
        "record::set({}, 'a', fail())",
    ] {
        assert_eq!(
            record_value(&format!(
                "def dynamic(x: Any) -> Any {{ x }}\ndef fail() -> Any {{ throw 'eager' }}\nmut message = ''\ntry {{ let result = {call} }} catch error {{ message = error.message }}\nmessage"
            )),
            Value::string("eager")
        );
    }
}

#[test]
fn structural_records_preserve_nominal_and_callable_values_through_any_parameters() {
    assert_eq!(
        record_value(
            "import std::outcome as outcome\n\
             type Row = { a: Int }\n\
             let row = Row { a: 7 }\n\
             let option: outcome::Option[Int] = outcome::Option::Some(9)\n\
             let callback = {|x: Int| -> Int x + 1}\n\
             def dynamic(x: Any) -> Any { x }\n\
             let input = record::set(dynamic({}), 'row', row)\n\
             let merged = record::merge(input, { option: option, callback: callback })\n\
             let selected = record::select(merged, dynamic(['callback', 'row', 'option']))\n\
             [record::get_or(selected, 'row', null) == row,\n\
              record::get_or(selected, 'option', null) == option,\n\
              record::get_or(selected, 'callback', null) == callback,\n\
              record::get_or({}, 'missing', row) == row,\n\
              record::get_or({}, 'missing', option) == option,\n\
              record::get_or({}, 'missing', callback) == callback,\n\
              record::keys(input) == ['row']]"
        ),
        Value::list(vec![Value::Bool(true); 7])
    );
}

#[test]
fn record_checker_and_runtime_reject_wrong_shapes_including_nominal_records() {
    for call in [
        "record::keys()",
        "record::keys({}, {})",
        "record::keys(1)",
        "record::keys[Int]({})",
        "record::has({}, 1)",
        "record::get_or({}, 'x')",
        "record::select({}, [1])",
        "record::set({}, 'x')",
        "record::merge({}, 1)",
        "record::unknown({})",
        "{} | record::keys",
        "record::keys(row)",
        "record::has(row, 'a')",
        "record::get_or(row, 'a', 1)",
        "record::select(row, [])",
        "record::set(row, 'a', 1)",
        "record::merge({}, row)",
    ] {
        let source = Sources(format!(
            "import std::record as record\ntype Row = {{ a: Int }}\nlet row = Row {{ a: 1 }}\n{call}"
        ));
        let report =
            ModuleProgramLoader::new(&source, &source).analyze(Path::new("/project/main.opaal"));
        assert!(
            report.program().is_none() && !report.issues().is_empty(),
            "{call}"
        );
    }
    for call in [
        "record::keys(dynamic(1))",
        "record::has({}, dynamic(1))",
        "record::get_or({}, dynamic(1), 0)",
        "record::select({}, dynamic([1]))",
        "record::select({}, dynamic(1))",
        "record::set({}, dynamic(1), 0)",
        "record::merge(dynamic(1), {})",
        "record::merge({}, dynamic(1))",
        "record::keys(dynamic(row))",
        "record::has(dynamic(row), 'a')",
        "record::get_or(dynamic(row), 'a', 0)",
        "record::select(dynamic(row), [])",
        "record::set(dynamic(row), 'a', 0)",
        "record::merge({}, dynamic(row))",
    ] {
        let source = format!(
            "import std::record as record\ntype Row = {{ a: Int }}\nlet row = Row {{ a: 1 }}\ndef dynamic(x: Any) -> Any {{ x }}\n{call}"
        );
        let outcome = execute(&load(&source), ResourceBudget::default());
        assert!(
            matches!(outcome.primary(), PrimaryOutcome::Error(_)),
            "{call}: {outcome:?}"
        );
    }
}

#[test]
fn all_record_descriptors_resolve_through_aliases_reexports_and_help() {
    let program = load(
        "import std::record as record\nimport './api.opaal' as api\napi::records::merge({ a: 1 }, { b: 2 })",
    );
    let root = program.graph().root();
    let help = ModuleHelpCatalog::snapshot(&program);
    for (name, arity, result) in [
        ("keys", 1, "List[String]"),
        ("has", 2, "Bool"),
        ("get_or", 3, "Any"),
        ("select", 2, "Record"),
        ("set", 3, "Record"),
        ("merge", 2, "Record"),
    ] {
        let direct = program.resolve_operation(root, &["record", name]).unwrap();
        let exported = program
            .resolve_operation(root, &["api", "records", name])
            .unwrap();
        assert_eq!(direct, exported);
        assert_eq!(direct.validate(), Ok(()));
        assert!(direct.type_parameters().is_empty());
        assert_eq!(direct.overloads()[0].parameters().len(), arity);
        assert!(direct.signature_labels()[0].ends_with(&format!("-> {result}")));
        assert!(!direct.supports_value_pipeline());
        assert!(direct.documentation().contains("nominal"));
        assert_eq!(
            help.query(root, &format!("api::records::{name}"))
                .unwrap()
                .operation(),
            Some(&direct)
        );
    }
    let source = program.sources().source(root).unwrap();
    let registry = standard_registry();
    let context = program
        .semantic_queries(&registry)
        .operation_signature_at(root, source.text().rfind("b: 2").unwrap())
        .unwrap();
    assert_eq!(context.active_parameter(), 1);
    assert_eq!(
        context.operation().signature_labels(),
        ["std::record::merge(left: Record, right: Record) -> Record"]
    );
    assert_eq!(
        match execute(&program, ResourceBudget::default()).primary() {
            PrimaryOutcome::Completed(completion) => completion.value().clone(),
            other => panic!("{other:?}"),
        },
        opaal_runtime::Record::new(vec![
            ("a".to_owned(), Value::Int(1)),
            ("b".to_owned(), Value::Int(2))
        ])
        .unwrap()
        .into()
    );
}
