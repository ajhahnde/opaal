//! Turning one parsed command pipeline into an inspectable [`ExecutionPlan`].
//!
//! Planning expands every command word, spread, and redirection target into
//! native arguments, resolves each stage's command internal-first (or forced
//! external), records the pipeline edges between stages, and captures each
//! stage's redirections in source order — all while retaining source spans and
//! without spawning a single process. The plan carries the working directory and
//! the resolved child environment so a later executor, or a debug printer, can
//! inspect exactly what would run.
//!
//! Planning only builds the plan. Rejecting NUL bytes, ambiguous
//! structured-to-byte edges, conflicting descriptor ownership, and unsupported
//! platform capabilities is a separate preflight concern.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use flashshell_syntax::{
    CommandItemKind, CommandStage, FileRedirection, IoNumber, OutputMode, PipeOperator, Pipeline,
    RedirectionKind, SourceFile, Span, StageKind,
};

use crate::command::{Carrier, CommandOutput, CommandRegistry};
use crate::eval::{
    CarrierBridge, CarrierMismatch, ExpandedWord, RuntimeError, RuntimeErrorKind, expand_spread,
    expand_word,
};
use crate::resolve::{ExecutableProbe, Resolution, ResolutionError, resolve_command};
use crate::{Environment, ScopeStack};

/// A complete, inspectable plan for one command pipeline.
///
/// `stages` are the ordered pipeline stages and `edges` the byte-pipeline
/// operators between them, so `edges.len() == stages.len() - 1`. `cwd` and
/// `environment` are the working directory and child environment every stage
/// would inherit; `span` is the whole pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionPlan {
    cwd: PathBuf,
    environment: Environment,
    stages: Vec<PlannedStage>,
    edges: Vec<PipelineEdge>,
    pipefail: bool,
    capture_limit: usize,
    span: Span,
}

impl ExecutionPlan {
    /// The working directory every stage would run in.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The resolved child environment every stage would inherit.
    #[must_use]
    pub const fn environment(&self) -> &Environment {
        &self.environment
    }

    /// The ordered pipeline stages.
    #[must_use]
    pub fn stages(&self) -> &[PlannedStage] {
        &self.stages
    }

    /// The byte-pipeline edges between consecutive stages.
    #[must_use]
    pub fn edges(&self) -> &[PipelineEdge] {
        &self.edges
    }

    /// Whether this plan uses rightmost-failure pipeline status aggregation.
    ///
    /// The value is copied from the session when the plan is built, so a later
    /// session-option change cannot alter an already running pipeline.
    #[must_use]
    pub const fn pipefail(&self) -> bool {
        self.pipefail
    }

    /// The maximum raw stdout bytes retained by command capture.
    ///
    /// The value is copied from the session when the plan is built. Reaching
    /// it exactly succeeds; observing a later byte produces a bounded capture
    /// error after the pipe has still been drained to EOF.
    #[must_use]
    pub const fn capture_limit(&self) -> usize {
        self.capture_limit
    }

    /// The whole-pipeline source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    /// Renders the plan as deterministic, human-readable text without executing
    /// it — what a plan-inspection command would print for a reader.
    ///
    /// Every native value (cwd, environment, argv, redirection targets) is shown
    /// with a lossy Unicode rendering, since this text is for a human reader, not
    /// serialization. Stages are listed in pipeline order with their resolution,
    /// argv, carrier contract, and source-order redirections; the byte-pipeline
    /// edges between them follow.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        // A plan never renders through `?`: writing to a `String` cannot fail.
        let _ = writeln!(out, "cwd {}", self.cwd.display());
        out.push_str("env\n");
        for (name, value) in self.environment.iter() {
            let _ = writeln!(out, "  {name}={}", value.to_string_lossy());
        }
        for (index, stage) in self.stages.iter().enumerate() {
            let _ = writeln!(
                out,
                "stage {index} {}",
                render_resolution(&stage.resolution)
            );
            out.push_str("  argv");
            for argument in &stage.argv {
                let _ = write!(out, " [{}]", argument.value().to_string_lossy());
            }
            out.push('\n');
            let inputs = stage
                .input_carriers
                .iter()
                .map(|carrier| format!("{carrier:?}"))
                .collect::<Vec<_>>()
                .join("|");
            let _ = writeln!(out, "  carriers in {inputs} out {:?}", stage.output_carrier);
            for redirection in &stage.redirections {
                let _ = writeln!(out, "  redir {}", render_redirection(&redirection.action));
            }
        }
        for (index, edge) in self.edges.iter().enumerate() {
            let operator = match edge.kind {
                PipeOperator::Stdout => "|",
                PipeOperator::StdoutAndStderr => "|&",
            };
            let _ = writeln!(out, "edge {index} {operator} {}", index + 1);
        }
        out
    }
}

