#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use opaal_cli::cli::{Mode, parse_args};
use opaal_cli::test::{TestLimits, TestRequest, run_tests};
use opaal_runtime::eval::{CancellationToken, ResourceBudget};
use opaal_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
};
use serde_json::{Value, json};

struct Sources(BTreeMap<PathBuf, Vec<u8>>);
impl ModuleCanonicalizer for Sources {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        self.0
            .contains_key(candidate)
            .then(|| candidate.to_path_buf())
            .ok_or_else(|| ModulePathError::new("missing"))
    }
}
impl ModuleSourceLoader for Sources {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.0
            .get(module.path())
            .cloned()
            .ok_or_else(|| ModuleSourceError::new("missing"))
    }
}

fn request() -> TestRequest {
    TestRequest {
        source: PathBuf::from("/tests/root.opaal"),
        list: false,
        case: None,
        format_json: true,
    }
}

fn run(source: &str, list: bool, case: Option<&str>, limits: TestLimits) -> (u8, Value) {
    let sources = Sources(BTreeMap::from([(
        PathBuf::from("/tests/root.opaal"),
        source.as_bytes().to_vec(),
    )]));
    let mut request = request();
    request.list = list;
    request.case = case.map(str::to_owned);
    let result = run_tests(&request, &sources, CancellationToken::never(), limits).unwrap();
    assert_eq!(result.output.last(), Some(&b'\n'));
    assert!(result.output.len() <= 64 * 1024);
    let view: Value = serde_json::from_slice(&result.output).unwrap();
    if view["failure"] == "TEST_SOURCE" {
        panic!(
            "{}",
            opaal_runtime::module::ModuleProgramLoader::new(&sources, &sources)
                .load_for_frontend(&request.source)
                .unwrap_err()
                .render()
        );
    }
    assert_eq!(view.as_object().unwrap().len(), 9);
    assert_eq!(view["schema"], "opaal.test-results.v1");
    assert_eq!(view["schema_version"], 1);
    assert_eq!(view["counts"].as_object().unwrap().len(), 6);
    for case in view["cases"].as_array().unwrap() {
        assert_eq!(case.as_object().unwrap().len(), 4);
        assert_eq!(case["callable_id"].as_object().unwrap().len(), 2);
        if !case["diagnostic"].is_null() {
            assert_eq!(case["diagnostic"].as_object().unwrap().len(), 4);
        }
    }
    (result.exit_code, view)
}

#[test]
fn ordinary_false_error_catch_and_rethrow_aggregate_without_payloads() {
    let source = "export { test_pass, test_false, test_error, test_caught, test_rethrow }\ndef test_pass() -> Bool { return true }\ndef test_false() -> Bool { return false }\ndef test_error() -> Bool { throw 'failure-message-canary' }\ndef test_caught() -> Bool { try { throw 'expected' } catch error { return error.category == 'user' }\nreturn false }\ndef test_rethrow() -> Bool { try { throw 'failure-message-canary' } catch error { throw error }\nreturn true }\n";
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 1, "{view}");
    assert_eq!(
        view["counts"],
        json!({"discovered":5,"selected":5,"passed":2,"failed":1,"error":2,"unrun":0})
    );
    assert_eq!(view["completeness"], "complete");
    assert!(view["failure"].is_null());
    assert!(!view.to_string().contains("failure-message-canary"));
    assert_eq!(view["cases"][1]["diagnostic"]["kind"], "error");
    assert!(!view["cases"][1]["diagnostic"]["span"].is_null());
}

#[test]
fn listing_does_not_initialize_and_running_does() {
    let source =
        "export { test_one }\nthrow 'initializer-canary'\ndef test_one() -> Bool { return true }\n";
    let (exit, view) = run(source, true, None, TestLimits::default());
    assert_eq!(exit, 0, "{view}");
    assert_eq!(view["cases"][0]["state"], "listed");
    assert_eq!(view["counts"]["passed"], 0);
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 1, "{view}");
    assert!(!view.to_string().contains("initializer-canary"));
}

