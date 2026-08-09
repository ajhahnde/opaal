//! Canonical module identity and import-graph analysis.
//!
//! This module is deliberately host-free. A caller injects path
//! canonicalization through [`ModuleCanonicalizer`], while the resolver records
//! the original request and source span alongside the canonical target. The
//! graph uses only canonical identities, rejects cycles before mutation, and
//! exposes structured diagnostics for checker, editor, and protocol clients.
//!
//! Static import syntax and injected recursive source loading build on this
//! graph. Program construction also resolves explicit exports/imports and every
//! lexical read without execution. Frontend wiring and execution remain
//! separate layers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use flash_syntax::{
    Block, Closure, CommandItemKind, ConditionalChain, ControlTransfer, Diagnostic, ElseBranch,
    Expression, ExpressionKind, LiteralKind, MatchArm, ParseOutcome, Pattern, Pipeline, RecordKey,
    RedirectionKind, Script, Severity, SourceFile, SourceId, Span, StageKind, Statement,
    StatementKind, Word, WordPart, WordPartKind, parse, render_diagnostic_sources,
};

/// The sole host capability needed to turn a candidate source path into its
/// unique module path.
///
/// Implementations must resolve relative components and filesystem aliases,
/// including symbolic links where the target supports them, and return an
/// absolute path. The runtime never accesses the filesystem directly.
pub trait ModuleCanonicalizer {
    /// Returns the unique absolute identity for `candidate`.
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError>;
}

/// A host-independent failure reported by a [`ModuleCanonicalizer`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModulePathError {
    message: String,
}

impl ModulePathError {
    /// Creates a canonicalization failure with a human-readable cause.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The canonicalizer's explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ModulePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ModulePathError {}

/// The injected capability for reading one already canonical source module.
///
/// Implementations may use a host filesystem or another target-appropriate
/// source store. The runtime receives owned bytes and performs UTF-8 decoding
/// and parsing itself.
pub trait ModuleSourceLoader {
    /// Reads all source bytes for `module`.
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError>;
}

/// A host-independent source read failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleSourceError {
    message: String,
}

impl ModuleSourceError {
    /// Creates a source read failure with a human-readable cause.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The source loader's explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ModuleSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ModuleSourceError {}

/// The canonical native path that uniquely identifies one source module.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModuleId(PathBuf);

impl ModuleId {
    /// The canonical native path for this module.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// One explicit import after its requested path has been resolved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleImport {
    importer: ModuleId,
    requested: PathBuf,
    target: ModuleId,
    span: Span,
}

impl ModuleImport {
    /// The canonical module containing the import.
    #[must_use]
    pub const fn importer(&self) -> &ModuleId {
        &self.importer
    }

    /// The path spelling requested by the source.
    #[must_use]
    pub fn requested(&self) -> &Path {
        &self.requested
    }

    /// The canonical target identity.
    #[must_use]
    pub const fn target(&self) -> &ModuleId {
        &self.target
    }

    /// The source range containing the requested path.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Resolves root and imported module paths through one injected canonicalizer.
pub struct ModuleResolver<'a> {
    canonicalizer: &'a dyn ModuleCanonicalizer,
}

impl<'a> ModuleResolver<'a> {
    /// Creates a resolver over `canonicalizer`.
    #[must_use]
    pub const fn new(canonicalizer: &'a dyn ModuleCanonicalizer) -> Self {
        Self { canonicalizer }
    }

    /// Resolves a root source path without an importing source span.
    pub fn resolve_root(&self, requested: &Path) -> Result<ModuleId, ModuleResolutionError> {
        self.resolve_candidate(None, requested, requested, None)
    }

    /// Resolves `requested` relative to the importing module's directory.
    ///
    /// An absolute request remains absolute. The import span is retained for
    /// diagnostics even when multiple spellings resolve to the same target.
    pub fn resolve_import(
        &self,
        importer: &ModuleId,
        requested: &Path,
        span: Span,
    ) -> Result<ModuleImport, ModuleResolutionError> {
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            importer
                .path()
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(requested)
        };
        let candidate = candidate.components().collect::<PathBuf>();
        let target =
            self.resolve_candidate(Some(importer.clone()), requested, &candidate, Some(span))?;

        Ok(ModuleImport {
            importer: importer.clone(),
            requested: requested.to_path_buf(),
            target,
            span,
        })
    }

    fn resolve_candidate(
        &self,
        importer: Option<ModuleId>,
        requested: &Path,
        candidate: &Path,
        span: Option<Span>,
    ) -> Result<ModuleId, ModuleResolutionError> {
        let canonical = self
            .canonicalizer
            .canonicalize(candidate)
            .map_err(|cause| ModuleResolutionError {
                context: Box::new(ModuleResolutionContext {
                    importer: importer.clone(),
                    requested: requested.to_path_buf(),
                    candidate: candidate.to_path_buf(),
                    span,
                }),
                cause,
            })?;

        if !canonical.is_absolute() {
            return Err(ModuleResolutionError {
                context: Box::new(ModuleResolutionContext {
                    importer,
                    requested: requested.to_path_buf(),
                    candidate: candidate.to_path_buf(),
                    span,
                }),
                cause: ModulePathError::new("canonicalizer returned a non-absolute path"),
            });
        }

        Ok(ModuleId(canonical))
    }
}

/// A failed root or import path resolution with all original context retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleResolutionError {
    context: Box<ModuleResolutionContext>,
    cause: ModulePathError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModuleResolutionContext {
    importer: Option<ModuleId>,
    requested: PathBuf,
    candidate: PathBuf,
    span: Option<Span>,
}

impl ModuleResolutionError {
    /// The importing module, or `None` when resolving a root.
    #[must_use]
    pub const fn importer(&self) -> Option<&ModuleId> {
        self.context.importer.as_ref()
    }

    /// The path spelling supplied by the caller or source.
    #[must_use]
    pub fn requested(&self) -> &Path {
        &self.context.requested
    }

