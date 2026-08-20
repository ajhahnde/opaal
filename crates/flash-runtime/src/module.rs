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

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flash_syntax::{
    BinaryOperator, Block, Closure, CommandHeadKind, CommandItemKind, ConditionalChain,
    ControlTransfer, ControlledParseOutcome, Diagnostic, ElseBranch, Expression, ExpressionKind,
    LiteralKind, MatchArm, ParseOutcome, Pattern, Pipeline, RecordKey, RedirectionKind, Script,
    Severity, SourceFile, SourceId, Span, StageKind, Statement, StatementKind, TypeReference,
    UnaryOperator, Word, WordPart, WordPartKind, parse_with_control, render_diagnostic_sources,
};

use crate::Value;
use crate::builtin::standard_registry;
use crate::carrier::{
    CarrierBridge, PipelineCarrierFault, StageCarrierContract, analyze_pipeline_carriers,
};
use crate::command::{
    Carrier, CommandClassification, CommandLifecycle, CommandOutput, CommandRegistry,
};
use crate::documentation::Documentation;
use crate::intrinsic::{DynamicBinding, ExpressionIntrinsic};

/// Host-free cooperative cancellation for static source analysis.
#[derive(Clone)]
pub struct AnalysisControl {
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl AnalysisControl {
    /// A control that preserves legacy checker and execution analysis behavior.
    #[must_use]
    pub fn never() -> Self {
        Self::cooperative(|| false)
    }

    /// Creates a control backed by a thread-safe cancellation predicate.
    ///
    /// The predicate must remain `true` after it first requests cancellation.
    #[must_use]
    pub fn cooperative(predicate: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            is_cancelled: Arc::new(predicate),
        }
    }

    /// Whether the current analysis should stop at its next polling boundary.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        (self.is_cancelled)()
    }
}

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
    /// The reserved live current-status value supplied by the evaluation host.
    DynamicStatus,
    /// The synthetic immutable `args: List[String]` root-program input.
    ScriptArguments,
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
    visible: Vec<ModuleVisibleBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModuleVisibleBinding {
    name: String,
    target: ModuleReferenceTarget,
    scope_span: Span,
    visible_from: usize,
    depth: usize,
}

impl ModuleVisibleBinding {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn target(&self) -> &ModuleReferenceTarget {
        &self.target
    }
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

    pub(crate) fn visible_bindings(
        &self,
        module: &ModuleId,
        cursor: usize,
    ) -> Vec<&ModuleVisibleBinding> {
        let Some(names) = self.by_module.get(module) else {
            return Vec::new();
        };
        let mut selected = BTreeMap::<&str, &ModuleVisibleBinding>::new();
        for binding in &names.visible {
            if binding.scope_span.start() <= cursor
                && cursor <= binding.scope_span.end()
                && binding.visible_from <= cursor
            {
                let replace = selected.get(binding.name()).is_none_or(|current| {
                    (binding.depth, binding.visible_from) > (current.depth, current.visible_from)
                });
                if replace {
                    selected.insert(binding.name(), binding);
                }
            }
        }
        selected.into_values().collect()
    }

    pub(crate) fn target_at(
        &self,
        module: &ModuleId,
        offset: usize,
    ) -> Option<(String, ModuleReferenceTarget)> {
        let names = self.by_module.get(module)?;
        if let Some(reference) = names
            .references
            .iter()
            .find(|reference| span_contains(reference.reference_span(), offset))
        {
            return Some((reference.name.clone(), reference.target.clone()));
        }
        if let Some(import) = names
            .imports
            .iter()
            .find(|import| span_contains(import.name_span(), offset))
        {
            let export = self
                .export(import.target(), import.name())
                .expect("validated imports retain their target export");
            return Some((
                import.name.clone(),
                ModuleReferenceTarget::Imported {
                    import_span: import.name_span(),
                    target_module: import.target().clone(),
                    declaration_span: export.declaration_span(),
                    export_span: export.export_span(),
                },
            ));
        }
        if let Some((name, declaration_span)) = names
            .visible
            .iter()
            .filter_map(|binding| match &binding.target {
                ModuleReferenceTarget::Local {
                    module: target_module,
                    declaration_span,
                } if target_module == module => Some((binding.name(), *declaration_span)),
                ModuleReferenceTarget::ScriptArguments
                | ModuleReferenceTarget::DynamicStatus
                | ModuleReferenceTarget::Local { .. }
                | ModuleReferenceTarget::Imported { .. } => None,
            })
            .find(|(_, span)| span_contains(*span, offset))
        {
            return Some((
                name.to_owned(),
                ModuleReferenceTarget::Local {
                    module: module.clone(),
                    declaration_span,
                },
            ));
        }
        names
            .exports
            .values()
            .find(|export| span_contains(export.export_span(), offset))
            .map(|export| {
                (
                    export.name.clone(),
                    ModuleReferenceTarget::Local {
                        module: module.clone(),
                        declaration_span: export.declaration_span(),
                    },
                )
            })
    }

    fn analyze(
        graph: &ModuleGraph,
        sources: &ModuleSourceRegistry,
        control: &AnalysisControl,
    ) -> Result<Self, Vec<ModuleNameError>> {
        let mut registry = Self::default();
        let mut errors = Vec::new();
        let mut poisoned_exports = BTreeMap::<ModuleId, BTreeSet<String>>::new();
        let mut poisoned_imports = BTreeMap::<ModuleId, BTreeSet<String>>::new();

        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
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
                if DynamicBinding::lookup(&name).is_some() {
                    errors.push(ModuleNameError::ReservedBinding {
                        module: entry.module().clone(),
                        name,
                        declaration_span: identifier.span(),
                    });
                    continue;
                }
                names.locals.entry(name).or_insert(identifier.span());
            }
            registry.by_module.insert(entry.module().clone(), names);
        }

        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
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
                        poisoned_exports
                            .entry(entry.module().clone())
                            .or_default()
                            .insert(name.clone());
                        errors.push(ModuleNameError::UnknownExport {
                            module: entry.module().clone(),
                            name,
                            export_span: identifier.span(),
                        });
                        continue;
                    };
                    if let Some(first) = names.exports.get(&name) {
                        errors.push(ModuleNameError::DuplicateExport {
                            module: entry.module().clone(),
                            name,
                            first_span: first.export_span(),
                            duplicate_span: identifier.span(),
                        });
                        continue;
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
            if control.is_cancelled() {
                return Ok(registry);
            }
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
                    if DynamicBinding::lookup(&name).is_some() {
                        errors.push(ModuleNameError::ReservedBinding {
                            module: entry.module().clone(),
                            name,
                            declaration_span: identifier.span(),
                        });
                        continue;
                    }
                    let target_names = registry
                        .by_module
                        .get(edge.target())
                        .expect("every graph target has a name table");
                    if !target_names.exports.contains_key(&name) {
                        let target_is_poisoned = poisoned_exports
                            .get(edge.target())
                            .is_some_and(|names| names.contains(&name));
                        if !target_is_poisoned {
                            let private_span = target_names.locals.get(&name).copied();
                            errors.push(ModuleNameError::UnavailableImport {
                                importer: entry.module().clone(),
                                target: edge.target().clone(),
                                name: name.clone(),
                                import_span: identifier.span(),
                                private_span,
                            });
                        }
                        poisoned_imports
                            .entry(entry.module().clone())
                            .or_default()
                            .insert(name);
                        continue;
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
                        errors.push(ModuleNameError::ImportConflict {
                            module: entry.module().clone(),
                            name,
                            first_span,
                            duplicate_span: identifier.span(),
                        });
                        continue;
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
            if control.is_cancelled() {
                return Ok(registry);
            }
            let (references, visible, mut reference_errors) = ReferenceResolver::new(
                entry,
                &registry,
                poisoned_imports.get(entry.module()),
                entry.module() == graph.root(),
                control,
            )
            .resolve();
            errors.append(&mut reference_errors);
            let names = registry
                .by_module
                .get_mut(entry.module())
                .expect("every registered source has a name table");
            names.references = references;
            names.visible = visible;
        }

        if errors.is_empty() {
            Ok(registry)
        } else {
            Err(errors)
        }
    }
}

/// A resolved value type from Flash's closed built-in annotation namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueType {
    Any,
    Null,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    Path,
    Duration,
    ByteSize,
    List(Box<Self>),
    Record,
    Table,
    Range,
    Status,
    Function,
    Closure,
}

impl ValueType {
    /// Whether one runtime value belongs to this exact resolved value family.
    ///
    /// `Any` accepts every value and `List[T]` recursively checks every element.
    /// No numeric, text, path, or callable-family conversion is attempted.
    #[must_use]
    pub fn accepts(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Any, _) => true,
            (Self::Null, Value::Null) => true,
            (Self::Bool, Value::Bool(_)) => true,
            (Self::Int, Value::Int(_)) => true,
            (Self::Float, Value::Float(_)) => true,
            (Self::String, Value::String(_)) => true,
            (Self::Bytes, Value::Bytes(_)) => true,
            (Self::Path, Value::Path(_)) => true,
            (Self::Duration, Value::Duration(_)) => true,
            (Self::ByteSize, Value::ByteSize(_)) => true,
            (Self::List(element_type), Value::List(values)) => {
                values.iter().all(|value| element_type.accepts(value))
            }
            (Self::Record, Value::Record(_)) => true,
            (Self::Table, Value::Table(_)) => true,
            (Self::Range, Value::Range(_)) => true,
            (Self::Status, Value::Status(_)) => true,
            (Self::Function, Value::Callable(callable)) => callable.family() == "function",
            (Self::Closure, Value::Callable(callable)) => callable.family() == "closure",
            _ => false,
        }
    }

    fn accepts_type(&self, actual: &Self) -> bool {
        match (self, actual) {
            (Self::Any, _) => true,
            (Self::List(expected), Self::List(actual)) => expected.accepts_type(actual),
            _ => self == actual,
        }
    }
}

impl fmt::Display for ValueType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => formatter.write_str("Any"),
            Self::Null => formatter.write_str("Null"),
            Self::Bool => formatter.write_str("Bool"),
            Self::Int => formatter.write_str("Int"),
            Self::Float => formatter.write_str("Float"),
            Self::String => formatter.write_str("String"),
            Self::Bytes => formatter.write_str("Bytes"),
            Self::Path => formatter.write_str("Path"),
            Self::Duration => formatter.write_str("Duration"),
            Self::ByteSize => formatter.write_str("ByteSize"),
            Self::List(element) => write!(formatter, "List[{element}]"),
            Self::Record => formatter.write_str("Record"),
            Self::Table => formatter.write_str("Table"),
            Self::Range => formatter.write_str("Range"),
            Self::Status => formatter.write_str("Status"),
            Self::Function => formatter.write_str("Function"),
            Self::Closure => formatter.write_str("Closure"),
        }
    }
}

/// One source annotation resolved to a built-in value type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTypeAnnotation {
    span: Span,
    value_type: ValueType,
}

impl ResolvedTypeAnnotation {
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }

    #[must_use]
    pub const fn value_type(&self) -> &ValueType {
        &self.value_type
    }
}

/// One resolved named-function parameter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionParameterSignature {
    name: String,
    declaration_span: Span,
    value_type: ValueType,
    annotation_span: Option<Span>,
}

impl FunctionParameterSignature {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn declaration_span(&self) -> Span {
        self.declaration_span
    }

    #[must_use]
    pub const fn value_type(&self) -> &ValueType {
        &self.value_type
    }

    #[must_use]
    pub const fn annotation_span(&self) -> Option<Span> {
        self.annotation_span
    }
}

/// A named function's resolved non-executing signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionSignature {
    name: String,
    declaration_span: Span,
    parameters: Vec<FunctionParameterSignature>,
    result: ValueType,
    result_annotation_span: Option<Span>,
    documentation: Option<Documentation>,
}

impl FunctionSignature {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn declaration_span(&self) -> Span {
        self.declaration_span
    }

    #[must_use]
    pub fn parameters(&self) -> &[FunctionParameterSignature] {
        &self.parameters
    }

    #[must_use]
    pub const fn result(&self) -> &ValueType {
        &self.result
    }

    #[must_use]
    pub const fn result_annotation_span(&self) -> Option<Span> {
        self.result_annotation_span
    }

