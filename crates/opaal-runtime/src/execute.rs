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

use opaal_platform::{
    Capability, ChildDescriptor, ChildProcess, DescriptorEndpoint, FileOpenMode, FileOpenRequest,
    ForegroundSignalGuard, ForegroundTerminalGuard, JobSignal, OwnedProcessGroup, Platform,
    ProcessGroup, ProcessGroupId, ProcessStatus, ProcessTransition, SignalError, SpawnRequest,
};
use opaal_syntax::{ConditionalChain, OutputMode, PipeOperator, Pipeline, SourceFile};

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
            // opaal-foundation-boundary(embedding-refusal): This narrow API accepts one stage; the pipeline API owns multiple stages.
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
    let mut signals = prepare_foreground_signals(plan, platform)?;
    let started = start_preflighted_pipeline(
        plan,
        platform,
        Some(clock),
        Some(writer),
        false,
        signals.as_mut(),
    );
    let mut started = match started {
        Ok(started) => started,
        Err(error) => {
            let restored = restore_foreground_signals(signals.take(), plan).err();
            return Err(attach_secondary(error, restored));
        }
    };
    // Held across the drain and the wait; `Drop` is the backstop that returns
    // the terminal even when the drain callback panics.
    let foreground = match take_foreground(plan, platform, started.group) {
        Ok(foreground) => foreground,
        Err(error) => {
            let cleanup = terminate_and_reap(
                platform,
                started.group,
                &mut started.children,
                &mut started.group_owner,
            );
            let error = attach_cleanup(error, cleanup, plan.span());
            let restored = restore_foreground_signals(signals.take(), plan).err();
            return Err(attach_secondary(error, restored));
        }
    };
    let group = started.group;
    let children = std::mem::take(&mut started.children);

    let (wait_result, drain_result) = thread::scope(|scope| {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let drain_task =
            scope.spawn(move || drain_stdout(platform, reader, drain, producer_span, ready_sender));
        ready_receiver
            .recv()
            .expect("the drain task signals before returning");
        let wait_result = wait_in_source_order(
            children,
            plan,
            platform,
            group,
            &mut started.group_owner,
            Some(clock),
        );
        let drain_result = drain_task
            .join()
            .expect("a drain callback panic is an implementation failure");
        (wait_result, drain_result)
    });

    let released = release_foreground(foreground, plan);
    let group_closed = release_owned_group(&mut started.group_owner, plan);
    let signals_restored = restore_foreground_signals(signals, plan);
    let completions = match wait_result {
        Ok(completions) => completions,
        Err(error) => {
            return Err(attach_secondaries(
                error,
                [
                    drain_result.err(),
                    released.err(),
                    group_closed.err(),
                    signals_restored.err(),
                ],
            ));
        }
    };
    if let Err(error) = drain_result {
        return Err(attach_secondaries(
            error,
            [released.err(), group_closed.err(), signals_restored.err()],
        ));
    }
    if let Err(error) = released {
        return Err(attach_secondaries(
            error,
            [group_closed.err(), signals_restored.err()],
        ));
    }
    if let Err(error) = group_closed {
        return Err(attach_secondary(error, signals_restored.err()));
    }
    signals_restored?;
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

pub(crate) fn decode_text_capture(
    captured: CommandCapture<Vec<u8>>,
    span: opaal_syntax::Span,
) -> Result<CommandCapture<String>, RuntimeError> {
    let (bytes, status) = captured.into_parts();
    let output = decode_text_bytes(bytes, span)?;
    Ok(CommandCapture { output, status })
}

pub(crate) fn decode_text_bytes(
    bytes: Vec<u8>,
    span: opaal_syntax::Span,
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
        span: opaal_syntax::Span,
    ) -> Result<CommandCapture<Vec<u8>>, RuntimeError> {
        self.ensure_within_limit(span)?;
        Ok(CommandCapture {
            output: self.output,
            status,
        })
    }

    pub(crate) fn ensure_within_limit(&self, span: opaal_syntax::Span) -> Result<(), RuntimeError> {
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
    chain: &opaal_syntax::AndChain,
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
    pipeline: &opaal_syntax::Pipeline,
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
    let mut signals = prepare_foreground_signals(plan, platform)?;
    let started = start_preflighted_pipeline(plan, platform, clock, None, false, signals.as_mut());
    let mut started = match started {
        Ok(started) => started,
        Err(error) => {
            let restored = restore_foreground_signals(signals.take(), plan).err();
            return Err(attach_secondary(error, restored));
        }
    };
    let foreground = match take_foreground(plan, platform, started.group) {
        Ok(foreground) => foreground,
        Err(error) => {
            let cleanup = terminate_and_reap(
                platform,
                started.group,
                &mut started.children,
                &mut started.group_owner,
            );
            let error = attach_cleanup(error, cleanup, plan.span());
            let restored = restore_foreground_signals(signals.take(), plan).err();
            return Err(attach_secondary(error, restored));
        }
    };
    let group = started.group;
    let waited = wait_in_source_order(
        std::mem::take(&mut started.children),
        plan,
        platform,
        group,
        &mut started.group_owner,
        clock,
    );
    let released = release_foreground(foreground, plan);
    let group_closed = release_owned_group(&mut started.group_owner, plan);
    let signals_restored = restore_foreground_signals(signals, plan);
    let completions = match waited {
        Ok(completions) => completions,
        Err(error) => {
            return Err(attach_secondaries(
                error,
                [released.err(), group_closed.err(), signals_restored.err()],
            ));
        }
    };
    if let Err(error) = released {
        return Err(attach_secondaries(
            error,
            [group_closed.err(), signals_restored.err()],
        ));
    }
    group_closed?;
    signals_restored?;
    Ok(completions)
}

fn prepare_foreground_signals(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
) -> Result<Option<Box<dyn ForegroundSignalGuard>>, RuntimeError> {
    let capabilities = platform.capabilities();
    if !capabilities.supports(Capability::ProcessGroups)
        || !capabilities.supports(Capability::Signals)
    {
        return Ok(None);
    }
    platform
        .prepare_foreground_signals()
        .map(Some)
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::ForegroundSignal(error), plan.span()))
}

