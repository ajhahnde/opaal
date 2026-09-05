//! Responsive single-worker language-server coordinator.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

use flash_syntax::{LanguageMajor, PositionEncoding, Severity, TextRange};
use serde_json::{Map, Value, json};

use crate::protocol::{
    ExitStatus, ServerError, initialize_result, request_failure_response, write_message,
};
use crate::query::{PreparedResponse, RequestControl, RequestError, prepare_request};
use crate::transport::read_frame;
use crate::uri::DocumentUri;
use crate::workspace::{
    ChangeOutcome, DiagnosticAnalysis, DiagnosticDocument, DiagnosticProjectionError,
    DiagnosticPublication, DiagnosticPublishOutcome, Workspace, WorkspaceSnapshot,
};

const PARSE_ERROR: i64 = -32_700;
const INVALID_REQUEST: i64 = -32_600;
const METHOD_NOT_FOUND: i64 = -32_601;
const INVALID_PARAMS: i64 = -32_602;
const SERVER_NOT_INITIALIZED: i64 = -32_002;

/// Runs the stdio server with a dedicated reader and one bounded analysis worker.
pub fn run<R>(input: R, output: &mut impl Write) -> Result<ExitStatus, ServerError>
where
    R: BufRead + Send + 'static,
{
    run_for_language(input, output, LanguageMajor::V1)
}

/// Runs the stdio server with one language selected before protocol documents
/// or worker state are accepted.
pub fn run_for_language<R>(
    input: R,
    output: &mut impl Write,
    language: LanguageMajor,
) -> Result<ExitStatus, ServerError>
where
    R: BufRead + Send + 'static,
{
    let (events, incoming) = mpsc::channel();
    let reader_events = events.clone();
    thread::spawn(move || read_messages(input, &reader_events));

    let (jobs, worker_jobs) = mpsc::sync_channel(1);
    thread::spawn(move || work(worker_jobs, &events));

    let mut coordinator = Coordinator::for_language(language);
    loop {
        match incoming.recv() {
            Ok(Event::Message(message)) => {
                let action = coordinator.handle(message);
                for response in action.output {
                    write_message(output, &response)?;
                }
                if let Some(exit) = action.exit {
                    coordinator.cancel_all();
                    return Ok(exit);
                }
                coordinator.start_next(&jobs);
            }
            Ok(Event::ParseError) => write_message(
                output,
                &error_response(Value::Null, PARSE_ERROR, "Parse error"),
            )?,
            Ok(Event::Worker(result)) => {
                for message in coordinator.complete(result) {
                    write_message(output, &message)?;
                }
                coordinator.start_next(&jobs);
            }
            Ok(Event::InputFailure(error)) => return Err(ServerError::Frame(error)),
            Ok(Event::Eof) | Err(_) => {
                coordinator.cancel_all();
                return Ok(ExitStatus::Failure);
            }
        }
    }
}

