#![forbid(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use flashshell_syntax::*;

const RANDOM_CASES: usize = 4_096;
const FRAGMENTS: &[&str] = &[
    " ",
    "\t",
    "\n",
    "\r\n",
    "\r",
    "\0",
    "a",
    "name",
    "if",
    "else",
    "let",
    "mut",
    "true",
    "false",
    "null",
    "0",
    "1.5",
    "0xff",
    "_",
    "$x",
    "$",
    "${",
    "$(",
    "'",
    "'text'",
    "\"",
    "\"text\"",
    "\\",
    "\\\n",
    "# comment",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    "{|",
    "||",
    "|",
    "|&",
    "&&",
    "&",
    "<",
    ">",
    ">>",
    ">&",
    "...",
    "..",
    "..=",
    "==",
    "!=",
    "+",
    "-",
    "*",
    "/",
    "%",
    ".",
    ",",
    ":",
    ";",
    "^",
    "=>",
    "->",
    "é",
    "界",
    "🙂",
    "e\u{301}",
    "\u{2003}",
    "\u{2028}",
    "ßeta",
];

#[test]
fn bounded_utf8_sources_preserve_syntax_invariants() {
    for (case, text) in generated_sources().into_iter().enumerate() {
        let source = SourceFile::new(
            SourceId::new(case as u32),
            format!("generated-{case}"),
            text,
        );
        let tokens = lex(&source);

        assert_lossless_progress(case, &source, &tokens);

        let classification = classify_tokens(&source, &tokens).unwrap_or_else(|error| {
            panic!(
                "classification rejected lexer spans for case {case} ({:?}): {error}",
                source.text()
            )
        });
        match classification {
            SyntaxClassification::Complete => {}
            SyntaxClassification::Incomplete(incomplete) => {
                assert_valid_span(case, &source, incomplete.span())
            }
            SyntaxClassification::Invalid(diagnostic) => {
                assert_diagnostic_spans(case, &source, &diagnostic)
            }
        }

        let parsed = catch_unwind(AssertUnwindSafe(|| parse(&source)))
            .unwrap_or_else(|_| panic!("parser panicked for case {case} ({:?})", source.text()));
        match parsed {
            ParseOutcome::Complete(script) => SpanChecker::new(case, &source).script(&script),
            ParseOutcome::Incomplete(incomplete) => {
                assert_valid_span(case, &source, incomplete.span())
            }
            ParseOutcome::Invalid(diagnostics) => {
                assert!(!diagnostics.is_empty(), "empty diagnostics for case {case}");
                for diagnostic in &diagnostics {
                    assert_diagnostic_spans(case, &source, diagnostic);
                }
            }
        }
    }
}

fn assert_lossless_progress(case: usize, source: &SourceFile, tokens: &[Token]) {
    let mut next_start = 0;
    for token in tokens {
        let span = token.span();
        assert_valid_span(case, source, span);
        assert!(!span.is_empty(), "empty token span for case {case}");
        assert_eq!(
            span.start(),
            next_start,
            "token gap/overlap for case {case}"
        );
        next_start = span.end();
    }
    assert_eq!(
        next_start,
        source.len(),
        "incomplete lexing for case {case}"
    );
}

fn assert_diagnostic_spans(case: usize, source: &SourceFile, diagnostic: &Diagnostic) {
    assert!(
        !diagnostic.labels().is_empty(),
        "unlabelled diagnostic for case {case}"
    );
    for label in diagnostic.labels() {
        assert_valid_span(case, source, label.span());
    }
}

fn assert_valid_span(case: usize, source: &SourceFile, span: Span) {
    assert_eq!(
        span.source_id(),
        source.id(),
        "wrong span source for case {case}"
    );
    source.slice(span).unwrap_or_else(|error| {
        panic!(
            "invalid span {span:?} for case {case} ({:?}): {error}",
            source.text()
        )
    });
}

fn generated_sources() -> Vec<String> {
    let mut sources = vec![
        String::new(),
        FRAGMENTS.concat(),
        "(".repeat(32),
        ")".repeat(32),
        "${$(\"{|[]})".repeat(8),
        "é🙂界".repeat(32),
    ];

    for left in FRAGMENTS {
        for right in FRAGMENTS {
            sources.push(format!("{left}{right}"));
        }
    }

    let mut generator = Generator::new(0x4f1b_bcdd_9a73_e251);
    for _ in 0..RANDOM_CASES {
        let fragment_count = generator.bounded(32);
        let mut source = String::new();
        for _ in 0..fragment_count {
            if generator.bounded(4) == 0 {
                source.push(generator.scalar());
            } else {
                source.push_str(FRAGMENTS[generator.bounded(FRAGMENTS.len())]);
            }
        }
        sources.push(source);
    }
    sources
}

