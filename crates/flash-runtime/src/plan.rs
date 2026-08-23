//! Turning one parsed command pipeline into an inspectable [`ExecutionPlan`].
//!
//! Planning expands every command word, spread, and redirection target into
//! native arguments, captures internal-command closures as typed callable
//! values, resolves each stage internal-first (or forced external), records the
//! pipeline edges between stages, and captures each stage's redirections in
//! source order — all while retaining source spans and without spawning a single
//! process. The plan carries the working directory and the resolved child
//! environment so a later executor, or a debug printer, can inspect exactly what
//! would run.
//!
//! Planning only builds the plan. Rejecting NUL bytes, ambiguous
//! structured-to-byte edges, conflicting descriptor ownership, and unsupported
//! platform capabilities is a separate preflight concern.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flash_syntax::{
    CommandItemKind, CommandStage, FileRedirection, IoNumber, OutputMode, PipeOperator, Pipeline,
    RedirectionKind, SourceFile, Span, StageKind, Word, WordPartKind,
};

use crate::carrier::{PipelineCarrierFault, StageCarrierContract, analyze_pipeline_carriers};
use crate::command::{
    Carrier, CommandArgumentFault, CommandArgumentFaultKind, CommandArgumentInput,
    CommandOptionTerminator, CommandOutput, CommandRegistry, CommandSignature,
};
use crate::eval::{
    ExpandedWord, ReservedCommandDetails, RuntimeError, RuntimeErrorKind,
    evaluate_closure_argument_with_binding_types, expand_spread, expand_word_with_environment,
};
use crate::help::{HelpCatalog, HelpSnapshot, render_help};
use crate::module::RuntimeBindingTypes;
use crate::resolve::{
    ExecutableProbe, Resolution, ResolutionError, resolve_command, resolve_external,
};
use crate::{Environment, ScopeStack, Value};

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
    process_group_policy: ProcessGroupPolicy,
    supervisor_input: Option<Arc<[u8]>>,
    supervisor_completion: bool,
    span: Span,
}

/// The maximal internal segments and external stages in one mixed plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MixedTopology {
    internal_segments: Vec<InternalSegment>,
    external_indices: Vec<usize>,
}

impl MixedTopology {
    pub(crate) fn internal_segments(&self) -> &[InternalSegment] {
        &self.internal_segments
    }

    pub(crate) fn external_indices(&self) -> &[usize] {
        &self.external_indices
    }
}

/// One maximal source-ordered range of internal stages in a mixed plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InternalSegment {
    ordinal: usize,
    stages: Range<usize>,
}

/// The final standard-output destination of an internal segment tail.
///
/// Pipeline assignment is applied before the stage's source-ordered local
/// redirections. The mixed executor uses this result to decide whether the
/// tail still drains into its external successor or into a local file.
pub(crate) enum InternalStdoutRoute<'a> {
    /// The initially assigned pipeline or session output remains selected.
    Default,
    /// A local byte-file redirection replaced the initially assigned output.
    File {
        target: &'a ExpandedWord,
        mode: OutputMode,
    },
    /// The resolved binding is not a writable byte destination the internal
    /// executor supports.
    Unsupported,
}

#[derive(Clone, Copy)]
enum InternalDescriptorRoute<'a> {
    DefaultOutput,
    Inherited,
    InputFile,
    OutputFile {
        target: &'a ExpandedWord,
        mode: OutputMode,
    },
}

/// Resolve an internal segment tail's stdout after pipeline assignment and
/// local redirections have applied from left to right.
pub(crate) fn internal_stdout_route(
    stage: &PlannedStage,
    merge_pipeline_output: bool,
) -> InternalStdoutRoute<'_> {
    let mut bindings = std::collections::BTreeMap::from([
        (0, InternalDescriptorRoute::Inherited),
        (1, InternalDescriptorRoute::DefaultOutput),
        (
            2,
            if merge_pipeline_output {
                InternalDescriptorRoute::DefaultOutput
            } else {
                InternalDescriptorRoute::Inherited
            },
        ),
    ]);

    for redirection in stage.redirections() {
        match redirection.action() {
            RedirectionAction::Input { descriptor, .. } => {
                bindings.insert(*descriptor, InternalDescriptorRoute::InputFile);
            }
            RedirectionAction::Output {
                descriptor,
                target,
                mode,
                ..
            } => {
                bindings.insert(
                    *descriptor,
                    InternalDescriptorRoute::OutputFile {
                        target,
                        mode: *mode,
                    },
                );
            }
            RedirectionAction::Duplicate {
                descriptor, source, ..
            } => {
                let Some(route) = bindings.get(source).copied() else {
                    return InternalStdoutRoute::Unsupported;
                };
                bindings.insert(*descriptor, route);
            }
            RedirectionAction::Close { descriptor, .. } => {
                bindings.remove(descriptor);
            }
        }
    }

    match bindings.get(&1) {
        Some(InternalDescriptorRoute::DefaultOutput) => InternalStdoutRoute::Default,
        Some(InternalDescriptorRoute::OutputFile { target, mode }) => InternalStdoutRoute::File {
            target,
            mode: *mode,
        },
        Some(InternalDescriptorRoute::Inherited | InternalDescriptorRoute::InputFile) | None => {
            InternalStdoutRoute::Unsupported
        }
    }
}