    /// Normalized documentation attached to this named definition.
    #[must_use]
    pub const fn documentation(&self) -> Option<&Documentation> {
        self.documentation.as_ref()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModuleTypes {
    annotations: Vec<ResolvedTypeAnnotation>,
    functions: Vec<FunctionSignature>,
    bindings: Vec<ResolvedBindingType>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedBindingType {
    declaration_span: Span,
    value_type: ValueType,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimeBindingTypes {
    by_source: BTreeMap<SourceId, Vec<ResolvedBindingType>>,
    functions_by_source: BTreeMap<SourceId, Vec<FunctionSignature>>,
}

impl RuntimeBindingTypes {
    pub(crate) fn binding_type(
        &self,
        source: SourceId,
        declaration_span: Span,
    ) -> Option<&ValueType> {
        self.by_source
            .get(&source)
            .and_then(|bindings| {
                bindings
                    .iter()
                    .find(|binding| binding.declaration_span == declaration_span)
            })
            .map(|binding| &binding.value_type)
    }

    pub(crate) fn function_result_type(
        &self,
        source: SourceId,
        declaration_span: Span,
    ) -> Option<&ValueType> {
        self.functions_by_source
            .get(&source)
            .and_then(|functions| {
                functions
                    .iter()
                    .find(|function| function.declaration_span() == declaration_span)
            })
            .map(FunctionSignature::result)
    }

    pub(crate) fn function_signature(
        &self,
        source: SourceId,
        declaration_span: Span,
    ) -> Option<&FunctionSignature> {
        self.functions_by_source.get(&source).and_then(|functions| {
            functions
                .iter()
                .find(|function| function.declaration_span() == declaration_span)
        })
    }

    pub(crate) fn analyze_source(
        source: &SourceFile,
        script: &Script,
    ) -> Result<Self, Box<ModuleTypeError>> {
        let entry = RegisteredModuleSource {
            module: ModuleId(PathBuf::from(source.name())),
            source: source.clone(),
            script: script.clone(),
        };
        let (types, errors) = TypeCollector::new(&entry, &AnalysisControl::never()).collect();
        if let Some(error) = errors.into_iter().next() {
            return Err(Box::new(error));
        }
        Ok(Self {
            by_source: BTreeMap::from([(source.id(), types.bindings)]),
            functions_by_source: BTreeMap::from([(source.id(), types.functions)]),
        })
    }
}

/// Resolved annotations and named-function signatures by canonical module.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleTypeRegistry {
    by_module: BTreeMap<ModuleId, ModuleTypes>,
}

impl ModuleTypeRegistry {
    /// Resolved annotations in deterministic source traversal order.
    #[must_use]
    pub fn annotations(&self, module: &ModuleId) -> &[ResolvedTypeAnnotation] {
        self.by_module
            .get(module)
            .map_or(&[], |types| types.annotations.as_slice())
    }

    /// Looks up the annotation occupying one exact complete source span.
    #[must_use]
    pub fn annotation(
        &self,
        module: &ModuleId,
        annotation_span: Span,
    ) -> Option<&ResolvedTypeAnnotation> {
        self.annotations(module)
            .iter()
            .find(|annotation| annotation.span() == annotation_span)
    }

    /// Named-function signatures in deterministic source traversal order.
    #[must_use]
    pub fn functions(&self, module: &ModuleId) -> &[FunctionSignature] {
        self.by_module
            .get(module)
            .map_or(&[], |types| types.functions.as_slice())
    }

    /// Looks up a named signature by its function-name declaration span.
    #[must_use]
    pub fn function(
        &self,
        module: &ModuleId,
        declaration_span: Span,
    ) -> Option<&FunctionSignature> {
        self.functions(module)
            .iter()
            .find(|signature| signature.declaration_span() == declaration_span)
    }

    pub(crate) fn binding_type(
        &self,
        module: &ModuleId,
        declaration_span: Span,
    ) -> Option<&ValueType> {
        self.by_module
            .get(module)
            .and_then(|types| {
                types
                    .bindings
                    .iter()
                    .find(|binding| binding.declaration_span == declaration_span)
            })
            .map(|binding| &binding.value_type)
    }

    fn analyze(
        sources: &ModuleSourceRegistry,
        names: &ModuleNameRegistry,
        control: &AnalysisControl,
    ) -> Result<Self, Vec<ModuleTypeError>> {
        let mut registry = Self::default();
        let mut errors = Vec::new();
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
            let (types, mut source_errors) = TypeCollector::new(entry, control).collect();
            errors.append(&mut source_errors);
            registry.by_module.insert(entry.module().clone(), types);
        }
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
            errors.extend(SignatureValidator::new(entry, names, &registry, control).validate());
        }
        let source_order = sources
            .entries()
            .enumerate()
            .map(|(index, entry)| (entry.module().clone(), index))
            .collect::<BTreeMap<_, _>>();
        errors.sort_by_key(|error| (source_order[error.module()], error.primary_span().start()));
        errors.dedup();
        if errors.is_empty() {
            Ok(registry)
        } else {
            Err(errors)
        }
    }
}

struct TypeCollector<'a> {
    entry: &'a RegisteredModuleSource,
    types: ModuleTypes,
    errors: Vec<ModuleTypeError>,
    control: &'a AnalysisControl,
}

impl<'a> TypeCollector<'a> {
    fn new(entry: &'a RegisteredModuleSource, control: &'a AnalysisControl) -> Self {
        Self {
            entry,
            types: ModuleTypes::default(),
            errors: Vec::new(),
            control,
        }
    }

    fn collect(mut self) -> (ModuleTypes, Vec<ModuleTypeError>) {
        self.statements(self.entry.script().statements())
            .expect("accumulating type collection does not fail fast");
        (self.types, self.errors)
    }

    fn statements(&mut self, statements: &[Statement]) -> Result<(), Box<ModuleTypeError>> {
        for statement in statements {
            if self.control.is_cancelled() {
                break;
            }
            self.statement(statement)?;
        }
        Ok(())
    }

    fn statement(&mut self, statement: &Statement) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                let value_type = declaration
                    .type_annotation
                    .as_ref()
                    .map(|annotation| self.resolve_type(annotation))
                    .or_else(|| match declaration.value.kind() {
                        ExpressionKind::CommandSubstitution(substitution) => {
                            Some(match substitution.capture() {
                                flash_syntax::CommandCaptureKind::Text => ValueType::String,
                                flash_syntax::CommandCaptureKind::Bytes => ValueType::Bytes,
                            })
                        }
                        _ => None,
                    });
                if let Some(value_type) = value_type {
                    self.types.bindings.push(ResolvedBindingType {
                        declaration_span: declaration.name.span(),
                        value_type,
                    });
                }
                self.expression(&declaration.value)
            }
            StatementKind::Assignment(assignment) => self.expression(&assignment.value),
            StatementKind::Environment(environment) => match environment {
                flash_syntax::EnvironmentStatement::Export { value, .. } => self.expression(value),
                flash_syntax::EnvironmentStatement::Unset { .. } => Ok(()),
            },
            StatementKind::Function(function) => {
                let mut parameters = Vec::with_capacity(function.parameters.len());
                for parameter in &function.parameters {
                    let value_type = match &parameter.type_annotation {
                        Some(annotation) => {
                            let value_type = self.resolve_type(annotation);
                            self.types.bindings.push(ResolvedBindingType {
                                declaration_span: parameter.name.span(),
                                value_type: value_type.clone(),
                            });
                            value_type
                        }
                        None => ValueType::Any,
                    };
                    parameters.push(FunctionParameterSignature {
                        name: self.text(parameter.name.span()).to_owned(),
                        declaration_span: parameter.name.span(),
                        value_type,
                        annotation_span: parameter
                            .type_annotation
                            .as_ref()
                            .map(|annotation| annotation.span),
                    });
                }
                let result = match &function.return_type {
                    Some(annotation) => self.resolve_type(annotation),
                    None => ValueType::Any,
                };
                self.types.functions.push(FunctionSignature {
                    name: self.text(function.name.span()).to_owned(),
                    declaration_span: function.name.span(),
                    parameters,
                    result,
                    result_annotation_span: function
                        .return_type
                        .as_ref()
                        .map(|annotation| annotation.span),
                    documentation: function
                        .documentation
                        .as_ref()
                        .map(|block| Documentation::from_block(self.entry.source(), block)),
                });
                self.statements(&function.body.statements)
            }
            StatementKind::If(statement) => {
                self.chain(&statement.condition)?;
                self.statements(&statement.then_block.statements)?;
                match &statement.else_branch {
                    Some(ElseBranch::Block(block)) => self.statements(&block.statements),
                    Some(ElseBranch::If(nested)) => self.statement(&Statement::new(
                        StatementKind::If(nested.kind().clone()),
                        nested.span(),
                    )),
                    None => Ok(()),
                }
            }
            StatementKind::While(statement) => {
                self.chain(&statement.condition)?;
                self.statements(&statement.body.statements)
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable)?;
                self.statements(&statement.body.statements)
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value)?;
                for arm in &statement.arms {
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal)?;
                    }
                    if let Some(guard) = &arm.guard {
                        self.expression(guard)?;
                    }
                    self.statements(&arm.body.statements)?;
                }
                Ok(())
            }
            StatementKind::Control(ControlTransfer::Return(Some(expression))) => {
                self.expression(expression)
            }
            StatementKind::Control(
                ControlTransfer::Break | ControlTransfer::Continue | ControlTransfer::Return(None),
            ) => Ok(()),
            StatementKind::Job(job) => self.chain(&job.chain),
        }
    }

    fn chain(&mut self, chain: &ConditionalChain) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                for stage in pipeline.stages() {
                    match stage.kind() {
                        StageKind::Expression(expression) => self.expression(expression)?,
                        StageKind::Command(command) => {
                            self.word(command.head.word())?;
                            for item in &command.items {
                                match item.kind() {
                                    CommandItemKind::Word(word) => self.word(word)?,
                                    CommandItemKind::Closure(closure) => self.closure(closure)?,
                                    CommandItemKind::Redirection(redirection) => {
                                        self.redirection(redirection.kind())?;
                                    }
                                    CommandItemKind::Spread(_) => {}
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn expression(&mut self, expression: &Expression) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(_) | ExpressionKind::Symbol(_) => Ok(()),
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
            ExpressionKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            ExpressionKind::GroupedJob(chain) => self.chain(chain),
            ExpressionKind::Call(call) => {
                self.expression(&call.callee)?;
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

    fn closure(&mut self, closure: &Closure) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for parameter in &closure.parameters {
            if let Some(annotation) = &parameter.type_annotation {
                let value_type = self.resolve_type(annotation);
                self.types.bindings.push(ResolvedBindingType {
                    declaration_span: parameter.name.span(),
                    value_type,
                });
            }
        }
        self.chain(&closure.body)
    }

    fn literal(&mut self, literal: &flash_syntax::Literal) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            self.word_parts(parts)?;
        }
        Ok(())
    }

    fn word(&mut self, word: &Word) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.word_parts(word.parts())
    }

    fn word_parts(&mut self, parts: &[WordPart]) -> Result<(), Box<ModuleTypeError>> {
        for part in parts {
            if self.control.is_cancelled() {
                break;
            }
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&mut self, part: &WordPart) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(substitution) => {
                self.chain(substitution.chain())?;
                if substitution.capture() == flash_syntax::CommandCaptureKind::Bytes {
                    self.errors.push(ModuleTypeError::ByteCaptureInWord {
                        module: self.entry.module().clone(),
                        span: substitution.modifier_span().unwrap_or_else(|| part.span()),
                    });
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => Ok(()),
        }
    }

    fn resolve_type(&mut self, reference: &TypeReference) -> ValueType {
        if self.control.is_cancelled() {
            return ValueType::Any;
        }
        let value_type = match self.resolve_type_value(reference) {
            Ok(value_type) => value_type,
            Err(error) => {
                self.errors.push(*error);
                ValueType::Any
            }
        };
        self.types.annotations.push(ResolvedTypeAnnotation {
            span: reference.span,
            value_type: value_type.clone(),
        });
        value_type
    }

    fn resolve_type_value(
        &mut self,
        reference: &TypeReference,
    ) -> Result<ValueType, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(ValueType::Any);
        }
        let name = self.text(reference.name.span());
        let value_type = if name == "List" {
            if reference.arguments.len() != 1 {
                return Err(Box::new(ModuleTypeError::InvalidTypeArity {
                    module: self.entry.module().clone(),
                    name: name.to_owned(),
                    expected: 1,
                    actual: reference.arguments.len(),
                    span: reference.span,
                }));
            }
            ValueType::List(Box::new(self.resolve_type_value(&reference.arguments[0])?))
        } else {
            if !reference.arguments.is_empty() {
                return Err(Box::new(ModuleTypeError::InvalidTypeArity {
                    module: self.entry.module().clone(),
                    name: name.to_owned(),
                    expected: 0,
                    actual: reference.arguments.len(),
                    span: reference.span,
                }));
            }
            match name {
                "Any" => ValueType::Any,
                "Null" => ValueType::Null,
                "Bool" => ValueType::Bool,
                "Int" => ValueType::Int,
                "Float" => ValueType::Float,
                "String" => ValueType::String,
                "Bytes" => ValueType::Bytes,
                "Path" => ValueType::Path,
                "Duration" => ValueType::Duration,
                "ByteSize" => ValueType::ByteSize,
                "Record" => ValueType::Record,
                "Table" => ValueType::Table,
                "Range" => ValueType::Range,
                "Status" => ValueType::Status,
                "Function" => ValueType::Function,
                "Closure" => ValueType::Closure,
                _ => {
                    return Err(Box::new(ModuleTypeError::UnknownType {
                        module: self.entry.module().clone(),
                        name: name.to_owned(),
                        span: reference.span,
                    }));
                }
            }
        };
        Ok(value_type)
    }

    fn text(&self, span: Span) -> &str {
        self.entry
            .source()
            .slice(span)
            .expect("parsed syntax spans belong to their module source")
    }
}

