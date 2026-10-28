#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opaal_platform::operational::{
    AtomicWriteRequest, FakeOperationalAdapter, HttpRequest, HttpResponse, OperationalAdapter,
    OperationalError, OperationalErrorKind, ProcessExit, ProcessOutput, ProcessRequest,
    ReadFileRequest,
};
use opaal_platform::{
    AuthorityEffect, AuthorityEnforcement, AuthorityProfile, Capabilities, FakePlatform,
};
use opaal_runtime::authority::{
    AuthorityContext, AuthorityRule, CapabilityRequest, EffectSet, EvaluationContextId,
    RequiredEnforcement,
};
use opaal_runtime::context::OperationalContext;
use opaal_runtime::eval::{CancelReason, CancellationToken, FakeClock};
use opaal_runtime::operational::{ModuleError, integrity, process};
use opaal_runtime::project::{ToolLock, parse_project_manifest, parse_tool_lock};

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "controlled_native_tools/acquisition.rs"]
mod acquisition;

#[path = "controlled_native_tools/cancellation.rs"]
mod cancellation;

const NATIVE_MANIFEST: &[u8] = br#"schema_version = 1
[project]
name = "native_test"
root_module = "tasks.opaal"
required_opaal = ">=1.9.0,<2.0.0"
[paths]
root = "."
evidence = "output"
[tools.checker]
adapter = "native"
[environments.ci]
authority = "authority.toml"
tool_lock = "tools.toml"
"#;

// Model the adapter's sealed result independently from the original descriptor.
// Actual Linux seals and descriptor execution are covered by the POSIX observer.
struct SnapshotAdapter {
    snapshot: File,
    original: PathBuf,
    mutate: bool,
    failure: Option<OperationalErrorKind>,
    clock: Arc<FakeClock>,
    acquisition_nanos: u64,
    execution_nanos: u64,
    snapshots: AtomicUsize,
    launches: AtomicUsize,
    cancel_on_launch: Option<Arc<AtomicBool>>,
    timeouts: Mutex<Vec<Duration>>,
    processes: FakeOperationalAdapter,
}

impl OperationalAdapter for SnapshotAdapter {
    fn snapshot_executable(
        &self,
        _source: &File,
        _max_bytes: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<File, OperationalError> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        if self.mutate {
            fs::write(&self.original, b"mutated during acquisition").unwrap();
        }
        self.clock.advance(self.acquisition_nanos);
        if let Some(kind) = self.failure {
            return Err(OperationalError::new(kind, "snapshot failure canary"));
        }
        if cancelled() {
            return Err(OperationalError::new(
                OperationalErrorKind::Cancelled,
                "snapshot cancelled",
            ));
        }
        Ok(self.snapshot.try_clone().unwrap())
    }

    fn run_process(
        &self,
        request: ProcessRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ProcessOutput, OperationalError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        self.timeouts.lock().unwrap().push(request.timeout);
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        use std::os::unix::fs::FileExt;
        let retained = request.executable_file.expect("snapshot, never pathname");
        actual.resize(
            usize::try_from(retained.metadata().unwrap().len()).unwrap(),
            0,
        );
        expected.resize(
            usize::try_from(self.snapshot.metadata().unwrap().len()).unwrap(),
            0,
        );
        retained.read_exact_at(&mut actual, 0).unwrap();
        self.snapshot.read_exact_at(&mut expected, 0).unwrap();
        assert_eq!(actual, expected);
        assert!(request.environment.is_empty());
        if let Some(cancel) = &self.cancel_on_launch {
            cancel.store(true, Ordering::SeqCst);
        }
        self.clock.advance(self.execution_nanos);
        self.processes.run_process(request, cancelled)
    }

    fn read_file(&self, _: ReadFileRequest<'_>) -> Result<Vec<u8>, OperationalError> {
        panic!("native acquisition must use the retained descriptor")
    }
    fn write_atomic(&self, _: AtomicWriteRequest<'_>) -> Result<(), OperationalError> {
        unreachable!()
    }
    fn wall_time_unix_nanos(&self) -> Result<i128, OperationalError> {
        unreachable!()
    }
    fn monotonic_nanos(&self) -> Result<u128, OperationalError> {
        unreachable!()
    }
    fn http_request(
        &self,
        _: HttpRequest<'_>,
        _: &dyn Fn() -> bool,
    ) -> Result<HttpResponse, OperationalError> {
        unreachable!()
    }
}