/// Session execution options that affect command planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pipefail: bool,
    capture_limit: usize,
}

impl SessionOptions {
    /// The default raw stdout budget for one command capture (8 MiB).
    pub const DEFAULT_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;

    /// Whether pipelines select their rightmost unsuccessful stage.
    #[must_use]
    pub const fn pipefail(self) -> bool {
        self.pipefail
    }

    /// Return these options with `pipefail` set to `enabled`.
    #[must_use]
    pub const fn with_pipefail(mut self, enabled: bool) -> Self {
        self.pipefail = enabled;
        self
    }

    /// Change the option used by plans created after this call.
    pub const fn set_pipefail(&mut self, enabled: bool) {
        self.pipefail = enabled;
    }

    /// The maximum raw stdout bytes retained by one command capture.
    #[must_use]
    pub const fn capture_limit(self) -> usize {
        self.capture_limit
    }

    /// Return these options with the command-capture byte limit set to `limit`.
    #[must_use]
    pub const fn with_capture_limit(mut self, limit: usize) -> Self {
        self.capture_limit = limit;
        self
    }

    /// Change the capture limit used by plans created after this call.
    pub const fn set_capture_limit(&mut self, limit: usize) {
        self.capture_limit = limit;
    }
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            pipefail: false,
            capture_limit: Self::DEFAULT_CAPTURE_LIMIT,
        }
    }
}

/// Renders a stage's resolution as `internal NAME` or `external PATH`.
fn render_resolution(resolution: &PlannedResolution) -> String {
    match resolution {
        PlannedResolution::Internal { name } => format!("internal {name}"),
        PlannedResolution::External { path } => format!("external {}", path.display()),
    }
}

/// Renders one descriptor action in its familiar redirection spelling.
fn render_redirection(action: &RedirectionAction) -> String {
    match action {
        RedirectionAction::Input {
            descriptor, target, ..
        } => format!("{descriptor}< [{}]", target.value().to_string_lossy()),
        RedirectionAction::Output {
            descriptor,
            mode,
            target,
            ..
        } => {
            let operator = match mode {
                OutputMode::Truncate => ">",
                OutputMode::Append => ">>",
            };
            format!(
                "{descriptor}{operator} [{}]",
                target.value().to_string_lossy()
            )
        }
        RedirectionAction::Duplicate {
            descriptor, source, ..
        } => format!("{descriptor}>&{source}"),
        RedirectionAction::Close { descriptor, .. } => format!("{descriptor}>&-"),
    }
}

/// One planned pipeline stage: its resolved command, argv, and redirections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedStage {
    resolution: PlannedResolution,
    input_carriers: BTreeSet<Carrier>,
    output_carrier: Carrier,
    argv: Vec<ExpandedWord>,
    redirections: Vec<PlannedRedirection>,
    span: Span,
}

impl PlannedStage {
    /// How the stage's command name resolved.
    #[must_use]
    pub const fn resolution(&self) -> &PlannedResolution {
        &self.resolution
    }

    /// The carrier this stage produces on its output edge.
    #[must_use]
    pub const fn output_carrier(&self) -> Carrier {
        self.output_carrier
    }

    /// Whether this stage accepts `carrier` on its input edge.
    #[must_use]
    pub fn accepts_input(&self, carrier: Carrier) -> bool {
        self.input_carriers.contains(&carrier)
    }

    /// The carrier set this stage accepts on its input edge, in a deterministic
    /// order.
    #[must_use]
    pub fn accepted_inputs(&self) -> Vec<Carrier> {
        self.input_carriers.iter().copied().collect()
    }

