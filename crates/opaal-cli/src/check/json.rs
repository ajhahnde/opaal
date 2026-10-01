//! Versioned machine presentation of existing source-analysis findings.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use opaal_runtime::module::{ModuleAnalysisOutcome, ModuleProgramError};
use opaal_syntax::{Diagnostic, LabelStyle, Severity, SourceFile, SourceId, Span};
use serde_json::{Value, json};

use super::CheckRun;

// Ordering expresses the completed-report precedence, not diagnostic order.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Outcome {
    Complete,
    Invalid,
    Failed,
    Refused,
    Cancelled,
}

impl Outcome {
    const fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Invalid => "invalid",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::Cancelled => "cancelled",
        }
    }
}

type Sources<'a> = BTreeMap<SourceId, (&'a SourceFile, &'a Path)>;

pub(super) fn render(run: &CheckRun) -> Vec<u8> {
    let mut outcome = Outcome::Complete;
    let mut diagnostics = Vec::new();
    match run.analysis.as_ref() {
        Some(ModuleAnalysisOutcome::Complete(report)) => {
            let sources = report
                .sources()
                .iter()
                .map(|entry| (entry.source().id(), (entry.source(), entry.module().path())))
                .collect::<Sources<'_>>();
            for issue in report.issues() {
                let error = issue.error();
                if issue.severity() == Severity::Error {
                    outcome = outcome.max(error_outcome(error));
                }
                let findings = error.diagnostics();
                if findings.is_empty() {
                    diagnostics.push(unspanned(&run.requested, error));
                } else {
                    diagnostics
                        .extend(findings.iter().map(|finding| diagnostic(finding, &sources)));
                }
            }
        }
        Some(ModuleAnalysisOutcome::Cancelled) => {
            outcome = Outcome::Cancelled;
            diagnostics.push(adapter("CHECKJSON003", "source analysis cancelled", None));
        }
        Some(ModuleAnalysisOutcome::BudgetExceeded(exceeded)) => {
            outcome = Outcome::Refused;
            diagnostics.push(unspanned(
                &run.requested,
                &ModuleProgramError::BudgetExceeded(*exceeded),
            ));
        }
        None => {}
    }
    format!(
        "{{\"schema_version\":1,\"outcome\":\"{}\",\"diagnostics\":{}}}\n",
        outcome.name(),
        Value::Array(diagnostics)
    )
    .into_bytes()
}

fn error_outcome(error: &ModuleProgramError) -> Outcome {
    match error {
        ModuleProgramError::Resolution(_) | ModuleProgramError::SourceRead { .. } => {
            Outcome::Failed
        }
        ModuleProgramError::BudgetExceeded(_) | ModuleProgramError::SourceIdentityExhausted => {
            Outcome::Refused
        }
        ModuleProgramError::InvalidUtf8 { .. }
        | ModuleProgramError::Syntax { .. }
        | ModuleProgramError::Graph(_)
        | ModuleProgramError::Aliases(_)
        | ModuleProgramError::Names(_)
        | ModuleProgramError::Signatures(_)
        | ModuleProgramError::Actions(_)
        | ModuleProgramError::Pipelines(_)
        | ModuleProgramError::ProjectRequired { .. } => Outcome::Invalid,
    }
}

fn diagnostic(finding: &Diagnostic, sources: &Sources<'_>) -> Value {
    let primary = finding
        .labels()
        .iter()
        .position(|label| label.style() == LabelStyle::Primary);
    let (source, range) = primary.map_or((None, Value::Null), |index| {
        location(finding.labels()[index].span(), sources)
    });
    let related = finding
        .labels()
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != primary)
        .map(|(_, label)| {
            let (source, range) = location(label.span(), sources);
            json!({ "message": label.message(), "source": source, "range": range })
        })
        .collect::<Vec<_>>();
    json!({
        "code": finding.code(),
        "severity": finding.severity().to_string(),
        "message": finding.message(),
        "primary_message": primary.map(|index| finding.labels()[index].message()),
        "notes": finding.notes(),
        "source": source,
        "range": range,
        "related": related,
    })
}