struct Fixture {
    root: PathBuf,
    source: File,
    lock: ToolLock,
    context: OperationalContext,
    effects: EffectSet,
    platform: FakePlatform,
    adapter: SnapshotAdapter,
}

impl Fixture {
    fn new(original: &[u8], snapshot: &[u8]) -> Self {
        Self::with_token(original, snapshot, CancellationToken::never())
    }

    fn with_token(original: &[u8], snapshot: &[u8], token: CancellationToken) -> Self {
        Self::with_clock(original, snapshot, token, Arc::new(FakeClock::new()))
    }

    fn with_clock(
        original: &[u8],
        snapshot: &[u8],
        token: CancellationToken,
        clock: Arc<FakeClock>,
    ) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "native-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let original_path = root.join("original");
        let snapshot_path = root.join("snapshot");
        fs::write(&original_path, original).unwrap();
        fs::write(&snapshot_path, snapshot).unwrap();
        let manifest =
            parse_project_manifest(Path::new("/project/opaal.toml"), NATIVE_MANIFEST).unwrap();
        let lock = parse_tool_lock(
            &manifest,
            "ci",
            format!(
                r#"schema_version = 1
project = "native_test"
environment = "ci"
platform = "x86_64-unknown-linux-gnu"
[child_environment]
inherit = []
[[tools]]
id = "checker"
adapter = "native"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvY2hlY2tlcg" }}
digest = "{}"
"#,
                integrity::sha256(original)
            )
            .as_bytes(),
        )
        .unwrap();
        let request = CapabilityRequest::process_run("checker").unwrap();
        let context = OperationalContext::new(
            AuthorityContext::new(
                EvaluationContextId::new(71).unwrap(),
                vec![AuthorityRule::grant(
                    request.clone(),
                    RequiredEnforcement::AcknowledgeUnenforced,
                )],
            )
            .unwrap(),
            token,
            clock.clone(),
            None,
        );
        Self {
            source: File::open(&original_path).unwrap(),
            root,
            lock,
            context,
            effects: EffectSet::new([request]),
            platform: FakePlatform::with_authority_profile(
                Capabilities::full(),
                AuthorityProfile::unsupported().with(
                    AuthorityEffect::ProcessRun,
                    AuthorityEnforcement::Unenforced,
                ),
            ),
            adapter: SnapshotAdapter {
                snapshot: File::open(snapshot_path).unwrap(),
                original: original_path,
                mutate: false,
                failure: None,
                clock,
                acquisition_nanos: 0,
                execution_nanos: 0,
                snapshots: AtomicUsize::new(0),
                launches: AtomicUsize::new(0),
                cancel_on_launch: None,
                timeouts: Mutex::new(Vec::new()),
                processes: FakeOperationalAdapter::new(),
            },
        }
    }

    fn run(&self, budget: &mut process::ProcessBudget) -> Result<process::ToolResult, ModuleError> {
        self.run_arguments(&["literal ; $() * ä"], budget)
    }

    fn run_arguments(
        &self,
        arguments: &[&str],
        budget: &mut process::ProcessBudget,
    ) -> Result<process::ToolResult, ModuleError> {
        process::run_retained(
            &self.context,
            &self.effects,
            &self.platform,
            &self.adapter,
            &self.lock,
            "checker",
            arguments,
            Path::new("/project"),
            1024,
            1024,
            Duration::from_secs(1),
            budget,
            &self.source,
        )
    }
}

