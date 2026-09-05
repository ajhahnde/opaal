use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use flash_migrate::{MigrationFormat, NativeSourceReader, analyze_roots};
use flash_syntax::{SourceFile, SourceId, VersionedParseOutcome, parse_v2};

const AUTOMATION_CORPUS: &str =
    include_str!("../../../tests/v2-foundation/migration/automation-corpus.tsv");

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("the migration crate lives below the repository root")
        .to_owned()
}

fn flash_root() -> PathBuf {
    repository_root().join("components/flash")
}

fn corpus_paths() -> Vec<&'static str> {
    let mut lines = AUTOMATION_CORPUS.lines();
    assert_eq!(lines.next(), Some("path"));
    lines.filter(|line| !line.is_empty()).collect()
}

#[test]
fn complete_sixty_script_automation_corpus_is_classified() {
    let repository = repository_root();
    let paths = corpus_paths();
    assert_eq!(paths.len(), 60);

    for relative in paths {
        let root = repository.join(relative);
        let report = analyze_roots(&NativeSourceReader, &[root])
            .unwrap_or_else(|error| panic!("migration analysis failed for {relative}: {error}"));
        assert!(!report.sources.is_empty(), "missing report for {relative}");
        assert!(
            report
                .sources
                .iter()
                .all(|source| source
                    .digest
                    .strip_prefix("sha256:")
                    .is_some_and(|digest| digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))),
            "invalid digest in {relative}"
        );
        assert!(
            report.sources.iter().all(|source| !source
                .findings
                .iter()
                .any(|finding| finding.code == "MIG1001")),
            "v1 parse failure in {relative}"
        );
        assert!(
            report.sources[0]
                .findings
                .iter()
                .any(|finding| finding.code == "MIG2001"),
            "missing language migration in {relative}"
        );
        for (source_index, migrated) in report.sources.iter().enumerate() {
            assert!(
                !migrated.findings.iter().any(|finding| {
                    finding.code.starts_with("MIG2")
                        && finding.severity == flash_migrate::FindingSeverity::Required
                        && finding.edit.is_none()
                }),
                "unresolved source edit in {relative}: {}",
                migrated.source_uri
            );
            let source = fs::read_to_string(&migrated.source_uri).unwrap_or_else(|error| {
                panic!(
                    "cannot reopen migration source {} from {relative}: {error}",
                    migrated.source_uri
                )
            });
            let applied = apply_edits(&source, migrated);
            if source.starts_with("#!") {
                assert!(
                    applied.starts_with("#!/usr/bin/env fsh\nlanguage 2\n"),
                    "migration moved or replaced the executable shebang in {}",
                    migrated.source_uri
                );
            }
            let parsed = parse_v2(&SourceFile::new(
                SourceId::new(u32::try_from(source_index + 1).unwrap()),
                migrated.source_uri.clone(),
                applied,
            ));
            assert!(
                matches!(parsed, VersionedParseOutcome::Complete(_)),
                "safe edits for {} from {relative} do not parse as Flash 2: {parsed:?}",
                migrated.source_uri
            );
        }
    }

    let build = analyze_roots(&NativeSourceReader, &[repository.join("build.fsh")]).unwrap();
    assert!(build.sources[0].findings.iter().any(|finding| {
        finding.code == "MIG2004"
            && finding.severity == flash_migrate::FindingSeverity::Suggested
            && finding.edit.is_none()
    }));
}

#[test]
fn focused_import_fixture_has_depth_first_sources_and_parseable_edits() {
    let root = flash_root().join("tests/v2-foundation/migration/imports/root.fsh");
    let report = analyze_roots(&NativeSourceReader, std::slice::from_ref(&root)).unwrap();
    assert_eq!(report.sources.len(), 2);
    assert!(report.sources[0].source_uri.ends_with("/imports/root.fsh"));
    assert!(
        report.sources[1]
            .source_uri
            .ends_with("/imports/./support.fsh")
    );

    let support = root.with_file_name("support.fsh");
    for (index, path) in [root, support].into_iter().enumerate() {
        let source = fs::read_to_string(path).unwrap();
        let migrated = apply_edits(&source, &report.sources[index]);
        let parsed = parse_v2(&SourceFile::new(
            SourceId::new(u32::try_from(index + 1).unwrap()),
            report.sources[index].source_uri.clone(),
            migrated,
        ));
        assert!(
            matches!(parsed, VersionedParseOutcome::Complete(_)),
            "applied migration must parse as Flash 2: {parsed:?}"
        );
    }
}

