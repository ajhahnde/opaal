#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_lsp::uri::DocumentUri;
use flash_lsp::workspace::{ChangeOutcome, DocumentError, Workspace};
use flash_runtime::module::{ModuleProgramLoader, ModuleResolver, ModuleSourceLoader};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "flash-lsp-workspace-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn uri(&self, name: &str) -> DocumentUri {
        DocumentUri::from_absolute_path(&self.path(name)).unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn unsaved_roots_and_imports_load_from_one_canonical_overlay_graph() {
    let directory = TestDirectory::new();
    let root_path = directory.path("main.fsh");
    fs::write(&root_path, "this disk text must not be parsed\n").unwrap();
    let root_uri = directory.uri("main.fsh");
    let dependency_uri = directory.uri("library.fsh");
    let mut workspace = Workspace::new();

    workspace
        .open(
            root_uri.clone(),
            1,
            "import { answer } from './library.fsh'\nexport RESULT = $answer\n".into(),
        )
        .unwrap();
    workspace
        .open(
            dependency_uri.clone(),
            7,
            "let answer = 42\nexport { answer }\n".into(),
        )
        .unwrap();

    let program = ModuleProgramLoader::new(&workspace, &workspace)
        .load(&root_path)
        .expect("both unsaved documents form a valid module program");
    let sources = program
        .sources()
        .entries()
        .map(|entry| entry.source().text())
        .collect::<Vec<_>>();
    assert_eq!(
        sources,
        [
            "import { answer } from './library.fsh'\nexport RESULT = $answer\n",
            "let answer = 42\nexport { answer }\n",
        ]
    );
    assert!(
        workspace
            .document(&dependency_uri)
            .unwrap()
            .is_provisional()
    );
}

#[cfg(unix)]
#[test]
fn canonical_aliases_share_one_owner_and_conflicting_opens_are_rejected() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new();
    let target = directory.path("target.fsh");
    let alias = directory.path("alias.fsh");
    fs::write(&target, "let disk = 1\n").unwrap();
    symlink(&target, &alias).unwrap();
    let target_uri = DocumentUri::from_absolute_path(&target).unwrap();
    let alias_uri = DocumentUri::from_absolute_path(&alias).unwrap();
    let mut workspace = Workspace::new();

    workspace
        .open(target_uri.clone(), 1, "let editor = 2\n".into())
        .unwrap();
    assert_eq!(
        workspace
            .open(alias_uri.clone(), 1, "let conflict = 3\n".into())
            .unwrap_err(),
        DocumentError::ModuleAlreadyOpen {
            owner: target_uri.clone(),
        }
    );
    assert_eq!(
        workspace
            .open(target_uri.clone(), 2, "let duplicate = 4\n".into())
            .unwrap_err(),
        DocumentError::AlreadyOpen
    );
    assert_eq!(workspace.len(), 1);

    let target_id = ModuleResolver::new(&workspace)
        .resolve_root(&alias)
        .unwrap();
    assert_eq!(
        ModuleSourceLoader::load(&workspace, &target_id).unwrap(),
        b"let editor = 2\n"
    );
}

#[test]
fn changes_require_a_strictly_increasing_present_version() {
    let directory = TestDirectory::new();
    let uri = directory.uri("new.fsh");
    let mut workspace = Workspace::new();
    workspace
        .open(uri.clone(), 3, "let value = 3\n".into())
        .unwrap();

    for version in [None, Some(3), Some(2)] {
        assert_eq!(
            workspace
                .change(&uri, version, "let ignored = 0\n".into())
                .unwrap(),
            ChangeOutcome::IgnoredInvalidVersion
        );
    }
    assert_eq!(workspace.document(&uri).unwrap().version(), 3);
    assert_eq!(workspace.document(&uri).unwrap().text(), "let value = 3\n");

    assert_eq!(
        workspace
            .change(&uri, Some(4), "let value = 4\n".into())
            .unwrap(),
        ChangeOutcome::Applied
    );
    assert_eq!(workspace.document(&uri).unwrap().version(), 4);
    assert_eq!(workspace.document(&uri).unwrap().text(), "let value = 4\n");

    let unknown = directory.uri("unknown.fsh");
    assert_eq!(
        workspace
            .change(&unknown, Some(1), String::new())
            .unwrap_err(),
        DocumentError::NotOpen
    );
}

