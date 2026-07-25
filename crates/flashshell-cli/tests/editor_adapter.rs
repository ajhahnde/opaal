#![forbid(unsafe_code)]

#[cfg(any(target_os = "macos", target_os = "linux"))]
use flashshell_cli::ReedlineEditor;
use flashshell_cli::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flashshell_cli::history::HistorySelection;

struct RecordingEditor {
    event: Option<EditorEvent>,
    prompts: Vec<EditorPrompt>,
}

impl LineEditor for RecordingEditor {
    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        self.prompts.push(prompt.clone());
        self.event
            .take()
            .ok_or_else(|| EditorError::new("scripted editor exhausted"))
    }
}

#[test]
fn session_code_consumes_editor_owned_events_without_a_terminal() {
    let prompt = EditorPrompt::new("fsh> ", "...> ");
    let mut editor = RecordingEditor {
        event: Some(EditorEvent::Submitted("echo hello".to_owned())),
        prompts: Vec::new(),
    };

    assert_eq!(
        editor
            .read_line(&prompt)
            .expect("scripted input should exist"),
        EditorEvent::Submitted("echo hello".to_owned())
    );
    assert_eq!(editor.prompts, vec![prompt]);
}

#[test]
fn default_prompt_has_stable_primary_and_continuation_text() {
    let prompt = EditorPrompt::default();

    assert_eq!(prompt.primary(), "fsh> ");
    assert_eq!(prompt.continuation(), "...> ");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn reedline_is_constructed_behind_the_line_editor_contract() {
    fn accepts_adapter(_: &mut dyn LineEditor) {}

    let mut editor = ReedlineEditor::new();
    accepts_adapter(&mut editor);

    let mut disabled = ReedlineEditor::with_history(HistorySelection::Disabled)
        .expect("disabled history should require no filesystem state");
    accepts_adapter(&mut disabled);
}
