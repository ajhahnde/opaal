#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use opaal_cli::check::{CheckRequest, HostCheckFilesystem, check_source};
use opaal_cli::cli::{Mode, parse_args};
use opaal_cli::completion::CompletionCatalog;
use opaal_cli::editor::{EditorPrompt, LineEditor};
use opaal_cli::format::{FormatRequest, HostFormatFilesystem, format_files};
use opaal_cli::history::HistorySelection;
use opaal_cli::interactive::{
    EvaluationControl, InteractiveDiagnostic, InteractiveEvaluationError, InteractiveEvaluator,
    run_interactive_driver,
};
use opaal_cli::plan::inspect_source;
use opaal_cli::project::{
    AuditRequest, CheckProjectRequest, ExecuteProjectRequest, InspectProjectRequest,
    PlanProjectRequest, audit_explicit_journal, check_explicit_project, execute_explicit_plan,
    inspect_project, plan_explicit_project,
};
use opaal_cli::report::{HostReport, write_report};
use opaal_cli::{RawLineEditor, ReedlineEditor};
use opaal_platform_posix::PosixPlatform;
use opaal_runtime::eval::{Clock, FakeClock};
use opaal_runtime::module::ModuleProgramLoader;
use opaal_runtime::outcome::{OutcomeEvidence, PrimaryOutcome};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::resolve::ExecutableProbe;
use opaal_runtime::script::{ScriptError, ScriptExecutionOutcome, execute_module_program_outcome};
use opaal_runtime::session::{BackgroundFailure, Session, SubmitError, SubmitOutcome};
use opaal_runtime::{Environment, Status, Value};

const HELP: &str = "OPAAL language client

Usage:
  opaal
  opaal SCRIPT [ARGUMENT]...
  opaal check [--] SOURCE
  opaal check --project opaal.toml --task TASK --environment ID --authority PATH --tools PATH [--input NAME=VALUE]... [--format json]
  opaal check --help
  opaal task inspect --project opaal.toml TASK
  opaal task --help
  opaal plan [--] SOURCE
  opaal plan --project opaal.toml --task TASK --environment ID --authority PATH --tools PATH [--input NAME=VALUE]... --expires-in SECONDSs --out PATH
  opaal plan --help
  opaal execute --plan PATH --accept DIGEST --run-id ID --authority PATH --secret-stdin ID --journal PATH
  opaal execute --help
  opaal audit --project opaal.toml --journal PATH --out PATH
  opaal audit --help
  opaal format --check [--] PATH...
  opaal format --write [--] PATH...
  opaal format --help

Arguments:
  SCRIPT        .opaal source file to execute
  [ARGUMENT]... Ordered UTF-8 strings exposed to the root module as $args
  SOURCE        .opaal source root to analyze without execution

Options:
      --             Stop parsing options; the next operand is SCRIPT
  -h, --help         Print help
  -V, --version      Print version

Every operand after SCRIPT belongs to the script, including option-like values.
";

const CHECK_HELP: &str = "Analyze OPAAL source without executing it

Usage:
  opaal check [--] SOURCE
  opaal check --project opaal.toml --task TASK --environment ID --authority PATH --tools PATH [--input NAME=VALUE]... [--format json]
  opaal check --help

SOURCE and every static import must be regular .opaal files. Checking performs
syntax, module, name, signature, and carrier analysis without ambient
configuration, history, host discovery, or execution. Success is silent;
diagnostics use stderr.
";

const TASK_HELP: &str = "Inspect typed tasks without executing them

Usage:
  opaal task inspect --project opaal.toml TASK
  opaal task --help

The exact manifest path is required. Inspection reads only the explicit project
source closure and reports the task's shared action signature and effect identities.
";

const FORMAT_HELP: &str = "Check or rewrite OPAAL source formatting

Usage:
  opaal format --check [--] PATH...
  opaal format --write [--] PATH...
  opaal format --help

Paths must be explicit regular .opaal files. Directories, final symlinks, stdin,
globs, recursion, and import traversal are not supported.
";