struct Generator(u64);

impl Generator {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bounded(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }

    fn scalar(&mut self) -> char {
        loop {
            let value = (self.next() % 0x11_0000) as u32;
            if let Some(character) = char::from_u32(value) {
                return character;
            }
        }
    }
}

struct SpanChecker<'source> {
    case: usize,
    source: &'source SourceFile,
}

impl<'source> SpanChecker<'source> {
    const fn new(case: usize, source: &'source SourceFile) -> Self {
        Self { case, source }
    }

    fn span(&self, span: Span) {
        assert_valid_span(self.case, self.source, span);
    }

    fn identifier(&self, identifier: Identifier) {
        self.span(identifier.span());
    }

    fn variable(&self, variable: VariableReference) {
        self.span(variable.span);
        self.identifier(variable.name);
    }

    fn type_reference(&self, reference: &TypeReference) {
        self.span(reference.span);
        self.identifier(reference.name);
        for argument in &reference.arguments {
            self.type_reference(argument);
        }
    }

    fn parameter(&self, parameter: &Parameter) {
        self.span(parameter.span);
        self.identifier(parameter.name);
        if let Some(annotation) = &parameter.type_annotation {
            self.type_reference(annotation);
        }
    }

    fn script(&self, script: &Script) {
        self.span(script.span());
        for statement in script.statements() {
            self.statement(statement);
        }
    }

