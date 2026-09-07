//! Non-interactive script execution through the persistent session driver.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::command::CommandRegistry;
use crate::eval::{CancellationToken, Clock, EvalLimits, ResourceBudget};
use crate::module::{ModuleId, ModuleProgram, ModuleSourceRegistry};
use crate::outcome::{ExecutionOutcome, FatalHostFailure, FatalHostFailureKind, PrimaryOutcome};
use crate::plan::SessionOptions;
use crate::resolve::ExecutableProbe;
use crate::session::{
    BackgroundFailure, BackgroundFailureReason, Session, SubmitError, SubmitOutcome,
};
use crate::{BindingMutability, Environment, ScopeStack, Status, Value};
use opaal_platform::Platform;

/// The normally completed result of one non-interactive source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptCompletion {
    value: Value,
    status: Option<Status>,
    background_failures: Vec<BackgroundFailure>,
}

impl ScriptCompletion {
    /// The exact final language value retained by the embedding boundary.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }

    /// The final foreground job status, or `None` when no job ran.
    #[must_use]
    pub const fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    /// Joined background failures in ascending job-identity order.
    #[must_use]
    pub fn background_failures(&self) -> &[BackgroundFailure] {
        &self.background_failures
    }
}

/// A source-anchored parse or runtime failure from script execution.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptError {
    rendered: String,
    background_failures: Vec<BackgroundFailure>,
}

impl ScriptError {
    fn submit(error: SubmitError) -> Self {
        let rendered = match error {
            SubmitError::Diagnostic(rendered) => rendered,
            SubmitError::Runtime { rendered, .. } => rendered,
            SubmitError::Output(error) => format!("opaal: output write failed: {error}\n"),
        };
        Self {
            rendered,
            background_failures: Vec::new(),
        }
    }

    fn module_submit(
        error: SubmitError,
        source: &opaal_syntax::SourceFile,
        sources: &ModuleSourceRegistry,
    ) -> Self {
        let SubmitError::Runtime { error, .. } = error else {
            return Self::submit(error);
        };
        let rendered = crate::session::render_runtime_diagnostic(
            source,
            &error,
            sources.entries().map(|entry| entry.source().clone()),
        );
        Self {
            rendered,
            background_failures: Vec::new(),
        }
    }

    /// Render the complete user-facing diagnostic.
    #[must_use]
    pub fn render(&self) -> &str {
        &self.rendered
    }

    /// Joined background failures in ascending job-identity order.
    #[must_use]
    pub fn background_failures(&self) -> &[BackgroundFailure] {
        &self.background_failures
    }

    fn with_background_failures(mut self, failures: Vec<BackgroundFailure>) -> Self {
        self.background_failures = failures;
        self
    }
}

impl fmt::Display for ScriptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl std::error::Error for ScriptError {}

/// The complete structured result of one non-interactive script boundary.
pub type ScriptExecutionOutcome = ExecutionOutcome<ScriptCompletion, ScriptError>;

enum ScriptFailure {
    Error(ScriptError),
    Fatal(FatalHostFailure),
}

impl ScriptFailure {
    fn module_submit(
        error: SubmitError,
        source: &opaal_syntax::SourceFile,
        sources: &ModuleSourceRegistry,
    ) -> Self {
        match error {
            SubmitError::Output(error) => Self::Fatal(FatalHostFailure::new(
                FatalHostFailureKind::Output,
                error.to_string(),
            )),
            other => Self::Error(ScriptError::module_submit(other, source, sources)),
        }
    }
}

/// Executes a fully loaded module program in named-dependency-first order.
///
/// Named dependencies initialize once per canonical module. Each module owns
/// an isolated lexical root seeded with immutable snapshots of its imports;
/// load-only dependencies remain dormant. The root module additionally sees
/// the explicitly supplied immutable `args` list in a synthetic parent frame.
#[allow(clippy::too_many_arguments)]
pub fn execute_module_program_outcome(
    program: &ModuleProgram,
    script_arguments: &[String],
    cwd: &Path,
    environment: &mut Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
    output: &mut dyn Write,
) -> ScriptExecutionOutcome {
    let limits = EvalLimits::pure_opaal(CancellationToken::never(), ResourceBudget::opaal());
    execute_module_program_outcome_with_limits(
        program,
        script_arguments,
        cwd,
        environment,
        registry,
        probe,
        options,
        platform,
        clock,
        output,
        &limits,
    )
}

