#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flashshell_cli::history::{
    DEFAULT_HISTORY_CAPACITY, EditorHistory, HistoryEnvironment, HistoryPlatform, HistorySelection,
    select_history,
};

#[derive(Default)]
struct FakeEnvironment {
    values: Vec<(OsString, OsString)>,
    requested: RefCell<Vec<OsString>>,
}

impl FakeEnvironment {
    fn with(values: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            requested: RefCell::new(Vec::new()),
        }
    }
}

impl HistoryEnvironment for FakeEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        self.requested.borrow_mut().push(name.to_owned());
        self.values
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.clone())
    }
}

#[test]
fn platform_paths_use_state_roots_and_disabled_mode_bypasses_discovery() {
    let linux_override =
        FakeEnvironment::with([("XDG_STATE_HOME", "/state"), ("HOME", "/home/ignored")]);
    assert_eq!(
        select_history(false, HistoryPlatform::Linux, &linux_override)
            .expect("absolute XDG state root should work"),
        HistorySelection::Persistent(PathBuf::from("/state/flashshell/history"))
    );
    assert_eq!(
        linux_override.requested.into_inner(),
        [OsString::from("XDG_STATE_HOME")]
    );

    let linux_fallback =
        FakeEnvironment::with([("XDG_STATE_HOME", "relative"), ("HOME", "/home/user")]);
    assert_eq!(
        select_history(false, HistoryPlatform::Linux, &linux_fallback)
            .expect("relative override should fall back to home"),
        HistorySelection::Persistent(PathBuf::from("/home/user/.local/state/flashshell/history"))
    );

    let macos_fallback = FakeEnvironment::with([("HOME", "/Users/user")]);
    assert_eq!(
        select_history(false, HistoryPlatform::MacOs, &macos_fallback)
            .expect("macOS home should select Application Support"),
        HistorySelection::Persistent(PathBuf::from(
            "/Users/user/Library/Application Support/flashshell/history"
        ))
    );

    struct NoAccess;
    impl HistoryEnvironment for NoAccess {
        fn value(&self, _name: &OsStr) -> Option<OsString> {
            panic!("--no-history must not inspect the environment");
        }
    }
    assert_eq!(
        select_history(true, HistoryPlatform::Linux, &NoAccess)
            .expect("disabled mode cannot fail discovery"),
        HistorySelection::Disabled
    );
}

#[test]
fn disabled_history_records_nothing_and_persistent_history_is_exactly_bounded() {
    let mut disabled = EditorHistory::open(HistorySelection::Disabled)
        .expect("disabled history should need no host state");
    assert!(!disabled.record("echo hidden").expect("disabled save"));
    assert!(
        disabled
            .search_substring("echo")
            .expect("disabled search")
            .is_empty()
    );
    assert_eq!(disabled.capacity(), 0);

    let directory = TempDirectory::new("bounded");
    let path = directory.path().join("flashshell/history");
    let mut history = EditorHistory::open(HistorySelection::Persistent(path.clone()))
        .expect("private history should initialize");
    assert_eq!(history.capacity(), DEFAULT_HISTORY_CAPACITY);
    for index in 0..(DEFAULT_HISTORY_CAPACITY + 2) {
        history
            .record(&format!("entry {index}"))
            .expect("bounded entry should synchronize");
    }
    let reopened = EditorHistory::open(HistorySelection::Persistent(path))
        .expect("bounded history should reopen");
    let entries = reopened.entries().expect("bounded entries should load");
    assert_eq!(entries.len(), DEFAULT_HISTORY_CAPACITY);
    assert_eq!(entries.first().map(String::as_str), Some("entry 2"));
    assert_eq!(entries.last().map(String::as_str), Some("entry 1001"));
}

#[test]
fn persistence_preserves_multiline_source_deduplicates_adjacent_entries_and_searches() {
    let directory = TempDirectory::new("roundtrip");
    let path = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent(path.clone());
    let multiline = "if true {\n    echo exact \\\\n \\\\r \\\\0\r\n}";

    let mut first = EditorHistory::open(selection.clone()).expect("first session opens");
    assert!(first.record("echo first").expect("first record is new"));
    assert!(
        !first
            .record("echo first")
            .expect("adjacent duplicate is skipped")
    );
    assert!(first.record(multiline).expect("multiline record is new"));
    assert!(
        first
            .record("echo first")
            .expect("non-adjacent duplicate is kept")
    );

    let reopened = EditorHistory::open(selection).expect("history reopens");
    assert_eq!(
        reopened.entries().expect("entries load"),
        ["echo first", multiline, "echo first"]
    );
    assert_eq!(
        reopened
            .search_substring("exact")
            .expect("substring search"),
        [multiline]
    );
}

#[test]
fn concurrent_sessions_merge_each_submission_without_lost_entries() {
    let directory = TempDirectory::new("concurrent");
    let path = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent(path);
    let mut first = EditorHistory::open(selection.clone()).expect("first session opens");
    let mut second = EditorHistory::open(selection.clone()).expect("second session opens");

    first.record("from first").expect("first session syncs");
    second
        .record("from first")
        .expect("cross-session adjacent duplicate merges");
    second
        .record("from second")
        .expect("second session merges first");
    first
        .record("first again")
        .expect("first session merges second");

    let reopened = EditorHistory::open(selection).expect("merged history reopens");
    assert_eq!(
        reopened.entries().expect("merged entries load"),
        ["from first", "from second", "first again"]
    );
}

#[test]
fn history_objects_are_private_and_unsafe_existing_files_are_rejected() {
    let directory = TempDirectory::new("permissions");
    let path = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent(path.clone());
    drop(EditorHistory::open(selection.clone()).expect("history initializes"));

    let parent = fs::symlink_metadata(path.parent().expect("history has parent"))
        .expect("history directory exists");
    let file = fs::symlink_metadata(&path).expect("history file exists");
    assert!(parent.is_dir());
    assert!(file.is_file());
    assert_eq!(parent.mode() & 0o777, 0o700);
    assert_eq!(file.mode() & 0o777, 0o600);
    assert_eq!(parent.uid(), file.uid());

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
        .expect("test can make file unsafe");
    let error = EditorHistory::open(selection).expect_err("public history must be rejected");
    assert!(error.to_string().contains("mode 0600"), "{error}");
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "flashshell-history-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("unique temporary directory should be created");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("temporary directory should be private");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
