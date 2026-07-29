//! Persistent interactive session driver.
//!
//! A [`Session`] evaluates independently submitted edit buffers against one
//! retained scope, environment, logical working directory, and last status, so
//! an interactive client observes the same accumulated state a script would.
//! Each submitted buffer runs its statements in source order: pure statements
//! and `export`/`unset` reuse the shared scope and environment, an all-internal
//! pipeline moves owned structured carriers through the internal executor, and
//! an external foreground pipeline runs through the ordinary byte executor,
//! and one contiguous internal island streams through owned process pipes.
//! Parse and runtime failures are recoverable and leave the accumulated state
//! untouched; only a failure to write built-in output to the caller's sink is
//! fatal.
//!
//! Human presentation is selected only for a final structured carrier at an
//! unredirected interactive output terminal. An all-external pipeline retains
//! the existing process executor.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flashshell_platform::{
    DescriptorEndpoint, DescriptorReadError, DescriptorWriteError, JobSignal, Platform,
    ProcessGroupId,
};
use flashshell_syntax::{
    Closure, CommandHeadKind, CommandItemKind, ConditionalChain, Diagnostic, Expression,
    ExpressionKind, LiteralKind, ParseOutcome, RecordKey, RedirectionKind, Script, Severity,
    SourceFile, SourceId, Span, StageKind, StatementKind, Word, WordPart, WordPartKind, parse,
    render_diagnostic,
};

use crate::background::{BackgroundJobs, QuarantinePolicy, escape_job_label};
use crate::builtin::{SessionState, standard_registry};
use crate::closure::OwnedClosureContext;
use crate::command::CommandRegistry;
use crate::eval::{
    Clock, Completion, EvalLimits, ExpandedWord, RuntimeError, RuntimeErrorKind,
    evaluate_in_environment, expand_word,
};
use crate::execute::{execute_foreground_status, start_mixed_pipeline};
use crate::internal::{
    InternalPayload, InternalPipelineOutcome, StageOutcome, execute_internal_pipeline,
    execute_internal_suffix, execute_stage,
};
use crate::job::JobPlacement;
use crate::plan::{
    ExecutionPlan, PlannedResolution, PlannedStage, RedirectionAction, SessionOptions,
    plan_pipeline_with_options, preflight,
};
use crate::presentation::{
    OutputDestination, TerminalPresentation, render_table, select_terminal_presentation,
};
use crate::resolve::ExecutableProbe;
use crate::stream::{BytePull, ByteStream, StreamPull, ValueStream};
use crate::{Duration, Environment, Record, ScopeStack, Status, Value};

