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

use flashshell_cli::terminal_editor::history::HistoryRing;
use flashshell_cli::terminal_editor::render::render_line;

#[test]
fn recall_walks_backwards_from_the_newest_entry() {
    let mut history = HistoryRing::new(16);
    history.record("first");
    history.record("second");

    assert_eq!(history.recall_previous("draft").as_deref(), Some("second"));
    assert_eq!(history.recall_previous("draft").as_deref(), Some("first"));
    assert_eq!(history.recall_previous("draft"), None);
}

#[test]
fn walking_forward_returns_the_parked_draft_last() {
    let mut history = HistoryRing::new(16);
    history.record("first");

    assert_eq!(history.recall_previous("draft").as_deref(), Some("first"));
    assert_eq!(history.recall_next().as_deref(), Some("draft"));
    assert_eq!(history.recall_next(), None);
}

#[test]
fn walking_forward_steps_through_the_entries_before_the_draft() {
    let mut history = HistoryRing::new(16);
    history.record("first");
    history.record("second");

    assert_eq!(history.recall_previous("draft").as_deref(), Some("second"));
    assert_eq!(history.recall_previous("draft").as_deref(), Some("first"));
    // The step back off the oldest entry must land on the entry between it and
    // the draft, not on the draft and not on the wrong end of the ring.
    assert_eq!(history.recall_next().as_deref(), Some("second"));
    assert_eq!(history.recall_next().as_deref(), Some("draft"));
}

#[test]
fn resetting_the_position_abandons_the_recall_and_the_draft() {
    let mut history = HistoryRing::new(16);
    history.record("first");
    let _ = history.recall_previous("draft");

    history.reset_position();

    assert_eq!(history.recall_next(), None);
    assert_eq!(history.recall_previous("new").as_deref(), Some("first"));
    // Whether the reset also cleared the parked draft is not observable here,
    // and not observable at all: the next recall re-parks it unconditionally.
    // What this pins is that the reset rewound the position.
    assert_eq!(history.recall_next().as_deref(), Some("new"));
}

#[test]
fn adjacent_duplicate_entries_are_dropped() {
    let mut history = HistoryRing::new(16);
    history.record("same");
    history.record("same");

    assert_eq!(history.len(), 1);
}

#[test]
fn a_non_adjacent_repeat_is_kept() {
    let mut history = HistoryRing::new(16);
    history.record("a");
    history.record("b");
    history.record("a");

    assert_eq!(history.len(), 3);
}

#[test]
fn an_empty_entry_is_not_recorded() {
    let mut history = HistoryRing::new(16);
    history.record("");

    assert!(history.is_empty());
}

#[test]
fn the_ring_drops_the_oldest_entry_past_capacity() {
    let mut history = HistoryRing::new(2);
    history.record("a");
    history.record("b");
    history.record("c");

    assert_eq!(history.len(), 2);
    assert_eq!(history.recall_previous("").as_deref(), Some("c"));
    assert_eq!(history.recall_previous("").as_deref(), Some("b"));
    assert_eq!(history.recall_previous(""), None);
}

#[test]
fn recording_resets_the_recall_position() {
    let mut history = HistoryRing::new(16);
    history.record("first");
    let _ = history.recall_previous("draft");

    history.record("second");

    assert_eq!(history.recall_previous("new").as_deref(), Some("second"));
}

#[test]
fn a_short_line_renders_prompt_text_and_cursor() {
    let rendered = render_line("fsh> ", "echo", 4, 80);

    assert_eq!(rendered, "\r\x1b[Kfsh> echo\r\x1b[10G");
}

#[test]
fn the_cursor_column_follows_the_cursor_position() {
    let rendered = render_line("fsh> ", "echo", 0, 80);

    assert_eq!(rendered, "\r\x1b[Kfsh> echo\r\x1b[6G");
}

#[test]
fn a_wide_line_scrolls_horizontally_and_keeps_the_cursor_visible() {
    // Prompt 5 + 20 characters of text does not fit 15 columns. The prompt
    // takes columns 1..=5, leaving ten cells; the cursor sits past the last
    // character and claims the tenth, so nine characters stay visible.
    let text = "abcdefghijklmnopqrst";
    let rendered = render_line("fsh> ", text, 20, 15);

    assert_eq!(rendered, "\r\x1b[Kfsh> lmnopqrst\r\x1b[15G");
}

#[test]
fn a_cursor_inside_the_text_fills_the_whole_window() {
    // The reserved trailing cell only costs a character when the cursor sits
    // past the end. With the cursor inside the text all ten cells carry one.
    let text = "abcdefghijklmnopqrst";
    let rendered = render_line("fsh> ", text, 5, 15);

    assert_eq!(rendered, "\r\x1b[Kfsh> abcdefghij\r\x1b[11G");
}

#[test]
fn a_prompt_wider_than_the_terminal_renders_no_text() {
    // A five-column prompt does not fit four columns. The prompt is written
    // whole and the cursor column runs past the row rather than being clamped:
    // a terminal this narrow cannot show an edit line at all, and pinning the
    // behaviour is worth more than pretending it degrades gracefully.
    let rendered = render_line("fsh> ", "abc", 3, 4);

    assert_eq!(rendered, "\r\x1b[Kfsh> \r\x1b[6G");
}
