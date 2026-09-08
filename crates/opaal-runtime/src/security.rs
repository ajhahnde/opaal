//! Opaque secret storage and deterministic redaction for embedding boundaries.

use std::collections::BTreeMap;
use std::fmt;

/// The stable replacement emitted for every registered secret representation.
pub const REDACTED: &str = "[redacted]";

/// Maximum explicitly injected secrets owned by one evaluation.
pub const MAX_SECRETS: usize = 8;

/// Maximum byte length of one explicitly injected secret.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// One explicit secret identity. It contains no secret bytes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SecretId(String);

impl SecretId {
    /// Build a nonempty secret identity.
    pub fn new(value: impl Into<String>) -> Result<Self, SecretError> {
        let value = value.into();
        if value.is_empty() {
            Err(SecretError::EmptyId)
        } else {
            Ok(Self(value))
        }
    }

    /// The public identity text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A secret construction or store error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretError {
    /// Secret identifiers must be nonempty.
    EmptyId,
    /// Secret payloads must be nonempty.
    EmptyPayload,
    /// One secret payload exceeded the fixed per-secret ceiling.
    PayloadTooLarge {
        /// Supplied payload length.
        size: usize,
        /// Fixed supported maximum.
        max: usize,
    },
    /// One evaluation exceeded the fixed secret-count ceiling.
    TooManySecrets {
        /// Fixed supported maximum.
        max: usize,
    },
    /// One store cannot contain the same identity twice.
    DuplicateId(SecretId),
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("a secret identity cannot be empty"),
            Self::EmptyPayload => formatter.write_str("a secret payload cannot be empty"),
            Self::PayloadTooLarge { size, max } => {
                write!(
                    formatter,
                    "secret payload has {size} bytes; maximum is {max}"
                )
            }
            Self::TooManySecrets { max } => {
                write!(formatter, "an evaluation can contain at most {max} secrets")
            }
            Self::DuplicateId(id) => {
                write!(formatter, "duplicate secret identity {:?}", id.as_str())
            }
        }
    }
}

impl std::error::Error for SecretError {}

/// One non-cloneable opaque secret owned by an evaluation.
pub struct Secret {
    id: SecretId,
    payload: Vec<u8>,
}

impl Secret {
    /// Take ownership of one nonempty payload.
    pub fn new(id: SecretId, payload: Vec<u8>) -> Result<Self, SecretError> {
        if payload.is_empty() {
            return Err(SecretError::EmptyPayload);
        }
        if payload.len() > MAX_SECRET_BYTES {
            return Err(SecretError::PayloadTooLarge {
                size: payload.len(),
                max: MAX_SECRET_BYTES,
            });
        }
        Ok(Self { id, payload })
    }

    /// The non-secret identity.
    #[must_use]
    pub const fn id(&self) -> &SecretId {
        &self.id
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Secret")
            .field("id", &self.id)
            .field("payload", &REDACTED)
            .finish()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.payload.fill(0);
        let _ = std::hint::black_box(&mut self.payload);
    }
}

/// Evaluation-owned secrets plus the redactor for every diagnostic/evidence
/// sink at that boundary.
#[derive(Default)]
pub struct SecretStore {
    secrets: BTreeMap<SecretId, Secret>,
}