const PLAN_HELP: &str = "Inspect the OPAAL planning boundary without execution

Usage:
  opaal plan [--] SOURCE
  opaal plan --project opaal.toml --task TASK --environment ID --authority PATH --tools PATH [--input NAME=VALUE]... --expires-in SECONDSs --out PATH
  opaal plan --help

The source form remains a host-free refusal. The project form writes one
identity-bound canonical plan without executing the task, probing a tool,
materializing a secret, or contacting an endpoint.
";

const EXECUTE_HELP: &str = "Execute one explicitly accepted OPAAL plan

Usage:
  opaal execute --plan PATH --accept sha256:DIGEST --run-id ID --authority PATH --secret-stdin ID --journal PATH
  opaal execute --help

Acceptance belongs only to this request. Execution revalidates every bound
identity before affected work and writes an exclusive hash-chained journal.
";

const AUDIT_HELP: &str = "Audit one OPAAL run journal without execution

Usage:
  opaal audit --project opaal.toml --journal PATH --out PATH
  opaal audit --help

Audit validates the closed schema, sequence, and hash chain and writes one
exclusive complete or incomplete evidence artifact. It never resumes a run.
";

fn main() -> ExitCode {
    let invocation = match parse_args(env::args_os().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => return emit_report(HostReport::misuse(&error.message())),
    };
    match invocation.mode {
        Mode::Help => emit_report(HostReport::success(HELP.as_bytes())),
        Mode::Version => {
            let version = format!("opaal {}\n", opaal_runtime::version());
            emit_report(HostReport::success(version.as_bytes()))
        }
        Mode::CheckHelp => emit_report(HostReport::success(CHECK_HELP.as_bytes())),
        Mode::Check { source } => run_checker(source),
        Mode::ProjectCheck {
            project,
            task,
            environment,
            authority,
            tools,
            inputs,
            format_json,
        } => run_project_checker(
            project,
            task,
            environment,
            authority,
            tools,
            inputs,
            format_json,
        ),
        Mode::TaskHelp => emit_report(HostReport::success(TASK_HELP.as_bytes())),
        Mode::TaskInspect { project, task } => run_task_inspect(project, task),
        Mode::PlanHelp => emit_report(HostReport::success(PLAN_HELP.as_bytes())),
        Mode::Plan { source } => run_planner(source),
        Mode::ProjectPlan {
            project,
            task,
            environment,
            authority,
            tools,
            inputs,
            expires_in_seconds,
            out,
        } => run_project_planner(
            project,
            task,
            environment,
            authority,
            tools,
            inputs,
            expires_in_seconds,
            out,
        ),
        Mode::ExecuteHelp => emit_report(HostReport::success(EXECUTE_HELP.as_bytes())),
        Mode::Execute {
            plan,
            accept,
            run_id,
            authority,
            secret_stdin,
            journal,
        } => run_execute(plan, accept, run_id, authority, secret_stdin, journal),
        Mode::AuditHelp => emit_report(HostReport::success(AUDIT_HELP.as_bytes())),
        Mode::Audit {
            project,
            journal,
            out,
        } => run_audit(project, journal, out),
        Mode::FormatHelp => emit_report(HostReport::success(FORMAT_HELP.as_bytes())),
        Mode::Format { operation, paths } => run_formatter(operation, paths),
        Mode::Script { path, arguments } => run_script(&path, &arguments),
        Mode::Interactive => run_interactive(),
    }
}

fn emit_report(report: HostReport<'_>) -> ExitCode {
    let mut output = io::stdout().lock();
    let mut diagnostics = io::stderr().lock();
    ExitCode::from(write_report(report, &mut output, &mut diagnostics).code())
}

fn has_opaal_extension(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("opaal"))
}

fn reject_non_opaal(path: &Path, role: &str) -> Option<ExitCode> {
    (!has_opaal_extension(path)).then(|| {
        let message = format!(
            "opaal: {role} `{}` must use the .opaal extension\n",
            path.display()
        );
        emit_report(HostReport::failure(message.as_bytes()))
    })
}