#[test]
fn invalid_signatures_are_caller_refusals_before_any_initialization() {
    for source in [
        "export { test_one }\nthrow 'initializer-canary'\ndef test_one(input: Bool) -> Bool { return input }\n",
        "export { test_one }\ndef test_one() -> Int { return 1 }\n",
        "export { test_one }\nlet test_one = true\n",
        "export { test_one }\ndef test_one[T](input: T) -> Bool { return true }\n",
    ] {
        let (exit, view) = run(source, false, None, TestLimits::default());
        assert_eq!(exit, 2, "{view}");
        assert_eq!(view["failure"], "TEST_SIGNATURE", "{view}");
        assert_eq!(view["counts"]["unrun"], 1);
    }
}

#[test]
fn empty_unknown_and_nonselected_invalid_inventory_refuse() {
    let (exit, view) = run(
        "def ordinary() -> Bool { return true }\n",
        false,
        None,
        TestLimits::default(),
    );
    assert_eq!(exit, 2);
    assert_eq!(view["failure"], "TEST_EMPTY");
    let source = "export { test_good, test_bad }\ndef test_good() -> Bool { return true }\ndef test_bad(input: Bool) -> Bool { return input }\n";
    let (exit, view) = run(source, false, Some("test_good"), TestLimits::default());
    assert_eq!(exit, 2);
    assert_eq!(view["failure"], "TEST_SIGNATURE");
    assert_eq!(view["counts"]["discovered"], 2);
    assert_eq!(view["counts"]["selected"], 1);
    let (exit, view) = run(source, false, Some("unknown"), TestLimits::default());
    assert_eq!(exit, 2);
    assert_eq!(view["failure"], "TEST_SELECTION");
}

#[test]
fn all_reachable_effects_are_checked_before_first_case() {
    for effect in ["env('PRIVATE')", "glob('*')", "helper()"] {
        let source = format!(
            "export {{ test_a, test_z }}\ndef helper() -> Bool {{ let secret = env('PRIVATE')\nreturn true }}\ndef test_a() -> Bool {{ return true }}\ndef test_z() -> Bool {{ {effect}\nreturn true }}\n"
        );
        let (exit, view) = run(&source, false, None, TestLimits::default());
        assert_eq!(exit, 2, "{effect}: {view}");
        assert_eq!(view["failure"], "TEST_EFFECT", "{effect}: {view}");
        assert_eq!(view["counts"]["unrun"], 2);
        assert!(!view.to_string().contains("PRIVATE"));
    }
}

#[test]
fn discarded_pure_expression_and_local_binding_have_identical_admission() {
    for expression in ["helper()", "let ignored = helper()"] {
        let source = format!(
            "export {{ test_one }}\ndef unused() -> Bool {{ let secret = env('PRIVATE')\nreturn true }}\ndef helper() -> Bool {{ return true }}\ndef test_one() -> Bool {{ {expression}\nreturn true }}\n"
        );
        let (exit, view) = run(&source, false, None, TestLimits::default());
        assert_eq!(exit, 0, "{expression}: {view}");
        assert_eq!(view["counts"]["passed"], 1);
    }
}

#[test]
fn opaque_calls_refuse_even_when_dynamically_pure() {
    let source = "export { test_one }\ndef invoke(f: Any) -> Bool { return f() }\ndef helper() -> Bool { return true }\ndef test_one() -> Bool { return invoke(helper) }\n";
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 2, "{view}");
    assert_eq!(view["failure"], "TEST_EFFECT");
}

