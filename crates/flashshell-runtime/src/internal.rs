//! Live execution of all-internal structured pipelines.
//!
//! The external executor owns process descriptors and byte pipelines. This
//! module owns the complementary carrier path: it moves `Empty`, `Value`, and
//! lazy `ValueStream` payloads directly between internal stages without display,
//! serialization, or edge materialization.

use std::ffi::OsStr;
use std::path::Path;

use flashshell_platform::{DirectoryReadRequest, Platform};

use crate::builtin::{BuiltinOutcome, BuiltinOutput, SessionState, execute_builtin};
use crate::command::{Carrier, CommandRegistry};
use crate::directory::{ListStep, list};
use crate::eval::{RuntimeError, RuntimeErrorKind};
use crate::plan::{ExecutionPlan, PlannedArgument, PlannedResolution, PlannedStage, preflight};
use crate::resolve::ExecutableProbe;
use crate::stream::{StreamPull, ValueStream};
use crate::structured::{
    DrainOutcome, GetStep, LineStep, SelectStep, SortOutcome, collect, first, get, last, length,
    lines, select, sort,
};
use crate::{Duration, Status, Value};

/// The documented item ceiling for commands that must drain a complete value
/// stream (`last`, `collect`, `length`, and `sort`).
pub const DEFAULT_MATERIALIZATION_LIMIT: usize = 1_000_000;

/// One owned payload moving across an internal pipeline edge.
pub enum InternalPayload {
    /// No payload.
    Empty,
    /// Exactly one structured value.
    Value(Value),
    /// A lazy ordered sequence of structured values.
    ValueStream(ValueStream),
}

impl InternalPayload {
    /// The carrier represented by this payload.
    #[must_use]
    pub const fn carrier(&self) -> Carrier {
        match self {
            Self::Empty => Carrier::Empty,
            Self::Value(_) => Carrier::Value,
            Self::ValueStream(_) => Carrier::ValueStream,
        }
    }
}

/// The control result of one all-internal pipeline.
pub enum InternalPipelineOutcome {
    /// Every stage completed normally.
    Completed {
        /// The final unpresented payload.
        payload: InternalPayload,
        /// The leaf or aggregate pipeline status.
        status: Status,
    },
    /// The `exit` built-in requested session termination.
    Exit(u8),
}

/// Execute a preplanned pipeline containing only internal stages.
#[allow(clippy::too_many_arguments)]
pub fn execute_internal_pipeline(
    plan: &ExecutionPlan,
    state: &mut SessionState,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
) -> Result<InternalPipelineOutcome, RuntimeError> {
    preflight(plan)?;
    let mut payload = InternalPayload::Empty;
    let mut statuses = Vec::with_capacity(plan.stages().len());

    for stage in plan.stages() {
        let PlannedResolution::Internal { name } = stage.resolution() else {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Unsupported {
                    feature: "an external stage in the internal pipeline executor",
                },
                stage.span(),
            ));
        };
        let upstream = statuses.last();
        match execute_stage(
            name,
            stage,
            payload,
            upstream,
            state,
            registry,
            probe,
            platform,
            plan.cwd(),
        )? {
            StageOutcome::Completed {
                payload: output,
                status,
            } => {
                payload = output;
                statuses.push(status);
            }
            StageOutcome::Exit(code) => return Ok(InternalPipelineOutcome::Exit(code)),
        }
    }

    let status = aggregate_status(statuses, plan.pipefail());
    state.set_current_status(Some(status.clone()));
    Ok(InternalPipelineOutcome::Completed { payload, status })
}

enum StageOutcome {
    Completed {
        payload: InternalPayload,
        status: Status,
    },
    Exit(u8),
}

#[allow(clippy::too_many_arguments)]
fn execute_stage(
    name: &str,
    stage: &PlannedStage,
    input: InternalPayload,
    upstream: Option<&Status>,
    state: &mut SessionState,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
    cwd: &Path,
) -> Result<StageOutcome, RuntimeError> {
    match name {
        "cd" | "pwd" | "which" | "command" | "exit" | "check" => {
            execute_session_builtin(stage, input, upstream, state, registry, probe, platform)
        }
        "first" => execute_first(stage, input),
        "last" => execute_last(stage, input),
        "collect" => execute_collect(stage, input),
        "length" => execute_length(stage, input),
        "lines" => execute_lines(stage, input),
        "select" => execute_select(stage, input),
        "get" => execute_get(stage, input),
        "sort" => execute_sort(stage, input),
        "ls" => execute_ls(stage, input, platform, cwd),
        _ => Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "this internal command in the live structured executor",
            },
            stage.span(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_session_builtin(
    stage: &PlannedStage,
    input: InternalPayload,
    upstream: Option<&Status>,
    state: &mut SessionState,
    registry: &CommandRegistry,
    probe: &dyn ExecutableProbe,
    platform: &dyn Platform,
) -> Result<StageOutcome, RuntimeError> {
    let carrier = input.carrier();
    match execute_builtin(stage, carrier, upstream, state, registry, probe, platform)? {
        BuiltinOutcome::Completed(completion) => {
            let payload = match completion.output() {
                BuiltinOutput::Empty => InternalPayload::Empty,
                BuiltinOutput::Value(value) => InternalPayload::Value(value.clone()),
                BuiltinOutput::ValueStream(values) => {
                    InternalPayload::ValueStream(ValueStream::from_values(values.clone()))
                }
                BuiltinOutput::ForwardInput(_) => input,
            };
            Ok(StageOutcome::Completed {
                payload,
                status: completion.status().clone(),
            })
        }
        BuiltinOutcome::Exit(request) => Ok(StageOutcome::Exit(request.code())),
        BuiltinOutcome::External(_) => Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "the command built-in inside an all-internal pipeline",
            },
            stage.span(),
        )),
    }
}

