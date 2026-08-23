//! Foreground execution of inspectable command plans.
//!
//! The executor handles external stages with inherited standard descriptors,
//! byte-pipeline assignments, and source-ordered redirections. It always runs
//! platform-independent preflight before touching the platform, starts every
//! stage before waiting, and never renders shell source.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use flash_platform::{
    Capability, ChildDescriptor, ChildProcess, DescriptorEndpoint, FileOpenMode, FileOpenRequest,
    ForegroundTerminalGuard, JobSignal, Platform, ProcessGroup, ProcessGroupId, ProcessStatus,
    ProcessTransition, SpawnRequest,
};
use flash_syntax::{ConditionalChain, OutputMode, PipeOperator, Pipeline, SourceFile};

use crate::command::CommandRegistry;
use crate::eval::{AUTOMATIC_RESUME_LIMIT, Clock, Instant, RuntimeError, RuntimeErrorKind};
use crate::job::ProcessId;
use crate::plan::{
    ExecutionPlan, InternalSegment, InternalStdoutRoute, PlannedRedirection, PlannedResolution,
    ProcessGroupPolicy, RedirectionAction, SessionOptions, internal_stdout_route,
    plan_pipeline_with_options, preflight,
};
use crate::resolve::ExecutableProbe;
use crate::{Duration, Environment, ScopeStack, Signal, Status};

/// Captured command output paired with its normal completion status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandCapture<T> {
    output: T,
    status: Status,
}

impl<T> CommandCapture<T> {
    /// The captured output.
    #[must_use]
    pub const fn output(&self) -> &T {
        &self.output
    }

    /// The nested command or aggregate pipeline status.
    #[must_use]
    pub const fn status(&self) -> &Status {
        &self.status
    }

    /// Consume the capture into its output and status.
    #[must_use]
    pub fn into_parts(self) -> (T, Status) {
        (self.output, self.status)
    }
}

/// Execute one external foreground stage.
///
/// A nonzero exit or signal termination is a normal [`ProcessStatus`]. Spawn
/// and wait failures are source-anchored runtime errors. Internal commands
/// remain unsupported until built-in execution is added.
pub fn execute_foreground(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
) -> Result<ProcessStatus, RuntimeError> {
    preflight(plan)?;

    if plan.stages().len() != 1 {
        return Err(RuntimeError::new(
            // flash-v1-boundary(embedding-refusal): This narrow API accepts one stage; the pipeline API owns multiple stages.
            RuntimeErrorKind::Unsupported {
                feature: "a foreground pipeline with more than one stage",
            },
            plan.span(),
        ));
    }

    let mut statuses = execute_preflighted_pipeline(plan, platform)?;
    Ok(statuses
        .pop()
        .expect("a one-stage plan produces one process status"))
}

/// Execute an arbitrary-length external foreground byte pipeline.
///
/// Every edge receives one uniquely owned pipe. The final descriptor map for
/// each stage is passed to direct spawn, all parent endpoint owners are released
/// immediately after their stage starts, and no child is waited before every
/// stage has spawned. The returned low-level statuses remain in source order;
/// [`execute_foreground_status`] adds language-level timing and aggregation.
pub fn execute_foreground_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
) -> Result<Vec<ProcessStatus>, RuntimeError> {
    preflight(plan)?;
    execute_preflighted_pipeline(plan, platform)
}

/// Execute a foreground pipeline and return its language-level completion
/// status.
///
/// Each completed process becomes a source-ordered leaf status. A multi-stage
/// plan returns an aggregate selected by the plan's snapshotted `pipefail`
/// option; a one-stage plan returns its leaf directly. Nonzero exits and signal
/// termination remain normal completion.
pub fn execute_foreground_status(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<Status, RuntimeError> {
    preflight(plan)?;
    let pipeline_started = clock.now();
    let completions = execute_preflighted_pipeline_timed(plan, platform, clock)?;
    let pipeline_duration = elapsed(pipeline_started, clock.now());
    Ok(aggregate_language_status(
        plan,
        completions,
        pipeline_duration,
    ))
}

/// Execute a foreground pipeline while incrementally draining its final stdout.
///
/// A dedicated scoped thread begins reading before the first child wait, so a
/// producer may emit more than one pipe buffer without deadlocking. `drain`
/// receives one borrowed chunk at a time; the executor never accumulates output.
/// A stage-local stdout redirection still wins because capture plumbing is
/// installed before source-ordered redirections. Text decoding, byte collection,
/// and capture limits belong to the command-substitution layer built on top.
pub fn execute_foreground_with_stdout_drain<D>(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
    drain: &mut D,
) -> Result<Status, RuntimeError>
where
    D: FnMut(&[u8]) + Send,
{
    preflight(plan)?;
    validate_preflighted_external_plan(plan)?;
    let producer_span = plan
        .stages()
        .last()
        .map_or(plan.span(), crate::plan::PlannedStage::span);
    let (reader, writer) = platform
        .pipe()
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::CapturePipe(error), producer_span))?
        .into_parts();
    let pipeline_started = clock.now();
    let started = start_preflighted_pipeline(plan, platform, Some(clock), Some(writer), false)?;
    // Held across the drain and the wait; `Drop` is the backstop that returns
    // the terminal even when the drain callback panics.
    let foreground = take_foreground(plan, platform, started.group)?;
    let group = started.group;
    let children = started.children;

    let (wait_result, drain_result) = thread::scope(|scope| {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let drain_task =
            scope.spawn(move || drain_stdout(platform, reader, drain, producer_span, ready_sender));
        ready_receiver
            .recv()
            .expect("the drain task signals before returning");
        let wait_result = wait_in_source_order(children, plan, platform, group, Some(clock));
        let drain_result = drain_task
            .join()
            .expect("a drain callback panic is an implementation failure");
        (wait_result, drain_result)
    });

    let released = release_foreground(foreground, plan);
    let completions = wait_result?;
    drain_result?;
    released?;
    let pipeline_duration = elapsed(pipeline_started, clock.now());
    Ok(aggregate_language_status(
        plan,
        completions,
        pipeline_duration,
    ))
}

