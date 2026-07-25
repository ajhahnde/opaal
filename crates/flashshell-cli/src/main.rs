#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::ExitCode;

#[cfg(target_os = "redox")]
use flashshell_cli::RawLineEditor;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flashshell_cli::ReedlineEditor;
use flashshell_cli::cli::{Mode, parse_args};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flashshell_cli::config::{
    ConfigDefaults, ConfigFatalError, ConfigInvocation, ConfigLimits, ConfigPlatform,
    ConfigRequest, HostConfigSource, ProcessConfigEnvironment, initialize_config,
};
#[cfg(target_os = "redox")]
use flashshell_cli::editor::EditorPrompt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flashshell_cli::history::{HistoryPlatform, ProcessHistoryEnvironment, select_history};
use flashshell_cli::interactive::{
    EvaluationControl, InteractiveDiagnostic, InteractiveEvaluator, InteractiveExit,
    InteractiveSessionError, run_interactive_session,
};
use flashshell_platform_posix::PosixPlatform;
use flashshell_runtime::eval::SystemClock;
use flashshell_runtime::plan::SessionOptions;
use flashshell_runtime::resolve::ExecutableProbe;
use flashshell_runtime::script::execute_script;
use flashshell_runtime::session::{Session, SubmitError, SubmitOutcome};
use flashshell_runtime::{Environment, ScopeStack, Status};

const HELP: &str = "FlashShell command shell

Usage: fsh [OPTIONS] [SCRIPT]

Options:
      --no-config    Skip loading the startup configuration
      --no-history   Disable interactive history for this session
  -h, --help         Print help
  -V, --version      Print version
";

fn main() -> ExitCode {
    let invocation = match parse_args(env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("fsh: {}", error.message());
            return ExitCode::from(2);
        }
    };

    match invocation.mode {
        Mode::Help => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Mode::Version => {
            println!("fsh {}", flashshell_runtime::version());
            ExitCode::SUCCESS
        }
        Mode::Script { path } => run_script(&path),
        Mode::Interactive => run_interactive(invocation.no_config, invocation.no_history),
    }
}

