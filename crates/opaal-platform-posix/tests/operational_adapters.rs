#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use opaal_platform::operational::{
    AtomicWriteRequest, HttpRequest, MaterializedSecretHeader, OperationalAdapter,
    OperationalErrorKind, ProcessRequest, ReadFileRequest,
};
use opaal_platform_posix::operational::PosixOperationalAdapter;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

const CA: &[u8] = include_bytes!("fixtures/ca.pem");
const SERVER_CERT: &[u8] = include_bytes!("fixtures/server.pem");
const SERVER_KEY: &[u8] = include_bytes!("fixtures/server-key.pem");

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "opaal-operational-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("unique temporary root is created");
        Self(fs::canonicalize(path).expect("fixture root has one physical no-follow identity"))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn descriptor_relative_files_preserve_limits_atomicity_and_symlink_refusal() {
    let root = TempRoot::new();
    let adapter = PosixOperationalAdapter::new();
    let target = root.0.join("evidence.json");
    adapter
        .write_atomic(AtomicWriteRequest {
            root: &root.0,
            path: &target,
            bytes: b"first",
            max_bytes: 5,
        })
        .unwrap();
    assert_eq!(
        adapter
            .read_file(ReadFileRequest {
                root: &root.0,
                path: &target,
                max_bytes: 5
            })
            .unwrap(),
        b"first"
    );
    adapter
        .write_atomic(AtomicWriteRequest {
            root: &root.0,
            path: &target,
            bytes: b"second",
            max_bytes: 6,
        })
        .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"second");
    assert_eq!(
        adapter
            .read_file(ReadFileRequest {
                root: &root.0,
                path: &target,
                max_bytes: 5
            })
            .unwrap_err()
            .kind(),
        OperationalErrorKind::LimitExceeded
    );

    let outside = root.0.with_extension("outside");
    fs::write(&outside, b"outside").unwrap();
    let link = root.0.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    assert_eq!(
        adapter
            .read_file(ReadFileRequest {
                root: &root.0,
                path: &link,
                max_bytes: 64
            })
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Symlink
    );

    let native = root.0.join(OsString::from_vec(vec![
        b'n', b'a', b't', b'i', b'v', b'e', b'-', 0xff,
    ]));
    match fs::write(&native, [0xff, 0x00, 0x81]) {
        Ok(()) => assert_eq!(
            adapter
                .read_file(ReadFileRequest {
                    root: &root.0,
                    path: &native,
                    max_bytes: 3,
                })
                .unwrap(),
            [0xff, 0x00, 0x81]
        ),
        Err(cause) if cfg!(target_os = "macos") && cause.raw_os_error() == Some(libc::EILSEQ) => {
            let failure = adapter
                .read_file(ReadFileRequest {
                    root: &root.0,
                    path: &native,
                    max_bytes: 3,
                })
                .unwrap_err();
            assert!(
                matches!(
                    failure.kind(),
                    OperationalErrorKind::Io(_) | OperationalErrorKind::NotFound
                ),
                "the host refusal remains structured without lossy path conversion: {failure:?}"
            );
        }
        Err(cause) => panic!("cannot create native-byte fixture: {cause}"),
    }

    let linked_directory = root.0.join("linked-directory");
    std::os::unix::fs::symlink(root.0.as_path(), &linked_directory).unwrap();
    assert_eq!(
        adapter
            .read_file(ReadFileRequest {
                root: &root.0,
                path: &linked_directory.join("evidence.json"),
                max_bytes: 64,
            })
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Symlink
    );

    let linked_root = root.0.with_extension("linked-root");
    std::os::unix::fs::symlink(&root.0, &linked_root).unwrap();
    assert_eq!(
        adapter
            .read_file(ReadFileRequest {
                root: &linked_root,
                path: &linked_root.join("evidence.json"),
                max_bytes: 64,
            })
            .unwrap_err()
            .kind(),
        OperationalErrorKind::Symlink
    );
    fs::remove_file(linked_root).unwrap();
    fs::remove_file(outside).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn process_uses_empty_ambient_environment_and_bounded_timeout_cleanup() {
    let root = TempRoot::new();
    let adapter = PosixOperationalAdapter::new();
    let argv = [
        OsString::from("printf"),
        OsString::from("%s"),
        OsString::from("bounded"),
    ];
    let output = adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/usr/bin/printf"),
                executable_file: None,
                argv: &argv,
                environment: &[],
                cwd: &root.0,
                stdout_limit: 7,
                stderr_limit: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap();
    assert_eq!(output.stdout(), b"bounded");
    assert_eq!(
        output.status(),
        opaal_platform::operational::ProcessExit::Exited(0)
    );

    let error = adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/usr/bin/printf"),
                executable_file: None,
                argv: &[
                    OsString::from("printf"),
                    OsString::from("%s"),
                    OsString::from("first-excess"),
                ],
                environment: &[],
                cwd: &root.0,
                stdout_limit: "first-excess".len() - 1,
                stderr_limit: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::LimitExceeded);

    let argv = [
        OsString::from("sh"),
        OsString::from("-c"),
        OsString::from("sleep 30"),
    ];
    let started = Instant::now();
    let error = adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/bin/sh"),
                executable_file: None,
                argv: &argv,
                environment: &[],
                cwd: &root.0,
                stdout_limit: 0,
                stderr_limit: 0,
                timeout: Duration::from_millis(50),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(11),
        "TERM/KILL/reap cleanup stayed bounded"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn process_executes_the_retained_descriptor_instead_of_a_replaced_path() {
    let root = TempRoot::new();
    let adapter = PosixOperationalAdapter::new();
    let retained = fs::File::open("/usr/bin/printf").unwrap();
    let argv = [OsString::from("printf"), OsString::from("descriptor")];
    let output = adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/usr/bin/false"),
                executable_file: Some(&retained),
                argv: &argv,
                environment: &[],
                cwd: &root.0,
                stdout_limit: 10,
                stderr_limit: 0,
                timeout: Duration::from_secs(1),
            },
            &|| false,
        )
        .unwrap();
    assert_eq!(
        output.status(),
        opaal_platform::operational::ProcessExit::Exited(0)
    );
    assert_eq!(output.stdout(), b"descriptor");
}

