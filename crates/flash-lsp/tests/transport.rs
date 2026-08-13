#![forbid(unsafe_code)]

use flash_lsp::transport::{FrameError, read_frame, write_frame};

#[test]
fn framing_is_byte_exact_and_counts_utf8_body_bytes() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":"Flash ⚡"}"#.as_bytes();
    let mut framed = Vec::new();

    write_frame(&mut framed, body).expect("the response frame should be writable");

    assert_eq!(
        framed,
        [
            format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes(),
            body
        ]
        .concat()
    );
    let mut input = framed.as_slice();
    assert_eq!(read_frame(&mut input).unwrap(), Some(body.to_vec()));
    assert_eq!(read_frame(&mut input).unwrap(), None);
}

#[test]
fn framing_rejects_missing_malformed_and_duplicate_lengths() {
    let cases: &[(&[u8], FrameError)] = &[
        (
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            FrameError::MissingContentLength,
        ),
        (
            b"Content-Length: nope\r\n\r\n{}",
            FrameError::InvalidContentLength,
        ),
        (
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            FrameError::DuplicateContentLength,
        ),
        (b"Broken header\r\n\r\n{}", FrameError::InvalidHeader),
    ];

    for (input, expected) in cases {
        assert_eq!(read_frame(&mut &input[..]), Err(*expected));
    }
}

#[test]
fn framing_rejects_unsupported_encodings_and_truncated_bodies() {
    let unsupported = concat!(
        "Content-Length: 2\r\n",
        "Content-Type: application/vscode-jsonrpc; charset=iso-8859-1\r\n",
        "\r\n{}"
    );
    assert_eq!(
        read_frame(&mut unsupported.as_bytes()),
        Err(FrameError::UnsupportedEncoding)
    );
    assert_eq!(
        read_frame(&mut &b"Content-Length: 3\r\n\r\n{}"[..]),
        Err(FrameError::TruncatedBody)
    );
}
