#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use opaal_lsp::transport::{read_frame, write_frame};
use opaal_lsp::uri::DocumentUri;
use serde_json::{Value, json};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const OPAAL_SERVER: &str = env!("CARGO_BIN_EXE_opaal-language-server-fixture");

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("opaal-lsp-server-{}-{serial}", std::process::id()));
        fs::create_dir(&path).expect("temporary server directory should be created");
        Self(fs::canonicalize(path).expect("temporary server directory should canonicalize"))
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

#[test]
fn framed_declaration_formatting_matches_canonical_fixture_and_is_idempotent() {
    let directory = TempDirectory::new();
    let uri = directory.uri("formatting.opaal");
    let legacy = include_str!("../../../tests/golden/source-formatting/legacy.opaal");
    let canonical = include_str!("../../../tests/golden/source-formatting/canonical.opaal");
    let mut server = Server::start();
    server.initialize();
    server.open(&uri, 1, legacy);
    server.send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "textDocument/formatting", "params": {
            "textDocument": {"uri": uri.as_str()}, "options": {"tabSize": 8, "insertSpaces": false}
        }}),
    );
    let response = server.receive_response(2);
    assert_eq!(
        response["result"].as_array().unwrap().len(),
        1,
        "{response}"
    );
    assert_eq!(response["result"][0]["newText"], canonical);
    assert_eq!(
        response["result"][0]["range"]["end"],
        json!({"line": 7, "character": 0})
    );
    server.send(json!({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
        "textDocument": {"uri": uri.as_str(), "version": 2}, "contentChanges": [{"text": canonical}]
    }}));
    server.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "textDocument/formatting", "params": {
            "textDocument": {"uri": uri.as_str()}, "options": {}
        }}),
    );
    assert_eq!(server.receive_response(3)["result"], json!([]));
    let output = server.finish();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

impl Server {
    fn start() -> Self {
        Self::start_binary(env!("CARGO_BIN_EXE_opaal-language-server"))
    }

    fn start_binary(binary: &str) -> Self {
        let mut child = Command::new(binary)
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
        let response = self.receive_response(1);
        assert_eq!(
            response["result"]["capabilities"]["positionEncoding"], "utf-8",
            "{response}"
        );
        self.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    }

