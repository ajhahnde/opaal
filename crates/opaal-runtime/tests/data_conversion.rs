#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use opaal_platform::FakePlatform;
use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{CancellationToken, EvalLimits, FakeClock, ResourceBudget};
use opaal_runtime::format::{FromJsonStep, JsonMode, from_json};
use opaal_runtime::help::ModuleHelpCatalog;
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use opaal_runtime::outcome::PrimaryOutcome;
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::execute_module_program_outcome_with_limits;
use opaal_runtime::session::Session;
use opaal_runtime::{BindingMutability, Environment, Record, ScopeStack, Value, data};

struct NoExecutables;
impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _: &OsStr) -> bool {
        panic!("data operations cannot resolve a process")
    }
}

fn session(input: Value) -> Session {
    let mut scope = ScopeStack::new();
    scope
        .declare("input", BindingMutability::Immutable, input)
        .unwrap();
    Session::with_scope(
        scope,
        "/project",
        Environment::new(),
        SessionOptions::default(),
    )
}

fn value(input: Value, expression: &str) -> Value {
    session(input)
        .submit_with_value(
            "conversion.opaal",
            format!("import std::data as data\nimport std::string as string\n{expression}"),
            &NoExecutables,
            &FakePlatform::full(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .unwrap_or_else(|error| panic!("{expression}: {error:?}"))
        .1
}

fn decode(input: &[u8]) -> Value {
    value(Value::bytes(input.to_vec()), "data::json_decode(input)")
}

fn caught(input: Value, expression: &str) -> Value {
    value(
        input,
        &format!(
            "mut found = []\ntry {{ let result = {expression} }} catch error {{ found = [error.message, error.source.start, error.source.end] }}\nfound"
        ),
    )
}

fn message(input: Value, expression: &str) -> String {
    let Value::List(fields) = caught(input, expression) else {
        panic!("expected caught error")
    };
    let Value::String(message) = &fields[0] else {
        panic!("expected message")
    };
    message.to_string()
}

fn pipeline(input: &[u8]) -> FromJsonStep {
    let mut chunks = vec![input.to_vec()].into_iter();
    from_json(
        JsonMode::Document,
        Box::new(move || chunks.next()),
        8 * 1024 * 1024,
    )
    .pull()
}

#[test]
fn strict_json_roots_order_unicode_and_canonical_round_trips() {
    for input in [
        b"null".as_slice(),
        b"true",
        b"false",
        b"-1",
        b"1.25e2",
        b"[]",
        b"{}",
        br#"[null, true, 3, "hi"]"#,
        " {\"z\": [1, false], \"a\": \"é終\\ud83d\\ude00\\n\", \"\": null} \r\n".as_bytes(),
    ] {
        let expected = match pipeline(input) {
            FromJsonStep::Value(value) => value,
            other => panic!("valid corpus: {other:?}"),
        };
        let decoded = decode(input);
        assert_eq!(decoded, expected);
        let encoded = data::json_encode(&decoded).unwrap();
        assert_eq!(data::json_encode(&decode(&encoded)).unwrap(), encoded);
    }
    let decoded = decode(br#"{"z": 0, "a": 1}"#);
    let Value::Record(record) = &decoded else {
        panic!("expected record")
    };
    assert_eq!(record.entries()[0].0.as_ref(), "z");
    assert_eq!(data::json_encode(&decoded).unwrap(), br#"{"a":1,"z":0}"#);
    assert_eq!(
        decode(br#""\"\\\/\b\f\n\r\t\u0000\u007f\u20ac\ud83d\ude00""#),
        Value::string("\"\\/\u{8}\u{c}\n\r\t\0\u{7f}€😀")
    );
}

#[test]
fn numeric_conversion_is_identical_to_the_existing_stream_decoder() {
    for input in [
        "9223372036854775807",
        "9223372036854775808",
        "9007199254740993.0",
        "-9223372036854775809",
        "18446744073709551616",
        "1e400",
        "-0",
        "-0.0",
        "1e-400",
        "0.1",
        "1.234567890123456789e25",
    ] {
        match pipeline(input.as_bytes()) {
            FromJsonStep::Value(expected) => {
                assert_eq!(decode(input.as_bytes()), expected, "{input}")
            }
            FromJsonStep::Malformed { .. } => assert!(
                message(
                    Value::bytes(input.as_bytes().to_vec()),
                    "data::json_decode(input)"
                )
                .starts_with("DATA016:"),
                "{input}"
            ),
            other => panic!("numeric corpus: {other:?}"),
        }
    }
}

#[test]
fn malformed_documents_duplicate_keys_and_utf8_fail_without_payload_leaks() {
    for input in [
        "",
        " ",
        "null true",
        "[]{}",
        "[1,]",
        "{\"x\":1,}",
        "{x:1}",
        "[",
        "{",
        "tru",
        "NULL",
        "NaN",
        "Infinity",
        "+1",
        "01",
        "1.",
        "1e",
        "1e+",
        "--1",
        "1-2",
        "\u{feff}null",
        "\"unclosed",
        "\"\n\"",
        "\"\\x00\"",
        "\"\\u123\"",
        "\"\\ud800\"",
        "\"\\udc00\"",
        "\"\\ud800\\u0061\"",
        "[\"private-payload\", invalid]",
    ] {
        assert!(
            matches!(pipeline(input.as_bytes()), FromJsonStep::Malformed { .. }),
            "pipeline accepted {input:?}"
        );
        let error = message(
            Value::bytes(input.as_bytes().to_vec()),
            "data::json_decode(input)",
        );
        assert!(error.starts_with("DATA016:"), "{input:?}: {error}");
        assert!(!error.contains("private-payload"));
    }
    for input in [
        br#"{"a":1,"a":2}"#.as_slice(),
        br#"{"a": {"b":1,"b":2}}"#,
        br#"{"a":1,"\u0061":2}"#,
    ] {
        assert!(matches!(pipeline(input), FromJsonStep::DuplicateKey { .. }));
        assert!(
            message(Value::bytes(input.to_vec()), "data::json_decode(input)")
                .starts_with("DATA018:")
        );
    }
    let key = format!("\n\t\u{1b}{}", "sensitive".repeat(1000));
    let text = serde_json::to_string(&key).unwrap();
    let error = message(
        Value::bytes(format!("{{{text}:0,{text}:1}}").into_bytes()),
        "data::json_decode(input)",
    );
    assert!(
        error.len() < 250
            && !error.contains(&key)
            && !error.contains('\n')
            && error.contains("\\n\\t\\u{1b}")
    );
    for (bytes, offset) in [
        (vec![0xff], 0),
        (vec![b'"', 0xc3], 1),
        (vec![b'"', 0xed, 0xa0, 0x80, b'"'], 1),
    ] {
        assert_eq!(
            message(Value::bytes(bytes), "data::json_decode(input)"),
            format!("DATA015: invalid JSON UTF8 at byte {offset}")
        );
    }
}

#[test]
fn codec_errors_remain_catchable_and_rethrow_preserves_the_call_span() {
    let expression = "data::json_decode(input)";
    let text = format!(
        "import std::data as data\nimport std::string as string\nmut found = []\ntry {{ let result = {expression} }} catch error {{ found = [error.message, error.source.start, error.source.end] }}\nfound"
    );
    let start = text.find(expression).unwrap();
    assert_eq!(
        caught(Value::bytes(b"[1,]".to_vec()), expression),
        Value::list(vec![
            Value::string("DATA016: invalid JSON at byte 3"),
            Value::Int(start as i64),
            Value::Int((start + expression.len()) as i64)
        ])
    );
    let error = session(Value::bytes(b"[1,]".to_vec())).submit("conversion.opaal", "import std::data as data\ntry { data::json_decode(input) } catch error { throw error }", &NoExecutables, &FakePlatform::full(), &FakeClock::new(), &mut Vec::new()).unwrap_err();
    assert!(error.render().contains("DATA016") && error.render().contains("conversion.opaal"));
}

#[test]
fn released_codec_values_bytes_and_dynamic_errors_are_unchanged() {
    for bytes in [
        b"z=2\na={value=true}\n".as_slice(),
        b"when=2026-09-08T00:00:00Z",
        b"bad=[",
        &[0xff],
    ] {
        match data::toml_decode(bytes) {
            Ok(expected) => assert_eq!(
                value(Value::bytes(bytes.to_vec()), "data::toml_decode(input)"),
                expected
            ),
            Err(expected) => assert_eq!(
                message(Value::bytes(bytes.to_vec()), "data::toml_decode(input)"),
                expected.to_string()
            ),
        }
    }
    for input in [
        Value::Null,
        Value::string("é\n"),
        Value::list(vec![Value::Bool(true), Value::Int(4)]),
        Record::new(vec![("z".into(), Value::Int(1)), ("a".into(), Value::Null)])
            .unwrap()
            .into(),
    ] {
        assert_eq!(value(input.clone(), "data::get(input, [])"), input);
        assert_eq!(
            value(input.clone(), "data::json_encode(input)"),
            Value::bytes(data::json_encode(&input).unwrap())
        );
    }
    for (input, keys) in [
        (Value::Null, vec!["x"]),
        (
            Record::new(vec![("a".into(), Value::Null)]).unwrap().into(),
            vec!["a", "b"],
        ),
        (Record::new(vec![]).unwrap().into(), vec!["missing"]),
    ] {
        let expression = format!(
            "data::get(input, {})",
            serde_json::to_string(&keys).unwrap()
        );
        assert_eq!(
            message(input.clone(), &expression),
            data::get(&input, &keys).unwrap_err().to_string()
        );
    }
    for (input, expression, expected) in [
        (
            Value::Int(1),
            "data::toml_decode(input)",
            "OPERATION001: controlled argument 0 requires Bytes, found int",
        ),
        (
            Value::Int(1),
            "data::get({}, input)",
            "OPERATION001: controlled argument 1 requires List[String], found int",
        ),
        (
            Value::list(vec![Value::string("a"), Value::Int(1)]),
            "data::get({}, input)",
            "OPERATION001: controlled argument 1 requires String, found int",
        ),
    ] {
        assert_eq!(message(input, expression), expected);
    }
}

#[test]
fn recursive_encoder_refusals_and_plain_string_semantics_are_preserved() {
    use opaal_runtime::NativePath;
    use std::os::unix::ffi::OsStringExt;
    let values = [
        Value::bytes(vec![1]),
        Value::Path(NativePath::new(std::ffi::OsString::from_vec(vec![0xff]))),
    ];
    for input in values {
        let nested = Value::list(vec![
            Record::new(vec![("nested".into(), input)]).unwrap().into(),
        ]);
        assert_eq!(
            message(nested.clone(), "data::json_encode(input)"),
            data::json_encode(&nested).unwrap_err().to_string()
        );
    }
    assert_eq!(
        value(Value::string("plain-secret"), "data::json_encode(input)"),
        Value::bytes(br#""plain-secret""#.to_vec())
    );
    assert!(message(Value::Null, "data::json_encode({|x| x})").starts_with("DATA013:"));
    assert_eq!(
        value(
            Value::Null,
            "import std::outcome as outcome\nlet option: outcome::Option[Int] = outcome::Option::Some(1)\nmut message = ''\ntry { data::json_encode(option) } catch error { message = error.message }\nmessage"
        ),
        Value::string("DATA013: variant has no canonical JSON representation")
    );
}

struct Sources(String);
impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(path.to_path_buf())
    }
}
impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        Ok(if module.path().file_name().unwrap() == "api.opaal" {
            b"import std::data as codecs\nexport { codecs }".to_vec()
        } else {
            self.0.as_bytes().to_vec()
        })
    }
}

#[test]
fn shared_data_descriptors_resolve_aliases_reexports_help_and_pure_callbacks() {
    let text = "import std::data as data\nimport std::list as list\nimport './api.opaal' as api\nlist::map[Record, Any]([{ z: 2, a: 1 }], {|row: Record| -> Any api::codecs::json_decode(data::json_encode(row))})";
    let sources = Sources(text.into());
    let program = ModuleProgramLoader::new(&sources, &sources)
        .load(Path::new("/project/main.opaal"))
        .unwrap();
    let root = program.graph().root();
    let help = ModuleHelpCatalog::snapshot(&program);
    for (name, expected) in [
        ("toml_decode", "std::data::toml_decode(input: Bytes) -> Any"),
        (
            "get",
            "std::data::get(input: Any, keys: List[String]) -> Any",
        ),
        ("json_encode", "std::data::json_encode(input: Any) -> Bytes"),
        ("json_decode", "std::data::json_decode(input: Bytes) -> Any"),
    ] {
        let direct = program.resolve_operation(root, &["data", name]).unwrap();
        assert_eq!(direct.signature_labels(), [expected]);
        assert_eq!(
            program
                .resolve_operation(root, &["api", "codecs", name])
                .unwrap(),
            direct
        );
        assert_eq!(
            help.query(root, &format!("api::codecs::{name}"))
                .unwrap()
                .operation(),
            Some(&direct)
        );
        assert!(!direct.supports_value_pipeline());
    }
    let registry = standard_registry();
    let context = program
        .semantic_queries(&registry)
        .operation_signature_at(root, text.find("data::json_encode(row)").unwrap() + 19)
        .unwrap();
    assert_eq!(context.operation().id().name(), "json_encode");
    let result = execute_module_program_outcome_with_limits(
        &program,
        &[],
        Path::new("/project"),
        &mut Environment::new(),
        &registry,
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::full(),
        Arc::new(FakeClock::new()),
        &mut Vec::new(),
        &EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::default()),
    );
    assert!(
        matches!(result.primary(), PrimaryOutcome::Completed(completion) if completion.value() == &Value::list(vec![Record::new(vec![("a".into(), Value::Int(1)), ("z".into(), Value::Int(2))]).unwrap().into()])),
        "{result:?}"
    );
}

#[test]
fn checker_rejects_data_arity_types_generics_and_unknown_names() {
    for expression in [
        "data::json_decode()",
        "data::json_decode(1)",
        "data::json_decode[Int](data::json_encode(1))",
        "data::json_encode(1, 2)",
        "data::get({}, [1])",
        "data::toml_decode('x')",
        "data::unknown(1)",
        "{} | data::json_encode",
    ] {
        let sources = Sources(format!("import std::data as data\n{expression}"));
        assert!(
            ModuleProgramLoader::new(&sources, &sources)
                .load(Path::new("/project/main.opaal"))
                .is_err(),
            "{expression}"
        );
    }
}

#[test]
fn strict_utf8_conversion_preserves_bom_newlines_and_error_offsets() {
    assert_eq!(
        value(
            Value::bytes("\u{feff}é\r\n".as_bytes().to_vec()),
            "string::decode_utf8(input)"
        ),
        Value::string("\u{feff}é\r\n")
    );
    assert!(
        message(Value::bytes(vec![b'a', 0xff]), "string::decode_utf8(input)")
            .contains("invalid UTF8 at byte 1")
    );
}

#[test]
fn shared_data_does_not_admit_filesystem_clock_process_or_environment_authority() {
    use opaal_runtime::session::SubmitOutcome;
    for (import, call) in [
        ("std::filesystem", "restricted::read(input, 1)"),
        (
            "std::filesystem",
            "restricted::write_atomic(input, data::json_encode(1))",
        ),
        ("std::time", "restricted::wall_now()"),
        ("std::time", "restricted::monotonic_now()"),
        ("std::process", "restricted::run(input, [])"),
    ] {
        let result = session(Value::Null).submit("isolation.opaal", format!("import std::data as data\nimport {import} as restricted\nlet decoded = data::json_decode(data::json_encode(1))\n{call}"), &NoExecutables, &FakePlatform::full(), &FakeClock::new(), &mut Vec::new()).unwrap();
        assert!(
            matches!(result, SubmitOutcome::Refused(_)),
            "{call}: {result:?}"
        );
    }
    let result = session(Value::Null).submit("isolation.opaal", "import std::data as data\nimport std::list as list\ndef transform(x: Int) -> Any { let decoded = data::json_decode(data::json_encode(x)); env('OPAAL_DATA_SECRET') }\nlist::map[Int, Any]([1], transform)", &NoExecutables, &FakePlatform::full(), &FakeClock::new(), &mut Vec::new());
    assert!(
        matches!(result, Ok(SubmitOutcome::Refused(_))),
        "{result:?}"
    );
}

#[test]
fn repeated_near_limit_legacy_source_calls_do_not_add_cumulative_codec_charges() {
    let input = Value::string("x".repeat(data::MAX_DATA_BYTES - 2));
    assert_eq!(
        value(
            input,
            "let first = data::json_encode(input)\nlet second = data::json_encode(input)\nlet third = data::json_encode(input)\n[first == second, second == third, data::get(input, []) == input]"
        ),
        Value::list(vec![Value::Bool(true); 3])
    );
}