/// Capture a foreground pipeline's stdout as exact bytes with bounded storage.
///
/// The plan's snapshotted capture limit counts raw bytes. Once exceeded, the
/// collector stops retaining data but continues draining through EOF and reaps
/// every child before returning [`RuntimeErrorKind::CaptureLimitExceeded`].
pub fn capture_foreground_bytes(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<CommandCapture<Vec<u8>>, RuntimeError> {
    let mut collector = BoundedCapture::new(plan.capture_limit());
    let status = {
        let mut collect = |chunk: &[u8]| collector.push(chunk);
        execute_foreground_with_stdout_drain(plan, platform, clock, &mut collect)?
    };
    collector.finish(status, plan.span())
}

/// Capture a foreground pipeline's stdout as strict UTF-8 text.
///
/// Every trailing LF or CRLF sequence is removed after decoding. A lone
/// trailing carriage return remains data. Nonzero and signal statuses are
/// returned normally beside the text.
pub fn capture_foreground_text(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<CommandCapture<String>, RuntimeError> {
    let captured = capture_foreground_bytes(plan, platform, clock)?;
    decode_text_capture(captured, plan.span())
}

/// Execute the conditional-chain body of a command substitution and capture all
/// stdout from every reached pipeline as exact bytes.
///
/// `&&` and `||` retain their ordinary status short-circuit behavior. The one
/// session capture limit spans the complete chain rather than resetting for
/// each reached pipeline.
#[allow(clippy::too_many_arguments)]
pub fn capture_command_substitution_bytes(
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
    crate::session::capture_command_substitution_compat(
        chain,
        cwd,
        source,
        scope,
        environment,
        registry,
        probe,
        options,
        platform,
        clock,
    )
}

/// Execute the conditional-chain body of a command substitution and capture all
/// reached stdout as strict UTF-8 text with trailing line endings removed.
#[allow(clippy::too_many_arguments)]
pub fn capture_command_substitution_text(
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
) -> Result<CommandCapture<String>, RuntimeError> {
    let captured = capture_command_substitution_bytes(
        chain,
        cwd,
        source,
        scope,
        environment,
        registry,
        probe,
        options,
        platform,
        clock,
    )?;
    decode_text_capture(captured, chain.span())
}

pub(crate) fn decode_text_capture(
    captured: CommandCapture<Vec<u8>>,
    span: flash_syntax::Span,
) -> Result<CommandCapture<String>, RuntimeError> {
    let (bytes, status) = captured.into_parts();
    let output = decode_text_bytes(bytes, span)?;
    Ok(CommandCapture { output, status })
}

pub(crate) fn decode_text_bytes(
    bytes: Vec<u8>,
    span: flash_syntax::Span,
) -> Result<String, RuntimeError> {
    let mut output = match String::from_utf8(bytes) {
        Ok(output) => output,
        Err(error) => {
            let utf8 = error.utf8_error();
            return Err(RuntimeError::new(
                RuntimeErrorKind::CaptureInvalidUtf8 {
                    valid_up_to: utf8.valid_up_to(),
                    error_len: utf8.error_len(),
                },
                span,
            ));
        }
    };
    trim_trailing_line_endings(&mut output);
    Ok(output)
}

pub(crate) struct BoundedCapture {
    output: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedCapture {
    pub(crate) const fn new(limit: usize) -> Self {
        Self {
            output: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        let remaining = self.limit.saturating_sub(self.output.len());
        let retained = remaining.min(chunk.len());
        self.output.extend_from_slice(&chunk[..retained]);
        self.exceeded |= retained != chunk.len();
    }

    pub(crate) fn finish(
        self,
        status: Status,
        span: flash_syntax::Span,
    ) -> Result<CommandCapture<Vec<u8>>, RuntimeError> {
        self.ensure_within_limit(span)?;
        Ok(CommandCapture {
            output: self.output,
            status,
        })
    }

    pub(crate) fn ensure_within_limit(&self, span: flash_syntax::Span) -> Result<(), RuntimeError> {
        if self.exceeded {
            return Err(RuntimeError::new(
                RuntimeErrorKind::CaptureLimitExceeded { limit: self.limit },
                span,
            ));
        }
        Ok(())
    }
}

impl std::io::Write for BoundedCapture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.push(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn trim_trailing_line_endings(output: &mut String) {
    while output.ends_with('\n') {
        output.pop();
        if output.ends_with('\r') {
            output.pop();
        }
    }
}

fn aggregate_language_status(
    plan: &ExecutionPlan,
    completions: Vec<StageCompletion>,
    pipeline_duration: Duration,
) -> Status {
    let stages: Vec<Status> = completions
        .into_iter()
        .map(|completion| language_status(completion.status, completion.duration))
        .collect();
    aggregate_statuses(stages, plan.pipefail(), pipeline_duration)
}

/// Plan and execute a foreground external-command conditional chain.
///
/// Pipelines are planned only when reached. `&&` continues after a successful
/// status, while `||` continues after an unsuccessful status; the returned
/// value is the last status actually evaluated. Planning or execution errors
/// abort the chain and do not activate `||`.
#[allow(clippy::too_many_arguments)]
pub fn execute_foreground_chain(
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
) -> Result<Status, RuntimeError> {
    execute_conditional_chain_with(chain, &mut |pipeline| {
        plan_and_execute(
            pipeline,
            cwd,
            source,
            scope,
            environment,
            registry,
            probe,
            options,
            platform,
            clock,
        )
    })
}

fn execute_conditional_chain_with<E>(
    chain: &ConditionalChain,
    execute: &mut E,
) -> Result<Status, RuntimeError>
where
    E: FnMut(&Pipeline) -> Result<Status, RuntimeError>,
{
    let mut or_terms = chain.or_terms().iter();
    let first = or_terms
        .next()
        .expect("a parsed conditional chain contains an operand");
    let mut status = execute_and_chain_with(first, execute)?;
    for and_chain in or_terms {
        if status.is_ok() {
            break;
        }
        status = execute_and_chain_with(and_chain, execute)?;
    }
    Ok(status)
}

fn execute_and_chain_with<E>(
    chain: &flash_syntax::AndChain,
    execute: &mut E,
) -> Result<Status, RuntimeError>
where
    E: FnMut(&Pipeline) -> Result<Status, RuntimeError>,
{
    let mut pipelines = chain.and_terms().iter();
    let first = pipelines
        .next()
        .expect("a parsed and-chain contains an operand");
    let mut status = execute(first)?;
    for pipeline in pipelines {
        if !status.is_ok() {
            break;
        }
        status = execute(pipeline)?;
    }
    Ok(status)
}

#[allow(clippy::too_many_arguments)]
fn plan_and_execute(
    pipeline: &flash_syntax::Pipeline,
    cwd: &Path,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<Status, RuntimeError> {
    let plan = plan_pipeline_with_options(
        pipeline,
        cwd,
        source,
        scope,
        environment,
        registry,
        probe,
        options,
    )?;
    execute_foreground_status(&plan, platform, clock)
}

fn execute_preflighted_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
) -> Result<Vec<ProcessStatus>, RuntimeError> {
    execute_preflighted_pipeline_inner(plan, platform, None).map(|completions| {
        completions
            .into_iter()
            .map(|completion| completion.status)
            .collect()
    })
}

fn execute_preflighted_pipeline_timed(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<Vec<StageCompletion>, RuntimeError> {
    execute_preflighted_pipeline_inner(plan, platform, Some(clock))
}

fn execute_preflighted_pipeline_inner(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: Option<&dyn Clock>,
) -> Result<Vec<StageCompletion>, RuntimeError> {
    let started = start_preflighted_pipeline(plan, platform, clock, None, false)?;
    let foreground = take_foreground(plan, platform, started.group)?;
    let group = started.group;
    let waited = wait_in_source_order(started.children, plan, platform, group, clock);
    let released = release_foreground(foreground, plan);
    let completions = waited?;
    released?;
    Ok(completions)
}

/// The single process group every external member of one pipeline joins.
///
/// The first started member leads the group and later members join it, so the
/// whole pipeline can be signalled, stopped, and continued as one unit. A
/// platform without process groups leaves every member in the shell's own
/// group, which is the pre-job-control behavior.
struct PipelineGroup {
    placement: ProcessGroup,
}

impl PipelineGroup {
    /// Decide the placement of the first member against the live platform.
    fn new(platform: &dyn Platform, policy: ProcessGroupPolicy) -> Self {
        let placement = match policy {
            ProcessGroupPolicy::Isolate
                if platform.capabilities().supports(Capability::ProcessGroups) =>
            {
                ProcessGroup::New
            }
            ProcessGroupPolicy::Isolate | ProcessGroupPolicy::Inherit => ProcessGroup::Inherit,
        };
        Self { placement }
    }

    /// The placement the next member is spawned with.
    const fn placement(&self) -> ProcessGroup {
        self.placement
    }

    /// Adopt the leader's group so every later member joins it.
    ///
    /// An adapter that accepts [`ProcessGroup::New`] but reports no group leaves
    /// the placement unchanged, which makes each member its own leader instead
    /// of silently returning the pipeline to the shell's group.
    fn adopt(&mut self, child: &dyn ChildProcess) {
        if matches!(self.placement, ProcessGroup::New)
            && let Some(group) = child.process_group()
        {
            self.placement = ProcessGroup::Join(group);
        }
    }

    /// The established group, once a member has led one.
    const fn established(&self) -> Option<ProcessGroupId> {
        match self.placement {
            ProcessGroup::Join(group) => Some(group),
            ProcessGroup::Inherit | ProcessGroup::New => None,
        }
    }
}

/// Started external members of one pipeline and the group they share.
struct StartedPipeline {
    children: Vec<StartedChild>,
    group: Option<ProcessGroupId>,
    supervisor_completion: Option<Box<dyn DescriptorEndpoint>>,
}

/// One uniquely owned member of a started background pipeline.
#[derive(Debug)]
pub struct BackgroundMember {
    process: ProcessId,
    child: Box<dyn ChildProcess>,
    started_at: Instant,
}

impl BackgroundMember {
    /// The nonzero platform process identity.
    #[must_use]
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// The clock reading taken immediately before this member was spawned.
    #[must_use]
    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Consume the member into its process identity, child handle, and start
    /// reading.
    #[must_use]
    pub fn into_parts(self) -> (ProcessId, Box<dyn ChildProcess>, Instant) {
        (self.process, self.child, self.started_at)
    }
}

/// A completely started background external pipeline.
#[derive(Debug)]
pub struct BackgroundPipeline {
    members: Vec<BackgroundMember>,
    group: ProcessGroupId,
    started_at: Instant,
    supervisor_completion: Option<Box<dyn DescriptorEndpoint>>,
}

impl BackgroundPipeline {
    /// Members in source order.
    #[must_use]
    pub fn members(&self) -> &[BackgroundMember] {
        &self.members
    }

    /// The common process group established for every member.
    #[must_use]
    pub const fn group(&self) -> ProcessGroupId {
        self.group
    }

    /// The clock reading taken when pipeline startup began.
    #[must_use]
    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Consume the pipeline into source-ordered members.
    #[must_use]
    pub fn into_members(self) -> Vec<BackgroundMember> {
        self.members
    }

    /// Consume the pipeline into its members and optional supervisor reply reader.
    #[must_use]
    pub fn into_parts(self) -> (Vec<BackgroundMember>, Option<Box<dyn DescriptorEndpoint>>) {
        (self.members, self.supervisor_completion)
    }
}

/// Start an all-external pipeline for background observation without waiting.
///
/// Background startup requires process-group support and verifies that every
/// member joined the group led by the first child. Any startup failure
/// terminates and waits every child that was successfully spawned.
pub fn start_background_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
) -> Result<BackgroundPipeline, RuntimeError> {
    preflight(plan)?;
    if !platform.capabilities().supports(Capability::ProcessGroups) {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BackgroundProcessGroupUnavailable,
            plan.span(),
        ));
    }

    let pipeline_started = clock.now();
    let mut started = start_preflighted_pipeline(plan, platform, Some(clock), None, true)?;
    let Some(group) = started.group else {
        terminate_and_reap(&mut started.children);
        return Err(RuntimeError::new(
            RuntimeErrorKind::BackgroundProcessGroupUnavailable,
            plan.span(),
        ));
    };

    if started
        .children
        .iter()
        .any(|child| ProcessId::new(child.child.id()).is_none())
    {
        terminate_and_reap(&mut started.children);
        return Err(RuntimeError::new(
            RuntimeErrorKind::InvalidProcessIdentity,
            plan.span(),
        ));
    }

    let mut members = Vec::with_capacity(started.children.len());
    for child in started.children {
        let process = ProcessId::new(child.child.id())
            .expect("background process identities were validated before ownership transfer");
        let started_at = child
            .started_at
            .expect("background startup always records a member start reading");
        members.push(BackgroundMember {
            process,
            child: child.child,
            started_at,
        });
    }

    Ok(BackgroundPipeline {
        members,
        group,
        started_at: pipeline_started,
        supervisor_completion: started.supervisor_completion,
    })
}

/// Hand the terminal to `group` for as long as the returned guard lives.
///
/// A foreground job that owns the terminal is what makes a keyboard interrupt
/// reach the job instead of the shell. The handover is attempted only when the
/// platform can perform it and the shell actually has a terminal, so an absent
/// capability or a redirected session is not a failure — but a refused handover
/// on a real terminal is, because running the job anyway would silently send
/// the user's interrupts to the wrong process.
/// Give the terminal back to the shell, reporting a failed return.
///
/// The guard would also restore on drop, but silently: a shell that failed to
/// take its terminal back cannot read the next command, so the failure is
/// surfaced instead of discarded. A pipeline error stays primary, because the
/// failed job is the more useful diagnostic.
pub(crate) fn release_foreground(
    guard: Option<Box<dyn ForegroundTerminalGuard>>,
    plan: &ExecutionPlan,
) -> Result<(), RuntimeError> {
    let Some(mut guard) = guard else {
        return Ok(());
    };
    guard.restore().map_err(|error| {
        RuntimeError::new(RuntimeErrorKind::ForegroundTerminal(error), plan.span())
    })
}

pub(crate) fn take_foreground(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
) -> Result<Option<Box<dyn ForegroundTerminalGuard>>, RuntimeError> {
    let Some(group) = group else {
        return Ok(None);
    };
    if !platform
        .capabilities()
        .supports(Capability::ForegroundTerminal)
        || !platform.is_terminal()
    {
        return Ok(None);
    }

    platform.enter_foreground(group).map(Some).map_err(|error| {
        RuntimeError::new(RuntimeErrorKind::ForegroundTerminal(error), plan.span())
    })
}

/// Resume a stopped job where it was, so the wait can continue.
///
/// Until the job table can hold a stopped job and the built-ins can address it,
/// leaving one stopped would strand a process the session cannot reach. One
/// signal goes to the group rather than to each member, because the terminal
/// stops every member at once: resuming only the member currently being waited
/// on would release a producer whose consumer is still stopped, and the wait
/// would then block on a pipe nothing is draining.
///
/// A job with no group and a platform that cannot signal are both reported
/// rather than retried, because neither can be made to progress by observing
/// the job again.
fn resume_stopped_job(
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    signal: i32,
    automatic_resumes: &mut usize,
    span: flash_syntax::Span,
) -> Result<(), RuntimeError> {
    if *automatic_resumes >= AUTOMATIC_RESUME_LIMIT {
        return Err(RuntimeError::new(
            RuntimeErrorKind::RepeatedStop { signal },
            span,
        ));
    }
    let Some(group) = group else {
        return Err(RuntimeError::new(RuntimeErrorKind::UngroupedStop, span));
    };
    platform
        .signal_process_group(group, JobSignal::Continue)
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::JobSignal(error), span))?;
    *automatic_resumes += 1;
    Ok(())
}

fn start_preflighted_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: Option<&dyn Clock>,
    mut final_output: Option<Box<dyn DescriptorEndpoint>>,
    require_group: bool,
) -> Result<StartedPipeline, RuntimeError> {
    validate_preflighted_external_plan(plan)?;

    let (mut supervisor_reader, mut supervisor_writer) = if plan.supervisor_input().is_some() {
        let endpoints = platform
            .pipe()
            .map_err(|error| RuntimeError::new(RuntimeErrorKind::PipeCreate(error), plan.span()))?;
        let (reader, writer) = endpoints.into_parts();
        (Some(reader), Some(writer))
    } else {
        (None, None)
    };
    let (supervisor_completion_reader, mut supervisor_completion_writer) =
        if plan.expects_supervisor_completion() {
            let endpoints = platform.pipe().map_err(|error| {
                RuntimeError::new(RuntimeErrorKind::PipeCreate(error), plan.span())
            })?;
            let (reader, writer) = endpoints.into_parts();
            (Some(reader), Some(writer))
        } else {
            (None, None)
        };

    let mut pipes = Vec::with_capacity(plan.edges().len());
    for edge in plan.edges() {
        let endpoints = platform.pipe().map_err(|error| {
            RuntimeError::new(RuntimeErrorKind::PipeCreate(error), edge.operator_span())
        })?;
        let (reader, writer) = endpoints.into_parts();
        pipes.push((Some(reader), Some(writer)));
    }

    let environment: Vec<(OsString, OsString)> = plan
        .environment()
        .iter()
        .map(|(name, value)| (OsString::from(name), value.to_os_string()))
        .collect();
    let mut children: Vec<StartedChild> = Vec::with_capacity(plan.stages().len());
    let mut group = PipelineGroup::new(platform, plan.process_group_policy());

    for (index, stage) in plan.stages().iter().enumerate() {
        let input = index.checked_sub(1).and_then(|edge| pipes[edge].0.take());
        let edge_output = pipes.get_mut(index).and_then(|edge| edge.1.take());
        let merge_output =
            edge_output.is_some() && plan.edges()[index].kind() == PipeOperator::StdoutAndStderr;
        let output = edge_output.or_else(|| {
            (index + 1 == plan.stages().len())
                .then(|| final_output.take())
                .flatten()
        });
        let mut descriptor_map = StageDescriptorMap::new(input, output, merge_output);
        if index == 0
            && let Some(reader) = supervisor_reader.take()
        {
            descriptor_map.assign_owned(crate::capsule::CAPSULE_DESCRIPTOR, reader);
        }
        if index == 0
            && let Some(writer) = supervisor_completion_writer.take()
        {
            descriptor_map.assign_owned(crate::capsule::COMPLETION_DESCRIPTOR, writer);
        }
        if let Err(error) =
            descriptor_map.apply_redirections(stage.redirections(), plan.cwd(), platform)
        {
            drop(descriptor_map);
            drop(pipes);
            terminate_and_reap(&mut children);
            return Err(error);
        }
        let descriptors = descriptor_map.child_descriptors();
        let closed_descriptors = descriptor_map.closed_descriptors();

        let PlannedResolution::External { path } = stage.resolution() else {
            unreachable!("external stages were validated before pipe creation");
        };
        let argv: Vec<OsString> = stage
            .argv()
            .iter()
            .map(|argument| argument.value().to_os_string())
            .collect();
        let placement = group.placement();
        let request = SpawnRequest::new(path, &argv, &environment, plan.cwd())
            .expect("a planned command always carries argv zero")
            .with_descriptors(&descriptors)
            .expect("the final descriptor map has unique targets")
            .with_closed_descriptors(&closed_descriptors)
            .expect("a final descriptor cannot be both mapped and closed")
            .in_process_group(placement);
        let command_span = stage.argv()[0].span();
        let started_at = clock.map(Clock::now);
        let child = platform.spawn(&request).map_err(|error| {
            RuntimeError::new(RuntimeErrorKind::ProcessSpawn(error), command_span)
        });

        drop(descriptors);
        drop(closed_descriptors);
        drop(descriptor_map);

        match child {
            Ok(child) => {
                let reported_group = child.process_group();
                let group_is_valid = match placement {
                    ProcessGroup::New => reported_group.is_some(),
                    ProcessGroup::Join(expected) => reported_group == Some(expected),
                    ProcessGroup::Inherit => !require_group,
                };
                if require_group && !group_is_valid {
                    children.push(StartedChild { child, started_at });
                    drop(pipes);
                    terminate_and_reap(&mut children);
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::BackgroundProcessGroupUnavailable,
                        command_span,
                    ));
                }
                group.adopt(child.as_ref());
                children.push(StartedChild { child, started_at });
            }
            Err(error) => {
                drop(pipes);
                terminate_and_reap(&mut children);
                return Err(error);
            }
        }
    }

    drop(pipes);
    if let (Some(bytes), Some(mut writer)) = (plan.supervisor_input(), supervisor_writer.take()) {
        let mut written = 0;
        while written < bytes.len() {
            match writer.write(&bytes[written..]) {
                Ok(0) => {
                    terminate_and_reap(&mut children);
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::PipelineWrite(
                            flash_platform::DescriptorWriteError::Operation {
                                kind: std::io::ErrorKind::WriteZero,
                                message: "execution capsule pipe accepted zero bytes".to_owned(),
                            },
                        ),
                        plan.span(),
                    ));
                }
                Ok(count) => written += count,
                Err(error) => {
                    terminate_and_reap(&mut children);
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::PipelineWrite(error),
                        plan.span(),
                    ));
                }
            }
        }
    }
    Ok(StartedPipeline {
        children,
        group: group.established(),
        supervisor_completion: supervisor_completion_reader,
    })
}

