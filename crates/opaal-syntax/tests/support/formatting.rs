//! Shared test/fuzz oracle: resolve source-owned spellings, ignore only locations.

use opaal_syntax::*;

pub fn semantic_signature(source: &SourceFile, script: &Script) -> Vec<String> {
    let mut context = Context {
        source,
        values: Vec::new(),
    };
    script.statements().visit(&mut context);
    context.values
}

pub fn assert_roundtrip(source: &SourceFile) -> String {
    let ParseOutcome::Complete(script) = parse_opaal(source) else {
        panic!("roundtrip requires complete source");
    };
    let FormatOutcome::Complete(formatted) = format_source_opaal(source) else {
        panic!("complete source must format");
    };
    let rewritten = SourceFile::new(SourceId::new(1), source.name(), &formatted);
    let ParseOutcome::Complete(reparsed) = parse_opaal(&rewritten) else {
        panic!("formatted source must reparse: {formatted:?}");
    };
    assert_eq!(
        semantic_signature(source, &script),
        semantic_signature(&rewritten, &reparsed)
    );
    assert_eq!(
        format_source_opaal(&rewritten),
        FormatOutcome::Complete(formatted.clone())
    );
    let tokens = |file: &SourceFile| {
        lex_opaal(file)
            .iter()
            .filter(|token| !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline))
            .map(|token| (token.kind(), token.text(file).unwrap().to_owned()))
            .collect::<Vec<_>>()
    };
    assert_eq!(tokens(source), tokens(&rewritten));
    formatted
}

struct Context<'a> {
    source: &'a SourceFile,
    values: Vec<String>,
}

impl Context<'_> {
    fn tag(&mut self, value: impl ToString) {
        self.values.push(value.to_string());
    }
    fn text(&mut self, span: Span) {
        self.tag(self.source.slice(span).unwrap());
    }
}

