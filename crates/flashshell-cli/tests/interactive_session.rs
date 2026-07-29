#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use flashshell_cli::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};
use flashshell_cli::interactive::{
    EvaluationControl, InteractiveDiagnostic, InteractiveEvaluator, InteractiveExit,
    InteractiveNotice, InteractiveNoticeError, InteractiveNoticeId, InteractiveSessionError,
    format_job_notice, run_interactive_session,
};
use flashshell_platform::ProcessGroupId;
use flashshell_runtime::background::JobNoticeKind;
use flashshell_runtime::job::JobId;

type CallLog = Rc<RefCell<Vec<String>>>;

struct ScriptedEditor {
    events: VecDeque<Result<EditorEvent, EditorError>>,
    prompts: Vec<EditorPrompt>,
    notices: Vec<String>,
    notice_results: VecDeque<Result<(), EditorError>>,
    calls: Option<CallLog>,
}

impl ScriptedEditor {
    fn new(events: impl IntoIterator<Item = EditorEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            prompts: Vec::new(),
            notices: Vec::new(),
            notice_results: VecDeque::new(),
            calls: None,
        }
    }

    fn with_calls(events: impl IntoIterator<Item = EditorEvent>, calls: CallLog) -> Self {
        Self {
            calls: Some(calls),
            ..Self::new(events)
        }
    }
}

impl LineEditor for ScriptedEditor {
    fn write_notice(&mut self, rendered: &str) -> Result<(), EditorError> {
        if let Some(calls) = &self.calls {
            calls.borrow_mut().push("write_notice".to_owned());
        }
        self.notices.push(rendered.to_owned());
        self.notice_results.pop_front().unwrap_or(Ok(()))
    }

    fn read_line(&mut self, prompt: &EditorPrompt) -> Result<EditorEvent, EditorError> {
        if let Some(calls) = &self.calls {
            calls.borrow_mut().push("read_line".to_owned());
        }
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

struct NoticeEvaluator {
    notices: VecDeque<InteractiveNotice>,
    calls: CallLog,
    queue_during_evaluation: Option<InteractiveNotice>,
}

impl NoticeEvaluator {
    fn new(notices: impl IntoIterator<Item = InteractiveNotice>, calls: CallLog) -> Self {
        Self {
            notices: notices.into_iter().collect(),
            calls,
            queue_during_evaluation: None,
        }
    }
}

impl InteractiveEvaluator for NoticeEvaluator {
    fn next_notice(&mut self) -> Option<InteractiveNotice> {
        let notice = self.notices.pop_front()?;
        self.calls.borrow_mut().push("next_notice".to_owned());
        Some(notice)
    }

    fn acknowledge_notice(
        &mut self,
        _notice: &InteractiveNotice,
    ) -> Result<(), InteractiveNoticeError> {
        self.calls.borrow_mut().push("acknowledge".to_owned());
        Ok(())
    }

    fn evaluate(&mut self, _source: &str) -> Result<EvaluationControl, InteractiveDiagnostic> {
        self.calls.borrow_mut().push("evaluate".to_owned());
        if let Some(notice) = self.queue_during_evaluation.take() {
            self.notices.push_back(notice);
        }
        Ok(EvaluationControl::Continue)
    }
}

fn notice(id: u64, rendered: &str) -> InteractiveNotice {
    InteractiveNotice::new(
        InteractiveNoticeId::new(id).expect("scripted notice identity must be nonzero"),
        rendered,
    )
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

#[test]
fn notice_is_written_and_acknowledged_before_the_prompt_is_read() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::with_calls([EditorEvent::EndOfInput], Rc::clone(&calls));
    let mut evaluator = NoticeEvaluator::new([notice(1, "[1] 4123\n")], Rc::clone(&calls));
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut diagnostics,
    )
    .expect("notice and prompt should finish cleanly");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(
        calls.borrow().as_slice(),
        ["next_notice", "write_notice", "acknowledge", "read_line"]
    );
    assert_eq!(editor.notices, ["[1] 4123\n"]);
    assert!(diagnostics.is_empty());
}

#[test]
fn every_pending_notice_is_acknowledged_before_one_prompt() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::with_calls([EditorEvent::EndOfInput], Rc::clone(&calls));
    let mut evaluator = NoticeEvaluator::new(
        [notice(1, "[1] 4123\n"), notice(2, "[1] Done     command\n")],
        Rc::clone(&calls),
    );
    let mut diagnostics = Vec::new();

    run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut diagnostics,
    )
    .expect("notices and prompt should finish cleanly");

    assert_eq!(
        calls.borrow().as_slice(),
        [
            "next_notice",
            "write_notice",
            "acknowledge",
            "next_notice",
            "write_notice",
            "acknowledge",
            "read_line",
        ]
    );
    assert_eq!(editor.notices, ["[1] 4123\n", "[1] Done     command\n"]);
}

#[test]
fn notice_queued_during_evaluation_waits_for_the_next_prompt_boundary() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::with_calls(
        [
            EditorEvent::Submitted("queue-notice".to_owned()),
            EditorEvent::EndOfInput,
        ],
        Rc::clone(&calls),
    );
    let mut evaluator = NoticeEvaluator::new([], Rc::clone(&calls));
    evaluator.queue_during_evaluation = Some(notice(1, "[1] Done     command\n"));
    let mut diagnostics = Vec::new();

    run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut diagnostics,
    )
    .expect("queued notice should be rendered at the next loop boundary");

    assert_eq!(
        calls.borrow().as_slice(),
        [
            "read_line",
            "evaluate",
            "next_notice",
            "write_notice",
            "acknowledge",
            "read_line",
        ]
    );
}

#[test]
fn failed_notice_write_prevents_acknowledgement_and_prompt_read() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::with_calls([EditorEvent::EndOfInput], Rc::clone(&calls));
    editor
        .notice_results
        .push_back(Err(EditorError::new("scripted notice write failed")));
    let mut evaluator = NoticeEvaluator::new([notice(1, "[1] 4123\n")], Rc::clone(&calls));
    let mut diagnostics = Vec::new();

    let error = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut diagnostics,
    )
    .expect_err("notice write failure should be fatal");

    assert!(matches!(error, InteractiveSessionError::Editor(_)));
    assert_eq!(calls.borrow().as_slice(), ["next_notice", "write_notice"]);
    assert!(diagnostics.is_empty());
}

#[test]
fn structured_job_notices_have_stable_prompt_safe_formatting() {
    let job = JobId::new(1).expect("job identity should be nonzero");
    let group = ProcessGroupId::new(4123).expect("process group should be nonzero");

    assert_eq!(
        format_job_notice(job, &JobNoticeKind::Started { group }, "command"),
        "[1] 4123\n"
    );
    assert_eq!(
        format_job_notice(job, &JobNoticeKind::Stopped, "command"),
        "[1] Stopped  command\n"
    );
    assert_eq!(
        format_job_notice(job, &JobNoticeKind::Completed, "command"),
        "[1] Done     command\n"
    );
    assert_eq!(
        format_job_notice(
            job,
            &JobNoticeKind::ObservationFailed {
                message: "wait failed".to_owned(),
            },
            "command",
        ),
        "[1] Failed   command: wait failed\n"
    );
}
