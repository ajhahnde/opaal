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
use flash_cli::check::{CheckRequest, HostCheckFilesystem, check_source};
use flash_cli::cli::{Mode, parse_args};
use flash_cli::completion::{
    CompletionCandidateProvider, CompletionCatalog, CompletionSnapshotLimits,
};
use flash_cli::config::{
    ConfigDefaults, ConfigFatalError, ConfigInvocation, ConfigLimits, ConfigPlatform,
    ConfigRequest, HostConfigSource, ProcessConfigEnvironment, initialize_config,
};
use flash_cli::format::{FormatRequest, HostFormatFilesystem, format_files};
#[cfg(target_os = "redox")]
use flash_cli::history::EditorHistory;
use flash_cli::history::{HistoryPlatform, ProcessHistoryEnvironment, select_history};
use flash_cli::interactive::{
    EvaluationControl, ExitDecision, InteractiveDiagnostic, InteractiveEvaluationError,
    InteractiveEvaluator, InteractiveNotice, InteractiveNoticeError, InteractiveNoticeId,
    format_job_notice, format_live_jobs, run_interactive_driver,
};
use flash_cli::plan::{PlanRequest, inspect_source};
use flash_cli::report::{HostReport, write_report};
use flash_platform::{Platform, PlatformError};
use flash_platform_posix::PosixPlatform;
use flash_runtime::capsule::{
    MAX_CAPSULE_BYTES, decode_background_capsule, encode_supervisor_completion,
};
use flash_runtime::eval::{Clock, SystemClock};
use flash_runtime::module::{
    ModuleCanonicalizer, ModulePathError, ModuleProgramLoader, ModuleSourceError,
    ModuleSourceLoader,
};
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::script::{
    ScriptCompletion, ScriptError, execute_background_capsule, execute_module_program,
};
use flash_runtime::session::{BackgroundFailure, JobNoticeId, Session, SubmitError, SubmitOutcome};
use flash_runtime::{Environment, ScopeStack};

const HELP: &str = "Flash command shell

Usage:
  fsh [OPTIONS]
  fsh [OPTIONS] SCRIPT [ARGUMENT]...
  fsh check [--] SOURCE
  fsh check --help
  fsh plan [--] SOURCE
  fsh plan --help
  fsh format --check [--] PATH...
  fsh format --write [--] PATH...
  fsh format --help

Arguments:
  SCRIPT        Flash source file to execute
  [ARGUMENT]... Ordered UTF-8 strings exposed to the root module as $args
  SOURCE        Flash source root to analyze without execution

Options:
      --             Stop parsing options; the next operand is SCRIPT
      --no-config    Skip loading the startup configuration
      --no-history   Disable interactive history for this session
  -h, --help         Print help
  -V, --version      Print version

Every operand after SCRIPT belongs to the script, including option-like values.
";

const CHECK_HELP: &str = "Analyze Flash source without executing it

Usage:
  fsh check [--] SOURCE
  fsh check --help

Arguments:
  SOURCE     Root source whose canonical import closure is analyzed

Options:
      --         Stop parsing checker options; the next operand is SOURCE
      --help     Print checker help

SOURCE and its static imports must resolve to regular files. Canonical symlink
aliases are accepted. Checking performs syntax, module, name, signature, and
pipeline-carrier analysis without configuration, history, expansion, executable
probing, initialization, redirection, or execution. Successful checking is silent;
diagnostics are written to stderr.
";

const FORMAT_HELP: &str = "Check or rewrite Flash source formatting

Usage:
  fsh format --check [--] PATH...
  fsh format --write [--] PATH...
  fsh format --help

Options:
      --check    Report every source that is not canonically formatted
      --write    Atomically rewrite every changed source after batch preflight
      --         Stop parsing formatter options; remaining operands are paths
      --help     Print formatter help

PATH operands must name existing regular files. Directories, final symlinks,
stdin, globs, recursion, and import traversal are not supported. Successful
check and write operations are silent. Changed files preserve permission bits;
other metadata and multi-file transactionality are not promised.
";

const PLAN_HELP: &str = "Inspect one Flash execution plan without executing it

Usage:
  fsh plan [--] SOURCE
  fsh plan --help

Arguments:
  SOURCE     Regular source file containing one foreground command pipeline

Options:
      --         Stop parsing planner options; the next operand is SOURCE
      --help     Print planner help

