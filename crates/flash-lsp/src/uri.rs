//! Strict, lossless conversion between LSP document URIs and native paths.

use std::fmt;
use std::path::{Path, PathBuf};

/// One exact protocol URI accepted as a local Flash source document.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DocumentUri(String);

impl DocumentUri {
    /// Validates and retains one absolute local `file` URI exactly as received.
    pub fn parse(uri: impl Into<String>) -> Result<Self, FileUriError> {
        let uri = uri.into();
        decode_file_uri(&uri)?;
        Ok(Self(uri))
    }

    /// Encodes an absolute native path without lossy text conversion.
    pub fn from_absolute_path(path: &Path) -> Result<Self, FileUriError> {
        if !path.is_absolute() {
            return Err(FileUriError::RelativePath);
        }
        let bytes = native_path_bytes(path)?;
        if bytes.contains(&0) {
            return Err(FileUriError::NulByte);
        }

        let mut uri = String::from("file://");
        for &byte in bytes {
            if is_unreserved(byte) || byte == b'/' {
                uri.push(char::from(byte));
            } else {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                uri.push('%');
                uri.push(char::from(HEX[usize::from(byte >> 4)]));
                uri.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
        Ok(Self(uri))
    }

    /// The exact URI spelling retained for protocol replies.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decodes this URI to an absolute native path without replacement bytes.
    pub fn to_file_path(&self) -> Result<PathBuf, FileUriError> {
        decode_file_uri(&self.0)
    }
}

impl fmt::Display for DocumentUri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A file URI or native path that cannot identify a local source document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileUriError {
    UnsupportedScheme,
    UnsupportedAuthority,
    RelativePath,
    InvalidPercentEncoding,
    QueryOrFragment,
    NulByte,
    NonAsciiUri,
    InvalidUriCharacter,
    UnsupportedPlatform,
}

impl fmt::Display for FileUriError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedScheme => "document URI does not use the file scheme",
            Self::UnsupportedAuthority => "file URI has a non-local authority",
            Self::RelativePath => "file URI path is not absolute",
            Self::InvalidPercentEncoding => "file URI has invalid percent encoding",
            Self::QueryOrFragment => "file URI contains a query or fragment",
            Self::NulByte => "file URI path contains a NUL byte",
            Self::NonAsciiUri => "file URI contains an unencoded non-ASCII byte",
            Self::InvalidUriCharacter => "file URI path contains an unencoded invalid character",
            Self::UnsupportedPlatform => "native file URI conversion is unsupported on this host",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for FileUriError {}

fn decode_file_uri(uri: &str) -> Result<PathBuf, FileUriError> {
    if !uri.is_ascii() {
        return Err(FileUriError::NonAsciiUri);
    }
    let Some((scheme, remainder)) = uri.split_once(':') else {
        return Err(FileUriError::UnsupportedScheme);
    };
    if !scheme.eq_ignore_ascii_case("file") {
        return Err(FileUriError::UnsupportedScheme);
    }
    if remainder.contains(['?', '#']) {
        return Err(FileUriError::QueryOrFragment);
    }

    let encoded_path = if let Some(authority_and_path) = remainder.strip_prefix("//") {
        if !authority_and_path.starts_with('/') {
            return Err(FileUriError::UnsupportedAuthority);
        }
        authority_and_path
    } else {
        remainder
    };
    if !encoded_path.starts_with('/') {
        return Err(FileUriError::RelativePath);
    }

    let encoded = encoded_path.as_bytes();
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'%' {
            let Some(high) = encoded.get(index + 1).and_then(|byte| hex_value(*byte)) else {
                return Err(FileUriError::InvalidPercentEncoding);
            };
            let Some(low) = encoded.get(index + 2).and_then(|byte| hex_value(*byte)) else {
                return Err(FileUriError::InvalidPercentEncoding);
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            if !is_path_character(encoded[index]) {
                return Err(FileUriError::InvalidUriCharacter);
            }
            decoded.push(encoded[index]);
            index += 1;
        }
    }
    if decoded.contains(&0) {
        return Err(FileUriError::NulByte);
    }
    native_path_from_bytes(decoded)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

const fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

const fn is_path_character(byte: u8) -> bool {
    is_unreserved(byte)
        || matches!(
            byte,
            b'/' | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
}

#[cfg(unix)]
fn native_path_bytes(path: &Path) -> Result<&[u8], FileUriError> {
    use std::os::unix::ffi::OsStrExt;

    Ok(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn native_path_bytes(_path: &Path) -> Result<&[u8], FileUriError> {
    Err(FileUriError::UnsupportedPlatform)
}

#[cfg(unix)]
fn native_path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, FileUriError> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn native_path_from_bytes(_bytes: Vec<u8>) -> Result<PathBuf, FileUriError> {
    Err(FileUriError::UnsupportedPlatform)
}
