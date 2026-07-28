//! Live execution of all-internal structured pipelines.
//!
//! The external executor owns process descriptors. This module owns the
//! all-internal carrier path: it moves `Empty`, `Value`, lazy `ValueStream`, and
//! lazy `ByteStream` payloads directly between stages without implicit display,
//! serialization, decoding, or edge materialization.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::Path;
use std::rc::Rc;

use flashshell_platform::{
    DirectoryReadRequest, FileActionError, FileOpenMode, FileOpenRequest, Platform,
};
use flashshell_syntax::SourceFile;

use crate::builtin::{BuiltinOutcome, BuiltinOutput, SessionState, execute_builtin};
use crate::closure::{
    EachStep, OwnedClosureContext, UpdateStep, WhereStep, each_owned, update_owned, where_owned,
};
use crate::command::{Carrier, CommandRegistry};
use crate::convert::{Codec, DecodeStep, EncodeStep, decode, encode};
use crate::directory::{ListStep, list};
use crate::eval::{EvalLimits, RuntimeError, RuntimeErrorKind};
use crate::file::{OpenStep, open, write_complete};
use crate::format::{
    FromJsonStep, FromTextStep, JsonMode, ToJsonStep, ToTextStep, from_json, from_text, to_json,
    to_text,
};
use crate::plan::{ExecutionPlan, PlannedArgument, PlannedResolution, PlannedStage, preflight};
use crate::resolve::ExecutableProbe;
use crate::stream::{BytePull, ByteStream, StreamPull, ValueStream};
use crate::structured::{
    DrainOutcome, GetStep, LineStep, SelectStep, SortOutcome, collect, first, get, last, length,
    lines, select, sort,
};
use crate::{Duration, Status, Value};

/// The documented item ceiling for commands that must drain a complete value
/// stream (`last`, `collect`, `length`, and `sort`).
pub const DEFAULT_MATERIALIZATION_LIMIT: usize = 1_000_000;

/// Default byte budget for one JSON document or one unterminated text line.
pub const DEFAULT_FORMAT_LIMIT: usize = 8 * 1024 * 1024;

/// Maximum chunk size requested by one lazy `open` pull.
pub const DEFAULT_FILE_CHUNK_SIZE: usize = 64 * 1024;

/// One owned payload moving across an internal pipeline edge.
pub enum InternalPayload {
    /// No payload.
    Empty,
    /// Exactly one structured value.
    Value(Value),
    /// A lazy ordered sequence of structured values.
    ValueStream(ValueStream),
    /// A lazy ordered sequence of byte-preserving chunks.
    ByteStream(ByteStream),
}

