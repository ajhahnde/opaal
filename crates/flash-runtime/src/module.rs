//! Canonical module identity and import-graph analysis.
//!
//! This module is deliberately host-free. A caller injects path
//! canonicalization through [`ModuleCanonicalizer`], while the resolver records
//! the original request and source span alongside the canonical target. The
//! graph uses only canonical identities, rejects cycles before mutation, and
//! exposes structured diagnostics for checker, editor, and protocol clients.
//!
//! Source loading, import syntax, exported-name analysis, and execution are
//! separate layers built on this boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use flash_syntax::{Diagnostic, Severity, Span};

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