    /// The expanded argument vector, with `argv[0]` the command word.
    #[must_use]
    pub fn argv(&self) -> &[ExpandedWord] {
        &self.argv
    }

    /// The stage-local redirections in source order.
    #[must_use]
    pub fn redirections(&self) -> &[PlannedRedirection] {
        &self.redirections
    }

    /// The stage's source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// A resolved command: an internal command or an external executable path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedResolution {
    /// A bare name matched a registered internal command.
    Internal {
        /// The registered command name.
        name: String,
    },
    /// A name resolved to an external executable path.
    External {
        /// The resolved native executable path.
        path: PathBuf,
    },
}

/// One byte-pipeline edge between two consecutive stages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PipelineEdge {
    kind: PipeOperator,
    operator_span: Span,
}

impl PipelineEdge {
    /// Whether the edge carries stdout only or stdout and stderr merged.
    #[must_use]
    pub const fn kind(&self) -> PipeOperator {
        self.kind
    }

    /// The pipe operator's source span.
    #[must_use]
    pub const fn operator_span(&self) -> Span {
        self.operator_span
    }
}

/// One stage-local redirection: its descriptor action and source span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedRedirection {
    action: RedirectionAction,
    span: Span,
}

impl PlannedRedirection {
    /// The descriptor action this redirection performs.
    #[must_use]
    pub const fn action(&self) -> &RedirectionAction {
        &self.action
    }

    /// The whole-redirection source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// A single descriptor action with its expanded operands and operator span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirectionAction {
    /// `[n]< target`: open `target` for reading on descriptor `n` (default 0).
    Input {
        /// The affected descriptor.
        descriptor: u32,
        /// The `<` operator span.
        operator_span: Span,
        /// The expanded input target.
        target: ExpandedWord,
    },
    /// `[n]> target` / `[n]>> target`: open `target` for writing on descriptor
    /// `n` (default 1), truncating or appending per `mode`.
    Output {
        /// The affected descriptor.
        descriptor: u32,
        /// Whether the target is truncated or appended to.
        mode: OutputMode,
        /// The `>`/`>>` operator span.
        operator_span: Span,
        /// The expanded output target.
        target: ExpandedWord,
    },
    /// `n>&m`: duplicate descriptor `source` onto `descriptor`.
    Duplicate {
        /// The descriptor being assigned.
        descriptor: u32,
        /// The `>&` operator span.
        operator_span: Span,
        /// The descriptor being duplicated.
        source: u32,
        /// The source descriptor's source span.
        target_span: Span,
    },
    /// `n>&-`: close descriptor `descriptor` for the stage.
    Close {
        /// The descriptor being closed.
        descriptor: u32,
        /// The `>&` operator span.
        operator_span: Span,
        /// The `-` operand's source span.
        target_span: Span,
    },
}

/// Plans one command pipeline into an [`ExecutionPlan`].
///
/// Each stage's command word and arguments are expanded to native argv, the
/// command is resolved internal-first (a `^`-marked head forces external), and
/// stage-local redirection targets are expanded in source order. `cwd` and
/// `environment` are captured as the plan's working directory and child
/// environment. An unresolvable command, an ineligible word or spread, an
/// unrepresentable descriptor, or a stage form outside a command plan is a
/// [`RuntimeError`].
pub fn plan_pipeline(
    pipeline: &Pipeline,
    cwd: impl Into<PathBuf>,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
) -> Result<ExecutionPlan, RuntimeError> {
    plan_pipeline_with_options(
        pipeline,
        cwd,
        source,
        scope,
        environment,
        registry,
        probe,
        &SessionOptions::default(),
    )
}

