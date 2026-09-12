use crate::classification::classify_opaal_tokens;
use crate::lexer::lex_opaal_with_control;
use crate::{
    ActionDefinition, AndChain, AndOperator, Assignment, AstNode, BinaryExpression, BinaryOperator,
    Block, CallExpression, CapabilityName, Closure, CommandHead, CommandHeadKind, CommandItem,
    CommandItemKind, CommandStage, ConditionalChain, ConditionalOperator, ControlTransfer,
    Declaration, Delimiter, Diagnostic, DocumentationBlock, EffectRequest, ElseBranch,
    EnvironmentStatement, Expression, ExpressionKind, FileRedirection, ForStatement,
    FunctionDefinition, Identifier, IfStatement, IncompleteInput, IncompleteReason,
    IndexExpression, IoNumber, JobStatement, Keyword, ListPattern, Literal, LiteralKind, MatchArm,
    MatchStatement, MemberExpression, ModuleAliasImport, ModuleExportStatement, ModuleImportSource,
    NameReference, NominalRecordExpression, NominalRecordFieldExpression, NominalRecordPattern,
    NominalTypeDeclaration, NominalTypeField, NumberKind, Operator, OutputMode, Parameter, Pattern,
    PatternField, PipeOperator, Pipeline, QualifiedName, RecordEntry, RecordKey, Redirection,
    RedirectionKind, Script, Severity, SourceFile, Span, Stage, StageKind, Statement,
    StatementKind, StaticEffectArgument, SyntaxClassification, TaskDefinition, Token, TokenKind,
    TryStatement, TypeConstraint, TypeParameter, TypeReference, UnaryExpression, UnaryOperator,
    VariantDeclaration, VariantPattern, VariantTypeDeclaration, WhileStatement, Word, WordPart,
    WordPartKind,
};

/// The result of parsing one source file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseOutcome {
    Complete(Script),
    Incomplete(IncompleteInput),
    Invalid(Vec<Diagnostic>),
}

/// A controlled parse either completes with the ordinary result or is
/// cancelled without exposing partial syntax or diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlledParseOutcome {
    Parsed(ParseOutcome),
    Cancelled,
}

/// Parses one OPAAL source file through the shared lexer and structural classifier.
#[must_use]
pub fn parse_opaal(source: &SourceFile) -> ParseOutcome {
    match parse_opaal_with_control(source, &|| false) {
        ControlledParseOutcome::Parsed(outcome) => outcome,
        ControlledParseOutcome::Cancelled => {
            unreachable!("the default opaal parser never cancels")
        }
    }
}

/// Parses OPAAL with cooperative cancellation across lexing and parsing. The
/// predicate must remain `true` after it first requests cancellation.
#[must_use]
pub fn parse_opaal_with_control(
    source: &SourceFile,
    is_cancelled: &dyn Fn() -> bool,
) -> ControlledParseOutcome {
    let Some(tokens) = lex_opaal_with_control(source, is_cancelled) else {
        return ControlledParseOutcome::Cancelled;
    };
    let mut interpolation_spans = Vec::new();
    parse_tokens(source, tokens, is_cancelled, &mut interpolation_spans)
}

pub(crate) fn parse_opaal_with_interpolation_spans(
    source: &SourceFile,
) -> (ParseOutcome, Vec<Span>) {
    let is_cancelled = || false;
    let tokens = lex_opaal_with_control(source, &is_cancelled)
        .expect("the formatting parser never cancels during lexing");
    let mut interpolation_spans = Vec::new();
    let outcome = match parse_tokens(source, tokens, &is_cancelled, &mut interpolation_spans) {
        ControlledParseOutcome::Parsed(outcome) => outcome,
        ControlledParseOutcome::Cancelled => {
            unreachable!("the formatting parser never cancels")
        }
    };
    (outcome, interpolation_spans)
}

fn parse_tokens(
    source: &SourceFile,
    tokens: Vec<Token>,
    is_cancelled: &dyn Fn() -> bool,
    interpolation_spans: &mut Vec<Span>,
) -> ControlledParseOutcome {
    if is_cancelled() {
        return ControlledParseOutcome::Cancelled;
    }
    let classification = classify_opaal_tokens(source, &tokens)
        .expect("tokens produced from a source file have source-local spans");
    if is_cancelled() {
        return ControlledParseOutcome::Cancelled;
    }
    match classification {
        SyntaxClassification::Incomplete(incomplete) => {
            return ControlledParseOutcome::Parsed(ParseOutcome::Incomplete(incomplete));
        }
        SyntaxClassification::Invalid(diagnostic) => {
            return ControlledParseOutcome::Parsed(ParseOutcome::Invalid(vec![diagnostic]));
        }
        SyntaxClassification::Complete => {}
    }
    let parsed = Parser::new(source, tokens, is_cancelled, interpolation_spans).parse_script();
    if is_cancelled() {
        return ControlledParseOutcome::Cancelled;
    }
    match parsed {
        Ok(script) => ControlledParseOutcome::Parsed(ParseOutcome::Complete(script)),
        Err(ParseError::Incomplete(incomplete)) => {
            ControlledParseOutcome::Parsed(ParseOutcome::Incomplete(incomplete))
        }
        Err(ParseError::Invalid(diagnostic)) => {
            ControlledParseOutcome::Parsed(ParseOutcome::Invalid(vec![diagnostic]))
        }
        Err(ParseError::InvalidMany(diagnostics)) => {
            ControlledParseOutcome::Parsed(ParseOutcome::Invalid(diagnostics))
        }
        Err(ParseError::Cancelled) => ControlledParseOutcome::Cancelled,
    }
}

enum ParseError {
    Incomplete(IncompleteInput),
    Invalid(Diagnostic),
    InvalidMany(Vec<Diagnostic>),
    Cancelled,
}

type ParseResult<T> = Result<T, ParseError>;

struct Parser<'source, 'control, 'metadata> {
    source: &'source SourceFile,
    tokens: Vec<Token>,
    position: usize,
    continuation_depth: usize,
    control_condition_depth: usize,
    value_chain_depth: usize,
    diagnostics: Vec<Diagnostic>,
    is_cancelled: &'control dyn Fn() -> bool,
    interpolation_spans: &'metadata mut Vec<Span>,
}

impl<'source, 'control, 'metadata> Parser<'source, 'control, 'metadata> {
    fn new(
        source: &'source SourceFile,
        tokens: Vec<Token>,
        is_cancelled: &'control dyn Fn() -> bool,
        interpolation_spans: &'metadata mut Vec<Span>,
    ) -> Self {
        Self {
            source,
            tokens,
            position: 0,
            continuation_depth: 0,
            control_condition_depth: 0,
            value_chain_depth: 0,
            diagnostics: Vec::new(),
            is_cancelled,
            interpolation_spans,
        }
    }

    fn parse_script(mut self) -> ParseResult<Script> {
        self.check_cancelled()?;
        let statements = self.parse_statement_list(None)?;
        self.skip_separators();
        if !self.is_end() {
            let ParseError::Invalid(diagnostic) =
                self.invalid_here("unexpected syntax after statement")
            else {
                unreachable!("invalid syntax helper always returns one diagnostic")
            };
            self.diagnostics.push(diagnostic);
        }
        if !self.diagnostics.is_empty() {
            return Err(ParseError::InvalidMany(self.diagnostics));
        }
        Ok(Script::new(
            statements,
            self.source
                .span(0..self.source.len())
                .expect("the complete source range is valid"),
        ))
    }

    fn parse_statement_list(&mut self, closing: Option<Delimiter>) -> ParseResult<Vec<Statement>> {
        self.check_cancelled()?;
        let mut statements = Vec::new();
        let mut documentation = self.skip_separators_with_documentation();
        while !self.is_end() && !self.at_delimiter(closing) {
            self.check_cancelled()?;
            let statement = match self.parse_statement(closing.is_none(), documentation.take()) {
                Ok(statement) => statement,
                Err(ParseError::Invalid(diagnostic)) => {
                    self.diagnostics.push(diagnostic);
                    self.synchronize(closing);
                    documentation = self.skip_separators_with_documentation();
                    continue;
                }
                Err(error) => return Err(error),
            };
            let backgrounded = matches!(
                statement.kind(),
                StatementKind::Job(JobStatement {
                    background_span: Some(_),
                    ..
                })
            );
            statements.push(statement);
            self.skip_inline();

            if backgrounded {
                documentation = self.skip_separators_with_documentation();
                continue;
            }
            if self.at_delimiter(closing) || self.is_end() {
                break;
            }
            if self.current_kind() == Some(TokenKind::Operator(Operator::Background)) {
                let ParseError::Invalid(diagnostic) =
                    self.invalid_here("background marker can terminate only a job statement")
                else {
                    unreachable!("invalid syntax helper always returns one diagnostic")
                };
                self.diagnostics.push(diagnostic);
                self.synchronize(closing);
                documentation = self.skip_separators_with_documentation();
                continue;
            }
            if !self.at_statement_separator() {
                let ParseError::Invalid(diagnostic) =
                    self.invalid_here("expected a statement boundary")
                else {
                    unreachable!("invalid syntax helper always returns one diagnostic")
                };
                self.diagnostics.push(diagnostic);
                self.synchronize(closing);
                documentation = self.skip_separators_with_documentation();
                continue;
            }
            documentation = self.skip_separators_with_documentation();
        }
        Ok(statements)
    }

    fn parse_statement(
        &mut self,
        top_level: bool,
        documentation: Option<DocumentationBlock>,
    ) -> ParseResult<Statement> {
        self.check_cancelled()?;
        self.skip_inline();
        match self.current_kind() {
            Some(TokenKind::Keyword(Keyword::Import)) if top_level => self.parse_module_import(),
            Some(TokenKind::Keyword(Keyword::Import)) => {
                Err(self.invalid_here("imports are allowed only at module top level"))
            }
            Some(TokenKind::Keyword(Keyword::Let)) => self.parse_declaration(false),
            Some(TokenKind::Keyword(Keyword::Mut)) => self.parse_declaration(true),
            Some(TokenKind::Keyword(Keyword::Export)) => self.parse_export(top_level),
            Some(TokenKind::Keyword(Keyword::Unset)) => self.parse_unset(),
            Some(TokenKind::Keyword(Keyword::Def)) => self.parse_function(documentation),
            Some(TokenKind::Keyword(Keyword::Action)) => {
                self.parse_action(top_level, documentation)
            }
            Some(TokenKind::Keyword(Keyword::Task)) => self.parse_task(top_level, documentation),
            Some(TokenKind::Keyword(Keyword::Type)) => self.parse_nominal_type(top_level),
            Some(TokenKind::Keyword(Keyword::Enum)) => self.parse_variant_type(top_level),
            Some(TokenKind::Keyword(Keyword::If)) => {
                let node = self.parse_if_node()?;
                let span = node.span();
                Ok(Statement::new(StatementKind::If(node.into_kind()), span))
            }
            Some(TokenKind::Keyword(Keyword::While)) => self.parse_while(),
            Some(TokenKind::Keyword(Keyword::For)) => self.parse_for(),
            Some(TokenKind::Keyword(Keyword::Match)) => self.parse_match(),
            Some(TokenKind::Keyword(Keyword::Try)) => self.parse_try(),
            Some(TokenKind::Keyword(Keyword::Throw)) => self.parse_throw(),
            Some(TokenKind::Keyword(Keyword::Break)) => self.parse_control(ControlTransfer::Break),
            Some(TokenKind::Keyword(Keyword::Continue)) => {
                self.parse_control(ControlTransfer::Continue)
            }
            Some(TokenKind::Keyword(Keyword::Return)) => self.parse_return(),
            Some(TokenKind::Keyword(Keyword::Else)) => {
                Err(self.invalid_here("else requires a preceding if statement"))
            }
            Some(TokenKind::Keyword(Keyword::Catch)) => {
                Err(self.invalid_here("catch requires a preceding try statement"))
            }
            Some(TokenKind::Identifier) if self.name_is_assignment() => self.parse_assignment(),
            Some(_) => self.parse_job_statement(),
            None => Err(self.incomplete_here(IncompleteReason::Expression)),
        }
    }

