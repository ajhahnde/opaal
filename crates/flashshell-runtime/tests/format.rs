#![forbid(unsafe_code)]

//! Acceptance coverage for the explicit format boundary — `from <format>` and
//! `to <format>` — that crosses between a `ByteStream` and structured values.
//! Like the codec boundary in `convert.rs`, the layer is host-free and
//! span-independent: no process, terminal, or clock participates, malformed
//! input reports only its logical byte offset, and the executor attaches a
//! source span at the pipeline boundary later.
//!
//! JSON is not a streaming format — a document is well-formed only once its
//! last byte is read — so `from json` drains its byte source under an explicit
//! byte budget rather than truncating silently. `to json` stays lazy, emitting
//! one serialized chunk per pulled value.
//!
//! The line-oriented `text` format is genuinely streaming, so it needs no
//! whole-document budget — only a bound on one unterminated line. Its splitting
//! rule is the same implementation `lines` uses, so the two cannot drift.

use flashshell_runtime::eval::{CancelReason, CancellationToken, RuntimeError, RuntimeErrorKind};
use flashshell_runtime::format::{
    FromJsonStep, FromTextStep, JsonMode, ToJsonStep, ToTextStep, from_json, from_text, to_json,
    to_text,
};
use flashshell_runtime::stream::ValueStream;
use flashshell_runtime::{Record, Table, Value};
use flashshell_syntax::{SourceFile, SourceId};

/// A byte-chunk source that hands out the given chunks in order, then ends.
fn chunks(chunks: Vec<Vec<u8>>) -> impl FnMut() -> Option<Vec<u8>> + 'static {
    let mut chunks = chunks.into_iter();
    move || chunks.next()
}

/// One whole document as a single chunk.
fn document(text: &str) -> impl FnMut() -> Option<Vec<u8>> + 'static {
    chunks(vec![text.as_bytes().to_vec()])
}

/// A generous budget for tests that are not about the bound.
const AMPLE: usize = 64 * 1024;

