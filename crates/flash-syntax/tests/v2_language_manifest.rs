#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use flash_syntax::{
    ParseOutcome, SourceFile, SourceId, TokenKind, VersionedParseOutcome, lex, lex_v2, parse_v2,
    parse_v2_submission,
};

#[test]
fn lexical_manifest_is_the_exact_v1_v2_reserved_word_partition() {
    let root = language_root().join("lexical");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let mut spellings = BTreeSet::new();
    let mut v2_only = BTreeSet::new();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 3, "malformed lexical row {}", index + 1);
        let [spelling, v1_kind, v2_kind] = fields.as_slice() else {
            unreachable!()
        };
        assert!(spellings.insert(*spelling), "duplicate `{spelling}`");
        assert_eq!(*v2_kind, "keyword");

        let source = SourceFile::new(SourceId::new(1), "manifest", *spelling);
        assert_eq!(token_class(lex(&source)[0].kind()), *v1_kind);
        assert_eq!(token_class(lex_v2(&source)[0].kind()), *v2_kind);
        if *v1_kind == "identifier" {
            v2_only.insert(*spelling);
        }
    }

    assert_eq!(spellings.len(), 26);
    assert_eq!(
        v2_only,
        BTreeSet::from(["action", "enum", "language", "task", "type"])
    );
    let source = fs::read_to_string(root.join("reserved-words.fsh")).unwrap();
    assert_eq!(
        source.split_whitespace().collect::<BTreeSet<_>>(),
        spellings
    );
}

#[test]
fn grammar_manifest_executes_every_module_and_repl_boundary() {
    let root = language_root().join("grammar");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let mut observed = BTreeSet::new();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 4, "malformed grammar row {}", index + 1);
        let context = fields[0];
        let class = fields[1];
        let relative = fields[2];
        let code = fields[3];
        observed.insert((context, class));
        let text = fs::read_to_string(root.join(relative)).unwrap();
        let source = SourceFile::new(SourceId::new(2), relative, text);

        match (context, class) {
            ("module", "complete") => {
                assert!(matches!(
                    parse_v2(&source),
                    VersionedParseOutcome::Complete(_)
                ));
            }
            ("module", "incomplete") => {
                assert!(matches!(
                    parse_v2(&source),
                    VersionedParseOutcome::Incomplete(_)
                ));
            }
            ("module", "invalid") => {
                let VersionedParseOutcome::Invalid(diagnostics) = parse_v2(&source) else {
                    panic!("{relative} should be invalid");
                };
                assert_eq!(diagnostics[0].code(), code);
            }
            ("repl", "complete") => {
                assert!(matches!(
                    parse_v2_submission(&source),
                    ParseOutcome::Complete(_)
                ));
            }
            ("repl", "invalid") => {
                let ParseOutcome::Invalid(diagnostics) = parse_v2_submission(&source) else {
                    panic!("{relative} should be invalid");
                };
                assert_eq!(diagnostics[0].code(), code);
            }
            _ => panic!("unknown manifest route {context}/{class}"),
        }
    }

    assert_eq!(
        observed,
        BTreeSet::from([
            ("module", "complete"),
            ("module", "incomplete"),
            ("module", "invalid"),
            ("repl", "complete"),
            ("repl", "invalid"),
        ])
    );
}

#[test]
fn directive_detection_is_stable_across_leading_trivia_combinations() {
    let trivia = ["", " ", "\n", ";", "# note\n", "## docs\n"];
    for first in trivia {
        for second in trivia {
            let text = format!("{first}{second}language 2\nlet answer = 42\n");
            let source = SourceFile::new(SourceId::new(3), "property", text);
            assert!(matches!(
                parse_v2(&source),
                VersionedParseOutcome::Complete(_)
            ));
        }
    }
}

fn token_class(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Identifier => "identifier",
        TokenKind::Keyword(_) => "keyword",
        _ => panic!("manifest spelling produced {kind:?}"),
    }
}

fn language_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/v2-foundation/language")
}
