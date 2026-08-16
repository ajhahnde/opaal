#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use flash_lsp::protocol::request_failure_response;
use flash_lsp::query::{RequestControl, RequestError, prepare_request};
use flash_lsp::uri::DocumentUri;
use flash_lsp::workspace::Workspace;
use flash_syntax::{PositionEncoding, SourceFile, SourceId};
use serde_json::{Value, json};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("flash-lsp-queries-{}-{serial}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn uri(&self, name: &str) -> DocumentUri {
        DocumentUri::from_absolute_path(&self.0.join(name)).unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn position(text: &str, offset: usize, encoding: PositionEncoding) -> Value {
    let source = SourceFile::new(SourceId::new(0), "<request>", text);
    let position = source.text_position(offset, encoding).unwrap();
    json!({"line": position.line(), "character": position.character()})
}

fn positional(uri: &DocumentUri, text: &str, offset: usize, encoding: PositionEncoding) -> Value {
    json!({
        "textDocument": {"uri": uri.as_str()},
        "position": position(text, offset, encoding)
    })
}

fn request(
    workspace: &Workspace,
    encoding: PositionEncoding,
    control: &RequestControl,
    method: &str,
    params: Value,
) -> Result<Value, RequestError> {
    prepare_request(
        &workspace.diagnostic_snapshot(),
        encoding,
        control.clone(),
        method,
        &params,
    )
    .finish(workspace)
}

#[test]
fn completion_is_deterministic_and_retains_registry_results_without_a_program() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.fsh");
    let text = "pw\nif true {";
    let mut workspace = Workspace::new();
    workspace.open(uri.clone(), 1, text.into()).unwrap();

    let result = request(
        &workspace,
        PositionEncoding::Utf16,
        &RequestControl::new(),
        "textDocument/completion",
        positional(&uri, text, 2, PositionEncoding::Utf16),
    )
    .unwrap();

    assert_eq!(result.as_array().unwrap().len(), 1);
    assert_eq!(result[0]["label"], "pwd");
    assert_eq!(result[0]["sortText"], "0:pwd");
    assert_eq!(result[0]["textEdit"]["newText"], "pwd");
    assert_eq!(
        result[0]["textEdit"]["range"]["start"],
        json!({"line": 0, "character": 0})
    );
    assert_eq!(
        result[0]["textEdit"]["range"]["end"],
        json!({"line": 0, "character": 2})
    );
}