impl SecretStore {
    /// An empty store. It redacts nothing and discovers no ambient credentials.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            secrets: BTreeMap::new(),
        }
    }

    /// Add one explicitly injected secret.
    pub fn insert(&mut self, secret: Secret) -> Result<(), SecretError> {
        if self.secrets.contains_key(secret.id()) {
            return Err(SecretError::DuplicateId(secret.id().clone()));
        }
        if self.secrets.len() == MAX_SECRETS {
            return Err(SecretError::TooManySecrets { max: MAX_SECRETS });
        }
        self.secrets.insert(secret.id().clone(), secret);
        Ok(())
    }

    /// Whether the store contains `id`, without exposing its payload.
    #[must_use]
    pub fn contains(&self, id: &SecretId) -> bool {
        self.secrets.contains_key(id)
    }

    /// Secret identities in canonical order.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = &SecretId> {
        self.secrets.keys()
    }

    /// Redact raw and common encoded representations in arbitrary bytes.
    #[must_use]
    pub fn redact_bytes(&self, input: &[u8]) -> Vec<u8> {
        let mut needles = SecretNeedles::new();
        for secret in self.secrets.values() {
            needles.extend(needle_variants(secret.payload()));
        }
        needles.sort_longest_first();
        replace_needles(input, needles.as_slice())
    }

    /// Redact raw text secrets and common encoded representations without
    /// allowing an arbitrary binary payload to split a UTF-8 code point.
    #[must_use]
    pub fn redact_text(&self, input: &str) -> String {
        let mut needles = SecretNeedles::new();
        for secret in self.secrets.values() {
            for mut needle in needle_variants(secret.payload()) {
                if std::str::from_utf8(&needle).is_ok() {
                    needles.extend([needle]);
                } else {
                    clear_secret_bytes(&mut needle);
                }
            }
        }
        needles.sort_longest_first();
        String::from_utf8(replace_needles(input.as_bytes(), needles.as_slice()))
            .expect("valid UTF-8 needles and ASCII replacement preserve UTF-8")
    }
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretStore")
            .field("ids", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

struct SecretNeedles {
    values: Vec<Vec<u8>>,
}

impl SecretNeedles {
    fn new() -> Self {
        Self { values: Vec::new() }
    }

    fn extend(&mut self, needles: impl IntoIterator<Item = Vec<u8>>) {
        self.values.extend(needles);
    }

    fn sort_longest_first(&mut self) {
        self.values
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    }

    fn as_slice(&self) -> &[Vec<u8>] {
        &self.values
    }
}

impl Drop for SecretNeedles {
    fn drop(&mut self) {
        for needle in &mut self.values {
            clear_secret_bytes(needle);
        }
    }
}

fn clear_secret_bytes(bytes: &mut Vec<u8>) {
    bytes.fill(0);
    let _ = std::hint::black_box(bytes);
}

fn needle_variants(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut hex = encode_hex(payload);
    let mut uppercase_hex = hex.clone();
    uppercase_hex.make_ascii_uppercase();

    let standard_base64 = encode_base64(payload, false);
    let mut unpadded_standard_base64 = standard_base64.clone();
    while unpadded_standard_base64.last() == Some(&b'=') {
        unpadded_standard_base64.pop();
    }

    let url_base64 = encode_base64(payload, true);
    let mut padded_url_base64 = url_base64.clone();
    padded_url_base64.extend(std::iter::repeat_n(
        b'=',
        (4 - padded_url_base64.len() % 4) % 4,
    ));

    let mut percent = encode_percent(payload);
    let mut lowercase_percent = percent.clone();
    lowercase_percent.make_ascii_lowercase();

    let mut variants = vec![
        payload.to_vec(),
        std::mem::take(&mut hex),
        uppercase_hex,
        standard_base64,
        unpadded_standard_base64,
        url_base64,
        padded_url_base64,
        std::mem::take(&mut percent),
        lowercase_percent,
    ];
    if let Ok(text) = std::str::from_utf8(payload) {
        let mut quoted = serde_json::to_vec(text).expect("a string always serializes");
        variants.push(quoted[1..quoted.len() - 1].to_vec());
        clear_secret_bytes(&mut quoted);
    }
    variants
}

fn replace_needles(input: &[u8], needles: &[Vec<u8>]) -> Vec<u8> {
    let replacement = REDACTED.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        let matching_needle = needles
            .iter()
            .find(|needle| input[offset..].starts_with(needle.as_slice()));
        if input[offset..].starts_with(replacement)
            && matching_needle.is_none_or(|needle| needle.len() <= replacement.len())
        {
            output.extend_from_slice(replacement);
            offset += replacement.len();
        } else if let Some(needle) = matching_needle {
            output.extend_from_slice(replacement);
            offset += needle.len();
        } else {
            output.push(input[offset]);
            offset += 1;
        }
    }
    if contains_unredacted_needle(&output, needles) {
        clear_secret_bytes(&mut output);
        replacement.to_vec()
    } else {
        output
    }
}

fn contains_unredacted_needle(input: &[u8], needles: &[Vec<u8>]) -> bool {
    needles.iter().any(|needle| {
        input
            .windows(needle.len())
            .enumerate()
            .any(|(start, window)| {
                window == needle.as_slice()
                    && !contained_in_replacement(input, start, start + needle.len())
            })
    })
}

fn contained_in_replacement(input: &[u8], start: usize, end: usize) -> bool {
    let replacement = REDACTED.as_bytes();
    let earliest = start.saturating_sub(replacement.len() - 1);
    let latest = start.min(input.len().saturating_sub(replacement.len()));
    (earliest..=latest).any(|marker_start| {
        input[marker_start..].starts_with(replacement) && end <= marker_start + replacement.len()
    })
}

fn encode_hex(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)]);
        encoded.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    encoded
}

fn encode_base64(bytes: &[u8], url_safe: bool) -> Vec<u8> {
    let alphabet = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut encoded = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(alphabet[usize::from(first >> 2)]);
        encoded.push(alphabet[usize::from(((first & 0x03) << 4) | (second >> 4))]);
        if chunk.len() > 1 {
            encoded.push(alphabet[usize::from(((second & 0x0f) << 2) | (third >> 6))]);
        } else if !url_safe {
            encoded.push(b'=');
        }
        if chunk.len() > 2 {
            encoded.push(alphabet[usize::from(third & 0x3f)]);
        } else if !url_safe {
            encoded.push(b'=');
        }
    }
    encoded
}

fn encode_percent(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = Vec::with_capacity(bytes.len() * 3);
    for byte in bytes {
        encoded.push(b'%');
        encoded.push(DIGITS[usize::from(byte >> 4)]);
        encoded.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    encoded
}