fn restore_foreground_signals(
    signals: Option<Box<dyn ForegroundSignalGuard>>,
    plan: &ExecutionPlan,
) -> Result<(), RuntimeError> {
    let Some(mut signals) = signals else {
        return Ok(());
    };
    signals
        .restore()
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::ForegroundSignal(error), plan.span()))
}

/// The single process group every external member of one pipeline joins.
///
/// The first started member leads the group and later members join it, so the
/// whole pipeline can be signalled, stopped, and continued as one unit. A
/// platform without process groups leaves every member in the shell's own
/// group, which is the pre-job-control behavior.
struct PipelineGroup {
    placement: ProcessGroup,
    owner: Option<Box<dyn OwnedProcessGroup>>,
}

impl PipelineGroup {
    /// Decide the placement of the first member against the live platform.
    ///
    /// Foreground groups use a private ownership anchor, while the existing
    /// background route retains its leader-created group until that separately
    /// bounded lifecycle is redesigned.
    fn new(
        platform: &dyn Platform,
        policy: ProcessGroupPolicy,
        stable_owner: bool,
        span: opaal_syntax::Span,
    ) -> Result<Self, RuntimeError> {
        if !matches!(policy, ProcessGroupPolicy::Isolate)
            || !platform.capabilities().supports(Capability::ProcessGroups)
        {
            return Ok(Self {
                placement: ProcessGroup::Inherit,
                owner: None,
            });
        }
        if !stable_owner {
            return Ok(Self {
                placement: ProcessGroup::New,
                owner: None,
            });
        }
        let owner = platform.create_process_group().map_err(|error| {
            RuntimeError::new(RuntimeErrorKind::ForegroundProcessGroup(error), span)
        })?;
        let placement = ProcessGroup::Join(owner.id());
        Ok(Self {
            placement,
            owner: Some(owner),
        })
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

    fn take_owner(&mut self) -> Option<Box<dyn OwnedProcessGroup>> {
        self.owner.take()
    }
}

/// Started external members of one pipeline and the group they share.
struct StartedPipeline {
    children: Vec<StartedChild>,
    group: Option<ProcessGroupId>,
    group_owner: Option<Box<dyn OwnedProcessGroup>>,
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
    let mut started = start_preflighted_pipeline(plan, platform, Some(clock), None, true, None)?;
    let Some(group) = started.group else {
        let error = RuntimeError::new(
            RuntimeErrorKind::BackgroundProcessGroupUnavailable,
            plan.span(),
        );
        let cleanup = terminate_and_reap(
            platform,
            None,
            &mut started.children,
            &mut started.group_owner,
        );
        return Err(attach_cleanup(error, cleanup, plan.span()));
    };

