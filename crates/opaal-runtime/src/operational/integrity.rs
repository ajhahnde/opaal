//! SHA-256 integrity helpers with one canonical rendering.

use sha2::{Digest, Sha256};

/// Hash bytes and render `sha256:` followed by lowercase hexadecimal.
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut rendered, "{byte:02x}").expect("writing to a string cannot fail");
    }
    rendered
}

/// Whether text is the canonical SHA-256 spelling.
#[must_use]
pub fn is_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