    /// The path sent to the canonicalizer after relative joining.
    #[must_use]
    pub fn candidate(&self) -> &Path {
        &self.context.candidate
    }

    /// The import request span, absent only for a root request.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        self.context.span
    }

    /// The canonicalizer's failure.
    #[must_use]
    pub const fn cause(&self) -> &ModulePathError {
        &self.cause
    }

    /// Builds a source-anchored diagnostic for an import request.
    ///
    /// Root resolution has no source span and therefore returns `None`.
    #[must_use]
    pub fn diagnostic(&self) -> Option<Diagnostic> {
        let span = self.context.span?;
        Some(
            Diagnostic::new(Severity::Error, "MOD001", self.to_string())
                .with_primary(span, "this module path could not be resolved"),
        )
    }
}

impl fmt::Display for ModuleResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.context.importer {
            Some(importer) => write!(
                formatter,
                "cannot resolve module `{}` imported by `{}`: {}",
                self.context.requested.display(),
                importer.path().display(),
                self.cause
            ),
            None => write!(
                formatter,
                "cannot resolve root module `{}`: {}",
                self.context.requested.display(),
                self.cause
            ),
        }
    }
}

impl std::error::Error for ModuleResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// One canonical module's source and parsed syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredModuleSource {
    module: ModuleId,
    source: SourceFile,
    script: Script,
}

impl RegisteredModuleSource {
    /// The canonical module identity.
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    /// The stable source file assigned during traversal.
    #[must_use]
    pub const fn source(&self) -> &SourceFile {
        &self.source
    }

    /// The complete parsed module syntax.
    #[must_use]
    pub const fn script(&self) -> &Script {
        &self.script
    }
}

/// A bidirectional registry of canonical modules and stable source identities.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleSourceRegistry {
    by_module: BTreeMap<ModuleId, usize>,
    by_source: BTreeMap<SourceId, usize>,
    entries: Vec<RegisteredModuleSource>,
}

/// One top-level name made visible by a module export list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleExport {
    name: String,
    declaration_span: Span,
    export_span: Span,
}

impl ModuleExport {
    /// The exact exported identifier spelling.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The local declaration that owns the exported name.
    #[must_use]
    pub const fn declaration_span(&self) -> Span {
        self.declaration_span
    }

    /// The identifier in the explicit export list.
    #[must_use]
    pub const fn export_span(&self) -> Span {
        self.export_span
    }
}

/// One explicit imported name after its target export has been validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleNameImport {
    name: String,
    importer: ModuleId,
    target: ModuleId,
    name_span: Span,
}

/// The lexical binding selected by one statically resolved name read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleReferenceTarget {
    /// A declaration in the same canonical source module.
    Local {
        /// The module that owns the declaration.
        module: ModuleId,
        /// The identifier that introduced the binding.
        declaration_span: Span,
    },
    /// An explicit import whose definition and visibility live in another
    /// canonical source module.
    Imported {
        /// The identifier that introduced the binding in the importing module.
        import_span: Span,
        /// The canonical module that owns the exported declaration.
        target_module: ModuleId,
        /// The target module's declaration identifier.
        declaration_span: Span,
        /// The target module's explicit export-list identifier.
        export_span: Span,
    },
}

/// One source-spanned lexical read and its statically selected binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleNameReference {
    name: String,
    reference_span: Span,
    target: ModuleReferenceTarget,
}

impl ModuleNameReference {
    /// The exact identifier spelling used by this read.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The complete syntactic read, such as `$name` or `...$name`.
    #[must_use]
    pub const fn reference_span(&self) -> Span {
        self.reference_span
    }

    /// The local or imported binding selected for this read.
    #[must_use]
    pub const fn target(&self) -> &ModuleReferenceTarget {
        &self.target
    }
}

impl ModuleNameImport {
    /// The imported and local identifier spelling.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The canonical module containing the named import.
    #[must_use]
    pub const fn importer(&self) -> &ModuleId {
        &self.importer
    }

    /// The canonical module exporting the name.
    #[must_use]
    pub const fn target(&self) -> &ModuleId {
        &self.target
    }

    /// The identifier in the explicit import list.
    #[must_use]
    pub const fn name_span(&self) -> Span {
        self.name_span
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModuleNames {
    locals: BTreeMap<String, Span>,
    exports: BTreeMap<String, ModuleExport>,
    imports: Vec<ModuleNameImport>,
    references: Vec<ModuleNameReference>,
}

/// Deterministic local, exported, imported, and referenced names by module.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleNameRegistry {
    by_module: BTreeMap<ModuleId, ModuleNames>,
}

impl ModuleNameRegistry {
    /// Looks up one exported name from a canonical module.
    #[must_use]
    pub fn export(&self, module: &ModuleId, name: &str) -> Option<&ModuleExport> {
        self.by_module
            .get(module)
            .and_then(|names| names.exports.get(name))
    }

    /// Explicit imports in source order for one canonical module.
    #[must_use]
    pub fn imports(&self, module: &ModuleId) -> &[ModuleNameImport] {
        self.by_module
            .get(module)
            .map_or(&[], |names| names.imports.as_slice())
    }

    /// Lexical reads in deterministic source-traversal order for one module.
    #[must_use]
    pub fn references(&self, module: &ModuleId) -> &[ModuleNameReference] {
        self.by_module
            .get(module)
            .map_or(&[], |names| names.references.as_slice())
    }

    /// Looks up the lexical read occupying one exact complete source span.
    #[must_use]
    pub fn reference(
        &self,
        module: &ModuleId,
        reference_span: Span,
    ) -> Option<&ModuleNameReference> {
        self.references(module)
            .iter()
            .find(|reference| reference.reference_span() == reference_span)
    }

