#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    AuditRequest, CheckProjectRequest, ExecuteProjectRequest, InitProjectRequest,
    InspectProjectRequest, PlanProjectRequest, ProjectInputBinding, audit_explicit_journal,
    check_explicit_project, doctor_project, execute_explicit_plan, explain_task, init_project,
    inspect_audit_artifact, inspect_plan_artifact, inspect_project, plan_explicit_project,
    run_accepted_task,
};
use opaal_cli::report::{HostExit, HostReport, write_report};
use opaal_cli::{RawLineEditor, ReedlineEditor};
use opaal_platform_posix::PosixPlatform;
use opaal_platform_posix::standard_host::{PosixStandardHost, worker_entry};
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EvaluationContextId, RequiredEnforcement,
};
use opaal_runtime::eval::SystemClock;
use opaal_runtime::eval::{CancellationToken, Clock};
use opaal_runtime::module::ModuleProgramLoader;
use opaal_runtime::operational::standard::{StandardLimits, StandardState};
use opaal_runtime::operational::standard_source::StandardBinding;
use opaal_runtime::outcome::{OutcomeEvidence, PrimaryOutcome};
use opaal_runtime::plan::SessionOptions;
use opaal_runtime::script::{
    ScriptError, ScriptExecutionOutcome, execute_ambient_module_program_outcome_with_standard_host,
};
use opaal_runtime::session::{BackgroundFailure, Session, SubmitError, SubmitOutcome};
use opaal_runtime::{NativeSessionSnapshot, Status, Value};

const HELP: &str = "OPAAL language client

Usage:
  opaal
  opaal SCRIPT [ARGUMENT]...
  opaal check [--] SOURCE
  opaal check --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... [--format json]
  opaal check --help
  opaal test [--list] SOURCE [--case NAME] [--format text|json]
  opaal test --help
  opaal project init --example image-check --destination PATH --environment offline
  opaal project doctor --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... [--format text|json]
  opaal project --help
  opaal task inspect --project opaal.toml TASK [--format json]
  opaal task list --project opaal.toml [--format json]
  opaal task run --plan PATH --accept DIGEST --journal PATH --receipt-out PATH --audit-out PATH [--require-true]
  opaal task explain --receipt PATH [--audit PATH] [--format text|json]
  opaal task --help
  opaal plan [--] SOURCE
  opaal plan --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... --expires-in SECONDSs --out PATH [--format digest]
  opaal plan inspect PATH
  opaal plan --help
  opaal execute --plan PATH --accept DIGEST [--run-id ID] [--secret-stdin ID] --journal PATH [--receipt-out PATH] [--require-true]
  opaal execute --help
  opaal audit --project opaal.toml --journal PATH --out PATH
  opaal audit inspect PATH
  opaal audit --help
  opaal format --check [--] PATH...
  opaal format --write [--] PATH...
  opaal format --help

Arguments:
  SCRIPT        .opaal source file to execute
  [ARGUMENT]... Ordered UTF-8 strings exposed to the root module as args
  SOURCE        .opaal source root to analyze without execution

Options:
      --             Stop parsing options; the next operand is SCRIPT
  -h, --help         Print help
  -V, --version      Print version

Every operand after SCRIPT belongs to the script, including option-like values.
";

const TEST_HELP: &str = "Run named pure OPAAL policy tests

Usage:
  opaal test [--list] [--case NAME] [--format text|json] [--] SOURCE
  opaal test --help

Only root exports named test_ are discovered, in name order. Each must resolve
to a non-generic ordinary zero-argument Bool function. --case selects one exact
export; signatures of the entire inventory are still validated. --list never
initializes source. Running admits only statically proven pure initializers and
reachable calls, then invokes each case in fresh canonical module scopes with
the ordinary evaluator and no environment, process, I/O, random or other host.
true passes; false and uncaught ordinary Error fail and allow later cases.
Caught language resource Errors retain their ordinary semantics and counters.
Uncaught resource Error, refusal, invalid return and infrastructure stop the
suite, with remaining cases unrun. Limits: 128 cases, one million steps per
case including initialization, ten million suite steps, depth 256, one million
collection items, 16 MiB retained bytes per case, and a 64 KiB result view.
JSON is the closed opaal.test-results.v1 view, version 1, followed by LF; safe
codes and byte spans omit thrown messages and source snippets. Text uses fixed
facts without terminal controls. Exit: all passed/listed 0, ordinary failure 1,
invalid/refused/resource/infrastructure/output failure 2, requested cancel 130.
No source rewrite, directory discovery, fixtures, retries or host grants occur.
";

