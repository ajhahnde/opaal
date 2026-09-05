#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_lsp::uri::DocumentUri;
use flash_lsp::workspace::{ChangeOutcome, DiagnosticPublishOutcome, DocumentError, Workspace};
use flash_runtime::module::{ModuleProgramLoader, ModuleResolver, ModuleSourceLoader};
use flash_syntax::{LanguageMajor, PositionEncoding, Severity};

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
fn v2_source_budget_publishes_one_visible_incomplete_analysis_diagnostic() {
    let directory = TestDirectory::new();
    let uri = directory.uri("oversized.fsh");
    let mut workspace = Workspace::for_language(LanguageMajor::V2);
    let text = format!("language 2\n#{}\n", "x".repeat(8 * 1024 * 1024));
    workspace.open(uri.clone(), 1, text).unwrap();

    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    assert_eq!(analysis.documents().len(), 1);
    assert_eq!(analysis.documents()[0].uri(), &uri);
    let diagnostics = analysis.documents()[0].diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code(), Some("ANL001"));
    assert!(diagnostics[0].message().contains("source-bytes limit"));
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

#[test]
fn accepted_mutations_advance_one_generation_and_ignored_changes_do_not() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.fsh");
    let mut workspace = Workspace::new();

    assert_eq!(workspace.generation(), 0);
    workspace
        .open(uri.clone(), 1, "let value = 1\n".into())
        .unwrap();
    assert_eq!(workspace.generation(), 1);
    assert_eq!(workspace.document(&uri).unwrap().generation(), 1);

    assert_eq!(
        workspace
            .change(&uri, Some(1), "let ignored = 0\n".into())
            .unwrap(),
        ChangeOutcome::IgnoredInvalidVersion
    );
    assert_eq!(workspace.generation(), 1);
    assert_eq!(workspace.document(&uri).unwrap().generation(), 1);

    assert_eq!(
        workspace
            .change(&uri, Some(2), "let value = 2\n".into())
            .unwrap(),
        ChangeOutcome::Applied
    );
    assert_eq!(workspace.generation(), 2);
    assert_eq!(workspace.document(&uri).unwrap().generation(), 2);

    assert!(workspace.refresh_identities().unwrap().is_empty());
    assert_eq!(workspace.generation(), 2);

    let closed = workspace.close(&uri).unwrap();
    assert_eq!(workspace.generation(), 3);
    assert_eq!(closed.generation(), 2);
}

#[test]
fn diagnostic_snapshots_are_immutable_and_sort_roots_by_canonical_path() {
    let directory = TestDirectory::new();
    let later_uri = directory.uri("zeta.fsh");
    let earlier_uri = directory.uri("alpha.fsh");
    let mut workspace = Workspace::new();
    workspace
        .open(later_uri.clone(), 4, "let value = 'old'\n".into())
        .unwrap();
    workspace
        .open(earlier_uri.clone(), 7, "let alpha = 1\n".into())
        .unwrap();

    let snapshot = workspace.diagnostic_snapshot();
    assert_eq!(snapshot.generation(), 2);
    assert_eq!(
        snapshot
            .roots()
            .iter()
            .map(|root| root.uri())
            .collect::<Vec<_>>(),
        [&earlier_uri, &later_uri]
    );

    workspace
        .change(&later_uri, Some(5), "let value = 'new'\n".into())
        .unwrap();
    assert_eq!(snapshot.document(&later_uri).unwrap().version(), 4);
    assert_eq!(
        snapshot.document(&later_uri).unwrap().text(),
        "let value = 'old'\n"
    );
    assert_eq!(workspace.document(&later_uri).unwrap().version(), 5);

    let module = ModuleResolver::new(&snapshot)
        .resolve_root(&directory.path("zeta.fsh"))
        .unwrap();
    assert_eq!(
        ModuleSourceLoader::load(&snapshot, &module).unwrap(),
        b"let value = 'old'\n"
    );
}