fn location(span: Span, sources: &Sources<'_>) -> (Option<String>, Value) {
    let Some((source, path)) = sources.get(&span.source_id()) else {
        return (None, Value::Null);
    };
    let range = if source.span(span.start()..span.end()).is_ok() {
        json!({ "start": span.start(), "end": span.end() })
    } else {
        Value::Null
    };
    (Some(display_path(path)), range)
}

fn unspanned(requested: &Path, error: &ModuleProgramError) -> Value {
    let code = match error {
        ModuleProgramError::Resolution(_) => "MOD001",
        ModuleProgramError::SourceRead { .. } => "MOD003",
        ModuleProgramError::InvalidUtf8 { .. } => "MOD004",
        ModuleProgramError::SourceIdentityExhausted => "CHECKJSON002",
        ModuleProgramError::BudgetExceeded(_) => "CHECKJSON004",
        ModuleProgramError::Graph(_) => "CHECKJSON001",
        // Other variants always have shared diagnostics.
        _ => unreachable!("source-analysis error must supply shared diagnostics"),
    };
    let path = match error {
        ModuleProgramError::Resolution(error) => error.requested(),
        _ => error.module().map_or(requested, |module| module.path()),
    };
    adapter(code, &error.to_string(), Some(display_path(path)))
}

fn adapter(code: &str, message: &str, source: Option<String>) -> Value {
    json!({
        "code": code,
        "severity": "error",
        "message": message,
        "primary_message": null,
        "notes": [],
        "source": source,
        "range": null,
        "related": [],
    })
}