    if started
        .children
        .iter()
        .any(|child| ProcessId::new(child.child.id()).is_none())
    {
        let error = RuntimeError::new(RuntimeErrorKind::InvalidProcessIdentity, plan.span());
        let cleanup = terminate_and_reap(
            platform,
            Some(group),
            &mut started.children,
            &mut started.group_owner,
        );
        return Err(attach_cleanup(error, cleanup, plan.span()));
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
    span: opaal_syntax::Span,
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
    mut foreground_signals: Option<&mut Box<dyn ForegroundSignalGuard>>,
) -> Result<StartedPipeline, RuntimeError> {
    validate_preflighted_external_plan(plan)?;

    // Establish stable foreground ownership before creating pipeline
    // descriptors, so the private anchor cannot inherit or retain user I/O.
    let mut group = PipelineGroup::new(
        platform,
        plan.process_group_policy(),
        !require_group,
        plan.span(),
    )?;
    // Keep the caller's supported signals blocked until a verified user child
    // has joined the anchor. Forwarding before that would deliver an interrupt
    // only to the private anchor, which blocks it, then start the user program.
    let mut foreground_signals_active = false;

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
            let cleanup = terminate_and_reap(
                platform,
                group.established(),
                &mut children,
                &mut group.owner,
            );
            return Err(attach_cleanup(error, cleanup, plan.span()));
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
                let group_is_valid = process_group_report_is_exact(placement, child.as_ref());
                if placement.requires_capability() && !group_is_valid {
                    children.push(StartedChild { child, started_at });
                    drop(pipes);
                    let error = RuntimeError::new(
                        if require_group {
                            RuntimeErrorKind::BackgroundProcessGroupUnavailable
                        } else {
                            RuntimeErrorKind::ForegroundProcessGroupUnavailable
                        },
                        command_span,
                    );
                    // A mismatched report is not evidence of ownership. Only a
                    // previously validated group may receive a group signal;
                    // the newly returned direct handle is terminated itself.
                    let cleanup = terminate_and_reap(
                        platform,
                        group.established(),
                        &mut children,
                        &mut group.owner,
                    );
                    return Err(attach_cleanup(error, cleanup, plan.span()));
                }
                group.adopt(child.as_ref());
                children.push(StartedChild { child, started_at });
                if !foreground_signals_active
                    && let Some(established) = group.established()
                    && let Some(signals) = foreground_signals.as_deref_mut()
                    && let Err(error) = signals.forward_to(established)
                {
                    drop(pipes);
                    let primary =
                        RuntimeError::new(RuntimeErrorKind::ForegroundSignal(error), command_span);
                    let cleanup = terminate_and_reap(
                        platform,
                        Some(established),
                        &mut children,
                        &mut group.owner,
                    );
                    return Err(attach_cleanup(primary, cleanup, plan.span()));
                }
                if foreground_signals.is_some() && group.established().is_some() {
                    foreground_signals_active = true;
                }
            }
            Err(error) => {
                drop(pipes);
                let cleanup = terminate_and_reap(
                    platform,
                    group.established(),
                    &mut children,
                    &mut group.owner,
                );
                return Err(attach_cleanup(error, cleanup, plan.span()));
            }
        }
    }

    drop(pipes);
    if let (Some(bytes), Some(mut writer)) = (plan.supervisor_input(), supervisor_writer.take()) {
        let mut written = 0;
        while written < bytes.len() {
            match writer.write(&bytes[written..]) {
                Ok(0) => {
                    let error = RuntimeError::new(
                        RuntimeErrorKind::PipelineWrite(
                            opaal_platform::DescriptorWriteError::Operation {
                                kind: std::io::ErrorKind::WriteZero,
                                message: "execution capsule pipe accepted zero bytes".to_owned(),
                            },
                        ),
                        plan.span(),
                    );
                    let cleanup = terminate_and_reap(
                        platform,
                        group.established(),
                        &mut children,
                        &mut group.owner,
                    );
                    return Err(attach_cleanup(error, cleanup, plan.span()));
                }
                Ok(count) => written += count,
                Err(error) => {
                    let error =
                        RuntimeError::new(RuntimeErrorKind::PipelineWrite(error), plan.span());
                    let cleanup = terminate_and_reap(
                        platform,
                        group.established(),
                        &mut children,
                        &mut group.owner,
                    );
                    return Err(attach_cleanup(error, cleanup, plan.span()));
                }
            }
        }
    }
    Ok(StartedPipeline {
        children,
        group: group.established(),
        group_owner: group.take_owner(),
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
    /// Terminal ownership held for the external members. The shell process runs
    /// the internal island in its own group while the job owns the terminal; the
    /// island reads pipes rather than the keyboard, so it needs no ownership of
    /// its own, and it must not take the terminal back before the external
    /// members have finished with it.
    foreground: Option<Box<dyn ForegroundTerminalGuard>>,
    /// Scoped host-signal forwarding retained until every external member has
    /// been reaped and terminal ownership has returned.
    signals: Option<Box<dyn ForegroundSignalGuard>>,
    // On unwind, return the terminal and restore handlers before this control
    // drops the process-group anchor and makes its identifier reusable.
    control: MixedPipelineControl,
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
        let mut group_owner = self.control.take_group_owner();
        let mut statuses = Vec::with_capacity(children.len());
        let mut first_error = None;
        let foreground = self.foreground;
        let signals = self.signals;
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
                    let cleanup =
                        terminate_indexed_and_reap_retaining_owner(platform, group, &mut children);
                    if let Some(message) = cleanup {
                        let primary = first_error
                            .take()
                            .expect("the first mixed wait failure was just recorded");
                        first_error = Some(attach_cleanup(primary, Some(message), plan.span()));
                    }
                    break;
                }
                Err(_) => unreachable!("the first mixed wait failure stops ordinary waiting"),
            }
        }
        if first_error.is_none() && group_owner.is_some() {
            let mut failures = Vec::new();
            terminate_group(platform, group, &mut failures);
            if !failures.is_empty() {
                first_error = Some(RuntimeError::new(
                    RuntimeErrorKind::ProcessCleanup {
                        message: failures.join("; "),
                    },
                    plan.span(),
                ));
            }
        }
        // The terminal returns only after the last external member has been
        // waited, so a job that outlives the internal segments still owns it.
        let released = release_foreground(foreground, plan);
        let group_closed = release_owned_group(&mut group_owner, plan);
        let signals_restored = restore_foreground_signals(signals, plan);
        match first_error {
            Some(error) => Err(attach_secondaries(
                error,
                [released.err(), group_closed.err(), signals_restored.err()],
            )),
            None => {
                if let Err(error) = released {
                    return Err(attach_secondaries(
                        error,
                        [group_closed.err(), signals_restored.err()],
                    ));
                }
                group_closed?;
                signals_restored?;
                Ok((statuses, elapsed(self.started_at, clock.now())))
            }
        }
    }

    /// Stop and reap every spawned external stage after an unsuccessful path.
    pub(crate) fn terminate(
        self,
        platform: &dyn Platform,
        plan: &ExecutionPlan,
    ) -> Result<(), RuntimeError> {
        self.control.cancel_and_reap(platform);
        let released = release_foreground(self.foreground, plan);
        let mut group_owner = self.control.take_group_owner();
        let group_closed = release_owned_group(&mut group_owner, plan);
        let signals_restored = restore_foreground_signals(self.signals, plan);
        if let Some(message) = self.control.take_cleanup_failure() {
            let mut error =
                RuntimeError::new(RuntimeErrorKind::ProcessCleanup { message }, plan.span());
            error = attach_secondary(error, released.err());
            error = attach_secondary(error, group_closed.err());
            error = attach_secondary(error, signals_restored.err());
            return Err(error);
        }
        if let Err(error) = released {
            return Err(attach_secondaries(
                error,
                [group_closed.err(), signals_restored.err()],
            ));
        }
        group_closed?;
        signals_restored
    }
}

/// One idempotent cancellation owner shared by the mixed coordinator and all
/// scoped segment workers.
#[derive(Clone)]
pub(crate) struct MixedPipelineControl {
    cancelled: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<IndexedStartedChild>>>,
    group: Option<ProcessGroupId>,
    group_owner: Arc<Mutex<Option<Box<dyn OwnedProcessGroup>>>>,
    cleanup_failure: Arc<Mutex<Option<String>>>,
}

