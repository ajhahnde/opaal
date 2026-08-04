#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_cli::config::{
    ConfigDefaults, ConfigEnvironment, ConfigFailureKind, ConfigFile, ConfigFileError,
    ConfigInvocation, ConfigLimits, ConfigPlatform, ConfigRequest, ConfigSource, ConfigStatus,
    HostConfigSource, initialize_config,
};

#[derive(Default)]
struct FakeEnvironment {
    values: Vec<(OsString, OsString)>,
    reads: Cell<usize>,
}

impl FakeEnvironment {
    fn with(values: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            reads: Cell::new(0),
        }
    }
}

impl ConfigEnvironment for FakeEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        self.reads.set(self.reads.get() + 1);
        self.values
            .iter()
            .find_map(|(candidate, value)| (candidate == name).then(|| value.clone()))
    }
}

struct RecordingSource {
    calls: RefCell<Vec<PathBuf>>,
    responses: HashMap<PathBuf, Result<ConfigFile, ConfigFileError>>,
    default_response: Result<ConfigFile, ConfigFileError>,
}

impl RecordingSource {
    fn absent() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            responses: HashMap::new(),
            default_response: Ok(ConfigFile::Absent),
        }
    }

    fn with_response(
        mut self,
        path: impl Into<PathBuf>,
        response: Result<ConfigFile, ConfigFileError>,
    ) -> Self {
        self.responses.insert(path.into(), response);
        self
    }

    fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }

    fn call_paths(&self) -> Vec<PathBuf> {
        self.calls.borrow().clone()
    }
}

impl ConfigSource for RecordingSource {
    fn load(&self, path: &Path, _source_limit: usize) -> Result<ConfigFile, ConfigFileError> {
        self.calls.borrow_mut().push(path.to_path_buf());
        if let Some(res) = self.responses.get(path) {
            res.clone()
        } else {
            self.default_response.clone()
        }
    }
}

#[test]
fn only_interactive_enabled_startup_performs_discovery() {
    let environment = FakeEnvironment::with([("HOME", "/users/test")]);
    let source = RecordingSource::absent();
    let defaults = ConfigDefaults::default();

    let disabled = initialize_config(
        ConfigRequest::new(ConfigInvocation::Interactive, true, ConfigPlatform::Linux),
        &environment,
        &source,
        &defaults,
        &ConfigLimits::test_default(),
    )
    .expect("disabled config is a clean startup");
    assert_eq!(disabled.metadata().status(), ConfigStatus::Disabled);

    for invocation in [
        ConfigInvocation::Script,
        ConfigInvocation::Command,
        ConfigInvocation::BatchStdin,
        ConfigInvocation::Check,
        ConfigInvocation::Format,
        ConfigInvocation::Help,
        ConfigInvocation::Version,
    ] {
        let startup = initialize_config(
            ConfigRequest::new(invocation, false, ConfigPlatform::Linux),
            &environment,
            &source,
            &defaults,
            &ConfigLimits::test_default(),
        )
        .expect("non-interactive config is ineligible");
        assert_eq!(startup.metadata().status(), ConfigStatus::Ineligible);
    }

    assert_eq!(environment.reads.get(), 0);
    assert_eq!(source.call_count(), 0);
}

#[test]
fn platform_path_selection_is_single_native_and_missing_is_clean() {
    let source = RecordingSource::absent();
    let defaults = ConfigDefaults::default();
    let explicit = FakeEnvironment::with([
        ("XDG_CONFIG_HOME", "/state/config"),
        ("HOME", "/users/test"),
    ]);
    let startup = initialize_config(
        ConfigRequest::new(ConfigInvocation::Interactive, false, ConfigPlatform::Linux),
        &explicit,
        &source,
        &defaults,
        &ConfigLimits::test_default(),
    )
    .expect("missing selected config is clean");
    assert_eq!(startup.metadata().status(), ConfigStatus::Absent);
    assert_eq!(
        startup.metadata().selected_path(),
        Some(Path::new("/state/config/flash/config.fsh"))
    );
    assert!(startup.diagnostic().is_none());

    let linux_fallback =
        FakeEnvironment::with([("XDG_CONFIG_HOME", "relative"), ("HOME", "/users/test")]);
    let startup = initialize_config(
        ConfigRequest::new(ConfigInvocation::Interactive, false, ConfigPlatform::Linux),
        &linux_fallback,
        &source,
        &defaults,
        &ConfigLimits::test_default(),
    )
    .expect("Linux home fallback should select one path");
    assert_eq!(
        startup.metadata().selected_path(),
        Some(Path::new("/users/test/.config/flash/config.fsh"))
    );

    let fallback =
        FakeEnvironment::with([("XDG_CONFIG_HOME", "relative"), ("HOME", "/Users/test")]);
    let startup = initialize_config(
        ConfigRequest::new(ConfigInvocation::Interactive, false, ConfigPlatform::MacOs),
        &fallback,
        &source,
        &defaults,
        &ConfigLimits::test_default(),
    )
    .expect("macOS home fallback should select one path");
    assert_eq!(
        startup.metadata().selected_path(),
        Some(Path::new(
            "/Users/test/Library/Application Support/flash/config.fsh"
        ))
    );
}