/// Parent-owned resources for one maximal internal segment in a mixed pipeline.
pub(crate) struct MixedSegment {
    segment: InternalSegment,
    input: Option<Box<dyn DescriptorEndpoint>>,
    output: Option<Box<dyn DescriptorEndpoint>>,
}

impl MixedSegment {
    pub(crate) const fn segment(&self) -> &InternalSegment {
        &self.segment
    }

    /// Take the reader from an external predecessor, when one exists.
    pub(crate) fn take_input(&mut self) -> Option<Box<dyn DescriptorEndpoint>> {
        self.input.take()
    }

    /// Take the resolved byte output endpoint, when the segment does not use
    /// the final inherited session sink.
    pub(crate) fn take_output(&mut self) -> Option<Box<dyn DescriptorEndpoint>> {
        self.output.take()
    }
}

/// Running external stages and parent-owned internal-segment endpoints.
pub(crate) struct MixedPipeline {
    control: MixedPipelineControl,
    /// Terminal ownership held for the external members. The shell process runs
    /// the internal island in its own group while the job owns the terminal; the
    /// island reads pipes rather than the keyboard, so it needs no ownership of
    /// its own, and it must not take the terminal back before the external
    /// members have finished with it.
    foreground: Option<Box<dyn ForegroundTerminalGuard>>,
    /// The group the external members share, when the platform established one.
    /// Retained so a member that reports a stop can be resumed as one job.
    group: Option<ProcessGroupId>,
    segments: Vec<MixedSegment>,
    captured_output: Option<Box<dyn DescriptorEndpoint>>,
    started_at: Instant,
}