/// Executes a module program under one cancellation token and shared step budget.
///
/// The same budget crosses statement and module-initialization boundaries.
#[allow(clippy::too_many_arguments)]
pub fn execute_module_program_outcome_with_limits(
    program: &ModuleProgram,
    script_arguments: &[String],
    cwd: &Path,
    environment: &mut Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
    output: &mut dyn Write,
    limits: &EvalLimits,
) -> ScriptExecutionOutcome {
    let structured_outcomes = true;
    let mut session = Session::with_scope_and_registry(
        ScopeStack::new(),
        cwd,
        environment.clone(),
        *options,
        registry.clone(),
    );
    session.enable_script_job_control(Arc::clone(&clock));
    let binding_types = Arc::new(program.runtime_binding_types());
    let mut instances: BTreeMap<ModuleId, BTreeMap<String, Value>> = BTreeMap::new();
    let mut outcome: Result<(SubmitOutcome, Value), ScriptFailure> =
        Ok((SubmitOutcome::Continued, Value::Null));
    let mut budget = limits.resource_budget();

    for module in module_initialization_order(program) {
        let mut scope = ScopeStack::new();
        if &module == program.graph().root() {
            scope
                .declare(
                    "args",
                    BindingMutability::Immutable,
                    Value::list(
                        script_arguments
                            .iter()
                            .cloned()
                            .map(Value::string)
                            .collect(),
                    ),
                )
                .expect("a fresh root input frame has no binding collisions");
            scope.push();
        }
        for import in program.names().imports(&module) {
            let value = instances
                .get(import.target())
                .and_then(|exports| exports.get(import.name()))
                .expect("named dependencies initialize before their importers")
                .clone();
            scope
                .declare(import.name(), BindingMutability::Immutable, value)
                .expect("module name analysis rejects import binding collisions");
        }
        for alias in program.aliases().aliases(&module) {
            declare_qualified_alias_values(
                &mut scope,
                program,
                &instances,
                alias.name(),
                alias.target(),
            );
        }

        let source = program
            .sources()
            .source(&module)
            .expect("a loaded module program registers every source");
        let script = program
            .sources()
            .script(&module)
            .expect("a loaded module program registers every syntax tree");
        match session.submit_module_source(
            source,
            script,
            scope,
            Arc::clone(&binding_types),
            limits,
            &mut budget,
            probe,
            platform,
            clock.as_ref(),
            output,
        ) {
            Ok((SubmitOutcome::Continued, completed_scope, value)) => {
                let is_root = &module == program.graph().root();
                let exports = program
                    .names()
                    .exports(&module)
                    .filter_map(|export| {
                        completed_scope
                            .get(export.name())
                            .cloned()
                            .map(|value| (export.name().to_owned(), value))
                    })
                    .collect();
                instances.insert(module, exports);
                if is_root {
                    outcome = Ok((SubmitOutcome::Continued, value));
                }
            }
            Ok((exit @ SubmitOutcome::Exit(_), _, value)) => {
                outcome = Ok((exit, value));
                break;
            }
            Ok((cancelled @ SubmitOutcome::Cancelled(_), _, value)) => {
                outcome = Ok((cancelled, value));
                break;
            }
            Ok((refused @ SubmitOutcome::Refused(_), _, value)) => {
                outcome = Ok((refused, value));
                break;
            }
            Err(error) => {
                outcome = Err(if structured_outcomes {
                    ScriptFailure::module_submit(error, source, program.sources())
                } else {
                    ScriptFailure::Error(ScriptError::module_submit(
                        error,
                        source,
                        program.sources(),
                    ))
                });
                break;
            }
        }
    }

    finish_script_session_outcome(&mut session, environment, platform, outcome)
}

/// Execute an already validated OPAAL module program through the legacy
/// `Result` adapter.
///
/// New embedding and CLI boundaries should consume
/// [`execute_module_program_outcome`] so cancellation, refusal, and fatal host
/// failure remain distinct. This adapter cannot parse or select a source
/// language; callers must first construct an OPAAL-only [`ModuleProgram`].
#[allow(clippy::too_many_arguments)]
pub fn execute_module_program(
    program: &ModuleProgram,
    script_arguments: &[String],
    cwd: &Path,
    environment: &mut Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
    output: &mut dyn Write,
) -> Result<ScriptCompletion, ScriptError> {
    let (primary, _, _) = execute_module_program_outcome(
        program,
        script_arguments,
        cwd,
        environment,
        registry,
        probe,
        options,
        platform,
        clock,
        output,
    )
    .into_parts();
    match primary {
        PrimaryOutcome::Completed(completion) => Ok(completion),
        PrimaryOutcome::Error(error) => Err(error),
        PrimaryOutcome::Cancelled(cancellation) => Err(ScriptError {
            rendered: format!(
                "opaal: evaluation cancelled ({:?}) at bytes {}..{}\n",
                cancellation.reason(),
                cancellation.span().start(),
                cancellation.span().end()
            ),
            background_failures: Vec::new(),
        }),
        PrimaryOutcome::Refused(refusal) => Err(ScriptError {
            rendered: format!("opaal: {refusal}\n"),
            background_failures: Vec::new(),
        }),
        PrimaryOutcome::FatalHostFailure(failure) => Err(ScriptError {
            rendered: format!("opaal: fatal host {failure}\n"),
            background_failures: Vec::new(),
        }),
    }
}

