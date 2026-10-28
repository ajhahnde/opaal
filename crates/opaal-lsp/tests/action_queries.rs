#![forbid(unsafe_code)]

use std::fs;

use opaal_lsp::query::{RequestControl, prepare_request};
use opaal_lsp::uri::DocumentUri;
use opaal_lsp::workspace::Workspace;
use opaal_syntax::{PositionEncoding, SourceFile, SourceId};
use serde_json::{Value, json};

fn position(text: &str, offset: usize) -> Value {
    let source = SourceFile::new(SourceId::new(0), "<request>", text);
    let position = source
        .text_position(offset, PositionEncoding::Utf16)
        .unwrap();
    json!({"line": position.line(), "character": position.character()})
}

fn request(
    workspace: &Workspace,
    method: &str,
    uri: &DocumentUri,
    text: &str,
    offset: usize,
) -> Value {
    prepare_request(
        &workspace.diagnostic_snapshot(),
        PositionEncoding::Utf16,
        RequestControl::new(),
        method,
        &json!({
            "textDocument": {"uri": uri.as_str()},
            "position": position(text, offset),
        }),
    )
    .finish(workspace)
    .unwrap()
}

#[test]
fn action_identity_signature_effects_and_definition_are_shared() {
    let directory =
        std::env::temp_dir().join(format!("opaal-lsp-action-queries-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let uri = DocumentUri::from_absolute_path(&directory.join("main.opaal")).unwrap();
    let text = "## Observe one value.\naction observe(value: String) -> String\n{\neffects {\n    clock.wall,\n}\n\n    return value\n}\nlet result = observe('ready')\n";
    let mut workspace = Workspace::new();
    workspace.open(uri.clone(), 1, text.to_owned()).unwrap();
    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    assert!(
        analysis
            .documents()
            .iter()
            .all(|document| document.diagnostics().is_empty()),
        "{analysis:#?}"
    );

    let call = text.rfind("observe('ready')").unwrap() + 2;
    let hover = request(&workspace, "textDocument/hover", &uri, text, call);
    let markdown = hover["contents"]["value"].as_str().unwrap();
    assert!(
        markdown.contains("action observe(value: String) -> String"),
        "{markdown}"
    );
    assert!(markdown.contains("Declared effects:"), "{markdown}");
    assert!(markdown.contains("`clock.wall`"), "{markdown}");
    assert!(
        markdown.contains("Canonical action identity:"),
        "{markdown}"
    );
    let digest = markdown
        .split("Contract digest: `sha256:")
        .nth(1)
        .and_then(|tail| tail.split('`').next())
        .expect("hover exposes the shared action digest");
    assert_eq!(digest.len(), 64);
    assert!(markdown.contains("Observe one value."), "{markdown}");

    let argument = text.rfind("'ready'").unwrap() + 2;
    let signature = request(
        &workspace,
        "textDocument/signatureHelp",
        &uri,
        text,
        argument,
    );
    assert_eq!(
        signature["signatures"][0]["label"],
        "action observe(value: String) -> String"
    );

    let definition = request(&workspace, "textDocument/definition", &uri, text, call);
    assert_eq!(definition["uri"], uri.as_str());
    assert_eq!(
        definition["range"]["start"],
        json!({"line": 1, "character": 7})
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn native_project_uses_shared_process_policy_and_effect_observers_without_a_lock() {
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("opaal-lsp-native-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join("opaal.toml"),
        include_str!("../../../examples/repository-check/live.toml"),
    )
    .unwrap();
    fs::write(
        directory.join("policy.opaal"),
        include_str!("../../../examples/repository-check/policy.opaal"),
    )
    .unwrap();
    let text = include_str!("../../../examples/repository-check/tasks.opaal");
    fs::write(directory.join("tasks.opaal"), text).unwrap();
    let uri = DocumentUri::from_absolute_path(&directory.join("tasks.opaal")).unwrap();
    let mut workspace = Workspace::new();
    workspace
        .select_project(&DocumentUri::from_absolute_path(&directory.join("opaal.toml")).unwrap())
        .unwrap();
    workspace.open(uri.clone(), 1, text.to_owned()).unwrap();
    let diagnostics = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    assert!(
        diagnostics
            .documents()
            .iter()
            .all(|document| document.diagnostics().is_empty()),
        "{diagnostics:?}"
    );
    let hover = request(
        &workspace,
        "textDocument/hover",
        &uri,
        text,
        text.find("process::run(").unwrap() + "process::".len() + 1,
    );
    let markdown = hover["contents"]["value"].as_str().unwrap();
    for expected in ["def run(", "ToolIdentity", "List[String]", "ToolResult"] {
        assert!(markdown.contains(expected), "{markdown}");
    }
    let completion = request(
        &workspace,
        "textDocument/completion",
        &uri,
        text,
        text.find("process::run(").unwrap() + "process::".len(),
    );
    let names = completion
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"process::run") && names.contains(&"process::ToolResult"));
    let signature = request(
        &workspace,
        "textDocument/signatureHelp",
        &uri,
        text,
        text.find("[\"--norc\"").unwrap() + 2,
    );
    let label = signature["signatures"][0]["label"].as_str().unwrap();
    assert!(
        label.contains("ToolIdentity") && label.contains("ToolResult"),
        "{label}"
    );
    let action = request(
        &workspace,
        "textDocument/hover",
        &uri,
        text,
        text.find("check_repository(").unwrap() + 2,
    );
    let action = action["contents"]["value"].as_str().unwrap();
    for expected in ["process.run", "filesystem.read", "stdout.write"] {
        assert!(action.contains(expected), "{action}");
    }
    let definition = request(
        &workspace,
        "textDocument/definition",
        &uri,
        text,
        text.find("policy::checked").unwrap() + "policy::".len() + 1,
    );
    assert_eq!(
        definition["uri"],
        DocumentUri::from_absolute_path(&directory.join("policy.opaal"))
            .unwrap()
            .as_str()
    );
    assert!(!directory.join("live-tools.toml").exists());
    assert!(!directory.join("live-authority.toml").exists());
    let edits = prepare_request(
        &workspace.diagnostic_snapshot(),
        PositionEncoding::Utf16,
        RequestControl::new(),
        "textDocument/formatting",
        &json!({"textDocument": {"uri": uri.as_str()},
                "options": {"tabSize": 4, "insertSpaces": true}}),
    )
    .finish(&workspace)
    .unwrap();
    assert_eq!(edits, json!([]));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn contained_action_errors_keep_original_unicode_ranges_and_refuse_formatting() {
    for text in [
        "action old(value: String) -> String effects {} {}\n",
        "action duplicate(value: String) -> String { effects {} let label = '界🙂'\neffects {} }\n",
        "action semi(value: String) -> String { effects { filesystem.read('界🙂'); } }\n",
    ] {
        let directory = std::env::temp_dir();
        let uri =
            DocumentUri::from_absolute_path(&directory.join("opaal-action-error.opaal")).unwrap();
        let mut workspace = Workspace::new();
        workspace.open(uri.clone(), 1, text.to_owned()).unwrap();
        let result = workspace
            .diagnostic_snapshot()
            .analyze_diagnostics(PositionEncoding::Utf16)
            .unwrap();
        let diagnostic = &result.documents()[0].diagnostics()[0];
        assert_eq!(diagnostic.code(), Some("OP1000"));
        let offset = if text.contains(';') {
            text.find(';').unwrap()
        } else {
            text.rfind("effects").unwrap()
        };
        let file = SourceFile::new(SourceId::new(0), "error", text);
        let expected = file.text_position(offset, PositionEncoding::Utf16).unwrap();
        assert_eq!(diagnostic.range().start(), expected);
        let edits = prepare_request(
            &workspace.diagnostic_snapshot(),
            PositionEncoding::Utf16,
            RequestControl::new(),
            "textDocument/formatting",
            &json!({"textDocument": {"uri": uri.as_str()},
            "options": {"tabSize": 4, "insertSpaces": true}}),
        )
        .finish(&workspace)
        .unwrap();
        assert_eq!(edits, json!([]));
    }
}
