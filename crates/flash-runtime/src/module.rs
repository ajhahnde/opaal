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
    BinaryOperator, Block, Closure, CommandItemKind, ConditionalChain, ControlTransfer, Diagnostic,
    ElseBranch, Expression, ExpressionKind, LiteralKind, MatchArm, ParseOutcome, Pattern, Pipeline,
    RecordKey, RedirectionKind, Script, Severity, SourceFile, SourceId, Span, StageKind, Statement,
    StatementKind, TypeReference, UnaryOperator, Word, WordPart, WordPartKind, parse,
    render_diagnostic_sources,
};

use crate::Value;

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
            let references =
                ReferenceResolver::new(entry, &registry, entry.module() == graph.root())
                    .resolve()?;
            registry
                .by_module
                .get_mut(entry.module())
                .expect("every registered source has a name table")
                .references = references;
        }

        Ok(registry)
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

    fn binding_type(&self, module: &ModuleId, declaration_span: Span) -> Option<&ValueType> {
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
    ) -> Result<Self, Box<ModuleTypeError>> {
        let mut registry = Self::default();
        for entry in sources.entries() {
            let types = TypeCollector::new(entry).collect()?;
            registry.by_module.insert(entry.module().clone(), types);
        }
        for entry in sources.entries() {
            SignatureValidator::new(entry, names, &registry).validate()?;
        }
        Ok(registry)
    }
}

struct TypeCollector<'a> {
    entry: &'a RegisteredModuleSource,
    types: ModuleTypes,
}

impl<'a> TypeCollector<'a> {
    fn new(entry: &'a RegisteredModuleSource) -> Self {
        Self {
            entry,
            types: ModuleTypes::default(),
        }
    }

    fn collect(mut self) -> Result<ModuleTypes, Box<ModuleTypeError>> {
        self.statements(self.entry.script().statements())?;
        Ok(self.types)
    }

    fn statements(&mut self, statements: &[Statement]) -> Result<(), Box<ModuleTypeError>> {
        for statement in statements {
            self.statement(statement)?;
        }
        Ok(())
    }

    fn statement(&mut self, statement: &Statement) -> Result<(), Box<ModuleTypeError>> {
        match statement.kind() {
            StatementKind::Import(_) | StatementKind::ModuleExport(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                if let Some(annotation) = &declaration.type_annotation {
                    let value_type = self.resolve_type(annotation)?;
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
                            let value_type = self.resolve_type(annotation)?;
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
                    Some(annotation) => self.resolve_type(annotation)?,
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
            ExpressionKind::CommandSubstitution(chain) | ExpressionKind::GroupedJob(chain) => {
                self.chain(chain)
            }
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
        for parameter in &closure.parameters {
            if let Some(annotation) = &parameter.type_annotation {
                let value_type = self.resolve_type(annotation)?;
                self.types.bindings.push(ResolvedBindingType {
                    declaration_span: parameter.name.span(),
                    value_type,
                });
            }
        }
        self.chain(&closure.body)
    }

    fn literal(&mut self, literal: &flash_syntax::Literal) -> Result<(), Box<ModuleTypeError>> {
        if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
            self.word_parts(parts)?;
        }
        Ok(())
    }

    fn word(&mut self, word: &Word) -> Result<(), Box<ModuleTypeError>> {
        self.word_parts(word.parts())
    }

    fn word_parts(&mut self, parts: &[WordPart]) -> Result<(), Box<ModuleTypeError>> {
        for part in parts {
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&mut self, part: &WordPart) -> Result<(), Box<ModuleTypeError>> {
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => self.expression(expression),
            WordPartKind::CommandSubstitution(chain) => self.chain(chain),
            _ => Ok(()),
        }
    }

    fn redirection(&mut self, redirection: &RedirectionKind) -> Result<(), Box<ModuleTypeError>> {
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => Ok(()),
        }
    }

    fn resolve_type(
        &mut self,
        reference: &TypeReference,
    ) -> Result<ValueType, Box<ModuleTypeError>> {
        let value_type = self.resolve_type_value(reference)?;
        self.types.annotations.push(ResolvedTypeAnnotation {
            span: reference.span,
            value_type: value_type.clone(),
        });
        Ok(value_type)
    }

    fn resolve_type_value(
        &mut self,
        reference: &TypeReference,
    ) -> Result<ValueType, Box<ModuleTypeError>> {
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
}

impl<'a> SignatureValidator<'a> {
    fn new(
        entry: &'a RegisteredModuleSource,
        names: &'a ModuleNameRegistry,
        types: &'a ModuleTypeRegistry,
    ) -> Self {
        Self {
            entry,
            names,
            types,
        }
    }

    fn validate(&self) -> Result<(), Box<ModuleTypeError>> {
        self.statements(self.entry.script().statements())
    }

    fn statements(&self, statements: &[Statement]) -> Result<(), Box<ModuleTypeError>> {
        for statement in statements {
            self.statement(statement)?;
        }
        Ok(())
    }

    fn function(
        &self,
        function: &flash_syntax::FunctionDefinition,
    ) -> Result<(), Box<ModuleTypeError>> {
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
        Err(Box::new(ModuleTypeError::ResultMismatch {
            module: self.entry.module().clone(),
            name: signature.name().to_owned(),
            result_span,
            expected: signature.result().clone(),
            actual,
            annotation_span: signature
                .result_annotation_span()
                .unwrap_or_else(|| signature.declaration_span()),
        }))
    }

    fn statement(&self, statement: &Statement) -> Result<(), Box<ModuleTypeError>> {
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
        self.chain(&statement.condition)?;
        self.statements(&statement.then_block.statements)?;
        match &statement.else_branch {
            Some(ElseBranch::Block(block)) => self.statements(&block.statements),
            Some(ElseBranch::If(nested)) => self.if_statement(nested.kind()),
            None => Ok(()),
        }
    }

    fn chain(&self, chain: &ConditionalChain) -> Result<(), Box<ModuleTypeError>> {
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
            ExpressionKind::CommandSubstitution(chain) | ExpressionKind::GroupedJob(chain) => {
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
            return Ok(None);
        };
        let (target_module, declaration_span) = match reference.target() {
            ModuleReferenceTarget::ScriptArguments => return Ok(None),
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
            return Err(Box::new(ModuleTypeError::CallArity {
                module: self.entry.module().clone(),
                name: signature.name().to_owned(),
                call_span,
                expected: signature.parameters().len(),
                actual: call.arguments.len(),
                declaration_span: signature.declaration_span(),
            }));
        }
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
                return Err(Box::new(ModuleTypeError::ArgumentMismatch {
                    module: self.entry.module().clone(),
                    name: signature.name().to_owned(),
                    parameter: parameter.name().to_owned(),
                    argument_span: argument.span(),
                    expected: parameter.value_type().clone(),
                    actual,
                    parameter_span: parameter
                        .annotation_span()
                        .unwrap_or_else(|| parameter.declaration_span()),
                }));
            }
        }
        Ok(Some(signature.result().clone()))
    }

