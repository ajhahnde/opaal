//! Persistent interactive session driver.
//!
//! A [`Session`] evaluates independently submitted edit buffers against one
//! retained scope, environment, logical working directory, and last status, so
//! an interactive client observes the same accumulated state a script would.
//! Each submitted buffer runs its statements in source order: pure statements
//! and `export`/`unset` reuse the shared scope and environment, an all-internal
//! pipeline moves owned structured carriers through the internal executor, and
//! an external foreground pipeline runs through the ordinary byte executor,
//! and maximal internal segments stream concurrently through owned process
//! pipes.
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
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use flash_platform::{
    DescriptorEndpoint, DescriptorReadError, DescriptorWriteError, DirectoryReadRequest,
    DirectoryStream, FileOpenMode, FileOpenRequest, JobSignal, Platform, ProcessGroupId,
};
use flash_syntax::{
    CommandHeadKind, ConditionalChain, Diagnostic, OutputMode, ParseOutcome, Script, Severity,
    SourceFile, SourceId, Span, StageKind, StatementKind, parse, render_diagnostic,
};

use crate::background::{BackgroundJobs, ForegroundJobOutcome, QuarantinePolicy, escape_job_label};
use crate::builtin::{SessionState, standard_registry};
use crate::closure::OwnedClosureContext;
use crate::command::CommandRegistry;
use crate::eval::{
    Abort, CancellationToken, CapturePosition, CapturedChain, Clock, EvalLimits, EvaluationContext,
    EvaluationHost, EvaluationPolicy, ExpandedWord, HostedEvaluationFailure,
    HostedEvaluationOutcome, ResourceBudget, RuntimeError, RuntimeErrorKind, evaluate_with_host,
    expand_word_with_environment,
};
use crate::execute::{
    BoundedCapture, CommandCapture, MixedPipelineControl, MixedSegment, aggregate_statuses,
    execute_foreground_status, execute_foreground_with_stdout_drain, start_mixed_pipeline,
};
use crate::format::{ToTextStep, to_text};
use crate::internal::{
    DEFAULT_MATERIALIZATION_LIMIT, InternalPayload, InternalPipelineOutcome, StageOutcome,
    execute_internal_pipeline, execute_internal_suffix, execute_stage,
};
use crate::job::JobPlacement;
use crate::module::RuntimeBindingTypes;
use crate::plan::{
    ExecutionPlan, InternalStdoutRoute, PlannedResolution, PlannedStage, SessionOptions,
    internal_stdout_route, plan_pipeline_with_options_and_binding_types, preflight,
};
use crate::presentation::{
    OutputDestination, TerminalPresentation, render_table, select_terminal_presentation,
};
use crate::resolve::ExecutableProbe;
use crate::stream::{BytePull, ByteStream, StreamPull, ValueStream};
use crate::{Duration, Environment, Record, ScopeStack, Status, Table, Value};

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
    /// A recoverable source diagnostic; the session state is unchanged.
    Diagnostic(String),
    /// A structured runtime failure plus its retained-source rendering. Module
    /// execution may re-render it with its complete program registry.
    Runtime {
        /// The source-spanned runtime failure.
        error: Box<RuntimeError>,
        /// The complete rendering over every retained evaluation source.
        rendered: String,
    },
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
            Self::Runtime { rendered, .. } => rendered,
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

    /// The live lexical scope used by later interactive submissions.
    #[must_use]
    pub const fn scope(&self) -> &ScopeStack {
        &self.scope
    }

    /// The command namespace used for planning and interactive discovery.
    #[must_use]
    pub const fn registry(&self) -> &CommandRegistry {
        &self.registry
    }

    /// The most recent normally completed status, if a job has run.
    #[must_use]
    pub const fn current_status(&self) -> Option<&Status> {
        self.state.current_status()
    }

    pub(crate) fn seed_current_status(&mut self, status: Option<Status>) {
        self.state.set_current_status(status);
    }

    /// Enable the interactive background-job coordinator with one owned clock.
    pub fn enable_interactive_job_control(&mut self, clock: Arc<dyn Clock>) {
        if self.jobs.is_none() {
            self.jobs = Some(BackgroundJobs::new(clock, true));
        }
    }

    /// Enable the background-job coordinator for a non-interactive session.
    ///
    /// The coordinator is identical; only the rendering boundary differs. A
    /// script has no prompt, so it drains notices once, at its join.
    pub fn enable_script_job_control(&mut self, clock: Arc<dyn Clock>) {
        if self.jobs.is_none() {
            self.jobs = Some(BackgroundJobs::new(clock, false));
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
    /// `output`. On a recoverable [`SubmitError`] the accumulated
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
        let source = Arc::new(SourceFile::new(SourceId::new(self.next_source), name, text));
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

        let binding_types = RuntimeBindingTypes::analyze_source(&source, &script)
            .map_err(|error| render(&source, &[error.diagnostic()]))?;

        self.submit_parsed(
            source,
            &script,
            false,
            Some(Arc::new(binding_types)),
            probe,
            platform,
            clock,
            output,
        )
    }

    /// Executes one source from a fully analyzed module program in an isolated
    /// lexical root while retaining the session's shared execution state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit_module_source(
        &mut self,
        source: &SourceFile,
        script: &Script,
        mut scope: ScopeStack,
        binding_types: Arc<RuntimeBindingTypes>,
        probe: &dyn ExecutableProbe,
        platform: &dyn Platform,
        clock: &dyn Clock,
        output: &mut dyn Write,
    ) -> Result<(SubmitOutcome, ScopeStack), SubmitError> {
        std::mem::swap(&mut self.scope, &mut scope);
        let outcome = self.submit_parsed(
            Arc::new(source.clone()),
            script,
            true,
            Some(binding_types),
            probe,
            platform,
            clock,
            output,
        );
        std::mem::swap(&mut self.scope, &mut scope);
        outcome.map(|outcome| (outcome, scope))
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_parsed(
        &mut self,
        source: Arc<SourceFile>,
        script: &Script,
        imports_analyzed: bool,
        binding_types: Option<Arc<RuntimeBindingTypes>>,
        probe: &dyn ExecutableProbe,
        platform: &dyn Platform,
        clock: &dyn Clock,
        output: &mut dyn Write,
    ) -> Result<SubmitOutcome, SubmitError> {
        let source_file = source.as_ref();
        let binding_types =
            binding_types.unwrap_or_else(|| Arc::new(RuntimeBindingTypes::default()));
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
                StatementKind::Import(_) if imports_analyzed => continue,
                StatementKind::ModuleExport(_) if imports_analyzed => continue,
                StatementKind::Job(job) if job.background_span.is_some() => {
                    let background_span = job
                        .background_span
                        .expect("the guarded background statement has a marker");
                    let Some(jobs) = jobs.as_mut() else {
                        let error = RuntimeError::new(
                            RuntimeErrorKind::Unsupported {
                                feature: "background execution in a session without job control",
                            },
                            background_span,
                        );
                        return Err(runtime(source_file, &error));
                    };
                    let mut child_scope = scope.clone();
                    let direct_pipeline = one_background_pipeline(&job.chain).filter(|pipeline| {
                        pipeline_is_all_external(
                            pipeline,
                            source_file,
                            &mut child_scope,
                            state.environment(),
                            registry,
                        )
                    });
                    let plan = if let Some(pipeline) = direct_pipeline {
                        plan_pipeline_with_options_and_binding_types(
                            pipeline,
                            state.cwd(),
                            source_file,
                            &mut child_scope,
                            state.environment(),
                            registry,
                            probe,
                            options,
                            Arc::clone(&binding_types),
                        )
                        .map_err(|error| runtime(source_file, &error))?
                    } else {
                        background_shell_plan(
                            &job.chain,
                            source_file,
                            state.cwd(),
                            state.environment(),
                            state.current_status(),
                            scope,
                            options,
                            platform,
                        )
                        .map_err(|error| runtime(source_file, &error))?
                    };
                    let command = source_file
                        .slice(job.chain.span())
                        .map(escape_job_label)
                        .expect("a parsed chain span belongs to its source");
                    jobs.start(&plan, platform, command)
                        .map_err(|error| runtime(source_file, &error))?;
                    state.set_current_status(Some(
                        Status::exit(0, crate::Duration::ZERO)
                            .expect("zero is a valid launch status"),
                    ));
                }
                _ => {
                    let mut pending_scope = scope.clone();
                    let mut pending_state = state.clone();
                    let one = Script::new(vec![statement.clone()], statement.span());
                    let evaluated = {
                        let mut host = SessionEvaluationHost {
                            state: &mut pending_state,
                            options,
                            registry,
                            probe,
                            platform,
                            clock,
                            jobs,
                            output,
                        };
                        evaluate_with_host(
                            &one,
                            Arc::clone(&source),
                            &mut pending_scope,
                            &EvalLimits::default(),
                            Arc::clone(&binding_types),
                            &mut host,
                        )
                    };
                    match evaluated {
                        Ok(HostedEvaluationOutcome::Value(_)) => {
                            *scope = pending_scope;
                            *state = pending_state;
                        }
                        Ok(HostedEvaluationOutcome::Exit(code)) => {
                            *scope = pending_scope;
                            *state = pending_state;
                            return Ok(SubmitOutcome::Exit(code));
                        }
                        Ok(HostedEvaluationOutcome::Stopped(job)) => {
                            debug_assert!(
                                jobs.as_ref().and_then(|jobs| jobs.job(job)).is_some(),
                                "a stopped managed outcome retains its coordinator record"
                            );
                        }
                        Ok(HostedEvaluationOutcome::Cancelled(_)) => {
                            unreachable!("default evaluation limits never cancel")
                        }
                        Err(HostedEvaluationFailure::Runtime(error)) => {
                            return Err(runtime(source_file, &error));
                        }
                        Err(HostedEvaluationFailure::Output(error)) => {
                            return Err(SubmitError::Output(error));
                        }
                    }
                }
            }
        }

        Ok(SubmitOutcome::Continued)
    }
}