    fn parse_module_import(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("import keyword is current").span();
        self.skip_inline();
        let source = match self.current_kind() {
            Some(TokenKind::SingleQuoted) => {
                let path = self.take().expect("local import path is current").span();
                if path.len() == 2 {
                    return Err(self.invalid_at(path, "module import path cannot be empty"));
                }
                ModuleImportSource::Local { path }
            }
            Some(TokenKind::Identifier) => {
                let namespace = self.parse_identifier()?;
                let namespace_text = self.source.slice(namespace.span()).ok();
                if !matches!(namespace_text, Some("std" | "project")) {
                    return Err(self.invalid_at(
                        namespace.span(),
                        "opaal module imports require a local path, `std::name`, or `project::name`",
                    ));
                }
                self.expect_operator(Operator::Colon, "qualified module requires `::`")?;
                self.expect_operator(Operator::Colon, "qualified module requires `::`")?;
                let module = self.parse_identifier()?;
                let span = self.span(namespace.span().start(), module.span().end());
                if namespace_text == Some("project") {
                    let module_text = self.source.slice(module.span()).ok();
                    if !matches!(
                        module_text,
                        Some("context" | "tools" | "endpoints" | "secrets")
                    ) {
                        return Err(self.invalid_at(
                            module.span(),
                            "project module must be `context`, `tools`, `endpoints`, or `secrets`",
                        ));
                    }
                    ModuleImportSource::Project {
                        namespace,
                        module,
                        span,
                    }
                } else {
                    ModuleImportSource::Standard {
                        namespace,
                        module,
                        span,
                    }
                }
            }
            _ => {
                return Err(self.invalid_here(
                    "opaal import requires a local path or qualified standard module",
                ));
            }
        };
        self.skip_inline();
        self.expect_contextual_identifier("as", "opaal module import requires `as alias`")?;
        self.skip_inline();
        let alias = self.parse_identifier()?;
        let span = self.span(start.start(), alias.span().end());
        Ok(Statement::new(
            StatementKind::ModuleImport(ModuleAliasImport { source, alias }),
            span,
        ))
    }