Inspection parses and statically analyzes SOURCE, expands its one pipeline against
an empty lexical scope and the inherited environment, resolves executables through
read-only PATH metadata checks, and performs structural preflight. It does not load
configuration or history, evaluate command substitution, mutate session state, open
redirections, create pipes, spawn processes, or access a terminal. The deterministic
plan is written to stdout; diagnostics are written to stderr.
The plan includes inherited environment values and may contain secrets; treat the
output as sensitive.
";

fn main() -> ExitCode {
    let invocation = match parse_args(env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => {
            let message = error.message();
            return emit_report(HostReport::misuse(&message));
        }
    };

    match invocation.mode {
        Mode::Help => emit_report(HostReport::success(HELP.as_bytes())),
        Mode::Version => {
            let version = format!("fsh {}\n", flash_runtime::version());
            emit_report(HostReport::success(version.as_bytes()))
        }
        Mode::CheckHelp => emit_report(HostReport::success(CHECK_HELP.as_bytes())),
        Mode::Check { source } => run_checker(source),
        Mode::PlanHelp => emit_report(HostReport::success(PLAN_HELP.as_bytes())),
        Mode::Plan { source } => run_planner(source),
        Mode::FormatHelp => emit_report(HostReport::success(FORMAT_HELP.as_bytes())),
        Mode::Format { operation, paths } => run_formatter(operation, paths),
        Mode::Script { path, arguments } => run_script(&path, &arguments),
        Mode::AsyncCapsule {
            descriptor,
            completion_descriptor,
        } => run_async_capsule(descriptor, completion_descriptor),
        Mode::Interactive => run_interactive(invocation.no_config, invocation.no_history),
    }
}

fn emit_report(report: HostReport<'_>) -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut output = stdout.lock();
    let mut diagnostics = stderr.lock();
    ExitCode::from(write_report(report, &mut output, &mut diagnostics).code())
}

fn run_checker(source: PathBuf) -> ExitCode {
    let request = CheckRequest::new(source);
    let run = check_source(&request, &HostCheckFilesystem);
    if run.is_success() {
        emit_report(HostReport::success(b""))
    } else {
        let diagnostics = run.rendered_issues().concat();
        emit_report(HostReport::failure(diagnostics.as_bytes()))
    }
}

fn run_planner(source: PathBuf) -> ExitCode {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let diagnostic = format!("fsh: cannot read the current directory: {error}\n");
            return emit_report(HostReport::failure(diagnostic.as_bytes()));
        }
    };
    let request = PlanRequest::new(source, cwd, process_environment());
    let run = inspect_source(&request, &HostCheckFilesystem, &NativeExecutableProbe);
    if let Some(plan) = run.rendered_plan() {
        emit_report(HostReport::success(plan.as_bytes()))
    } else {
        let diagnostics = run.rendered_issues().concat();
        emit_report(HostReport::failure(diagnostics.as_bytes()))
    }
}

fn run_formatter(operation: flash_cli::cli::FormatOperation, paths: Vec<PathBuf>) -> ExitCode {
    let request = FormatRequest::new(operation, paths);
    let mut filesystem = HostFormatFilesystem;
    let run = format_files(&request, &mut filesystem);
    if run.is_success() {
        emit_report(HostReport::success(b""))
    } else {
        let diagnostics = run
            .failures()
            .iter()
            .map(|failure| failure.rendered())
            .collect::<String>();
        emit_report(HostReport::failure(diagnostics.as_bytes()))
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
        Err(ConfigFatalError::InvalidDefaults(detail)) => {
            eprintln!("fsh: cannot initialize startup defaults: {detail}");
            return ExitCode::FAILURE;
        }
    };

    // A safe-mode diagnostic is written before the first prompt is ever drawn.
    if let Some(diagnostic) = startup.diagnostic() {
        eprint!("{diagnostic}");
    }

    let selection = match select_history(
        no_history || !startup.interactive_settings().history(),
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
        startup.session_options(),
    );
    let mut evaluator = SessionEvaluator::new(session, startup.interactive_settings().completion());
    let mut output = io::stdout();
    let mut diagnostics = io::stderr();

    ExitCode::from(
        run_interactive_driver(
            &mut editor,
            &mut evaluator,
            startup.prompt(),
            &mut output,
            &mut diagnostics,
        )
        .code(),
    )
}