fn read_messages(mut input: impl BufRead, events: &mpsc::Sender<Event>) {
    loop {
        match read_frame(&mut input) {
            Ok(Some(body)) => match serde_json::from_slice(&body) {
                Ok(message) => {
                    if events.send(Event::Message(message)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    if events.send(Event::ParseError).is_err() {
                        return;
                    }
                }
            },
            Ok(None) => {
                let _ = events.send(Event::Eof);
                return;
            }
            Err(error) => {
                let _ = events.send(Event::InputFailure(error));
                return;
            }
        }
    }
}

fn work(jobs: Receiver<Job>, events: &mpsc::Sender<Event>) {
    while let Ok(job) = jobs.recv() {
        let result = match job {
            Job::Diagnostics {
                serial,
                snapshot,
                encoding,
                control,
            } => WorkerResult::Diagnostics {
                serial,
                control: control.clone(),
                result: snapshot
                    .analyze_diagnostics_controlled(encoding, &control.analysis_control()),
            },
            Job::Request {
                serial,
                id,
                method,
                params,
                snapshot,
                encoding,
                control,
            } => WorkerResult::Request {
                serial,
                id,
                prepared: prepare_request(&snapshot, encoding, control, &method, &params),
            },
        };
        if events.send(Event::Worker(result)).is_err() {
            return;
        }
    }
}

enum Event {
    Message(Value),
    ParseError,
    InputFailure(crate::transport::FrameError),
    Eof,
    Worker(WorkerResult),
}

enum Job {
    Diagnostics {
        serial: u64,
        snapshot: WorkspaceSnapshot,
        encoding: PositionEncoding,
        control: RequestControl,
    },
    Request {
        serial: u64,
        id: Value,
        method: String,
        params: Value,
        snapshot: WorkspaceSnapshot,
        encoding: PositionEncoding,
        control: RequestControl,
    },
}

impl Job {
    fn is_diagnostics(&self) -> bool {
        matches!(self, Self::Diagnostics { .. })
    }
}

enum WorkerResult {
    Diagnostics {
        serial: u64,
        control: RequestControl,
        result: Result<Option<DiagnosticAnalysis>, DiagnosticProjectionError>,
    },
    Request {
        serial: u64,
        id: Value,
        prepared: PreparedResponse,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    WaitingForInitialize,
    Running,
    ShutdownRequested,
}

struct TrackedControl {
    serial: u64,
    request_id: Option<Value>,
    control: RequestControl,
}

struct Coordinator {
    lifecycle: Lifecycle,
    workspace: Workspace,
    encoding: PositionEncoding,
    related_information: bool,
    pending: VecDeque<Job>,
    controls: Vec<TrackedControl>,
    worker_busy: bool,
    next_serial: u64,
}

impl Coordinator {
    fn for_language(language: LanguageMajor) -> Self {
        Self {
            lifecycle: Lifecycle::WaitingForInitialize,
            workspace: Workspace::for_language(language),
            encoding: PositionEncoding::Utf16,
            related_information: false,
            pending: VecDeque::new(),
            controls: Vec::new(),
            worker_busy: false,
            next_serial: 0,
        }
    }

    fn handle(&mut self, message: Value) -> Action {
        let Value::Object(object) = message else {
            return Action::response(error_response(
                Value::Null,
                INVALID_REQUEST,
                "Invalid Request",
            ));
        };
        self.handle_object(&object)
    }

    fn handle_object(&mut self, object: &Map<String, Value>) -> Action {
        let id = object.get("id").cloned();
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || id.as_ref().is_some_and(|id| !valid_id(id))
        {
            return invalid_message(id);
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return invalid_message(id);
        };
        if object
            .get("params")
            .is_some_and(|params| !params.is_object() && !params.is_array())
        {
            return request_error(id, INVALID_PARAMS, "Invalid params");
        }

        if method == "exit" {
            return if id.is_none() {
                Action::exit(if self.lifecycle == Lifecycle::ShutdownRequested {
                    ExitStatus::Success
                } else {
                    ExitStatus::Failure
                })
            } else {
                request_error(id, INVALID_REQUEST, "Invalid Request")
            };
        }
        if method == "$/cancelRequest" {
            return self.cancel_request(object, id);
        }

        match self.lifecycle {
            Lifecycle::WaitingForInitialize => self.before_initialize(method, object, id),
            Lifecycle::Running => self.running(method, object, id),
            Lifecycle::ShutdownRequested => request_error(id, INVALID_REQUEST, "Invalid Request"),
        }
    }

    fn before_initialize(
        &mut self,
        method: &str,
        object: &Map<String, Value>,
        id: Option<Value>,
    ) -> Action {
        if method != "initialize" {
            return request_error(id, SERVER_NOT_INITIALIZED, "Server not initialized");
        }
        let Some(id) = id else {
            return Action::none();
        };
        let Some(params) = object.get("params").and_then(Value::as_object) else {
            return Action::response(error_response(id, INVALID_PARAMS, "Invalid params"));
        };
        let utf8 = offered_utf8(params);
        self.encoding = if utf8 {
            PositionEncoding::Utf8
        } else {
            PositionEncoding::Utf16
        };
        self.related_information = supports_related_information(params);
        self.lifecycle = Lifecycle::Running;
        Action::response(success_response(
            id,
            initialize_result(if utf8 { "utf-8" } else { "utf-16" }),
        ))
    }

    fn running(&mut self, method: &str, object: &Map<String, Value>, id: Option<Value>) -> Action {
        match method {
            "initialized" => notification_only(id),
            "shutdown" => {
                let Some(id) = id else {
                    return Action::none();
                };
                let mut output = self.cancel_pending_requests();
                self.lifecycle = Lifecycle::ShutdownRequested;
                output.push(success_response(id, Value::Null));
                Action { output, exit: None }
            }
            "initialize" => request_error(id, INVALID_REQUEST, "Invalid Request"),
            "textDocument/didOpen" => {
                if id.is_some() {
                    return request_error(id, INVALID_REQUEST, "Invalid Request");
                }
                self.did_open(object);
                Action::none()
            }
            "textDocument/didChange" => {
                if id.is_some() {
                    return request_error(id, INVALID_REQUEST, "Invalid Request");
                }
                self.did_change(object);
                Action::none()
            }
            "textDocument/didClose" => {
                if id.is_some() {
                    return request_error(id, INVALID_REQUEST, "Invalid Request");
                }
                self.did_close(object);
                Action::none()
            }
            method if supported_request(method) => {
                let Some(id) = id else {
                    return Action::none();
                };
                self.queue_request(
                    id,
                    method.to_owned(),
                    object.get("params").cloned().unwrap_or(Value::Null),
                );
                Action::none()
            }
            _ => request_error(id, METHOD_NOT_FOUND, "Method not found"),
        }
    }

    fn cancel_request(&mut self, object: &Map<String, Value>, id: Option<Value>) -> Action {
        if id.is_some() {
            return request_error(id, INVALID_REQUEST, "Invalid Request");
        }
        if let Some(target) = object
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("id"))
            .filter(|target| valid_id(target))
        {
            for tracked in &self.controls {
                if tracked.request_id.as_ref() == Some(target) {
                    tracked.control.cancel();
                }
            }
        }
        Action::none()
    }

    fn did_open(&mut self, object: &Map<String, Value>) {
        let Some(document) = object
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("textDocument"))
            .and_then(Value::as_object)
        else {
            return;
        };
        let Some(uri) = document
            .get("uri")
            .and_then(Value::as_str)
            .and_then(|uri| DocumentUri::parse(uri).ok())
        else {
            return;
        };
        let Some(version) = document
            .get("version")
            .and_then(Value::as_i64)
            .and_then(|version| i32::try_from(version).ok())
        else {
            return;
        };
        let Some(text) = document.get("text").and_then(Value::as_str) else {
            return;
        };
        if self.workspace.open(uri, version, text.to_owned()).is_ok() {
            self.workspace_changed();
        }
    }

    fn did_change(&mut self, object: &Map<String, Value>) {
        let Some(params) = object.get("params").and_then(Value::as_object) else {
            return;
        };
        let Some(document) = params.get("textDocument").and_then(Value::as_object) else {
            return;
        };
        let Some(uri) = document
            .get("uri")
            .and_then(Value::as_str)
            .and_then(|uri| DocumentUri::parse(uri).ok())
        else {
            return;
        };
        let version = document
            .get("version")
            .and_then(Value::as_i64)
            .and_then(|version| i32::try_from(version).ok());
        let Some(changes) = params.get("contentChanges").and_then(Value::as_array) else {
            return;
        };
        let [change] = changes.as_slice() else {
            return;
        };
        let Some(change) = change.as_object() else {
            return;
        };
        if change.contains_key("range") || change.contains_key("rangeLength") {
            return;
        }
        let Some(text) = change.get("text").and_then(Value::as_str) else {
            return;
        };
        if matches!(
            self.workspace.change(&uri, version, text.to_owned()),
            Ok(ChangeOutcome::Applied)
        ) {
            self.workspace_changed();
        }
    }

    fn did_close(&mut self, object: &Map<String, Value>) {
        let Some(uri) = object
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("textDocument"))
            .and_then(Value::as_object)
            .and_then(|document| document.get("uri"))
            .and_then(Value::as_str)
            .and_then(|uri| DocumentUri::parse(uri).ok())
        else {
            return;
        };
        if self.workspace.close(&uri).is_ok() {
            self.workspace_changed();
        }
    }

    fn workspace_changed(&mut self) {
        for tracked in &self.controls {
            tracked.control.invalidate();
        }
        let removed = self
            .pending
            .iter()
            .filter(|job| job.is_diagnostics())
            .map(|job| match job {
                Job::Diagnostics { serial, .. } => *serial,
                Job::Request { .. } => unreachable!("filtered jobs are diagnostics"),
            })
            .collect::<Vec<_>>();
        self.pending.retain(|job| !job.is_diagnostics());
        self.controls
            .retain(|tracked| !removed.contains(&tracked.serial));
        self.queue_diagnostics();
    }

    fn queue_diagnostics(&mut self) {
        let serial = self.serial();
        let control = RequestControl::new();
        self.controls.push(TrackedControl {
            serial,
            request_id: None,
            control: control.clone(),
        });
        self.pending.push_back(Job::Diagnostics {
            serial,
            snapshot: self.workspace.diagnostic_snapshot(),
            encoding: self.encoding,
            control,
        });
    }

    fn queue_request(&mut self, id: Value, method: String, params: Value) {
        let serial = self.serial();
        let control = RequestControl::new();
        self.controls.push(TrackedControl {
            serial,
            request_id: Some(id.clone()),
            control: control.clone(),
        });
        self.pending.push_back(Job::Request {
            serial,
            id,
            method,
            params,
            snapshot: self.workspace.diagnostic_snapshot(),
            encoding: self.encoding,
            control,
        });
    }

    fn serial(&mut self) -> u64 {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        serial
    }

    fn start_next(&mut self, jobs: &SyncSender<Job>) {
        if self.worker_busy {
            return;
        }
        let Some(job) = self.pending.pop_front() else {
            return;
        };
        match jobs.try_send(job) {
            Ok(()) => self.worker_busy = true,
            Err(TrySendError::Full(job)) => self.pending.push_front(job),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn complete(&mut self, result: WorkerResult) -> Vec<Value> {
        self.worker_busy = false;
        match result {
            WorkerResult::Diagnostics {
                serial,
                control,
                result,
            } => {
                if !self.take_control(serial) || control.is_cancelled() {
                    return Vec::new();
                }
                let Ok(Some(analysis)) = result else {
                    return Vec::new();
                };
                match self.workspace.publish_diagnostics(analysis) {
                    DiagnosticPublishOutcome::Published(publication) => {
                        diagnostic_messages(&publication, self.related_information)
                    }
                    DiagnosticPublishOutcome::Stale => Vec::new(),
                }
            }
            WorkerResult::Request {
                serial,
                id,
                prepared,
            } => {
                if !self.take_control(serial) {
                    return Vec::new();
                }
                vec![match prepared.finish(&self.workspace) {
                    Ok(result) => success_response(id, result),
                    Err(error) => request_failure_response(id, error),
                }]
            }
        }
    }

    fn take_control(&mut self, serial: u64) -> bool {
        let Some(index) = self
            .controls
            .iter()
            .position(|tracked| tracked.serial == serial)
        else {
            return false;
        };
        self.controls.remove(index);
        true
    }

    fn cancel_pending_requests(&mut self) -> Vec<Value> {
        let mut output = Vec::new();
        for tracked in self.controls.drain(..) {
            tracked.control.cancel();
            if let Some(id) = tracked.request_id {
                output.push(request_failure_response(id, RequestError::RequestCancelled));
            }
        }
        self.pending.clear();
        output
    }

    fn cancel_all(&mut self) {
        for tracked in &self.controls {
            tracked.control.cancel();
        }
        self.controls.clear();
        self.pending.clear();
    }
}

fn supported_request(method: &str) -> bool {
    matches!(
        method,
        "textDocument/completion"
            | "textDocument/hover"
            | "textDocument/signatureHelp"
            | "textDocument/definition"
            | "textDocument/references"
            | "textDocument/formatting"
    )
}

fn diagnostic_messages(publication: &DiagnosticPublication, related: bool) -> Vec<Value> {
    publication
        .documents()
        .iter()
        .map(|document| diagnostic_message(document, related))
        .collect()
}

fn diagnostic_message(document: &DiagnosticDocument, related: bool) -> Value {
    let diagnostics = document
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            let mut message = diagnostic.message().to_owned();
            if let Some(annotation) = diagnostic.primary_annotation() {
                message.push_str(": ");
                message.push_str(annotation);
            }
            if !related {
                for information in diagnostic.related_information() {
                    message.push_str("\nrelated: ");
                    message.push_str(information.uri().as_str());
                    message.push_str(": ");
                    message.push_str(information.message());
                }
            }
            for note in diagnostic.notes() {
                message.push_str("\nnote: ");
                message.push_str(note);
            }
            let mut value = json!({
                "range": range_value(diagnostic.range()),
                "severity": severity_value(diagnostic.severity()),
                "message": message,
                "source": "flash"
            });
            if let Some(code) = diagnostic.code() {
                value["code"] = json!(code);
            }
            if related && !diagnostic.related_information().is_empty() {
                value["relatedInformation"] = Value::Array(
                    diagnostic
                        .related_information()
                        .iter()
                        .map(|information| {
                            json!({
                                "location": {
                                    "uri": information.uri().as_str(),
                                    "range": range_value(information.range())
                                },
                                "message": information.message()
                            })
                        })
                        .collect(),
                );
            }
            value
        })
        .collect::<Vec<_>>();
    let mut params = json!({
        "uri": document.uri().as_str(),
        "diagnostics": diagnostics
    });
    if let Some(version) = document.version() {
        params["version"] = json!(version);
    }
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": params
    })
}

