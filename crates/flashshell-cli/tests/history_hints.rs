#![forbid(unsafe_code)]

use flashshell_cli::hint::{
    HintCatalog, HintEngine, MAX_HINT_BUFFER_BYTES, MAX_HINT_HISTORY_ENTRIES, MAX_HINT_SUFFIX_BYTES,
};

#[test]
fn newest_longer_exact_prefix_supplies_only_its_suffix() {
    let catalog = HintCatalog::new([
        "echo hello again",
        "echo hello",
        "echo historic",
        "echo hello from older history",
    ]);
    let engine = HintEngine::new();

    let hint = engine
        .hint("echo h", "echo h".len(), &catalog)
        .expect("a newer longer prefix match should hint");
    assert_eq!(hint.suffix(), "ello again");

    let exact_is_skipped = engine
        .hint("echo hello", "echo hello".len(), &catalog)
        .expect("an exact entry should not hide another longer match");
    assert_eq!(exact_is_skipped.suffix(), " again");
}

#[test]
fn cursor_and_parser_state_make_acceptance_append_only() {
    let catalog = HintCatalog::new([
        "echo hello world",
        "if true {\n    echo yes\n}",
        "echo \"λ-world\"",
    ]);
    let engine = HintEngine::new();

    assert!(engine.hint("echo hello", 4, &catalog).is_none());
    assert!(engine.hint("else", "else".len(), &catalog).is_none());

    let multiline = "if true {\n    echo";
    assert_eq!(
        engine
            .hint(multiline, multiline.len(), &catalog)
            .expect("incomplete multiline source may use exact history")
            .suffix(),
        " yes\n}"
    );

    let unicode = "echo \"λ";
    assert_eq!(
        engine
            .hint(unicode, unicode.len(), &catalog)
            .expect("UTF-8 prefixes should remain byte exact")
            .suffix(),
        "-world\""
    );
}

#[test]
fn empty_or_absent_history_never_reveals_a_command() {
    let engine = HintEngine::new();
    let populated = HintCatalog::new(["export API_TOKEN = \"private\""]);

    assert!(engine.hint("", 0, &populated).is_none());
    assert!(
        engine
            .hint("export API", "export API".len(), &HintCatalog::default())
            .is_none()
    );
}

#[test]
fn work_and_rendering_are_bounded_without_truncating_source() {
    let engine = HintEngine::new();
    let oversized_input = "x".repeat(MAX_HINT_BUFFER_BYTES + 1);
    let oversized_suffix = format!("go {}", "x".repeat(MAX_HINT_SUFFIX_BYTES + 1));
    let entries = (0..(MAX_HINT_HISTORY_ENTRIES + 5))
        .map(|index| format!("miss {index}"))
        .chain(["go beyond bound".to_owned()]);
    let catalog = HintCatalog::new(entries);
    let exact_size_catalog = HintCatalog::new([oversized_suffix, "go bounded".to_owned()]);

    assert!(
        engine
            .hint(&oversized_input, oversized_input.len(), &catalog)
            .is_none()
    );
    assert!(engine.hint("go ", 3, &catalog).is_none());
    assert_eq!(
        engine
            .hint("go ", 3, &exact_size_catalog)
            .expect("an oversized suffix should be skipped, not truncated")
            .suffix(),
        "bounded"
    );
}
