//! Standard platform-independent internal commands and session state.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use flash_platform::{Platform, WorkingDirectoryRequest};

use crate::command::{
    Carrier, CommandClassification, CommandLifecycle, CommandNamespaceEntry, CommandRegistry,
    CommandSignature, V1_LANGUAGE_MAJOR,
};
use crate::documentation::{CommandDocumentation, Documentation};
use crate::eval::{RuntimeError, RuntimeErrorKind};
use crate::plan::{PlannedArgument, PlannedResolution, PlannedStage};
use crate::resolve::{ExecutableProbe, Resolution, ResolutionError, resolve_command};
use crate::{Duration, Environment, NativePath, Record, Status, Value};

/// Mutable shell-session state shared by built-ins and later execution layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    cwd: PathBuf,
    environment: Environment,
    current_status: Option<Status>,
}

impl SessionState {
    /// Build a session from its logical cwd and child environment.
    pub fn new(cwd: impl Into<PathBuf>, environment: Environment) -> Self {
        Self {
            cwd: cwd.into(),
            environment,
            current_status: None,
        }
    }

    /// The logical working directory inherited by planned children.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The child-process environment.
    #[must_use]
    pub const fn environment(&self) -> &Environment {
        &self.environment
    }

    /// Mutable access used by the language's `export` and `unset` statements.
    pub const fn environment_mut(&mut self) -> &mut Environment {
        &mut self.environment
    }

    /// The most recent normally completed status, if one exists.
    #[must_use]
    pub const fn current_status(&self) -> Option<&Status> {
        self.current_status.as_ref()
    }

    /// Replace the session's current completed status.
    pub fn set_current_status(&mut self, status: Option<Status>) {
        self.current_status = status;
    }
}

/// Data produced by a completed standard built-in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuiltinOutput {
    /// No pipeline data.
    Empty,
    /// One structured value.
    Value(Value),
    /// An ordered finite value stream.
    ValueStream(Vec<Value>),
    /// The caller must forward the existing input carrier unchanged.
    ForwardInput(Carrier),
}

/// One normally completed internal command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinCompletion {
    output: BuiltinOutput,
    status: Status,
}

impl BuiltinCompletion {
    /// The internal command's structured output contract.
    #[must_use]
    pub const fn output(&self) -> &BuiltinOutput {
        &self.output
    }

    /// The internal command's normal leaf status.
    #[must_use]
    pub const fn status(&self) -> &Status {
        &self.status
    }
}

/// A request for the session boundary to terminate with one host exit code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExitRequest {
    code: u8,
}

impl ExitRequest {
    /// The requested process exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self.code
    }
}

/// The possible successful control outcomes of a standard built-in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuiltinOutcome {
    /// The command completed inside the runtime.
    Completed(BuiltinCompletion),
    /// `exit` requested session termination.
    Exit(ExitRequest),
}