fn chain_is_standalone_help(chain: &ConditionalChain, source: &SourceFile) -> bool {
    let [and_chain] = chain.or_terms() else {
        return false;
    };
    let [pipeline] = and_chain.and_terms() else {
        return false;
    };
    let [stage] = pipeline.stages() else {
        return false;
    };
    let StageKind::Command(command) = stage.kind() else {
        return false;
    };
    command.head.kind() == CommandHeadKind::Bare
        && source.slice(command.head.word().span()).ok() == Some("help")
}

/// One pipeline's control result inside a conditional chain.
enum ChainStep {
    Status(Status),
    Stopped(crate::job::JobId),
    Exit(u8),
}

/// A runtime failure or a fatal output-write failure raised while executing a job.
enum Interrupt {
    Runtime(RuntimeError),
    Output(io::Error),
}

/// Final stdout selected for every reached pipeline in one chain.
enum ChainOutput<'output> {
    Session(&'output mut dyn Write),
    Capture {
        collector: &'output mut BoundedCapture,
        span: Span,
    },
}

impl ChainOutput<'_> {
    const fn is_capture(&self) -> bool {
        matches!(self, Self::Capture { .. })
    }

    fn ensure_capture_within_limit(&self) -> Result<(), RuntimeError> {
        match self {
            Self::Session(_) => Ok(()),
            Self::Capture { collector, span } => collector.ensure_within_limit(*span),
        }
    }
}

/// Session-owned evaluator host for reached command pipelines.
///
/// Language-owned state is transactional outside this boundary. The ordinary
/// session executor retains ownership of planning, process activity, job
/// transitions, status, and output for every reached chain.
struct SessionEvaluationHost<'session> {
    state: &'session mut SessionState,
    options: &'session SessionOptions,
    registry: &'session CommandRegistry,
    probe: &'session dyn ExecutableProbe,
    platform: &'session dyn Platform,
    clock: &'session dyn Clock,
    jobs: &'session mut Option<BackgroundJobs>,
    output: &'session mut dyn Write,
}

impl EvaluationHost for SessionEvaluationHost<'_> {
    fn environment(&mut self) -> &mut Environment {
        self.state.environment_mut()
    }

    fn current_status(&self) -> Option<&Status> {
        self.state.current_status()
    }

    fn policy(&self) -> EvaluationPolicy {
        EvaluationPolicy::General
    }

    fn manages_foreground_jobs(&self) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(BackgroundJobs::manages_foreground)
    }

    fn language_state_checkpoint(&mut self) -> Box<dyn std::any::Any> {
        Box::new(self.state.clone())
    }

    fn restore_language_state(&mut self, checkpoint: Box<dyn std::any::Any>) {
        *self.state = *checkpoint
            .downcast::<SessionState>()
            .expect("a session evaluation checkpoint is session state");
    }

    fn read_directory(
        &mut self,
        path: &Path,
    ) -> Result<Box<dyn DirectoryStream>, crate::eval::RuntimeErrorKind> {
        self.platform
            .read_directory(DirectoryReadRequest::new(path, self.state.cwd()))
            .map_err(crate::eval::RuntimeErrorKind::DirectoryRead)
    }

    fn execute_chain(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        context: EvaluationContext,
    ) -> Result<Status, Abort> {
        let inspection_only = chain_is_standalone_help(chain, &context.source);
        let error_source = Arc::clone(&context.source);
        let _ = &context.cancel;
        if context.manage_foreground
            && self
                .jobs
                .as_ref()
                .is_some_and(BackgroundJobs::manages_foreground)
            && foreground_chain_requires_supervisor(
                chain,
                &context.source,
                scope,
                self.state,
                self.options,
                self.registry,
                self.probe,
                &context.binding_types,
            )
        {
            let plan = supervisor_plan(
                chain,
                &context.source,
                self.state.cwd(),
                self.state.environment(),
                self.state.current_status(),
                scope,
                self.options,
                self.platform,
                true,
            )
            .map_err(|error| Abort::Error(error.with_source(Arc::clone(&error_source))))?;
            let command = context
                .source
                .slice(chain.span())
                .map(escape_job_label)
                .expect("a parsed chain span belongs to its source");
            let outcome = self
                .jobs
                .as_mut()
                .expect("managed foreground execution retains a coordinator")
                .start_foreground(&plan, self.platform, command)
                .map_err(|error| Abort::Error(error.with_source(Arc::clone(&error_source))))?;
            let step = apply_foreground_completion(outcome, scope, self.state, chain.span(), true)
                .map_err(|error| Abort::Error(error.with_source(Arc::clone(&error_source))))?;
            return match step {
                ChainStep::Status(status) => Ok(status),
                ChainStep::Exit(code) => Err(Abort::Exit(code)),
                ChainStep::Stopped(job) => Err(Abort::Stopped(job)),
            };
        }
        let mut output = ChainOutput::Session(self.output);
        let step = run_chain(
            chain,
            self.state,
            scope,
            self.options,
            self.registry,
            &context.source,
            &context.binding_types,
            self.probe,
            self.platform,
            self.clock,
            self.jobs,
            context.manage_foreground,
            &mut output,
        )
        .map_err(|interrupt| interrupt.into_abort(error_source))?;
        match step {
            ChainStep::Status(status) => {
                if !inspection_only {
                    self.state.set_current_status(Some(status.clone()));
                }
                Ok(status)
            }
            ChainStep::Exit(code) => Err(Abort::Exit(code)),
            ChainStep::Stopped(job) => Err(Abort::Stopped(job)),
        }
    }

    fn capture_chain(
        &mut self,
        chain: &ConditionalChain,
        scope: &mut ScopeStack,
        span: Span,
        _position: CapturePosition,
        context: EvaluationContext,
    ) -> Result<CapturedChain, Abort> {
        let error_source = Arc::clone(&context.source);
        let mut collector = BoundedCapture::new(self.options.capture_limit());
        let mut output = ChainOutput::Capture {
            collector: &mut collector,
            span,
        };
        let step = run_chain(
            chain,
            self.state,
            scope,
            self.options,
            self.registry,
            &context.source,
            &context.binding_types,
            self.probe,
            self.platform,
            self.clock,
            self.jobs,
            false,
            &mut output,
        )
        .map_err(|interrupt| interrupt.into_abort(error_source))?;
        let ChainStep::Status(status) = step else {
            return match step {
                ChainStep::Exit(code) => Err(Abort::Exit(code)),
                ChainStep::Stopped(job) => Err(Abort::Stopped(job)),
                ChainStep::Status(_) => unreachable!(),
            };
        };
        self.state.set_current_status(Some(status.clone()));
        let captured = collector
            .finish(status, span)
            .map_err(|error| Abort::Error(error.with_source(context.source)))?;
        let (bytes, status) = captured.into_parts();
        Ok(CapturedChain { bytes, status })
    }
}

