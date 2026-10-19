use super::*;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, PermissionsExt};

use opaal_platform_posix::operational::PosixOperationalAdapter;

static OBSERVER: Mutex<()> = Mutex::new(());

#[test]
fn image_frozen_real_spawn_and_missing_loader_fail_without_fallback() {
    let _observer = OBSERVER.lock().unwrap();
    assert!(snapshots().is_empty());
    for name in ["failed-spawn", "missing-interpreter"] {
        let mut original = image();
        if name == "missing-interpreter" {
            let loader = b"/opaal-absent-native-test-loader\0";
            assert!(!Path::new("/opaal-absent-native-test-loader").exists());
            original[56..58].copy_from_slice(&2_u16.to_le_bytes());
            original[120..124].copy_from_slice(&3_u32.to_le_bytes());
            original[128..136].copy_from_slice(&176_u64.to_le_bytes());
            original[152..160].copy_from_slice(&(loader.len() as u64).to_le_bytes());
            original[176..176 + loader.len()].copy_from_slice(loader);
        }
        process::validate_native_image(&original, &|| Ok(())).unwrap();
        let fixture = Fixture::new(&original, &original);
        fs::set_permissions(&fixture.adapter.original, fs::Permissions::from_mode(0o700)).unwrap();
        let cwd = if name == "failed-spawn" {
            fixture.root.join("missing-directory")
        } else {
            fixture.root.clone()
        };
        let mut budget = process::ProcessBudget::default();
        println!("OPAAL_LAUNCH_FAILURE_START {name}");
        let error = process::run_retained(
            &fixture.context,
            &fixture.effects,
            &fixture.platform,
            &PosixOperationalAdapter::new(),
            &fixture.lock,
            "checker",
            &[],
            &cwd,
            1024,
            1024,
            Duration::from_secs(1),
            &mut budget,
            &fixture.source,
        )
        .unwrap_err();
        assert!(
            matches!(error, ModuleError::Adapter(ref cause)
            if cause.kind() == OperationalErrorKind::Io(std::io::ErrorKind::NotFound)),
            "{error:?}"
        );
        assert_eq!(budget.attempts(), 1);
        assert!(snapshots().is_empty());
        observe_image_case(
            name,
            serde_json::json!({
                "exit": 2, "attempts": budget.attempts(), "fallback_attempts": 0,
            }),
        );
    }
}

fn snapshots() -> Vec<PathBuf> {
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            fs::read_link(path).is_ok_and(|target| {
                target
                    .as_os_str()
                    .as_encoded_bytes()
                    .starts_with(b"/memfd:opaal-native")
            })
        })
        .collect()
}

struct Probe {
    phase: AtomicUsize,
    polls: AtomicUsize,
    target: usize,
    boundary: usize,
    reason: CancelReason,
    clock: Arc<FakeClock>,
}

impl Probe {
    fn enter(&self, phase: usize) {
        self.polls.store(0, Ordering::SeqCst);
        self.phase.store(phase, Ordering::SeqCst);
    }

    fn poll(&self) -> bool {
        if self.phase.load(Ordering::SeqCst) != self.target
            || self.polls.fetch_add(1, Ordering::SeqCst) + 1 < self.boundary
        {
            return false;
        }
        let handles = snapshots();
        assert_eq!(handles.len(), 1);
        assert_eq!(
            fs::metadata(&handles[0]).unwrap().len(),
            if self.target == 1 {
                64 * 1024
            } else {
                128 * 1024
            },
        );
        if self.reason == CancelReason::Timeout {
            self.clock.advance(1_000_000_000);
            false
        } else {
            true
        }
    }
}

struct Adapter {
    probe: Arc<Probe>,
    mutate: bool,
    unreadable: bool,
    acquisitions: AtomicUsize,
    launches: AtomicUsize,
}

