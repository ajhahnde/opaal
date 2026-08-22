#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::cell::Cell;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use flash_cli::completion::{
    CompletionCandidateProvider, CompletionEngine, CompletionKind, CompletionSnapshotLimits,
    live_completion_catalog,
};
use flash_runtime::builtin::standard_registry;
use flash_runtime::{BindingMutability, Environment, ScopeStack, Value};

static UNIQUE: AtomicU32 = AtomicU32::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let id = UNIQUE.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "flash-live-completion-{tag}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create completion fixture");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn executable(path: &Path) {
    fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write executable fixture");
    let mut permissions = fs::metadata(path)
        .expect("inspect executable fixture")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).expect("mark fixture executable");
}

#[test]
fn a_live_snapshot_combines_scope_path_executables_and_cwd_paths() {
    let fixture = Fixture::new("catalog");
    let bin = fixture.path().join("bin");
    let cwd = fixture.path().join("work");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(cwd.join("outbox")).unwrap();
    executable(&bin.join("alpha-tool"));
    fs::write(bin.join("alpha-data"), b"not executable").unwrap();
    #[cfg(target_os = "linux")]
    executable(&bin.join(OsString::from_vec(vec![b'a', 0xff])));
    fs::write(cwd.join("output.log"), b"").unwrap();
    fs::write(cwd.join("space name"), b"").unwrap();
    #[cfg(target_os = "linux")]
    fs::write(cwd.join(OsString::from_vec(vec![b'o', 0xff])), b"").unwrap();

    let mut environment = Environment::new();
    environment.set("PATH", bin.as_os_str());
    let mut scope = ScopeStack::new();
    scope
        .declare("later", BindingMutability::Immutable, Value::Int(1))
        .unwrap();
    let catalog = live_completion_catalog(
        &standard_registry(),
        &scope,
        &cwd,
        &environment,
        CompletionSnapshotLimits::default(),
    );
    let engine = CompletionEngine::new(catalog);

    assert_eq!(
        engine
            .complete("^alpha", 6)
            .iter()
            .map(|completion| (completion.value(), completion.kind()))
            .collect::<Vec<_>>(),
        [("alpha-tool", CompletionKind::ExternalCommand)]
    );
    assert_eq!(engine.complete("$la", 3)[0].value(), "$later");
    assert_eq!(
        engine
            .complete("echo > out", 10)
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["outbox/", "output.log"]
    );
    assert_eq!(
        engine
            .complete("cat ./out", 9)
            .iter()
            .map(|completion| completion.value())
            .collect::<Vec<_>>(),
        ["./outbox/", "./output.log"]
    );
    assert!(engine.complete("cat space", 9).is_empty());
}

#[test]
fn crossing_a_snapshot_ceiling_discards_the_host_family() {
    let fixture = Fixture::new("ceiling");
    let bin = fixture.path().join("bin");
    let extra_bin = fixture.path().join("extra-bin");
    let cwd = fixture.path().join("work");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&extra_bin).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    for name in ["alpha-one", "alpha-two", "alpha-three"] {
        executable(&bin.join(name));
    }
    for name in ["path-one", "path-two", "path-three"] {
        fs::write(cwd.join(name), b"").unwrap();
    }
    let mut environment = Environment::new();
    environment.set("PATH", bin.as_os_str());

    let catalog = live_completion_catalog(
        &standard_registry(),
        &ScopeStack::new(),
        &cwd,
        &environment,
        CompletionSnapshotLimits::new(1, 2),
    );
    let engine = CompletionEngine::new(catalog);

    assert!(engine.complete("^alpha", 6).is_empty());
    assert!(engine.complete("echo > path", 11).is_empty());

    environment.set(
        "PATH",
        std::env::join_paths([bin.as_path(), extra_bin.as_path()]).unwrap(),
    );
    let directory_capped = CompletionEngine::new(live_completion_catalog(
        &standard_registry(),
        &ScopeStack::new(),
        &cwd,
        &environment,
        CompletionSnapshotLimits::new(1, 100),
    ));
    assert!(directory_capped.complete("^alpha", 6).is_empty());
}

#[test]
fn recursive_snapshots_are_bounded_exact_and_do_not_follow_symlinks() {
    let fixture = Fixture::new("recursive");
    let cwd = fixture.path().join("work");
    fs::create_dir_all(cwd.join("scripts/nested/deep")).unwrap();
    fs::create_dir_all(cwd.join(".hidden-dir")).unwrap();
    fs::write(cwd.join("scripts/a.fsh"), b"").unwrap();
    fs::write(cwd.join("scripts/nested/b.fsh"), b"").unwrap();
    fs::write(cwd.join("scripts/nested/deep/c.fsh"), b"").unwrap();
    fs::write(cwd.join("space name"), b"").unwrap();
    fs::write(cwd.join("quote'name"), b"").unwrap();
    fs::write(cwd.join("🚀.fsh"), b"").unwrap();
    fs::write(cwd.join(".secret"), b"").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("scripts", cwd.join("linked")).unwrap();

    let catalog = live_completion_catalog(
        &standard_registry(),
        &ScopeStack::new(),
        &cwd,
        &Environment::new(),
        CompletionSnapshotLimits::default(),
    );
    let engine = CompletionEngine::new(catalog);

    assert_eq!(engine.complete("cat 'space", 10)[0].value(), "'space name'");
    assert_eq!(
        engine.complete("cat 'quote", 10)[0].value(),
        "'quote'\\''name'"
    );
    let unicode = "cat './🚀";
    assert_eq!(
        engine.complete(unicode, unicode.len())[0].value(),
        "'./🚀.fsh'"
    );
    assert_eq!(
        engine.complete("cat scripts/nested/d", 20)[0].value(),
        "scripts/nested/deep/"
    );
    assert_eq!(engine.complete("cat './.se", 10)[0].value(), "'./.secret'");
    assert!(engine.complete("cat linked/a", 12).is_empty());
}

#[test]
fn provider_generations_reject_stale_results_and_collection_can_cancel() {
    let fixture = Fixture::new("generation");
    let cwd = fixture.path().join("work");
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("first"), b"").unwrap();
    let registry = standard_registry();
    let scope = ScopeStack::new();
    let environment = Environment::new();
    let mut provider = CompletionCandidateProvider::default();

    let first = provider
        .snapshot(&registry, &scope, &cwd, &environment, &|| false)
        .unwrap();
    fs::write(cwd.join("second"), b"").unwrap();
    let second = provider
        .snapshot(&registry, &scope, &cwd, &environment, &|| false)
        .unwrap();
    assert!(second.generation() > first.generation());

    let mut engine = CompletionEngine::new(second);
    assert!(!engine.install_catalog(first));
    assert_eq!(engine.complete("cat ./se", 8)[0].value(), "./second");

    let checks = Cell::new(0_u8);
    assert!(
        provider
            .snapshot(&registry, &scope, &cwd, &environment, &|| {
                checks.set(checks.get().saturating_add(1));
                checks.get() > 2
            })
            .is_none()
    );

    let vanished = fixture.path().join("vanished");
    fs::create_dir(&vanished).unwrap();
    fs::remove_dir(&vanished).unwrap();
    let empty = provider
        .snapshot(&registry, &scope, &vanished, &environment, &|| false)
        .unwrap();
    assert!(
        CompletionEngine::new(empty)
            .complete("cat ./", 6)
            .is_empty()
    );
}