    /// Explicit exports in deterministic name order for one canonical module.
    pub fn exports<'a>(&'a self, module: &ModuleId) -> impl Iterator<Item = &'a ModuleExport> {
        self.by_module
            .get(module)
            .into_iter()
            .flat_map(|names| names.exports.values())
    }

    fn analyze(
        graph: &ModuleGraph,
        sources: &ModuleSourceRegistry,
    ) -> Result<Self, Box<ModuleNameError>> {
        let mut registry = Self::default();

        for entry in sources.entries() {
            let mut names = ModuleNames::default();
            for statement in entry.script().statements() {
                let identifier = match statement.kind() {
                    StatementKind::Declaration(declaration) => Some(declaration.name),
                    StatementKind::Function(function) => Some(function.name),
                    _ => None,
                };
                let Some(identifier) = identifier else {
                    continue;
                };
                let name = entry
                    .source()
                    .slice(identifier.span())
                    .expect("parsed identifiers belong to their module source")
                    .to_owned();
                names.locals.entry(name).or_insert(identifier.span());
            }
            registry.by_module.insert(entry.module().clone(), names);
        }

        for entry in sources.entries() {
            for statement in entry.script().statements() {
                let StatementKind::ModuleExport(export) = statement.kind() else {
                    continue;
                };
                for identifier in &export.names {
                    let name = entry
                        .source()
                        .slice(identifier.span())
                        .expect("parsed identifiers belong to their module source")
                        .to_owned();
                    let names = registry
                        .by_module
                        .get_mut(entry.module())
                        .expect("every registered source has a name table");
                    let Some(declaration_span) = names.locals.get(&name).copied() else {
                        return Err(Box::new(ModuleNameError::UnknownExport {
                            module: entry.module().clone(),
                            name,
                            export_span: identifier.span(),
                        }));
                    };
                    if let Some(first) = names.exports.get(&name) {
                        return Err(Box::new(ModuleNameError::DuplicateExport {
                            module: entry.module().clone(),
                            name,
                            first_span: first.export_span(),
                            duplicate_span: identifier.span(),
                        }));
                    }
                    names.exports.insert(
                        name.clone(),
                        ModuleExport {
                            name,
                            declaration_span,
                            export_span: identifier.span(),
                        },
                    );
                }
            }
        }

        for entry in sources.entries() {
            for statement in entry.script().statements() {
                let StatementKind::Import(import) = statement.kind() else {
                    continue;
                };
                if import.names.is_empty() {
                    continue;
                }
                let edge = graph
                    .imports()
                    .iter()
                    .find(|edge| edge.importer() == entry.module() && edge.span() == import.path)
                    .expect("every parsed import has one resolved graph edge");
                for identifier in &import.names {
                    let name = entry
                        .source()
                        .slice(identifier.span())
                        .expect("parsed identifiers belong to their module source")
                        .to_owned();
                    let target_names = registry
                        .by_module
                        .get(edge.target())
                        .expect("every graph target has a name table");
                    if !target_names.exports.contains_key(&name) {
                        let private_span = target_names.locals.get(&name).copied();
                        return Err(Box::new(ModuleNameError::UnavailableImport {
                            importer: entry.module().clone(),
                            target: edge.target().clone(),
                            name,
                            import_span: identifier.span(),
                            private_span,
                        }));
                    }

                    let importer_names = registry
                        .by_module
                        .get_mut(entry.module())
                        .expect("every registered source has a name table");
                    let conflict = importer_names.locals.get(&name).copied().or_else(|| {
                        importer_names
                            .imports
                            .iter()
                            .find(|imported| imported.name() == name)
                            .map(ModuleNameImport::name_span)
                    });
                    if let Some(first_span) = conflict {
                        return Err(Box::new(ModuleNameError::ImportConflict {
                            module: entry.module().clone(),
                            name,
                            first_span,
                            duplicate_span: identifier.span(),
                        }));
                    }
                    importer_names.imports.push(ModuleNameImport {
                        name,
                        importer: entry.module().clone(),
                        target: edge.target().clone(),
                        name_span: identifier.span(),
                    });
                }
            }
        }

        for entry in sources.entries() {
            let references = ReferenceResolver::new(entry, &registry).resolve()?;
            registry
                .by_module
                .get_mut(entry.module())
                .expect("every registered source has a name table")
                .references = references;
        }

        Ok(registry)
    }
}

struct ReferenceResolver<'a> {
    entry: &'a RegisteredModuleSource,
    scopes: Vec<BTreeMap<String, ModuleReferenceTarget>>,
    references: Vec<ModuleNameReference>,
}

impl<'a> ReferenceResolver<'a> {
    fn new(entry: &'a RegisteredModuleSource, registry: &ModuleNameRegistry) -> Self {
        let mut root = BTreeMap::new();
        for import in registry.imports(entry.module()) {
            let export = registry
                .export(import.target(), import.name())
                .expect("validated imports always retain their target export");
            root.insert(
                import.name().to_owned(),
                ModuleReferenceTarget::Imported {
                    import_span: import.name_span(),
                    target_module: import.target().clone(),
                    declaration_span: export.declaration_span(),
                    export_span: export.export_span(),
                },
            );
        }
        Self {
            entry,
            scopes: vec![root],
            references: Vec::new(),
        }
    }

    fn resolve(mut self) -> Result<Vec<ModuleNameReference>, Box<ModuleNameError>> {
        self.statements(self.entry.script().statements())?;
        Ok(self.references)
    }

    fn statements(&mut self, statements: &[Statement]) -> Result<(), Box<ModuleNameError>> {
        for statement in statements {
            self.statement(statement)?;
        }
        Ok(())
    }