#[test]
fn diagnostics_are_normalized_deduplicated_versioned_and_published_atomically() {
    let directory = TestDirectory::new();
    let consumer_uri = directory.uri("consumer.fsh");
    let library_uri = directory.uri("library.fsh");
    let main_uri = directory.uri("main.fsh");
    let mut workspace = Workspace::new();
    workspace
        .open(
            main_uri.clone(),
            3,
            concat!(
                "import { echo } from './library.fsh'\n",
                "echo()\n",
                "ls | ^cat\n",
                "let marker = 1\n",
                "export { marker }\n",
            )
            .into(),
        )
        .unwrap();
    workspace
        .open(
            consumer_uri.clone(),
            1,
            "import { marker } from './main.fsh'\nlet copy = $marker\n".into(),
        )
        .unwrap();
    workspace
        .open(
            library_uri.clone(),
            9,
            "def echo(value: String) -> String { $value }\nexport { echo }\n".into(),
        )
        .unwrap();

    let stale = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    workspace
        .change(
            &consumer_uri,
            Some(2),
            "import { marker } from './main.fsh'\nlet copy = $marker\n".into(),
        )
        .unwrap();
    assert_eq!(
        workspace.publish_diagnostics(stale),
        DiagnosticPublishOutcome::Stale
    );

    let current = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    let DiagnosticPublishOutcome::Published(publication) = workspace.publish_diagnostics(current)
    else {
        panic!("the current generation must publish");
    };
    assert_eq!(publication.generation(), workspace.generation());
    assert_eq!(publication.documents().len(), 1);
    let document = &publication.documents()[0];
    assert_eq!(document.uri(), &main_uri);
    assert_eq!(document.version(), Some(3));
    assert_eq!(
        document
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code())
            .collect::<Vec<_>>(),
        [Some("SIG003"), Some("PIP003")]
    );
    assert!(
        document
            .diagnostics()
            .iter()
            .all(|diagnostic| diagnostic.severity() == Severity::Error)
    );

    let signature = &document.diagnostics()[0];
    assert_eq!(signature.range().start().line(), 1);
    assert_eq!(
        signature.primary_annotation(),
        Some("expected 1 arguments, found 0")
    );
    assert_eq!(signature.related_information().len(), 1);
    assert_eq!(signature.related_information()[0].uri(), &library_uri);
    assert_eq!(
        signature.related_information()[0].message(),
        "function declared here"
    );
    let pipeline = &document.diagnostics()[1];
    assert_eq!(pipeline.notes().len(), 1);
    assert!(pipeline.notes()[0].contains("encode"));

    workspace
        .change(
            &main_uri,
            Some(4),
            concat!(
                "import { echo } from './library.fsh'\n",
                "echo('ok')\n",
                "let marker = 1\n",
                "export { marker }\n",
            )
            .into(),
        )
        .unwrap();
    let cleared = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    let DiagnosticPublishOutcome::Published(publication) = workspace.publish_diagnostics(cleared)
    else {
        panic!("the clearing generation must publish");
    };
    assert_eq!(publication.documents().len(), 1);
    assert_eq!(publication.documents()[0].uri(), &main_uri);
    assert_eq!(publication.documents()[0].version(), Some(4));
    assert!(publication.documents()[0].diagnostics().is_empty());
}

#[test]
fn disk_dependency_diagnostics_omit_versions_and_are_cleared_after_invalidation() {
    let directory = TestDirectory::new();
    let root_uri = directory.uri("main.fsh");
    let dependency_path = directory.path("library.fsh");
    fs::write(&dependency_path, "let broken =\n").unwrap();
    let dependency_uri =
        DocumentUri::from_absolute_path(&fs::canonicalize(&dependency_path).unwrap()).unwrap();
    let mut workspace = Workspace::new();
    workspace
        .open(
            root_uri.clone(),
            1,
            "import './library.fsh'\nlet root = 1\n".into(),
        )
        .unwrap();

    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf8)
        .unwrap();
    let DiagnosticPublishOutcome::Published(publication) = workspace.publish_diagnostics(analysis)
    else {
        panic!("the current generation must publish");
    };
    assert_eq!(publication.documents().len(), 1);
    assert_eq!(publication.documents()[0].uri(), &dependency_uri);
    assert_eq!(publication.documents()[0].version(), None);
    assert!(!publication.documents()[0].diagnostics().is_empty());

    fs::write(&dependency_path, "let fixed = 1\n").unwrap();
    workspace
        .change(
            &root_uri,
            Some(2),
            "import './library.fsh'\nlet root = 1\n".into(),
        )
        .unwrap();
    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf8)
        .unwrap();
    let DiagnosticPublishOutcome::Published(publication) = workspace.publish_diagnostics(analysis)
    else {
        panic!("the clearing generation must publish");
    };
    assert_eq!(publication.documents().len(), 1);
    assert_eq!(publication.documents()[0].uri(), &dependency_uri);
    assert_eq!(publication.documents()[0].version(), None);
    assert!(publication.documents()[0].diagnostics().is_empty());
}

#[test]
fn closing_a_diagnostic_owner_publishes_one_unversioned_clear() {
    let directory = TestDirectory::new();
    let uri = directory.uri("main.fsh");
    let mut workspace = Workspace::new();
    workspace
        .open(uri.clone(), 6, "let broken =\n".into())
        .unwrap();
    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    let DiagnosticPublishOutcome::Published(initial) = workspace.publish_diagnostics(analysis)
    else {
        panic!("the current generation must publish");
    };
    assert_eq!(initial.documents()[0].version(), Some(6));

    workspace.close(&uri).unwrap();
    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    let DiagnosticPublishOutcome::Published(cleared) = workspace.publish_diagnostics(analysis)
    else {
        panic!("the closing generation must publish");
    };
    assert_eq!(cleared.documents().len(), 1);
    assert_eq!(cleared.documents()[0].uri(), &uri);
    assert_eq!(cleared.documents()[0].version(), None);
    assert!(cleared.documents()[0].diagnostics().is_empty());

    let analysis = workspace
        .diagnostic_snapshot()
        .analyze_diagnostics(PositionEncoding::Utf16)
        .unwrap();
    let DiagnosticPublishOutcome::Published(unchanged) = workspace.publish_diagnostics(analysis)
    else {
        panic!("the unchanged generation remains current");
    };
    assert!(unchanged.documents().is_empty());
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
    assert_eq!(workspace.generation(), 1);
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
    assert_eq!(workspace.generation(), 2);
    assert_eq!(workspace.document(&uri).unwrap().generation(), 2);
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
