#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use opaal_syntax::{ParseOutcome, SourceFile, SourceId, TokenKind, lex_opaal, parse_opaal};

#[test]
fn lexical_manifest_is_the_exact_opaal_reserved_word_set() {
    let root = language_root().join("lexical");
    let manifest = fs::read_to_string(root.join("manifest.tsv")).unwrap();
    let mut spellings = BTreeSet::new();

    for (index, row) in manifest.lines().enumerate() {
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let fields = row.split('\t').collect::<Vec<_>>();
        assert_eq!(fields.len(), 2, "malformed lexical row {}", index + 1);
        let [spelling, opaal_kind] = fields.as_slice() else {
            unreachable!()
        };
        assert!(spellings.insert(*spelling), "duplicate `{spelling}`");
        assert_eq!(*opaal_kind, "keyword");

        let source = SourceFile::new(SourceId::new(1), "manifest", *spelling);
        assert_eq!(token_class(lex_opaal(&source)[0].kind()), *opaal_kind);
    }

    assert_eq!(spellings.len(), 25);
    let source = fs::read_to_string(root.join("reserved-words.opaal")).unwrap();
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
                assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
            }
            ("module", "incomplete") => {
                assert!(matches!(parse_opaal(&source), ParseOutcome::Incomplete(_)));
            }
            ("module", "invalid") => {
                let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
                    panic!("{relative} should be invalid");
                };
                assert_eq!(diagnostics[0].code(), code);
            }
            ("repl", "complete") => {
                assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
            }
            ("repl", "invalid") => {
                let ParseOutcome::Invalid(diagnostics) = parse_opaal(&source) else {
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
fn directive_free_parsing_is_stable_across_leading_trivia_combinations() {
    let trivia = ["", " ", "\n", ";", "# note\n", "## docs\n"];
    for first in trivia {
        for second in trivia {
            let text = format!("{first}{second}let answer = 42\n");
            let source = SourceFile::new(SourceId::new(3), "property", text);
            assert!(matches!(parse_opaal(&source), ParseOutcome::Complete(_)));
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
        .join("tests/opaal-foundation/language")
}