    fn statement(&self, statement: &Statement) {
        self.span(statement.span());
        match statement.kind() {
            StatementKind::Declaration(declaration) => {
                self.identifier(declaration.name);
                if let Some(annotation) = &declaration.type_annotation {
                    self.type_reference(annotation);
                }
                self.expression(&declaration.value);
            }
            StatementKind::Assignment(assignment) => {
                self.variable(assignment.target);
                self.expression(&assignment.value);
            }
            StatementKind::Environment(environment) => match environment {
                EnvironmentStatement::Export { name, value } => {
                    self.identifier(*name);
                    self.expression(value);
                }
                EnvironmentStatement::Unset { name } => self.identifier(*name),
            },
            StatementKind::Function(function) => {
                self.identifier(function.name);
                for parameter in &function.parameters {
                    self.parameter(parameter);
                }
                if let Some(return_type) = &function.return_type {
                    self.type_reference(return_type);
                }
                self.block(&function.body);
            }
            StatementKind::If(statement) => self.if_statement(statement),
            StatementKind::While(statement) => {
                self.chain(&statement.condition);
                self.block(&statement.body);
            }
            StatementKind::For(statement) => {
                self.identifier(statement.binding);
                self.expression(&statement.iterable);
                self.block(&statement.body);
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value);
                for arm in &statement.arms {
                    self.span(arm.span);
                    self.pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.expression(guard);
                    }
                    self.block(&arm.body);
                }
            }
            StatementKind::Control(ControlTransfer::Return(Some(expression))) => {
                self.expression(expression)
            }
            StatementKind::Control(ControlTransfer::Break | ControlTransfer::Continue)
            | StatementKind::Control(ControlTransfer::Return(None)) => {}
            StatementKind::Job(statement) => {
                self.chain(&statement.chain);
                if let Some(span) = statement.background_span {
                    self.span(span);
                }
            }
        }
    }

    fn if_statement(&self, statement: &IfStatement) {
        self.chain(&statement.condition);
        self.block(&statement.then_block);
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.block(block),
            Some(ElseBranch::If(statement)) => {
                self.span(statement.span());
                self.if_statement(statement.kind());
            }
            None => {}
        }
    }

    fn block(&self, block: &Block) {
        self.span(block.span);
        for statement in &block.statements {
            self.statement(statement);
        }
    }

    fn pattern(&self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard(span) => self.span(*span),
            Pattern::Literal(literal) => self.literal(literal),
            Pattern::Binding(identifier) => self.identifier(*identifier),
        }
    }

    fn expression(&self, expression: &Expression) {
        self.span(expression.span());
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(variable) => self.variable(*variable),
            ExpressionKind::Symbol(identifier) => self.identifier(*identifier),
            ExpressionKind::List(items) => {
                for item in items {
                    self.expression(item);
                }
            }
            ExpressionKind::Record(entries) => {
                for entry in entries {
                    self.span(entry.span);
                    self.record_key(&entry.key);
                    self.expression(&entry.value);
                }
            }
            ExpressionKind::Closure(closure) => self.closure(closure),
            ExpressionKind::CommandSubstitution(chain) | ExpressionKind::GroupedJob(chain) => {
                self.chain(chain)
            }
            ExpressionKind::Call(call) => {
                self.expression(&call.callee);
                for argument in &call.arguments {
                    self.expression(argument);
                }
            }
            ExpressionKind::Index(index) => {
                self.expression(&index.target);
                self.expression(&index.index);
            }
            ExpressionKind::Member(member) => {
                self.expression(&member.target);
                self.identifier(member.member);
            }
            ExpressionKind::Unary(unary) => {
                self.span(unary.operator.span());
                self.expression(&unary.operand);
            }
            ExpressionKind::Binary(binary) => {
                self.expression(&binary.left);
                self.span(binary.operator.span());
                self.expression(&binary.right);
            }
        }
    }

    fn literal(&self, literal: &Literal) {
        self.span(literal.span());
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            for part in parts {
                self.word_part(part);
            }
        }
    }

    fn record_key(&self, key: &RecordKey) {
        match key {
            RecordKey::Identifier(identifier) => self.identifier(*identifier),
            RecordKey::SingleQuoted(span) => self.span(*span),
            RecordKey::DoubleQuoted(part) => self.word_part(part),
        }
    }

    fn closure(&self, closure: &Closure) {
        self.span(closure.span);
        for parameter in &closure.parameters {
            self.parameter(parameter);
        }
        self.chain(&closure.body);
    }

    fn chain(&self, chain: &ConditionalChain) {
        self.span(chain.span());
        for operator in chain.operators() {
            self.span(operator.span());
        }
        for term in chain.or_terms() {
            self.and_chain(term);
        }
    }

    fn and_chain(&self, chain: &AndChain) {
        self.span(chain.span());
        for operator in chain.operators() {
            self.span(operator.span());
        }
        for term in chain.and_terms() {
            self.pipeline(term);
        }
    }

    fn pipeline(&self, pipeline: &Pipeline) {
        self.span(pipeline.span());
        for operator in pipeline.operators() {
            self.span(operator.span());
        }
        for stage in pipeline.stages() {
            self.span(stage.span());
            match stage.kind() {
                StageKind::Command(command) => self.command(command),
                StageKind::Expression(expression) => self.expression(expression),
            }
        }
    }

    fn command(&self, command: &CommandStage) {
        self.span(command.head.span());
        self.word(command.head.word());
        for item in &command.items {
            self.span(item.span());
            match item.kind() {
                CommandItemKind::Word(word) => self.word(word),
                CommandItemKind::Spread(variable) => self.variable(*variable),
                CommandItemKind::Closure(closure) => self.closure(closure),
                CommandItemKind::Redirection(redirection) => self.redirection(redirection),
            }
        }
    }

    fn word(&self, word: &Word) {
        self.span(word.span());
        for part in word.parts() {
            self.word_part(part);
        }
    }

    fn word_part(&self, part: &WordPart) {
        self.span(part.span());
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => {
                for part in parts {
                    self.word_part(part);
                }
            }
            WordPartKind::Variable(identifier) => self.identifier(*identifier),
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(chain) => self.chain(chain),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape => {}
        }
    }

    fn redirection(&self, redirection: &Redirection) {
        self.span(redirection.span());
        match redirection.kind() {
            RedirectionKind::Input {
                descriptor,
                operator_span,
                target,
            } => {
                if let Some(descriptor) = descriptor {
                    self.span(descriptor.span());
                }
                self.span(*operator_span);
                self.word(target);
            }
            RedirectionKind::File(file) => {
                if let Some(descriptor) = file.descriptor {
                    self.span(descriptor.span());
                }
                self.span(file.operator_span);
                self.word(&file.target);
            }
            RedirectionKind::Duplicate {
                descriptor,
                operator_span,
                target,
            } => {
                self.span(descriptor.span());
                self.span(*operator_span);
                self.span(target.span());
            }
            RedirectionKind::Close {
                descriptor,
                operator_span,
                target_span,
            } => {
                self.span(descriptor.span());
                self.span(*operator_span);
                self.span(*target_span);
            }
        }
    }
}
