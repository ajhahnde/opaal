#![forbid(unsafe_code)]

//! Test-only driver that runs an interactive session on the raw-mode editor.
//!
//! The shipped `fsh` selects its editor by target. This fixture selects the
//! raw-mode editor unconditionally so the host pseudoterminal suite can drive
//! it on macOS and Linux without adding a product-visible switch.
//!
//! The evaluator lives here rather than in the library so no test-only code is
//! compiled into the shipped shell.

use std::env;
use std::io::{self, Write};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use flash_cli::completion::CompletionCatalog;
use flash_cli::editor::{EditorPrompt, LineEditor};
use flash_cli::history::{
    EditorHistory, HistoryPlatform, ProcessHistoryEnvironment, select_history,
};
use flash_cli::interactive::{
    EvaluationControl, InteractiveEvaluationError, InteractiveEvaluator, InteractiveExit,
    InteractiveNotice, InteractiveNoticeError, InteractiveNoticeId, run_interactive_session,
};
use flash_cli::terminal_editor::TerminalEditor;
use flash_platform::Platform;
use flash_platform_posix::PosixPlatform;

fn main() -> ExitCode {
    // The adapter answers the two ends from two different descriptors, and
    // nothing in-process can tell them apart — both look like a terminal under
    // any harness that wires all three stdio to one pty. Reporting them from a
    // real process lets the caller give the two ends different kinds.
    if env::args().nth(1).as_deref() == Some("--report-terminal-ends") {
        let platform = PosixPlatform;
        let mut output = io::stdout();
        let _ = writeln!(
            output,
            "stdin={} stdout={}",
            platform.is_terminal(),
            platform.is_output_terminal()
        );
        let _ = output.flush();
        return ExitCode::SUCCESS;
    }

    let prompt = if env::var_os("FLASH_TEST_SAFE_MODE").is_some() {
        EditorPrompt::safe_mode()
    } else {
        EditorPrompt::new(
            env::var("FLASH_TEST_PROMPT").unwrap_or_else(|_| ">> ".to_owned()),
            env::var("FLASH_TEST_CONTINUATION_PROMPT").unwrap_or_else(|_| "...> ".to_owned()),
        )
    };
    let mut editor = if env::var_os("FLASH_TEST_PERSISTENT_HISTORY").is_some() {
        let selection = match select_history(
            false,
            HistoryPlatform::current(),
            &ProcessHistoryEnvironment,
        ) {
            Ok(selection) => selection,
            Err(error) => {
                eprintln!("fixture: {error}");
                return ExitCode::FAILURE;
            }
        };
        let history = match EditorHistory::open(selection) {
            Ok(history) => history,
            Err(error) => {
                eprintln!("fixture: {error}");
                return ExitCode::FAILURE;
            }
        };
        match TerminalEditor::with_history(
            PosixPlatform,
            io::stdin(),
            io::stdout(),
            Box::new(history),
        ) {
            Ok(editor) => editor,
            Err(error) => {
                eprintln!("fixture: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        TerminalEditor::new(PosixPlatform, io::stdin(), io::stdout())
    };
    if env::var_os("FLASH_TEST_TERMINAL_RESTORE").is_some() {
        let before = match terminal_state() {
            Ok(state) => state,
            Err(error) => {
                eprintln!("fixture: {error}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(error) = editor.read_line(&prompt) {
            eprintln!("fixture: {error}");
            return ExitCode::FAILURE;
        }
        let after = match terminal_state() {
            Ok(state) => state,
            Err(error) => {
                eprintln!("fixture: {error}");
                return ExitCode::FAILURE;
            }
        };
        let restored = terminal_states_equal(&before, &after);
        if !restored {
            eprintln!(
                "fixture: terminal before={} after={}",
                String::from_utf8_lossy(&before).trim(),
                String::from_utf8_lossy(&after).trim()
            );
        }
        println!("terminal-restored={restored}");
        return if restored {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let mut evaluator = EchoEvaluator::new();
    let mut output = io::stdout();
    let mut diagnostics = io::stderr();

    match run_interactive_session(
        &mut editor,
        &mut evaluator,
        &prompt,
        &mut output,
        &mut diagnostics,
    ) {
        Ok(InteractiveExit::EndOfInput) => ExitCode::SUCCESS,
        Ok(InteractiveExit::Requested(code)) => ExitCode::from(code),
        Err(error) => {
            eprintln!("fixture: {error}");
            ExitCode::FAILURE
        }
    }
}

fn terminal_states_equal(before: &[u8], after: &[u8]) -> bool {
    if before == after {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        // macOS raises PENDIN as transient kernel state when canonical mode is
        // restored with bytes already delivered by the pty. It is not an
        // editor-selected mode, so compare every stable stty field around it.
        normalize_macos_stty(before) == normalize_macos_stty(after)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn normalize_macos_stty(state: &[u8]) -> String {
    String::from_utf8_lossy(state)
        .trim()
        .split(':')
        .enumerate()
        .map(|(index, field)| {
            // macOS has emitted both named and positional `stty -g` formats.
            // Mask PENDIN from an explicit lflag field when present, then fall
            // back to the fourth positional field used by older output.
            if let Some(value) = field.strip_prefix("lflag=") {
                return u64::from_str_radix(value, 16)
                    .map(|value| format!("lflag={:x}", value & !0x2000_0000_u64))
                    .unwrap_or_else(|_| field.to_owned());
            }
            if index == 3 && !field.contains('=') {
                return u64::from_str_radix(field, 16)
                    .map(|value| format!("{:x}", value & !0x2000_0000_u64))
                    .unwrap_or_else(|_| field.to_owned());
            }
            field.to_owned()
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn terminal_state() -> io::Result<Vec<u8>> {
    let output = Command::new("stty")
        .arg("-g")
        .stdin(std::process::Stdio::inherit())
        .output()?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "stty -g failed with {}",
            output.status
        )))
    }
}

/// An evaluator that prints each submitted buffer back, prefixed.
///
/// The pseudoterminal suite asserts on what the editor submitted, so the whole
/// runtime is deliberately left out of the loop.
struct EchoEvaluator {
    external_notice_at: Option<Instant>,
    external_notice_pending: bool,
}

impl EchoEvaluator {
    fn new() -> Self {
        let external_notice_pending = env::var_os("FLASH_TEST_EXTERNAL_NOTICE").is_some();
        Self {
            external_notice_at: external_notice_pending
                .then(|| Instant::now() + Duration::from_millis(150)),
            external_notice_pending,
        }
    }
}

impl InteractiveEvaluator for EchoEvaluator {
    fn completion_catalog(&mut self) -> Option<CompletionCatalog> {
        Some(CompletionCatalog::from_runtime(
            &flash_runtime::builtin::standard_registry(),
            &flash_runtime::ScopeStack::new(),
        ))
    }

    fn next_notice(&mut self) -> Option<InteractiveNotice> {
        if !self.external_notice_pending
            || self
                .external_notice_at
                .is_some_and(|at| Instant::now() < at)
        {
            return None;
        }
        Some(InteractiveNotice::new(
            InteractiveNoticeId::new(1).unwrap(),
            "[1] Done     external worker\n",
        ))
    }

    fn acknowledge_notice(
        &mut self,
        notice: &InteractiveNotice,
    ) -> Result<(), InteractiveNoticeError> {
        if notice.id().get() != 1 || !self.external_notice_pending {
            return Err(InteractiveNoticeError::new("unexpected fixture notice"));
        }
        self.external_notice_pending = false;
        Ok(())
    }

    fn evaluate(
        &mut self,
        source: &str,
        output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        if source == "exit" {
            return Ok(EvaluationControl::Exit(0));
        }
        writeln!(output, "submitted: {source}")
            .map_err(InteractiveEvaluationError::ProgramOutput)?;
        Ok(EvaluationControl::Continue)
    }
}