#[test]
fn module_and_imported_alias_cases_use_canonical_identity_and_fresh_scopes() {
    let root = "import './dependency.opaal' as dependency\nexport { test_a, test_b }\nlet test_a = dependency::check\nlet test_b = dependency::check\n";
    let dependency =
        "export { check }\nlet values = [1, 2]\ndef check() -> Bool { return values[0] == 1 }\n";
    let sources = Sources(BTreeMap::from([
        (PathBuf::from("/tests/root.opaal"), root.as_bytes().to_vec()),
        (
            PathBuf::from("/tests/dependency.opaal"),
            dependency.as_bytes().to_vec(),
        ),
    ]));
    let result = run_tests(
        &request(),
        &sources,
        CancellationToken::never(),
        TestLimits::default(),
    )
    .unwrap();
    let view: Value = serde_json::from_slice(&result.output).unwrap();
    if view["failure"] == "TEST_SOURCE" {
        panic!(
            "{}",
            opaal_runtime::module::ModuleProgramLoader::new(&sources, &sources)
                .load_for_frontend(&request().source)
                .unwrap_err()
                .render()
        );
    }
    assert_eq!(result.exit_code, 0, "{view}");
    assert_eq!(view["counts"]["passed"], 2);
    assert_eq!(view["cases"][0]["callable_id"]["name"], "check");
    assert_eq!(view["cases"][1]["name"], "test_b");
}

#[test]
fn effectful_initializers_refuse_while_list_remains_host_free() {
    let source = "export { test_one }\nlet secret = env('PRIVATE')\ndef test_one() -> Bool { return true }\n";
    assert_eq!(run(source, true, None, TestLimits::default()).0, 0);
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 2);
    assert_eq!(view["failure"], "TEST_EFFECT");
    assert_eq!(view["cases"][0]["state"], "unrun");
}

#[test]
fn budgets_stop_and_preserve_unrun_cases_without_resetting_caught_limits() {
    let source = "export { test_a, test_b }\ndef recurse() -> Bool { return recurse() }\ndef test_a() -> Bool { return recurse() }\ndef test_b() -> Bool { return true }\n";
    let (exit, view) = run(
        source,
        false,
        None,
        TestLimits {
            case_budget: ResourceBudget::steps(100).with_call_depth(8),
            ..TestLimits::default()
        },
    );
    assert_eq!(exit, 2, "{view}");
    assert_eq!(view["failure"], "TEST_RESOURCE");
    assert_eq!(view["cases"][1]["state"], "unrun");
    let source = "export { test_a, test_b }\ndef test_a() -> Bool { try { let too_large = [1, 2, 3] } catch error { return error.category == 'resource' }\nreturn false }\ndef test_b() -> Bool { return true }\n";
    let (exit, view) = run(
        source,
        false,
        None,
        TestLimits {
            case_budget: ResourceBudget::steps(1000).with_collection_items(2),
            ..TestLimits::default()
        },
    );
    assert_eq!(exit, 0, "{view}");
    assert_eq!(view["counts"]["passed"], 2);
}