#[test]
fn semantic_completion_hover_and_signature_help_use_shared_program_data() {
    let directory = TestDirectory::new();
    let root_uri = directory.uri("main.fsh");
    let library_uri = directory.uri("library.fsh");
    let root = concat!(
        "import { greet } from './library.fsh'\n",
        "let message = greet('world')\n",
        "let copied = $greet\n",
        "let whole = int(3.9)\n",
        "let home = env('HOME')\n",
        "let files = glob('*.fsh')\n",
        "let latest = $status\n",
        "pwd\n",
        "kill --kill 1\n",
    );
    let library = concat!(
        "## Imported greeting\n",
        "def greet(name: String) -> String { $name }\n",
        "export { greet }\n",
        "cd '/tmp'\n",
    );
    let mut workspace = Workspace::new();
    workspace.open(root_uri.clone(), 4, root.into()).unwrap();
    workspace.open(library_uri, 7, library.into()).unwrap();
    let control = RequestControl::new();

    let variable_cursor = root.find("$greet").unwrap() + 3;
    let completion = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/completion",
        positional(&root_uri, root, variable_cursor, PositionEncoding::Utf16),
    )
    .unwrap();
    assert_eq!(completion[0]["label"], "$greet");
    assert_eq!(completion[0]["sortText"], "1:greet");

    let call = root.find("greet('world')").unwrap() + 2;
    let hover = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, call, PositionEncoding::Utf16),
    )
    .unwrap();
    let hover_text = hover["contents"]["value"].as_str().unwrap();
    assert!(hover_text.contains("def greet(name: String) -> String"));
    assert!(hover_text.contains("Imported greeting"));

    let argument = root.find("'world'").unwrap() + 2;
    let signature = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/signatureHelp",
        positional(&root_uri, root, argument, PositionEncoding::Utf16),
    )
    .unwrap();
    assert_eq!(
        signature["signatures"][0]["label"],
        "greet(name: String) -> String"
    );
    assert_eq!(signature["activeParameter"], 0);

    let intrinsic_call = root.find("int(3.9)").unwrap() + 1;
    let intrinsic_completion = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/completion",
        positional(&root_uri, root, intrinsic_call + 1, PositionEncoding::Utf16),
    )
    .unwrap();
    assert!(
        intrinsic_completion
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["label"] == "int")
    );
    let intrinsic_hover = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, intrinsic_call, PositionEncoding::Utf16),
    )
    .unwrap();
    let intrinsic_hover = intrinsic_hover["contents"]["value"].as_str().unwrap();
    assert!(
        intrinsic_hover.contains("int(value: Int | Float) -> Int"),
        "{intrinsic_hover}"
    );

    let intrinsic_argument = root.find("3.9").unwrap() + 1;
    let intrinsic_signature = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/signatureHelp",
        positional(&root_uri, root, intrinsic_argument, PositionEncoding::Utf16),
    )
    .unwrap();
    assert_eq!(
        intrinsic_signature["signatures"][0]["label"],
        "int(value: Int | Float) -> Int"
    );
    assert_eq!(intrinsic_signature["activeParameter"], 0);

    let env_call = root.find("env('HOME')").unwrap() + 1;
    let env_hover = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, env_call, PositionEncoding::Utf16),
    )
    .unwrap();
    assert!(
        env_hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("env(name: String) -> Any")
    );

    let glob_call = root.find("glob('*.fsh')").unwrap() + 1;
    let glob_hover = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, glob_call, PositionEncoding::Utf16),
    )
    .unwrap();
    assert!(
        glob_hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("glob(pattern: String | Path) -> List[Path]")
    );
    let glob_argument = root.find("'*.fsh'").unwrap() + 2;
    let glob_signature = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/signatureHelp",
        positional(&root_uri, root, glob_argument, PositionEncoding::Utf16),
    )
    .unwrap();
    assert_eq!(
        glob_signature["signatures"][0]["label"],
        "glob(pattern: String | Path) -> List[Path]"
    );

    let status_read = root.find("$status").unwrap() + 2;
    let status_hover = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, status_read, PositionEncoding::Utf16),
    )
    .unwrap();
    assert!(
        status_hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("dynamic $status: Any")
    );

    let command = root.find("pwd\n").unwrap() + 1;
    let command_hover = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, command, PositionEncoding::Utf16),
    )
    .unwrap();
    assert!(
        command_hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("pwd")
    );

    let flag_cursor = root.find("--kill").unwrap() + 4;
    let flags = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/completion",
        positional(&root_uri, root, flag_cursor, PositionEncoding::Utf16),
    )
    .unwrap();
    assert_eq!(flags[0]["label"], "--kill");
    assert_eq!(flags[0]["sortText"], "2:--kill");

    let import_name = root.find("greet").unwrap() + 2;
    let effects = request(
        &workspace,
        PositionEncoding::Utf16,
        &control,
        "textDocument/hover",
        positional(&root_uri, root, import_name, PositionEncoding::Utf16),
    )
    .unwrap();
    let effects = effects["contents"]["value"].as_str().unwrap();
    assert!(effects.contains("Module initializer effects"));
    assert!(effects.contains("working directory"));
}

#[test]
fn definition_and_references_project_canonical_cross_file_locations() {
    let directory = TestDirectory::new();
    let root_uri = directory.uri("main.fsh");
    let library_uri = directory.uri("library.fsh");
    let root = concat!(
        "import { greet } from './library.fsh'\n",
        "let message = greet('world')\n",
    );
    let library = concat!(
        "def greet(name: String) -> String { $name }\n",
        "export { greet }\n",
    );
    let mut workspace = Workspace::new();
    workspace.open(root_uri.clone(), 1, root.into()).unwrap();
    workspace
        .open(library_uri.clone(), 2, library.into())
        .unwrap();
    let call = root.rfind("greet").unwrap() + 2;
    let control = RequestControl::new();

    let definition = request(
        &workspace,
        PositionEncoding::Utf8,
        &control,
        "textDocument/definition",
        positional(&root_uri, root, call, PositionEncoding::Utf8),
    )
    .unwrap();
    assert_eq!(definition["uri"], library_uri.as_str());
    assert_eq!(
        definition["range"]["start"],
        json!({"line": 0, "character": 4})
    );
    assert_eq!(
        definition["range"]["end"],
        json!({"line": 0, "character": 9})
    );

    let mut params = positional(&root_uri, root, call, PositionEncoding::Utf8);
    params["context"] = json!({"includeDeclaration": true});
    let references = request(
        &workspace,
        PositionEncoding::Utf8,
        &control,
        "textDocument/references",
        params,
    )
    .unwrap();
    let references = references.as_array().unwrap();
    assert_eq!(references.len(), 4);
    assert_eq!(references[0]["uri"], root_uri.as_str());
    assert_eq!(references[2]["uri"], library_uri.as_str());

    let declaration = library.find("greet").unwrap() + 2;
    let mut params = positional(&library_uri, library, declaration, PositionEncoding::Utf8);
    params["context"] = json!({"includeDeclaration": true});
    let from_dependency_root = request(
        &workspace,
        PositionEncoding::Utf8,
        &control,
        "textDocument/references",
        params,
    )
    .unwrap();
    assert_eq!(
        from_dependency_root.as_array().unwrap().len(),
        4,
        "all open-root programs contribute current cross-file references"
    );
}

