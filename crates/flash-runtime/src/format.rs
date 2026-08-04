//! The explicit format boundary: `from <format>` and `to <format>`.
//!
//! This is the structured counterpart to [`crate::convert`]'s codec boundary.
//! `decode`/`encode` cross between bytes and *text*; `from`/`to` cross between
//! bytes and *structured values*. Both layers are host-free and
//! span-independent — no process, terminal, or clock participates, and a
//! malformed input reports only its logical byte offset, leaving the executor
//! to attach the command's source span at the pipeline boundary.
//!
//! JSON is not a streaming format: a document is well-formed only once its last
//! byte has been read. The value model permits a non-streaming format to
//! materialize "only through a documented budget", so [`from_json`] drains its
//! byte source under an explicit byte bound and reports
//! [`FromJsonStep::LimitExceeded`] rather than truncating. [`to_json`] has no
//! such problem and stays lazy, serializing one value per pull.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use serde::Deserializer;
use serde::de::{self, MapAccess, SeqAccess, Visitor};

use crate::convert::{Codec, DecodeStep, Decoder, decode};
use crate::eval::{CancelReason, RuntimeError};
use crate::stream::{StreamPull, ValueStream};
use crate::structured::LineCarry;
use crate::value::{Record, Table, Value};

/// How `from json` reads a document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonMode {
    /// The document is one value: one `Value` step, then `End`.
    Document,
    /// The document must be a top-level array, and each element becomes its own
    /// step, so a JSON array bridges into a value stream.
    Array,
}

/// One step of parsing a byte stream into structured values.
#[derive(Debug)]
pub enum FromJsonStep {
    /// The next parsed value.
    Value(Value),
    /// The document is exhausted; further steps stay `End`.
    End,
    /// The input is not well-formed JSON at this logical byte offset from the
    /// start of the stream. The executor attaches the command's source span at
    /// the pipeline boundary.
    Malformed {
        /// The logical byte offset of the malformed input.
        offset: usize,
    },
    /// `JsonMode::Array` met a document that is not an array; the field names
    /// the value family that was found instead.
    NotArray {
        /// The family name of the parsed document.
        actual: &'static str,
    },
    /// An object repeated a key. This is well-formed JSON but not a well-formed
    /// record, whose contract makes a duplicate key an error at the later key,
    /// so it is deliberately not folded into `Malformed` — there is no syntax
    /// error and therefore no meaningful byte offset.
    DuplicateKey {
        /// The repeated key, at its later occurrence.
        key: String,
    },
    /// The byte source would exceed the materialization budget. The source is
    /// not drained further and nothing is truncated.
    LimitExceeded {
        /// The budget in bytes.
        limit: usize,
    },
}

/// A pull-driven JSON parser produced by [`from_json`].
pub struct FromJson {
    mode: JsonMode,
    chunks: Box<dyn FnMut() -> Option<Vec<u8>>>,
    limit: usize,
    /// Parsed values still to hand out, in order, reversed so `pop` is the next.
    pending: Vec<Value>,
    /// Latched once a terminal step was returned.
    done: bool,
    /// Whether the source has been drained and parsed yet.
    parsed: bool,
}

/// Builds a parser that turns the byte chunks yielded by `chunks` into values.
///
/// The source yields `None` at end of input. `limit` bounds the number of
/// **bytes** materialized before parsing; a source that would exceed it is
/// refused rather than truncated.
pub fn from_json(
    mode: JsonMode,
    chunks: impl FnMut() -> Option<Vec<u8>> + 'static,
    limit: usize,
) -> FromJson {
    FromJson {
        mode,
        chunks: Box::new(chunks),
        limit,
        pending: Vec::new(),
        done: false,
        parsed: false,
    }
}