    fn initialize_project(&mut self, manifest: &DocumentUri) {
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
                },
                "initializationOptions": {
                    "opaal": {"projectManifest": manifest.as_str()}
                }
            }
        }));
        let response = self.receive_response(1);
        assert_eq!(
            response["result"]["capabilities"]["positionEncoding"], "utf-8",
            "{response}"
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

#[test]
fn native_frozen_passive_lsp_case() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = TempDirectory::new();
    fs::write(
        directory.path("opaal.toml"),
        r#"schema_version = 1
[project]
name = "native_test"
root_module = "tasks.opaal"
required_opaal = ">=1.6.0,<2.0.0"
[paths]
root = "."
evidence = "output"
[tools.checker]
adapter = "native"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    )
    .unwrap();
    let marker = directory.path("launches");
    let tool = directory.path("checker");
    fs::write(
        &tool,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    for mode in ["initializer", "action", "tool"] {
        assert!(Command::new(&tool).arg(mode).status().unwrap().success());
    }
    assert_eq!(fs::read(&marker).unwrap(), b"initializer\naction\ntool\n");
    fs::remove_file(&marker).unwrap();
    let text = format!(
        "import project::tools as tools\nimport std::process as process\n^sh \"{}\" \"initializer\"\naction check() -> Bool effects {{ process.run(tools::checker); }} {{\n    let result = process::run(tools::checker, [\"action\"])\n    throw \"SENSITIVE_PASSIVE_ACTION\"\n}}\naction outer() -> Bool effects {{ process.run(tools::checker); }} {{ return check() }}\ntask check = outer\n",
        tool.display()
    );
    fs::write(directory.path("tasks.opaal"), &text).unwrap();
    let uri = directory.uri("tasks.opaal");
    let mut server = Server::start();
    server.initialize_project(&directory.uri("opaal.toml"));
    server.open(&uri, 1, &text);
    let diagnostics = wait_for_diagnostics(&mut server, &uri, 1);
    assert_eq!(
        diagnostics["params"]["diagnostics"],
        json!([]),
        "{diagnostics}"
    );
    for (id, method, line, character) in [
        (2, "textDocument/hover", 4, 27),
        (3, "textDocument/completion", 4, 26),
        (4, "textDocument/signatureHelp", 4, 33),
        (
            5,
            "textDocument/definition",
            7,
            text.lines().nth(7).unwrap().rfind("check()").unwrap() as u64 + 2,
        ),
    ] {
        server.send(position_request(id, method, &uri, line, character));
        let response = server.receive_response(id);
        assert!(
            response.get("error").is_none() && !response["result"].is_null(),
            "{response}"
        );
        if method == "textDocument/hover" {
            let hover = response["result"]["contents"]["value"].as_str().unwrap();
            assert!(
                hover.contains("ToolIdentity") && hover.contains("ToolResult"),
                "{hover}"
            );
        }
        if method == "textDocument/definition" {
            assert_eq!(response["result"]["uri"], uri.as_str());
        }
    }
    let received = serde_json::to_string(&server.received).unwrap();
    assert!(!received.contains("SENSITIVE_PASSIVE_ACTION"));
    let output = server.finish();
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{output:?}"
    );
    assert!(!marker.exists());
    assert!(!directory.path("authority.toml").exists() && !directory.path("tools.toml").exists());
    let inventory =
        include_bytes!("../../../tests/golden/controlled-external-tools/evidence-cases.json");
    let cases: Value = serde_json::from_slice(inventory).unwrap();
    let observed = json!({"launches":0,"evaluated_initializers":0,"evaluated_actions":0});
    let row = cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "passive-lsp")
        .unwrap();
    assert_eq!(observed, row["expected"]);
    let host = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => panic!("unsupported evidence host"),
    };
    println!(
        "OPAAL_EVIDENCE_CASE {}",
        json!({"name":"passive-lsp","observed":observed,
        "host":host,"inventory_digest":opaal_runtime::workflow::digest_bytes(inventory)})
    );
}