/// Compatibility entry for the legacy free capture helpers.
///
/// It delegates to the same session chain runner used by evaluator-hosted
/// substitution, so conditional traversal and pipeline topology stay singular.
#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_command_substitution_compat(
    chain: &ConditionalChain,
    cwd: &Path,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<CommandCapture<Vec<u8>>, RuntimeError> {
    let mut state = SessionState::new(cwd, environment.clone());
    let mut jobs = None;
    let mut collector = BoundedCapture::new(options.capture_limit());
    let mut output = ChainOutput::Capture {
        collector: &mut collector,
        span: chain.span(),
    };
    let step = run_chain(
        chain,
        &mut state,
        scope,
        options,
        registry,
        source,
        &Arc::new(RuntimeBindingTypes::default()),
        probe,
        platform,
        clock,
        &mut jobs,
        false,
        &mut output,
    )
    .map_err(|interrupt| match interrupt {
        Interrupt::Runtime(error) => error,
        Interrupt::Output(error) => {
            unreachable!("bounded command capture cannot raise sink error: {error}")
        }
    })?;
    let status = match step {
        ChainStep::Status(status) => status,
        ChainStep::Exit(code) => Status::exit(i64::from(code), Duration::ZERO)
            .expect("an exit request code is a valid status"),
        ChainStep::Stopped(_) => unreachable!("compatibility capture does not manage jobs"),
    };
    collector.finish(status, chain.span())
}

