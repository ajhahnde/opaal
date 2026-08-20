//! Capability-bounded orchestration for execution-plan inspection.

use std::path::{Path, PathBuf};

use flash_runtime::module::{
    ModuleCanonicalizer, ModuleProgramError, ModuleProgramLoader, ModuleSourceLoader,
};
use flash_runtime::plan::{SessionOptions, plan_pipeline_with_options, preflight};
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::{Environment, ScopeStack};
use flash_syntax::{
    Diagnostic, Pipeline, Severity, StageKind, StatementKind, render_diagnostic,
    render_diagnostic_sources,
};

/// One explicit execution-plan inspection request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanRequest {
    source: PathBuf,
    cwd: PathBuf,
    environment: Environment,
}

impl PlanRequest {
    /// Creates a request over one source and immutable launch snapshot.
    #[must_use]
    pub const fn new(source: PathBuf, cwd: PathBuf, environment: Environment) -> Self {
        Self {
            source,
            cwd,
            environment,
        }
    }

    /// The native root path supplied by the caller.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// The working directory captured for the plan.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The child-environment snapshot captured for the plan.
    #[must_use]
    pub const fn environment(&self) -> &Environment {
        &self.environment
    }
}

/// Source capabilities available to plan inspection.
///
/// Runtime platforms, sessions, terminals, process creation, and writable file
/// operations are deliberately absent. The separate executable probe performs
/// only read-only command-resolution checks.
pub trait PlanFilesystem: ModuleCanonicalizer + ModuleSourceLoader {}

impl<T> PlanFilesystem for T where T: ModuleCanonicalizer + ModuleSourceLoader {}

/// A completed plan inspection with either one plan or ordered diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanRun {
    rendered_plan: Option<String>,
    rendered_issues: Vec<String>,
}

impl PlanRun {
    /// The complete deterministic plan, present only after successful inspection.
    #[must_use]
    pub fn rendered_plan(&self) -> Option<&str> {
        self.rendered_plan.as_deref()
    }

    /// Ordered source-backed or path-qualified diagnostics.
    #[must_use]
    pub fn rendered_issues(&self) -> &[String] {
        &self.rendered_issues
    }

    /// Whether inspection produced one structurally valid plan.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.rendered_plan.is_some()
    }
}