#[cfg(target_os = "redox")]
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
        Err(ConfigFatalError::InvalidDefaults(detail)) => {
            eprintln!("fsh: cannot initialize startup defaults: {detail}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(diagnostic) = startup.diagnostic() {
        eprint!("{diagnostic}");
    }

    let session = Session::with_scope(
        startup.scope().clone(),
        cwd,
        startup.environment().clone(),
        startup.session_options(),
    );
    let mut evaluator = SessionEvaluator::new(session, startup.interactive_settings().completion());
    let mut output = io::stdout();
    let mut diagnostics = io::stderr();

    // A real terminal gets the raw-mode editor; a pipe or an uncooperative
    // console falls back to canonical line reading. Both ends are checked: the
    // editor redraws its row with cursor escapes, and a session typed at from
    // a keyboard but redirected to a file must not have them written into it.
    let platform = PosixPlatform;
    let outcome = if platform.is_terminal() && platform.is_output_terminal() {
        let selection = match select_history(
            no_history || !startup.interactive_settings().history(),
            HistoryPlatform::current(),
            &ProcessHistoryEnvironment,
        ) {
            Ok(selection) => selection,
            Err(error) => {
                eprintln!("fsh: {error}");
                return ExitCode::FAILURE;
            }
        };
        let history = match EditorHistory::open(selection) {
            Ok(history) => history,
            Err(error) => {
                eprintln!("fsh: {error}");
                return ExitCode::FAILURE;
            }
        };
        let mut editor = match TerminalEditor::with_history(
            platform,
            io::stdin(),
            io::stdout(),
            Box::new(history),
        ) {
            Ok(editor) => editor,
            Err(error) => {
                eprintln!("fsh: {error}");
                return ExitCode::FAILURE;
            }
        };
        run_interactive_driver(
            &mut editor,
            &mut evaluator,
            startup.prompt(),
            &mut output,
            &mut diagnostics,
        )
    } else {
        let mut editor = RawLineEditor::new();
        run_interactive_driver(
            &mut editor,
            &mut evaluator,
            startup.prompt(),
            &mut output,
            &mut diagnostics,
        )
    };

    ExitCode::from(outcome.code())
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
    completion_enabled: bool,
    completion_provider: CompletionCandidateProvider,
}

impl SessionEvaluator {
    fn new(mut session: Session, completion_enabled: bool) -> Self {
        let clock = Arc::new(SystemClock::new());
        session.enable_interactive_job_control(clock.clone());
        Self {
            session,
            probe: NativeExecutableProbe,
            platform: PosixPlatform,
            clock,
            pending_notice: None,
            exit_refused: false,
            completion_enabled,
            completion_provider: CompletionCandidateProvider::new(
                CompletionSnapshotLimits::default(),
            ),
        }
    }
}

impl InteractiveEvaluator for SessionEvaluator {
    fn completion_catalog(&mut self) -> Option<CompletionCatalog> {
        Some(if self.completion_enabled {
            self.completion_provider.snapshot(
                self.session.registry(),
                self.session.scope(),
                self.session.cwd(),
                self.session.environment(),
                &|| false,
            )?
        } else {
            CompletionCatalog::new()
        })
    }

    fn fatal_cleanup(&mut self) -> Vec<String> {
        self.session
            .hang_up_background_jobs(&self.platform)
            .into_iter()
            .map(|failure| format!("fsh: {}\n", failure.render()))
            .collect()
    }

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

    fn evaluate(
        &mut self,
        source: &str,
        output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        let outcome = self.session.submit(
            "<interactive>",
            source,
            &self.probe,
            &self.platform,
            self.clock.as_ref(),
            output,
        );
        // Any submitted input that is not itself an exit request, successful or
        // failing, clears the refusal: the warning must describe the state the
        // user is actually leaving. An `exit` submission is the second attempt,
        // not input between two of them, so it must not clear its own gate.
        if !matches!(outcome, Ok(SubmitOutcome::Exit(_))) {
            self.exit_refused = false;
        }
        match outcome {
            Ok(SubmitOutcome::Continued) => Ok(EvaluationControl::Continue),
            Ok(SubmitOutcome::Exit(code)) => Ok(EvaluationControl::Exit(code)),
            Err(SubmitError::Diagnostic(rendered)) => {
                Err(InteractiveDiagnostic::new(rendered).into())
            }
            Err(SubmitError::Runtime { rendered, .. }) => {
                Err(InteractiveDiagnostic::new(rendered).into())
            }
            Err(SubmitError::Output(error)) => {
                Err(InteractiveEvaluationError::ProgramOutput(error))
            }
        }
    }
}