fn apply_edits(source: &str, migrated: &flash_migrate::MigrationSource) -> String {
    let mut edits = migrated
        .findings
        .iter()
        .filter_map(|finding| finding.edit.as_ref())
        .collect::<Vec<_>>();
    edits.sort_by_key(|edit| (edit.start, edit.end));
    for pair in edits.windows(2) {
        assert!(pair[0].end <= pair[1].start, "migration edits overlap");
    }
    let mut output = source.to_owned();
    for edit in edits.into_iter().rev() {
        output.replace_range(edit.start..edit.end, &edit.replacement);
    }
    output
}

#[test]
fn cli_json_preserves_percent_encoded_source_uri_and_exact_statuses() {
    let flash = flash_root();
    let binary = env!("CARGO_BIN_EXE_fsh-migrate-v1-v2");
    let required = Command::new(binary)
        .current_dir(&flash)
        .args([
            "--format",
            "json",
            "tests/v2-foundation/migration/source uri/naïve.fsh",
        ])
        .output()
        .unwrap();
    assert_eq!(required.status.code(), Some(1));
    assert!(required.stderr.is_empty());
    let stdout = String::from_utf8(required.stdout).unwrap();
    assert!(stdout.ends_with('\n'));
    assert!(
        stdout.contains(
            "\"source_uri\":\"tests/v2-foundation/migration/source%20uri/na%C3%AFve.fsh\""
        )
    );
    assert_eq!(stdout.lines().count(), 1);

    let misuse = Command::new(binary)
        .arg("--format")
        .arg("yaml")
        .output()
        .unwrap();
    assert_eq!(misuse.status.code(), Some(2));
    assert!(misuse.stdout.is_empty());
    assert!(String::from_utf8(misuse.stderr).unwrap().contains("usage:"));

    let clean = Command::new(binary)
        .current_dir(&flash)
        .args([
            "--format",
            "json",
            "tests/v2-foundation/language/grammar/complete/directive-only.fsh",
        ])
        .output()
        .unwrap();
    assert_eq!(clean.status.code(), Some(0));
    assert!(clean.stderr.is_empty());
    assert!(
        String::from_utf8(clean.stdout)
            .unwrap()
            .contains("\"findings\":[]")
    );

    let unavailable = Command::new(binary)
        .args(["--format", "json", "this-source-does-not-exist.fsh"])
        .output()
        .unwrap();
    assert_eq!(unavailable.status.code(), Some(1));
    assert!(unavailable.stderr.is_empty());
    let unavailable_json = String::from_utf8(unavailable.stdout).unwrap();
    assert!(unavailable_json.contains("\"complete\":false"));
    assert!(unavailable_json.contains("\"kind\":\"source\""));
}

#[test]
fn json_rendering_is_byte_deterministic() {
    let root = flash_root().join("tests/v2-foundation/migration/imports/root.fsh");
    let first = analyze_roots(&NativeSourceReader, std::slice::from_ref(&root)).unwrap();
    let second = analyze_roots(&NativeSourceReader, &[root]).unwrap();
    assert_eq!(
        first.render(MigrationFormat::Json).unwrap(),
        second.render(MigrationFormat::Json).unwrap()
    );
}

#[test]
fn golden_workflow_reports_the_exact_preserved_source_migration() {
    let flash = flash_root();
    let relative = "tests/v2-foundation/v1/preserved.fsh";
    let path = flash.join(relative);
    let before = fs::read(&path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fsh-migrate-v1-v2"))
        .current_dir(&flash)
        .args(["--format", "json", relative])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(&path).unwrap(), before, "analysis must not write");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "{\"schema\":1,\"sources\":[{\"source_uri\":\"tests/v2-foundation/v1/preserved.fsh\",",
            "\"digest\":\"sha256:a604ad1257b89169ad1c4add15f9a99a897fd3c19bdc42bb071d52dda4420beb\",",
            "\"detected_language\":1,\"target_language\":2,\"findings\":[{\"code\":\"MIG2001\",",
            "\"severity\":\"required\",\"start\":0,\"end\":0,",
            "\"message\":\"add 'language 2' before the first statement\",",
            "\"edit\":{\"start\":0,\"end\":0,\"replacement\":\"language 2\\n\\n\"}}],",
            "\"unresolved\":false}]}\n",
        )
    );
}
