//! The structured directory-listing boundary behind `ls`.
//!
//! A platform directory walk yields native entries; this layer turns each into
//! one record and nothing else. It performs no host access itself: the walk is
//! supplied as a closure, so the mapping is testable without a filesystem and
//! the same code drives the POSIX adapter and the future FlashOS one.
//!
//! Laziness is deliberate. One host entry is consumed per pulled record, so a
//! pipeline that stops early stops the walk with it and a large directory is
//! never materialized to answer `ls | first 1`.

use flashshell_platform::{DirectoryEntry, DirectoryEntryKind, DirectoryReadError};

use crate::value::{ByteSize, NativePath, Record, Value};

/// One step of listing a directory.
#[derive(Debug)]
pub enum ListStep {
    /// The next entry, as a record of `name`, `type`, and `size`.
    Entry(Value),
    /// The walk is exhausted; further steps stay `End`.
    End,
    /// The platform refused the walk or failed part-way through it. The
    /// executor attaches the command's source span at the pipeline boundary.
    Failed(DirectoryReadError),
}

/// A pull-driven directory listing produced by [`list`].
pub struct ListDirectory {
    entries: Box<dyn FnMut() -> Result<Option<DirectoryEntry>, DirectoryReadError>>,
    /// Latched once a terminal step was returned, so a failed walk is reported
    /// exactly once and the source is never advanced past it.
    done: bool,
}

/// Builds a listing that turns each entry yielded by `entries` into one record.
///
/// The source is the platform's walk, already narrowed to "advance by one";
/// exhaustion is `Ok(None)` and a host failure is reported as-is rather than
/// being flattened into an empty listing, so an unsupported platform can never
/// be mistaken for an empty directory.
pub fn list(
    entries: impl FnMut() -> Result<Option<DirectoryEntry>, DirectoryReadError> + 'static,
) -> ListDirectory {
    ListDirectory {
        entries: Box::new(entries),
        done: false,
    }
}

impl ListDirectory {
    /// Pulls the next entry record, exhaustion, or a walk failure.
    pub fn pull(&mut self) -> ListStep {
        if self.done {
            return ListStep::End;
        }
        match (self.entries)() {
            Ok(Some(entry)) => ListStep::Entry(entry_record(&entry)),
            Ok(None) => {
                self.done = true;
                ListStep::End
            }
            Err(error) => {
                self.done = true;
                ListStep::Failed(error)
            }
        }
    }
}

/// The observable record shape of one listed entry.
///
/// The field order is part of the contract, and there is deliberately no
/// timestamp field: the only ratified clock capability is a monotonic one, so a
/// wall-clock time would have to be invented rather than observed.
fn entry_record(entry: &DirectoryEntry) -> Value {
    let name = Value::Path(NativePath::new(entry.name()));
    let kind = Value::string(kind_name(entry.kind()));
    // Absent rather than zero: a directory or a link has no byte length the
    // shell measured, and zero would claim it did.
    let size = match entry.size() {
        Some(bytes) => Value::ByteSize(ByteSize::new(bytes)),
        None => Value::Null,
    };
    let record = Record::new(vec![
        ("name".to_owned(), name),
        ("type".to_owned(), kind),
        ("size".to_owned(), size),
    ])
    .expect("the listing field names are distinct");
    Value::Record(record)
}

const fn kind_name(kind: DirectoryEntryKind) -> &'static str {
    match kind {
        DirectoryEntryKind::File => "file",
        DirectoryEntryKind::Directory => "dir",
        DirectoryEntryKind::Symlink => "symlink",
        DirectoryEntryKind::Other => "other",
    }
}