#[test]
fn opaal_protocol_launcher_accepts_directive_free_documents() {
    let directory = TempDirectory::new();
    let uri = directory.uri("main.opaal");
    let mut server = Server::start_binary(OPAAL_SERVER);
    server.initialize();
    server.open(&uri, 1, "let answer = 42\n");

    let initial = wait_for_diagnostics(&mut server, &uri, 1);
    assert_eq!(initial["params"]["diagnostics"], json!([]));

    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": {"uri": uri.as_str(), "version": 2},
            "contentChanges": [{"text": "let answer = 42\n"}]
        }
    }));
    let valid = wait_for_diagnostics(&mut server, &uri, 2);
    assert_eq!(valid["params"]["diagnostics"], json!([]));

    let output = server.finish();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
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
    let uri = directory.uri("main.opaal");
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
            "contentChanges": [{"text": "let answer = 42\nanswer\n"}]
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
    let uri = directory.uri("large.opaal");
    let source = "".to_owned()
        + &(0..20_000)
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

    let mut references = position_request(11, "textDocument/references", &uri, 20_000, 5);
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
    let uri = directory.uri("effects.opaal");
    let marker = directory.path("must-not-exist");
    let source = format!(
        "cd '/'\nexport OPAAL_LSP_EFFECT = 'forbidden'\n^/usr/bin/touch '{}'\necho forbidden > '{}'\n^/bin/sleep 1 &\n",
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

#[test]
fn explicit_project_selection_uses_saved_modules_and_editor_overlays() {
    let directory = TempDirectory::new();
    let project_root = directory.path("project");
    fs::create_dir(&project_root).unwrap();
    let manifest = directory.path("opaal.toml");
    fs::write(
        &manifest,
        concat!(
            "schema_version = 1\n",
            "[project]\n",
            "name = \"editor_demo\"\n",
            "root_module = \"tasks.opaal\"\n",
            "required_opaal = \">=1.0.0-alpha.1,<2.0.0\"\n",
            "[paths]\n",
            "root = \"project\"\n",
            "evidence = \"evidence.json\"\n",
        ),
    )
    .unwrap();
    let saved = concat!(
        "import project::context as context\n",
        "import './library.opaal' as library\n",
        "let selected_root = context::root\n",
        "let selected_value = library::answer\n",
    );
    let root = project_root.join("tasks.opaal");
    let library = project_root.join("library.opaal");
    let scratch = project_root.join("scratch.opaal");
    fs::write(&root, saved).unwrap();
    fs::write(&library, "let answer = 42\nexport { answer }\n").unwrap();
    fs::write(&scratch, "let saved = 1\n").unwrap();
    let manifest_uri = DocumentUri::from_absolute_path(&manifest).unwrap();
    let root_uri = DocumentUri::from_absolute_path(&root).unwrap();
    let library_uri = DocumentUri::from_absolute_path(&library).unwrap();
    let scratch_uri = DocumentUri::from_absolute_path(&scratch).unwrap();

    let mut server = Server::start();
    server.initialize_project(&manifest_uri);
    server.open(&root_uri, 1, saved);
    assert_eq!(
        wait_for_diagnostics(&mut server, &root_uri, 1)["params"]["diagnostics"],
        json!([])
    );
    server.send(position_request(
        2,
        "textDocument/definition",
        &root_uri,
        3,
        32,
    ));
    assert_eq!(
        server.receive_response(2)["result"]["uri"],
        library_uri.as_str()
    );
    let library_source = "let answer = 42\nexport { answer }\n";
    server.open(&library_uri, 1, library_source);
    assert_eq!(
        wait_for_diagnostics(&mut server, &library_uri, 1)["params"]["diagnostics"],
        json!([])
    );
    let mut references = position_request(4, "textDocument/references", &library_uri, 0, 6);
    references["params"]["context"] = json!({"includeDeclaration": true});
    server.send(references);
    let references = server.receive_response(4)["result"]
        .as_array()
        .cloned()
        .unwrap();
    assert!(
        references
            .iter()
            .any(|location| location["uri"] == root_uri.as_str())
    );
    assert!(
        references
            .iter()
            .any(|location| location["uri"] == library_uri.as_str())
    );
    server.open(
        &scratch_uri,
        1,
        "import project::context as context\nlet local = context::root\n",
    );
    assert_eq!(
        wait_for_diagnostics(&mut server, &scratch_uri, 1)["params"]["diagnostics"],
        json!([])
    );

    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": {"uri": root_uri.as_str(), "version": 2},
            "contentChanges": [{"text": "import project::context as context\nlet selected_root = missing\n"}]
        }
    }));
    let overlay = wait_for_diagnostics(&mut server, &root_uri, 2);
    assert!(
        overlay["params"]["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| diagnostics.iter().any(|diagnostic| {
                diagnostic["code"] == "MOD009"
                    && diagnostic["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("missing"))
            })),
        "{overlay}"
    );
    assert_eq!(fs::read_to_string(&root).unwrap(), saved);
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "initializationOptions": {
                "opaal": {"projectManifest": manifest_uri.as_str()}
            }
        }
    }));
    assert_eq!(server.receive_response(3)["error"]["code"], -32602);
    assert!(server.finish().status.success());
}