impl FromJson {
    /// Pulls the next parsed value, exhaustion, or a first-class terminal state.
    ///
    /// Deliberately not `Iterator::next`, mirroring [`ValueStream::pull`]: a
    /// malformed document, a refused budget, and a duplicate key stay
    /// first-class steps rather than folding into an `Option`.
    pub fn pull(&mut self) -> FromJsonStep {
        if self.done {
            return FromJsonStep::End;
        }
        if !self.parsed {
            self.parsed = true;
            if let Some(step) = self.drain_and_parse() {
                self.done = true;
                return step;
            }
        }
        match self.pending.pop() {
            Some(value) => FromJsonStep::Value(value),
            None => {
                self.done = true;
                FromJsonStep::End
            }
        }
    }

    /// Pulls one value, panicking on any other step. Test and caller convenience
    /// for the common single-document case.
    ///
    /// # Panics
    ///
    /// Panics when the next step is not a value.
    pub fn pull_value(&mut self) -> Value {
        match self.pull() {
            FromJsonStep::Value(value) => value,
            other => panic!("expected a JSON value, got {other:?}"),
        }
    }

    /// Drains the byte source under the budget and parses it, filling `pending`.
    /// Returns a terminal step when the document cannot be turned into values.
    fn drain_and_parse(&mut self) -> Option<FromJsonStep> {
        let mut input: Vec<u8> = Vec::new();
        while let Some(chunk) = (self.chunks)() {
            if input.len() + chunk.len() > self.limit {
                return Some(FromJsonStep::LimitExceeded { limit: self.limit });
            }
            input.extend_from_slice(&chunk);
        }

        let duplicate = Rc::new(RefCell::new(None));
        let mut deserializer = serde_json::Deserializer::from_slice(&input);
        let parsed = match deserializer.deserialize_any(ValueVisitor {
            duplicate: Rc::clone(&duplicate),
        }) {
            Ok(value) => value,
            Err(error) => return Some(parse_error(&input, &error, &duplicate)),
        };
        // A document with trailing non-whitespace is malformed, not a prefix.
        if let Err(error) = deserializer.end() {
            return Some(parse_error(&input, &error, &duplicate));
        }

        match self.mode {
            JsonMode::Document => self.pending.push(parsed),
            JsonMode::Array => match parsed {
                Value::List(items) => self.pending.extend(items.iter().rev().cloned()),
                other => {
                    return Some(FromJsonStep::NotArray {
                        actual: other.family_name(),
                    });
                }
            },
        }
        None
    }
}

/// Classifies a parse failure, separating a duplicate record key from a genuine
/// syntax error.
///
/// The duplicate key travels in its own slot rather than being recovered from
/// the error's text: serde_json owns that wording, and matching on it would make
/// a correct classification depend on a message format no contract pins.
fn parse_error(
    input: &[u8],
    error: &serde_json::Error,
    duplicate: &Rc<RefCell<Option<String>>>,
) -> FromJsonStep {
    if let Some(key) = duplicate.borrow_mut().take() {
        return FromJsonStep::DuplicateKey { key };
    }
    FromJsonStep::Malformed {
        offset: byte_offset(input, error.line(), error.column()),
    }
}

/// Recomputes a logical byte offset from serde_json's one-based line and column.
///
/// A column past the end of its line clamps to the line end, so a report can
/// never index past the buffer.
fn byte_offset(input: &[u8], line: usize, column: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut start = 0;
    let mut seen = 1;
    while seen < line {
        match input[start..].iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                start += index + 1;
                seen += 1;
            }
            None => return input.len(),
        }
    }
    let line_end = input[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(input.len(), |index| start + index);
    // serde_json's column is one-based and points just past the offending byte
    // for a syntax error, so the offending byte itself is one back.
    let offset = start + column.saturating_sub(1);
    offset.min(line_end)
}

