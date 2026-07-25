#![forbid(unsafe_code)]

//! Acceptance coverage for the explicit codec boundary — `decode <codec>` and
//! `encode <codec>` — that crosses between a `ByteStream` and text or byte
//! values. The layer is host-free and span-independent, matching `resolve` and
//! `stream`: no process, terminal, or clock participates, and a malformed input
//! reports only its logical byte offset while the executor attaches a source
//! span at the pipeline boundary later.
//!
//! `decode` streams a lazy byte-chunk source into values; UTF-8 carries an
//! incomplete trailing multibyte sequence across chunk boundaries. `encode` is a
//! transformer over a `ValueStream`, serializing each value into a byte chunk and
//! passing an upstream producer error or cancellation through unchanged. The
//! `from`/`to` format boundary is separate; only its carrier contract is asserted
//! here (in `builtins.rs`).

use flashshell_runtime::Value;
use flashshell_runtime::convert::{Codec, DecodeStep, EncodeStep, decode, encode};
use flashshell_runtime::eval::{CancelReason, CancellationToken, RuntimeError, RuntimeErrorKind};
use flashshell_runtime::stream::ValueStream;
use flashshell_syntax::{SourceFile, SourceId};

/// A byte-chunk source that hands out the given chunks in order, then ends.
fn chunks(chunks: Vec<Vec<u8>>) -> impl FnMut() -> Option<Vec<u8>> + 'static {
    let mut chunks = chunks.into_iter();
    move || chunks.next()
}

/// Pulls one `Value` from a decoder, failing on any other step.
fn decode_value(decoder: &mut flashshell_runtime::convert::Decoder) -> Value {
    match decoder.pull() {
        DecodeStep::Value(value) => value,
        other => panic!("expected a decoded value, got {other:?}"),
    }
}

/// Pulls one byte chunk from an encoder, failing on any other step.
fn encode_chunk(encoder: &mut flashshell_runtime::convert::Encoder) -> Vec<u8> {
    match encoder.pull() {
        EncodeStep::Chunk(bytes) => bytes,
        other => panic!("expected an encoded chunk, got {other:?}"),
    }
}

#[test]
fn decode_utf8_yields_a_string_value_per_valid_chunk() {
    let mut decoder = decode(
        Codec::Utf8 { lossy: false },
        chunks(vec![b"hello".to_vec(), b" world".to_vec()]),
    );
    assert_eq!(decode_value(&mut decoder), Value::string("hello"));
    assert_eq!(decode_value(&mut decoder), Value::string(" world"));
    assert!(matches!(decoder.pull(), DecodeStep::End));
    // Exhaustion is stable.
    assert!(matches!(decoder.pull(), DecodeStep::End));
}

#[test]
fn decode_utf8_carries_a_split_multibyte_sequence_across_chunks() {
    // U+00E9 (é) is 0xC3 0xA9. Split it across two chunks: the first chunk ends
    // with an incomplete sequence that must be held until the next chunk arrives,
    // never emitted as malformed and never as an empty string.
    let mut decoder = decode(
        Codec::Utf8 { lossy: false },
        chunks(vec![vec![b'a', 0xC3], vec![0xA9, b'b']]),
    );
    // The first pull consumes both chunks because the first ends mid-sequence.
    assert_eq!(decode_value(&mut decoder), Value::string("a"));
    assert_eq!(decode_value(&mut decoder), Value::string("\u{00E9}b"));
    assert!(matches!(decoder.pull(), DecodeStep::End));
}

#[test]
fn decode_utf8_strict_reports_the_offset_of_malformed_input() {
    // 0xFF is never valid UTF-8. Its logical offset is 3 (after "abc").
    let mut decoder = decode(
        Codec::Utf8 { lossy: false },
        chunks(vec![vec![b'a', b'b', b'c', 0xFF, b'd']]),
    );
    assert_eq!(decode_value(&mut decoder), Value::string("abc"));
    assert!(matches!(
        decoder.pull(),
        DecodeStep::Malformed { offset: 3 }
    ));
}