pub use crate::background::{
    BackgroundFailure, BackgroundFailureReason, JobCommandError, JobNotice, JobNoticeError,
    JobNoticeId, JobNoticeKind, JobSnapshot, JobSnapshotState, LiveJob, LiveJobState,
};

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
    jobs: Option<BackgroundJobs>,
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
        Self::with_scope_and_registry(scope, cwd, environment, options, standard_registry())
    }

    /// Build a session with an explicit command registry.
    ///
    /// Script execution uses the same persistent driver as interactive input
    /// while retaining the registry selected by its caller.
    #[must_use]
    pub fn with_scope_and_registry(
        scope: ScopeStack,
        cwd: impl Into<PathBuf>,
        environment: Environment,
        options: SessionOptions,
        registry: CommandRegistry,
    ) -> Self {
        Self {
            scope,
            state: SessionState::new(cwd, environment),
            options,
            registry,
            next_source: 1,
            jobs: None,
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

    /// Enable the interactive background-job coordinator with one owned clock.
    pub fn enable_interactive_job_control(&mut self, clock: Arc<dyn Clock>) {
        if self.jobs.is_none() {
            self.jobs = Some(BackgroundJobs::new(clock));
        }
    }

    /// Enable the background-job coordinator for a non-interactive session.
    ///
    /// The coordinator is identical; only the rendering boundary differs. A
    /// script has no prompt, so it drains notices once, at its join.
    pub fn enable_script_job_control(&mut self, clock: Arc<dyn Clock>) {
        if self.jobs.is_none() {
            self.jobs = Some(BackgroundJobs::new(clock));
        }
    }

    /// Apply every background observation already queued, without blocking.
    ///
    /// Call before asking which jobs are live: an observation that has arrived
    /// but not been applied still reads as a running job.
    pub fn refresh_background_jobs(&mut self) {
        if let Some(jobs) = self.jobs.as_mut() {
            jobs.apply_pending_observations();
        }
    }

    /// Every background job that has not reached a terminal aggregate.
    #[must_use]
    pub fn live_background_jobs(&self) -> Vec<LiveJob> {
        self.jobs
            .as_ref()
            .map(BackgroundJobs::live_jobs)
            .unwrap_or_default()
    }

    /// Resume, wait, and drain every background job this session started.
    ///
    /// Returns one entry per failing job in job-identity order. A successful
    /// job produces no entry and no output.
    pub fn join_background_jobs(&mut self, platform: &dyn Platform) -> Vec<BackgroundFailure> {
        let Some(jobs) = self.jobs.as_mut() else {
            return Vec::new();
        };
        let failures = jobs.wait_for_quiescence(platform);
        // Draining moves every completion through `Notified` and `Reaped`, so a
        // mode that never reaches a prompt still runs the whole lifecycle.
        jobs.drain_notices();
        failures
    }

    /// Resume, hang up, wait, and drain every background job before exit.
    ///
    /// The asymmetry with the join is deliberate: an interactive loop has
    /// already rendered its pending notices through the editor, so the drain
    /// here only clears what the hang-up itself produced.
    pub fn hang_up_background_jobs(&mut self, platform: &dyn Platform) -> Vec<BackgroundFailure> {
        let Some(jobs) = self.jobs.as_mut() else {
            return Vec::new();
        };
        let failures = jobs.hang_up_all(platform);
        jobs.drain_notices();
        failures
    }

    /// Peek the next structured job notice without acknowledging it.
    pub fn next_job_notice(&mut self) -> Option<JobNotice> {
        self.jobs.as_mut().and_then(BackgroundJobs::next_notice)
    }

    /// Acknowledge one successfully rendered job notice.
    pub fn acknowledge_job_notice(&mut self, notice: JobNoticeId) -> Result<(), JobNoticeError> {
        self.jobs
            .as_mut()
            .ok_or(JobNoticeError::NotPending { notice })?
            .acknowledge(notice)
    }

    /// Inspect one addressable background job.
    #[must_use]
    pub fn background_job(&self, job: crate::job::JobId) -> Option<&crate::job::Job> {
        self.jobs.as_ref().and_then(|jobs| jobs.job(job))
    }

    /// Inspect one completed aggregate while its notice remains unacknowledged.
    #[must_use]
    pub fn background_completion(&self, job: crate::job::JobId) -> Option<&Status> {
        self.jobs.as_ref().and_then(|jobs| jobs.completion(job))
    }

    /// Inspect the process group retained for one addressable background job.
    #[must_use]
    pub fn background_group(&self, job: crate::job::JobId) -> Option<ProcessGroupId> {
        self.jobs.as_ref().and_then(|jobs| jobs.group(job))
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
            jobs,
            ..
        } = self;

        for statement in script.statements() {
            match statement.kind() {
                StatementKind::Job(job) => {
                    if let Some(background_span) = job.background_span {
                        let Some(jobs) = jobs.as_mut() else {
                            let error = RuntimeError::new(
                                RuntimeErrorKind::Unsupported {
                                    feature: "background execution in a session without job control",
                                },
                                background_span,
                            );
                            return Err(runtime(&source, &error));
                        };
                        if let Err(error) = validate_background_chain(
                            &job.chain,
                            &source,
                            scope,
                            state.environment(),
                        ) {
                            return Err(runtime(&source, &error));
                        }
                        let mut child_scope = ScopeStack::from_environment(state.environment());
                        let direct_pipeline =
                            one_background_pipeline(&job.chain).filter(|pipeline| {
                                pipeline_is_all_external(
                                    pipeline,
                                    &source,
                                    &mut child_scope,
                                    registry,
                                )
                            });
                        let plan = if let Some(pipeline) = direct_pipeline {
                            plan_pipeline_with_options(
                                pipeline,
                                state.cwd(),
                                &source,
                                &mut child_scope,
                                state.environment(),
                                registry,
                                probe,
                                options,
                            )
                            .map_err(|error| runtime(&source, &error))?
                        } else {
                            background_shell_plan(
                                &job.chain,
                                &source,
                                state.cwd(),
                                state.environment(),
                                options,
                                platform,
                            )
                            .map_err(|error| runtime(&source, &error))?
                        };
                        let command = source
                            .slice(job.chain.span())
                            .map(escape_job_label)
                            .expect("a parsed chain span belongs to its source");
                        jobs.start(&plan, platform, command)
                            .map_err(|error| runtime(&source, &error))?;
                        state.set_current_status(Some(
                            Status::exit(0, crate::Duration::ZERO)
                                .expect("zero is a valid launch status"),
                        ));
                        continue;
                    }
                    let step = run_chain(
                        &job.chain, state, scope, options, registry, &source, probe, platform,
                        clock, jobs, output,
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

fn validate_background_chain(
    chain: &ConditionalChain,
    source: &SourceFile,
    scope: &ScopeStack,
    environment: &Environment,
) -> Result<(), RuntimeError> {
    BackgroundChainValidator {
        source,
        scope,
        environment,
        local_bindings: Vec::new(),
    }
    .chain(chain)
}

struct BackgroundChainValidator<'a> {
    source: &'a SourceFile,
    scope: &'a ScopeStack,
    environment: &'a Environment,
    local_bindings: Vec<String>,
}

impl BackgroundChainValidator<'_> {
    fn chain(&mut self, chain: &ConditionalChain) -> Result<(), RuntimeError> {
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                for stage in pipeline.stages() {
                    match stage.kind() {
                        StageKind::Command(command) => {
                            self.word(command.head.word())?;
                            if command.head.kind() != CommandHeadKind::ForcedExternal {
                                self.command_head_function(command.head.word())?;
                            }
                            for item in &command.items {
                                match item.kind() {
                                    CommandItemKind::Word(word) => self.word(word)?,
                                    CommandItemKind::Spread(variable) => {
                                        self.variable(variable.name.span(), variable.span)?;
                                    }
                                    CommandItemKind::Closure(closure) => {
                                        self.closure(closure)?;
                                    }
                                    CommandItemKind::Redirection(redirection) => {
                                        self.redirection(redirection.kind())?;
                                    }
                                }
                            }
                        }
                        StageKind::Expression(expression) => self.expression(expression)?,
                    }
                }
            }
        }
        Ok(())
    }

    fn command_head_function(&self, word: &Word) -> Result<(), RuntimeError> {
        let mut scope = self.scope.clone();
        let Ok(expanded) = crate::eval::expand_word(word, self.source, &mut scope) else {
            return Ok(());
        };
        let Some(name) = expanded.value().to_str() else {
            return Ok(());
        };
        if self.is_shell_function(name) {
            return Err(self.shell_function(word.span()));
        }
        Ok(())
    }

    fn word(&mut self, word: &Word) -> Result<(), RuntimeError> {
        for part in word.parts() {
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&mut self, part: &WordPart) -> Result<(), RuntimeError> {
        match part.kind() {
            WordPartKind::Variable(identifier) => self.variable(identifier.span(), part.span()),
            WordPartKind::DoubleQuoted(parts) => {
                for part in parts {
                    self.word_part(part)?;
                }
                Ok(())
            }
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(chain) => self.chain(chain),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape => Ok(()),
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind) -> Result<(), RuntimeError> {
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => Ok(()),
        }
    }

    fn expression(&mut self, expression: &Expression) -> Result<(), RuntimeError> {
        match expression.kind() {
            ExpressionKind::Literal(literal) => {
                if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
                    for part in parts {
                        self.word_part(part)?;
                    }
                }
                Ok(())
            }
            ExpressionKind::Variable(variable) => {
                self.variable(variable.name.span(), variable.span)
            }
            ExpressionKind::Symbol(_) => Ok(()),
            ExpressionKind::List(elements) => {
                for element in elements {
                    self.expression(element)?;
                }
                Ok(())
            }
            ExpressionKind::Record(entries) => {
                for entry in entries {
                    if let RecordKey::DoubleQuoted(part) = &entry.key {
                        self.word_part(part)?;
                    }
                    self.expression(&entry.value)?;
                }
                Ok(())
            }
            ExpressionKind::Closure(closure) => self.closure(closure),
            ExpressionKind::CommandSubstitution(chain) | ExpressionKind::GroupedJob(chain) => {
                self.chain(chain)
            }
            ExpressionKind::Call(call) => {
                if let ExpressionKind::Symbol(identifier) = call.callee.kind() {
                    let name = self.text(identifier.span());
                    if !self.is_local(name) && self.is_shell_function(name) {
                        return Err(self.shell_function(call.callee.span()));
                    }
                }
                self.expression(&call.callee)?;
                for argument in &call.arguments {
                    self.expression(argument)?;
                }
                Ok(())
            }
            ExpressionKind::Index(index) => {
                self.expression(&index.target)?;
                self.expression(&index.index)
            }
            ExpressionKind::Member(member) => self.expression(&member.target),
            ExpressionKind::Unary(unary) => self.expression(&unary.operand),
            ExpressionKind::Binary(binary) => {
                self.expression(&binary.left)?;
                self.expression(&binary.right)
            }
        }
    }

    fn closure(&mut self, closure: &Closure) -> Result<(), RuntimeError> {
        let outer_count = self.local_bindings.len();
        for parameter in &closure.parameters {
            self.local_bindings
                .push(self.text(parameter.name.span()).to_owned());
        }
        let result = self.chain(&closure.body);
        self.local_bindings.truncate(outer_count);
        result
    }

    fn variable(&self, name_span: Span, reference_span: Span) -> Result<(), RuntimeError> {
        let name = self.text(name_span);
        if self.is_local(name) {
            return Ok(());
        }
        if let Some(Value::Callable(callable)) = self.scope.get(name) {
            return Err(if callable.family() == "function" {
                self.shell_function(reference_span)
            } else {
                self.unavailable_binding(reference_span)
            });
        }
        if self.environment.contains(name) {
            return Ok(());
        }
        Err(self.unavailable_binding(reference_span))
    }

    fn is_local(&self, name: &str) -> bool {
        self.local_bindings.iter().rev().any(|local| local == name)
    }

    fn is_shell_function(&self, name: &str) -> bool {
        matches!(
            self.scope.get(name),
            Some(Value::Callable(callable)) if callable.family() == "function"
        )
    }

    fn unavailable_binding(&self, span: Span) -> RuntimeError {
        RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "a binding unavailable in the background subshell environment",
            },
            span,
        )
    }

    fn shell_function(&self, span: Span) -> RuntimeError {
        RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "a shell function in a background subshell",
            },
            span,
        )
    }

    fn text(&self, span: Span) -> &str {
        self.source
            .slice(span)
            .expect("a parsed syntax span belongs to its source")
    }
}