impl InternalPayload {
    /// The carrier represented by this payload.
    #[must_use]
    pub const fn carrier(&self) -> Carrier {
        match self {
            Self::Empty => Carrier::Empty,
            Self::Value(_) => Carrier::Value,
            Self::ValueStream(_) => Carrier::ValueStream,
            Self::ByteStream(_) => Carrier::ByteStream,
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
        /// The shared closure environment after every lazy stage has drained.
        closure_context: Box<OwnedClosureContext>,
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
    source: &SourceFile,
) -> Result<InternalPipelineOutcome, RuntimeError> {
    preflight(plan)?;
    let mut payload = InternalPayload::Empty;
    let mut statuses = Vec::with_capacity(plan.stages().len());
    let closure_context = OwnedClosureContext::new(
        source.clone(),
        state.environment().clone(),
        EvalLimits::default(),
    );

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
            &closure_context,
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
    Ok(InternalPipelineOutcome::Completed {
        payload,
        status,
        closure_context: Box::new(closure_context),
    })
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
    closure_context: &OwnedClosureContext,
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
        "each" => execute_each(stage, input, closure_context),
        "where" => execute_where(stage, input, closure_context),
        "update" => execute_update(stage, input, closure_context),
        "decode" => execute_decode(stage, input),
        "encode" => execute_encode(stage, input),
        "from" => execute_from(stage, input),
        "to" => execute_to(stage, input),
        "open" => execute_open(stage, input, platform, cwd),
        "save" => execute_save(stage, input, platform, cwd),
        _ => Err(RuntimeError::new(
            RuntimeErrorKind::Unsupported {
                feature: "this internal command in the live structured executor",
            },
            stage.span(),
        )),
    }
}

fn execute_each(
    stage: &PlannedStage,
    input: InternalPayload,
    context: &OwnedClosureContext,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "each", 1, Some(1))?;
    let closure = typed_value("each", &stage.arguments()[0])?.clone();
    let mut mapper = each_owned(
        expect_stream(stage, input)?,
        closure,
        stage.span(),
        context.clone(),
    );
    let stream = ValueStream::from_pull_fn(move || match mapper.pull() {
        EachStep::Item(value) => StreamPull::Item(value),
        EachStep::End => StreamPull::End,
        EachStep::Failed(error) => StreamPull::Failed(error),
        EachStep::Cancelled(reason) => StreamPull::Cancelled(reason),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_where(
    stage: &PlannedStage,
    input: InternalPayload,
    context: &OwnedClosureContext,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "where", 1, Some(1))?;
    let closure = typed_value("where", &stage.arguments()[0])?.clone();
    let mut filter = where_owned(
        expect_stream(stage, input)?,
        closure,
        stage.span(),
        context.clone(),
    );
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || match filter.pull() {
        WhereStep::Item(value) => StreamPull::Item(value),
        WhereStep::End => StreamPull::End,
        WhereStep::PredicateNotBool { actual } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "where",
                message: format!("predicate must return Bool, found {actual}"),
            },
            span,
        )),
        WhereStep::Failed(error) => StreamPull::Failed(error),
        WhereStep::Cancelled(reason) => StreamPull::Cancelled(reason),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_update(
    stage: &PlannedStage,
    input: InternalPayload,
    context: &OwnedClosureContext,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "update", 2, Some(2))?;
    let key_word = word("update", &stage.arguments()[0])?;
    let key = key_word
        .value()
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorKind::BuiltinArgument {
                    command: "update",
                    message: "the field name must be valid UTF-8".to_owned(),
                },
                key_word.span(),
            )
        })?;
    let replacement = match &stage.arguments()[1] {
        PlannedArgument::Value { value, .. } => value.clone(),
        PlannedArgument::Word(word) => {
            word.value().to_str().map(Value::string).ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorKind::BuiltinArgument {
                        command: "update",
                        message: "a static replacement must be valid UTF-8".to_owned(),
                    },
                    word.span(),
                )
            })?
        }
    };
    let mut updater = update_owned(
        expect_stream(stage, input)?,
        key,
        replacement,
        stage.span(),
        context.clone(),
    );
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || match updater.pull() {
        UpdateStep::Record(value) => StreamPull::Item(value),
        UpdateStep::End => StreamPull::End,
        UpdateStep::MissingKey { key } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "update",
                message: format!("record has no field `{key}`"),
            },
            span,
        )),
        UpdateStep::NotRecord { actual } => StreamPull::Failed(RuntimeError::new(
            RuntimeErrorKind::StructuredCommand {
                command: "update",
                message: format!("expected Record input, found {actual}"),
            },
            span,
        )),
        UpdateStep::Failed(error) => StreamPull::Failed(error),
        UpdateStep::Cancelled(reason) => StreamPull::Cancelled(reason),
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_decode(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "decode", 1, Some(1))?;
    let codec = parse_codec(stage, "decode", true)?;
    let bridge = Rc::new(RefCell::new(ByteInputBridge::new(expect_bytes(
        stage, input,
    )?)));
    let source = Rc::clone(&bridge);
    let mut decoder = decode(codec, move || source.borrow_mut().next_chunk());
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || {
        let step = decoder.pull();
        if let Some(terminal) = bridge.borrow_mut().take_terminal() {
            return terminal.into_value_pull();
        }
        match step {
            DecodeStep::Value(value) => StreamPull::Item(value),
            DecodeStep::End => StreamPull::End,
            DecodeStep::Malformed { offset } => StreamPull::Failed(structured_error_at(
                "decode",
                format!("malformed input at byte offset {offset}"),
                span,
            )),
        }
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_encode(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "encode", 1, Some(1))?;
    let codec = parse_codec(stage, "encode", false)?;
    let mut encoder = encode(codec, expect_values(stage, input)?);
    let span = stage.span();
    let stream = ByteStream::from_pull_fn(move || match encoder.pull() {
        EncodeStep::Chunk(chunk) => BytePull::Chunk(chunk),
        EncodeStep::End => BytePull::End,
        EncodeStep::NotEncodable { actual } => BytePull::Failed(structured_error_at(
            "encode",
            format!("cannot encode {actual} with the selected codec"),
            span,
        )),
        EncodeStep::Failed(error) => BytePull::Failed(error),
        EncodeStep::Cancelled(reason) => BytePull::Cancelled(reason),
    });
    completed(InternalPayload::ByteStream(stream))
}