#[test]
fn formatting_returns_zero_or_one_full_document_edit() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.fsh");
    let text = "if true {\nlet value=1\n}\n";
    let mut workspace = Workspace::new();
    workspace.open(uri.clone(), 1, text.into()).unwrap();
    let params = json!({
        "textDocument": {"uri": uri.as_str()},
        "options": {"tabSize": 8, "insertSpaces": false}
    });

    let edits = request(
        &workspace,
        PositionEncoding::Utf16,
        &RequestControl::new(),
        "textDocument/formatting",
        params,
    )
    .unwrap();
    assert_eq!(edits.as_array().unwrap().len(), 1);
    assert_eq!(edits[0]["newText"], "if true {\n    let value=1\n}\n");
    assert_eq!(
        edits[0]["range"]["start"],
        json!({"line": 0, "character": 0})
    );
    assert_eq!(edits[0]["range"]["end"], json!({"line": 3, "character": 0}));

    workspace
        .change(&uri, Some(2), "if true {\n    let value=1\n}\n".into())
        .unwrap();
    let unchanged = request(
        &workspace,
        PositionEncoding::Utf16,
        &RequestControl::new(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri.as_str()}, "options": {}}),
    )
    .unwrap();
    assert_eq!(unchanged, json!([]));

    workspace.change(&uri, Some(3), "if true {".into()).unwrap();
    let incomplete = request(
        &workspace,
        PositionEncoding::Utf16,
        &RequestControl::new(),
        "textDocument/formatting",
        json!({"textDocument": {"uri": uri.as_str()}, "options": {}}),
    )
    .unwrap();
    assert_eq!(incomplete, json!([]));
}

#[test]
fn invalid_positions_fail_while_unopened_or_dynamic_targets_use_standard_empty_shapes() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.fsh");
    let unopened = directory.uri("closed.fsh");
    let text = "let value = 1\n";
    let mut workspace = Workspace::new();
    workspace.open(uri.clone(), 1, text.into()).unwrap();
    let control = RequestControl::new();

    assert_eq!(
        request(
            &workspace,
            PositionEncoding::Utf16,
            &control,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": uri.as_str()},
                "position": {"line": 0, "character": 999}
            }),
        ),
        Err(RequestError::InvalidParams)
    );
    assert_eq!(
        request(
            &workspace,
            PositionEncoding::Utf16,
            &control,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": unopened.as_str()},
                "position": {"line": 0, "character": 0}
            }),
        )
        .unwrap(),
        Value::Null
    );
    assert_eq!(
        request(
            &workspace,
            PositionEncoding::Utf16,
            &control,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": "untitled:buffer"},
                "position": {"line": 0, "character": 0}
            }),
        )
        .unwrap(),
        Value::Null
    );
    assert_eq!(
        request(
            &workspace,
            PositionEncoding::Utf16,
            &control,
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri.as_str()},
                "position": {"line": 0, "character": 0},
                "context": {"includeDeclaration": false}
            }),
        )
        .unwrap(),
        json!([])
    );
}

#[test]
fn explicit_cancellation_wins_and_generation_changes_return_content_modified() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.fsh");
    let text = "let value = 1\n";
    let mut workspace = Workspace::new();
    workspace.open(uri.clone(), 1, text.into()).unwrap();

    let cancelled = RequestControl::new();
    cancelled.cancel();
    assert_eq!(
        request(
            &workspace,
            PositionEncoding::Utf16,
            &cancelled,
            "textDocument/hover",
            positional(&uri, text, 4, PositionEncoding::Utf16),
        ),
        Err(RequestError::RequestCancelled)
    );

    let control = RequestControl::new();
    let prepared = prepare_request(
        &workspace.diagnostic_snapshot(),
        PositionEncoding::Utf16,
        control.clone(),
        "textDocument/hover",
        &positional(&uri, text, 4, PositionEncoding::Utf16),
    );
    workspace
        .change(&uri, Some(2), "let value = 2\n".into())
        .unwrap();
    assert_eq!(
        prepared.finish(&workspace),
        Err(RequestError::ContentModified)
    );

    control.cancel();
    assert_eq!(
        prepared.finish(&workspace),
        Err(RequestError::RequestCancelled)
    );

    assert_eq!(
        request_failure_response(json!("stale"), RequestError::ContentModified),
        json!({
            "jsonrpc": "2.0",
            "id": "stale",
            "error": {"code": -32801, "message": "Content modified"}
        })
    );
    assert_eq!(
        request_failure_response(json!(9), RequestError::InvalidParams),
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "error": {"code": -32602, "message": "Invalid params"}
        })
    );
}
