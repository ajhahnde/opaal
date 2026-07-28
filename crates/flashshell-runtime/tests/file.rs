#![forbid(unsafe_code)]

//! Acceptance coverage for the byte-preserving file boundary behind `open`
//! and `save`.
//!
//! The layer is host-free and span-independent. A caller supplies one bounded
//! read action or one byte-chunk source plus write action, so every streaming
//! property is observable without touching a filesystem. The platform seam
//! opens the actual endpoint; this layer never decodes, renders, or serializes
//! the bytes crossing it.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;

use flashshell_platform::FileActionError;
use flashshell_runtime::file::{OpenStep, SaveStep, open, save};

fn operation(kind: io::ErrorKind, message: &str) -> FileActionError {
    FileActionError::Operation {
        kind,
        message: message.to_owned(),
    }
}

#[test]
fn open_is_lazy_bounded_and_byte_preserving() {
    let source = Rc::new(b"\0a\xffbcdef".to_vec());
    let position = Rc::new(Cell::new(0usize));
    let reads = Rc::new(Cell::new(0usize));
    let mut opened = open(
        {
            let source = Rc::clone(&source);
            let position = Rc::clone(&position);
            let reads = Rc::clone(&reads);
            move |buffer| {
                reads.set(reads.get() + 1);
                let start = position.get();
                let amount = buffer.len().min(source.len().saturating_sub(start));
                buffer[..amount].copy_from_slice(&source[start..start + amount]);
                position.set(start + amount);
                Ok(amount)
            }
        },
        3,
    );

    assert_eq!(
        reads.get(),
        0,
        "constructing open must not touch the source"
    );
    assert!(matches!(
        opened.pull(),
        OpenStep::Chunk(ref bytes) if bytes == b"\0a\xff"
    ));
    assert_eq!(reads.get(), 1, "one pull performs one bounded read");
    assert!(matches!(
        opened.pull(),
        OpenStep::Chunk(ref bytes) if bytes == b"bcd"
    ));
    assert_eq!(reads.get(), 2);
    assert!(matches!(
        opened.pull(),
        OpenStep::Chunk(ref bytes) if bytes == b"ef"
    ));
    assert!(matches!(opened.pull(), OpenStep::End));
    assert!(matches!(opened.pull(), OpenStep::End));
    assert_eq!(reads.get(), 4, "latched EOF never reads again");
}

#[test]
fn open_reports_a_read_failure_once_and_then_ends() {
    let reads = Rc::new(Cell::new(0usize));
    let mut opened = open(
        {
            let reads = Rc::clone(&reads);
            move |_buffer| {
                reads.set(reads.get() + 1);
                Err(operation(io::ErrorKind::PermissionDenied, "denied"))
            }
        },
        16,
    );

    assert!(matches!(
        opened.pull(),
        OpenStep::Failed(FileActionError::Operation {
            kind: io::ErrorKind::PermissionDenied,
            ..
        })
    ));
    assert!(matches!(opened.pull(), OpenStep::End));
    assert_eq!(reads.get(), 1);
}

#[test]
fn save_pulls_one_chunk_at_a_time_and_completes_partial_writes() {
    let chunks = Rc::new(RefCell::new(
        vec![b"\0a\xff".to_vec(), b"bcde".to_vec()].into_iter(),
    ));
    let pulls = Rc::new(Cell::new(0usize));
    let written = Rc::new(RefCell::new(Vec::new()));
    let writes = Rc::new(Cell::new(0usize));
    let mut saved = save(
        {
            let chunks = Rc::clone(&chunks);
            let pulls = Rc::clone(&pulls);
            move || {
                pulls.set(pulls.get() + 1);
                Ok(chunks.borrow_mut().next())
            }
        },
        {
            let written = Rc::clone(&written);
            let writes = Rc::clone(&writes);
            move |bytes| {
                writes.set(writes.get() + 1);
                let amount = bytes.len().min(2);
                written.borrow_mut().extend_from_slice(&bytes[..amount]);
                Ok(amount)
            }
        },
    );

    assert_eq!(pulls.get(), 0, "constructing save must not pull input");
    assert!(matches!(saved.pull(), SaveStep::Wrote { bytes: 3 }));
    assert_eq!(pulls.get(), 1, "one step pulls one input chunk");
    assert_eq!(&*written.borrow(), b"\0a\xff");
    assert_eq!(writes.get(), 2, "the first partial write is completed");

    assert!(matches!(saved.pull(), SaveStep::Wrote { bytes: 4 }));
    assert_eq!(pulls.get(), 2);
    assert_eq!(&*written.borrow(), b"\0a\xffbcde");
    assert_eq!(writes.get(), 4);

    assert!(matches!(saved.pull(), SaveStep::End));
    assert!(matches!(saved.pull(), SaveStep::End));
    assert_eq!(pulls.get(), 3, "latched EOF never pulls again");
}

#[test]
fn save_propagates_an_input_failure_without_writing() {
    let pulls = Rc::new(Cell::new(0usize));
    let writes = Rc::new(Cell::new(0usize));
    let mut saved = save(
        {
            let pulls = Rc::clone(&pulls);
            move || {
                pulls.set(pulls.get() + 1);
                Err(operation(io::ErrorKind::BrokenPipe, "upstream failed"))
            }
        },
        {
            let writes = Rc::clone(&writes);
            move |_bytes| {
                writes.set(writes.get() + 1);
                Ok(0)
            }
        },
    );

    assert!(matches!(
        saved.pull(),
        SaveStep::Failed(FileActionError::Operation {
            kind: io::ErrorKind::BrokenPipe,
            ..
        })
    ));
    assert!(matches!(saved.pull(), SaveStep::End));
    assert_eq!(pulls.get(), 1);
    assert_eq!(writes.get(), 0);
}

#[test]
fn save_reports_a_write_failure_once_and_does_not_pull_a_later_chunk() {
    let pulls = Rc::new(Cell::new(0usize));
    let mut saved = save(
        {
            let pulls = Rc::clone(&pulls);
            move || {
                pulls.set(pulls.get() + 1);
                Ok(Some(vec![1, 2, 3]))
            }
        },
        |_bytes| Err(operation(io::ErrorKind::StorageFull, "disk full")),
    );

    assert!(matches!(
        saved.pull(),
        SaveStep::Failed(FileActionError::Operation {
            kind: io::ErrorKind::StorageFull,
            ..
        })
    ));
    assert!(matches!(saved.pull(), SaveStep::End));
    assert_eq!(pulls.get(), 1);
}

#[test]
fn save_treats_a_zero_byte_write_as_a_failure_instead_of_spinning() {
    let writes = Rc::new(Cell::new(0usize));
    let mut chunk = Some(vec![1, 2, 3]);
    let mut saved = save(move || Ok(chunk.take()), {
        let writes = Rc::clone(&writes);
        move |_bytes| {
            writes.set(writes.get() + 1);
            Ok(0)
        }
    });

    assert!(matches!(
        saved.pull(),
        SaveStep::Failed(FileActionError::Operation {
            kind: io::ErrorKind::WriteZero,
            ..
        })
    ));
    assert!(matches!(saved.pull(), SaveStep::End));
    assert_eq!(writes.get(), 1);
}
