//! The explicit codec boundary between a byte stream and text or byte values.
//!
//! `decode <codec>` and `encode <codec>` are the two halves of the ratified
//! codec crossing (see the value-model specification): a byte stream never
//! becomes structured text implicitly, and a structured value never becomes
//! bytes implicitly. [`decode`] turns a lazy byte-chunk source into values, and
//! [`encode`] turns a [`ValueStream`] back into byte chunks.
//!
//! The layer is host-free and span-independent, matching [`crate::resolve`] and
//! [`crate::stream`]: nothing here touches a process, terminal, or clock, and a
//! malformed input reports only its logical byte offset. The executor that later
//! drives a boundary inside a pipeline attaches the command's source span. UTF-8
//! decoding carries an incomplete trailing multibyte sequence across chunk
//! boundaries, so a byte source may split a code point without corrupting it.
//!
//! The format boundary — `from <format>` and `to <format>` — is separate; only
//! its carrier contract is registered so far, and its format conversions arrive
//! with the format library. This codec layer is not yet wired into a live
//! pipeline executor.

use std::str;

use crate::Value;
use crate::eval::{CancelReason, RuntimeError};
use crate::stream::{StreamPull, ValueStream};

/// A codec selected by `decode <codec>` / `encode <codec>`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    /// UTF-8 text. Strict decoding fails at the offset of malformed input;
    /// `lossy` substitutes the U+FFFD replacement character and continues.
    Utf8 {
        /// Whether malformed input is replaced rather than reported.
        lossy: bool,
    },
    /// Byte-preserving: bytes cross unchanged as `Bytes` values and are never
    /// interpreted as text.
    Bytes,
}

/// One step of decoding a byte stream into values.
#[derive(Debug)]
pub enum DecodeStep {
    /// The next decoded value: a text `String` under UTF-8, a `Bytes` value under
    /// the byte-preserving codec.
    Value(Value),
    /// The byte source is exhausted with no bytes held over; further steps stay
    /// `End`.
    End,
    /// Strict UTF-8 met malformed input (or an unfinishable sequence at end of
    /// input) at this logical byte offset from the start of the stream. The
    /// executor attaches the `decode` command's source span at the pipeline
    /// boundary.
    Malformed {
        /// The logical byte offset of the malformed input.
        offset: usize,
    },
}

/// A pull-driven decoder produced by [`decode`].
pub struct Decoder {
    codec: Codec,
    chunks: Box<dyn FnMut() -> Option<Vec<u8>>>,
    /// Bytes of an incomplete trailing multibyte sequence held for the next
    /// chunk (UTF-8 only); its first byte sits at logical offset `offset`.
    carry: Vec<u8>,
    /// The logical byte offset of the start of `carry`.
    offset: usize,
    /// Latched once `End` or a strict `Malformed` was returned.
    done: bool,
}

/// Builds a decoder that turns the byte chunks yielded by `chunks` into values
/// under `codec`. The source yields `None` at end of input.
pub fn decode(codec: Codec, chunks: impl FnMut() -> Option<Vec<u8>> + 'static) -> Decoder {
    Decoder {
        codec,
        chunks: Box::new(chunks),
        carry: Vec::new(),
        offset: 0,
        done: false,
    }
}

impl Decoder {
    /// Pulls the next decoded value, exhaustion, or (strict UTF-8) a malformed
    /// input at its logical offset.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`]:
    /// exhaustion and a malformed input stay first-class steps rather than folding
    /// into an `Option`.
    pub fn pull(&mut self) -> DecodeStep {
        if self.done {
            return DecodeStep::End;
        }
        match self.codec {
            Codec::Bytes => self.next_bytes(),
            Codec::Utf8 { lossy: false } => self.next_utf8_strict(),
            Codec::Utf8 { lossy: true } => self.next_utf8_lossy(),
        }
    }

    /// One `Bytes` value per non-empty chunk; bytes are never interpreted.
    fn next_bytes(&mut self) -> DecodeStep {
        loop {
            match (self.chunks)() {
                Some(chunk) if chunk.is_empty() => continue,
                Some(chunk) => {
                    self.offset += chunk.len();
                    return DecodeStep::Value(Value::bytes(chunk));
                }
                None => {
                    self.done = true;
                    return DecodeStep::End;
                }
            }
        }
    }

    /// Strict UTF-8: emit the maximal valid prefix, hold an incomplete trailing
    /// sequence for the next chunk, and report the offset of truly malformed
    /// input (or an unfinishable sequence at end of input).
    fn next_utf8_strict(&mut self) -> DecodeStep {
        loop {
            match str::from_utf8(&self.carry) {
                Ok(text) if !text.is_empty() => {
                    let value = Value::string(text);
                    self.offset += self.carry.len();
                    self.carry.clear();
                    return DecodeStep::Value(value);
                }
                Ok(_) => {} // Empty carry: pull more bytes below.
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        return self.emit_valid_prefix(valid);
                    }
                    if error.error_len().is_some() {
                        // A complete but invalid sequence: no more input can fix it.
                        self.done = true;
                        return DecodeStep::Malformed {
                            offset: self.offset,
                        };
                    }
                    // An incomplete trailing sequence: pull more bytes below.
                }
            }