impl InternalSegment {
    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub(crate) fn stages(&self) -> Range<usize> {
        self.stages.clone()
    }
}

impl ExecutionPlan {
    /// Partition a mixed plan into maximal internal segments and external stages.
    ///
    /// All-internal and all-external plans retain their dedicated executors and
    /// therefore do not have a mixed topology.
    pub(crate) fn mixed_topology(&self) -> Option<MixedTopology> {
        let mut internal_segments = Vec::new();
        let mut external_indices = Vec::new();
        let mut open_segment = None;

        for (index, stage) in self.stages.iter().enumerate() {
            match stage.resolution() {
                PlannedResolution::Internal { .. } => {
                    open_segment.get_or_insert(index);
                }
                PlannedResolution::External { .. } => {
                    if let Some(start) = open_segment.take() {
                        internal_segments.push(InternalSegment {
                            ordinal: internal_segments.len(),
                            stages: start..index,
                        });
                    }
                    external_indices.push(index);
                }
            }
        }
        if let Some(start) = open_segment {
            internal_segments.push(InternalSegment {
                ordinal: internal_segments.len(),
                stages: start..self.stages.len(),
            });
        }

        (!internal_segments.is_empty() && !external_indices.is_empty()).then_some(MixedTopology {
            internal_segments,
            external_indices,
        })
    }

    /// Build a one-stage external plan whose argument vector is already known.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn single_external(
        path: PathBuf,
        argv: Vec<ExpandedWord>,
        cwd: PathBuf,
        environment: Environment,
        pipefail: bool,
        capture_limit: usize,
        span: Span,
    ) -> Self {
        assert!(!argv.is_empty(), "an external plan requires argv zero");
        Self {
            cwd,
            environment,
            stages: vec![PlannedStage {
                resolution: PlannedResolution::External { path },
                input_carriers: BTreeSet::from([Carrier::ByteStream]),
                output_carrier: Carrier::ByteStream,
                argv,
                arguments: Vec::new(),
                redirections: Vec::new(),
                help: None,
                span,
            }],
            edges: Vec::new(),
            pipefail,
            capture_limit,
            process_group_policy: ProcessGroupPolicy::Isolate,
            supervisor_input: None,
            supervisor_completion: false,
            span,
        }
    }

    pub(crate) fn with_supervisor_input(mut self, bytes: Vec<u8>) -> Self {
        debug_assert_eq!(self.stages.len(), 1);
        self.supervisor_input = Some(Arc::from(bytes));
        self
    }

    pub(crate) const fn with_supervisor_completion(mut self) -> Self {
        self.supervisor_completion = true;
        self
    }

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

    pub(crate) const fn process_group_policy(&self) -> ProcessGroupPolicy {
        self.process_group_policy
    }

    pub(crate) fn supervisor_input(&self) -> Option<&[u8]> {
        self.supervisor_input.as_deref()
    }

    pub(crate) const fn expects_supervisor_completion(&self) -> bool {
        self.supervisor_completion
    }

    /// The whole-pipeline source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    /// Renders the plan as deterministic, human-readable text without executing
    /// it — what a plan-inspection command would print for a reader.
    ///
    /// Native values use escaped byte rendering so distinct plans never collapse
    /// through lossy Unicode. Stages, arguments, retained spans, redirections,
    /// help snapshots, and byte-pipeline edges remain in source order. This is
    /// human-facing inspection output, not serialization or executable input.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        // A plan never renders through `?`: writing to a `String` cannot fail.
        let _ = writeln!(out, "plan span {}..{}", self.span.start(), self.span.end());
        let _ = writeln!(out, "cwd {}", render_native(self.cwd.as_os_str()));
        out.push_str("env\n");
        for (name, value) in self.environment.iter() {
            let _ = writeln!(
                out,
                "  {}={}",
                render_bytes(name.as_bytes()),
                render_native(value)
            );
        }
        let _ = writeln!(out, "pipefail {}", self.pipefail);
        let _ = writeln!(out, "capture-limit {}", self.capture_limit);
        let _ = writeln!(
            out,
            "process-group {}",
            match self.process_group_policy {
                ProcessGroupPolicy::Isolate => "isolate",
                ProcessGroupPolicy::Inherit => "inherit",
            }
        );
        for (index, stage) in self.stages.iter().enumerate() {
            let _ = writeln!(
                out,
                "stage {index} span {}..{} {}",
                stage.span.start(),
                stage.span.end(),
                render_resolution(&stage.resolution)
            );
            out.push_str("  argv\n");
            for (argument_index, argument) in stage.argv.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "    {argument_index} span {}..{} {}",
                    argument.span().start(),
                    argument.span().end(),
                    render_native(argument.value())
                );
            }
            if !stage.arguments.is_empty() {
                out.push_str("  arguments\n");
                for (argument_index, argument) in stage.arguments.iter().enumerate() {
                    match argument {
                        PlannedArgument::Word(word) => {
                            let _ = writeln!(
                                out,
                                "    {argument_index} word span {}..{} {}",
                                word.span().start(),
                                word.span().end(),
                                render_native(word.value())
                            );
                        }
                        PlannedArgument::Value { value, span } => {
                            let rendered = format!("{value:?}");
                            let _ = writeln!(
                                out,
                                "    {argument_index} value span {}..{} {}",
                                span.start(),
                                span.end(),
                                render_bytes(rendered.as_bytes())
                            );
                        }
                    }
                }
            }
            let inputs = stage
                .input_carriers
                .iter()
                .map(|carrier| format!("{carrier:?}"))
                .collect::<Vec<_>>()
                .join("|");
            let _ = writeln!(out, "  carriers in {inputs} out {:?}", stage.output_carrier);
            for redirection in &stage.redirections {
                let _ = writeln!(
                    out,
                    "  redir span {}..{} {}",
                    redirection.span.start(),
                    redirection.span.end(),
                    render_redirection(&redirection.action)
                );
            }
            if let Some(snapshot) = &stage.help {
                let mode = if snapshot.detailed() {
                    "detail"
                } else {
                    "list"
                };
                let rendered = render_help(snapshot);
                let _ = writeln!(
                    out,
                    "  help {mode} entries {} {}",
                    snapshot.entries().len(),
                    render_bytes(&rendered)
                );
            }
        }
        for (index, edge) in self.edges.iter().enumerate() {
            let operator = match edge.kind {
                PipeOperator::Stdout => "|",
                PipeOperator::StdoutAndStderr => "|&",
            };
            let _ = writeln!(
                out,
                "edge {index} span {}..{} {operator} {}",
                edge.operator_span.start(),
                edge.operator_span.end(),
                index + 1
            );
        }
        out
    }
}

