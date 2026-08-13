//! Content-Length framing for Language Server Protocol streams.

use std::fmt;
use std::io::{self, BufRead, Write};

/// A malformed or incomplete protocol frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    Io,
    InvalidHeader,
    MissingContentLength,
    DuplicateContentLength,
    InvalidContentLength,
    UnsupportedEncoding,
    TruncatedHeader,
    TruncatedBody,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Io => "protocol input failed",
            Self::InvalidHeader => "protocol frame contains an invalid header",
            Self::MissingContentLength => "protocol frame has no Content-Length header",
            Self::DuplicateContentLength => "protocol frame repeats Content-Length",
            Self::InvalidContentLength => "protocol frame has an invalid Content-Length",
            Self::UnsupportedEncoding => "protocol frame uses an unsupported content encoding",
            Self::TruncatedHeader => "protocol frame header is truncated",
            Self::TruncatedBody => "protocol frame body is truncated",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for FrameError {}

/// Reads one frame body, or `None` for clean end-of-input between frames.
pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, FrameError> {
    let mut content_length = None;
    let mut saw_header = false;

    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|_| FrameError::Io)?;
        if read == 0 {
            return if saw_header {
                Err(FrameError::TruncatedHeader)
            } else {
                Ok(None)
            };
        }
        saw_header = true;
        if !line.ends_with(b"\r\n") {
            return Err(FrameError::InvalidHeader);
        }
        line.truncate(line.len() - 2);
        if line.is_empty() {
            break;
        }
        if !line.is_ascii() {
            return Err(FrameError::InvalidHeader);
        }

        let header = std::str::from_utf8(&line).map_err(|_| FrameError::InvalidHeader)?;
        let (name, value) = header.split_once(':').ok_or(FrameError::InvalidHeader)?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(FrameError::DuplicateContentLength);
            }
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(FrameError::InvalidContentLength);
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| FrameError::InvalidContentLength)?,
            );
        } else if name.eq_ignore_ascii_case("Content-Type") {
            validate_content_type(value)?;
        } else if name.eq_ignore_ascii_case("Content-Encoding")
            && !value.eq_ignore_ascii_case("identity")
        {
            return Err(FrameError::UnsupportedEncoding);
        }
    }

    let length = content_length.ok_or(FrameError::MissingContentLength)?;
    let mut body = vec![0; length];
    if let Err(error) = reader.read_exact(&mut body) {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Err(FrameError::TruncatedBody)
        } else {
            Err(FrameError::Io)
        };
    }
    Ok(Some(body))
}

fn validate_content_type(value: &str) -> Result<(), FrameError> {
    let mut parts = value.split(';').map(str::trim);
    if !parts
        .next()
        .is_some_and(|media| media.eq_ignore_ascii_case("application/vscode-jsonrpc"))
    {
        return Err(FrameError::UnsupportedEncoding);
    }
    for parameter in parts {
        let Some((name, value)) = parameter.split_once('=') else {
            return Err(FrameError::UnsupportedEncoding);
        };
        if !name.trim().eq_ignore_ascii_case("charset")
            || !(value.trim().eq_ignore_ascii_case("utf-8")
                || value.trim().eq_ignore_ascii_case("utf8"))
        {
            return Err(FrameError::UnsupportedEncoding);
        }
    }
    Ok(())
}

/// Writes one byte-exact protocol frame without adding unframed output.
pub fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)
}
