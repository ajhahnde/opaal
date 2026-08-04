use std::error::Error;
use std::fmt::{self, Write as _};
use std::io::{self, Write};

use flash_runtime::background::{JobNoticeKind, LiveJob, LiveJobState};
use flash_runtime::job::JobId;

use crate::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};

/// Control flow requested after evaluating one submitted edit buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvaluationControl {
    Continue,
    Exit(u8),
}

/// A recoverable diagnostic rendered by the interactive evaluator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractiveDiagnostic {
    rendered: String,
}

impl InteractiveDiagnostic {
    #[must_use]
    pub fn new(rendered: impl Into<String>) -> Self {
        Self {
            rendered: rendered.into(),
        }
    }

    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

/// Opaque identity of one interactive notice.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InteractiveNoticeId(u64);

impl InteractiveNoticeId {
    /// Build a nonzero identity supplied by the evaluator.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// The evaluator-local numeric identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One complete editor-owned notice awaiting successful presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractiveNotice {
    id: InteractiveNoticeId,
    rendered: String,
}

impl InteractiveNotice {
    /// Build one notice from its opaque identity and complete rendered text.
    #[must_use]
    pub fn new(id: InteractiveNoticeId, rendered: impl Into<String>) -> Self {
        Self {
            id,
            rendered: rendered.into(),
        }
    }

    /// The identity returned to the evaluator after a successful write.
    #[must_use]
    pub const fn id(&self) -> InteractiveNoticeId {
        self.id
    }

    /// Complete text written by the editor before the next prompt.
    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

/// Failure while acknowledging a successfully written interactive notice.
#[derive(Debug)]
pub struct InteractiveNoticeError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl InteractiveNoticeError {
    /// Build a notice acknowledgement failure without an underlying source.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Build a notice acknowledgement failure with an underlying source.
    pub fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for InteractiveNoticeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for InteractiveNoticeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Format one structured runtime job notice for prompt-boundary presentation.
#[must_use]
pub fn format_job_notice(job: JobId, kind: &JobNoticeKind, command: &str) -> String {
    match kind {
        JobNoticeKind::Started { group } => format!("[{}] {group}\n", job.get()),
        JobNoticeKind::Stopped => format!("[{}] Stopped  {command}\n", job.get()),
        JobNoticeKind::Completed => format!("[{}] Done     {command}\n", job.get()),
        JobNoticeKind::ObservationFailed { message } => {
            format!("[{}] Failed   {command}: {message}\n", job.get())
        }
    }
}

/// Render one refusal listing for the live jobs a session would abandon.
#[must_use]
pub fn format_live_jobs(jobs: &[LiveJob]) -> String {
    let mut rendered = format!(
        "fsh: {} live background job{}\n",
        jobs.len(),
        if jobs.len() == 1 { "" } else { "s" }
    );
    for live in jobs {
        let state = match live.state() {
            LiveJobState::Running => "Running",
            LiveJobState::Stopped => "Stopped",
        };
        let _ = writeln!(
            rendered,
            "[{}] {state}  {}",
            live.job().get(),
            live.command()
        );
    }
    rendered.push_str("fsh: exit again to hang up\n");
    rendered
}

/// Whether an interactive session may end, and what to show if it may not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitDecision {
    /// The session may end now.
    Permitted,
    /// The session may not end yet; the text is one complete shell-owned notice.
    Refused {
        /// One complete refusal notice, newline-terminated.
        rendered: String,
    },
}

/// Stateful evaluation boundary owned for the lifetime of an interactive session.
pub trait InteractiveEvaluator {
    /// Peek one notice to present before the next prompt.
    fn next_notice(&mut self) -> Option<InteractiveNotice> {
        None
    }

    /// Decide whether the session may end.
    ///
    /// Asked at every exit route. A refusal is presented through the editor, so
    /// the editor keeps sole ownership of terminal writes at this boundary.
    fn request_exit(&mut self) -> ExitDecision {
        ExitDecision::Permitted
    }

    /// Acknowledge one notice only after the editor has written it successfully.
    fn acknowledge_notice(
        &mut self,
        _notice: &InteractiveNotice,
    ) -> Result<(), InteractiveNoticeError> {
        Ok(())
    }