fn run_script(path: &Path, arguments: &[String]) -> ExitCode {
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
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let result = execute_module_program(
        &program,
        arguments,
        &cwd,
        &mut environment,
        &registry,
        &NativeExecutableProbe,
        &SessionOptions::default(),
        &PosixPlatform,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
        &mut output,
    );
    let flush = output.flush();
    drop(output);

    finish_script_report(result, flush)
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

fn run_async_capsule(descriptor: u32, completion_descriptor: Option<u32>) -> ExitCode {
    match PosixPlatform.ignore_hangup() {
        Ok(()) | Err(PlatformError::Unsupported { .. }) => {}
        Err(error) => {
            eprintln!("fsh: cannot ignore hang-up in the background child: {error}");
            return ExitCode::FAILURE;
        }
    }

    let mut endpoint = match PosixPlatform.inherit_descriptor(descriptor) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("fsh: cannot inherit execution capsule descriptor: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match endpoint.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len().saturating_add(count) > MAX_CAPSULE_BYTES + 18 {
                    eprintln!("fsh: execution capsule exceeds its byte limit");
                    return ExitCode::FAILURE;
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) => {
                eprintln!("fsh: cannot read execution capsule: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    drop(endpoint);

    let capsule = match decode_background_capsule(&bytes) {
        Ok(capsule) => capsule,
        Err(error) => {
            eprintln!("fsh: invalid execution capsule: {error}");
            return ExitCode::FAILURE;
        }
    };
    let registry = flash_runtime::builtin::standard_registry();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let result = execute_background_capsule(
        capsule,
        &registry,
        &NativeExecutableProbe,
        &PosixPlatform,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
        &mut output,
    );
    let flush = output.flush();
    drop(output);
    let result = match (result, &flush) {
        (Ok((completion, envelope)), Ok(())) => {
            if let Some(descriptor) = completion_descriptor {
                let bytes = match encode_supervisor_completion(&envelope) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        eprintln!("fsh: cannot encode supervisor completion: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                let mut endpoint = match PosixPlatform.inherit_descriptor(descriptor) {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        eprintln!("fsh: cannot inherit completion descriptor: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                let mut written = 0;
                while written < bytes.len() {
                    match endpoint.write(&bytes[written..]) {
                        Ok(0) => {
                            eprintln!("fsh: completion descriptor accepted zero bytes");
                            return ExitCode::FAILURE;
                        }
                        Ok(count) => written += count,
                        Err(error) => {
                            eprintln!("fsh: cannot write supervisor completion: {error}");
                            return ExitCode::FAILURE;
                        }
                    }
                }
            }
            Ok(completion)
        }
        (Ok((completion, _)), Err(_)) => Ok(completion),
        (Err(error), _) => Err(error),
    };
    finish_script_report(result, flush)
}

fn finish_script_report(
    result: Result<ScriptCompletion, ScriptError>,
    output_flush: io::Result<()>,
) -> ExitCode {
    if let Err(flush_error) = output_flush {
        let mut diagnostics = match &result {
            Ok(completion) => render_background_failures(completion.background_failures()),
            Err(error) => {
                let mut diagnostics = render_background_failures(error.background_failures());
                diagnostics.push_str(error.render());
                diagnostics
            }
        };
        diagnostics.push_str(&format!(
            "fsh: cannot flush command output: {flush_error}\n"
        ));
        return emit_report(HostReport::failure(diagnostics.as_bytes()));
    }

    match result {
        Ok(completion) => {
            let diagnostics = render_background_failures(completion.background_failures());
            match completion.status() {
                Some(status) if diagnostics.is_empty() => {
                    emit_report(HostReport::completed(status, b""))
                }
                Some(status) => emit_report(HostReport::completed_with_diagnostic(
                    status,
                    b"",
                    diagnostics.as_bytes(),
                )),
                None if diagnostics.is_empty() => emit_report(HostReport::success(b"")),
                None => emit_report(HostReport::failure(diagnostics.as_bytes())),
            }
        }
        Err(error) => {
            let mut diagnostics = render_background_failures(error.background_failures());
            diagnostics.push_str(error.render());
            emit_report(HostReport::failure(diagnostics.as_bytes()))
        }
    }
}

fn render_background_failures(failures: &[BackgroundFailure]) -> String {
    failures
        .iter()
        .map(|failure| format!("fsh: {}\n", failure.render()))
        .collect()
}

struct NativeExecutableProbe;

impl ExecutableProbe for NativeExecutableProbe {
    fn is_executable(&self, path: &OsStr) -> bool {
        fs::metadata(Path::new(path))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
}