fn execute_first(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "first", 0, Some(1))?;
    let count = optional_count(stage, "first", 1)?;
    let values = drain("first", stage, first(expect_stream(stage, input)?, count))?;
    completed(InternalPayload::ValueStream(ValueStream::from_values(
        values,
    )))
}

fn execute_last(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "last", 0, Some(1))?;
    let count = optional_count(stage, "last", 1)?;
    let values = drain(
        "last",
        stage,
        last(
            expect_stream(stage, input)?,
            count,
            DEFAULT_MATERIALIZATION_LIMIT,
        ),
    )?;
    completed(InternalPayload::ValueStream(ValueStream::from_values(
        values,
    )))
}

fn execute_collect(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "collect", 0, Some(0))?;
    let value = drain(
        "collect",
        stage,
        collect(expect_stream(stage, input)?, DEFAULT_MATERIALIZATION_LIMIT),
    )?;
    completed(InternalPayload::Value(value))
}

fn execute_length(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "length", 0, Some(0))?;
    let value = drain(
        "length",
        stage,
        length(expect_stream(stage, input)?, DEFAULT_MATERIALIZATION_LIMIT),
    )?;
    completed(InternalPayload::Value(value))
}

fn execute_lines(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "lines", 0, Some(0))?;
    let mut splitter = lines(expect_stream(stage, input)?);
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || match splitter.pull() {
        LineStep::Line(value) => StreamPull::Item(value),
        LineStep::End => StreamPull::End,
        LineStep::NotText { actual } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "lines",
                message: format!("expected String input, found {actual}"),
            },
            span,
        )),
        LineStep::Failed(error) => StreamPull::Failed(error),
        LineStep::Cancelled(reason) => StreamPull::Cancelled(reason),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_select(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "select", 1, None)?;
    let columns = utf8_words(stage, "select")?;
    let mut projection = select(expect_stream(stage, input)?, columns);
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || match projection.pull() {
        SelectStep::Record(value) => StreamPull::Item(value),
        SelectStep::End => StreamPull::End,
        SelectStep::MissingColumn { column } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "select",
                message: format!("record has no column `{column}`"),
            },
            span,
        )),
        SelectStep::NotRecord { actual } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "select",
                message: format!("expected Record input, found {actual}"),
            },
            span,
        )),
        SelectStep::Failed(error) => StreamPull::Failed(error),
        SelectStep::Cancelled(reason) => StreamPull::Cancelled(reason),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_get(stage: &PlannedStage, input: InternalPayload) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "get", 1, Some(1))?;
    let key = utf8_words(stage, "get")?
        .into_iter()
        .next()
        .expect("arity guarantees one key");
    let mut projection = get(expect_stream(stage, input)?, key);
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || match projection.pull() {
        GetStep::Value(value) => StreamPull::Item(value),
        GetStep::End => StreamPull::End,
        GetStep::MissingKey { key } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "get",
                message: format!("record has no field `{key}`"),
            },
            span,
        )),
        GetStep::NotRecord { actual } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "get",
                message: format!("expected Record input, found {actual}"),
            },
            span,
        )),
        GetStep::Failed(error) => StreamPull::Failed(error),
        GetStep::Cancelled(reason) => StreamPull::Cancelled(reason),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_sort(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "sort", 0, Some(1))?;
    let key = utf8_words(stage, "sort")?.into_iter().next();
    let values = match sort(
        expect_stream(stage, input)?,
        key,
        DEFAULT_MATERIALIZATION_LIMIT,
    ) {
        SortOutcome::Sorted(values) => values,
        SortOutcome::LimitExceeded { limit } => {
            return Err(structured_error(
                "sort",
                stage,
                format!("input exceeds the {limit}-item materialization limit"),
            ));
        }
        SortOutcome::Incomparable { left, right } => {
            return Err(structured_error(
                "sort",
                stage,
                format!("cannot order {left} and {right} values"),
            ));
        }
        SortOutcome::MissingKey { key } => {
            return Err(structured_error(
                "sort",
                stage,
                format!("record has no field `{key}`"),
            ));
        }
        SortOutcome::NotRecord { actual } => {
            return Err(structured_error(
                "sort",
                stage,
                format!("expected Record input for keyed sort, found {actual}"),
            ));
        }
        SortOutcome::Failed(error) => return Err(error),
        SortOutcome::Cancelled(reason) => {
            return Err(RuntimeError::new(
                RuntimeErrorKind::StreamCancelled { reason },
                stage.span(),
            ));
        }
    };
    completed(InternalPayload::ValueStream(ValueStream::from_values(
        values,
    )))
}

