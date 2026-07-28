#![forbid(unsafe_code)]

//! Acceptance coverage for structured `ls` — the crossing from a platform
//! directory walk to a stream of records.
//!
//! Like the format and codec boundaries, the mapping layer is host-free: it is
//! driven by a closure yielding platform entries, so every shape below is
//! asserted without a filesystem. The walk stays lazy, one host entry per
//! pulled record, so a pipeline that stops early stops the walk with it.
//!
//! The record carries `name`, `type`, and `size` and deliberately carries no
//! timestamp: the only ratified clock capability is a monotonic one, and a
//! wall-clock field would have to be invented rather than observed.

use flashshell_platform::{
    Capability, DirectoryEntry, DirectoryEntryKind, DirectoryReadError, PlatformError,
};
use flashshell_runtime::directory::{ListStep, list};
use flashshell_runtime::{ByteSize, NativePath, Value};

/// An entry source that hands out the given entries in order, then ends.
fn entries(
    entries: Vec<DirectoryEntry>,
) -> impl FnMut() -> Result<Option<DirectoryEntry>, DirectoryReadError> + 'static {
    let mut entries = entries.into_iter();
    move || Ok(entries.next())
}

fn file(name: &str, size: u64) -> DirectoryEntry {
    DirectoryEntry::new(name.into(), DirectoryEntryKind::File, Some(size))
}

fn directory(name: &str) -> DirectoryEntry {
    DirectoryEntry::new(name.into(), DirectoryEntryKind::Directory, None)
}

/// The record fields of one step, or a panic naming what arrived instead.
fn record(step: ListStep) -> Value {
    match step {
        ListStep::Entry(value) => value,
        other => panic!("expected one entry record, got {other:?}"),
    }
}

#[test]
fn a_regular_file_becomes_a_record_of_name_type_and_size() {
    let mut listing = list(entries(vec![file("notes.txt", 4096)]));

    let Value::Record(entry) = record(listing.pull()) else {
        panic!("a listing yields records");
    };

    let names: Vec<&str> = entry
        .entries()
        .iter()
        .map(|(key, _)| key.as_ref())
        .collect();
    assert_eq!(
        names,
        vec!["name", "type", "size"],
        "the field order is part of the observable shape",
    );
    assert_eq!(
        entry.get("name"),
        Some(&Value::Path(NativePath::new("notes.txt"))),
        "the name stays a native path so its bytes survive",
    );
    assert_eq!(entry.get("type"), Some(&Value::string("file")));
    assert_eq!(
        entry.get("size"),
        Some(&Value::ByteSize(ByteSize::new(4096))),
    );
}

#[test]
fn a_directory_reports_a_null_size_rather_than_zero() {
    // Zero would be a measurement the shell never made; null says so.
    let mut listing = list(entries(vec![directory("src")]));

    let Value::Record(entry) = record(listing.pull()) else {
        panic!("a listing yields records");
    };

    assert_eq!(entry.get("type"), Some(&Value::string("dir")));
    assert_eq!(entry.get("size"), Some(&Value::Null));
}

#[test]
fn a_link_reports_itself_and_an_unknown_kind_is_other() {
    let mut listing = list(entries(vec![
        DirectoryEntry::new("link".into(), DirectoryEntryKind::Symlink, None),
        DirectoryEntry::new("socket".into(), DirectoryEntryKind::Other, None),
    ]));

    let Value::Record(link) = record(listing.pull()) else {
        panic!("a listing yields records");
    };
    let Value::Record(other) = record(listing.pull()) else {
        panic!("a listing yields records");
    };

    assert_eq!(link.get("type"), Some(&Value::string("symlink")));
    assert_eq!(link.get("size"), Some(&Value::Null));
    assert_eq!(other.get("type"), Some(&Value::string("other")));
}

#[test]
fn a_listing_ends_and_stays_ended() {
    let mut listing = list(entries(vec![file("only.txt", 1)]));

    assert!(matches!(listing.pull(), ListStep::Entry(_)));
    assert!(matches!(listing.pull(), ListStep::End));
    assert!(matches!(listing.pull(), ListStep::End));
}

#[test]
fn an_empty_directory_yields_no_records_at_all() {
    let mut listing = list(entries(Vec::new()));

    assert!(matches!(listing.pull(), ListStep::End));
}

#[test]
fn the_walk_advances_one_host_entry_per_pull() {
    // Laziness is the milestone's no-full-materialization rule: a consumer that
    // takes one record must not have paid for the whole directory.
    let mut served = 0usize;
    let mut listing = list(move || {
        served += 1;
        Ok(Some(DirectoryEntry::new(
            format!("entry-{served}").into(),
            DirectoryEntryKind::File,
            Some(served as u64),
        )))
    });

    let Value::Record(first) = record(listing.pull()) else {
        panic!("a listing yields records");
    };

    assert_eq!(
        first.get("name"),
        Some(&Value::Path(NativePath::new("entry-1")))
    );
    // An endless source is fine precisely because nothing drained it.
    assert!(matches!(listing.pull(), ListStep::Entry(_)));
}

#[test]
fn a_walk_failure_is_reported_once_and_ends_the_listing() {
    let mut served = false;
    let mut listing = list(move || {
        if served {
            panic!("the source must not be advanced after a failure");
        }
        served = true;
        Err(DirectoryReadError::Operation {
            kind: std::io::ErrorKind::PermissionDenied,
            message: "permission denied".to_owned(),
        })
    });

    let ListStep::Failed(error) = listing.pull() else {
        panic!("the host failure surfaces as a failed step");
    };

    assert!(matches!(
        error,
        DirectoryReadError::Operation {
            kind: std::io::ErrorKind::PermissionDenied,
            ..
        },
    ));
    assert!(matches!(listing.pull(), ListStep::End));
}

#[test]
fn an_absent_capability_surfaces_unchanged_rather_than_as_an_empty_listing() {
    // An unsupported platform must not look like an empty directory.
    let mut listing = list(|| {
        Err(DirectoryReadError::Platform(PlatformError::Unsupported {
            capability: Capability::DirectoryRead,
        }))
    });

    let ListStep::Failed(error) = listing.pull() else {
        panic!("the capability gap surfaces as a failed step");
    };

    assert_eq!(
        error,
        DirectoryReadError::Platform(PlatformError::Unsupported {
            capability: Capability::DirectoryRead,
        }),
    );
}
