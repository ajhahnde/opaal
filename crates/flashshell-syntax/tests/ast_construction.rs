#![forbid(unsafe_code)]

use flashshell_syntax::{
    AstNode, BinaryExpression, BinaryOperator, CommandHead, CommandHeadKind, CommandItem,
    CommandItemKind, CommandStage, Expression, ExpressionKind, FileRedirection, Identifier,
    IoNumber, Literal, LiteralKind, OutputMode, Redirection, RedirectionKind, Script, SourceFile,
    SourceId, Statement, StatementKind, Word, WordPart, WordPartKind,
};

#[test]
fn nodes_and_word_parts_retain_exact_source_spans() {
    let source = SourceFile::new(
        SourceId::new(70),
        "ast.fsh",
        "let total = 1 + 2\necho pre\"$name\"post\n",
    );

    let addition = Expression::new(
        ExpressionKind::Binary(BinaryExpression {
            left: Box::new(integer(&source, 12..13)),
            operator: AstNode::new(BinaryOperator::Add, span(&source, 14..15)),
            right: Box::new(integer(&source, 16..17)),
        }),
        span(&source, 12..17),
    );
    let declaration = Statement::new(
        StatementKind::Declaration(flashshell_syntax::Declaration {
            mutable: false,
            name: Identifier::new(span(&source, 4..9)),
            type_annotation: None,
            value: addition,
        }),
        span(&source, 0..17),
    );

    let double_quoted = WordPart::new(
        WordPartKind::DoubleQuoted(vec![WordPart::new(
            WordPartKind::Variable(Identifier::new(span(&source, 28..32))),
            span(&source, 27..32),
        )]),
        span(&source, 26..33),
    );
    let argument = Word::new(
        vec![
            WordPart::new(WordPartKind::Bare, span(&source, 23..26)),
            double_quoted,
            WordPart::new(WordPartKind::Bare, span(&source, 33..37)),
        ],
        span(&source, 23..37),
    );
    let command = Statement::new(
        StatementKind::Job(flashshell_syntax::JobStatement {
            chain: flashshell_syntax::ConditionalChain::from_pipeline(
                flashshell_syntax::Pipeline::from_stage(AstNode::new(
                    flashshell_syntax::StageKind::Command(CommandStage {
                        head: CommandHead::new(
                            CommandHeadKind::Bare,
                            Word::new(
                                vec![WordPart::new(WordPartKind::Bare, span(&source, 18..22))],
                                span(&source, 18..22),
                            ),
                            span(&source, 18..22),
                        ),
                        items: vec![CommandItem::new(
                            CommandItemKind::Word(argument),
                            span(&source, 23..37),
                        )],
                    }),
                    span(&source, 18..37),
                )),
            ),
            background_span: None,
        }),
        span(&source, 18..37),
    );
    let script = Script::new(vec![declaration, command], span(&source, 0..38));

    assert_eq!(source.slice(script.span()).unwrap(), &source.text()[0..38]);
    let StatementKind::Declaration(declaration) = script.statements()[0].kind() else {
        panic!("expected declaration");
    };
    let ExpressionKind::Binary(addition) = declaration.value.kind() else {
        panic!("expected binary expression");
    };
    assert_eq!(source.slice(addition.operator.span()).unwrap(), "+");

    let StatementKind::Job(job) = script.statements()[1].kind() else {
        panic!("expected command job");
    };
    let flashshell_syntax::StageKind::Command(stage) =
        job.chain.or_terms()[0].and_terms()[0].stages()[0].kind()
    else {
        panic!("expected command stage");
    };
    let CommandItemKind::Word(word) = stage.items[0].kind() else {
        panic!("expected word argument");
    };
    assert_eq!(word.parts().len(), 3);
    assert_eq!(source.slice(word.parts()[1].span()).unwrap(), "\"$name\"");
    let WordPartKind::DoubleQuoted(parts) = word.parts()[1].kind() else {
        panic!("expected double-quoted part");
    };
    assert_eq!(source.slice(parts[0].span()).unwrap(), "$name");
}

#[test]
fn command_items_and_redirections_remain_in_source_order() {
    let source = SourceFile::new(
        SourceId::new(71),
        "redirects.fsh",
        "^build first 2>errors second >output 2>&1",
    );

    let items = vec![
        word_item(&source, 7..12),
        redirect_item(
            &source,
            13..21,
            RedirectionKind::File(FileRedirection {
                descriptor: Some(IoNumber::new(span(&source, 13..14))),
                mode: OutputMode::Truncate,
                operator_span: span(&source, 14..15),
                target: bare_word(&source, 15..21),
            }),
        ),
        word_item(&source, 22..28),
        redirect_item(
            &source,
            29..36,
            RedirectionKind::File(FileRedirection {
                descriptor: None,
                mode: OutputMode::Truncate,
                operator_span: span(&source, 29..30),
                target: bare_word(&source, 30..36),
            }),
        ),
        redirect_item(
            &source,
            37..41,
            RedirectionKind::Duplicate {
                descriptor: IoNumber::new(span(&source, 37..38)),
                operator_span: span(&source, 38..40),
                target: IoNumber::new(span(&source, 40..41)),
            },
        ),
    ];
    let stage = CommandStage {
        head: CommandHead::new(
            CommandHeadKind::ForcedExternal,
            bare_word(&source, 1..6),
            span(&source, 0..6),
        ),
        items,
    };

    assert_eq!(stage.items.len(), 5);
    assert!(matches!(stage.items[0].kind(), CommandItemKind::Word(_)));
    assert!(matches!(
        stage.items[1].kind(),
        CommandItemKind::Redirection(_)
    ));
    assert!(matches!(stage.items[2].kind(), CommandItemKind::Word(_)));
    assert_eq!(
        stage
            .redirections()
            .map(|redirection| source.slice(redirection.span()).unwrap())
            .collect::<Vec<_>>(),
        vec!["2>errors", ">output", "2>&1"]
    );
}

fn integer(source: &SourceFile, range: std::ops::Range<usize>) -> Expression {
    Expression::new(
        ExpressionKind::Literal(Literal::new(
            LiteralKind::Integer,
            span(source, range.clone()),
        )),
        span(source, range),
    )
}

fn bare_word(source: &SourceFile, range: std::ops::Range<usize>) -> Word {
    Word::new(
        vec![WordPart::new(
            WordPartKind::Bare,
            span(source, range.clone()),
        )],
        span(source, range),
    )
}

fn word_item(source: &SourceFile, range: std::ops::Range<usize>) -> CommandItem {
    CommandItem::new(
        CommandItemKind::Word(bare_word(source, range.clone())),
        span(source, range),
    )
}

fn redirect_item(
    source: &SourceFile,
    range: std::ops::Range<usize>,
    kind: RedirectionKind,
) -> CommandItem {
    let span = span(source, range);
    CommandItem::new(
        CommandItemKind::Redirection(Redirection::new(kind, span)),
        span,
    )
}

fn span(source: &SourceFile, range: std::ops::Range<usize>) -> flashshell_syntax::Span {
    source.span(range).unwrap()
}