    fn literal(
        &self,
        literal: &flash_syntax::Literal,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
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
        self.word_parts(word.parts())
    }

    fn word_parts(&self, parts: &[WordPart]) -> Result<(), Box<ModuleTypeError>> {
        for part in parts {
            self.word_part(part)?;
        }
        Ok(())
    }

    fn word_part(&self, part: &WordPart) -> Result<(), Box<ModuleTypeError>> {
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => {
                self.expression(expression).map(|_| ())
            }
            WordPartKind::CommandSubstitution(chain) => self.chain(chain),
            _ => Ok(()),
        }
    }

    fn redirection(&self, redirection: &RedirectionKind) -> Result<(), Box<ModuleTypeError>> {
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => Ok(()),
        }
    }
}

struct ReferenceResolver<'a> {
    entry: &'a RegisteredModuleSource,
    scopes: Vec<BTreeMap<String, ModuleReferenceTarget>>,
    references: Vec<ModuleNameReference>,
}

impl<'a> ReferenceResolver<'a> {
    fn new(
        entry: &'a RegisteredModuleSource,
        registry: &ModuleNameRegistry,
        is_root: bool,
    ) -> Self {
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
        let mut scopes = Vec::with_capacity(usize::from(is_root) + 1);
        if is_root {
            scopes.push(BTreeMap::from([(
                "args".to_owned(),
                ModuleReferenceTarget::ScriptArguments,
            )]));
        }
        scopes.push(root);
        Self {
            entry,
            scopes,
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
        ModuleReferenceTarget::ScriptArguments => {
            unreachable!("the synthetic parent input cannot collide in a module scope")
        }
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
    types: ModuleTypeRegistry,
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

    /// Resolved annotations and named-function signatures.
    #[must_use]
    pub const fn types(&self) -> &ModuleTypeRegistry {
        &self.types
    }

    pub(crate) fn runtime_binding_types(&self) -> RuntimeBindingTypes {
        let by_source = self
            .sources
            .entries()
            .map(|entry| {
                let bindings = self
                    .types
                    .by_module
                    .get(entry.module())
                    .map_or_else(Vec::new, |types| types.bindings.clone());
                (entry.source().id(), bindings)
            })
            .collect();
        RuntimeBindingTypes { by_source }
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
        let types = match ModuleTypeRegistry::analyze(&sources, &names) {
            Ok(types) => types,
            Err(error) => {
                return Err(Box::new((ModuleProgramError::Signatures(error), sources)));
            }
        };
        Ok(ModuleProgram {
            graph,
            sources,
            names,
            types,
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
    ArgumentMismatch {
        module: ModuleId,
        name: String,
        parameter: String,
        argument_span: Span,
        expected: ValueType,
        actual: ValueType,
        parameter_span: Span,
    },
    ResultMismatch {
        module: ModuleId,
        name: String,
        result_span: Span,
        expected: ValueType,
        actual: ValueType,
        annotation_span: Span,
    },
}

impl ModuleTypeError {
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        match self {
            Self::UnknownType { module, .. }
            | Self::InvalidTypeArity { module, .. }
            | Self::CallArity { module, .. }
            | Self::ArgumentMismatch { module, .. }
            | Self::ResultMismatch { module, .. } => module,
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