    fn statement(&mut self, statement: &Statement) -> Result<(), Box<ModuleNameError>> {
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                self.expression(&declaration.value)?;
                self.declare(declaration.name.span())
            }
            StatementKind::Assignment(assignment) => {
                self.variable(assignment.target.name.span(), assignment.target.span)?;
                self.expression(&assignment.value)
            }
            StatementKind::Environment(environment) => match environment {
                flash_syntax::EnvironmentStatement::Export { value, .. } => self.expression(value),
                flash_syntax::EnvironmentStatement::Unset { .. } => Ok(()),
            },
            StatementKind::Function(function) => {
                self.ensure_available(function.name.span())?;
                self.push_scope();
                self.insert_local(function.name.span());
                self.push_scope();
                let result = (|| {
                    for parameter in &function.parameters {
                        self.declare(parameter.name.span())?;
                    }
                    self.block(&function.body)
                })();
                self.pop_scope();
                self.pop_scope();
                result?;
                self.insert_local(function.name.span());
                Ok(())
            }
            StatementKind::If(statement) => self.if_statement(statement),
            StatementKind::While(statement) => {
                self.chain(&statement.condition)?;
                self.block(&statement.body)
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable)?;
                self.push_scope();
                let result = (|| {
                    self.declare(statement.binding.span())?;
                    self.statements(&statement.body.statements)
                })();
                self.pop_scope();
                result
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value)?;
                for arm in &statement.arms {
                    self.match_arm(arm)?;
                }
                Ok(())
            }
            StatementKind::Control(control) => match control {
                ControlTransfer::Return(Some(expression)) => self.expression(expression),
                ControlTransfer::Break
                | ControlTransfer::Continue
                | ControlTransfer::Return(None) => Ok(()),
            },
            StatementKind::Job(job) => self.chain(&job.chain),
        }
    }

    fn if_statement(
        &mut self,
        statement: &flash_syntax::IfStatement,
    ) -> Result<(), Box<ModuleNameError>> {
        self.chain(&statement.condition)?;
        self.block(&statement.then_block)?;
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.block(block),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => Ok(()),
        }
    }

    fn match_arm(&mut self, arm: &MatchArm) -> Result<(), Box<ModuleNameError>> {
        self.push_scope();
        let result = (|| {
            match &arm.pattern {
                Pattern::Binding(identifier) => self.declare(identifier.span())?,
                Pattern::Literal(literal) => self.literal(literal)?,
                Pattern::Wildcard(_) => {}
            }
            if let Some(guard) = &arm.guard {
                self.expression(guard)?;
            }
            self.statements(&arm.body.statements)
        })();
        self.pop_scope();
        result
    }

    fn block(&mut self, block: &Block) -> Result<(), Box<ModuleNameError>> {
        self.push_scope();
        let result = self.statements(&block.statements);
        self.pop_scope();
        result
    }

    fn chain(&mut self, chain: &ConditionalChain) -> Result<(), Box<ModuleNameError>> {
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                self.pipeline(pipeline)?;
            }
        }
        Ok(())
    }

    fn pipeline(&mut self, pipeline: &Pipeline) -> Result<(), Box<ModuleNameError>> {
        for stage in pipeline.stages() {
            match stage.kind() {
                StageKind::Command(command) => {
                    self.word(command.head.word())?;
                    for item in &command.items {
                        match item.kind() {
                            CommandItemKind::Word(word) => self.word(word)?,
                            CommandItemKind::Spread(variable) => {
                                self.variable(variable.name.span(), item.span())?;
                            }
                            CommandItemKind::Closure(closure) => self.closure(closure)?,
                            CommandItemKind::Redirection(redirection) => {
                                self.redirection(redirection.kind())?;
                            }
                        }
                    }
                }
                StageKind::Expression(expression) => self.expression(expression)?,
            }
        }
        Ok(())
    }

    fn expression(&mut self, expression: &Expression) -> Result<(), Box<ModuleNameError>> {
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(variable) => {
                self.variable(variable.name.span(), variable.span)
            }
            ExpressionKind::Symbol(_) => Ok(()),
            ExpressionKind::List(elements) => {
                for element in elements {
                    self.expression(element)?;
                }
                Ok(())
            }
            ExpressionKind::Record(entries) => {
                for entry in entries {
                    if let RecordKey::DoubleQuoted(part) = &entry.key {
                        self.word_part(part)?;
                    }
                    self.expression(&entry.value)?;
                }
                Ok(())
            }
            ExpressionKind::Closure(closure) => self.closure(closure),
            ExpressionKind::CommandSubstitution(chain) | ExpressionKind::GroupedJob(chain) => {
                self.chain(chain)
            }
            ExpressionKind::Call(call) => {
                if let ExpressionKind::Symbol(identifier) = call.callee.kind() {
                    self.variable(identifier.span(), call.callee.span())?;
                } else {
                    self.expression(&call.callee)?;
                }
                for argument in &call.arguments {
                    self.expression(argument)?;
                }
                Ok(())
            }
            ExpressionKind::Index(index) => {
                self.expression(&index.target)?;
                self.expression(&index.index)
            }
            ExpressionKind::Member(member) => self.expression(&member.target),
            ExpressionKind::Unary(unary) => self.expression(&unary.operand),
            ExpressionKind::Binary(binary) => {
                self.expression(&binary.left)?;
                self.expression(&binary.right)
            }
        }
    }

    fn closure(&mut self, closure: &Closure) -> Result<(), Box<ModuleNameError>> {
        self.push_scope();
        let result = (|| {
            for parameter in &closure.parameters {
                self.declare(parameter.name.span())?;
            }
            self.chain(&closure.body)
        })();
        self.pop_scope();
        result
    }

    fn literal(&mut self, literal: &flash_syntax::Literal) -> Result<(), Box<ModuleNameError>> {
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            self.word_parts(parts)?;
        }
        Ok(())
    }

    fn word(&mut self, word: &Word) -> Result<(), Box<ModuleNameError>> {
        self.word_parts(word.parts())
    }

    fn word_parts(&mut self, parts: &[WordPart]) -> Result<(), Box<ModuleNameError>> {
        for part in parts {
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&mut self, part: &WordPart) -> Result<(), Box<ModuleNameError>> {
        match part.kind() {
            WordPartKind::Variable(identifier) => self.variable(identifier.span(), part.span()),
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(chain) => self.chain(chain),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape => Ok(()),
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind) -> Result<(), Box<ModuleNameError>> {
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => Ok(()),
        }
    }

    fn variable(
        &mut self,
        name_span: Span,
        reference_span: Span,
    ) -> Result<(), Box<ModuleNameError>> {
        let name = self.text(name_span).to_owned();
        let Some(target) = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(&name))
            .cloned()
        else {
            return Err(Box::new(ModuleNameError::UnknownReference {
                module: self.entry.module().clone(),
                name,
                reference_span,
            }));
        };
        self.references.push(ModuleNameReference {
            name,
            reference_span,
            target,
        });
        Ok(())
    }

    fn ensure_available(&self, declaration_span: Span) -> Result<(), Box<ModuleNameError>> {
        let name = self.text(declaration_span);
        let scope = self
            .scopes
            .last()
            .expect("reference resolution always retains a root scope");
        if let Some(first) = scope.get(name) {
            return Err(Box::new(ModuleNameError::DuplicateBinding {
                module: self.entry.module().clone(),
                name: name.to_owned(),
                first_span: binding_span(first),
                duplicate_span: declaration_span,
            }));
        }
        Ok(())
    }

    fn declare(&mut self, declaration_span: Span) -> Result<(), Box<ModuleNameError>> {
        self.ensure_available(declaration_span)?;
        self.insert_local(declaration_span);
        Ok(())
    }

    fn insert_local(&mut self, declaration_span: Span) {
        let name = self.text(declaration_span).to_owned();
        self.scopes
            .last_mut()
            .expect("reference resolution always retains a root scope")
            .insert(
                name,
                ModuleReferenceTarget::Local {
                    module: self.entry.module().clone(),
                    declaration_span,
                },
            );
    }

    fn push_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes
            .pop()
            .expect("each reference-analysis scope is popped exactly once");
    }

    fn text(&self, span: Span) -> &str {
        self.entry
            .source()
            .slice(span)
            .expect("parsed syntax spans belong to their module source")
    }
}

