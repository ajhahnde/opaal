use std::collections::BTreeSet;

use crate::{
    Block, Delimiter, Diagnostic, ElseBranch, IfStatement, IncompleteInput, Operator, ParseOutcome,
    Script, SourceFile, Statement, StatementKind, Token, TokenKind, lex_opaal,
    parser::parse_opaal_with_interpolation_spans,
};

/// The result of formatting one source file through the shared parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormatOutcome {
    Complete(String),
    Incomplete(IncompleteInput),
    Invalid(Vec<Diagnostic>),
}

/// Canonically formats one complete OPAAL source.
#[must_use]
pub fn format_source_opaal(source: &SourceFile) -> FormatOutcome {
    let (outcome, interpolation_spans) = parse_opaal_with_interpolation_spans(source);
    match outcome {
        ParseOutcome::Complete(script) => {
            let tokens = lex_opaal(source);
            let layout = FormatLayout::for_script(&script, &tokens, &interpolation_spans);
            FormatOutcome::Complete(format_tokens(source, &tokens, &layout))
        }
        ParseOutcome::Incomplete(incomplete) => FormatOutcome::Incomplete(incomplete),
        ParseOutcome::Invalid(diagnostics) => FormatOutcome::Invalid(diagnostics),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenDelimiter {
    IndentingBrace,
    InterpolationBrace,
    Parenthesis,
    Bracket,
}

#[derive(Default)]
struct FormatLayout {
    newline_before: BTreeSet<usize>,
    newline_after: BTreeSet<usize>,
    space_before: BTreeSet<usize>,
    space_after: BTreeSet<usize>,
    no_space_before: BTreeSet<usize>,
    no_space_after: BTreeSet<usize>,
    interpolation_open: BTreeSet<usize>,
    interpolation_close: BTreeSet<usize>,
    suppress_source_newline: BTreeSet<usize>,
}

impl FormatLayout {
    fn for_script(script: &Script, tokens: &[Token], interpolation_spans: &[crate::Span]) -> Self {
        let mut layout = Self::default();
        for span in interpolation_spans {
            layout.interpolation_open.insert(span.start());
            layout.interpolation_close.insert(span.end() - 1);
            layout.no_space_after.insert(span.start() + 1);
            layout.no_space_before.insert(span.end() - 1);
        }
        layout.statements(script.statements(), tokens);
        layout
    }

    fn statements(&mut self, statements: &[Statement], tokens: &[Token]) {
        for statement in statements {
            match statement.kind() {
                StatementKind::Function(function) => {
                    self.signature(tokens_in(
                        tokens,
                        statement.span().start(),
                        function.body.span.start(),
                    ));
                    self.declaration_body(&function.body);
                    self.statements(&function.body.statements, tokens);
                }
                StatementKind::Action(action) => {
                    self.signature(tokens_in(
                        tokens,
                        statement.span().start(),
                        action.effects_span.start(),
                    ));
                    self.newline_before.insert(action.effects_span.start());
                    self.newline_before.insert(action.effects_span.end() - 1);
                    self.newline_after.insert(action.effects_span.end());
                    self.declaration_body(&action.body);
                    for request in &action.effects {
                        self.newline_after.insert(request.span.end());
                    }
                    self.spacing(
                        tokens_in(
                            tokens,
                            action.effects_span.start(),
                            action.effects_span.end(),
                        ),
                        action.effects_span.start(),
                    );
                    for token in tokens_in(
                        tokens,
                        action.effects_span.start(),
                        action.effects_span.end(),
                    ) {
                        if token.kind() == TokenKind::Delimiter(Delimiter::LeftBrace) {
                            self.space_before.insert(token.span().start());
                            self.newline_after.insert(token.span().end());
                        }
                    }
                    self.statements(&action.body.statements, tokens);
                }
                StatementKind::Task(_) => {
                    for token in tokens_in(tokens, statement.span().start(), statement.span().end())
                    {
                        if token.kind() == TokenKind::Operator(Operator::Assign) {
                            self.space_before.insert(token.span().start());
                            self.space_after.insert(token.span().end());
                        }
                    }
                }
                StatementKind::If(branch) => self.if_statement(branch, tokens),
                StatementKind::While(loop_statement) => {
                    self.statements(&loop_statement.body.statements, tokens)
                }
                StatementKind::For(loop_statement) => {
                    self.statements(&loop_statement.body.statements, tokens)
                }
                StatementKind::Match(match_statement) => {
                    for arm in &match_statement.arms {
                        self.statements(&arm.body.statements, tokens);
                    }
                }
                StatementKind::Try(handler) => {
                    self.statements(&handler.try_block.statements, tokens);
                    self.statements(&handler.catch_block.statements, tokens);
                }
                StatementKind::ModuleImport(_)
                | StatementKind::ModuleExport(_)
                | StatementKind::NominalType(_)
                | StatementKind::VariantType(_)
                | StatementKind::Declaration(_)
                | StatementKind::Assignment(_)
                | StatementKind::Environment(_)
                | StatementKind::Throw(_)
                | StatementKind::Control(_)
                | StatementKind::Job(_) => {}
            }
        }
    }

    fn if_statement(&mut self, branch: &IfStatement, tokens: &[Token]) {
        self.statements(&branch.then_block.statements, tokens);
        match &branch.else_branch {
            Some(ElseBranch::Block(block)) => self.statements(&block.statements, tokens),
            Some(ElseBranch::If(branch)) => self.if_statement(branch.kind(), tokens),
            None => {}
        }
    }

    fn declaration_body(&mut self, body: &Block) {
        self.newline_before.insert(body.span.start());
        self.newline_before.insert(body.span.end() - 1);
        self.newline_after.insert(body.span.start() + 1);
        self.newline_after.insert(body.span.end());
    }

    fn signature(&mut self, tokens: &[Token]) {
        let compact_length = tokens
            .iter()
            .filter(|token| !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline))
            .map(|token| token.span().len().saturating_add(1))
            .sum::<usize>();
        if compact_length <= 100
            && tokens.iter().all(|token| {
                !matches!(
                    token.kind(),
                    TokenKind::Comment
                        | TokenKind::DocumentationComment
                        | TokenKind::LineContinuation
                )
            })
        {
            let signature_end = tokens
                .iter()
                .rfind(|token| !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline))
                .map_or(0, |token| token.span().end());
            self.suppress_source_newline.extend(
                tokens
                    .iter()
                    .filter(|token| {
                        token.kind() == TokenKind::Newline && token.span().end() <= signature_end
                    })
                    .map(|token| token.span().start()),
            );
        }
        self.spacing(tokens, usize::MAX);
    }

    fn spacing(&mut self, tokens: &[Token], signature_end: usize) {
        for (index, token) in tokens.iter().enumerate() {
            match token.kind() {
                TokenKind::Operator(Operator::Arrow) => {
                    self.space_before.insert(token.span().start());
                    self.space_after.insert(token.span().end());
                }
                TokenKind::Operator(Operator::Colon)
                    if !tokens.get(index.wrapping_sub(1)).is_some_and(|token| {
                        token.kind() == TokenKind::Operator(Operator::Colon)
                    }) && !tokens.get(index + 1).is_some_and(|token| {
                        token.kind() == TokenKind::Operator(Operator::Colon)
                    }) =>
                {
                    self.no_space_before.insert(token.span().start());
                    self.space_after.insert(token.span().end());
                }
                TokenKind::Operator(Operator::Comma) => {
                    self.space_after.insert(token.span().end());
                }
                TokenKind::Delimiter(Delimiter::LeftParenthesis)
                    if token.span().end() <= signature_end =>
                {
                    self.no_space_after.insert(token.span().end());
                    if let Some(next) = tokens[index + 1..].iter().find(|next| {
                        !matches!(next.kind(), TokenKind::Whitespace | TokenKind::Newline)
                    }) {
                        self.no_space_before.insert(next.span().start());
                    }
                }
                TokenKind::Delimiter(Delimiter::RightParenthesis)
                    if token.span().end() <= signature_end =>
                {
                    self.no_space_before.insert(token.span().start());
                }
                _ => {}
            }
        }
    }
}

