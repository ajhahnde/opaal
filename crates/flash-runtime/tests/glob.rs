#![forbid(unsafe_code)]

//! Assembled-session acceptance for the explicit `glob(pattern)` expression.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flash_platform::{
    Capabilities, ChildProcess, DescriptorEndpoint, DirectoryEntry, DirectoryEntryKind,
    DirectoryReadError, DirectoryReadRequest, DirectoryStream, FakePlatform, FileActionError,
    FileOpenRequest, PipeEndpoints, Platform, SpawnError, SpawnRequest,
};
use flash_platform_posix::PosixPlatform;
use flash_runtime::Environment;
use flash_runtime::eval::FakeClock;
use flash_runtime::plan::SessionOptions;
use flash_runtime::resolve::ExecutableProbe;
use flash_runtime::session::{Session, SubmitError};

#[derive(Default)]
struct NoExecutables;

impl ExecutableProbe for NoExecutables {
    fn is_executable(&self, _path: &OsStr) -> bool {
        false
    }
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "flash-runtime-glob-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("temporary glob directory should be created");
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

fn session(cwd: &Path) -> Session {
    Session::new(cwd, Environment::new(), SessionOptions::default())
}

fn submit(session: &mut Session, source: &str) -> Result<(), SubmitError> {
    session
        .submit(
            "glob.fsh",
            source,
            &NoExecutables,
            &PosixPlatform,
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .map(|_| ())
}

#[test]
fn recursive_glob_is_sorted_and_does_not_cross_hidden_or_symlink_directories() {
    let temp = TempDirectory::new("recursive");
    let scripts = temp.path().join("scripts");
    fs::create_dir_all(scripts.join("nested/deep")).unwrap();
    fs::create_dir_all(scripts.join(".hidden-dir")).unwrap();
    fs::write(scripts.join("a.fsh"), b"").unwrap();
    fs::write(scripts.join("nested/b.fsh"), b"").unwrap();
    fs::write(scripts.join("nested/deep/c.fsh"), b"").unwrap();
    fs::write(scripts.join(".hidden.fsh"), b"").unwrap();
    fs::write(scripts.join(".hidden-dir/d.fsh"), b"").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("nested", scripts.join("linked")).unwrap();

    let mut session = session(temp.path());
    submit(
        &mut session,
        "let matches = glob('scripts/**/*.fsh')\n\
         export FIRST = $matches[0]\n\
         export SECOND = $matches[1]\n\
         export THIRD = $matches[2]",
    )
    .expect("recursive glob should succeed");

    assert_eq!(
        session.environment().get("FIRST"),
        Some(OsStr::new("scripts/a.fsh"))
    );
    assert_eq!(
        session.environment().get("SECOND"),
        Some(OsStr::new("scripts/nested/b.fsh"))
    );
    assert_eq!(
        session.environment().get("THIRD"),
        Some(OsStr::new("scripts/nested/deep/c.fsh"))
    );

    let error = submit(&mut session, "$matches[3]").expect_err("there are exactly three matches");
    assert!(error.render().contains("index"), "{}", error.render());
}

#[test]
fn component_patterns_select_hidden_entries_only_with_an_explicit_dot() {
    let temp = TempDirectory::new("components");
    fs::write(temp.path().join("alpha.fsh"), b"").unwrap();
    fs::write(temp.path().join("beta.fsh"), b"").unwrap();
    fs::write(temp.path().join(".secret.fsh"), b"").unwrap();

    let mut session = session(temp.path());
    submit(
        &mut session,
        "let visible = glob('[a-b]*.fsh')\n\
         let hidden = glob('.*.fsh')\n\
         export FIRST = $visible[0]\n\
         export SECOND = $visible[1]\n\
         export HIDDEN = $hidden[0]\n\
         export EMPTY = glob('missing-*.fsh') == []",
    )
    .expect("component glob should succeed");

    assert_eq!(
        session.environment().get("FIRST"),
        Some(OsStr::new("alpha.fsh"))
    );
    assert_eq!(
        session.environment().get("SECOND"),
        Some(OsStr::new("beta.fsh"))
    );
    assert_eq!(
        session.environment().get("HIDDEN"),
        Some(OsStr::new(".secret.fsh"))
    );
    assert_eq!(session.environment().get("EMPTY"), Some(OsStr::new("true")));

    let error = submit(&mut session, "glob('[z-a]')").expect_err("descending ranges are invalid");
    assert!(
        error.render().contains("glob pattern"),
        "{}",
        error.render()
    );
}

#[test]
fn question_negated_class_escape_and_unicode_character_matching_are_exact() {
    let temp = TempDirectory::new("pattern-atoms");
    fs::create_dir_all(temp.path().join("patterns")).unwrap();
    fs::create_dir_all(temp.path().join("unicode")).unwrap();
    fs::write(temp.path().join("patterns/a1.fsh"), b"").unwrap();
    fs::write(temp.path().join("patterns/b1.fsh"), b"").unwrap();
    fs::write(temp.path().join("patterns/*.fsh"), b"").unwrap();
    fs::write(temp.path().join("unicode/é.fsh"), b"").unwrap();

    let mut session = session(temp.path());
    submit(
        &mut session,
        "let selected = glob('patterns/[!b]?.fsh')\n\
         let escaped = glob('patterns/\\*.fsh')\n\
         let unicode = glob('unicode/?.fsh')\n\
         export SELECTED = $selected[0]\n\
         export ESCAPED = $escaped[0]\n\
         export UNICODE = $unicode[0]",
    )
    .expect("component atoms should match exactly");

    assert_eq!(
        session.environment().get("SELECTED"),
        Some(OsStr::new("patterns/a1.fsh"))
    );
    assert_eq!(
        session.environment().get("ESCAPED"),
        Some(OsStr::new("patterns/*.fsh"))
    );
    assert_eq!(
        session.environment().get("UNICODE"),
        Some(OsStr::new("unicode/é.fsh"))
    );
}

#[test]
fn absolute_and_path_patterns_preserve_their_native_spelling() {
    let temp = TempDirectory::new("absolute");
    fs::write(temp.path().join("only.fsh"), b"").unwrap();
    let absolute_pattern = fs::canonicalize(temp.path()).unwrap().join("*.fsh");

    let mut session = session(temp.path());
    submit(
        &mut session,
        &format!(
            "let from_path = glob('only.fsh')[0]\n\
             export PATH_MATCH = glob($from_path)[0]\n\
             export ABSOLUTE = glob('{}')[0]",
            absolute_pattern.display()
        ),
    )
    .expect("string and path patterns should succeed");

    assert_eq!(
        session.environment().get("PATH_MATCH"),
        Some(OsStr::new("only.fsh"))
    );
    assert_eq!(
        session.environment().get("ABSOLUTE"),
        Some(absolute_pattern.with_file_name("only.fsh").as_os_str())
    );
}

#[cfg(unix)]
#[test]
fn wildcard_matches_preserve_non_utf8_native_names() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let native = OsString::from_vec(vec![b'n', 0xff, b'.', b'f', b's', b'h']);
    let platform = NativeEntryPlatform {
        entries: vec![DirectoryEntry::new(
            native.clone(),
            DirectoryEntryKind::File,
            Some(0),
        )],
    };

    let mut session = session(Path::new("/work"));
    session
        .submit(
            "glob.fsh",
            "let matches = glob('*.fsh')\nexport NATIVE = $matches[0]",
            &NoExecutables,
            &platform,
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .expect("a wildcard should preserve the native match");

    assert_eq!(
        session.environment().get("NATIVE").map(OsStr::as_bytes),
        Some(native.as_os_str().as_bytes())
    );
}

#[test]
fn an_unavailable_directory_route_is_not_flattened_into_no_matches() {
    let mut session = session(Path::new("/work"));
    let error = session
        .submit(
            "glob.fsh",
            "glob('*')",
            &NoExecutables,
            &FakePlatform::none(),
            &FakeClock::new(),
            &mut Vec::new(),
        )
        .expect_err("the platform failure should surface");

    assert!(
        error.render().contains("DirectoryRead"),
        "{}",
        error.render()
    );
}

#[test]
fn a_lexical_callable_shadows_the_glob_intrinsic_without_platform_access() {
    let mut session = session(Path::new("/work"));
    let mut output = Vec::new();
    session
        .submit(
            "glob.fsh",
            "def glob(pattern: String) -> String { 'shadowed' }\n\
             export RESULT = glob('*.fsh')",
            &NoExecutables,
            &FakePlatform::none(),
            &FakeClock::new(),
            &mut output,
        )
        .expect("the lexical callable should win before platform access");

    assert!(output.is_empty());
    assert_eq!(
        session.environment().get("RESULT"),
        Some(OsStr::new("shadowed"))
    );
}

#[derive(Debug)]
struct NativeEntryPlatform {
    entries: Vec<DirectoryEntry>,
}

impl Platform for NativeEntryPlatform {
    fn capabilities(&self) -> Capabilities {
        Capabilities::full()
    }

    fn pipe(&self) -> Result<PipeEndpoints, flash_platform::PipeError> {
        FakePlatform::full().pipe()
    }

    fn open_file(
        &self,
        request: FileOpenRequest<'_>,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        FakePlatform::full().open_file(request)
    }

    fn inherit_descriptor(
        &self,
        descriptor: u32,
    ) -> Result<Box<dyn DescriptorEndpoint>, FileActionError> {
        FakePlatform::full().inherit_descriptor(descriptor)
    }

    fn read_directory(
        &self,
        _request: DirectoryReadRequest<'_>,
    ) -> Result<Box<dyn DirectoryStream>, DirectoryReadError> {
        Ok(Box::new(NativeEntryStream {
            entries: self.entries.clone().into_iter(),
        }))
    }

    fn spawn(&self, request: &SpawnRequest<'_>) -> Result<Box<dyn ChildProcess>, SpawnError> {
        FakePlatform::full().spawn(request)
    }
}

#[derive(Debug)]
struct NativeEntryStream {
    entries: std::vec::IntoIter<DirectoryEntry>,
}

impl DirectoryStream for NativeEntryStream {
    fn next_entry(&mut self) -> Result<Option<DirectoryEntry>, DirectoryReadError> {
        Ok(self.entries.next())
    }
}
