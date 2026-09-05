use std::collections::BTreeSet;

use flash_syntax::{
    Block, CommandCaptureKind, CommandHeadKind, CommandItemKind, ConditionalChain, ElseBranch,
    Expression, ExpressionKind, Pattern, RedirectionKind, Script, Span, StageKind, Statement,
    StatementKind, Word, WordPart, WordPartKind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReferenceKind {
    Name,
    ExpressionValue,
    WordValue,
    Spread,
    Assignment,
    Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Reference {
    pub(crate) name: String,
    pub(crate) name_span: Span,
    pub(crate) full_span: Span,
    pub(crate) kind: ReferenceKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandUse {
    pub(crate) name: Option<String>,
    pub(crate) span: Span,
    pub(crate) forced_external: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReservedUse {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) spelling: String,
    pub(crate) replacement: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SourceScan {
    pub(crate) bindings: Vec<(String, Span)>,
    pub(crate) top_level_bindings: BTreeSet<String>,
    pub(crate) references: Vec<Reference>,
    callable_ranges: Vec<(String, usize, usize)>,
    pub(crate) exports: BTreeSet<String>,
    pub(crate) commands: Vec<CommandUse>,
    pub(crate) intrinsic_calls: Vec<(String, Span)>,
    pub(crate) effect_spans: Vec<Span>,
    pub(crate) identifiers: BTreeSet<String>,
    pub(crate) reserved_uses: Vec<ReservedUse>,
    shadow_ranges: Vec<(String, usize, usize)>,
}

impl SourceScan {
    pub(crate) fn reference_is_shadowed(&self, name: &str, span: Span) -> bool {
        self.shadow_ranges.iter().any(|(shadowed, start, end)| {
            shadowed == name && *start <= span.start() && span.end() <= *end
        })
    }

    pub(crate) fn command_is_callable(&self, name: &str, span: Span) -> bool {
        self.callable_ranges.iter().any(|(callable, start, end)| {
            callable == name && *start <= span.start() && span.end() <= *end
        })
    }
}

pub(crate) fn scan(source: &flash_syntax::SourceFile, script: &Script) -> SourceScan {
    let mut scan = SourceScan::default();
    for statement in script.statements() {
        let mut names = Vec::new();
        direct_bindings(source, statement, &mut names);
        scan.top_level_bindings.extend(names);
    }
    scan_scoped_statements(source, script.statements(), source.len(), &mut scan);
    scan
}

fn scan_scoped_statements(
    source: &flash_syntax::SourceFile,
    statements: &[Statement],
    scope_end: usize,
    scan: &mut SourceScan,
) {
    for statement in statements {
        scan_statement(source, statement, scan);
        let mut names = Vec::new();
        direct_bindings(source, statement, &mut names);
        for name in names {
            scan.shadow_ranges
                .push((name, statement.span().end(), scope_end));
        }
        if let StatementKind::Function(function) = statement.kind() {
            scan.callable_ranges.push((
                text(source, function.name.span()),
                statement.span().end(),
                scope_end,
            ));
        }
    }
}

fn text(source: &flash_syntax::SourceFile, span: Span) -> String {
    source
        .slice(span)
        .expect("parsed spans belong to their source")
        .to_owned()
}

fn binding(source: &flash_syntax::SourceFile, span: Span, scan: &mut SourceScan) {
    let name = text(source, span);
    scan.identifiers.insert(name.clone());
    scan.bindings.push((name, span));
}

fn reference(
    source: &flash_syntax::SourceFile,
    name_span: Span,
    full_span: Span,
    kind: ReferenceKind,
    scan: &mut SourceScan,
) {
    let name = text(source, name_span);
    scan.identifiers.insert(name.clone());
    scan.references.push(Reference {
        name,
        name_span,
        full_span,
        kind,
    });
}

fn scan_statement(source: &flash_syntax::SourceFile, statement: &Statement, scan: &mut SourceScan) {
    match statement.kind() {
        StatementKind::Import(import) => {
            for name in &import.names {
                scan.identifiers.insert(text(source, name.span()));
            }
        }
        StatementKind::ModuleImport(import) => {
            binding(source, import.alias.span(), scan);
        }
        StatementKind::ModuleExport(export) => {
            for name in &export.names {
                scan.exports.insert(text(source, name.span()));
            }
        }
        StatementKind::NominalType(declaration) => {
            binding(source, declaration.name.span(), scan);
        }
        StatementKind::VariantType(declaration) => {
            binding(source, declaration.name.span(), scan);
        }
        StatementKind::Declaration(declaration) => {
            scan_pattern(source, &declaration.pattern, scan);
            scan_expression(source, &declaration.value, scan);
        }
        StatementKind::Assignment(assignment) => {
            reference(
                source,
                assignment.target.name.span(),
                assignment.target.span,
                ReferenceKind::Assignment,
                scan,
            );
            scan_expression(source, &assignment.value, scan);
        }
        StatementKind::Environment(environment) => {
            scan.effect_spans.push(statement.span());
            match environment {
                flash_syntax::EnvironmentStatement::Export { value, .. } => {
                    scan_expression(source, value, scan);
                }
                flash_syntax::EnvironmentStatement::Unset { .. } => {}
            }
        }
        StatementKind::Function(function) => {
            let name = text(source, function.name.span());
            scan.identifiers.insert(name.clone());
            scan.bindings.push((name, function.name.span()));
            scan.callable_ranges.push((
                text(source, function.name.span()),
                function.body.span.start(),
                function.body.span.end(),
            ));
            add_shadow(
                source,
                function.name.span(),
                function.body.span.start(),
                function.body.span.end(),
                scan,
            );
            for parameter in &function.parameters {
                scan_pattern(source, &parameter.pattern, scan);
                add_pattern_shadow(
                    source,
                    &parameter.pattern,
                    function.body.span.start(),
                    function.body.span.end(),
                    scan,
                );
            }
            scan_block(source, &function.body, scan);
        }
        StatementKind::If(statement) => {
            scan_chain(source, &statement.condition, scan);
            scan_block(source, &statement.then_block, scan);
            if let Some(branch) = &statement.else_branch {
                match branch {
                    ElseBranch::Block(block) => scan_block(source, block, scan),
                    ElseBranch::If(statement) => scan_if(source, statement.kind(), scan),
                }
            }
        }
        StatementKind::While(statement) => {
            scan_chain(source, &statement.condition, scan);
            scan_block(source, &statement.body, scan);
        }
        StatementKind::For(statement) => {
            binding(source, statement.binding.span(), scan);
            add_shadow(
                source,
                statement.binding.span(),
                statement.body.span.start(),
                statement.body.span.end(),
                scan,
            );
            scan_expression(source, &statement.iterable, scan);
            scan_block(source, &statement.body, scan);
        }
        StatementKind::Match(statement) => {
            scan_expression(source, &statement.value, scan);
            for arm in &statement.arms {
                scan_pattern(source, &arm.pattern, scan);
                add_pattern_shadow(source, &arm.pattern, arm.span.start(), arm.span.end(), scan);
                if let Some(guard) = &arm.guard {
                    scan_expression(source, guard, scan);
                }
                scan_block(source, &arm.body, scan);
            }
        }
        StatementKind::Try(statement) => {
            scan_block(source, &statement.try_block, scan);
            binding(source, statement.catch_binding.span(), scan);
            add_shadow(
                source,
                statement.catch_binding.span(),
                statement.catch_block.span.start(),
                statement.catch_block.span.end(),
                scan,
            );
            scan_block(source, &statement.catch_block, scan);
        }
        StatementKind::Throw(expression) => scan_expression(source, expression, scan),
        StatementKind::Control(flash_syntax::ControlTransfer::Return(Some(expression))) => {
            scan_expression(source, expression, scan);
        }
        StatementKind::Control(_) => {}
        StatementKind::Job(job) => {
            if let Some(span) = job.background_span {
                scan.effect_spans.push(span);
            }
            scan_chain(source, &job.chain, scan);
        }
    }
}

fn scan_if(
    source: &flash_syntax::SourceFile,
    statement: &flash_syntax::IfStatement,
    scan: &mut SourceScan,
) {
    scan_chain(source, &statement.condition, scan);
    scan_block(source, &statement.then_block, scan);
    if let Some(branch) = &statement.else_branch {
        match branch {
            ElseBranch::Block(block) => scan_block(source, block, scan),
            ElseBranch::If(statement) => scan_if(source, statement.kind(), scan),
        }
    }
}

fn scan_block(source: &flash_syntax::SourceFile, block: &Block, scan: &mut SourceScan) {
    scan_scoped_statements(source, &block.statements, block.span.end(), scan);
}

fn scan_pattern(source: &flash_syntax::SourceFile, pattern: &Pattern, scan: &mut SourceScan) {
    match pattern {
        Pattern::Binding(identifier) => binding(source, identifier.span(), scan),
        Pattern::List(pattern) => {
            for element in &pattern.elements {
                scan_pattern(source, element, scan);
            }
            if let Some(rest) = pattern.rest {
                binding(source, rest.span(), scan);
            }
        }
        Pattern::NominalRecord(pattern) => {
            for field in &pattern.fields {
                scan_pattern(source, &field.pattern, scan);
            }
        }
        Pattern::Variant(pattern) => {
            for item in &pattern.payload {
                scan_pattern(source, item, scan);
            }
        }
        Pattern::Wildcard(_) | Pattern::Literal(_) => {}
    }
}

fn scan_expression(
    source: &flash_syntax::SourceFile,
    expression: &Expression,
    scan: &mut SourceScan,
) {
    match expression.kind() {
        ExpressionKind::Variable(variable) => reference(
            source,
            variable.name.span(),
            variable.span,
            ReferenceKind::ExpressionValue,
            scan,
        ),
        ExpressionKind::Symbol(identifier) => reference(
            source,
            identifier.span(),
            identifier.span(),
            ReferenceKind::Name,
            scan,
        ),
        ExpressionKind::Qualified(_) | ExpressionKind::Literal(_) => {}
        ExpressionKind::List(items) => {
            for item in items {
                scan_expression(source, item, scan);
            }
        }
        ExpressionKind::Record(entries) => {
            for entry in entries {
                if let flash_syntax::RecordKey::Identifier(identifier) = &entry.key {
                    add_quoted_reserved_use(source, identifier.span(), scan);
                }
                scan_expression(source, &entry.value, scan);
            }
        }
        ExpressionKind::NominalRecord(record) => {
            for field in &record.fields {
                scan_expression(source, &field.value, scan);
            }
        }
        ExpressionKind::Closure(closure) => {
            for parameter in &closure.parameters {
                scan_pattern(source, &parameter.pattern, scan);
                add_pattern_shadow(
                    source,
                    &parameter.pattern,
                    closure.body.span().start(),
                    closure.body.span().end(),
                    scan,
                );
            }
            scan_chain(source, &closure.body, scan);
        }
        ExpressionKind::CommandSubstitution(substitution) => {
            let span = substitution.chain().span();
            scan.effect_spans.push(match substitution.capture() {
                CommandCaptureKind::Text | CommandCaptureKind::Bytes => span,
            });
            scan_chain(source, substitution.chain(), scan);
        }
        ExpressionKind::GroupedJob(chain) => scan_chain(source, chain, scan),
        ExpressionKind::Call(call) => {
            if let ExpressionKind::Symbol(identifier) = call.callee.kind() {
                let name = text(source, identifier.span());
                if matches!(name.as_str(), "env" | "glob") {
                    scan.intrinsic_calls.push((name, expression.span()));
                }
            }
            scan_expression(source, &call.callee, scan);
            for argument in &call.arguments {
                scan_expression(source, argument, scan);
            }
        }
        ExpressionKind::Index(index) => {
            scan_expression(source, &index.target, scan);
            scan_expression(source, &index.index, scan);
        }
        ExpressionKind::Member(member) => {
            let spelling = text(source, member.member.span());
            if is_new_reserved(&spelling) {
                scan.reserved_uses.push(ReservedUse {
                    start: member.member.span().start() - 1,
                    end: member.member.span().end(),
                    replacement: format!("['{spelling}']"),
                    spelling,
                });
            }
            scan_expression(source, &member.target, scan);
        }
        ExpressionKind::Unary(unary) => scan_expression(source, &unary.operand, scan),
        ExpressionKind::Binary(binary) => {
            scan_expression(source, &binary.left, scan);
            scan_expression(source, &binary.right, scan);
        }
    }
}

fn scan_chain(source: &flash_syntax::SourceFile, chain: &ConditionalChain, scan: &mut SourceScan) {
    for or_term in chain.or_terms() {
        for pipeline in or_term.and_terms() {
            for stage in pipeline.stages() {
                match stage.kind() {
                    StageKind::Expression(expression) => scan_expression(source, expression, scan),
                    StageKind::Command(command) => {
                        let name = simple_word(source, command.head.word());
                        if let Some(name) = &name {
                            scan.identifiers.insert(name.clone());
                            scan.references.push(Reference {
                                name: name.clone(),
                                name_span: command.head.span(),
                                full_span: command.head.span(),
                                kind: ReferenceKind::Command,
                            });
                        }
                        scan.commands.push(CommandUse {
                            name,
                            span: command.head.span(),
                            forced_external: command.head.kind() == CommandHeadKind::ForcedExternal,
                        });
                        for item in &command.items {
                            match item.kind() {
                                CommandItemKind::Word(word) => scan_word(source, word, scan),
                                CommandItemKind::Spread(variable) => reference(
                                    source,
                                    variable.name.span(),
                                    variable.span,
                                    ReferenceKind::Spread,
                                    scan,
                                ),
                                CommandItemKind::Closure(closure) => {
                                    for parameter in &closure.parameters {
                                        scan_pattern(source, &parameter.pattern, scan);
                                        add_pattern_shadow(
                                            source,
                                            &parameter.pattern,
                                            closure.body.span().start(),
                                            closure.body.span().end(),
                                            scan,
                                        );
                                    }
                                    scan_chain(source, &closure.body, scan);
                                }
                                CommandItemKind::Redirection(redirection) => {
                                    scan.effect_spans.push(redirection.span());
                                    match redirection.kind() {
                                        RedirectionKind::Input { target, .. } => {
                                            scan_word(source, target, scan);
                                        }
                                        RedirectionKind::File(file) => {
                                            scan_word(source, &file.target, scan);
                                        }
                                        RedirectionKind::Duplicate { .. }
                                        | RedirectionKind::Close { .. } => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn scan_word(source: &flash_syntax::SourceFile, word: &Word, scan: &mut SourceScan) {
    for part in word.parts() {
        scan_word_part(source, part, scan);
    }
}

fn scan_word_part(source: &flash_syntax::SourceFile, part: &WordPart, scan: &mut SourceScan) {
    match part.kind() {
        WordPartKind::Variable(identifier) => reference(
            source,
            identifier.span(),
            part.span(),
            ReferenceKind::WordValue,
            scan,
        ),
        WordPartKind::BracedInterpolation(expression) => {
            scan_expression(source, expression, scan);
        }
        WordPartKind::CommandSubstitution(substitution) => {
            scan.effect_spans.push(substitution.chain().span());
            scan_chain(source, substitution.chain(), scan);
        }
        WordPartKind::DoubleQuoted(parts) => {
            for part in parts {
                scan_word_part(source, part, scan);
            }
        }
        WordPartKind::SingleQuoted => {}
        WordPartKind::Bare
        | WordPartKind::BareEscape
        | WordPartKind::DoubleText
        | WordPartKind::DoubleEscape => {}
    }
}

fn simple_word(source: &flash_syntax::SourceFile, word: &Word) -> Option<String> {
    if word.parts().len() != 1 || !matches!(word.parts()[0].kind(), WordPartKind::Bare) {
        return None;
    }
    Some(text(source, word.span()))
}

fn add_quoted_reserved_use(source: &flash_syntax::SourceFile, span: Span, scan: &mut SourceScan) {
    let spelling = text(source, span);
    if is_new_reserved(&spelling) {
        scan.reserved_uses.push(ReservedUse {
            start: span.start(),
            end: span.end(),
            replacement: format!("'{spelling}'"),
            spelling,
        });
    }
}

fn is_new_reserved(spelling: &str) -> bool {
    matches!(spelling, "action" | "enum" | "language" | "task" | "type")
}

fn direct_bindings(
    source: &flash_syntax::SourceFile,
    statement: &Statement,
    names: &mut Vec<String>,
) {
    match statement.kind() {
        StatementKind::ModuleImport(import) => names.push(text(source, import.alias.span())),
        StatementKind::NominalType(declaration) => {
            names.push(text(source, declaration.name.span()))
        }
        StatementKind::VariantType(declaration) => {
            names.push(text(source, declaration.name.span()))
        }
        StatementKind::Declaration(declaration) => {
            pattern_names(source, &declaration.pattern, names)
        }
        StatementKind::Function(function) => names.push(text(source, function.name.span())),
        _ => {}
    }
}

fn pattern_names(source: &flash_syntax::SourceFile, pattern: &Pattern, names: &mut Vec<String>) {
    match pattern {
        Pattern::Binding(identifier) => names.push(text(source, identifier.span())),
        Pattern::List(pattern) => {
            for element in &pattern.elements {
                pattern_names(source, element, names);
            }
            if let Some(rest) = pattern.rest {
                names.push(text(source, rest.span()));
            }
        }
        Pattern::NominalRecord(pattern) => {
            for field in &pattern.fields {
                pattern_names(source, &field.pattern, names);
            }
        }
        Pattern::Variant(pattern) => {
            for item in &pattern.payload {
                pattern_names(source, item, names);
            }
        }
        Pattern::Wildcard(_) | Pattern::Literal(_) => {}
    }
}

fn add_pattern_shadow(
    source: &flash_syntax::SourceFile,
    pattern: &Pattern,
    start: usize,
    end: usize,
    scan: &mut SourceScan,
) {
    let mut names = Vec::new();
    pattern_names(source, pattern, &mut names);
    for name in names {
        scan.shadow_ranges.push((name, start, end));
    }
}

fn add_shadow(
    source: &flash_syntax::SourceFile,
    binding: Span,
    start: usize,
    end: usize,
    scan: &mut SourceScan,
) {
    scan.shadow_ranges.push((text(source, binding), start, end));
}
