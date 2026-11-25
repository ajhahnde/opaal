use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Block, Delimiter, Diagnostic, ElseBranch, IfStatement, IncompleteInput, Operator, ParseOutcome,
    Script, SourceFile, Statement, StatementKind, Token, TokenKind, lex_opaal,
    parser::{FormatSpans, parse_opaal_for_formatting},
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
    let (outcome, spans) = parse_opaal_for_formatting(source);
    match outcome {
        ParseOutcome::Complete(script) => {
            let tokens = lex_opaal(source);
            let layout = FormatLayout::for_script(&script, &tokens, &spans);
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
    DoubleQuote,
}

#[derive(Default)]
struct FormatLayout {
    comma_after: BTreeSet<usize>,
    omitted_commas: BTreeSet<usize>,
    newline_before: BTreeSet<usize>,
    newline_after: BTreeSet<usize>,
    space_before: BTreeSet<usize>,
    space_after: BTreeSet<usize>,
    no_space_before: BTreeSet<usize>,
    no_space_after: BTreeSet<usize>,
    interpolation_open: BTreeSet<usize>,
    interpolation_close: BTreeSet<usize>,
    suppress_source_newline: BTreeSet<usize>,
    if_starts: BTreeSet<usize>,
    if_arm_opens: BTreeMap<usize, usize>,
    if_arm_closes: BTreeMap<usize, (usize, bool)>,
}

impl FormatLayout {
    fn for_script(script: &Script, tokens: &[Token], spans: &FormatSpans) -> Self {
        let mut layout = Self::default();
        for comma in &spans.spaced_commas {
            layout.no_space_before.insert(comma.start());
            layout.space_after.insert(comma.end());
        }
        for keyword in &spans.comprehension_keywords {
            layout.space_before.insert(keyword.start());
            layout.space_after.insert(keyword.end());
        }
        for &(open, separator, close) in &spans.slice_brackets {
            layout
                .no_space_before
                .extend([open.start(), separator.start(), close.start()]);
            layout.no_space_after.extend([open.end(), separator.end()]);
            let after_separator =
                tokens.partition_point(|token| token.span().start() < separator.end());
            if let Some(comment) = tokens[after_separator..]
                .iter()
                .find(|token| token.kind() != TokenKind::Whitespace)
                .filter(|token| {
                    matches!(
                        token.kind(),
                        TokenKind::Comment | TokenKind::DocumentationComment
                    )
                })
            {
                // `#` needs whitespace to start a comment after `:`.
                layout.space_before.insert(comment.span().start());
            }
        }
        layout
            .omitted_commas
            .extend(spans.tuple_trailing_commas.iter().map(|span| span.start()));
        for span in &spans.interpolations {
            layout.interpolation_open.insert(span.start());
            layout.interpolation_close.insert(span.end() - 1);
            let open_end = span.start() + 1;
            if tokens_in(tokens, open_end, span.end())
                .iter()
                .find(|token| token.kind() != TokenKind::Whitespace)
                .is_some_and(|token| token.kind() == TokenKind::Delimiter(Delimiter::LeftBrace))
            {
                // Adjacent opening braces would spell a literal brace.
                layout.space_after.insert(open_end);
            } else {
                layout.no_space_after.insert(open_end);
            }
            layout.no_space_before.insert(span.end() - 1);
        }
        for arm in &spans.if_arms {
            let conditional = arm.conditional.start();
            layout.if_starts.insert(conditional);
            layout.if_arm_opens.insert(arm.open.start(), conditional);
            layout
                .if_arm_closes
                .insert(arm.close.start(), (conditional, arm.next_else.is_none()));
            layout.space_before.insert(arm.open.start());
            layout.newline_after.insert(arm.open.end());
            layout.newline_before.insert(arm.close.start());
            let before_open =
                tokens.partition_point(|token| token.span().start() < arm.open.start());
            if let Some(header_end) = tokens[..before_open].iter().rfind(|token| {
                !matches!(
                    token.kind(),
                    TokenKind::Whitespace
                        | TokenKind::Newline
                        | TokenKind::Comment
                        | TokenKind::DocumentationComment
                        | TokenKind::LineContinuation
                )
            }) {
                layout.join_plain_newline(tokens_in(
                    tokens,
                    header_end.span().end(),
                    arm.open.start(),
                ));
            }
            if let Some(next_else) = arm.next_else {
                layout.space_before.insert(next_else.start());
                layout.join_plain_newline(tokens_in(tokens, arm.close.end(), next_else.start()));
            }
        }
        for &start in &layout.if_starts {
            let index = tokens.partition_point(|token| token.span().start() < start);
            if let Some(assign) = tokens[..index]
                .iter()
                .rfind(|token| token.kind() != TokenKind::Whitespace)
                && assign.kind() == TokenKind::Operator(Operator::Assign)
            {
                layout.space_before.insert(assign.span().start());
                layout.space_after.insert(assign.span().end());
            }
        }
        for token in tokens {
            if token.kind() == TokenKind::Operator(Operator::Spread) {
                layout.no_space_after.insert(token.span().end());
            }
        }
        layout.statements(script.statements(), tokens);
        layout
    }

    fn join_plain_newline(&mut self, gap: &[Token]) {
        let mut significant = gap
            .iter()
            .filter(|token| token.kind() != TokenKind::Whitespace);
        if let Some(newline) = significant.next()
            && newline.kind() == TokenKind::Newline
            && significant.next().is_none()
        {
            self.suppress_source_newline.insert(newline.span().start());
        }
    }

    fn statements(&mut self, statements: &[Statement], tokens: &[Token]) {
        for statement in statements {
            for import in statement.kind().module_imports() {
                if let Some(members) = import.members() {
                    self.member_import(members, tokens);
                }
            }
            match statement.kind() {
                StatementKind::ModuleExport(export) => {
                    for token in tokens_in(tokens, statement.span().start(), statement.span().end())
                    {
                        if token.kind() == TokenKind::Operator(Operator::Comma) {
                            self.no_space_before.insert(token.span().start());
                        }
                    }
                    for selector in &export.names {
                        if let crate::ModuleExportSelector::Prefix { marker, .. } = selector {
                            self.no_space_before.insert(marker.start());
                        }
                    }
                }
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
                        action.body.span.start(),
                    ));
                    self.newline_before.insert(action.effects_span.start());
                    self.newline_before.insert(action.effects_span.end() - 1);
                    self.newline_after.insert(action.effects_span.end());
                    self.declaration_body(&action.body);
                    let effect_tokens = tokens_in(
                        tokens,
                        action.effects_span.start(),
                        action.effects_span.end(),
                    );
                    let mut remaining = effect_tokens.iter();
                    for request in &action.effects {
                        let end = request.span.end();
                        let next = remaining
                            .by_ref()
                            .find(|token| {
                                token.span().start() >= end
                                    && !matches!(
                                        token.kind(),
                                        TokenKind::Whitespace
                                            | TokenKind::Newline
                                            | TokenKind::Comment
                                            | TokenKind::DocumentationComment
                                            | TokenKind::LineContinuation
                                    )
                            })
                            .unwrap();
                        if next.kind() == TokenKind::Operator(Operator::Comma) {
                            self.no_space_before.insert(next.span().start());
                            self.newline_after.insert(next.span().end());
                        } else {
                            self.comma_after.insert(end);
                            self.newline_after.insert(end);
                        }
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
                StatementKind::ModuleImportGroup(entries) => {
                    let group_tokens =
                        tokens_in(tokens, statement.span().start(), statement.span().end());
                    let open = group_tokens
                        .iter()
                        .find(|token| token.kind() == TokenKind::Delimiter(Delimiter::LeftBrace))
                        .unwrap();
                    self.space_before.insert(open.span().start());
                    self.newline_after.insert(open.span().end());
                    self.newline_before.insert(statement.span().end() - 1);
                    let mut remaining = group_tokens.iter();
                    for entry in entries {
                        self.newline_before.insert(entry.source().span().start());
                        let end = entry.end();
                        let next = remaining
                            .by_ref()
                            .find(|token| {
                                token.span().start() >= end
                                    && !matches!(
                                        token.kind(),
                                        TokenKind::Whitespace
                                            | TokenKind::Newline
                                            | TokenKind::Comment
                                            | TokenKind::DocumentationComment
                                            | TokenKind::LineContinuation
                                    )
                            })
                            .unwrap();
                        if next.kind() == TokenKind::Operator(Operator::Comma) {
                            self.no_space_before.insert(next.span().start());
                            self.newline_after.insert(next.span().end());
                        } else {
                            self.comma_after.insert(end);
                            self.newline_after.insert(end);
                        }
                    }
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
                StatementKind::IndexedAssignment(assignment) => {
                    self.no_space_before.insert(assignment.brackets.start());
                    self.space_before.insert(assignment.operator.start());
                    self.space_after.insert(assignment.operator.end());
                }
                StatementKind::Assignment(assignment) => {
                    if let crate::ExpressionKind::Binary(binary) = assignment.value.kind() {
                        for token in tokens_in(
                            tokens,
                            binary.operator.span().start(),
                            binary.operator.span().end(),
                        ) {
                            if matches!(
                                token.kind(),
                                TokenKind::Operator(
                                    Operator::AddAssign
                                        | Operator::SubtractAssign
                                        | Operator::MultiplyAssign
                                        | Operator::DivideAssign
                                        | Operator::RemainderAssign
                                )
                            ) {
                                self.space_before.insert(token.span().start());
                                self.space_after.insert(token.span().end());
                            }
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
                | StatementKind::ModuleMemberImport(_)
                | StatementKind::NominalType(_)
                | StatementKind::VariantType(_)
                | StatementKind::Declaration(_)
                | StatementKind::Environment(_)
                | StatementKind::Throw(_)
                | StatementKind::Control(_)
                | StatementKind::Job(_) => {}
            }
        }
    }

    fn member_import(&mut self, import: &crate::ModuleMemberImport, tokens: &[Token]) {
        let list = tokens_in(tokens, import.span.start(), import.span.end());
        let multiline = list.iter().any(|token| {
            matches!(
                token.kind(),
                TokenKind::Newline | TokenKind::Comment | TokenKind::DocumentationComment
            )
        });
        self.no_space_before.insert(import.span.start());
        for token in tokens_in(tokens, import.source.span().end(), import.span.start()) {
            self.no_space_before.insert(token.span().start());
            self.no_space_after.insert(token.span().end());
        }
        if multiline {
            self.newline_after.insert(import.span.start() + 1);
            self.newline_before.insert(import.span.end() - 1);
            for member in &import.members {
                self.newline_before.insert(member.member.span().start());
                let next = list
                    .iter()
                    .find(|token| {
                        token.span().start() >= member.span.end()
                            && !matches!(
                                token.kind(),
                                TokenKind::Whitespace
                                    | TokenKind::Newline
                                    | TokenKind::Comment
                                    | TokenKind::DocumentationComment
                                    | TokenKind::LineContinuation
                            )
                    })
                    .unwrap();
                if next.kind() == TokenKind::Operator(Operator::Comma) {
                    self.no_space_before.insert(next.span().start());
                    self.newline_after.insert(next.span().end());
                } else {
                    self.comma_after.insert(member.span.end());
                    self.newline_after.insert(member.span.end());
                }
            }
        } else {
            self.no_space_after.insert(import.span.start() + 1);
            self.no_space_before.insert(import.span.end() - 1);
            for token in list {
                if token.kind() == TokenKind::Operator(Operator::Comma) {
                    self.no_space_before.insert(token.span().start());
                    self.space_after.insert(token.span().end());
                }
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
    let mut line_indent = 0usize;
    let mut if_indents: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
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
                at_line_start = open_delimiters.last() != Some(&OpenDelimiter::DoubleQuote);
                pending_space = false;
                if at_line_start {
                    // A continuation leaves `#` inside a word unless whitespace intervenes.
                    match &tokens[index + 1..] {
                        [next, ..]
                            if next.kind() == TokenKind::WordText
                                && next.text(source).is_ok_and(|text| text.starts_with('#')) =>
                        {
                            at_line_start = false;
                        }
                        [space, comment, ..]
                            if indent == 0
                                && space.kind() == TokenKind::Whitespace
                                && matches!(
                                    comment.kind(),
                                    TokenKind::Comment | TokenKind::DocumentationComment
                                ) =>
                        {
                            at_line_start = false;
                            pending_space = true;
                        }
                        _ => {}
                    }
                }
            }
            kind => {
                suppress_source_newline = false;
                let start = token.span().start();
                if layout.omitted_commas.contains(&start) {
                    pending_space = false;
                    continue;
                }
                let if_close = layout.if_arm_closes.get(&start);
                if closes_indenting_brace(kind, start, layout, &mut open_delimiters) {
                    indent = indent.saturating_sub(1);
                }
                if let Some((conditional, _)) = if_close {
                    indent = if_indents[conditional].0;
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
                if at_line_start {
                    line_indent = indent;
                }
                if layout.if_starts.contains(&start) {
                    if_indents.insert(start, (line_indent, indent));
                }
                write_prefix(&mut output, indent, &mut at_line_start, &mut pending_space);
                output.push_str(
                    token
                        .text(source)
                        .expect("lexer spans belong to their source"),
                );
                if layout.comma_after.contains(&token.span().end()) {
                    output.push(',');
                }
                previous_syntax_token_end = Some(token.span().end());

                if let Some(open) = opening_delimiter(kind, token.span().start(), layout) {
                    open_delimiters.push(open);
                    if open == OpenDelimiter::IndentingBrace {
                        indent += 1;
                    }
                }
                if let Some(conditional) = layout.if_arm_opens.get(&start) {
                    indent = if_indents[conditional].0 + 1;
                }
                if let Some((conditional, true)) = if_close {
                    indent = if_indents.remove(conditional).unwrap().1;
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

    let ends_with_continuation = tokens
        .iter()
        .rfind(|token| token.kind() != TokenKind::Whitespace)
        .is_some_and(|token| token.kind() == TokenKind::LineContinuation);
    if !output.is_empty() && (!output.ends_with('\n') || ends_with_continuation) {
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
        TokenKind::DoubleQuoteStart => Some(OpenDelimiter::DoubleQuote),
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
        TokenKind::DoubleQuoteEnd => {
            pop_expected(open, OpenDelimiter::DoubleQuote);
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