impl MixedPipeline {
    /// Clone the one controller shared by every internal segment worker.
    pub(crate) fn control(&self) -> MixedPipelineControl {
        self.control.clone()
    }

    /// Take every source-ordered internal segment resource.
    pub(crate) fn take_segments(&mut self) -> Vec<MixedSegment> {
        std::mem::take(&mut self.segments)
    }

    /// Take the reader installed for a captured final external stage.
    pub(crate) fn take_captured_output(&mut self) -> Option<Box<dyn DescriptorEndpoint>> {
        self.captured_output.take()
    }

    /// Wait every external stage and return source-indexed language statuses.
    pub(crate) fn wait(
        self,
        plan: &ExecutionPlan,
        platform: &dyn Platform,
        clock: &dyn Clock,
    ) -> Result<(Vec<(usize, Status)>, Duration), RuntimeError> {
        let mut children = self.control.take_children();
        let mut statuses = Vec::with_capacity(children.len());
        let mut first_error = None;
        let foreground = self.foreground;
        let group = self.group;
        for child_index in 0..children.len() {
            let started = &mut children[child_index];
            let stage = &plan.stages()[started.index];
            let mut automatic_resumes = 0;
            let waited = loop {
                match started.child.child.wait_for_transition() {
                    Ok(ProcessTransition::Completed(status)) => break Ok(status),
                    Ok(ProcessTransition::Continued) => {}
                    Ok(ProcessTransition::Stopped { signal }) => {
                        if let Err(error) = resume_stopped_job(
                            platform,
                            group,
                            signal,
                            &mut automatic_resumes,
                            stage.span(),
                        ) {
                            break Err(error);
                        }
                    }
                    Err(error) => {
                        break Err(RuntimeError::new(
                            RuntimeErrorKind::ProcessWait(error),
                            stage.span(),
                        ));
                    }
                }
            };
            match waited {
                Ok(status) => {
                    let duration = started
                        .child
                        .started_at
                        .map_or(Duration::ZERO, |start| elapsed(start, clock.now()));
                    statuses.push((started.index, language_status(status, duration)));
                }
                Err(error) if first_error.is_none() => {
                    first_error = Some(error);
                    terminate_indexed_and_reap(&mut children[child_index..]);
                    break;
                }
                Err(_) => unreachable!("the first mixed wait failure stops ordinary waiting"),
            }
        }
        // The terminal returns only after the last external member has been
        // waited, so a job that outlives the internal segments still owns it.
        let released = release_foreground(foreground, plan);
        match first_error {
            Some(error) => Err(error),
            None => {
                released?;
                Ok((statuses, elapsed(self.started_at, clock.now())))
            }
        }
    }

