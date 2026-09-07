#![forbid(unsafe_code)]

use opaal_syntax::{
    LabelStyle, SourceFile, SourceId, SyntaxClassification, classify_opaal_tokens, lex_opaal,
};

#[test]
fn mismatched_and_stray_closing_delimiters_are_invalid_not_incomplete() {
    for (text, offending) in [("echo ([)]", ")"), ("echo }", "}")] {
        let source = SourceFile::new(SourceId::new(500), "delimiter.opaal", text);
        let tokens = lex_opaal(&source);
        let SyntaxClassification::Invalid(diagnostic) =
            classify_opaal_tokens(&source, &tokens).unwrap()
        else {
            panic!("expected invalid classification for {text:?}");
        };

        assert_eq!(diagnostic.code(), "OP0002");
        assert_eq!(diagnostic.message(), "unexpected closing delimiter");
        assert_eq!(diagnostic.labels().len(), 1);
        assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
        assert_eq!(
            source.slice(diagnostic.labels()[0].span()).unwrap(),
            offending
        );
    }
}

#[test]
fn invalid_source_characters_keep_an_exact_diagnostic_span_inside_word_parts() {
    for (text, offending) in [
        ("echo 'before\0after'", "\0"),
        ("# before\0after\n", "\0"),
        ("echo \\\0", "\0"),
        ("echo 'before\rafter'", "\r"),
    ] {
        let source = SourceFile::new(SourceId::new(600), "invalid-source.opaal", text);
        let tokens = lex_opaal(&source);
        let SyntaxClassification::Invalid(diagnostic) =
            classify_opaal_tokens(&source, &tokens).unwrap()
        else {
            panic!("expected invalid classification for {text:?}");
        };

        assert_eq!(diagnostic.code(), "OP0001");
        assert_eq!(
            source.slice(diagnostic.labels()[0].span()).unwrap(),
            offending
        );
    }
}