fn execute_from(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "from", 1, Some(2))?;
    let format = utf8_word("from", &stage.arguments()[0], "format")?;
    let input = expect_bytes(stage, input)?;
    match format {
        "json" => execute_from_json(stage, input),
        "text" => {
            expect_arity(stage, "from", 1, Some(1))?;
            execute_from_text(stage, input)
        }
        other => Err(structured_error_at(
            "from",
            format!("unknown format `{other}`; expected `json` or `text`"),
            stage.arguments()[0].span(),
        )),
    }
}

fn execute_from_json(
    stage: &PlannedStage,
    input: ByteStream,
) -> Result<StageOutcome, RuntimeError> {
    let mode = match stage.arguments().get(1) {
        None => JsonMode::Document,
        Some(argument) => match utf8_word("from", argument, "JSON mode")? {
            "document" => JsonMode::Document,
            "array" => JsonMode::Array,
            other => {
                return Err(structured_error_at(
                    "from",
                    format!("unknown JSON mode `{other}`; expected `document` or `array`"),
                    argument.span(),
                ));
            }
        },
    };
    let bridge = Rc::new(RefCell::new(ByteInputBridge::new(input)));
    let source = Rc::clone(&bridge);
    let mut parser = from_json(
        mode,
        move || source.borrow_mut().next_chunk(),
        DEFAULT_FORMAT_LIMIT,
    );
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || {
        let step = parser.pull();
        if let Some(terminal) = bridge.borrow_mut().take_terminal() {
            return terminal.into_value_pull();
        }
        match step {
            FromJsonStep::Value(value) => StreamPull::Item(value),
            FromJsonStep::End => StreamPull::End,
            FromJsonStep::Malformed { offset } => StreamPull::Failed(structured_error_at(
                "from",
                format!("malformed JSON at byte offset {offset}"),
                span,
            )),
            FromJsonStep::NotArray { actual } => StreamPull::Failed(structured_error_at(
                "from",
                format!("JSON array mode requires a List, found {actual}"),
                span,
            )),
            FromJsonStep::DuplicateKey { key } => StreamPull::Failed(structured_error_at(
                "from",
                format!("JSON object repeats field `{key}`"),
                span,
            )),
            FromJsonStep::LimitExceeded { limit } => StreamPull::Failed(structured_error_at(
                "from",
                format!("JSON input exceeds the {limit}-byte materialization limit"),
                span,
            )),
        }
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_from_text(
    stage: &PlannedStage,
    input: ByteStream,
) -> Result<StageOutcome, RuntimeError> {
    let bridge = Rc::new(RefCell::new(ByteInputBridge::new(input)));
    let source = Rc::clone(&bridge);
    let mut parser = from_text(
        move || source.borrow_mut().next_chunk(),
        DEFAULT_FORMAT_LIMIT,
    );
    let span = stage.span();
    let stream = ValueStream::from_pull_fn(move || {
        let step = parser.pull();
        if let Some(terminal) = bridge.borrow_mut().take_terminal() {
            return terminal.into_value_pull();
        }
        match step {
            FromTextStep::Line(value) => StreamPull::Item(value),
            FromTextStep::End => StreamPull::End,
            FromTextStep::Malformed { offset } => StreamPull::Failed(structured_error_at(
                "from",
                format!("malformed text at byte offset {offset}"),
                span,
            )),
            FromTextStep::LineTooLong { limit } => StreamPull::Failed(structured_error_at(
                "from",
                format!("text line exceeds the {limit}-byte limit"),
                span,
            )),
        }
    });
    completed(InternalPayload::ValueStream(stream))
}

fn execute_to(stage: &PlannedStage, input: InternalPayload) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "to", 1, Some(1))?;
    let format = utf8_word("to", &stage.arguments()[0], "format")?;
    let values = expect_values(stage, input)?;
    let span = stage.span();
    let stream = match format {
        "json" => {
            let mut writer = to_json(values);
            ByteStream::from_pull_fn(move || match writer.pull() {
                ToJsonStep::Chunk(chunk) => BytePull::Chunk(chunk),
                ToJsonStep::End => BytePull::End,
                ToJsonStep::NotEncodable { actual } => BytePull::Failed(structured_error_at(
                    "to",
                    format!("JSON cannot encode {actual}"),
                    span,
                )),
                ToJsonStep::Failed(error) => BytePull::Failed(error),
                ToJsonStep::Cancelled(reason) => BytePull::Cancelled(reason),
            })
        }
        "text" => {
            let mut writer = to_text(values);
            ByteStream::from_pull_fn(move || match writer.pull() {
                ToTextStep::Chunk(chunk) => BytePull::Chunk(chunk),
                ToTextStep::End => BytePull::End,
                ToTextStep::NotEncodable { actual } => BytePull::Failed(structured_error_at(
                    "to",
                    format!("text cannot encode {actual}"),
                    span,
                )),
                ToTextStep::Failed(error) => BytePull::Failed(error),
                ToTextStep::Cancelled(reason) => BytePull::Cancelled(reason),
            })
        }
        other => {
            return Err(structured_error_at(
                "to",
                format!("unknown format `{other}`; expected `json` or `text`"),
                stage.arguments()[0].span(),
            ));
        }
    };
    completed(InternalPayload::ByteStream(stream))
}

