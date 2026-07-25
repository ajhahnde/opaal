use std::io::{self, BufRead, Write};

use crate::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};

/// Minimal canonical-input editor used while richer terminal editing is unavailable.
pub struct RawLineEditor<R: BufRead = io::StdinLock<'static>> {
    input: R,
}

impl RawLineEditor {
    #[cfg_attr(test, allow(dead_code))]
    #[must_use]
    pub fn new() -> Self {
        Self {
            input: io::stdin().lock(),
        }
    }
}

impl<R: BufRead> RawLineEditor<R> {
    pub fn from_reader(input: R) -> Self {
        Self { input }
    }
}

impl<R: BufRead> LineEditor for RawLineEditor<R> {
    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        let mut output = io::stdout();
        let _ = output.write_all(prompt.primary().as_bytes());
        let _ = output.flush();

        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) => Ok(EditorEvent::EndOfInput),
            Ok(_) => Ok(EditorEvent::Submitted(
                line.trim_end_matches(['\n', '\r']).to_owned(),
            )),
            Err(error) => Err(EditorError::with_source("stdin read failed", error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{EditorEvent, EditorPrompt, LineEditor};

    #[test]
    fn reads_one_line_as_submitted() {
        let mut editor = RawLineEditor::from_reader(&b"ls\n"[..]);

        let event = editor.read_line(&EditorPrompt::default()).unwrap();

        assert_eq!(event, EditorEvent::Submitted("ls".to_owned()));
    }

    #[test]
    fn empty_input_is_end_of_input() {
        let mut editor = RawLineEditor::from_reader(&b""[..]);

        let event = editor.read_line(&EditorPrompt::default()).unwrap();

        assert_eq!(event, EditorEvent::EndOfInput);
    }
}
