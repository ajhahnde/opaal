//! URI-free semantic queries over one complete canonical module program.

use std::collections::BTreeMap;

use flash_syntax::{
    Block, CommandItemKind, CompletionContext, ConditionalChain, ControlTransfer, ElseBranch,
    Expression, ExpressionKind, LiteralKind, MatchArm, Pattern, RecordKey, RedirectionKind, Span,
    StageKind, Statement, StatementKind, Word, WordPart, WordPartKind, completion_target,
};

use crate::command::{CommandClassification, CommandRegistry, CommandSignature, NamespaceClass};
use crate::intrinsic::{DynamicBinding, ExpressionIntrinsic};
use crate::module::{
    FunctionSignature, ModuleEffectSummary, ModuleId, ModuleNameImport, ModuleProgram,
    ModuleReferenceTarget, ValueType,
};

/// One canonical source location without an editor URI or protocol position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLocation {
    module: ModuleId,
    span: Span,
}

impl SourceLocation {
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// The semantic class of one visible lexical or intrinsic name.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NameKind {
    Intrinsic,
    DynamicBinding,
    ScriptArguments,
    Binding,
    Function,
    ImportedBinding,
    ImportedFunction,
}

/// Hover data for one reserved value supplied by the active evaluation host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DynamicBindingHover {
    binding: DynamicBinding,
}

impl DynamicBindingHover {
    #[must_use]
    pub const fn binding(self) -> DynamicBinding {
        self.binding
    }
}

/// Hover data for one unshadowed expression intrinsic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntrinsicHover {
    intrinsic: ExpressionIntrinsic,
}

impl IntrinsicHover {
    #[must_use]
    pub const fn intrinsic(self) -> ExpressionIntrinsic {
        self.intrinsic
    }
}

/// One deterministically ordered name visible at a source byte cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleName {
    name: String,
    kind: NameKind,
    definition: Option<SourceLocation>,
    value_type: ValueType,
}

impl VisibleName {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn kind(&self) -> NameKind {
        self.kind
    }

    #[must_use]
    pub const fn definition(&self) -> Option<&SourceLocation> {
        self.definition.as_ref()
    }

    #[must_use]
    pub const fn value_type(&self) -> &ValueType {
        &self.value_type
    }
}

/// Registry-owned metadata for one invocable command spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandMetadata {
    name: String,
    canonical_name: String,
    class: NamespaceClass,
    signature: CommandSignature,
}

impl CommandMetadata {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn canonical_name(&self) -> &str {
        &self.canonical_name
    }

    #[must_use]
    pub const fn class(&self) -> NamespaceClass {
        self.class
    }

    #[must_use]
    pub const fn signature(&self) -> &CommandSignature {
        &self.signature
    }
}

/// Hover data for one resolved non-function binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingHover {
    name: String,
    definition: Option<SourceLocation>,
    value_type: ValueType,
}

impl BindingHover {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn definition(&self) -> Option<&SourceLocation> {
        self.definition.as_ref()
    }

    #[must_use]
    pub const fn value_type(&self) -> &ValueType {
        &self.value_type
    }
}

/// Hover data for one resolved named callable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionHover {
    definition: SourceLocation,
    signature: FunctionSignature,
}

impl FunctionHover {
    #[must_use]
    pub const fn definition(&self) -> &SourceLocation {
        &self.definition
    }

    #[must_use]
    pub const fn signature(&self) -> &FunctionSignature {
        &self.signature
    }
}

/// Named-import target and its shared direct/transitive initializer effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedImportEffects {
    target: ModuleId,
    direct: ModuleEffectSummary,
    transitive: ModuleEffectSummary,
}

impl NamedImportEffects {
    #[must_use]
    pub const fn target(&self) -> &ModuleId {
        &self.target
    }

    #[must_use]
    pub const fn direct(&self) -> &ModuleEffectSummary {
        &self.direct
    }

    #[must_use]
    pub const fn transitive(&self) -> &ModuleEffectSummary {
        &self.transitive
    }
}

/// The protocol-neutral hover variants owned by shared semantic data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticHover {
    Intrinsic(IntrinsicHover),
    DynamicBinding(DynamicBindingHover),
    Binding(BindingHover),
    Function(FunctionHover),
    Command(CommandMetadata),
}

/// One enclosing intrinsic call and its active argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntrinsicSignatureContext {
    call_span: Span,
    active_parameter: usize,
    intrinsic: ExpressionIntrinsic,
}