    fn parse_nominal_type(&mut self, top_level: bool) -> ParseResult<Statement> {
        if !top_level {
            return Err(self.invalid_here("nominal types are allowed only at module top level"));
        }
        let start = self.take().expect("type keyword is current").span();
        self.skip_inline();
        let name = self.parse_identifier()?;
        let type_parameters = self.parse_type_parameters()?;
        self.skip_inline();
        self.expect_operator(Operator::Assign, "type declaration requires `=`")?;
        self.skip_layout();
        self.expect_delimiter(
            Delimiter::LeftBrace,
            "nominal record declaration requires `{`",
        )?;
        self.skip_layout();
        let mut fields = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBrace)) {
            let field_name = self.parse_identifier()?;
            let field_start = field_name.span().start();
            self.skip_inline();
            self.expect_operator(Operator::Colon, "nominal field requires `:`")?;
            self.skip_inline();
            let value_type = self.parse_type_reference()?;
            let field_span = self.span(field_start, value_type.span.end());
            fields.push(NominalTypeField {
                name: field_name,
                value_type,
                span: field_span,
            });
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
        }
        let close = self.expect_delimiter(
            Delimiter::RightBrace,
            "nominal record declaration is not closed",
        )?;
        let span = self.span(start.start(), close.span().end());
        Ok(Statement::new(
            StatementKind::NominalType(NominalTypeDeclaration {
                name,
                type_parameters,
                fields,
            }),
            span,
        ))
    }

    fn parse_variant_type(&mut self, top_level: bool) -> ParseResult<Statement> {
        if !top_level {
            return Err(self.invalid_here("variant types are allowed only at module top level"));
        }
        let start = self.take().expect("enum keyword is current").span();
        self.skip_inline();
        let name = self.parse_identifier()?;
        let type_parameters = self.parse_type_parameters()?;
        self.skip_layout();
        self.expect_delimiter(Delimiter::LeftBrace, "variant declaration requires `{`")?;
        self.skip_layout();
        let mut variants = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBrace)) {
            let variant_name = self.parse_identifier()?;
            let variant_start = variant_name.span().start();
            self.skip_inline();
            let mut payload = Vec::new();
            let mut end = variant_name.span().end();
            if self.take_delimiter(Delimiter::LeftParenthesis).is_some() {
                self.skip_layout();
                while !self.at_delimiter(Some(Delimiter::RightParenthesis)) {
                    payload.push(self.parse_type_reference()?);
                    self.skip_layout();
                    if self.take_operator(Operator::Comma).is_none() {
                        break;
                    }
                    self.skip_layout();
                }
                let close = self.expect_delimiter(
                    Delimiter::RightParenthesis,
                    "variant payload is not closed",
                )?;
                end = close.span().end();
            }
            variants.push(VariantDeclaration {
                name: variant_name,
                payload,
                span: self.span(variant_start, end),
            });
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
        }
        if variants.is_empty() {
            return Err(self.invalid_here("variant declaration cannot be empty"));
        }
        let close =
            self.expect_delimiter(Delimiter::RightBrace, "variant declaration is not closed")?;
        Ok(Statement::new(
            StatementKind::VariantType(VariantTypeDeclaration {
                name,
                type_parameters,
                variants,
            }),
            self.span(start.start(), close.span().end()),
        ))
    }

    fn parse_declaration(&mut self, mutable: bool) -> ParseResult<Statement> {
        let start = self.take().expect("declaration starts on a keyword").span();
        self.skip_inline();
        let pattern = self.parse_pattern()?;
        let name = self.pattern_primary_binding(&pattern).ok_or_else(|| {
            self.invalid_at(
                self.pattern_span(&pattern),
                "declaration pattern must bind a name",
            )
        })?;
        self.skip_inline();
        let type_annotation = if self.take_operator(Operator::Colon).is_some() {
            self.skip_inline();
            Some(self.parse_type_reference()?)
        } else {
            None
        };
        self.skip_inline();
        self.expect_operator(Operator::Assign, "declaration requires `=`")?;
        self.skip_layout();
        let value = self.parse_expression()?;
        let span = self.span(start.start(), value.span().end());
        Ok(Statement::new(
            StatementKind::Declaration(Declaration {
                mutable,
                pattern,
                name,
                type_annotation,
                value,
            }),
            span,
        ))
    }

    fn parse_assignment(&mut self) -> ParseResult<Statement> {
        let target = self.parse_name_reference()?;
        let start = target.span;
        self.skip_inline();
        self.expect_operator(Operator::Assign, "assignment requires `=`")?;
        self.skip_layout();
        let value = self.parse_expression()?;
        let span = self.span(start.start(), value.span().end());
        Ok(Statement::new(
            StatementKind::Assignment(Assignment { target, value }),
            span,
        ))
    }

    fn parse_export(&mut self, top_level: bool) -> ParseResult<Statement> {
        let start = self.take().expect("export keyword is current").span();
        self.skip_inline();
        if self.at_delimiter(Some(Delimiter::LeftBrace)) {
            if !top_level {
                return Err(
                    self.invalid_here("module exports are allowed only at module top level")
                );
            }
            let (names, close) =
                self.parse_module_name_list("module export list cannot be empty")?;
            let span = self.span(start.start(), close.span().end());
            return Ok(Statement::new(
                StatementKind::ModuleExport(ModuleExportStatement { names }),
                span,
            ));
        }
        let name = self.parse_identifier()?;
        self.skip_inline();
        self.expect_operator(Operator::Assign, "export requires `=`")?;
        self.skip_layout();
        let value = self.parse_expression()?;
        let span = self.span(start.start(), value.span().end());
        Ok(Statement::new(
            StatementKind::Environment(EnvironmentStatement::Export { name, value }),
            span,
        ))
    }

    fn parse_module_name_list(
        &mut self,
        empty_message: &str,
    ) -> ParseResult<(Vec<Identifier>, Token)> {
        self.expect_delimiter(Delimiter::LeftBrace, "module name list requires `{`")?;
        self.skip_layout();
        if self.at_delimiter(Some(Delimiter::RightBrace)) {
            return Err(self.invalid_here(empty_message));
        }
        let mut names = Vec::new();
        loop {
            names.push(self.parse_identifier()?);
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
            if self.at_delimiter(Some(Delimiter::RightBrace)) {
                break;
            }
        }
        let close =
            self.expect_delimiter(Delimiter::RightBrace, "module name list is not closed")?;
        Ok((names, close))
    }

    fn parse_unset(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("unset keyword is current").span();
        self.skip_inline();
        let name = self.parse_identifier()?;
        let span = self.span(start.start(), name.span().end());
        Ok(Statement::new(
            StatementKind::Environment(EnvironmentStatement::Unset { name }),
            span,
        ))
    }

    fn parse_function(
        &mut self,
        documentation: Option<DocumentationBlock>,
    ) -> ParseResult<Statement> {
        let start = self.take().expect("def keyword is current").span();
        self.skip_inline();
        let name = self.parse_identifier()?;
        let type_parameters = self.parse_type_parameters()?;
        self.skip_inline();
        self.expect_delimiter(Delimiter::LeftParenthesis, "function requires parameters")?;
        let parameters = self.parse_parameters(Delimiter::RightParenthesis)?;
        self.expect_delimiter(
            Delimiter::RightParenthesis,
            "function parameters are not closed",
        )?;
        self.skip_inline();
        let return_type = if self.take_operator(Operator::Arrow).is_some() {
            self.skip_layout();
            Some(self.parse_type_reference()?)
        } else {
            None
        };
        self.skip_inline();
        let body = self.parse_block()?;
        let span = self.span(start.start(), body.span.end());
        Ok(Statement::new(
            StatementKind::Function(FunctionDefinition {
                documentation,
                name,
                type_parameters,
                parameters,
                return_type,
                body,
            }),
            span,
        ))
    }

    fn parse_action(
        &mut self,
        top_level: bool,
        documentation: Option<DocumentationBlock>,
    ) -> ParseResult<Statement> {
        if !top_level {
            return Err(self.invalid_here("actions are allowed only at module top level"));
        }
        let start = self.take().expect("action keyword is current").span();
        self.skip_inline();
        let name = self.parse_identifier()?;
        let type_parameters = self.parse_type_parameters()?;
        self.skip_inline();
        self.expect_delimiter(Delimiter::LeftParenthesis, "action requires parameters")?;
        let parameters = self.parse_parameters(Delimiter::RightParenthesis)?;
        self.expect_delimiter(
            Delimiter::RightParenthesis,
            "action parameters are not closed",
        )?;
        self.skip_inline();
        self.expect_operator(Operator::Arrow, "action requires an explicit result type")?;
        self.skip_layout();
        let return_type = self.parse_type_reference()?;
        self.skip_layout();
        let effects_keyword =
            self.expect_contextual_identifier("effects", "action requires `effects {}`")?;
        self.skip_inline();
        let (effects, effects_block_span) = self.parse_effect_block()?;
        let effects_span = self.span(effects_keyword.span().start(), effects_block_span.end());
        self.skip_layout();
        let body = self.parse_block()?;
        let span = self.span(start.start(), body.span.end());
        Ok(Statement::new(
            StatementKind::Action(ActionDefinition {
                documentation,
                name,
                type_parameters,
                parameters,
                return_type,
                effects,
                effects_span,
                body,
            }),
            span,
        ))
    }

    fn parse_effect_block(&mut self) -> ParseResult<(Vec<EffectRequest>, Span)> {
        let left = self.expect_delimiter(Delimiter::LeftBrace, "effects block requires `{`")?;
        self.skip_layout();
        let mut requests = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBrace)) {
            let family = self.parse_identifier()?;
            let start = family.span().start();
            self.expect_operator(Operator::Dot, "capability requires `family.operation`")?;
            let operation = self.parse_identifier()?;
            let capability_span = self.span(start, operation.span().end());
            self.skip_inline();
            let mut arguments = Vec::new();
            if self.take_delimiter(Delimiter::LeftParenthesis).is_some() {
                self.skip_layout();
                while !self.at_delimiter(Some(Delimiter::RightParenthesis)) {
                    let argument = match self.current_kind() {
                        Some(TokenKind::Identifier) => {
                            StaticEffectArgument::Qualified(self.parse_qualified_name()?)
                        }
                        Some(TokenKind::Keyword(
                            Keyword::Null | Keyword::True | Keyword::False,
                        ))
                        | Some(TokenKind::Number(_))
                        | Some(TokenKind::SingleQuoted)
                        | Some(TokenKind::DoubleQuoteStart) => {
                            StaticEffectArgument::Literal(self.parse_literal()?)
                        }
                        _ => {
                            return Err(self.invalid_here(
                                "effect arguments must be literals or qualified project identities",
                            ));
                        }
                    };
                    arguments.push(argument);
                    self.skip_layout();
                    if self.take_operator(Operator::Comma).is_none() {
                        break;
                    }
                    self.skip_layout();
                }
                self.expect_delimiter(
                    Delimiter::RightParenthesis,
                    "effect arguments are not closed",
                )?;
                self.skip_inline();
            }
            let semicolon =
                self.expect_operator(Operator::Semicolon, "effect request requires `;`")?;
            requests.push(EffectRequest {
                capability: CapabilityName {
                    family,
                    operation,
                    span: capability_span,
                },
                arguments,
                span: self.span(start, semicolon.span().end()),
            });
            self.skip_layout();
        }
        let right = self.expect_delimiter(Delimiter::RightBrace, "effects block is not closed")?;
        Ok((requests, self.span(left.span().start(), right.span().end())))
    }

    fn parse_task(
        &mut self,
        top_level: bool,
        documentation: Option<DocumentationBlock>,
    ) -> ParseResult<Statement> {
        if !top_level {
            return Err(self.invalid_here("tasks are allowed only at module top level"));
        }
        let start = self.take().expect("task keyword is current").span();
        self.skip_inline();
        let name = self.parse_identifier()?;
        self.skip_inline();
        self.expect_operator(Operator::Assign, "task requires `=`")?;
        self.skip_layout();
        let action = self.parse_qualified_name()?;
        let span = self.span(start.start(), action.span.end());
        Ok(Statement::new(
            StatementKind::Task(TaskDefinition {
                documentation,
                name,
                action,
            }),
            span,
        ))
    }

    fn parse_try(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("try keyword is current").span();
        self.skip_inline();
        let try_block = self.parse_block()?;
        self.skip_inline();
        self.expect_keyword(Keyword::Catch, "try statement requires `catch`")?;
        self.skip_inline();
        let catch_binding = self.parse_identifier()?;
        self.skip_inline();
        let catch_block = self.parse_block()?;
        let span = self.span(start.start(), catch_block.span.end());
        Ok(Statement::new(
            StatementKind::Try(TryStatement {
                try_block,
                catch_binding,
                catch_block,
            }),
            span,
        ))
    }

    fn parse_throw(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("throw keyword is current").span();
        self.skip_layout();
        let expression = self.parse_expression()?;
        let span = self.span(start.start(), expression.span().end());
        Ok(Statement::new(StatementKind::Throw(expression), span))
    }

    fn parse_if_node(&mut self) -> ParseResult<AstNode<IfStatement>> {
        let start = self.take().expect("if keyword is current").span();
        self.skip_inline();
        let condition = self.parse_control_condition()?;
        self.skip_inline();
        let then_block = self.parse_block()?;
        let mut end = then_block.span.end();
        self.skip_inline();
        let else_branch = if self.take_keyword(Keyword::Else).is_some() {
            self.skip_inline();
            if self.at_keyword(Keyword::If) {
                let nested = self.parse_if_node()?;
                end = nested.span().end();
                Some(ElseBranch::If(Box::new(nested)))
            } else {
                let block = self.parse_block()?;
                end = block.span.end();
                Some(ElseBranch::Block(block))
            }
        } else {
            None
        };
        Ok(AstNode::new(
            IfStatement {
                condition,
                then_block,
                else_branch,
            },
            self.span(start.start(), end),
        ))
    }

    fn parse_while(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("while keyword is current").span();
        self.skip_inline();
        let condition = self.parse_control_condition()?;
        self.skip_inline();
        let body = self.parse_block()?;
        let span = self.span(start.start(), body.span.end());
        Ok(Statement::new(
            StatementKind::While(WhileStatement { condition, body }),
            span,
        ))
    }

    fn parse_for(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("for keyword is current").span();
        self.skip_inline();
        let binding = self.parse_identifier()?;
        self.skip_inline();
        self.expect_keyword(Keyword::In, "for binding requires `in`")?;
        self.skip_layout();
        let iterable = self.parse_expression()?;
        self.skip_inline();
        let body = self.parse_block()?;
        let span = self.span(start.start(), body.span.end());
        Ok(Statement::new(
            StatementKind::For(ForStatement {
                binding,
                iterable,
                body,
            }),
            span,
        ))
    }

    fn parse_match(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("match keyword is current").span();
        self.skip_inline();
        let value = self.parse_expression()?;
        self.skip_inline();
        self.expect_delimiter(Delimiter::LeftBrace, "match requires an arm block")?;
        self.skip_separators();
        let mut arms = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBrace)) {
            match self.parse_match_arm() {
                Ok(arm) => arms.push(arm),
                Err(ParseError::Invalid(diagnostic)) => {
                    self.diagnostics.push(diagnostic);
                    self.synchronize(Some(Delimiter::RightBrace));
                    self.skip_separators();
                    continue;
                }
                Err(error) => return Err(error),
            }
            self.skip_inline();
            if self.at_delimiter(Some(Delimiter::RightBrace)) {
                break;
            }
            if !self.at_statement_separator() {
                let ParseError::Invalid(diagnostic) =
                    self.invalid_here("match arms require a separator")
                else {
                    unreachable!("invalid syntax helper always returns one diagnostic")
                };
                self.diagnostics.push(diagnostic);
                self.synchronize(Some(Delimiter::RightBrace));
                self.skip_separators();
                continue;
            }
            self.skip_separators();
        }
        let close = self.expect_delimiter(Delimiter::RightBrace, "match block is not closed")?;
        let span = self.span(start.start(), close.span().end());
        Ok(Statement::new(
            StatementKind::Match(MatchStatement { value, arms }),
            span,
        ))
    }

    fn parse_match_arm(&mut self) -> ParseResult<MatchArm> {
        let start = self.current_span()?;
        let pattern = self.parse_pattern()?;
        self.skip_inline();
        let guard = if self.take_keyword(Keyword::If).is_some() {
            self.skip_inline();
            Some(self.parse_expression()?)
        } else {
            None
        };
        self.skip_inline();
        self.expect_operator(Operator::MatchArrow, "match arm requires `=>`")?;
        self.skip_layout();
        let body = self.parse_block()?;
        Ok(MatchArm {
            pattern,
            guard,
            span: self.span(start.start(), body.span.end()),
            body,
        })
    }

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        match self.current_kind() {
            Some(TokenKind::Identifier) => {
                let name = self.parse_qualified_name()?;
                self.skip_inline();
                if name.segments.len() == 1
                    && self.source.slice(name.segments[0].span()).ok() == Some("_")
                {
                    return Ok(Pattern::Wildcard(name.span));
                }
                if self.at_delimiter(Some(Delimiter::LeftBrace)) {
                    let start = name.span.start();
                    self.take();
                    self.skip_layout();
                    let mut fields = Vec::new();
                    while !self.at_delimiter(Some(Delimiter::RightBrace)) {
                        let field = self.parse_identifier()?;
                        let field_start = field.span().start();
                        self.skip_inline();
                        self.expect_operator(Operator::Colon, "record pattern field requires `:`")?;
                        self.skip_layout();
                        let pattern = self.parse_pattern()?;
                        let span = self.span(field_start, self.pattern_span(&pattern).end());
                        fields.push(PatternField {
                            name: field,
                            pattern,
                            span,
                        });
                        self.skip_layout();
                        if self.take_operator(Operator::Comma).is_none() {
                            break;
                        }
                        self.skip_layout();
                    }
                    let close = self
                        .expect_delimiter(Delimiter::RightBrace, "record pattern is not closed")?;
                    return Ok(Pattern::NominalRecord(NominalRecordPattern {
                        name,
                        fields,
                        span: self.span(start, close.span().end()),
                    }));
                }
                if self.at_delimiter(Some(Delimiter::LeftParenthesis)) {
                    let start = name.span.start();
                    self.take();
                    self.skip_layout();
                    let mut payload = Vec::new();
                    while !self.at_delimiter(Some(Delimiter::RightParenthesis)) {
                        payload.push(self.parse_pattern()?);
                        self.skip_layout();
                        if self.take_operator(Operator::Comma).is_none() {
                            break;
                        }
                        self.skip_layout();
                    }
                    let close = self.expect_delimiter(
                        Delimiter::RightParenthesis,
                        "variant pattern is not closed",
                    )?;
                    return Ok(Pattern::Variant(VariantPattern {
                        constructor: name,
                        payload,
                        span: self.span(start, close.span().end()),
                    }));
                }
                if name.segments.len() > 1 {
                    return Ok(Pattern::Variant(VariantPattern {
                        span: name.span,
                        constructor: name,
                        payload: Vec::new(),
                    }));
                }
                Ok(Pattern::Binding(name.segments[0]))
            }
            Some(TokenKind::Delimiter(Delimiter::LeftBracket)) => self.parse_list_pattern(),
            Some(TokenKind::Keyword(Keyword::Null | Keyword::True | Keyword::False))
            | Some(TokenKind::Number(_))
            | Some(TokenKind::SingleQuoted)
            | Some(TokenKind::DoubleQuoteStart) => Ok(Pattern::Literal(self.parse_literal()?)),
            Some(_) => Err(self.invalid_here("expected a match pattern")),
            None => Err(self.incomplete_here(IncompleteReason::Expression)),
        }
    }

    fn parse_list_pattern(&mut self) -> ParseResult<Pattern> {
        let open = self.take().expect("list-pattern opener is current");
        self.skip_layout();
        let mut elements = Vec::new();
        let mut rest = None;
        while !self.at_delimiter(Some(Delimiter::RightBracket)) {
            if self.take_operator(Operator::Spread).is_some() {
                if rest.is_some() {
                    return Err(self.invalid_here("list pattern has more than one rest binding"));
                }
                rest = Some(self.parse_identifier()?);
                self.skip_layout();
                if !self.at_delimiter(Some(Delimiter::RightBracket)) {
                    return Err(self.invalid_here("list rest binding must be last"));
                }
                break;
            }
            elements.push(self.parse_pattern()?);
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
        }
        let close = self.expect_delimiter(Delimiter::RightBracket, "list pattern is not closed")?;
        Ok(Pattern::List(ListPattern {
            elements,
            rest,
            span: self.span(open.span().start(), close.span().end()),
        }))
    }

    fn parse_control(&mut self, transfer: ControlTransfer) -> ParseResult<Statement> {
        let span = self.take().expect("control keyword is current").span();
        Ok(Statement::new(StatementKind::Control(transfer), span))
    }

    fn parse_return(&mut self) -> ParseResult<Statement> {
        let start = self.take().expect("return keyword is current").span();
        self.skip_inline();
        let value = if self.at_expression_end() {
            None
        } else {
            Some(self.parse_expression()?)
        };
        let end = value
            .as_ref()
            .map_or(start.end(), |expression| expression.span().end());
        Ok(Statement::new(
            StatementKind::Control(ControlTransfer::Return(value)),
            self.span(start.start(), end),
        ))
    }

    fn parse_job_statement(&mut self) -> ParseResult<Statement> {
        let chain = self.parse_conditional_chain()?;
        let start = chain.span();
        self.skip_inline();
        let background_span = self
            .take_operator(Operator::Background)
            .map(|token| token.span());
        let end = background_span.map_or(chain.span().end(), Span::end);
        Ok(Statement::new(
            StatementKind::Job(JobStatement {
                chain,
                background_span,
            }),
            self.span(start.start(), end),
        ))
    }

    fn parse_block(&mut self) -> ParseResult<Block> {
        let open = self.expect_delimiter(Delimiter::LeftBrace, "expected a block")?;
        let statements = self.parse_statement_list(Some(Delimiter::RightBrace))?;
        let close = self.expect_delimiter(Delimiter::RightBrace, "block is not closed")?;
        Ok(Block {
            statements,
            span: self.span(open.span().start(), close.span().end()),
        })
    }

    fn parse_parameters(&mut self, closing: Delimiter) -> ParseResult<Vec<Parameter>> {
        self.skip_layout();
        let mut parameters = Vec::new();
        while !self.at_delimiter(Some(closing)) {
            let start = self.current_span()?;
            let pattern = self.parse_pattern()?;
            let name = self.pattern_primary_binding(&pattern).ok_or_else(|| {
                self.invalid_at(
                    self.pattern_span(&pattern),
                    "parameter pattern must bind a name",
                )
            })?;
            self.skip_inline();
            let type_annotation = if self.take_operator(Operator::Colon).is_some() {
                self.skip_layout();
                Some(self.parse_type_reference()?)
            } else {
                None
            };
            let end = type_annotation
                .as_ref()
                .map_or(name.span().end(), |reference| reference.span.end());
            parameters.push(Parameter {
                pattern,
                name,
                type_annotation,
                span: self.span(start.start(), end),
            });
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
            if self.at_delimiter(Some(closing)) {
                break;
            }
        }
        Ok(parameters)
    }

    fn parse_type_parameters(&mut self) -> ParseResult<Vec<TypeParameter>> {
        self.skip_inline();
        if self.take_delimiter(Delimiter::LeftBracket).is_none() {
            return Ok(Vec::new());
        }
        self.skip_layout();
        if self.at_delimiter(Some(Delimiter::RightBracket)) {
            return Err(self.invalid_here("generic parameter list cannot be empty"));
        }
        let mut parameters = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBracket)) {
            let name = self.parse_identifier()?;
            let start = name.span().start();
            self.skip_inline();
            let mut constraints = Vec::new();
            let mut end = name.span().end();
            if self.take_operator(Operator::Colon).is_some() {
                loop {
                    self.skip_inline();
                    let constraint = self.parse_identifier()?;
                    end = constraint.span().end();
                    let value = match self.source.slice(constraint.span()).ok() {
                        Some("Equal") => TypeConstraint::Equal,
                        Some("Ordered") => TypeConstraint::Ordered,
                        _ => {
                            return Err(self.invalid_at(
                                constraint.span(),
                                "generic constraints are `Equal` or `Ordered`",
                            ));
                        }
                    };
                    if constraints.contains(&value) {
                        return Err(self.invalid_at(
                            constraint.span(),
                            "generic constraint cannot be repeated",
                        ));
                    }
                    constraints.push(value);
                    self.skip_inline();
                    if self.take_operator(Operator::Plus).is_none() {
                        break;
                    }
                }
            }
            parameters.push(TypeParameter {
                name,
                constraints,
                span: self.span(start, end),
            });
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
        }
        self.expect_delimiter(
            Delimiter::RightBracket,
            "generic parameter list is not closed",
        )?;
        Ok(parameters)
    }

    fn parse_type_reference(&mut self) -> ParseResult<TypeReference> {
        if self.is_end() {
            return Err(self.incomplete_here(IncompleteReason::TypeReference));
        }
        let qualified = self.parse_qualified_name()?;
        let name = *qualified
            .segments
            .last()
            .expect("a qualified type name has at least one segment");
        let start = qualified.span;
        self.skip_inline();
        let mut arguments = Vec::new();
        let mut end = name.span().end();
        if self.take_delimiter(Delimiter::LeftBracket).is_some() {
            self.skip_layout();
            loop {
                arguments.push(self.parse_type_reference()?);
                self.skip_layout();
                if self.take_operator(Operator::Comma).is_none() {
                    break;
                }
                self.skip_layout();
                if self.at_delimiter(Some(Delimiter::RightBracket)) {
                    break;
                }
            }
            let close =
                self.expect_delimiter(Delimiter::RightBracket, "type arguments are not closed")?;
            end = close.span().end();
        }
        Ok(TypeReference {
            name,
            arguments,
            span: self.span(start.start(), end),
        })
    }

    fn parse_qualified_name(&mut self) -> ParseResult<QualifiedName> {
        let first = self.parse_identifier()?;
        let start = first.span().start();
        let mut end = first.span().end();
        let mut segments = vec![first];
        loop {
            let checkpoint = self.position;
            self.skip_inline();
            if self.take_operator(Operator::Colon).is_none()
                || self.take_operator(Operator::Colon).is_none()
            {
                self.position = checkpoint;
                break;
            }
            self.skip_inline();
            let segment = self.parse_identifier()?;
            end = segment.span().end();
            segments.push(segment);
        }
        Ok(QualifiedName {
            segments,
            span: self.span(start, end),
        })
    }

    fn pattern_span(&self, pattern: &Pattern) -> Span {
        match pattern {
            Pattern::Wildcard(span) => *span,
            Pattern::Literal(literal) => literal.span(),
            Pattern::Binding(identifier) => identifier.span(),
            Pattern::List(pattern) => pattern.span,
            Pattern::NominalRecord(pattern) => pattern.span,
            Pattern::Variant(pattern) => pattern.span,
        }
    }

    fn pattern_primary_binding(&self, pattern: &Pattern) -> Option<Identifier> {
        match pattern {
            Pattern::Binding(identifier) => Some(*identifier),
            Pattern::List(pattern) => pattern
                .elements
                .iter()
                .find_map(|pattern| self.pattern_primary_binding(pattern))
                .or(pattern.rest),
            Pattern::NominalRecord(pattern) => pattern
                .fields
                .iter()
                .find_map(|field| self.pattern_primary_binding(&field.pattern)),
            Pattern::Variant(pattern) => pattern
                .payload
                .iter()
                .find_map(|pattern| self.pattern_primary_binding(pattern)),
            Pattern::Wildcard(_) | Pattern::Literal(_) => None,
        }
    }

    fn parse_conditional_chain(&mut self) -> ParseResult<ConditionalChain> {
        self.check_cancelled()?;
        let first = self.parse_and_chain()?;
        let start = first.span();
        let mut end = first.span().end();
        let mut terms = vec![first];
        let mut operators = Vec::new();
        loop {
            self.check_cancelled()?;
            self.skip_inline();
            let Some(operator) = self.take_operator(Operator::Or) else {
                break;
            };
            operators.push(AstNode::new(ConditionalOperator::Or, operator.span()));
            self.skip_layout();
            if self.is_end() {
                return Err(
                    self.incomplete_at(IncompleteReason::ConditionalOperand, operator.span())
                );
            }
            let term = self.parse_and_chain()?;
            end = term.span().end();
            terms.push(term);
        }
        Ok(ConditionalChain::new(
            terms,
            operators,
            self.span(start.start(), end),
        ))
    }

    fn parse_control_condition(&mut self) -> ParseResult<ConditionalChain> {
        self.control_condition_depth += 1;
        let condition = self.parse_conditional_chain();
        self.control_condition_depth -= 1;
        condition
    }

    fn parse_value_chain(&mut self) -> ParseResult<ConditionalChain> {
        self.value_chain_depth += 1;
        let chain = self.parse_conditional_chain();
        self.value_chain_depth -= 1;
        chain
    }

    fn parse_and_chain(&mut self) -> ParseResult<AndChain> {
        self.check_cancelled()?;
        let first = self.parse_pipeline()?;
        let start = first.span();
        let mut end = first.span().end();
        let mut terms = vec![first];
        let mut operators = Vec::new();
        loop {
            self.check_cancelled()?;
            self.skip_inline();
            let Some(operator) = self.take_operator(Operator::And) else {
                break;
            };
            operators.push(AstNode::new(AndOperator::And, operator.span()));
            self.skip_layout();
            if self.is_end() {
                return Err(
                    self.incomplete_at(IncompleteReason::ConditionalOperand, operator.span())
                );
            }
            let term = self.parse_pipeline()?;
            end = term.span().end();
            terms.push(term);
        }
        Ok(AndChain::new(
            terms,
            operators,
            self.span(start.start(), end),
        ))
    }

    fn parse_pipeline(&mut self) -> ParseResult<Pipeline> {
        self.check_cancelled()?;
        let first = self.parse_stage()?;
        let start = first.span();
        let mut end = first.span().end();
        let mut stages = vec![first];
        let mut operators = Vec::new();
        loop {
            self.check_cancelled()?;
            self.skip_inline();
            let operator = match self.current_kind() {
                Some(TokenKind::Operator(Operator::Pipe)) => PipeOperator::Stdout,
                Some(TokenKind::Operator(Operator::PipeBoth)) => PipeOperator::StdoutAndStderr,
                _ => break,
            };
            let token = self.take().expect("pipeline operator is current");
            operators.push(AstNode::new(operator, token.span()));
            self.skip_layout();
            if self.is_end() {
                return Err(self.incomplete_at(IncompleteReason::PipelineStage, token.span()));
            }
            let stage = self.parse_stage()?;
            end = stage.span().end();
            stages.push(stage);
        }
        Ok(Pipeline::new(
            stages,
            operators,
            self.span(start.start(), end),
        ))
    }

    fn parse_stage(&mut self) -> ParseResult<Stage> {
        self.skip_inline();
        if matches!(
            self.current_kind(),
            Some(TokenKind::Operator(Operator::Pipe | Operator::PipeBoth))
        ) {
            return Err(self.invalid_here("pipeline operator cannot begin a stage"));
        }
        if let Some(TokenKind::Keyword(keyword)) = self.current_kind()
            && !matches!(keyword, Keyword::Null | Keyword::True | Keyword::False)
        {
            return Err(
                self.invalid_here("reserved words cannot start a command stage without `^`")
            );
        }
        if self.current_kind() == Some(TokenKind::Operator(Operator::Caret))
            && !self.starts_forced_command()
        {
            return if self.tokens.get(self.position + 1).is_none() {
                Err(self.incomplete_here(IncompleteReason::Expression))
            } else {
                Err(self.invalid_here("`^` must be adjacent to a command name"))
            };
        }
        if self.starts_forced_command() || !self.starts_expression_stage() {
            self.parse_command_stage()
        } else {
            let expression = self.parse_expression()?;
            let span = expression.span();
            Ok(Stage::new(StageKind::Expression(expression), span))
        }
    }

    fn parse_command_stage(&mut self) -> ParseResult<Stage> {
        let forced = self.starts_forced_command();
        let marker = if forced {
            Some(self.take().expect("forced command marker is current"))
        } else {
            None
        };
        let word = self.parse_word()?;
        if let Some(marker) = marker
            && marker.span().end() != word.span().start()
        {
            return Err(self.invalid_at(marker.span(), "`^` must be adjacent to a command name"));
        }
        let head_start = marker.map_or(word.span().start(), |token| token.span().start());
        let head = CommandHead::new(
            if forced {
                CommandHeadKind::ForcedExternal
            } else {
                CommandHeadKind::Bare
            },
            word,
            self.span(head_start, self.previous_end()),
        );
        let start = head.span();
        let mut end = head.span().end();
        let mut items = Vec::new();
        loop {
            self.skip_inline();
            if self.at_command_end() {
                break;
            }
            let item = self.parse_command_item()?;
            end = item.span().end();
            items.push(item);
        }
        Ok(Stage::new(
            StageKind::Command(CommandStage { head, items }),
            self.span(start.start(), end),
        ))
    }

    fn parse_command_item(&mut self) -> ParseResult<CommandItem> {
        if self.starts_redirection() {
            return self.parse_redirection_item();
        }
        if self.current_kind() == Some(TokenKind::Operator(Operator::Spread)) {
            return self.parse_spread_item();
        }
        if self.starts_closure() {
            let closure = self.parse_closure()?;
            let span = closure.span;
            return Ok(CommandItem::new(CommandItemKind::Closure(closure), span));
        }
        let word = self.parse_word()?;
        let span = word.span();
        Ok(CommandItem::new(CommandItemKind::Word(word), span))
    }

    fn parse_spread_item(&mut self) -> ParseResult<CommandItem> {
        let spread = self.take().expect("spread operator is current");
        let Some(open) = self.current().copied() else {
            return Err(self.incomplete_at(IncompleteReason::Expression, spread.span()));
        };
        if open.kind() != TokenKind::Delimiter(Delimiter::LeftBrace)
            || spread.span().end() != open.span().start()
        {
            return Err(
                self.invalid_at(spread.span(), "spread requires an adjacent `{expression}`")
            );
        }
        let interpolation = self.parse_interpolation()?;
        let span = self.span(spread.span().start(), interpolation.span().end());
        let WordPartKind::Interpolation(expression) = interpolation.into_kind() else {
            unreachable!("interpolation parser returns its matching part")
        };
        Ok(CommandItem::new(CommandItemKind::Spread(*expression), span))
    }

    fn parse_redirection_item(&mut self) -> ParseResult<CommandItem> {
        let start = self.current_span()?;
        let descriptor = if self.io_number_before_redirection() {
            Some(IoNumber::new(
                self.take().expect("descriptor is current").span(),
            ))
        } else {
            None
        };
        let operator = self
            .take()
            .ok_or_else(|| self.incomplete_here(IncompleteReason::RedirectionTarget))?;
        let operator_span = operator.span();
        let kind = match operator.kind() {
            TokenKind::Operator(Operator::Less) => {
                self.skip_inline();
                let target = self.parse_redirection_target(operator_span)?;
                RedirectionKind::Input {
                    descriptor,
                    operator_span,
                    target,
                }
            }
            TokenKind::Operator(Operator::Greater | Operator::Append) => {
                let mode = if operator.kind() == TokenKind::Operator(Operator::Append) {
                    OutputMode::Append
                } else {
                    OutputMode::Truncate
                };
                self.skip_inline();
                let target = self.parse_redirection_target(operator_span)?;
                RedirectionKind::File(FileRedirection {
                    descriptor,
                    mode,
                    operator_span,
                    target,
                })
            }
            TokenKind::Operator(Operator::Duplicate) => {
                let Some(descriptor) = descriptor else {
                    return Err(self.invalid_at(
                        operator_span,
                        "descriptor duplication requires a source descriptor",
                    ));
                };
                self.skip_inline();
                match self.current_kind() {
                    Some(TokenKind::Number(NumberKind::Integer)) => RedirectionKind::Duplicate {
                        descriptor,
                        operator_span,
                        target: IoNumber::new(
                            self.take().expect("target descriptor is current").span(),
                        ),
                    },
                    Some(TokenKind::Operator(Operator::Minus)) => RedirectionKind::Close {
                        descriptor,
                        operator_span,
                        target_span: self.take().expect("close marker is current").span(),
                    },
                    Some(_) => {
                        return Err(self.invalid_here(
                            "descriptor duplication requires a decimal descriptor or `-`",
                        ));
                    }
                    None => {
                        return Err(
                            self.incomplete_at(IncompleteReason::RedirectionTarget, operator_span)
                        );
                    }
                }
            }
            _ => return Err(self.invalid_at(operator_span, "expected a redirection operator")),
        };
        let end = match &kind {
            RedirectionKind::Input { target, .. } => target.span().end(),
            RedirectionKind::File(file) => file.target.span().end(),
            RedirectionKind::Duplicate { target, .. } => target.span().end(),
            RedirectionKind::Close { target_span, .. } => target_span.end(),
        };
        let span = self.span(start.start(), end);
        Ok(CommandItem::new(
            CommandItemKind::Redirection(Redirection::new(kind, span)),
            span,
        ))
    }

    fn parse_redirection_target(&mut self, operator: Span) -> ParseResult<Word> {
        if self.is_end() || self.at_command_end() {
            return Err(self.incomplete_at(IncompleteReason::RedirectionTarget, operator));
        }
        self.parse_word()
    }

    fn parse_word(&mut self) -> ParseResult<Word> {
        self.check_cancelled()?;
        let start = self.current_span()?;
        let mut parts = Vec::new();
        let mut end = start.start();
        loop {
            self.check_cancelled()?;
            if !parts.is_empty() && self.current_span().is_ok_and(|span| span.start() != end) {
                break;
            }
            let Some(part) = self.parse_word_part()? else {
                break;
            };
            end = part.span().end();
            parts.push(part);
        }
        if parts.is_empty() {
            return Err(self.invalid_here("expected a command word"));
        }
        Ok(Word::new(parts, self.span(start.start(), end)))
    }

    fn parse_word_part(&mut self) -> ParseResult<Option<WordPart>> {
        self.check_cancelled()?;
        let Some(token) = self.current().copied() else {
            return Ok(None);
        };
        if matches!(
            token.kind(),
            TokenKind::Delimiter(Delimiter::LeftBrace | Delimiter::RightBrace)
        ) && self
            .tokens
            .get(self.position + 1)
            .is_some_and(|next| next.kind() == token.kind() && token.is_adjacent_to(next))
        {
            let end = self.tokens[self.position + 1].span().end();
            self.position += 2;
            return Ok(Some(WordPart::new(
                WordPartKind::EscapedBrace,
                self.span(token.span().start(), end),
            )));
        }
        match token.kind() {
            TokenKind::BareEscape => {
                self.position += 1;
                Ok(Some(WordPart::new(WordPartKind::BareEscape, token.span())))
            }
            TokenKind::SingleQuoted => {
                self.position += 1;
                Ok(Some(WordPart::new(
                    WordPartKind::SingleQuoted,
                    token.span(),
                )))
            }
            TokenKind::DoubleQuoteStart => self.parse_double_quoted().map(Some),
            TokenKind::InterpolationStart | TokenKind::Delimiter(Delimiter::LeftBrace) => {
                self.parse_interpolation().map(Some)
            }
            TokenKind::EscapedBrace => {
                self.position += 1;
                Ok(Some(WordPart::new(
                    WordPartKind::EscapedBrace,
                    token.span(),
                )))
            }
            kind if is_bare_word_token(kind) => {
                let start = token.span();
                self.position += 1;
                let mut end = start.end();
                while let Some(next) = self.current().copied()
                    && next.span().start() == end
                    && is_bare_word_token(next.kind())
                {
                    self.check_cancelled()?;
                    end = next.span().end();
                    self.position += 1;
                }
                Ok(Some(WordPart::new(
                    WordPartKind::Bare,
                    self.span(start.start(), end),
                )))
            }
            _ => Ok(None),
        }
    }

    fn parse_double_quoted(&mut self) -> ParseResult<WordPart> {
        self.check_cancelled()?;
        let open = self.take().expect("double quote opener is current");
        let mut parts = Vec::new();
        while self.current_kind() != Some(TokenKind::DoubleQuoteEnd) {
            self.check_cancelled()?;
            let token = self.current().copied().ok_or_else(|| {
                self.incomplete_at(IncompleteReason::UnmatchedDoubleQuote, open.span())
            })?;
            let part = match token.kind() {
                TokenKind::DoubleText => {
                    self.position += 1;
                    WordPart::new(WordPartKind::DoubleText, token.span())
                }
                TokenKind::DoubleEscape | TokenKind::LineContinuation => {
                    self.position += 1;
                    WordPart::new(WordPartKind::DoubleEscape, token.span())
                }
                TokenKind::EscapedBrace => {
                    self.position += 1;
                    WordPart::new(WordPartKind::EscapedBrace, token.span())
                }
                TokenKind::InterpolationStart => self.parse_interpolation()?,
                _ => return Err(self.invalid_here("invalid double-quoted word part")),
            };
            parts.push(part);
        }
        let close = self.take().expect("double quote closer is current");
        Ok(WordPart::new(
            WordPartKind::DoubleQuoted(parts),
            self.span(open.span().start(), close.span().end()),
        ))
    }

    fn parse_interpolation(&mut self) -> ParseResult<WordPart> {
        let open = self.take().expect("interpolation opener is current");
        self.continuation_depth += 1;
        self.skip_layout();
        let expression = self.parse_expression()?;
        self.skip_layout();
        let close = self.expect_delimiter(Delimiter::RightBrace, "interpolation is not closed")?;
        self.continuation_depth -= 1;
        let span = self.span(open.span().start(), close.span().end());
        self.interpolation_spans.push(span);
        Ok(WordPart::new(
            WordPartKind::Interpolation(Box::new(expression)),
            span,
        ))
    }

    fn parse_closure(&mut self) -> ParseResult<Closure> {
        let open = self.expect_delimiter(Delimiter::LeftBrace, "expected a closure")?;
        let marker = self.take().expect("closure marker follows opener");
        self.continuation_depth += 1;
        let parameters = if marker.kind() == TokenKind::Operator(Operator::Or) {
            Vec::new()
        } else {
            let parameters = self.parse_closure_parameters()?;
            self.expect_operator(Operator::Pipe, "closure parameters require a closing `|`")?;
            parameters
        };
        self.skip_layout();
        let result_type = if self.take_operator(Operator::Arrow).is_some() {
            self.skip_layout();
            Some(self.parse_type_reference()?)
        } else {
            None
        };
        self.skip_layout();
        let body = self.parse_value_chain()?;
        self.skip_layout();
        let close = self.expect_delimiter(Delimiter::RightBrace, "closure body is not closed")?;
        self.continuation_depth -= 1;
        Ok(Closure {
            parameters,
            result_type,
            body: Box::new(body),
            span: self.span(open.span().start(), close.span().end()),
        })
    }

    fn parse_closure_parameters(&mut self) -> ParseResult<Vec<Parameter>> {
        self.skip_layout();
        let mut parameters = Vec::new();
        while self.current_kind() != Some(TokenKind::Operator(Operator::Pipe)) {
            let start = self.current_span()?;
            let pattern = self.parse_pattern()?;
            let name = self.pattern_primary_binding(&pattern).ok_or_else(|| {
                self.invalid_at(
                    self.pattern_span(&pattern),
                    "parameter pattern must bind a name",
                )
            })?;
            self.skip_inline();
            let type_annotation = if self.take_operator(Operator::Colon).is_some() {
                self.skip_layout();
                Some(self.parse_type_reference()?)
            } else {
                None
            };
            let end = type_annotation
                .as_ref()
                .map_or(name.span().end(), |reference| reference.span.end());
            parameters.push(Parameter {
                pattern,
                name,
                type_annotation,
                span: self.span(start.start(), end),
            });
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
        }
        Ok(parameters)
    }

    fn parse_expression(&mut self) -> ParseResult<Expression> {
        self.check_cancelled()?;
        self.parse_equality()
    }

    fn parse_equality(&mut self) -> ParseResult<Expression> {
        let left = self.parse_comparison()?;
        self.skip_inline();
        let operator = match self.current_kind() {
            Some(TokenKind::Operator(Operator::Equal)) => BinaryOperator::Equal,
            Some(TokenKind::Operator(Operator::NotEqual)) => BinaryOperator::NotEqual,
            _ => return Ok(left),
        };
        let token = self.take().expect("equality operator is current");
        self.skip_layout();
        let right = self.parse_comparison()?;
        let expression = self.binary(left, operator, token.span(), right);
        self.skip_inline();
        if matches!(
            self.current_kind(),
            Some(TokenKind::Operator(Operator::Equal | Operator::NotEqual))
        ) {
            return Err(self.invalid_here("equality operators are non-associative"));
        }
        Ok(expression)
    }

    fn parse_comparison(&mut self) -> ParseResult<Expression> {
        let left = self.parse_range()?;
        self.skip_inline();
        let operator = match self.current_kind() {
            Some(TokenKind::Operator(Operator::Less)) => BinaryOperator::Less,
            Some(TokenKind::Operator(Operator::LessEqual)) => BinaryOperator::LessEqual,
            Some(TokenKind::Operator(Operator::Greater)) => BinaryOperator::Greater,
            Some(TokenKind::Operator(Operator::GreaterEqual)) => BinaryOperator::GreaterEqual,
            Some(TokenKind::Keyword(Keyword::In)) => BinaryOperator::In,
            _ => return Ok(left),
        };
        let token = self.take().expect("comparison operator is current");
        self.skip_layout();
        let right = self.parse_range()?;
        let expression = self.binary(left, operator, token.span(), right);
        self.skip_inline();
        if matches!(
            self.current_kind(),
            Some(TokenKind::Operator(
                Operator::Less | Operator::LessEqual | Operator::Greater | Operator::GreaterEqual
            )) | Some(TokenKind::Keyword(Keyword::In))
        ) {
            return Err(self.invalid_here("comparison operators are non-associative"));
        }
        Ok(expression)
    }

    fn parse_range(&mut self) -> ParseResult<Expression> {
        let left = self.parse_additive()?;
        self.skip_inline();
        let operator = match self.current_kind() {
            Some(TokenKind::Operator(Operator::Range)) => BinaryOperator::Range,
            Some(TokenKind::Operator(Operator::RangeInclusive)) => BinaryOperator::RangeInclusive,
            _ => return Ok(left),
        };
        let token = self.take().expect("range operator is current");
        self.skip_layout();
        let right = self.parse_additive()?;
        let expression = self.binary(left, operator, token.span(), right);
        self.skip_inline();
        if matches!(
            self.current_kind(),
            Some(TokenKind::Operator(
                Operator::Range | Operator::RangeInclusive
            ))
        ) {
            return Err(self.invalid_here("range operators are non-associative"));
        }
        Ok(expression)
    }

    fn parse_additive(&mut self) -> ParseResult<Expression> {
        let mut expression = self.parse_multiplicative()?;
        loop {
            self.check_cancelled()?;
            self.skip_inline();
            let operator = match self.current_kind() {
                Some(TokenKind::Operator(Operator::Plus)) => BinaryOperator::Add,
                Some(TokenKind::Operator(Operator::Minus)) => BinaryOperator::Subtract,
                _ => break,
            };
            let token = self.take().expect("additive operator is current");
            self.skip_layout();
            let right = self.parse_multiplicative()?;
            expression = self.binary(expression, operator, token.span(), right);
        }
        Ok(expression)
    }

    fn parse_multiplicative(&mut self) -> ParseResult<Expression> {
        let mut expression = self.parse_unary()?;
        loop {
            self.check_cancelled()?;
            self.skip_inline();
            let operator = match self.current_kind() {
                Some(TokenKind::Operator(Operator::Star)) => BinaryOperator::Multiply,
                Some(TokenKind::Operator(Operator::Slash)) => BinaryOperator::Divide,
                Some(TokenKind::Operator(Operator::Percent)) => BinaryOperator::Remainder,
                _ => break,
            };
            let token = self.take().expect("multiplicative operator is current");
            self.skip_layout();
            let right = self.parse_unary()?;
            expression = self.binary(expression, operator, token.span(), right);
        }
        Ok(expression)
    }

    fn parse_unary(&mut self) -> ParseResult<Expression> {
        self.check_cancelled()?;
        let operator = match self.current_kind() {
            Some(TokenKind::Operator(Operator::Bang)) => UnaryOperator::Not,
            Some(TokenKind::Operator(Operator::Plus)) => UnaryOperator::Positive,
            Some(TokenKind::Operator(Operator::Minus)) => UnaryOperator::Negative,
            _ => return self.parse_postfix(),
        };
        let token = self.take().expect("unary operator is current");
        self.skip_layout();
        let operand = self.parse_unary()?;
        let span = self.span(token.span().start(), operand.span().end());
        Ok(Expression::new(
            ExpressionKind::Unary(UnaryExpression {
                operator: AstNode::new(operator, token.span()),
                operand: Box::new(operand),
            }),
            span,
        ))
    }

    fn parse_postfix(&mut self) -> ParseResult<Expression> {
        self.check_cancelled()?;
        let mut expression = self.parse_primary()?;
        loop {
            self.check_cancelled()?;
            self.skip_inline();
            match self.current_kind() {
                Some(TokenKind::Operator(Operator::Colon))
                    if matches!(
                        expression.kind(),
                        ExpressionKind::Name(_) | ExpressionKind::Qualified(_)
                    ) =>
                {
                    let checkpoint = self.position;
                    self.position += 1;
                    if self.take_operator(Operator::Colon).is_none() {
                        self.position = checkpoint;
                        break;
                    }
                    self.skip_inline();
                    let segment = self.parse_identifier()?;
                    let mut segments = match expression.into_kind() {
                        ExpressionKind::Name(reference) => vec![reference.name],
                        ExpressionKind::Qualified(name) => name.segments,
                        _ => unreachable!("qualified postfix starts from a name"),
                    };
                    let start = segments[0].span().start();
                    let end = segment.span().end();
                    segments.push(segment);
                    let name = QualifiedName {
                        segments,
                        span: self.span(start, end),
                    };
                    expression =
                        Expression::new(ExpressionKind::Qualified(name.clone()), name.span);
                }
                Some(TokenKind::Delimiter(Delimiter::LeftBrace))
                    if matches!(
                        expression.kind(),
                        ExpressionKind::Name(_) | ExpressionKind::Qualified(_)
                    ) && self.starts_nominal_record() =>
                {
                    let name = match expression.into_kind() {
                        ExpressionKind::Name(reference) => QualifiedName {
                            segments: vec![reference.name],
                            span: reference.span,
                        },
                        ExpressionKind::Qualified(name) => name,
                        _ => unreachable!("record construction starts from a name"),
                    };
                    let start = name.span.start();
                    self.position += 1;
                    self.continuation_depth += 1;
                    self.skip_layout();
                    let mut fields = Vec::new();
                    while !self.at_delimiter(Some(Delimiter::RightBrace)) {
                        let field = self.parse_identifier()?;
                        let field_start = field.span().start();
                        self.skip_inline();
                        self.expect_operator(Operator::Colon, "nominal field requires `:`")?;
                        self.skip_layout();
                        let value = self.parse_expression()?;
                        fields.push(NominalRecordFieldExpression {
                            name: field,
                            span: self.span(field_start, value.span().end()),
                            value,
                        });
                        self.skip_layout();
                        if self.take_operator(Operator::Comma).is_none() {
                            break;
                        }
                        self.skip_layout();
                    }
                    let close = self.expect_delimiter(
                        Delimiter::RightBrace,
                        "nominal record construction is not closed",
                    )?;
                    self.continuation_depth -= 1;
                    let span = self.span(start, close.span().end());
                    expression = Expression::new(
                        ExpressionKind::NominalRecord(NominalRecordExpression { name, fields }),
                        span,
                    );
                }
                Some(TokenKind::Delimiter(Delimiter::LeftParenthesis)) => {
                    self.position += 1;
                    self.continuation_depth += 1;
                    self.skip_layout();
                    let mut arguments = Vec::new();
                    while !self.at_delimiter(Some(Delimiter::RightParenthesis)) {
                        arguments.push(self.parse_expression()?);
                        self.skip_layout();
                        if self.take_operator(Operator::Comma).is_none() {
                            break;
                        }
                        self.skip_layout();
                        if self.at_delimiter(Some(Delimiter::RightParenthesis)) {
                            break;
                        }
                    }
                    let close =
                        self.expect_delimiter(Delimiter::RightParenthesis, "call is not closed")?;
                    self.continuation_depth -= 1;
                    let span = self.span(expression.span().start(), close.span().end());
                    expression = Expression::new(
                        ExpressionKind::Call(CallExpression {
                            callee: Box::new(expression),
                            type_arguments: Vec::new(),
                            arguments,
                        }),
                        span,
                    );
                }
                Some(TokenKind::Delimiter(Delimiter::LeftBracket)) => {
                    let can_have_type_arguments = matches!(
                        expression.kind(),
                        ExpressionKind::Name(_) | ExpressionKind::Qualified(_)
                    ) && self.bracket_is_followed_by_call();
                    if can_have_type_arguments {
                        self.position += 1;
                        self.continuation_depth += 1;
                        self.skip_layout();
                        let mut type_arguments = Vec::new();
                        while !self.at_delimiter(Some(Delimiter::RightBracket)) {
                            type_arguments.push(self.parse_type_reference()?);
                            self.skip_layout();
                            if self.take_operator(Operator::Comma).is_none() {
                                break;
                            }
                            self.skip_layout();
                        }
                        self.expect_delimiter(
                            Delimiter::RightBracket,
                            "explicit type arguments are not closed",
                        )?;
                        self.continuation_depth -= 1;
                        self.skip_inline();
                        if self.at_delimiter(Some(Delimiter::LeftParenthesis)) {
                            self.position += 1;
                            self.continuation_depth += 1;
                            self.skip_layout();
                            let mut arguments = Vec::new();
                            while !self.at_delimiter(Some(Delimiter::RightParenthesis)) {
                                arguments.push(self.parse_expression()?);
                                self.skip_layout();
                                if self.take_operator(Operator::Comma).is_none() {
                                    break;
                                }
                                self.skip_layout();
                            }
                            let close = self.expect_delimiter(
                                Delimiter::RightParenthesis,
                                "call is not closed",
                            )?;
                            self.continuation_depth -= 1;
                            let span = self.span(expression.span().start(), close.span().end());
                            expression = Expression::new(
                                ExpressionKind::Call(CallExpression {
                                    callee: Box::new(expression),
                                    type_arguments,
                                    arguments,
                                }),
                                span,
                            );
                            continue;
                        }
                    }
                    self.position += 1;
                    self.continuation_depth += 1;
                    self.skip_layout();
                    let index = self.parse_expression()?;
                    self.skip_layout();
                    let close =
                        self.expect_delimiter(Delimiter::RightBracket, "index is not closed")?;
                    self.continuation_depth -= 1;
                    let span = self.span(expression.span().start(), close.span().end());
                    expression = Expression::new(
                        ExpressionKind::Index(IndexExpression {
                            target: Box::new(expression),
                            index: Box::new(index),
                        }),
                        span,
                    );
                }
                Some(TokenKind::Operator(Operator::Dot)) => {
                    self.position += 1;
                    self.skip_inline();
                    let member = self.parse_identifier()?;
                    let span = self.span(expression.span().start(), member.span().end());
                    expression = Expression::new(
                        ExpressionKind::Member(MemberExpression {
                            target: Box::new(expression),
                            member,
                        }),
                        span,
                    );
                }
                _ => break,
            }
        }
        Ok(expression)
    }

    fn parse_primary(&mut self) -> ParseResult<Expression> {
        self.check_cancelled()?;
        self.skip_inline();
        let Some(kind) = self.current_kind() else {
            return Err(self.incomplete_here(IncompleteReason::Expression));
        };
        match kind {
            TokenKind::Keyword(Keyword::Null | Keyword::True | Keyword::False)
            | TokenKind::Number(_)
            | TokenKind::SingleQuoted
            | TokenKind::DoubleQuoteStart => {
                let literal = self.parse_literal()?;
                let span = literal.span();
                Ok(Expression::new(ExpressionKind::Literal(literal), span))
            }
            TokenKind::Identifier => {
                let name = self.parse_name_reference()?;
                Ok(Expression::new(ExpressionKind::Name(name), name.span))
            }
            TokenKind::Delimiter(Delimiter::LeftBracket) => self.parse_list(),
            TokenKind::Delimiter(Delimiter::LeftBrace) if self.starts_closure() => {
                let closure = self.parse_closure()?;
                let span = closure.span;
                Ok(Expression::new(ExpressionKind::Closure(closure), span))
            }
            TokenKind::Delimiter(Delimiter::LeftBrace) => self.parse_record(),
            TokenKind::Delimiter(Delimiter::LeftParenthesis) => self.parse_grouped_job(),
            _ => Err(self.invalid_here("expected an expression")),
        }
    }

    fn parse_literal(&mut self) -> ParseResult<Literal> {
        let token = self
            .current()
            .copied()
            .ok_or_else(|| self.incomplete_here(IncompleteReason::Expression))?;
        match token.kind() {
            TokenKind::Keyword(Keyword::Null) => {
                self.position += 1;
                Ok(Literal::new(LiteralKind::Null, token.span()))
            }
            TokenKind::Keyword(Keyword::True) => {
                self.position += 1;
                Ok(Literal::new(LiteralKind::Boolean(true), token.span()))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.position += 1;
                Ok(Literal::new(LiteralKind::Boolean(false), token.span()))
            }
            TokenKind::Number(NumberKind::Integer) => {
                self.position += 1;
                Ok(Literal::new(LiteralKind::Integer, token.span()))
            }
            TokenKind::Number(NumberKind::Float) => {
                self.position += 1;
                Ok(Literal::new(LiteralKind::Float, token.span()))
            }
            TokenKind::SingleQuoted => {
                self.position += 1;
                Ok(Literal::new(LiteralKind::SingleQuoted, token.span()))
            }
            TokenKind::DoubleQuoteStart => {
                let part = self.parse_double_quoted()?;
                let span = part.span();
                let WordPartKind::DoubleQuoted(parts) = part.into_kind() else {
                    unreachable!("double quote parser returns a double-quoted part")
                };
                Ok(Literal::new(LiteralKind::DoubleQuoted(parts), span))
            }
            _ => Err(self.invalid_here("expected a literal")),
        }
    }

    fn parse_list(&mut self) -> ParseResult<Expression> {
        let open = self.take().expect("list opener is current");
        self.continuation_depth += 1;
        self.skip_layout();
        let mut values = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBracket)) {
            values.push(self.parse_expression()?);
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
            if self.at_delimiter(Some(Delimiter::RightBracket)) {
                break;
            }
        }
        let close = self.expect_delimiter(Delimiter::RightBracket, "list is not closed")?;
        self.continuation_depth -= 1;
        let span = self.span(open.span().start(), close.span().end());
        Ok(Expression::new(ExpressionKind::List(values), span))
    }

    fn parse_record(&mut self) -> ParseResult<Expression> {
        let open = self.take().expect("record opener is current");
        self.continuation_depth += 1;
        self.skip_layout();
        let mut entries = Vec::new();
        while !self.at_delimiter(Some(Delimiter::RightBrace)) {
            let start = self.current_span()?;
            let key = match self.current_kind() {
                Some(TokenKind::Identifier) => RecordKey::Identifier(self.parse_identifier()?),
                Some(TokenKind::SingleQuoted) => {
                    RecordKey::SingleQuoted(self.take().expect("record key is current").span())
                }
                Some(TokenKind::DoubleQuoteStart) => {
                    RecordKey::DoubleQuoted(self.parse_double_quoted()?)
                }
                Some(_) => return Err(self.invalid_here("expected a record key")),
                None => return Err(self.incomplete_here(IncompleteReason::Expression)),
            };
            self.skip_inline();
            self.expect_operator(Operator::Colon, "record entry requires `:`")?;
            self.skip_layout();
            let value = self.parse_expression()?;
            let span = self.span(start.start(), value.span().end());
            entries.push(RecordEntry { key, value, span });
            self.skip_layout();
            if self.take_operator(Operator::Comma).is_none() {
                break;
            }
            self.skip_layout();
            if self.at_delimiter(Some(Delimiter::RightBrace)) {
                break;
            }
        }
        let close = self.expect_delimiter(Delimiter::RightBrace, "record is not closed")?;
        self.continuation_depth -= 1;
        let span = self.span(open.span().start(), close.span().end());
        Ok(Expression::new(ExpressionKind::Record(entries), span))
    }

    fn parse_grouped_job(&mut self) -> ParseResult<Expression> {
        let open = self.take().expect("group opener is current");
        self.continuation_depth += 1;
        self.skip_layout();
        let chain = self.parse_conditional_chain()?;
        self.skip_layout();
        if self.current_kind() == Some(TokenKind::Operator(Operator::Background)) {
            return Err(self.invalid_here("backgrounding is forbidden inside a grouped job"));
        }
        let close =
            self.expect_delimiter(Delimiter::RightParenthesis, "grouped job is not closed")?;
        self.continuation_depth -= 1;
        let span = self.span(open.span().start(), close.span().end());
        Ok(Expression::new(
            ExpressionKind::GroupedJob(Box::new(chain)),
            span,
        ))
    }

    fn binary(
        &self,
        left: Expression,
        operator: BinaryOperator,
        operator_span: Span,
        right: Expression,
    ) -> Expression {
        let span = self.span(left.span().start(), right.span().end());
        Expression::new(
            ExpressionKind::Binary(BinaryExpression {
                left: Box::new(left),
                operator: AstNode::new(operator, operator_span),
                right: Box::new(right),
            }),
            span,
        )
    }

    fn parse_identifier(&mut self) -> ParseResult<Identifier> {
        match self.take() {
            Some(token) if token.kind() == TokenKind::Identifier => {
                Ok(Identifier::new(token.span()))
            }
            Some(token) => Err(self.invalid_at(token.span(), "expected an identifier")),
            None => Err(self.incomplete_here(IncompleteReason::Expression)),
        }
    }

    fn parse_name_reference(&mut self) -> ParseResult<NameReference> {
        let name = self.parse_identifier()?;
        Ok(NameReference {
            name,
            span: name.span(),
        })
    }

    fn name_is_assignment(&self) -> bool {
        self.next_non_inline(self.position + 1)
            .is_some_and(|token| token.kind() == TokenKind::Operator(Operator::Assign))
    }

    fn starts_expression_stage(&self) -> bool {
        match self.current_kind() {
            Some(TokenKind::Identifier) => {
                let next_position = self.next_non_inline_position(self.position + 1);
                let next = next_position.and_then(|position| self.tokens.get(position));
                if self.value_chain_depth > 0
                    && next.is_none_or(|token| {
                        matches!(
                            token.kind(),
                            TokenKind::Newline
                                | TokenKind::Operator(
                                    Operator::Semicolon
                                        | Operator::Pipe
                                        | Operator::PipeBoth
                                        | Operator::And
                                        | Operator::Or
                                        | Operator::Background
                                )
                                | TokenKind::Delimiter(
                                    Delimiter::RightParenthesis
                                        | Delimiter::RightBracket
                                        | Delimiter::RightBrace
                                )
                        )
                    })
                {
                    return true;
                }
                if self.control_condition_depth > 0
                    && next.is_some_and(|token| {
                        token.kind() == TokenKind::Delimiter(Delimiter::LeftBrace)
                    })
                {
                    return true;
                }
                if let (Some(position), Some(next)) = (next_position, next)
                    && next.kind() == TokenKind::Operator(Operator::Minus)
                    && !self
                        .current()
                        .is_some_and(|current| current.is_adjacent_to(next))
                    && self.tokens.get(position + 1).is_some_and(|second| {
                        second.kind() == TokenKind::Operator(Operator::Minus)
                            && next.is_adjacent_to(second)
                    })
                {
                    return false;
                }
                next.is_some_and(|token| {
                    matches!(
                        token.kind(),
                        TokenKind::Delimiter(Delimiter::LeftParenthesis | Delimiter::LeftBracket)
                            | TokenKind::Operator(
                                Operator::Colon
                                    | Operator::Dot
                                    | Operator::Equal
                                    | Operator::NotEqual
                                    | Operator::Less
                                    | Operator::LessEqual
                                    | Operator::Greater
                                    | Operator::GreaterEqual
                                    | Operator::Plus
                                    | Operator::Minus
                                    | Operator::Star
                                    | Operator::Slash
                                    | Operator::Percent
                                    | Operator::Range
                                    | Operator::RangeInclusive
                            )
                    )
                }) || (next
                    .is_some_and(|token| token.kind() == TokenKind::Operator(Operator::Colon))
                    && self
                        .next_non_inline(self.position + 2)
                        .is_some_and(|token| token.kind() == TokenKind::Operator(Operator::Colon)))
            }
            Some(
                TokenKind::Number(_)
                | TokenKind::SingleQuoted
                | TokenKind::DoubleQuoteStart
                | TokenKind::Keyword(Keyword::Null | Keyword::True | Keyword::False)
                | TokenKind::Delimiter(
                    Delimiter::LeftParenthesis | Delimiter::LeftBracket | Delimiter::LeftBrace,
                )
                | TokenKind::Operator(Operator::Bang | Operator::Plus | Operator::Minus),
            ) => true,
            _ => false,
        }
    }

    fn starts_forced_command(&self) -> bool {
        let Some(marker) = self.current() else {
            return false;
        };
        if marker.kind() != TokenKind::Operator(Operator::Caret) {
            return false;
        }
        self.tokens
            .get(self.position + 1)
            .is_some_and(|next| marker.is_adjacent_to(next) && can_start_word(next.kind()))
    }

    fn starts_closure(&self) -> bool {
        let Some(open) = self.current() else {
            return false;
        };
        open.kind() == TokenKind::Delimiter(Delimiter::LeftBrace)
            && self.tokens.get(self.position + 1).is_some_and(|marker| {
                open.is_adjacent_to(marker)
                    && matches!(
                        marker.kind(),
                        TokenKind::Operator(Operator::Pipe | Operator::Or)
                    )
            })
    }

    fn starts_nominal_record(&self) -> bool {
        if self.current_kind() != Some(TokenKind::Delimiter(Delimiter::LeftBrace)) {
            return false;
        }
        let Some(first) = self.next_non_layout_position(self.position + 1) else {
            return false;
        };
        match self.tokens[first].kind() {
            TokenKind::Delimiter(Delimiter::RightBrace) => true,
            TokenKind::Identifier => {
                self.next_non_layout_position(first + 1)
                    .is_some_and(|position| {
                        let colon = &self.tokens[position];
                        colon.kind() == TokenKind::Operator(Operator::Colon)
                            && !self.tokens.get(position + 1).is_some_and(|second| {
                                second.kind() == TokenKind::Operator(Operator::Colon)
                                    && colon.is_adjacent_to(second)
                            })
                    })
            }
            _ => false,
        }
    }

    fn starts_redirection(&self) -> bool {
        matches!(
            self.current_kind(),
            Some(TokenKind::Operator(
                Operator::Less | Operator::Greater | Operator::Append | Operator::Duplicate
            ))
        ) || self.io_number_before_redirection()
    }

    fn io_number_before_redirection(&self) -> bool {
        let Some(number) = self.current() else {
            return false;
        };
        if number.kind() != TokenKind::Number(NumberKind::Integer) {
            return false;
        }
        self.tokens.get(self.position + 1).is_some_and(|operator| {
            number.is_adjacent_to(operator)
                && matches!(
                    operator.kind(),
                    TokenKind::Operator(
                        Operator::Less | Operator::Greater | Operator::Append | Operator::Duplicate
                    )
                )
        })
    }

    fn at_command_end(&self) -> bool {
        if self.starts_closure() {
            return false;
        }
        matches!(
            self.current_kind(),
            None | Some(TokenKind::Newline)
                | Some(TokenKind::Operator(
                    Operator::Semicolon
                        | Operator::Pipe
                        | Operator::PipeBoth
                        | Operator::And
                        | Operator::Or
                        | Operator::Background
                ))
                | Some(TokenKind::Delimiter(
                    Delimiter::RightParenthesis | Delimiter::RightBrace | Delimiter::RightBracket
                ))
        )
    }

    fn at_expression_end(&self) -> bool {
        matches!(
            self.current_kind(),
            None | Some(TokenKind::Newline)
                | Some(TokenKind::Operator(
                    Operator::Semicolon
                        | Operator::Pipe
                        | Operator::PipeBoth
                        | Operator::And
                        | Operator::Or
                        | Operator::Background
                        | Operator::Comma
                        | Operator::MatchArrow
                ))
                | Some(TokenKind::Delimiter(
                    Delimiter::RightParenthesis
                        | Delimiter::RightBrace
                        | Delimiter::RightBracket
                        | Delimiter::LeftBrace
                ))
        )
    }

    fn skip_inline(&mut self) {
        while matches!(
            self.current_kind(),
            Some(
                TokenKind::Whitespace
                    | TokenKind::Comment
                    | TokenKind::DocumentationComment
                    | TokenKind::LineContinuation
            )
        ) || (self.continuation_depth > 0 && self.current_kind() == Some(TokenKind::Newline))
        {
            if (self.is_cancelled)() {
                break;
            }
            self.position += 1;
        }
    }

    fn skip_layout(&mut self) {
        while matches!(
            self.current_kind(),
            Some(
                TokenKind::Whitespace
                    | TokenKind::Comment
                    | TokenKind::DocumentationComment
                    | TokenKind::LineContinuation
                    | TokenKind::Newline
            )
        ) {
            if (self.is_cancelled)() {
                break;
            }
            self.position += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.current_kind(),
            Some(
                TokenKind::Whitespace
                    | TokenKind::Comment
                    | TokenKind::DocumentationComment
                    | TokenKind::LineContinuation
                    | TokenKind::Newline
                    | TokenKind::Operator(Operator::Semicolon)
            )
        ) {
            if (self.is_cancelled)() {
                break;
            }
            self.position += 1;
        }
    }

    fn skip_separators_with_documentation(&mut self) -> Option<DocumentationBlock> {
        let mut lines = Vec::new();
        let mut newlines_after_documentation = 0usize;

        while let Some(token) = self.current().copied() {
            if (self.is_cancelled)() {
                break;
            }
            match token.kind() {
                TokenKind::DocumentationComment => {
                    if !lines.is_empty() && newlines_after_documentation != 1 {
                        lines.clear();
                    }
                    lines.push(token.span());
                    newlines_after_documentation = 0;
                    self.position += 1;
                }
                TokenKind::Newline => {
                    if !lines.is_empty() {
                        newlines_after_documentation += 1;
                        if newlines_after_documentation > 1 {
                            lines.clear();
                            newlines_after_documentation = 0;
                        }
                    }
                    self.position += 1;
                }
                TokenKind::Whitespace => self.position += 1,
                TokenKind::Comment
                | TokenKind::LineContinuation
                | TokenKind::Operator(Operator::Semicolon) => {
                    lines.clear();
                    newlines_after_documentation = 0;
                    self.position += 1;
                }
                _ => break,
            }
        }

        (!lines.is_empty() && newlines_after_documentation == 1)
            .then_some(DocumentationBlock { lines })
    }

    fn synchronize(&mut self, closing: Option<Delimiter>) {
        let mut delimiters = Vec::new();
        while let Some(token) = self.current().copied() {
            if (self.is_cancelled)() {
                break;
            }
            if delimiters.is_empty() {
                if closing.is_some_and(|delimiter| token.kind() == TokenKind::Delimiter(delimiter))
                {
                    break;
                }
                if matches!(
                    token.kind(),
                    TokenKind::Newline
                        | TokenKind::Operator(Operator::Semicolon | Operator::Background)
                ) {
                    self.position += 1;
                    break;
                }
            }

            match token.kind() {
                TokenKind::Delimiter(
                    delimiter @ (Delimiter::LeftParenthesis
                    | Delimiter::LeftBracket
                    | Delimiter::LeftBrace),
                ) => delimiters.push(delimiter),
                TokenKind::Delimiter(
                    Delimiter::RightParenthesis | Delimiter::RightBracket | Delimiter::RightBrace,
                ) if !delimiters.is_empty() => {
                    delimiters.pop();
                }
                _ => {}
            }
            self.position += 1;
        }
    }

    fn at_statement_separator(&self) -> bool {
        matches!(
            self.current_kind(),
            Some(TokenKind::Newline | TokenKind::Operator(Operator::Semicolon))
        )
    }

    fn at_delimiter(&self, delimiter: Option<Delimiter>) -> bool {
        delimiter
            .is_some_and(|delimiter| self.current_kind() == Some(TokenKind::Delimiter(delimiter)))
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        self.current_kind() == Some(TokenKind::Keyword(keyword))
    }

    fn take_keyword(&mut self, keyword: Keyword) -> Option<Token> {
        if self.at_keyword(keyword) {
            self.take()
        } else {
            None
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword, message: &str) -> ParseResult<Token> {
        self.take_keyword(keyword)
            .ok_or_else(|| self.expected(message, IncompleteReason::Expression))
    }

    fn expect_contextual_identifier(
        &mut self,
        spelling: &str,
        message: &str,
    ) -> ParseResult<Token> {
        let matches = self.current().is_some_and(|token| {
            token.kind() == TokenKind::Identifier
                && self.source.slice(token.span()).ok() == Some(spelling)
        });
        if matches {
            Ok(self
                .take()
                .expect("matching contextual identifier is current"))
        } else {
            Err(self.expected(message, IncompleteReason::Expression))
        }
    }

    fn take_operator(&mut self, operator: Operator) -> Option<Token> {
        if self.current_kind() == Some(TokenKind::Operator(operator)) {
            self.take()
        } else {
            None
        }
    }

    fn expect_operator(&mut self, operator: Operator, message: &str) -> ParseResult<Token> {
        self.take_operator(operator)
            .ok_or_else(|| self.expected(message, IncompleteReason::Expression))
    }

    fn take_delimiter(&mut self, delimiter: Delimiter) -> Option<Token> {
        if self.current_kind() == Some(TokenKind::Delimiter(delimiter)) {
            self.take()
        } else {
            None
        }
    }

    fn expect_delimiter(&mut self, delimiter: Delimiter, message: &str) -> ParseResult<Token> {
        self.take_delimiter(delimiter)
            .ok_or_else(|| self.expected(message, IncompleteReason::Expression))
    }

    fn expected(&self, message: &str, reason: IncompleteReason) -> ParseError {
        if self.is_end() {
            self.incomplete_here(reason)
        } else {
            self.invalid_here(message)
        }
    }

    fn next_non_inline(&self, position: usize) -> Option<&Token> {
        self.next_non_inline_position(position)
            .and_then(|position| self.tokens.get(position))
    }

    fn next_non_inline_position(&self, mut position: usize) -> Option<usize> {
        while let Some(token) = self.tokens.get(position) {
            if !matches!(
                token.kind(),
                TokenKind::Whitespace
                    | TokenKind::Comment
                    | TokenKind::DocumentationComment
                    | TokenKind::LineContinuation
            ) {
                return Some(position);
            }
            position += 1;
        }
        None
    }

    fn next_non_layout_position(&self, mut position: usize) -> Option<usize> {
        while let Some(token) = self.tokens.get(position) {
            if !matches!(
                token.kind(),
                TokenKind::Whitespace
                    | TokenKind::Newline
                    | TokenKind::Comment
                    | TokenKind::DocumentationComment
                    | TokenKind::LineContinuation
            ) {
                return Some(position);
            }
            position += 1;
        }
        None
    }

    fn bracket_is_followed_by_call(&self) -> bool {
        let mut position = self.position;
        let mut depth = 0usize;
        while let Some(token) = self.tokens.get(position) {
            match token.kind() {
                TokenKind::Delimiter(Delimiter::LeftBracket) => depth += 1,
                TokenKind::Delimiter(Delimiter::RightBracket) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        position += 1;
                        break;
                    }
                }
                _ => {}
            }
            position += 1;
        }
        if depth != 0 {
            return false;
        }
        while let Some(token) = self.tokens.get(position) {
            if matches!(
                token.kind(),
                TokenKind::Whitespace
                    | TokenKind::Comment
                    | TokenKind::DocumentationComment
                    | TokenKind::LineContinuation
            ) || (self.continuation_depth > 0 && token.kind() == TokenKind::Newline)
            {
                position += 1;
                continue;
            }
            return token.kind() == TokenKind::Delimiter(Delimiter::LeftParenthesis);
        }
        false
    }

    fn current(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn current_kind(&self) -> Option<TokenKind> {
        self.current().map(Token::kind)
    }

    fn current_span(&self) -> ParseResult<Span> {
        self.current()
            .map(Token::span)
            .ok_or_else(|| self.incomplete_here(IncompleteReason::Expression))
    }

    fn take(&mut self) -> Option<Token> {
        let token = self.current().copied()?;
        self.position += 1;
        Some(token)
    }

    fn previous_end(&self) -> usize {
        self.tokens
            .get(self.position.saturating_sub(1))
            .map_or(0, |token| token.span().end())
    }

    fn is_end(&self) -> bool {
        self.position >= self.tokens.len()
    }

    fn check_cancelled(&self) -> ParseResult<()> {
        if (self.is_cancelled)() {
            Err(ParseError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        self.source
            .span(start..end)
            .expect("parser combines source-local token boundaries")
    }

    fn incomplete_here(&self, reason: IncompleteReason) -> ParseError {
        let span = self.current().map_or_else(
            || self.span(self.source.len(), self.source.len()),
            Token::span,
        );
        self.incomplete_at(reason, span)
    }

    fn incomplete_at(&self, reason: IncompleteReason, span: Span) -> ParseError {
        ParseError::Incomplete(IncompleteInput::new(reason, span))
    }

    fn invalid_here(&self, message: &str) -> ParseError {
        let span = self.current().map_or_else(
            || self.span(self.source.len(), self.source.len()),
            Token::span,
        );
        self.invalid_at(span, message)
    }

    fn invalid_at(&self, span: Span, message: &str) -> ParseError {
        ParseError::Invalid(
            Diagnostic::new(Severity::Error, "OP1000", message).with_primary(span, message),
        )
    }
}

fn can_start_word(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(_)
            | TokenKind::Number(_)
            | TokenKind::WordText
            | TokenKind::BareEscape
            | TokenKind::SingleQuoted
            | TokenKind::DoubleQuoteStart
            | TokenKind::InterpolationStart
    ) || is_bare_word_token(kind)
}

fn is_bare_word_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(_)
            | TokenKind::Number(_)
            | TokenKind::WordText
            | TokenKind::Operator(
                Operator::Assign
                    | Operator::Equal
                    | Operator::NotEqual
                    | Operator::Plus
                    | Operator::Minus
                    | Operator::Star
                    | Operator::Slash
                    | Operator::Percent
                    | Operator::Bang
                    | Operator::Range
                    | Operator::RangeInclusive
                    | Operator::Arrow
                    | Operator::MatchArrow
                    | Operator::Dot
                    | Operator::Comma
                    | Operator::Colon
                    | Operator::Caret
            )
    )
}