/// Session execution options that affect command planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOptions {
    pipefail: bool,
    capture_limit: usize,
    process_group_policy: ProcessGroupPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessGroupPolicy {
    Isolate,
    Inherit,
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

    pub(crate) const fn inherit_process_group(mut self) -> Self {
        self.process_group_policy = ProcessGroupPolicy::Inherit;
        self
    }
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            pipefail: false,
            capture_limit: Self::DEFAULT_CAPTURE_LIMIT,
            process_group_policy: ProcessGroupPolicy::Isolate,
        }
    }
}

/// Renders a stage's source and canonical internal identity or external path.
fn render_resolution(resolution: &PlannedResolution) -> String {
    match resolution {
        PlannedResolution::Internal {
            source_name,
            canonical_name,
        } if source_name != canonical_name => {
            format!("internal {source_name} (canonical {canonical_name})")
        }
        PlannedResolution::Internal { canonical_name, .. } => {
            format!("internal {canonical_name}")
        }
        PlannedResolution::External { path } => {
            format!("external {}", render_native(path.as_os_str()))
        }
    }
}

/// Renders one native unit with unambiguous byte escapes.
fn render_native(value: &OsStr) -> String {
    render_bytes(value.as_bytes())
}

fn render_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut rendered = String::from("[");
    for byte in bytes {
        match *byte {
            b'\\' => rendered.push_str("\\\\"),
            b'[' => rendered.push_str("\\["),
            b']' => rendered.push_str("\\]"),
            0x20..=0x7e => rendered.push(char::from(*byte)),
            byte => {
                let _ = write!(rendered, "\\x{byte:02x}");
            }
        }
    }
    rendered.push(']');
    rendered
}

/// Renders one descriptor action in its familiar redirection spelling.
fn render_redirection(action: &RedirectionAction) -> String {
    match action {
        RedirectionAction::Input {
            descriptor,
            operator_span,
            target,
        } => format!(
            "{descriptor}< operator-span {}..{} target-span {}..{} {}",
            operator_span.start(),
            operator_span.end(),
            target.span().start(),
            target.span().end(),
            render_native(target.value())
        ),
        RedirectionAction::Output {
            descriptor,
            mode,
            operator_span,
            target,
        } => {
            let operator = match mode {
                OutputMode::Truncate => ">",
                OutputMode::Append => ">>",
            };
            format!(
                "{descriptor}{operator} operator-span {}..{} target-span {}..{} {}",
                operator_span.start(),
                operator_span.end(),
                target.span().start(),
                target.span().end(),
                render_native(target.value())
            )
        }
        RedirectionAction::Duplicate {
            descriptor,
            operator_span,
            source,
            target_span,
        } => format!(
            "{descriptor}>&{source} operator-span {}..{} target-span {}..{}",
            operator_span.start(),
            operator_span.end(),
            target_span.start(),
            target_span.end()
        ),
        RedirectionAction::Close {
            descriptor,
            operator_span,
            target_span,
        } => format!(
            "{descriptor}>&- operator-span {}..{} target-span {}..{}",
            operator_span.start(),
            operator_span.end(),
            target_span.start(),
            target_span.end()
        ),
    }
}

