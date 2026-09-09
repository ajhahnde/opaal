#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use opaal_platform::operational::{
    AtomicWriteRequest, FakeOperationalAdapter, HttpHeader, HttpRequest, HttpResponse,
    MAX_HTTP_HEADER_BYTES, MaterializedSecretHeader, OperationalAdapter, OperationalCall,
    OperationalError, OperationalErrorKind, ProcessExit, ProcessOutput, ProcessRequest,
    ReadFileRequest,
};
use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::Value;
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EffectSet, EvaluationContextId,
    RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancellationToken, FakeClock};
use opaal_runtime::operational::{
    MAX_FILE_BYTES, MAX_TOTAL_READ_BYTES, data, filesystem, http, integrity, path, process, time,
    url, version,
};
use opaal_runtime::project::{parse_project_manifest, parse_tool_lock};
use opaal_runtime::security::{Secret, SecretId};

fn context(rules: Vec<AuthorityRule>) -> OperationalContext {
    let evaluation = EvaluationContextId::new(71).expect("nonzero identity");
    OperationalContext::new(
        AuthorityContext::new(evaluation, rules).expect("fixture rules are unique"),
        CancellationToken::never(),
        Arc::new(FakeClock::new()),
        None,
    )
}

fn platform(profile: AuthorityProfile) -> FakePlatform {
    FakePlatform::with_authority_profile(Capabilities::full(), profile)
}