impl IntrinsicSignatureContext {
    #[must_use]
    pub const fn call_span(self) -> Span {
        self.call_span
    }

    #[must_use]
    pub const fn active_parameter(self) -> usize {
        self.active_parameter
    }

    #[must_use]
    pub const fn intrinsic(self) -> ExpressionIntrinsic {
        self.intrinsic
    }
}

/// One enclosing resolved function call and its active argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureContext {
    call_span: Span,
    active_parameter: usize,
    definition: SourceLocation,
    signature: FunctionSignature,
}

impl SignatureContext {
    #[must_use]
    pub const fn call_span(&self) -> Span {
        self.call_span
    }

    #[must_use]
    pub const fn active_parameter(&self) -> usize {
        self.active_parameter
    }

    #[must_use]
    pub const fn definition(&self) -> &SourceLocation {
        &self.definition
    }

    #[must_use]
    pub const fn signature(&self) -> &FunctionSignature {
        &self.signature
    }
}

/// Shared semantic queries over immutable program and command metadata only.
pub struct SemanticQueries<'a> {
    program: &'a ModuleProgram,
    commands: &'a CommandRegistry,
}

impl ModuleProgram {
    /// Creates a host-free query facade over this complete program.
    #[must_use]
    pub const fn semantic_queries<'a>(
        &'a self,
        commands: &'a CommandRegistry,
    ) -> SemanticQueries<'a> {
        SemanticQueries {
            program: self,
            commands,
        }
    }
}

