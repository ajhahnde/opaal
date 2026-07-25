//! Persistent interactive session driver.
//!
//! A [`Session`] evaluates independently submitted edit buffers against one
//! retained scope, environment, logical working directory, and last status, so
//! an interactive client observes the same accumulated state a script would.
//! Each submitted buffer runs its statements in source order: pure statements
//! and `export`/`unset` reuse the shared scope and environment, a single-stage
//! internal command dispatches against the session state, and an external
//! foreground pipeline runs through the ordinary executor. Parse and runtime
//! failures are recoverable and leave the accumulated state untouched; only a
//! failure to write built-in output to the caller's sink is fatal.
//!
//! General mixed internal/external pipeline execution and structured value
//! presentation remain later milestones; a pipeline that is not a lone internal
//! stage is planned and executed as an external pipeline exactly as in script
//! mode.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flashshell_platform::Platform;
use flashshell_syntax::{
    Diagnostic, ParseOutcome, Script, Severity, SourceFile, SourceId, StatementKind, parse,
    render_diagnostic,
};

use crate::builtin::{
    BuiltinOutcome, BuiltinOutput, SessionState, execute_builtin, standard_registry,
};
use crate::command::{Carrier, CommandRegistry};
use crate::eval::{
    Clock, Completion, EvalLimits, RuntimeError, RuntimeErrorKind, evaluate_in_environment,
};
use crate::execute::execute_foreground_status;
use crate::plan::{PlannedResolution, SessionOptions, plan_pipeline_with_options};
use crate::resolve::ExecutableProbe;
use crate::{Environment, ScopeStack, Status, Value};

/// The control decision produced by one submitted edit buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitOutcome {
    /// The session should keep reading; `current_status` holds the last result.
    Continued,
    /// `exit` requested session termination with this host exit code.
    Exit(u8),
}

/// A failure raised while submitting one edit buffer.
#[derive(Debug)]
pub enum SubmitError {
    /// A recoverable parse or runtime failure; the session state is unchanged.
    Diagnostic(String),
    /// A fatal failure to write built-in output to the caller's sink.
    Output(io::Error),
}

impl SubmitError {
    /// The rendered recoverable diagnostic, or an empty string for a fatal
    /// output failure.
    #[must_use]
    pub fn render(&self) -> &str {
        match self {
            Self::Diagnostic(rendered) => rendered,
            Self::Output(_) => "",
        }
    }

    /// Whether this failure ends the session rather than being recoverable.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(self, Self::Output(_))
    }
}

/// Persistent interactive session state and standard command registry.
pub struct Session {
    scope: ScopeStack,
    state: SessionState,
    options: SessionOptions,
    registry: CommandRegistry,
    next_source: u32,
}

impl Session {
    /// Build a session from its initial logical cwd, environment, and options.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, environment: Environment, options: SessionOptions) -> Self {
        Self::with_scope(ScopeStack::new(), cwd, environment, options)
    }

    /// Build a session seeded with an already-established scope.
    ///
    /// Startup configuration commits its bindings into `scope`, so an
    /// interactive client seeds the session with the config transaction result.
    #[must_use]
    pub fn with_scope(
        scope: ScopeStack,
        cwd: impl Into<PathBuf>,
        environment: Environment,
        options: SessionOptions,
    ) -> Self {
        Self {
            scope,
            state: SessionState::new(cwd, environment),
            options,
            registry: standard_registry(),
            next_source: 1,
        }
    }

    /// The retained logical working directory.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        self.state.cwd()
    }

    /// The retained child-process environment.
    #[must_use]
    pub const fn environment(&self) -> &Environment {
        self.state.environment()
    }

    /// The most recent normally completed status, if a job has run.
    #[must_use]
    pub const fn current_status(&self) -> Option<&Status> {
        self.state.current_status()
    }

    /// Evaluate one submitted edit buffer against the retained session state.
    ///
    /// Statements run in source order; built-in textual output is written to
    /// `output`. On a recoverable [`SubmitError::Diagnostic`] the accumulated
    /// scope, environment, cwd, and status remain exactly as they were before
    /// the failing statement.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        name: impl Into<String>,
        text: impl Into<String>,
        probe: &dyn ExecutableProbe,
        platform: &dyn Platform,
        clock: &dyn Clock,
        output: &mut dyn Write,
    ) -> Result<SubmitOutcome, SubmitError> {
        let source = SourceFile::new(SourceId::new(self.next_source), name, text);
        self.next_source = self.next_source.wrapping_add(1);

        let script = match parse(&source) {
            ParseOutcome::Complete(script) => script,
            ParseOutcome::Incomplete(input) => {
                let diagnostic = Diagnostic::new(
                    Severity::Error,
                    "SYN002",
                    format!("incomplete input: {}", input.reason()),
                )
                .with_primary(input.span(), "input ends before this construct is complete");
                return Err(render(&source, &[diagnostic]));
            }
            ParseOutcome::Invalid(diagnostics) => return Err(render(&source, &diagnostics)),
        };

        let Session {
            scope,
            state,
            options,
            registry,
            ..
        } = self;

        for statement in script.statements() {
            match statement.kind() {
                StatementKind::Job(job) => {
                    if job.background_span.is_some() {
                        let error = RuntimeError::new(
                            RuntimeErrorKind::Unsupported {
                                feature: "background job execution",
                            },
                            statement.span(),
                        );
                        return Err(runtime(&source, &error));
                    }
                    let step = run_chain(
                        &job.chain, state, scope, options, registry, &source, probe, platform,
                        clock, output,
                    )
                    .map_err(|interrupt| interrupt.into_submit(&source))?;
                    match step {
                        ChainStep::Exit(code) => return Ok(SubmitOutcome::Exit(code)),
                        ChainStep::Status(status) => state.set_current_status(Some(status)),
                    }
                }
                _ => {
                    let one = Script::new(vec![statement.clone()], statement.span());
                    match evaluate_in_environment(
                        &one,
                        &source,
                        scope,
                        state.environment_mut(),
                        &EvalLimits::default(),
                    )
                    .map_err(|error| runtime(&source, &error))?
                    {
                        Completion::Value(_) => {}
                        Completion::Cancelled(_) => {
                            unreachable!("default evaluation limits never cancel")
                        }
                    }
                }
            }
        }

        Ok(SubmitOutcome::Continued)
    }
}

