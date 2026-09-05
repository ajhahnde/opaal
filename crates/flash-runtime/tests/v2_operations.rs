#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use flash_platform::FakePlatform;
use flash_runtime::Environment;
use flash_runtime::Value;
use flash_runtime::builtin::standard_registry;
use flash_runtime::eval::{FakeClock, RuntimeError, RuntimeErrorKind};
use flash_runtime::help::{ModuleHelpCatalog, ModuleHelpKind};
use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader, ValueType,
};
use flash_runtime::operation::{OperationInputType, OperationPurity, OperationStreamPrimary};
use flash_runtime::plan::SessionOptions;
use flash_runtime::query::SemanticHover;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::execute_module_program;
use flash_runtime::stream::{
    StreamCardinality, StreamCleanupFailure, StreamContractViolation, ValueStream,
};
use flash_syntax::{LanguageMajor, SourceFile, SourceId};

struct FixtureModules;

struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

impl ModuleCanonicalizer for FixtureModules {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        Ok(candidate.to_path_buf())
    }
}

impl ModuleSourceLoader for FixtureModules {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        fs::read(module.path()).map_err(|error| ModuleSourceError::new(error.to_string()))
    }
}

#[test]
fn operation_manifest_checks_and_executes_expression_and_pipeline_forms() {
    let root = operation_root();
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let modules = FixtureModules;

    for row in manifest.lines() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed operation manifest row");
        let report = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
            .analyze_with_commands(&root.join(fields[1]), &standard_registry());
        match fields[0] {
            "complete" => {
                let program = report.program().unwrap_or_else(|| {
                    panic!(
                        "{} must pass shared analysis: {:?}",
                        fields[1],
                        report.issues()
                    )
                });
                execute_module_program(
                    program,
                    &[],
                    &root,
                    &mut Environment::new(),
                    &standard_registry(),
                    &NoExecutables,
                    &SessionOptions::default(),
                    &FakePlatform::full(),
                    Arc::new(FakeClock::new()),
                    &mut Vec::new(),
                )
                .unwrap_or_else(|error| panic!("{} must execute: {}", fields[1], error.render()));
            }
            "invalid" => {
                let codes = report
                    .issues()
                    .iter()
                    .flat_map(|issue| issue.error().diagnostics())
                    .map(|diagnostic| diagnostic.code().to_owned())
                    .collect::<Vec<_>>();
                assert!(
                    report.program().is_none() && codes.iter().any(|code| code == fields[2]),
                    "{} must fail with {}: {codes:?}",
                    fields[1],
                    fields[2]
                );
            }
            class => panic!("unknown operation corpus class {class:?}"),
        }
    }
}