/// Build the child-environment snapshot from the UTF-8-named process variables.
fn process_environment() -> Environment {
    Environment::from_snapshot(
        env::vars_os()
            .filter_map(|(name, value)| name.into_string().ok().map(|name| (name, value))),
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run_interactive(no_config: bool, no_history: bool) -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("fsh: cannot read the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Consider configuration exactly once, before any editor or prompt work.
    let defaults = ConfigDefaults::new(ScopeStack::new(), process_environment());
    let request = ConfigRequest::new(
        ConfigInvocation::Interactive,
        no_config,
        ConfigPlatform::current(),
    );
    let startup = match initialize_config(
        request,
        &ProcessConfigEnvironment,
        &HostConfigSource,
        &defaults,
        &ConfigLimits::default(),
    ) {
        Ok(startup) => startup,
        Err(ConfigFatalError::Cancelled(_)) => {
            eprintln!("fsh: startup configuration was cancelled");
            return ExitCode::FAILURE;
        }
    };

    // A safe-mode diagnostic is written before the first prompt is ever drawn.
    if let Some(diagnostic) = startup.diagnostic() {
        eprint!("{diagnostic}");
    }

    let selection = match select_history(
        no_history,
        HistoryPlatform::current(),
        &ProcessHistoryEnvironment,
    ) {
        Ok(selection) => selection,
        Err(error) => {
            eprintln!("fsh: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut editor = match ReedlineEditor::with_history(selection) {
        Ok(editor) => editor,
        Err(error) => {
            eprintln!("fsh: {error}");
            return ExitCode::FAILURE;
        }
    };

    let session = Session::with_scope(
        startup.scope().clone(),
        cwd,
        startup.environment().clone(),
        SessionOptions::default(),
    );
    let mut evaluator = SessionEvaluator::new(session);
    let mut diagnostics = io::stderr();

    match run_interactive_session(
        &mut editor,
        &mut evaluator,
        startup.prompt(),
        &mut diagnostics,
    ) {
        Ok(InteractiveExit::EndOfInput) => ExitCode::SUCCESS,
        Ok(InteractiveExit::Requested(code)) => ExitCode::from(code),
        Err(error) => {
            report_session_error(&error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "redox")]
fn run_interactive(_no_config: bool, _no_history: bool) -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("fsh: cannot read the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let session = Session::with_scope(
        ScopeStack::new(),
        cwd,
        process_environment(),
        SessionOptions::default(),
    );
    let mut editor = RawLineEditor::new();
    let mut evaluator = SessionEvaluator::new(session);
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
            report_session_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn report_session_error(error: &InteractiveSessionError) {
    // A broken diagnostic channel cannot be reported through itself.
    if !matches!(error, InteractiveSessionError::DiagnosticOutput(_)) {
        eprintln!("fsh: {error}");
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            eprintln!("fsh:   caused by: {cause}");
            source = cause.source();
        }
    }
}

/// Bridges the runtime session driver into the interactive evaluation loop.
struct SessionEvaluator {
    session: Session,
    probe: NativeExecutableProbe,
    platform: PosixPlatform,
    clock: SystemClock,
}

impl SessionEvaluator {
    fn new(session: Session) -> Self {
        Self {
            session,
            probe: NativeExecutableProbe,
            platform: PosixPlatform,
            clock: SystemClock::new(),
        }
    }
}

impl InteractiveEvaluator for SessionEvaluator {
    fn evaluate(&mut self, source: &str) -> Result<EvaluationControl, InteractiveDiagnostic> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        let outcome = self.session.submit(
            "<interactive>",
            source,
            &self.probe,
            &self.platform,
            &self.clock,
            &mut output,
        );
        match outcome {
            Ok(SubmitOutcome::Continued) => {
                let _ = output.flush();
                Ok(EvaluationControl::Continue)
            }
            Ok(SubmitOutcome::Exit(code)) => {
                let _ = output.flush();
                Ok(EvaluationControl::Exit(code))
            }
            Err(SubmitError::Diagnostic(rendered)) => Err(InteractiveDiagnostic::new(rendered)),
            Err(SubmitError::Output(error)) => Err(InteractiveDiagnostic::new(format!(
                "fsh: cannot write command output: {error}\n"
            ))),
        }
    }
}

fn run_script(path: &Path) -> ExitCode {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("fsh: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            eprintln!(
                "fsh: {}: source is not UTF-8 at byte {}",
                path.display(),
                error.utf8_error().valid_up_to()
            );
            return ExitCode::FAILURE;
        }
    };
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("fsh: cannot read the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut environment = process_environment();
    let registry = flashshell_runtime::builtin::standard_registry();
    let result = execute_script(
        path.to_string_lossy(),
        text,
        &cwd,
        &mut environment,
        &registry,
        &NativeExecutableProbe,
        &SessionOptions::default(),
        &PosixPlatform,
        &SystemClock::new(),
    );

    match result {
        Ok(completion) => completion.status().map_or(ExitCode::SUCCESS, status_exit),
        Err(error) => {
            eprint!("{}", error.render());
            ExitCode::FAILURE
        }
    }
}

fn status_exit(status: &Status) -> ExitCode {
    let code = match (status.code(), status.signal()) {
        (Some(code), None) => u8::try_from(code).unwrap_or(1),
        (None, Some(signal)) => signal
            .number()
            .and_then(|number| u8::try_from(128_i64.saturating_add(number)).ok())
            .unwrap_or(1),
        _ => 1,
    };
    ExitCode::from(code)
}

struct NativeExecutableProbe;

impl ExecutableProbe for NativeExecutableProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        fs::metadata(Path::new(path))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
}