const CHECK_HELP: &str = "Analyze OPAAL source without executing it

Usage:
  opaal check [--] SOURCE
  opaal check --format json [--] SOURCE
  opaal check --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... [--format json]
  opaal check --help

SOURCE and every static import must be regular .opaal files. Checking performs
syntax, module, name, signature, and carrier analysis without ambient
configuration, history, host discovery, or execution. Human checks are silent
on success and send diagnostics to stderr. Source --format json writes one
schema_version 1 diagnostic document to stdout, followed by LF. Its outcome is
complete, invalid, cancelled, refused, or failed; diagnostics preserve analysis
order, primary/related label messages and notes. Locations contain source display
identities and zero-based UTF-8 byte ranges with exclusive ends, or null when
unavailable. Each diagnostic has a nonempty code, severity (error, warning, or
note), message, primary_message (string or null), notes (string array), source
(string or null), range ({start,end} or null), and related (array of objects
with message, source, and range). Without errors, warnings and notes retain
outcome complete. Multiple failure kinds use refused > failed > invalid
precedence. Cancellation and budget refusal discard partial findings.
Example: {\"schema_version\":1,\"outcome\":\"complete\",\"diagnostics\":[]}
Source text is omitted. Native paths escape controls, backslashes,
and invalid bytes (\\xNN); display identities are not filesystem operands.
Message wording may change. Exit is 0 for complete, 1 for source failures, and
2 for usage errors. Pre-analysis failures use stderr; output failures may leave
truncated stdout. Consumers must validate the complete document and exit status
and reject unknown schema versions. OS termination need not emit final JSON.
Project --format json uses the separate project-check artifact schema.
The environment selects authority and tools. --input
parses lexical values without I/O; --input-file snapshots one regular file for
a Path parameter.
";

const TASK_HELP: &str = "Inspect tasks, run accepted plans and explain recorded outcomes

Usage:
  opaal task inspect --project opaal.toml TASK [--format json]
  opaal task list --project opaal.toml [--format json]
  opaal task run --help
  opaal task explain --help
  opaal task --help

The exact manifest path is required. Inspection reads only the explicit project
source closure and reports shared task/action identities, typed parameters and
binding forms, result, documentation, effects, tools, and environments. Listing
sorts tasks by qualified identity; an empty list succeeds. JSON uses the closed
opaal.project-inspection.v1 schema, version 1, and ends with LF. Tools are
manifest requirements, not observations of installed executables. Discovery never
runs initializers or tools and never searches upward or reads account settings.
";

const TASK_RUN_HELP: &str = "Run one explicitly accepted task with complete evidence

Usage:
  opaal task run --plan PATH --accept DIGEST --journal PATH --receipt-out PATH --audit-out PATH [--run-id ID] [--secret-stdin ID] [--require-true]

The exact saved plan and digest acceptance are required. No new plan or input
override is inferred. Existing source, inputs, authority, tools, targets and
expiry are revalidated before effects. All three evidence paths must be fresh,
distinct project-contained files. Audit is reserved before effects; partial
reservations remain on failure. Owned finish precedes one bounded journal audit.
Program stdout is unchanged; the fixed safe administrative summary uses stderr.
Successful execution, cleanup, journal, receipt, audit and summary are required
for exit 0. --require-true selects Bool true/false as 0/1; other failures exit 2.
Requested cancellation retains 130 despite later evidence failure; timeout is 2.
Recover valid journal evidence with audit into a fresh path. Retry requires a
new reviewed plan and fresh files; no task or stdin is replayed automatically.
";

const TASK_EXPLAIN_HELP: &str = "Explain explicit recorded task evidence without execution

Usage:
  opaal task explain --receipt PATH [--audit PATH] [--format text|json]

Only bounded receipt/audit artifacts are read. Source, tools, journal and daemon
are not reopened. Matching identities and known complete terminal outcomes are
required. Missing audit or legacy unknown facts cannot prove a passed check.
Complete audit alone does not mean the task passed. Text and closed JSON
opaal.task-explanation.v1 contain fixed facts, safe codes and transfer counts;
messages and payloads are omitted. A valid view exits 0 even for recorded failure;
invalid, inconsistent or unwritable views exit 2. Recorded evidence does not
authenticate its author or prove the original writer's fsync or command exit.
";

const PROJECT_HELP: &str = "Prepare and diagnose an explicit image-check project

Usage:
  opaal project init --example image-check --destination PATH --environment offline
  opaal project init --example image-check --destination PATH --environment ci --docker ABS_PATH --socket ABS_PATH --qualified-version VERSION
  opaal project doctor --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... [--format text|json]
  opaal project --help

Init embeds the fixed version-matched example and selects the offline host lock.
The parent must exist without symlink components. Destination must be absent.
Files are exclusive mode0600; directories mode0700. Failure after creation
retains partial files: inspect them separately and retry in a fresh destination.
Live setup requires Linux x86_64 and explicit independently qualified Docker
29.9.0 executable/socket facts. It hashes the executable without launching it,
creates four empty directories and binds the unchanged profile. No provisioning,
account reads, daemon connection or automatic plan acceptance occurs.

Doctor observes explicit bounded local controls, source, input files and tool
identities through the shared static checker. It never consumes task stdin,
initializes a module, materializes a secret, launches a tool or contacts Docker.
Text defaults; JSON uses the closed opaal.project-doctor.v1 schema, version1,
with final LF and fixed safe findings. Live readiness is always unknown for ci;
local ready is no daemon-health or executable-plan assertion. Exit0 means local
ready, 1 means findings, 2 means invalid invocation/controls or I/O, and requested
cancellation is130. Init succeeds with0 and refuses or fails incomplete with2.
";

const FORMAT_HELP: &str = "Check or rewrite OPAAL source formatting

Usage:
  opaal format --check [--] PATH...
  opaal format --write [--] PATH...
  opaal format --help

Paths must be explicit regular .opaal files. Directories, final symlinks, stdin,
globs, recursion, and import traversal are not supported.

Named function and action bodies use separate brace lines; effects requests
retain their semicolons. Existing same-line declarations remain executable.
Check is read-only, silent on success, and reports FMT001 for noncanonical source.
Write preflights all operands, then atomically replaces each changed file while
preserving mode bits. A later replacement failure can leave earlier files changed.
After a rewrite, regenerate project plans before accepting and executing them.
";

const PLAN_HELP: &str = "Inspect the OPAAL planning boundary without execution

Usage:
  opaal plan [--] SOURCE
  opaal plan --project opaal.toml --task TASK --environment ID [--input NAME=VALUE | --input-file NAME=PATH]... --expires-in SECONDSs --out PATH [--format digest]
  opaal plan inspect PATH
  opaal plan --help

The source form remains a host-free refusal. The project form writes one
identity-bound canonical plan without executing the task, probing a tool,
materializing a secret, or contacting an endpoint. The environment selects
authority and tools, and input binding modes match project check.
--format digest prints only sha256:HEX and LF after an executable plan is
persisted successfully. Refusal or persistence failure prints no digest. Supply
that exact digest explicitly to execute --accept; output grants no authority.
The inspect form validates one canonical plan and prints a bounded, deterministic,
redacted human view. Invalid, old, future, or tampered artifacts are not rendered.
";

const EXECUTE_HELP: &str = "Execute one explicitly accepted OPAAL plan

Usage:
  opaal execute --plan PATH --accept sha256:DIGEST [--run-id ID] [--secret-stdin ID] --journal PATH [--receipt-out PATH] [--require-true]
  opaal execute --help

Acceptance belongs only to this request. Execution revalidates every bound
identity before affected work and writes an exclusive hash-chained journal.
The run ID is generated when omitted. --secret-stdin is omitted for a
secret-free plan and must name the exact requirement for a one-secret plan.
Plans declaring stream output require --receipt-out, an exclusive new file in
the project. Data stdout/stderr contain only program output; safe outcome and
transfer counts go to the receipt. --secret-stdin conflicts with stdin.read.
--require-true requires a statically and dynamically Bool task and --receipt-out.
After owned cleanup and evidence persistence, true exits 0 and false exits 1.
Operational/type/evidence failures exit 2; requested cancellation retains 130
even if evidence later fails. Timeout exits 2. Ordinary execute keeps its exit
policy. The selector observes the returned value, never stdout/stderr. Explicitly
caught recoverable Errors may still yield a valid Bool; sticky refusal and
cancellation remain failures. A candidate alone does not prove success.
New runs emit journal/audit v4 and receipt v2; legacy artifacts remain readable.
";

const AUDIT_HELP: &str = "Audit one OPAAL run journal without execution

Usage:
  opaal audit --project opaal.toml --journal PATH --out PATH
  opaal audit inspect PATH
  opaal audit --help

Audit validates the closed schema, sequence, and hash chain and writes one
exclusive complete or incomplete evidence artifact. It never resumes a run.
The inspect form validates one canonical audit and prints only its recorded,
redacted evidence; journal-byte validation belongs to audit generation.
";

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    if let Some(code) = worker_entry(&arguments) {
        return ExitCode::from(code as u8);
    }
    let invocation = match parse_args(arguments.into_iter().skip(1)) {
        Ok(invocation) => invocation,
        Err(error) => return emit_report(HostReport::misuse(&error.message())),
    };
    match invocation.mode {
        Mode::Help => emit_report(HostReport::success(HELP.as_bytes())),
        Mode::Version => {
            let version = format!("opaal {}\n", opaal_runtime::version());
            emit_report(HostReport::success(version.as_bytes()))
        }
        Mode::ProjectHelp => emit_project_report(HostReport::success(PROJECT_HELP.as_bytes())),
        Mode::ProjectInit {
            destination,
            environment,
            docker,
            socket,
            qualified_version,
        } => {
            match init_project(&InitProjectRequest {
                destination,
                environment,
                docker,
                socket,
                qualified_version,
            }) {
                Ok(()) => emit_project_report(HostReport::success(b"project created\n")),
                Err(error) => emit_project_report(HostReport::failure(error.rendered().as_bytes())),
            }
        }
        Mode::ProjectDoctor {
            project,
            task,
            environment,
            inputs,
            format_json,
        } => {
            let request = CheckProjectRequest::with_bindings(project, task, environment, inputs);
            let request = if format_json {
                request.with_json()
            } else {
                request
            };
            let signals = match opaal_platform_posix::standard_host::SignalLease::capture() {
                Ok(signals) => signals,
                Err(_) => {
                    return emit_project_report(HostReport::failure(
                        b"opaal: diagnosis signal capture failed\n",
                    ));
                }
            };
            let control =
                opaal_runtime::module::AnalysisControl::cooperative(signals.cancellation());
            let diagnosis = doctor_project(&request, &control);
            if control.is_cancelled() {
                return ExitCode::from(130);
            }
            match diagnosis {
                Ok(run) => {
                    let status =
                        Status::exit(i64::from(run.exit_code), opaal_runtime::Duration::ZERO)
                            .expect("doctor exit code");
                    emit_project_report(HostReport::completed_with_diagnostic(
                        &status,
                        &run.output,
                        b"",
                    ))
                }
                Err(error) => emit_project_report(HostReport::failure(error.rendered().as_bytes())),
            }
        }
        Mode::TestHelp => emit_project_report(HostReport::success(TEST_HELP.as_bytes())),
        Mode::Test {
            source,
            list,
            case,
            format_json,
        } => {
            let Ok(signals) = opaal_platform_posix::standard_host::SignalLease::capture() else {
                return emit_project_report(HostReport::failure(
                    b"TEST_HOST: signal capture failed\n",
                ));
            };
            let request = opaal_cli::test::TestRequest {
                source,
                list,
                case,
                format_json,
            };
            let cancellation = CancellationToken::from_fn(signals.cancellation());
            let run = opaal_cli::test::run_tests(
                &request,
                &HostCheckFilesystem,
                cancellation.clone(),
                opaal_cli::test::TestLimits::default(),
            );
            let stdout = io::stdout();
            let stderr = io::stderr();
            let mut output = TaskStream::new(
                stdout.as_fd(),
                opaal_platform::standard_host::StandardStream::Stdout,
                stdout.is_terminal(),
                signals.cancellation(),
            );
            let mut diagnostics = TaskStream::new(
                stderr.as_fd(),
                opaal_platform::standard_host::StandardStream::Stderr,
                stderr.is_terminal(),
                signals.cancellation(),
            );
            let exit = match run {
                Ok(run) => {
                    let status =
                        Status::exit(i64::from(run.exit_code), opaal_runtime::Duration::ZERO)
                            .expect("test exit code");
                    write_report(
                        HostReport::completed_with_diagnostic(&status, &run.output, b""),
                        &mut output,
                        &mut diagnostics,
                    )
                }
                Err(_) => write_report(
                    HostReport::failure(b"TEST_RESOURCE: result view limit exceeded\n"),
                    &mut output,
                    &mut diagnostics,
                ),
            };
            if cancellation.is_cancelled() {
                ExitCode::from(
                    if cancellation.reason() == opaal_runtime::eval::CancelReason::Requested {
                        130
                    } else {
                        2
                    },
                )
            } else {
                ExitCode::from(if exit == HostExit::Failure {
                    2
                } else {
                    exit.code()
                })
            }
        }
        Mode::CheckHelp => emit_report(HostReport::success(CHECK_HELP.as_bytes())),
        Mode::Check {
            source,
            format_json,
        } => run_checker(source, format_json),
        Mode::ProjectCheck {
            project,
            task,
            environment,
            inputs,
            format_json,
        } => run_project_checker(project, task, environment, inputs, format_json),
        Mode::TaskHelp => emit_report(HostReport::success(TASK_HELP.as_bytes())),
        Mode::TaskRunHelp => emit_project_report(HostReport::success(TASK_RUN_HELP.as_bytes())),
        Mode::TaskExplainHelp => {
            emit_project_report(HostReport::success(TASK_EXPLAIN_HELP.as_bytes()))
        }
        Mode::TaskRun { request, audit_out } => {
            let Ok(signals) = opaal_platform_posix::standard_host::SignalLease::capture() else {
                return emit_project_report(HostReport::failure(
                    b"TASK004: task cancellation signal capture failed\n",
                ));
            };
            let stdin = io::stdin();
            let terminal = stdin.is_terminal();
            let mut input = TaskStream::new(
                stdin.as_fd(),
                opaal_platform::standard_host::StandardStream::Stdin,
                false,
                signals.cancellation(),
            );
            let stderr = io::stderr();
            let mut summary = TaskStream::new(
                stderr.as_fd(),
                opaal_platform::standard_host::StandardStream::Stderr,
                stderr.is_terminal(),
                signals.cancellation(),
            );
            ExitCode::from(
                run_accepted_task(
                    &request,
                    &audit_out,
                    terminal,
                    &mut input,
                    &mut summary,
                    signals,
                )
                .exit_code,
            )
        }
        Mode::TaskExplain {
            receipt,
            audit,
            format_json,
        } => {
            let Ok(signals) = opaal_platform_posix::standard_host::SignalLease::capture() else {
                return emit_project_report(HostReport::failure(
                    b"TASK004: explanation signal capture failed\n",
                ));
            };
            let cancelled = signals.cancellation();
            let explanation = explain_task(&receipt, audit.as_deref(), format_json);
            if cancelled() {
                return ExitCode::from(130);
            }
            let stdout = io::stdout();
            let stderr = io::stderr();
            let mut output = TaskStream::new(
                stdout.as_fd(),
                opaal_platform::standard_host::StandardStream::Stdout,
                stdout.is_terminal(),
                signals.cancellation(),
            );
            let mut diagnostics = TaskStream::new(
                stderr.as_fd(),
                opaal_platform::standard_host::StandardStream::Stderr,
                stderr.is_terminal(),
                signals.cancellation(),
            );
            let exit = match explanation {
                Ok(bytes) => {
                    write_report(HostReport::success(&bytes), &mut output, &mut diagnostics)
                }
                Err(_) => write_report(
                    HostReport::failure(
                        b"TASK002: task evidence is invalid, unavailable or inconsistent\n",
                    ),
                    &mut output,
                    &mut diagnostics,
                ),
            };
            if cancelled() {
                ExitCode::from(130)
            } else {
                ExitCode::from(if exit == HostExit::Failure {
                    2
                } else {
                    exit.code()
                })
            }
        }
        Mode::TaskInspect {
            project,
            task,
            format_json,
        } => run_task_inspect(project, Some(task), format_json),
        Mode::TaskList {
            project,
            format_json,
        } => run_task_inspect(project, None, format_json),
        Mode::PlanHelp => emit_report(HostReport::success(PLAN_HELP.as_bytes())),
        Mode::PlanInspect { path } => run_plan_inspect(path),
        Mode::Plan { source } => run_planner(source),
        Mode::ProjectPlan {
            project,
            task,
            environment,
            inputs,
            expires_in_seconds,
            out,
            format_digest,
        } => run_project_planner(
            project,
            task,
            environment,
            inputs,
            expires_in_seconds,
            out,
            format_digest,
        ),
        Mode::ExecuteHelp => emit_report(HostReport::success(EXECUTE_HELP.as_bytes())),
        Mode::Execute {
            plan,
            accept,
            run_id,
            secret_stdin,
            journal,
            receipt_out,
            require_true,
        } => run_execute(
            plan,
            accept,
            run_id,
            secret_stdin,
            journal,
            receipt_out,
            require_true,
        ),
        Mode::AuditHelp => emit_report(HostReport::success(AUDIT_HELP.as_bytes())),
        Mode::AuditInspect { path } => run_audit_inspect(path),
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

struct TaskStream<'a> {
    descriptor: std::os::fd::BorrowedFd<'a>,
    stream: opaal_platform::standard_host::StandardStream,
    terminal: bool,
    null_sink: bool,
    cancelled: Box<dyn Fn() -> bool + Send + Sync>,
    started: std::time::Instant,
    host: Option<opaal_platform_posix::standard_host::PosixStandardHost>,
}

impl<'a> TaskStream<'a> {
    fn new(
        descriptor: std::os::fd::BorrowedFd<'a>,
        stream: opaal_platform::standard_host::StandardStream,
        terminal: bool,
        cancelled: Box<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        let null_sink = descriptor
            .try_clone_to_owned()
            .and_then(|fd| std::fs::File::from(fd).metadata())
            .is_ok_and(|status| {
                std::fs::metadata("/dev/null")
                    .is_ok_and(|null| status.dev() == null.dev() && status.ino() == null.ino())
            });
        Self {
            descriptor,
            stream,
            terminal,
            null_sink,
            cancelled,
            started: std::time::Instant::now(),
            host: None,
        }
    }

    fn prepare(&mut self) -> io::Result<()> {
        let failure = |_| io::Error::other("task stream binding failed");
        if self.host.is_none() {
            self.started = std::time::Instant::now();
            let mut host = opaal_platform_posix::standard_host::PosixStandardHost::for_cli(1)
                .map_err(failure)?;
            if self.terminal {
                host.bind_terminal_stream(self.stream, self.descriptor)
                    .map_err(failure)?;
            } else {
                host.bind_stream(self.stream, self.descriptor)
                    .map_err(failure)?;
            }
            self.host = Some(host);
        }
        Ok(())
    }
}

impl io::Read for TaskStream<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        use opaal_platform::standard_host::StandardHost;
        if bytes.is_empty() || self.null_sink {
            return Ok(0);
        }
        self.prepare()?;
        let cancelled = || {
            (self.cancelled)()
                || self.started.elapsed() >= opaal_runtime::operational::standard::OPERATION_TIMEOUT
        };
        let result = self
            .host
            .as_mut()
            .expect("bound input host")
            .read(bytes, &cancelled);
        // Secret-input workers finish before the task continues or persists evidence.
        let cleanup = self.flush();
        match result {
            Ok(count) => cleanup.map(|()| count),
            Err(_) => Err(
                if self.started.elapsed() >= opaal_runtime::operational::standard::OPERATION_TIMEOUT
                {
                    io::Error::new(io::ErrorKind::TimedOut, "task input timed out")
                } else {
                    io::Error::other("task input failed")
                },
            ),
        }
    }
}