struct SignatureValidator<'a> {
    entry: &'a RegisteredModuleSource,
    names: &'a ModuleNameRegistry,
    types: &'a ModuleTypeRegistry,
    errors: RefCell<Vec<ModuleTypeError>>,
    control: &'a AnalysisControl,
}

impl<'a> SignatureValidator<'a> {
    fn new(
        entry: &'a RegisteredModuleSource,
        names: &'a ModuleNameRegistry,
        types: &'a ModuleTypeRegistry,
        control: &'a AnalysisControl,
    ) -> Self {
        Self {
            entry,
            names,
            types,
            errors: RefCell::new(Vec::new()),
            control,
        }
    }

    fn validate(self) -> Vec<ModuleTypeError> {
        self.statements(self.entry.script().statements())
            .expect("accumulating signature validation does not fail fast");
        self.errors.into_inner()
    }

    fn statements(&self, statements: &[Statement]) -> Result<(), Box<ModuleTypeError>> {
        for statement in statements {
            if self.control.is_cancelled() {
                break;
            }
            self.statement(statement)?;
        }
        Ok(())
    }

    fn function(
        &self,
        function: &flash_syntax::FunctionDefinition,
    ) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        let signature = self
            .types
            .function(self.entry.module(), function.name.span())
            .expect("every collected named function has one resolved signature");
        self.function_statements(&function.body.statements, signature)?;

        let Some(StatementKind::Job(job)) = function.body.statements.last().map(Statement::kind)
        else {
            return Ok(());
        };
        let Some((span, actual)) = self.chain_value(&job.chain)? else {
            return Ok(());
        };
        self.check_result(signature, span, Some(actual))
    }

    fn function_statements(
        &self,
        statements: &[Statement],
        signature: &FunctionSignature,
    ) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for statement in statements {
            self.function_statement(statement, signature)?;
        }
        Ok(())
    }

    fn function_statement(
        &self,
        statement: &Statement,
        signature: &FunctionSignature,
    ) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match statement.kind() {
            StatementKind::Function(function) => self.function(function),
            StatementKind::Control(ControlTransfer::Return(value)) => {
                let (span, actual) = match value {
                    Some(expression) => (expression.span(), self.expression(expression)?),
                    None => (statement.span(), Some(ValueType::Null)),
                };
                self.check_result(signature, span, actual)
            }
            StatementKind::If(statement) => {
                self.chain(&statement.condition)?;
                self.function_statements(&statement.then_block.statements, signature)?;
                match &statement.else_branch {
                    Some(ElseBranch::Block(block)) => {
                        self.function_statements(&block.statements, signature)
                    }
                    Some(ElseBranch::If(nested)) => {
                        self.function_if_statement(nested.kind(), signature)
                    }
                    None => Ok(()),
                }
            }
            StatementKind::While(statement) => {
                self.chain(&statement.condition)?;
                self.function_statements(&statement.body.statements, signature)
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable)?;
                self.function_statements(&statement.body.statements, signature)
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value)?;
                for arm in &statement.arms {
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal)?;
                    }
                    if let Some(guard) = &arm.guard {
                        self.expression(guard)?;
                    }
                    self.function_statements(&arm.body.statements, signature)?;
                }
                Ok(())
            }
            _ => self.statement(statement),
        }
    }

    fn function_if_statement(
        &self,
        statement: &flash_syntax::IfStatement,
        signature: &FunctionSignature,
    ) -> Result<(), Box<ModuleTypeError>> {
        self.chain(&statement.condition)?;
        self.function_statements(&statement.then_block.statements, signature)?;
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => {
                self.function_statements(&block.statements, signature)
            }
            Some(ElseBranch::If(nested)) => self.function_if_statement(nested.kind(), signature),
            None => Ok(()),
        }
    }

    fn check_result(
        &self,
        signature: &FunctionSignature,
        result_span: Span,
        actual: Option<ValueType>,
    ) -> Result<(), Box<ModuleTypeError>> {
        let Some(actual) = actual else {
            return Ok(());
        };
        if actual == ValueType::Any || signature.result().accepts_type(&actual) {
            return Ok(());
        }
        self.errors
            .borrow_mut()
            .push(ModuleTypeError::ResultMismatch {
                module: self.entry.module().clone(),
                name: signature.name().to_owned(),
                result_span,
                expected: signature.result().clone(),
                actual,
                annotation_span: signature
                    .result_annotation_span()
                    .unwrap_or_else(|| signature.declaration_span()),
            });
        Ok(())
    }

    fn statement(&self, statement: &Statement) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                self.expression(&declaration.value).map(|_| ())
            }
            StatementKind::Assignment(assignment) => self.expression(&assignment.value).map(|_| ()),
            StatementKind::Environment(environment) => match environment {
                flash_syntax::EnvironmentStatement::Export { value, .. } => {
                    self.expression(value).map(|_| ())
                }
                flash_syntax::EnvironmentStatement::Unset { .. } => Ok(()),
            },
            StatementKind::Function(function) => self.function(function),
            StatementKind::If(statement) => {
                self.chain(&statement.condition)?;
                self.statements(&statement.then_block.statements)?;
                match &statement.else_branch {
                    Some(ElseBranch::Block(block)) => self.statements(&block.statements),
                    Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
                    None => Ok(()),
                }
            }
            StatementKind::While(statement) => {
                self.chain(&statement.condition)?;
                self.statements(&statement.body.statements)
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable)?;
                self.statements(&statement.body.statements)
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value)?;
                for arm in &statement.arms {
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal)?;
                    }
                    if let Some(guard) = &arm.guard {
                        self.expression(guard)?;
                    }
                    self.statements(&arm.body.statements)?;
                }
                Ok(())
            }
            StatementKind::Control(ControlTransfer::Return(Some(expression))) => {
                self.expression(expression).map(|_| ())
            }
            StatementKind::Control(
                ControlTransfer::Break | ControlTransfer::Continue | ControlTransfer::Return(None),
            ) => Ok(()),
            StatementKind::Job(job) => self.chain(&job.chain),
        }
    }

    fn if_statement(
        &self,
        statement: &flash_syntax::IfStatement,
    ) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.chain(&statement.condition)?;
        self.statements(&statement.then_block.statements)?;
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.statements(&block.statements),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => Ok(()),
        }
    }

    fn chain(&self, chain: &ConditionalChain) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                for stage in pipeline.stages() {
                    match stage.kind() {
                        StageKind::Expression(expression) => {
                            self.expression(expression)?;
                        }
                        StageKind::Command(command) => {
                            self.word(command.head.word())?;
                            for item in &command.items {
                                match item.kind() {
                                    CommandItemKind::Word(word) => self.word(word)?,
                                    CommandItemKind::Closure(closure) => {
                                        self.chain(&closure.body)?;
                                    }
                                    CommandItemKind::Redirection(redirection) => {
                                        self.redirection(redirection.kind())?;
                                    }
                                    CommandItemKind::Spread(_) => {}
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn chain_value(
        &self,
        chain: &ConditionalChain,
    ) -> Result<Option<(Span, ValueType)>, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(None);
        }
        if chain.or_terms().len() != 1 || chain.or_terms()[0].and_terms().len() != 1 {
            return Ok(None);
        }
        let pipeline = &chain.or_terms()[0].and_terms()[0];
        if pipeline.stages().len() != 1 {
            return Ok(None);
        }
        let StageKind::Expression(expression) = pipeline.stages()[0].kind() else {
            return Ok(None);
        };
        Ok(self
            .expression(expression)?
            .map(|value_type| (expression.span(), value_type)))
    }

    fn expression(
        &self,
        expression: &Expression,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(None);
        }
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(_) => Ok(self
                .names
                .reference(self.entry.module(), expression.span())
                .and_then(|reference| self.reference_type(reference.target()))),
            ExpressionKind::Symbol(_) => Ok(None),
            ExpressionKind::List(elements) => {
                let mut element_type = None;
                for element in elements {
                    let Some(current) = self.expression(element)? else {
                        return Ok(None);
                    };
                    if let Some(previous) = &element_type {
                        if previous != &current {
                            return Ok(None);
                        }
                    } else {
                        element_type = Some(current);
                    }
                }
                Ok(element_type.map(|element| ValueType::List(Box::new(element))))
            }
            ExpressionKind::Record(entries) => {
                for entry in entries {
                    if let RecordKey::DoubleQuoted(part) = &entry.key {
                        self.word_part(part)?;
                    }
                    self.expression(&entry.value)?;
                }
                Ok(Some(ValueType::Record))
            }
            ExpressionKind::Closure(closure) => {
                self.chain(&closure.body)?;
                Ok(Some(ValueType::Closure))
            }
            ExpressionKind::CommandSubstitution(substitution) => {
                self.chain(substitution.chain())?;
                Ok(Some(match substitution.capture() {
                    flash_syntax::CommandCaptureKind::Text => ValueType::String,
                    flash_syntax::CommandCaptureKind::Bytes => ValueType::Bytes,
                }))
            }
            ExpressionKind::GroupedJob(chain) => {
                self.chain(chain)?;
                Ok(None)
            }
            ExpressionKind::Call(call) => self.call(expression.span(), call),
            ExpressionKind::Index(index) => {
                let target = self.expression(&index.target)?;
                let index_type = self.expression(&index.index)?;
                Ok(match (target, index_type) {
                    (Some(ValueType::List(element)), Some(ValueType::Int)) => Some(*element),
                    (Some(ValueType::String), Some(ValueType::Int)) => Some(ValueType::String),
                    _ => None,
                })
            }
            ExpressionKind::Member(member) => {
                self.expression(&member.target)?;
                Ok(None)
            }
            ExpressionKind::Unary(unary) => {
                let operand = self.expression(&unary.operand)?;
                Ok(match (unary.operator.kind(), operand) {
                    (UnaryOperator::Not, Some(ValueType::Bool)) => Some(ValueType::Bool),
                    (
                        UnaryOperator::Positive | UnaryOperator::Negative,
                        Some(value_type @ (ValueType::Int | ValueType::Float)),
                    ) => Some(value_type),
                    _ => None,
                })
            }
            ExpressionKind::Binary(binary) => {
                let left = self.expression(&binary.left)?;
                let right = self.expression(&binary.right)?;
                Ok(self.binary_type(*binary.operator.kind(), left, right))
            }
        }
    }

    fn reference_type(&self, target: &ModuleReferenceTarget) -> Option<ValueType> {
        match target {
            ModuleReferenceTarget::DynamicStatus => Some(ValueType::Any),
            ModuleReferenceTarget::ScriptArguments => {
                Some(ValueType::List(Box::new(ValueType::String)))
            }
            ModuleReferenceTarget::Local {
                module,
                declaration_span,
            } => self.declaration_type(module, *declaration_span),
            ModuleReferenceTarget::Imported {
                target_module,
                declaration_span,
                ..
            } => self.declaration_type(target_module, *declaration_span),
        }
    }

    fn declaration_type(&self, module: &ModuleId, declaration_span: Span) -> Option<ValueType> {
        if self.types.function(module, declaration_span).is_some() {
            return Some(ValueType::Function);
        }
        self.types.binding_type(module, declaration_span).cloned()
    }

    fn binary_type(
        &self,
        operator: BinaryOperator,
        left: Option<ValueType>,
        right: Option<ValueType>,
    ) -> Option<ValueType> {
        match operator {
            BinaryOperator::Equal | BinaryOperator::NotEqual => Some(ValueType::Bool),
            BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual => {
                Self::ordered_pair(left.as_ref(), right.as_ref()).then_some(ValueType::Bool)
            }
            BinaryOperator::In => {
                Self::membership_pair(left.as_ref(), right.as_ref()).then_some(ValueType::Bool)
            }
            BinaryOperator::Range | BinaryOperator::RangeInclusive => {
                matches!((left, right), (Some(ValueType::Int), Some(ValueType::Int)))
                    .then_some(ValueType::Range)
            }
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Remainder => Self::numeric_pair(left.as_ref(), right.as_ref()),
        }
    }

    fn numeric_pair(left: Option<&ValueType>, right: Option<&ValueType>) -> Option<ValueType> {
        match (left, right) {
            (Some(ValueType::Int), Some(ValueType::Int)) => Some(ValueType::Int),
            (Some(ValueType::Int | ValueType::Float), Some(ValueType::Int | ValueType::Float)) => {
                Some(ValueType::Float)
            }
            _ => None,
        }
    }

    fn ordered_pair(left: Option<&ValueType>, right: Option<&ValueType>) -> bool {
        match (left, right) {
            (Some(ValueType::Int | ValueType::Float), Some(ValueType::Int | ValueType::Float)) => {
                true
            }
            (Some(left), Some(right)) if left == right => match left {
                ValueType::String
                | ValueType::Bytes
                | ValueType::Path
                | ValueType::Duration
                | ValueType::ByteSize => true,
                ValueType::List(element) => Self::ordered_pair(Some(element), Some(element)),
                _ => false,
            },
            _ => false,
        }
    }

    fn membership_pair(element: Option<&ValueType>, container: Option<&ValueType>) -> bool {
        match (element, container) {
            (Some(ValueType::Int), Some(ValueType::Range))
            | (Some(ValueType::String), Some(ValueType::String | ValueType::Record)) => true,
            (Some(element), Some(ValueType::List(item))) => item.accepts_type(element),
            _ => false,
        }
    }

    fn call(
        &self,
        call_span: Span,
        call: &flash_syntax::CallExpression,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(None);
        }
        if !matches!(call.callee.kind(), ExpressionKind::Symbol(_)) {
            self.expression(&call.callee)?;
        }
        let mut argument_types = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            argument_types.push(self.expression(argument)?);
        }

        let Some(reference) = self
            .names
            .reference(self.entry.module(), call.callee.span())
        else {
            if let ExpressionKind::Symbol(identifier) = call.callee.kind()
                && let Some(intrinsic) = ExpressionIntrinsic::lookup(self.text(identifier.span()))
            {
                return Ok(Some(self.validate_intrinsic_call(
                    intrinsic,
                    call_span,
                    call,
                    &argument_types,
                )));
            }
            return Ok(None);
        };
        let (target_module, declaration_span) = match reference.target() {
            ModuleReferenceTarget::DynamicStatus | ModuleReferenceTarget::ScriptArguments => {
                return Ok(None);
            }
            ModuleReferenceTarget::Local {
                module,
                declaration_span,
            } => (module, *declaration_span),
            ModuleReferenceTarget::Imported {
                target_module,
                declaration_span,
                ..
            } => (target_module, *declaration_span),
        };
        let Some(signature) = self.types.function(target_module, declaration_span) else {
            return Ok(None);
        };
        if call.arguments.len() != signature.parameters().len() {
            self.errors.borrow_mut().push(ModuleTypeError::CallArity {
                module: self.entry.module().clone(),
                name: signature.name().to_owned(),
                call_span,
                expected: signature.parameters().len(),
                actual: call.arguments.len(),
                declaration_span: signature.declaration_span(),
            });
            return Ok(Some(ValueType::Any));
        }
        let mut invalid = false;
        for ((argument, actual), parameter) in call
            .arguments
            .iter()
            .zip(argument_types)
            .zip(signature.parameters())
        {
            let Some(actual) = actual else {
                continue;
            };
            if actual == ValueType::Any {
                continue;
            }
            if !parameter.value_type().accepts_type(&actual) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::ArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: signature.name().to_owned(),
                        parameter: parameter.name().to_owned(),
                        argument_span: argument.span(),
                        expected: parameter.value_type().clone(),
                        actual,
                        parameter_span: parameter
                            .annotation_span()
                            .unwrap_or_else(|| parameter.declaration_span()),
                    });
                invalid = true;
            }
        }
        Ok(Some(if invalid {
            ValueType::Any
        } else {
            signature.result().clone()
        }))
    }

    fn validate_intrinsic_call(
        &self,
        intrinsic: ExpressionIntrinsic,
        call_span: Span,
        call: &flash_syntax::CallExpression,
        argument_types: &[Option<ValueType>],
    ) -> ValueType {
        if call.arguments.len() != intrinsic.arity() {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::IntrinsicCallArity {
                    module: self.entry.module().clone(),
                    name: intrinsic.name(),
                    call_span,
                    expected: intrinsic.arity(),
                    actual: call.arguments.len(),
                });
            return ValueType::Any;
        }
        let Some(actual) = argument_types[0].as_ref() else {
            return intrinsic.result_type();
        };
        if !intrinsic.accepts_type(actual) {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::IntrinsicArgumentMismatch {
                    module: self.entry.module().clone(),
                    name: intrinsic.name(),
                    argument_span: call.arguments[0].span(),
                    expected: intrinsic.parameter_type_label(),
                    actual: actual.clone(),
                });
            ValueType::Any
        } else {
            intrinsic.result_type()
        }
    }

    fn text(&self, span: Span) -> &str {
        self.entry
            .source()
            .slice(span)
            .expect("parsed syntax spans belong to their module source")
    }

    fn literal(
        &self,
        literal: &flash_syntax::Literal,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(None);
        }
        let value_type = match literal.kind() {
            LiteralKind::Null => ValueType::Null,
            LiteralKind::Boolean(_) => ValueType::Bool,
            LiteralKind::Integer => ValueType::Int,
            LiteralKind::Float => ValueType::Float,
            LiteralKind::SingleQuoted => ValueType::String,
            LiteralKind::DoubleQuoted(parts) => {
                self.word_parts(parts)?;
                ValueType::String
            }
        };
        Ok(Some(value_type))
    }

    fn word(&self, word: &Word) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.word_parts(word.parts())
    }

    fn word_parts(&self, parts: &[WordPart]) -> Result<(), Box<ModuleTypeError>> {
        for part in parts {
            if self.control.is_cancelled() {
                break;
            }
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&self, part: &WordPart) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => {
                self.expression(expression).map(|_| ())
            }
            WordPartKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            _ => Ok(()),
        }
    }

    fn redirection(&self, redirection: &RedirectionKind) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => Ok(()),
        }
    }
}