/// One planned pipeline stage: its resolved command, argv, and redirections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedStage {
    resolution: PlannedResolution,
    input_carriers: BTreeSet<Carrier>,
    output_carrier: Carrier,
    argv: Vec<ExpandedWord>,
    arguments: Vec<PlannedArgument>,
    redirections: Vec<PlannedRedirection>,
    help: Option<HelpSnapshot>,
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

    /// The source-order internal-command arguments.
    ///
    /// Ordinary words and spread elements retain their native encoding, while a
    /// closure argument is a captured callable value. External stages never
    /// contain value arguments and continue to use [`Self::argv`] exclusively.
    #[must_use]
    pub fn arguments(&self) -> &[PlannedArgument] {
        &self.arguments
    }

    /// The stage-local redirections in source order.
    #[must_use]
    pub fn redirections(&self) -> &[PlannedRedirection] {
        &self.redirections
    }

    /// Immutable metadata selected while planning an inspection-only `help` stage.
    #[must_use]
    pub const fn help_snapshot(&self) -> Option<&HelpSnapshot> {
        self.help.as_ref()
    }

    /// The stage's source span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// One source-order argument to a planned internal command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedArgument {
    /// One ordinary word or one element produced by a spread.
    Word(ExpandedWord),
    /// A typed value that must not be encoded into native argv.
    Value {
        /// The captured runtime value.
        value: Value,
        /// The source span of the typed argument.
        span: Span,
    },
}

impl PlannedArgument {
    /// The source span that produced this argument.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Word(word) => word.span(),
            Self::Value { span, .. } => *span,
        }
    }

    /// The ordinary expanded word, when this argument is argv-compatible.
    #[must_use]
    pub const fn as_word(&self) -> Option<&ExpandedWord> {
        match self {
            Self::Word(word) => Some(word),
            Self::Value { .. } => None,
        }
    }

    /// The typed runtime value, when this argument is not native argv.
    #[must_use]
    pub const fn as_value(&self) -> Option<&Value> {
        match self {
            Self::Word(_) => None,
            Self::Value { value, .. } => Some(value),
        }
    }
}

