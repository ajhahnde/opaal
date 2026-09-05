//! Capability-bounded orchestration for execution-plan inspection.

use std::path::{Path, PathBuf};

use flash_runtime::module::{
    AnalysisLimitKind, AnalysisLimits, ModuleCanonicalizer, ModuleId, ModulePathError,
    ModuleProgramError, ModuleProgramLoader, ModuleResolver, ModuleSourceError, ModuleSourceLoader,
};
use flash_runtime::outcome::{Refusal, RefusalReason};
use flash_runtime::plan::{SessionOptions, plan_pipeline_with_options, preflight};
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::{Environment, ScopeStack};
use flash_syntax::{
    Diagnostic, LanguageDetection, LanguageMajor, Pipeline, Severity, SourceFile, SourceId,
    StageKind, StatementKind, detect_source_language, render_diagnostic, render_diagnostic_sources,
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

/// Host observations available only to the frozen Flash 1 planner.
///
/// Flash 2 is refused before either method or [`ExecutableProbe`] is called.
/// Resolving an explicitly supplied relative source path remains an input read,
/// not a planner cwd snapshot.
pub trait PlanHost: ExecutableProbe {
    /// Capture the launcher's current working directory.
    fn current_dir(&self) -> Result<PathBuf, String>;

    /// Capture the launcher's inherited child environment.
    fn environment(&self) -> Environment;
}

/// A completed plan inspection with either one plan or ordered diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanRun {
    rendered_plan: Option<String>,
    rendered_issues: Vec<String>,
    refusal: Option<Refusal>,
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

    /// Structured Flash 2 refusal, when planning was rejected by generation.
    #[must_use]
    pub const fn refusal(&self) -> Option<Refusal> {
        self.refusal
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
    inspect_source_with(request.source(), filesystem, probe, || {
        Ok((request.cwd().to_path_buf(), request.environment().clone()))
    })
}

/// Inspect a source while deferring every ambient host observation.
///
/// The source and its static imports are the only inputs read before the
/// language generation is known. Flash 2 returns a structured refusal without
/// capturing planner cwd, environment, or executable metadata. Flash 1 retains
/// its exact planner behavior by taking one immutable host snapshot after
/// analysis.
#[must_use]
pub fn inspect_host_source<F, H>(source: &Path, filesystem: &F, host: &H) -> PlanRun
where
    F: PlanFilesystem,
    H: PlanHost,
{
    inspect_source_with(source, filesystem, host, || {
        let cwd = host.current_dir()?;
        Ok((cwd, host.environment()))
    })
}

fn inspect_source_with<F, S>(
    source_path: &Path,
    filesystem: &F,
    probe: &dyn ExecutableProbe,
    snapshot: S,
) -> PlanRun
where
    F: PlanFilesystem,
    S: FnOnce() -> Result<(PathBuf, Environment), String>,
{
    let detected = match detect_plan_language(source_path, filesystem) {
        Ok(detected) => detected,
        Err(rendered_issue) => {
            return PlanRun {
                rendered_plan: None,
                rendered_issues: vec![rendered_issue],
                refusal: None,
            };
        }
    };
    if let Some(detected) = detected.flash_2 {
        return flash_2_refusal(&detected.source, detected.directive_span);
    }

    let commands = flash_runtime::builtin::standard_registry();
    let report = if let Some(root) = detected.root.as_ref() {
        let cached = CachedPlanFilesystem {
            inner: filesystem,
            root,
        };
        ModuleProgramLoader::for_language(&cached, &cached, detected.language)
            .analyze_with_commands(source_path, &commands)
    } else {
        ModuleProgramLoader::for_language(filesystem, filesystem, detected.language)
            .analyze_with_commands(source_path, &commands)
    };
    let sources = report
        .sources()
        .iter()
        .map(|entry| entry.source())
        .collect::<Vec<_>>();

    if let Some(source) = sources.first()
        && let LanguageDetection::Complete(directive) = detect_source_language(source)
        && directive.major() == LanguageMajor::V2
    {
        return flash_2_refusal(source, directive.span());
    }

    if report.has_errors() {
        let rendered_issues = report
            .issues()
            .iter()
            .filter(|issue| issue.severity() == Severity::Error)
            .flat_map(|issue| {
                let diagnostics = issue.error().diagnostics();
                if diagnostics.is_empty() {
                    vec![render_unspanned(source_path, issue.error())]
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
            refusal: None,
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
                refusal: None,
            };
        }
    };

    let (cwd, environment) = match snapshot() {
        Ok(snapshot) => snapshot,
        Err(cause) => {
            return PlanRun {
                rendered_plan: None,
                rendered_issues: vec![format!("fsh: cannot read the current directory: {cause}\n")],
                refusal: None,
            };
        }
    };

    let mut scope = ScopeStack::new();
    let plan = match plan_pipeline_with_options(
        pipeline,
        &cwd,
        source,
        &mut scope,
        &environment,
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
                refusal: None,
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
            refusal: None,
        };
    }

    PlanRun {
        rendered_plan: Some(plan.render()),
        rendered_issues: Vec::new(),
        refusal: None,
    }
}

struct DetectedPlanLanguage {
    language: LanguageMajor,
    flash_2: Option<DetectedFlash2Source>,
    root: Option<CachedPlanRoot>,
}

struct DetectedFlash2Source {
    source: SourceFile,
    directive_span: flash_syntax::Span,
}

struct CachedPlanRoot {
    requested: PathBuf,
    canonical: PathBuf,
    bytes: Vec<u8>,
}

struct CachedPlanFilesystem<'a, F> {
    inner: &'a F,
    root: &'a CachedPlanRoot,
}