#[test]
fn native_frozen_shared_acquisition_launch_deadline() {
    let original = image();
    for evaluation_ms in [500, 1000, 2000] {
        let mut fixture = Fixture::new(&original, &original);
        fixture
            .context
            .narrow_deadline(opaal_runtime::lifetime::Deadline::at(
                opaal_runtime::eval::Instant::from_nanos(evaluation_ms * 1_000_000),
            ));
        fixture.adapter.acquisition_nanos = 750_000_000;
        fixture
            .adapter
            .processes
            .push_process(Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                Vec::new(),
                Vec::new(),
                Duration::ZERO,
            )));
        let mut budget = process::ProcessBudget::default();
        let result = fixture.run(&mut budget);
        let timeouts = fixture.adapter.timeouts.lock().unwrap();
        if evaluation_ms == 500 {
            assert!(matches!(
                result,
                Err(ModuleError::Cancelled(CancelReason::Timeout))
            ));
            assert!(timeouts.is_empty());
            assert_eq!(budget.attempts(), 0);
        } else {
            result.unwrap();
            assert_eq!(timeouts.len(), 1);
            assert!(timeouts[0] <= Duration::from_millis(250), "{timeouts:?}");
            assert!(!timeouts[0].is_zero());
            assert_eq!(budget.attempts(), 1);
        }
    }
    observe_image_case(
        "shared-acquisition-launch-deadline",
        serde_json::json!({
            "remaining_launch_ms_at_most": 250,
        }),
    );
}

#[test]
fn native_launch_deadline_keeps_timeout_sticky_after_an_attempt() {
    let original = image();
    let mut fixture = Fixture::new(&original, &original);
    fixture.adapter.acquisition_nanos = 750_000_000;
    fixture.adapter.execution_nanos = 250_000_000;
    let mut budget = process::ProcessBudget::default();
    assert!(matches!(
        fixture.run(&mut budget),
        Err(ModuleError::Cancelled(CancelReason::Timeout))
    ));
    assert_eq!(
        fixture.context.poll_cancellation(),
        Some(CancelReason::Timeout)
    );
    assert_eq!(fixture.adapter.launches.load(Ordering::SeqCst), 1);
    assert_eq!(budget.attempts(), 1);
}

fn observe_image_case(name: &str, observed: serde_json::Value) {
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/controlled-external-tools/image-cases.json"
    ));
    let inventory: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let case = inventory["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap();
    assert_eq!(observed, case["expected"], "{name}");
    let host = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => panic!("unsupported observer host"),
    };
    if !case["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == host)
    {
        return;
    }
    println!(
        "OPAAL_IMAGE_CASE {}",
        serde_json::json!({
            "name": name, "observed": observed, "host": host,
            "inventory_digest": integrity::sha256(bytes),
        })
    );
}

#[test]
fn native_frozen_request_argument_bounds() {
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/controlled-external-tools/image-cases.json"
    ));
    let inventory: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let original = image();
    let mut checked = 0;
    for case in inventory["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let subject = input["subject"].as_str().unwrap_or("");
        if !matches!(
            subject,
            "argv-count-including-zero"
                | "argv-bytes-including-nuls"
                | "argument-bytes-including-nul"
        ) && name != "argument-interior-nul"
        {
            continue;
        }
        let fixture = Fixture::new(&original, &original);
        let value = input["value"].as_u64().unwrap_or(0) as usize;
        let arguments = match subject {
            "argv-count-including-zero" => vec![String::new(); value - 1],
            "argv-bytes-including-nuls" => {
                let mut remaining =
                    value - fixture.lock.tools()["checker"].path().bytes().len() - 1;
                let mut arguments = Vec::new();
                while remaining > 0 {
                    let length = remaining.min(32768);
                    arguments.push("a".repeat(length - 1));
                    remaining -= length;
                }
                arguments
            }
            "argument-bytes-including-nul" => vec!["a".repeat(value - 1)],
            _ => vec!["a\0b".to_owned()],
        };
        let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        fixture
            .adapter
            .processes
            .push_process(Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                Vec::new(),
                Vec::new(),
                Duration::ZERO,
            )));
        let mut budget = process::ProcessBudget::default();
        let result = fixture.run_arguments(&arguments, &mut budget);
        let admitted = result.is_ok();
        if let Err(error) = result {
            assert_eq!(error.code(), "NATIVE004", "{name}");
        }
        assert_eq!(
            fixture.adapter.snapshots.load(Ordering::SeqCst),
            usize::from(admitted)
        );
        assert_eq!(
            fixture.adapter.launches.load(Ordering::SeqCst),
            usize::from(admitted)
        );
        assert_eq!(budget.attempts(), usize::from(admitted));
        let mut observed = serde_json::json!({"admitted": admitted});
        if name == "argument-interior-nul" {
            observed["attempts"] = budget.attempts().into();
        }
        observe_image_case(name, observed);
        checked += 1;
    }
    assert_eq!(checked, 7);
}

