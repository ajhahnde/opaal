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

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flashshell_platform::{
    DescriptorEndpoint, DescriptorReadError, DescriptorWriteError, Platform, ProcessGroupId,
};
use flashshell_syntax::{
    Diagnostic, ParseOutcome, Script, Severity, SourceFile, SourceId, StatementKind, parse,
    render_diagnostic,
};

use crate::background::{BackgroundJobs, escape_job_label};
use crate::builtin::{SessionState, standard_registry};
use crate::closure::OwnedClosureContext;
use crate::command::CommandRegistry;
use crate::eval::{
    Clock, Completion, EvalLimits, RuntimeError, RuntimeErrorKind, evaluate_in_environment,
};
use crate::execute::{execute_foreground_status, start_mixed_pipeline};
use crate::internal::{
    InternalPayload, InternalPipelineOutcome, StageOutcome, execute_internal_pipeline,
    execute_stage,
};
use crate::plan::{
    PlannedResolution, PlannedStage, RedirectionAction, SessionOptions, plan_pipeline_with_options,
};
use crate::presentation::{
    OutputDestination, TerminalPresentation, render_table, select_terminal_presentation,
};
use crate::resolve::ExecutableProbe;
use crate::stream::{BytePull, ByteStream, StreamPull};
use crate::{Environment, ScopeStack, Status, Value};

pub use crate::background::{
    BackgroundFailure, BackgroundFailureReason, JobNotice, JobNoticeError, JobNoticeId,
    JobNoticeKind, LiveJob, LiveJobState,
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
                                    feature: "background execution in a non-interactive session",
                                },
                                background_span,
                            );
                            return Err(runtime(&source, &error));
                        };
                        let pipeline = match one_background_pipeline(&job.chain) {
                            Ok(pipeline) => pipeline,
                            Err(error) => return Err(runtime(&source, &error)),
                        };
                        let plan = plan_pipeline_with_options(
                            pipeline,
                            state.cwd(),
                            &source,
                            scope,
                            state.environment(),
                            registry,
                            probe,
                            options,
                        )
                        .map_err(|error| runtime(&source, &error))?;
                        if let Some(stage) = plan.stages().iter().find(|stage| {
                            matches!(stage.resolution(), PlannedResolution::Internal { .. })
                        }) {
                            let error = RuntimeError::new(
                                RuntimeErrorKind::Unsupported {
                                    feature: "background internal-command execution",
                                },
                                stage.span(),
                            );
                            return Err(runtime(&source, &error));
                        }
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

fn one_background_pipeline(
    chain: &flashshell_syntax::ConditionalChain,
) -> Result<&flashshell_syntax::Pipeline, RuntimeError> {
    if let Some(operator) = chain.operators().first() {
        return Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "background conditional-chain execution",
            },
            operator.span(),
        ));
    }
    let and_chain = chain
        .or_terms()
        .first()
        .expect("a parsed conditional chain contains an operand");
    if let Some(operator) = and_chain.operators().first() {
        return Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "background conditional-chain execution",
            },
            operator.span(),
        ));
    }
    Ok(and_chain
        .and_terms()
        .first()
        .expect("a parsed and-chain contains an operand"))
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
        let outcome = execute_internal_pipeline(
            &plan,
            &mut pending_state,
            registry,
            probe,
            platform,
            source,
        )?;
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
