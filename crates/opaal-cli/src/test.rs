//! Named ordinary Bool functions, admitted statically and invoked without a host.

use std::io::{self, Write};
use std::path::PathBuf;

use opaal_runtime::builtin::standard_registry;
use opaal_runtime::eval::{
    CancelReason, CancellationToken, Completion, ResourceBudget, RuntimeError, RuntimeErrorKind,
};
use opaal_runtime::module::{
    AnalysisControl, AnalysisLimits, CallableKind, ModuleAnalysisOutcome, ModuleProgramLoader,
    ValueType,
};
use opaal_runtime::script::invoke_pure_function;
use opaal_runtime::workflow::native_path;
use serde_json::{Value, json};

use crate::check::CheckFilesystem;

/// One explicit source and optional exact export selection.
pub struct TestRequest {
    pub source: PathBuf,
    pub list: bool,
    pub case: Option<String>,
    pub format_json: bool,
}

/// Finite caller limits; smaller ceilings can be injected by contract tests.
pub struct TestLimits {
    pub case_budget: ResourceBudget,
    pub suite_steps: u64,
    pub cases: usize,
    pub view_bytes: usize,
}

impl Default for TestLimits {
    fn default() -> Self {
        Self {
            case_budget: ResourceBudget::opaal(),
            suite_steps: 10_000_000,
            cases: 128,
            view_bytes: 64 * 1024,
        }
    }
}

/// A complete bounded view and its independent command exit.
pub struct TestRun {
    pub output: Vec<u8>,
    pub exit_code: u8,
}