/// Construct the standard internal-command registry.
#[must_use]
pub fn standard_registry() -> CommandRegistry {
    let entries = [
        documented(
            CommandSignature::new("cd", [Carrier::Empty], Carrier::Empty),
            "cd [PATH]",
            "Change the logical working directory.",
        ),
        documented(
            CommandSignature::new("pwd", [Carrier::Empty], Carrier::Value),
            "pwd",
            "Return the logical working directory.",
        ),
        documented(
            CommandSignature::new("which", [Carrier::Empty], Carrier::ValueStream),
            "which NAME...",
            "Resolve command names without executing them.",
        ),
        documented(
            CommandSignature::new(
                "command",
                [Carrier::Empty, Carrier::ByteStream],
                Carrier::ByteStream,
            ),
            "command NAME [ARG...]",
            "Run a command through explicit external resolution.",
        ),
        documented(
            CommandSignature::new("exit", [Carrier::Empty], Carrier::Empty),
            "exit [CODE]",
            "End the current session.",
        ),
        documented(
            CommandSignature::passthrough(
                "check",
                [
                    Carrier::Empty,
                    Carrier::ByteStream,
                    Carrier::Value,
                    Carrier::ValueStream,
                ],
            ),
            "check",
            "Raise a catchable error unless the upstream stage succeeded.",
        ),
        // The explicit byte/structured boundaries (see the value-model spec).
        // `decode`/`from` parse a byte stream into structured values; `encode`/`to`
        // serialize structured values back into a byte stream. Registering their
        // carrier contracts makes the pipeline-validation bridge hints name real
        // commands. `decode`/`encode` implement the codec crossing in `convert`;
        // `from`/`to` use the format conversions in `format`.
        documented(
            CommandSignature::new("decode", [Carrier::ByteStream], Carrier::ValueStream),
            "decode CODEC",
            "Decode bytes into structured values.",
        ),
        documented(
            CommandSignature::new("from", [Carrier::ByteStream], Carrier::ValueStream),
            "from FORMAT",
            "Parse formatted bytes into structured values.",
        ),
        documented(
            CommandSignature::new(
                "encode",
                [Carrier::Value, Carrier::ValueStream],
                Carrier::ByteStream,
            ),
            "encode CODEC",
            "Encode structured values as bytes.",
        ),
        documented(
            CommandSignature::new(
                "to",
                [Carrier::Value, Carrier::ValueStream],
                Carrier::ByteStream,
            ),
            "to FORMAT",
            "Serialize structured values in a named format.",
        ),
        // The closure-free structured commands (see `structured`). Each consumes a
        // value stream: `first`/`last` reshape it to a bounded value stream,
        // `collect` materializes one `List` value, `length` counts, and `lines`
        // splits the text carried by its values into one value per line.
        documented(
            CommandSignature::new("first", [Carrier::ValueStream], Carrier::ValueStream),
            "first [COUNT]",
            "Keep the first values from a stream.",
        ),
        documented(
            CommandSignature::new("last", [Carrier::ValueStream], Carrier::ValueStream),
            "last [COUNT]",
            "Keep the last values from a stream.",
        ),
        documented(
            CommandSignature::new("collect", [Carrier::ValueStream], Carrier::Value),
            "collect",
            "Collect a value stream into one list.",
        ),
        documented(
            CommandSignature::new("length", [Carrier::ValueStream], Carrier::Value),
            "length",
            "Count values in a stream.",
        ),
        documented(
            CommandSignature::new("lines", [Carrier::ValueStream], Carrier::ValueStream),
            "lines",
            "Split textual values into lines.",
        ),
        // The closure-driven structured commands (see `closure`). `each` maps and
        // `where` filters the value stream by applying a closure per item.
        documented(
            CommandSignature::new("each", [Carrier::ValueStream], Carrier::ValueStream),
            "each { |VALUE| EXPRESSION }",
            "Transform each value with a closure.",
        ),
        documented(
            CommandSignature::new("where", [Carrier::ValueStream], Carrier::ValueStream),
            "where { |VALUE| PREDICATE }",
            "Keep values accepted by a predicate closure.",
        ),
        // The read-only record projections (see `structured`). `select` narrows
        // each record to named columns; `get` extracts one field per record.
        documented(
            CommandSignature::new("select", [Carrier::ValueStream], Carrier::ValueStream),
            "select FIELD...",
            "Project named fields from records.",
        ),
        documented(
            CommandSignature::new("get", [Carrier::ValueStream], Carrier::ValueStream),
            "get FIELD",
            "Extract one named field from each record.",
        ),
        // `update` replaces one field per record, applying a closure replacement
        // to the current value (see `closure`).
        documented(
            CommandSignature::new("update", [Carrier::ValueStream], Carrier::ValueStream),
            "update FIELD VALUE",
            "Replace a field in each record.",
        ),
        // `sort` materializes the stream and orders it, or records by a key (see
        // `structured`).
        documented(
            CommandSignature::new("sort", [Carrier::ValueStream], Carrier::ValueStream),
            "sort [FIELD]",
            "Sort values or records by a field.",
        ),
        // `ls` is a structured producer: it takes no pipeline input and yields
        // one record per directory entry (see `directory`).
        documented(
            CommandSignature::new("ls", [Carrier::Empty], Carrier::ValueStream),
            "ls [PATH]",
            "List directory entries as records.",
        ),
        // File bytes remain bytes. `open` produces them lazily and `save`
        // consumes them; parsing and serialization stay explicit `from`/`to`
        // stages (see `file`).
        documented(
            CommandSignature::new("open", [Carrier::Empty], Carrier::ByteStream),
            "open PATH",
            "Read a file as bytes.",
        ),
        documented(
            CommandSignature::new("save", [Carrier::ByteStream], Carrier::Empty),
            "save PATH",
            "Write pipeline bytes to a file.",
        ),
        // Job-control commands need the session-owned coordinator rather than
        // the clonable `SessionState` used by ordinary built-ins. Their
        // signatures still belong in the standard registry so planning,
        // preflight, and editor completion see the real carrier contract.
        documented(
            CommandSignature::new("jobs", [Carrier::Empty], Carrier::ValueStream),
            "jobs",
            "List addressable background jobs.",
        ),
        documented(
            CommandSignature::new("fg", [Carrier::Empty], Carrier::Empty),
            "fg %JOB",
            "Resume a job in the foreground.",
        ),
        documented(
            CommandSignature::new("bg", [Carrier::Empty], Carrier::Empty),
            "bg %JOB",
            "Resume a stopped job in the background.",
        ),
        documented(
            CommandSignature::new("wait", [Carrier::Empty], Carrier::Empty),
            "wait [%JOB]",
            "Wait for one job or all jobs.",
        ),
        documented(
            CommandSignature::new("kill", [Carrier::Empty], Carrier::Empty).with_flags([
                "--hangup",
                "--interrupt",
                "--terminate",
                "--kill",
                "--stop",
                "--continue",
            ]),
            "kill [SIGNAL] %JOB",
            "Send a signal to an addressable job.",
        ),
        documented(
            CommandSignature::new("help", [Carrier::Empty], Carrier::ByteStream),
            "help [NAME]",
            "Inspect built-in and visible function metadata without execution.",
        ),
    ];
    CommandRegistry::try_from_entries(V1_LANGUAGE_MAJOR, entries)
        .expect("the standard command namespace manifest must be valid")
}