impl SemanticQueries<'_> {
    /// Returns visible lexical and unshadowed intrinsic names in exact name order.
    #[must_use]
    pub fn visible_names(&self, module: &ModuleId, cursor: usize) -> Option<Vec<VisibleName>> {
        let source = self.program.sources().source(module)?;
        if cursor > source.len() || !source.text().is_char_boundary(cursor) {
            return None;
        }
        let mut visible = self
            .program
            .names()
            .visible_bindings(module, cursor)
            .into_iter()
            .map(|binding| {
                let (definition, value_type, function, imported) =
                    self.target_metadata(binding.target());
                let kind = match (binding.target(), function, imported) {
                    (ModuleReferenceTarget::DynamicStatus, _, _) => NameKind::DynamicBinding,
                    (ModuleReferenceTarget::ScriptArguments, _, _) => NameKind::ScriptArguments,
                    (_, true, true) => NameKind::ImportedFunction,
                    (_, false, true) => NameKind::ImportedBinding,
                    (_, true, false) => NameKind::Function,
                    (_, false, false) => NameKind::Binding,
                };
                VisibleName {
                    name: binding.name().to_owned(),
                    kind,
                    definition,
                    value_type,
                }
            })
            .map(|name| (name.name.clone(), name))
            .collect::<BTreeMap<_, _>>();
        for intrinsic in ExpressionIntrinsic::ALL {
            visible
                .entry(intrinsic.name().to_owned())
                .or_insert_with(|| VisibleName {
                    name: intrinsic.name().to_owned(),
                    kind: NameKind::Intrinsic,
                    definition: None,
                    value_type: ValueType::Function,
                });
        }
        Some(visible.into_values().collect())
    }

    /// Resolves a lexical read, import, export, or declaration to its definition.
    #[must_use]
    pub fn definition_at(&self, module: &ModuleId, offset: usize) -> Option<SourceLocation> {
        self.program
            .names()
            .target_at(module, offset)
            .and_then(|(_, target)| target_location(&target))
    }

    /// Returns deterministic inverse references for one canonical definition.
    #[must_use]
    pub fn references_to(
        &self,
        definition: &SourceLocation,
        include_declaration: bool,
    ) -> Vec<SourceLocation> {
        let mut locations = Vec::new();
        if include_declaration {
            locations.push(definition.clone());
        }
        for entry in self.program.sources().entries() {
            for export in self.program.names().exports(entry.module()) {
                if entry.module() == definition.module()
                    && export.declaration_span() == definition.span()
                {
                    locations.push(SourceLocation {
                        module: entry.module().clone(),
                        span: export.export_span(),
                    });
                }
            }
            for import in self.program.names().imports(entry.module()) {
                if self.import_definition(import).as_ref() == Some(definition) {
                    locations.push(SourceLocation {
                        module: entry.module().clone(),
                        span: import.name_span(),
                    });
                }
            }
            for reference in self.program.names().references(entry.module()) {
                if target_location(reference.target()).as_ref() == Some(definition) {
                    locations.push(SourceLocation {
                        module: entry.module().clone(),
                        span: reference.reference_span(),
                    });
                }
            }
        }
        let source_order = self
            .program
            .sources()
            .entries()
            .enumerate()
            .map(|(index, entry)| (entry.module(), index))
            .collect::<BTreeMap<_, _>>();
        locations.sort_by_key(|location| {
            (
                source_order[location.module()],
                location.span.start(),
                location.span.end(),
            )
        });
        locations.dedup();
        locations
    }

    /// Returns shared binding, callable, or command hover data at one byte.
    #[must_use]
    pub fn hover_at(&self, module: &ModuleId, offset: usize) -> Option<SemanticHover> {
        if let Some((name, target)) = self.program.names().target_at(module, offset) {
            if matches!(target, ModuleReferenceTarget::DynamicStatus) {
                return Some(SemanticHover::DynamicBinding(DynamicBindingHover {
                    binding: DynamicBinding::CurrentStatus,
                }));
            }
            let (definition, value_type, function, _) = self.target_metadata(&target);
            if function {
                let definition = definition?;
                let signature = self
                    .program
                    .types()
                    .function(definition.module(), definition.span())?
                    .clone();
                return Some(SemanticHover::Function(FunctionHover {
                    definition,
                    signature,
                }));
            }
            return Some(SemanticHover::Binding(BindingHover {
                name,
                definition,
                value_type,
            }));
        }
        self.intrinsic_at(module, offset)
            .map(|intrinsic| SemanticHover::Intrinsic(IntrinsicHover { intrinsic }))
            .or_else(|| self.command_at(module, offset).map(SemanticHover::Command))
    }

    /// Finds the smallest enclosing resolved function call and active argument.
    #[must_use]
    pub fn signature_at(&self, module: &ModuleId, offset: usize) -> Option<SignatureContext> {
        let script = self.program.sources().script(module)?;
        let (call, call_span) = CallFinder::new(offset).find(script.statements())?;
        let reference = self.program.names().reference(module, call.callee.span())?;
        let definition = target_location(reference.target())?;
        let signature = self
            .program
            .types()
            .function(definition.module(), definition.span())?
            .clone();
        let active_parameter = call
            .arguments
            .iter()
            .position(|argument| offset <= argument.span().end())
            .unwrap_or(call.arguments.len());
        Some(SignatureContext {
            call_span,
            active_parameter,
            definition,
            signature,
        })
    }

    /// Finds the smallest enclosing unshadowed intrinsic call and active argument.
    #[must_use]
    pub fn intrinsic_signature_at(
        &self,
        module: &ModuleId,
        offset: usize,
    ) -> Option<IntrinsicSignatureContext> {
        let script = self.program.sources().script(module)?;
        let (call, call_span) = CallFinder::new(offset).find(script.statements())?;
        let intrinsic = self.intrinsic_call(module, call, call.callee.span().start())?;
        let active_parameter = call
            .arguments
            .iter()
            .position(|argument| offset <= argument.span().end())
            .unwrap_or(call.arguments.len());
        Some(IntrinsicSignatureContext {
            call_span,
            active_parameter,
            intrinsic,
        })
    }

    /// Returns shared effects for a named import identifier.
    #[must_use]
    pub fn import_effects_at(
        &self,
        module: &ModuleId,
        offset: usize,
    ) -> Option<NamedImportEffects> {
        let import = self
            .program
            .names()
            .imports(module)
            .iter()
            .find(|import| contains(import.name_span(), offset))?;
        Some(NamedImportEffects {
            target: import.target().clone(),
            direct: self.program.effects().direct(import.target()).clone(),
            transitive: self.program.effects().transitive(import.target()).clone(),
        })
    }

    /// Returns invocable command spellings with the requested prefix.
    #[must_use]
    pub fn command_candidates(&self, prefix: &str) -> Vec<CommandMetadata> {
        self.commands
            .namespace_entries()
            .filter(|entry| entry.name().starts_with(prefix))
            .filter_map(|entry| self.command_metadata(entry.name()))
            .collect()
    }

    /// Returns registry-owned flags in exact spelling order.
    #[must_use]
    pub fn command_flags<'a>(&'a self, name: &str) -> Vec<&'a str> {
        self.commands
            .lookup(name)
            .into_iter()
            .flat_map(CommandSignature::flags)
            .collect()
    }

    fn target_metadata(
        &self,
        target: &ModuleReferenceTarget,
    ) -> (Option<SourceLocation>, ValueType, bool, bool) {
        if matches!(target, ModuleReferenceTarget::DynamicStatus) {
            return (
                None,
                DynamicBinding::CurrentStatus.result_type(),
                false,
                false,
            );
        }
        let Some(definition) = target_location(target) else {
            return (
                None,
                ValueType::List(Box::new(ValueType::String)),
                false,
                false,
            );
        };
        if self
            .program
            .types()
            .function(definition.module(), definition.span())
            .is_some()
        {
            return (
                Some(definition),
                ValueType::Function,
                true,
                matches!(target, ModuleReferenceTarget::Imported { .. }),
            );
        }
        let value_type = self
            .program
            .types()
            .binding_type(definition.module(), definition.span())
            .cloned()
            .unwrap_or(ValueType::Any);
        (
            Some(definition),
            value_type,
            false,
            matches!(target, ModuleReferenceTarget::Imported { .. }),
        )
    }

    fn import_definition(&self, import: &ModuleNameImport) -> Option<SourceLocation> {
        self.program
            .names()
            .export(import.target(), import.name())
            .map(|export| SourceLocation {
                module: import.target().clone(),
                span: export.declaration_span(),
            })
    }

    fn command_at(&self, module: &ModuleId, offset: usize) -> Option<CommandMetadata> {
        let source = self.program.sources().source(module)?;
        let target = completion_target(source.text(), offset)?;
        if !matches!(
            target.context(),
            CompletionContext::Command {
                forced_external: false
            }
        ) {
            return None;
        }
        let name = source.text().get(target.replacement())?;
        self.command_metadata(name)
    }

    fn intrinsic_at(&self, module: &ModuleId, offset: usize) -> Option<ExpressionIntrinsic> {
        let script = self.program.sources().script(module)?;
        let (call, _) = CallFinder::new(offset).find(script.statements())?;
        self.intrinsic_call(module, call, offset)
    }

    fn intrinsic_call(
        &self,
        module: &ModuleId,
        call: &flash_syntax::CallExpression,
        offset: usize,
    ) -> Option<ExpressionIntrinsic> {
        if !contains(call.callee.span(), offset)
            || self
                .program
                .names()
                .reference(module, call.callee.span())
                .is_some()
        {
            return None;
        }
        let ExpressionKind::Symbol(identifier) = call.callee.kind() else {
            return None;
        };
        let source = self.program.sources().source(module)?;
        ExpressionIntrinsic::lookup(source.slice(identifier.span()).ok()?)
    }

    fn command_metadata(&self, name: &str) -> Option<CommandMetadata> {
        match self.commands.classify(name) {
            CommandClassification::Core { signature, .. } => Some(CommandMetadata {
                name: name.to_owned(),
                canonical_name: signature.name().to_owned(),
                class: NamespaceClass::Core,
                signature: signature.clone(),
            }),
            CommandClassification::Alias {
                canonical_name,
                signature,
                ..
            } => Some(CommandMetadata {
                name: name.to_owned(),
                canonical_name: canonical_name.to_owned(),
                class: NamespaceClass::Alias,
                signature: signature.clone(),
            }),
            CommandClassification::Unknown | CommandClassification::Reserved { .. } => None,
        }
    }
}

