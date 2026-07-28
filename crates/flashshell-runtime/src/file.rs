//! The byte-preserving file boundary behind `open` and `save`.
//!
//! Opening the platform endpoint is deliberately outside this module. The
//! runtime layer receives one bounded read action, or one byte-chunk source and
//! write action, so its streaming behavior is testable without a filesystem.
//! The internal carrier executor supplies those actions from an owned platform
//! file endpoint under `FileActions`.
//!
//! Neither direction interprets the bytes. Parsing remains an explicit
//! `from <format>` stage and serialization remains an explicit `to <format>`
//! stage.

use std::io;

use flashshell_platform::FileActionError;

type ReadAction = dyn FnMut(&mut [u8]) -> Result<usize, FileActionError>;
type ChunkSource = dyn FnMut() -> Result<Option<Vec<u8>>, FileActionError>;
type WriteAction = dyn FnMut(&[u8]) -> Result<usize, FileActionError>;

/// One step of reading bytes for `open`.
#[derive(Debug)]
pub enum OpenStep {
    /// The next non-empty byte chunk.
    Chunk(Vec<u8>),
    /// The file is exhausted; further steps stay `End`.
    End,
    /// Reading failed. The executor attaches the command's source span.
    Failed(FileActionError),
}

/// A pull-driven file reader produced by [`open`].
pub struct OpenFile {
    read: Box<ReadAction>,
    buffer: Vec<u8>,
    done: bool,
}

/// Builds a lazy byte source over one already-opened file.
///
/// `chunk_size` is the maximum number of bytes one pull can request and must be
/// non-zero. A read of zero means EOF. The reader is never called before the
/// first pull or after a terminal step.
///
/// # Panics
///
/// Panics when `chunk_size` is zero.
pub fn open(
    read: impl FnMut(&mut [u8]) -> Result<usize, FileActionError> + 'static,
    chunk_size: usize,
) -> OpenFile {
    assert!(chunk_size > 0, "an open chunk must hold at least one byte");
    OpenFile {
        read: Box::new(read),
        buffer: vec![0; chunk_size],
        done: false,
    }
}

impl OpenFile {
    /// Pulls one bounded byte chunk, EOF, or the first read failure.
    pub fn pull(&mut self) -> OpenStep {
        if self.done {
            return OpenStep::End;
        }

        match (self.read)(&mut self.buffer) {
            Ok(0) => {
                self.done = true;
                OpenStep::End
            }
            Ok(amount) if amount <= self.buffer.len() => {
                OpenStep::Chunk(self.buffer[..amount].to_vec())
            }
            Ok(amount) => {
                self.done = true;
                OpenStep::Failed(FileActionError::Operation {
                    kind: io::ErrorKind::InvalidData,
                    message: format!(
                        "file reader reported {amount} bytes for a {}-byte buffer",
                        self.buffer.len()
                    ),
                })
            }
            Err(error) => {
                self.done = true;
                OpenStep::Failed(error)
            }
        }
    }
}

/// One step of writing bytes for `save`.
#[derive(Debug)]
pub enum SaveStep {
    /// One complete input chunk was written.
    Wrote {
        /// The number of bytes in that chunk.
        bytes: usize,
    },
    /// The input is exhausted; further steps stay `End`.
    End,
    /// Pulling input or writing the current chunk failed. The executor attaches
    /// the command's source span.
    Failed(FileActionError),
}

/// A pull-driven file writer produced by [`save`].
pub struct SaveFile {
    chunks: Box<ChunkSource>,
    write: Box<WriteAction>,
    done: bool,
}

/// Builds a byte sink over one already-opened truncate target.
///
/// One call to [`SaveFile::pull`] advances the input by at most one chunk and
/// writes that chunk completely before returning. Partial writes are retried;
/// a zero-byte write while data remains is a failure instead of a spin.
pub fn save(
    chunks: impl FnMut() -> Result<Option<Vec<u8>>, FileActionError> + 'static,
    write: impl FnMut(&[u8]) -> Result<usize, FileActionError> + 'static,
) -> SaveFile {
    SaveFile {
        chunks: Box::new(chunks),
        write: Box::new(write),
        done: false,
    }
}

impl SaveFile {
    /// Pulls and completely writes one input chunk, or returns a terminal step.
    pub fn pull(&mut self) -> SaveStep {
        if self.done {
            return SaveStep::End;
        }

        let chunk = match (self.chunks)() {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                self.done = true;
                return SaveStep::End;
            }
            Err(error) => {
                self.done = true;
                return SaveStep::Failed(error);
            }
        };

        match write_complete(&mut self.write, &chunk) {
            Ok(bytes) => SaveStep::Wrote { bytes },
            Err(error) => {
                self.done = true;
                SaveStep::Failed(error)
            }
        }
    }
}

/// Write one complete chunk with the same progress and bounds checks as
/// [`SaveFile`].
pub(crate) fn write_complete(
    write: &mut dyn FnMut(&[u8]) -> Result<usize, FileActionError>,
    chunk: &[u8],
) -> Result<usize, FileActionError> {
    let mut written = 0usize;
    while written < chunk.len() {
        match write(&chunk[written..]) {
            Ok(0) => {
                return Err(FileActionError::Operation {
                    kind: io::ErrorKind::WriteZero,
                    message: "file writer made no progress".to_owned(),
                });
            }
            Ok(amount) if amount <= chunk.len() - written => written += amount,
            Ok(amount) => {
                return Err(FileActionError::Operation {
                    kind: io::ErrorKind::InvalidData,
                    message: format!(
                        "file writer reported {amount} bytes for a {}-byte remainder",
                        chunk.len() - written
                    ),
                });
            }
            Err(error) => return Err(error),
        }
    }
    Ok(written)
}
