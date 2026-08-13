#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_lsp::transport::{read_frame, write_frame};
use flash_lsp::uri::DocumentUri;
use serde_json::{Value, json};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("flash-lsp-server-{}-{serial}", std::process::id()));
        fs::create_dir(&path).expect("temporary server directory should be created");
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn uri(&self, name: &str) -> DocumentUri {
        DocumentUri::from_absolute_path(&self.path(name)).expect("temporary path should be a URI")
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    received: Vec<Value>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_flash-language-server"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("language server should start");
        let input = child.stdin.take().expect("server stdin should be piped");
        let output = BufReader::new(child.stdout.take().expect("server stdout should be piped"));
        Self {
            child,
            input,
            output,
            received: Vec::new(),
        }
    }

    fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).expect("protocol message should encode");
        write_frame(&mut self.input, &body).expect("protocol frame should write");
        self.input.flush().expect("protocol frame should flush");
    }

    fn receive(&mut self) -> Value {
        let body = read_frame(&mut self.output)
            .expect("server frame should be valid")
            .expect("server should produce a frame");
        let message: Value =
            serde_json::from_slice(&body).expect("server frame should contain JSON");
        self.received.push(message.clone());
        message
    }

    fn receive_response(&mut self, id: i64) -> Value {
        loop {
            let message = self.receive();
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    fn initialize(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {
                    "general": {"positionEncodings": ["utf-8"]},
                    "textDocument": {
                        "publishDiagnostics": {"relatedInformation": true}
                    }
                }
            }
        }));
        assert_eq!(
            self.receive_response(1)["result"]["capabilities"]["positionEncoding"],
            "utf-8"
        );
        self.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    }

    fn response_count(&self, id: i64) -> usize {
        self.received
            .iter()
            .filter(|message| message.get("id") == Some(&json!(id)))
            .count()
    }

    fn open(&mut self, uri: &DocumentUri, version: i32, text: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri.as_str(), "version": version, "text": text}}
        }));
    }

    fn finish(mut self) -> std::process::Output {
        self.send(json!({"jsonrpc": "2.0", "id": 90, "method": "shutdown"}));
        assert_eq!(self.receive_response(90)["result"], Value::Null);
        self.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        drop(self.input);
        self.child
            .wait_with_output()
            .expect("language server should exit")
    }
}

fn position_request(id: i64, method: &str, uri: &DocumentUri, line: u64, character: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {
            "textDocument": {"uri": uri.as_str()},
            "position": {"line": line, "character": character}
        }
    })
}

fn wait_for_diagnostics(server: &mut Server, uri: &DocumentUri, version: i32) -> Value {
    loop {
        let message = server.receive();
        if message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == uri.as_str()
            && message["params"]["version"] == version
        {
            return message;
        }
    }
}

#[test]
fn executable_serves_overlays_diagnostics_queries_and_clean_shutdown() {
    let directory = TempDirectory::new();
    let uri = directory.uri("main.fsh");
    let mut server = Server::start();
    server.initialize();
    server.open(&uri, 1, "let broken =\n");

    let diagnostics = wait_for_diagnostics(&mut server, &uri, 1);
    assert!(
        !diagnostics["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": {"uri": uri.as_str(), "version": 2},
            "contentChanges": [{"text": "let answer = 42\n$answer\n"}]
        }
    }));
    let cleared = wait_for_diagnostics(&mut server, &uri, 2);
    assert_eq!(cleared["params"]["diagnostics"], json!([]));

    server.send(position_request(2, "textDocument/hover", &uri, 1, 2));
    let hover = server.receive_response(2);
    assert!(
        hover["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("answer")
    );

    let output = server.finish();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "all stdout must be framed and consumed"
    );
    assert!(
        output.stderr.is_empty(),
        "clean shutdown must not report to stderr"
    );
}

#[test]
fn cancellation_and_document_changes_terminate_each_request_once() {
    let directory = TempDirectory::new();
    let uri = directory.uri("large.fsh");
    let source = (0..20_000)
        .map(|index| format!("let value_{index} = {index}\n"))
        .collect::<String>();
    let mut server = Server::start();
    server.initialize();
    server.open(&uri, 1, &source);
    let _ = wait_for_diagnostics(&mut server, &uri, 1);

    server.send(position_request(10, "textDocument/hover", &uri, 0, 2));
    server.send(json!({
        "jsonrpc": "2.0",
        "method": "$/cancelRequest",
        "params": {"id": 10}
    }));
    assert_eq!(server.receive_response(10)["error"]["code"], -32800);

    let mut references = position_request(11, "textDocument/references", &uri, 19_999, 5);
    references["params"]["context"] = json!({"includeDeclaration": true});
    server.send(references);
    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": {"uri": uri.as_str(), "version": 2},
            "contentChanges": [{"text": "let current = 1\n"}]
        }
    }));
    assert_eq!(server.receive_response(11)["error"]["code"], -32801);
    assert_eq!(server.response_count(10), 1);
    assert_eq!(server.response_count(11), 1);

    let output = server.finish();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}

#[test]
fn analysis_never_executes_effectful_source() {
    let directory = TempDirectory::new();
    let uri = directory.uri("effects.fsh");
    let marker = directory.path("must-not-exist");
    let source = format!(
        "cd '/'\nexport FLASH_LSP_EFFECT = 'forbidden'\n^/usr/bin/touch '{}'\necho forbidden > '{}'\n^/bin/sleep 1 &\n",
        marker.display(),
        marker.display()
    );
    let mut server = Server::start();
    server.initialize();
    server.open(&uri, 1, &source);
    let _ = wait_for_diagnostics(&mut server, &uri, 1);

    assert!(!Path::new(&marker).exists());
    assert!(server.finish().status.success());
    assert!(!Path::new(&marker).exists());
}