#[test]
fn legacy_config_fallback_policy_and_diagnose_paths() {
    let environment = FakeEnvironment::with([("HOME", "/users/test")]);
    let defaults = ConfigDefaults::default();
    let limits = ConfigLimits::test_default();
    let request = ConfigRequest::new(ConfigInvocation::Interactive, false, ConfigPlatform::Linux);

    let primary_path = PathBuf::from("/users/test/.config/flash/config.fsh");
    let legacy_path = PathBuf::from("/users/test/.config/flashshell/config.fsh");

    // 10: Fehlen beide, ist der Status Absent und selected_path zeigt auf den kanonischen Pfad.
    // 6: Der kanonische Pfad wird vor dem Legacy-Pfad abgefragt.
    let source_both_absent = RecordingSource::absent();
    let startup = initialize_config(
        request,
        &environment,
        &source_both_absent,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::Absent);
    assert_eq!(
        startup.metadata().selected_path(),
        Some(primary_path.as_path())
    );
    assert_eq!(
        source_both_absent.call_paths(),
        vec![primary_path.clone(), legacy_path.clone()]
    );

    // 7: Ist die kanonische Datei vorhanden, wird die Legacy-Datei nicht abgefragt.
    let source_primary_only = RecordingSource::absent().with_response(
        &primary_path,
        Ok(ConfigFile::Source("let a = 1\n".to_owned())),
    );
    let startup = initialize_config(
        request,
        &environment,
        &source_primary_only,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::Loaded);
    assert_eq!(
        startup.metadata().selected_path(),
        Some(primary_path.as_path())
    );
    assert_eq!(source_primary_only.call_paths(), vec![primary_path.clone()]);

    // 8: Fehlt die kanonische Datei und existiert die Legacy-Datei, wird die Legacy-Datei geladen.
    // 15: Die tatsächlich geladene Legacy-Datei erscheint in metadata.selected_path().
    let source_legacy_only = RecordingSource::absent().with_response(
        &legacy_path,
        Ok(ConfigFile::Source("let b = 2\n".to_owned())),
    );
    let startup = initialize_config(
        request,
        &environment,
        &source_legacy_only,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::Loaded);
    assert_eq!(
        startup.metadata().selected_path(),
        Some(legacy_path.as_path())
    );
    assert_eq!(
        source_legacy_only.call_paths(),
        vec![primary_path.clone(), legacy_path.clone()]
    );

    // 9: Sind beide vorhanden, gewinnt die kanonische Datei.
    let source_both_present = RecordingSource::absent()
        .with_response(
            &primary_path,
            Ok(ConfigFile::Source("let val = 'primary'\n".to_owned())),
        )
        .with_response(
            &legacy_path,
            Ok(ConfigFile::Source("let val = 'legacy'\n".to_owned())),
        );
    let startup = initialize_config(
        request,
        &environment,
        &source_both_present,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::Loaded);
    assert_eq!(
        startup.metadata().selected_path(),
        Some(primary_path.as_path())
    );
    assert_eq!(source_both_present.call_paths(), vec![primary_path.clone()]);

    // 11: Ein Trust-Fehler am kanonischen Pfad löst Safe Mode aus und darf keinen Legacy-Fallback auslösen.
    let source_primary_trust = RecordingSource::absent()
        .with_response(
            &primary_path,
            Err(ConfigFileError::trust("untrusted primary")),
        )
        .with_response(
            &legacy_path,
            Ok(ConfigFile::Source("let c = 3\n".to_owned())),
        );
    let startup = initialize_config(
        request,
        &environment,
        &source_primary_trust,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::SafeMode);
    assert_eq!(
        startup.metadata().failure().unwrap().kind(),
        ConfigFailureKind::ConfigTrust
    );
    assert_eq!(
        source_primary_trust.call_paths(),
        vec![primary_path.clone()]
    );

    // 12: Ein Read-Fehler am kanonischen Pfad löst Safe Mode aus und darf keinen Legacy-Fallback auslösen.
    let source_primary_read = RecordingSource::absent()
        .with_response(
            &primary_path,
            Err(ConfigFileError::read("cannot read primary")),
        )
        .with_response(
            &legacy_path,
            Ok(ConfigFile::Source("let c = 3\n".to_owned())),
        );
    let startup = initialize_config(
        request,
        &environment,
        &source_primary_read,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::SafeMode);
    assert_eq!(
        startup.metadata().failure().unwrap().kind(),
        ConfigFailureKind::ConfigRead
    );
    assert_eq!(source_primary_read.call_paths(), vec![primary_path.clone()]);

    // 13: Ein Budget-Fehler am kanonischen Pfad löst Safe Mode aus und darf keinen Legacy-Fallback auslösen.
    let source_primary_budget = RecordingSource::absent()
        .with_response(
            &primary_path,
            Err(ConfigFileError::budget("budget exceeded")),
        )
        .with_response(
            &legacy_path,
            Ok(ConfigFile::Source("let c = 3\n".to_owned())),
        );
    let startup = initialize_config(
        request,
        &environment,
        &source_primary_budget,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::SafeMode);
    assert_eq!(
        startup.metadata().failure().unwrap().kind(),
        ConfigFailureKind::ConfigBudget
    );
    assert_eq!(
        source_primary_budget.call_paths(),
        vec![primary_path.clone()]
    );

    // 14: Fehlt der kanonische Pfad und ist die Legacy-Datei unsicher, wird deren Trust-Fehler gemeldet.
    let source_legacy_untrusted = RecordingSource::absent().with_response(
        &legacy_path,
        Err(ConfigFileError::trust("untrusted legacy")),
    );
    let startup = initialize_config(
        request,
        &environment,
        &source_legacy_untrusted,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup.metadata().status(), ConfigStatus::SafeMode);
    assert_eq!(
        startup.metadata().selected_path(),
        Some(legacy_path.as_path())
    );
    assert_eq!(
        startup.metadata().failure().unwrap().kind(),
        ConfigFailureKind::ConfigTrust
    );

    // 16: Parse- und Runtime-Diagnosen verwenden den tatsächlich ausgewählten Dateipfad.
    let source_legacy_parse = RecordingSource::absent().with_response(
        &legacy_path,
        Ok(ConfigFile::Source("let invalid = \n".to_owned())),
    );
    let startup_parse = initialize_config(
        request,
        &environment,
        &source_legacy_parse,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup_parse.metadata().status(), ConfigStatus::SafeMode);
    assert_eq!(
        startup_parse.metadata().selected_path(),
        Some(legacy_path.as_path())
    );
    assert!(
        startup_parse
            .diagnostic()
            .unwrap()
            .contains("/users/test/.config/flashshell/config.fsh")
    );

    let source_legacy_runtime = RecordingSource::absent().with_response(
        &legacy_path,
        Ok(ConfigFile::Source(
            "let error = $nonexistent_var\n".to_owned(),
        )),
    );
    let startup_runtime = initialize_config(
        request,
        &environment,
        &source_legacy_runtime,
        &defaults,
        &limits,
    )
    .unwrap();
    assert_eq!(startup_runtime.metadata().status(), ConfigStatus::SafeMode);
    assert_eq!(
        startup_runtime.metadata().selected_path(),
        Some(legacy_path.as_path())
    );
    assert!(
        startup_runtime
            .diagnostic()
            .unwrap()
            .contains("/users/test/.config/flashshell/config.fsh")
    );
}