struct ReferenceResolver<'a> {
    entry: &'a RegisteredModuleSource,
    scopes: Vec<ReferenceScope>,
    references: Vec<ModuleNameReference>,
    visible: Vec<ModuleVisibleBinding>,
    errors: Vec<ModuleNameError>,
    control: &'a AnalysisControl,
}

struct ReferenceScope {
    bindings: BTreeMap<String, Option<ModuleReferenceTarget>>,
    span: Span,
}

impl<'a> ReferenceResolver<'a> {
    fn new(
        entry: &'a RegisteredModuleSource,
        registry: &ModuleNameRegistry,
        poisoned_imports: Option<&BTreeSet<String>>,
        is_root: bool,
        control: &'a AnalysisControl,
    ) -> Self {
        let source_span = entry.script().span();
        let mut scopes = Vec::with_capacity(usize::from(is_root) + 2);
        let mut visible = Vec::new();
        let dynamic_target = ModuleReferenceTarget::DynamicStatus;
        scopes.push(ReferenceScope {
            bindings: BTreeMap::from([(
                DynamicBinding::CurrentStatus.name().to_owned(),
                Some(dynamic_target.clone()),
            )]),
            span: source_span,
        });
        visible.push(ModuleVisibleBinding {
            name: DynamicBinding::CurrentStatus.name().to_owned(),
            target: dynamic_target,
            scope_span: source_span,
            visible_from: source_span.start(),
            depth: 0,
        });
        if is_root {
            let target = ModuleReferenceTarget::ScriptArguments;
            scopes.push(ReferenceScope {
                bindings: BTreeMap::from([("args".to_owned(), Some(target.clone()))]),
                span: source_span,
            });
            visible.push(ModuleVisibleBinding {
                name: "args".to_owned(),
                target,
                scope_span: source_span,
                visible_from: source_span.start(),
                depth: scopes.len().saturating_sub(1),
            });
        }
        let mut root = BTreeMap::new();
        for import in registry.imports(entry.module()) {
            let export = registry
                .export(import.target(), import.name())
                .expect("validated imports always retain their target export");
            let target = ModuleReferenceTarget::Imported {
                import_span: import.name_span(),
                target_module: import.target().clone(),
                declaration_span: export.declaration_span(),
                export_span: export.export_span(),
            };
            root.insert(import.name().to_owned(), Some(target.clone()));
            visible.push(ModuleVisibleBinding {
                name: import.name().to_owned(),
                target,
                scope_span: source_span,
                visible_from: source_span.start(),
                depth: scopes.len(),
            });
        }
        if let Some(poisoned_imports) = poisoned_imports {
            let locals = &registry
                .by_module
                .get(entry.module())
                .expect("every registered source has a name table")
                .locals;
            for name in poisoned_imports {
                if !locals.contains_key(name) {
                    root.entry(name.clone()).or_insert(None);
                }
            }
        }
        scopes.push(ReferenceScope {
            bindings: root,
            span: source_span,
        });
        Self {
            entry,
            scopes,
            references: Vec::new(),
            visible,
            errors: Vec::new(),
            control,
        }
    }

    fn resolve(
        mut self,
    ) -> (
        Vec<ModuleNameReference>,
        Vec<ModuleVisibleBinding>,
        Vec<ModuleNameError>,
    ) {
        self.statements(self.entry.script().statements())
            .expect("accumulating name traversal does not fail fast");
        (self.references, self.visible, self.errors)
    }

    fn statements(&mut self, statements: &[Statement]) -> Result<(), Box<ModuleNameError>> {
        for statement in statements {
            if self.control.is_cancelled() {
                break;
            }
            self.statement(statement)?;
        }
        Ok(())
    }

    fn statement(&mut self, statement: &Statement) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                self.expression(&declaration.value)?;
                self.declare(declaration.name.span(), statement.span().end())
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
                let available = self.ensure_available(function.name.span());
                self.push_scope(function.body.span);
                self.insert_local(function.name.span(), function.body.span.start());
                self.push_scope(function.body.span);
                let result = (|| {
                    for parameter in &function.parameters {
                        self.declare(parameter.name.span(), function.body.span.start())?;
                    }
                    self.block(&function.body)
                })();
                self.pop_scope();
                self.pop_scope();
                result?;
                if available {
                    self.insert_local(function.name.span(), statement.span().end());
                }
                Ok(())
            }
            StatementKind::If(statement) => self.if_statement(statement),
            StatementKind::While(statement) => {
                self.chain(&statement.condition)?;
                self.block(&statement.body)
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable)?;
                self.push_scope(statement.body.span);
                let result = (|| {
                    self.declare(statement.binding.span(), statement.body.span.start())?;
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
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.chain(&statement.condition)?;
        self.block(&statement.then_block)?;
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.block(block),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => Ok(()),
        }
    }

    fn match_arm(&mut self, arm: &MatchArm) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.push_scope(arm.span);
        let result = (|| {
            match &arm.pattern {
                Pattern::Binding(identifier) => {
                    self.declare(identifier.span(), identifier.span().end())?
                }
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
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.push_scope(block.span);
        let result = self.statements(&block.statements);
        self.pop_scope();
        result
    }

    fn chain(&mut self, chain: &ConditionalChain) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                self.pipeline(pipeline)?;
            }
        }
        Ok(())
    }

    fn pipeline(&mut self, pipeline: &Pipeline) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
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
        if self.control.is_cancelled() {
            return Ok(());
        }
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
            ExpressionKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            ExpressionKind::GroupedJob(chain) => self.chain(chain),
            ExpressionKind::Call(call) => {
                if let ExpressionKind::Symbol(identifier) = call.callee.kind() {
                    let name = self.text(identifier.span());
                    if ExpressionIntrinsic::lookup(name).is_none()
                        || self.visible_target(name).is_some()
                    {
                        self.variable(identifier.span(), call.callee.span())?;
                    }
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
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.push_scope(closure.span);
        let result = (|| {
            for parameter in &closure.parameters {
                self.declare(parameter.name.span(), closure.body.span().start())?;
            }
            self.chain(&closure.body)
        })();
        self.pop_scope();
        result
    }

    fn literal(&mut self, literal: &flash_syntax::Literal) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            self.word_parts(parts)?;
        }
        Ok(())
    }

    fn word(&mut self, word: &Word) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        self.word_parts(word.parts())
    }

    fn word_parts(&mut self, parts: &[WordPart]) -> Result<(), Box<ModuleNameError>> {
        for part in parts {
            if self.control.is_cancelled() {
                break;
            }
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&mut self, part: &WordPart) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        match part.kind() {
            WordPartKind::Variable(identifier) => self.variable(identifier.span(), part.span()),
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape => Ok(()),
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind) -> Result<(), Box<ModuleNameError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
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
        let Some(target) = self.visible_target(&name) else {
            self.errors.push(ModuleNameError::UnknownReference {
                module: self.entry.module().clone(),
                name,
                reference_span,
            });
            return Ok(());
        };
        let Some(target) = target else {
            return Ok(());
        };
        self.references.push(ModuleNameReference {
            name,
            reference_span,
            target,
        });
        Ok(())
    }

    fn visible_target(&self, name: &str) -> Option<Option<ModuleReferenceTarget>> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .cloned()
    }

    fn ensure_available(&mut self, declaration_span: Span) -> bool {
        let name = self.text(declaration_span).to_owned();
        if DynamicBinding::lookup(&name).is_some() {
            self.errors.push(ModuleNameError::ReservedBinding {
                module: self.entry.module().clone(),
                name,
                declaration_span,
            });
            return false;
        }
        let scope = self
            .scopes
            .last()
            .expect("reference resolution always retains a root scope");
        if let Some(Some(first)) = scope.bindings.get(&name) {
            self.errors.push(ModuleNameError::DuplicateBinding {
                module: self.entry.module().clone(),
                name,
                first_span: binding_span(first),
                duplicate_span: declaration_span,
            });
            return false;
        }
        true
    }

    fn declare(
        &mut self,
        declaration_span: Span,
        visible_from: usize,
    ) -> Result<(), Box<ModuleNameError>> {
        if self.ensure_available(declaration_span) {
            self.insert_local(declaration_span, visible_from);
        }
        Ok(())
    }

    fn insert_local(&mut self, declaration_span: Span, visible_from: usize) {
        let name = self.text(declaration_span).to_owned();
        let target = ModuleReferenceTarget::Local {
            module: self.entry.module().clone(),
            declaration_span,
        };
        let depth = self.scopes.len().saturating_sub(1);
        let scope = self
            .scopes
            .last_mut()
            .expect("reference resolution always retains a root scope");
        scope.bindings.insert(name.clone(), Some(target.clone()));
        self.visible.push(ModuleVisibleBinding {
            name,
            target,
            scope_span: scope.span,
            visible_from,
            depth,
        });
    }

    fn push_scope(&mut self, span: Span) {
        self.scopes.push(ReferenceScope {
            bindings: BTreeMap::new(),
            span,
        });
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
        ModuleReferenceTarget::DynamicStatus => {
            unreachable!("the reserved dynamic binding cannot collide as a lexical binding")
        }
        ModuleReferenceTarget::ScriptArguments => {
            unreachable!("the synthetic parent input cannot collide in a module scope")
        }
        ModuleReferenceTarget::Local {
            declaration_span, ..
        } => *declaration_span,
        ModuleReferenceTarget::Imported { import_span, .. } => *import_span,
    }
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start() <= offset && offset < span.end()
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