fn record(entries: Vec<(&str, Value)>) -> Record {
    Record::new(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .unwrap()
}

/// Parses one document and returns its single value, failing on any other step.
fn parse_one(text: &str) -> Value {
    let mut parser = from_json(JsonMode::Document, document(text), AMPLE);
    let value = match parser.pull() {
        FromJsonStep::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    };
    assert!(matches!(parser.pull(), FromJsonStep::End));
    value
}

/// Serializes one value and returns the emitted UTF-8 chunk.
fn render_one(value: Value) -> String {
    let mut writer = to_json(ValueStream::once(value));
    let chunk = match writer.pull() {
        ToJsonStep::Chunk(bytes) => bytes,
        other => panic!("expected a chunk, got {other:?}"),
    };
    assert!(matches!(writer.pull(), ToJsonStep::End));
    String::from_utf8(chunk).expect("JSON output is UTF-8")
}

#[test]
fn json_scalars_and_containers_map_onto_the_value_families() {
    assert_eq!(parse_one("null"), Value::Null);
    assert_eq!(parse_one("true"), Value::Bool(true));
    assert_eq!(parse_one(r#""text""#), Value::string("text"));

    // An integral number that fits i64 is an Int; anything else is a Float.
    assert_eq!(parse_one("42"), Value::Int(42));
    assert_eq!(parse_one("-7"), Value::Int(-7));
    assert_eq!(parse_one("2.5"), parse_one("2.5"));
    assert_eq!(parse_one("2.5").family_name(), "float");
    assert_eq!(parse_one("42").family_name(), "int");

    assert_eq!(
        parse_one("[1, \"two\", null]"),
        Value::list(vec![Value::Int(1), Value::string("two"), Value::Null])
    );

    // Object keys keep document order, because Record field order is observable.
    let parsed = parse_one(r#"{"second": 2, "first": 1}"#);
    let Value::Record(fields) = parsed else {
        panic!("expected a record");
    };
    assert_eq!(
        fields
            .entries()
            .iter()
            .map(|(key, _)| key.as_ref())
            .collect::<Vec<&str>>(),
        ["second", "first"]
    );

    // Nesting is recursive in both directions.
    assert_eq!(
        parse_one(r#"{"a": [{"b": 1}]}"#),
        Value::from(record(vec![(
            "a",
            Value::list(vec![Value::from(record(vec![("b", Value::Int(1))]))])
        )]))
    );
}

#[test]
fn a_malformed_document_reports_its_logical_byte_offset() {
    let mut parser = from_json(JsonMode::Document, document("{\"a\": }"), AMPLE);
    let FromJsonStep::Malformed { offset } = parser.pull() else {
        panic!("expected a malformed report");
    };
    // The offending byte is the `}` that should have opened a value.
    assert_eq!(offset, 6);

    // The offset is logical across the whole input, not per line.
    let mut multiline = from_json(JsonMode::Document, document("[\n  1,\n  @\n]"), AMPLE);
    let FromJsonStep::Malformed { offset } = multiline.pull() else {
        panic!("expected a malformed report");
    };
    assert_eq!(offset, 9);

    // A malformed report latches; further pulls stay End rather than re-parsing.
    assert!(matches!(multiline.pull(), FromJsonStep::End));
}

#[test]
fn duplicate_object_keys_are_reported_as_their_own_step() {
    // Well-formed JSON, but not a well-formed Record: the record contract makes
    // a duplicate key an error at the later key, so this is not a syntax error
    // and must not be reported with a byte offset.
    let mut parser = from_json(JsonMode::Document, document(r#"{"a": 1, "a": 2}"#), AMPLE);
    assert!(matches!(
        parser.pull(),
        FromJsonStep::DuplicateKey { ref key } if key == "a"
    ));
}

#[test]
fn array_mode_yields_one_step_per_element_and_refuses_a_non_array() {
    let mut parser = from_json(JsonMode::Array, document(r#"[1, "two", null]"#), AMPLE);
    assert!(matches!(parser.pull(), FromJsonStep::Value(Value::Int(1))));
    assert!(matches!(
        parser.pull(),
        FromJsonStep::Value(Value::String(_))
    ));
    assert!(matches!(parser.pull(), FromJsonStep::Value(Value::Null)));
    assert!(matches!(parser.pull(), FromJsonStep::End));
    assert!(matches!(parser.pull(), FromJsonStep::End));

    let mut empty = from_json(JsonMode::Array, document("[]"), AMPLE);
    assert!(matches!(empty.pull(), FromJsonStep::End));

    // A document that is not an array names the family that was found instead.
    let mut object = from_json(JsonMode::Array, document(r#"{"a": 1}"#), AMPLE);
    assert!(matches!(
        object.pull(),
        FromJsonStep::NotArray { actual: "record" }
    ));

    // Document mode accepts the same object as one value.
    let mut whole = from_json(JsonMode::Document, document(r#"{"a": 1}"#), AMPLE);
    assert!(matches!(
        whole.pull(),
        FromJsonStep::Value(Value::Record(_))
    ));
}

#[test]
fn the_byte_budget_refuses_an_oversized_document_without_truncating() {
    // A document of exactly the budget still parses: the bound is inclusive.
    let exact = "[1, 2, 3]";
    let mut fits = from_json(JsonMode::Document, document(exact), exact.len());
    assert!(matches!(fits.pull(), FromJsonStep::Value(Value::List(_))));

    // One byte over the budget is refused, and the source is not drained further.
    let mut exceeds = from_json(JsonMode::Document, document(exact), exact.len() - 1);
    assert!(matches!(
        exceeds.pull(),
        FromJsonStep::LimitExceeded { limit } if limit == exact.len() - 1
    ));

    // The bound counts bytes across chunks, not chunks.
    let mut split = from_json(
        JsonMode::Document,
        chunks(vec![b"[1, ".to_vec(), b"2, 3]".to_vec()]),
        4,
    );
    assert!(matches!(
        split.pull(),
        FromJsonStep::LimitExceeded { limit: 4 }
    ));

    // An unbounded producer is capped rather than exhausting memory.
    let mut endless = from_json(JsonMode::Document, || Some(vec![b' '; 128]), 1_024);
    assert!(matches!(
        endless.pull(),
        FromJsonStep::LimitExceeded { limit: 1_024 }
    ));
}

#[test]
fn a_document_split_across_chunks_is_joined_before_parsing() {
    let mut parser = from_json(
        JsonMode::Document,
        chunks(vec![
            br#"{"na"#.to_vec(),
            br#"me": "fs"#.to_vec(),
            br#"h"}"#.to_vec(),
        ]),
        AMPLE,
    );
    assert_eq!(
        parser.pull_value(),
        Value::from(record(vec![("name", Value::string("fsh"))]))
    );
}

#[test]
fn json_output_renders_every_encodable_family_and_refuses_the_rest() {
    assert_eq!(render_one(Value::Null), "null");
    assert_eq!(render_one(Value::Bool(false)), "false");
    assert_eq!(render_one(Value::Int(-7)), "-7");
    assert_eq!(render_one(Value::string("a\"b")), r#""a\"b""#);
    assert_eq!(
        render_one(Value::list(vec![Value::Int(1), Value::Null])),
        "[1,null]"
    );
    assert_eq!(
        render_one(Value::from(record(vec![
            ("b", Value::Int(2)),
            ("a", Value::Int(1)),
        ]))),
        r#"{"b":2,"a":1}"#
    );

    // A family JSON cannot represent is refused by name rather than invented.
    for value in [
        Value::Bytes(std::sync::Arc::from(b"\x00".as_slice())),
        Value::from(flashshell_runtime::Range::new(0, 3, false)),
    ] {
        let family = value.family_name();
        let mut writer = to_json(ValueStream::once(value));
        assert!(
            matches!(writer.pull(), ToJsonStep::NotEncodable { actual } if actual == family),
            "{family} must not be encodable"
        );
    }
}

#[test]
fn a_table_renders_as_an_array_of_row_objects() {
    let table = Table::new(
        vec!["name".to_owned(), "size".to_owned()],
        vec![
            vec![Value::string("a"), Value::Int(1)],
            vec![Value::string("b"), Value::Null],
        ],
    )
    .unwrap();
    assert_eq!(
        render_one(Value::from(table)),
        r#"[{"name":"a","size":1},{"name":"b","size":null}]"#
    );

    // The documented cost of the interoperable shape: a table with columns but
    // no rows loses its column names, because the array form has nowhere to put
    // them. Recorded as a limitation rather than silently surprising.
    let empty = Table::new(vec!["only".to_owned()], Vec::new()).unwrap();
    assert_eq!(render_one(Value::from(empty)), "[]");
}

#[test]
fn json_output_is_lazy_and_emits_one_chunk_per_value() {
    let mut counter = 0_i64;
    let source = ValueStream::from_fn(move || {
        counter += 1;
        Some(Ok(Value::Int(counter)))
    });

    let mut writer = to_json(source);
    assert!(matches!(writer.pull(), ToJsonStep::Chunk(ref bytes) if bytes == b"1"));
    assert!(matches!(writer.pull(), ToJsonStep::Chunk(ref bytes) if bytes == b"2"));
    // An unbounded source stayed bounded because only two values were pulled.
}

#[test]
fn both_directions_pass_upstream_failure_and_cancellation_through() {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", "x");
    let span = file.span(0..1).unwrap();
    let boom = RuntimeError::new(RuntimeErrorKind::ExecutionUnsupported, span);
    let expected = boom.clone();
    let mut produced = false;
    let mut failing = to_json(ValueStream::from_fn(move || {
        if produced {
            None
        } else {
            produced = true;
            Some(Err(boom.clone()))
        }
    }));
    match failing.pull() {
        ToJsonStep::Failed(error) => assert_eq!(error, expected),
        other => panic!("expected the upstream failure, got {other:?}"),
    }

    // An already-tripped token stops the upstream stream at its next pull, so the
    // pending value is never serialized.
    let token = CancellationToken::from_fn(|| true);
    let mut cancelled =
        to_json(ValueStream::from_values(vec![Value::Int(1)]).with_cancellation(token));
    assert!(matches!(
        cancelled.pull(),
        ToJsonStep::Cancelled(CancelReason::Requested)
    ));

    // A zero budget refuses before any byte is read, on the parse side.
    let mut refused = from_json(JsonMode::Document, document("[1]"), 0);
    assert!(matches!(
        refused.pull(),
        FromJsonStep::LimitExceeded { limit: 0 }
    ));
}

/// A generous single-line bound for tests that are not about the bound.
const AMPLE_LINE: usize = 4 * 1024;

/// Pulls one line from a text parser, failing on any other step.
fn text_line(parser: &mut flashshell_runtime::format::FromText) -> String {
    match parser.pull() {
        FromTextStep::Line(Value::String(text)) => text.as_ref().to_owned(),
        other => panic!("expected a line, got {other:?}"),
    }
}

#[test]
fn the_text_format_splits_lines_exactly_as_the_lines_command_does() {
    let mut parser = from_text(document("first\nsecond\n"), AMPLE_LINE);
    assert_eq!(text_line(&mut parser), "first");
    assert_eq!(text_line(&mut parser), "second");
    // A trailing terminator emits no empty final line.
    assert!(matches!(parser.pull(), FromTextStep::End));
    assert!(matches!(parser.pull(), FromTextStep::End));

    // An unterminated final line is still flushed.
    let mut unterminated = from_text(document("only"), AMPLE_LINE);
    assert_eq!(text_line(&mut unterminated), "only");
    assert!(matches!(unterminated.pull(), FromTextStep::End));

    // CRLF and a trailing CR on the flushed line leave no carriage return, while
    // a lone interior CR is ordinary content and blank lines are preserved.
    let mut endings = from_text(document("a\r\n\r\nb\rc\r"), AMPLE_LINE);
    assert_eq!(text_line(&mut endings), "a");
    assert_eq!(text_line(&mut endings), "");
    assert_eq!(text_line(&mut endings), "b\rc");
    assert!(matches!(endings.pull(), FromTextStep::End));

    // A line split across chunks is joined.
    let mut split = from_text(
        chunks(vec![b"beg".to_vec(), b"in\nen".to_vec(), b"d\n".to_vec()]),
        AMPLE_LINE,
    );
    assert_eq!(text_line(&mut split), "begin");
    assert_eq!(text_line(&mut split), "end");
    assert!(matches!(split.pull(), FromTextStep::End));

    // An empty source is End, not one empty line.
    let mut empty = from_text(chunks(Vec::new()), AMPLE_LINE);
    assert!(matches!(empty.pull(), FromTextStep::End));
}

#[test]
fn the_text_format_is_lazy_and_bounds_one_unterminated_line() {
    // Streaming: a bounded reader over an endless source stays bounded, because
    // the format needs no whole-document budget.
    let mut endless = from_text(|| Some(b"line\n".to_vec()), AMPLE_LINE);
    assert_eq!(text_line(&mut endless), "line");
    assert_eq!(text_line(&mut endless), "line");

    // But a single line that never terminates would materialize without bound,
    // so it is refused rather than truncated.
    let mut runaway = from_text(|| Some(vec![b'x'; 64]), 128);
    assert!(matches!(
        runaway.pull(),
        FromTextStep::LineTooLong { limit: 128 }
    ));

    // A line of exactly the bound is accepted: the bound is inclusive.
    let exact = "x".repeat(8);
    let mut fits = from_text(document(&format!("{exact}\n")), 8);
    assert_eq!(text_line(&mut fits), exact);
}

#[test]
fn the_text_format_decodes_strictly_and_reports_a_malformed_offset() {
    let mut parser = from_text(chunks(vec![b"ok\n\xff".to_vec()]), AMPLE_LINE);
    assert_eq!(text_line(&mut parser), "ok");
    let FromTextStep::Malformed { offset } = parser.pull() else {
        panic!("expected a malformed report");
    };
    assert_eq!(offset, 3);
    // The report latches.
    assert!(matches!(parser.pull(), FromTextStep::End));
}

#[test]
fn text_output_writes_one_terminated_line_per_value() {
    let mut writer = to_text(ValueStream::from_values(vec![
        Value::string("first"),
        Value::Int(2),
        Value::Bool(true),
    ]));
    assert!(matches!(writer.pull(), ToTextStep::Chunk(ref bytes) if bytes == b"first\n"));
    assert!(matches!(writer.pull(), ToTextStep::Chunk(ref bytes) if bytes == b"2\n"));
    assert!(matches!(writer.pull(), ToTextStep::Chunk(ref bytes) if bytes == b"true\n"));
    assert!(matches!(writer.pull(), ToTextStep::End));

    // A string containing a newline is written through unchanged: `to text` is a
    // line-oriented writer, not an escaping serializer.
    let mut embedded = to_text(ValueStream::once(Value::string("a\nb")));
    assert!(matches!(embedded.pull(), ToTextStep::Chunk(ref bytes) if bytes == b"a\nb\n"));
}

#[test]
fn text_output_accepts_exactly_the_families_that_are_word_eligible() {
    // The `text` format and argv share one eligibility rule: a value that can
    // become a command argument can be written as a line, and nothing else can.
    // Terminal rendering of a compound value is never a serialization path, and
    // this writer is the second of the two doors that keep it that way.
    for value in [
        Value::Bool(true),
        Value::Int(1),
        Value::string("text"),
        Value::from(flashshell_runtime::ByteSize::new(4)),
    ] {
        let family = value.family_name();
        let mut writer = to_text(ValueStream::once(value));
        assert!(
            matches!(writer.pull(), ToTextStep::Chunk(_)),
            "{family} must be writable as a line"
        );
    }

    for value in [
        Value::Null,
        Value::list(vec![Value::Int(1)]),
        Value::from(record(vec![("a", Value::Int(1))])),
        Value::from(Table::new(vec!["a".to_owned()], vec![vec![Value::Int(1)]]).unwrap()),
        Value::Bytes(std::sync::Arc::from(b"raw".as_slice())),
    ] {
        let family = value.family_name();
        let mut writer = to_text(ValueStream::once(value));
        assert!(
            matches!(writer.pull(), ToTextStep::NotEncodable { actual } if actual == family),
            "{family} must not be writable as a line"
        );
    }
}

#[test]
fn text_output_passes_upstream_failure_and_cancellation_through() {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", "x");
    let span = file.span(0..1).unwrap();
    let boom = RuntimeError::new(RuntimeErrorKind::ExecutionUnsupported, span);
    let expected = boom.clone();
    let mut produced = false;
    let mut failing = to_text(ValueStream::from_fn(move || {
        if produced {
            None
        } else {
            produced = true;
            Some(Err(boom.clone()))
        }
    }));
    match failing.pull() {
        ToTextStep::Failed(error) => assert_eq!(error, expected),
        other => panic!("expected the upstream failure, got {other:?}"),
    }

    let token = CancellationToken::from_fn(|| true);
    let mut cancelled =
        to_text(ValueStream::from_values(vec![Value::string("later")]).with_cancellation(token));
    assert!(matches!(
        cancelled.pull(),
        ToTextStep::Cancelled(CancelReason::Requested)
    ));
}