#[test]
fn native_frozen_environment_encoded_bounds() {
    let original = image();
    let manifest =
        parse_project_manifest(Path::new("/project/opaal.toml"), NATIVE_MANIFEST).unwrap();
    let value = "YWFh".repeat(16383) + "YWE";
    assert_eq!(value.len(), 65535);
    for (name, variable) in [
        ("environment-encoded-bytes-exact", "A"),
        ("environment-encoded-bytes-first-excess", "AB"),
    ] {
        let lock = parse_tool_lock(
            &manifest,
            "ci",
            format!(
                r#"schema_version = 1
project = "native_test"
environment = "ci"
platform = "x86_64-unknown-linux-gnu"
[child_environment]
inherit = []
[[child_environment.variables]]
name = "{variable}"
value = {{ encoding = "base64url-nopad", platform = "unix", value = "{value}" }}
[[tools]]
id = "checker"
adapter = "native"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvY2hlY2tlcg" }}
digest = "{}"
"#,
                integrity::sha256(&original)
            )
            .as_bytes(),
        );
        match &lock {
            Ok(lock) => {
                assert_eq!(
                    lock.variables()[0].name().len() + lock.variables()[0].value().encoded().len(),
                    65536
                );
                assert_eq!(
                    process::child_environment(lock).unwrap()[0]
                        .1
                        .as_encoded_bytes(),
                    vec![b'a'; 49151]
                );
            }
            Err(error) => assert_eq!(error.code(), "LOCK007"),
        }
        observe_image_case(name, serde_json::json!({"admitted": lock.is_ok()}));
    }
}

#[test]
fn native_frozen_authority_refuses_before_acquisition() {
    let original = image();
    for name in ["missing-effect", "missing-grant", "enforced-grant"] {
        let mut fixture = Fixture::new(&original, &original);
        if name == "missing-effect" {
            fixture.effects = EffectSet::new([]);
        } else {
            let request = CapabilityRequest::process_run("checker").unwrap();
            let rules = if name == "missing-grant" {
                Vec::new()
            } else {
                vec![AuthorityRule::grant(request, RequiredEnforcement::Enforced)]
            };
            fixture.context = OperationalContext::new(
                AuthorityContext::new(EvaluationContextId::new(71).unwrap(), rules).unwrap(),
                CancellationToken::never(),
                fixture.adapter.clock.clone(),
                None,
            );
        }
        let mut budget = process::ProcessBudget::default();
        assert_eq!(fixture.run(&mut budget).unwrap_err().code(), "OPERATION002");
        assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.adapter.launches.load(Ordering::SeqCst), 0);
        observe_image_case(
            name,
            serde_json::json!({"admitted": false, "attempts": budget.attempts()}),
        );
    }
}

