#![forbid(unsafe_code)]

use opaal_lsp::query::{RequestControl, prepare_request};
use opaal_lsp::uri::DocumentUri;
use opaal_lsp::workspace::Workspace;
use opaal_syntax::{PositionEncoding, SourceFile, SourceId};
use serde_json::{Value, json};

fn request(
    workspace: &Workspace,
    uri: &DocumentUri,
    text: &str,
    offset: usize,
    method: &str,
) -> Value {
    let source = SourceFile::new(SourceId::new(0), "<request>", text);
    let position = source
        .text_position(offset, PositionEncoding::Utf16)
        .unwrap();
    prepare_request(
        &workspace.diagnostic_snapshot(),
        PositionEncoding::Utf16,
        RequestControl::new(),
        method,
        &json!({"textDocument": {"uri": uri.as_str()},
            "position": {"line": position.line(), "character": position.character()}}),
    )
    .finish(workspace)
    .unwrap()
}

#[test]
fn docker_aliases_use_shared_signature_nominal_fields_and_completion() {
    let root = std::env::temp_dir().join(format!("opaal-lsp-docker-{}", std::process::id()));
    let uri = DocumentUri::from_absolute_path(&root.join("main.opaal")).unwrap();
    let api = DocumentUri::from_absolute_path(&root.join("api.opaal")).unwrap();
    let text = "import './api.opaal' as api\nlet image: api::docker::Image = api::docker::decode_image({})\nlet observed: String = image.id\n";
    let mut workspace = Workspace::new();
    workspace
        .open(
            api,
            1,
            "import std::docker as docker\nexport { docker }\n".into(),
        )
        .unwrap();
    workspace.open(uri.clone(), 1, text.into()).unwrap();
    let diagnostics = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    assert!(
        diagnostics
            .documents()
            .iter()
            .all(|doc| doc.diagnostics().is_empty()),
        "{diagnostics:?}"
    );
    let signature = request(
        &workspace,
        &uri,
        text,
        text.find("{}").unwrap() + 1,
        "textDocument/signatureHelp",
    );
    assert_eq!(
        signature["signatures"][0]["label"],
        "std::docker::decode_image(document: Record) -> Image"
    );
    let hover = request(
        &workspace,
        &uri,
        text,
        text.find("decode_image").unwrap() + 2,
        "textDocument/hover",
    );
    let markdown = hover["contents"]["value"].as_str().unwrap();
    assert!(
        markdown.contains("single-image manifest Descriptor"),
        "{markdown}"
    );
    let completion = request(
        &workspace,
        &uri,
        text,
        text.find("api::docker::decode_image").unwrap() + "api::docker::".len(),
        "textDocument/completion",
    );
    let names = completion
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect::<Vec<_>>();
    for name in [
        "api::docker::decode_image",
        "api::docker::inspect_image",
        "api::docker::Image",
        "api::docker::TargetIdentity",
    ] {
        assert!(names.contains(&name), "{names:?}");
    }
    let field = request(
        &workspace,
        &uri,
        text,
        text.rfind("image.id").unwrap() + 2,
        "textDocument/hover",
    );
    assert!(field.to_string().contains("Image"), "{field}");
}