fn range_value(range: TextRange) -> Value {
    json!({
        "start": {
            "line": range.start().line(),
            "character": range.start().character()
        },
        "end": {
            "line": range.end().line(),
            "character": range.end().character()
        }
    })
}

const fn severity_value(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 1,
        Severity::Warning => 2,
        Severity::Note => 3,
    }
}

fn offered_utf8(params: &Map<String, Value>) -> bool {
    params
        .get("capabilities")
        .and_then(Value::as_object)
        .and_then(|capabilities| capabilities.get("general"))
        .and_then(Value::as_object)
        .and_then(|general| general.get("positionEncodings"))
        .and_then(Value::as_array)
        .is_some_and(|encodings| encodings.iter().any(|encoding| encoding == "utf-8"))
}

fn supports_related_information(params: &Map<String, Value>) -> bool {
    params
        .get("capabilities")
        .and_then(Value::as_object)
        .and_then(|capabilities| capabilities.get("textDocument"))
        .and_then(Value::as_object)
        .and_then(|text_document| text_document.get("publishDiagnostics"))
        .and_then(Value::as_object)
        .and_then(|diagnostics| diagnostics.get("relatedInformation"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn valid_id(id: &Value) -> bool {
    id.is_null() || id.is_string() || id.is_number()
}

fn notification_only(id: Option<Value>) -> Action {
    if id.is_some() {
        request_error(id, INVALID_REQUEST, "Invalid Request")
    } else {
        Action::none()
    }
}

fn invalid_message(id: Option<Value>) -> Action {
    Action::response(error_response(
        id.unwrap_or(Value::Null),
        INVALID_REQUEST,
        "Invalid Request",
    ))
}

fn request_error(id: Option<Value>, code: i64, message: &str) -> Action {
    id.map_or_else(Action::none, |id| {
        Action::response(error_response(id, code, message))
    })
}

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

struct Action {
    output: Vec<Value>,
    exit: Option<ExitStatus>,
}

impl Action {
    const fn none() -> Self {
        Self {
            output: Vec::new(),
            exit: None,
        }
    }

    fn response(response: Value) -> Self {
        Self {
            output: vec![response],
            exit: None,
        }
    }

    const fn exit(exit: ExitStatus) -> Self {
        Self {
            output: Vec::new(),
            exit: Some(exit),
        }
    }
}