impl OperationalAdapter for Adapter {
    fn snapshot_executable(
        &self,
        source: &File,
        max_bytes: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<File, OperationalError> {
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        if self.mutate {
            let writer = fs::OpenOptions::new()
                .write(true)
                .open(format!("/proc/self/fd/{}", source.as_raw_fd()))
                .unwrap();
            writer
                .write_all_at(b"changed before sealing", 4096)
                .unwrap();
        }
        self.probe.enter(1);
        let snapshot =
            PosixOperationalAdapter::new().snapshot_executable(source, max_bytes, cancelled)?;
        self.probe.enter(2);
        if self.unreadable {
            let write_only = fs::OpenOptions::new()
                .write(true)
                .open(format!("/proc/self/fd/{}", snapshot.as_raw_fd()))
                .unwrap();
            Ok(write_only)
        } else {
            Ok(snapshot)
        }
    }

    fn run_process(
        &self,
        request: ProcessRequest<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ProcessOutput, OperationalError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        PosixOperationalAdapter::new().run_process(request, cancelled)
    }

    fn read_file(&self, _: ReadFileRequest<'_>) -> Result<Vec<u8>, OperationalError> {
        panic!("native acquisition must retain the original descriptor")
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

fn run(
    fixture: &Fixture,
    adapter: &Adapter,
    budget: &mut process::ProcessBudget,
) -> Result<process::ToolResult, ModuleError> {
    fs::set_permissions(&fixture.adapter.original, fs::Permissions::from_mode(0o700)).unwrap();
    process::run_retained(
        &fixture.context,
        &fixture.effects,
        &fixture.platform,
        adapter,
        &fixture.lock,
        "checker",
        &[],
        Path::new("/project"),
        1024,
        1024,
        Duration::from_secs(1),
        budget,
        &fixture.source,
    )
}

#[test]
fn native_frozen_acquisition_phases_preserve_requested_and_deadline_causes() {
    let _observer = OBSERVER.lock().unwrap();
    assert!(snapshots().is_empty());
    for (phase, target, boundary) in [
        // The fixed image has two hash chunks and one ELF program header.
        ("copy", 1, 4),
        ("sealed-hash", 2, 3),
        ("elf-validation", 2, 7),
        ("before-launch", 2, 9),
    ] {
        for reason in [CancelReason::Requested, CancelReason::Timeout] {
            let cause = if reason == CancelReason::Requested {
                "requested"
            } else {
                "deadline"
            };
            let name = format!("{phase}-{cause}");
            let original = image();
            let probe = Arc::new(Probe {
                phase: AtomicUsize::new(0),
                polls: AtomicUsize::new(0),
                target,
                boundary,
                reason,
                clock: Arc::new(FakeClock::new()),
            });
            let predicate = probe.clone();
            let fixture = Fixture::with_clock(
                &original,
                &original,
                CancellationToken::from_fn(move || predicate.poll()),
                probe.clock.clone(),
            );
            let adapter = Adapter {
                probe: probe.clone(),
                mutate: false,
                unreadable: false,
                acquisitions: AtomicUsize::new(0),
                launches: AtomicUsize::new(0),
            };
            let mut budget = process::ProcessBudget::default();
            println!("OPAAL_ACQUISITION_START {name}");
            assert!(
                matches!(run(&fixture, &adapter, &mut budget), Err(ModuleError::Cancelled(actual)) if actual == reason)
            );
            assert_eq!(probe.phase.load(Ordering::SeqCst), target);
            assert_eq!(probe.polls.load(Ordering::SeqCst), boundary);
            assert_eq!(adapter.acquisitions.load(Ordering::SeqCst), 1);
            assert_eq!(adapter.launches.load(Ordering::SeqCst), 0);
            assert_eq!(budget.attempts(), 0);
            assert!(snapshots().is_empty());
            observe_image_case(
                &name,
                serde_json::json!({
                    "exit": if reason == CancelReason::Requested { 130 } else { 2 },
                    "attempts": budget.attempts(), "remaining_snapshot_handles": snapshots().len(),
                }),
            );
        }
    }
}

#[test]
fn native_frozen_snapshot_identity_and_read_failures_release_handles() {
    let _observer = OBSERVER.lock().unwrap();
    assert!(snapshots().is_empty());
    for (name, mutate, unreadable, code) in [
        (
            "snapshot-original-stale-before-acquisition",
            false,
            false,
            "PROCESS003",
        ),
        (
            "snapshot-preseal-mutation-mismatches-lock",
            true,
            false,
            "NATIVE003",
        ),
        ("snapshot-post-seal-read-failure", false, true, "NATIVE002"),
    ] {
        let original = image();
        let fixture = Fixture::new(&original, &original);
        if !mutate && !unreadable {
            fs::write(&fixture.adapter.original, b"stale original").unwrap();
        }
        let adapter = Adapter {
            probe: Arc::new(Probe {
                phase: AtomicUsize::new(0),
                polls: AtomicUsize::new(0),
                target: 0,
                boundary: usize::MAX,
                reason: CancelReason::Requested,
                clock: fixture.adapter.clock.clone(),
            }),
            mutate,
            unreadable,
            acquisitions: AtomicUsize::new(0),
            launches: AtomicUsize::new(0),
        };
        let mut budget = process::ProcessBudget::default();
        println!("OPAAL_ACQUISITION_START {name}");
        assert_eq!(
            run(&fixture, &adapter, &mut budget).unwrap_err().code(),
            code
        );
        assert_eq!(
            adapter.acquisitions.load(Ordering::SeqCst),
            usize::from(mutate || unreadable)
        );
        assert_eq!(adapter.launches.load(Ordering::SeqCst), 0);
        assert_eq!(budget.attempts(), 0);
        assert!(snapshots().is_empty());
        observe_image_case(
            name,
            serde_json::json!({
                "error": "refused", "attempts": budget.attempts(), "fallback_attempts": 0,
                "remaining_snapshot_handles": snapshots().len(),
            }),
        );
    }
}
