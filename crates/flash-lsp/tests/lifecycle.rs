#![forbid(unsafe_code)]

use flash_lsp::protocol::{ExitStatus, ServerError, run};
use flash_lsp::transport::{read_frame, write_frame};
use serde_json::{Value, json};

fn framed(messages: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for message in messages {
        write_frame(&mut bytes, &serde_json::to_vec(message).unwrap()).unwrap();
    }
    bytes
}

fn responses(bytes: &[u8]) -> Vec<Value> {
    let mut input = bytes;
    let mut values = Vec::new();
    while let Some(body) = read_frame(&mut input).unwrap() {
        values.push(serde_json::from_slice(&body).unwrap());
    }
    values
}

#[test]
fn initialize_negotiates_utf8_and_advertises_only_the_first_slice() {
    let input = framed(&[
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {
                    "general": {"positionEncodings": ["utf-32", "utf-8", "utf-16"]}
                }
            }
        }),
        json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
        json!({"jsonrpc": "2.0", "method": "exit"}),
    ]);
    let mut output = Vec::new();

    assert_eq!(
        run(&mut input.as_slice(), &mut output).unwrap(),
        ExitStatus::Success
    );

    let output = responses(&output);
    assert_eq!(output.len(), 2, "notifications must not receive responses");
    assert_eq!(output[0]["id"], 1);
    assert_eq!(
        output[0]["result"]["capabilities"]["positionEncoding"],
        "utf-8"
    );
    assert_eq!(
        output[0]["result"]["capabilities"]["textDocumentSync"],
        json!({"openClose": true, "change": 1})
    );
    assert_eq!(
        output[0]["result"]["capabilities"]["completionProvider"],
        json!({"resolveProvider": false})
    );
    assert_eq!(output[0]["result"]["capabilities"]["hoverProvider"], true);
    assert_eq!(
        output[0]["result"]["capabilities"]["signatureHelpProvider"],
        json!({})
    );
    assert_eq!(
        output[0]["result"]["capabilities"]["definitionProvider"],
        true
    );
    assert_eq!(
        output[0]["result"]["capabilities"]["referencesProvider"],
        true
    );
    assert_eq!(
        output[0]["result"]["capabilities"]["documentFormattingProvider"],
        true
    );
    assert!(
        output[0]["result"]["capabilities"]
            .get("semanticTokensProvider")
            .is_none()
    );
    assert_eq!(
        output[0]["result"]["serverInfo"]["name"],
        "Flash Language Server"
    );
    assert_eq!(
        output[1],
        json!({"jsonrpc": "2.0", "id": 2, "result": null})
    );
}

#[test]
fn initialize_defaults_to_utf16_and_rejects_invalid_lifecycle_requests() {
    let input = framed(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "textDocument/hover", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"capabilities": {}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "initialize", "params": {"capabilities": {}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "flash/unknown"}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "shutdown"}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "textDocument/hover", "params": {}}),
        json!({"jsonrpc": "2.0", "method": "exit"}),
    ]);
    let mut output = Vec::new();

    assert_eq!(
        run(&mut input.as_slice(), &mut output).unwrap(),
        ExitStatus::Success
    );

    let output = responses(&output);
    assert_eq!(output[0]["error"]["code"], -32002);
    assert_eq!(
        output[1]["result"]["capabilities"]["positionEncoding"],
        "utf-16"
    );
    assert_eq!(output[2]["error"]["code"], -32600);
    assert_eq!(output[3]["error"]["code"], -32601);
    assert_eq!(output[4]["result"], Value::Null);
    assert_eq!(output[5]["error"]["code"], -32600);
}

#[test]
fn cancellation_is_a_notification_and_cancelled_responses_use_the_lsp_code() {
    let input = framed(&[
        json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": "work"}}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities": {}}}),
        json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 99}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown"}),
        json!({"jsonrpc": "2.0", "method": "exit"}),
    ]);
    let mut output = Vec::new();

    assert_eq!(
        run(&mut input.as_slice(), &mut output).unwrap(),
        ExitStatus::Success
    );
    assert_eq!(responses(&output).len(), 2);

    assert_eq!(
        flash_lsp::protocol::request_cancelled(json!(99)),
        json!({
            "jsonrpc": "2.0",
            "id": 99,
            "error": {"code": -32800, "message": "Request cancelled"}
        })
    );
}

#[test]
fn malformed_json_and_batch_messages_receive_standard_errors() {
    let mut input = Vec::new();
    write_frame(&mut input, b"{").unwrap();
    write_frame(&mut input, br#"[]"#).unwrap();
    write_frame(&mut input, br#"{"jsonrpc":"2.0","method":"exit"}"#).unwrap();
    let mut output = Vec::new();

    assert_eq!(
        run(&mut input.as_slice(), &mut output).unwrap(),
        ExitStatus::Failure
    );
    let output = responses(&output);
    assert_eq!(output[0]["id"], Value::Null);
    assert_eq!(output[0]["error"]["code"], -32700);
    assert_eq!(output[1]["error"]["code"], -32600);
}

#[test]
fn exit_before_shutdown_and_eof_are_not_successful_shutdowns() {
    let exit_input = framed(&[json!({"jsonrpc": "2.0", "method": "exit"})]);
    let mut output = Vec::new();
    assert_eq!(
        run(&mut exit_input.as_slice(), &mut output).unwrap(),
        ExitStatus::Failure
    );

    let eof_input = framed(&[json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"capabilities": {}}
    })]);
    assert_eq!(
        run(&mut eof_input.as_slice(), &mut output).unwrap(),
        ExitStatus::Failure
    );
}

#[test]
fn framing_failures_are_fatal_server_errors() {
    let mut output = Vec::new();
    assert!(matches!(
        run(&mut &b"Content-Length: 4\r\n\r\n{}"[..], &mut output),
        Err(ServerError::Frame(_))
    ));
}