fn one_background_pipeline(
    chain: &flashshell_syntax::ConditionalChain,
) -> Option<&flashshell_syntax::Pipeline> {
    if !chain.operators().is_empty() {
        return None;
    }
    let and_chain = chain
        .or_terms()
        .first()
        .expect("a parsed conditional chain contains an operand");
    if !and_chain.operators().is_empty() {
        return None;
    }
    Some(
        and_chain
            .and_terms()
            .first()
            .expect("a parsed and-chain contains an operand"),
    )
}

fn pipeline_is_all_external(
    pipeline: &flashshell_syntax::Pipeline,
    source: &SourceFile,
    scope: &mut ScopeStack,
    registry: &CommandRegistry,
) -> bool {
    pipeline.stages().iter().all(|stage| {
        let StageKind::Command(command) = stage.kind() else {
            return false;
        };
        if command.head.kind() == CommandHeadKind::ForcedExternal {
            return true;
        }
        let Ok(head) = expand_word(command.head.word(), source, scope) else {
            return false;
        };
        let Some(name) = head.value().to_str() else {
            return true;
        };
        !registry.contains(name)
    })
}

fn background_shell_plan(
    chain: &ConditionalChain,
    source: &SourceFile,
    cwd: &Path,
    environment: &Environment,
    options: &SessionOptions,
    platform: &dyn Platform,
) -> Result<ExecutionPlan, RuntimeError> {
    let span = chain.span();
    let executable = platform
        .shell_executable()
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::ShellExecutable(error), span))?;
    let text = source
        .slice(span)
        .expect("a parsed chain span belongs to its source");

    let mut argv = vec![
        ExpandedWord::synthetic(executable.as_os_str().to_os_string(), span),
        ExpandedWord::synthetic(OsString::from("--async-chain"), span),
        ExpandedWord::synthetic(OsString::from(text), span),
    ];
    if options.pipefail() {
        argv.push(ExpandedWord::synthetic(
            OsString::from("--async-pipefail"),
            span,
        ));
    }
    argv.extend([
        ExpandedWord::synthetic(OsString::from("--async-capture-limit"), span),
        ExpandedWord::synthetic(OsString::from(options.capture_limit().to_string()), span),
    ]);

    Ok(ExecutionPlan::single_external(
        executable,
        argv,
        cwd.to_owned(),
        environment.clone(),
        options.pipefail(),
        options.capture_limit(),
        span,
    ))
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
    jobs: &mut Option<BackgroundJobs>,
    output: &mut dyn Write,
) -> Result<ChainStep, Interrupt> {
    let mut or_terms = chain.or_terms().iter();
    let first = or_terms
        .next()
        .expect("a parsed conditional chain contains an operand");
    let mut step = run_and_chain(
        first, state, scope, options, registry, source, probe, platform, clock, jobs, output,
    )?;
    for and_chain in or_terms {
        match &step {
            ChainStep::Exit(_) => return Ok(step),
            // `||` runs the next operand only when the current one succeeded not.
            ChainStep::Status(status) if status.is_ok() => break,
            ChainStep::Status(_) => {}
        }
        step = run_and_chain(
            and_chain, state, scope, options, registry, source, probe, platform, clock, jobs,
            output,
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
    jobs: &mut Option<BackgroundJobs>,
    output: &mut dyn Write,
) -> Result<ChainStep, Interrupt> {
    let mut pipelines = chain.and_terms().iter();
    let first = pipelines
        .next()
        .expect("a parsed and-chain contains an operand");
    let mut step = run_pipeline(
        first, state, scope, options, registry, source, probe, platform, clock, jobs, output,
    )?;
    for pipeline in pipelines {
        match &step {
            ChainStep::Exit(_) => return Ok(step),
            // `&&` runs the next operand only while the current one succeeds.
            ChainStep::Status(status) if !status.is_ok() => break,
            ChainStep::Status(_) => {}
        }
        step = run_pipeline(
            pipeline, state, scope, options, registry, source, probe, platform, clock, jobs, output,
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
    jobs: &mut Option<BackgroundJobs>,
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
    validate_job_builtin_arguments(&plan)?;

    if let Some((command, stage)) = sole_job_command(&plan) {
        let status = execute_job_builtin(command, stage, jobs, platform)?;
        return Ok(ChainStep::Status(status));
    }

    if plan
        .stages()
        .iter()
        .all(|stage| matches!(stage.resolution(), PlannedResolution::Internal { .. }))
    {
        let final_stage = plan
            .stages()
            .last()
            .expect("a parsed pipeline contains at least one stage");
        let destination = output_destination(final_stage, platform)?;
        let presentation = select_terminal_presentation(final_stage.output_carrier(), destination)
            .map_err(|error| {
                RuntimeError::new(RuntimeErrorKind::Presentation(error), final_stage.span())
            })?;
        // Internal execution is transactional through final lazy rendering. A
        // producer can fail only when the sink pulls it, so committing session
        // state before presentation would leave a successful status behind a
        // failed pipeline.
        let mut pending_state = state.clone();
        let outcome = if is_job_table_head(&plan) {
            execute_job_table_pipeline(
                &plan,
                &mut pending_state,
                registry,
                probe,
                platform,
                source,
                jobs,
            )?
        } else {
            execute_internal_pipeline(&plan, &mut pending_state, registry, probe, platform, source)?
        };
        return match outcome {
            InternalPipelineOutcome::Exit(code) => {
                *state = pending_state;
                Ok(ChainStep::Exit(code))
            }
            InternalPipelineOutcome::Completed {
                payload,
                status,
                closure_context,
            } => {
                render_payload(payload, presentation.as_ref(), final_stage.span(), output)?;
                *pending_state.environment_mut() = closure_context.environment_snapshot();
                *state = pending_state;
                Ok(ChainStep::Status(status))
            }
        };
    }

    if plan
        .stages()
        .iter()
        .any(|stage| matches!(stage.resolution(), PlannedResolution::Internal { .. }))
    {
        return run_mixed_pipeline(
            &plan, state, registry, source, probe, platform, clock, output,
        );
    }

    let status = execute_foreground_status(&plan, platform, clock)?;
    Ok(ChainStep::Status(status))
}

fn validate_job_builtin_arguments(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    let has_external = plan
        .stages()
        .iter()
        .any(|stage| !matches!(stage.resolution(), PlannedResolution::Internal { .. }));
    for stage in plan.stages() {
        let PlannedResolution::Internal { name } = stage.resolution() else {
            continue;
        };
        let Some(command) = job_command_name(name.as_str()) else {
            continue;
        };
        // Job commands run against the session-owned coordinator, which the
        // mixed executor cannot reach. Refusing the plan keeps the boundary a
        // diagnostic instead of a half-executed pipeline.
        if has_external {
            return Err(RuntimeError::new(
                RuntimeErrorKind::JobControlNotInternal { command },
                stage.span(),
            ));
        }
        // The read-only table may head a structured pipeline; the four commands
        // with process or terminal effects may not be composed at all.
        if command != "jobs" && plan.stages().len() > 1 {
            return Err(RuntimeError::new(
                RuntimeErrorKind::JobControlNotSoleStage { command },
                stage.span(),
            ));
        }
        match command {
            "jobs" => validate_jobs_arguments(stage)?,
            "fg" | "bg" => {
                parse_optional_job_target(stage, command)?;
            }
            "wait" => {
                parse_job_targets(stage, command)?;
            }
            _ => {
                parse_kill_arguments(stage)?;
            }
        }
    }
    Ok(())
}

/// The job-control command spelled by this internal stage name.
fn job_command_name(name: &str) -> Option<&'static str> {
    match name {
        "jobs" => Some("jobs"),
        "fg" => Some("fg"),
        "bg" => Some("bg"),
        "wait" => Some("wait"),
        "kill" => Some("kill"),
        _ => None,
    }
}

/// The state-changing job command this plan consists of, if any.
///
/// Validation has already refused these commands as a pipeline member, so a
/// match here is the complete pipeline.
fn sole_job_command(plan: &ExecutionPlan) -> Option<(&'static str, &PlannedStage)> {
    let [stage] = plan.stages() else {
        return None;
    };
    let PlannedResolution::Internal { name } = stage.resolution() else {
        return None;
    };
    match job_command_name(name.as_str()) {
        Some("jobs") | None => None,
        Some(command) => Some((command, stage)),
    }
}

/// Run one job command against the session-owned coordinator.
///
/// These commands never produce a payload; their result is the stage status.
fn execute_job_builtin(
    command: &'static str,
    stage: &PlannedStage,
    jobs: &mut Option<BackgroundJobs>,
    platform: &dyn Platform,
) -> Result<Status, RuntimeError> {
    let Some(coordinator) = jobs.as_mut() else {
        return Err(RuntimeError::new(
            RuntimeErrorKind::JobControlUnavailable { command },
            stage.span(),
        ));
    };
    let operation = |message: String| {
        RuntimeError::new(
            RuntimeErrorKind::JobOperation { command, message },
            stage.span(),
        )
    };
    match command {
        "bg" => {
            let target = match parse_optional_job_target(stage, command)? {
                Some(target) => target,
                None => coordinator
                    .newest_stopped()
                    .ok_or_else(|| operation("no stopped job to continue".to_owned()))?,
            };
            coordinator
                .signal_job(
                    target,
                    JobSignal::Continue,
                    QuarantinePolicy::Reject,
                    platform,
                )
                .map_err(|error| operation(error.to_string()))?;
        }
        "kill" => {
            let (signal, targets) = parse_kill_arguments(stage)?;
            for target in targets {
                // Source order, and a refusal ends the operation: a later
                // delivery must not be reported as if the earlier one happened.
                coordinator
                    .signal_job(target, signal, QuarantinePolicy::Deliver, platform)
                    .map_err(|error| operation(error.to_string()))?;
            }
        }
        _ => {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Unsupported {
                    feature: "this job-control command in the live session",
                },
                stage.span(),
            ));
        }
    }
    Ok(Status::exit(0, Duration::ZERO).expect("zero is a valid job-command status"))
}

/// Whether this all-internal plan begins with the read-only job table.
fn is_job_table_head(plan: &ExecutionPlan) -> bool {
    plan.stages().first().is_some_and(|stage| {
        matches!(stage.resolution(), PlannedResolution::Internal { name } if name == "jobs")
    })
}

/// Execute a `jobs` head and hand its snapshot to the ordinary internal suffix.
///
/// The snapshot is produced here rather than in the built-in executor because
/// the coordinator is deliberately outside the clonable [`SessionState`] that
/// ordinary lazy built-ins roll back.
fn execute_job_table_pipeline(
    plan: &ExecutionPlan,
    state: &mut SessionState,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    source: &SourceFile,
    jobs: &mut Option<BackgroundJobs>,
) -> Result<InternalPipelineOutcome, RuntimeError> {
    let head = plan
        .stages()
        .first()
        .expect("a job-table head was just observed");
    preflight(plan)?;
    let Some(coordinator) = jobs.as_mut() else {
        return Err(RuntimeError::new(
            RuntimeErrorKind::JobControlUnavailable { command: "jobs" },
            head.span(),
        ));
    };
    let rows = coordinator
        .snapshot()
        .iter()
        .map(job_table_row)
        .collect::<Vec<_>>();
    let status = Status::exit(0, Duration::ZERO).expect("zero is a valid snapshot status");
    execute_internal_suffix(
        plan,
        1,
        InternalPayload::ValueStream(ValueStream::from_values(rows)),
        vec![status],
        state,
        registry,
        probe,
        platform,
        source,
    )
}

/// Render one addressable job as its stable seven-field record.
///
/// Host identities stay strings: the platform's widened unsigned identifiers do
/// not fit the language's signed `Int` without narrowing them.
fn job_table_row(snapshot: &JobSnapshot) -> Value {
    let entries = vec![
        (
            "job".to_owned(),
            Value::string(format!("%{}", snapshot.job().get())),
        ),
        ("state".to_owned(), Value::string(snapshot.state().label())),
        (
            "placement".to_owned(),
            Value::string(match snapshot.placement() {
                JobPlacement::Foreground => "foreground",
                JobPlacement::Background => "background",
            }),
        ),
        (
            "group".to_owned(),
            Value::string(snapshot.group().get().to_string()),
        ),
        ("command".to_owned(), Value::string(snapshot.command())),
        (
            "status".to_owned(),
            snapshot
                .status()
                .map_or(Value::Null, |status| Value::Status(status.clone())),
        ),
        (
            "signal".to_owned(),
            snapshot
                .signal()
                .map_or(Value::Null, |signal| Value::Int(i64::from(signal))),
        ),
    ];
    Value::Record(Record::new(entries).expect("the job row keys are distinct"))
}

fn validate_jobs_arguments(stage: &PlannedStage) -> Result<(), RuntimeError> {
    if let Some(argument) = stage.arguments().first() {
        return Err(job_argument_error(
            "jobs",
            "jobs accepts no job arguments",
            argument.span(),
        ));
    }
    Ok(())
}

fn parse_optional_job_target(
    stage: &PlannedStage,
    command: &'static str,
) -> Result<Option<crate::job::JobId>, RuntimeError> {
    if let Some(argument) = stage.arguments().get(1) {
        return Err(job_argument_error(
            command,
            format!("{command} accepts at most one job argument"),
            argument.span(),
        ));
    }
    Ok(parse_job_targets(stage, command)?.first().copied())
}

/// The written targets in source order, rejecting a repeated identity.
fn parse_job_targets(
    stage: &PlannedStage,
    command: &'static str,
) -> Result<Vec<crate::job::JobId>, RuntimeError> {
    let mut seen = BTreeSet::new();
    let mut targets = Vec::with_capacity(stage.arguments().len());
    for argument in stage.arguments() {
        let job = parse_job_target(command, argument)?;
        if !seen.insert(job) {
            return Err(job_argument_error(
                command,
                format!("job `%{}` is repeated", job.get()),
                argument.span(),
            ));
        }
        targets.push(job);
    }
    Ok(targets)
}

/// The selected signal and the written targets in source order.
fn parse_kill_arguments(
    stage: &PlannedStage,
) -> Result<(JobSignal, Vec<crate::job::JobId>), RuntimeError> {
    let mut selector = None;
    let mut targets_started = false;
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();

    for argument in stage.arguments() {
        let word = job_word("kill", argument)?;
        let bytes = word.value().as_bytes();
        if bytes.starts_with(b"--") {
            let rendered = String::from_utf8_lossy(bytes);
            if targets_started {
                return Err(job_argument_error(
                    "kill",
                    "a signal selector must precede every job argument",
                    argument.span(),
                ));
            }
            if !matches!(
                bytes,
                b"--hangup"
                    | b"--interrupt"
                    | b"--terminate"
                    | b"--kill"
                    | b"--stop"
                    | b"--continue"
            ) {
                return Err(job_argument_error(
                    "kill",
                    format!("unknown signal selector `{rendered}`"),
                    argument.span(),
                ));
            }
            if selector.replace(bytes).is_some() {
                return Err(job_argument_error(
                    "kill",
                    "kill accepts only one signal selector",
                    argument.span(),
                ));
            }
            continue;
        }

        targets_started = true;
        let job = parse_job_target_word("kill", word)?;
        if !seen.insert(job) {
            return Err(job_argument_error(
                "kill",
                format!("job `%{}` is repeated", job.get()),
                argument.span(),
            ));
        }
        targets.push(job);
    }

    if targets.is_empty() {
        return Err(job_argument_error(
            "kill",
            "kill requires at least one explicit `%n` target",
            stage.span(),
        ));
    }
    let signal = match selector {
        Some(b"--hangup") => JobSignal::Hangup,
        Some(b"--interrupt") => JobSignal::Interrupt,
        Some(b"--kill") => JobSignal::Kill,
        Some(b"--stop") => JobSignal::Stop,
        Some(b"--continue") => JobSignal::Continue,
        // An omitted selector terminates: the accepted set was already checked.
        _ => JobSignal::Terminate,
    };
    Ok((signal, targets))
}

fn parse_job_target(
    command: &'static str,
    argument: &crate::plan::PlannedArgument,
) -> Result<crate::job::JobId, RuntimeError> {
    parse_job_target_word(command, job_word(command, argument)?)
}

fn parse_job_target_word(
    command: &'static str,
    word: &ExpandedWord,
) -> Result<crate::job::JobId, RuntimeError> {
    let bytes = word.value().as_bytes();
    if !bytes.starts_with(b"%") {
        return Err(job_argument_error(
            command,
            "job arguments use `%n`, not a bare process or job number",
            word.span(),
        ));
    }
    let digits = &bytes[1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(job_argument_error(
            command,
            "job identity must contain only ASCII decimal digits",
            word.span(),
        ));
    }
    let value = std::str::from_utf8(digits)
        .expect("ASCII digits are valid UTF-8")
        .parse::<u64>()
        .map_err(|_| {
            job_argument_error(
                command,
                "job identity exceeds the supported range",
                word.span(),
            )
        })?;
    crate::job::JobId::new(value).ok_or_else(|| {
        job_argument_error(
            command,
            "job identity must be a nonzero decimal number",
            word.span(),
        )
    })
}

fn job_word<'a>(
    command: &'static str,
    argument: &'a crate::plan::PlannedArgument,
) -> Result<&'a ExpandedWord, RuntimeError> {
    argument.as_word().ok_or_else(|| {
        job_argument_error(
            command,
            "expected a word job argument, found a typed value",
            argument.span(),
        )
    })
}

