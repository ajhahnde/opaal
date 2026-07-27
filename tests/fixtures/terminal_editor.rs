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
use std::process::ExitCode;

use flashshell_cli::editor::EditorPrompt;
use flashshell_cli::interactive::{
    EvaluationControl, InteractiveDiagnostic, InteractiveEvaluator, InteractiveExit,
    run_interactive_session,
};
use flashshell_cli::terminal_editor::TerminalEditor;
use flashshell_platform::Platform;
use flashshell_platform_posix::PosixPlatform;

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

    let mut editor = TerminalEditor::new(PosixPlatform, io::stdin(), io::stdout());
    let mut evaluator = EchoEvaluator;
    let mut diagnostics = io::stderr();

    match run_interactive_session(
        &mut editor,
        &mut evaluator,
        &EditorPrompt::default(),
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

/// An evaluator that prints each submitted buffer back, prefixed.
///
/// The pseudoterminal suite asserts on what the editor submitted, so the whole
/// runtime is deliberately left out of the loop.
struct EchoEvaluator;

impl InteractiveEvaluator for EchoEvaluator {
    fn evaluate(&mut self, source: &str) -> Result<EvaluationControl, InteractiveDiagnostic> {
        if source == "exit" {
            return Ok(EvaluationControl::Exit(0));
        }
        let mut output = io::stdout();
        let _ = writeln!(output, "submitted: {source}");
        let _ = output.flush();
        Ok(EvaluationControl::Continue)
    }
}
