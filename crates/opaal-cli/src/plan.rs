//! Host-free OPAAL planning-boundary inspection.

use std::path::Path;

use opaal_runtime::module::{ModuleCanonicalizer, ModuleProgramLoader, ModuleSourceLoader};
use opaal_runtime::outcome::{Refusal, RefusalReason};
use opaal_syntax::{Diagnostic, Severity, render_diagnostic, render_diagnostic_sources};

pub trait PlanFilesystem: ModuleCanonicalizer + ModuleSourceLoader {}

impl<T> PlanFilesystem for T where T: ModuleCanonicalizer + ModuleSourceLoader {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlanRun {
    rendered_issues: Vec<String>,
    refusal: Option<Refusal>,
}

impl PlanRun {
    #[must_use]
    pub fn rendered_issues(&self) -> &[String] {
        &self.rendered_issues
    }

    #[must_use]
    pub const fn is_success(&self) -> bool {
        false
    }

    #[must_use]
    pub const fn refusal(&self) -> Option<Refusal> {
        self.refusal
    }
}

/// Validates an explicit OPAAL source graph and returns the delivered
/// host-free planning refusal. No ambient host capability is accepted.
#[must_use]
pub fn inspect_source(source_path: &Path, filesystem: &impl PlanFilesystem) -> PlanRun {
    let report = ModuleProgramLoader::new(filesystem, filesystem).analyze(source_path);
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
                    vec![format!(
                        "opaal plan: {}: {}\n",
                        source_path.display(),
                        issue.error()
                    )]
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
    let span = source
        .span(0..0)
        .expect("the beginning of a source is a valid span");
    let refusal = Refusal::new(RefusalReason::Unsupported, "OPAAL execution planning", span);
    let diagnostic = Diagnostic::new(
        Severity::Error,
        "PLAN004",
        "OPAAL execution planning requires a future authority and controlled-planning contract",
    )
    .with_primary(span, "planning was refused before ambient host observation");
    PlanRun {
        rendered_issues: vec![
            render_diagnostic(source, &diagnostic)
                .expect("the planning refusal belongs to the root source"),
        ],
        refusal: Some(refusal),
    }
}