fn job_argument_error(
    command: &'static str,
    message: impl Into<String>,
    span: Span,
) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::BuiltinArgument {
            command,
            message: message.into(),
        },
        span,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_mixed_pipeline(
    plan: &crate::plan::ExecutionPlan,
    state: &mut SessionState,
    registry: &CommandRegistry,
    source: &SourceFile,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    output: &mut dyn Write,
) -> Result<ChainStep, Interrupt> {
    let first_internal = plan
        .stages()
        .iter()
        .position(|stage| matches!(stage.resolution(), PlannedResolution::Internal { .. }))
        .expect("the caller found an internal stage");
    let last_internal = plan
        .stages()
        .iter()
        .rposition(|stage| matches!(stage.resolution(), PlannedResolution::Internal { .. }))
        .expect("the caller found an internal stage");
    let internal = first_internal..last_internal + 1;

    let presentation = if last_internal + 1 == plan.stages().len() {
        let final_stage = &plan.stages()[last_internal];
        let destination = output_destination(final_stage, platform)?;
        Some(
            select_terminal_presentation(final_stage.output_carrier(), destination).map_err(
                |error| {
                    RuntimeError::new(RuntimeErrorKind::Presentation(error), final_stage.span())
                },
            )?,
        )
    } else {
        None
    };

    let mut pending_state = state.clone();
    let closure_context = OwnedClosureContext::new(
        source.clone(),
        pending_state.environment().clone(),
        EvalLimits::default(),
    );
    let mut mixed = start_mixed_pipeline(plan, platform, clock, internal.clone())?;
    let mut payload = mixed.take_input().map_or(InternalPayload::Empty, |reader| {
        InternalPayload::ByteStream(pipe_byte_stream(
            reader,
            plan.stages()[first_internal].span(),
        ))
    });
    let mut indexed_statuses = Vec::with_capacity(plan.stages().len());

    let internal_result = (|| {
        for index in internal {
            let stage = &plan.stages()[index];
            let PlannedResolution::Internal { name } = stage.resolution() else {
                return Err(Interrupt::Runtime(RuntimeError::new(
                    RuntimeErrorKind::Unsupported {
                        feature: "a mixed pipeline with more than one internal stage island",
                    },
                    stage.span(),
                )));
            };
            let upstream = indexed_statuses.last().map(|(_, status)| status);
            match execute_stage(
                name,
                stage,
                payload,
                upstream,
                &mut pending_state,
                registry,
                probe,
                platform,
                plan.cwd(),
                &closure_context,
            )? {
                StageOutcome::Completed {
                    payload: output_payload,
                    status,
                } => {
                    payload = output_payload;
                    indexed_statuses.push((index, status));
                }
                StageOutcome::Exit(code) => return Ok(Some(ChainStep::Exit(code))),
            }
        }

        if let Some(writer) = mixed.take_output() {
            drain_payload_to_pipe(payload, writer, plan.stages()[last_internal].span())?;
        } else {
            let final_stage = &plan.stages()[last_internal];
            render_payload(
                payload,
                presentation
                    .as_ref()
                    .expect("a final internal stage selected presentation")
                    .as_ref(),
                final_stage.span(),
                output,
            )?;
        }
        Ok(None)
    })();

    let requested_exit = match internal_result {
        Ok(exit) => exit,
        Err(error) => {
            mixed.terminate();
            return Err(error);
        }
    };
    if let Some(exit) = requested_exit {
        mixed.terminate();
        *pending_state.environment_mut() = closure_context.environment_snapshot();
        *state = pending_state;
        return Ok(exit);
    }

    let (external_statuses, pipeline_duration) = mixed.wait(plan, platform, clock)?;
    indexed_statuses.extend(external_statuses);
    indexed_statuses.sort_by_key(|(index, _)| *index);
    let statuses: Vec<Status> = indexed_statuses
        .into_iter()
        .map(|(_, status)| status)
        .collect();
    let selected = if plan.pipefail() {
        statuses
            .iter()
            .rposition(|status| !status.is_ok())
            .unwrap_or(statuses.len() - 1)
    } else {
        statuses.len() - 1
    };
    let status = Status::aggregate(statuses, selected, pipeline_duration)
        .expect("a mixed pipeline has source-ordered leaf statuses");
    pending_state.set_current_status(Some(status.clone()));
    *pending_state.environment_mut() = closure_context.environment_snapshot();
    *state = pending_state;
    Ok(ChainStep::Status(status))
}