fn declare_qualified_alias_values(
    scope: &mut ScopeStack,
    program: &ModuleProgram,
    instances: &BTreeMap<ModuleId, BTreeMap<String, Value>>,
    prefix: &str,
    target: &ModuleId,
) {
    if let Some(exports) = instances.get(target) {
        for (name, value) in exports {
            scope
                .declare(
                    format!("{prefix}::{name}"),
                    BindingMutability::Immutable,
                    value.clone(),
                )
                .expect("module alias analysis rejects qualified binding collisions");
        }
    }
    for alias in program.aliases().exports(target) {
        declare_qualified_alias_values(
            scope,
            program,
            instances,
            &format!("{prefix}::{}", alias.name()),
            alias.target(),
        );
    }
}

fn module_initialization_order(program: &ModuleProgram) -> Vec<ModuleId> {
    fn visit(
        program: &ModuleProgram,
        module: &ModuleId,
        initialized: &mut BTreeSet<ModuleId>,
        order: &mut Vec<ModuleId>,
    ) {
        if initialized.contains(module) {
            return;
        }
        if program.sources().script(module).is_none() {
            initialized.insert(module.clone());
            return;
        }
        for import in program.names().imports(module) {
            visit(program, import.target(), initialized, order);
        }
        for alias in program.aliases().aliases(module) {
            visit(program, alias.target(), initialized, order);
        }
        if initialized.insert(module.clone()) {
            order.push(module.clone());
        }
    }

    let mut initialized = BTreeSet::new();
    let mut order = Vec::new();
    visit(
        program,
        program.graph().root(),
        &mut initialized,
        &mut order,
    );
    order
}

fn finish_script_session_outcome(
    session: &mut Session,
    environment: &mut Environment,
    platform: &dyn Platform,
    outcome: Result<(SubmitOutcome, Value), ScriptFailure>,
) -> ScriptExecutionOutcome {
    // Cleanup always runs after the primary-producing evaluation route. Pure
    // opaal currently owns no external resources; later adapters attach their
    // typed cleanup and partial-effect evidence through `ExecutionOutcome`.
    let failures = session.join_background_jobs(platform);
    let primary = match outcome {
        Err(ScriptFailure::Error(error)) => {
            PrimaryOutcome::Error(error.with_background_failures(failures))
        }
        Err(ScriptFailure::Fatal(failure)) => PrimaryOutcome::FatalHostFailure(failure),
        Ok((SubmitOutcome::Cancelled(cancellation), _)) => PrimaryOutcome::Cancelled(cancellation),
        Ok((SubmitOutcome::Refused(refusal), _)) => PrimaryOutcome::Refused(refusal),
        Ok((outcome, value)) => {
            let foreground = match outcome {
                SubmitOutcome::Continued => session.current_status().cloned(),
                SubmitOutcome::Exit(code) => Some(
                    Status::exit(i64::from(code), crate::Duration::ZERO)
                        .expect("an explicit script exit is a valid status"),
                ),
                SubmitOutcome::Cancelled(_) | SubmitOutcome::Refused(_) => {
                    unreachable!("control outcomes are selected before completion")
                }
            };
            let status = background_exit_status(&failures).or(foreground);
            *environment = session.environment().clone();
            PrimaryOutcome::Completed(ScriptCompletion {
                value,
                status,
                background_failures: failures,
            })
        }
    };
    ExecutionOutcome::new(primary, Vec::new())
}

/// The exit status a failing background job imposes on its script.
///
/// The first failure in job-identity order wins. A quarantined record has no
/// aggregate status the platform ever established, so it contributes the
/// generic failure code rather than a status the shell would be inventing.
fn background_exit_status(failures: &[BackgroundFailure]) -> Option<Status> {
    let first = failures.first()?;
    match first.reason() {
        BackgroundFailureReason::Exited(status) => Some(status.clone()),
        BackgroundFailureReason::Observation(_) | BackgroundFailureReason::Signal(_) => {
            Some(Status::exit(1, crate::Duration::ZERO).expect("one is a valid failure status"))
        }
    }
}