fn tokens_in(tokens: &[Token], start: usize, end: usize) -> &[Token] {
    let first = tokens.partition_point(|token| token.span().start() < start);
    let last = tokens.partition_point(|token| token.span().end() <= end);
    &tokens[first..last]
}

fn format_tokens(source: &SourceFile, tokens: &[Token], layout: &FormatLayout) -> String {
    let mut output = String::new();
    let mut open_delimiters = Vec::new();
    let mut indent = 0usize;
    let mut at_line_start = true;
    let mut pending_space = false;
    let mut suppress_source_newline = false;
    let mut previous_syntax_token_end = None;
    let mut pending_break = false;

    for (index, token) in tokens.iter().enumerate() {
        match token.kind() {
            TokenKind::Whitespace => {
                if !at_line_start
                    && !previous_syntax_token_end
                        .is_some_and(|end| layout.no_space_after.contains(&end))
                {
                    pending_space = true;
                }
            }
            TokenKind::Newline => {
                if layout
                    .suppress_source_newline
                    .contains(&token.span().start())
                {
                    if !at_line_start
                        && !previous_syntax_token_end
                            .is_some_and(|end| layout.no_space_after.contains(&end))
                    {
                        pending_space = true;
                    }
                    continue;
                }
                if !suppress_source_newline {
                    output.push('\n');
                }
                suppress_source_newline = false;
                at_line_start = true;
                pending_space = false;
            }
            TokenKind::LineContinuation => {
                write_prefix(&mut output, indent, &mut at_line_start, &mut pending_space);
                output.push_str(
                    token
                        .text(source)
                        .expect("lexer spans belong to their source"),
                );
                previous_syntax_token_end = Some(token.span().end());
                at_line_start = true;
                pending_space = false;
            }
            kind => {
                suppress_source_newline = false;
                if closes_indenting_brace(kind, token.span().start(), layout, &mut open_delimiters)
                {
                    indent = indent.saturating_sub(1);
                }

                if layout.newline_before.contains(&token.span().start()) && !at_line_start {
                    output.push('\n');
                    at_line_start = true;
                    pending_space = false;
                }
                if layout.no_space_before.contains(&token.span().start()) {
                    pending_space = false;
                } else if layout.space_before.contains(&token.span().start()) && !at_line_start {
                    pending_space = true;
                }
                write_prefix(&mut output, indent, &mut at_line_start, &mut pending_space);
                output.push_str(
                    token
                        .text(source)
                        .expect("lexer spans belong to their source"),
                );
                previous_syntax_token_end = Some(token.span().end());

                if let Some(open) = opening_delimiter(kind, token.span().start(), layout) {
                    open_delimiters.push(open);
                    if open == OpenDelimiter::IndentingBrace {
                        indent += 1;
                    }
                }
                if layout.space_after.contains(&token.span().end()) {
                    pending_space = true;
                }
                if layout.no_space_after.contains(&token.span().end()) {
                    pending_space = false;
                }
                pending_break |= layout.newline_after.contains(&token.span().end());
                let trailing_comment = tokens[index + 1..]
                    .iter()
                    .find(|next| next.kind() != TokenKind::Whitespace)
                    .is_some_and(|next| {
                        matches!(
                            next.kind(),
                            TokenKind::Comment | TokenKind::DocumentationComment
                        )
                    });
                if pending_break && !trailing_comment {
                    output.push('\n');
                    at_line_start = true;
                    pending_space = false;
                    suppress_source_newline = true;
                    pending_break = false;
                }
            }
        }
    }

    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn write_prefix(
    output: &mut String,
    indent: usize,
    at_line_start: &mut bool,
    pending_space: &mut bool,
) {
    if *at_line_start {
        for _ in 0..indent {
            output.push_str("    ");
        }
        *at_line_start = false;
    } else if *pending_space {
        output.push(' ');
    }
    *pending_space = false;
}

fn opening_delimiter(
    kind: TokenKind,
    start: usize,
    layout: &FormatLayout,
) -> Option<OpenDelimiter> {
    match kind {
        TokenKind::Delimiter(Delimiter::LeftBrace)
            if layout.interpolation_open.contains(&start) =>
        {
            Some(OpenDelimiter::InterpolationBrace)
        }
        TokenKind::Delimiter(Delimiter::LeftBrace) => Some(OpenDelimiter::IndentingBrace),
        TokenKind::InterpolationStart => Some(OpenDelimiter::InterpolationBrace),
        TokenKind::Delimiter(Delimiter::LeftParenthesis) => Some(OpenDelimiter::Parenthesis),
        TokenKind::Delimiter(Delimiter::LeftBracket) => Some(OpenDelimiter::Bracket),
        _ => None,
    }
}

fn closes_indenting_brace(
    kind: TokenKind,
    start: usize,
    layout: &FormatLayout,
    open: &mut Vec<OpenDelimiter>,
) -> bool {
    let expected = match kind {
        TokenKind::Delimiter(Delimiter::RightBrace)
            if layout.interpolation_close.contains(&start) =>
        {
            Some(OpenDelimiter::InterpolationBrace)
        }
        TokenKind::Delimiter(Delimiter::RightBrace) => Some(OpenDelimiter::IndentingBrace),
        TokenKind::Delimiter(Delimiter::RightParenthesis) => {
            pop_expected(open, OpenDelimiter::Parenthesis);
            return false;
        }
        TokenKind::Delimiter(Delimiter::RightBracket) => {
            pop_expected(open, OpenDelimiter::Bracket);
            return false;
        }
        _ => None,
    };

    let Some(expected) = expected else {
        return false;
    };
    match open.pop() {
        Some(actual) if actual == expected => expected == OpenDelimiter::IndentingBrace,
        Some(actual) => {
            open.push(actual);
            false
        }
        None => false,
    }
}

fn pop_expected(open: &mut Vec<OpenDelimiter>, expected: OpenDelimiter) {
    match open.pop() {
        Some(actual) if actual == expected => {}
        Some(actual) => {
            open.push(actual);
        }
        None => {}
    }
}