fn binding_span(target: &ModuleReferenceTarget) -> Span {
    match target {
        ModuleReferenceTarget::Local {
            declaration_span, ..
        } => *declaration_span,
        ModuleReferenceTarget::Imported { import_span, .. } => *import_span,
    }
}

impl ModuleSourceRegistry {
    /// The number of unique canonical source modules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no source module has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The registered source for a canonical module.
    #[must_use]
    pub fn source(&self, module: &ModuleId) -> Option<&SourceFile> {
        self.entry(module).map(RegisteredModuleSource::source)
    }

    /// The parsed syntax for a canonical module.
    #[must_use]
    pub fn script(&self, module: &ModuleId) -> Option<&Script> {
        self.entry(module).map(RegisteredModuleSource::script)
    }

    /// The canonical module assigned to a stable source identity.
    #[must_use]
    pub fn module(&self, source: SourceId) -> Option<&ModuleId> {
        self.by_source
            .get(&source)
            .and_then(|index| self.entries.get(*index))
            .map(RegisteredModuleSource::module)
    }

    /// Entries in deterministic first-visit depth-first order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &RegisteredModuleSource> {
        self.entries.iter()
    }

    fn entry(&self, module: &ModuleId) -> Option<&RegisteredModuleSource> {
        self.by_module
            .get(module)
            .and_then(|index| self.entries.get(*index))
    }

    fn next_source_id(&self) -> Result<SourceId, ModuleProgramError> {
        u32::try_from(self.entries.len())
            .map(SourceId::new)
            .map_err(|_| ModuleProgramError::SourceIdentityExhausted)
    }

    fn register(&mut self, module: ModuleId, source: SourceFile, script: Script) {
        debug_assert!(!self.by_module.contains_key(&module));
        debug_assert!(!self.by_source.contains_key(&source.id()));
        let index = self.entries.len();
        self.by_module.insert(module.clone(), index);
        self.by_source.insert(source.id(), index);
        self.entries.push(RegisteredModuleSource {
            module,
            source,
            script,
        });
    }
}

/// A completely loaded, parsed, and canonically graphed Flash source program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleProgram {
    graph: ModuleGraph,
    sources: ModuleSourceRegistry,
    names: ModuleNameRegistry,
}

/// A module-program construction failure rendered while its analyzed sources
/// are still available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleProgramLoadError {
    error: Box<ModuleProgramError>,
    rendered: String,
}

impl ModuleProgramLoadError {
    fn new(error: ModuleProgramError, sources: &ModuleSourceRegistry) -> Self {
        let diagnostics = error.diagnostics();
        let mut available = sources
            .entries()
            .map(RegisteredModuleSource::source)
            .collect::<Vec<_>>();
        if let ModuleProgramError::Syntax { source, .. } = &error {
            available.push(source.as_ref());
        }
        let rendered = if diagnostics.is_empty() {
            format!("{error}\n")
        } else {
            diagnostics
                .iter()
                .map(|diagnostic| {
                    render_diagnostic_sources(available.iter().copied(), diagnostic)
                        .expect("module diagnostics reference retained analyzed sources")
                })
                .collect()
        };
        Self {
            error: Box::new(error),
            rendered,
        }
    }

    /// The structured construction failure.
    #[must_use]
    pub fn error(&self) -> &ModuleProgramError {
        self.error.as_ref()
    }

    /// The complete registry-backed user-facing diagnostic.
    #[must_use]
    pub fn render(&self) -> &str {
        &self.rendered
    }
}

impl fmt::Display for ModuleProgramLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ModuleProgramLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl ModuleProgram {
    /// The canonical acyclic dependency graph.
    #[must_use]
    pub const fn graph(&self) -> &ModuleGraph {
        &self.graph
    }

    /// Every canonical module's source and parsed syntax.
    #[must_use]
    pub const fn sources(&self) -> &ModuleSourceRegistry {
        &self.sources
    }