fn target_location(target: &ModuleReferenceTarget) -> Option<SourceLocation> {
    match target {
        ModuleReferenceTarget::DynamicStatus | ModuleReferenceTarget::ScriptArguments => None,
        ModuleReferenceTarget::Local {
            module,
            declaration_span,
        } => Some(SourceLocation {
            module: module.clone(),
            span: *declaration_span,
        }),
        ModuleReferenceTarget::Imported {
            target_module,
            declaration_span,
            ..
        } => Some(SourceLocation {
            module: target_module.clone(),
            span: *declaration_span,
        }),
    }
}

fn contains(span: Span, offset: usize) -> bool {
    span.start() <= offset && offset < span.end()
}

struct CallFinder<'a> {
    offset: usize,
    best: Option<(&'a flash_syntax::CallExpression, Span)>,
}

impl<'a> CallFinder<'a> {
    const fn new(offset: usize) -> Self {
        Self { offset, best: None }
    }

    fn find(
        mut self,
        statements: &'a [Statement],
    ) -> Option<(&'a flash_syntax::CallExpression, Span)> {
        self.statements(statements);
        self.best
    }

    fn statements(&mut self, statements: &'a [Statement]) {
        for statement in statements {
            if contains(statement.span(), self.offset) {
                self.statement(statement);
            }
        }
    }