#[test]
fn exact_case_suite_and_view_ceilings_refuse_first_excess() {
    let source = "export { test_a, test_b }\ndef test_a() -> Bool { return true }\ndef test_b() -> Bool { return true }\n";
    let (exit, _) = run(
        source,
        false,
        None,
        TestLimits {
            cases: 2,
            ..TestLimits::default()
        },
    );
    assert_eq!(exit, 0);
    let sources = Sources(BTreeMap::from([(
        PathBuf::from("/tests/root.opaal"),
        source.as_bytes().to_vec(),
    )]));
    assert_eq!(
        run_tests(
            &request(),
            &sources,
            CancellationToken::never(),
            TestLimits {
                cases: 1,
                ..TestLimits::default()
            }
        )
        .err(),
        Some("TEST_RESOURCE")
    );
    let (_, view) = run(
        source,
        false,
        None,
        TestLimits {
            suite_steps: 1,
            ..TestLimits::default()
        },
    );
    assert_eq!(view["failure"], "TEST_RESOURCE");
    assert_eq!(view["cases"][1]["state"], "unrun");
    let program = opaal_runtime::module::ModuleProgramLoader::new(&sources, &sources)
        .load_for_frontend(&request().source)
        .unwrap();
    let (module, signature) = program.exported_function("test_a").unwrap();
    let mut measured = ResourceBudget::opaal();
    opaal_runtime::script::invoke_pure_function(
        &program,
        &module,
        signature,
        CancellationToken::never(),
        &mut measured,
    )
    .unwrap();
    assert_eq!(
        run(
            source,
            false,
            None,
            TestLimits {
                suite_steps: measured.used() * 2,
                ..TestLimits::default()
            }
        )
        .0,
        0
    );
    let (exit, view) = run(
        source,
        false,
        None,
        TestLimits {
            suite_steps: measured.used() * 2 - 1,
            ..TestLimits::default()
        },
    );
    assert_eq!(exit, 2);
    assert_eq!(view["failure"], "TEST_RESOURCE");
    let baseline = run_tests(
        &request(),
        &sources,
        CancellationToken::never(),
        TestLimits::default(),
    )
    .unwrap();
    assert!(
        run_tests(
            &request(),
            &sources,
            CancellationToken::never(),
            TestLimits {
                view_bytes: baseline.output.len(),
                ..TestLimits::default()
            }
        )
        .is_ok()
    );
    assert_eq!(
        run_tests(
            &request(),
            &sources,
            CancellationToken::never(),
            TestLimits {
                view_bytes: baseline.output.len() - 1,
                ..TestLimits::default()
            }
        )
        .err(),
        Some("TEST_RESOURCE")
    );
}

#[test]
fn cooperative_cancellation_never_passes_or_leaks_partial_source() {
    let source = "export { test_one }\ndef test_one() -> Bool { return true }\n";
    let sources = Sources(BTreeMap::from([(
        PathBuf::from("/tests/root.opaal"),
        source.as_bytes().to_vec(),
    )]));
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = polls.clone();
    let cancellation =
        CancellationToken::from_fn(move || observed.fetch_add(1, Ordering::SeqCst) >= 5);
    let result = run_tests(&request(), &sources, cancellation, TestLimits::default()).unwrap();
    assert_eq!(result.exit_code, 130);
    let view: Value = serde_json::from_slice(&result.output).unwrap();
    assert_eq!(view["failure"], "TEST_CANCELLED");
    assert_eq!(view["counts"]["passed"], 0);
}

#[test]
fn test_cli_rejects_duplicate_missing_unknown_and_stdin_arguments() {
    for args in [
        vec!["test"],
        vec!["test", "-"],
        vec!["test", "a.opaal", "b.opaal"],
        vec!["test", "--case"],
        vec!["test", "--format", "xml", "a.opaal"],
        vec!["test", "--list", "--list", "a.opaal"],
        vec!["test", "--case", "test_a", "--case", "test_a", "a.opaal"],
        vec!["test", "--format", "json", "--format", "text", "a.opaal"],
        vec!["test", "--unknown", "a.opaal"],
    ] {
        assert!(parse_args(args.iter().map(Into::into)).is_err(), "{args:?}");
    }
    assert!(matches!(
        parse_args(["test", "--list", "--format", "json", "--", "-cases.opaal"].map(Into::into))
            .unwrap()
            .mode,
        Mode::Test {
            list: true,
            format_json: true,
            ..
        }
    ));
}