fn execute_open(
    stage: &PlannedStage,
    input: InternalPayload,
    platform: &dyn Platform,
    cwd: &Path,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "open", 1, Some(1))?;
    expect_empty(stage, input)?;
    let path = word("open", &stage.arguments()[0])?;
    let mut endpoint = platform
        .open_file_io(FileOpenRequest::new(
            Path::new(path.value()),
            cwd,
            FileOpenMode::Read,
        ))
        .map_err(|error| file_error("open", error, path.span()))?;
    let mut reader = open(move |buffer| endpoint.read(buffer), DEFAULT_FILE_CHUNK_SIZE);
    let span = stage.span();
    let stream = ByteStream::from_pull_fn(move || match reader.pull() {
        OpenStep::Chunk(chunk) => BytePull::Chunk(chunk),
        OpenStep::End => BytePull::End,
        OpenStep::Failed(error) => BytePull::Failed(file_error("open", error, span)),
    });
    completed(InternalPayload::ByteStream(stream))
}

fn execute_save(
    stage: &PlannedStage,
    input: InternalPayload,
    platform: &dyn Platform,
    cwd: &Path,
) -> Result<StageOutcome, RuntimeError> {
    expect_arity(stage, "save", 1, Some(1))?;
    let mut input = expect_bytes(stage, input)?;
    let path = word("save", &stage.arguments()[0])?;
    let mut endpoint = platform
        .open_file_io(FileOpenRequest::new(
            Path::new(path.value()),
            cwd,
            FileOpenMode::WriteTruncate,
        ))
        .map_err(|error| file_error("save", error, path.span()))?;
    loop {
        match input.pull() {
            BytePull::Chunk(chunk) => {
                let mut write = |bytes: &[u8]| endpoint.write(bytes);
                write_complete(&mut write, &chunk)
                    .map_err(|error| file_error("save", error, stage.span()))?;
            }
            BytePull::End => return completed(InternalPayload::Empty),
            BytePull::Failed(error) => return Err(error),
            BytePull::Cancelled(reason) => {
                return Err(RuntimeError::new(
                    RuntimeErrorKind::StreamCancelled { reason },
                    stage.span(),
                ));
            }
        }
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

struct ByteInputBridge {
    input: ByteStream,
    terminal: Option<BytePull>,
}

impl ByteInputBridge {
    fn new(input: ByteStream) -> Self {
        Self {
            input,
            terminal: None,
        }
    }

    fn next_chunk(&mut self) -> Option<Vec<u8>> {
        match self.input.pull() {
            BytePull::Chunk(chunk) => Some(chunk),
            BytePull::End => None,
            terminal @ (BytePull::Failed(_) | BytePull::Cancelled(_)) => {
                self.terminal = Some(terminal);
                None
            }
        }
    }

    fn take_terminal(&mut self) -> Option<BytePull> {
        self.terminal.take()
    }
}

impl BytePull {
    fn into_value_pull(self) -> StreamPull {
        match self {
            Self::Failed(error) => StreamPull::Failed(error),
            Self::Cancelled(reason) => StreamPull::Cancelled(reason),
            Self::Chunk(_) | Self::End => {
                unreachable!("only terminal byte-source failures are retained")
            }
        }
    }
}

fn parse_codec(
    stage: &PlannedStage,
    command: &'static str,
    allow_lossy: bool,
) -> Result<Codec, RuntimeError> {
    let argument = &stage.arguments()[0];
    match utf8_word(command, argument, "codec")? {
        "utf8" => Ok(Codec::Utf8 { lossy: false }),
        "utf8-lossy" if allow_lossy => Ok(Codec::Utf8 { lossy: true }),
        "bytes" => Ok(Codec::Bytes),
        other => {
            let expected = if allow_lossy {
                "`utf8`, `utf8-lossy`, or `bytes`"
            } else {
                "`utf8` or `bytes`"
            };
            Err(structured_error_at(
                command,
                format!("unknown codec `{other}`; expected {expected}"),
                argument.span(),
            ))
        }
    }
}

fn expect_empty(stage: &PlannedStage, input: InternalPayload) -> Result<(), RuntimeError> {
    match input {
        InternalPayload::Empty => Ok(()),
        other => Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinInputCarrier {
                command: command_name(stage),
                input: other.carrier(),
            },
            stage.span(),
        )),
    }
}

