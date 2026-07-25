//! Editor-neutral history autosuggestions with bounded synchronous work.

use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

/// Maximum edit-buffer size considered for an inline hint.
pub const MAX_HINT_BUFFER_BYTES: usize = 4_096;
/// Maximum suffix size rendered or accepted as one hint.
pub const MAX_HINT_SUFFIX_BYTES: usize = 4_096;
/// Maximum number of newest loaded history entries considered per request.
pub const MAX_HINT_HISTORY_ENTRIES: usize = 1_000;

/// Newest-first immutable source snapshot used by [`HintEngine`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HintCatalog {
    entries: Vec<String>,
}

impl HintCatalog {
    /// Collects at most the bounded number of newest source entries.
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            entries: entries
                .into_iter()
                .take(MAX_HINT_HISTORY_ENTRIES)
                .map(Into::into)
                .collect(),
        }
    }
}

/// One exact suffix that can be displayed and appended to the current buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hint {
    suffix: String,
}

impl Hint {
    /// The exact UTF-8 source suffix, without presentation escapes.
    #[must_use]
    pub fn suffix(&self) -> &str {
        &self.suffix
    }
}

/// Pure, append-only autosuggestions over one loaded history snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct HintEngine;

impl HintEngine {
    /// Constructs the stateless hint engine.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Returns the newest exact-prefix history suffix for an end cursor.
    #[must_use]
    pub fn hint(&self, source: &str, cursor: usize, catalog: &HintCatalog) -> Option<Hint> {
        if source.is_empty()
            || source.len() > MAX_HINT_BUFFER_BYTES
            || cursor != source.len()
            || !source.is_char_boundary(cursor)
        {
            return None;
        }

        let source_file = SourceFile::new(SourceId::new(0), "<interactive>", source);
        if matches!(parse(&source_file), ParseOutcome::Invalid(_)) {
            return None;
        }

        catalog.entries.iter().find_map(|entry| {
            let suffix = entry.strip_prefix(source)?;
            if suffix.is_empty() || suffix.len() > MAX_HINT_SUFFIX_BYTES {
                return None;
            }
            Some(Hint {
                suffix: suffix.to_owned(),
            })
        })
    }
}