#[test]
fn decode_utf8_strict_reports_an_incomplete_sequence_at_end_of_input() {
    // A lone leading byte at end of input is malformed, not silently dropped.
    let mut decoder = decode(Codec::Utf8 { lossy: false }, chunks(vec![vec![b'a', 0xC3]]));
    assert_eq!(decode_value(&mut decoder), Value::string("a"));
    assert!(matches!(
        decoder.pull(),
        DecodeStep::Malformed { offset: 1 }
    ));
}

#[test]
fn decode_utf8_lossy_substitutes_the_replacement_character() {
    let mut decoder = decode(
        Codec::Utf8 { lossy: true },
        chunks(vec![vec![b'a', 0xFF, b'b']]),
    );
    // The malformed byte becomes U+FFFD and decoding continues rather than failing.
    assert_eq!(decode_value(&mut decoder), Value::string("a\u{FFFD}b"));
    assert!(matches!(decoder.pull(), DecodeStep::End));
}

#[test]
fn decode_bytes_yields_one_bytes_value_per_chunk() {
    let mut decoder = decode(Codec::Bytes, chunks(vec![vec![0x00, 0xFF], vec![0x01]]));
    // The byte-preserving codec never interprets the bytes as text; an otherwise
    // malformed UTF-8 sequence passes through unchanged.
    assert_eq!(decode_value(&mut decoder), Value::bytes(vec![0x00, 0xFF]));
    assert_eq!(decode_value(&mut decoder), Value::bytes(vec![0x01]));
    assert!(matches!(decoder.pull(), DecodeStep::End));
}

#[test]
fn encode_utf8_serializes_string_values_to_byte_chunks() {
    let input = ValueStream::from_values(vec![Value::string("hi"), Value::string(" there")]);
    let mut encoder = encode(Codec::Utf8 { lossy: false }, input);
    assert_eq!(encode_chunk(&mut encoder), b"hi".to_vec());
    assert_eq!(encode_chunk(&mut encoder), b" there".to_vec());
    assert!(matches!(encoder.pull(), EncodeStep::End));
}

#[test]
fn encode_utf8_refuses_a_non_text_value() {
    let input = ValueStream::from_values(vec![Value::Int(7)]);
    let mut encoder = encode(Codec::Utf8 { lossy: false }, input);
    assert!(matches!(
        encoder.pull(),
        EncodeStep::NotEncodable { actual } if actual == Value::Int(7).family_name()
    ));
}

#[test]
fn encode_bytes_serializes_bytes_values_and_refuses_text() {
    let input = ValueStream::from_values(vec![Value::bytes(vec![0xDE, 0xAD])]);
    let mut encoder = encode(Codec::Bytes, input);
    assert_eq!(encode_chunk(&mut encoder), vec![0xDE, 0xAD]);
    assert!(matches!(encoder.pull(), EncodeStep::End));

    let text = ValueStream::from_values(vec![Value::string("no")]);
    let mut encoder = encode(Codec::Bytes, text);
    assert!(matches!(
        encoder.pull(),
        EncodeStep::NotEncodable { actual } if actual == Value::string("no").family_name()
    ));
}

#[test]
fn encode_passes_an_upstream_producer_failure_through() {
    let file = SourceFile::new(SourceId::new(1), "test.fsh", "x");
    let span = file.span(0..1).unwrap();
    let boom = RuntimeError::new(RuntimeErrorKind::ExecutionUnsupported, span);
    let expected = boom.clone();
    let mut produced = false;
    let input = ValueStream::from_fn(move || {
        if produced {
            None
        } else {
            produced = true;
            Some(Err(boom.clone()))
        }
    });
    let mut encoder = encode(Codec::Utf8 { lossy: false }, input);
    match encoder.pull() {
        EncodeStep::Failed(error) => assert_eq!(error, expected),
        other => panic!("expected the upstream failure, got {other:?}"),
    }
}

#[test]
fn encode_passes_an_upstream_cancellation_through() {
    // An already-tripped token stops the upstream stream at its next pull, so the
    // pending value is never encoded.
    let token = CancellationToken::from_fn(|| true);
    let input = ValueStream::from_values(vec![Value::string("later")]).with_cancellation(token);
    let mut encoder = encode(Codec::Utf8 { lossy: false }, input);
    assert!(matches!(
        encoder.pull(),
        EncodeStep::Cancelled(CancelReason::Requested)
    ));
}
