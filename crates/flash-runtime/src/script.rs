//! Non-interactive script execution through the persistent session driver.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;

use flash_platform::Platform;
use flash_syntax::SourceFile;

use crate::command::CommandRegistry;
use crate::eval::Clock;
use crate::module::ModuleProgram;
use crate::plan::SessionOptions;
use crate::resolve::ExecutableProbe;
use crate::session::{
    BackgroundFailure, BackgroundFailureReason, Session, SubmitError, SubmitOutcome,
};
use crate::{Environment, ScopeStack, Status};

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
            SubmitError::Output(error) => format!("fsh: output write failed: {error}\n"),
        };
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
        None,
    )
}

/// Executes only the root source of a fully loaded module program.
///
/// Static imports have already been analyzed by [`ModuleProgram`], so they are
/// load-only declarations here. Imported source initialization is not executed.
#[allow(clippy::too_many_arguments)]
pub fn execute_module_program(
    program: &ModuleProgram,
    cwd: &Path,
    environment: &mut Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: Arc<dyn Clock>,
) -> Result<ScriptCompletion, ScriptError> {
    let root = program.graph().root();
    let source = program
        .sources()
        .source(root)
        .expect("a loaded module program registers its root source");
    let script = program
        .sources()
        .script(root)
        .expect("a loaded module program registers its root syntax");
    execute_source(
        source.name(),
        source.text(),
        cwd,
        environment,
        ScopeStack::new(),
        registry,
        probe,
        options,
        platform,
        clock,
        true,
        Some((source, script)),
    )
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
        None,
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
    analyzed_root: Option<(&SourceFile, &flash_syntax::Script)>,
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
    let outcome = if let Some((source, script)) = analyzed_root {
        session.submit_module_root(source, script, probe, platform, clock.as_ref(), &mut output)
    } else {
        session.submit(name, text, probe, platform, clock.as_ref(), &mut output)
    };

    // The join runs on every exit route, including a failing one: a script must
    // not orphan a child because one of its later statements failed.
    let failures = session.join_background_jobs(platform);
    for failure in &failures {
        eprintln!("fsh: {}", failure.render());
    }

    let outcome = outcome.map_err(ScriptError::submit)?;
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
