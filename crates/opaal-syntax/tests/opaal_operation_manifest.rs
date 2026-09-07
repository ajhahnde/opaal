#![forbid(unsafe_code)]

use opaal_syntax::{
    ExpressionKind, FormatOutcome, ParseOutcome, SourceFile, SourceId, StageKind,
    format_source_opaal, parse_opaal,
};

#[test]
fn qualified_opaal_pipeline_operation_is_an_expression_stage_and_formats_idempotently() {
    let source = SourceFile::new(
        SourceId::new(920),
        "operation.opaal",
        "\n[1, 2] | value::length\n",
    );
    let ParseOutcome::Complete(parsed) = parse_opaal(&source) else {
        panic!("the opaal operation pipeline must parse");
    };
    let opaal_syntax::StatementKind::Job(job) = parsed.statements()[0].kind() else {
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

    let FormatOutcome::Complete(formatted) = format_source_opaal(&source) else {
        panic!("the operation pipeline must format");
    };
    let reparsed = SourceFile::new(SourceId::new(921), "formatted.opaal", formatted.clone());
    assert_eq!(
        format_source_opaal(&reparsed),
        FormatOutcome::Complete(formatted)
    );
}