/// A resolved command with retained source/canonical identity or an external path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlannedResolution {
    /// A bare name matched a registered internal command.
    Internal {
        /// The expanded source spelling used by the caller.
        source_name: String,
        /// The canonical executor identity.
        canonical_name: String,
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
    plan_pipeline_with_options_and_binding_types(
        pipeline,
        cwd,
        source,
        scope,
        environment,
        registry,
        probe,
        options,
        Arc::new(RuntimeBindingTypes::default()),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_pipeline_with_options_and_binding_types(
    pipeline: &Pipeline,
    cwd: impl Into<PathBuf>,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    options: &SessionOptions,
    binding_types: Arc<RuntimeBindingTypes>,
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
                // flash-v1-boundary(carrier-refusal): Expression stages do not have native command-plan argv.
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
            binding_types: Arc::clone(&binding_types),
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
        process_group_policy: options.process_group_policy,
        supervisor_input: None,
        supervisor_completion: false,
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
    binding_types: Arc<RuntimeBindingTypes>,
}

fn plan_stage(
    command: &CommandStage,
    span: Span,
    scope: &mut ScopeStack,
    context: &StagePlanningContext<'_>,
) -> Result<PlannedStage, RuntimeError> {
    if command.head.kind() == flash_syntax::CommandHeadKind::Bare
        && context.source.slice(command.head.word().span()).ok() == Some("help")
    {
        return plan_help_stage(command, span, scope, context);
    }

    // argv[0] is the expanded command word; the head marker only steers
    // resolution and is never part of the name.
    let head = expand_word_with_environment(
        command.head.word(),
        context.source,
        scope,
        context.environment,
    )?;
    let force_external = command.head.kind() == flash_syntax::CommandHeadKind::ForcedExternal;
    let (mut resolution, mut input_carriers, mut output_carrier) =
        resolve(head.value(), force_external, command.head.span(), context)?;
    if matches!(
        &resolution,
        PlannedResolution::Internal { canonical_name, .. } if canonical_name == "help"
    ) {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinArgument {
                command: "help",
                message: "the help command head must be the static source word `help`".to_owned(),
            },
            command.head.span(),
        ));
    }
    let lower_command = matches!(
        &resolution,
        PlannedResolution::Internal { canonical_name, .. } if canonical_name == "command"
    );
    let mut command_path = None;

    let mut argv = vec![head];
    let mut arguments = Vec::new();
    let mut redirections = Vec::new();
    for item in &command.items {
        match item.kind() {
            CommandItemKind::Word(word) => {
                let word =
                    expand_word_with_environment(word, context.source, scope, context.environment)?;
                if lower_command && command_path.is_none() {
                    command_path = Some(resolve_dynamic_external(&word, context)?);
                }
                argv.push(word.clone());
                arguments.push(PlannedArgument::Word(word));
            }
            CommandItemKind::Spread(variable) => {
                let words = expand_spread(variable, item.span(), context.source, scope)?;
                if lower_command
                    && command_path.is_none()
                    && let Some(word) = words.first()
                {
                    command_path = Some(resolve_dynamic_external(word, context)?);
                }
                argv.extend(words.iter().cloned());
                arguments.extend(words.into_iter().map(PlannedArgument::Word));
            }
            CommandItemKind::Closure(closure) => {
                if lower_command {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::BuiltinArgument {
                            command: "command",
                            message: "expected a word argument, found a typed value".to_owned(),
                        },
                        item.span(),
                    ));
                }
                if matches!(resolution, PlannedResolution::External { .. }) {
                    return Err(RuntimeError::new(
                        // flash-v1-boundary(carrier-refusal): Closures are typed values and never native argv.
                        RuntimeErrorKind::Unsupported {
                            feature: "a closure argument to an external command",
                        },
                        item.span(),
                    ));
                }
                arguments.push(PlannedArgument::Value {
                    value: evaluate_closure_argument_with_binding_types(
                        closure,
                        context.source,
                        scope,
                        Arc::clone(&context.binding_types),
                    )?,
                    span: item.span(),
                });
            }
            CommandItemKind::Redirection(redirection) => {
                let action = plan_redirection(
                    redirection.kind(),
                    context.source,
                    scope,
                    context.environment,
                )?;
                redirections.push(PlannedRedirection {
                    action,
                    span: redirection.span(),
                });
            }
        }
    }

    if let PlannedResolution::Internal { canonical_name, .. } = &resolution {
        let signature = context
            .registry
            .lookup(canonical_name)
            .expect("an internal resolution retains its registered signature");
        validate_planned_arguments(signature, &arguments, span)?;
        if signature.arguments().terminator() == CommandOptionTerminator::Accepted {
            remove_option_terminator(&mut arguments);
            argv.truncate(1);
            argv.extend(arguments.iter().filter_map(|argument| match argument {
                PlannedArgument::Word(word) => Some(word.clone()),
                PlannedArgument::Value { .. } => None,
            }));
        }
    }

    if lower_command {
        let Some(path) = command_path else {
            return Err(RuntimeError::new(
                RuntimeErrorKind::BuiltinArity {
                    command: "command",
                    minimum: 1,
                    maximum: None,
                    actual: 0,
                },
                span,
            ));
        };
        resolution = PlannedResolution::External { path };
        input_carriers = BTreeSet::from([Carrier::ByteStream]);
        output_carrier = Carrier::ByteStream;
        argv.remove(0);
        arguments.clear();
    }

    Ok(PlannedStage {
        resolution,
        input_carriers,
        output_carrier,
        argv,
        arguments,
        redirections,
        help: None,
        span,
    })
}

fn validate_planned_arguments(
    signature: &CommandSignature,
    arguments: &[PlannedArgument],
    stage_span: Span,
) -> Result<(), RuntimeError> {
    let inputs = arguments
        .iter()
        .map(|argument| match argument {
            PlannedArgument::Word(word) => {
                CommandArgumentInput::Word(Some(word.value().as_bytes().to_vec()))
            }
            PlannedArgument::Value { .. } => CommandArgumentInput::Closure,
        })
        .collect::<Vec<_>>();
    let Some(fault) = signature.arguments().validate(&inputs).into_iter().next() else {
        return Ok(());
    };
    let span = fault
        .argument_index()
        .and_then(|index| arguments.get(index))
        .map_or(stage_span, PlannedArgument::span);
    Err(RuntimeError::new(
        runtime_argument_fault(standard_command_name(signature.name()), &fault),
        span,
    ))
}

