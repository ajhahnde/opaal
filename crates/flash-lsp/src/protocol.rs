//! Narrow JSON-RPC lifecycle coordinator for the Flash language server.

use std::fmt;
use std::io::{self, BufRead, Write};

use serde_json::{Map, Value, json};

use crate::transport::{FrameError, read_frame, write_frame};

const PARSE_ERROR: i64 = -32_700;
const INVALID_REQUEST: i64 = -32_600;
const METHOD_NOT_FOUND: i64 = -32_601;
const INVALID_PARAMS: i64 = -32_602;
const SERVER_NOT_INITIALIZED: i64 = -32_002;
const REQUEST_CANCELLED: i64 = -32_800;

/// The process-level result required by the LSP shutdown handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitStatus {
    Success,
    Failure,
}

/// A fatal transport or output failure.
#[derive(Debug)]
pub enum ServerError {
    Frame(FrameError),
    Output(io::Error),
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => error.fmt(formatter),
            Self::Output(error) => write!(formatter, "protocol output failed: {error}"),
        }
    }
}

impl std::error::Error for ServerError {}

/// Runs the framed lifecycle coordinator until `exit`, EOF, or a fatal I/O error.
pub fn run(input: &mut impl BufRead, output: &mut impl Write) -> Result<ExitStatus, ServerError> {
    let mut coordinator = Coordinator::new();
    loop {
        let Some(body) = read_frame(input).map_err(ServerError::Frame)? else {
            return Ok(ExitStatus::Failure);
        };
        let message = match serde_json::from_slice(&body) {
            Ok(message) => message,
            Err(_) => {
                write_message(
                    output,
                    &error_response(Value::Null, PARSE_ERROR, "Parse error"),
                )?;
                continue;
            }
        };
        let action = coordinator.handle(message);
        if let Some(response) = action.response {
            write_message(output, &response)?;
        }
        if let Some(status) = action.exit {
            return Ok(status);
        }
    }
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<(), ServerError> {
    let body = serde_json::to_vec(message).map_err(|error| {
        ServerError::Output(io::Error::other(format!("cannot encode response: {error}")))
    })?;
    write_frame(output, &body).map_err(ServerError::Output)?;
    output.flush().map_err(ServerError::Output)
}

/// Builds the exact standard LSP response for an explicitly cancelled request.
#[must_use]
pub fn request_cancelled(id: Value) -> Value {
    error_response(id, REQUEST_CANCELLED, "Request cancelled")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    WaitingForInitialize,
    Running,
    ShutdownRequested,
}

struct Coordinator {
    lifecycle: Lifecycle,
}

impl Coordinator {
    const fn new() -> Self {
        Self {
            lifecycle: Lifecycle::WaitingForInitialize,
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

        let position_encoding = if offered_utf8(params) {
            "utf-8"
        } else {
            "utf-16"
        };
        self.lifecycle = Lifecycle::Running;
        Action::response(success_response(id, initialize_result(position_encoding)))
    }

    fn running(&mut self, method: &str, _object: &Map<String, Value>, id: Option<Value>) -> Action {
        match method {
            "initialized" => {
                if id.is_some() {
                    request_error(id, INVALID_REQUEST, "Invalid Request")
                } else {
                    Action::none()
                }
            }
            "shutdown" => {
                let Some(id) = id else {
                    return Action::none();
                };
                self.lifecycle = Lifecycle::ShutdownRequested;
                Action::response(success_response(id, Value::Null))
            }
            "initialize" => request_error(id, INVALID_REQUEST, "Invalid Request"),
            _ => request_error(id, METHOD_NOT_FOUND, "Method not found"),
        }
    }

    fn cancel_request(&self, object: &Map<String, Value>, id: Option<Value>) -> Action {
        if id.is_some() {
            return request_error(id, INVALID_REQUEST, "Invalid Request");
        }
        let _valid = object
            .get("params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("id"))
            .is_some_and(valid_id);
        Action::none()
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

fn initialize_result(position_encoding: &str) -> Value {
    json!({
        "capabilities": {
            "positionEncoding": position_encoding,
            "textDocumentSync": {"openClose": true, "change": 1},
            "completionProvider": {"resolveProvider": false},
            "hoverProvider": true,
            "signatureHelpProvider": {},
            "definitionProvider": true,
            "referencesProvider": true,
            "documentFormattingProvider": true
        },
        "serverInfo": {
            "name": "Flash Language Server",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn valid_id(id: &Value) -> bool {
    id.is_null() || id.is_string() || id.is_number()
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
    response: Option<Value>,
    exit: Option<ExitStatus>,
}

impl Action {
    const fn none() -> Self {
        Self {
            response: None,
            exit: None,
        }
    }

    const fn response(response: Value) -> Self {
        Self {
            response: Some(response),
            exit: None,
        }
    }

    const fn exit(exit: ExitStatus) -> Self {
        Self {
            response: None,
            exit: Some(exit),
        }
    }
}
