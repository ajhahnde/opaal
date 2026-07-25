use std::error::Error;
use std::fmt;

pub const DEFAULT_PRIMARY_PROMPT: &str = "fsh> ";
pub const DEFAULT_CONTINUATION_PROMPT: &str = "...> ";

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

/// Synchronous input boundary consumed by an interactive FlashShell session.
pub trait LineEditor {
    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError>;
}
