//! Canonical module identity and import-graph analysis.
//!
//! This module is deliberately host-free. A caller injects path
//! canonicalization through [`ModuleCanonicalizer`], while the resolver records
//! the original request and source span alongside the canonical target. The
//! graph uses only canonical identities, rejects cycles before mutation, and
//! exposes structured diagnostics for checker, editor, and protocol clients.
//!
//! Static import syntax and injected recursive source loading build on this
//! graph. Exported-name analysis, frontend wiring, and execution remain
//! separate layers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use flash_syntax::{
    Diagnostic, ParseOutcome, Script, Severity, SourceFile, SourceId, Span, StatementKind, parse,
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
        let root = self
            .resolver
            .resolve_root(requested)
            .map_err(ModuleProgramError::Resolution)?;
        let mut graph = ModuleGraph::new(root.clone());
        let mut sources = ModuleSourceRegistry::default();
        self.load_module(root, None, &mut graph, &mut sources)?;
        Ok(ModuleProgram { graph, sources })
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
                    diagnostics: vec![diagnostic],
                });
            }
            ParseOutcome::Invalid(diagnostics) => {
                return Err(ModuleProgramError::Syntax {
                    module,
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
        diagnostics: Vec<Diagnostic>,
    },
    /// An import violated the canonical graph contract.
    Graph(ModuleGraphError),
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
