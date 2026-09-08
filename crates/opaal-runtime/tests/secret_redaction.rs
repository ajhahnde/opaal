#![forbid(unsafe_code)]

use opaal_runtime::security::{
    MAX_SECRET_BYTES, MAX_SECRETS, REDACTED, Secret, SecretError, SecretId, SecretStore,
};

fn secret(id: &str, payload: &[u8]) -> Secret {
    Secret::new(
        SecretId::new(id).expect("the fixture identity is valid"),
        payload.to_vec(),
    )
    .expect("the fixture payload is nonempty")
}

#[test]
fn secret_debug_and_store_debug_expose_identity_but_never_payload() {
    let value = secret("readiness_token", b"canary-value");
    let rendered = format!("{value:?}");
    assert!(rendered.contains("readiness_token"));
    assert!(rendered.contains(REDACTED));
    assert!(!rendered.contains("canary-value"));

    let mut store = SecretStore::new();
    store.insert(value).expect("the identity is unique");
    let rendered = format!("{store:?}");
    assert!(rendered.contains("readiness_token"));
    assert!(!rendered.contains("canary-value"));
}

#[test]
fn raw_hex_base64_base64url_percent_and_json_forms_are_redacted() {
    let mut store = SecretStore::new();
    store
        .insert(secret("plain", b"secret"))
        .expect("the first identity is unique");
    store
        .insert(secret("escaped", b"a\"b\\c"))
        .expect("the second identity is unique");

    let text = concat!(
        "raw=secret ",
        "hex=736563726574 ",
        "base64=c2VjcmV0 ",
        "percent=%73%65%63%72%65%74 ",
        "json=a\\\"b\\\\c"
    );
    let redacted = store.redact_text(text);

    for forbidden in [
        "secret",
        "736563726574",
        "c2VjcmV0",
        "%73%65%63%72%65%74",
        "a\\\"b\\\\c",
    ] {
        assert!(
            !redacted.contains(forbidden),
            "redacted text retained {forbidden:?}: {redacted:?}"
        );
    }
    assert_eq!(redacted.matches(REDACTED).count(), 5);
}

#[test]
fn arbitrary_non_utf8_bytes_are_redacted_without_lossy_conversion() {
    let mut store = SecretStore::new();
    store
        .insert(secret("binary", &[0xff, 0x00, 0x81]))
        .expect("the identity is unique");
    let mut input = b"before:".to_vec();
    input.extend_from_slice(&[0xff, 0x00, 0x81]);
    input.extend_from_slice(b":after");

    assert_eq!(
        store.redact_bytes(&input),
        b"before:[redacted]:after".to_vec()
    );
}

#[test]
fn binary_needles_cannot_split_utf8_text_and_redaction_is_idempotent() {
    let mut store = SecretStore::new();
    store
        .insert(secret("continuation-byte", &[0xa9]))
        .expect("the identity is unique");
    store
        .insert(secret("marker-fragment", b"red"))
        .expect("the identity is unique");

    assert_eq!(store.redact_text("café"), "café");
    assert_eq!(store.redact_text("red alert"), "[redacted] alert");
    assert_eq!(
        store.redact_text(&store.redact_text("red alert")),
        "[redacted] alert"
    );
}

#[test]
fn a_secret_extending_the_redaction_marker_is_not_preserved_as_existing_output() {
    let mut store = SecretStore::new();
    store
        .insert(secret("marker-prefixed", b"[redacted]-canary"))
        .expect("the identity is unique");

    let redacted = store.redact_text("value=[redacted]-canary");
    assert_eq!(redacted, "value=[redacted]");
    assert_eq!(store.redact_text(&redacted), redacted);
}

#[test]
fn secrets_overlapping_an_existing_marker_fail_closed() {
    let payload = b"redacted]\0\0";
    let mut store = SecretStore::new();
    store
        .insert(secret("marker-overlap", payload))
        .expect("the identity is unique");

    let redacted = store.redact_bytes(b"[redacted]\0\0 suffix");

    assert_eq!(redacted, REDACTED.as_bytes());
    assert!(
        !redacted
            .windows(payload.len())
            .any(|window| window == payload)
    );
    assert_eq!(store.redact_bytes(&redacted), redacted);
}

#[test]
fn replacement_boundaries_cannot_recreate_a_secret() {
    let payload = b"]a";
    let mut store = SecretStore::new();
    store
        .insert(secret("replacement-boundary", payload))
        .expect("the identity is unique");

    let redacted = store.redact_bytes(b"]aa");

    assert_eq!(redacted, REDACTED.as_bytes());
    assert!(
        !redacted
            .windows(payload.len())
            .any(|window| window == payload)
    );
    assert_eq!(store.redact_bytes(&redacted), redacted);
}

#[test]
fn empty_and_duplicate_secrets_are_refused() {
    assert_eq!(SecretId::new(""), Err(SecretError::EmptyId));
    let id = SecretId::new("token").expect("the identity is valid");
    assert!(matches!(
        Secret::new(id.clone(), Vec::new()),
        Err(SecretError::EmptyPayload)
    ));

    let mut store = SecretStore::new();
    store
        .insert(Secret::new(id.clone(), b"first".to_vec()).expect("nonempty"))
        .expect("the identity is new");
    assert_eq!(
        store.insert(Secret::new(id.clone(), b"second".to_vec()).expect("nonempty")),
        Err(SecretError::DuplicateId(id))
    );
}

#[test]
fn secret_size_and_count_ceilings_are_enforced() {
    let too_large = Secret::new(
        SecretId::new("large").expect("the identity is valid"),
        vec![0; MAX_SECRET_BYTES + 1],
    );
    assert!(matches!(
        too_large,
        Err(SecretError::PayloadTooLarge {
            size,
            max: MAX_SECRET_BYTES,
        }) if size == MAX_SECRET_BYTES + 1
    ));

    let mut store = SecretStore::new();
    for index in 0..MAX_SECRETS {
        store
            .insert(secret(&format!("secret-{index}"), b"value"))
            .expect("the fixed ceiling has not been reached");
    }
    assert_eq!(
        store.insert(secret("one-too-many", b"value")),
        Err(SecretError::TooManySecrets { max: MAX_SECRETS })
    );
}
