//! A Flash-owned raw-mode line editor.
//!
//! The editor is portable: it compiles on every target and is exercised by the
//! host test suite. Only its selection in `main` is target-specific. Editing is
//! owns the complete multiline submission so cursor movement and edits can
//! cross continuation-line boundaries without changing the stored UTF-8.

pub mod buffer;
pub mod history;
pub mod key;
pub mod render;

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::time::Duration;

use flash_platform::{Platform, PlatformError};
use flash_syntax::{ParseOutcome, SourceFile, SourceId, parse};

use crate::completion::{CompletionCatalog, CompletionEngine};
use crate::editor::{
    EditorError, EditorEvent, EditorEventSource, EditorPrompt, LineEditor, NoEditorEvents,
};
use crate::highlight::SyntaxHighlighter;
use crate::hint::{HintCatalog, HintEngine};
use buffer::EditBuffer;
use history::{HistoryPersistence, HistoryRing};
use key::{Key, KeyDecoder};
use render::render_submission;

/// How many submissions one session recalls.
const HISTORY_CAPACITY: usize = 1000;
/// How many terminal bytes one readiness notification can drain.
const INPUT_BUFFER_CAPACITY: usize = 256;

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
    completion: CompletionEngine,
    hints: HintEngine,
    drawn_cursor_row: usize,
    pending_input: VecDeque<u8>,
    persistence: Option<Box<dyn HistoryPersistence>>,
}

impl<P: Platform, R: Read, W: Write> TerminalEditor<P, R, W> {
    pub fn new(platform: P, input: R, output: W) -> Self {
        Self {
            platform,
            input,
            output,
            decoder: KeyDecoder::new(),
            history: HistoryRing::new(HISTORY_CAPACITY),
            completion: CompletionEngine::new(CompletionCatalog::new()),
            hints: HintEngine::new(),
            drawn_cursor_row: 0,
            pending_input: VecDeque::new(),
            persistence: None,
        }
    }

    /// Construct the portable editor over the shared persistent-history
    /// contract and load its retained submission snapshot.
    pub fn with_history(
        platform: P,
        input: R,
        output: W,
        mut persistence: Box<dyn HistoryPersistence>,
    ) -> Result<Self, EditorError> {
        let mut editor = Self::new(platform, input, output);
        editor.history.load(persistence.entries()?);
        editor.persistence = Some(persistence);
        Ok(editor)
    }

    fn read_byte(&mut self, timeout: Duration) -> Result<Option<Option<u8>>, EditorError> {
        if let Some(byte) = self.pending_input.pop_front() {
            return Ok(Some(Some(byte)));
        }

        let mut bytes = [0_u8; INPUT_BUFFER_CAPACITY];
        let read = match self.platform.read_terminal_input(&mut bytes, timeout) {
            Ok(None) => return Ok(None),
            Ok(Some(0)) => return Ok(Some(None)),
            Ok(Some(read)) => read,
            Err(PlatformError::Unsupported { .. }) => match self.input.read(&mut bytes) {
                Ok(0) => return Ok(Some(None)),
                Ok(read) => read,
                Err(error) => {
                    return Err(EditorError::with_source("terminal read failed", error));
                }
            },
            Err(error) => {
                return Err(EditorError::with_source(
                    "reading terminal input failed",
                    error,
                ));
            }
        };
        if read > bytes.len() {
            return Err(EditorError::new(
                "terminal read exceeded the supplied input buffer",
            ));
        }
        self.pending_input.extend(&bytes[..read]);
        Ok(Some(self.pending_input.pop_front()))
    }

    fn draw(&mut self, rendered: &str) -> Result<(), EditorError> {
        self.output
            .write_all(rendered.as_bytes())
            .and_then(|()| self.output.flush())
            .map_err(|error| EditorError::with_source("terminal write failed", error))
    }

    fn clear_rendered_submission(&self) -> String {
        let mut rendered = String::from("\r");
        if self.drawn_cursor_row > 0 {
            rendered.push_str(&format!("\x1b[{}A", self.drawn_cursor_row));
        }
        rendered.push_str("\x1b[J");
        rendered
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
    fn write_notice(&mut self, rendered: &str) -> Result<(), EditorError> {
        self.draw(rendered)
    }

    fn set_completion_catalog(&mut self, catalog: CompletionCatalog) {
        let _ = self.completion.install_catalog(catalog);
    }

    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        self.read_line_with_events(prompt, &mut NoEditorEvents)
    }