#[test]
fn the_reference_policy_cases_are_proven_pure_and_pass() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/image-check/policy-tests.opaal");
    let filesystem = opaal_cli::check::HostCheckFilesystem;
    let program = opaal_runtime::module::ModuleProgramLoader::new(&filesystem, &filesystem)
        .load_for_frontend(&source)
        .unwrap();
    assert!(
        program
            .effects()
            .transitive(program.graph().root())
            .occurrences()
            .is_empty()
    );
    for export in program.names().exports(program.graph().root()) {
        let (module, signature) = program.exported_function(export.name()).unwrap();
        let effects = program.function_effects(
            &module,
            signature,
            &opaal_runtime::module::AnalysisControl::never(),
        );
        assert!(
            effects.occurrences().is_empty(),
            "{}: {effects:?}",
            export.name()
        );
    }
    let result = run_tests(
        &TestRequest {
            source,
            ..request()
        },
        &filesystem,
        CancellationToken::never(),
        TestLimits::default(),
    )
    .unwrap();
    assert_eq!(
        result.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&result.output)
    );
}

#[test]
fn pure_callback_arguments_are_analyzed_before_any_case_runs() {
    for callback in ["{|x| env('PRIVATE')}", "effectful"] {
        let source = format!(
            "import std::list as list\nexport {{ test_a, test_b }}\ndef effectful(x: Int) -> String {{ return env('PRIVATE') }}\ndef test_a() -> Bool {{ return true }}\ndef test_b() -> Bool {{ let values = list::map[Int, String]([1], {callback})\nreturn true }}\n"
        );
        let (exit, view) = run(&source, false, None, TestLimits::default());
        assert_eq!(exit, 2, "{view}");
        assert_eq!(view["failure"], "TEST_EFFECT");
        assert_eq!(view["counts"]["unrun"], 2);
    }
    let source = "import std::list as list\nexport { test_one }\ndef test_one() -> Bool { let values = list::map[Int, Int]([1, 2], {|x| x + 1})\nreturn values[1] == 3 }\n";
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 0, "{view}");
}

#[test]
fn dynamic_non_bool_returns_are_infrastructure_and_stop_the_suite() {
    let source = "export { test_a, test_b }\ndef dynamic(x: Any) -> Any { return x }\ndef test_a() -> Bool { return dynamic(1) }\ndef test_b() -> Bool { return true }\n";
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 2, "{view}");
    assert_eq!(view["failure"], "TEST_RETURN");
    assert_eq!(view["cases"][1]["state"], "unrun");
}

#[test]
fn native_test_command_refuses_an_unwritable_result_sink() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/image-check/policy-tests.opaal");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_opaal"))
        .arg("test")
        .arg(source)
        .args(["--format", "json"])
        .stdout(std::fs::File::open("/dev/null").unwrap())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("delivery expectations"));
}

#[test]
fn an_exported_test_namespace_invalidates_the_entire_inventory() {
    let source = "import std::math as test_namespace\nexport { test_namespace, test_one }\ndef test_one() -> Bool { throw 'must-not-run' }\n";
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 2, "{view}");
    assert_eq!(view["failure"], "TEST_SIGNATURE");
    assert_eq!(view["counts"]["discovered"], 2);
    assert_eq!(view["counts"]["unrun"], 2);
}

#[test]
fn script_arguments_do_not_enter_pure_test_contexts() {
    let source =
        "export { test_one }\ndef test_one() -> Bool { let arguments = args\nreturn true }\n";
    let (exit, view) = run(source, false, None, TestLimits::default());
    assert_eq!(exit, 2, "{view}");
    assert_eq!(view["failure"], "TEST_EFFECT");
}

#[test]
fn repeated_long_callable_ids_refuse_before_retaining_an_oversized_inventory() {
    let name = "f".repeat(40_000);
    let source = format!(
        "export {{ test_a, test_b }}\ndef {name}() -> Bool {{ return true }}\nlet test_a = {name}\nlet test_b = {name}\n"
    );
    let sources = Sources(BTreeMap::from([(
        PathBuf::from("/tests/root.opaal"),
        source.into_bytes(),
    )]));
    assert_eq!(
        run_tests(
            &request(),
            &sources,
            CancellationToken::never(),
            TestLimits::default()
        )
        .err(),
        Some("TEST_RESOURCE")
    );
}