// Keep Unicode readable, escape controls/backslashes, and preserve each invalid
// native byte as \xNN. These are display identities, not filesystem operands.
fn display_path(path: &Path) -> String {
    let mut display = String::new();
    let mut remaining = path.as_os_str().as_bytes();
    while !remaining.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(remaining) {
            Ok(text) => (text, &remaining[remaining.len()..]),
            Err(error) => {
                let valid = std::str::from_utf8(&remaining[..error.valid_up_to()])
                    .expect("UTF-8 prefix is validated");
                let end = error.valid_up_to()
                    + error
                        .error_len()
                        .unwrap_or(remaining.len() - error.valid_up_to());
                (valid, &remaining[error.valid_up_to()..end])
            }
        };
        for character in valid.chars() {
            if character == '\\' || character.is_control() {
                display.extend(character.escape_default());
            } else {
                display.push(character);
            }
        }
        for byte in invalid {
            write!(display, "\\x{byte:02x}").expect("writing to a String cannot fail");
        }
        remaining = &remaining[valid.len() + invalid.len()..];
    }
    display
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    use opaal_runtime::module::{
        ModuleCanonicalizer, ModuleGraphError, ModulePathError, ModuleResolver,
    };

    use super::*;

    #[test]
    fn labels_notes_and_severities_survive_without_source_excerpts() {
        let first = SourceFile::new(SourceId::new(1), "first", "é🙂 abc");
        let second = SourceFile::new(SourceId::new(2), "second", "related");
        let sources = BTreeMap::from([
            (first.id(), (&first, Path::new("/project/first.opaal"))),
            (second.id(), (&second, Path::new("/project/second.opaal"))),
        ]);
        for severity in [Severity::Error, Severity::Warning, Severity::Note] {
            let finding = Diagnostic::new(severity, "TEST001", "quoted \"message\"\nwith\ttab")
                .with_secondary(second.span(0..2).unwrap(), "earlier secondary")
                .with_primary(first.span(2..6).unwrap(), "🙂 primary")
                .with_primary(second.span(2..7).unwrap(), "later primary")
                .with_secondary(first.span(7..10).unwrap(), "last secondary")
                .with_note("first\nnote")
                .with_note("second note");
            assert_eq!(
                diagnostic(&finding, &sources),
                json!({
                    "code": "TEST001", "severity": severity.to_string(),
                    "message": "quoted \"message\"\nwith\ttab", "primary_message": "🙂 primary",
                    "notes": ["first\nnote", "second note"],
                    "source": "/project/first.opaal", "range": {"start": 2, "end": 6},
                    "related": [
                        {"message": "earlier secondary", "source": "/project/second.opaal", "range": {"start": 0, "end": 2}},
                        {"message": "later primary", "source": "/project/second.opaal", "range": {"start": 2, "end": 7}},
                        {"message": "last secondary", "source": "/project/first.opaal", "range": {"start": 7, "end": 10}},
                    ],
                })
            );
        }
    }

    #[test]
    fn missing_primary_sources_and_invalid_spans_use_null_locations() {
        let source = SourceFile::new(SourceId::new(1), "long", "long source");
        let short = SourceFile::new(source.id(), "short", "é");
        let sources = BTreeMap::from([(source.id(), (&short, Path::new("short.opaal")))]);
        let finding = Diagnostic::new(Severity::Note, "TEST002", "note")
            .with_secondary(source.span(0..1).unwrap(), "invalid UTF-8 boundary")
            .with_primary(source.span(2..8).unwrap(), "out of bounds");
        let rendered = diagnostic(&finding, &sources);
        assert_eq!(rendered["source"], "short.opaal");
        assert!(rendered["range"].is_null());
        assert!(rendered["related"][0]["range"].is_null());
        let rendered = diagnostic(&finding, &BTreeMap::new());
        assert!(rendered["source"].is_null());
        assert!(rendered["range"].is_null());
        let no_primary = Diagnostic::new(Severity::Warning, "TEST003", "warning")
            .with_secondary(source.span(0..2).unwrap(), "context");
        let rendered = diagnostic(&no_primary, &sources);
        assert!(rendered["primary_message"].is_null());
        assert!(rendered["source"].is_null());
        assert!(rendered["range"].is_null());
        assert_eq!(rendered["related"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn graph_and_identity_adapter_codes_and_precedence_are_explicit() {
        struct Canonical;
        impl ModuleCanonicalizer for Canonical {
            fn canonicalize(
                &self,
                candidate: &Path,
            ) -> Result<std::path::PathBuf, ModulePathError> {
                Ok(candidate.to_path_buf())
            }
        }
        let module = ModuleResolver::new(&Canonical)
            .resolve_root(Path::new("/project/missing.opaal"))
            .unwrap();
        let graph = ModuleProgramError::Graph(ModuleGraphError::UnknownImporter(module));
        let requested = Path::new("root.opaal");
        for (error, code, outcome) in [
            (graph, "CHECKJSON001", Outcome::Invalid),
            (
                ModuleProgramError::SourceIdentityExhausted,
                "CHECKJSON002",
                Outcome::Refused,
            ),
        ] {
            let rendered = unspanned(requested, &error);
            assert_eq!(rendered["code"], code);
            assert_eq!(rendered["source"], "root.opaal");
            assert!(rendered["range"].is_null());
            assert!(error_outcome(&error) == outcome);
        }
        assert!(Outcome::Complete < Outcome::Invalid);
        assert!(Outcome::Invalid < Outcome::Failed);
        assert!(Outcome::Failed < Outcome::Refused);
    }

    #[test]
    fn native_path_display_does_not_collapse_invalid_bytes_or_backslashes() {
        let path = OsString::from_vec(b"/project/\xff\xfe-\\x01-\n\t-\".opaal".to_vec());
        assert_eq!(
            display_path(Path::new(&path)),
            "/project/\\xff\\xfe-\\\\x01-\\n\\t-\".opaal"
        );
        assert_eq!(
            display_path(Path::new("/project/é🙂.opaal")),
            "/project/é🙂.opaal"
        );
    }
}