fn runtime_argument_fault(command: &'static str, fault: &CommandArgumentFault) -> RuntimeErrorKind {
    match fault.kind() {
        CommandArgumentFaultKind::Arity {
            minimum,
            maximum,
            actual,
        } => RuntimeErrorKind::BuiltinArity {
            command,
            minimum: *minimum,
            maximum: *maximum,
            actual: *actual,
        },
        CommandArgumentFaultKind::UnknownOption { option } => RuntimeErrorKind::BuiltinArgument {
            command,
            message: format!("unknown option `{option}`"),
        },
        CommandArgumentFaultKind::MissingOptionValues {
            option,
            expected,
            actual,
        } => RuntimeErrorKind::BuiltinArgument {
            command,
            message: format!("option `{option}` expects {expected} value(s), found {actual}"),
        },
        CommandArgumentFaultKind::RepeatedOption { option } => RuntimeErrorKind::BuiltinArgument {
            command,
            message: format!("option `{option}` cannot be repeated"),
        },
        CommandArgumentFaultKind::ConflictingOptions { option, conflict } => {
            RuntimeErrorKind::BuiltinArgument {
                command,
                message: format!("options `{option}` and `{conflict}` conflict"),
            }
        }
        CommandArgumentFaultKind::OptionAfterPositional { option } => {
            RuntimeErrorKind::BuiltinArgument {
                command,
                message: format!("option `{option}` must precede positional arguments"),
            }
        }
        CommandArgumentFaultKind::UnexpectedKind {
            position,
            expected,
            actual,
        } => RuntimeErrorKind::BuiltinArgument {
            command,
            message: format!(
                "argument {} expects {expected:?}, found {actual:?}",
                position + 1
            ),
        },
        CommandArgumentFaultKind::DynamicTail => RuntimeErrorKind::BuiltinArgument {
            command,
            message: "a dynamic argument tail is not accepted".to_owned(),
        },
    }
}

fn remove_option_terminator(arguments: &mut Vec<PlannedArgument>) {
    let Some(index) = arguments.iter().position(|argument| {
        matches!(
            argument,
            PlannedArgument::Word(word) if word.value().as_bytes() == b"--"
        )
    }) else {
        return;
    };
    arguments.remove(index);
}

fn standard_command_name(name: &str) -> &'static str {
    match name {
        "cd" => "cd",
        "pwd" => "pwd",
        "which" => "which",
        "command" => "command",
        "exit" => "exit",
        "check" => "check",
        "decode" => "decode",
        "from" => "from",
        "encode" => "encode",
        "to" => "to",
        "first" => "first",
        "last" => "last",
        "collect" => "collect",
        "length" => "length",
        "lines" => "lines",
        "each" => "each",
        "where" => "where",
        "select" => "select",
        "get" => "get",
        "update" => "update",
        "sort" => "sort",
        "ls" => "ls",
        "open" => "open",
        "save" => "save",
        "jobs" => "jobs",
        "fg" => "fg",
        "bg" => "bg",
        "wait" => "wait",
        "kill" => "kill",
        "help" => "help",
        _ => "internal",
    }
}

fn resolve_dynamic_external(
    target: &ExpandedWord,
    context: &StagePlanningContext<'_>,
) -> Result<PathBuf, RuntimeError> {
    let resolved =
        resolve_external(target.value(), context.environment, context.probe).map_err(|error| {
            let ResolutionError::NotFound { name } = error else {
                unreachable!("direct external resolution cannot observe namespace reservations");
            };
            RuntimeError::new(RuntimeErrorKind::CommandNotFound { name }, target.span())
        })?;
    Ok(resolved.path().to_owned())
}

fn plan_help_stage(
    command: &CommandStage,
    span: Span,
    scope: &ScopeStack,
    context: &StagePlanningContext<'_>,
) -> Result<PlannedStage, RuntimeError> {
    let signature = context
        .registry
        .lookup("help")
        .expect("the standard help command is registered");
    let mut head_scope = scope.clone();
    let head = expand_word_with_environment(
        command.head.word(),
        context.source,
        &mut head_scope,
        context.environment,
    )?;
    let mut argv = vec![head];
    let mut arguments = Vec::new();
    let mut redirections = Vec::new();
    let mut query = None;
    let mut query_span = command.head.span();
    let argument_items = command
        .items
        .iter()
        .filter(|item| !matches!(item.kind(), CommandItemKind::Redirection(_)))
        .collect::<Vec<_>>();
    if argument_items.len() > 1 {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinArity {
                command: "help",
                minimum: 0,
                maximum: Some(1),
                actual: argument_items.len(),
            },
            argument_items[1].span(),
        ));
    }

    for item in &command.items {
        match item.kind() {
            CommandItemKind::Word(word) => {
                if !is_static_word(word) {
                    return Err(RuntimeError::new(
                        RuntimeErrorKind::BuiltinArgument {
                            command: "help",
                            message: "the optional name must be one static source word".to_owned(),
                        },
                        item.span(),
                    ));
                }
                query_span = item.span();
                let mut query_scope = scope.clone();
                let expanded = expand_word_with_environment(
                    word,
                    context.source,
                    &mut query_scope,
                    context.environment,
                )?;
                let name = expanded.value().to_str().ok_or_else(|| {
                    RuntimeError::new(
                        RuntimeErrorKind::BuiltinArgument {
                            command: "help",
                            message: "the optional name must be UTF-8 source text".to_owned(),
                        },
                        item.span(),
                    )
                })?;
                query = Some(name.to_owned());
                argv.push(expanded.clone());
                arguments.push(PlannedArgument::Word(expanded));
            }
            CommandItemKind::Spread(_) | CommandItemKind::Closure(_) => {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::BuiltinArgument {
                        command: "help",
                        message: "the optional name must be one static source word".to_owned(),
                    },
                    item.span(),
                ));
            }
            CommandItemKind::Redirection(redirection) => {
                let mut redirection_scope = scope.clone();
                let action = plan_redirection(
                    redirection.kind(),
                    context.source,
                    &mut redirection_scope,
                    context.environment,
                )?;
                redirections.push(PlannedRedirection {
                    action,
                    span: redirection.span(),
                });
            }
        }
    }

    let catalog = HelpCatalog::snapshot(context.registry, scope);
    let entries = catalog.query(query.as_deref());
    if query.is_some() && entries.is_empty() {
        return Err(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "help",
                message: format!(
                    "unknown help name `{}`",
                    query.as_deref().unwrap_or_default()
                ),
            },
            query_span,
        ));
    }

    Ok(PlannedStage {
        resolution: PlannedResolution::Internal {
            source_name: signature.name().to_owned(),
            canonical_name: signature.name().to_owned(),
        },
        input_carriers: signature.inputs().collect(),
        output_carrier: signature.output().resolve(context.input_carrier),
        argv,
        arguments,
        redirections,
        help: Some(HelpSnapshot::new(entries, query.is_some())),
        span,
    })
}

