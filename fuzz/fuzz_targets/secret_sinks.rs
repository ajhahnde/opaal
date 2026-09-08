#![no_main]

use libfuzzer_sys::fuzz_target;
use opaal_runtime::security::{REDACTED, Secret, SecretId, SecretStore};

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }
    let split = 1 + usize::from(data[0]) % (data.len() - 1);
    let payload = &data[1..=split];
    if REDACTED
        .as_bytes()
        .windows(payload.len())
        .any(|window| window == payload)
    {
        return;
    }

    let mut store = SecretStore::new();
    let secret = Secret::new(
        SecretId::new("fuzz-secret").expect("the fixed identity is valid"),
        payload.to_vec(),
    )
    .expect("the split always produces a nonempty payload");
    store.insert(secret).expect("the store starts empty");

    let mut sink = data.to_vec();
    sink.extend_from_slice(payload);
    sink.extend_from_slice(data);
    let redacted = store.redact_bytes(&sink);
    assert!(
        !redacted
            .windows(payload.len())
            .any(|window| window == payload)
    );
    assert_eq!(store.redact_bytes(&redacted), redacted);
});