    fn evaluate(&mut self, source: &str) -> Result<EvaluationControl, InteractiveDiagnostic>;
}

/// Normal reason for leaving an interactive session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractiveExit {
    EndOfInput,
    Requested(u8),
}

/// Fatal failure that prevents an interactive session from continuing.
#[derive(Debug)]
pub enum InteractiveSessionError {
    Editor(EditorError),
    NoticeAcknowledgement(InteractiveNoticeError),
    DiagnosticOutput(io::Error),
    UnsupportedEditorEvent(&'static str),
}

impl fmt::Display for InteractiveSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Editor(error) => write!(formatter, "interactive editor failed: {error}"),
            Self::NoticeAcknowledgement(error) => {
                write!(
                    formatter,
                    "interactive notice acknowledgement failed: {error}"
                )
            }
            Self::DiagnosticOutput(error) => {
                write!(formatter, "interactive diagnostic output failed: {error}")
            }
            Self::UnsupportedEditorEvent(event) => {
                write!(
                    formatter,
                    "interactive editor event is not supported: {event}"
                )
            }
        }
    }
}

impl Error for InteractiveSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Editor(error) => Some(error),
            Self::NoticeAcknowledgement(error) => Some(error),
            Self::DiagnosticOutput(error) => Some(error),
            Self::UnsupportedEditorEvent(_) => None,
        }
    }
}

/// Write and acknowledge every notice the evaluator currently holds.
///
/// Run at the prompt boundary and again before a permitted exit, so a completion
/// observed while the editor owned the prompt is still shown to the user rather
/// than discarded with the session.
fn render_pending_notices(
    editor: &mut dyn LineEditor,
    evaluator: &mut dyn InteractiveEvaluator,
) -> Result<(), InteractiveSessionError> {
    while let Some(notice) = evaluator.next_notice() {
        editor
            .write_notice(notice.rendered())
            .map_err(InteractiveSessionError::Editor)?;
        evaluator
            .acknowledge_notice(&notice)
            .map_err(InteractiveSessionError::NoticeAcknowledgement)?;
    }
    Ok(())
}

/// Runs one synchronous interactive session with persistent editor and evaluator state.
pub fn run_interactive_session(
    editor: &mut dyn LineEditor,
    evaluator: &mut dyn InteractiveEvaluator,
    prompt: &EditorPrompt,
    diagnostic_output: &mut dyn Write,
) -> Result<InteractiveExit, InteractiveSessionError> {
    loop {
        render_pending_notices(editor, evaluator)?;

        let event = editor
            .read_line(prompt)
            .map_err(InteractiveSessionError::Editor)?;

        match event {
            EditorEvent::Submitted(source) => match evaluator.evaluate(&source) {
                Ok(EvaluationControl::Continue) => {}
                Ok(EvaluationControl::Exit(status)) => match evaluator.request_exit() {
                    ExitDecision::Permitted => {
                        render_pending_notices(editor, evaluator)?;
                        return Ok(InteractiveExit::Requested(status));
                    }
                    ExitDecision::Refused { rendered } => editor
                        .write_notice(&rendered)
                        .map_err(InteractiveSessionError::Editor)?,
                },
                Err(diagnostic) => diagnostic_output
                    .write_all(diagnostic.rendered().as_bytes())
                    .map_err(InteractiveSessionError::DiagnosticOutput)?,
            },
            EditorEvent::Cancelled => {}
            EditorEvent::EndOfInput => match evaluator.request_exit() {
                ExitDecision::Permitted => {
                    render_pending_notices(editor, evaluator)?;
                    return Ok(InteractiveExit::EndOfInput);
                }
                ExitDecision::Refused { rendered } => editor
                    .write_notice(&rendered)
                    .map_err(InteractiveSessionError::Editor)?,
            },
            EditorEvent::HostCommand(_) => {
                return Err(InteractiveSessionError::UnsupportedEditorEvent(
                    "host command",
                ));
            }
            EditorEvent::ExternalBreak(_) => {
                return Err(InteractiveSessionError::UnsupportedEditorEvent(
                    "external break",
                ));
            }
        }
    }
}