#[test]
fn project_references_survive_an_unrelated_root_error() {
    let directory = TempDirectory::new();
    let project_root = directory.path("project");
    fs::create_dir(&project_root).unwrap();
    let manifest = directory.path("opaal.toml");
    fs::write(
        &manifest,
        concat!(
            "schema_version = 1\n",
            "[project]\n",
            "name = \"editor_demo\"\n",
            "root_module = \"a.opaal\"\n",
            "required_opaal = \">=1.0.0-alpha.1,<2.0.0\"\n",
            "[paths]\n",
            "root = \"project\"\n",
            "evidence = \"evidence.json\"\n",
        ),
    )
    .unwrap();
    let root = project_root.join("a.opaal");
    let library = project_root.join("z.opaal");
    fs::write(
        &root,
        "import './z.opaal' as library\nlet broken = missing\n",
    )
    .unwrap();
    let library_source = "let answer = 42\nexport { answer }\n";
    fs::write(&library, library_source).unwrap();
    let manifest_uri = DocumentUri::from_absolute_path(&manifest).unwrap();
    let library_uri = DocumentUri::from_absolute_path(&library).unwrap();

    let mut server = Server::start();
    server.initialize_project(&manifest_uri);
    server.open(&library_uri, 1, library_source);
    assert_eq!(
        wait_for_diagnostics(&mut server, &library_uri, 1)["params"]["diagnostics"],
        json!([])
    );
    let mut references = position_request(2, "textDocument/references", &library_uri, 0, 6);
    references["params"]["context"] = json!({"includeDeclaration": true});
    server.send(references);
    assert_eq!(
        server.receive_response(2)["result"],
        json!([{
            "uri": library_uri.as_str(),
            "range": {
                "start": {"line": 0, "character": 4},
                "end": {"line": 0, "character": 10}
            }
        }, {
            "uri": library_uri.as_str(),
            "range": {
                "start": {"line": 1, "character": 9},
                "end": {"line": 1, "character": 15}
            }
        }])
    );
    assert!(server.finish().status.success());
}

#[test]
fn root_uri_never_selects_a_project_and_outside_documents_stay_standalone() {
    let directory = TempDirectory::new();
    let project_root = directory.path("project");
    fs::create_dir(&project_root).unwrap();
    let manifest = directory.path("opaal.toml");
    fs::write(
        &manifest,
        concat!(
            "schema_version = 1\n",
            "[project]\n",
            "name = \"editor_demo\"\n",
            "root_module = \"tasks.opaal\"\n",
            "required_opaal = \">=1.0.0-alpha.1,<2.0.0\"\n",
            "[paths]\n",
            "root = \"project\"\n",
            "evidence = \"evidence.json\"\n",
        ),
    )
    .unwrap();
    let root = project_root.join("tasks.opaal");
    fs::write(&root, "let saved = 1\n").unwrap();
    let outside = directory.path("outside.opaal");
    let project_source = "import project::context as context\nlet value = context::root\n";
    fs::write(&outside, project_source).unwrap();
    let root_uri = DocumentUri::from_absolute_path(&root).unwrap();
    let outside_uri = DocumentUri::from_absolute_path(&outside).unwrap();
    let manifest_uri = DocumentUri::from_absolute_path(&manifest).unwrap();

    let mut selected = Server::start();
    selected.initialize_project(&manifest_uri);
    selected.open(&outside_uri, 1, project_source);
    assert!(
        !wait_for_diagnostics(&mut selected, &outside_uri, 1)["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(selected.finish().status.success());

    let mut standalone = Server::start();
    standalone.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "rootUri": manifest_uri.as_str(),
            "workspaceFolders": [{"uri": manifest_uri.as_str(), "name": "decoy"}],
            "capabilities": {}
        }
    }));
    assert!(standalone.receive_response(1)["result"].is_object());
    standalone.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    standalone.open(&root_uri, 1, project_source);
    assert!(
        !wait_for_diagnostics(&mut standalone, &root_uri, 1)["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(standalone.finish().status.success());
}

#[test]
fn invalid_project_initialization_is_rejected_without_starting_the_server() {
    let directory = TempDirectory::new();
    let wrong_name = directory.path("project.toml");
    fs::write(&wrong_name, "schema_version = 1\n").unwrap();
    let missing = directory.path("missing/opaal.toml");
    let directory_manifest = directory.path("directory/opaal.toml");
    fs::create_dir_all(&directory_manifest).unwrap();
    let malformed = directory.path("malformed/opaal.toml");
    fs::create_dir_all(malformed.parent().unwrap()).unwrap();
    fs::write(&malformed, "schema_version = 1\n").unwrap();
    let incompatible = directory.path("incompatible/opaal.toml");
    fs::create_dir_all(incompatible.parent().unwrap()).unwrap();
    fs::write(
        &incompatible,
        concat!(
            "schema_version = 1\n",
            "[project]\n",
            "name = \"future_project\"\n",
            "root_module = \"tasks.opaal\"\n",
            "required_opaal = \">=2.0.0\"\n",
            "[paths]\n",
            "root = \".\"\n",
            "evidence = \"evidence.json\"\n",
        ),
    )
    .unwrap();
    let oversized = directory.path("oversized/opaal.toml");
    fs::create_dir_all(oversized.parent().unwrap()).unwrap();
    fs::write(&oversized, vec![b'x'; 1_048_577]).unwrap();
    let mut server = Server::start();
    let invalid = [
        json!("opaal.toml"),
        json!(42),
        json!(
            DocumentUri::from_absolute_path(&wrong_name)
                .unwrap()
                .as_str()
        ),
        json!(DocumentUri::from_absolute_path(&missing).unwrap().as_str()),
        json!(
            DocumentUri::from_absolute_path(&directory_manifest)
                .unwrap()
                .as_str()
        ),
        json!(
            DocumentUri::from_absolute_path(&malformed)
                .unwrap()
                .as_str()
        ),
        json!(
            DocumentUri::from_absolute_path(&incompatible)
                .unwrap()
                .as_str()
        ),
        json!(
            DocumentUri::from_absolute_path(&oversized)
                .unwrap()
                .as_str()
        ),
    ];
    for (offset, manifest) in invalid.into_iter().enumerate() {
        let id = i64::try_from(offset).unwrap() + 10;
        server.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "capabilities": {},
                "initializationOptions": {"opaal": {"projectManifest": manifest}}
            }
        }));
        assert_eq!(server.receive_response(id)["error"]["code"], -32602);
    }
    server.initialize();
    assert!(server.finish().status.success());
}