    /// Static local, export, explicit-import, and lexical-reference tables.
    #[must_use]
    pub const fn names(&self) -> &ModuleNameRegistry {
        &self.names
    }
}

/// Recursively resolves, reads, decodes, parses, and graphs Flash modules.
pub struct ModuleProgramLoader<'a> {
    resolver: ModuleResolver<'a>,
    source_loader: &'a dyn ModuleSourceLoader,
}

impl<'a> ModuleProgramLoader<'a> {
    /// Creates a program loader over injected path and source capabilities.
    #[must_use]
    pub const fn new(
        canonicalizer: &'a dyn ModuleCanonicalizer,
        source_loader: &'a dyn ModuleSourceLoader,
    ) -> Self {
        Self {
            resolver: ModuleResolver::new(canonicalizer),
            source_loader,
        }
    }

    /// Loads one root and every reachable static import without executing any
    /// source statement.
    pub fn load(&self, requested: &Path) -> Result<ModuleProgram, ModuleProgramError> {
        self.load_retaining_sources(requested).map_err(|failure| {
            let (error, _) = *failure;
            error
        })
    }

    /// Loads one module program and retains enough source context to render a
    /// frontend diagnostic if construction fails.
    pub fn load_for_frontend(
        &self,
        requested: &Path,
    ) -> Result<ModuleProgram, ModuleProgramLoadError> {
        self.load_retaining_sources(requested).map_err(|failure| {
            let (error, sources) = *failure;
            ModuleProgramLoadError::new(error, &sources)
        })
    }

    fn load_retaining_sources(
        &self,
        requested: &Path,
    ) -> Result<ModuleProgram, Box<(ModuleProgramError, ModuleSourceRegistry)>> {
        let root = match self.resolver.resolve_root(requested) {
            Ok(root) => root,
            Err(error) => {
                return Err(Box::new((
                    ModuleProgramError::Resolution(error),
                    ModuleSourceRegistry::default(),
                )));
            }
        };
        let mut graph = ModuleGraph::new(root.clone());
        let mut sources = ModuleSourceRegistry::default();
        if let Err(error) = self.load_module(root, None, &mut graph, &mut sources) {
            return Err(Box::new((error, sources)));
        }
        let names = match ModuleNameRegistry::analyze(&graph, &sources) {
            Ok(names) => names,
            Err(error) => return Err(Box::new((ModuleProgramError::Names(error), sources))),
        };
        Ok(ModuleProgram {
            graph,
            sources,
            names,
        })
    }

    fn load_module(
        &self,
        module: ModuleId,
        imported_by: Option<ModuleImport>,
        graph: &mut ModuleGraph,
        sources: &mut ModuleSourceRegistry,
    ) -> Result<(), ModuleProgramError> {
        if sources.source(&module).is_some() {
            return Ok(());
        }

        let bytes =
            self.source_loader
                .load(&module)
                .map_err(|cause| ModuleProgramError::SourceRead {
                    module: module.clone(),
                    imported_by: imported_by.clone().map(Box::new),
                    cause,
                })?;
        let source_id = sources.next_source_id()?;
        let source_name = module.path().to_string_lossy().into_owned();
        let source = SourceFile::from_bytes(source_id, source_name, bytes).map_err(|error| {
            ModuleProgramError::InvalidUtf8 {
                module: module.clone(),
                imported_by: imported_by.clone().map(Box::new),
                valid_up_to: error.utf8_error().valid_up_to(),
            }
        })?;
        let script = match parse(&source) {
            ParseOutcome::Complete(script) => script,
            ParseOutcome::Incomplete(incomplete) => {
                let diagnostic = Diagnostic::new(
                    Severity::Error,
                    "SYN002",
                    format!("incomplete input: {}", incomplete.reason()),
                )
                .with_primary(
                    incomplete.span(),
                    "input ends before this construct is complete",
                );
                return Err(ModuleProgramError::Syntax {
                    module,
                    source: Box::new(source),
                    diagnostics: vec![diagnostic],
                });
            }
            ParseOutcome::Invalid(diagnostics) => {
                return Err(ModuleProgramError::Syntax {
                    module,
                    source: Box::new(source),
                    diagnostics,
                });
            }
        };

        let imports = script
            .statements()
            .iter()
            .filter_map(|statement| match statement.kind() {
                StatementKind::Import(import) => Some(import.path),
                _ => None,
            })
            .map(|span| {
                let quoted = source
                    .slice(span)
                    .expect("parsed import spans belong to their source");
                (PathBuf::from(&quoted[1..quoted.len() - 1]), span)
            })
            .collect::<Vec<_>>();
        sources.register(module.clone(), source, script);

        for (requested, span) in imports {
            let import = self
                .resolver
                .resolve_import(&module, &requested, span)
                .map_err(ModuleProgramError::Resolution)?;
            graph
                .add_import(import.clone())
                .map_err(ModuleProgramError::Graph)?;
            self.load_module(import.target().clone(), Some(import), graph, sources)?;
        }
        Ok(())
    }
}

/// A failure while building explicit module export/import-name tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleNameError {
    /// An export list names no top-level lexical declaration or function.
    UnknownExport {
        module: ModuleId,
        name: String,
        export_span: Span,
    },
    /// One module exports the same name more than once.
    DuplicateExport {
        module: ModuleId,
        name: String,
        first_span: Span,
        duplicate_span: Span,
    },
    /// A target module does not explicitly export the requested name.
    UnavailableImport {
        importer: ModuleId,
        target: ModuleId,
        name: String,
        import_span: Span,
        private_span: Option<Span>,
    },
    /// An imported binding conflicts with a local or earlier imported name.
    ImportConflict {
        module: ModuleId,
        name: String,
        first_span: Span,
        duplicate_span: Span,
    },
    /// A lexical read has no binding visible at its source position.
    UnknownReference {
        module: ModuleId,
        name: String,
        reference_span: Span,
    },
    /// One static lexical scope declares the same binding more than once.
    DuplicateBinding {
        module: ModuleId,
        name: String,
        first_span: Span,
        duplicate_span: Span,
    },
}

