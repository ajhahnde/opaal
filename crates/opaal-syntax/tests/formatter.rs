#![forbid(unsafe_code)]

use opaal_syntax::{FormatOutcome, SourceFile, SourceId, format_source_opaal};

#[path = "support/formatting.rs"]
mod formatting;

#[test]
fn horizontal_trivia_and_block_indentation_are_canonical() {
    let source = SourceFile::new(
        SourceId::new(3_000),
        "spacing.opaal",
        "def demo(name: string) {\n\techo   \"{name}\"   >   output # kept\n}\n",
    );

    assert_eq!(
        complete_format(&source),
        "def demo(name: string)\n{\n    echo \"{name}\" > output # kept\n}\n"
    );
}

#[test]
fn interpolation_spacing_is_canonical() {
    let source = SourceFile::new(
        SourceId::new(3_005),
        "interpolation.opaal",
        "echo \"pre{  name   }post\" ...{  [\"a\", \"b\"]   }\n",
    );

    assert_eq!(
        complete_format(&source),
        "echo \"pre{name}post\" ...{[\"a\", \"b\"]}\n"
    );
}

#[test]
fn incomplete_and_invalid_inputs_keep_their_parse_outcomes() {
    let incomplete = SourceFile::new(SourceId::new(3_001), "incomplete.opaal", "echo \"");
    let FormatOutcome::Incomplete(reason) = format_source_opaal(&incomplete) else {
        panic!("expected incomplete format outcome");
    };
    assert_eq!(reason.reason(), "unmatched double quote");

    let invalid = SourceFile::new(SourceId::new(3_002), "invalid.opaal", "| broken\n");
    let FormatOutcome::Invalid(diagnostics) = format_source_opaal(&invalid) else {
        panic!("expected invalid format outcome");
    };
    assert_eq!(
        diagnostics[0].message(),
        "pipeline operator cannot begin a stage"
    );
}

fn complete_format(source: &SourceFile) -> String {
    let outcome = format_source_opaal(source);
    let FormatOutcome::Complete(formatted) = outcome else {
        panic!("expected complete format outcome: {outcome:?}, source: {source:?}");
    };
    formatted
}

fn roundtrip(text: &str) -> String {
    let source = SourceFile::new(SourceId::new(3_006), "roundtrip.opaal", text);
    formatting::assert_roundtrip(&source)
}

#[test]
fn action_layout_preserves_punctuation_inside_command_words() {
    for command in [",eturn value", "echo key:value a,b"] {
        for source in [
            format!("action sample() -> Int {{ effects {{}}  {command} }}\n"),
            format!("action sample() -> Int {{ effects {{}}  if true {{ {command} }} }}\n"),
        ] {
            let formatted = roundtrip(&source);
            assert!(formatted.contains(command), "{formatted}");
        }
    }
    roundtrip(
        "##Keep the same value.\naction identity(value:String)->String # signature\n{\neffects{ clock.wall, # declaration only\n}\n# body comment\n ,eturn value }\n",
    );
}

#[test]
fn contained_requests_insert_only_a_final_comma_before_comments() {
    let text = "action value() -> String { effects { # open\nfilesystem.read('a,;{}'), # first\nclock.wall # last\n} # close\nreturn 'line\r\n界🙂' }\n";
    let formatted = roundtrip(text);
    assert_eq!(
        formatted,
        "action value() -> String\n{\n    effects { # open\n        filesystem.read('a,;{}'), # first\n        clock.wall, # last\n    } # close\n    return 'line\r\n界🙂'\n}\n"
    );
}

#[test]
fn semantic_oracle_detects_changed_spellings_structure_and_documentation_attachment() {
    use opaal_syntax::{ParseOutcome, parse_opaal};
    let signature = |text: &str| {
        let file = SourceFile::new(SourceId::new(42), "oracle.opaal", text);
        let ParseOutcome::Complete(script) = parse_opaal(&file) else {
            panic!("oracle input must parse: {text:?}");
        };
        formatting::semantic_signature(&file, &script)
    };
    for (before, after) in [
        ("let value = 1\n", "let other = 1\n"),
        ("let value = 1\n", "let value = 2\n"),
        ("let value = 1.5\n", "let value = 1.6\n"),
        ("let value = '界'\n", "let value = '海'\n"),
        ("echo \"pre{value}post\"\n", "echo \"pre{other}post\"\n"),
        ("echo first second\n", "echo second first\n"),
        ("echo value 2>&1\n", "echo value 1>&2\n"),
        ("let value = {'key': 1}\n", "let value = {'other': 1}\n"),
        (
            "import 'one.opaal' as module\n",
            "import 'two.opaal' as module\n",
        ),
        (
            "def identity(value: Int) -> Int { return value }\n",
            "def identity(value: String) -> Int { return value }\n",
        ),
        (
            "action value() -> Int { effects { clock.wall, }  return 1 }\n",
            "action value() -> Int { effects { clock.monotonic, }  return 1 }\n",
        ),
        (
            "def value() { return 1; 2 }\n",
            "def value() { return 1 + 2 }\n",
        ),
        (
            "##Same doc\ndef one() {}\ndef two() {}\n",
            "def one() {}\n##Same doc\ndef two() {}\n",
        ),
    ] {
        assert_ne!(
            signature(before),
            signature(after),
            "{before:?} versus {after:?}"
        );
    }
    assert_eq!(
        signature("def value() { return 1 }\n"),
        signature("def value()\n{\n    return 1\n}\n")
    );
}