/// Plans one command pipeline and snapshots its session execution options.
///
/// This is the option-aware form of [`plan_pipeline`]. Planning remains free of
/// platform calls and process execution.
#[allow(clippy::too_many_arguments)]
pub fn plan_pipeline_with_options(
    pipeline: &Pipeline,
    cwd: impl Into<PathBuf>,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
) -> Result<ExecutionPlan, RuntimeError> {
    let mut stages = Vec::with_capacity(pipeline.stages().len());
    for stage in pipeline.stages() {
        let has_upstream = !stages.is_empty();
        let input_carrier = stages
            .last()
            .map_or(Carrier::Empty, PlannedStage::output_carrier);
        let span = stage.span();
        let StageKind::Command(command) = stage.kind() else {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Unsupported {
                    feature: "an expression stage in a command plan",
                },
                span,
            ));
        };
        let context = StagePlanningContext {
            source,
            environment,
            registry,
            probe,
            input_carrier,
            has_upstream,
        };
        stages.push(plan_stage(command, span, scope, &context)?);
    }

    let edges = pipeline
        .operators()
        .iter()
        .map(|operator| PipelineEdge {
            kind: *operator.kind(),
            operator_span: operator.span(),
        })
        .collect();

    Ok(ExecutionPlan {
        cwd: cwd.into(),
        environment: environment.clone(),
        stages,
        edges,
        pipefail: options.pipefail(),
        capture_limit: options.capture_limit(),
        span: pipeline.span(),
    })
}

struct StagePlanningContext<'a> {
    source: &'a SourceFile,
    environment: &'a Environment,
    registry: &'a CommandRegistry,
    probe: &'a dyn ExecutableProbe,
    input_carrier: Carrier,
    has_upstream: bool,
}

fn plan_stage(
    command: &CommandStage,
    span: Span,
    scope: &mut ScopeStack,
    context: &StagePlanningContext<'_>,
) -> Result<PlannedStage, RuntimeError> {
    // argv[0] is the expanded command word; the head marker only steers
    // resolution and is never part of the name.
    let head = expand_word(command.head.word(), context.source, scope)?;
    let force_external = command.head.kind() == flashshell_syntax::CommandHeadKind::ForcedExternal;
    let (resolution, input_carriers, output_carrier) =
        resolve(head.value(), force_external, command.head.span(), context)?;

    let mut argv = vec![head];
    let mut redirections = Vec::new();
    for item in &command.items {
        match item.kind() {
            CommandItemKind::Word(word) => {
                argv.push(expand_word(word, context.source, scope)?);
            }
            CommandItemKind::Spread(variable) => {
                argv.extend(expand_spread(variable, item.span(), context.source, scope)?);
            }
            CommandItemKind::Closure(_) => {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::Unsupported {
                        feature: "a closure command argument",
                    },
                    item.span(),
                ));
            }
            CommandItemKind::Redirection(redirection) => {
                let action = plan_redirection(redirection.kind(), context.source, scope)?;
                redirections.push(PlannedRedirection {
                    action,
                    span: redirection.span(),
                });
            }
        }
    }

    Ok(PlannedStage {
        resolution,
        input_carriers,
        output_carrier,
        argv,
        redirections,
        span,
    })
}

/// Resolves a stage's command name and its pipeline-carrier contract.
///
/// An internal command's carriers come from its signature; an external command
/// consumes and produces only `ByteStream`.
fn resolve(
    name: &OsStr,
    force_external: bool,
    head_span: Span,
    context: &StagePlanningContext<'_>,
) -> Result<(PlannedResolution, BTreeSet<Carrier>, Carrier), RuntimeError> {
    match resolve_command(
        name,
        force_external,
        context.registry,
        context.environment,
        context.probe,
    ) {
        Ok(Resolution::Internal(signature)) => {
            if signature.name() == "check"
                && signature.output() == CommandOutput::SameAsInput
                && !context.has_upstream
            {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::CheckRequiresUpstream,
                    head_span,
                ));
            }
            Ok((
                PlannedResolution::Internal {
                    name: signature.name().to_owned(),
                },
                signature.inputs().collect(),
                signature.output().resolve(context.input_carrier),
            ))
        }
        Ok(Resolution::External(command)) => Ok((
            PlannedResolution::External {
                path: command.path().to_owned(),
            },
            BTreeSet::from([Carrier::ByteStream]),
            Carrier::ByteStream,
        )),
        Err(ResolutionError::NotFound { name }) => Err(RuntimeError::new(
            RuntimeErrorKind::CommandNotFound { name },
            head_span,
        )),
    }
}

