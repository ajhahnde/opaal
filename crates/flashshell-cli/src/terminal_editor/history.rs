//! In-session command recall.
//!
//! History lives only as long as the session: persistent history is a host
//! feature with its own on-disk permission rules and is not part of this
//! editor.

use std::collections::VecDeque;

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

    fn entry_at_position(&self) -> Option<String> {
        let index = self.entries.len().checked_sub(self.position)?;
        self.entries.get(index).cloned()
    }
}