impl MixedPipelineControl {
    fn new(
        children: Vec<IndexedStartedChild>,
        group: Option<ProcessGroupId>,
        group_owner: Option<Box<dyn OwnedProcessGroup>>,
    ) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            children: Arc::new(Mutex::new(children)),
            group,
            group_owner: Arc::new(Mutex::new(group_owner)),
            cleanup_failure: Arc::new(Mutex::new(None)),
        }
    }

    /// Whether a failure or explicit exit has begun peer cancellation.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Trip cancellation once, terminate every live child, and perform the
    /// final waits before the originating failure or exit path returns.
    pub(crate) fn cancel_and_reap(&self, platform: &dyn Platform) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut children = self
            .children
            .lock()
            .expect("mixed child controller must not be poisoned");
        let failure =
            terminate_indexed_and_reap_retaining_owner(platform, self.group, &mut children);
        children.clear();
        *self
            .cleanup_failure
            .lock()
            .expect("mixed cleanup failure slot must not be poisoned") = failure;
    }

    fn take_cleanup_failure(&self) -> Option<String> {
        self.cleanup_failure
            .lock()
            .expect("mixed cleanup failure slot must not be poisoned")
            .take()
    }

    fn take_children(&self) -> Vec<IndexedStartedChild> {
        let mut children = self
            .children
            .lock()
            .expect("mixed child controller must not be poisoned");
        std::mem::take(&mut *children)
    }

    fn take_group_owner(&self) -> Option<Box<dyn OwnedProcessGroup>> {
        self.group_owner
            .lock()
            .expect("mixed process-group owner must not be poisoned")
            .take()
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
            // opaal-foundation-boundary(executor-invariant): Only a classified mixed topology enters the mixed executor.
            RuntimeErrorKind::Unsupported {
                feature: "a non-mixed plan in the mixed pipeline executor",
            },
            plan.span(),
        )
    })?;

    let mut signals = prepare_foreground_signals(plan, platform)?;
    let mut group = PipelineGroup::new(platform, plan.process_group_policy(), true, plan.span())?;
    let mut foreground_signals_active = false;

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
            let cleanup = terminate_indexed_and_reap(
                platform,
                group.established(),
                &mut children,
                &mut group.owner,
            );
            let error = attach_cleanup(error, cleanup, plan.span());
            let restored = restore_foreground_signals(signals.take(), plan).err();
            return Err(attach_secondary(error, restored));
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
                let placement = group.placement();
                if placement.requires_capability()
                    && !process_group_report_is_exact(placement, child.child.child.as_ref())
                {
                    children.push(child);
                    drop(pipes);
                    let primary = RuntimeError::new(
                        RuntimeErrorKind::ForegroundProcessGroupUnavailable,
                        stage.argv()[0].span(),
                    );
                    let cleanup = terminate_indexed_and_reap(
                        platform,
                        group.established(),
                        &mut children,
                        &mut group.owner,
                    );
                    let error = attach_cleanup(primary, cleanup, plan.span());
                    let restored = restore_foreground_signals(signals.take(), plan).err();
                    return Err(attach_secondary(error, restored));
                }
                group.adopt(child.child.child.as_ref());
                children.push(child);
                if !foreground_signals_active
                    && let Some(established) = group.established()
                    && let Some(signal_guard) = signals.as_deref_mut()
                    && let Err(error) = signal_guard.forward_to(established)
                {
                    drop(pipes);
                    let primary = RuntimeError::new(
                        RuntimeErrorKind::ForegroundSignal(error),
                        stage.argv()[0].span(),
                    );
                    let cleanup = terminate_indexed_and_reap(
                        platform,
                        Some(established),
                        &mut children,
                        &mut group.owner,
                    );
                    let error = attach_cleanup(primary, cleanup, plan.span());
                    let restored = restore_foreground_signals(signals.take(), plan).err();
                    return Err(attach_secondary(error, restored));
                }
                if signals.is_some() && group.established().is_some() {
                    foreground_signals_active = true;
                }
            }
            Err(error) => {
                drop(pipes);
                let cleanup = terminate_indexed_and_reap(
                    platform,
                    group.established(),
                    &mut children,
                    &mut group.owner,
                );
                let error = attach_cleanup(error, cleanup, plan.span());
                let restored = restore_foreground_signals(signals.take(), plan).err();
                return Err(attach_secondary(error, restored));
            }
        }
    }

    drop(pipes);
    let established = group.established();
    let foreground = match take_foreground(plan, platform, established) {
        Ok(foreground) => foreground,
        Err(error) => {
            let cleanup =
                terminate_indexed_and_reap(platform, established, &mut children, &mut group.owner);
            let error = attach_cleanup(error, cleanup, plan.span());
            let restored = restore_foreground_signals(signals.take(), plan).err();
            return Err(attach_secondary(error, restored));
        }
    };
    Ok(MixedPipeline {
        control: MixedPipelineControl::new(children, established, group.take_owner()),
        foreground,
        signals,
        group: established,
        segments,
        captured_output: captured_output.take(),
        started_at,
    })
}

fn validate_preflighted_external_plan(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    if plan.stages().is_empty() {
        return Err(RuntimeError::new(
            // opaal-foundation-boundary(executor-invariant): Parsed pipelines always contain at least one stage.
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
            // opaal-foundation-boundary(executor-invariant): Internal stages use the structured or mixed executor.
            RuntimeErrorKind::Unsupported {
                feature: "foreground internal-command execution",
            },
            stage.span(),
        ));
    }
    Ok(())
}

