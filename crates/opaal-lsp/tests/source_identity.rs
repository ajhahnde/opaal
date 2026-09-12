#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use opaal_lsp::query::{RequestControl, prepare_request};
use opaal_lsp::uri::DocumentUri;
use opaal_lsp::workspace::Workspace;
use opaal_syntax::{PositionEncoding, SourceFile, SourceId};
use serde_json::{Value, json};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "opaal-lsp-source-identity-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn uri(&self, name: &str) -> DocumentUri {
        DocumentUri::from_absolute_path(&self.path(name)).unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn open_opaal_imports_accept_directive_free_overlays() {
    let directory = TestDirectory::new();
    fs::write(directory.path("main.opaal"), "").unwrap();
    let root_uri = directory.uri("main.opaal");
    let library_uri = directory.uri("library.opaal");
    let mut workspace = Workspace::new();
    workspace
        .open(root_uri, 1, "import './library.opaal' as library\n".into())
        .unwrap();
    workspace
        .open(library_uri.clone(), 1, "let value = 1\n".into())
        .unwrap();

    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    assert!(workspace.document(&library_uri).is_some());
    assert!(
        analysis
            .documents()
            .iter()
            .all(|document| document.diagnostics().is_empty())
    );
}

#[test]
fn opaal_document_formatting_never_inserts_a_source_header() {
    let directory = TestDirectory::new();
    fs::write(directory.path("main.opaal"), "").unwrap();
    let uri = directory.uri("main.opaal");
    let mut workspace = Workspace::new();
    workspace
        .open(uri.clone(), 1, "let answer =  { value:42 }\n".into())
        .unwrap();
    let snapshot = workspace.diagnostic_snapshot();
    let response = prepare_request(
        &snapshot,
        PositionEncoding::Utf16,
        RequestControl::new(),
        "textDocument/formatting",
        &json!({"textDocument": {"uri": uri.as_str()}, "options": {}}),
    )
    .finish(&workspace)
    .unwrap();

    assert_eq!(response[0]["newText"], "let answer = { value:42 }\n");
}

#[test]
fn the_opaal_protocol_workspace_analyzes_directive_free_documents() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.opaal");
    let mut workspace = Workspace::new();

    workspace
        .open(uri.clone(), 1, "let answer = 42\n".into())
        .unwrap();

    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    assert!(workspace.document(&uri).is_some());
    assert!(
        analysis
            .documents()
            .iter()
            .all(|document| document.diagnostics().is_empty())
    );
}

#[test]
fn opaal_alias_reexport_and_nominal_type_queries_share_identity_and_provenance() {
    let directory = TestDirectory::new();
    fs::write(directory.path("main.opaal"), "").unwrap();
    let root_uri = directory.uri("main.opaal");
    let facade_uri = directory.uri("facade.opaal");
    let model_uri = directory.uri("model.opaal");
    let root = concat!(
        "import './facade.opaal' as api\n",
        "let item: api::model::Item = api::model::Item { value: 1 }\n",
        "let api::model::Item { value: selected } = item\n",
        "export { api }\n",
    );
    let facade = concat!("import './model.opaal' as model\n", "export { model }\n",);
    let model = "type Item = { value: Int, }\n\nexport { Item }\n";
    let mut workspace = Workspace::new();
    workspace.open(root_uri.clone(), 1, root.into()).unwrap();
    workspace
        .open(facade_uri.clone(), 1, facade.into())
        .unwrap();
    workspace.open(model_uri.clone(), 1, model.into()).unwrap();

    let import_cursor = root.find("api").unwrap() + 1;
    let export_cursor = root.rfind("api").unwrap() + 1;
    let import_hover = lsp_request(
        &workspace,
        "textDocument/hover",
        positional(&root_uri, root, import_cursor),
    );
    let export_hover = lsp_request(
        &workspace,
        "textDocument/hover",
        positional(&root_uri, root, export_cursor),
    );
    let import_markdown = import_hover["contents"]["value"].as_str().unwrap();
    let export_markdown = export_hover["contents"]["value"].as_str().unwrap();
    assert!(
        import_markdown.contains("module `api`"),
        "{import_markdown}"
    );
    assert!(
        import_markdown.contains("facade.opaal"),
        "{import_markdown}"
    );
    assert!(import_markdown.contains("local"), "{import_markdown}");
    assert_eq!(export_markdown, import_markdown);

    let definition = lsp_request(
        &workspace,
        "textDocument/definition",
        positional(&root_uri, root, export_cursor),
    );
    assert_eq!(definition["uri"], root_uri.as_str());
    assert_eq!(
        definition["range"]["start"],
        json!({"line": 0, "character": 27})
    );

    let item_cursor = model.find("Item").unwrap() + 1;
    let item_hover = lsp_request(
        &workspace,
        "textDocument/hover",
        positional(&model_uri, model, item_cursor),
    );
    let item_markdown = item_hover["contents"]["value"].as_str().unwrap();
    assert!(item_markdown.contains("type `Item`"), "{item_markdown}");
    assert!(
        item_markdown.contains("Nominal identity"),
        "{item_markdown}"
    );
    assert!(item_markdown.contains("model.opaal"), "{item_markdown}");

    for needle in [
        "api::model::Item { value: 1 }",
        "api::model::Item { value: selected }",
    ] {
        let cursor = root.find(needle).unwrap() + needle.find("Item").unwrap() + 1;
        let hover = lsp_request(
            &workspace,
            "textDocument/hover",
            positional(&root_uri, root, cursor),
        );
        let markdown = hover["contents"]["value"]
            .as_str()
            .unwrap_or_else(|| panic!("{needle:?} returned {hover}"));
        assert!(markdown.contains("type `Item`"), "{markdown}");
        let definition = lsp_request(
            &workspace,
            "textDocument/definition",
            positional(&root_uri, root, cursor),
        );
        assert_eq!(definition["uri"], model_uri.as_str());
        assert_eq!(
            definition["range"]["start"],
            json!({"line": 0, "character": 5})
        );
    }
}

