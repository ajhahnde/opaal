//! Editor-neutral, parser-aware completion over immutable candidate snapshots.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;

use flashshell_runtime::ScopeStack;
use flashshell_runtime::command::CommandRegistry;
use flashshell_syntax::{
    Delimiter, Operator, ParseOutcome, SourceFile, SourceId, Token, TokenKind, lex, parse,
};

/// The semantic source of one completion candidate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CompletionKind {
    /// A command registered inside FlashShell.
    InternalCommand,
    /// A named callable visible in the lexical scope.
    Function,
    /// An executable supplied by a host snapshot.
    ExternalCommand,
    /// A visible lexical binding.
    Variable,
    /// A flag advertised by an internal command signature.
    Flag,
    /// A UTF-8 path spelling supplied by a host snapshot.
    Path,
}

/// One editor-neutral replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    value: String,
    replacement: Range<usize>,
    kind: CompletionKind,
    append_whitespace: bool,
}

impl Completion {
    /// The exact text to insert.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The half-open UTF-8 byte range replaced in the edit buffer.
    #[must_use]
    pub fn replacement(&self) -> Range<usize> {
        self.replacement.clone()
    }

    /// The source category used for deterministic ordering and presentation.
    #[must_use]
    pub const fn kind(&self) -> CompletionKind {
        self.kind
    }

    /// Whether the editor should add a separating space after insertion.
    #[must_use]
    pub const fn append_whitespace(&self) -> bool {
        self.append_whitespace
    }
}

/// Immutable candidates used by [`CompletionEngine`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionCatalog {
    internal: BTreeMap<String, BTreeSet<String>>,
    functions: BTreeSet<String>,
    variables: BTreeSet<String>,
    external: BTreeSet<String>,
    paths: BTreeSet<String>,
}

impl CompletionCatalog {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshots authoritative runtime registry and lexical-scope names.
    #[must_use]
    pub fn from_runtime(registry: &CommandRegistry, scope: &ScopeStack) -> Self {
        let internal = registry
            .names()
            .map(|name| {
                let flags = registry
                    .lookup(name)
                    .expect("registry names always have signatures")
                    .flags()
                    .map(str::to_owned)
                    .collect();
                (name.to_owned(), flags)
            })
            .collect();
        let mut functions = BTreeSet::new();
        let mut variables = BTreeSet::new();
        for (name, value) in scope.visible_bindings() {
            variables.insert(name.to_owned());
            if matches!(value, flashshell_runtime::Value::Callable(callable) if callable.family() == "function")
            {
                functions.insert(name.to_owned());
            }
        }
        Self {
            internal,
            functions,
            variables,
            external: BTreeSet::new(),
            paths: BTreeSet::new(),
        }
    }

    /// Replaces the external-command snapshot.
    #[must_use]
    pub fn with_external_commands(
        mut self,
        commands: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.external = commands.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces the UTF-8 path snapshot.
    #[must_use]
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }
}

/// Pure completion over a fixed catalog.
#[derive(Clone, Debug, Default)]
pub struct CompletionEngine {
    catalog: CompletionCatalog,
}

impl CompletionEngine {
    /// Builds an engine over one immutable candidate snapshot.
    #[must_use]
    pub const fn new(catalog: CompletionCatalog) -> Self {
        Self { catalog }
    }

    /// Completes the source at one UTF-8 byte cursor.
    #[must_use]
    pub fn complete(&self, source: &str, cursor: usize) -> Vec<Completion> {
        if cursor > source.len() || !source.is_char_boundary(cursor) {
            return Vec::new();
        }

        let source_file = SourceFile::new(SourceId::new(0), "<interactive>", source);
        if matches!(parse(&source_file), ParseOutcome::Invalid(_)) {
            return Vec::new();
        }
        let tokens = lex(&source_file);
        let Some(active) = ActiveWord::at(source, &tokens, cursor) else {
            return Vec::new();
        };
        if active.quoted {
            return Vec::new();
        }

        let prior = significant_before(&tokens, active.range.start);
        let stage = current_stage(&prior);
        let context = classify_context(source, stage, &active);
        self.candidates(context, active)
    }

    fn candidates(&self, context: Context<'_>, active: ActiveWord<'_>) -> Vec<Completion> {
        let mut completions = Vec::new();
        let mut seen = HashSet::new();
        let mut add = |values: &BTreeSet<String>, kind, prefix: &str, decorate: bool| {
            for value in values.iter().filter(|value| value.starts_with(prefix)) {
                let replacement_value = if decorate {
                    format!("${value}")
                } else {
                    value.clone()
                };
                if seen.insert(replacement_value.clone()) {
                    completions.push(Completion {
                        value: replacement_value,
                        replacement: active.range.clone(),
                        kind,
                        append_whitespace: !matches!(
                            kind,
                            CompletionKind::Variable | CompletionKind::Path
                        ),
                    });
                }
            }
        };

        match context {
            Context::Command { forced_external } => {
                if !forced_external {
                    let names = self.catalog.internal.keys().cloned().collect();
                    add(&names, CompletionKind::InternalCommand, active.text, false);
                    add(
                        &self.catalog.functions,
                        CompletionKind::Function,
                        active.text,
                        false,
                    );
                }
                add(
                    &self.catalog.external,
                    CompletionKind::ExternalCommand,
                    active.text,
                    false,
                );
            }
            Context::Variable => add(
                &self.catalog.variables,
                CompletionKind::Variable,
                active.text.strip_prefix('$').unwrap_or(active.text),
                true,
            ),
            Context::Flag { command } => {
                if let Some(flags) = self.catalog.internal.get(command) {
                    add(flags, CompletionKind::Flag, active.text, false);
                }
            }
            Context::Path => add(
                &self.catalog.paths,
                CompletionKind::Path,
                active.text,
                false,
            ),
            Context::None => {}
        }
        completions
    }
}