    /// Stop and reap every spawned external stage after an unsuccessful path.
    pub(crate) fn terminate(self) {
        self.control.cancel_and_reap();
    }
}

/// One idempotent cancellation owner shared by the mixed coordinator and all
/// scoped segment workers.
#[derive(Clone)]
pub(crate) struct MixedPipelineControl {
    cancelled: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<IndexedStartedChild>>>,
}

impl MixedPipelineControl {
    fn new(children: Vec<IndexedStartedChild>) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            children: Arc::new(Mutex::new(children)),
        }
    }

    /// Whether a failure or explicit exit has begun peer cancellation.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Trip cancellation once, terminate every live child, and perform the
    /// final waits before the originating failure or exit path returns.
    pub(crate) fn cancel_and_reap(&self) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut children = self
            .children
            .lock()
            .expect("mixed child controller must not be poisoned");
        terminate_indexed_and_reap(&mut children);
        children.clear();
    }

    fn take_children(&self) -> Vec<IndexedStartedChild> {
        let mut children = self
            .children
            .lock()
            .expect("mixed child controller must not be poisoned");
        std::mem::take(&mut *children)
    }
}

/// Start all external stages around every maximal internal segment.
///
/// External-to-external edges retain ordinary kernel pipes. Exactly the two
/// edges touching each segment keep one endpoint in the parent for lazy pulls
/// or checked partial writes.
pub(crate) fn start_mixed_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
    capture_final_stdout: bool,
) -> Result<MixedPipeline, RuntimeError> {
    preflight(plan)?;
    let topology = plan.mixed_topology().ok_or_else(|| {
        RuntimeError::new(
            // flash-v1-boundary(executor-invariant): Only a classified mixed topology enters the mixed executor.
            RuntimeErrorKind::Unsupported {
                feature: "a non-mixed plan in the mixed pipeline executor",
            },
            plan.span(),
        )
    })?;

    let mut pipes = Vec::with_capacity(plan.edges().len());
    for (index, edge) in plan.edges().iter().enumerate() {
        let both_internal = matches!(
            plan.stages()[index].resolution(),
            PlannedResolution::Internal { .. }
        ) && matches!(
            plan.stages()[index + 1].resolution(),
            PlannedResolution::Internal { .. }
        );
        if both_internal {
            pipes.push((None, None));
            continue;
        }
        let endpoints = platform.pipe().map_err(|error| {
            RuntimeError::new(RuntimeErrorKind::PipeCreate(error), edge.operator_span())
        })?;
        let (reader, writer) = endpoints.into_parts();
        pipes.push((Some(reader), Some(writer)));
    }

    let final_stage = plan
        .stages()
        .last()
        .expect("a mixed pipeline has a final stage");
    let (mut captured_output, mut capture_writer) = if capture_final_stdout
        && matches!(final_stage.resolution(), PlannedResolution::External { .. })
    {
        let (reader, writer) = platform
            .pipe()
            .map_err(|error| {
                RuntimeError::new(RuntimeErrorKind::CapturePipe(error), final_stage.span())
            })?
            .into_parts();
        (Some(reader), Some(writer))
    } else {
        (None, None)
    };

    let mut segments = Vec::with_capacity(topology.internal_segments().len());
    for segment in topology.internal_segments().iter().cloned() {
        let stages = segment.stages();
        let input = stages
            .start
            .checked_sub(1)
            .and_then(|edge| pipes[edge].0.take());
        let last_stage = stages
            .end
            .checked_sub(1)
            .expect("an internal segment is nonempty");
        let pipeline_writer = (stages.end < plan.stages().len())
            .then(|| pipes[last_stage].1.take())
            .flatten();
        let merge_pipeline_output = pipeline_writer.is_some()
            && plan.edges()[last_stage].kind() == PipeOperator::StdoutAndStderr;
        let output = match internal_stdout_route(&plan.stages()[last_stage], merge_pipeline_output)
        {
            InternalStdoutRoute::Default => pipeline_writer,
            InternalStdoutRoute::File { target, mode } => {
                drop(pipeline_writer);
                let mode = match mode {
                    OutputMode::Truncate => FileOpenMode::WriteTruncate,
                    OutputMode::Append => FileOpenMode::WriteAppend,
                };
                let endpoint = platform
                    .open_file(FileOpenRequest::new(
                        Path::new(target.value()),
                        plan.cwd(),
                        mode,
                    ))
                    .map_err(|error| {
                        RuntimeError::new(RuntimeErrorKind::RedirectionSetup(error), target.span())
                    })?;
                Some(endpoint)
            }
            InternalStdoutRoute::Unsupported => {
                unreachable!("mixed preflight rejects unsupported internal stdout routes")
            }
        };
        segments.push(MixedSegment {
            segment,
            input,
            output,
        });
    }
    let environment: Vec<(OsString, OsString)> = plan
        .environment()
        .iter()
        .map(|(name, value)| (OsString::from(name), value.to_os_string()))
        .collect();
    let mut children = Vec::with_capacity(topology.external_indices().len());
    let started_at = clock.now();
    let mut group = PipelineGroup::new(platform, plan.process_group_policy());

    for (index, stage) in plan.stages().iter().enumerate() {
        if matches!(stage.resolution(), PlannedResolution::Internal { .. }) {
            continue;
        }
        let input_endpoint = index.checked_sub(1).and_then(|edge| pipes[edge].0.take());
        let pipeline_output = pipes.get_mut(index).and_then(|edge| edge.1.take());
        let merge_output = pipeline_output.is_some()
            && plan.edges()[index].kind() == PipeOperator::StdoutAndStderr;
        let edge_output = pipeline_output.or_else(|| {
            (index + 1 == plan.stages().len())
                .then(|| capture_writer.take())
                .flatten()
        });
        let mut descriptor_map = StageDescriptorMap::new(input_endpoint, edge_output, merge_output);
        if let Err(error) =
            descriptor_map.apply_redirections(stage.redirections(), plan.cwd(), platform)
        {
            drop(descriptor_map);
            drop(pipes);
            terminate_indexed_and_reap(&mut children);
            return Err(error);
        }
        let descriptors = descriptor_map.child_descriptors();
        let closed_descriptors = descriptor_map.closed_descriptors();
        let PlannedResolution::External { path } = stage.resolution() else {
            unreachable!("the internal island was validated before pipe creation");
        };
        let argv: Vec<OsString> = stage
            .argv()
            .iter()
            .map(|argument| argument.value().to_os_string())
            .collect();
        let request = SpawnRequest::new(path, &argv, &environment, plan.cwd())
            .expect("a planned command always carries argv zero")
            .with_descriptors(&descriptors)
            .expect("the final descriptor map has unique targets")
            .with_closed_descriptors(&closed_descriptors)
            .expect("a final descriptor cannot be both mapped and closed")
            .in_process_group(group.placement());
        let child = platform
            .spawn(&request)
            .map(|child| IndexedStartedChild {
                index,
                child: StartedChild {
                    child,
                    started_at: Some(clock.now()),
                },
            })
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorKind::ProcessSpawn(error),
                    stage.argv()[0].span(),
                )
            });

        drop(descriptors);
        drop(closed_descriptors);
        drop(descriptor_map);

        match child {
            Ok(child) => {
                group.adopt(child.child.child.as_ref());
                children.push(child);
            }
            Err(error) => {
                drop(pipes);
                terminate_indexed_and_reap(&mut children);
                return Err(error);
            }
        }
    }

    drop(pipes);
    let established = group.established();
    let foreground = match take_foreground(plan, platform, established) {
        Ok(foreground) => foreground,
        Err(error) => {
            terminate_indexed_and_reap(&mut children);
            return Err(error);
        }
    };
    Ok(MixedPipeline {
        control: MixedPipelineControl::new(children),
        foreground,
        group: established,
        segments,
        captured_output: captured_output.take(),
        started_at,
    })
}

