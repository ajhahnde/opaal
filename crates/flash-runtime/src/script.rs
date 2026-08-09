//! Non-interactive script execution through the persistent session driver.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use flash_platform::Platform;
use flash_syntax::{Diagnostic, Severity, render_diagnostic_sources};

use crate::command::CommandRegistry;
use crate::eval::Clock;
use crate::module::{ModuleId, ModuleProgram, ModuleSourceRegistry};
use crate::plan::SessionOptions;
use crate::resolve::ExecutableProbe;
use crate::session::{
    BackgroundFailure, BackgroundFailureReason, Session, SubmitError, SubmitOutcome,
};
use crate::{BindingMutability, Environment, ScopeStack, Status, Value};

/// The normally completed result of one non-interactive source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptCompletion {
    status: Option<Status>,
}

impl ScriptCompletion {
    /// The final foreground job status, or `None` when no job ran.
    #[must_use]
    pub const fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }
}

/// A source-anchored parse or runtime failure from script execution.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptError {
    rendered: String,
}

impl ScriptError {
    fn submit(error: SubmitError) -> Self {
        let rendered = match error {
            SubmitError::Diagnostic(rendered) => rendered,
            SubmitError::Runtime { rendered, .. } => rendered,
            SubmitError::Output(error) => format!("fsh: output write failed: {error}\n"),
        };
        Self { rendered }
    }

    fn module_submit(error: SubmitError, sources: &ModuleSourceRegistry) -> Self {
        let SubmitError::Runtime { error, .. } = error else {
            return Self::submit(error);
        };
        let mut diagnostic = Diagnostic::new(Severity::Error, "RUN001", error.to_string())
            .with_primary(error.span(), "runtime failure");
        for frame in error.frames() {
            diagnostic = diagnostic.with_secondary(frame.call_site(), "called from here");
        }
        let rendered =
            render_diagnostic_sources(sources.entries().map(|entry| entry.source()), &diagnostic)
                .expect("module runtime diagnostics reference retained program sources");
        Self { rendered }
    }

    /// Render the complete user-facing diagnostic.
    #[must_use]
    pub fn render(&self) -> &str {
        &self.rendered
    }
}

impl fmt::Display for ScriptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.rendered)
    }
}

impl std::error::Error for ScriptError {}

/// Parse and execute one source file in statement order.
///
/// Pure statements, environment mutations, internal commands, external
/// processes, and mixed byte boundaries all reuse the same stateful execution
/// path as interactive submissions.
#[allow(clippy::too_many_arguments)]
pub fn execute_script(
    name: impl Into<String>,
    text: impl Into<String>,
    cwd: &Path,
    environment: &mut Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
) -> Result<ScriptCompletion, ScriptError> {
    execute_source(
        name,
        text,
        cwd,
        environment,
        ScopeStack::new(),
        registry,
        probe,
        options,
        platform,
        clock,
        true,
    )
}

/// Executes a fully loaded module program in named-dependency-first order.
///
/// Named dependencies initialize once per canonical module. Each module owns
/// an isolated lexical root seeded with immutable snapshots of its imports;
/// load-only dependencies remain dormant. The root module additionally sees
/// the explicitly supplied immutable `args` list in a synthetic parent frame.
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
) -> Result<ScriptCompletion, ScriptError> {
    let mut session = Session::with_scope_and_registry(
        ScopeStack::new(),
        cwd,
        environment.clone(),
        *options,
        registry.clone(),
    );
    session.enable_script_job_control(Arc::clone(&clock));
    let mut output = io::stdout().lock();
    let mut instances: BTreeMap<ModuleId, BTreeMap<String, Value>> = BTreeMap::new();
    let mut outcome: Result<SubmitOutcome, ScriptError> = Ok(SubmitOutcome::Continued);

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
            probe,
            platform,
            clock.as_ref(),
            &mut output,
        ) {
            Ok((SubmitOutcome::Continued, completed_scope)) => {
                let exports = program
                    .names()
                    .exports(&module)
                    .map(|export| {
                        let value = completed_scope
                            .get(export.name())
                            .expect("an analyzed export names a completed root binding")
                            .clone();
                        (export.name().to_owned(), value)
                    })
                    .collect();
                instances.insert(module, exports);
            }
            Ok((exit @ SubmitOutcome::Exit(_), _)) => {
                outcome = Ok(exit);
                break;
            }
            Err(error) => {
                outcome = Err(ScriptError::module_submit(error, program.sources()));
                break;
            }
        }
    }

    finish_script_session(&mut session, environment, platform, outcome)
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
        for import in program.names().imports(module) {
            visit(program, import.target(), initialized, order);
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

/// Execute one isolated conditional chain without creating a background-job
/// coordinator.
///
/// Environment entries become the child's complete immutable lexical seed, and
/// every external process inherits the caller's process group.
#[allow(clippy::too_many_arguments)]
pub fn execute_chain_subshell(
    name: impl Into<String>,
    text: impl Into<String>,
    cwd: &Path,
    environment: &mut Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
) -> Result<ScriptCompletion, ScriptError> {
    let options = options.inherit_process_group();
    let scope = ScopeStack::from_environment(environment);
    execute_source(
        name,
        text,
        cwd,
        environment,
        scope,
        registry,
        probe,
        &options,
        platform,
        clock,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_source(
    name: impl Into<String>,
    text: impl Into<String>,
    cwd: &Path,
    environment: &mut Environment,
    scope: ScopeStack,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
    enable_background_jobs: bool,
) -> Result<ScriptCompletion, ScriptError> {
    let mut session = Session::with_scope_and_registry(
        scope,
        cwd,
        environment.clone(),
        *options,
        registry.clone(),
    );
    if enable_background_jobs {
        session.enable_script_job_control(Arc::clone(&clock));
    }
    let mut output = io::stdout().lock();
    let outcome = session
        .submit(name, text, probe, platform, clock.as_ref(), &mut output)
        .map_err(ScriptError::submit);

    finish_script_session(&mut session, environment, platform, outcome)
}

fn finish_script_session(
    session: &mut Session,
    environment: &mut Environment,
    platform: &dyn Platform,
    outcome: Result<SubmitOutcome, ScriptError>,
) -> Result<ScriptCompletion, ScriptError> {
    // The join runs on every exit route, including a failing one: a script must
    // not orphan a child because one of its later statements failed.
    let failures = session.join_background_jobs(platform);
    for failure in &failures {
        eprintln!("fsh: {}", failure.render());
    }

    let outcome = outcome?;
    let foreground = match outcome {
        SubmitOutcome::Continued => session.current_status().cloned(),
        SubmitOutcome::Exit(code) => Some(
            Status::exit(i64::from(code), crate::Duration::ZERO)
                .expect("an explicit script exit is a valid status"),
        ),
    };
    let status = background_exit_status(&failures).or(foreground);
    *environment = session.environment().clone();
    Ok(ScriptCompletion { status })
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