impl ModuleNameError {
    /// The module whose declaration directly failed.
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        match self {
            Self::UnknownExport { module, .. }
            | Self::DuplicateExport { module, .. }
            | Self::ImportConflict { module, .. }
            | Self::UnknownReference { module, .. }
            | Self::DuplicateBinding { module, .. } => module,
            Self::UnavailableImport { importer, .. } => importer,
        }
    }

    /// The structured source diagnostic for this name failure.
    #[must_use]
    pub fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::UnknownExport {
                export_span, name, ..
            } => Diagnostic::new(Severity::Error, "MOD005", self.to_string()).with_primary(
                *export_span,
                format!("no top-level declaration or function named `{name}`"),
            ),
            Self::DuplicateExport {
                first_span,
                duplicate_span,
                ..
            } => Diagnostic::new(Severity::Error, "MOD006", self.to_string())
                .with_primary(*duplicate_span, "this name is exported again")
                .with_secondary(*first_span, "first exported here"),
            Self::UnavailableImport {
                import_span,
                private_span,
                ..
            } => {
                let diagnostic = Diagnostic::new(Severity::Error, "MOD007", self.to_string())
                    .with_primary(
                        *import_span,
                        "this name is not exported by the target module",
                    );
                if let Some(span) = private_span {
                    diagnostic.with_secondary(*span, "a private declaration exists here")
                } else {
                    diagnostic
                }
            }
            Self::ImportConflict {
                first_span,
                duplicate_span,
                ..
            } => Diagnostic::new(Severity::Error, "MOD008", self.to_string())
                .with_primary(*duplicate_span, "this imported binding conflicts")
                .with_secondary(*first_span, "the name is already bound here"),
            Self::UnknownReference {
                name,
                reference_span,
                ..
            } => Diagnostic::new(Severity::Error, "MOD009", self.to_string()).with_primary(
                *reference_span,
                format!("no lexical binding named `{name}` is visible here"),
            ),
            Self::DuplicateBinding {
                first_span,
                duplicate_span,
                ..
            } => Diagnostic::new(Severity::Error, "MOD010", self.to_string())
                .with_primary(*duplicate_span, "this binding is declared again")
                .with_secondary(*first_span, "the binding was first declared here"),
        }
    }
}

impl fmt::Display for ModuleNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownExport { module, name, .. } => write!(
                formatter,
                "module `{}` cannot export unknown name `{name}`",
                module.path().display()
            ),
            Self::DuplicateExport { module, name, .. } => write!(
                formatter,
                "module `{}` exports `{name}` more than once",
                module.path().display()
            ),
            Self::UnavailableImport { target, name, .. } => write!(
                formatter,
                "module `{}` does not export `{name}`",
                target.path().display()
            ),
            Self::ImportConflict { module, name, .. } => write!(
                formatter,
                "module `{}` imports conflicting name `{name}`",
                module.path().display()
            ),
            Self::UnknownReference { module, name, .. } => write!(
                formatter,
                "module `{}` references unknown binding `{name}`",
                module.path().display()
            ),
            Self::DuplicateBinding { module, name, .. } => write!(
                formatter,
                "module `{}` declares binding `{name}` more than once in one scope",
                module.path().display()
            ),
        }
    }
}

impl std::error::Error for ModuleNameError {}

/// A failure while constructing a recursively parsed module program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleProgramError {
    /// A root or import request could not be canonicalized.
    Resolution(ModuleResolutionError),
    /// Canonical source bytes could not be read.
    SourceRead {
        module: ModuleId,
        imported_by: Option<Box<ModuleImport>>,
        cause: ModuleSourceError,
    },
    /// A source module was not valid UTF-8.
    InvalidUtf8 {
        module: ModuleId,
        imported_by: Option<Box<ModuleImport>>,
        valid_up_to: usize,
    },
    /// A loaded module was incomplete or invalid Flash syntax.
    Syntax {
        module: ModuleId,
        source: Box<SourceFile>,
        diagnostics: Vec<Diagnostic>,
    },
    /// An import violated the canonical graph contract.
    Graph(ModuleGraphError),
    /// Static module export/import-name analysis failed.
    Names(Box<ModuleNameError>),
    /// More source identities were required than `SourceId` can represent.
    SourceIdentityExhausted,
}

impl ModuleProgramError {
    /// The source diagnostics represented by this failure.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        match self {
            Self::Resolution(error) => error.diagnostic().into_iter().collect(),
            Self::SourceRead { imported_by, .. } => imported_by
                .as_ref()
                .map(|import| {
                    Diagnostic::new(Severity::Error, "MOD003", self.to_string())
                        .with_primary(import.span(), "this module source could not be read")
                })
                .into_iter()
                .collect(),
            Self::InvalidUtf8 { imported_by, .. } => imported_by
                .as_ref()
                .map(|import| {
                    Diagnostic::new(Severity::Error, "MOD004", self.to_string())
                        .with_primary(import.span(), "this module source is not valid UTF-8")
                })
                .into_iter()
                .collect(),
            Self::Syntax { diagnostics, .. } => diagnostics.clone(),
            Self::Graph(ModuleGraphError::Cycle(cycle)) => vec![cycle.diagnostic()],
            Self::Names(error) => vec![error.diagnostic()],
            Self::Graph(ModuleGraphError::UnknownImporter(_)) | Self::SourceIdentityExhausted => {
                Vec::new()
            }
        }
    }

    /// The canonical module directly associated with the failure, when one is
    /// available.
    #[must_use]
    pub const fn module(&self) -> Option<&ModuleId> {
        match self {
            Self::SourceRead { module, .. }
            | Self::InvalidUtf8 { module, .. }
            | Self::Syntax { module, .. } => Some(module),
            Self::Names(error) => Some(error.module()),
            Self::Resolution(_) | Self::Graph(_) | Self::SourceIdentityExhausted => None,
        }
    }
}

