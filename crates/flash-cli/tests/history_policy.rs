#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_cli::history::{
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
        HistorySelection::Persistent {
            primary: PathBuf::from("/state/flash/history"),
            legacy: PathBuf::from("/state/flashshell/history"),
        }
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
        HistorySelection::Persistent {
            primary: PathBuf::from("/home/user/.local/state/flash/history"),
            legacy: PathBuf::from("/home/user/.local/state/flashshell/history"),
        }
    );

    let macos_fallback = FakeEnvironment::with([("HOME", "/Users/user")]);
    assert_eq!(
        select_history(false, HistoryPlatform::MacOs, &macos_fallback)
            .expect("macOS home should select Application Support"),
        HistorySelection::Persistent {
            primary: PathBuf::from("/Users/user/Library/Application Support/flash/history"),
            legacy: PathBuf::from("/Users/user/Library/Application Support/flashshell/history"),
        }
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
    let path = directory.path().join("flash/history");
    let legacy = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent {
        primary: path.clone(),
        legacy: legacy.clone(),
    };
    let mut history =
        EditorHistory::open(selection.clone()).expect("private history should initialize");
    assert_eq!(history.capacity(), DEFAULT_HISTORY_CAPACITY);
    for index in 0..(DEFAULT_HISTORY_CAPACITY + 2) {
        history
            .record(&format!("entry {index}"))
            .expect("bounded entry should synchronize");
    }
    let reopened = EditorHistory::open(selection).expect("bounded history should reopen");
    let entries = reopened.entries().expect("bounded entries should load");
    assert_eq!(entries.len(), DEFAULT_HISTORY_CAPACITY);
    assert_eq!(entries.first().map(String::as_str), Some("entry 2"));
    assert_eq!(entries.last().map(String::as_str), Some("entry 1001"));
}

#[test]
fn persistence_preserves_multiline_source_deduplicates_adjacent_entries_and_searches() {
    let directory = TempDirectory::new("roundtrip");
    let path = directory.path().join("flash/history");
    let legacy = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent {
        primary: path,
        legacy,
    };
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
    let path = directory.path().join("flash/history");
    let legacy = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent {
        primary: path,
        legacy,
    };
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
    let path = directory.path().join("flash/history");
    let legacy = directory.path().join("flashshell/history");
    let selection = HistorySelection::Persistent {
        primary: path.clone(),
        legacy,
    };
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

#[test]
fn legacy_history_fallback_policy_and_validation() {
    let directory = TempDirectory::new("legacy-fallback");
    let primary_dir = directory.path().join("flash");
    let legacy_dir = directory.path().join("flashshell");
    let primary_path = primary_dir.join("history");
    let legacy_path = legacy_dir.join("history");
    let selection = HistorySelection::Persistent {
        primary: primary_path.clone(),
        legacy: legacy_path.clone(),
    };

    // 6: Sind beide Dateien nicht vorhanden, wird nur flash/history erzeugt.
    // 7: Die neu erzeugte Directory- und File-Permission ist weiterhin 0700 beziehungsweise 0600.
    let history_new =
        EditorHistory::open(selection.clone()).expect("creates primary when both missing");
    assert!(primary_path.is_file());
    assert!(!legacy_path.exists());
    let parent_meta = fs::symlink_metadata(&primary_dir).unwrap();
    let file_meta = fs::symlink_metadata(&primary_path).unwrap();
    assert_eq!(parent_meta.mode() & 0o777, 0o700);
    assert_eq!(file_meta.mode() & 0o777, 0o600);
    drop(history_new);
    fs::remove_dir_all(&primary_dir).unwrap();

    // 8: Existiert nur die Legacy-Datei, werden deren bestehende Einträge geladen.
    // 9: Nach dem Laden der Legacy-Datei werden neue Einträge in die Legacy-Datei geschrieben.
    // 10: Beim Legacy-Fallback wird kein neuer kanonischer History-Pfad erzeugt.
    fs::create_dir_all(&legacy_dir).unwrap();
    fs::set_permissions(&legacy_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&legacy_path, "legacy entry\n").unwrap();
    fs::set_permissions(&legacy_path, fs::Permissions::from_mode(0o600)).unwrap();

    let mut legacy_history =
        EditorHistory::open(selection.clone()).expect("opens existing legacy history");
    assert_eq!(legacy_history.entries().unwrap(), vec!["legacy entry"]);
    legacy_history.record("new via legacy").unwrap();
    drop(legacy_history);
    assert!(!primary_path.exists());
    let legacy_contents = fs::read_to_string(&legacy_path).unwrap();
    assert!(legacy_contents.contains("new via legacy"));

    // 11: Existieren beide Dateien, werden nur Einträge der kanonischen Datei geladen.
    // 12: Existieren beide Dateien, wird die Legacy-Datei weder verändert noch zusammengeführt.
    fs::create_dir_all(&primary_dir).unwrap();
    fs::set_permissions(&primary_dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&primary_path, "primary entry\n").unwrap();
    fs::set_permissions(&primary_path, fs::Permissions::from_mode(0o600)).unwrap();

    let mut both_history =
        EditorHistory::open(selection.clone()).expect("opens primary when both exist");
    assert_eq!(both_history.entries().unwrap(), vec!["primary entry"]);
    both_history.record("another primary").unwrap();
    drop(both_history);
    let unchanged_legacy = fs::read_to_string(&legacy_path).unwrap();
    assert_eq!(unchanged_legacy, legacy_contents);

    // 13: Eine ungültige kanonische Datei verhindert jeden Legacy-Fallback.
    fs::write(&primary_path, "\\z_invalid_escape\n").unwrap();
    fs::set_permissions(&primary_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(EditorHistory::open(selection.clone()).is_err());
    assert_eq!(fs::read_to_string(&legacy_path).unwrap(), unchanged_legacy);

    // 14: Eine kanonische Symlink-Datei verhindert jeden Legacy-Fallback.
    fs::remove_file(&primary_path).unwrap();
    let target = directory.path().join("target");
    fs::write(&target, "target\n").unwrap();
    symlink(&target, &primary_path).unwrap();
    assert!(EditorHistory::open(selection.clone()).is_err());

    // 15: Eine falsche kanonische Permission verhindert jeden Legacy-Fallback.
    fs::remove_file(&primary_path).unwrap();
    fs::write(&primary_path, "primary entry\n").unwrap();
    fs::set_permissions(&primary_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(EditorHistory::open(selection.clone()).is_err());

    // Remove primary completely to test failing legacy file cases when primary is absent:
    fs::remove_dir_all(&primary_dir).unwrap();

    // 16: Fehlt die kanonische Datei und ist die Legacy-Datei ein Symlink, schlägt das Öffnen fehl.
    fs::remove_file(&legacy_path).unwrap();
    symlink(&target, &legacy_path).unwrap();
    assert!(EditorHistory::open(selection.clone()).is_err());

    // 17: Fehlt die kanonische Datei und hat die Legacy-Datei falsche Berechtigungen, schlägt das Öffnen fehl.
    fs::remove_file(&legacy_path).unwrap();
    fs::write(&legacy_path, "legacy entry\n").unwrap();
    fs::set_permissions(&legacy_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(EditorHistory::open(selection.clone()).is_err());
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("flash-history-{label}-{}-{id}", std::process::id()));
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