fn validate_preflighted_external_plan(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    if plan.stages().is_empty() {
        return Err(RuntimeError::new(
            // flash-v1-boundary(executor-invariant): Parsed pipelines always contain at least one stage.
            RuntimeErrorKind::Unsupported {
                feature: "an empty foreground pipeline",
            },
            plan.span(),
        ));
    }
    for stage in plan.stages() {
        validate_external_stage(stage)?;
    }
    Ok(())
}

fn validate_external_stage(stage: &crate::plan::PlannedStage) -> Result<(), RuntimeError> {
    if !matches!(stage.resolution(), PlannedResolution::External { .. }) {
        return Err(RuntimeError::new(
            // flash-v1-boundary(executor-invariant): Internal stages use the structured or mixed executor.
            RuntimeErrorKind::Unsupported {
                feature: "foreground internal-command execution",
            },
            stage.span(),
        ));
    }
    Ok(())
}

fn terminate_and_reap(children: &mut [StartedChild]) {
    for child in &mut *children {
        let _ = child.child.terminate();
    }
    for child in children {
        let _ = child.child.wait();
    }
}

struct StartedChild {
    child: Box<dyn ChildProcess>,
    started_at: Option<Instant>,
}

struct IndexedStartedChild {
    index: usize,
    child: StartedChild,
}