fn documented(
    signature: CommandSignature,
    invocation: &'static str,
    summary: &'static str,
) -> CommandNamespaceEntry {
    CommandNamespaceEntry::core(
        signature.with_documentation(CommandDocumentation::new(
            invocation,
            Documentation::new(summary),
        )),
        CommandLifecycle::introduced(V1_LANGUAGE_MAJOR),
    )
}

/// Execute one planned standard internal command without spawning a process.
#[allow(clippy::too_many_arguments)]
pub fn execute_builtin(
    stage: &PlannedStage,
    input: Carrier,
    upstream_status: Option<&Status>,
    session: &mut SessionState,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
) -> Result<BuiltinOutcome, RuntimeError> {
    let PlannedResolution::Internal { canonical_name, .. } = stage.resolution() else {
        return Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "executing an external stage as a built-in",
            },
            stage.span(),
        ));
    };
    let signature = registry.lookup(canonical_name).ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "an unregistered internal command",
            },
            stage.span(),
        )
    })?;
    let command = standard_name(canonical_name).ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "a non-standard internal command",
            },
            stage.span(),
        )
    })?;
    if !signature.accepts(input) {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinInputCarrier { command, input },
            stage.span(),
        ));
    }

    match command {
        "cd" => execute_cd(stage, session, platform),
        "pwd" => execute_pwd(stage, session),
        "which" => execute_which(stage, session, registry, probe),
        "command" => unreachable!("command stages are lowered to external stages while planning"),
        "exit" => execute_exit(stage, session),
        "check" => execute_check(stage, input, upstream_status, session),
        _ => unreachable!("standard_name returns only standard built-ins"),
    }
}

fn execute_cd(
    stage: &PlannedStage,
    session: &mut SessionState,
    platform: &dyn Platform,
) -> Result<BuiltinOutcome, RuntimeError> {
    expect_arity(stage, "cd", 0, Some(1))?;
    let (target, span) = match stage.arguments().first() {
        Some(argument) => {
            let argument = word_argument("cd", argument)?;
            (argument.value().to_os_string(), argument.span())
        }
        None => (
            session
                .environment
                .get("HOME")
                .ok_or_else(|| RuntimeError::new(RuntimeErrorKind::MissingHome, stage.span()))?
                .to_os_string(),
            stage.span(),
        ),
    };
    let resolved = platform
        .resolve_working_directory(WorkingDirectoryRequest::new(
            Path::new(&target),
            &session.cwd,
        ))
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::WorkingDirectory(error), span))?;

    let previous = session.cwd.as_os_str().to_os_string();
    session.environment.set("OLDPWD", previous);
    session
        .environment
        .set("PWD", resolved.as_os_str().to_os_string());
    session.cwd = resolved;
    Ok(completed(session, BuiltinOutput::Empty, 0))
}

fn execute_pwd(
    stage: &PlannedStage,
    session: &mut SessionState,
) -> Result<BuiltinOutcome, RuntimeError> {
    expect_arity(stage, "pwd", 0, Some(0))?;
    Ok(completed(
        session,
        BuiltinOutput::Value(Value::Path(NativePath::new(
            session.cwd.as_os_str().to_os_string(),
        ))),
        0,
    ))
}