/// Parses, analyzes, plans, preflights, and renders one exact command pipeline.
///
/// This function has no execution-capable dependency. Planning receives a new
/// empty lexical scope, the injected immutable launch snapshot, the standard
/// command registry, default session options, and a read-only executable probe.
#[must_use]
pub fn inspect_source<F>(
    request: &PlanRequest,
    filesystem: &F,
    probe: &dyn ExecutableProbe,
) -> PlanRun
where
    F: PlanFilesystem,
{
    let commands = flash_runtime::builtin::standard_registry();
    let report = ModuleProgramLoader::new(filesystem, filesystem)
        .analyze_with_commands(request.source(), &commands);
    let sources = report
        .sources()
        .iter()
        .map(|entry| entry.source())
        .collect::<Vec<_>>();

    if report.has_errors() {
        let rendered_issues = report
            .issues()
            .iter()
            .filter(|issue| issue.severity() == Severity::Error)
            .flat_map(|issue| {
                let diagnostics = issue.error().diagnostics();
                if diagnostics.is_empty() {
                    vec![render_unspanned(request.source(), issue.error())]
                } else {
                    diagnostics
                        .iter()
                        .map(|diagnostic| {
                            render_diagnostic_sources(sources.iter().copied(), diagnostic)
                                .expect("module diagnostics address retained plan sources")
                        })
                        .collect()
                }
            })
            .collect();
        return PlanRun {
            rendered_plan: None,
            rendered_issues,
        };
    }

    let program = report
        .program()
        .expect("an error-free module analysis exposes a program");
    let root = program.graph().root();
    let source = program
        .sources()
        .source(root)
        .expect("the complete program retains its root source");
    let script = program
        .sources()
        .script(root)
        .expect("the complete program retains its parsed root");
    let pipeline = match exact_pipeline(script) {
        Ok(pipeline) => pipeline,
        Err(diagnostic) => {
            return PlanRun {
                rendered_plan: None,
                rendered_issues: vec![
                    render_diagnostic(source, &diagnostic)
                        .expect("the plan-shape diagnostic belongs to the root source"),
                ],
            };
        }
    };

    let mut scope = ScopeStack::new();
    let plan = match plan_pipeline_with_options(
        pipeline,
        request.cwd(),
        source,
        &mut scope,
        request.environment(),
        &commands,
        probe,
        &SessionOptions::default(),
    ) {
        Ok(plan) => plan,
        Err(error) => {
            let diagnostic = Diagnostic::new(Severity::Error, "PLAN002", error.to_string())
                .with_primary(
                    error.span(),
                    "this pipeline cannot be inspected without execution",
                );
            return PlanRun {
                rendered_plan: None,
                rendered_issues: vec![
                    render_diagnostic(source, &diagnostic)
                        .expect("the planning diagnostic belongs to the root source"),
                ],
            };
        }
    };

    if let Err(error) = preflight(&plan) {
        let diagnostic = Diagnostic::new(Severity::Error, "PLAN003", error.to_string())
            .with_primary(
                error.span(),
                "this execution plan is not structurally valid",
            );
        return PlanRun {
            rendered_plan: None,
            rendered_issues: vec![
                render_diagnostic(source, &diagnostic)
                    .expect("the preflight diagnostic belongs to the root source"),
            ],
        };
    }

    PlanRun {
        rendered_plan: Some(plan.render()),
        rendered_issues: Vec::new(),
    }
}

fn exact_pipeline(script: &flash_syntax::Script) -> Result<&Pipeline, Diagnostic> {
    let [statement] = script.statements() else {
        return Err(shape_diagnostic(
            script.span(),
            "the source must contain exactly one statement",
        ));
    };
    let StatementKind::Job(job) = statement.kind() else {
        return Err(shape_diagnostic(
            statement.span(),
            "this statement is not a command pipeline",
        ));
    };
    if let Some(span) = job.background_span {
        return Err(shape_diagnostic(
            span,
            "background execution cannot be inspected as one foreground plan",
        ));
    }
    let [and_chain] = job.chain.or_terms() else {
        unreachable!("a parsed conditional chain has at least one term");
    };
    if !job.chain.operators().is_empty() || !and_chain.operators().is_empty() {
        return Err(shape_diagnostic(
            job.chain.span(),
            "conditional chains depend on execution status",
        ));
    }
    let [pipeline] = and_chain.and_terms() else {
        unreachable!("a parsed and-chain has at least one pipeline");
    };
    if let Some(stage) = pipeline
        .stages()
        .iter()
        .find(|stage| !matches!(stage.kind(), StageKind::Command(_)))
    {
        return Err(shape_diagnostic(
            stage.span(),
            "this stage is not an ordinary command stage",
        ));
    }
    Ok(pipeline)
}

fn shape_diagnostic(span: flash_syntax::Span, label: &'static str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        "PLAN001",
        "plan inspection requires exactly one foreground command pipeline",
    )
    .with_primary(span, label)
}

fn render_unspanned(requested: &Path, error: &ModuleProgramError) -> String {
    let (path, cause) = match error {
        ModuleProgramError::Resolution(error) => (error.requested(), error.cause().to_string()),
        ModuleProgramError::SourceRead { module, cause, .. } => (module.path(), cause.to_string()),
        ModuleProgramError::InvalidUtf8 {
            module,
            valid_up_to,
            ..
        } => (
            module.path(),
            format!("invalid UTF-8 at byte {valid_up_to}"),
        ),
        _ => (
            error.module().map_or(requested, |module| module.path()),
            error.to_string(),
        ),
    };
    format!("fsh plan: {}: {cause}\n", path.display())
}
