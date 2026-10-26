#![forbid(unsafe_code)]

use opaal_syntax::{
    BinaryOperator, ExpressionKind, FormatOutcome, Operator, ParseOutcome, SourceFile, SourceId,
    StatementKind, TokenKind, format_source_opaal, lex_opaal, parse_opaal,
};

#[test]
fn compound_assignment_lowers_once_with_original_operand_and_operator_spans() {
    for (spelling, token, operation) in [
        ("+=", Operator::AddAssign, BinaryOperator::Add),
        ("-=", Operator::SubtractAssign, BinaryOperator::Subtract),
        ("*=", Operator::MultiplyAssign, BinaryOperator::Multiply),
        ("/=", Operator::DivideAssign, BinaryOperator::Divide),
        ("%=", Operator::RemainderAssign, BinaryOperator::Remainder),
    ] {
        let text = format!("total{spelling}\n    amount + 2 * 3\n");
        let source = SourceFile::new(SourceId::new(801), "assignment.opaal", text);
        let tokens = lex_opaal(&source);
        let operator = tokens
            .iter()
            .find(|t| t.kind() == TokenKind::Operator(token))
            .unwrap();
        assert_eq!(source.slice(operator.span()).unwrap(), spelling);
        let ParseOutcome::Complete(script) = parse_opaal(&source) else {
            panic!("{source:?}")
        };
        assert_eq!(script.statements().len(), 1);
        let StatementKind::Assignment(assignment) = script.statements()[0].kind() else {
            panic!()
        };
        let ExpressionKind::Binary(binary) = assignment.value.kind() else {
            panic!()
        };
        assert_eq!(binary.operator.kind(), &operation);
        assert_eq!(binary.operator.span(), operator.span());
        assert_eq!(binary.left.kind(), &ExpressionKind::Name(assignment.target));
        assert_eq!(binary.left.span(), assignment.target.span);
        assert_eq!(source.slice(binary.right.span()).unwrap(), "amount + 2 * 3");
        assert!(matches!(binary.right.kind(), ExpressionKind::Binary(_)));
        let pending = SourceFile::new(
            SourceId::new(802),
            "pending.opaal",
            format!("total {spelling}\n"),
        );
        assert!(matches!(parse_opaal(&pending), ParseOutcome::Incomplete(_)));
    }
}

#[test]
fn compound_assignment_is_not_an_expression_or_a_chained_assignment() {
    for text in [
        "let n = (total += 1)",
        "total += other += 1",
        "total +=",
        "total +=\n",
        "total.field += 1",
        "total[0] += 1",
        "module::total += 1",
        "total + = 1",
    ] {
        let source = SourceFile::new(SourceId::new(803), "invalid.opaal", text);
        assert!(
            !matches!(parse_opaal(&source), ParseOutcome::Complete(_)),
            "{text}"
        );
    }
}

#[test]
fn formatting_spaces_only_assignment_tokens_and_preserves_command_words() {
    let command = "^echo a+=b a-=b a*=b a/=b a%=b += -= *= /= %= > a+=b\n";
    let text = format!(
        "mut total = 10\ntotal+=2 # 界🙂 kept\ntotal-=1\ntotal*=3\ntotal/=2\ntotal%=4\n{command}"
    );
    let expected = format!(
        "mut total = 10\ntotal += 2 # 界🙂 kept\ntotal -= 1\ntotal *= 3\ntotal /= 2\ntotal %= 4\n{command}"
    );
    let source = SourceFile::new(SourceId::new(804), "format.opaal", text);
    assert_eq!(
        format_source_opaal(&source),
        FormatOutcome::Complete(expected.clone())
    );
    let canonical = SourceFile::new(source.id(), "format.opaal", expected.clone());
    assert_eq!(
        format_source_opaal(&canonical),
        FormatOutcome::Complete(expected)
    );
}
