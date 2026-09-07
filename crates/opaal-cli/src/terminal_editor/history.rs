//! In-session command recall.
//!
//! Storage is injected separately; this ring owns deterministic recall and the
//! immutable snapshot used for portable inline hints.

use std::collections::VecDeque;

use crate::editor::EditorError;

/// Persistent storage consumed by the portable editor without exposing a
/// frontend-specific history implementation.
pub trait HistoryPersistence {
    /// Load the retained oldest-to-newest submission snapshot.
    fn entries(&mut self) -> Result<Vec<String>, EditorError>;

    /// Merge and durably record one accepted submission.
    fn record(&mut self, source: &str) -> Result<(), EditorError>;
}

/// Recall over the submissions of one session.
#[derive(Debug)]
pub struct HistoryRing {
    entries: VecDeque<String>,
    capacity: usize,
    /// How many entries back the cursor currently sits; zero means "at the draft".
    position: usize,
    /// The in-progress line parked when recall started.
    draft: String,
}

impl HistoryRing {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            position: 0,
            draft: String::new(),
        }
    }

    /// Store one submission, dropping empties and adjacent exact duplicates.
    pub fn record(&mut self, entry: &str) {
        self.position = 0;
        self.draft.clear();
        if entry.is_empty() {
            return;
        }
        if self.entries.back().is_some_and(|newest| newest == entry) {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry.to_owned());
    }

    /// Step one entry towards the oldest, parking `draft` on the first step.
    ///
    /// Only the first step parks anything, so edits made to a recalled entry
    /// are dropped when recall continues past it. That is deliberate: the
    /// stored history stays exactly what was submitted, and only the line the
    /// operator was actually writing survives the walk.
    pub fn recall_previous(&mut self, draft: &str) -> Option<String> {
        if self.position >= self.entries.len() {
            return None;
        }
        if self.position == 0 {
            self.draft = draft.to_owned();
        }
        self.position += 1;
        self.entry_at_position()
    }

    /// Step one entry towards the newest, returning the parked draft last.
    pub fn recall_next(&mut self) -> Option<String> {
        if self.position == 0 {
            return None;
        }
        self.position -= 1;
        if self.position == 0 {
            return Some(std::mem::take(&mut self.draft));
        }
        self.entry_at_position()
    }

    /// Abandon any in-progress recall without touching the stored entries.
    pub fn reset_position(&mut self) {
        self.position = 0;
        self.draft.clear();
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retained entries from newest to oldest for bounded hint matching.
    pub fn newest_entries(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().rev().map(String::as_str)
    }

    /// Load an oldest-to-newest persistent snapshot through normal retention
    /// and duplicate rules.
    pub fn load(&mut self, entries: impl IntoIterator<Item = impl AsRef<str>>) {
        self.entries.clear();
        for entry in entries {
            self.record(entry.as_ref());
        }
        self.reset_position();
    }

    fn entry_at_position(&self) -> Option<String> {
        let index = self.entries.len().checked_sub(self.position)?;
        self.entries.get(index).cloned()
    }
}