#[test]
fn opaal_workflow_operations_share_completion_hover_and_signature() {
    let directory = TestDirectory::new();
    fs::create_dir(directory.path("support")).unwrap();
    let root = include_str!("../../../tests/opaal-foundation/workflow/workspace/root.opaal");
    let facade =
        include_str!("../../../tests/opaal-foundation/workflow/workspace/support/facade.opaal");
    fs::write(directory.path("root.opaal"), root).unwrap();
    fs::write(directory.path("support/facade.opaal"), facade).unwrap();
    let root_uri = directory.uri("root.opaal");
    let facade_uri = directory.uri("support/facade.opaal");
    let mut workspace = Workspace::new();
    workspace.open(root_uri.clone(), 1, root.into()).unwrap();
    workspace.open(facade_uri, 1, facade.into()).unwrap();

    let direct = root.find("value::length").unwrap();
    let completion = lsp_request(
        &workspace,
        "textDocument/completion",
        positional(&root_uri, root, direct + "value::le".len()),
    );
    assert!(completion.as_array().unwrap().iter().any(|candidate| {
        candidate["label"] == "value::length" && candidate["textEdit"]["newText"] == "value::length"
    }));

    let reexported = root.find("api::value::length").unwrap();
    let completion = lsp_request(
        &workspace,
        "textDocument/completion",
        positional(&root_uri, root, reexported + "api::value::le".len()),
    );
    assert!(completion.as_array().unwrap().iter().any(|candidate| {
        candidate["label"] == "api::value::length"
            && candidate["textEdit"]["newText"] == "api::value::length"
    }));

    let direct_hover = lsp_request(
        &workspace,
        "textDocument/hover",
        positional(&root_uri, root, direct + "value::".len() + 2),
    );
    let reexported_hover = lsp_request(
        &workspace,
        "textDocument/hover",
        positional(&root_uri, root, reexported + "api::value::".len() + 2),
    );
    assert_eq!(direct_hover, reexported_hover);
    let hover = direct_hover["contents"]["value"].as_str().unwrap();
    assert!(
        hover.contains("Canonical identity: `std::value::length`"),
        "{hover}"
    );
    assert!(
        hover.contains("std::value::length[T](input: List[T]) -> Int"),
        "{hover}"
    );

    let argument = root[direct..].find("\"one\"").unwrap() + direct + 2;
    let signature = lsp_request(
        &workspace,
        "textDocument/signatureHelp",
        positional(&root_uri, root, argument),
    );
    assert_eq!(
        signature["signatures"][0]["label"],
        "std::value::length[T](input: List[T]) -> Int"
    );
    assert_eq!(signature["activeParameter"], 0);

    let formatting = lsp_request(
        &workspace,
        "textDocument/formatting",
        json!({"textDocument": {"uri": root_uri.as_str()}, "options": {}}),
    );
    assert_eq!(formatting, json!([]));

    let prepared = prepare_request(
        &workspace.diagnostic_snapshot(),
        PositionEncoding::Utf16,
        RequestControl::new(),
        "textDocument/hover",
        &positional(&root_uri, root, direct + "value::".len() + 2),
    );
    workspace
        .change(&root_uri, Some(2), root.replace("direct()", "reexported()"))
        .unwrap();
    assert_eq!(
        prepared.finish(&workspace),
        Err(opaal_lsp::query::RequestError::ContentModified)
    );
}

fn positional(uri: &DocumentUri, text: &str, offset: usize) -> Value {
    let source = SourceFile::new(SourceId::new(999), "<request>", text);
    let position = source
        .text_position(offset, PositionEncoding::Utf16)
        .unwrap();
    json!({
        "textDocument": {"uri": uri.as_str()},
        "position": {"line": position.line(), "character": position.character()}
    })
}

fn lsp_request(workspace: &Workspace, method: &str, params: Value) -> Value {
    prepare_request(
        &workspace.diagnostic_snapshot(),
        PositionEncoding::Utf16,
        RequestControl::new(),
        method,
        &params,
    )
    .finish(workspace)
    .unwrap()
}
