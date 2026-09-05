#![forbid(unsafe_code)]

use flash_syntax::{
    ExpressionKind, FormatOutcome, ParseOutcome, SourceFile, SourceId, StageKind,
    VersionedParseOutcome, format_source_v2, parse, parse_v2,
};

#[test]
fn qualified_v2_pipeline_operation_is_an_expression_stage_and_formats_idempotently() {
    let source = SourceFile::new(
        SourceId::new(920),
        "operation.fsh",
        "language 2\n\n[1, 2] | value::length\n",
    );
    let VersionedParseOutcome::Complete(parsed) = parse_v2(&source) else {
        panic!("the v2 operation pipeline must parse");
    };
    let flash_syntax::StatementKind::Job(job) = parsed.script().statements()[0].kind() else {
        panic!("the operation pipeline is a job statement");
    };
    let pipeline = &job.chain.or_terms()[0].and_terms()[0];
    assert_eq!(pipeline.stages().len(), 2);
    assert!(matches!(
        pipeline.stages()[0].kind(),
        StageKind::Expression(_)
    ));
    let StageKind::Expression(operation) = pipeline.stages()[1].kind() else {
        panic!("the qualified pipeline operation must be an expression stage");
    };
    assert!(matches!(operation.kind(), ExpressionKind::Qualified(_)));

    let FormatOutcome::Complete(formatted) = format_source_v2(&source) else {
        panic!("the operation pipeline must format");
    };
    let reparsed = SourceFile::new(SourceId::new(921), "formatted.fsh", formatted.clone());
    assert_eq!(
        format_source_v2(&reparsed),
        FormatOutcome::Complete(formatted)
    );
}

#[test]
fn qualified_spelling_remains_a_command_head_in_frozen_v1() {
    let source = SourceFile::new(SourceId::new(922), "v1.fsh", "value::length\n");
    let ParseOutcome::Complete(parsed) = parse(&source) else {
        panic!("the frozen v1 spelling must still parse");
    };
    let flash_syntax::StatementKind::Job(job) = parsed.statements()[0].kind() else {
        panic!("the v1 spelling is a job statement");
    };
    let pipeline = &job.chain.or_terms()[0].and_terms()[0];
    assert!(matches!(pipeline.stages()[0].kind(), StageKind::Command(_)));
}
