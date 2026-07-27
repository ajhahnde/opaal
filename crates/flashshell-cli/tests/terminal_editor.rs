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

use flashshell_cli::terminal_editor::buffer::EditBuffer;

#[test]
fn inserting_advances_the_cursor() {
    let mut buffer = EditBuffer::new();

    buffer.insert('a');
    buffer.insert('b');

    assert_eq!(buffer.text(), "ab");
    assert_eq!(buffer.cursor_chars(), 2);
}

#[test]
fn inserting_at_the_cursor_splits_the_text() {
    let mut buffer = EditBuffer::from_text("ac");
    buffer.move_left();

    buffer.insert('b');

    assert_eq!(buffer.text(), "abc");
    assert_eq!(buffer.cursor_chars(), 2);
}

#[test]
fn movement_steps_whole_characters() {
    let mut buffer = EditBuffer::from_text("äb");

    buffer.move_home();
    assert_eq!(buffer.cursor(), 0);
    assert!(buffer.move_right());
    // 'ä' occupies two bytes, so one character step moves the byte cursor by two.
    assert_eq!(buffer.cursor(), 2);
    assert_eq!(buffer.cursor_chars(), 1);
}

#[test]
fn movement_at_the_edges_reports_no_change() {
    let mut buffer = EditBuffer::from_text("a");

    buffer.move_home();
    assert!(!buffer.move_left());
    buffer.move_end();
    assert!(!buffer.move_right());
}

#[test]
fn backspace_removes_the_whole_preceding_character() {
    let mut buffer = EditBuffer::from_text("aä");

    assert!(buffer.backspace());

    assert_eq!(buffer.text(), "a");
    assert_eq!(buffer.cursor_chars(), 1);
}

#[test]
fn backspace_at_the_start_reports_no_change() {
    let mut buffer = EditBuffer::from_text("a");
    buffer.move_home();

    assert!(!buffer.backspace());
    assert_eq!(buffer.text(), "a");
}

#[test]
fn delete_removes_the_character_under_the_cursor() {
    let mut buffer = EditBuffer::from_text("abc");
    buffer.move_home();

    assert!(buffer.delete());

    assert_eq!(buffer.text(), "bc");
    assert_eq!(buffer.cursor(), 0);
}

#[test]
fn delete_at_the_end_reports_no_change() {
    let mut buffer = EditBuffer::from_text("abc");

    assert!(!buffer.delete());
    assert_eq!(buffer.text(), "abc");
}

#[test]
fn kill_to_end_drops_the_tail() {
    let mut buffer = EditBuffer::from_text("abcd");
    buffer.move_home();
    buffer.move_right();

    buffer.kill_to_end();

    assert_eq!(buffer.text(), "a");
    assert_eq!(buffer.cursor_chars(), 1);
}

#[test]
fn kill_to_start_drops_the_head_and_rewinds() {
    let mut buffer = EditBuffer::from_text("abcd");
    buffer.move_home();
    buffer.move_right();
    buffer.move_right();

    buffer.kill_to_start();

    assert_eq!(buffer.text(), "cd");
    assert_eq!(buffer.cursor(), 0);
}

#[test]
fn kill_word_back_removes_trailing_space_and_one_word() {
    let mut buffer = EditBuffer::from_text("echo hallo  ");

    buffer.kill_word_back();

    assert_eq!(buffer.text(), "echo ");
    assert_eq!(buffer.cursor_chars(), 5);
}

#[test]
fn kill_word_back_on_leading_space_only_empties_the_buffer() {
    let mut buffer = EditBuffer::from_text("   ");

    buffer.kill_word_back();

    assert_eq!(buffer.text(), "");
    assert!(buffer.is_empty());
}