fn pipe_byte_stream(
    mut reader: Box<dyn DescriptorEndpoint>,
    span: flashshell_syntax::Span,
) -> ByteStream {
    const CHUNK_SIZE: usize = 64 * 1024;
    ByteStream::from_pull_fn(move || {
        let mut chunk = vec![0; CHUNK_SIZE];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return BytePull::End,
                Ok(amount) => {
                    chunk.truncate(amount);
                    return BytePull::Chunk(chunk);
                }
                Err(DescriptorReadError::Operation {
                    kind: io::ErrorKind::Interrupted,
                    ..
                }) => {}
                Err(error) => {
                    return BytePull::Failed(RuntimeError::new(
                        RuntimeErrorKind::PipelineRead(error),
                        span,
                    ));
                }
            }
        }
    })
}

fn drain_payload_to_pipe(
    payload: InternalPayload,
    mut writer: Box<dyn DescriptorEndpoint>,
    span: flashshell_syntax::Span,
) -> Result<(), Interrupt> {
    let InternalPayload::ByteStream(mut bytes) = payload else {
        return Err(Interrupt::Runtime(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "an implicit structured-to-external conversion",
            },
            span,
        )));
    };
    loop {
        match bytes.pull() {
            BytePull::Chunk(chunk) => {
                let mut remaining = chunk.as_slice();
                while !remaining.is_empty() {
                    match writer.write(remaining) {
                        Ok(0) => {
                            return Err(Interrupt::Runtime(RuntimeError::new(
                                RuntimeErrorKind::PipelineWrite(DescriptorWriteError::Operation {
                                    kind: io::ErrorKind::WriteZero,
                                    message: "pipe writer accepted zero bytes".to_owned(),
                                }),
                                span,
                            )));
                        }
                        Ok(amount) if amount <= remaining.len() => {
                            remaining = &remaining[amount..];
                        }
                        Ok(amount) => {
                            return Err(Interrupt::Runtime(RuntimeError::new(
                                RuntimeErrorKind::PipelineWrite(DescriptorWriteError::Operation {
                                    kind: io::ErrorKind::InvalidData,
                                    message: format!(
                                        "pipe writer reported {amount} bytes for a {}-byte remainder",
                                        remaining.len()
                                    ),
                                }),
                                span,
                            )));
                        }
                        Err(DescriptorWriteError::Operation {
                            kind: io::ErrorKind::Interrupted,
                            ..
                        }) => {}
                        Err(DescriptorWriteError::Operation {
                            kind: io::ErrorKind::BrokenPipe,
                            ..
                        }) => return Ok(()),
                        Err(error) => {
                            return Err(Interrupt::Runtime(RuntimeError::new(
                                RuntimeErrorKind::PipelineWrite(error),
                                span,
                            )));
                        }
                    }
                }
            }
            BytePull::End => return Ok(()),
            BytePull::Failed(error) => return Err(Interrupt::Runtime(error)),
            BytePull::Cancelled(reason) => {
                return Err(Interrupt::Runtime(RuntimeError::new(
                    RuntimeErrorKind::StreamCancelled { reason },
                    span,
                )));
            }
        }
    }
}