#[cfg(target_os = "macos")]
#[test]
fn process_is_unsupported_before_pathname_execution_on_macos() {
    let root = TempRoot::new();
    let adapter = PosixOperationalAdapter::new();
    let retained = fs::File::open("/usr/bin/printf").unwrap();
    for executable_file in [None, Some(&retained)] {
        let error = adapter
            .run_process(
                ProcessRequest {
                    executable: Path::new("/usr/bin/printf"),
                    executable_file,
                    argv: &[OsString::from("printf"), OsString::from("unreachable")],
                    environment: &[],
                    cwd: &root.0,
                    stdout_limit: 16,
                    stderr_limit: 0,
                    timeout: Duration::from_secs(1),
                },
                &|| false,
            )
            .unwrap_err();
        assert_eq!(error.kind(), OperationalErrorKind::Unsupported);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn process_term_kill_wait_and_descendant_cleanup_are_bounded() {
    let root = TempRoot::new();
    let adapter = PosixOperationalAdapter::new();
    let started = Instant::now();
    let error = adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/bin/sh"),
                executable_file: None,
                argv: &[
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from("trap '' TERM; while :; do sleep 1; done"),
                ],
                environment: &[],
                cwd: &root.0,
                stdout_limit: 0,
                stderr_limit: 0,
                timeout: Duration::from_millis(100),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::TimedOut);
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "a TERM-ignoring leader reaches the KILL phase"
    );
    assert!(
        started.elapsed() < Duration::from_secs(11),
        "both termination waits and reap stay bounded"
    );

    let started = Instant::now();
    let error = adapter
        .run_process(
            ProcessRequest {
                executable: Path::new("/bin/sh"),
                executable_file: None,
                argv: &[
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from("sleep 30 &"),
                ],
                environment: &[],
                cwd: &root.0,
                stdout_limit: 0,
                stderr_limit: 0,
                timeout: Duration::from_secs(10),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::Protocol);
    assert!(started.elapsed() < Duration::from_secs(6));
}

#[test]
fn ca_only_tls_validates_hostname_and_returns_redirect_status_without_following() {
    let (port, server) = tls_server();
    let adapter = PosixOperationalAdapter::new();
    let response = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port,
                tls_server_name: Some("opaal-golden.invalid"),
                ca_pem: Some(CA),
                method: "GET",
                path_and_query: "/readiness",
                headers: &[],
                secret_header: Some(MaterializedSecretHeader {
                    name: "authorization",
                    value: b"fixture-canary",
                }),
                body: &[],
                max_response_bytes: 1024,
                timeout: Duration::from_secs(2),
            },
            &|| false,
        )
        .unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.body(), b"stay-here");
    assert!(
        server.join().unwrap(),
        "server observed the exact secret sink once"
    );
}