#[test]
fn operation_identity_overloads_help_and_budgeted_stream_share_one_descriptor() {
    let root = operation_root();
    let modules = FixtureModules;
    let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(&root.join("complete/reference.fsh"))
        .expect("the operation reference fixture loads");
    let module = program.graph().root();
    let direct = program
        .resolve_operation(module, &["value", "length"])
        .expect("the direct standard alias resolves");
    let reexported = program
        .resolve_operation(module, &["api", "value", "length"])
        .expect("the explicit alias re-export resolves");

    assert_eq!(direct.id(), reexported.id());
    assert_eq!(direct.id().qualified_name(), "std::value::length");
    assert_eq!(direct.type_parameters(), &["T"]);
    assert_eq!(direct.validate(), Ok(()));
    assert_eq!(direct.purity(), OperationPurity::Pure);
    assert!(direct.downstream().is_foundation_only());
    assert_eq!(direct.overloads().len(), 2);
    assert!(matches!(
        direct.overloads()[0].input(),
        OperationInputType::Value(_)
    ));
    assert!(matches!(
        direct.overloads()[1].input(),
        OperationInputType::ValueStream(_)
    ));
    assert_eq!(
        direct.execute_value(Value::list(vec![Value::Int(1), Value::Int(2)])),
        Ok(Value::Int(2))
    );
    assert!(direct.execute_value(Value::Int(2)).is_err());
    assert!(
        direct
            .execute_value_with_types(Value::list(vec![Value::Int(1)]), &[ValueType::String],)
            .is_err()
    );

    let help = ModuleHelpCatalog::snapshot(&program)
        .query(module, "api::value::length")
        .expect("operation help follows the same alias path");
    assert_eq!(help.kind(), ModuleHelpKind::Operation);
    assert_eq!(help.operation().unwrap().id(), direct.id());
    assert!(
        help.operation()
            .unwrap()
            .documentation()
            .contains("bounded")
    );

    let source = program.sources().source(module).unwrap();
    let commands = standard_registry();
    let queries = program.semantic_queries(&commands);
    let operation_cursor = source.text().find("value::length([1, 2])").unwrap() + 9;
    let SemanticHover::Operation(hover) = queries.hover_at(module, operation_cursor).unwrap()
    else {
        panic!("the qualified operation should expose descriptor hover");
    };
    assert_eq!(hover.operation().id(), direct.id());
    assert_eq!(
        queries
            .operation_signature_at(module, operation_cursor + 8)
            .unwrap()
            .operation()
            .id(),
        direct.id()
    );
    let candidates = queries.operation_candidates(module, "api::value::le");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].spelling(), "api::value::length");
    assert_eq!(candidates[0].operation().id(), direct.id());

    let outcome = direct.execute_value_stream(
        ValueStream::from_values(vec![Value::Int(1), Value::Int(2)])
            .with_contract(ValueType::Int, StreamCardinality::Exact(2)),
        2,
    );
    match outcome.primary() {
        OperationStreamPrimary::Value(Value::Int(2)) => {}
        other => panic!("at-limit stream length must succeed: {other:?}"),
    }
    assert_eq!(outcome.delivered_items(), 2);

    let outcome = direct.execute_value_stream(
        ValueStream::from_values(Vec::new())
            .with_contract(ValueType::Int, StreamCardinality::Exact(0)),
        0,
    );
    assert!(matches!(
        outcome.primary(),
        OperationStreamPrimary::Value(Value::Int(0))
    ));
    assert_eq!(outcome.delivered_items(), 0);

    let outcome = direct.execute_value_stream(
        ValueStream::once(Value::Int(1)).with_contract(ValueType::Int, StreamCardinality::Exact(1)),
        0,
    );
    assert!(matches!(
        outcome.primary(),
        OperationStreamPrimary::LimitExceeded { limit: 0 }
    ));
    assert_eq!(outcome.delivered_items(), 0);
    let outcome = direct.execute_value_stream(
        ValueStream::from_values(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
            .with_contract(ValueType::Int, StreamCardinality::Exact(3)),
        2,
    );
    match outcome.primary() {
        OperationStreamPrimary::LimitExceeded { limit: 2 } => {}
        other => panic!("first excess stream item must refuse: {other:?}"),
    }
    assert_eq!(outcome.delivered_items(), 2);

    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let outcome = direct.execute_value_stream(
        ValueStream::from_values(vec![Value::Int(1)]).with_cleanup({
            let cleanup_calls = Arc::clone(&cleanup_calls);
            move || {
                cleanup_calls.fetch_add(1, Ordering::SeqCst);
                Err(StreamCleanupFailure::new("close failed"))
            }
        }),
        1,
    );
    assert!(matches!(
        outcome.primary(),
        OperationStreamPrimary::CleanupFailed(failure) if failure.message() == "close failed"
    ));
    assert!(outcome.cleanup_failure().is_none());
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 1);

    let cancelled = Arc::new(AtomicBool::new(false));
    let outcome = direct.execute_value_stream(
        ValueStream::from_fn({
            let cancelled = Arc::clone(&cancelled);
            let mut emitted = false;
            move || {
                if emitted {
                    None
                } else {
                    emitted = true;
                    cancelled.store(true, Ordering::SeqCst);
                    Some(Ok(Value::Int(1)))
                }
            }
        })
        .with_contract(ValueType::Int, StreamCardinality::Unknown)
        .with_cancellation(flash_runtime::eval::CancellationToken::from_fn({
            let cancelled = Arc::clone(&cancelled);
            move || cancelled.load(Ordering::SeqCst)
        }))
        .with_cleanup(|| Err(StreamCleanupFailure::new("cancel cleanup"))),
        8,
    );
    assert!(matches!(
        outcome.primary(),
        OperationStreamPrimary::Cancelled(flash_runtime::eval::CancelReason::Requested)
    ));
    assert_eq!(outcome.delivered_items(), 1);
    assert_eq!(
        outcome.cleanup_failure().map(StreamCleanupFailure::message),
        Some("cancel cleanup")
    );

    let producer_source = SourceFile::new(SourceId::new(900), "producer.fsh", "x");
    let producer_span = producer_source.span(0..1).unwrap();
    let mut producer_step = 0;
    let outcome = direct.execute_value_stream(
        ValueStream::from_fn(move || {
            producer_step += 1;
            match producer_step {
                1 => Some(Ok(Value::Int(1))),
                2 => Some(Err(RuntimeError::new(
                    RuntimeErrorKind::Unsupported {
                        feature: "producer",
                    },
                    producer_span,
                ))),
                _ => panic!("a terminal producer failure must not be advanced again"),
            }
        })
        .with_contract(ValueType::Int, StreamCardinality::Unknown)
        .with_cleanup(|| Err(StreamCleanupFailure::new("failure cleanup"))),
        8,
    );
    assert!(matches!(
        outcome.primary(),
        OperationStreamPrimary::Failed(error)
            if matches!(error.kind(), RuntimeErrorKind::Unsupported { feature: "producer" })
    ));
    assert_eq!(outcome.delivered_items(), 1);
    assert_eq!(
        outcome.cleanup_failure().map(StreamCleanupFailure::message),
        Some("failure cleanup")
    );

    let outcome = direct.execute_value_stream(
        ValueStream::from_values(vec![Value::Int(1), Value::string("wrong")])
            .with_contract(ValueType::Int, StreamCardinality::Exact(2)),
        8,
    );
    assert!(matches!(
        outcome.primary(),
        OperationStreamPrimary::ContractViolation(StreamContractViolation::ElementType {
            expected: ValueType::Int,
            actual: "string",
        })
    ));
    assert_eq!(outcome.delivered_items(), 1);
}

#[test]
fn golden_workflow_retains_int_two_in_the_embedding_api() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/v2/source.fsh");
    let modules = FixtureModules;
    let program = ModuleProgramLoader::for_language(&modules, &modules, LanguageMajor::V2)
        .load(&source)
        .expect("the complete workflow source should analyze");
    let mut output = Vec::new();
    let completion = execute_module_program(
        &program,
        &["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()],
        source.parent().unwrap(),
        &mut Environment::new(),
        &standard_registry(),
        &NoExecutables,
        &SessionOptions::default(),
        &FakePlatform::full(),
        Arc::new(FakeClock::new()),
        &mut output,
    )
    .expect("the pure workflow should execute");

    assert_eq!(completion.value(), &Value::Int(2));
    assert!(completion.status().is_none());
    assert!(
        output.is_empty(),
        "embedding retains rather than prints the value"
    );
}

fn operation_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language/operations")
}