struct BoundedView {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedView {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("test result view limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn diagnostic(
    code: &'static str,
    kind: &'static str,
    error: Option<&RuntimeError>,
    program: &opaal_runtime::module::ModuleProgram,
) -> Value {
    let location = error.and_then(|error| {
        let source = error.source()?;
        source.slice(error.span()).ok()?;
        let module = program
            .sources()
            .entries()
            .find(|entry| entry.source().id() == source.id())?
            .module();
        Some((
            native_path(module.path()),
            json!({"start":error.span().start(),"end":error.span().end()}),
        ))
    });
    json!({"code":code,"kind":kind,"source":location.as_ref().map(|(source,_)| source),"span":location.as_ref().map(|(_,span)| span)})
}

fn render(
    request: &TestRequest,
    cases: Vec<Value>,
    discovered: usize,
    failure: Option<&'static str>,
    exit_code: u8,
    limit: usize,
) -> Result<TestRun, &'static str> {
    let count = |state: &str| cases.iter().filter(|case| case["state"] == state).count();
    let counts = json!({"discovered":discovered,"selected":cases.len(),"passed":count("passed"),"failed":count("failed"),"error":count("error"),"unrun":count("unrun")});
    let document = json!({"schema":"opaal.test-results.v1","schema_version":1,"source":native_path(&request.source),"mode":if request.list {"list"} else {"run"},"selected":request.case,"completeness":if failure.is_some() {"incomplete"} else {"complete"},"cases":cases,"counts":counts,"failure":failure});
    let mut output = BoundedView {
        bytes: Vec::new(),
        limit: limit.min(64 * 1024),
    };
    if request.format_json {
        serde_json::to_writer(&mut output, &document).map_err(|_| "TEST_RESOURCE")?;
        output.write_all(b"\n").map_err(|_| "TEST_RESOURCE")?;
    } else {
        for case in document["cases"].as_array().expect("constructed case list") {
            let name = case["name"].as_str().expect("validated identifier");
            writeln!(
                &mut output,
                "{}: {name}",
                case["state"].as_str().expect("case state")
            )
            .map_err(|_| "TEST_RESOURCE")?;
            if let Some(code) = case["diagnostic"]["code"].as_str() {
                writeln!(&mut output, "  {code}").map_err(|_| "TEST_RESOURCE")?;
            }
        }
        if let Some(code) = failure {
            writeln!(&mut output, "suite: {code}").map_err(|_| "TEST_RESOURCE")?;
        }
        writeln!(
            &mut output,
            "discovered: {discovered}; selected: {}; passed: {}; failed: {}; error: {}; unrun: {}",
            counts["selected"],
            counts["passed"],
            counts["failed"],
            counts["error"],
            counts["unrun"]
        )
        .map_err(|_| "TEST_RESOURCE")?;
    }
    Ok(TestRun {
        output: output.bytes,
        exit_code,
    })
}

/// Analyze once; admit the complete selected closure before initializing any case.
/// No environment, platform, executable probe, terminal or standard host enters this API.
pub fn run_tests<F: CheckFilesystem>(
    request: &TestRequest,
    filesystem: &F,
    cancellation: CancellationToken,
    limits: TestLimits,
) -> Result<TestRun, &'static str> {
    let token = cancellation.clone();
    let control = AnalysisControl::cooperative(move || token.is_cancelled());
    let commands = standard_registry();
    let analysis = ModuleProgramLoader::new(filesystem, filesystem)
        .analyze_with_commands_and_limits_controlled(
            &request.source,
            &commands,
            &control,
            AnalysisLimits::default(),
        );
    let report = match analysis {
        ModuleAnalysisOutcome::Complete(report) => report,
        ModuleAnalysisOutcome::Cancelled => {
            return render(
                request,
                Vec::new(),
                0,
                Some("TEST_CANCELLED"),
                cancel_exit(&cancellation),
                limits.view_bytes,
            );
        }
        ModuleAnalysisOutcome::BudgetExceeded(_) => {
            return render(
                request,
                Vec::new(),
                0,
                Some("TEST_RESOURCE"),
                2,
                limits.view_bytes,
            );
        }
    };
    let Some(program) = report.program() else {
        return render(
            request,
            Vec::new(),
            0,
            Some("TEST_SOURCE"),
            2,
            limits.view_bytes,
        );
    };
    let mut inventory = program
        .names()
        .exports(program.graph().root())
        .map(|export| export.name())
        .chain(
            program
                .aliases()
                .exports(program.graph().root())
                .map(|export| export.name()),
        )
        .filter(|name| name.starts_with("test_"))
        .take(limits.cases.min(128) + 1)
        .collect::<Vec<_>>();
    if inventory.len() > limits.cases.min(128) {
        return Err("TEST_RESOURCE");
    }
    inventory.sort_unstable();
    let discovered = inventory.len();
    let mut failure = if discovered == 0 {
        Some("TEST_EMPTY")
    } else {
        None
    };
    let mut selected = Vec::new();
    let mut cases = Vec::new();
    let mut case_bytes = 0;
    for export in inventory {
        let resolved = program.exported_function(export);
        let valid = resolved.as_ref().is_some_and(|(_, signature)| {
            signature.kind() == CallableKind::Function
                && signature.type_parameters().is_empty()
                && signature.parameters().is_empty()
                && signature.result() == &ValueType::Bool
        });
        if !valid {
            failure = Some("TEST_SIGNATURE");
        }
        if request.case.as_deref().is_some_and(|name| name != export) {
            continue;
        }
        let (module, name) = resolved
            .as_ref()
            .map(|(module, signature)| (module, signature.name()))
            .unwrap_or((program.graph().root(), export));
        let entry = json!({"name":export,"callable_id":{"module":native_path(module.path()),"name":name},"state":"unrun","diagnostic":null});
        let mut bounded = BoundedView {
            bytes: Vec::new(),
            limit: limits.view_bytes.min(64 * 1024).saturating_sub(case_bytes),
        };
        serde_json::to_writer(&mut bounded, &entry).map_err(|_| "TEST_RESOURCE")?;
        case_bytes += bounded.bytes.len();
        cases.push(entry);
        selected.push(resolved);
    }
    if request.case.is_some() && cases.is_empty() {
        failure = Some("TEST_SELECTION");
    }
    let mut exit_code = if failure.is_some() { 2 } else { 0 };
    if failure.is_none() && !request.list {
        if !program
            .effects()
            .transitive(program.graph().root())
            .occurrences()
            .is_empty()
        {
            failure = Some("TEST_EFFECT");
        }
        for resolved in &selected {
            let (module, signature) = resolved.as_ref().expect("valid signature");
            if !program
                .function_effects(module, signature, &control)
                .occurrences()
                .is_empty()
            {
                failure = Some("TEST_EFFECT");
            }
            if cancellation.is_cancelled() {
                failure = Some("TEST_CANCELLED");
                break;
            }
        }
    }
    if failure.is_none() && request.list {
        for case in &mut cases {
            case["state"] = json!("listed");
        }
    } else if failure.is_none() {
        let mut used = 0;
        for (case, resolved) in cases.iter_mut().zip(selected) {
            if cancellation.is_cancelled() {
                failure = Some("TEST_CANCELLED");
                break;
            }
            let remaining = limits.suite_steps.min(10_000_000).saturating_sub(used);
            if remaining == 0 {
                failure = Some("TEST_RESOURCE");
                break;
            }
            let mut budget = limits
                .case_budget
                .with_step_ceiling(remaining.min(1_000_000));
            let (module, signature) = resolved.expect("admitted signature");
            let result = invoke_pure_function(
                program,
                &module,
                signature,
                cancellation.clone(),
                &mut budget,
            );
            used += budget.used();
            match result {
                Ok(Completion::Value(opaal_runtime::Value::Bool(value))) => {
                    case["state"] = json!(if value { "passed" } else { "failed" });
                    if !value {
                        exit_code = 1;
                    }
                }
                Ok(Completion::Cancelled(_)) => {
                    case["diagnostic"] = diagnostic("TEST_CANCELLED", "cancelled", None, program);
                    failure = Some("TEST_CANCELLED");
                    break;
                }
                Ok(Completion::Value(_)) => {
                    case["diagnostic"] = diagnostic("TEST_RETURN", "invalid", None, program);
                    failure = Some("TEST_RETURN");
                    break;
                }
                Err(error) => {
                    let (code, kind, stop) = match error.kind() {
                        RuntimeErrorKind::ResourceBudgetExceeded => {
                            ("TEST_RESOURCE", "resource", true)
                        }
                        RuntimeErrorKind::PolicyRefused { .. } => ("TEST_EFFECT", "refused", true),
                        RuntimeErrorKind::FunctionResultTypeMismatch { .. }
                        | RuntimeErrorKind::ExecutionUnsupported => {
                            ("TEST_RETURN", "invalid", true)
                        }
                        _ => ("TEST_ERROR", "error", false),
                    };
                    case["state"] = json!("error");
                    case["diagnostic"] = diagnostic(code, kind, Some(&error), program);
                    if stop {
                        failure = Some(code);
                        break;
                    }
                    exit_code = 1;
                }
            }
        }
    }
    if cancellation.is_cancelled() {
        failure = Some("TEST_CANCELLED");
    }
    if failure.is_some() {
        exit_code = if failure == Some("TEST_CANCELLED") {
            cancel_exit(&cancellation)
        } else {
            2
        };
    }
    render(
        request,
        cases,
        discovered,
        failure,
        exit_code,
        limits.view_bytes,
    )
}

fn cancel_exit(cancellation: &CancellationToken) -> u8 {
    if cancellation.reason() == CancelReason::Requested {
        130
    } else {
        2
    }
}