#[test]
fn tls_refuses_wrong_hostname_and_invalid_ca() {
    let adapter = PosixOperationalAdapter::new();
    let error = adapter
        .http_request(
            HttpRequest {
                connect_host: "example.com",
                port: 443,
                tls_server_name: Some("example.com"),
                ca_pem: Some(CA),
                method: "GET",
                path_and_query: "/",
                headers: &[],
                secret_header: None,
                body: &[],
                max_response_bytes: 0,
                timeout: Duration::from_secs(2),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::Unsupported);

    let (port, server) = tls_failure_server();
    let error = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port,
                tls_server_name: Some("wrong.invalid"),
                ca_pem: Some(CA),
                method: "GET",
                path_and_query: "/",
                headers: &[],
                secret_header: None,
                body: &[],
                max_response_bytes: 0,
                timeout: Duration::from_secs(2),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::Tls);
    server.join().unwrap();

    let (port, server) = tls_failure_server();
    let error = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port,
                tls_server_name: Some("opaal-golden.invalid"),
                ca_pem: Some(b"not a PEM certificate"),
                method: "GET",
                path_and_query: "/",
                headers: &[],
                secret_header: None,
                body: &[],
                max_response_bytes: 0,
                timeout: Duration::from_secs(2),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::Tls);
    server.join().unwrap();
}

#[test]
fn tls_certificate_time_validation_refuses_early_and_expired_instants() {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(CA).unwrap())
        .unwrap();
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .unwrap();
    let certificate = CertificateDer::from_pem_slice(SERVER_CERT).unwrap();
    let server_name = ServerName::try_from("opaal-golden.invalid").unwrap();
    assert!(
        verifier
            .verify_server_cert(
                &certificate,
                &[],
                &server_name,
                &[],
                UnixTime::since_unix_epoch(Duration::ZERO),
            )
            .is_err()
    );
    assert!(
        verifier
            .verify_server_cert(
                &certificate,
                &[],
                &server_name,
                &[],
                UnixTime::since_unix_epoch(Duration::from_secs(7_258_118_400)),
            )
            .is_err()
    );
}

#[test]
fn http_response_body_limit_is_inclusive_even_when_headers_and_body_arrive_together() {
    let adapter = PosixOperationalAdapter::new();
    let body = vec![b'x'; 1024];
    let (port, server) = plain_http_server(&body);
    let response = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port,
                tls_server_name: None,
                ca_pem: None,
                method: "GET",
                path_and_query: "/",
                headers: &[],
                secret_header: None,
                body: &[],
                max_response_bytes: body.len(),
                timeout: Duration::from_secs(2),
            },
            &|| false,
        )
        .unwrap();
    assert_eq!(response.body().len(), body.len());
    server.join().unwrap();

    let excess = vec![b'x'; 1025];
    let (port, server) = plain_http_server(&excess);
    let error = adapter
        .http_request(
            HttpRequest {
                connect_host: "127.0.0.1",
                port,
                tls_server_name: None,
                ca_pem: None,
                method: "GET",
                path_and_query: "/",
                headers: &[],
                secret_header: None,
                body: &[],
                max_response_bytes: 1024,
                timeout: Duration::from_secs(2),
            },
            &|| false,
        )
        .unwrap_err();
    assert_eq!(error.kind(), OperationalErrorKind::LimitExceeded);
    server.join().unwrap();
}

fn tls_server() -> (u16, thread::JoinHandle<bool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let certificate = CertificateDer::from_pem_slice(SERVER_CERT).unwrap();
    let key = PrivateKeyDer::from_pem_slice(SERVER_KEY).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
    let task = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let connection = ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = StreamOwned::new(connection, socket);
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).unwrap();
            if count == 0 {
                return false;
            }
            request.extend_from_slice(&chunk[..count]);
        }
        stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nstay-here",
            )
            .unwrap();
        stream.flush().unwrap();
        request
            .windows(b"authorization: fixture-canary".len())
            .filter(|window| *window == b"authorization: fixture-canary")
            .count()
            == 1
    });
    (port, task)
}

fn tls_failure_server() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let certificate = CertificateDer::from_pem_slice(SERVER_CERT).unwrap();
    let key = PrivateKeyDer::from_pem_slice(SERVER_KEY).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .unwrap();
    let task = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let connection = ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = StreamOwned::new(connection, socket);
        let mut byte = [0u8; 1];
        let _ = stream.read(&mut byte);
    });
    (port, task)
}

fn plain_http_server(body: &[u8]) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    let task = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request);
        let _ = socket.write_all(&response);
    });
    (port, task)
}