impl Write for TaskStream<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        use opaal_platform::standard_host::StandardHost;
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.null_sink {
            return rustix::io::write(self.descriptor, bytes).map_err(Into::into);
        }
        self.prepare()?;
        let cancelled = || {
            (self.cancelled)()
                || self.started.elapsed() >= opaal_runtime::operational::standard::OPERATION_TIMEOUT
        };
        self.host
            .as_mut()
            .expect("bound output host")
            .write(
                self.stream,
                &bytes[..bytes
                    .len()
                    .min(opaal_platform::standard_host::MAX_STREAM_CHUNK_BYTES)],
                &cancelled,
            )
            .map_err(|_| io::Error::other("task view output failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        use opaal_platform::standard_host::StandardHost;
        if let Some(mut host) = self.host.take() {
            host.close()
                .map_err(|_| io::Error::other("task view output cleanup failed"))?;
        }
        Ok(())
    }
}

fn emit_project_report(report: HostReport<'_>) -> ExitCode {
    let mut output = io::stdout().lock();
    let mut diagnostics = io::stderr().lock();
    let exit = write_report(report, &mut output, &mut diagnostics);
    ExitCode::from(if exit == HostExit::Failure {
        2
    } else {
        exit.code()
    })
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

fn run_checker(source: PathBuf, format_json: bool) -> ExitCode {
    if let Some(exit) = reject_non_opaal(&source, "source") {
        return exit;
    }
    let request = CheckRequest::new(source);
    let run = check_source(&request, &HostCheckFilesystem);
    if format_json {
        let json = run.render_json();
        emit_report(if run.is_success() {
            HostReport::success(&json)
        } else {
            HostReport::failure_with_output(&json, b"")
        })
    } else if run.is_success() {
        emit_report(HostReport::success(b""))
    } else {
        emit_report(HostReport::failure(
            run.rendered_issues().concat().as_bytes(),
        ))
    }
}

fn run_task_inspect(project: PathBuf, task: Option<String>, format_json: bool) -> ExitCode {
    let request = match task {
        Some(task) => InspectProjectRequest::new(project, task),
        None => InspectProjectRequest::list(project),
    };
    let request = if format_json {
        request.with_json()
    } else {
        request
    };
    match inspect_project(&request) {
        Ok(run) => emit_report(HostReport::success(run.output())),
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_project_checker(
    project: PathBuf,
    task: String,
    environment: String,
    inputs: Vec<ProjectInputBinding>,
    format_json: bool,
) -> ExitCode {
    let request = CheckProjectRequest::with_bindings(project, task, environment, inputs);
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
    inputs: Vec<ProjectInputBinding>,
    expires_in_seconds: u64,
    out: PathBuf,
    format_digest: bool,
) -> ExitCode {
    let request = PlanProjectRequest::with_bindings(
        project,
        task,
        environment,
        inputs,
        expires_in_seconds,
        out,
    );
    let request = if format_digest {
        request.with_digest()
    } else {
        request
    };
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

fn run_plan_inspect(path: PathBuf) -> ExitCode {
    match inspect_plan_artifact(&path) {
        Ok(output) => emit_report(HostReport::success(&output)),
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

fn run_audit_inspect(path: PathBuf) -> ExitCode {
    match inspect_audit_artifact(&path) {
        Ok(output) => emit_report(HostReport::success(&output)),
        Err(error) => emit_report(HostReport::failure(error.rendered().as_bytes())),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_execute(
    plan: PathBuf,
    accept: String,
    run_id: Option<String>,
    secret_stdin: Option<String>,
    journal: PathBuf,
    receipt_out: Option<PathBuf>,
    require_true: bool,
) -> ExitCode {
    let request = ExecuteProjectRequest::new(plan, accept, run_id, secret_stdin, journal)
        .with_receipt_out(receipt_out)
        .with_require_true(require_true);
    let stdin = io::stdin();
    let is_terminal = stdin.is_terminal();
    let mut input = stdin;
    match execute_explicit_plan(&request, is_terminal, &mut input) {
        Ok(run) => {
            let status = Status::exit(i64::from(run.exit_code()), opaal_runtime::Duration::ZERO)
                .expect("execution exit fits a status");
            emit_report(HostReport::completed(&status, run.output()))
        }
        Err(error) => {
            let status = Status::exit(
                if require_true { 2 } else { 1 },
                opaal_runtime::Duration::ZERO,
            )
            .expect("failure exit fits a status");
            emit_report(HostReport::completed_with_diagnostic(
                &status,
                b"",
                error.rendered().as_bytes(),
            ))
        }
    }
}

fn run_script(path: &Path, arguments: &[String]) -> ExitCode {
    if let Some(exit) = reject_non_opaal(path, "script") {
        return exit;
    }
    let filesystem = HostCheckFilesystem;
    let registry = opaal_runtime::builtin::standard_registry();
    let program = match ModuleProgramLoader::new(&filesystem, &filesystem)
        .load_for_frontend_with_commands(path, &registry)
    {
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
    let snapshot = match NativeSessionSnapshot::capture() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return emit_report(HostReport::failure(format!("opaal: {error}\n").as_bytes()));
        }
    };
    let clock = Arc::new(SystemClock::new());
    let mut standard = match standard_binding(clock.clone(), true) {
        Ok(binding) => binding,
        Err(error) => {
            return emit_report(HostReport::failure(format!("opaal: {error}\n").as_bytes()));
        }
    };
    let mut output = io::stdout();
    let outcome = execute_ambient_module_program_outcome_with_standard_host(
        &program,
        arguments,
        snapshot,
        &registry,
        &PosixPlatform,
        &SessionOptions::default(),
        &PosixPlatform,
        clock,
        &mut output,
        &mut standard,
    );
    let flush = output.flush();
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
    let snapshot = match NativeSessionSnapshot::capture() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return emit_report(HostReport::failure(format!("opaal: {error}\n").as_bytes()));
        }
    };
    let session = Session::from_ambient_snapshot(snapshot, SessionOptions::default());
    let mut evaluator = OpaalEvaluator {
        session,
        clock: SystemClock::new(),
        lend_stdin: io::stdin().is_terminal() && io::stdout().is_terminal(),
    };
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
    clock: SystemClock,
    lend_stdin: bool,
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
        let mut standard = standard_binding(Arc::new(self.clock.clone()), self.lend_stdin)
            .map_err(|error| InteractiveDiagnostic::new(format!("opaal: {error}\n")))?;
        let outcome = self
            .session
            .submit_with_source_loader_and_standard_host(
                source_name(),
                source,
                &HostCheckFilesystem,
                &HostCheckFilesystem,
                &mut standard,
                &PosixPlatform,
                &PosixPlatform,
                &self.clock,
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
        let result = match outcome {
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
        };
        let cleanup = standard.take_cleanup_errors();
        if cleanup.is_empty() {
            return result;
        }
        let mut rendered = match result {
            Err(InteractiveEvaluationError::Diagnostic(primary)) => primary.rendered().to_owned(),
            Err(InteractiveEvaluationError::ProgramOutput(error)) => {
                format!("opaal: output failure: {error}\n")
            }
            Err(InteractiveEvaluationError::HostFailure(rendered)) => rendered,
            Ok(_) => String::new(),
        };
        for error in cleanup {
            rendered.push_str(&format!("opaal: cleanup failure: {error}\n"));
        }
        Err(InteractiveEvaluationError::HostFailure(rendered))
    }
}

fn standard_binding(
    clock: Arc<dyn Clock>,
    lend_stdin: bool,
) -> Result<StandardBinding, opaal_runtime::operational::ModuleError> {
    static NEXT_EVALUATION: AtomicU64 = AtomicU64::new(1);
    let evaluation = NEXT_EVALUATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .ok()
        .and_then(EvaluationContextId::new)
        .ok_or_else(|| {
            opaal_runtime::operational::ModuleError::invalid(
                "OPERATION001",
                "evaluation identity exhausted",
            )
        })?;
    let authority = AuthorityContext::new(
        evaluation,
        [
            Some(CapabilityRequest::entropy_system()),
            lend_stdin.then(CapabilityRequest::stdin_read),
            Some(CapabilityRequest::stdout_write()),
            Some(CapabilityRequest::stderr_write()),
        ]
        .into_iter()
        .flatten()
        .map(|request| AuthorityRule::grant(request, RequiredEnforcement::Enforced)),
    )
    .expect("distinct explicit standard host grants");
    // Construction and binding start no worker; an unavailable role refuses
    // only when reached, including calls that transfer no bytes.
    let mut cancellation = CancellationToken::never();
    let host = PosixStandardHost::for_cli(evaluation.get())
        .ok()
        .and_then(|mut host| {
            cancellation = CancellationToken::from_fn(host.capture_cancellation_signals().ok()?);
            use opaal_platform::standard_host::StandardStream;
            let stdin = io::stdin();
            let stdout = io::stdout();
            let stderr = io::stderr();
            for (role, descriptor, terminal) in [
                (StandardStream::Stdin, stdin.as_fd(), stdin.is_terminal()),
                (StandardStream::Stdout, stdout.as_fd(), stdout.is_terminal()),
                (StandardStream::Stderr, stderr.as_fd(), stderr.is_terminal()),
            ] {
                if role == StandardStream::Stdin && !lend_stdin {
                    continue;
                }
                let _ = if terminal {
                    host.bind_terminal_stream(role, descriptor)
                } else {
                    host.bind_stream(role, descriptor)
                };
            }
            Some(Box::new(host) as Box<dyn opaal_platform::standard_host::StandardHost>)
        });
    let state = StandardState::new(host, StandardLimits::default())?;
    Ok(StandardBinding::new(
        authority,
        cancellation,
        clock,
        None,
        state,
    ))
}

const fn source_name() -> &'static str {
    "<interactive>"
}

fn finish_script_outcome_report(
    outcome: ScriptExecutionOutcome,
    output_flush: io::Result<()>,
) -> ExitCode {
    let (primary, evidence, _downstream) = outcome.into_parts();
    let cleanup_failed = evidence
        .iter()
        .any(|item| matches!(item, OutcomeEvidence::CleanupFailure(_)));
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
            if cleanup_failed {
                return emit_report(HostReport::failure(diagnostics.as_bytes()));
            }
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