#[test]
fn native_frozen_pure_initializer_and_default_embedding_have_no_process_host() {
    use opaal_runtime::eval::{ResourceBudget, RuntimeErrorKind};
    use opaal_runtime::module::{
        ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
    };
    use opaal_runtime::outcome::PrimaryOutcome;
    use opaal_runtime::project::load_project_program;
    use opaal_runtime::script::{execute_module_program_outcome, invoke_pure_function};

    struct Source(String);
    impl ModuleCanonicalizer for Source {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf, ModulePathError> {
            Ok(path.to_path_buf())
        }
    }
    impl ModuleSourceLoader for Source {
        fn load(&self, _: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
            Ok(self.0.as_bytes().to_vec())
        }
    }
    struct NoExecutables;
    impl opaal_runtime::resolve::ExecutableProbe for NoExecutables {
        fn is_executable(&self, _: &std::ffi::OsStr) -> bool {
            panic!("denied native calls must not resolve an executable")
        }
    }
    for (name, body) in [
        (
            "pure-call",
            "export { denied }\ndef denied() -> Bool { let result = process::run(tools::checker, [])\nreturn true }",
        ),
        (
            "module-initializer-call",
            "let denied = process::run(tools::checker, [])",
        ),
        (
            "default-embedding-call",
            "action denied() -> Bool { effects { process.run(tools::checker), }  let result = process::run(tools::checker, [])\nreturn true }\ndenied()",
        ),
    ] {
        let source = Source(format!(
            "import std::process as process\nimport project::tools as tools\n{body}\n"
        ));
        let manifest =
            parse_project_manifest(Path::new("/project/opaal.toml"), NATIVE_MANIFEST).unwrap();
        let project = load_project_program(manifest, &source, &source).unwrap();
        if name == "pure-call" {
            let (module, signature) = project.modules().exported_function("denied").unwrap();
            let error = invoke_pure_function(
                project.modules(),
                &module,
                signature,
                CancellationToken::never(),
                &mut ResourceBudget::opaal(),
            )
            .unwrap_err();
            assert!(
                matches!(error.kind(), RuntimeErrorKind::PolicyRefused { .. }),
                "{error}"
            );
        } else {
            let outcome = execute_module_program_outcome(
                project.modules(),
                &[],
                Path::new("/project"),
                &mut opaal_runtime::Environment::new(),
                &opaal_runtime::builtin::standard_registry(),
                &NoExecutables,
                &opaal_runtime::plan::SessionOptions::default(),
                &FakePlatform::full(),
                Arc::new(FakeClock::new()),
                &mut Vec::new(),
            );
            assert!(
                matches!(outcome.primary(), PrimaryOutcome::Refused(_)),
                "{name}: {outcome:?}"
            );
        }
        // These APIs accept no operational adapter or process budget.
        observe_image_case(name, serde_json::json!({"attempts": 0, "admitted": false}));
    }
}

#[test]
fn native_frozen_eight_attempts_and_ninth_refusal() {
    let original = image();
    let fixture = Fixture::new(&original, &original);
    let mut budget = process::ProcessBudget::default();
    for _ in 0..8 {
        fixture
            .adapter
            .processes
            .push_process(Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                Vec::new(),
                Vec::new(),
                Duration::ZERO,
            )));
        fixture.run(&mut budget).unwrap();
    }
    let calls = fixture.adapter.processes.calls();
    assert_eq!(calls.len(), 8);
    let probes = calls
        .iter()
        .filter(|call| {
            matches!(call,
        opaal_platform::operational::OperationalCall::Process { argv, .. }
            if argv.get(1).is_some_and(|arg| arg == "--version"))
        })
        .count();
    observe_image_case(
        "shared-eight-attempts",
        serde_json::json!({
            "attempts": budget.attempts(), "implicit_probes": probes,
        }),
    );
    assert_eq!(fixture.run(&mut budget).unwrap_err().code(), "PROCESS001");
    assert_eq!(fixture.adapter.launches.load(Ordering::SeqCst), 8);
    assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 8);
    observe_image_case(
        "ninth-attempt-refuses",
        serde_json::json!({
            "attempts": budget.attempts(), "exit": 2,
        }),
    );
}