/// One potential module-initializer effect found without execution or probing.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModuleEffect {
    /// A change to the session-owned logical working directory.
    WorkingDirectory,
    /// A change to the child environment shared by the program session.
    ChildEnvironment,
    /// A change to or dependency on the shared command status.
    Status,
    /// Bytes or structured payloads routed to program output.
    Output,
    /// A filesystem read requested by syntax with known semantics.
    FilesystemRead,
    /// A filesystem write requested by syntax with known semantics.
    FilesystemWrite,
    /// Foreground or background process activity.
    Process,
    /// Observation or mutation of program-owned job state.
    Job,
    /// Whole-program termination requested by `exit`.
    ProgramExit,
    /// Behavior whose external effects cannot be proven more precisely.
    OpaqueExternal,
}

/// One source-anchored potential initializer effect.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ModuleEffectOccurrence {
    effect: ModuleEffect,
    span: Span,
}

impl ModuleEffectOccurrence {
    /// The statically classified effect.
    #[must_use]
    pub const fn effect(self) -> ModuleEffect {
        self.effect
    }

    /// The syntax that introduces this potential effect.
    #[must_use]
    pub const fn span(self) -> Span {
        self.span
    }
}

/// Deterministically ordered potential effects for one initializer boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleEffectSummary {
    occurrences: Vec<ModuleEffectOccurrence>,
}

impl ModuleEffectSummary {
    /// Source-ordered effect sites, with identical effect/span pairs retained once.
    #[must_use]
    pub fn occurrences(&self) -> &[ModuleEffectOccurrence] {
        &self.occurrences
    }

    fn push(&mut self, effect: ModuleEffect, span: Span) {
        let occurrence = ModuleEffectOccurrence { effect, span };
        if !self.occurrences.contains(&occurrence) {
            self.occurrences.push(occurrence);
        }
    }

    fn extend(&mut self, other: &Self) {
        for occurrence in &other.occurrences {
            self.push(occurrence.effect, occurrence.span);
        }
    }
}

/// Direct and named-dependency-folded effect summaries by canonical module.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleEffectRegistry {
    direct: BTreeMap<ModuleId, ModuleEffectSummary>,
    transitive: BTreeMap<ModuleId, ModuleEffectSummary>,
}

impl ModuleEffectRegistry {
    /// Effects introduced by this module's initializer itself.
    #[must_use]
    pub fn direct(&self, module: &ModuleId) -> &ModuleEffectSummary {
        static EMPTY: ModuleEffectSummary = ModuleEffectSummary {
            occurrences: Vec::new(),
        };
        self.direct.get(module).unwrap_or(&EMPTY)
    }

    /// Effects in once-only named-dependency initialization order, then direct effects.
    #[must_use]
    pub fn transitive(&self, module: &ModuleId) -> &ModuleEffectSummary {
        static EMPTY: ModuleEffectSummary = ModuleEffectSummary {
            occurrences: Vec::new(),
        };
        self.transitive.get(module).unwrap_or(&EMPTY)
    }

    fn analyze(
        graph: &ModuleGraph,
        sources: &ModuleSourceRegistry,
        names: &ModuleNameRegistry,
        commands: &CommandRegistry,
        control: &AnalysisControl,
    ) -> Self {
        let mut direct = BTreeMap::new();
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Self::default();
            }
            direct.insert(
                entry.module().clone(),
                StaticEffectAnalyzer::analyze(
                    entry.module(),
                    entry.source(),
                    entry.script(),
                    sources,
                    names,
                    commands,
                    control,
                ),
            );
        }

        fn visit(
            module: &ModuleId,
            names: &ModuleNameRegistry,
            direct: &BTreeMap<ModuleId, ModuleEffectSummary>,
            initialized: &mut BTreeSet<ModuleId>,
            summary: &mut ModuleEffectSummary,
            control: &AnalysisControl,
        ) {
            if control.is_cancelled() || initialized.contains(module) {
                return;
            }
            for import in names.imports(module) {
                visit(
                    import.target(),
                    names,
                    direct,
                    initialized,
                    summary,
                    control,
                );
            }
            if initialized.insert(module.clone())
                && let Some(module_summary) = direct.get(module)
            {
                summary.extend(module_summary);
            }
        }

        let mut transitive = BTreeMap::new();
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Self::default();
            }
            let mut summary = ModuleEffectSummary::default();
            visit(
                entry.module(),
                names,
                &direct,
                &mut BTreeSet::new(),
                &mut summary,
                control,
            );
            transitive.insert(entry.module().clone(), summary);
        }
        debug_assert!(transitive.contains_key(graph.root()));
        Self { direct, transitive }
    }
}

struct StaticEffectAnalyzer<'a> {
    module: ModuleId,
    source: SourceFile,
    sources: &'a ModuleSourceRegistry,
    names: &'a ModuleNameRegistry,
    commands: &'a CommandRegistry,
    summary: ModuleEffectSummary,
    active_functions: BTreeSet<(ModuleId, usize)>,
    control: &'a AnalysisControl,
}

impl<'a> StaticEffectAnalyzer<'a> {
    fn analyze(
        module: &'a ModuleId,
        source: &'a SourceFile,
        script: &'a Script,
        sources: &'a ModuleSourceRegistry,
        names: &'a ModuleNameRegistry,
        commands: &'a CommandRegistry,
        control: &'a AnalysisControl,
    ) -> ModuleEffectSummary {
        let mut analyzer = Self {
            module: module.clone(),
            source: source.clone(),
            sources,
            names,
            commands,
            summary: ModuleEffectSummary::default(),
            active_functions: BTreeSet::new(),
            control,
        };
        analyzer.statements(script.statements());
        analyzer.summary
    }

    fn statements(&mut self, statements: &[Statement]) {
        for statement in statements {
            if self.control.is_cancelled() {
                break;
            }
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement) {
        if self.control.is_cancelled() {
            return;
        }
        match statement.kind() {
            StatementKind::Import(_)
            | StatementKind::ModuleExport(_)
            | StatementKind::Function(_) => {}
            StatementKind::Declaration(declaration) => self.expression(&declaration.value),
            StatementKind::Assignment(assignment) => self.expression(&assignment.value),
            StatementKind::Environment(environment) => {
                self.summary
                    .push(ModuleEffect::ChildEnvironment, statement.span());
                if let flash_syntax::EnvironmentStatement::Export { value, .. } = environment {
                    self.expression(value);
                }
            }
            StatementKind::If(statement) => {
                self.if_statement(statement);
            }
            StatementKind::While(statement) => {
                self.chain(&statement.condition);
                self.statements(&statement.body.statements);
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable);
                self.statements(&statement.body.statements);
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value);
                for arm in &statement.arms {
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal);
                    }
                    if let Some(guard) = &arm.guard {
                        self.expression(guard);
                    }
                    self.statements(&arm.body.statements);
                }
            }
            StatementKind::Control(ControlTransfer::Return(Some(expression))) => {
                self.expression(expression);
            }
            StatementKind::Control(
                ControlTransfer::Break | ControlTransfer::Continue | ControlTransfer::Return(None),
            ) => {}
            StatementKind::Job(job) => {
                if let Some(span) = job.background_span {
                    self.summary.push(ModuleEffect::Job, span);
                    self.summary.push(ModuleEffect::Process, span);
                    self.summary.push(ModuleEffect::Status, span);
                    self.summary.push(ModuleEffect::OpaqueExternal, span);
                }
                self.chain(&job.chain);
            }
        }
    }

    fn if_statement(&mut self, statement: &flash_syntax::IfStatement) {
        if self.control.is_cancelled() {
            return;
        }
        self.chain(&statement.condition);
        self.statements(&statement.then_block.statements);
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.statements(&block.statements),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => {}
        }
    }

    fn chain(&mut self, chain: &ConditionalChain) {
        if self.control.is_cancelled() {
            return;
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                self.pipeline(pipeline);
            }
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline) {
        if self.control.is_cancelled() {
            return;
        }
        if !self.pipeline_is_standalone_help(pipeline) {
            self.summary.push(ModuleEffect::Status, pipeline.span());
        }
        let last = pipeline.stages().len().saturating_sub(1);
        for (index, stage) in pipeline.stages().iter().enumerate() {
            match stage.kind() {
                StageKind::Expression(expression) => {
                    self.expression(expression);
                    if index == last {
                        self.summary.push(ModuleEffect::Output, stage.span());
                    }
                }
                StageKind::Command(command) => self.command(command, index == last),
            }
        }
    }

    fn pipeline_is_standalone_help(&self, pipeline: &Pipeline) -> bool {
        let [stage] = pipeline.stages() else {
            return false;
        };
        let StageKind::Command(command) = stage.kind() else {
            return false;
        };
        command.head.kind() == CommandHeadKind::Bare
            && static_word_text(command.head.word(), &self.source).is_some_and(|name| {
                match self.commands.classify(&name) {
                    CommandClassification::Core { signature, .. }
                    | CommandClassification::Alias { signature, .. } => signature.name() == "help",
                    CommandClassification::Unknown | CommandClassification::Reserved { .. } => {
                        false
                    }
                }
            })
    }

    fn command(&mut self, command: &flash_syntax::CommandStage, is_last: bool) {
        if self.control.is_cancelled() {
            return;
        }
        self.word(command.head.word());
        let mut redirects_output = false;
        for item in &command.items {
            match item.kind() {
                CommandItemKind::Word(word) => self.word(word),
                CommandItemKind::Spread(_) => {}
                CommandItemKind::Closure(closure) => self.chain(&closure.body),
                CommandItemKind::Redirection(redirection) => {
                    redirects_output |= self.redirection(redirection.kind(), redirection.span());
                }
            }
        }

        let span = command.head.span();
        if command.head.kind() == CommandHeadKind::ForcedExternal {
            self.external(span, is_last && !redirects_output);
            return;
        }
        let Some(name) = static_word_text(command.head.word(), &self.source) else {
            self.external(span, is_last && !redirects_output);
            return;
        };
        let (canonical, output) = match self.commands.classify(&name) {
            CommandClassification::Core { signature, .. } => {
                (signature.name(), Some(signature.output()))
            }
            CommandClassification::Alias {
                canonical_name,
                signature,
                ..
            } => (canonical_name, Some(signature.output())),
            CommandClassification::Unknown => {
                self.external(span, is_last && !redirects_output);
                return;
            }
            CommandClassification::Reserved { .. } => {
                self.summary.push(ModuleEffect::OpaqueExternal, span);
                return;
            }
        };

        match canonical {
            "cd" => {
                self.summary.push(ModuleEffect::WorkingDirectory, span);
                self.summary.push(ModuleEffect::ChildEnvironment, span);
            }
            "ls" | "open" => self.summary.push(ModuleEffect::FilesystemRead, span),
            "save" => self.summary.push(ModuleEffect::FilesystemWrite, span),
            "fg" | "bg" | "wait" | "kill" => {
                self.summary.push(ModuleEffect::Job, span);
                self.summary.push(ModuleEffect::Process, span);
            }
            "jobs" => self.summary.push(ModuleEffect::Job, span),
            "exit" => self.summary.push(ModuleEffect::ProgramExit, span),
            "command" => {
                self.external(span, is_last && !redirects_output);
                return;
            }
            _ => {}
        }
        if is_last
            && !redirects_output
            && output.is_some_and(|output| match output {
                CommandOutput::Fixed(carrier) => carrier != Carrier::Empty,
                CommandOutput::SameAsInput => true,
            })
        {
            self.summary.push(ModuleEffect::Output, span);
        }
    }

    fn external(&mut self, span: Span, output: bool) {
        self.summary.push(ModuleEffect::Process, span);
        self.summary.push(ModuleEffect::OpaqueExternal, span);
        if output {
            self.summary.push(ModuleEffect::Output, span);
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind, span: Span) -> bool {
        if self.control.is_cancelled() {
            return false;
        }
        match redirection {
            RedirectionKind::Input { target, .. } => {
                self.summary.push(ModuleEffect::FilesystemRead, span);
                self.word(target);
                false
            }
            RedirectionKind::File(file) => {
                self.summary.push(ModuleEffect::FilesystemWrite, span);
                self.word(&file.target);
                file.descriptor.is_none_or(|descriptor| {
                    self.source
                        .slice(descriptor.span())
                        .is_ok_and(|text| text == "1")
                })
            }
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => false,
        }
    }

    fn expression(&mut self, expression: &Expression) {
        if self.control.is_cancelled() {
            return;
        }
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(_) => {
                if self
                    .names
                    .reference(&self.module, expression.span())
                    .is_some_and(|reference| {
                        matches!(reference.target(), ModuleReferenceTarget::DynamicStatus)
                    })
                {
                    self.summary.push(ModuleEffect::Status, expression.span());
                }
            }
            ExpressionKind::Symbol(_) => {}
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
            ExpressionKind::Closure(_) => {}
            ExpressionKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            ExpressionKind::GroupedJob(chain) => self.chain(chain),
            ExpressionKind::Call(call) => {
                for argument in &call.arguments {
                    self.expression(argument);
                }
                self.call(expression.span(), &call.callee);
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

    fn call(&mut self, call_span: Span, callee: &Expression) {
        if self.control.is_cancelled() {
            return;
        }
        let Some(reference) = self.names.reference(&self.module, callee.span()) else {
            if let ExpressionKind::Symbol(identifier) = callee.kind()
                && let Some(intrinsic) = self
                    .source
                    .slice(identifier.span())
                    .ok()
                    .and_then(ExpressionIntrinsic::lookup)
            {
                match intrinsic {
                    ExpressionIntrinsic::Env => {
                        self.summary.push(ModuleEffect::ChildEnvironment, call_span);
                    }
                    ExpressionIntrinsic::Glob => {
                        self.summary.push(ModuleEffect::FilesystemRead, call_span);
                    }
                    ExpressionIntrinsic::Float | ExpressionIntrinsic::Int => {}
                }
                return;
            }
            self.expression(callee);
            self.summary.push(ModuleEffect::OpaqueExternal, call_span);
            return;
        };
        let (module, declaration_span) = match reference.target() {
            ModuleReferenceTarget::Local {
                module,
                declaration_span,
            } => (module.clone(), *declaration_span),
            ModuleReferenceTarget::Imported {
                target_module,
                declaration_span,
                ..
            } => (target_module.clone(), *declaration_span),
            ModuleReferenceTarget::DynamicStatus | ModuleReferenceTarget::ScriptArguments => {
                self.summary.push(ModuleEffect::OpaqueExternal, call_span);
                return;
            }
        };
        let key = (module.clone(), declaration_span.start());
        if !self.active_functions.insert(key.clone()) {
            return;
        }
        let body = self.sources.script(&module).and_then(|script| {
            script.statements().iter().find_map(|statement| {
                let StatementKind::Function(function) = statement.kind() else {
                    return None;
                };
                (function.name.span() == declaration_span)
                    .then_some(function.body.statements.clone())
            })
        });
        if let Some(body) = body {
            let defining_source = self
                .sources
                .source(&module)
                .expect("a known callable belongs to a registered source")
                .clone();
            let previous_module = std::mem::replace(&mut self.module, module);
            let previous_source = std::mem::replace(&mut self.source, defining_source);
            self.statements(&body);
            self.module = previous_module;
            self.source = previous_source;
        } else {
            self.summary.push(ModuleEffect::OpaqueExternal, call_span);
        }
        self.active_functions.remove(&key);
    }

    fn literal(&mut self, literal: &flash_syntax::Literal) {
        if self.control.is_cancelled() {
            return;
        }
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            for part in parts {
                self.word_part(part);
            }
        }
    }

    fn word(&mut self, word: &Word) {
        if self.control.is_cancelled() {
            return;
        }
        for part in word.parts() {
            self.word_part(part);
        }
    }

    fn word_part(&mut self, part: &WordPart) {
        if self.control.is_cancelled() {
            return;
        }
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => {
                for part in parts {
                    self.word_part(part);
                }
            }
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape
            | WordPartKind::Variable(_) => {}
        }
    }
}

