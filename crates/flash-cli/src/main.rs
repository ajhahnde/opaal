#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

#[cfg(target_os = "redox")]
use flash_cli::RawLineEditor;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flash_cli::ReedlineEditor;
#[cfg(target_os = "redox")]
use flash_cli::TerminalEditor;
use flash_cli::cli::{Mode, parse_args};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flash_cli::config::{
    ConfigDefaults, ConfigFatalError, ConfigInvocation, ConfigLimits, ConfigPlatform,
    ConfigRequest, HostConfigSource, ProcessConfigEnvironment, initialize_config,
};
#[cfg(target_os = "redox")]
use flash_cli::editor::EditorPrompt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use flash_cli::history::{HistoryPlatform, ProcessHistoryEnvironment, select_history};
use flash_cli::interactive::{
    EvaluationControl, ExitDecision, InteractiveDiagnostic, InteractiveEvaluator, InteractiveExit,
    InteractiveNotice, InteractiveNoticeError, InteractiveNoticeId, InteractiveSessionError,
    format_job_notice, format_live_jobs, run_interactive_session,
};
use flash_platform::{Platform, PlatformError};
use flash_platform_posix::PosixPlatform;
use flash_runtime::eval::{Clock, SystemClock};
use flash_runtime::module::{
    ModuleCanonicalizer, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::{execute_chain_subshell, execute_module_program};
use flash_runtime::session::{BackgroundFailure, JobNoticeId, Session, SubmitError, SubmitOutcome};
use flash_runtime::{Environment, ScopeStack, Status};

const HELP: &str = "Flash command shell

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
            println!("fsh {}", flash_runtime::version());
            ExitCode::SUCCESS
        }
        Mode::Script { path } => run_script(&path),
        Mode::AsyncChain {
            text,
            pipefail,
            capture_limit,
        } => run_async_chain(text, pipefail, capture_limit),
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

    // Only a shell holding the keyboard arranges anything. A shell reading a
    // redirected input is still interrupted along with the terminal's foreground
    // group, and that is what should happen to it: one that ignored the
    // interrupt could not be stopped from the keyboard that started it. Where it
    // is installed the arrangement lasts the whole session, and the guard puts
    // the previous handling back on every exit path including a panic. A
    // platform without the capability delivers no terminal signals either, so an
    // unsupported result is not a startup failure.
    let _signals = if PosixPlatform.is_terminal() {
        match PosixPlatform.install_job_control_signals() {
            Ok(guard) => Some(guard),
            Err(PlatformError::Unsupported { .. }) => None,
            Err(error) => {
                eprintln!("fsh: cannot arrange terminal signals: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
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
        Err(error) => fatal_interactive_exit(&mut evaluator, &error),
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

    // Only a shell holding the keyboard arranges anything. A shell reading a
    // redirected input is still interrupted along with the terminal's foreground
    // group, and that is what should happen to it: one that ignored the
    // interrupt could not be stopped from the keyboard that started it. Where it
    // is installed the arrangement lasts the whole session, and the guard puts
    // the previous handling back on every exit path including a panic. A
    // platform without the capability delivers no terminal signals either, so an
    // unsupported result is not a startup failure.
    let _signals = if PosixPlatform.is_terminal() {
        match PosixPlatform.install_job_control_signals() {
            Ok(guard) => Some(guard),
            Err(PlatformError::Unsupported { .. }) => None,
            Err(error) => {
                eprintln!("fsh: cannot arrange terminal signals: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let session = Session::with_scope(
        ScopeStack::new(),
        cwd,
        process_environment(),
        SessionOptions::default(),
    );
    let mut evaluator = SessionEvaluator::new(session);
    let mut diagnostics = io::stderr();

    // A real terminal gets the raw-mode editor; a pipe or an uncooperative
    // console falls back to canonical line reading. Both ends are checked: the
    // editor redraws its row with cursor escapes, and a session typed at from
    // a keyboard but redirected to a file must not have them written into it.
    let platform = PosixPlatform;
    let outcome = if platform.is_terminal() && platform.is_output_terminal() {
        let mut editor = TerminalEditor::new(platform, io::stdin(), io::stdout());
        run_interactive_session(
            &mut editor,
            &mut evaluator,
            &EditorPrompt::default(),
            &mut diagnostics,
        )
    } else {
        let mut editor = RawLineEditor::new();
        run_interactive_session(
            &mut editor,
            &mut evaluator,
            &EditorPrompt::default(),
            &mut diagnostics,
        )
    };

    match outcome {
        Ok(InteractiveExit::EndOfInput) => ExitCode::SUCCESS,
        Ok(InteractiveExit::Requested(code)) => ExitCode::from(code),
        Err(error) => fatal_interactive_exit(&mut evaluator, &error),
    }
}

/// End a fatal interactive session, hanging up every live job first.
///
/// A fatal failure has no second attempt to offer, so it never refuses: it hangs
/// up unconditionally and then propagates the original failure.
fn fatal_interactive_exit(
    evaluator: &mut SessionEvaluator,
    error: &InteractiveSessionError,
) -> ExitCode {
    for failure in evaluator.hang_up(&PosixPlatform) {
        eprintln!("fsh: {}", failure.render());
    }
    report_session_error(error);
    ExitCode::FAILURE
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
    clock: Arc<SystemClock>,
    pending_notice: Option<JobNoticeId>,
    /// Whether the immediately preceding submission was a refused exit.
    exit_refused: bool,
}

impl SessionEvaluator {
    fn new(mut session: Session) -> Self {
        let clock = Arc::new(SystemClock::new());
        session.enable_interactive_job_control(clock.clone());
        Self {
            session,
            probe: NativeExecutableProbe,
            platform: PosixPlatform,
            clock,
            pending_notice: None,
            exit_refused: false,
        }
    }

    /// Resume, hang up, and wait every live job before the session ends.
    fn hang_up(&mut self, platform: &dyn Platform) -> Vec<BackgroundFailure> {
        self.session.hang_up_background_jobs(platform)
    }
}

impl InteractiveEvaluator for SessionEvaluator {
    fn next_notice(&mut self) -> Option<InteractiveNotice> {
        let notice = self.session.next_job_notice()?;
        let id = notice.id();
        let rendered = format_job_notice(notice.job(), notice.kind(), notice.command());
        let interactive_id = InteractiveNoticeId::new(id.get())?;
        self.pending_notice = Some(id);
        Some(InteractiveNotice::new(interactive_id, rendered))
    }

    fn acknowledge_notice(
        &mut self,
        notice: &InteractiveNotice,
    ) -> Result<(), InteractiveNoticeError> {
        let pending = self.pending_notice.ok_or_else(|| {
            InteractiveNoticeError::new(format!("job notice {} is not pending", notice.id().get()))
        })?;
        if pending.get() != notice.id().get() {
            return Err(InteractiveNoticeError::new(format!(
                "job notice {} does not match pending notice {}",
                notice.id().get(),
                pending.get()
            )));
        }
        self.session
            .acknowledge_job_notice(pending)
            .map_err(|error| {
                InteractiveNoticeError::with_source(
                    format!("cannot acknowledge job notice {}", pending.get()),
                    error,
                )
            })?;
        self.pending_notice = None;
        Ok(())
    }

    fn request_exit(&mut self) -> ExitDecision {
        // Decide on the current table, not on one still holding observations
        // that arrived while the editor owned the prompt.
        self.session.refresh_background_jobs();
        let live = self.session.live_background_jobs();
        if live.is_empty() {
            return ExitDecision::Permitted;
        }
        if !self.exit_refused {
            self.exit_refused = true;
            return ExitDecision::Refused {
                rendered: format_live_jobs(&live),
            };
        }
        for failure in self.session.hang_up_background_jobs(&self.platform) {
            eprintln!("fsh: {}", failure.render());
        }
        ExitDecision::Permitted
    }

    fn evaluate(&mut self, source: &str) -> Result<EvaluationControl, InteractiveDiagnostic> {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        let outcome = self.session.submit(
            "<interactive>",
            source,
            &self.probe,
            &self.platform,
            self.clock.as_ref(),
            &mut output,
        );
        // Any submitted input that is not itself an exit request, successful or
        // failing, clears the refusal: the warning must describe the state the
        // user is actually leaving. An `exit` submission is the second attempt,
        // not input between two of them, so it must not clear its own gate.
        if !matches!(outcome, Ok(SubmitOutcome::Exit(_))) {
            self.exit_refused = false;
        }
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
            Err(SubmitError::Runtime { rendered, .. }) => Err(InteractiveDiagnostic::new(rendered)),
            Err(SubmitError::Output(error)) => Err(InteractiveDiagnostic::new(format!(
                "fsh: cannot write command output: {error}\n"
            ))),
        }
    }
}

fn run_script(path: &Path) -> ExitCode {
    let filesystem = HostModuleFilesystem;
    let program = match ModuleProgramLoader::new(&filesystem, &filesystem).load_for_frontend(path) {
        Ok(program) => program,
        Err(error) => {
            if error.error().diagnostics().is_empty() {
                eprintln!("fsh: {error}");
            } else {
                eprint!("{}", error.render());
            }
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
    let registry = flash_runtime::builtin::standard_registry();
    let result = execute_module_program(
        &program,
        &cwd,
        &mut environment,
        &registry,
        &NativeExecutableProbe,
        &SessionOptions::default(),
        &PosixPlatform,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
    );

    match result {
        Ok(completion) => completion.status().map_or(ExitCode::SUCCESS, status_exit),
        Err(error) => {
            eprint!("{}", error.render());
            ExitCode::FAILURE
        }
    }
}

struct HostModuleFilesystem;

impl ModuleCanonicalizer for HostModuleFilesystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        fs::canonicalize(candidate).map_err(|error| ModulePathError::new(error.to_string()))
    }
}

impl ModuleSourceLoader for HostModuleFilesystem {
    fn load(&self, module: &flash_runtime::module::ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        fs::read(module.path()).map_err(|error| ModuleSourceError::new(error.to_string()))
    }
}

fn run_async_chain(text: String, pipefail: bool, capture_limit: usize) -> ExitCode {
    match PosixPlatform.ignore_hangup() {
        Ok(()) | Err(PlatformError::Unsupported { .. }) => {}
        Err(error) => {
            eprintln!("fsh: cannot ignore hang-up in the background child: {error}");
            return ExitCode::FAILURE;
        }
    }

    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("fsh: cannot read the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut environment = process_environment();
    let registry = flash_runtime::builtin::standard_registry();
    let options = SessionOptions::default()
        .with_pipefail(pipefail)
        .with_capture_limit(capture_limit);
    let result = execute_chain_subshell(
        "<background-chain>",
        text,
        &cwd,
        &mut environment,
        &registry,
        &NativeExecutableProbe,
        &options,
        &PosixPlatform,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
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