fn terminate_indexed_and_reap(children: &mut [IndexedStartedChild]) {
    for child in &mut *children {
        let _ = child.child.child.terminate();
    }
    for child in children {
        let _ = child.child.child.wait();
    }
}

struct StageCompletion {
    status: ProcessStatus,
    duration: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescriptorBinding {
    Inherited(u32),
    Owned(usize),
}

#[derive(Debug)]
struct StageDescriptorMap {
    bindings: BTreeMap<u32, DescriptorBinding>,
    resources: Vec<Option<Box<dyn DescriptorEndpoint>>>,
    touched: BTreeSet<u32>,
}

impl StageDescriptorMap {
    fn new(
        input: Option<Box<dyn DescriptorEndpoint>>,
        output: Option<Box<dyn DescriptorEndpoint>>,
        merge_output: bool,
    ) -> Self {
        let mut this = Self {
            bindings: BTreeMap::from([
                (0, DescriptorBinding::Inherited(0)),
                (1, DescriptorBinding::Inherited(1)),
                (2, DescriptorBinding::Inherited(2)),
            ]),
            resources: Vec::new(),
            touched: BTreeSet::new(),
        };
        if let Some(input) = input {
            this.assign_owned(0, input);
        }
        if let Some(output) = output {
            let resource = this.push_resource(output);
            this.assign(1, DescriptorBinding::Owned(resource));
            if merge_output {
                this.assign(2, DescriptorBinding::Owned(resource));
            }
        }
        this
    }

    fn apply_redirections(
        &mut self,
        redirections: &[PlannedRedirection],
        cwd: &Path,
        platform: &dyn Platform,
    ) -> Result<(), RuntimeError> {
        for redirection in redirections {
            match redirection.action() {
                RedirectionAction::Input {
                    descriptor, target, ..
                } => {
                    let endpoint = platform
                        .open_file(FileOpenRequest::new(
                            Path::new(target.value()),
                            cwd,
                            FileOpenMode::Read,
                        ))
                        .map_err(|error| {
                            RuntimeError::new(
                                RuntimeErrorKind::RedirectionSetup(error),
                                target.span(),
                            )
                        })?;
                    self.assign_owned(*descriptor, endpoint);
                }
                RedirectionAction::Output {
                    descriptor,
                    mode,
                    target,
                    ..
                } => {
                    let mode = match mode {
                        OutputMode::Truncate => FileOpenMode::WriteTruncate,
                        OutputMode::Append => FileOpenMode::WriteAppend,
                    };
                    let endpoint = platform
                        .open_file(FileOpenRequest::new(Path::new(target.value()), cwd, mode))
                        .map_err(|error| {
                            RuntimeError::new(
                                RuntimeErrorKind::RedirectionSetup(error),
                                target.span(),
                            )
                        })?;
                    self.assign_owned(*descriptor, endpoint);
                }
                RedirectionAction::Duplicate {
                    descriptor,
                    source,
                    target_span,
                    ..
                } => {
                    let binding = *self
                        .bindings
                        .get(source)
                        .expect("preflight established that the source descriptor is open");
                    let binding = match binding {
                        DescriptorBinding::Inherited(source) => {
                            let endpoint =
                                platform.inherit_descriptor(source).map_err(|error| {
                                    RuntimeError::new(
                                        RuntimeErrorKind::RedirectionSetup(error),
                                        *target_span,
                                    )
                                })?;
                            DescriptorBinding::Owned(self.push_resource(endpoint))
                        }
                        owned => owned,
                    };
                    self.assign(*descriptor, binding);
                }
                RedirectionAction::Close { descriptor, .. } => self.close(*descriptor),
            }
        }
        Ok(())
    }