/// Builds a [`Value`] directly from a serde data stream.
///
/// Parsing through a visitor rather than into `serde_json::Value` is what keeps
/// object key order: the visitor observes keys in document order, so `Record`'s
/// observable insertion order survives by construction instead of depending on
/// the `preserve_order` feature (which would pull `indexmap`, `hashbrown`, and
/// `equivalent` into the shipped image).
struct ValueVisitor {
    /// Receives the repeated key when an object violates the record contract,
    /// so the caller can classify the failure without reading error text.
    duplicate: Rc<RefCell<Option<String>>>,
}

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Int(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        // An unsigned value beyond i64 keeps its magnitude as a float rather
        // than wrapping into a negative integer.
        i64::try_from(value).map_or_else(
            |_| float_value(value as f64),
            |integer| Ok(Value::Int(integer)),
        )
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        float_value(value)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::string(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = access.next_element_seed(ValueSeed {
            duplicate: Rc::clone(&self.duplicate),
        })? {
            items.push(item);
        }
        Ok(Value::list(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut entries: Vec<(String, Value)> = Vec::new();
        while let Some(key) = access.next_key::<String>()? {
            let value = access.next_value_seed(ValueSeed {
                duplicate: Rc::clone(&self.duplicate),
            })?;
            entries.push((key, value));
        }
        // The record constructor is the single authority on key uniqueness, so
        // the duplicate rule is not restated here.
        Record::new(entries).map(Value::from).map_err(|repeated| {
            // Keep the first duplicate found: an outer object must not overwrite
            // the inner one that actually failed.
            let mut slot = self.duplicate.borrow_mut();
            if slot.is_none() {
                *slot = Some(repeated.key().to_owned());
            }
            de::Error::custom("a JSON object repeated a key")
        })
    }
}

/// Rejects a non-finite number, which the `Float` family does not admit.
fn float_value<E: de::Error>(value: f64) -> Result<Value, E> {
    crate::value::FiniteFloat::new(value)
        .map(Value::from)
        .map_err(|_| de::Error::custom("a JSON number that is not finite"))
}

/// Threads [`ValueVisitor`] and its duplicate-key slot through nested arrays
/// and objects.
struct ValueSeed {
    duplicate: Rc<RefCell<Option<String>>>,
}

impl<'de> de::DeserializeSeed<'de> for ValueSeed {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(ValueVisitor {
            duplicate: self.duplicate,
        })
    }
}

/// One step of serializing structured values into a byte stream.
#[derive(Debug)]
pub enum ToJsonStep {
    /// The next value's serialized bytes.
    Chunk(Vec<u8>),
    /// The input stream is exhausted; further steps stay `End`.
    End,
    /// The value's family has no JSON representation. Inventing one — a byte
    /// array, a native path string, a nanosecond count — would be exactly the
    /// implicit lossy conversion the value model forbids, so an explicit
    /// conversion must come first.
    NotEncodable {
        /// The family name of the refused value.
        actual: &'static str,
    },
    /// The upstream producer raised a runtime error carrying its own span.
    Failed(RuntimeError),
    /// The upstream stream was cancelled.
    Cancelled(CancelReason),
}

/// A pull-driven JSON serializer produced by [`to_json`].
pub struct ToJson {
    input: ValueStream,
    done: bool,
}

/// Builds a serializer that turns each value of `input` into one byte chunk.
///
/// Serialization stays lazy — one upstream pull and one serialization per step —
/// so a bounded downstream keeps an unbounded upstream bounded.
#[must_use]
pub fn to_json(input: ValueStream) -> ToJson {
    ToJson { input, done: false }
}

impl ToJson {
    /// Pulls the next serialized chunk, exhaustion, or a first-class terminal
    /// state.
    pub fn pull(&mut self) -> ToJsonStep {
        if self.done {
            return ToJsonStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(value) => match render(&value) {
                Some(text) => ToJsonStep::Chunk(text.into_bytes()),
                None => {
                    self.done = true;
                    ToJsonStep::NotEncodable {
                        actual: value.family_name(),
                    }
                }
            },
            StreamPull::End => {
                self.done = true;
                ToJsonStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                ToJsonStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                ToJsonStep::Cancelled(reason)
            }
        }
    }
}

/// Serializes one value, or `None` when its family has no JSON representation.
fn render(value: &Value) -> Option<String> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Some(out)
}

