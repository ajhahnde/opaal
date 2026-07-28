//! A FlashShell-owned raw-mode line editor.
//!
//! The editor is portable: it compiles on every target and is exercised by the
//! host test suite. Only its selection in `main` is target-specific. Editing is
//! confined to the current physical line — the cursor never moves up into an
//! earlier continuation line, which keeps redrawing to a single row.

pub mod buffer;
pub mod history;
pub mod key;
pub mod render;

use std::io::{Read, Write};

use flashshell_platform::Platform;
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

use crate::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};
use buffer::EditBuffer;
use history::HistoryRing;
use key::{Key, KeyDecoder};
use render::render_line;

/// How many submissions one session recalls.
const HISTORY_CAPACITY: usize = 1000;

/// Access to what a writer has accumulated, for test observation only.
///
/// Deliberately not implemented for `std::io::Stdout`: a terminal keeps no
/// transcript, so the only honest implementation would return nothing and
/// silently answer "the editor drew nothing" in the shipped path. Leaving it
/// out makes that a compile error instead. The real terminal is observed from
/// the other end, over a pseudoterminal, in `tests/terminal_editor_pty.rs`.
pub trait DrawnOutput {
    fn drawn(&self) -> &[u8];
}

impl DrawnOutput for Vec<u8> {
    fn drawn(&self) -> &[u8] {
        self
    }
}

/// A raw-mode line editor built on the platform terminal capability.
pub struct TerminalEditor<P, R, W> {
    platform: P,
    input: R,
    output: W,
    decoder: KeyDecoder,
    history: HistoryRing,
}

impl<P: Platform, R: Read, W: Write> TerminalEditor<P, R, W> {
    pub fn new(platform: P, input: R, output: W) -> Self {
        Self {
            platform,
            input,
            output,
            decoder: KeyDecoder::new(),
            history: HistoryRing::new(HISTORY_CAPACITY),
        }
    }

    fn read_byte(&mut self) -> Result<Option<u8>, EditorError> {
        let mut byte = [0_u8; 1];
        match self.input.read(&mut byte) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(byte[0])),
            Err(error) => Err(EditorError::with_source("terminal read failed", error)),
        }
    }

    fn draw(&mut self, rendered: &str) -> Result<(), EditorError> {
        self.output
            .write_all(rendered.as_bytes())
            .and_then(|()| self.output.flush())
            .map_err(|error| EditorError::with_source("terminal write failed", error))
    }
}

impl<P: Platform, R: Read, W: Write + DrawnOutput> TerminalEditor<P, R, W> {
    /// Everything the editor has drawn — the test observation point.
    pub fn drawn(&self) -> &[u8] {
        self.output.drawn()
    }
}

/// Whether the accumulated source still needs another physical line.
///
/// This mirrors the host validator in `reedline_editor.rs`: only `Incomplete`
/// retains the buffer, while `Complete` and `Invalid` both submit so the
/// session's normal diagnostic boundary receives the source unchanged.
fn needs_continuation(source: &str) -> bool {
    let file = SourceFile::new(SourceId::new(0), "<interactive>", source);
    matches!(parse(&file), ParseOutcome::Incomplete(_))
}

impl<P: Platform, R: Read, W: Write> LineEditor for TerminalEditor<P, R, W> {
    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        // Raw mode lasts only for this call, so evaluation runs cooked.
        let guard = self.platform.enter_raw_mode().ok();
        let columns = self
            .platform
            .terminal_size()
            .map(|size| size.columns())
            .unwrap_or(80);

        let mut accumulated = String::new();
        let mut line = EditBuffer::new();
        let mut first_line = true;
        self.history.reset_position();

        let result = loop {
            let active = if first_line {
                prompt.primary()
            } else {
                prompt.continuation()
            };
            let rendered = render_line(active, line.text(), line.cursor_chars(), columns);
            self.draw(&rendered)?;

            let Some(byte) = self.read_byte()? else {
                // Input ended: submit anything already typed, else stop.
                if accumulated.is_empty() && line.is_empty() {
                    break EditorEvent::EndOfInput;
                }
                accumulated.push_str(line.text());
                break EditorEvent::Submitted(accumulated);
            };
            let Some(key) = self.decoder.push(byte) else {
                continue;
            };

            match key {
                Key::Char(value) => line.insert(value),
                Key::Backspace => {
                    let _ = line.backspace();
                }
                Key::Delete => {
                    let _ = line.delete();
                }
                Key::Left => {
                    let _ = line.move_left();
                }
                Key::Right => {
                    let _ = line.move_right();
                }
                Key::Home => line.move_home(),
                Key::End => line.move_end(),
                Key::KillToEnd => line.kill_to_end(),
                Key::KillToStart => line.kill_to_start(),
                Key::KillWordBack => line.kill_word_back(),
                Key::Up => {
                    if let Some(entry) = self.history.recall_previous(line.text()) {
                        line = EditBuffer::from_text(&entry);
                    }
                }
                Key::Down => {
                    if let Some(entry) = self.history.recall_next() {
                        line = EditBuffer::from_text(&entry);
                    }
                }
                Key::Cancel => {
                    self.draw("\r\n")?;
                    break EditorEvent::Cancelled;
                }
                Key::EndOfFileOrDelete => {
                    if accumulated.is_empty() && line.is_empty() {
                        self.draw("\r\n")?;
                        break EditorEvent::EndOfInput;
                    }
                    let _ = line.delete();
                }
                Key::Enter => {
                    accumulated.push_str(line.text());
                    self.draw("\r\n")?;
                    if needs_continuation(&accumulated) {
                        accumulated.push('\n');
                        line.clear();
                        first_line = false;
                        continue;
                    }
                    self.history.record(&accumulated);
                    break EditorEvent::Submitted(accumulated);
                }
            }
        };

        drop(guard);
        Ok(result)
    }
}