    fn statement(&mut self, statement: &'a Statement) {
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => {}
            StatementKind::Declaration(declaration) => self.expression(&declaration.value),
            StatementKind::Assignment(assignment) => self.expression(&assignment.value),
            StatementKind::Environment(environment) => {
                if let flash_syntax::EnvironmentStatement::Export { value, .. } = environment {
                    self.expression(value);
                }
            }
            StatementKind::Function(function) => self.block(&function.body),
            StatementKind::If(statement) => {
                self.chain(&statement.condition);
                self.block(&statement.then_block);
                match &statement.else_branch {
                    Some(ElseBranch::Block(block)) => self.block(block),
                    Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
                    None => {}
                }
            }
            StatementKind::While(statement) => {
                self.chain(&statement.condition);
                self.block(&statement.body);
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable);
                self.block(&statement.body);
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value);
                for arm in &statement.arms {
                    self.match_arm(arm);
                }
            }
            StatementKind::Control(ControlTransfer::Return(Some(expression))) => {
                self.expression(expression);
            }
            StatementKind::Control(
                ControlTransfer::Break | ControlTransfer::Continue | ControlTransfer::Return(None),
            ) => {}
            StatementKind::Job(job) => self.chain(&job.chain),
        }
    }

    fn if_statement(&mut self, statement: &'a flash_syntax::IfStatement) {
        self.chain(&statement.condition);
        self.block(&statement.then_block);
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.block(block),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => {}
        }
    }

    fn match_arm(&mut self, arm: &'a MatchArm) {
        if let Pattern::Literal(literal) = &arm.pattern {
            self.literal(literal);
        }
        if let Some(guard) = &arm.guard {
            self.expression(guard);
        }
        self.statements(&arm.body.statements);
    }

    fn block(&mut self, block: &'a Block) {
        if contains(block.span, self.offset) {
            self.statements(&block.statements);
        }
    }

    fn chain(&mut self, chain: &'a ConditionalChain) {
        if !contains(chain.span(), self.offset) {
            return;
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                for stage in pipeline.stages() {
                    if !contains(stage.span(), self.offset) {
                        continue;
                    }
                    match stage.kind() {
                        StageKind::Expression(expression) => self.expression(expression),
                        StageKind::Command(command) => {
                            self.word(command.head.word());
                            for item in &command.items {
                                match item.kind() {
                                    CommandItemKind::Word(word) => self.word(word),
                                    CommandItemKind::Closure(closure) => self.chain(&closure.body),
                                    CommandItemKind::Redirection(redirection) => {
                                        self.redirection(redirection.kind());
                                    }
                                    CommandItemKind::Spread(_) => {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn expression(&mut self, expression: &'a Expression) {
        if !contains(expression.span(), self.offset) {
            return;
        }
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(_) | ExpressionKind::Symbol(_) => {}
            ExpressionKind::List(items) => {
                for item in items {
                    self.expression(item);
                }
            }
            ExpressionKind::Record(entries) => {
                for entry in entries {
                    if let RecordKey::DoubleQuoted(part) = &entry.key {
                        self.word_part(part);
                    }
                    self.expression(&entry.value);
                }
            }
            ExpressionKind::Closure(closure) => self.chain(&closure.body),
            ExpressionKind::CommandSubstitution(chain) | ExpressionKind::GroupedJob(chain) => {
                self.chain(chain);
            }
            ExpressionKind::Call(call) => {
                self.expression(&call.callee);
                for argument in &call.arguments {
                    self.expression(argument);
                }
                if self
                    .best
                    .is_none_or(|(_, best)| expression.span().len() < best.len())
                {
                    self.best = Some((call, expression.span()));
                }
            }
            ExpressionKind::Index(index) => {
                self.expression(&index.target);
                self.expression(&index.index);
            }
            ExpressionKind::Member(member) => self.expression(&member.target),
            ExpressionKind::Unary(unary) => self.expression(&unary.operand),
            ExpressionKind::Binary(binary) => {
                self.expression(&binary.left);
                self.expression(&binary.right);
            }
        }
    }

    fn literal(&mut self, literal: &'a flash_syntax::Literal) {
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            for part in parts {
                self.word_part(part);
            }
        }
    }

    fn word(&mut self, word: &'a Word) {
        for part in word.parts() {
            self.word_part(part);
        }
    }

    fn word_part(&mut self, part: &'a WordPart) {
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => {
                for part in parts {
                    self.word_part(part);
                }
            }
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(chain) => self.chain(chain),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape
            | WordPartKind::Variable(_) => {}
        }
    }

    fn redirection(&mut self, redirection: &'a RedirectionKind) {
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => {}
        }
    }
}