fn expect_bytes(stage: &PlannedStage, input: InternalPayload) -> Result<ByteStream, RuntimeError> {
    match input {
        InternalPayload::ByteStream(stream) => Ok(stream),
        other => Err(RuntimeError::new(
            RuntimeErrorKind::BuiltinInputCarrier {
                command: command_name(stage),
                input: other.carrier(),
            },
            stage.span(),
        )),
    }
}

fn expect_values(
    stage: &PlannedStage,
    input: InternalPayload,
) -> Result<ValueStream, RuntimeError> {
    match input {
        InternalPayload::Value(value) => Ok(ValueStream::once(value)),
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

fn utf8_word<'a>(
    command: &'static str,
    argument: &'a PlannedArgument,
    subject: &str,
) -> Result<&'a str, RuntimeError> {
    let word = word(command, argument)?;
    word.value().to_str().ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorKind::BuiltinArgument {
                command,
                message: format!("{subject} must be valid UTF-8"),
            },
            word.span(),
        )
    })
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

fn typed_value<'a>(
    command: &'static str,
    argument: &'a PlannedArgument,
) -> Result<&'a Value, RuntimeError> {
    argument.as_value().ok_or_else(|| {
        RuntimeError::new(
            RuntimeErrorKind::BuiltinArgument {
                command,
                message: "expected a closure argument".to_owned(),
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

fn structured_error_at(
    command: &'static str,
    message: String,
    span: flashshell_syntax::Span,
) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::StructuredCommand { command, message },
        span,
    )
}

fn file_error(
    command: &'static str,
    error: FileActionError,
    span: flashshell_syntax::Span,
) -> RuntimeError {
    structured_error_at(command, error.to_string(), span)
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
            "each" => "each",
            "where" => "where",
            "update" => "update",
            "decode" => "decode",
            "encode" => "encode",
            "from" => "from",
            "to" => "to",
            "open" => "open",
            "save" => "save",
            _ => "internal command",
        },
        PlannedResolution::External { .. } => "internal command",
    }
}