#[test]
fn native_frozen_maintained_probe_shares_native_attempt_budget() {
    let original = image();
    let fixture = Fixture::new(&original, &original);
    let manifest = parse_project_manifest(
        Path::new("/project/opaal.toml"),
        &String::from_utf8(NATIVE_MANIFEST.to_vec())
            .unwrap()
            .replace(
                "adapter = \"native\"",
                "adapter = \"git\"\nversion = \">=2.0.0,<3.0.0\"",
            )
            .into_bytes(),
    )
    .unwrap();
    let variables = ["HOME", "TMPDIR", "PATH", "CARGO_HOME", "RUSTC", "RUSTDOC", "LC_ALL", "TZ", "CARGO_NET_OFFLINE", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL"]
        .into_iter().map(|name| format!("[[child_environment.variables]]\nname = \"{name}\"\nvalue = {{ encoding = \"base64url-nopad\", platform = \"unix\", value = \"eA\" }}\n"))
        .collect::<String>();
    let lock = parse_tool_lock(
        &manifest,
        "ci",
        format!(
            r#"schema_version = 1
project = "native_test"
environment = "ci"
platform = "x86_64-unknown-linux-gnu"
[child_environment]
inherit = []
{variables}
[[tools]]
id = "checker"
adapter = "git"
version = "2.50.0"
path = {{ encoding = "base64url-nopad", platform = "unix", value = "L3Rvb2wvY2hlY2tlcg" }}
digest = "{}"
"#,
            integrity::sha256(&original)
        )
        .as_bytes(),
    )
    .unwrap();
    let adapter = FakeOperationalAdapter::new();
    adapter.insert_file("/tool/checker", original);
    adapter.push_process(Ok(ProcessOutput::new(
        ProcessExit::Exited(0),
        b"git version 2.50.0\n".to_vec(),
        Vec::new(),
        Duration::ZERO,
    )));
    let mut budget = process::ProcessBudget::default();
    process::probe(
        &fixture.context,
        &fixture.effects,
        &fixture.platform,
        &adapter,
        &lock,
        "checker",
        Path::new("/project"),
        &mut budget,
    )
    .unwrap();
    assert_eq!(budget.attempts(), 1);
    for _ in 0..7 {
        fixture
            .adapter
            .processes
            .push_process(Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                Vec::new(),
                Vec::new(),
                Duration::ZERO,
            )));
        fixture.run(&mut budget).unwrap();
    }
    assert_eq!(fixture.run(&mut budget).unwrap_err().code(), "PROCESS001");
    assert_eq!(
        adapter
            .calls()
            .iter()
            .filter(|call| matches!(call,
        opaal_platform::operational::OperationalCall::Process { argv, .. }
            if argv == &["/tool/checker", "--version"]))
            .count(),
        1
    );
    assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 7);
    assert_eq!(fixture.adapter.launches.load(Ordering::SeqCst), 7);
    observe_image_case(
        "maintained-probe-shares-budget",
        serde_json::json!({"attempts": budget.attempts(), "exit": 2}),
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn image() -> Vec<u8> {
    let mut bytes = vec![0; 128 * 1024];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2_u16.to_le_bytes());
    let machine = if cfg!(target_arch = "aarch64") {
        183_u16
    } else {
        62_u16
    };
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&1_u16.to_le_bytes());
    bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
    bytes
}

#[test]
fn original_and_post_snapshot_identity_fail_before_any_attempt() {
    let original = image();
    let mut corrupted = original.clone();
    corrupted[120] = 1;
    let cases = [
        (original.as_slice(), corrupted.as_slice(), "NATIVE003"),
        (b"#!/bin/sh\n", b"#!/bin/sh\n", "NATIVE001"),
    ];
    for (original, snapshot, code) in cases {
        let mut fixture = Fixture::new(original, snapshot);
        fixture.adapter.mutate = true;
        let mut budget = process::ProcessBudget::default();
        assert_eq!(fixture.run(&mut budget).unwrap_err().code(), code);
        assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 1);
        assert_eq!(budget.attempts(), 0);
        assert!(fixture.adapter.processes.calls().is_empty());
    }
    let fixture = Fixture::new(&original, &original);
    fs::write(&fixture.adapter.original, b"changed before acquisition").unwrap();
    let mut budget = process::ProcessBudget::default();
    assert_eq!(fixture.run(&mut budget).unwrap_err().code(), "PROCESS003");
    assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 0);
    assert_eq!(budget.attempts(), 0);
}

#[test]
fn acquisition_failure_and_deadline_never_launch_or_charge() {
    let original = image();
    for (failure, elapsed) in [
        (Some(OperationalErrorKind::Unsupported), 0),
        (Some(OperationalErrorKind::Unsupported), 1_000_000_000),
        (None, 1_000_000_000),
    ] {
        let mut fixture = Fixture::new(&original, &original);
        fixture.adapter.failure = failure;
        fixture.adapter.acquisition_nanos = elapsed;
        let mut budget = process::ProcessBudget::default();
        let error = fixture.run(&mut budget).unwrap_err();
        if elapsed == 0 {
            assert_eq!(error.code(), "NATIVE002");
            assert!(!error.to_string().contains("canary"));
        } else {
            assert!(matches!(
                error,
                ModuleError::Cancelled(CancelReason::Timeout)
            ));
        }
        assert_eq!(budget.attempts(), 0);
        assert!(fixture.adapter.processes.calls().is_empty());
    }
}

