#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::rc::Rc;

use flash_cli::completion::{CompletionCatalog, CompletionEngine};
use flash_cli::editor::{EditorError, EditorEvent, EditorPrompt, LineEditor};
use flash_cli::interactive::{
    EvaluationControl, ExitDecision, InteractiveDiagnostic, InteractiveEvaluationError,
    InteractiveEvaluator, InteractiveExit, InteractiveNotice, InteractiveNoticeError,
    InteractiveNoticeId, InteractiveSessionError, format_job_notice, format_live_jobs,
    run_interactive_driver, run_interactive_session,
};
use flash_cli::report::HostExit;
use flash_platform::ProcessGroupId;
use flash_runtime::background::JobNoticeKind;
use flash_runtime::background::{LiveJob, LiveJobState};
use flash_runtime::builtin::standard_registry;
use flash_runtime::job::JobId;
use flash_runtime::{BindingMutability, ScopeStack, Value};

type CallLog = Rc<RefCell<Vec<String>>>;

struct ScriptedEditor {
    events: VecDeque<Result<EditorEvent, EditorError>>,
    prompts: Vec<EditorPrompt>,
    notices: Vec<String>,
    notice_results: VecDeque<Result<(), EditorError>>,
    completion_values: Vec<Vec<String>>,
    calls: Option<CallLog>,
}

impl ScriptedEditor {
    fn new(events: impl IntoIterator<Item = EditorEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            prompts: Vec::new(),
            notices: Vec::new(),
            notice_results: VecDeque::new(),
            completion_values: Vec::new(),
            calls: None,
        }
    }

    fn with_calls(events: impl IntoIterator<Item = EditorEvent>, calls: CallLog) -> Self {
        Self {
            calls: Some(calls),
            ..Self::new(events)
        }
    }

    fn notices(&self) -> &[String] {
        &self.notices
    }
}

/// An evaluator that refuses the first `refusals` exit attempts.
struct GatedEvaluator {
    refusals: usize,
    rendered: String,
    exit_requests: usize,
}

impl GatedEvaluator {
    fn refusing_once(rendered: &str) -> Self {
        Self {
            refusals: 1,
            rendered: rendered.to_owned(),
            exit_requests: 0,
        }
    }

    fn permitting() -> Self {
        Self {
            refusals: 0,
            rendered: String::new(),
            exit_requests: 0,
        }
    }

    const fn exit_requests(&self) -> usize {
        self.exit_requests
    }
}

impl InteractiveEvaluator for GatedEvaluator {
    fn request_exit(&mut self) -> ExitDecision {
        self.exit_requests += 1;
        if self.refusals == 0 {
            return ExitDecision::Permitted;
        }
        self.refusals -= 1;
        ExitDecision::Refused {
            rendered: self.rendered.clone(),
        }
    }

