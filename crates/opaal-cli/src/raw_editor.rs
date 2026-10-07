use std::io::{self, BufRead, Write};

use opaal_syntax::{ParseOutcome, SourceFile, SourceId, parse_opaal};

use crate::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};

/// Minimal canonical-input editor used while richer terminal editing is unavailable.
pub struct RawLineEditor<R: BufRead = io::StdinLock<'static>> {
    input: R,
    eof: bool,
}

impl RawLineEditor {
    #[cfg_attr(test, allow(dead_code))]
    #[must_use]
    pub fn new() -> Self {
        Self {
            input: io::stdin().lock(),
            eof: false,
        }
    }
}

impl Default for RawLineEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: BufRead> RawLineEditor<R> {
    pub fn from_reader(input: R) -> Self {
        Self { input, eof: false }
    }
}

impl<R: BufRead> LineEditor for RawLineEditor<R> {
    fn write_notice(&mut self, rendered: &str) -> Result<(), EditorError> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        output
            .write_all(rendered.as_bytes())
            .and_then(|()| output.flush())
            .map_err(|error| EditorError::with_source("notice write failed", error))
    }

    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        if self.eof {
            return Ok(EditorEvent::EndOfInput);
        }
        let mut output = io::stdout();
        let mut line = String::new();
        loop {
            let text = if line.is_empty() {
                prompt.primary()
            } else {
                prompt.continuation()
            };
            let _ = output.write_all(text.as_bytes());
            let _ = output.flush();
            match self.input.read_line(&mut line) {
                Ok(0) => {
                    self.eof = true;
                    return if line.is_empty() {
                        Ok(EditorEvent::EndOfInput)
                    } else {
                        Ok(EditorEvent::Submitted(line))
                    };
                }
                Ok(_) => {
                    let source = SourceFile::new(SourceId::new(0), "<interactive>", line.as_str());
                    if !matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)) {
                        return Ok(EditorEvent::Submitted(
                            line.trim_end_matches(['\n', '\r']).to_owned(),
                        ));
                    }
                }
                Err(error) => return Err(EditorError::with_source("stdin read failed", error)),
            }
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

    #[test]
    fn accumulates_only_incomplete_buffers_and_preserves_physical_newlines() {
        let text = "def identity(value: String) -> String # signature\r\n# body\r\n{\r\nreturn value\r\n}\r\nidentity('ready')\n| broken\n";
        let mut editor = RawLineEditor::from_reader(text.as_bytes());
        let prompt = EditorPrompt::default();
        assert_eq!(
            editor.read_line(&prompt).unwrap(),
            EditorEvent::Submitted(
                text[..text.find("identity('ready')").unwrap()]
                    .trim_end_matches(['\n', '\r'])
                    .to_owned()
            )
        );
        assert_eq!(
            editor.read_line(&prompt).unwrap(),
            EditorEvent::Submitted("identity('ready')".into())
        );
        assert_eq!(
            editor.read_line(&prompt).unwrap(),
            EditorEvent::Submitted("| broken".into())
        );
        assert_eq!(editor.read_line(&prompt).unwrap(), EditorEvent::EndOfInput);
    }

    #[test]
    fn pending_eof_submits_once_and_invalid_next_token_does_not_accumulate() {
        let prompt = EditorPrompt::default();
        let mut pending = RawLineEditor::from_reader(&b"def waiting()\n{\n"[..]);
        assert_eq!(
            pending.read_line(&prompt).unwrap(),
            EditorEvent::Submitted("def waiting()\n{\n".into())
        );
        assert_eq!(pending.read_line(&prompt).unwrap(), EditorEvent::EndOfInput);
        assert_eq!(pending.read_line(&prompt).unwrap(), EditorEvent::EndOfInput);

        let mut invalid = RawLineEditor::from_reader(&b"def waiting()\nlet next = 1\n2\n"[..]);
        assert_eq!(
            invalid.read_line(&prompt).unwrap(),
            EditorEvent::Submitted("def waiting()\nlet next = 1".into())
        );
        assert_eq!(
            invalid.read_line(&prompt).unwrap(),
            EditorEvent::Submitted("2".into())
        );
    }

    #[test]
    fn input_failure_remains_an_editor_error() {
        struct FailedInput;
        impl io::Read for FailedInput {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("read sentinel"))
            }
        }
        impl BufRead for FailedInput {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                Err(io::Error::other("read sentinel"))
            }
            fn consume(&mut self, _: usize) {}
        }
        let mut editor = RawLineEditor::from_reader(FailedInput);
        let error = editor.read_line(&EditorPrompt::default()).unwrap_err();
        assert_eq!(error.to_string(), "stdin read failed");
        assert_eq!(
            std::error::Error::source(&error).unwrap().to_string(),
            "read sentinel"
        );
    }
}
