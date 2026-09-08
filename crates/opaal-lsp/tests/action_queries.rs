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
    let text = concat!(
        "## Observe one value.\n",
        "action observe(value: String) -> String\n",
        "effects {\n",
        "    clock.wall;\n",
        "}\n",
        "{\n",
        "    return value\n",
        "}\n",
        "let result = observe('ready')\n",
    );
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