fn output_destination(
    stage: &PlannedStage,
    platform: &dyn Platform,
) -> Result<OutputDestination, RuntimeError> {
    if stage.redirections().iter().any(|redirection| {
        let descriptor = match redirection.action() {
            RedirectionAction::Input { descriptor, .. }
            | RedirectionAction::Output { descriptor, .. }
            | RedirectionAction::Duplicate { descriptor, .. }
            | RedirectionAction::Close { descriptor, .. } => descriptor,
        };
        *descriptor == 1
    }) {
        return Ok(OutputDestination::Redirected);
    }
    if !platform.is_output_terminal() {
        return Ok(OutputDestination::NonInteractive);
    }
    let size = platform.terminal_size().map_err(|error| {
        RuntimeError::new(RuntimeErrorKind::TerminalPresentation(error), stage.span())
    })?;
    Ok(OutputDestination::InteractiveTerminal {
        columns: usize::from(size.columns()),
    })
}

/// Write a completed internal pipeline's human output after destination
/// selection.
fn render_payload(
    output: InternalPayload,
    presentation: Option<&TerminalPresentation>,
    span: flashshell_syntax::Span,
    sink: &mut dyn Write,
) -> Result<(), Interrupt> {
    match output {
        InternalPayload::Empty => Ok(()),
        InternalPayload::Value(value) => write_value(
            &value,
            presentation.expect("a Value carrier has terminal-presentation proof"),
            sink,
        )
        .map_err(Interrupt::Output),
        InternalPayload::ValueStream(mut values) => {
            let presentation =
                presentation.expect("a ValueStream carrier has terminal-presentation proof");
            loop {
                match values.pull() {
                    StreamPull::Item(value) => {
                        write_value(&value, presentation, sink).map_err(Interrupt::Output)?;
                    }
                    StreamPull::End => return Ok(()),
                    StreamPull::Failed(error) => return Err(Interrupt::Runtime(error)),
                    StreamPull::Cancelled(reason) => {
                        return Err(Interrupt::Runtime(RuntimeError::new(
                            RuntimeErrorKind::StreamCancelled { reason },
                            span,
                        )));
                    }
                }
            }
        }
        InternalPayload::ByteStream(mut bytes) => loop {
            match bytes.pull() {
                BytePull::Chunk(chunk) => sink.write_all(&chunk).map_err(Interrupt::Output)?,
                BytePull::End => return Ok(()),
                BytePull::Failed(error) => return Err(Interrupt::Runtime(error)),
                BytePull::Cancelled(reason) => {
                    return Err(Interrupt::Runtime(RuntimeError::new(
                        RuntimeErrorKind::StreamCancelled { reason },
                        span,
                    )));
                }
            }
        },
    }
}

fn write_value(
    value: &Value,
    presentation: &TerminalPresentation,
    sink: &mut dyn Write,
) -> io::Result<()> {
    match value {
        Value::Table(table) => writeln!(sink, "{}", render_table(table, presentation.columns())),
        _ => writeln!(sink, "{value}"),
    }
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