    fn child_descriptors(&self) -> Vec<ChildDescriptor<'_>> {
        self.touched
            .iter()
            .filter_map(|target| match self.bindings.get(target) {
                Some(DescriptorBinding::Owned(resource)) => Some(ChildDescriptor::new(
                    *target,
                    self.resources[*resource]
                        .as_deref()
                        .expect("a mapped resource remains owned"),
                )),
                Some(DescriptorBinding::Inherited(source)) => {
                    debug_assert_eq!(target, source);
                    None
                }
                None => None,
            })
            .collect()
    }

    fn closed_descriptors(&self) -> Vec<u32> {
        self.touched
            .iter()
            .filter(|descriptor| !self.bindings.contains_key(descriptor))
            .copied()
            .collect()
    }

    fn assign_owned(&mut self, descriptor: u32, endpoint: Box<dyn DescriptorEndpoint>) {
        let resource = self.push_resource(endpoint);
        self.assign(descriptor, DescriptorBinding::Owned(resource));
    }

    fn push_resource(&mut self, endpoint: Box<dyn DescriptorEndpoint>) -> usize {
        let resource = self.resources.len();
        self.resources.push(Some(endpoint));
        resource
    }

    fn assign(&mut self, descriptor: u32, binding: DescriptorBinding) {
        let replaced = self.bindings.insert(descriptor, binding);
        self.touched.insert(descriptor);
        if let Some(DescriptorBinding::Owned(resource)) = replaced {
            self.release_if_unused(resource);
        }
    }

    fn close(&mut self, descriptor: u32) {
        let removed = self.bindings.remove(&descriptor);
        self.touched.insert(descriptor);
        if let Some(DescriptorBinding::Owned(resource)) = removed {
            self.release_if_unused(resource);
        }
    }

    fn release_if_unused(&mut self, resource: usize) {
        let still_used = self
            .bindings
            .values()
            .any(|binding| *binding == DescriptorBinding::Owned(resource));
        if !still_used {
            drop(self.resources[resource].take());
        }
    }
}

fn wait_in_source_order(
    children: Vec<StartedChild>,
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    clock: Option<&dyn Clock>,
) -> Result<Vec<StageCompletion>, RuntimeError> {
    let mut statuses = Vec::with_capacity(children.len());
    let mut first_error = None;
    for (mut child, stage) in children.into_iter().zip(plan.stages()) {
        let mut automatic_resumes = 0;
        let waited = loop {
            match child.child.wait_for_transition() {
                Ok(ProcessTransition::Completed(status)) => break Ok(status),
                Ok(ProcessTransition::Continued) => {}
                Ok(ProcessTransition::Stopped { signal }) => {
                    if let Err(error) = resume_stopped_job(
                        platform,
                        group,
                        signal,
                        &mut automatic_resumes,
                        stage.span(),
                    ) {
                        // A stop nothing can lift would otherwise hold the
                        // stage's descriptors for the life of the host,
                        // including a capture pipe a reader is still draining
                        // to end of file. Termination reaches a stopped
                        // process, so ending it here releases them.
                        let _ = child.child.terminate();
                        let _ = child.child.wait();
                        break Err(error);
                    }
                }
                Err(error) => {
                    break Err(RuntimeError::new(
                        RuntimeErrorKind::ProcessWait(error),
                        stage.span(),
                    ));
                }
            }
        };
        match waited {
            Ok(status) => {
                let duration = match (child.started_at, clock) {
                    (Some(started_at), Some(clock)) => elapsed(started_at, clock.now()),
                    _ => Duration::ZERO,
                };
                statuses.push(StageCompletion { status, duration });
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(statuses),
    }
}

fn drain_stdout<D>(
    platform: &dyn Platform,
    reader: Box<dyn DescriptorEndpoint>,
    drain: &mut D,
    producer_span: flash_syntax::Span,
    ready: mpsc::SyncSender<()>,
) -> Result<(), RuntimeError>
where
    D: FnMut(&[u8]),
{
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut buffer = [0u8; CHUNK_SIZE];
    let first = read_capture_chunk(platform, reader.as_ref(), &mut buffer, producer_span);
    ready
        .send(())
        .expect("the waiting executor retains the drain-ready receiver");
    let mut amount = first?;
    loop {
        if amount == 0 {
            return Ok(());
        }
        drain(&buffer[..amount]);
        amount = read_capture_chunk(platform, reader.as_ref(), &mut buffer, producer_span)?;
    }
}

fn read_capture_chunk(
    platform: &dyn Platform,
    reader: &dyn DescriptorEndpoint,
    buffer: &mut [u8],
    producer_span: flash_syntax::Span,
) -> Result<usize, RuntimeError> {
    platform
        .read_descriptor(reader, buffer)
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::CaptureRead(error), producer_span))
}

pub(crate) fn elapsed(start: Instant, end: Instant) -> Duration {
    Duration::from_nanos(i128::from(end.as_nanos().saturating_sub(start.as_nanos())))
}

pub(crate) fn language_status(status: ProcessStatus, duration: Duration) -> Status {
    match status {
        ProcessStatus::Exited(code) => Status::exit(i64::from(code), duration),
        ProcessStatus::Signaled(number) => Status::signaled(
            Signal::new(Some(i64::from(number)), None)
                .expect("a platform signal status always carries its number"),
            duration,
        ),
    }
    .expect("monotonic execution durations are valid")
}

/// Aggregate source-ordered leaf statuses with the shared pipeline-selection
/// rule.
pub(crate) fn aggregate_statuses(
    stages: Vec<Status>,
    pipefail: bool,
    pipeline_duration: Duration,
) -> Status {
    if let [stage] = stages.as_slice() {
        return stage.clone();
    }
    let selected = if pipefail {
        stages
            .iter()
            .rposition(|stage| !stage.is_ok())
            .unwrap_or(stages.len() - 1)
    } else {
        stages.len() - 1
    };
    Status::aggregate(stages, selected, pipeline_duration)
        .expect("executor completion satisfies aggregate status invariants")
}