#[derive(Clone, Debug)]
struct ActiveWord<'source> {
    range: Range<usize>,
    text: &'source str,
    quoted: bool,
}

impl<'source> ActiveWord<'source> {
    fn at(source: &'source str, tokens: &[Token], cursor: usize) -> Option<Self> {
        let mut containing = tokens.iter().position(|token| {
            token.span().start() <= cursor
                && cursor <= token.span().end()
                && is_word_component(token.kind())
        });

        if containing.is_none() {
            let occupied = tokens.iter().any(|token| {
                token.span().start() < cursor
                    && cursor < token.span().end()
                    && !matches!(token.kind(), TokenKind::Whitespace | TokenKind::Newline)
            });
            return (!occupied).then_some(Self {
                range: cursor..cursor,
                text: "",
                quoted: false,
            });
        }

        let index = containing.take().expect("checked above");
        if tokens[index].kind() == TokenKind::Variable {
            let prefix = tokens[index].span().start()..cursor;
            return Some(Self {
                text: &source[prefix],
                range: tokens[index].span().start()..tokens[index].span().end(),
                quoted: false,
            });
        }

        let mut first = index;
        while first > 0
            && tokens[first - 1].span().end() == tokens[first].span().start()
            && is_word_component(tokens[first - 1].kind())
        {
            first -= 1;
        }
        let mut last = index;
        while last + 1 < tokens.len()
            && tokens[last].span().end() == tokens[last + 1].span().start()
            && is_word_component(tokens[last + 1].kind())
        {
            last += 1;
        }
        let start = tokens[first].span().start();
        let end = tokens[last].span().end();
        Some(Self {
            text: &source[start..cursor],
            range: start..end,
            quoted: (first..=last).any(|token| is_quoted(tokens[token].kind())),
        })
    }
}

enum Context<'source> {
    Command { forced_external: bool },
    Variable,
    Flag { command: &'source str },
    Path,
    None,
}

fn classify_context<'source>(
    source: &'source str,
    stage: &[&Token],
    active: &ActiveWord<'_>,
) -> Context<'source> {
    if active.text.starts_with('$') {
        return Context::Variable;
    }
    if stage
        .last()
        .is_some_and(|token| is_file_redirect(token.kind()))
    {
        return Context::Path;
    }

    let forced_external = stage.first().is_some_and(|token| {
        token.kind() == TokenKind::Operator(Operator::Caret)
            && token.span().end() == active.range.start
    });
    let head_tokens = if forced_external { &stage[1..] } else { stage };
    let Some(head_end) = first_word_end(head_tokens) else {
        return Context::Command { forced_external };
    };
    if active.range.start < head_end {
        return Context::Command { forced_external };
    }

    let head_start = head_tokens[0].span().start();
    let command = &source[head_start..head_end];
    if active.text.starts_with('-') {
        return Context::Flag { command };
    }
    if active.text.contains('/') || active.text.starts_with('.') || active.text.starts_with('~') {
        Context::Path
    } else {
        Context::None
    }
}

fn significant_before(tokens: &[Token], offset: usize) -> Vec<&Token> {
    tokens
        .iter()
        .filter(|token| {
            token.span().end() <= offset
                && !matches!(
                    token.kind(),
                    TokenKind::Whitespace | TokenKind::Comment | TokenKind::LineContinuation
                )
        })
        .collect()
}

fn current_stage<'tokens>(tokens: &'tokens [&Token]) -> &'tokens [&'tokens Token] {
    let start = tokens
        .iter()
        .rposition(|token| is_command_boundary(token.kind()))
        .map_or(0, |position| position + 1);
    &tokens[start..]
}

fn first_word_end(tokens: &[&Token]) -> Option<usize> {
    let first = tokens.first()?;
    if !is_word_component(first.kind()) {
        return None;
    }
    let mut end = first.span().end();
    for token in &tokens[1..] {
        if token.span().start() != end || !is_word_component(token.kind()) {
            break;
        }
        end = token.span().end();
    }
    Some(end)
}

fn is_command_boundary(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Newline
            | TokenKind::Operator(
                Operator::Semicolon
                    | Operator::Pipe
                    | Operator::PipeBoth
                    | Operator::And
                    | Operator::Or
                    | Operator::Background
            )
            | TokenKind::Delimiter(Delimiter::LeftBrace | Delimiter::RightBrace)
    )
}

fn is_file_redirect(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Operator(Operator::Less | Operator::Greater | Operator::Append)
    )
}

fn is_quoted(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::SingleQuoted
            | TokenKind::DoubleQuoteStart
            | TokenKind::DoubleText
            | TokenKind::DoubleEscape
            | TokenKind::DoubleQuoteEnd
    )
}

fn is_word_component(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Keyword(_)
            | TokenKind::Number(_)
            | TokenKind::WordText
            | TokenKind::BareEscape
            | TokenKind::SingleQuoted
            | TokenKind::DoubleQuoteStart
            | TokenKind::DoubleText
            | TokenKind::DoubleEscape
            | TokenKind::DoubleQuoteEnd
            | TokenKind::Variable
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
            )
    )
}