#[test]
fn host_source_follows_a_symlink_but_rejects_the_opened_untrusted_object() {
    let directory = TestDirectory::new("config-trust");
    let trusted = directory.path().join("trusted.fsh");
    let link = directory.path().join("config.fsh");
    write_file(&trusted, 0o600, b"let loaded = 7\n");
    symlink(&trusted, &link).expect("test symlink should be created");

    assert_eq!(
        HostConfigSource
            .load(&link, 1024)
            .expect("trusted symlink target should load"),
        ConfigFile::Source("let loaded = 7\n".to_owned())
    );

    fs::set_permissions(&trusted, fs::Permissions::from_mode(0o622))
        .expect("test permissions should change");
    let error = HostConfigSource
        .load(&link, 1024)
        .expect_err("group/other-writable target must be rejected");
    assert!(error.is_trust_failure());
}

#[test]
fn host_source_enforces_exact_size_and_utf8_without_truncation() {
    let directory = TestDirectory::new("config-bounds");
    let exact = directory.path().join("exact.fsh");
    let invalid = directory.path().join("invalid.fsh");
    write_file(&exact, 0o600, b"1234");
    write_file(&invalid, 0o600, b"ok\xff");

    assert_eq!(
        HostConfigSource
            .load(&exact, 4)
            .expect("exact source limit should load"),
        ConfigFile::Source("1234".to_owned())
    );
    assert!(
        HostConfigSource
            .load(&exact, 3)
            .expect_err("one byte over the source limit must fail")
            .is_budget_failure()
    );
    assert!(
        HostConfigSource
            .load(&invalid, 4)
            .expect_err("invalid UTF-8 must fail")
            .is_read_failure()
    );
    assert!(
        HostConfigSource
            .load(directory.path(), 4)
            .expect_err("an opened directory must fail trust")
            .is_trust_failure()
    );
}

fn write_file(path: &Path, mode: u32, bytes: &[u8]) {
    use std::io::Write;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .expect("test file should be created");
    file.write_all(bytes).expect("test bytes should write");
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("flash-{label}-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