fn plan_redirection(
    kind: &RedirectionKind,
    source: &SourceFile,
    scope: &mut ScopeStack,
) -> Result<RedirectionAction, RuntimeError> {
    match kind {
        RedirectionKind::Input {
            descriptor,
            operator_span,
            target,
        } => Ok(RedirectionAction::Input {
            descriptor: descriptor_or(descriptor.as_ref(), 0, source)?,
            operator_span: *operator_span,
            target: expand_word(target, source, scope)?,
        }),
        RedirectionKind::File(FileRedirection {
            descriptor,
            mode,
            operator_span,
            target,
        }) => Ok(RedirectionAction::Output {
            descriptor: descriptor_or(descriptor.as_ref(), 1, source)?,
            mode: *mode,
            operator_span: *operator_span,
            target: expand_word(target, source, scope)?,
        }),
        RedirectionKind::Duplicate {
            descriptor,
            operator_span,
            target,
        } => Ok(RedirectionAction::Duplicate {
            descriptor: descriptor_value(*descriptor, source)?,
            operator_span: *operator_span,
            source: descriptor_value(*target, source)?,
            target_span: target.span(),
        }),
        RedirectionKind::Close {
            descriptor,
            operator_span,
            target_span,
        } => Ok(RedirectionAction::Close {
            descriptor: descriptor_value(*descriptor, source)?,
            operator_span: *operator_span,
            target_span: *target_span,
        }),
    }
}

/// Parses an optional descriptor number, falling back to `default` when absent.
fn descriptor_or(
    descriptor: Option<&IoNumber>,
    default: u32,
    source: &SourceFile,
) -> Result<u32, RuntimeError> {
    match descriptor {
        Some(number) => descriptor_value(*number, source),
        None => Ok(default),
    }
}

/// Parses a descriptor number's decimal spelling into a `u32`.
fn descriptor_value(number: IoNumber, source: &SourceFile) -> Result<u32, RuntimeError> {
    let text = source
        .slice(number.span())
        .expect("a lexed descriptor span is always valid source");
    text.parse::<u32>().map_err(|_| {
        RuntimeError::new(
            RuntimeErrorKind::RedirectionDescriptorOverflow,
            number.span(),
        )
    })
}

/// Validates a built plan before any stage is spawned.
///
/// Preflight rejects the statically detectable faults: a NUL byte in any argv
/// argument or redirection target (no external argv or platform path can carry
/// it), a descriptor duplication whose source is not open in the stage's
/// descriptor map, a pipeline head whose command cannot begin a pipeline (it
/// does not accept an empty input), and an incompatible pipeline edge (a
/// producer carrier the consumer does not accept, or a merged stdout+stderr edge
/// whose producer is not a byte stream). A carrier mismatch carries an
/// actionable diagnostic naming both commands, the accepted carrier set, and the
/// explicit boundary that would repair a structured-to-byte crossing. Platform
/// capability validation occurs at execution time so this pass remains
/// platform-independent.
pub fn preflight(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    for stage in plan.stages() {
        check_nul(stage)?;
        check_descriptor_ownership(stage)?;
    }
    check_head_input(plan)?;
    check_edges(plan)?;
    Ok(())
}