    fn read_line_with_events(
        &mut self,
        prompt: &EditorPrompt,
        events: &mut dyn EditorEventSource,
    ) -> Result<EditorEvent, EditorError> {
        // Raw mode lasts only for this call, so evaluation runs cooked.
        let guard = self.platform.enter_raw_mode().ok();
        let mut line = EditBuffer::new();
        self.history.reset_position();
        let mut dirty = true;
        let mut last_columns = None;

        let result = loop {
            // Re-read dimensions while the edit is active. A resize never
            // mutates source state; the next decoded input event simply redraws
            // the same buffer against the current cell grid.
            let columns = self
                .platform
                .terminal_size()
                .map(|size| size.columns())
                .unwrap_or(80);
            if dirty || last_columns != Some(columns) {
                let hint = self
                    .hints
                    .hint(
                        line.text(),
                        line.cursor(),
                        &HintCatalog::new(self.history.newest_entries()),
                    )
                    .map(|hint| hint.suffix().to_owned())
                    .unwrap_or_default();
                let display = if hint.is_empty() {
                    line.text().to_owned()
                } else {
                    format!("{}{hint}", line.text())
                };
                let highlights = SyntaxHighlighter::new().highlight(line.text());
                let (rendered, cursor_row) = render_submission(
                    prompt,
                    &display,
                    line.cursor(),
                    columns,
                    self.drawn_cursor_row,
                    line.text().len(),
                    &highlights,
                );
                self.draw(&rendered)?;
                self.drawn_cursor_row = cursor_row;
                dirty = false;
                last_columns = Some(columns);
            }

            if let Some(event) = events.next_external_print() {
                let mut external = self.clear_rendered_submission();
                external.push_str(event.rendered());
                self.draw(&external)?;
                events.acknowledge_external_print(&event)?;
                self.drawn_cursor_row = 0;
                dirty = true;
                continue;
            }

            let Some(byte) = self.read_byte(Duration::from_millis(50))? else {
                continue;
            };
            let Some(byte) = byte else {
                // Input ended: submit anything already typed, else stop.
                if line.is_empty() {
                    break EditorEvent::EndOfInput;
                }
                break EditorEvent::Submitted(line.text().to_owned());
            };
            let Some(key) = self.decoder.push(byte) else {
                continue;
            };
            dirty = true;

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
                    if !line.move_right()
                        && let Some(hint) = self.hints.hint(
                            line.text(),
                            line.cursor(),
                            &HintCatalog::new(self.history.newest_entries()),
                        )
                    {
                        let cursor = line.cursor();
                        let _ = line.replace(cursor..cursor, hint.suffix());
                    }
                }
                Key::Home => line.move_home(),
                Key::End => line.move_end(),
                Key::KillToEnd => line.kill_to_end(),
                Key::KillToStart => line.kill_to_start(),
                Key::KillWordBack => line.kill_word_back(),
                Key::Complete => {
                    let completions = self.completion.complete(line.text(), line.cursor());
                    if let [completion] = completions.as_slice() {
                        let mut value = completion.value().to_owned();
                        if completion.append_whitespace() {
                            value.push(' ');
                        }
                        let _ = line.replace(completion.replacement(), &value);
                    } else if !completions.is_empty() {
                        let values = completions
                            .iter()
                            .map(|completion| completion.value())
                            .collect::<Vec<_>>()
                            .join("  ");
                        let mut rendered = self.clear_rendered_submission();
                        rendered.push_str(&values);
                        rendered.push_str("\r\n");
                        self.draw(&rendered)?;
                        self.drawn_cursor_row = 0;
                    }
                }
                Key::Up => {
                    if !line.move_up()
                        && let Some(entry) = self.history.recall_previous(line.text())
                    {
                        line = EditBuffer::from_text(&entry);
                    }
                }
                Key::Down => {
                    if !line.move_down()
                        && let Some(entry) = self.history.recall_next()
                    {
                        line = EditBuffer::from_text(&entry);
                    }
                }
                Key::Cancel => {
                    self.draw("\r\n")?;
                    self.drawn_cursor_row = 0;
                    break EditorEvent::Cancelled;
                }
                Key::EndOfFileOrDelete => {
                    if line.is_empty() {
                        self.draw("\r\n")?;
                        self.drawn_cursor_row = 0;
                        break EditorEvent::EndOfInput;
                    }
                    let _ = line.delete();
                }
                Key::Enter => {
                    if needs_continuation(line.text()) {
                        line.insert('\n');
                        continue;
                    }
                    self.draw("\r\n")?;
                    self.drawn_cursor_row = 0;
                    if let Some(persistence) = &mut self.persistence {
                        persistence.record(line.text())?;
                    }
                    self.history.record(line.text());
                    break EditorEvent::Submitted(line.text().to_owned());
                }
            }
        };

        drop(guard);
        Ok(result)
    }
}
