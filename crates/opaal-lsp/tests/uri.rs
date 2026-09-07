#![forbid(unsafe_code)]

use std::path::Path;

use opaal_lsp::uri::{DocumentUri, FileUriError};

#[test]
fn file_uris_require_an_absolute_local_path_and_strict_percent_encoding() {
    for (uri, expected) in [
        (
            "https://example.test/main.opaal",
            FileUriError::UnsupportedScheme,
        ),
        (
            "file://server/main.opaal",
            FileUriError::UnsupportedAuthority,
        ),
        ("file:relative.opaal", FileUriError::RelativePath),
        ("file:///tmp/bad%2", FileUriError::InvalidPercentEncoding),
        (
            "file:///tmp/main.opaal?query",
            FileUriError::QueryOrFragment,
        ),
        (
            "file:///tmp/main.opaal#fragment",
            FileUriError::QueryOrFragment,
        ),
        ("file:///tmp/%00.opaal", FileUriError::NulByte),
        ("file:///tmp/é.opaal", FileUriError::NonAsciiUri),
        (
            "file:///tmp/a file.opaal",
            FileUriError::InvalidUriCharacter,
        ),
        (
            "file:///tmp/a\\file.opaal",
            FileUriError::InvalidUriCharacter,
        ),
    ] {
        assert_eq!(DocumentUri::parse(uri).unwrap_err(), expected, "{uri}");
    }
}

#[test]
fn ascii_file_uris_round_trip_without_changing_the_protocol_spelling() {
    let uri = DocumentUri::parse("file:///tmp/a%20file-%E2%82%AC.opaal").unwrap();

    assert_eq!(uri.as_str(), "file:///tmp/a%20file-%E2%82%AC.opaal");
    assert_eq!(
        uri.to_file_path().unwrap(),
        Path::new("/tmp/a file-€.opaal")
    );
    assert_eq!(
        DocumentUri::from_absolute_path(&uri.to_file_path().unwrap()).unwrap(),
        uri
    );
}

#[cfg(unix)]
#[test]
fn percent_encoding_preserves_non_utf8_native_path_bytes() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    let path = PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\xff.opaal".to_vec()));
    let uri = DocumentUri::from_absolute_path(&path).unwrap();

    assert_eq!(uri.as_str(), "file:///tmp/non-utf8-%FF.opaal");
    assert_eq!(uri.to_file_path().unwrap(), path);
}

#[test]
fn native_paths_must_be_absolute() {
    assert_eq!(
        DocumentUri::from_absolute_path(Path::new("relative.opaal")).unwrap_err(),
        FileUriError::RelativePath
    );
}