impl<F> ModuleCanonicalizer for CachedPlanFilesystem<'_, F>
where
    F: PlanFilesystem,
{
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        if candidate == self.root.requested {
            Ok(self.root.canonical.clone())
        } else {
            self.inner.canonicalize(candidate)
        }
    }
}

impl<F> ModuleSourceLoader for CachedPlanFilesystem<'_, F>
where
    F: PlanFilesystem,
{
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        if module.path() == self.root.canonical {
            Ok(self.root.bytes.clone())
        } else {
            self.inner.load(module)
        }
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        if module.path() == self.root.canonical {
            Ok(self.root.bytes[..self.root.bytes.len().min(maximum)].to_vec())
        } else {
            self.inner.load_bounded(module, maximum)
        }
    }
}

fn detect_plan_language<F>(
    source_path: &Path,
    filesystem: &F,
) -> Result<DetectedPlanLanguage, String>
where
    F: PlanFilesystem,
{
    let module = ModuleResolver::new(filesystem)
        .resolve_root(source_path)
        .map_err(|error| {
            format!(
                "fsh plan: {}: {}\n",
                error.requested().display(),
                error.cause()
            )
        })?;
    let detection_limit = AnalysisLimits::V2
        .limit(AnalysisLimitKind::SourceBytes)
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(usize::MAX);
    let bytes = filesystem
        .load_bounded(&module, detection_limit.saturating_add(1))
        .map_err(|error| format!("fsh plan: {}: {error}\n", module.path().display()))?;
    let root = (bytes.len() <= detection_limit).then(|| CachedPlanRoot {
        requested: source_path.to_path_buf(),
        canonical: module.path().to_path_buf(),
        bytes: bytes.clone(),
    });
    let source = SourceFile::from_bytes(
        SourceId::new(u32::MAX),
        module.path().to_string_lossy(),
        bytes,
    );
    let Some(source) = source.ok() else {
        return Ok(DetectedPlanLanguage {
            language: LanguageMajor::V1,
            flash_2: None,
            root,
        });
    };
    let (language, flash_2) = match detect_source_language(&source) {
        LanguageDetection::Complete(directive) if directive.major() == LanguageMajor::V2 => (
            LanguageMajor::V2,
            Some(DetectedFlash2Source {
                source,
                directive_span: directive.span(),
            }),
        ),
        LanguageDetection::Complete(_) => (LanguageMajor::V1, None),
        LanguageDetection::Invalid(diagnostics)
            if diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() != "FS2001") =>
        {
            (LanguageMajor::V2, None)
        }
        LanguageDetection::Invalid(_) => (LanguageMajor::V1, None),
    };
    Ok(DetectedPlanLanguage {
        language,
        flash_2,
        root,
    })
}

fn flash_2_refusal(source: &SourceFile, span: flash_syntax::Span) -> PlanRun {
    let refusal = Refusal::new(
        RefusalReason::Unsupported,
        "Flash 2 execution planning",
        span,
    );
    let diagnostic = Diagnostic::new(
        Severity::Error,
        "PLAN004",
        "Flash 2 execution planning requires explicit authority and controlled-planning contracts",
    )
    .with_primary(
        span,
        "planning was refused before the planner captured cwd, environment, or executable state",
    );
    PlanRun {
        rendered_plan: None,
        rendered_issues: vec![
            render_diagnostic(source, &diagnostic)
                .expect("the planning refusal belongs to the detected root source"),
        ],
        refusal: Some(refusal),
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