/// A completely loaded, parsed, and canonically graphed Flash source program.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleProgram {
    graph: ModuleGraph,
    sources: ModuleSourceRegistry,
    names: ModuleNameRegistry,
    types: ModuleTypeRegistry,
    effects: ModuleEffectRegistry,
}

/// One static command diagnostic found without expansion or host probing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModulePipelineError {
    module: ModuleId,
    diagnostic: Diagnostic,
}

impl ModulePipelineError {
    /// The canonical source module containing the faulty pipeline.
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    /// The structured `CMD001`-`CMD002` or `PIP001`-`PIP004` diagnostic.
    #[must_use]
    pub const fn diagnostic(&self) -> &Diagnostic {
        &self.diagnostic
    }

    fn primary_span(&self) -> Span {
        self.diagnostic
            .labels()
            .iter()
            .find(|label| label.style() == flash_syntax::LabelStyle::Primary)
            .expect("a pipeline diagnostic has a primary label")
            .span()
    }
}

impl fmt::Display for ModulePipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.diagnostic.message())
    }
}

impl std::error::Error for ModulePipelineError {}

struct StaticPipelineAnalyzer<'a> {
    module: &'a ModuleId,
    source: &'a SourceFile,
    commands: &'a CommandRegistry,
    errors: Vec<ModulePipelineError>,
    control: &'a AnalysisControl,
}

impl<'a> StaticPipelineAnalyzer<'a> {
    fn analyze(
        module: &'a ModuleId,
        source: &'a SourceFile,
        script: &'a Script,
        commands: &'a CommandRegistry,
        control: &'a AnalysisControl,
    ) -> Vec<ModulePipelineError> {
        let mut analyzer = Self {
            module,
            source,
            commands,
            errors: Vec::new(),
            control,
        };
        analyzer.statements(script.statements());
        analyzer
            .errors
            .sort_by_key(|error| error.primary_span().start());
        analyzer.errors
    }