#[cfg(unix)]
#[test]
fn project_manifest_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;

    let directory = TempDirectory::new();
    let target = directory.path("actual.toml");
    fs::write(&target, "schema_version = 1\n").unwrap();
    let manifest = directory.path("opaal.toml");
    symlink(&target, &manifest).unwrap();
    let manifest_uri = DocumentUri::from_absolute_path(&manifest).unwrap();
    let mut server = Server::start();
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "initializationOptions": {
                "opaal": {"projectManifest": manifest_uri.as_str()}
            }
        }
    }));
    assert_eq!(server.receive_response(1)["error"]["code"], -32602);
    server.initialize();
    assert!(server.finish().status.success());
}

#[cfg(unix)]
#[test]
fn project_source_symlinks_cannot_become_editor_overlays() {
    use std::os::unix::fs::symlink;

    let directory = TempDirectory::new();
    let project_root = directory.path("project");
    fs::create_dir(&project_root).unwrap();
    let manifest = directory.path("opaal.toml");
    fs::write(
        &manifest,
        concat!(
            "schema_version = 1\n",
            "[project]\n",
            "name = \"editor_demo\"\n",
            "root_module = \"tasks.opaal\"\n",
            "required_opaal = \">=1.0.0-alpha.1,<2.0.0\"\n",
            "[paths]\n",
            "root = \"project\"\n",
            "evidence = \"evidence.json\"\n",
        ),
    )
    .unwrap();
    let target = directory.path("outside.opaal");
    fs::write(&target, "let outside = 1\n").unwrap();
    let root = project_root.join("tasks.opaal");
    symlink(&target, &root).unwrap();
    let manifest_uri = DocumentUri::from_absolute_path(&manifest).unwrap();
    let root_uri = DocumentUri::from_absolute_path(&root).unwrap();
    let mut server = Server::start();
    server.initialize_project(&manifest_uri);
    server.open(&root_uri, 1, "let overlaid = 2\n");
    assert!(
        !wait_for_diagnostics(&mut server, &root_uri, 1)["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(server.finish().status.success());
}
