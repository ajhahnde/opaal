#![forbid(unsafe_code)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

use flashshell_cli::highlight::{HighlightKind, SyntaxHighlighter};
use flashshell_syntax::{ParseOutcome, SourceFile, SourceId, parse};

#[test]
fn complete_source_uses_stable_semantic_categories_without_changing_text() {
    let source = "let cafe = 42\n^echo \"hello 💡 $name\" | check # note";
    let segments = SyntaxHighlighter::new().highlight(source);

    assert_lossless(source, &segments);
    assert_segment(&segments, "let", HighlightKind::Keyword);
    assert_segment(&segments, "42", HighlightKind::Literal);
    assert_segment(&segments, "^", HighlightKind::Operator);
    assert_segment(&segments, "\"", HighlightKind::String);
    assert_segment(&segments, "$name", HighlightKind::Expansion);
    assert_segment(&segments, "|", HighlightKind::Operator);
    assert_segment(&segments, "# note", HighlightKind::Comment);
    assert!(
        segments
            .iter()
            .all(|segment| segment.kind() != HighlightKind::Invalid)
    );
}

#[test]
fn incomplete_multiline_source_keeps_token_styles_without_error_coloring() {
    let source = "if true {\n    echo \"💡 $name";
    let parsed = SourceFile::new(SourceId::new(1), "<test>", source);
    assert!(matches!(parse(&parsed), ParseOutcome::Incomplete(_)));

    let segments = SyntaxHighlighter::new().highlight(source);
    assert_lossless(source, &segments);
    assert_segment(&segments, "if", HighlightKind::Keyword);
    assert_segment(&segments, "true", HighlightKind::Literal);
    assert_segment(&segments, "{", HighlightKind::Delimiter);
    assert_segment(&segments, "$name", HighlightKind::Expansion);
    assert!(
        segments
            .iter()
            .all(|segment| segment.kind() != HighlightKind::Invalid)
    );
}

#[test]
fn parser_and_lexer_errors_override_only_their_source_ranges() {
    let grammatical = "else echo still-visible";
    let parsed = SourceFile::new(SourceId::new(2), "<test>", grammatical);
    assert!(matches!(parse(&parsed), ParseOutcome::Invalid(_)));
    let grammatical_segments = SyntaxHighlighter::new().highlight(grammatical);
    assert_lossless(grammatical, &grammatical_segments);
    assert_segment(&grammatical_segments, "else", HighlightKind::Invalid);
    assert_segment(&grammatical_segments, "visible", HighlightKind::Plain);

    let lexical = "echo \"bad\\q but visible\"";
    let lexical_segments = SyntaxHighlighter::new().highlight(lexical);
    assert_lossless(lexical, &lexical_segments);
    assert_segment(&lexical_segments, "\\q", HighlightKind::Invalid);
    assert!(lexical_segments.iter().any(|segment| {
        segment.text().contains("but visible") && segment.kind() == HighlightKind::String
    }));
}

#[test]
fn empty_and_escape_heavy_unicode_buffers_are_ansi_free_and_exact() {
    let highlighter = SyntaxHighlighter::new();
    assert!(highlighter.highlight("").is_empty());

    let source = "echo 'λ' \\ space \"💡\\n${value}\"\n";
    let segments = highlighter.highlight(source);
    assert_lossless(source, &segments);
    assert_segment(&segments, "'λ'", HighlightKind::String);
    assert_segment(&segments, "\\ ", HighlightKind::Escape);
    assert_segment(&segments, "\\n", HighlightKind::Escape);
    assert_segment(&segments, "${", HighlightKind::Expansion);
    assert!(
        segments
            .iter()
            .all(|segment| !segment.text().contains('\u{1b}'))
    );
}

fn assert_lossless(source: &str, segments: &[flashshell_cli::highlight::HighlightSegment]) {
    assert_eq!(
        segments
            .iter()
            .map(flashshell_cli::highlight::HighlightSegment::text)
            .collect::<String>(),
        source
    );
    assert!(segments.iter().all(|segment| !segment.text().is_empty()));
}

fn assert_segment(
    segments: &[flashshell_cli::highlight::HighlightSegment],
    text: &str,
    kind: HighlightKind,
) {
    assert!(
        segments
            .iter()
            .any(|segment| segment.text() == text && segment.kind() == kind),
        "missing {kind:?} segment {text:?}: {segments:?}"
    );
}