fn execute_which(
    stage: &PlannedStage,
    session: &mut SessionState,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
) -> Result<BuiltinOutcome, RuntimeError> {
    expect_arity(stage, "which", 1, None)?;
    let mut missing = false;
    let mut output = Vec::with_capacity(stage.arguments().len());
    for argument in stage.arguments() {
        let argument = word_argument("which", argument)?;
        let name = argument.value();
        let classification = name.to_str().map(|name| registry.classify(name));
        let (kind, target, path) = match classification {
            Some(CommandClassification::Core { .. }) => ("internal", Value::Null, Value::Null),
            Some(CommandClassification::Alias { canonical_name, .. }) => {
                ("alias", Value::string(canonical_name), Value::Null)
            }
            Some(CommandClassification::Reserved { replacement, .. }) => {
                missing = true;
                (
                    "reserved",
                    replacement.map_or(Value::Null, Value::string),
                    Value::Null,
                )
            }
            Some(CommandClassification::Unknown) | None => {
                match resolve_command(name, false, registry, &session.environment, probe) {
                    Ok(Resolution::External(command)) => (
                        "external",
                        Value::Null,
                        Value::Path(NativePath::new(command.path().as_os_str().to_os_string())),
                    ),
                    Err(ResolutionError::NotFound { .. }) => {
                        missing = true;
                        ("missing", Value::Null, Value::Null)
                    }
                    Ok(Resolution::Internal { .. }) | Err(ResolutionError::Reserved { .. }) => {
                        unreachable!(
                            "unknown and native names cannot resolve through the namespace"
                        )
                    }
                }
            }
        };
        output.push(Value::Record(
            Record::new(vec![
                (
                    "name".to_owned(),
                    Value::Path(NativePath::new(name.to_os_string())),
                ),
                ("kind".to_owned(), Value::string(kind)),
                ("target".to_owned(), target),
                ("path".to_owned(), path),
            ])
            .expect("which record keys are unique"),
        ));
    }
    Ok(completed(
        session,
        BuiltinOutput::ValueStream(output),
        i64::from(missing),
    ))
}

fn execute_exit(
    stage: &PlannedStage,
    session: &SessionState,
) -> Result<BuiltinOutcome, RuntimeError> {
    expect_arity(stage, "exit", 0, Some(1))?;
    let code = match stage.arguments().first() {
        Some(argument) => {
            let argument = word_argument("exit", argument)?;
            parse_exit_code(argument.value()).ok_or_else(|| {
                RuntimeError::new(RuntimeErrorKind::InvalidExitCode, argument.span())
            })?
        }
        None => default_exit_code(session.current_status()),
    };
    Ok(BuiltinOutcome::Exit(ExitRequest { code }))
}

fn execute_check(
    stage: &PlannedStage,
    input: Carrier,
    upstream_status: Option<&Status>,
    session: &mut SessionState,
) -> Result<BuiltinOutcome, RuntimeError> {
    expect_arity(stage, "check", 0, Some(0))?;
    let upstream = upstream_status
        .ok_or_else(|| RuntimeError::new(RuntimeErrorKind::CheckRequiresUpstream, stage.span()))?;
    if !upstream.is_ok() {
        return Err(RuntimeError::new(
            RuntimeErrorKind::UnsuccessfulStatus {
                status: Box::new(upstream.clone()),
            },
            stage.span(),
        ));
    }
    Ok(completed(session, BuiltinOutput::ForwardInput(input), 0))
}

fn expect_arity(
    stage: &PlannedStage,
    command: &'static str,
    minimum: usize,
    maximum: Option<usize>,
) -> Result<(), RuntimeError> {
    let actual = stage.arguments().len();
    if actual < minimum || maximum.is_some_and(|maximum| actual > maximum) {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinArity {
                command,
                minimum,
                maximum,
                actual,
            },
            stage.span(),
        ));
    }
    Ok(())
}

fn word_argument<'a>(
    command: &'static str,
    argument: &'a PlannedArgument,
) -> Result<&'a crate::eval::ExpandedWord, RuntimeError> {
    argument.as_word().ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorKind::BuiltinArgument {
                command,
                message: "expected a word argument, found a typed value".to_owned(),
            },
            argument.span(),
        )
    })
}

fn completed(session: &mut SessionState, output: BuiltinOutput, code: i64) -> BuiltinOutcome {
    let status = Status::exit(code, Duration::ZERO).expect("built-in duration is valid");
    session.current_status = Some(status.clone());
    BuiltinOutcome::Completed(BuiltinCompletion { output, status })
}

fn parse_exit_code(value: &OsStr) -> Option<u8> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn default_exit_code(status: Option<&Status>) -> u8 {
    match status.and_then(Status::code) {
        Some(code) => u8::try_from(code).unwrap_or(1),
        None if status.is_none() => 0,
        None => 1,
    }
}

fn standard_name(name: &str) -> Option<&'static str> {
    match name {
        "cd" => Some("cd"),
        "pwd" => Some("pwd"),
        "which" => Some("which"),
        "command" => Some("command"),
        "exit" => Some("exit"),
        "check" => Some("check"),
        _ => None,
    }
}