    fn statements(&mut self, statements: &[Statement]) {
        for statement in statements {
            if self.control.is_cancelled() {
                break;
            }
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement) {
        if self.control.is_cancelled() {
            return;
        }
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => {}
            StatementKind::Declaration(declaration) => self.expression(&declaration.value),
            StatementKind::Assignment(assignment) => self.expression(&assignment.value),
            StatementKind::Environment(environment) => {
                if let flash_syntax::EnvironmentStatement::Export { value, .. } = environment {
                    self.expression(value);
                }
            }
            StatementKind::Function(function) => self.statements(&function.body.statements),
            StatementKind::If(statement) => self.if_statement(statement),
            StatementKind::While(statement) => {
                self.chain(&statement.condition);
                self.statements(&statement.body.statements);
            }
            StatementKind::For(statement) => {
                self.expression(&statement.iterable);
                self.statements(&statement.body.statements);
            }
            StatementKind::Match(statement) => {
                self.expression(&statement.value);
                for arm in &statement.arms {
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal);
                    }
                    if let Some(guard) = &arm.guard {
                        self.expression(guard);
                    }
                    self.statements(&arm.body.statements);
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

    fn if_statement(&mut self, statement: &flash_syntax::IfStatement) {
        if self.control.is_cancelled() {
            return;
        }
        self.chain(&statement.condition);
        self.statements(&statement.then_block.statements);
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.statements(&block.statements),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => {}
        }
    }

    fn chain(&mut self, chain: &ConditionalChain) {
        if self.control.is_cancelled() {
            return;
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                self.pipeline(pipeline);
            }
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline) {
        if self.control.is_cancelled() {
            return;
        }
        let contracts = pipeline
            .stages()
            .iter()
            .map(|stage| match stage.kind() {
                StageKind::Command(command) => self.command_contract(command),
                StageKind::Expression(_) => StageCarrierContract::unknown(),
            })
            .collect::<Vec<_>>();
        let operators = pipeline
            .operators()
            .iter()
            .map(|operator| *operator.kind())
            .collect::<Vec<_>>();
        for fault in analyze_pipeline_carriers(&contracts, &operators) {
            self.errors.push(self.carrier_diagnostic(pipeline, fault));
        }
        if pipeline.stages().len() > 1 {
            for stage in pipeline.stages() {
                if matches!(stage.kind(), StageKind::Expression(_)) {
                    self.errors.push(ModulePipelineError {
                        module: self.module.clone(),
                        diagnostic: Diagnostic::new(
                            Severity::Error,
                            "PIP004",
                            "an expression cannot be a stage in a multi-stage command pipeline",
                        )
                        .with_primary(stage.span(), "this is an expression stage"),
                    });
                }
            }
        }

        for stage in pipeline.stages() {
            match stage.kind() {
                StageKind::Expression(expression) => self.expression(expression),
                StageKind::Command(command) => {
                    self.word(command.head.word());
                    for item in &command.items {
                        match item.kind() {
                            CommandItemKind::Word(word) => self.word(word),
                            CommandItemKind::Closure(closure) => self.closure(closure),
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

    fn command_contract(&mut self, command: &flash_syntax::CommandStage) -> StageCarrierContract {
        if self.control.is_cancelled() {
            return StageCarrierContract::unknown();
        }
        if command.head.kind() == CommandHeadKind::ForcedExternal {
            return StageCarrierContract::known(
                self.source
                    .slice(command.head.word().span())
                    .expect("a parsed command head belongs to its source"),
                [Carrier::ByteStream],
                CommandOutput::Fixed(Carrier::ByteStream),
            );
        }
        let Some(name) = static_word_text(command.head.word(), self.source) else {
            return StageCarrierContract::unknown();
        };
        match self.commands.classify(&name) {
            CommandClassification::Core {
                signature,
                lifecycle,
            }
            | CommandClassification::Alias {
                signature,
                lifecycle,
                ..
            } => {
                if lifecycle.deprecated_since_release().is_some() {
                    self.errors.push(self.deprecated_command_diagnostic(
                        command.head.word().span(),
                        &name,
                        lifecycle,
                    ));
                }
                StageCarrierContract::known(name, signature.inputs(), signature.output())
            }
            CommandClassification::Reserved {
                purpose,
                replacement,
                ..
            } => {
                self.errors.push(self.reserved_command_diagnostic(
                    command.head.word().span(),
                    &name,
                    purpose,
                    replacement,
                ));
                StageCarrierContract::unknown()
            }
            CommandClassification::Unknown => StageCarrierContract::known(
                name,
                [Carrier::ByteStream],
                CommandOutput::Fixed(Carrier::ByteStream),
            ),
        }
    }

    fn deprecated_command_diagnostic(
        &self,
        span: Span,
        name: &str,
        lifecycle: &CommandLifecycle,
    ) -> ModulePipelineError {
        let release = lifecycle
            .deprecated_since_release()
            .expect("the caller observed deprecation metadata");
        let mut diagnostic = Diagnostic::new(
            Severity::Warning,
            "CMD001",
            format!("`{name}` is deprecated since {release}"),
        )
        .with_primary(span, "this command spelling is deprecated");
        if let Some(replacement) = lifecycle.replacement() {
            diagnostic = diagnostic.with_note(format!("use `{replacement}` instead"));
        }
        ModulePipelineError {
            module: self.module.clone(),
            diagnostic,
        }
    }

    fn reserved_command_diagnostic(
        &self,
        span: Span,
        name: &str,
        purpose: &str,
        replacement: Option<&str>,
    ) -> ModulePipelineError {
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            "CMD002",
            format!("`{name}` is reserved and cannot be invoked as a bare command"),
        )
        .with_primary(span, purpose);
        if let Some(replacement) = replacement {
            diagnostic = diagnostic.with_note(format!("use `{replacement}` instead"));
        }
        diagnostic = diagnostic.with_note(format!(
            "use `^{name}` or `command {name}` for intentional external execution"
        ));
        ModulePipelineError {
            module: self.module.clone(),
            diagnostic,
        }
    }

    fn carrier_diagnostic(
        &self,
        pipeline: &Pipeline,
        fault: PipelineCarrierFault,
    ) -> ModulePipelineError {
        let diagnostic = match fault {
            PipelineCarrierFault::HeadInput {
                stage,
                command,
                accepted,
            } => Diagnostic::new(
                Severity::Error,
                "PIP001",
                format!("`{command}` needs an upstream structured pipeline value"),
            )
            .with_primary(
                pipeline.stages()[stage].span(),
                format!(
                    "accepts {}, not an empty or byte input",
                    carrier_set(&accepted)
                ),
            ),
            PipelineCarrierFault::MergedEdgeNotByteStream {
                edge,
                producer_command,
                produced,
            } => Diagnostic::new(
                Severity::Error,
                "PIP002",
                format!("`|&` requires bytes, but `{producer_command}` produces {produced:?}"),
            )
            .with_primary(
                pipeline.operators()[edge].span(),
                "stderr can merge only with a byte stream",
            ),
            PipelineCarrierFault::CarrierMismatch { edge, mismatch } => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    "PIP003",
                    format!(
                        "`{}` produces {:?}, but `{}` accepts {}",
                        mismatch.producer_command,
                        mismatch.produced,
                        mismatch.consumer_command,
                        carrier_set(&mismatch.accepted),
                    ),
                )
                .with_primary(
                    pipeline.operators()[edge].span(),
                    "these adjacent carrier contracts are incompatible",
                );
                diagnostic = match mismatch.bridge {
                    Some(CarrierBridge::StructuredToByte) => diagnostic.with_note(
                        "add an explicit `encode`/`to` boundary to serialize structured values",
                    ),
                    Some(CarrierBridge::ByteToStructured) => diagnostic.with_note(
                        "add an explicit `decode`/`from` boundary to parse bytes into values",
                    ),
                    None => diagnostic,
                };
                diagnostic
            }
        };
        ModulePipelineError {
            module: self.module.clone(),
            diagnostic,
        }
    }

    fn expression(&mut self, expression: &Expression) {
        if self.control.is_cancelled() {
            return;
        }
        match expression.kind() {
            ExpressionKind::Literal(literal) => self.literal(literal),
            ExpressionKind::Variable(_) | ExpressionKind::Symbol(_) => {}
            ExpressionKind::List(elements) => {
                for element in elements {
                    self.expression(element);
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
            ExpressionKind::Closure(closure) => self.closure(closure),
            ExpressionKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            ExpressionKind::GroupedJob(chain) => self.chain(chain),
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
            ExpressionKind::Member(member) => self.expression(&member.target),
            ExpressionKind::Unary(unary) => self.expression(&unary.operand),
            ExpressionKind::Binary(binary) => {
                self.expression(&binary.left);
                self.expression(&binary.right);
            }
        }
    }

    fn closure(&mut self, closure: &Closure) {
        if self.control.is_cancelled() {
            return;
        }
        self.chain(&closure.body);
    }

    fn literal(&mut self, literal: &flash_syntax::Literal) {
        if self.control.is_cancelled() {
            return;
        }
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            self.word_parts(parts);
        }
    }

    fn word(&mut self, word: &Word) {
        if self.control.is_cancelled() {
            return;
        }
        self.word_parts(word.parts());
    }

    fn word_parts(&mut self, parts: &[WordPart]) {
        for part in parts {
            if self.control.is_cancelled() {
                break;
            }
            self.word_part(part);
        }
    }

    fn word_part(&mut self, part: &WordPart) {
        if self.control.is_cancelled() {
            return;
        }
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            WordPartKind::Bare
            | WordPartKind::BareEscape
            | WordPartKind::SingleQuoted
            | WordPartKind::DoubleText
            | WordPartKind::DoubleEscape
            | WordPartKind::Variable(_) => {}
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind) {
        if self.control.is_cancelled() {
            return;
        }
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => {}
        }
    }
}

fn carrier_set(carriers: &[Carrier]) -> String {
    carriers
        .iter()
        .map(|carrier| format!("{carrier:?}"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn static_word_text(word: &Word, source: &SourceFile) -> Option<String> {
    let mut text = String::new();
    append_static_parts(word.parts(), source, &mut text).then_some(text)
}

fn append_static_parts(parts: &[WordPart], source: &SourceFile, text: &mut String) -> bool {
    for part in parts {
        let raw = source
            .slice(part.span())
            .expect("a parsed word part belongs to its source");
        match part.kind() {
            WordPartKind::Bare | WordPartKind::DoubleText => text.push_str(raw),
            WordPartKind::SingleQuoted => text.push_str(&raw[1..raw.len() - 1]),
            WordPartKind::BareEscape => text.push_str(&raw[1..]),
            WordPartKind::DoubleEscape => text.push_str(&decode_static_double_escape(raw)),
            WordPartKind::DoubleQuoted(inner) => {
                if !append_static_parts(inner, source, text) {
                    return false;
                }
            }
            WordPartKind::Variable(_)
            | WordPartKind::BracedInterpolation(_)
            | WordPartKind::CommandSubstitution(_) => return false,
        }
    }
    true
}

fn decode_static_double_escape(raw: &str) -> String {
    if matches!(raw, "\\\n" | "\\\r\n") {
        return String::new();
    }
    let body = &raw[1..];
    match body.chars().next().expect("a validated escape has a body") {
        '\\' => "\\".to_owned(),
        '"' => "\"".to_owned(),
        '$' => "$".to_owned(),
        'n' => "\n".to_owned(),
        'r' => "\r".to_owned(),
        't' => "\t".to_owned(),
        '0' => "\0".to_owned(),
        'u' => {
            let hex = body
                .trim_start_matches('u')
                .trim_start_matches('{')
                .trim_end_matches('}');
            u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map_or_else(|| body.to_owned(), |scalar| scalar.to_string())
        }
        _ => body.to_owned(),
    }
}

/// One decoded source retained by a module-analysis report.
///
/// Invalid syntax leaves `script` absent while preserving the source identity
/// and text needed to render its diagnostics. Read and UTF-8 failures cannot
/// produce a retained [`SourceFile`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleAnalysisSource {
    module: ModuleId,
    source: SourceFile,
    script: Option<Script>,
}

impl ModuleAnalysisSource {
    /// The canonical identity assigned to this source.
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    /// The decoded source with its stable discovery identity.
    #[must_use]
    pub const fn source(&self) -> &SourceFile {
        &self.source
    }

    /// Parsed syntax, absent only when parsing this retained source failed.
    #[must_use]
    pub const fn script(&self) -> Option<&Script> {
        self.script.as_ref()
    }
}

/// One structured error retained in deterministic module-analysis order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleAnalysisIssue {
    error: ModuleProgramError,
}

impl ModuleAnalysisIssue {
    fn new(error: ModuleProgramError) -> Self {
        Self { error }
    }

    /// The structured module-program error represented by this issue.
    #[must_use]
    pub const fn error(&self) -> &ModuleProgramError {
        &self.error
    }

    /// The issue severity. Namespace deprecations are warnings; construction
    /// failures and reserved-name diagnostics are errors.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match &self.error {
            ModuleProgramError::Pipelines(error) => error.diagnostic().severity(),
            _ => Severity::Error,
        }
    }
}

/// Host-free analysis of one canonical root and its static import closure.
///
/// Retained sources and ordered issues remain available after a failed run. A
/// complete executable program is exposed only when no error was found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleAnalysisReport {
    sources: Vec<ModuleAnalysisSource>,
    issues: Vec<ModuleAnalysisIssue>,
    program: Option<ModuleProgram>,
}

/// A controlled analysis either exposes one complete report or no partial state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleAnalysisOutcome {
    Complete(Box<ModuleAnalysisReport>),
    Cancelled,
}

impl ModuleAnalysisReport {
    /// Decoded sources in deterministic first-visit depth-first order.
    #[must_use]
    pub fn sources(&self) -> &[ModuleAnalysisSource] {
        &self.sources
    }

    /// Structured issues in analysis-owned order.
    #[must_use]
    pub fn issues(&self) -> &[ModuleAnalysisIssue] {
        &self.issues
    }

    /// The complete program, present exactly when analysis has no errors.
    #[must_use]
    pub const fn program(&self) -> Option<&ModuleProgram> {
        self.program.as_ref()
    }

    /// Whether at least one error-classified issue prevents a complete program.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity() == Severity::Error)
    }
}

/// A module-program construction failure rendered while its analyzed sources
/// are still available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleProgramLoadError {
    error: Box<ModuleProgramError>,
    rendered: String,
}

