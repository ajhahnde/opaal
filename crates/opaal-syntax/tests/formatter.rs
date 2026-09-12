#![forbid(unsafe_code)]

use opaal_syntax::{FormatOutcome, SourceFile, SourceId, format_source_opaal};

#[test]
fn horizontal_trivia_and_block_indentation_are_canonical() {
    let source = SourceFile::new(
        SourceId::new(3_000),
        "spacing.opaal",
        "def demo(name: string) {\n\techo   \"{name}\"   >   output # kept\n}\n",
    );

    assert_eq!(
        complete_format(&source),
        "def demo(name: string) {\n    echo \"{name}\" > output # kept\n}\n"
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
    let FormatOutcome::Complete(formatted) = format_source_opaal(source) else {
        panic!("expected complete format outcome");
    };
    formatted
}