#[test]
fn accepted_snapshot_survives_original_mutation_during_acquisition() {
    let original = image();
    let mut fixture = Fixture::new(&original, &original);
    fixture.adapter.mutate = true;
    fixture
        .adapter
        .processes
        .push_process(Ok(ProcessOutput::new(
            ProcessExit::Signaled(10),
            Vec::new(),
            Vec::new(),
            Duration::from_millis(1),
        )));
    let mut budget = process::ProcessBudget::default();
    let result = fixture.run(&mut budget).unwrap();
    assert!(!result.status().is_ok());
    assert_eq!(budget.attempts(), 1);
    assert_eq!(
        fs::read(&fixture.adapter.original).unwrap(),
        b"mutated during acquisition"
    );
    assert!(matches!(
        &fixture.adapter.processes.calls()[0],
        opaal_platform::operational::OperationalCall::Process { argv, .. }
            if argv == &["/tool/checker", "literal ; $() * ä"]
    ));
}

#[test]
fn acquisition_hash_and_launch_boundaries_preserve_requested_cancellation() {
    let original = image();
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = polls.clone();
    let fixture = Fixture::with_token(
        &original,
        &original,
        CancellationToken::from_fn(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            false
        }),
    );
    fixture
        .adapter
        .processes
        .push_process(Ok(ProcessOutput::new(
            ProcessExit::Exited(0),
            Vec::new(),
            Vec::new(),
            Duration::ZERO,
        )));
    fixture.run(&mut process::ProcessBudget::default()).unwrap();
    assert!(polls.load(Ordering::SeqCst) > 10);
    for boundary in 0..polls.load(Ordering::SeqCst) {
        let counter = AtomicUsize::new(0);
        let fixture = Fixture::with_token(
            &original,
            &original,
            CancellationToken::from_fn(move || counter.fetch_add(1, Ordering::SeqCst) >= boundary),
        );
        fixture
            .adapter
            .processes
            .push_process(Ok(ProcessOutput::new(
                ProcessExit::Exited(0),
                Vec::new(),
                Vec::new(),
                Duration::ZERO,
            )));
        let mut budget = process::ProcessBudget::default();
        assert!(
            matches!(
                fixture.run(&mut budget),
                Err(ModuleError::Cancelled(CancelReason::Requested))
            ),
            "poll {boundary}"
        );
        assert!(budget.attempts() <= 1);
        assert_eq!(
            fixture.adapter.launches.load(Ordering::SeqCst),
            budget.attempts()
        );
    }
}

#[test]
fn native_attempts_share_the_eight_attempt_budget_without_probes_or_retries() {
    let original = image();
    let fixture = Fixture::new(&original, &original);
    let mut budget = process::ProcessBudget::default();
    for attempt in 0..process::MAX_PROCESS_ATTEMPTS {
        if attempt == 0 {
            fixture
                .adapter
                .processes
                .push_process(Err(OperationalError::new(
                    OperationalErrorKind::Io(std::io::ErrorKind::PermissionDenied),
                    "spawn failed",
                )));
            assert!(fixture.run(&mut budget).is_err());
        } else {
            fixture
                .adapter
                .processes
                .push_process(Ok(ProcessOutput::new(
                    ProcessExit::Exited(17),
                    b"stdout".to_vec(),
                    b"stderr".to_vec(),
                    Duration::from_millis(1),
                )));
            let result = fixture.run(&mut budget).unwrap();
            assert!(!result.status().is_ok());
            assert_eq!(result.stdout(), b"stdout");
            assert_eq!(result.stderr(), b"stderr");
        }
        assert_eq!(budget.attempts(), attempt + 1);
        assert_eq!(fixture.adapter.processes.calls().len(), attempt + 1);
    }
    assert_eq!(fixture.run(&mut budget).unwrap_err().code(), "PROCESS001");
    assert_eq!(fixture.adapter.snapshots.load(Ordering::SeqCst), 8);
    assert_eq!(fixture.adapter.processes.calls().len(), 8);
}