impl fmt::Display for ModuleProgramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolution(error) => error.fmt(formatter),
            Self::SourceRead { module, cause, .. } => write!(
                formatter,
                "cannot read module `{}`: {cause}",
                module.path().display()
            ),
            Self::InvalidUtf8 {
                module,
                valid_up_to,
                ..
            } => write!(
                formatter,
                "module `{}` is not UTF-8 at byte {valid_up_to}",
                module.path().display()
            ),
            Self::Syntax { module, .. } => write!(
                formatter,
                "module `{}` contains invalid Flash syntax",
                module.path().display()
            ),
            Self::Graph(error) => error.fmt(formatter),
            Self::Names(error) => error.fmt(formatter),
            Self::SourceIdentityExhausted => {
                formatter.write_str("module program exhausted stable source identities")
            }
        }
    }
}

impl std::error::Error for ModuleProgramError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resolution(error) => Some(error),
            Self::SourceRead { cause, .. } => Some(cause),
            Self::Graph(error) => Some(error),
            Self::Names(error) => Some(error),
            Self::InvalidUtf8 { .. } | Self::Syntax { .. } | Self::SourceIdentityExhausted => None,
        }
    }
}

/// A canonically identified, source-ordered module dependency graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleGraph {
    root: ModuleId,
    modules: BTreeSet<ModuleId>,
    imports: Vec<ModuleImport>,
    outgoing: BTreeMap<ModuleId, Vec<usize>>,
}

impl ModuleGraph {
    /// Starts a graph with one canonical root module.
    #[must_use]
    pub fn new(root: ModuleId) -> Self {
        Self {
            modules: BTreeSet::from([root.clone()]),
            root,
            imports: Vec::new(),
            outgoing: BTreeMap::new(),
        }
    }

    /// The graph's entry module.
    #[must_use]
    pub const fn root(&self) -> &ModuleId {
        &self.root
    }

    /// Every unique canonical module in native-path order.
    pub fn modules(&self) -> impl ExactSizeIterator<Item = &ModuleId> {
        self.modules.iter()
    }

    /// Every accepted explicit import in insertion/source traversal order.
    #[must_use]
    pub fn imports(&self) -> &[ModuleImport] {
        &self.imports
    }

    /// Adds one resolved import if its importer is known and it does not close a
    /// cycle. A rejected edge leaves the graph unchanged.
    pub fn add_import(&mut self, import: ModuleImport) -> Result<(), ModuleGraphError> {
        if !self.modules.contains(import.importer()) {
            return Err(ModuleGraphError::UnknownImporter(import.importer().clone()));
        }

        if let Some(path) = self.path_between(import.target(), import.importer()) {
            let mut imports = Vec::with_capacity(path.len() + 1);
            imports.push(import);
            imports.extend(path);
            return Err(ModuleGraphError::Cycle(ModuleCycle { imports }));
        }

        let importer = import.importer().clone();
        let target = import.target().clone();
        let index = self.imports.len();
        self.imports.push(import);
        self.outgoing.entry(importer).or_default().push(index);
        self.modules.insert(target);
        Ok(())
    }

    fn path_between(&self, start: &ModuleId, target: &ModuleId) -> Option<Vec<ModuleImport>> {
        let mut visited = BTreeSet::new();
        self.path_between_inner(start, target, &mut visited)
    }

    fn path_between_inner(
        &self,
        current: &ModuleId,
        target: &ModuleId,
        visited: &mut BTreeSet<ModuleId>,
    ) -> Option<Vec<ModuleImport>> {
        if current == target {
            return Some(Vec::new());
        }
        if !visited.insert(current.clone()) {
            return None;
        }

        for index in self.outgoing.get(current).into_iter().flatten() {
            let import = &self.imports[*index];
            if let Some(mut remainder) = self.path_between_inner(import.target(), target, visited) {
                let mut path = Vec::with_capacity(remainder.len() + 1);
                path.push(import.clone());
                path.append(&mut remainder);
                return Some(path);
            }
        }
        None
    }
}

/// A failure to add an import edge to a [`ModuleGraph`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleGraphError {
    /// The edge's importing module has not been reached by this graph.
    UnknownImporter(ModuleId),
    /// The edge would close an import cycle.
    Cycle(ModuleCycle),
}

impl fmt::Display for ModuleGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownImporter(importer) => write!(
                formatter,
                "module graph does not contain importer `{}`",
                importer.path().display()
            ),
            Self::Cycle(cycle) => cycle.fmt(formatter),
        }
    }
}

impl std::error::Error for ModuleGraphError {}

/// The ordered import edges that close one cycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleCycle {
    imports: Vec<ModuleImport>,
}

impl ModuleCycle {
    /// The newly rejected edge first, followed by the existing path back to its
    /// importer.
    #[must_use]
    pub fn imports(&self) -> &[ModuleImport] {
        &self.imports
    }

    /// Builds a multi-source structured cycle diagnostic.
    ///
    /// The rejected edge is primary and the existing cycle edges are secondary.
    /// Rendering clients may group the labels by [`Span::source_id`].
    #[must_use]
    pub fn diagnostic(&self) -> Diagnostic {
        let first = self
            .imports
            .first()
            .expect("a module cycle always contains its rejected edge");
        let mut diagnostic = Diagnostic::new(Severity::Error, "MOD002", self.to_string())
            .with_primary(first.span(), "this import closes the cycle");
        for import in &self.imports[1..] {
            diagnostic = diagnostic.with_secondary(import.span(), "cycle continues here");
        }
        diagnostic
    }
}

impl fmt::Display for ModuleCycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("module import cycle: ")?;
        let first = self
            .imports
            .first()
            .expect("a module cycle always contains its rejected edge");
        write!(formatter, "{}", first.importer().path().display())?;
        for import in &self.imports {
            write!(formatter, " -> {}", import.target().path().display())?;
        }
        Ok(())
    }
}