/// Writes one value's JSON text, returning `None` at the first unencodable
/// family so a nested refusal aborts the whole value rather than emitting a
/// partial document.
fn write_value(out: &mut String, value: &Value) -> Option<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Int(integer) => {
            // Ignoring the result: writing into a String cannot fail.
            let _ = write!(out, "{integer}");
        }
        Value::Float(float) => {
            let number = serde_json::Number::from_f64(float.get())?;
            let _ = write!(out, "{number}");
        }
        Value::String(text) => write_string(out, text),
        Value::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Record(record) => {
            out.push('{');
            for (index, (key, field)) in record.entries().iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, field)?;
            }
            out.push('}');
        }
        Value::Table(table) => write_table(out, table)?,
        // Every remaining family — bytes, paths, durations, byte sizes, ranges,
        // statuses, and callables — has no JSON counterpart.
        _ => return None,
    }
    Some(())
}

/// Writes a table as an array of row objects keyed by the column names.
///
/// This is the interoperable shape every other tool expects. Its documented
/// cost: a table with columns but no rows serializes as `[]` and loses its
/// column names, because the array form has nowhere to carry them.
fn write_table(out: &mut String, table: &Table) -> Option<()> {
    out.push('[');
    for (index, cells) in table.rows().iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        out.push('{');
        for (position, column) in table.columns().iter().enumerate() {
            if position != 0 {
                out.push(',');
            }
            write_string(out, column);
            out.push(':');
            write_value(out, &cells[position])?;
        }
        out.push('}');
    }
    out.push(']');
    Some(())
}

/// Writes a JSON string literal, delegating the escaping rules to the format
/// library rather than restating them.
fn write_string(out: &mut String, text: &str) {
    // Ignoring the result: serializing a string cannot fail, and writing into a
    // String cannot fail.
    if let Ok(encoded) = serde_json::to_string(text) {
        out.push_str(&encoded);
    }
}

/// One step of parsing a byte stream into lines of text.
#[derive(Debug)]
pub enum FromTextStep {
    /// The next line, as a `String` value, with its terminator and a single
    /// preceding `\r` removed.
    Line(Value),
    /// The source is exhausted and no partial line remains; further steps stay
    /// `End`.
    End,
    /// Strict UTF-8 decoding met malformed input at this logical byte offset.
    /// The executor attaches the command's source span at the pipeline boundary.
    Malformed {
        /// The logical byte offset of the malformed input.
        offset: usize,
    },
    /// One line grew past the bound without a terminator. Unlike a whole
    /// document, a single line is the only thing this format must materialize,
    /// so it is the only thing that needs a budget — and it is refused rather
    /// than truncated.
    LineTooLong {
        /// The budget in bytes for one line.
        limit: usize,
    },
}

/// A pull-driven line reader produced by [`from_text`].
pub struct FromText {
    decoder: Decoder,
    carry: LineCarry,
    limit: usize,
    /// The decoder reached `End`; only a trailing partial line remains.
    input_ended: bool,
    /// Latched once a terminal step was returned.
    done: bool,
}

/// Builds a reader that turns the byte chunks yielded by `chunks` into one
/// `String` value per line.
///
/// Unlike JSON this format is genuinely streaming: a line is complete at its
/// terminator, so no whole-document budget is required and an endless source
/// with a bounded reader stays bounded. `limit` bounds one *line* in bytes,
/// which is the only unbounded materialization the format can be forced into.
pub fn from_text(chunks: impl FnMut() -> Option<Vec<u8>> + 'static, limit: usize) -> FromText {
    FromText {
        decoder: decode(Codec::Utf8 { lossy: false }, chunks),
        carry: LineCarry::default(),
        limit,
        input_ended: false,
        done: false,
    }
}