/// One pipeline's control result inside a conditional chain.
enum ChainStep {
    Status(Status),
    Exit(u8),
}

/// A runtime failure or a fatal output-write failure raised while executing a job.
enum Interrupt {
    Runtime(RuntimeError),
    Output(io::Error),
}

impl Interrupt {
    fn into_submit(self, source: &SourceFile) -> SubmitError {
        match self {
            Self::Runtime(error) => runtime(source, &error),
            Self::Output(error) => SubmitError::Output(error),
        }
    }
}

impl From<RuntimeError> for Interrupt {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_chain(
    chain: &flashshell_syntax::ConditionalChain,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    output: &mut dyn Write,
) -> Result<ChainStep, Interrupt> {
    let mut or_terms = chain.or_terms().iter();
    let first = or_terms
        .next()
        .expect("a parsed conditional chain contains an operand");
    let mut step = run_and_chain(
        first, state, scope, options, registry, source, probe, platform, clock, output,
    )?;
    for and_chain in or_terms {
        match &step {
            ChainStep::Exit(_) => return Ok(step),
            // `||` runs the next operand only when the current one succeeded not.
            ChainStep::Status(status) if status.is_ok() => break,
            ChainStep::Status(_) => {}
        }
        step = run_and_chain(
            and_chain, state, scope, options, registry, source, probe, platform, clock, output,
        )?;
    }
    Ok(step)
}

#[allow(clippy::too_many_arguments)]
fn run_and_chain(
    chain: &flashshell_syntax::AndChain,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    output: &mut dyn Write,
) -> Result<ChainStep, Interrupt> {
    let mut pipelines = chain.and_terms().iter();
    let first = pipelines
        .next()
        .expect("a parsed and-chain contains an operand");
    let mut step = run_pipeline(
        first, state, scope, options, registry, source, probe, platform, clock, output,
    )?;
    for pipeline in pipelines {
        match &step {
            ChainStep::Exit(_) => return Ok(step),
            // `&&` runs the next operand only while the current one succeeds.
            ChainStep::Status(status) if !status.is_ok() => break,
            ChainStep::Status(_) => {}
        }
        step = run_pipeline(
            pipeline, state, scope, options, registry, source, probe, platform, clock, output,
        )?;
    }
    Ok(step)
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    pipeline: &flashshell_syntax::Pipeline,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    output: &mut dyn Write,
) -> Result<ChainStep, Interrupt> {
    let plan = plan_pipeline_with_options(
        pipeline,
        state.cwd(),
        source,
        scope,
        state.environment(),
        registry,
        probe,
        options,
    )?;

    // A lone internal stage dispatches against the mutable session state; every
    // other shape runs as an external pipeline, exactly as in script mode.
    if plan.edges().is_empty()
        && plan.stages().len() == 1
        && matches!(
            plan.stages()[0].resolution(),
            PlannedResolution::Internal { .. }
        )
    {
        let upstream = state.current_status().cloned();
        let outcome = execute_builtin(
            &plan.stages()[0],
            Carrier::Empty,
            upstream.as_ref(),
            state,
            registry,
            probe,
            platform,
        )?;
        return match outcome {
            BuiltinOutcome::Exit(request) => Ok(ChainStep::Exit(request.code())),
            BuiltinOutcome::Completed(completion) => {
                render_output(completion.output(), output).map_err(Interrupt::Output)?;
                Ok(ChainStep::Status(completion.status().clone()))
            }
            BuiltinOutcome::External(_) => Err(Interrupt::Runtime(RuntimeError::new(
                RuntimeErrorKind::Unsupported {
                    feature: "the command built-in in interactive sessions",
                },
                plan.stages()[0].span(),
            ))),
        };
    }

    let status = execute_foreground_status(&plan, platform, clock)?;
    Ok(ChainStep::Status(status))
}

/// Write a completed built-in's textual output, one value per line.
///
/// Structured presentation (tables and typed rendering) is a later milestone;
/// this renders scalar and stream values through their human display form so a
/// command like `pwd` is not silent.
fn render_output(output: &BuiltinOutput, sink: &mut dyn Write) -> io::Result<()> {
    match output {
        BuiltinOutput::Empty | BuiltinOutput::ForwardInput(_) => Ok(()),
        BuiltinOutput::Value(value) => write_value(value, sink),
        BuiltinOutput::ValueStream(values) => {
            for value in values {
                write_value(value, sink)?;
            }
            Ok(())
        }
    }
}

fn write_value(value: &Value, sink: &mut dyn Write) -> io::Result<()> {
    writeln!(sink, "{value}")
}

fn render(source: &SourceFile, diagnostics: &[Diagnostic]) -> SubmitError {
    let rendered = diagnostics
        .iter()
        .map(|diagnostic| {
            render_diagnostic(source, diagnostic)
                .expect("diagnostics always carry source-local primary spans")
        })
        .collect::<String>();
    SubmitError::Diagnostic(rendered)
}

fn runtime(source: &SourceFile, error: &RuntimeError) -> SubmitError {
    let diagnostic = Diagnostic::new(Severity::Error, "RUN001", error.to_string())
        .with_primary(error.span(), "runtime failure");
    render(source, &[diagnostic])
}
