//! Source-independent documentation metadata shared by analysis and help.

use flash_syntax::{DocumentationBlock, SourceFile};

/// Source-independent normalized documentation prose.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Documentation {
    text: String,
}

impl Documentation {
    /// Creates documentation from already-normalized prose.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    pub(crate) fn from_block(source: &SourceFile, block: &DocumentationBlock) -> Self {
        let mut lines = block
            .lines
            .iter()
            .map(|span| {
                let marked = source
                    .slice(*span)
                    .expect("documentation spans belong to their parsed source")
                    .strip_prefix("##")
                    .expect("documentation tokens start with the documentation marker");
                marked.strip_prefix(' ').unwrap_or(marked).to_owned()
            })
            .collect::<Vec<_>>();
        while lines.first().is_some_and(String::is_empty) {
            lines.remove(0);
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Self::new(lines.join("\n"))
    }

    /// Complete normalized prose, with source lines joined by `\n`.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The first nonempty normalized line.
    #[must_use]
    pub fn summary(&self) -> &str {
        self.text
            .lines()
            .find(|line| !line.is_empty())
            .unwrap_or("")
    }

    /// Whether the normalized prose contains no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Required user-facing metadata attached to one internal command signature.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandDocumentation {
    invocation: String,
    documentation: Documentation,
}

impl CommandDocumentation {
    /// Creates a command's stable invocation spelling and prose.
    #[must_use]
    pub fn new(invocation: impl Into<String>, documentation: Documentation) -> Self {
        Self {
            invocation: invocation.into(),
            documentation,
        }
    }

    /// The stable human invocation spelling.
    #[must_use]
    pub fn invocation(&self) -> &str {
        &self.invocation
    }

    /// The command's normalized prose.
    #[must_use]
    pub const fn documentation(&self) -> &Documentation {
        &self.documentation
    }
}
