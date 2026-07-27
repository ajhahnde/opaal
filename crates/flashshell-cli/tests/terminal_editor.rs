//! Unit coverage for the raw-mode editor's pure parts.

use flashshell_cli::terminal_editor::key::{Key, KeyDecoder};

/// Feed every byte of `input` and collect the keys that emerge.
fn decode(input: &[u8]) -> Vec<Key> {
    let mut decoder = KeyDecoder::new();
    input
        .iter()
        .filter_map(|byte| decoder.push(*byte))
        .collect()
}

#[test]
fn ascii_bytes_decode_to_characters() {
    assert_eq!(decode(b"ab"), vec![Key::Char('a'), Key::Char('b')]);
}

#[test]
fn a_multibyte_character_split_across_pushes_decodes_once() {
    // U+00E4 LATIN SMALL LETTER A WITH DIAERESIS is 0xC3 0xA4.
    assert_eq!(decode(&[0xC3, 0xA4]), vec![Key::Char('ä')]);
}

#[test]
fn an_invalid_utf8_sequence_is_discarded() {
    assert_eq!(decode(&[0xFF, b'a']), vec![Key::Char('a')]);
}

#[test]
fn arrow_sequences_decode_to_movement_keys() {
    assert_eq!(
        decode(b"\x1b[D\x1b[C\x1b[A\x1b[B"),
        vec![Key::Left, Key::Right, Key::Up, Key::Down]
    );
}

#[test]
fn a_csi_sequence_split_across_pushes_decodes_once() {
    let mut decoder = KeyDecoder::new();

    assert_eq!(decoder.push(0x1b), None);
    assert_eq!(decoder.push(b'['), None);
    assert_eq!(decoder.push(b'3'), None);
    assert_eq!(decoder.push(b'~'), Some(Key::Delete));
}

#[test]
fn home_and_end_decode_from_both_spellings() {
    assert_eq!(
        decode(b"\x1b[H\x1b[F\x1b[1~\x1b[4~\x01\x05"),
        vec![
            Key::Home,
            Key::End,
            Key::Home,
            Key::End,
            Key::Home,
            Key::End
        ]
    );
}

#[test]
fn control_bytes_decode_to_their_editing_keys() {
    assert_eq!(
        decode(b"\x7f\x08\x0b\x15\x17\x03\x04\x0d\x0a"),
        vec![
            Key::Backspace,
            Key::Backspace,
            Key::KillToEnd,
            Key::KillToStart,
            Key::KillWordBack,
            Key::Cancel,
            Key::EndOfFileOrDelete,
            Key::Enter,
            Key::Enter
        ]
    );
}

#[test]
fn an_unknown_escape_sequence_produces_no_key() {
    assert_eq!(decode(b"\x1b[99Z"), vec![]);
}

#[test]
fn a_lone_escape_followed_by_text_does_not_swallow_the_text() {
    // An unterminated CSI is abandoned when a new escape starts.
    assert_eq!(decode(b"\x1b[\x1b[Da"), vec![Key::Left, Key::Char('a')]);
}

#[test]
fn a_lone_escape_followed_by_an_ordinary_character_yields_that_character() {
    // ESC not followed by a recognized sequence family is abandoned, and the
    // byte that follows it is reinterpreted rather than swallowed.
    assert_eq!(decode(&[0x1b, b'x']), vec![Key::Char('x')]);
}

#[test]
fn a_csi_parameter_run_that_exceeds_the_cap_is_abandoned_and_recovers() {
    // A noisy serial link could otherwise drive the parameter buffer without
    // bound. Feed far more digits than any real CSI parameter carries, then
    // a complete, recognized sequence, and check the decoder still recovers.
    let mut input = vec![0x1b, b'['];
    input.extend(std::iter::repeat_n(b'9', 64));
    input.extend_from_slice(b"\x1b[D");

    let keys = decode(&input);

    // Once the run is abandoned, stray digits fall back to ordinary
    // characters; what matters is that the trailing sequence still decodes.
    assert_eq!(keys.last(), Some(&Key::Left));
    assert!(
        keys.iter()
            .all(|key| matches!(key, Key::Char('9') | Key::Left))
    );
}

#[test]
fn a_truncated_multibyte_leader_is_dropped_and_the_next_one_still_decodes() {
    // The first leader byte is abandoned because another leader arrives
    // before its continuation byte; the following character still decodes.
    assert_eq!(decode(&[0xC3, 0xC3, 0xA4]), vec![Key::Char('ä')]);
}