#[cfg(unix)]
#[test]
fn provisional_identity_recanonicalizes_when_a_new_path_becomes_an_alias() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new();
    let alias = directory.path("future.fsh");
    let target = directory.path("created.fsh");
    let uri = DocumentUri::from_absolute_path(&alias).unwrap();
    let mut workspace = Workspace::new();
    workspace
        .open(uri.clone(), 1, "let editor = 1\n".into())
        .unwrap();
    let provisional = workspace
        .document(&uri)
        .unwrap()
        .module_path()
        .to_path_buf();
    assert_eq!(
        provisional,
        fs::canonicalize(&directory.0).unwrap().join("future.fsh")
    );
    assert!(workspace.document(&uri).unwrap().is_provisional());

    fs::write(&target, "let disk = 2\n").unwrap();
    symlink(&target, &alias).unwrap();
    let transitions = workspace.refresh_identities().unwrap();

    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].uri(), &uri);
    assert_eq!(transitions[0].previous(), provisional);
    assert_eq!(transitions[0].current(), fs::canonicalize(&target).unwrap());
    assert!(!workspace.document(&uri).unwrap().is_provisional());

    let target_id = ModuleResolver::new(&workspace)
        .resolve_root(&target)
        .unwrap();
    assert_eq!(
        ModuleSourceLoader::load(&workspace, &target_id).unwrap(),
        b"let editor = 1\n"
    );
}

#[cfg(unix)]
#[test]
fn conflicting_recanonicalization_is_rejected_without_changing_owners() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new();
    let first_path = directory.path("first.fsh");
    let second_path = directory.path("second.fsh");
    let target = directory.path("target.fsh");
    let first_uri = DocumentUri::from_absolute_path(&first_path).unwrap();
    let second_uri = DocumentUri::from_absolute_path(&second_path).unwrap();
    let mut workspace = Workspace::new();
    workspace
        .open(first_uri.clone(), 1, "let first = 1\n".into())
        .unwrap();
    workspace
        .open(second_uri.clone(), 1, "let second = 2\n".into())
        .unwrap();
    let first_identity = workspace
        .document(&first_uri)
        .unwrap()
        .module_path()
        .to_path_buf();
    let second_identity = workspace
        .document(&second_uri)
        .unwrap()
        .module_path()
        .to_path_buf();

    fs::write(&target, "let disk = 3\n").unwrap();
    symlink(&target, &first_path).unwrap();
    symlink(&target, &second_path).unwrap();

    assert!(matches!(
        workspace.refresh_identities(),
        Err(DocumentError::ModuleAlreadyOpen { .. })
    ));
    assert_eq!(
        workspace.document(&first_uri).unwrap().module_path(),
        first_identity
    );
    assert_eq!(
        workspace.document(&second_uri).unwrap().module_path(),
        second_identity
    );
    assert!(workspace.document(&first_uri).unwrap().is_provisional());
    assert!(workspace.document(&second_uri).unwrap().is_provisional());
}

#[test]
fn closing_an_overlay_restores_read_only_disk_loading() {
    let directory = TestDirectory::new();
    let path = directory.path("main.fsh");
    fs::write(&path, "let disk = 1\n").unwrap();
    let uri = DocumentUri::from_absolute_path(&path).unwrap();
    let mut workspace = Workspace::new();
    workspace
        .open(uri.clone(), 1, "let editor = 2\n".into())
        .unwrap();

    let open_id = ModuleResolver::new(&workspace).resolve_root(&path).unwrap();
    assert_eq!(
        ModuleSourceLoader::load(&workspace, &open_id).unwrap(),
        b"let editor = 2\n"
    );
    workspace.close(&uri).unwrap();

    let disk_id = ModuleResolver::new(&workspace).resolve_root(&path).unwrap();
    assert_eq!(
        ModuleSourceLoader::load(&workspace, &disk_id).unwrap(),
        b"let disk = 1\n"
    );
    assert!(workspace.is_empty());
}

#[test]
fn existing_directories_cannot_become_document_overlays() {
    let directory = TestDirectory::new();
    let uri = DocumentUri::from_absolute_path(&directory.0).unwrap();
    let mut workspace = Workspace::new();

    assert!(matches!(
        workspace.open(uri, 1, String::new()),
        Err(DocumentError::Host(_))
    ));
    assert!(workspace.is_empty());
}

#[test]
fn lexical_aliases_of_a_provisional_path_resolve_to_the_overlay() {
    let directory = TestDirectory::new();
    fs::create_dir(directory.path("nested")).unwrap();
    let path = directory.path("new.fsh");
    let uri = DocumentUri::from_absolute_path(&path).unwrap();
    let mut workspace = Workspace::new();
    workspace
        .open(uri.clone(), 1, "let editor = 1\n".into())
        .unwrap();
    let expected = workspace
        .document(&uri)
        .unwrap()
        .module_path()
        .to_path_buf();

    let alias = directory.path("nested").join("..").join("new.fsh");
    let id = ModuleResolver::new(&workspace)
        .resolve_root(&alias)
        .unwrap();
    assert_eq!(id.path(), Path::new(&expected));
    assert_eq!(
        ModuleSourceLoader::load(&workspace, &id).unwrap(),
        b"let editor = 1\n"
    );
}