#[test]
fn complete_repository_sources_preserve_resolved_semantics() {
    use opaal_syntax::{ParseOutcome, parse_opaal};
    use std::path::Path;

    fn visit(path: &Path, count: &mut usize) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, count);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "opaal")
            {
                let text = std::fs::read_to_string(&path).unwrap();
                let file = SourceFile::new(SourceId::new(0), path.display().to_string(), text);
                if matches!(parse_opaal(&file), ParseOutcome::Complete(_)) {
                    formatting::assert_roundtrip(&file);
                    *count += 1;
                }
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut count = 0;
    visit(&root.join("tests"), &mut count);
    visit(&root.join("examples"), &mut count);
    assert!(
        count >= 100,
        "expected the complete repository corpus, found {count}"
    );
}

#[test]
fn typed_untyped_generic_and_nested_declarations_share_body_layout() {
    let text = "def identity(\n value : String\n) ->\nString { return value }\ndef five() {\ndef inner[T](value:T)->T { return value }\nreturn inner(5)\n}\n";
    assert_eq!(
        roundtrip(text),
        concat!(
            "def identity(value: String) -> String\n{\n    return value\n}\n",
            "def five()\n{\n    def inner[T](value: T) -> T\n    {\n        return value\n    }\n    return inner(5)\n}\n",
        )
    );
    assert_eq!(roundtrip("def empty() {}\n"), "def empty()\n{\n}\n");
}

#[test]
fn every_declaration_block_is_visited_and_other_braces_keep_their_policy() {
    let nested = "def nested() { return 1 }";
    for block in [
        format!("def outer() {{\n{nested}\n}}\n"),
        format!("action outer() -> Int {{ effects {{}} \n{nested}\n}}\n"),
        format!("if true {{\n{nested}\n}} else {{\n{nested}\n}}\n"),
        format!("if true {{}} else if false {{\n{nested}\n}}\n"),
        format!("while true {{\n{nested}\n}}\n"),
        format!("for item in [1] {{\n{nested}\n}}\n"),
        format!("match 1 {{\nvalue => {{\n{nested}\n}}\n}}\n"),
        format!("try {{\n{nested}\n}} catch error {{\n{nested}\n}}\n"),
    ] {
        let formatted = roundtrip(&block);
        assert!(!formatted.contains("nested() {"), "{formatted:?}");
        assert!(formatted.contains("def nested()\n"));
    }
    for other in [
        "if true {} else {}\nwhile false {}\nfor item in [1] {}\n",
        "match 1 { value => {} }\ntry {} catch error {}\n",
        "type Row = { value: Int }\nenum Answer { Yes, No }\n",
        "let closure = {|value: Int| -> Int value}\nlet record = {value: 1}\n",
        "let words = ['def fake() {}', \"literal {1}\"]\n",
    ] {
        assert_eq!(roundtrip(other), other);
    }
}

#[test]
fn forced_breaks_preserve_trailing_comments_and_blank_lines() {
    for text in [
        "##Keep the same value.\naction identity(value: String) -> String # signature\n{\n    effects { # effects-open\n        clock.wall, # request\n    } # effects-close\n    # before body\n    # body-open\n    return value\n} # body-close\n",
        "##Keep the same value.\ndef identity(value: String) -> String # signature\n\n# before body\n{ # body-open\n\n    return value\n\n} # body-close\n\n",
        "def empty()\n{\n\n}\n\n# after\n",
        "def empty()\n\n{\n}\n",
        "action empty() -> Int\n\n{\n    effects {\n    }\n\n\n}\n",
    ] {
        assert_eq!(roundtrip(text), text);
    }
    assert_eq!(
        roundtrip("action empty()->Int { effects{}} # close\n"),
        "action empty() -> Int\n{\n    effects {\n    }\n} # close\n"
    );
}

#[test]
fn comment_continuation_and_long_signatures_keep_source_wrapping() {
    for signature in [
        "def identity(# parameter\nvalue: String\n) -> String",
        "def identity(\\\nvalue: String) -> String",
        "def identity(\nextraordinarily_long_parameter_name: String,\nanother_extraordinarily_long_parameter_name: String\n) -> String",
    ] {
        let text = format!("{signature}\n{{\n    return value\n}}\n");
        assert_eq!(roundtrip(&text), text);
    }
}