impl FromText {
    /// Pulls the next line, exhaustion, malformed input, or a refused line bound.
    pub fn pull(&mut self) -> FromTextStep {
        if self.done {
            return FromTextStep::End;
        }
        loop {
            if let Some(line) = self.carry.next_line() {
                return FromTextStep::Line(Value::string(line));
            }
            if self.input_ended {
                self.done = true;
                return match self.carry.flush() {
                    Some(line) => FromTextStep::Line(Value::string(line)),
                    None => FromTextStep::End,
                };
            }
            // Only an unterminated line is held, so the bound is checked here
            // rather than against the whole stream.
            if self.carry.len() > self.limit {
                self.done = true;
                return FromTextStep::LineTooLong { limit: self.limit };
            }
            match self.decoder.pull() {
                DecodeStep::Value(Value::String(text)) => self.carry.push(&text),
                DecodeStep::Value(_) => unreachable!("the UTF-8 codec yields only String values"),
                DecodeStep::End => self.input_ended = true,
                DecodeStep::Malformed { offset } => {
                    self.done = true;
                    return FromTextStep::Malformed { offset };
                }
            }
        }
    }
}

/// One step of writing structured values as lines of text.
#[derive(Debug)]
pub enum ToTextStep {
    /// The next value's line, including its terminator.
    Chunk(Vec<u8>),
    /// The input stream is exhausted; further steps stay `End`.
    End,
    /// The value's family has no single-line text form.
    NotEncodable {
        /// The family name of the refused value.
        actual: &'static str,
    },
    /// The upstream producer raised a runtime error carrying its own span.
    Failed(RuntimeError),
    /// The upstream stream was cancelled.
    Cancelled(CancelReason),
}

/// A pull-driven line writer produced by [`to_text`].
pub struct ToText {
    input: ValueStream,
    done: bool,
}

/// Builds a writer that turns each value of `input` into one terminated line.
#[must_use]
pub fn to_text(input: ValueStream) -> ToText {
    ToText { input, done: false }
}

impl ToText {
    /// Pulls the next line, exhaustion, or a first-class terminal state.
    pub fn pull(&mut self) -> ToTextStep {
        if self.done {
            return ToTextStep::End;
        }
        match self.input.pull() {
            StreamPull::Item(value) => match line_encoding(&value) {
                Some(mut bytes) => {
                    bytes.push(b'\n');
                    ToTextStep::Chunk(bytes)
                }
                None => {
                    self.done = true;
                    ToTextStep::NotEncodable {
                        actual: value.family_name(),
                    }
                }
            },
            StreamPull::End => {
                self.done = true;
                ToTextStep::End
            }
            StreamPull::Failed(error) => {
                self.done = true;
                ToTextStep::Failed(error)
            }
            StreamPull::Cancelled(reason) => {
                self.done = true;
                ToTextStep::Cancelled(reason)
            }
        }
    }
}

/// One value's line bytes, or `None` when its family has no single-line form.
///
/// The eligible families are deliberately the same set that may become a command
/// argument (see `word_encoding` in `crate::eval`): a value that can be written
/// as a word can be written as a line, and nothing else can. That keeps the
/// no-implicit-serialization rule in one place — a list, record, or table has an
/// obvious-looking human rendering, and neither argv nor this writer will accept
/// it as bytes. `null` is excluded for the same reason it is excluded from argv:
/// it denotes absence and must be converted explicitly.
fn line_encoding(value: &Value) -> Option<Vec<u8>> {
    let text = match value {
        Value::Bool(flag) => (if *flag { "true" } else { "false" }).to_owned(),
        Value::Int(integer) => integer.to_string(),
        Value::Float(float) => float.to_string(),
        Value::String(text) => text.as_ref().to_owned(),
        Value::Duration(duration) => duration.to_string(),
        Value::ByteSize(size) => size.to_string(),
        // A path carries native units, which may not be UTF-8; it crosses as its
        // exact bytes rather than through a lossy string.
        Value::Path(path) => return Some(path.as_os_str().as_encoded_bytes().to_vec()),
        _ => return None,
    };
    Some(text.into_bytes())
}