fn execute_ls(
    stage: &PlannedStage,
    input: InternalPayload,
    platform: &dyn Platform,
    cwd: &Path,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "ls", 0, Some(1))?;
    if input.carrier() != Carrier::Empty {
        return Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinInputCarrier {
                command: "ls",
                input: input.carrier(),
            },
            stage.span(),
        ));
    }
    let path = stage
        .arguments()
        .first()
        .map(|argument| word("ls", argument))
        .transpose()?
        .map_or_else(|| OsStr::new("."), |argument| argument.value());
    let mut entries = platform
        .read_directory(DirectoryReadRequest::new(Path::new(path), cwd))
        .map_err(|error| RuntimeError::new(RuntimeErrorKind::DirectoryRead(error), stage.span()))?;
    let mut listing = list(move || entries.next_entry());
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || match listing.pull() {
        ListStep::Entry(value) => StreamPull::Item(value),
        ListStep::End => StreamPull::End,
        ListStep::Failed(error) => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::DirectoryRead(error),
            span,
        )),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn expect_stream(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<ValueStream, RuntimeError> {
    match input {
        InternalPayload::ValueStream(stream) => Ok(stream),
        other => Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinInputCarrier {
                command: command_name(stage),
                input: other.carrier(),
            },
            stage.span(),
        )),
    }
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

fn optional_count(
    stage: &PlannedStage,
    command: &'static str,
    default: usize,
) -> Result<usize, RuntimeError> {
    let Some(argument) = stage.arguments().first() else {
        return Ok(default);
    };
    let argument = word(command, argument)?;
    argument
        .value()
        .to_str()
        .and_then(|text| text.parse::<usize>().ok())
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorKind::BuiltinArgument {
                    command,
                    message: "count must be a nonnegative decimal integer".to_owned(),
                },
                argument.span(),
            )
        })
}

fn utf8_words(stage: &PlannedStage, command: &'static str) -> Result<Vec<String>, RuntimeError> {
    stage
        .arguments()
        .iter()
        .map(|argument| {
            let word = word(command, argument)?;
            word.value().to_str().map(str::to_owned).ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorKind::BuiltinArgument {
                        command,
                        message: "field names must be valid UTF-8".to_owned(),
                    },
                    word.span(),
                )
            })
        })
        .collect()
}

fn word<'a>(
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

fn drain<T>(
    command: &'static str,
    stage: &PlannedStage,
    outcome: DrainOutcome<T>,
) -> Result<T, RuntimeError> {
    match outcome {
        DrainOutcome::Done(value) => Ok(value),
        DrainOutcome::LimitExceeded { limit } => Err(structured_error(
            command,
            stage,
            format!("input exceeds the {limit}-item materialization limit"),
        )),
        DrainOutcome::Failed(error) => Err(error),
        DrainOutcome::Cancelled(reason) => Err(RuntimeError::new(
            RuntimeErrorKind::StreamCancelled { reason },
            stage.span(),
        )),
    }
}

fn structured_error(command: &'static str, stage: &PlannedStage, message: String) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::StructuredCommand { command, message },
        stage.span(),
    )
}

fn completed(payload: InternalPayload) -> Result<StageOutcome, RuntimeError> {
    Ok(StageOutcome::Completed {
        payload,
        status: success_status(),
    })
}

fn success_status() -> Status {
    Status::exit(0, Duration::ZERO).expect("zero-duration success is valid")
}

fn aggregate_status(statuses: Vec<Status>, pipefail: bool) -> Status {
    if statuses.len() == 1 {
        return statuses
            .into_iter()
            .next()
            .expect("one status was measured");
    }
    let selected = if pipefail {
        statuses
            .iter()
            .rposition(|status| !status.is_ok())
            .unwrap_or(statuses.len() - 1)
    } else {
        statuses.len() - 1
    };
    Status::aggregate(statuses, selected, Duration::ZERO)
        .expect("an internal pipeline aggregates leaf statuses")
}

fn command_name(stage: &PlannedStage) -> &'static str {
    match stage.resolution() {
        PlannedResolution::Internal { name } => match name.as_str() {
            "first" => "first",
            "last" => "last",
            "collect" => "collect",
            "length" => "length",
            "lines" => "lines",
            "select" => "select",
            "get" => "get",
            "sort" => "sort",
            "ls" => "ls",
            _ => "internal command",
        },
        PlannedResolution::External { .. } => "internal command",
    }
}