#[test]
fn pure_modules_are_canonical_bounded_and_native_path_safe() {
    let decoded = data::toml_decode(b"z = 2\na = { value = true }\n").expect("valid bounded TOML");
    assert_eq!(
        data::json_encode(&decoded).expect("supported values encode"),
        br#"{"a":{"value":true},"z":2}"#
    );
    assert_eq!(data::get(&decoded, &["a", "value"]), Ok(&Value::Bool(true)));
    assert!(data::toml_decode(b"when = 2026-09-08T00:00:00Z").is_err());

    let joined = path::join(Path::new("/project"), ["target", "artifact"])
        .expect("relative components remain contained");
    assert_eq!(joined, Path::new("/project/target/artifact"));
    assert!(path::join(Path::new("/project"), ["../escape"]).is_err());
    let native = PathBuf::from(OsString::from_vec(b"/project/native-\xff".to_vec()));
    assert_eq!(
        path::normalize(&native).unwrap().as_os_str().as_bytes(),
        b"/project/native-\xff"
    );

    assert_eq!(
        integrity::sha256(b"abc"),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let parsed = version::Version::parse("1.0.0-alpha.1").expect("canonical SemVer");
    assert_eq!(parsed.render(), "1.0.0-alpha.1");
    assert!(
        version::VersionRange::parse(">=1.0.0-alpha.1,<2.0.0")
            .unwrap()
            .matches(&parsed)
    );

    let normalized = url::Url::parse("HTTPS://example.com");
    assert!(normalized.is_err(), "schemes are canonical lowercase");
    assert_eq!(
        url::Url::parse("https://EXAMPLE.com:443/a?q=1")
            .unwrap()
            .render(),
        "https://example.com/a?q=1"
    );
    assert!(
        !url::Url::parse("http://example.com/")
            .unwrap()
            .is_literal_loopback()
    );
    assert_eq!(
        url::Url::parse("https://[0:0:0:0:0:0:0:1]/%2f")
            .unwrap()
            .render(),
        "https://[::1]/%2F"
    );
    assert!(url::Url::parse("https://example.com/%ZZ").is_err());
    assert!(url::Url::parse("https://-example.com/").is_err());
    assert!(url::Url::parse("https://example.com/\\host").is_err());
}

#[test]
fn data_and_timestamp_limits_are_inclusive_and_first_excess_refuses() {
    let exact = Value::string("a".repeat(data::MAX_DATA_BYTES - 2));
    assert_eq!(
        data::json_encode(&exact).unwrap().len(),
        data::MAX_DATA_BYTES
    );
    let excess = Value::string("a".repeat(data::MAX_DATA_BYTES - 1));
    assert_eq!(data::json_encode(&excess).unwrap_err().code(), "DATA010");

    assert_eq!(
        time::Timestamp::from_unix_nanos(0).unwrap().to_string(),
        "1970-01-01T00:00:00.000000000Z"
    );
    assert_eq!(
        time::Timestamp::from_unix_nanos(-1).unwrap().to_string(),
        "1969-12-31T23:59:59.999999999Z"
    );
    assert_eq!(
        time::Timestamp::from_unix_nanos(951_782_400_123_456_789)
            .unwrap()
            .to_string(),
        "2000-02-29T00:00:00.123456789Z"
    );
    assert!(time::Timestamp::from_unix_nanos(253_402_300_800_000_000_000).is_err());
}

#[test]
fn filesystem_requires_exact_authority_and_preserves_status_boundaries() {
    let root = Path::new("/project");
    let target = Path::new("/project/input.bin");
    let evidence = Path::new("/project/evidence.json");
    let read_request = CapabilityRequest::filesystem_read(root).unwrap();
    let write_request = CapabilityRequest::filesystem_write(evidence).unwrap();
    let mut context = context(vec![
        AuthorityRule::grant(read_request.clone(), RequiredEnforcement::Enforced),
        AuthorityRule::grant(write_request.clone(), RequiredEnforcement::Enforced),
    ]);
    let effects = EffectSet::new([read_request, write_request]);
    let platform = platform(
        AuthorityProfile::unsupported()
            .with(
                AuthorityEffect::FilesystemRead,
                AuthorityEnforcement::Enforced,
            )
            .with(
                AuthorityEffect::FilesystemWrite,
                AuthorityEnforcement::Enforced,
            ),
    );
    let adapter = FakeOperationalAdapter::new();
    adapter.insert_file(target, b"input".to_vec());
    let mut reads = filesystem::ReadBudget::default();
    assert_eq!(
        filesystem::read(
            &context, &effects, &platform, &adapter, root, root, target, 5, &mut reads
        )
        .unwrap(),
        b"input"
    );
    filesystem::write_atomic(
        &context, &effects, &platform, &adapter, evidence, root, evidence, b"result",
    )
    .unwrap();
    assert_eq!(adapter.file(evidence).unwrap(), b"result");

    assert!(
        filesystem::read(
            &context,
            &effects,
            &platform,
            &adapter,
            root,
            Path::new("/other"),
            Path::new("/other/input.bin"),
            5,
            &mut reads,
        )
        .is_err(),
        "authority for one root cannot authorize another root"
    );
    assert!(
        filesystem::write_atomic(
            &context,
            &effects,
            &platform,
            &adapter,
            evidence,
            root,
            Path::new("/project/other.json"),
            b"result",
        )
        .is_err(),
        "write authority must equal the exact evidence target"
    );

    let denied = EffectSet::default();
    assert!(
        filesystem::read(
            &context, &denied, &platform, &adapter, root, root, target, 5, &mut reads
        )
        .is_err()
    );
    let _ = &mut context;
}

#[test]
fn aggregate_file_budget_accepts_equality_and_refuses_the_first_excess() {
    let root = Path::new("/project");
    let request = CapabilityRequest::filesystem_read(root).unwrap();
    let context = context(vec![AuthorityRule::grant(
        request.clone(),
        RequiredEnforcement::Enforced,
    )]);
    let effects = EffectSet::new([request]);
    let platform = platform(AuthorityProfile::unsupported().with(
        AuthorityEffect::FilesystemRead,
        AuthorityEnforcement::Enforced,
    ));
    let adapter = FakeOperationalAdapter::new();
    adapter.insert_file("/project/a", vec![0; MAX_FILE_BYTES]);
    adapter.insert_file("/project/b", vec![0; MAX_FILE_BYTES]);
    adapter.insert_file("/project/c", vec![0]);
    let mut budget = filesystem::ReadBudget::default();
    assert_eq!(
        filesystem::read(
            &context,
            &effects,
            &platform,
            &adapter,
            root,
            root,
            Path::new("/project/a"),
            MAX_FILE_BYTES,
            &mut budget,
        )
        .unwrap()
        .len(),
        MAX_FILE_BYTES
    );
    assert_eq!(
        filesystem::read(
            &context,
            &effects,
            &platform,
            &adapter,
            root,
            root,
            Path::new("/project/b"),
            MAX_FILE_BYTES,
            &mut budget,
        )
        .unwrap()
        .len(),
        MAX_FILE_BYTES
    );
    assert_eq!(budget.used(), MAX_TOTAL_READ_BYTES);
    assert!(
        filesystem::read(
            &context,
            &effects,
            &platform,
            &adapter,
            root,
            root,
            Path::new("/project/c"),
            1,
            &mut budget,
        )
        .is_err()
    );
}

#[test]
fn fake_adapter_call_log_is_ordered_and_payload_free() {
    let adapter = FakeOperationalAdapter::new();
    adapter.insert_file("/project/input", b"file-payload".to_vec());
    adapter.set_times(7, 11);
    adapter.push_http(Ok(HttpResponse::new(204, Vec::new(), Vec::new())));
    adapter.push_process(Ok(ProcessOutput::new(
        ProcessExit::Exited(0),
        Vec::new(),
        Vec::new(),
        Duration::from_nanos(3),
    )));

    adapter
        .read_file(ReadFileRequest {
            root: Path::new("/project"),
            path: Path::new("/project/input"),
            max_bytes: 12,
        })
        .unwrap();
    adapter
        .write_atomic(AtomicWriteRequest {
            root: Path::new("/project"),
            path: Path::new("/project/evidence"),
            bytes: b"write-payload",
            max_bytes: 13,
        })
        .unwrap();
    adapter.wall_time_unix_nanos().unwrap();
    adapter.monotonic_nanos().unwrap();
    adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port: 43119,
                tls_server_name: None,
                ca_pem: None,
                method: "POST",
                path_and_query: "/sink",
                headers: &[],
                secret_header: Some(MaterializedSecretHeader {
                    name: "authorization",
                    value: b"secret-payload",
                }),
                body: b"body-payload",
                max_response_bytes: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap();
    adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/tool/git"),
                executable_file: None,
                argv: &[OsString::from("/tool/git"), OsString::from("status")],
                environment: &[(OsString::from("LC_ALL"), OsString::from("C"))],
                cwd: Path::new("/project"),
                stdout_limit: 0,
                stderr_limit: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap();

    let calls = adapter.calls();
    assert!(matches!(calls[0], OperationalCall::Read { .. }));
    assert!(matches!(calls[1], OperationalCall::Write { bytes: 13, .. }));
    assert_eq!(calls[2], OperationalCall::WallTime);
    assert_eq!(calls[3], OperationalCall::MonotonicTime);
    assert!(matches!(calls[4], OperationalCall::Http { .. }));
    assert!(matches!(calls[5], OperationalCall::Process { .. }));
    let rendered = format!("{calls:?}");
    for payload in [
        "file-payload",
        "write-payload",
        "secret-payload",
        "body-payload",
    ] {
        assert!(!rendered.contains(payload));
    }
}

#[test]
fn http_wire_shape_and_header_byte_limits_fail_closed() {
    let adapter = FakeOperationalAdapter::new();
    adapter.push_http(Ok(HttpResponse::new(200, Vec::new(), Vec::new())));
    let exact = HttpHeader::new("x", vec![b'a'; MAX_HTTP_HEADER_BYTES - 5]).unwrap();
    adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port: 80,
                tls_server_name: None,
                ca_pem: None,
                method: "GET",
                path_and_query: "/",
                headers: &[exact],
                secret_header: None,
                body: &[],
                max_response_bytes: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap();

    let excess = HttpHeader::new("x", vec![b'a'; MAX_HTTP_HEADER_BYTES - 4]).unwrap();
    let error = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port: 80,
                tls_server_name: None,
                ca_pem: None,
                method: "GET",
                path_and_query: "/",
                headers: &[excess],
                secret_header: None,
                body: &[],
                max_response_bytes: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::LimitExceeded);

    for request in [
        HttpRequest {
            connect_host: "127.0.0.1\r\nx-injected: yes",
            port: 80,
            tls_server_name: None,
            ca_pem: None,
            method: "GET",
            path_and_query: "/",
            headers: &[],
            secret_header: None,
            body: &[],
            max_response_bytes: 0,
            timeout: Duration::from_secs(1),
        },
        HttpRequest {
            connect_host: "127.0.0.1",
            port: 80,
            tls_server_name: None,
            ca_pem: None,
            method: "GET",
            path_and_query: "/\r\nx-injected: yes",
            headers: &[],
            secret_header: None,
            body: &[],
            max_response_bytes: 0,
            timeout: Duration::from_secs(1),
        },
    ] {
        assert_eq!(
            adapter.http_request(request, &|| false).unwrap_err().kind(),
            OperationalErrorKind::InvalidInput
        );
    }
    let host = HttpHeader::new("host", b"attacker.invalid".to_vec()).unwrap();
    let error = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port: 80,
                tls_server_name: None,
                ca_pem: None,
                method: "GET",
                path_and_query: "/",
                headers: &[host],
                secret_header: None,
                body: &[],
                max_response_bytes: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::InvalidInput);
}

