use std::error::Error;
use std::fmt;

use crate::completion::CompletionCatalog;

pub const DEFAULT_PRIMARY_PROMPT: &str = ">> ";
pub const DEFAULT_CONTINUATION_PROMPT: &str = "...> ";
pub const SAFE_PRIMARY_PROMPT: &str = "[SAFE] >> ";

/// Text rendered around one interactive edit buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorPrompt {
    primary: String,
    continuation: String,
}

impl Default for EditorPrompt {
    fn default() -> Self {
        Self::new(DEFAULT_PRIMARY_PROMPT, DEFAULT_CONTINUATION_PROMPT)
    }
}

impl EditorPrompt {
    #[must_use]
    pub fn new(primary: impl Into<String>, continuation: impl Into<String>) -> Self {
        Self {
            primary: primary.into(),
            continuation: continuation.into(),
        }
    }

    #[must_use]
    pub fn primary(&self) -> &str {
        &self.primary
    }

    #[must_use]
    pub fn continuation(&self) -> &str {
        &self.continuation
    }

    /// The fixed prompt used after startup configuration enters safe mode.
    #[must_use]
    pub fn safe_mode() -> Self {
        Self::new(SAFE_PRIMARY_PROMPT, DEFAULT_CONTINUATION_PROMPT)
    }
}

/// One editor result, independent of the terminal-editing implementation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EditorEvent {
    /// A complete input buffer ready for evaluation.
    Submitted(String),
    /// The current edit was cancelled and the session should re-prompt.
    Cancelled,
    /// End-of-input received while the edit buffer was empty.
    EndOfInput,
    /// A request delegated to a future host integration.
    HostCommand(String),
    /// An external interruption delegated to a future host integration.
    ExternalBreak(String),
}

/// One editor-owned external print that may arrive during an active read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorExternalPrint {
    id: u64,
    rendered: String,
}

impl EditorExternalPrint {
    #[must_use]
    pub fn new(id: u64, rendered: impl Into<String>) -> Self {
        Self {
            id,
            rendered: rendered.into(),
        }
    }

    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn rendered(&self) -> &str {
        &self.rendered
    }
}

/// Pull boundary used by an editor while it owns the active input buffer.
pub trait EditorEventSource {
    fn next_external_print(&mut self) -> Option<EditorExternalPrint> {
        None
    }

    fn acknowledge_external_print(
        &mut self,
        _event: &EditorExternalPrint,
    ) -> Result<(), EditorError> {
        Ok(())
    }
}

/// Event source for editors used without an interactive evaluator.
pub struct NoEditorEvents;

impl EditorEventSource for NoEditorEvents {}

/// Failure reported by the selected terminal editor.
#[derive(Debug)]
pub struct EditorError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl EditorError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for EditorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EditorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Synchronous input boundary consumed by an interactive Flash session.
pub trait LineEditor {
    /// Write and flush one complete shell-owned notice before drawing a prompt.
    fn write_notice(&mut self, rendered: &str) -> Result<(), EditorError>;

    /// Replaces immutable completion candidates before the next edit begins.
    fn set_completion_catalog(&mut self, _catalog: CompletionCatalog) {}

    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError>;

    /// Read while allowing the editor to present asynchronous external output.
    fn read_line_with_events(
        &mut self,
        prompt: &EditorPrompt,
        _events: &mut dyn EditorEventSource,
    ) -> Result<EditorEvent, EditorError> {
        self.read_line(prompt)
    }
}
