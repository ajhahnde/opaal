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
    let rendered = render_line(">> ", "echo", 4, 80);

    assert_eq!(rendered, "\r\x1b[K>> echo\r\x1b[8G");
}

#[test]
fn the_cursor_column_follows_the_cursor_position() {
    let rendered = render_line(">> ", "echo", 0, 80);

    assert_eq!(rendered, "\r\x1b[K>> echo\r\x1b[4G");
}

#[test]
fn a_wide_line_scrolls_horizontally_and_keeps_the_cursor_visible() {
    // Prompt 3 + 20 characters of text does not fit 15 columns. The prompt
    // takes columns 1..=3, leaving twelve cells; the cursor sits past the last
    // character and claims the twelfth, so eleven characters stay visible.
    let text = "abcdefghijklmnopqrst";
    let rendered = render_line(">> ", text, 20, 15);

    assert_eq!(rendered, "\r\x1b[K>> jklmnopqrst\r\x1b[15G");
}

#[test]
fn a_cursor_inside_the_text_fills_the_whole_window() {
    // The reserved trailing cell only costs a character when the cursor sits
    // past the end. With the cursor inside the text all twelve cells carry one.
    let text = "abcdefghijklmnopqrst";
    let rendered = render_line(">> ", text, 5, 15);

    assert_eq!(rendered, "\r\x1b[K>> abcdefghijkl\r\x1b[9G");
}

#[test]
fn a_prompt_wider_than_the_terminal_renders_no_text() {
    // A three-column prompt fits four columns. The prompt is written
    // and one cell remains for text, but the cursor is past the end of a
    // three-character string so it runs past the terminal width.
    let rendered = render_line(">> ", "abc", 3, 4);

    assert_eq!(rendered, "\r\x1b[K>> \r\x1b[4G");
}

#[test]
fn editing_mid_line_keeps_text_visible_to_the_right_of_the_cursor() {
    // Cursor at character 15 of 20 in a ten-cell window. Anchoring the window
    // on the cursor alone would end it there and show "ghijklmnop" at column
    // 15, hiding every character still to come. The quarter-row margin shifts
    // the window two cells right, so "qr" stays on screen while editing.
    let text = "abcdefghijklmnopqrst";
    let rendered = render_line(">> ", text, 15, 15);

    assert_eq!(rendered, "\r\x1b[K>> hijklmnopqrs\r\x1b[12G");
}

#[test]
fn a_control_character_never_reaches_the_drawn_row() {
    // A recalled multi-line submission is the reachable source. Raw newlines
    // would walk the terminal down a row and strand the absolute column
    // request that follows, so they are drawn as spaces.
    let rendered = render_line(">> ", "if true {\nprintln\n}", 19, 80);

    assert_eq!(rendered, "\r\x1b[K>> if true { println }\r\x1b[23G");

    let pasted = render_line(">> ", "a\u{85}b", 3, 80);

    assert_eq!(pasted, "\r\x1b[K>> a b\r\x1b[7G");
}

use flashshell_cli::editor::{EditorEvent, EditorPrompt, LineEditor};
use flashshell_cli::terminal_editor::TerminalEditor;
use flashshell_platform::{
    Capabilities, FakePlatform, RecordingPlatform, TerminalCallLog, TerminalSize,
};

/// A terminal-free editor over scripted input, collecting everything drawn.
fn editor(input: &'static [u8]) -> TerminalEditor<FakePlatform, &'static [u8], Vec<u8>> {
    let platform =
        FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(80, 24));
    TerminalEditor::new(platform, input, Vec::new())
}

/// The same editor over a platform that records what the editor asked it for.
///
/// The editor takes its platform by value, so the log handle is taken before
/// the move and read back through the returned pair.
fn recording_editor(
    platform: FakePlatform,
    input: &'static [u8],
) -> (
    TerminalEditor<RecordingPlatform, &'static [u8], Vec<u8>>,
    TerminalCallLog,
) {
    let recording = RecordingPlatform::new(platform);
    let log = recording.log();
    (TerminalEditor::new(recording, input, Vec::new()), log)
}

