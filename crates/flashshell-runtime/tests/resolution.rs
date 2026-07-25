//! Acceptance tests for `^external` command resolution and `PATH` lookup.
//!
//! Resolution is a pure function of the expanded native name, the environment's
//! `PATH`, and an injected executable probe. These tests supply a fixed set of
//! executable paths, so they touch no real filesystem, process, or platform.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use flashshell_runtime::Environment;
use flashshell_runtime::resolve::{ExecutableProbe, ResolutionError, resolve_external};

/// A probe backed by an explicit set of native executable paths.
struct FakeExecutables(HashSet<OsString>);

impl FakeExecutables {
    fn new<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self(paths.into_iter().map(Into::into).collect())
    }
}

impl ExecutableProbe for FakeExecutables {
    fn is_executable(&self, path: &OsStr) -> bool {
        self.0.contains(path)
    }
}

fn env_with_path(value: &str) -> Environment {
    Environment::from_snapshot([("PATH", value)])
}

/// Extracts the searched name from a `NotFound`, failing on any other outcome.
fn not_found_name(error: ResolutionError) -> OsString {
    match error {
        ResolutionError::NotFound { name } => name,
        other => panic!("expected NotFound, found {other:?}"),
    }
}

#[test]
fn a_bare_name_resolves_in_the_first_matching_path_element() {
    let env = env_with_path("/usr/bin:/bin");
    let probe = FakeExecutables::new(["/bin/ls"]);

    let resolved = resolve_external(OsStr::new("ls"), &env, &probe).expect("ls resolves");

    assert_eq!(resolved.path(), Path::new("/bin/ls"));
}

#[test]
fn the_earliest_matching_element_wins_over_a_later_one() {
    let env = env_with_path("/usr/local/bin:/usr/bin");
    // Both directories hold an executable of the same name.
    let probe = FakeExecutables::new(["/usr/local/bin/git", "/usr/bin/git"]);

    let resolved = resolve_external(OsStr::new("git"), &env, &probe).expect("git resolves");

    assert_eq!(resolved.path(), Path::new("/usr/local/bin/git"));
}

#[test]
fn a_non_executable_earlier_candidate_is_skipped_for_a_later_directory() {
    let env = env_with_path("/usr/local/bin:/usr/bin");
    // Only the later directory actually holds the executable.
    let probe = FakeExecutables::new(["/usr/bin/cargo"]);

    let resolved = resolve_external(OsStr::new("cargo"), &env, &probe).expect("cargo resolves");

    assert_eq!(resolved.path(), Path::new("/usr/bin/cargo"));
}

#[test]
fn empty_path_elements_are_dropped_and_never_mean_the_working_directory() {
    // Leading, doubled, and trailing separators all produce empty elements.
    let env = env_with_path(":/usr/bin::");
    let probe = FakeExecutables::new(["/usr/bin/make"]);

    let resolved = resolve_external(OsStr::new("make"), &env, &probe).expect("make resolves");

    assert_eq!(resolved.path(), Path::new("/usr/bin/make"));
}

#[test]
fn a_path_like_name_resolves_to_itself_without_consulting_path() {
    let env = env_with_path("/usr/bin");
    // The PATH directory would match the basename, but a `/`-containing name is
    // never searched; only the name itself is probed.
    let probe = FakeExecutables::new(["./build.sh", "/usr/bin/build.sh"]);

    let resolved = resolve_external(OsStr::new("./build.sh"), &env, &probe).expect("resolves");

    assert_eq!(resolved.path(), Path::new("./build.sh"));
}

#[test]
fn a_path_like_name_the_probe_rejects_is_not_found() {
    let env = env_with_path("/usr/bin");
    let probe = FakeExecutables::new(["/usr/bin/absent"]);

    let error = resolve_external(OsStr::new("./absent"), &env, &probe).expect_err("not found");

    assert_eq!(not_found_name(error), OsString::from("./absent"));
}

#[test]
fn a_bare_name_with_no_path_entry_does_not_fall_back_to_the_working_directory() {
    let env = Environment::new();
    // An executable of that name exists in the working directory, but with no
    // PATH there are no search directories.
    let probe = FakeExecutables::new(["./tool", "tool"]);

    let error = resolve_external(OsStr::new("tool"), &env, &probe).expect_err("not found");

    assert_eq!(not_found_name(error), OsString::from("tool"));
}

#[test]
fn a_bare_name_no_element_accepts_is_not_found_with_the_searched_name() {
    let env = env_with_path("/usr/bin:/bin");
    let probe = FakeExecutables::new(["/usr/bin/other"]);

    let error = resolve_external(OsStr::new("missing"), &env, &probe).expect_err("not found");

    assert_eq!(not_found_name(error), OsString::from("missing"));
}

#[test]
fn a_path_of_only_empty_elements_has_no_search_directories() {
    let env = env_with_path(":::");
    let probe = FakeExecutables::new(["/anything"]);

    let error = resolve_external(OsStr::new("anything"), &env, &probe).expect_err("not found");

    assert_eq!(not_found_name(error), OsString::from("anything"));
}

#[test]
fn native_non_utf8_bytes_are_preserved_through_the_join() {
    // A PATH directory whose bytes are not valid UTF-8.
    let dir = OsString::from_vec(vec![b'/', b'o', 0xFF, b'/', b'b', b'i', b'n']);
    let mut path_value = dir.clone().into_vec();
    path_value.extend_from_slice(b"/prog");
    let candidate = OsString::from_vec(path_value);

    let env = Environment::from_snapshot([("PATH", dir)]);
    let probe = FakeExecutables::new([candidate.clone()]);

    let resolved = resolve_external(OsStr::new("prog"), &env, &probe).expect("resolves");

    assert_eq!(resolved.path().as_os_str().as_bytes(), candidate.as_bytes());
}