    fn evaluate(
        &mut self,
        source: &str,
        _output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        match source {
            "exit 0" => Ok(EvaluationControl::Exit(0)),
            _ => Ok(EvaluationControl::Continue),
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

    fn set_completion_catalog(&mut self, catalog: CompletionCatalog) {
        if let Some(calls) = &self.calls {
            calls.borrow_mut().push("set_completion_catalog".to_owned());
        }
        self.completion_values.push(
            CompletionEngine::new(catalog)
                .complete("$la", 3)
                .into_iter()
                .map(|completion| completion.value().to_owned())
                .collect(),
        );
    }
}

#[derive(Default)]
struct SnapshotEvaluator {
    scope: ScopeStack,
}

impl InteractiveEvaluator for SnapshotEvaluator {
    fn completion_catalog(&mut self) -> Option<CompletionCatalog> {
        Some(CompletionCatalog::from_runtime(
            &standard_registry(),
            &self.scope,
        ))
    }

    fn evaluate(
        &mut self,
        source: &str,
        _output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        assert_eq!(source, "define");
        self.scope
            .declare("later", BindingMutability::Immutable, Value::Int(1))
            .expect("the scripted binding is new");
        Ok(EvaluationControl::Continue)
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
        assert_eq!(self.scope.as_deref(), Some("Flash"));
        assert_eq!(self.environment.as_deref(), Some("helix"));
        assert_eq!(self.cwd.as_deref(), Some("/workspace"));
        assert!(self.pipefail);
        assert_eq!(self.status, Some(23));
    }
}

impl InteractiveEvaluator for StatefulEvaluator {
    fn evaluate(
        &mut self,
        source: &str,
        _output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        self.seen.push(source.to_owned());
        match source {
            "seed" => {
                self.scope = Some("Flash".to_owned());
                self.environment = Some("helix".to_owned());
                self.cwd = Some("/workspace".to_owned());
                self.pipefail = true;
                self.status = Some(23);
                Ok(EvaluationControl::Continue)
            }
            "parse-error" => {
                self.assert_seeded();
                Err(InteractiveDiagnostic::new("parse diagnostic\n").into())
            }
            "runtime-error" => {
                self.assert_seeded();
                Err(InteractiveDiagnostic::new("runtime diagnostic\n").into())
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

    fn evaluate(
        &mut self,
        _source: &str,
        _output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
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

struct DriverEvaluator {
    calls: CallLog,
    diagnostic: Option<&'static str>,
}

impl InteractiveEvaluator for DriverEvaluator {
    fn fatal_cleanup(&mut self) -> Vec<String> {
        self.calls.borrow_mut().push("hang_up".to_owned());
        Vec::new()
    }

    fn evaluate(
        &mut self,
        _source: &str,
        _output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        match self.diagnostic {
            Some(rendered) => Err(InteractiveDiagnostic::new(rendered).into()),
            None => Ok(EvaluationControl::Continue),
        }
    }
}

struct RecordingWriter {
    bytes: Vec<u8>,
    calls: CallLog,
    fail_flush: bool,
}

impl RecordingWriter {
    fn new(calls: CallLog) -> Self {
        Self {
            bytes: Vec::new(),
            calls,
            fail_flush: false,
        }
    }

    fn failing_flush(calls: CallLog) -> Self {
        Self {
            fail_flush: true,
            ..Self::new(calls)
        }
    }
}

impl Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls.borrow_mut().push("write".to_owned());
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.calls.borrow_mut().push("flush".to_owned());
        if self.fail_flush {
            Err(io::Error::other("scripted flush failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_recoverable_diagnostic_is_flushed_before_the_next_prompt() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::new([
        EditorEvent::Submitted("diagnose".to_owned()),
        EditorEvent::EndOfInput,
    ]);
    let mut evaluator = DriverEvaluator {
        calls: Rc::clone(&calls),
        diagnostic: Some("recoverable diagnostic\n"),
    };
    let mut diagnostics = RecordingWriter::new(Rc::clone(&calls));

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut diagnostics,
    )
    .expect("a flushed diagnostic should remain recoverable");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(diagnostics.bytes, b"recoverable diagnostic\n");
    assert_eq!(calls.borrow().as_slice(), ["write", "flush"]);
}

#[test]
fn a_failed_diagnostic_flush_is_not_reported_recursively() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::new([EditorEvent::Submitted("diagnose".to_owned())]);
    let mut evaluator = DriverEvaluator {
        calls: Rc::clone(&calls),
        diagnostic: Some("recoverable diagnostic\n"),
    };
    let mut diagnostics = RecordingWriter::failing_flush(Rc::clone(&calls));

    let exit = run_interactive_driver(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut diagnostics,
    );

    assert_eq!(exit, HostExit::Failure);
    assert_eq!(diagnostics.bytes, b"recoverable diagnostic\n");
    assert_eq!(calls.borrow().as_slice(), ["write", "flush", "hang_up"]);
}

#[test]
fn fatal_interactive_exit_hangs_up_jobs_before_reporting_status_one() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::new([EditorEvent::HostCommand("host".to_owned())]);
    let mut evaluator = DriverEvaluator {
        calls: Rc::clone(&calls),
        diagnostic: None,
    };
    let mut diagnostics = RecordingWriter::new(Rc::clone(&calls));

    let exit = run_interactive_driver(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut diagnostics,
    );

    assert_eq!(exit, HostExit::Failure);
    assert_eq!(calls.borrow().as_slice(), ["hang_up", "write", "flush"]);
    assert_eq!(
        String::from_utf8(diagnostics.bytes).expect("fatal report should be UTF-8"),
        "fsh: interactive editor event is not supported: host command\n"
    );
}

#[test]
fn a_failed_program_output_flush_is_a_fatal_interactive_failure() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::new([EditorEvent::Submitted("continue".to_owned())]);
    let mut evaluator = DriverEvaluator {
        calls: Rc::clone(&calls),
        diagnostic: None,
    };
    let mut output = RecordingWriter::failing_flush(CallLog::default());
    let mut diagnostics = RecordingWriter::new(Rc::clone(&calls));

    let exit = run_interactive_driver(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut output,
        &mut diagnostics,
    );

    assert_eq!(exit, HostExit::Failure);
    assert_eq!(calls.borrow().first().map(String::as_str), Some("hang_up"));
    assert_eq!(
        String::from_utf8(diagnostics.bytes).expect("fatal report should be UTF-8"),
        concat!(
            "fsh: interactive command output failed: scripted flush failure\n",
            "fsh:   caused by: scripted flush failure\n"
        )
    );
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
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &prompt,
        &mut output,
        &mut diagnostics,
    )
    .expect("scripted session should finish cleanly");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(evaluator.seen, ["seed"]);
    assert_eq!(editor.prompts, vec![prompt; 3]);
    assert!(diagnostics.is_empty());
}

#[test]
fn completion_is_refreshed_from_live_state_before_every_prompt() {
    let calls = CallLog::default();
    let mut editor = ScriptedEditor::with_calls(
        [
            EditorEvent::Submitted("define".to_owned()),
            EditorEvent::EndOfInput,
        ],
        Rc::clone(&calls),
    );
    let mut evaluator = SnapshotEvaluator::default();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .expect("snapshot refresh should not change session control");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(
        editor.completion_values,
        [Vec::<String>::new(), vec!["$later".to_owned()]]
    );
    assert_eq!(
        calls.borrow().as_slice(),
        [
            "set_completion_catalog",
            "read_line",
            "set_completion_catalog",
            "read_line",
        ]
    );
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
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &prompt,
        &mut output,
        &mut diagnostics,
    )
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
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &prompt,
        &mut output,
        &mut diagnostics,
    )
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
        &mut Vec::new(),
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
        &mut Vec::new(),
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
        &mut Vec::new(),
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
        &mut Vec::new(),
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

#[test]
fn live_job_refusal_names_the_explicit_force_command_for_every_job() {
    let jobs = [
        LiveJob::new(
            JobId::new(2).unwrap(),
            LiveJobState::Running,
            "sleep 10".to_owned(),
        ),
        LiveJob::new(
            JobId::new(7).unwrap(),
            LiveJobState::Stopped,
            "worker".to_owned(),
        ),
    ];

    let rendered = format_live_jobs(&jobs);

    assert!(rendered.contains("kill --kill %2"));
    assert!(rendered.contains("kill --kill %7"));
    assert!(rendered.ends_with("fsh: exit again to hang up\n"));
}

#[test]
fn a_refused_exit_writes_the_refusal_and_keeps_the_session_alive() {
    let mut editor = ScriptedEditor::new(vec![EditorEvent::EndOfInput, EditorEvent::EndOfInput]);
    let mut evaluator = GatedEvaluator::refusing_once("fsh: 1 live background job\n");
    let mut diagnostics = Vec::new();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut diagnostics,
    )
    .expect("a refused exit is not a failure");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(
        editor.notices(),
        ["fsh: 1 live background job\n".to_owned()],
        "the refusal must be written through the editor, not the diagnostic stream"
    );
    assert_eq!(evaluator.exit_requests(), 2);
    assert!(diagnostics.is_empty());
}

#[test]
fn a_refused_explicit_exit_is_also_gated() {
    let mut editor = ScriptedEditor::new(vec![
        EditorEvent::Submitted("exit 0".to_owned()),
        EditorEvent::Submitted("exit 0".to_owned()),
    ]);
    let mut evaluator = GatedEvaluator::refusing_once("fsh: 1 live background job\n");

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .expect("a refused exit is not a failure");

    assert_eq!(exit, InteractiveExit::Requested(0));
    assert_eq!(evaluator.exit_requests(), 2);
}

#[test]
fn a_permitted_exit_asks_once_and_writes_nothing() {
    let mut editor = ScriptedEditor::new(vec![EditorEvent::EndOfInput]);
    let mut evaluator = GatedEvaluator::permitting();

    let exit = run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .expect("a permitted exit is not a failure");

    assert_eq!(exit, InteractiveExit::EndOfInput);
    assert_eq!(evaluator.exit_requests(), 1);
    assert!(editor.notices().is_empty());
}