/// The head word a stage's reader typed, used to name a stage in a diagnostic.
///
/// `argv[0]` is always present: planning pushes the expanded command word first.
fn head_word(stage: &PlannedStage) -> String {
    stage
        .argv()
        .first()
        .map(|word| word.value().to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The explicit boundary that would repair a structured-to-byte crossing, if the
/// mismatch is exactly that crossing.
///
/// A structured producer that a byte consumer accepts is bridged by serializing;
/// a byte producer that a structured consumer accepts is bridged by parsing. Any
/// other mismatch (for example an empty carrier) has no single-boundary repair.
fn bridge_for(produced: Carrier, accepted: &[Carrier]) -> Option<CarrierBridge> {
    let is_structured = |carrier: Carrier| matches!(carrier, Carrier::Value | Carrier::ValueStream);
    if is_structured(produced) && accepted.contains(&Carrier::ByteStream) {
        Some(CarrierBridge::StructuredToByte)
    } else if produced == Carrier::ByteStream && accepted.iter().copied().any(is_structured) {
        Some(CarrierBridge::ByteToStructured)
    } else {
        None
    }
}

/// Rejects a pipeline head whose command can only consume a structured input.
///
/// The head stage has no upstream: its input is the inherited terminal, a byte
/// source (or nothing). A command that accepts `Empty` or `ByteStream` can
/// therefore begin a pipeline, but one that consumes only a value or value
/// stream has no structured source at the head and needs an upstream stage.
fn check_head_input(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    let Some(head) = plan.stages().first() else {
        return Ok(());
    };
    if head.accepts_input(Carrier::Empty) || head.accepts_input(Carrier::ByteStream) {
        return Ok(());
    }
    Err(RuntimeError::new(
        RuntimeErrorKind::PipelineHeadInput {
            command: head_word(head),
            accepted: head.accepted_inputs(),
        },
        head.span(),
    ))
}

/// Rejects a NUL byte in any argv argument or redirection target.
fn check_nul(stage: &PlannedStage) -> Result<(), RuntimeError> {
    for argument in stage.argv() {
        reject_nul(argument)?;
    }
    for redirection in stage.redirections() {
        match redirection.action() {
            RedirectionAction::Input { target, .. } | RedirectionAction::Output { target, .. } => {
                reject_nul(target)?
            }
            RedirectionAction::Duplicate { .. } | RedirectionAction::Close { .. } => {}
        }
    }
    Ok(())
}

/// A NUL byte anchors on the single contributing part when there is exactly one,
/// otherwise on the whole word.
fn reject_nul(word: &ExpandedWord) -> Result<(), RuntimeError> {
    if word.value().as_bytes().contains(&0) {
        let span = match word.parts() {
            [single] => *single,
            _ => word.span(),
        };
        return Err(RuntimeError::new(
            RuntimeErrorKind::ArgumentContainsNul,
            span,
        ));
    }
    Ok(())
}

/// Rejects an incompatible carrier edge between two consecutive stages.
///
/// A merged stdout+stderr edge that does not carry bytes is the more specific
/// fault and is reported first; otherwise a carrier the consumer does not accept
/// is reported with an actionable diagnostic naming both stages, the accepted
/// set, and a repair boundary when the crossing is structured-to-byte.
fn check_edges(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    for (index, edge) in plan.edges().iter().enumerate() {
        let producer = &plan.stages()[index];
        let consumer = &plan.stages()[index + 1];
        let carried = producer.output_carrier();
        // A merged stdout+stderr edge is only meaningful for a byte producer.
        if edge.kind() == PipeOperator::StdoutAndStderr && carried != Carrier::ByteStream {
            return Err(RuntimeError::new(
                RuntimeErrorKind::MergedEdgeNotByteStream {
                    producer_command: head_word(producer),
                    produced: carried,
                },
                edge.operator_span(),
            ));
        }
        if !consumer.accepts_input(carried) {
            let accepted = consumer.accepted_inputs();
            let bridge = bridge_for(carried, &accepted);
            return Err(RuntimeError::new(
                RuntimeErrorKind::CarrierMismatch(Box::new(CarrierMismatch {
                    producer_command: head_word(producer),
                    produced: carried,
                    consumer_command: head_word(consumer),
                    accepted,
                    bridge,
                })),
                edge.operator_span(),
            ));
        }
    }
    Ok(())
}

/// Rejects duplication from a descriptor not open in the stage's descriptor map.
///
/// The map begins with the session descriptors 0, 1, and 2; each redirection
/// applies left-to-right. An open/input action adds its descriptor, a
/// duplication requires its source to be open and adds its destination, and a
/// close removes its descriptor (closing an absent one is a successful no-op).
fn check_descriptor_ownership(stage: &PlannedStage) -> Result<(), RuntimeError> {
    let mut open: BTreeSet<u32> = BTreeSet::from([0, 1, 2]);
    for redirection in stage.redirections() {
        match redirection.action() {
            RedirectionAction::Input { descriptor, .. }
            | RedirectionAction::Output { descriptor, .. } => {
                open.insert(*descriptor);
            }
            RedirectionAction::Duplicate {
                descriptor,
                source,
                target_span,
                ..
            } => {
                if !open.contains(source) {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::DescriptorNotOpen {
                            descriptor: *source,
                        },
                        *target_span,
                    ));
                }
                open.insert(*descriptor);
            }
            RedirectionAction::Close { descriptor, .. } => {
                open.remove(descriptor);
            }
        }
    }
    Ok(())
}