#[test]
fn literal_bytes_and_non_declaration_trivia_survive() {
    for text in [
        "",
        "# comment",
        "## documentation\n# comment\n",
        "echo \"line\none\"\n",
        "echo 'a\\b' # kept\r\n",
    ] {
        let formatted = roundtrip(text);
        if text.is_empty() {
            assert!(formatted.is_empty());
        }
    }
    assert_eq!(roundtrip("def empty() {}\r\n"), "def empty()\n{\n}\n");
}

#[test]
fn canonical_goldens_and_bounded_large_sources_roundtrip() {
    assert_eq!(
        roundtrip(include_str!(
            "../../../tests/golden/source-formatting/legacy.opaal"
        )),
        include_str!("../../../tests/golden/source-formatting/canonical.opaal"),
    );
    let comments = include_str!("../../../tests/golden/source-formatting/action-comments.opaal");
    assert_eq!(roundtrip(comments), comments);

    let siblings = (0..1024)
        .map(|index| format!("def function_{index}() {{ return {index} }}\n"))
        .collect::<String>();
    assert_eq!(roundtrip(&siblings).matches("()\n{").count(), 1024);
    let nested = format!(
        "{}return 1\n{}",
        "def nested() {\n".repeat(64),
        "}\n".repeat(64)
    );
    assert_eq!(roundtrip(&nested).matches("def nested()\n").count(), 64);
    let large = format!(
        "# {}\ndef unicode(value: String) -> String {{ return value }}\n",
        "界🙂".repeat(40_000)
    );
    assert!(large.len() >= 256 * 1024);
    roundtrip(&large);
}

#[test]
fn import_groups_are_multiline_and_insert_only_the_missing_final_comma_before_comments() {
    for (input, expected) in [
        (
            "import {std::math,std::io}",
            "import {\n    std::math,\n    std::io,\n}\n",
        ),
        ("import {std::math,}", "import {\n    std::math,\n}\n"),
        (
            "import {std::math # final\n}",
            "import {\n    std::math, # final\n}\n",
        ),
        (
            "import {std::math ## final\n}",
            "import {\n    std::math, ## final\n}\n",
        ),
        (
            "import {'./a,b}.opaal' as tools}",
            "import {\n    './a,b}.opaal' as tools,\n}\n",
        ),
    ] {
        assert_eq!(roundtrip(input), expected);
    }
    roundtrip(
        "import { # start\n std::math # origin\n as # alias\n m, ## between\n './界/a.opaal' # final\n}\nimport std::io\n",
    );
    for input in ["import {", "import {std::math,", "import std::math as"] {
        let source = SourceFile::new(SourceId::new(9), "incomplete", input);
        assert!(matches!(
            format_source_opaal(&source),
            FormatOutcome::Incomplete(_)
        ));
    }
}

#[test]
fn member_imports_format_compact_and_comment_bearing_selectors() {
    assert_eq!(
        roundtrip("import std::math :: { abs ,floor as round_down }\n"),
        "import std::math::{abs, floor as round_down}\n"
    );
    assert_eq!(
        roundtrip("import std::math::{\nabs, # magnitude\nfloor as round_down\n}\n"),
        "import std::math::{\n    abs, # magnitude\n    floor as round_down,\n}\n"
    );
    assert_eq!(
        roundtrip("import {std::math::{abs},std::io}\n"),
        "import {\n    std::math::{abs},\n    std::io,\n}\n"
    );
}

#[test]
fn list_literal_spreads_preserve_comments_tokens_and_one_level_spelling() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/source-formatting/list-literal-spread");
    let source = SourceFile::new(
        SourceId::new(3_100),
        "spread.opaal",
        std::fs::read_to_string(root.join("noncanonical.opaal")).unwrap(),
    );
    assert_eq!(
        complete_format(&source),
        std::fs::read_to_string(root.join("canonical.opaal")).unwrap()
    );
    formatting::assert_roundtrip(&source);
}

#[test]
fn list_spread_spacing_keeps_existing_command_and_pattern_grammars() {
    let source = SourceFile::new(
        SourceId::new(3_101),
        "rest.opaal",
        "let [first, ...rest] = [1, 2]\n",
    );
    assert_eq!(complete_format(&source), source.text());
    for text in ["let [first, ... rest] = [1, 2]", "^echo ... {items}"] {
        let source = SourceFile::new(SourceId::new(3_102), "invalid.opaal", text);
        assert!(matches!(
            format_source_opaal(&source),
            FormatOutcome::Invalid(_)
        ));
    }
}

#[test]
fn export_prefixes_tighten_only_the_marker_and_keep_comments_and_order() {
    assert_eq!(
        roundtrip("export   { exact, check \t* , Item }\n"),
        "export { exact, check*, Item }\n"
    );
    let formatted = roundtrip("export {\n# selectors\ncheck *, # local\nexact,\n}\n");
    assert!(formatted.contains("check*, # local"), "{formatted}");
    assert!(formatted.contains("# selectors"));
    assert!(formatted.find("check*").unwrap() < formatted.find("exact").unwrap());
}
