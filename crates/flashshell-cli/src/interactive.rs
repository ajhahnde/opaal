use std::error::Error;
use std::fmt;
use std::io::{self, Write};

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

/// Stateful evaluation boundary owned for the lifetime of an interactive session.
pub trait InteractiveEvaluator {
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
    DiagnosticOutput(io::Error),
    UnsupportedEditorEvent(&'static str),
}

impl fmt::Display for InteractiveSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Editor(error) => write!(formatter, "interactive editor failed: {error}"),
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
            Self::DiagnosticOutput(error) => Some(error),
            Self::UnsupportedEditorEvent(_) => None,
        }
    }
}

/// Runs one synchronous interactive session with persistent editor and evaluator state.
pub fn run_interactive_session(
    editor: &mut dyn LineEditor,
    evaluator: &mut dyn InteractiveEvaluator,
    prompt: &EditorPrompt,
    diagnostic_output: &mut dyn Write,
) -> Result<InteractiveExit, InteractiveSessionError> {
    loop {
        let event = editor
            .read_line(prompt)
            .map_err(InteractiveSessionError::Editor)?;

        match event {
            EditorEvent::Submitted(source) => match evaluator.evaluate(&source) {
                Ok(EvaluationControl::Continue) => {}
                Ok(EvaluationControl::Exit(status)) => {
                    return Ok(InteractiveExit::Requested(status));
                }
                Err(diagnostic) => diagnostic_output
                    .write_all(diagnostic.rendered().as_bytes())
                    .map_err(InteractiveSessionError::DiagnosticOutput)?,
            },
            EditorEvent::Cancelled => {}
            EditorEvent::EndOfInput => return Ok(InteractiveExit::EndOfInput),
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
