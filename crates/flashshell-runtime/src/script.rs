//! Non-interactive script execution through the persistent session driver.

use std::fmt;
use std::io;
use std::path::Path;

use flashshell_platform::Platform;

use crate::command::CommandRegistry;
use crate::eval::Clock;
use crate::plan::SessionOptions;
use crate::resolve::ExecutableProbe;
use crate::session::{Session, SubmitError, SubmitOutcome};
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
    clock: &dyn Clock,
) -> Result<ScriptCompletion, ScriptError> {
    let mut session = Session::with_scope_and_registry(
        ScopeStack::new(),
        cwd,
        environment.clone(),
        *options,
        registry.clone(),
    );
    let mut output = io::stdout().lock();
    let outcome = session
        .submit(name, text, probe, platform, clock, &mut output)
        .map_err(ScriptError::submit)?;
    let status = match outcome {
        SubmitOutcome::Continued => session.current_status().cloned(),
        SubmitOutcome::Exit(code) => Some(
            Status::exit(i64::from(code), crate::Duration::ZERO)
                .expect("an explicit script exit is a valid status"),
        ),
    };
    *environment = session.environment().clone();
    Ok(ScriptCompletion { status })
}