fn is_static_word(word: &Word) -> bool {
    word.parts().iter().all(|part| match part.kind() {
        WordPartKind::Bare | WordPartKind::BareEscape | WordPartKind::SingleQuoted => true,
        WordPartKind::DoubleQuoted(parts) => parts.iter().all(|part| {
            matches!(
                part.kind(),
                WordPartKind::DoubleText | WordPartKind::DoubleEscape
            )
        }),
        WordPartKind::DoubleText
        | WordPartKind::DoubleEscape
        | WordPartKind::Variable(_)
        | WordPartKind::BracedInterpolation(_)
        | WordPartKind::CommandSubstitution(_) => false,
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
        Ok(Resolution::Internal {
            source_name,
            canonical_name,
            signature,
        }) => {
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
                    source_name,
                    canonical_name: canonical_name.to_owned(),
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
        Err(ResolutionError::Reserved {
            name,
            purpose,
            replacement,
        }) => Err(RuntimeError::new(
            RuntimeErrorKind::ReservedCommand(Box::new(ReservedCommandDetails::new(
                name,
                purpose,
                replacement,
            ))),
            head_span,
        )),
    }
}

fn plan_redirection(
    kind: &RedirectionKind,
    source: &SourceFile,
    scope: &mut ScopeStack,
    environment: &Environment,
) -> Result<RedirectionAction, RuntimeError> {
    match kind {
        RedirectionKind::Input {
            descriptor,
            operator_span,
            target,
        } => Ok(RedirectionAction::Input {
            descriptor: descriptor_or(descriptor.as_ref(), 0, source)?,
            operator_span: *operator_span,
            target: expand_word_with_environment(target, source, scope, environment)?,
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
            target: expand_word_with_environment(target, source, scope, environment)?,
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
/// descriptor map, an unsupported resolved stdout route at an internal segment
/// tail, a pipeline head whose command cannot begin a pipeline (it does not
/// accept an empty input), and an incompatible pipeline edge (a
/// producer carrier the consumer does not accept, or a merged stdout+stderr edge
/// whose producer is not a byte stream). A carrier mismatch carries an
/// actionable diagnostic naming both commands, the accepted carrier set, and the
/// explicit boundary that would repair a structured-to-byte crossing. Platform
/// capability validation occurs at execution time so this pass remains
/// platform-independent.
pub fn preflight(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    for (index, stage) in plan.stages().iter().enumerate() {
        check_nul(stage)?;
        check_descriptor_ownership(stage)?;
        check_internal_stdout_route(plan, index, stage)?;
    }
    check_carriers(plan)?;
    Ok(())
}

fn check_internal_stdout_route(
    plan: &ExecutionPlan,
    index: usize,
    stage: &PlannedStage,
) -> Result<(), RuntimeError> {
    if !matches!(stage.resolution(), PlannedResolution::Internal { .. })
        || plan
            .stages()
            .get(index + 1)
            .is_some_and(|next| matches!(next.resolution(), PlannedResolution::Internal { .. }))
    {
        return Ok(());
    }
    let merge_pipeline_output = plan
        .edges()
        .get(index)
        .is_some_and(|edge| edge.kind() == PipeOperator::StdoutAndStderr);
    if matches!(
        internal_stdout_route(stage, merge_pipeline_output),
        InternalStdoutRoute::Unsupported
    ) {
        return Err(RuntimeError::new(
            // flash-v1-boundary(carrier-refusal): Structured stdout requires an explicit byte conversion.
            RuntimeErrorKind::Unsupported {
                feature: "this resolved stdout route on an internal byte stream",
            },
            stage.span(),
        ));
    }
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

/// Maps the first shared carrier fault back to the established runtime surface.
fn check_carriers(plan: &ExecutionPlan) -> Result<(), RuntimeError> {
    let stages = plan
        .stages()
        .iter()
        .map(|stage| {
            StageCarrierContract::known(
                head_word(stage),
                stage.accepted_inputs(),
                CommandOutput::Fixed(stage.output_carrier()),
            )
        })
        .collect::<Vec<_>>();
    let operators = plan
        .edges()
        .iter()
        .map(PipelineEdge::kind)
        .collect::<Vec<_>>();
    let Some(fault) = analyze_pipeline_carriers(&stages, &operators)
        .into_iter()
        .next()
    else {
        return Ok(());
    };
    match fault {
        PipelineCarrierFault::HeadInput {
            stage,
            command,
            accepted,
        } => Err(RuntimeError::new(
            RuntimeErrorKind::PipelineHeadInput { command, accepted },
            plan.stages()[stage].span(),
        )),
        PipelineCarrierFault::MergedEdgeNotByteStream {
            edge,
            producer_command,
            produced,
        } => Err(RuntimeError::new(
            RuntimeErrorKind::MergedEdgeNotByteStream {
                producer_command,
                produced,
            },
            plan.edges()[edge].operator_span(),
        )),
        PipelineCarrierFault::CarrierMismatch { edge, mismatch } => Err(RuntimeError::new(
            RuntimeErrorKind::CarrierMismatch(Box::new(mismatch)),
            plan.edges()[edge].operator_span(),
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use flash_syntax::SourceId;

    fn topology_plan(kinds: &str) -> ExecutionPlan {
        let source = SourceFile::new(SourceId::new(1), "topology.fsh", kinds);
        let stages = kinds
            .char_indices()
            .map(|(index, kind)| PlannedStage {
                resolution: match kind {
                    'I' => PlannedResolution::Internal {
                        source_name: format!("internal-{index}"),
                        canonical_name: format!("internal-{index}"),
                    },
                    'E' => PlannedResolution::External {
                        path: PathBuf::from(format!("/bin/external-{index}")),
                    },
                    other => panic!("unexpected topology stage {other:?}"),
                },
                input_carriers: BTreeSet::from([Carrier::ByteStream]),
                output_carrier: Carrier::ByteStream,
                argv: Vec::new(),
                arguments: Vec::new(),
                redirections: Vec::new(),
                help: None,
                span: source
                    .span(index..index + 1)
                    .expect("a topology stage has a valid span"),
            })
            .collect::<Vec<_>>();
        let edges = (0..stages.len().saturating_sub(1))
            .map(|index| PipelineEdge {
                kind: PipeOperator::Stdout,
                operator_span: source
                    .span(index..index + 1)
                    .expect("a topology edge has a valid span"),
            })
            .collect();
        ExecutionPlan {
            cwd: PathBuf::from("/work"),
            environment: Environment::new(),
            stages,
            edges,
            pipefail: false,
            capture_limit: SessionOptions::DEFAULT_CAPTURE_LIMIT,
            process_group_policy: ProcessGroupPolicy::Isolate,
            supervisor_input: None,
            supervisor_completion: false,
            span: source
                .span(0..kinds.len())
                .expect("a topology plan has a valid span"),
        }
    }

    #[test]
    fn mixed_topology_partitions_maximal_internal_segments() {
        let cases = [
            ("I", None),
            ("III", None),
            ("E", None),
            ("EEE", None),
            ("IEI", Some((vec![0..1, 2..3], vec![1]))),
            ("IEIE", Some((vec![0..1, 2..3], vec![1, 3]))),
            ("EIEI", Some((vec![1..2, 3..4], vec![0, 2]))),
            ("EIE", Some((std::iter::once(1..2).collect(), vec![0, 2]))),
            ("IEEI", Some((vec![0..1, 3..4], vec![1, 2]))),
            ("IIEEIIEII", Some((vec![0..2, 4..6, 7..9], vec![2, 3, 6]))),
            (
                "EIIEEIIEIIE",
                Some((vec![1..3, 5..7, 8..10], vec![0, 3, 4, 7, 10])),
            ),
        ];

        for (kinds, expected) in cases {
            let actual = topology_plan(kinds).mixed_topology().map(|topology| {
                let segments = topology
                    .internal_segments()
                    .iter()
                    .enumerate()
                    .map(|(ordinal, segment)| {
                        assert_eq!(segment.ordinal(), ordinal, "topology {kinds}");
                        segment.stages()
                    })
                    .collect::<Vec<_>>();
                (segments, topology.external_indices().to_vec())
            });
            assert_eq!(actual, expected, "topology {kinds}");
        }
    }
}