#[test]
fn secret_header_materializes_once_and_redirect_status_is_not_followed() {
    let manifest = parse_project_manifest(
        Path::new("/project/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[endpoints.readiness]
url = "https://127.0.0.1:43119/readiness"
methods = ["GET"]
secret_headers = ["authorization"]
tls = true
tls_server_name = "opaal-golden.invalid"
ca = "ca.pem"
[secrets.token]
kind = "injected"
"#,
    )
    .unwrap();
    let endpoint = &manifest.endpoints()["readiness"];
    assert_eq!(endpoint.id(), "readiness");
    let network = CapabilityRequest::network_http("readiness", "GET").unwrap();
    let reveal = CapabilityRequest::secret_reveal("token", "readiness", "authorization").unwrap();
    let mut context = context(vec![
        AuthorityRule::grant(network.clone(), RequiredEnforcement::Enforced),
        AuthorityRule::grant(reveal.clone(), RequiredEnforcement::Enforced),
    ]);
    let secret_id = SecretId::new("token").unwrap();
    context
        .insert_secret(Secret::new(secret_id.clone(), b"canary-value".to_vec()).unwrap())
        .unwrap();
    let sink = http::secret_header(endpoint, "authorization", secret_id.clone()).unwrap();
    assert!(!format!("{sink:?}").contains("canary-value"));
    let adapter = FakeOperationalAdapter::new();
    adapter.push_http(Ok(HttpResponse::new(
        302,
        vec![HttpHeader::new("x-reflection", b"63616e6172792d76616c7565".to_vec()).unwrap()],
        b"canary-value".to_vec(),
    )));
    let platform = platform(
        AuthorityProfile::unsupported()
            .with(AuthorityEffect::NetworkHttp, AuthorityEnforcement::Enforced)
            .with(
                AuthorityEffect::SecretReveal,
                AuthorityEnforcement::Enforced,
            ),
    );
    let mut budget = http::HttpBudget::default();
    let response = http::request(
        &mut context,
        &EffectSet::new([network, reveal]),
        &platform,
        &adapter,
        endpoint,
        "GET",
        &[],
        Some(sink),
        b"request-body-is-not-logged",
        1024,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut budget,
    )
    .unwrap();
    assert_eq!(
        response.status(),
        302,
        "redirects are returned as ordinary status"
    );
    assert_eq!(response.body(), b"[redacted]");
    assert_eq!(response.headers()[0].value(), b"[redacted]");
    assert!(!context.contains_secret(&secret_id));
    assert_eq!(
        adapter.calls(),
        vec![OperationalCall::Http {
            host: "127.0.0.1".to_owned(),
            port: 43119,
            method: "GET".to_owned(),
            path: "/readiness".to_owned(),
            secret_header: Some("authorization".to_owned())
        }]
    );
    let rendered_calls = format!("{:?}", adapter.calls());
    assert!(!rendered_calls.contains("canary-value"));
    assert!(!rendered_calls.contains("request-body-is-not-logged"));

    adapter.push_http(Ok(HttpResponse::new(
        200,
        vec![],
        b"later canary-value reflection".to_vec(),
    )));
    let later = http::request(
        &mut context,
        &EffectSet::new([CapabilityRequest::network_http("readiness", "GET").unwrap()]),
        &platform,
        &adapter,
        endpoint,
        "GET",
        &[],
        None,
        &[],
        1024,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut budget,
    )
    .unwrap();
    assert_eq!(later.body(), b"later [redacted] reflection");
    assert!(
        context
            .insert_secret(Secret::new(secret_id, b"replacement".to_vec()).unwrap())
            .is_err(),
        "one consumed identity cannot be injected again in the same evaluation"
    );

    adapter.push_http(Err(OperationalError::new(
        OperationalErrorKind::Network,
        "adapter reflected canary-value",
    )));
    let error = http::request(
        &mut context,
        &EffectSet::new([CapabilityRequest::network_http("readiness", "GET").unwrap()]),
        &platform,
        &adapter,
        endpoint,
        "GET",
        &[],
        None,
        &[],
        0,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut budget,
    )
    .unwrap_err();
    assert!(!error.to_string().contains("canary-value"));
    assert!(error.to_string().contains("[redacted]"));
}

#[test]
fn secret_header_counts_toward_exact_header_limits_without_early_consumption() {
    let manifest = parse_project_manifest(
        Path::new("/project/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[endpoints.readiness]
url = "https://127.0.0.1:43119/readiness"
methods = ["GET"]
secret_headers = ["authorization"]
tls = true
tls_server_name = "opaal-golden.invalid"
ca = "ca.pem"
[secrets.token]
kind = "injected"
"#,
    )
    .unwrap();
    let endpoint = &manifest.endpoints()["readiness"];
    let network = CapabilityRequest::network_http("readiness", "GET").unwrap();
    let reveal = CapabilityRequest::secret_reveal("token", "readiness", "authorization").unwrap();
    let platform = platform(
        AuthorityProfile::unsupported()
            .with(AuthorityEffect::NetworkHttp, AuthorityEnforcement::Enforced)
            .with(
                AuthorityEffect::SecretReveal,
                AuthorityEnforcement::Enforced,
            ),
    );
    let headers = (0..127)
        .map(|index| HttpHeader::new(format!("x-{index}"), Vec::new()).unwrap())
        .collect::<Vec<_>>();
    let mut exact_context = context(vec![
        AuthorityRule::grant(network.clone(), RequiredEnforcement::Enforced),
        AuthorityRule::grant(reveal.clone(), RequiredEnforcement::Enforced),
    ]);
    let id = SecretId::new("token").unwrap();
    exact_context
        .insert_secret(Secret::new(id.clone(), b"value".to_vec()).unwrap())
        .unwrap();
    let adapter = FakeOperationalAdapter::new();
    adapter.push_http(Ok(HttpResponse::new(200, vec![], vec![])));
    let effects = EffectSet::new([network.clone(), reveal.clone()]);
    let mut exact_budget = http::HttpBudget::default();
    http::request(
        &mut exact_context,
        &effects,
        &platform,
        &adapter,
        endpoint,
        "GET",
        &headers,
        Some(http::secret_header(endpoint, "authorization", id).unwrap()),
        &[],
        0,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut exact_budget,
    )
    .unwrap();
    for _ in 1..http::MAX_HTTP_REQUESTS {
        adapter.push_http(Ok(HttpResponse::new(200, vec![], vec![])));
        http::request(
            &mut exact_context,
            &effects,
            &platform,
            &adapter,
            endpoint,
            "GET",
            &[],
            None,
            &[],
            0,
            Some(b"fixture CA"),
            Duration::from_secs(1),
            &mut exact_budget,
        )
        .unwrap();
    }
    assert_eq!(exact_budget.requests(), http::MAX_HTTP_REQUESTS);
    let call_count = adapter.calls().len();
    assert!(
        http::request(
            &mut exact_context,
            &effects,
            &platform,
            &adapter,
            endpoint,
            "GET",
            &[],
            None,
            &[],
            0,
            Some(b"fixture CA"),
            Duration::from_secs(1),
            &mut exact_budget,
        )
        .is_err()
    );
    assert_eq!(adapter.calls().len(), call_count);

    let too_many = (0..128)
        .map(|index| HttpHeader::new(format!("x-{index}"), Vec::new()).unwrap())
        .collect::<Vec<_>>();
    let mut excess_context = context(vec![
        AuthorityRule::grant(network.clone(), RequiredEnforcement::Enforced),
        AuthorityRule::grant(reveal.clone(), RequiredEnforcement::Enforced),
    ]);
    let id = SecretId::new("token").unwrap();
    excess_context
        .insert_secret(Secret::new(id.clone(), b"value".to_vec()).unwrap())
        .unwrap();
    let result = http::request(
        &mut excess_context,
        &EffectSet::new([network, reveal]),
        &platform,
        &FakeOperationalAdapter::new(),
        endpoint,
        "GET",
        &too_many,
        Some(http::secret_header(endpoint, "authorization", id.clone()).unwrap()),
        &[],
        0,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut http::HttpBudget::default(),
    );
    assert!(result.is_err());
    assert!(excess_context.contains_secret(&id));

    let polls = Arc::new(AtomicUsize::new(0));
    let cancellation = CancellationToken::from_fn({
        let polls = Arc::clone(&polls);
        move || polls.fetch_add(1, Ordering::SeqCst) >= 2
    });
    let network = CapabilityRequest::network_http("readiness", "GET").unwrap();
    let mut cancelled_context = OperationalContext::new(
        AuthorityContext::new(
            EvaluationContextId::new(72).unwrap(),
            [AuthorityRule::grant(
                network.clone(),
                RequiredEnforcement::Enforced,
            )],
        )
        .unwrap(),
        cancellation,
        Arc::new(FakeClock::new()),
        None,
    );
    let adapter = FakeOperationalAdapter::new();
    adapter.push_http(Ok(HttpResponse::new(200, vec![], vec![])));
    let result = http::request(
        &mut cancelled_context,
        &EffectSet::new([network]),
        &platform,
        &adapter,
        endpoint,
        "GET",
        &[],
        None,
        &[],
        0,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut http::HttpBudget::default(),
    );
    assert!(matches!(
        result,
        Err(opaal_runtime::operational::ModuleError::Cancelled(
            opaal_runtime::eval::CancelReason::Requested
        ))
    ));

    let polls = Arc::new(AtomicUsize::new(0));
    let cancellation = CancellationToken::from_fn({
        let polls = Arc::clone(&polls);
        move || polls.fetch_add(1, Ordering::SeqCst) >= 2
    });
    let network = CapabilityRequest::network_http("readiness", "GET").unwrap();
    let reveal = CapabilityRequest::secret_reveal("token", "readiness", "authorization").unwrap();
    let mut cancelled_context = OperationalContext::new(
        AuthorityContext::new(
            EvaluationContextId::new(73).unwrap(),
            [
                AuthorityRule::grant(network.clone(), RequiredEnforcement::Enforced),
                AuthorityRule::grant(reveal.clone(), RequiredEnforcement::Enforced),
            ],
        )
        .unwrap(),
        cancellation,
        Arc::new(FakeClock::new()),
        None,
    );
    let secret_id = SecretId::new("token").unwrap();
    cancelled_context
        .insert_secret(Secret::new(secret_id.clone(), b"still-owned".to_vec()).unwrap())
        .unwrap();
    let adapter = FakeOperationalAdapter::new();
    adapter.push_http(Ok(HttpResponse::new(200, vec![], vec![])));
    let result = http::request(
        &mut cancelled_context,
        &EffectSet::new([network, reveal]),
        &platform,
        &adapter,
        endpoint,
        "GET",
        &[],
        Some(http::secret_header(endpoint, "authorization", secret_id.clone()).unwrap()),
        &[],
        0,
        Some(b"fixture CA"),
        Duration::from_secs(1),
        &mut http::HttpBudget::default(),
    );
    assert!(matches!(
        result,
        Err(opaal_runtime::operational::ModuleError::Cancelled(
            opaal_runtime::eval::CancelReason::Requested
        ))
    ));
    assert!(cancelled_context.contains_secret(&secret_id));
    assert!(adapter.calls().is_empty());
}

#[test]
fn fixed_probe_uses_locked_executable_and_complete_sanitized_environment() {
    let executable = b"fake git executable";
    let digest = integrity::sha256(executable);
    let manifest = parse_project_manifest(
        Path::new("/project/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[tools.git]
adapter = "git"
version = ">=2.50.0,<3.0.0"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    )
    .unwrap();
    let variables = ["HOME", "TMPDIR", "PATH", "CARGO_HOME", "RUSTC", "RUSTDOC", "LC_ALL", "TZ", "CARGO_NET_OFFLINE", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL"]
        .into_iter().map(|name| format!("[[child_environment.variables]]\nname = \"{name}\"\nvalue = {{ encoding = \"base64url-nopad\", platform = \"unix\", value = \"eA\" }}\n"))
        .collect::<String>();
    let lock_text = format!(
        r#"schema_version = 1
project = "demo"
environment = "ci"
platform = "fixture"
[child_environment]
inherit = []
{variables}
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvZ2l0" }}
version = "2.50.0"
digest = "{digest}"
"#
    );
    let lock = parse_tool_lock(&manifest, "ci", lock_text.as_bytes()).unwrap();
    let request = CapabilityRequest::process_run("git").unwrap();
    let context = context(vec![AuthorityRule::grant(
        request.clone(),
        RequiredEnforcement::AcknowledgeUnenforced,
    )]);
    let effects = EffectSet::new([request]);
    let platform = platform(AuthorityProfile::unsupported().with(
        AuthorityEffect::ProcessRun,
        AuthorityEnforcement::Unenforced,
    ));
    let adapter = FakeOperationalAdapter::new();
    adapter.insert_file("/tool/git", executable.to_vec());
    adapter.push_process(Ok(ProcessOutput::new(
        ProcessExit::Exited(0),
        b"git version 2.50.0\n".to_vec(),
        vec![],
        Duration::from_millis(1),
    )));
    let mut budget = process::ProcessBudget::default();
    let result = process::probe(
        &context,
        &effects,
        &platform,
        &adapter,
        &lock,
        "git",
        Path::new("/project"),
        &mut budget,
    )
    .unwrap();
    assert!(result.status().is_ok());
    let process_call = adapter
        .calls()
        .into_iter()
        .find_map(|call| match call {
            OperationalCall::Process {
                executable,
                argv,
                environment,
            } => Some((executable, argv, environment)),
            _ => None,
        })
        .unwrap();
    assert_eq!(process_call.0, PathBuf::from("/tool/git"));
    assert_eq!(
        process_call.1,
        vec![OsString::from("/tool/git"), OsString::from("--version")]
    );
    assert_eq!(process_call.2.len(), 11);

    adapter.insert_file("/tool/git", b"changed executable".to_vec());
    adapter.push_process(Ok(ProcessOutput::new(
        ProcessExit::Exited(0),
        Vec::new(),
        Vec::new(),
        Duration::from_millis(1),
    )));
    assert!(
        process::run(
            &context,
            &effects,
            &platform,
            &adapter,
            &lock,
            "git",
            &["status"],
            Path::new("/project"),
            1024,
            1024,
            Duration::from_secs(1),
            &mut budget,
        )
        .is_err(),
        "every maintained run revalidates the locked executable digest"
    );
    assert_eq!(budget.attempts(), 1);

    let exact_adapter = FakeOperationalAdapter::new();
    exact_adapter.insert_file("/tool/git", executable.to_vec());
    for _ in 0..process::MAX_PROCESS_ATTEMPTS {
        exact_adapter.push_process(Ok(ProcessOutput::new(
            ProcessExit::Exited(0),
            Vec::new(),
            Vec::new(),
            Duration::from_millis(1),
        )));
    }
    let mut exact_budget = process::ProcessBudget::default();
    for _ in 0..process::MAX_PROCESS_ATTEMPTS {
        process::run(
            &context,
            &effects,
            &platform,
            &exact_adapter,
            &lock,
            "git",
            &["status"],
            Path::new("/project"),
            0,
            0,
            Duration::from_secs(1),
            &mut exact_budget,
        )
        .unwrap();
    }
    let call_count = exact_adapter.calls().len();
    assert!(
        process::run(
            &context,
            &effects,
            &platform,
            &exact_adapter,
            &lock,
            "git",
            &["status"],
            Path::new("/project"),
            0,
            0,
            Duration::from_secs(1),
            &mut exact_budget,
        )
        .is_err()
    );
    assert_eq!(exact_adapter.calls().len(), call_count);
}

#[cfg(target_os = "linux")]
#[test]
fn retained_executable_is_rehashed_after_probe_before_action_run() {
    static NEXT_EXECUTABLE: AtomicUsize = AtomicUsize::new(0);

    let executable = b"fake git executable";
    let digest = integrity::sha256(executable);
    let executable_path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "retained-executable-{}-{}",
        std::process::id(),
        NEXT_EXECUTABLE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&executable_path, executable).unwrap();
    let retained = std::fs::File::open(&executable_path).unwrap();
    let manifest = parse_project_manifest(
        Path::new("/project/opaal.toml"),
        br#"
schema_version = 1
[project]
name = "demo"
root_module = "tasks.opaal"
required_opaal = ">=1.0.0-alpha.1,<2.0.0"
[paths]
root = "."
evidence = "evidence.json"
[tools.git]
adapter = "git"
version = ">=2.50.0,<3.0.0"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#,
    )
    .unwrap();
    let variables = ["HOME", "TMPDIR", "PATH", "CARGO_HOME", "RUSTC", "RUSTDOC", "LC_ALL", "TZ", "CARGO_NET_OFFLINE", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL"]
        .into_iter().map(|name| format!("[[child_environment.variables]]\nname = \"{name}\"\nvalue = {{ encoding = \"base64url-nopad\", platform = \"unix\", value = \"eA\" }}\n"))
        .collect::<String>();
    let encoded_path = opaal_runtime::workflow::native_path(&executable_path)["value"]
        .as_str()
        .unwrap()
        .to_owned();
    let lock = parse_tool_lock(
        &manifest,
        "ci",
        format!(
            r#"schema_version = 1
project = "demo"
environment = "ci"
platform = "fixture"
[child_environment]
inherit = []
{variables}
[[tools]]
id = "git"
adapter = "git"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "{encoded_path}" }}
version = "2.50.0"
digest = "{digest}"
"#
        )
        .as_bytes(),
    )
    .unwrap();
    let request = CapabilityRequest::process_run("git").unwrap();
    let context = context(vec![AuthorityRule::grant(
        request.clone(),
        RequiredEnforcement::AcknowledgeUnenforced,
    )]);
    let effects = EffectSet::new([request]);
    let platform = platform(AuthorityProfile::unsupported().with(
        AuthorityEffect::ProcessRun,
        AuthorityEnforcement::Unenforced,
    ));
    let adapter = FakeOperationalAdapter::new();
    adapter.push_process(Ok(ProcessOutput::new(
        ProcessExit::Exited(0),
        b"git version 2.50.0\n".to_vec(),
        Vec::new(),
        Duration::from_millis(1),
    )));
    let mut budget = process::ProcessBudget::default();
    process::probe_retained(
        &context,
        &effects,
        &platform,
        &adapter,
        &lock,
        "git",
        Path::new("/project"),
        &mut budget,
        &retained,
    )
    .unwrap();

    std::fs::write(&executable_path, b"mutated in place").unwrap();
    let error = process::run_retained(
        &context,
        &effects,
        &platform,
        &adapter,
        &lock,
        "git",
        &["status"],
        Path::new("/project"),
        1024,
        1024,
        Duration::from_secs(1),
        &mut budget,
        &retained,
    )
    .unwrap_err();
    assert_eq!(error.code(), "PROCESS003");
    assert_eq!(budget.attempts(), 1);
    assert_eq!(
        adapter
            .calls()
            .into_iter()
            .filter(|call| matches!(call, OperationalCall::Process { .. }))
            .count(),
        1,
        "the mutated descriptor must be refused before the action spawn"
    );
    std::fs::remove_file(executable_path).unwrap();
}