            match (self.chunks)() {
                Some(chunk) if chunk.is_empty() => {}
                Some(chunk) => self.carry.extend(chunk),
                None => return self.flush_utf8_strict_at_eof(),
            }
        }
    }

    /// Emits the valid UTF-8 prefix of `carry` up to `valid` bytes and keeps the
    /// remainder for the next step.
    fn emit_valid_prefix(&mut self, valid: usize) -> DecodeStep {
        let prefix =
            str::from_utf8(&self.carry[..valid]).expect("valid_up_to marks a valid UTF-8 boundary");
        let value = Value::string(prefix);
        self.offset += valid;
        self.carry.drain(..valid);
        DecodeStep::Value(value)
    }

    /// Resolves whatever remains in `carry` when the byte source ends.
    fn flush_utf8_strict_at_eof(&mut self) -> DecodeStep {
        if self.carry.is_empty() {
            self.done = true;
            return DecodeStep::End;
        }
        match str::from_utf8(&self.carry) {
            Ok(text) => {
                let value = Value::string(text);
                self.offset += self.carry.len();
                self.carry.clear();
                DecodeStep::Value(value)
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    self.emit_valid_prefix(valid)
                } else {
                    // An incomplete sequence at end of input can never complete.
                    self.done = true;
                    DecodeStep::Malformed {
                        offset: self.offset,
                    }
                }
            }
        }
    }

    /// Lossy UTF-8: one `String` per chunk with malformed bytes replaced by
    /// U+FFFD inline, still holding an incomplete trailing sequence for the next
    /// chunk so a split code point is not mistaken for an error.
    fn next_utf8_lossy(&mut self) -> DecodeStep {
        loop {
            match (self.chunks)() {
                Some(chunk) if chunk.is_empty() => {}
                Some(chunk) => {
                    self.carry.extend(chunk);
                    let (text, consumed) = decode_utf8_lossy_holding_incomplete(&self.carry);
                    self.offset += consumed;
                    self.carry.drain(..consumed);
                    if text.is_empty() {
                        // The whole chunk was an incomplete prefix; pull more.
                        continue;
                    }
                    return DecodeStep::Value(Value::string(text));
                }
                None => {
                    if self.carry.is_empty() {
                        self.done = true;
                        return DecodeStep::End;
                    }
                    // Flush the held incomplete tail; it can never complete.
                    let text = String::from_utf8_lossy(&self.carry).into_owned();
                    self.offset += self.carry.len();
                    self.carry.clear();
                    return DecodeStep::Value(Value::string(text));
                }
            }
        }
    }
}

/// Decodes `bytes` as UTF-8, replacing malformed sequences with U+FFFD, but stops
/// at an incomplete trailing sequence and leaves those bytes unconsumed. Returns
/// the decoded text and the number of leading bytes it consumed.
fn decode_utf8_lossy_holding_incomplete(bytes: &[u8]) -> (String, usize) {
    let mut text = String::new();
    let mut index = 0;
    loop {
        match str::from_utf8(&bytes[index..]) {
            Ok(rest) => {
                text.push_str(rest);
                index = bytes.len();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                text.push_str(
                    str::from_utf8(&bytes[index..index + valid])
                        .expect("valid_up_to marks a valid UTF-8 boundary"),
                );
                index += valid;
                match error.error_len() {
                    Some(length) => {
                        text.push('\u{FFFD}');
                        index += length;
                    }
                    None => break, // Incomplete trailing sequence: hold it.
                }
            }
        }
    }
    (text, index)
}

/// One step of encoding values into a byte stream.
#[derive(Debug)]
pub enum EncodeStep {
    /// The next byte chunk serialized from one value.
    Chunk(Vec<u8>),
    /// The upstream value stream is exhausted; further steps stay `End`.
    End,
    /// The codec cannot serialize a value of this family (for example a non-text
    /// value under UTF-8). Carries the offending value's family name.
    NotEncodable {
        /// The family name of the value the codec rejected.
        actual: &'static str,
    },
    /// The upstream producer raised a runtime error, passed through unchanged.
    Failed(RuntimeError),
    /// The upstream stream was cancelled, passed through unchanged.
    Cancelled(CancelReason),
}

/// A pull-driven encoder produced by [`encode`].
pub struct Encoder {
    codec: Codec,
    input: ValueStream,
    done: bool,
}

/// Builds an encoder that serializes the values pulled from `input` into byte
/// chunks under `codec`.
#[must_use]
pub fn encode(codec: Codec, input: ValueStream) -> Encoder {
    Encoder {
        codec,
        input,
        done: false,
    }
}

impl Encoder {
    /// Pulls the next serialized byte chunk, exhaustion, an unencodable-value
    /// report, or a passed-through upstream failure or cancellation.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`crate::stream::ValueStream::pull`]:
    /// the non-`Chunk` steps stay first-class terminal states.
    pub fn pull(&mut self) -> EncodeStep {
        if self.done {
            return EncodeStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(value) => match self.encode_value(&value) {
                Ok(bytes) => EncodeStep::Chunk(bytes),
                Err(actual) => {
                    self.done = true;
                    EncodeStep::NotEncodable { actual }
                }
            },
            StreamPull::End => {
                self.done = true;
                EncodeStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                EncodeStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                EncodeStep::Cancelled(reason)
            }
        }
    }

    /// Serializes one value under the codec, or names the family it cannot encode.
    fn encode_value(&self, value: &Value) -> Result<Vec<u8>, &'static str> {
        match self.codec {
            Codec::Utf8 { .. } => match value {
                Value::String(text) => Ok(text.as_bytes().to_vec()),
                other => Err(other.family_name()),
            },
            Codec::Bytes => match value {
                Value::Bytes(bytes) => Ok(bytes.to_vec()),
                other => Err(other.family_name()),
            },
        }
    }
}