fn one_background_pipeline(
    chain: &flash_syntax::ConditionalChain,
) -> Option<&flash_syntax::Pipeline> {
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
    pipeline: &flash_syntax::Pipeline,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
) -> bool {
    pipeline.stages().iter().all(|stage| {
        let StageKind::Command(command) = stage.kind() else {
            return false;
        };
        if command.head.kind() == CommandHeadKind::ForcedExternal {
            return true;
        }
        let Ok(head) =
            expand_word_with_environment(command.head.word(), source, scope, environment)
        else {
            return false;
        };
        let Some(name) = head.value().to_str() else {
            return true;
        };
        if matches!(
            scope.get(name),
            Some(Value::Callable(callable)) if callable.family() == "function"
        ) {
            return false;
        }
        matches!(
            registry.classify(name),
            crate::command::CommandClassification::Unknown
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn foreground_chain_requires_supervisor(
    chain: &ConditionalChain,
    source: &SourceFile,
    scope: &ScopeStack,
    state: &SessionState,
    options: &SessionOptions,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    binding_types: &Arc<RuntimeBindingTypes>,
) -> bool {
    if chain_contains_static_job_command(chain, source) {
        return false;
    }

    let mut planned_scope = scope.clone();
    let pipeline_total = chain
        .or_terms()
        .iter()
        .map(|and_chain| and_chain.and_terms().len())
        .sum::<usize>();
    let mut pipeline_count = 0_usize;
    let mut has_internal = false;
    let mut has_external = false;
    for and_chain in chain.or_terms() {
        for pipeline in and_chain.and_terms() {
            pipeline_count += 1;
            let Ok(plan) = plan_pipeline_with_options_and_binding_types(
                pipeline,
                state.cwd(),
                source,
                &mut planned_scope,
                state.environment(),
                registry,
                probe,
                options,
                Arc::clone(binding_types),
            ) else {
                return pipeline_total > 1;
            };
            for stage in plan.stages() {
                match stage.resolution() {
                    PlannedResolution::Internal { .. } => has_internal = true,
                    PlannedResolution::External { .. } => has_external = true,
                }
            }
        }
    }

    has_external && (has_internal || pipeline_count > 1)
}

fn chain_contains_static_job_command(chain: &ConditionalChain, source: &SourceFile) -> bool {
    chain.or_terms().iter().any(|and_chain| {
        and_chain.and_terms().iter().any(|pipeline| {
            pipeline.stages().iter().any(|stage| {
                let StageKind::Command(command) = stage.kind() else {
                    return false;
                };
                command.head.kind() == CommandHeadKind::Bare
                    && source
                        .slice(command.head.word().span())
                        .ok()
                        .and_then(job_command_name)
                        .is_some()
            })
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn background_shell_plan(
    chain: &ConditionalChain,
    source: &SourceFile,
    cwd: &Path,
    environment: &Environment,
    current_status: Option<&Status>,
    scope: &ScopeStack,
    options: &SessionOptions,
    platform: &dyn Platform,
) -> Result<ExecutionPlan, RuntimeError> {
    supervisor_plan(
        chain,
        source,
        cwd,
        environment,
        current_status,
        scope,
        options,
        platform,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn supervisor_plan(
    chain: &ConditionalChain,
    source: &SourceFile,
    cwd: &Path,
    environment: &Environment,
    current_status: Option<&Status>,
    scope: &ScopeStack,
    options: &SessionOptions,
    platform: &dyn Platform,
    completion: bool,
) -> Result<ExecutionPlan, RuntimeError> {
    let span = chain.span();
    let executable = platform
        .shell_executable()
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::ShellExecutable(error), span))?;
    let text = source
        .slice(span)
        .expect("a parsed chain span belongs to its source");

    let capsule = crate::capsule::encode_background_capsule(
        if completion {
            "<foreground-chain>"
        } else {
            "<background-chain>"
        },
        text,
        cwd,
        environment,
        current_status,
        scope,
        *options,
    )
    .map_err(|error| {
        RuntimeError::new(
            RuntimeErrorKind::ExecutionCapsule {
                message: error.to_string(),
            },
            span,
        )
    })?;

    let mut argv = vec![
        ExpandedWord::synthetic(executable.as_os_str().to_os_string(), span),
        ExpandedWord::synthetic(OsString::from("--async-capsule"), span),
        ExpandedWord::synthetic(
            OsString::from(crate::capsule::CAPSULE_DESCRIPTOR.to_string()),
            span,
        ),
    ];
    if completion {
        argv.push(ExpandedWord::synthetic(
            OsString::from("--async-completion"),
            span,
        ));
        argv.push(ExpandedWord::synthetic(
            OsString::from(crate::capsule::COMPLETION_DESCRIPTOR.to_string()),
            span,
        ));
    }

    let plan = ExecutionPlan::single_external(
        executable,
        argv,
        cwd.to_owned(),
        environment.clone(),
        options.pipefail(),
        options.capture_limit(),
        span,
    )
    .with_supervisor_input(capsule);
    Ok(if completion {
        plan.with_supervisor_completion()
    } else {
        plan
    })
}

impl Interrupt {
    fn into_abort(self, source: Arc<SourceFile>) -> Abort {
        match self {
            Self::Runtime(error)
                if matches!(error.kind(), RuntimeErrorKind::StreamCancelled { .. }) =>
            {
                let RuntimeErrorKind::StreamCancelled { reason } = error.kind() else {
                    unreachable!("guard selected stream cancellation")
                };
                Abort::Cancelled(crate::eval::Cancellation::new(*reason, error.span()))
            }
            Self::Runtime(error) if error.source().is_some() => Abort::Error(error),
            Self::Runtime(error) => Abort::Error(error.with_source(source)),
            Self::Output(error) => Abort::Output(error),
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
    chain: &flash_syntax::ConditionalChain,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    binding_types: &Arc<RuntimeBindingTypes>,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    jobs: &mut Option<BackgroundJobs>,
    manage_foreground: bool,
    output: &mut ChainOutput<'_>,
) -> Result<ChainStep, Interrupt> {
    if let [and_chain] = chain.or_terms()
        && let [pipeline] = and_chain.and_terms()
    {
        return run_pipeline_checked(
            pipeline,
            state,
            scope,
            options,
            registry,
            source,
            binding_types,
            probe,
            platform,
            clock,
            jobs,
            manage_foreground,
            output,
        );
    }

    let mut or_terms = chain.or_terms().iter();
    let first = or_terms
        .next()
        .expect("a parsed conditional chain contains an operand");
    let mut step = run_and_chain(
        first,
        state,
        scope,
        options,
        registry,
        source,
        binding_types,
        probe,
        platform,
        clock,
        jobs,
        output,
    )?;
    for and_chain in or_terms {
        match &step {
            ChainStep::Exit(_) => return Ok(step),
            ChainStep::Stopped(_) => return Ok(step),
            // `||` runs the next operand only when the current one succeeded not.
            ChainStep::Status(status) if status.is_ok() => break,
            ChainStep::Status(_) => {}
        }
        step = run_and_chain(
            and_chain,
            state,
            scope,
            options,
            registry,
            source,
            binding_types,
            probe,
            platform,
            clock,
            jobs,
            output,
        )?;
    }
    Ok(step)
}

#[allow(clippy::too_many_arguments)]
fn run_and_chain(
    chain: &flash_syntax::AndChain,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    binding_types: &Arc<RuntimeBindingTypes>,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    jobs: &mut Option<BackgroundJobs>,
    output: &mut ChainOutput<'_>,
) -> Result<ChainStep, Interrupt> {
    let mut pipelines = chain.and_terms().iter();
    let first = pipelines
        .next()
        .expect("a parsed and-chain contains an operand");
    let mut step = run_pipeline_checked(
        first,
        state,
        scope,
        options,
        registry,
        source,
        binding_types,
        probe,
        platform,
        clock,
        jobs,
        false,
        output,
    )?;
    for pipeline in pipelines {
        match &step {
            ChainStep::Exit(_) => return Ok(step),
            ChainStep::Stopped(_) => return Ok(step),
            // `&&` runs the next operand only while the current one succeeds.
            ChainStep::Status(status) if !status.is_ok() => break,
            ChainStep::Status(_) => {}
        }
        step = run_pipeline_checked(
            pipeline,
            state,
            scope,
            options,
            registry,
            source,
            binding_types,
            probe,
            platform,
            clock,
            jobs,
            false,
            output,
        )?;
    }
    Ok(step)
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline_checked(
    pipeline: &flash_syntax::Pipeline,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    binding_types: &Arc<RuntimeBindingTypes>,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    jobs: &mut Option<BackgroundJobs>,
    manage_foreground: bool,
    output: &mut ChainOutput<'_>,
) -> Result<ChainStep, Interrupt> {
    let step = run_pipeline(
        pipeline,
        state,
        scope,
        options,
        registry,
        source,
        binding_types,
        probe,
        platform,
        clock,
        jobs,
        manage_foreground,
        output,
    )?;
    output.ensure_capture_within_limit()?;
    Ok(step)
}

#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    pipeline: &flash_syntax::Pipeline,
    state: &mut SessionState,
    scope: &mut ScopeStack,
    options: &SessionOptions,
    registry: &CommandRegistry,
    source: &SourceFile,
    binding_types: &Arc<RuntimeBindingTypes>,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    clock: &dyn Clock,
    jobs: &mut Option<BackgroundJobs>,
    manage_foreground: bool,
    output: &mut ChainOutput<'_>,
) -> Result<ChainStep, Interrupt> {
    let plan = plan_pipeline_with_options_and_binding_types(
        pipeline,
        state.cwd(),
        source,
        scope,
        state.environment(),
        registry,
        probe,
        options,
        Arc::clone(binding_types),
    )?;
    validate_job_builtin_arguments(&plan)?;

    if let Some((command, stage)) = sole_job_command(&plan) {
        return execute_job_builtin(command, stage, scope, state, jobs, platform)
            .map_err(Interrupt::from);
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
        let presentation = match output {
            ChainOutput::Session(_) => {
                let destination = output_destination(final_stage, platform)?;
                select_terminal_presentation(final_stage.output_carrier(), destination).map_err(
                    |error| {
                        RuntimeError::new(RuntimeErrorKind::Presentation(error), final_stage.span())
                    },
                )?
            }
            ChainOutput::Capture { .. } => None,
        };
        // Internal execution is transactional through final lazy rendering. A
        // producer can fail only when the sink pulls it, so committing session
        // state before presentation would leave a successful status behind a
        // failed pipeline.
        let mut pending_state = state.clone();
        let closure_environment = pending_state.environment().clone();
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
                route_final_payload(
                    payload,
                    presentation.as_ref(),
                    final_stage,
                    plan.cwd(),
                    platform,
                    output,
                )?;
                pending_state.environment_mut().apply_delta(
                    &closure_environment,
                    &closure_context.environment_snapshot(),
                );
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

    if manage_foreground
        && jobs
            .as_ref()
            .is_some_and(BackgroundJobs::manages_foreground)
    {
        let command = source
            .slice(plan.span())
            .map(escape_job_label)
            .expect("a planned pipeline span belongs to its source");
        let outcome = jobs
            .as_mut()
            .expect("foreground management requires a coordinator")
            .start_foreground(&plan, platform, command)?;
        return apply_foreground_completion(outcome, scope, state, plan.span(), false)
            .map_err(Interrupt::from);
    }

    let status = match output {
        ChainOutput::Session(_) => execute_foreground_status(&plan, platform, clock)?,
        ChainOutput::Capture { collector, .. } => {
            let mut collect = |chunk: &[u8]| collector.push(chunk);
            execute_foreground_with_stdout_drain(&plan, platform, clock, &mut collect)?
        }
    };
    Ok(ChainStep::Status(status))
}

fn validate_job_builtin_arguments(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    let has_external = plan
        .stages()
        .iter()
        .any(|stage| !matches!(stage.resolution(), PlannedResolution::Internal { .. }));
    for stage in plan.stages() {
        let PlannedResolution::Internal { canonical_name, .. } = stage.resolution() else {
            continue;
        };
        let Some(command) = job_command_name(canonical_name.as_str()) else {
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
    let PlannedResolution::Internal { canonical_name, .. } = stage.resolution() else {
        return None;
    };
    match job_command_name(canonical_name.as_str()) {
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
    scope: &mut ScopeStack,
    state: &mut SessionState,
    jobs: &mut Option<BackgroundJobs>,
    platform: &dyn Platform,
) -> Result<ChainStep, RuntimeError> {
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
        "fg" => {
            let target = match parse_optional_job_target(stage, command)? {
                Some(target) => target,
                None => coordinator.newest_foreground_eligible().ok_or_else(|| {
                    operation("no stopped or running background job to foreground".to_owned())
                })?,
            };
            let outcome = coordinator
                .foreground_job(target, platform)
                .map_err(|error| operation(error.to_string()))?;
            return apply_foreground_completion(outcome, scope, state, stage.span(), false);
        }
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
        "wait" => {
            let targets = parse_job_targets(stage, command)?;
            return coordinator
                .wait_for_jobs(&targets, platform)
                .map(ChainStep::Status)
                .map_err(|error| operation(error.to_string()));
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
    Ok(ChainStep::Status(
        Status::exit(0, Duration::ZERO).expect("zero is a valid job-command status"),
    ))
}

fn apply_foreground_completion(
    outcome: ForegroundJobOutcome,
    scope: &mut ScopeStack,
    state: &mut SessionState,
    span: Span,
    completion_required: bool,
) -> Result<ChainStep, RuntimeError> {
    let ForegroundJobOutcome::Completed {
        status,
        supervisor_completion,
    } = outcome
    else {
        let ForegroundJobOutcome::Stopped(job) = outcome else {
            unreachable!("foreground outcomes are completed or stopped")
        };
        return Ok(ChainStep::Stopped(job));
    };
    let Some(bytes) = supervisor_completion.filter(|bytes| !bytes.is_empty()) else {
        if completion_required {
            return Err(RuntimeError::new(
                RuntimeErrorKind::ExecutionCapsule {
                    message: "the foreground supervisor returned no completion".to_owned(),
                },
                span,
            ));
        }
        return Ok(ChainStep::Status(status));
    };
    let completion = crate::capsule::decode_supervisor_completion(&bytes).map_err(|error| {
        RuntimeError::new(
            RuntimeErrorKind::ExecutionCapsule {
                message: format!("invalid supervisor completion: {error}"),
            },
            span,
        )
    })?;
    let outcome = completion.outcome();
    let returned_status = completion.status().cloned();
    if matches!(outcome, crate::capsule::SupervisorOutcome::Continued) && returned_status.is_none()
    {
        return Err(RuntimeError::new(
            RuntimeErrorKind::ExecutionCapsule {
                message: "the foreground supervisor completion has no status".to_owned(),
            },
            span,
        ));
    }
    let mut pending_scope = scope.clone();
    let mut pending_state = state.clone();
    completion
        .apply(&mut pending_scope, &mut pending_state)
        .map_err(|error| {
            RuntimeError::new(
                RuntimeErrorKind::ExecutionCapsule {
                    message: error.to_string(),
                },
                span,
            )
        })?;
    *scope = pending_scope;
    *state = pending_state;
    match outcome {
        crate::capsule::SupervisorOutcome::Continued => Ok(ChainStep::Status(
            returned_status.expect("continued completion status was validated"),
        )),
        crate::capsule::SupervisorOutcome::Exit(code) => Ok(ChainStep::Exit(code)),
    }
}

/// Whether this all-internal plan begins with the read-only job table.
fn is_job_table_head(plan: &ExecutionPlan) -> bool {
    plan.stages().first().is_some_and(|stage| {
        matches!(
            stage.resolution(),
            PlannedResolution::Internal { canonical_name, .. } if canonical_name == "jobs"
        )
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
    output: &mut ChainOutput<'_>,
) -> Result<ChainStep, Interrupt> {
    let topology = plan
        .mixed_topology()
        .expect("the caller found a mixed pipeline topology");
    let last_internal = topology
        .internal_segments()
        .last()
        .expect("a mixed topology has an internal segment")
        .stages()
        .end
        .checked_sub(1)
        .expect("an internal segment is nonempty");

    let final_is_internal = last_internal + 1 == plan.stages().len();
    let presentation = if final_is_internal && !output.is_capture() {
        let final_stage = &plan.stages()[last_internal];
        let destination = output_destination(final_stage, platform)?;
        select_terminal_presentation(final_stage.output_carrier(), destination).map_err(
            |error| RuntimeError::new(RuntimeErrorKind::Presentation(error), final_stage.span()),
        )?
    } else {
        None
    };

    let closure_environment = state.environment().clone();
    let preparation = Arc::new(MixedPreparation::new(state.clone()));
    let mut mixed = start_mixed_pipeline(plan, platform, clock, output.is_capture())?;
    let control = mixed.control();
    let mut segments = mixed.take_segments();
    let final_segment = final_is_internal.then(|| {
        segments
            .pop()
            .expect("the final internal segment has resources")
    });

    let captured_output = mixed.take_captured_output();
    let mut segment_results = if let Some(reader) = captured_output {
        let ChainOutput::Capture { collector, .. } = output else {
            unreachable!("only capture installs a mixed final-output reader")
        };
        let producer_span = plan
            .stages()
            .last()
            .expect("a mixed pipeline has a final stage")
            .span();
        let (results, drain_result) = thread::scope(|scope| {
            let drain_control = control.clone();
            let drain = scope.spawn(move || {
                let result = drain_capture_endpoint(platform, reader, collector, producer_span);
                if result.is_err() {
                    drain_control.cancel_and_reap();
                }
                result
            });
            let mut workers = Vec::with_capacity(segments.len());
            for segment in segments {
                let preparation = Arc::clone(&preparation);
                let closure_environment = closure_environment.clone();
                let control = control.clone();
                workers.push(scope.spawn(move || {
                    let result = run_mixed_segment(
                        segment,
                        plan,
                        registry,
                        source,
                        probe,
                        platform,
                        preparation,
                        closure_environment,
                        control.clone(),
                        None,
                        None,
                    );
                    if result.as_ref().is_err_and(|failure| !failure.triggered) {
                        control.cancel_and_reap();
                    }
                    result
                }));
            }
            let results = workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .expect("a mixed segment worker must not panic")
                })
                .collect::<Vec<_>>();
            let drain_result = drain.join().expect("a mixed capture drain must not panic");
            (results, drain_result)
        });
        if let Err(error) = drain_result {
            mixed.terminate();
            return Err(Interrupt::Runtime(error));
        }
        results
    } else {
        thread::scope(|scope| {
            let mut workers = Vec::with_capacity(segments.len());
            for segment in segments {
                let preparation = Arc::clone(&preparation);
                let closure_environment = closure_environment.clone();
                let control = control.clone();
                workers.push(scope.spawn(move || {
                    let result = run_mixed_segment(
                        segment,
                        plan,
                        registry,
                        source,
                        probe,
                        platform,
                        preparation,
                        closure_environment,
                        control.clone(),
                        None,
                        None,
                    );
                    if result.as_ref().is_err_and(|failure| !failure.triggered) {
                        control.cancel_and_reap();
                    }
                    result
                }));
            }

            let final_result = final_segment.map(|segment| {
                let result = run_mixed_segment(
                    segment,
                    plan,
                    registry,
                    source,
                    probe,
                    platform,
                    Arc::clone(&preparation),
                    closure_environment.clone(),
                    control.clone(),
                    presentation.as_ref(),
                    Some(output),
                );
                if result.as_ref().is_err_and(|failure| !failure.triggered) {
                    control.cancel_and_reap();
                }
                result
            });

            let mut results = workers
                .into_iter()
                .map(|worker| {
                    worker
                        .join()
                        .expect("a mixed segment worker must not panic")
                })
                .collect::<Vec<_>>();
            if let Some(result) = final_result {
                results.push(result);
            }
            results
        })
    };

    segment_results.sort_by_key(|result| match result {
        Ok(result) => result.ordinal(),
        Err(failure) => failure.stage_index,
    });
    let requested_exit_stage = segment_results.iter().find_map(|result| match result {
        Ok(MixedSegmentResult::Exit { stage_index, .. }) => Some(*stage_index),
        _ => None,
    });
    let failure_index = segment_results
        .iter()
        .position(|result| {
            matches!(
                result,
                Err(failure)
                    if !failure.triggered
                        && requested_exit_stage
                            .is_none_or(|exit_stage| failure.stage_index < exit_stage)
            )
        })
        .or_else(|| {
            requested_exit_stage
                .is_none()
                .then(|| segment_results.iter().position(Result::is_err))
                .flatten()
        });
    if let Some(failure_index) = failure_index {
        let Err(failure) = segment_results.remove(failure_index) else {
            unreachable!("the selected mixed segment result is a failure");
        };
        mixed.terminate();
        return Err(*failure.interrupt);
    }

    let mut pending_state = preparation
        .state
        .lock()
        .expect("mixed preparation state must not be poisoned")
        .pending_state
        .clone();
    let mut status_slots = StageStatusSlots::new(plan.stages().len());
    let mut deferred_checks = Vec::new();
    let mut requested_exit = None;
    for result in segment_results.into_iter().flatten() {
        match result {
            MixedSegmentResult::Completed {
                statuses,
                deferred_checks: segment_checks,
                closure_environment: updated,
                ..
            } => {
                for (index, status) in statuses {
                    status_slots.complete(index, status);
                }
                deferred_checks.extend(segment_checks);
                pending_state
                    .environment_mut()
                    .apply_delta(&closure_environment, &updated);
            }
            MixedSegmentResult::Exit {
                code,
                closure_environment: updated,
                ..
            } => {
                pending_state
                    .environment_mut()
                    .apply_delta(&closure_environment, &updated);
                requested_exit = Some(code);
            }
            MixedSegmentResult::StoppedByExit {
                closure_environment: updated,
                ..
            } => {
                pending_state
                    .environment_mut()
                    .apply_delta(&closure_environment, &updated);
            }
            MixedSegmentResult::Skipped { .. } => {}
        }
    }
    if let Some(code) = requested_exit {
        mixed.terminate();
        *state = pending_state;
        return Ok(ChainStep::Exit(code));
    }

    let (external_statuses, pipeline_duration) = mixed.wait(plan, platform, clock)?;
    for (index, status) in external_statuses {
        status_slots.complete(index, status);
    }
    deferred_checks.sort_by_key(|check| check.check_stage);
    for check in deferred_checks {
        let upstream = status_slots.completed(check.upstream_stage);
        if !upstream.is_ok() {
            return Err(Interrupt::Runtime(RuntimeError::new(
                RuntimeErrorKind::UnsuccessfulStatus {
                    status: Box::new(upstream.clone()),
                },
                plan.stages()[check.check_stage].span(),
            )));
        }
    }
    let status = aggregate_statuses(
        status_slots.into_statuses(),
        plan.pipefail(),
        pipeline_duration,
    );
    pending_state.set_current_status(Some(status.clone()));
    *state = pending_state;
    Ok(ChainStep::Status(status))
}

enum StageStatusSlot {
    Pending,
    Completed(Status),
}

struct StageStatusSlots {
    slots: Vec<StageStatusSlot>,
}

impl StageStatusSlots {
    fn new(stage_count: usize) -> Self {
        Self {
            slots: std::iter::repeat_with(|| StageStatusSlot::Pending)
                .take(stage_count)
                .collect(),
        }
    }

    fn complete(&mut self, index: usize, status: Status) {
        let slot = self
            .slots
            .get_mut(index)
            .expect("a mixed status index belongs to the source plan");
        assert!(
            matches!(slot, StageStatusSlot::Pending),
            "a mixed source stage completes exactly once"
        );
        *slot = StageStatusSlot::Completed(status);
    }

    fn completed(&self, index: usize) -> &Status {
        match self
            .slots
            .get(index)
            .expect("a deferred check names a source stage")
        {
            StageStatusSlot::Completed(status) => status,
            StageStatusSlot::Pending => {
                panic!("a deferred check runs only after its upstream status completes")
            }
        }
    }

    fn into_statuses(self) -> Vec<Status> {
        self.slots
            .into_iter()
            .map(|slot| match slot {
                StageStatusSlot::Completed(status) => status,
                StageStatusSlot::Pending => {
                    panic!("a mixed pipeline aggregates only complete status slots")
                }
            })
            .collect()
    }
}

struct DeferredCheck {
    check_stage: usize,
    upstream_stage: usize,
}

struct MixedPreparation {
    state: Mutex<MixedPreparationState>,
    ready: Condvar,
}

impl MixedPreparation {
    fn new(pending_state: SessionState) -> Self {
        Self {
            state: Mutex::new(MixedPreparationState {
                next_segment: 0,
                stopped: false,
                requested_exit: None,
                pending_state,
            }),
            ready: Condvar::new(),
        }
    }
}

struct MixedPreparationState {
    next_segment: usize,
    stopped: bool,
    requested_exit: Option<MixedExit>,
    pending_state: SessionState,
}

#[derive(Clone, Copy)]
struct MixedExit {
    ordinal: usize,
}

struct MixedSegmentFailure {
    stage_index: usize,
    triggered: bool,
    interrupt: Box<Interrupt>,
}

impl MixedSegmentFailure {
    fn runtime(
        stage_index: usize,
        error: RuntimeError,
        cancellation_was_active: bool,
        control: &MixedPipelineControl,
    ) -> Self {
        let triggered = cancellation_was_active
            || (control.is_cancelled()
                && matches!(error.kind(), RuntimeErrorKind::StreamCancelled { .. }));
        Self {
            stage_index,
            triggered,
            interrupt: Box::new(Interrupt::Runtime(error)),
        }
    }

    fn interrupt(
        stage_index: usize,
        interrupt: Interrupt,
        cancellation_was_active: bool,
        control: &MixedPipelineControl,
    ) -> Self {
        let triggered = cancellation_was_active
            || (control.is_cancelled()
                && matches!(
                    &interrupt,
                    Interrupt::Runtime(error)
                        if matches!(error.kind(), RuntimeErrorKind::StreamCancelled { .. })
                ));
        Self {
            stage_index,
            triggered,
            interrupt: Box::new(interrupt),
        }
    }
}

enum MixedSegmentResult {
    Completed {
        ordinal: usize,
        statuses: Vec<(usize, Status)>,
        deferred_checks: Vec<DeferredCheck>,
        closure_environment: Environment,
    },
    Exit {
        ordinal: usize,
        stage_index: usize,
        code: u8,
        closure_environment: Environment,
    },
    StoppedByExit {
        ordinal: usize,
        closure_environment: Environment,
    },
    Skipped {
        ordinal: usize,
    },
}

impl MixedSegmentResult {
    const fn ordinal(&self) -> usize {
        match self {
            Self::Completed { ordinal, .. }
            | Self::Exit { ordinal, .. }
            | Self::StoppedByExit { ordinal, .. }
            | Self::Skipped { ordinal } => *ordinal,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_mixed_segment(
    mut resource: MixedSegment,
    plan: &ExecutionPlan,
    registry: &CommandRegistry,
    source: &SourceFile,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    preparation: Arc<MixedPreparation>,
    closure_environment: Environment,
    control: MixedPipelineControl,
    presentation: Option<&TerminalPresentation>,
    output: Option<&mut ChainOutput<'_>>,
) -> Result<MixedSegmentResult, MixedSegmentFailure> {
    let ordinal = resource.segment().ordinal();
    let stages = resource.segment().stages();
    let first_stage = stages.start;
    let last_stage = stages
        .end
        .checked_sub(1)
        .expect("an internal segment is nonempty");
    let mut payload = resource
        .take_input()
        .map_or(InternalPayload::Empty, |reader| {
            InternalPayload::ByteStream(pipe_byte_stream(reader, plan.stages()[first_stage].span()))
        });
    let cancellation_control = control.clone();
    let closure_context = OwnedClosureContext::new(
        source.clone(),
        closure_environment,
        EvalLimits::new(
            CancellationToken::from_fn(move || cancellation_control.is_cancelled()),
            ResourceBudget::unlimited(),
        ),
    );
    let mut statuses = Vec::with_capacity(stages.len());
    let mut deferred_checks = Vec::new();

    let mut prepared = preparation
        .state
        .lock()
        .expect("mixed preparation state must not be poisoned");
    while !prepared.stopped && prepared.next_segment != ordinal {
        prepared = preparation
            .ready
            .wait(prepared)
            .expect("mixed preparation state must not be poisoned");
    }
    if prepared.stopped {
        return Ok(MixedSegmentResult::Skipped { ordinal });
    }

    for index in stages {
        let cancellation_was_active = control.is_cancelled();
        if cancellation_was_active {
            prepared.stopped = true;
            preparation.ready.notify_all();
            return Ok(MixedSegmentResult::Skipped { ordinal });
        }
        let stage = &plan.stages()[index];
        let PlannedResolution::Internal { canonical_name, .. } = stage.resolution() else {
            unreachable!("mixed topology contains only internal segment stages");
        };
        let deferred_check = canonical_name == "check"
            && index == first_stage
            && index > 0
            && matches!(
                plan.stages()[index - 1].resolution(),
                PlannedResolution::External { .. }
            );
        // The ordinary check path still validates its carrier and arity and
        // creates its zero-valued leaf. This provisional status is never used
        // as evidence: the coordinator retains the exact external dependency
        // below and must validate it after the child has been waited.
        let provisional_upstream = deferred_check.then(|| {
            Status::exit(0, Duration::ZERO).expect("a deferred check placeholder is valid")
        });
        let upstream = provisional_upstream
            .as_ref()
            .or_else(|| statuses.last().map(|(_, status)| status));
        match execute_stage(
            canonical_name,
            stage,
            payload,
            upstream,
            &mut prepared.pending_state,
            registry,
            probe,
            platform,
            plan.cwd(),
            &closure_context,
        ) {
            Ok(StageOutcome::Completed {
                payload: output_payload,
                status,
            }) => {
                payload = output_payload;
                statuses.push((index, status));
                if deferred_check {
                    deferred_checks.push(DeferredCheck {
                        check_stage: index,
                        upstream_stage: index - 1,
                    });
                }
            }
            Ok(StageOutcome::Exit(code)) => {
                prepared.stopped = true;
                prepared.requested_exit = Some(MixedExit { ordinal });
                preparation.ready.notify_all();
                let result = MixedSegmentResult::Exit {
                    ordinal,
                    stage_index: index,
                    code,
                    closure_environment: closure_context.environment_snapshot(),
                };
                drop(prepared);
                drop(resource);
                control.cancel_and_reap();
                return Ok(result);
            }
            Err(error) => {
                prepared.stopped = true;
                preparation.ready.notify_all();
                return Err(MixedSegmentFailure::runtime(
                    index,
                    error,
                    cancellation_was_active,
                    &control,
                ));
            }
        }
    }
    prepared.next_segment += 1;
    preparation.ready.notify_all();
    drop(prepared);

    if control.is_cancelled() {
        let requested_exit = preparation
            .state
            .lock()
            .expect("mixed preparation state must not be poisoned")
            .requested_exit;
        if requested_exit.is_some_and(|exit| ordinal < exit.ordinal) {
            return Ok(MixedSegmentResult::StoppedByExit {
                ordinal,
                closure_environment: closure_context.environment_snapshot(),
            });
        }
        return Ok(MixedSegmentResult::Skipped { ordinal });
    }
    let cancellation_was_active = false;
    let drained = if let Some(writer) = resource.take_output() {
        drain_payload_to_pipe(payload, writer, plan.stages()[last_stage].span())
    } else {
        route_final_payload(
            payload,
            presentation,
            &plan.stages()[last_stage],
            plan.cwd(),
            platform,
            output.expect("a final internal segment owns the session output"),
        )
    };
    if let Err(interrupt) = drained {
        let mut prepared = preparation
            .state
            .lock()
            .expect("mixed preparation state must not be poisoned");
        prepared.stopped = true;
        preparation.ready.notify_all();
        if prepared
            .requested_exit
            .is_some_and(|exit| ordinal < exit.ordinal)
            && matches!(
                &interrupt,
                Interrupt::Runtime(error)
                    if matches!(error.kind(), RuntimeErrorKind::StreamCancelled { .. })
            )
        {
            return Ok(MixedSegmentResult::StoppedByExit {
                ordinal,
                closure_environment: closure_context.environment_snapshot(),
            });
        }
        return Err(MixedSegmentFailure::interrupt(
            last_stage,
            interrupt,
            cancellation_was_active,
            &control,
        ));
    }

    Ok(MixedSegmentResult::Completed {
        ordinal,
        statuses,
        deferred_checks,
        closure_environment: closure_context.environment_snapshot(),
    })
}

fn pipe_byte_stream(
    mut reader: Box<dyn DescriptorEndpoint>,
    span: flash_syntax::Span,
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

fn drain_capture_endpoint(
    platform: &dyn Platform,
    reader: Box<dyn DescriptorEndpoint>,
    collector: &mut BoundedCapture,
    span: Span,
) -> Result<(), RuntimeError> {
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut chunk = [0; CHUNK_SIZE];
    loop {
        let amount = platform
            .read_descriptor(reader.as_ref(), &mut chunk)
            .map_err(|error| RuntimeError::new(RuntimeErrorKind::CaptureRead(error), span))?;
        if amount == 0 {
            return Ok(());
        }
        collector.push(&chunk[..amount]);
    }
}

fn drain_payload_to_pipe(
    payload: InternalPayload,
    mut writer: Box<dyn DescriptorEndpoint>,
    span: flash_syntax::Span,
) -> Result<(), Interrupt> {
    let mut bytes = match payload {
        InternalPayload::Empty => return Ok(()),
        InternalPayload::ByteStream(bytes) => bytes,
        InternalPayload::Value(_) | InternalPayload::ValueStream(_) => {
            return Err(Interrupt::Runtime(RuntimeError::new(
                RuntimeErrorKind::Unsupported {
                    feature: "an implicit structured-to-external conversion",
                },
                span,
            )));
        }
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
    if !matches!(
        internal_stdout_route(stage, false),
        InternalStdoutRoute::Default
    ) {
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
    span: flash_syntax::Span,
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
            let mut records = Vec::new();
            loop {
                match values.pull() {
                    StreamPull::Item(Value::Record(record)) => {
                        if records.len() == DEFAULT_MATERIALIZATION_LIMIT {
                            return Err(Interrupt::Runtime(RuntimeError::new(
                                RuntimeErrorKind::StructuredCommand {
                                    command: "presentation",
                                    message: format!(
                                        "record table exceeds the {}-item materialization limit",
                                        DEFAULT_MATERIALIZATION_LIMIT
                                    ),
                                },
                                span,
                            )));
                        }
                        records.push(record);
                    }
                    StreamPull::Item(value) => {
                        write_record_table(&mut records, presentation, sink)
                            .map_err(Interrupt::Output)?;
                        write_value(&value, presentation, sink).map_err(Interrupt::Output)?;
                    }
                    StreamPull::End => {
                        write_record_table(&mut records, presentation, sink)
                            .map_err(Interrupt::Output)?;
                        return Ok(());
                    }
                    StreamPull::Failed(error) => {
                        write_record_table(&mut records, presentation, sink)
                            .map_err(Interrupt::Output)?;
                        return Err(Interrupt::Runtime(error));
                    }
                    StreamPull::Cancelled(reason) => {
                        write_record_table(&mut records, presentation, sink)
                            .map_err(Interrupt::Output)?;
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

fn route_final_payload(
    payload: InternalPayload,
    presentation: Option<&TerminalPresentation>,
    stage: &PlannedStage,
    cwd: &Path,
    platform: &dyn Platform,
    output: &mut ChainOutput<'_>,
) -> Result<(), Interrupt> {
    match output {
        ChainOutput::Session(sink) => {
            render_payload_with_redirection(payload, presentation, stage, cwd, platform, *sink)
        }
        ChainOutput::Capture { collector, .. } => match internal_stdout_route(stage, false) {
            InternalStdoutRoute::Default => {
                capture_internal_payload(payload, stage.span(), collector)
            }
            InternalStdoutRoute::File { target, mode } => {
                drain_payload_to_redirected_file(payload, target, mode, stage, cwd, platform)
            }
            InternalStdoutRoute::Unsupported => Err(Interrupt::Runtime(RuntimeError::new(
                RuntimeErrorKind::Unsupported {
                    feature: "this resolved stdout route on an internal byte stream",
                },
                stage.span(),
            ))),
        },
    }
}

fn capture_internal_payload(
    payload: InternalPayload,
    span: Span,
    collector: &mut BoundedCapture,
) -> Result<(), Interrupt> {
    match payload {
        InternalPayload::Empty | InternalPayload::ByteStream(_) => {
            render_payload(payload, None, span, collector)
        }
        InternalPayload::Value(value) => {
            capture_text_values(ValueStream::from_values(vec![value]), span, collector)
        }
        InternalPayload::ValueStream(values) => capture_text_values(values, span, collector),
    }
}

fn capture_text_values(
    values: ValueStream,
    span: Span,
    collector: &mut BoundedCapture,
) -> Result<(), Interrupt> {
    let mut writer = to_text(values);
    loop {
        match writer.pull() {
            ToTextStep::Chunk(chunk) => collector.push(&chunk),
            ToTextStep::End => return Ok(()),
            ToTextStep::NotEncodable { actual } => {
                return Err(Interrupt::Runtime(RuntimeError::new(
                    RuntimeErrorKind::StructuredCommand {
                        command: "command substitution",
                        message: format!(
                            "text cannot encode {actual}; add an explicit `to`/`encode` boundary"
                        ),
                    },
                    span,
                )));
            }
            ToTextStep::Failed(error) => return Err(Interrupt::Runtime(error)),
            ToTextStep::Cancelled(reason) => {
                return Err(Interrupt::Runtime(RuntimeError::new(
                    RuntimeErrorKind::StreamCancelled { reason },
                    span,
                )));
            }
        }
    }
}

fn render_payload_with_redirection(
    payload: InternalPayload,
    presentation: Option<&TerminalPresentation>,
    stage: &PlannedStage,
    cwd: &Path,
    platform: &dyn Platform,
    sink: &mut dyn Write,
) -> Result<(), Interrupt> {
    match internal_stdout_route(stage, false) {
        InternalStdoutRoute::Default => render_payload(payload, presentation, stage.span(), sink),
        InternalStdoutRoute::File { target, mode } => {
            drain_payload_to_redirected_file(payload, target, mode, stage, cwd, platform)
        }
        InternalStdoutRoute::Unsupported => Err(Interrupt::Runtime(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "this resolved stdout route on an internal byte stream",
            },
            stage.span(),
        ))),
    }
}

fn drain_payload_to_redirected_file(
    payload: InternalPayload,
    target: &ExpandedWord,
    mode: OutputMode,
    stage: &PlannedStage,
    cwd: &Path,
    platform: &dyn Platform,
) -> Result<(), Interrupt> {
    let mode = match mode {
        OutputMode::Truncate => FileOpenMode::WriteTruncate,
        OutputMode::Append => FileOpenMode::WriteAppend,
    };
    let endpoint = platform
        .open_file(FileOpenRequest::new(Path::new(target.value()), cwd, mode))
        .map_err(|error| {
            Interrupt::Runtime(RuntimeError::new(
                RuntimeErrorKind::RedirectionSetup(error),
                target.span(),
            ))
        })?;
    drain_payload_to_pipe(payload, endpoint, stage.span())
}

/// Present one consecutive run of records as a single width-aware table.
///
/// Record streams remain lazy throughout the pipeline. Only their final human
/// presentation materializes a bounded run, because column widths and the
/// first-seen union of record keys cannot be known before the run is complete.
fn write_record_table(
    records: &mut Vec<Record>,
    presentation: &TerminalPresentation,
    sink: &mut dyn Write,
) -> io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let table = Table::from_records(std::mem::take(records));
    writeln!(sink, "{}", render_table(&table, presentation.columns()))
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
    let rendered = render_runtime_diagnostic(source, error, std::iter::empty());
    SubmitError::Runtime {
        error: Box::new(error.clone()),
        rendered,
    }
}

pub(crate) fn render_runtime_diagnostic(
    submitting_source: &SourceFile,
    error: &RuntimeError,
    additional_sources: impl IntoIterator<Item = SourceFile>,
) -> String {
    let mut diagnostic = Diagnostic::new(Severity::Error, "RUN001", error.to_string())
        .with_primary(error.span(), "runtime failure");
    for label in error.labels() {
        diagnostic = diagnostic.with_secondary(label.span(), label.message());
    }
    for frame in error.frames() {
        diagnostic = diagnostic.with_secondary(frame.call_site(), "called from here");
    }
    let mut sources = Vec::<SourceFile>::new();
    if let Some(primary) = error.source() {
        sources.push(primary.clone());
    }
    for label in error.labels() {
        if !sources
            .iter()
            .any(|candidate| candidate.id() == label.source().id())
        {
            sources.push(label.source().clone());
        }
    }
    for frame in error.frames() {
        if !sources
            .iter()
            .any(|candidate| candidate.id() == frame.source().id())
        {
            sources.push(frame.source().clone());
        }
    }
    if !sources
        .iter()
        .any(|candidate| candidate.id() == submitting_source.id())
    {
        sources.push(submitting_source.clone());
    }
    for additional in additional_sources {
        if !sources
            .iter()
            .any(|candidate| candidate.id() == additional.id())
        {
            sources.push(additional);
        }
    }
    flash_syntax::render_diagnostic_sources(sources.iter(), &diagnostic)
        .expect("runtime diagnostics retain every referenced evaluation source")
}