fn run_checker(source: PathBuf) -> ExitCode {
    if let Some(exit) = reject_non_opaal(&source, "source") {
        return exit;
    }
    let request = CheckRequest::new(source);
    let run = check_source(&request, &HostCheckFilesystem);
    if run.is_success() {
        emit_report(HostReport::success(b""))
    } else {
        emit_report(HostReport::failure(
            run.rendered_issues().concat().as_bytes(),
        ))
    }
}

fn run_task_inspect(project: PathBuf, task: String) -> ExitCode {
    match inspect_project(&InspectProjectRequest::new(project, task)) {
        Ok(run) => emit_report(HostReport::success(run.output())),
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_project_checker(
    project: PathBuf,
    task: String,
    environment: String,
    authority: PathBuf,
    tools: PathBuf,
    inputs: Vec<(String, String)>,
    format_json: bool,
) -> ExitCode {
    let request = CheckProjectRequest::new(project, task, environment, authority, tools, inputs);
    let request = if format_json {
        request.with_json()
    } else {
        request
    };
    match check_explicit_project(&request) {
        Ok(run) if run.is_successful() => emit_report(HostReport::success(run.output())),
        Ok(run) => {
            let refused = Status::exit(1, opaal_runtime::Duration::ZERO)
                .expect("one is a valid refused-check status");
            emit_report(HostReport::completed_with_diagnostic(
                &refused,
                run.output(),
                run.diagnostic(),
            ))
        }
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

fn run_formatter(operation: opaal_cli::cli::FormatOperation, paths: Vec<PathBuf>) -> ExitCode {
    if let Some(path) = paths.iter().find(|path| !has_opaal_extension(path)) {
        return reject_non_opaal(path, "format path").expect("a non-OPAAL path was selected");
    }
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

fn run_planner(source: PathBuf) -> ExitCode {
    if let Some(exit) = reject_non_opaal(&source, "source") {
        return exit;
    }
    let run = inspect_source(&source, &HostCheckFilesystem);
    let diagnostics = run.rendered_issues().concat();
    emit_report(HostReport::failure(diagnostics.as_bytes()))
}

#[allow(clippy::too_many_arguments)]
fn run_project_planner(
    project: PathBuf,
    task: String,
    environment: String,
    authority: PathBuf,
    tools: PathBuf,
    inputs: Vec<(String, String)>,
    expires_in_seconds: u64,
    out: PathBuf,
) -> ExitCode {
    let request = PlanProjectRequest::new(
        project,
        task,
        environment,
        authority,
        tools,
        inputs,
        expires_in_seconds,
        out,
    );
    match plan_explicit_project(&request) {
        Ok(run) if run.is_successful() => emit_report(HostReport::success(run.output())),
        Ok(run) => {
            let refused = Status::exit(1, opaal_runtime::Duration::ZERO)
                .expect("one is a valid refused-plan status");
            emit_report(HostReport::completed_with_diagnostic(
                &refused,
                run.output(),
                run.diagnostic(),
            ))
        }
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

fn run_audit(project: PathBuf, journal: PathBuf, out: PathBuf) -> ExitCode {
    match audit_explicit_journal(&AuditRequest::new(project, journal, out)) {
        Ok(run) if run.is_complete() => emit_report(HostReport::success(run.output())),
        Ok(run) => {
            let incomplete = Status::exit(1, opaal_runtime::Duration::ZERO)
                .expect("one is a valid incomplete-audit status");
            emit_report(HostReport::completed(&incomplete, run.output()))
        }
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

fn run_execute(
    plan: PathBuf,
    accept: String,
    run_id: String,
    authority: PathBuf,
    secret_stdin: String,
    journal: PathBuf,
) -> ExitCode {
    let request =
        ExecuteProjectRequest::new(plan, accept, run_id, authority, secret_stdin, journal);
    let stdin = io::stdin();
    let is_terminal = stdin.is_terminal();
    let mut input = stdin.lock();
    match execute_explicit_plan(&request, is_terminal, &mut input) {
        Ok(run) if run.is_successful() => emit_report(HostReport::success(run.output())),
        Ok(run) => {
            let failed = Status::exit(1, opaal_runtime::Duration::ZERO)
                .expect("one is a valid failed-execution status");
            emit_report(HostReport::completed(&failed, run.output()))
        }
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

fn run_script(path: &Path, arguments: &[String]) -> ExitCode {
    if let Some(exit) = reject_non_opaal(path, "script") {
        return exit;
    }
    let filesystem = HostCheckFilesystem;
    let program = match ModuleProgramLoader::new(&filesystem, &filesystem).load_for_frontend(path) {
        Ok(program) => program,
        Err(error) => {
            let rendered = if error.error().diagnostics().is_empty() {
                format!("opaal: {error}\n")
            } else {
                error.render().to_owned()
            };
            return emit_report(HostReport::failure(rendered.as_bytes()));
        }
    };
    let mut environment = Environment::new();
    let registry = opaal_runtime::builtin::standard_registry();
    let mut output = io::stdout().lock();
    let outcome = execute_module_program_outcome(
        &program,
        arguments,
        Path::new(""),
        &mut environment,
        &registry,
        &NoHost,
        &SessionOptions::default(),
        &PosixPlatform,
        Arc::new(FakeClock::new()) as Arc<dyn Clock>,
        &mut output,
    );
    let flush = output.flush();
    drop(output);
    finish_script_outcome_report(outcome, flush)
}

fn run_interactive() -> ExitCode {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        let mut editor = match ReedlineEditor::with_history(HistorySelection::Disabled) {
            Ok(editor) => editor,
            Err(error) => {
                let message = format!("opaal: {error}\n");
                return emit_report(HostReport::failure(message.as_bytes()));
            }
        };
        run_interactive_with_editor(&mut editor)
    } else {
        let mut editor = RawLineEditor::new();
        run_interactive_with_editor(&mut editor)
    }
}

fn run_interactive_with_editor(editor: &mut dyn LineEditor) -> ExitCode {
    let session = Session::new(
        PathBuf::new(),
        Environment::new(),
        SessionOptions::default(),
    );
    let mut evaluator = OpaalEvaluator { session };
    let mut output = io::stdout();
    let mut diagnostics = io::stderr();
    ExitCode::from(
        run_interactive_driver(
            editor,
            &mut evaluator,
            &EditorPrompt::default(),
            &mut output,
            &mut diagnostics,
        )
        .code(),
    )
}

struct OpaalEvaluator {
    session: Session,
}

impl InteractiveEvaluator for OpaalEvaluator {
    fn completion_catalog(&mut self) -> Option<CompletionCatalog> {
        Some(
            CompletionCatalog::from_runtime(self.session.registry(), self.session.scope())
                .with_operations(self.session.visible_operation_names()),
        )
    }

    fn evaluate(
        &mut self,
        source: &str,
        output: &mut dyn Write,
    ) -> Result<EvaluationControl, InteractiveEvaluationError> {
        let clock = FakeClock::new();
        let outcome = self
            .session
            .submit_with_value(
                source_name(),
                source,
                &NoHost,
                &PosixPlatform,
                &clock,
                output,
            )
            .and_then(|(outcome, value)| {
                if matches!(outcome, SubmitOutcome::Continued)
                    && !matches!(value, Value::Null | Value::Status(_))
                {
                    writeln!(output, "{value}").map_err(SubmitError::Output)?;
                }
                Ok(outcome)
            });
        match outcome {
            Ok(SubmitOutcome::Continued) => Ok(EvaluationControl::Continue),
            Ok(SubmitOutcome::Exit(code)) => Ok(EvaluationControl::Exit(code)),
            Ok(SubmitOutcome::Cancelled(cancellation)) => Err(InteractiveDiagnostic::new(format!(
                "cancelled[{:?}] at bytes {}..{}\n",
                cancellation.reason(),
                cancellation.span().start(),
                cancellation.span().end()
            ))
            .into()),
            Ok(SubmitOutcome::Refused(refusal)) => Err(InteractiveDiagnostic::new(format!(
                "refused[{}]: operation `{}` did not begin\n",
                refusal.reason(),
                refusal.operation()
            ))
            .into()),
            Err(SubmitError::Diagnostic(rendered)) | Err(SubmitError::Runtime { rendered, .. }) => {
                Err(InteractiveDiagnostic::new(rendered).into())
            }
            Err(SubmitError::Output(error)) => {
                Err(InteractiveEvaluationError::ProgramOutput(error))
            }
        }
    }
}

const fn source_name() -> &'static str {
    "<interactive>"
}

fn finish_script_outcome_report(
    outcome: ScriptExecutionOutcome,
    output_flush: io::Result<()>,
) -> ExitCode {
    let (primary, evidence, _downstream) = outcome.into_parts();
    if let Err(error) = output_flush {
        let mut diagnostics = render_outcome_evidence(&evidence);
        diagnostics.push_str(&format!("opaal: fatal[output]: {error}\n"));
        return emit_report(HostReport::failure(diagnostics.as_bytes()));
    }
    let evidence = render_outcome_evidence(&evidence);
    match primary {
        PrimaryOutcome::Completed(completion) => {
            let mut diagnostics = render_background_failures(completion.background_failures());
            diagnostics.push_str(&evidence);
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
                None => emit_report(HostReport::success_with_diagnostic(
                    b"",
                    diagnostics.as_bytes(),
                )),
            }
        }
        PrimaryOutcome::Error(error) => {
            let mut diagnostics = render_background_failures(error.background_failures());
            diagnostics.push_str(error.render());
            diagnostics.push_str(&evidence);
            emit_report(HostReport::failure(diagnostics.as_bytes()))
        }
        PrimaryOutcome::Cancelled(cancellation) => {
            let mut diagnostics = format!(
                "opaal: cancelled[{:?}] at bytes {}..{}\n",
                cancellation.reason(),
                cancellation.span().start(),
                cancellation.span().end()
            );
            diagnostics.push_str(&evidence);
            emit_report(HostReport::failure(diagnostics.as_bytes()))
        }
        PrimaryOutcome::Refused(refusal) => {
            let mut diagnostics = format!(
                "opaal: refused[{}]: operation `{}` did not begin\n",
                refusal.reason(),
                refusal.operation()
            );
            diagnostics.push_str(&evidence);
            emit_report(HostReport::failure(diagnostics.as_bytes()))
        }
        PrimaryOutcome::FatalHostFailure(failure) => {
            let mut diagnostics = format!(
                "opaal: fatal[{}]: {}\n",
                failure.kind().name(),
                failure.message()
            );
            diagnostics.push_str(&evidence);
            emit_report(HostReport::failure(diagnostics.as_bytes()))
        }
    }
}

fn render_outcome_evidence(evidence: &[OutcomeEvidence<ScriptError>]) -> String {
    let mut rendered = String::new();
    for item in evidence {
        match item {
            OutcomeEvidence::Completed(completed) => {
                rendered.push_str(&format!(
                    "opaal: completed evidence: {}",
                    completed.operation()
                ));
                if let Some(status) = completed.status() {
                    rendered.push_str(&format!(" ({status})"));
                }
                rendered.push('\n');
            }
            OutcomeEvidence::PartialEffect(partial) => rendered.push_str(&format!(
                "opaal: partial effect [{}]: {}\n",
                partial.operation(),
                partial.detail()
            )),
            OutcomeEvidence::CleanupFailure(error) => {
                rendered.push_str("opaal: cleanup failure: ");
                rendered.push_str(error.render().trim_start_matches("opaal: "));
                if !rendered.ends_with('\n') {
                    rendered.push('\n');
                }
            }
        }
    }
    rendered
}

fn render_background_failures(failures: &[BackgroundFailure]) -> String {
    failures
        .iter()
        .map(|failure| format!("opaal: {}\n", failure.render()))
        .collect()
}

struct NoHost;

impl ExecutableProbe for NoHost {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}