trait Visit {
    fn visit(&self, context: &mut Context<'_>);
    fn visit_node(&self, _span: Span, context: &mut Context<'_>) {
        self.visit(context);
    }
}

impl<T: Visit> Visit for AstNode<T> {
    fn visit(&self, context: &mut Context<'_>) {
        self.kind().visit_node(self.span(), context);
    }
}
impl<T: Visit> Visit for [T] {
    fn visit(&self, context: &mut Context<'_>) {
        context.tag(self.len());
        for item in self {
            item.visit(context);
        }
    }
}
impl<T: Visit> Visit for Vec<T> {
    fn visit(&self, context: &mut Context<'_>) {
        self.as_slice().visit(context);
    }
}
impl<T: Visit> Visit for Option<T> {
    fn visit(&self, context: &mut Context<'_>) {
        context.tag(self.is_some());
        if let Some(value) = self {
            value.visit(context);
        }
    }
}
impl<T: Visit> Visit for Box<T> {
    fn visit(&self, context: &mut Context<'_>) {
        self.as_ref().visit(context);
    }
}
impl Visit for Span {
    fn visit(&self, _context: &mut Context<'_>) {}
}

// Destructure every field/variant: AST additions must update the oracle.
macro_rules! fields {
    ($type:ident { $($field:ident),* $(,)? }) => {
        impl Visit for $type {
            fn visit(&self, context: &mut Context<'_>) {
                let Self { $($field),* } = self;
                context.tag(stringify!($type));
                $($field.visit(context);)*
            }
        }
    };
}
macro_rules! variants {
    ($type:ident { $($variant:ident $(($value:ident))?),* $(,)? }) => {
        impl Visit for $type {
            fn visit(&self, context: &mut Context<'_>) {
                match self {
                    $(Self::$variant $(($value))? => {
                        context.tag(concat!(stringify!($type), "::", stringify!($variant)));
                        $($value.visit(context);)?
                    }),*
                }
            }
        }
    };
}

impl Visit for bool {
    fn visit(&self, context: &mut Context<'_>) {
        context.tag(self);
    }
}
impl Visit for Identifier {
    fn visit(&self, context: &mut Context<'_>) {
        context.text(self.span());
    }
}
impl Visit for IoNumber {
    fn visit(&self, context: &mut Context<'_>) {
        context.text(self.span());
    }
}
impl Visit for DocumentationBlock {
    fn visit(&self, context: &mut Context<'_>) {
        let Self { lines } = self;
        context.tag(lines.len());
        for line in lines {
            context.text(*line);
        }
    }
}
impl Visit for Literal {
    fn visit(&self, context: &mut Context<'_>) {
        match self.kind() {
            LiteralKind::Null => context.tag("Null"),
            LiteralKind::Boolean(value) => {
                context.tag("Boolean");
                value.visit(context);
            }
            LiteralKind::Integer => {
                context.tag("Integer");
                context.text(self.span());
            }
            LiteralKind::Float => {
                context.tag("Float");
                context.text(self.span());
            }
            LiteralKind::SingleQuoted => {
                context.tag("SingleQuoted");
                context.text(self.span());
            }
            LiteralKind::DoubleQuoted(parts) => {
                context.tag("DoubleQuoted");
                parts.visit(context);
            }
        }
    }
}
impl Visit for WordPartKind {
    fn visit(&self, _context: &mut Context<'_>) {
        unreachable!("word parts need their source span");
    }
    fn visit_node(&self, span: Span, context: &mut Context<'_>) {
        match self {
            Self::Bare
            | Self::BareEscape
            | Self::SingleQuoted
            | Self::DoubleText
            | Self::DoubleEscape
            | Self::EscapedBrace => {
                context.tag(format!("{self:?}"));
                context.text(span);
            }
            Self::DoubleQuoted(parts) => {
                context.tag("DoubleQuoted");
                parts.visit(context);
            }
            Self::Interpolation(expression) => {
                context.tag("Interpolation");
                expression.visit(context);
            }
        }
    }
}
impl Visit for ModuleImportSource {
    fn visit(&self, context: &mut Context<'_>) {
        match self {
            Self::Local { path } => {
                context.tag("Local");
                context.text(*path);
            }
            Self::Standard {
                namespace,
                module,
                span: _,
            } => {
                context.tag("Standard");
                namespace.visit(context);
                module.visit(context);
            }
            Self::Project {
                namespace,
                module,
                span: _,
            } => {
                context.tag("Project");
                namespace.visit(context);
                module.visit(context);
            }
        }
    }
}
impl Visit for RecordKey {
    fn visit(&self, context: &mut Context<'_>) {
        match self {
            Self::Identifier(name) => {
                context.tag("Identifier");
                name.visit(context);
            }
            Self::SingleQuoted(span) => {
                context.tag("SingleQuoted");
                context.text(*span);
            }
            Self::DoubleQuoted(part) => {
                context.tag("DoubleQuoted");
                part.visit(context);
            }
        }
    }
}
impl Visit for EnvironmentStatement {
    fn visit(&self, context: &mut Context<'_>) {
        match self {
            Self::Export { name, value } => {
                context.tag("Export");
                name.visit(context);
                value.visit(context);
            }
            Self::Unset { name } => {
                context.tag("Unset");
                name.visit(context);
            }
        }
    }
}
impl Visit for RedirectionKind {
    fn visit(&self, context: &mut Context<'_>) {
        match self {
            Self::Input {
                descriptor,
                operator_span: _,
                target,
            } => {
                context.tag("Input");
                descriptor.visit(context);
                target.visit(context);
            }
            Self::File(file) => {
                context.tag("File");
                file.visit(context);
            }
            Self::Duplicate {
                descriptor,
                operator_span: _,
                target,
            } => {
                context.tag("Duplicate");
                descriptor.visit(context);
                target.visit(context);
            }
            Self::Close {
                descriptor,
                operator_span: _,
                target_span: _,
            } => {
                context.tag("Close");
                descriptor.visit(context);
            }
        }
    }
}
impl Visit for ConditionalChain {
    fn visit(&self, context: &mut Context<'_>) {
        self.or_terms().visit(context);
        self.operators().visit(context);
    }
}
impl Visit for AndChain {
    fn visit(&self, context: &mut Context<'_>) {
        self.and_terms().visit(context);
        self.operators().visit(context);
    }
}
impl Visit for Pipeline {
    fn visit(&self, context: &mut Context<'_>) {
        self.stages().visit(context);
        self.operators().visit(context);
    }
}
impl Visit for Word {
    fn visit(&self, context: &mut Context<'_>) {
        self.parts().visit(context);
    }
}
impl Visit for CommandHead {
    fn visit(&self, context: &mut Context<'_>) {
        self.kind().visit(context);
        self.word().visit(context);
    }
}

variants!(StatementKind { ModuleImport(v), ModuleExport(v), NominalType(v), VariantType(v), Declaration(v), Assignment(v), Environment(v), Function(v), Action(v), Task(v), If(v), While(v), For(v), Match(v), Try(v), Throw(v), Control(v), Job(v) });
variants!(ExpressionKind { Literal(v), Name(v), Qualified(v), List(v), Record(v), NominalRecord(v), Closure(v), GroupedJob(v), Call(v), Index(v), Member(v), Unary(v), Binary(v) });
variants!(Pattern { Wildcard(v), Literal(v), Binding(v), List(v), NominalRecord(v), Variant(v) });
variants!(ElseBranch { Block(v), If(v) });
variants!(ControlTransfer { Break, Continue, Return(v) });
variants!(StaticEffectArgument { Literal(v), Qualified(v) });
variants!(StageKind { Command(v), Expression(v) });
variants!(CommandItemKind { Word(v), Spread(v), Closure(v), Redirection(v) });
variants!(TypeConstraint { Equal, Ordered });
variants!(UnaryOperator {
    Not,
    Positive,
    Negative
});
variants!(BinaryOperator {
    Multiply,
    Divide,
    Remainder,
    Add,
    Subtract,
    Range,
    RangeInclusive,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    Equal,
    NotEqual
});
variants!(ConditionalOperator { Or });
variants!(AndOperator { And });
variants!(PipeOperator {
    Stdout,
    StdoutAndStderr
});
variants!(CommandHeadKind {
    Bare,
    ForcedExternal
});
variants!(OutputMode { Truncate, Append });

fields!(ModuleAliasImport { source, alias });
fields!(ModuleExportStatement { names });
fields!(NominalTypeDeclaration {
    name,
    type_parameters,
    fields
});
fields!(VariantTypeDeclaration {
    name,
    type_parameters,
    variants
});
fields!(VariantDeclaration {
    name,
    payload,
    span
});
fields!(TypeParameter {
    name,
    constraints,
    span
});
fields!(NominalTypeField {
    name,
    value_type,
    span
});
fields!(Declaration {
    mutable,
    pattern,
    name,
    type_annotation,
    value
});
fields!(Assignment { target, value });
fields!(FunctionDefinition {
    documentation,
    name,
    type_parameters,
    parameters,
    return_type,
    body
});
fields!(ActionDefinition {
    documentation,
    name,
    type_parameters,
    parameters,
    return_type,
    effects,
    effects_span,
    body
});
fields!(EffectRequest {
    capability,
    arguments,
    span
});
fields!(CapabilityName {
    family,
    operation,
    span
});
fields!(TaskDefinition {
    documentation,
    name,
    action
});
fields!(Parameter {
    pattern,
    name,
    type_annotation,
    span
});
fields!(TypeReference {
    name,
    arguments,
    span
});
fields!(Block { statements, span });
fields!(IfStatement {
    condition,
    then_block,
    else_branch
});
fields!(WhileStatement { condition, body });
fields!(ForStatement {
    binding,
    iterable,
    body
});
fields!(MatchStatement { value, arms });
fields!(MatchArm {
    pattern,
    guard,
    body,
    span
});
fields!(TryStatement {
    try_block,
    catch_binding,
    catch_block
});
fields!(ListPattern {
    elements,
    rest,
    span
});
fields!(NominalRecordPattern { name, fields, span });
fields!(PatternField {
    name,
    pattern,
    span
});
fields!(VariantPattern {
    constructor,
    payload,
    span
});
fields!(QualifiedName { segments, span });
fields!(JobStatement {
    chain,
    background_span
});
fields!(NameReference { name, span });
fields!(RecordEntry { key, value, span });
fields!(NominalRecordExpression { name, fields });
fields!(NominalRecordFieldExpression { name, value, span });
fields!(Closure {
    parameters,
    result_type,
    body,
    span
});
fields!(CallExpression {
    callee,
    type_arguments,
    arguments
});
fields!(IndexExpression { target, index });
fields!(MemberExpression { target, member });
fields!(UnaryExpression { operator, operand });
fields!(BinaryExpression {
    left,
    operator,
    right
});
fields!(CommandStage { head, items });
fields!(FileRedirection {
    descriptor,
    mode,
    operator_span,
    target
});
