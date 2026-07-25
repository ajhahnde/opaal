//! Non-interactive script parsing and external foreground execution.

use std::fmt;
use std::path::Path;

use flashshell_platform::Platform;
use flashshell_syntax::{
    Diagnostic, IncompleteInput, ParseOutcome, Severity, SourceFile, SourceId, StatementKind,
    parse, render_diagnostic,
};

use crate::command::CommandRegistry;
use crate::eval::{Clock, Completion, EvalLimits, RuntimeError, evaluate_in_environment};
use crate::execute::execute_foreground_chain;
use crate::plan::SessionOptions;
use crate::resolve::ExecutableProbe;
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
    fn incomplete(source: &SourceFile, input: IncompleteInput) -> Self {
        let diagnostic = Diagnostic::new(
            Severity::Error,
            "SYN002",
            format!("incomplete input: {}", input.reason()),
        )
        .with_primary(
            input.span(),
            "source ends before this construct is complete",
        );
        Self::diagnostics(source, &[diagnostic])
    }

    fn diagnostics(source: &SourceFile, diagnostics: &[Diagnostic]) -> Self {
        let rendered = diagnostics
            .iter()
            .map(|diagnostic| {
                render_diagnostic(source, diagnostic)
                    .expect("parser diagnostics always carry source-local primary spans")
            })
            .collect::<Vec<_>>()
            .join("");
        Self { rendered }
    }

    fn runtime(source: &SourceFile, error: &RuntimeError) -> Self {
        let diagnostic = Diagnostic::new(Severity::Error, "RUN001", error.to_string())
            .with_primary(error.span(), "runtime failure");
        Self::diagnostics(source, &[diagnostic])
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
/// Pure statements and environment mutations reuse one scope and environment.
/// Foreground jobs use the external pipeline executor; internal stages remain a
/// precise unsupported runtime form until mixed-carrier execution is available.
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
    let source = SourceFile::new(SourceId::new(1), name, text);
    let script = match parse(&source) {
        ParseOutcome::Complete(script) => script,
        ParseOutcome::Incomplete(input) => return Err(ScriptError::incomplete(&source, input)),
        ParseOutcome::Invalid(diagnostics) => {
            return Err(ScriptError::diagnostics(&source, &diagnostics));
        }
    };

    let mut scope = ScopeStack::new();
    let mut status = None;
    for statement in script.statements() {
        match statement.kind() {
            StatementKind::Job(job) => {
                if job.background_span.is_some() {
                    let error = RuntimeError::new(
                        crate::eval::RuntimeErrorKind::Unsupported {
                            feature: "background job execution",
                        },
                        statement.span(),
                    );
                    return Err(ScriptError::runtime(&source, &error));
                }
                status = Some(
                    execute_foreground_chain(
                        &job.chain,
                        cwd,
                        &source,
                        &mut scope,
                        environment,
                        registry,
                        probe,
                        options,
                        platform,
                        clock,
                    )
                    .map_err(|error| ScriptError::runtime(&source, &error))?,
                );
            }
            _ => {
                let one = flashshell_syntax::Script::new(vec![statement.clone()], statement.span());
                match evaluate_in_environment(
                    &one,
                    &source,
                    &mut scope,
                    environment,
                    &EvalLimits::default(),
                )
                .map_err(|error| ScriptError::runtime(&source, &error))?
                {
                    Completion::Value(_) => {}
                    Completion::Cancelled(_) => {
                        unreachable!("default evaluation limits never cancel")
                    }
                }
            }
        }
    }

    Ok(ScriptCompletion { status })
}