impl ModuleProgramLoadError {
    fn new(error: ModuleProgramError, sources: &[ModuleAnalysisSource]) -> Self {
        let diagnostics = error.diagnostics();
        let available = sources
            .iter()
            .map(ModuleAnalysisSource::source)
            .collect::<Vec<_>>();
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

    /// Resolved annotations and named-function signatures.
    #[must_use]
    pub const fn types(&self) -> &ModuleTypeRegistry {
        &self.types
    }

    /// Host-free direct and named-dependency-folded initializer effects.
    #[must_use]
    pub const fn effects(&self) -> &ModuleEffectRegistry {
        &self.effects
    }

    pub(crate) fn runtime_binding_types(&self) -> RuntimeBindingTypes {
        let mut by_source = BTreeMap::new();
        let mut functions_by_source = BTreeMap::new();
        for entry in self.sources.entries() {
            let types = self.types.by_module.get(entry.module());
            by_source.insert(
                entry.source().id(),
                types.map_or_else(Vec::new, |types| types.bindings.clone()),
            );
            functions_by_source.insert(
                entry.source().id(),
                types.map_or_else(Vec::new, |types| types.functions.clone()),
            );
        }
        RuntimeBindingTypes {
            by_source,
            functions_by_source,
        }
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
        let report = self.analyze(requested);
        match report.program {
            Some(program) => Ok(program),
            None => Err(report
                .issues
                .into_iter()
                .next()
                .expect("an incomplete module analysis has an issue")
                .error),
        }
    }

    /// Loads one module program and retains enough source context to render a
    /// frontend diagnostic if construction fails.
    pub fn load_for_frontend(
        &self,
        requested: &Path,
    ) -> Result<ModuleProgram, ModuleProgramLoadError> {
        let report = self.analyze(requested);
        match report.program {
            Some(program) => Ok(program),
            None => {
                let error = report
                    .issues
                    .into_iter()
                    .next()
                    .expect("an incomplete module analysis has an issue")
                    .error;
                Err(ModuleProgramLoadError::new(error, &report.sources))
            }
        }
    }

    /// Analyzes one root and every reachable static import without executing
    /// source. Discovery accumulates independent branch failures while
    /// retaining every decoded source available for later diagnostics. On a
    /// complete graph, name analysis accumulates independent export, import,
    /// binding, and reference failures. On clean names, signature analysis
    /// accumulates independent annotation, known-call, and result failures.
    #[must_use]
    pub fn analyze(&self, requested: &Path) -> ModuleAnalysisReport {
        self.complete_never_cancelled(self.analyze_controlled(requested, &AnalysisControl::never()))
    }

    /// Analyzes with cooperative cancellation and never exposes a partial
    /// diagnostic report or program when cancellation wins.
    #[must_use]
    pub fn analyze_controlled(
        &self,
        requested: &Path,
        control: &AnalysisControl,
    ) -> ModuleAnalysisOutcome {
        self.analyze_internal(requested, None, control)
    }

    /// Performs full non-executing analysis, including static pipeline carrier
    /// checking against the injected command registry.
    ///
    /// Pipeline checking walks every retained parsed source even when an earlier
    /// graph, name, or signature phase failed. It never expands a word or probes
    /// an executable. The legacy execution loaders continue to use [`Self::analyze`]
    /// so runtime preflight retains its established error surface.
    #[must_use]
    pub fn analyze_with_commands(
        &self,
        requested: &Path,
        commands: &CommandRegistry,
    ) -> ModuleAnalysisReport {
        self.complete_never_cancelled(self.analyze_with_commands_controlled(
            requested,
            commands,
            &AnalysisControl::never(),
        ))
    }

    /// Performs full command-aware analysis with cooperative cancellation.
    #[must_use]
    pub fn analyze_with_commands_controlled(
        &self,
        requested: &Path,
        commands: &CommandRegistry,
        control: &AnalysisControl,
    ) -> ModuleAnalysisOutcome {
        self.analyze_internal(requested, Some(commands), control)
    }

    fn complete_never_cancelled(&self, outcome: ModuleAnalysisOutcome) -> ModuleAnalysisReport {
        match outcome {
            ModuleAnalysisOutcome::Complete(report) => *report,
            ModuleAnalysisOutcome::Cancelled => {
                unreachable!("a never-cancelled analysis control cannot cancel")
            }
        }
    }

    fn analyze_internal(
        &self,
        requested: &Path,
        commands: Option<&CommandRegistry>,
        control: &AnalysisControl,
    ) -> ModuleAnalysisOutcome {
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let root_result = self.resolver.resolve_root(requested);
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let root = match root_result {
            Ok(root) => root,
            Err(error) => {
                return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                    sources: Vec::new(),
                    issues: vec![ModuleAnalysisIssue::new(ModuleProgramError::Resolution(
                        error,
                    ))],
                    program: None,
                }));
            }
        };
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let mut graph = ModuleGraph::new(root.clone());
        let mut sources = ModuleSourceRegistry::default();
        let mut retained = Vec::new();
        let mut attempted = BTreeSet::new();
        let mut issues = Vec::new();
        self.discover_module(
            root,
            None,
            &mut graph,
            &mut sources,
            &mut retained,
            &mut attempted,
            &mut issues,
            control,
        );
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let mut pipeline_issues = Vec::new();
        if let Some(commands) = commands {
            for entry in &retained {
                if control.is_cancelled() {
                    return ModuleAnalysisOutcome::Cancelled;
                }
                if let Some(script) = entry.script() {
                    pipeline_issues.extend(
                        StaticPipelineAnalyzer::analyze(
                            entry.module(),
                            entry.source(),
                            script,
                            commands,
                            control,
                        )
                        .into_iter()
                        .map(|error| {
                            ModuleAnalysisIssue::new(ModuleProgramError::Pipelines(Box::new(error)))
                        }),
                    );
                }
            }
        }
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }

        if !issues.is_empty() {
            issues.extend(pipeline_issues);
            return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                sources: retained,
                issues,
                program: None,
            }));
        }

        let names_result = ModuleNameRegistry::analyze(&graph, &sources, control);
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let names = match names_result {
            Ok(names) => names,
            Err(errors) => {
                let mut issues = errors
                    .into_iter()
                    .map(|error| {
                        ModuleAnalysisIssue::new(ModuleProgramError::Names(Box::new(error)))
                    })
                    .collect::<Vec<_>>();
                issues.extend(pipeline_issues);
                return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                    sources: retained,
                    issues,
                    program: None,
                }));
            }
        };
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let types_result = ModuleTypeRegistry::analyze(&sources, &names, control);
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        let types = match types_result {
            Ok(types) => types,
            Err(errors) => {
                let mut issues = errors
                    .into_iter()
                    .map(|error| {
                        ModuleAnalysisIssue::new(ModuleProgramError::Signatures(Box::new(error)))
                    })
                    .collect::<Vec<_>>();
                issues.extend(pipeline_issues);
                return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                    sources: retained,
                    issues,
                    program: None,
                }));
            }
        };
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        if pipeline_issues
            .iter()
            .any(|issue| issue.severity() == Severity::Error)
        {
            return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                sources: retained,
                issues: pipeline_issues,
                program: None,
            }));
        }
        let effect_commands = commands.cloned().unwrap_or_else(standard_registry);
        let effects =
            ModuleEffectRegistry::analyze(&graph, &sources, &names, &effect_commands, control);
        if control.is_cancelled() {
            return ModuleAnalysisOutcome::Cancelled;
        }
        ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
            sources: retained,
            issues: pipeline_issues,
            program: Some(ModuleProgram {
                graph,
                sources,
                names,
                types,
                effects,
            }),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn discover_module(
        &self,
        module: ModuleId,
        imported_by: Option<ModuleImport>,
        graph: &mut ModuleGraph,
        sources: &mut ModuleSourceRegistry,
        retained: &mut Vec<ModuleAnalysisSource>,
        attempted: &mut BTreeSet<ModuleId>,
        issues: &mut Vec<ModuleAnalysisIssue>,
        control: &AnalysisControl,
    ) {
        if control.is_cancelled() {
            return;
        }
        if !attempted.insert(module.clone()) {
            return;
        }

        if control.is_cancelled() {
            return;
        }
        let bytes = match self.source_loader.load(&module) {
            Ok(bytes) => bytes,
            Err(cause) => {
                issues.push(ModuleAnalysisIssue::new(ModuleProgramError::SourceRead {
                    module: module.clone(),
                    imported_by: imported_by.map(Box::new),
                    cause,
                }));
                return;
            }
        };
        if control.is_cancelled() {
            return;
        }
        let source_id = match u32::try_from(retained.len()) {
            Ok(id) => SourceId::new(id),
            Err(_) => {
                issues.push(ModuleAnalysisIssue::new(
                    ModuleProgramError::SourceIdentityExhausted,
                ));
                return;
            }
        };
        let source_name = module.path().to_string_lossy().into_owned();
        let source = match SourceFile::from_bytes(source_id, source_name, bytes) {
            Ok(source) => source,
            Err(error) => {
                issues.push(ModuleAnalysisIssue::new(ModuleProgramError::InvalidUtf8 {
                    module,
                    imported_by: imported_by.map(Box::new),
                    valid_up_to: error.utf8_error().valid_up_to(),
                }));
                return;
            }
        };
        let script = match parse_with_control(&source, &|| control.is_cancelled()) {
            ControlledParseOutcome::Cancelled => return,
            ControlledParseOutcome::Parsed(ParseOutcome::Complete(script)) => script,
            ControlledParseOutcome::Parsed(ParseOutcome::Incomplete(incomplete)) => {
                let diagnostic = Diagnostic::new(
                    Severity::Error,
                    "SYN002",
                    format!("incomplete input: {}", incomplete.reason()),
                )
                .with_primary(
                    incomplete.span(),
                    "input ends before this construct is complete",
                );
                retained.push(ModuleAnalysisSource {
                    module: module.clone(),
                    source: source.clone(),
                    script: None,
                });
                issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Syntax {
                    module,
                    source: Box::new(source),
                    diagnostics: vec![diagnostic],
                }));
                return;
            }
            ControlledParseOutcome::Parsed(ParseOutcome::Invalid(diagnostics)) => {
                retained.push(ModuleAnalysisSource {
                    module: module.clone(),
                    source: source.clone(),
                    script: None,
                });
                issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Syntax {
                    module,
                    source: Box::new(source),
                    diagnostics,
                }));
                return;
            }
        };
        if control.is_cancelled() {
            return;
        }

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
        sources.register(module.clone(), source.clone(), script.clone());
        retained.push(ModuleAnalysisSource {
            module: module.clone(),
            source,
            script: Some(script),
        });

        for (requested, span) in imports {
            if control.is_cancelled() {
                return;
            }
            let import = match self.resolver.resolve_import(&module, &requested, span) {
                Ok(import) => import,
                Err(error) => {
                    issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Resolution(
                        error,
                    )));
                    continue;
                }
            };
            if let Err(error) = graph.add_import(import.clone()) {
                issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Graph(error)));
                continue;
            }
            self.discover_module(
                import.target().clone(),
                Some(import),
                graph,
                sources,
                retained,
                attempted,
                issues,
                control,
            );
        }
    }
}

/// A failure while building explicit module export/import-name tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleNameError {
    /// A lexical declaration attempts to occupy a reserved dynamic name.
    ReservedBinding {
        module: ModuleId,
        name: String,
        declaration_span: Span,
    },
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
            Self::ReservedBinding { module, .. }
            | Self::UnknownExport { module, .. }
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
            Self::ReservedBinding {
                declaration_span,
                name,
                ..
            } => Diagnostic::new(Severity::Error, "MOD011", self.to_string()).with_primary(
                *declaration_span,
                format!("`{name}` is a reserved dynamic binding"),
            ),
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
            Self::ReservedBinding { module, name, .. } => write!(
                formatter,
                "module `{}` cannot declare reserved binding `{name}`",
                module.path().display()
            ),
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

/// A failure while resolving annotations or validating a known function call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleTypeError {
    UnknownType {
        module: ModuleId,
        name: String,
        span: Span,
    },
    InvalidTypeArity {
        module: ModuleId,
        name: String,
        expected: usize,
        actual: usize,
        span: Span,
    },
    CallArity {
        module: ModuleId,
        name: String,
        call_span: Span,
        expected: usize,
        actual: usize,
        declaration_span: Span,
    },
    IntrinsicCallArity {
        module: ModuleId,
        name: &'static str,
        call_span: Span,
        expected: usize,
        actual: usize,
    },
    ArgumentMismatch {
        module: ModuleId,
        name: String,
        parameter: String,
        argument_span: Span,
        expected: ValueType,
        actual: ValueType,
        parameter_span: Span,
    },
    IntrinsicArgumentMismatch {
        module: ModuleId,
        name: &'static str,
        argument_span: Span,
        expected: &'static str,
        actual: ValueType,
    },
    ResultMismatch {
        module: ModuleId,
        name: String,
        result_span: Span,
        expected: ValueType,
        actual: ValueType,
        annotation_span: Span,
    },
    ByteCaptureInWord {
        module: ModuleId,
        span: Span,
    },
}

impl ModuleTypeError {
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        match self {
            Self::UnknownType { module, .. }
            | Self::InvalidTypeArity { module, .. }
            | Self::CallArity { module, .. }
            | Self::IntrinsicCallArity { module, .. }
            | Self::ArgumentMismatch { module, .. }
            | Self::IntrinsicArgumentMismatch { module, .. }
            | Self::ResultMismatch { module, .. }
            | Self::ByteCaptureInWord { module, .. } => module,
        }
    }

    const fn primary_span(&self) -> Span {
        match self {
            Self::UnknownType { span, .. } | Self::InvalidTypeArity { span, .. } => *span,
            Self::CallArity { call_span, .. } | Self::IntrinsicCallArity { call_span, .. } => {
                *call_span
            }
            Self::ArgumentMismatch { argument_span, .. }
            | Self::IntrinsicArgumentMismatch { argument_span, .. } => *argument_span,
            Self::ResultMismatch { result_span, .. } => *result_span,
            Self::ByteCaptureInWord { span, .. } => *span,
        }
    }

    #[must_use]
    pub fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::UnknownType { span, name, .. } => {
                Diagnostic::new(Severity::Error, "SIG001", self.to_string())
                    .with_primary(*span, format!("unknown value type `{name}`"))
            }
            Self::InvalidTypeArity {
                span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG002", self.to_string()).with_primary(
                *span,
                format!("expected {expected} type arguments, found {actual}"),
            ),
            Self::CallArity {
                call_span,
                declaration_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG003", self.to_string())
                .with_primary(
                    *call_span,
                    format!("expected {expected} arguments, found {actual}"),
                )
                .with_secondary(*declaration_span, "function declared here"),
            Self::IntrinsicCallArity {
                call_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG003", self.to_string()).with_primary(
                *call_span,
                format!("expected {expected} arguments, found {actual}"),
            ),
            Self::ArgumentMismatch {
                argument_span,
                parameter_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG004", self.to_string())
                .with_primary(
                    *argument_span,
                    format!("this argument is `{actual}`, expected `{expected}`"),
                )
                .with_secondary(*parameter_span, "parameter type declared here"),
            Self::IntrinsicArgumentMismatch {
                argument_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG004", self.to_string()).with_primary(
                *argument_span,
                format!("this argument is `{actual}`, expected `{expected}`"),
            ),
            Self::ResultMismatch {
                result_span,
                annotation_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG005", self.to_string())
                .with_primary(
                    *result_span,
                    format!("this result is `{actual}`, expected `{expected}`"),
                )
                .with_secondary(*annotation_span, "function result type declared here"),
            Self::ByteCaptureInWord { span, .. } => {
                Diagnostic::new(Severity::Error, "SIG006", self.to_string()).with_primary(
                    *span,
                    "byte capture is a `Bytes` value and cannot be inserted into a command word",
                )
            }
        }
    }
}

impl fmt::Display for ModuleTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownType { module, name, .. } => write!(
                formatter,
                "module `{}` uses unknown value type `{name}`",
                module.path().display()
            ),
            Self::InvalidTypeArity {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` gives type `{name}` {actual} arguments; expected {expected}",
                module.path().display()
            ),
            Self::CallArity {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` calls `{name}` with {actual} arguments; expected {expected}",
                module.path().display()
            ),
            Self::IntrinsicCallArity {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` calls intrinsic `{name}` with {actual} arguments; expected {expected}",
                module.path().display()
            ),
            Self::ArgumentMismatch {
                module,
                name,
                parameter,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` passes `{actual}` to `{name}` parameter `{parameter}`; expected `{expected}`",
                module.path().display()
            ),
            Self::IntrinsicArgumentMismatch {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` passes `{actual}` to intrinsic `{name}`; expected `{expected}`",
                module.path().display()
            ),
            Self::ResultMismatch {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` returns `{actual}` from `{name}`; expected `{expected}`",
                module.path().display()
            ),
            Self::ByteCaptureInWord { module, .. } => write!(
                formatter,
                "module `{}` inserts a `Bytes` capture into a command word; decode it explicitly or bind it as a value",
                module.path().display()
            ),
        }
    }
}

impl std::error::Error for ModuleTypeError {}

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
    /// Static type resolution or known-call validation failed.
    Signatures(Box<ModuleTypeError>),
    /// Static command-pipeline carrier analysis failed.
    Pipelines(Box<ModulePipelineError>),
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
            Self::Signatures(error) => vec![error.diagnostic()],
            Self::Pipelines(error) => vec![error.diagnostic().clone()],
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
            Self::Signatures(error) => Some(error.module()),
            Self::Pipelines(error) => Some(error.module()),
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
            Self::Signatures(error) => error.fmt(formatter),
            Self::Pipelines(error) => error.fmt(formatter),
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
            Self::Signatures(error) => Some(error),
            Self::Pipelines(error) => Some(error),
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
