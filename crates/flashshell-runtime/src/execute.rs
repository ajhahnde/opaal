//! Foreground execution of inspectable command plans.
//!
//! The executor handles external stages with inherited standard descriptors,
//! byte-pipeline assignments, and source-ordered redirections. It always runs
//! platform-independent preflight before touching the platform, starts every
//! stage before waiting, and never renders shell source.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::ops::Range;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use flashshell_platform::{
    Capability, ChildDescriptor, ChildProcess, DescriptorEndpoint, FileOpenMode, FileOpenRequest,
    ForegroundTerminalGuard, Platform, ProcessGroup, ProcessGroupId, ProcessStatus, SpawnRequest,
};
use flashshell_syntax::{ConditionalChain, OutputMode, PipeOperator, Pipeline, SourceFile};

use crate::command::CommandRegistry;
use crate::eval::{Clock, Instant, RuntimeError, RuntimeErrorKind};
use crate::plan::{
    ExecutionPlan, PlannedRedirection, PlannedResolution, RedirectionAction, SessionOptions,
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
    let started = start_preflighted_pipeline(plan, platform, Some(clock), Some(writer))?;
    // Held across the drain and the wait; `Drop` is the backstop that returns
    // the terminal even when the drain callback panics.
    let foreground = take_foreground(plan, platform, started.group)?;
    let children = started.children;

    let (wait_result, drain_result) = thread::scope(|scope| {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let drain_task =
            scope.spawn(move || drain_stdout(platform, reader, drain, producer_span, ready_sender));
        ready_receiver
            .recv()
            .expect("the drain task signals before returning");
        let wait_result = wait_in_source_order(children, plan, Some(clock));
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
    let mut collector = BoundedCapture::new(options.capture_limit());
    let status = execute_conditional_chain_with(chain, &mut |pipeline| {
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
        let mut collect = |chunk: &[u8]| collector.push(chunk);
        let status = execute_foreground_with_stdout_drain(&plan, platform, clock, &mut collect)?;
        collector.ensure_within_limit(chain.span())?;
        Ok(status)
    })?;
    collector.finish(status, chain.span())
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

fn decode_text_capture(
    captured: CommandCapture<Vec<u8>>,
    span: flashshell_syntax::Span,
) -> Result<CommandCapture<String>, RuntimeError> {
    let (bytes, status) = captured.into_parts();
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
    Ok(CommandCapture { output, status })
}

struct BoundedCapture {
    output: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedCapture {
    const fn new(limit: usize) -> Self {
        Self {
            output: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let remaining = self.limit.saturating_sub(self.output.len());
        let retained = remaining.min(chunk.len());
        self.output.extend_from_slice(&chunk[..retained]);
        self.exceeded |= retained != chunk.len();
    }

    fn finish(
        self,
        status: Status,
        span: flashshell_syntax::Span,
    ) -> Result<CommandCapture<Vec<u8>>, RuntimeError> {
        self.ensure_within_limit(span)?;
        Ok(CommandCapture {
            output: self.output,
            status,
        })
    }

    fn ensure_within_limit(&self, span: flashshell_syntax::Span) -> Result<(), RuntimeError> {
        if self.exceeded {
            return Err(RuntimeError::new(
                RuntimeErrorKind::CaptureLimitExceeded { limit: self.limit },
                span,
            ));
        }
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

    if let [stage] = stages.as_slice() {
        return stage.clone();
    }

    let selected = if plan.pipefail() {
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
    chain: &flashshell_syntax::AndChain,
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
    pipeline: &flashshell_syntax::Pipeline,
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
    let started = start_preflighted_pipeline(plan, platform, clock, None)?;
    let foreground = take_foreground(plan, platform, started.group)?;
    let waited = wait_in_source_order(started.children, plan, clock);
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
    fn new(platform: &dyn Platform) -> Self {
        let placement = if platform.capabilities().supports(Capability::ProcessGroups) {
            ProcessGroup::New
        } else {
            ProcessGroup::Inherit
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
fn release_foreground(
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

fn take_foreground(
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

fn start_preflighted_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: Option<&dyn Clock>,
    mut final_output: Option<Box<dyn DescriptorEndpoint>>,
) -> Result<StartedPipeline, RuntimeError> {
    validate_preflighted_external_plan(plan)?;

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
    let mut group = PipelineGroup::new(platform);

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
        let request = SpawnRequest::new(path, &argv, &environment, plan.cwd())
            .expect("a planned command always carries argv zero")
            .with_descriptors(&descriptors)
            .expect("the final descriptor map has unique targets")
            .with_closed_descriptors(&closed_descriptors)
            .expect("a final descriptor cannot be both mapped and closed")
            .in_process_group(group.placement());
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
    Ok(StartedPipeline {
        children,
        group: group.established(),
    })
}

/// Running external stages surrounding one contiguous internal stage island.
///
/// The boundary endpoints remain parent-owned so the internal executor can pull
/// an external prefix lazily and push into an external suffix without capture.
pub(crate) struct MixedPipeline {
    children: Vec<IndexedStartedChild>,
    /// Terminal ownership held for the external members. The shell process runs
    /// the internal island in its own group while the job owns the terminal; the
    /// island reads pipes rather than the keyboard, so it needs no ownership of
    /// its own, and it must not take the terminal back before the external
    /// members have finished with it.
    foreground: Option<Box<dyn ForegroundTerminalGuard>>,
    input: Option<Box<dyn DescriptorEndpoint>>,
    output: Option<Box<dyn DescriptorEndpoint>>,
    started_at: Instant,
}

impl MixedPipeline {
    /// Take the external-prefix reader, when the internal island has one.
    pub(crate) fn take_input(&mut self) -> Option<Box<dyn DescriptorEndpoint>> {
        self.input.take()
    }

    /// Take the external-suffix writer, when the internal island has one.
    pub(crate) fn take_output(&mut self) -> Option<Box<dyn DescriptorEndpoint>> {
        self.output.take()
    }

    /// Wait every external stage and return source-indexed language statuses.
    pub(crate) fn wait(
        self,
        plan: &ExecutionPlan,
        clock: &dyn Clock,
    ) -> Result<(Vec<(usize, Status)>, Duration), RuntimeError> {
        let mut statuses = Vec::with_capacity(self.children.len());
        let mut first_error = None;
        let foreground = self.foreground;
        for mut started in self.children {
            let stage = &plan.stages()[started.index];
            match started.child.child.wait() {
                Ok(status) => {
                    let duration = started
                        .child
                        .started_at
                        .map_or(Duration::ZERO, |start| elapsed(start, clock.now()));
                    statuses.push((started.index, language_status(status, duration)));
                }
                Err(error) if first_error.is_none() => {
                    first_error = Some(RuntimeError::new(
                        RuntimeErrorKind::ProcessWait(error),
                        stage.span(),
                    ));
                }
                Err(_) => {}
            }
        }
        // The terminal returns only after the last external member has been
        // waited, so a job that outlives the internal island still owns it.
        let released = release_foreground(foreground, plan);
        match first_error {
            Some(error) => Err(error),
            None => {
                released?;
                Ok((statuses, elapsed(self.started_at, clock.now())))
            }
        }
    }

    /// Stop and reap every spawned external stage after an internal failure.
    pub(crate) fn terminate(mut self) {
        terminate_indexed_and_reap(&mut self.children);
    }
}

/// Start all external stages around one contiguous internal island.
///
/// External-to-external edges retain ordinary kernel pipes. Exactly the two
/// edges touching the island keep one endpoint in the parent for lazy pulls or
/// checked partial writes.
pub(crate) fn start_mixed_pipeline(
    plan: &ExecutionPlan,
    platform: &dyn Platform,
    clock: &dyn Clock,
    internal: Range<usize>,
) -> Result<MixedPipeline, RuntimeError> {
    preflight(plan)?;
    if internal.is_empty()
        || internal.end > plan.stages().len()
        || plan.stages()[internal.clone()]
            .iter()
            .any(|stage| !matches!(stage.resolution(), PlannedResolution::Internal { .. }))
        || plan.stages()[..internal.start]
            .iter()
            .chain(&plan.stages()[internal.end..])
            .any(|stage| !matches!(stage.resolution(), PlannedResolution::External { .. }))
    {
        return Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "a mixed pipeline with more than one internal stage island",
            },
            plan.span(),
        ));
    }

    let mut pipes = Vec::with_capacity(plan.edges().len());
    for (index, edge) in plan.edges().iter().enumerate() {
        if internal.contains(&index) && internal.contains(&(index + 1)) {
            pipes.push((None, None));
            continue;
        }
        let endpoints = platform.pipe().map_err(|error| {
            RuntimeError::new(RuntimeErrorKind::PipeCreate(error), edge.operator_span())
        })?;
        let (reader, writer) = endpoints.into_parts();
        pipes.push((Some(reader), Some(writer)));
    }

    let input = internal
        .start
        .checked_sub(1)
        .and_then(|edge| pipes[edge].0.take());
    let output = (internal.end < plan.stages().len())
        .then(|| pipes[internal.end - 1].1.take())
        .flatten();
    let environment: Vec<(OsString, OsString)> = plan
        .environment()
        .iter()
        .map(|(name, value)| (OsString::from(name), value.to_os_string()))
        .collect();
    let mut children = Vec::with_capacity(plan.stages().len() - internal.len());
    let started_at = clock.now();
    let mut group = PipelineGroup::new(platform);

    for (index, stage) in plan.stages().iter().enumerate() {
        if internal.contains(&index) {
            continue;
        }
        let input_endpoint = index.checked_sub(1).and_then(|edge| pipes[edge].0.take());
        let edge_output = pipes.get_mut(index).and_then(|edge| edge.1.take());
        let merge_output =
            edge_output.is_some() && plan.edges()[index].kind() == PipeOperator::StdoutAndStderr;
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
    let foreground = take_foreground(plan, platform, group.established())?;
    Ok(MixedPipeline {
        children,
        foreground,
        input,
        output,
        started_at,
    })
}

fn validate_preflighted_external_plan(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    if plan.stages().is_empty() {
        return Err(RuntimeError::new(
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
    clock: Option<&dyn Clock>,
) -> Result<Vec<StageCompletion>, RuntimeError> {
    let mut statuses = Vec::with_capacity(children.len());
    let mut first_error = None;
    for (mut child, stage) in children.into_iter().zip(plan.stages()) {
        match child.child.wait() {
            Ok(status) => {
                let duration = match (child.started_at, clock) {
                    (Some(started_at), Some(clock)) => elapsed(started_at, clock.now()),
                    _ => Duration::ZERO,
                };
                statuses.push(StageCompletion { status, duration });
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(RuntimeError::new(
                        RuntimeErrorKind::ProcessWait(error),
                        stage.span(),
                    ));
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
    producer_span: flashshell_syntax::Span,
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
    producer_span: flashshell_syntax::Span,
) -> Result<usize, RuntimeError> {
    platform
        .read_descriptor(reader, buffer)
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::CaptureRead(error), producer_span))
}

fn elapsed(start: Instant, end: Instant) -> Duration {
    Duration::from_nanos(i128::from(end.as_nanos().saturating_sub(start.as_nanos())))
}

fn language_status(status: ProcessStatus, duration: Duration) -> Status {
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
