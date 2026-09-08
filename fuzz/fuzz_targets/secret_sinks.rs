#![no_main]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use opaal_platform::operational::{FakeOperationalAdapter, HttpResponse};
use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EffectSet, EvaluationContextId,
    RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, FakeClock};
use opaal_runtime::operational::{ModuleError, http};
use opaal_runtime::project::parse_project_manifest;
use opaal_runtime::security::{MAX_SECRET_BYTES, REDACTED, Secret, SecretId, SecretStore};

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 || data.len() > MAX_SECRET_BYTES {
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
        SecretId::new("token").expect("the fixed identity is valid"),
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

    let manifest = parse_project_manifest(
        Path::new("/project/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "fuzz"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[endpoints.sink]
url = "https://127.0.0.1:443/"
methods = ["GET"]
secret_headers = ["authorization"]
tls = true
tls_server_name = "sink.invalid"
ca = "ca.pem"
[secrets.token]
kind = "injected"
"#,
    )
    .expect("the fixed endpoint manifest is valid");
    let endpoint = &manifest.endpoints()["sink"];
    let network = CapabilityRequest::network_http("sink", "GET").unwrap();
    let reveal = CapabilityRequest::secret_reveal("token", "sink", "authorization").unwrap();
    let mut context = OperationalContext::new(
        AuthorityContext::new(
            EvaluationContextId::new(1).unwrap(),
            [
                AuthorityRule::grant(network.clone(), RequiredEnforcement::Enforced),
                AuthorityRule::grant(reveal.clone(), RequiredEnforcement::Enforced),
            ],
        )
        .unwrap(),
        CancellationToken::never(),
        Arc::new(FakeClock::new()),
        None,
    );
    let secret_id = SecretId::new("token").unwrap();
    context
        .insert_secret(Secret::new(secret_id.clone(), payload.to_vec()).unwrap())
        .unwrap();
    let adapter = FakeOperationalAdapter::new();
    adapter.push_http(Ok(HttpResponse::new(200, Vec::new(), sink)));
    let platform = FakePlatform::with_authority_profile(
        Capabilities::full(),
        AuthorityProfile::unsupported()
            .with(AuthorityEffect::NetworkHttp, AuthorityEnforcement::Enforced)
            .with(
                AuthorityEffect::SecretReveal,
                AuthorityEnforcement::Enforced,
            ),
    );
    let response = http::request(
        &mut context,
        &EffectSet::new([network, reveal]),
        &platform,
        &adapter,
        endpoint,
        "GET",
        &[],
        Some(http::secret_header(endpoint, "authorization", secret_id).unwrap()),
        &[],
        MAX_SECRET_BYTES * 3,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut http::HttpBudget::default(),
    );
    match response {
        Ok(response) => assert!(
            !response
                .body()
                .windows(payload.len())
                .any(|window| window == payload)
        ),
        Err(ModuleError::Adapter(error)) => {
            assert_eq!(error.kind(), opaal_platform::operational::OperationalErrorKind::InvalidInput);
            assert!(adapter.calls().is_empty());
        }
        Err(error) => panic!("unexpected bounded secret-sink outcome: {error}"),
    }
});
