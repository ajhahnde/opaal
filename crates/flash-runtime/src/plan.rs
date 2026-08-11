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
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flash_syntax::{
    CommandItemKind, CommandStage, FileRedirection, IoNumber, OutputMode, PipeOperator, Pipeline,
    RedirectionKind, SourceFile, Span, StageKind, Word, WordPartKind,
};

use crate::carrier::{PipelineCarrierFault, StageCarrierContract, analyze_pipeline_carriers};
use crate::command::{Carrier, CommandOutput, CommandRegistry};
use crate::eval::{
    ExpandedWord, RuntimeError, RuntimeErrorKind, evaluate_closure_argument_with_binding_types,
    expand_spread, expand_word,
};
use crate::help::{HelpCatalog, HelpSnapshot};
use crate::module::RuntimeBindingTypes;
use crate::resolve::{ExecutableProbe, Resolution, ResolutionError, resolve_command};
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
    span: Span,
}

impl ExecutionPlan {
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
            span,
        }
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
            let values = stage
                .arguments
                .iter()
                .filter_map(PlannedArgument::as_value)
                .collect::<Vec<_>>();
            if !values.is_empty() {
                out.push_str("  values");
                for value in values {
                    let _ = write!(out, " [{value:?}]");
                }
                out.push('\n');
            }
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
    let head = expand_word(command.head.word(), context.source, scope)?;
    let force_external = command.head.kind() == flash_syntax::CommandHeadKind::ForcedExternal;
    let (resolution, input_carriers, output_carrier) =
        resolve(head.value(), force_external, command.head.span(), context)?;
    if matches!(&resolution, PlannedResolution::Internal { name } if name == "help") {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinArgument {
                command: "help",
                message: "the help command head must be the static source word `help`".to_owned(),
            },
            command.head.span(),
        ));
    }

    let mut argv = vec![head];
    let mut arguments = Vec::new();
    let mut redirections = Vec::new();
    for item in &command.items {
        match item.kind() {
            CommandItemKind::Word(word) => {
                let word = expand_word(word, context.source, scope)?;
                argv.push(word.clone());
                arguments.push(PlannedArgument::Word(word));
            }
            CommandItemKind::Spread(variable) => {
                let words = expand_spread(variable, item.span(), context.source, scope)?;
                argv.extend(words.iter().cloned());
                arguments.extend(words.into_iter().map(PlannedArgument::Word));
            }
            CommandItemKind::Closure(closure) => {
                if matches!(resolution, PlannedResolution::External { .. }) {
                    return Err(RuntimeError::new(
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
        arguments,
        redirections,
        help: None,
        span,
    })
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
    let head = expand_word(command.head.word(), context.source, &mut head_scope)?;
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
                let expanded = expand_word(word, context.source, &mut query_scope)?;
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
                let action =
                    plan_redirection(redirection.kind(), context.source, &mut redirection_scope)?;
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
            name: signature.name().to_owned(),
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
    check_carriers(plan)?;
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