fn terminate_and_reap(
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    children: &mut [StartedChild],
    group_owner: &mut Option<Box<dyn OwnedProcessGroup>>,
) -> Option<String> {
    let mut failures = terminate_and_reap_retaining_owner(platform, group, children)
        .into_iter()
        .collect::<Vec<_>>();
    release_group_owner(group_owner, &mut failures);
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn terminate_and_reap_retaining_owner(
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    children: &mut [StartedChild],
) -> Option<String> {
    let mut failures = Vec::new();
    terminate_group(platform, group, &mut failures);
    for child in &mut *children {
        if let Err(error) = child.child.terminate()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            failures.push(error.to_string());
        }
    }
    for child in children {
        if let Err(error) = child.child.wait() {
            failures.push(error.to_string());
        }
    }
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn process_group_report_is_exact(placement: ProcessGroup, child: &dyn ChildProcess) -> bool {
    match placement {
        ProcessGroup::Inherit => child.process_group().is_none(),
        ProcessGroup::New => ProcessGroupId::new(child.id())
            .is_some_and(|group| child.process_group() == Some(group)),
        ProcessGroup::Join(group) => child.process_group() == Some(group),
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

fn terminate_indexed_and_reap(
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    children: &mut [IndexedStartedChild],
    group_owner: &mut Option<Box<dyn OwnedProcessGroup>>,
) -> Option<String> {
    let mut failures = terminate_indexed_and_reap_retaining_owner(platform, group, children)
        .into_iter()
        .collect::<Vec<_>>();
    release_group_owner(group_owner, &mut failures);
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn terminate_indexed_and_reap_retaining_owner(
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    children: &mut [IndexedStartedChild],
) -> Option<String> {
    let mut failures = Vec::new();
    terminate_group(platform, group, &mut failures);
    for child in &mut *children {
        if let Err(error) = child.child.child.terminate()
            && error.kind() != std::io::ErrorKind::NotFound
        {
            failures.push(error.to_string());
        }
    }
    for child in children {
        if let Err(error) = child.child.child.wait() {
            failures.push(error.to_string());
        }
    }
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn release_group_owner(
    group_owner: &mut Option<Box<dyn OwnedProcessGroup>>,
    failures: &mut Vec<String>,
) {
    let Some(mut owner) = group_owner.take() else {
        return;
    };
    if let Err(error) = owner.release() {
        failures.push(error.to_string());
    }
}

fn release_owned_group(
    group_owner: &mut Option<Box<dyn OwnedProcessGroup>>,
    plan: &ExecutionPlan,
) -> Result<(), RuntimeError> {
    let mut failures = Vec::new();
    release_group_owner(group_owner, &mut failures);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(RuntimeError::new(
            RuntimeErrorKind::ProcessCleanup {
                message: failures.join("; "),
            },
            plan.span(),
        ))
    }
}

fn terminate_group(
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    failures: &mut Vec<String>,
) {
    let Some(group) = group else {
        return;
    };
    if let Err(error) = platform.signal_process_group(group, JobSignal::Kill)
        && !matches!(
            error,
            SignalError::Operation {
                kind: std::io::ErrorKind::NotFound,
                ..
            }
        )
    {
        failures.push(error.to_string());
    }
}

fn attach_cleanup(
    primary: RuntimeError,
    cleanup: Option<String>,
    span: opaal_syntax::Span,
) -> RuntimeError {
    let Some(message) = cleanup else {
        return primary;
    };
    primary.with_cause(Arc::new(RuntimeError::new(
        RuntimeErrorKind::ProcessCleanup { message },
        span,
    )))
}

pub(crate) fn attach_secondary(
    primary: RuntimeError,
    secondary: Option<RuntimeError>,
) -> RuntimeError {
    let Some(mut secondary) = secondary else {
        return primary;
    };
    if let Some(existing) = primary.cause() {
        secondary = secondary.with_cause(Arc::new(existing.clone()));
    }
    primary.with_cause(Arc::new(secondary))
}

fn attach_secondaries(
    mut primary: RuntimeError,
    secondaries: impl IntoIterator<Item = Option<RuntimeError>>,
) -> RuntimeError {
    for secondary in secondaries {
        primary = attach_secondary(primary, secondary);
    }
    primary
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
    mut children: Vec<StartedChild>,
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    group: Option<ProcessGroupId>,
    group_owner: &mut Option<Box<dyn OwnedProcessGroup>>,
    clock: Option<&dyn Clock>,
) -> Result<Vec<StageCompletion>, RuntimeError> {
    let mut statuses = Vec::with_capacity(children.len());
    for child_index in 0..children.len() {
        let child = &mut children[child_index];
        let stage = &plan.stages()[child_index];
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
                // A failed observation or a stop that cannot be lifted ends
                // ordinary waiting. Force the common group first so peers and
                // descendants cannot retain pipes, then consume every direct
                // child handle before returning the primary failure.
                let cleanup = terminate_and_reap_retaining_owner(platform, group, &mut children);
                return Err(attach_cleanup(error, cleanup, plan.span()));
            }
        }
    }
    let mut failures = Vec::new();
    if group_owner.is_some() {
        terminate_group(platform, group, &mut failures);
    }
    if !failures.is_empty() {
        return Err(RuntimeError::new(
            RuntimeErrorKind::ProcessCleanup {
                message: failures.join("; "),
            },
            plan.span(),
        ));
    }
    Ok(statuses)
}

fn drain_stdout<D>(
    platform: &dyn Platform,
    reader: Box<dyn DescriptorEndpoint>,
    drain: &mut D,
    producer_span: opaal_syntax::Span,
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
    producer_span: opaal_syntax::Span,
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use opaal_platform::{
        Capabilities, FakePlatform, FileActionError, ForegroundSignalGuard, OwnedProcessGroup,
        PipeEndpoints, PipeError, PlatformError, ProcessGroupError, SignalError, SpawnError,
        TerminateError, WaitError,
    };
    use opaal_syntax::{ParseOutcome, SourceFile, SourceId, StatementKind, parse_opaal};

    use crate::builtin::standard_registry;
    use crate::eval::{ExpandedWord, FakeClock};

    #[test]
    fn mixed_cleanup_retains_the_stage_failure_and_its_existing_cause() {
        let source = SourceFile::new(SourceId::new(4), "mixed-failure.opaal", "^fixture");
        let span = source.span(0..8).expect("fixture span is valid");
        let stage_cause = RuntimeError::new(
            RuntimeErrorKind::ProcessCleanup {
                message: "stage cause".to_owned(),
            },
            span,
        );
        let primary = RuntimeError::new(RuntimeErrorKind::ForegroundProcessGroupUnavailable, span)
            .with_cause(Arc::new(stage_cause));
        let cleanup = RuntimeError::new(
            RuntimeErrorKind::ProcessCleanup {
                message: "mixed cleanup".to_owned(),
            },
            span,
        );

        let error = attach_secondary(primary, Some(cleanup));
        assert!(matches!(
            error.kind(),
            RuntimeErrorKind::ForegroundProcessGroupUnavailable
        ));
        let cleanup = error.cause().expect("cleanup remains visible");
        assert!(matches!(
            cleanup.kind(),
            RuntimeErrorKind::ProcessCleanup { message } if message == "mixed cleanup"
        ));
        assert!(matches!(
            cleanup.cause().map(RuntimeError::kind),
            Some(RuntimeErrorKind::ProcessCleanup { message }) if message == "stage cause"
        ));
    }

    #[derive(Default)]
    struct LifecycleLog {
        forwarded: AtomicUsize,
        restored: AtomicUsize,
        terminal_restored: AtomicUsize,
        group_kills: AtomicUsize,
        group_kill_target: AtomicU64,
        group_releases: AtomicUsize,
        released_before_group_kill: AtomicBool,
        released_before_terminal_restore: AtomicBool,
        terminal_handed_over: AtomicBool,
        panic_wait: AtomicBool,
        terminated: AtomicUsize,
        waits: AtomicUsize,
    }

    struct LifecycleSignals {
        log: Arc<LifecycleLog>,
        restored: bool,
    }

    struct LifecycleGroup {
        log: Arc<LifecycleLog>,
        released: bool,
    }

    impl std::fmt::Debug for LifecycleGroup {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("LifecycleGroup").finish()
        }
    }

    impl OwnedProcessGroup for LifecycleGroup {
        fn id(&self) -> ProcessGroupId {
            ProcessGroupId::new(77).expect("the lifecycle group id is nonzero")
        }

        fn release(&mut self) -> Result<(), ProcessGroupError> {
            if !self.released {
                self.released = true;
                if self.log.group_kills.load(Ordering::SeqCst) == 0 {
                    self.log
                        .released_before_group_kill
                        .store(true, Ordering::SeqCst);
                }
                if self.log.terminal_handed_over.load(Ordering::SeqCst)
                    && self.log.terminal_restored.load(Ordering::SeqCst) == 0
                {
                    self.log
                        .released_before_terminal_restore
                        .store(true, Ordering::SeqCst);
                }
                self.log.group_releases.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    impl Drop for LifecycleGroup {
        fn drop(&mut self) {
            let _ = self.release();
        }
    }

    impl std::fmt::Debug for LifecycleSignals {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("LifecycleSignals").finish()
        }
    }

    impl ForegroundSignalGuard for LifecycleSignals {
        fn forward_to(&mut self, _group: ProcessGroupId) -> Result<(), PlatformError> {
            self.log.forwarded.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn restore(&mut self) -> Result<(), PlatformError> {
            if !self.restored {
                self.restored = true;
                self.log.restored.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    impl Drop for LifecycleSignals {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    struct LifecycleChild {
        log: Arc<LifecycleLog>,
        reported_group: Option<ProcessGroupId>,
        fail_first_wait: AtomicBool,
        cleanup_fails: bool,
        terminated: bool,
    }

    impl std::fmt::Debug for LifecycleChild {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("LifecycleChild").finish()
        }
    }

    impl ChildProcess for LifecycleChild {
        fn id(&self) -> u64 {
            77
        }

        fn process_group(&self) -> Option<ProcessGroupId> {
            self.reported_group
        }

        fn wait(&mut self) -> Result<ProcessStatus, WaitError> {
            if self.log.panic_wait.swap(false, Ordering::SeqCst) {
                panic!("injected foreground wait panic");
            }
            self.log.waits.fetch_add(1, Ordering::SeqCst);
            if self.fail_first_wait.swap(false, Ordering::SeqCst) {
                return Err(WaitError::new(
                    std::io::ErrorKind::Other,
                    "injected foreground wait failure",
                ));
            }
            if self.terminated && self.cleanup_fails {
                return Err(WaitError::new(
                    std::io::ErrorKind::Other,
                    "injected final reap failure",
                ));
            }
            Ok(if self.terminated {
                ProcessStatus::Signaled(9)
            } else {
                ProcessStatus::Exited(0)
            })
        }

        fn terminate(&mut self) -> Result<(), TerminateError> {
            self.terminated = true;
            self.log.terminated.fetch_add(1, Ordering::SeqCst);
            if self.cleanup_fails {
                Err(TerminateError::new(
                    std::io::ErrorKind::Other,
                    "injected child termination failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    struct LifecyclePlatform {
        inner: FakePlatform,
        log: Arc<LifecycleLog>,
        reported_group: Option<ProcessGroupId>,
        wait_fails: bool,
        terminal_handover_fails: bool,
        terminal_restore_fails: bool,
        fail_second_spawn: bool,
        wrong_second_group: bool,
        cleanup_fails: bool,
        spawns: AtomicUsize,
    }

    struct EveryExecutable;

    struct LifecycleTerminal {
        log: Arc<LifecycleLog>,
        restore_fails: bool,
        restored: bool,
    }

    impl std::fmt::Debug for LifecycleTerminal {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("LifecycleTerminal").finish()
        }
    }

    impl ForegroundTerminalGuard for LifecycleTerminal {
        fn restore(&mut self) -> Result<(), PlatformError> {
            if !self.restored {
                self.restored = true;
                self.log.terminal_restored.fetch_add(1, Ordering::SeqCst);
            }
            if self.restore_fails {
                Err(PlatformError::Unavailable {
                    capability: Capability::ForegroundTerminal,
                    reason: "injected terminal restoration failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }

        fn previous_owner(&self) -> Option<ProcessGroupId> {
            None
        }
    }

    impl Drop for LifecycleTerminal {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    impl crate::resolve::ExecutableProbe for EveryExecutable {
        fn is_executable(&self, _path: &std::ffi::OsStr) -> bool {
            true
        }
    }

    impl Platform for LifecyclePlatform {
        fn capabilities(&self) -> Capabilities {
            Capabilities::full()
        }

        fn is_terminal(&self) -> bool {
            true
        }

        fn create_process_group(&self) -> Result<Box<dyn OwnedProcessGroup>, ProcessGroupError> {
            Ok(Box::new(LifecycleGroup {
                log: Arc::clone(&self.log),
                released: false,
            }))
        }

        fn pipe(&self) -> Result<PipeEndpoints, PipeError> {
            self.inner.pipe()
        }

        fn open_file(
            &self,
            request: FileOpenRequest<'_>,
        ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
            self.inner.open_file(request)
        }

        fn inherit_descriptor(
            &self,
            descriptor: u32,
        ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
            self.inner.inherit_descriptor(descriptor)
        }

        fn spawn(&self, _request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
            let index = self.spawns.fetch_add(1, Ordering::SeqCst);
            if self.fail_second_spawn && index == 1 {
                return Err(SpawnError::Operation {
                    kind: std::io::ErrorKind::Other,
                    message: "injected second-stage spawn failure".to_owned(),
                });
            }
            Ok(Box::new(LifecycleChild {
                log: Arc::clone(&self.log),
                reported_group: if self.wrong_second_group && index == 1 {
                    ProcessGroupId::new(88)
                } else {
                    self.reported_group
                },
                fail_first_wait: AtomicBool::new(self.wait_fails),
                cleanup_fails: self.cleanup_fails,
                terminated: false,
            }))
        }

        fn signal_process_group(
            &self,
            group: ProcessGroupId,
            signal: JobSignal,
        ) -> Result<(), SignalError> {
            if signal == JobSignal::Kill {
                self.log.group_kills.fetch_add(1, Ordering::SeqCst);
                self.log
                    .group_kill_target
                    .store(group.get(), Ordering::SeqCst);
            }
            Ok(())
        }

        fn prepare_foreground_signals(
            &self,
        ) -> Result<Box<dyn ForegroundSignalGuard>, PlatformError> {
            Ok(Box::new(LifecycleSignals {
                log: Arc::clone(&self.log),
                restored: false,
            }))
        }

        fn enter_foreground(
            &self,
            _group: ProcessGroupId,
        ) -> Result<Box<dyn ForegroundTerminalGuard>, PlatformError> {
            if self.terminal_handover_fails {
                return Err(PlatformError::Unavailable {
                    capability: Capability::ForegroundTerminal,
                    reason: "injected terminal handover failure".to_owned(),
                });
            }
            self.log.terminal_handed_over.store(true, Ordering::SeqCst);
            Ok(Box::new(LifecycleTerminal {
                log: Arc::clone(&self.log),
                restore_fails: self.terminal_restore_fails,
                restored: false,
            }))
        }
    }

    fn plan() -> ExecutionPlan {
        let source = SourceFile::new(SourceId::new(1), "lifecycle.opaal", "^fixture");
        let span = source.span(0..8).expect("fixture span is valid");
        ExecutionPlan::single_external(
            PathBuf::from("/fixture"),
            vec![ExpandedWord::synthetic(OsString::from("fixture"), span)],
            PathBuf::from("/work"),
            Environment::new(),
            false,
            SessionOptions::DEFAULT_CAPTURE_LIMIT,
            span,
        )
    }

    fn pipeline_plan() -> ExecutionPlan {
        let source = SourceFile::new(
            SourceId::new(2),
            "pipeline-lifecycle.opaal",
            "^first | ^second",
        );
        let ParseOutcome::Complete(script) = parse_opaal(&source) else {
            panic!("the lifecycle pipeline should parse");
        };
        let StatementKind::Job(job) = script.statements()[0].kind() else {
            panic!("the lifecycle fixture should contain one job");
        };
        plan_pipeline_with_options(
            &job.chain.or_terms()[0].and_terms()[0],
            "/work",
            &source,
            &mut ScopeStack::new(),
            &Environment::from_snapshot([("PATH", "/tools")]),
            &standard_registry(),
            &EveryExecutable,
            &SessionOptions::default(),
        )
        .expect("the lifecycle pipeline should plan")
    }

    fn mixed_pipeline_plan() -> ExecutionPlan {
        let source = SourceFile::new(
            SourceId::new(3),
            "mixed-pipeline-lifecycle.opaal",
            "^first | decode utf8 | collect",
        );
        let ParseOutcome::Complete(script) = parse_opaal(&source) else {
            panic!("the mixed lifecycle pipeline should parse");
        };
        let StatementKind::Job(job) = script.statements()[0].kind() else {
            panic!("the mixed lifecycle fixture should contain one job");
        };
        plan_pipeline_with_options(
            &job.chain.or_terms()[0].and_terms()[0],
            "/work",
            &source,
            &mut ScopeStack::new(),
            &Environment::from_snapshot([("PATH", "/tools")]),
            &standard_registry(),
            &EveryExecutable,
            &SessionOptions::default(),
        )
        .expect("the mixed lifecycle pipeline should plan")
    }

    #[test]
    fn terminal_handover_failure_kills_the_group_and_reaps_the_child() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: false,
            terminal_handover_fails: true,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_status(&plan(), &platform, &FakeClock::new())
            .expect_err("the injected terminal handover must fail");

        assert!(matches!(
            error.kind(),
            RuntimeErrorKind::ForegroundTerminal(_)
        ));
        assert_eq!(log.forwarded.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(log.waits.load(Ordering::SeqCst), 1);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
        assert!(!log.released_before_terminal_restore.load(Ordering::SeqCst));
    }

    #[test]
    fn wait_failure_kills_the_group_and_performs_a_final_reap() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: true,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_status(&plan(), &platform, &FakeClock::new())
            .expect_err("the injected child wait must fail");

        assert!(matches!(error.kind(), RuntimeErrorKind::ProcessWait(_)));
        assert_eq!(log.forwarded.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(log.waits.load(Ordering::SeqCst), 2);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminal_restored.load(Ordering::SeqCst), 1);
        assert!(!log.released_before_terminal_restore.load(Ordering::SeqCst));
    }

    #[test]
    fn later_spawn_failure_kills_the_established_group_and_reaps_earlier_members() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: false,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: true,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_pipeline(&pipeline_plan(), &platform)
            .expect_err("the injected second spawn must fail");

        assert!(matches!(error.kind(), RuntimeErrorKind::ProcessSpawn(_)));
        assert_eq!(log.forwarded.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(log.waits.load(Ordering::SeqCst), 1);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_failure_is_attached_without_replacing_the_primary_wait_failure() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log,
            reported_group: ProcessGroupId::new(77),
            wait_fails: true,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: true,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_status(&plan(), &platform, &FakeClock::new())
            .expect_err("the injected observation and cleanup must fail");

        assert!(matches!(error.kind(), RuntimeErrorKind::ProcessWait(_)));
        assert!(matches!(
            error.cause().map(RuntimeError::kind),
            Some(RuntimeErrorKind::ProcessCleanup { message })
                if message.contains("termination") && message.contains("reap")
        ));
    }

    #[test]
    fn wait_failure_retains_terminal_restoration_failure_as_secondary_evidence() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: true,
            terminal_handover_fails: false,
            terminal_restore_fails: true,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_status(&plan(), &platform, &FakeClock::new())
            .expect_err("the injected wait and terminal restoration must fail");

        assert!(matches!(error.kind(), RuntimeErrorKind::ProcessWait(_)));
        assert!(matches!(
            error.cause().map(RuntimeError::kind),
            Some(RuntimeErrorKind::ForegroundTerminal(_))
        ));
        assert_eq!(log.terminal_restored.load(Ordering::SeqCst), 1);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
        assert!(!log.released_before_terminal_restore.load(Ordering::SeqCst));
    }

    #[test]
    fn mismatched_foreground_member_cleans_the_independently_owned_group() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(88),
            wait_fails: false,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_status(&plan(), &platform, &FakeClock::new())
            .expect_err("a mismatched leader group must refuse foreground execution");

        assert!(matches!(
            error.kind(),
            RuntimeErrorKind::ForegroundProcessGroupUnavailable
        ));
        assert_eq!(log.forwarded.load(Ordering::SeqCst), 0);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kill_target.load(Ordering::SeqCst), 77);
        assert_eq!(log.group_releases.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(log.waits.load(Ordering::SeqCst), 1);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mixed_startup_mismatch_cleans_the_independently_owned_group() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(88),
            wait_fails: false,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error =
            start_mixed_pipeline(&mixed_pipeline_plan(), &platform, &FakeClock::new(), false)
                .err()
                .expect("a mismatched mixed leader group must refuse foreground execution");

        assert!(matches!(
            error.kind(),
            RuntimeErrorKind::ForegroundProcessGroupUnavailable
        ));
        assert_eq!(log.forwarded.load(Ordering::SeqCst), 0);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kill_target.load(Ordering::SeqCst), 77);
        assert_eq!(log.group_releases.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(log.waits.load(Ordering::SeqCst), 1);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mismatched_later_member_cleans_only_the_verified_established_group() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: false,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: true,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let error = execute_foreground_pipeline(&pipeline_plan(), &platform)
            .expect_err("a mismatched later group member must refuse foreground execution");

        assert!(matches!(
            error.kind(),
            RuntimeErrorKind::ForegroundProcessGroupUnavailable
        ));
        assert_eq!(log.forwarded.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kill_target.load(Ordering::SeqCst), 77);
        assert_eq!(log.terminated.load(Ordering::SeqCst), 2);
        assert_eq!(log.waits.load(Ordering::SeqCst), 2);
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn normal_completion_closes_the_group_before_releasing_its_stable_owner() {
        let log = Arc::new(LifecycleLog::default());
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: false,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        execute_foreground_status(&plan(), &platform, &FakeClock::new())
            .expect("normal foreground completion must close cleanly");

        assert_eq!(log.waits.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_kill_target.load(Ordering::SeqCst), 77);
        assert_eq!(log.group_releases.load(Ordering::SeqCst), 1);
        assert!(!log.released_before_group_kill.load(Ordering::SeqCst));
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
        assert_eq!(log.terminal_restored.load(Ordering::SeqCst), 1);
        assert!(!log.released_before_terminal_restore.load(Ordering::SeqCst));
    }

    #[test]
    fn panic_restores_the_terminal_before_releasing_the_group_identifier() {
        let log = Arc::new(LifecycleLog::default());
        log.panic_wait.store(true, Ordering::SeqCst);
        let platform = LifecyclePlatform {
            inner: FakePlatform::full(),
            log: Arc::clone(&log),
            reported_group: ProcessGroupId::new(77),
            wait_fails: false,
            terminal_handover_fails: false,
            terminal_restore_fails: false,
            fail_second_spawn: false,
            wrong_second_group: false,
            cleanup_fails: false,
            spawns: AtomicUsize::new(0),
        };

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = execute_foreground_status(&plan(), &platform, &FakeClock::new());
        }));
        assert!(panicked.is_err());
        assert_eq!(log.terminal_restored.load(Ordering::SeqCst), 1);
        assert_eq!(log.group_releases.load(Ordering::SeqCst), 1);
        assert!(!log.released_before_terminal_restore.load(Ordering::SeqCst));
        assert_eq!(log.restored.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mixed_completion_and_cancellation_keep_the_anchor_through_terminal_restoration() {
        for cancel in [false, true] {
            let log = Arc::new(LifecycleLog::default());
            let platform = LifecyclePlatform {
                inner: FakePlatform::full(),
                log: Arc::clone(&log),
                reported_group: ProcessGroupId::new(77),
                wait_fails: false,
                terminal_handover_fails: false,
                terminal_restore_fails: false,
                fail_second_spawn: false,
                wrong_second_group: false,
                cleanup_fails: false,
                spawns: AtomicUsize::new(0),
            };
            let plan = mixed_pipeline_plan();
            let clock = FakeClock::new();
            let pipeline = start_mixed_pipeline(&plan, &platform, &clock, false)
                .expect("the mixed foreground pipeline starts");
            if cancel {
                pipeline
                    .terminate(&platform, &plan)
                    .expect("mixed cancellation cleans the foreground group");
            } else {
                pipeline
                    .wait(&plan, &platform, &clock)
                    .expect("the mixed foreground pipeline completes");
            }
            assert_eq!(log.group_kills.load(Ordering::SeqCst), 1);
            assert_eq!(log.terminal_restored.load(Ordering::SeqCst), 1);
            assert_eq!(log.group_releases.load(Ordering::SeqCst), 1);
            assert!(!log.released_before_terminal_restore.load(Ordering::SeqCst));
            assert_eq!(log.restored.load(Ordering::SeqCst), 1);
        }
    }
}
