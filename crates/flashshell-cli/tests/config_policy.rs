#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flashshell_cli::config::{
    ConfigDefaults, ConfigEnvironment, ConfigFile, ConfigFileError, ConfigInvocation, ConfigLimits,
    ConfigPlatform, ConfigRequest, ConfigSource, ConfigStatus, HostConfigSource, initialize_config,
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
    calls: Cell<usize>,
    file: Result<ConfigFile, ConfigFileError>,
}

impl RecordingSource {
    fn absent() -> Self {
        Self {
            calls: Cell::new(0),
            file: Ok(ConfigFile::Absent),
        }
    }
}

impl ConfigSource for RecordingSource {
    fn load(&self, _path: &Path, _source_limit: usize) -> Result<ConfigFile, ConfigFileError> {
        self.calls.set(self.calls.get() + 1);
        self.file.clone()
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
    assert_eq!(source.calls.get(), 0);
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
        Some(Path::new("/state/config/flashshell/config.fsh"))
    );
    assert!(startup.diagnostic().is_none());

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
            "/Users/test/Library/Application Support/flashshell/config.fsh"
        ))
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
        let path =
            std::env::temp_dir().join(format!("flashshell-{label}-{}-{id}", std::process::id()));
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