#[test]
fn a_typed_line_submits_its_text() {
    let mut editor = editor(b"echo hallo\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("echo hallo".to_owned()));
}

#[test]
fn a_notice_is_written_through_the_editors_retained_output() {
    let mut editor = editor(b"");

    editor
        .write_notice("[1] Done     command\n")
        .expect("notice output should be writable");

    assert_eq!(editor.drawn(), b"[1] Done     command\n");
}

#[test]
fn backspace_edits_the_submitted_text() {
    let mut editor = editor(b"echo hallo\x7f\x7fx\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("echo halx".to_owned()));
}

#[test]
fn a_left_arrow_inserts_before_the_final_character() {
    let mut editor = editor(b"ac\x1b[Db\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("abc".to_owned()));
}

#[test]
fn incomplete_source_keeps_reading_under_the_continuation_prompt() {
    let mut editor = editor(b"if true {\rprintln\r}\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(
        event,
        EditorEvent::Submitted("if true {\nprintln\n}".to_owned())
    );
    assert!(
        String::from_utf8_lossy(editor.drawn()).contains("...> "),
        "the continuation prompt is drawn"
    );
}

#[test]
fn ctrl_c_cancels_the_line() {
    let mut editor = editor(b"echo hallo\x03");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Cancelled);
}

#[test]
fn ctrl_d_on_an_empty_buffer_is_end_of_input() {
    let mut editor = editor(b"\x04");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::EndOfInput);
}

#[test]
fn ctrl_d_on_a_non_empty_buffer_deletes_forward() {
    let mut editor = editor(b"abc\x1b[D\x04\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("ab".to_owned()));
}

#[test]
fn the_up_arrow_recalls_the_previous_submission() {
    let mut editor = editor(b"echo one\r\x1b[A\r");

    let first = editor.read_line(&EditorPrompt::default()).unwrap();
    let second = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(first, EditorEvent::Submitted("echo one".to_owned()));
    assert_eq!(second, EditorEvent::Submitted("echo one".to_owned()));
}

#[test]
fn input_exhaustion_with_an_empty_buffer_is_end_of_input() {
    let mut editor = editor(b"");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::EndOfInput);
}

#[test]
fn the_prompt_is_drawn_before_input_is_read() {
    let mut editor = editor(b"a\r");

    let _ = editor.read_line(&EditorPrompt::default()).unwrap();

    assert!(String::from_utf8_lossy(editor.drawn()).contains(">> "));
}

/// Each key must reach the buffer operation it names.
///
/// The decoder tests pin bytes to `Key` values and the buffer tests pin the
/// operations, but neither sees the wiring between them: a swapped or dropped
/// match arm compiles and passes both suites. These drive one key end to end
/// through `read_line` and read the wiring off the submitted text.
#[test]
fn home_moves_the_cursor_before_the_first_character() {
    let mut editor = editor(b"bc\x01a\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("abc".to_owned()));
}

#[test]
fn end_moves_the_cursor_past_the_last_character() {
    let mut editor = editor(b"ab\x01x\x05y\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("xaby".to_owned()));
}

#[test]
fn the_right_arrow_steps_towards_the_end() {
    let mut editor = editor(b"abc\x01\x1b[Cx\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("axbc".to_owned()));
}

#[test]
fn delete_removes_the_character_under_the_cursor_while_editing() {
    let mut editor = editor(b"abc\x01\x1b[3~\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("bc".to_owned()));
}

#[test]
fn kill_to_end_drops_everything_after_the_cursor() {
    let mut editor = editor(b"abcd\x1b[D\x1b[D\x0b\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("ab".to_owned()));
}

#[test]
fn kill_to_start_drops_everything_before_the_cursor() {
    let mut editor = editor(b"abcd\x1b[D\x15\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("d".to_owned()));
}

#[test]
fn kill_word_back_drops_the_word_before_the_cursor() {
    let mut editor = editor(b"ab cd\x17\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("ab ".to_owned()));
}

#[test]
fn the_down_arrow_walks_back_towards_the_newest_entry() {
    let mut editor = editor(b"one\rtwo\r\x1b[A\x1b[A\x1b[B\r");

    let _ = editor.read_line(&EditorPrompt::default()).unwrap();
    let _ = editor.read_line(&EditorPrompt::default()).unwrap();
    let recalled = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(recalled, EditorEvent::Submitted("two".to_owned()));
}

#[test]
fn input_ending_mid_line_submits_what_was_typed() {
    let mut editor = editor(b"echo hallo");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("echo hallo".to_owned()));
}

#[test]
fn input_ending_mid_continuation_submits_every_line_so_far() {
    let mut editor = editor(b"if true {\recho");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("if true {\necho".to_owned()));
}

#[test]
fn input_ending_on_an_empty_continuation_line_submits_the_lines_before_it() {
    // The third state of the end-of-input clause: nothing on the current line,
    // but earlier lines are already accumulated. Only a guard testing both
    // halves submits here rather than discarding the block.
    let mut editor = editor(b"if true {\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("if true {\n".to_owned()));
}

#[test]
fn a_recalled_multi_line_submission_still_draws_one_row() {
    let mut editor = editor(b"if true {\rprintln\r}\r\x1b[A\r");

    let _ = editor.read_line(&EditorPrompt::default()).unwrap();
    let recalled = editor.read_line(&EditorPrompt::default()).unwrap();

    // The newlines survive into the submitted source, so the block still
    // parses — only the drawing of them is flattened.
    assert_eq!(
        recalled,
        EditorEvent::Submitted("if true {\nprintln\n}".to_owned())
    );
    assert!(
        !String::from_utf8_lossy(editor.drawn()).contains("{\nprintln"),
        "the recalled block is never drawn with its newlines"
    );
}

#[test]
fn a_cancelled_recall_does_not_strand_the_next_one() {
    // `read_line` resets the recall position on entry. Without that reset the
    // cancelled walk below leaves the position parked at the exhausted end, so
    // the next Up hands back the empty draft instead of the newest entry.
    let mut editor = editor(b"one\rtwo\r\x1b[A\x1b[A\x03\x1b[A\r");

    let _ = editor.read_line(&EditorPrompt::default()).unwrap();
    let _ = editor.read_line(&EditorPrompt::default()).unwrap();
    let cancelled = editor.read_line(&EditorPrompt::default()).unwrap();
    let after = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(cancelled, EditorEvent::Cancelled);
    assert_eq!(after, EditorEvent::Submitted("two".to_owned()));
}

#[test]
fn every_read_line_acquires_raw_mode_and_re_reads_the_terminal_size() {
    // Both halves are invisible in the returned event: an editor that never
    // asked for raw mode submits exactly the same text, and a size read once
    // and cached looks identical until the window is resized mid-session.
    let platform =
        FakePlatform::with_terminal(Capabilities::full(), true, TerminalSize::new(80, 24));
    let (mut editor, log) = recording_editor(platform, b"one\rtwo\r");

    let first = editor.read_line(&EditorPrompt::default()).unwrap();
    let second = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(first, EditorEvent::Submitted("one".to_owned()));
    assert_eq!(second, EditorEvent::Submitted("two".to_owned()));
    assert_eq!(log.raw_mode_entries(), 2, "raw mode is acquired per call");
    assert_eq!(
        log.terminal_size_queries(),
        2,
        "the size is re-read per call, so a resize between lines is seen"
    );
}

#[test]
fn a_refused_raw_mode_is_still_requested_and_the_line_still_submits() {
    // Raw mode is best effort: a console that refuses it leaves the editor
    // drawing into a cooked terminal rather than failing the session. The
    // request itself is the part worth pinning, because the outcome alone
    // cannot distinguish "asked and was refused" from "never asked".
    let (mut editor, log) = recording_editor(FakePlatform::none(), b"echo hallo\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("echo hallo".to_owned()));
    assert_eq!(log.raw_mode_entries(), 1);
}

#[test]
fn an_unreadable_terminal_size_falls_back_to_eighty_columns() {
    // A platform without the terminal capability answers no size at all. The
    // fallback is only observable through the drawn row: 80 columns less the
    // five-column prompt leaves a 75-cell window, whose last cell is reserved
    // for the cursor, so 74 of the 100 characters are visible and the cursor
    // lands on column 80.
    let (mut editor, log) = recording_editor(FakePlatform::none(), b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r");

    let event = editor.read_line(&EditorPrompt::default()).unwrap();

    assert_eq!(event, EditorEvent::Submitted("a".repeat(100)));
    assert_eq!(log.terminal_size_queries(), 1);
    let expected = format!("\r\x1b[K>> {}\r\x1b[80G", "a".repeat(76));
    assert!(
        String::from_utf8_lossy(editor.drawn()).contains(&expected),
        "the final row is drawn for an 80-column terminal"
    );
}
