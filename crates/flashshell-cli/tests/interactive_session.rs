#![forbid(unsafe_code)]

use std::collections::VecDeque;

use flashshell_cli::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};
use flashshell_cli::interactive::{
    EvaluationControl, InteractiveDiagnostic, InteractiveEvaluator, InteractiveExit,
    run_interactive_session,
};

struct ScriptedEditor {
    events: VecDeque<Result<EditorEvent, EditorError>>,
    prompts: Vec<EditorPrompt>,
}

impl ScriptedEditor {
    fn new(events: impl IntoIterator<Item = EditorEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            prompts: Vec::new(),
        }
    }
}

impl LineEditor for ScriptedEditor {
    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        self.prompts.push(prompt.clone());
        self.events
            .pop_front()
            .unwrap_or_else(|| Err(EditorError::new("scripted input exhausted")))
    }
}

#[derive(Default)]
struct StatefulEvaluator {
    seen: Vec<String>,
    scope: Option<String>,
    environment: Option<String>,
    cwd: Option<String>,
    pipefail: bool,
    status: Option<i32>,
}

impl StatefulEvaluator {
    fn assert_seeded(&self) {
        assert_eq!(self.scope.as_deref(), Some("FlashShell"));
        assert_eq!(self.environment.as_deref(), Some("helix"));
        assert_eq!(self.cwd.as_deref(), Some("/workspace"));
        assert!(self.pipefail);
        assert_eq!(self.status, Some(23));
    }
}

impl InteractiveEvaluator for StatefulEvaluator {
    fn evaluate(&mut self, source: &str) -> Result<EvaluationControl, InteractiveDiagnostic> {
        self.seen.push(source.to_owned());
        match source {
            "seed" => {
                self.scope = Some("FlashShell".to_owned());
                self.environment = Some("helix".to_owned());
                self.cwd = Some("/workspace".to_owned());
                self.pipefail = true;
                self.status = Some(23);
                Ok(EvaluationControl::Continue)
            }
            "parse-error" => {
                self.assert_seeded();
                Err(InteractiveDiagnostic::new("parse diagnostic\n"))
            }
            "runtime-error" => {
                self.assert_seeded();
                Err(InteractiveDiagnostic::new("runtime diagnostic\n"))
            }
            "verify" => {
                self.assert_seeded();
                Ok(EvaluationControl::Continue)
            }
            "exit" => Ok(EvaluationControl::Exit(23)),
            unexpected => panic!("unexpected source {unexpected:?}"),
        }
    }
}

#[test]
fn ctrl_c_reprompts_without_evaluation_and_empty_ctrl_d_exits() {
    let prompt = EditorPrompt::default();
    let mut editor = ScriptedEditor::new([
        EditorEvent::Cancelled,
        EditorEvent::Submitted("seed".to_owned()),
        EditorEvent::EndOfInput,
    ]);
    let mut evaluator = StatefulEvaluator::default();
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(&mut editor, &mut evaluator, &prompt, &mut diagnostics)
        .expect("scripted session should finish cleanly");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(evaluator.seen, ["seed"]);
    assert_eq!(editor.prompts, vec![prompt; 3]);
    assert!(diagnostics.is_empty());
}

#[test]
fn parse_and_runtime_diagnostics_recover_with_the_same_session_state() {
    let prompt = EditorPrompt::default();
    let mut editor = ScriptedEditor::new([
        EditorEvent::Submitted("seed".to_owned()),
        EditorEvent::Submitted("parse-error".to_owned()),
        EditorEvent::Submitted("runtime-error".to_owned()),
        EditorEvent::Submitted("verify".to_owned()),
        EditorEvent::EndOfInput,
    ]);
    let mut evaluator = StatefulEvaluator::default();
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(&mut editor, &mut evaluator, &prompt, &mut diagnostics)
        .expect("diagnostics should be recoverable");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(
        evaluator.seen,
        ["seed", "parse-error", "runtime-error", "verify"]
    );
    evaluator.assert_seeded();
    assert_eq!(diagnostics, b"parse diagnostic\nruntime diagnostic\n");
}

#[test]
fn explicit_exit_stops_before_reading_or_evaluating_later_input() {
    let prompt = EditorPrompt::default();
    let mut editor = ScriptedEditor::new([
        EditorEvent::Submitted("exit".to_owned()),
        EditorEvent::Submitted("verify".to_owned()),
    ]);
    let mut evaluator = StatefulEvaluator::default();
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(&mut editor, &mut evaluator, &prompt, &mut diagnostics)
        .expect("explicit exit should be normal");

    assert_eq!(exit, InteractiveExit::Requested(23));
    assert_eq!(evaluator.seen, ["exit"]);
    assert_eq!(editor.prompts, [prompt]);
    assert!(diagnostics.is_empty());
}
