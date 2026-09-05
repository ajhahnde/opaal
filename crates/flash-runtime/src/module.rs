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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flash_syntax::{
    BinaryOperator, Block, Closure, CommandHeadKind, CommandItemKind, ConditionalChain,
    ControlTransfer, ControlledParseOutcome, ControlledVersionedParseOutcome, Diagnostic,
    ElseBranch, Expression, ExpressionKind, Identifier, LanguageMajor, LiteralKind, MatchArm,
    ParseOutcome, Pattern, Pipeline, RecordKey, RedirectionKind, Script, Severity, SourceFile,
    SourceId, Span, StageKind, Statement, StatementKind, TypeConstraint, TypeReference,
    UnaryOperator, VersionedParseOutcome, Word, WordPart, WordPartKind, parse_v2_with_control,
    parse_with_control, render_diagnostic_sources,
};

use crate::Value;
use crate::builtin::standard_registry;
use crate::carrier::{
    CarrierBridge, PipelineCarrierFault, StageCarrierContract, analyze_pipeline_carriers,
};
use crate::command::{
    Carrier, CommandArgumentFault, CommandArgumentFaultKind, CommandArgumentInput,
    CommandClassification, CommandLifecycle, CommandOutput, CommandRegistry, CommandSignature,
};
use crate::documentation::Documentation;
use crate::intrinsic::{DynamicBinding, ExpressionIntrinsic};
use crate::operation::{
    OperationDescriptor, OperationInputType, standard_operation, standard_operations,
};

/// Host-free cooperative cancellation for static source analysis.
#[derive(Clone)]
pub struct AnalysisControl {
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    cancelled: Arc<AtomicBool>,
    budget: Option<Arc<Mutex<AnalysisBudget>>>,
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
            cancelled: Arc::new(AtomicBool::new(false)),
            budget: None,
        }
    }

    /// Whether the current analysis should stop at its next polling boundary.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if (self.is_cancelled)() {
            self.cancelled.store(true, Ordering::Release);
            return true;
        }
        !self.charge(AnalysisLimitKind::WorkUnits, 1)
    }

    fn for_run(&self, limits: AnalysisLimits) -> Self {
        Self {
            is_cancelled: Arc::clone(&self.is_cancelled),
            cancelled: Arc::clone(&self.cancelled),
            budget: Some(Arc::new(Mutex::new(AnalysisBudget::new(limits)))),
        }
    }

    fn charge(&self, kind: AnalysisLimitKind, amount: u64) -> bool {
        self.budget.as_ref().is_none_or(|budget| {
            budget
                .lock()
                .expect("analysis budget state must not be poisoned")
                .charge(kind, amount)
        })
    }

    fn observe(&self, kind: AnalysisLimitKind, value: u64) -> bool {
        self.budget.as_ref().is_none_or(|budget| {
            budget
                .lock()
                .expect("analysis budget state must not be poisoned")
                .observe(kind, value)
        })
    }

    fn remaining(&self, kind: AnalysisLimitKind) -> Option<u64> {
        self.budget.as_ref().and_then(|budget| {
            budget
                .lock()
                .expect("analysis budget state must not be poisoned")
                .remaining(kind)
        })
    }

    fn limit_exceeded(&self) -> Option<AnalysisLimitExceeded> {
        self.budget.as_ref().and_then(|budget| {
            budget
                .lock()
                .expect("analysis budget state must not be poisoned")
                .exceeded
        })
    }

    fn usage(&self) -> AnalysisUsage {
        self.budget
            .as_ref()
            .map_or_else(AnalysisUsage::default, |budget| {
                budget
                    .lock()
                    .expect("analysis budget state must not be poisoned")
                    .usage()
            })
    }
}

/// One deterministic resource dimension charged by Flash 2 analysis.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AnalysisLimitKind {
    SourceBytes,
    Modules,
    ModuleDepth,
    AstNodes,
    TypeDepth,
    GenericInstantiations,
    OverloadCandidates,
    Diagnostics,
    WorkUnits,
}

impl AnalysisLimitKind {
    const COUNT: usize = 9;

    const fn index(self) -> usize {
        match self {
            Self::SourceBytes => 0,
            Self::Modules => 1,
            Self::ModuleDepth => 2,
            Self::AstNodes => 3,
            Self::TypeDepth => 4,
            Self::GenericInstantiations => 5,
            Self::OverloadCandidates => 6,
            Self::Diagnostics => 7,
            Self::WorkUnits => 8,
        }
    }

    /// Stable machine-readable name for this measurement unit.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SourceBytes => "source-bytes",
            Self::Modules => "modules",
            Self::ModuleDepth => "module-depth",
            Self::AstNodes => "ast-nodes",
            Self::TypeDepth => "type-depth",
            Self::GenericInstantiations => "generic-instantiations",
            Self::OverloadCandidates => "overload-candidates",
            Self::Diagnostics => "diagnostics",
            Self::WorkUnits => "work-units",
        }
    }
}

/// Exact configured ceilings for one Flash 2 root-closure analysis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalysisLimits {
    limits: [Option<u64>; AnalysisLimitKind::COUNT],
}

/// Exact deterministic counters consumed by one complete analysis.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnalysisUsage {
    used: [u64; AnalysisLimitKind::COUNT],
}

impl AnalysisUsage {
    /// The consumed count for one measurement dimension.
    #[must_use]
    pub const fn get(self, kind: AnalysisLimitKind) -> u64 {
        self.used[kind.index()]
    }
}

impl AnalysisLimits {
    /// Default Flash 2 ceilings. These are semantic counters, never clocks.
    pub const V2: Self = Self {
        limits: [
            Some(8 * 1024 * 1024),
            Some(256),
            Some(64),
            Some(1_000_000),
            Some(64),
            Some(100_000),
            Some(100_000),
            Some(1_024),
            Some(5_000_000),
        ],
    };

    /// No resource ceilings, retained only for the frozen v1 path and tests.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            limits: [None; AnalysisLimitKind::COUNT],
        }
    }

    /// Replaces one measurement ceiling while retaining every other setting.
    #[must_use]
    pub const fn with_limit(mut self, kind: AnalysisLimitKind, limit: u64) -> Self {
        self.limits[kind.index()] = Some(limit);
        self
    }

    /// The configured ceiling for one dimension, or `None` when unlimited.
    #[must_use]
    pub const fn limit(self, kind: AnalysisLimitKind) -> Option<u64> {
        self.limits[kind.index()]
    }
}

impl Default for AnalysisLimits {
    fn default() -> Self {
        Self::V2
    }
}

/// A deterministic analysis refusal at the first charge beyond one ceiling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalysisLimitExceeded {
    kind: AnalysisLimitKind,
    limit: u64,
}

impl AnalysisLimitExceeded {
    /// The exhausted measurement dimension.
    #[must_use]
    pub const fn kind(self) -> AnalysisLimitKind {
        self.kind
    }

    /// The exact inclusive ceiling that was exceeded.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

impl fmt::Display for AnalysisLimitExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Flash 2 analysis exceeded the {} limit of {}",
            self.kind.name(),
            self.limit
        )
    }
}

#[derive(Debug)]
struct AnalysisBudget {
    limits: AnalysisLimits,
    used: [u64; AnalysisLimitKind::COUNT],
    exceeded: Option<AnalysisLimitExceeded>,
}

impl AnalysisBudget {
    const fn new(limits: AnalysisLimits) -> Self {
        Self {
            limits,
            used: [0; AnalysisLimitKind::COUNT],
            exceeded: None,
        }
    }

    fn charge(&mut self, kind: AnalysisLimitKind, amount: u64) -> bool {
        if self.exceeded.is_some() {
            return false;
        }
        let index = kind.index();
        let Some(limit) = self.limits.limit(kind) else {
            self.used[index] = self.used[index].saturating_add(amount);
            return true;
        };
        let Some(next) = self.used[index].checked_add(amount) else {
            self.exceeded = Some(AnalysisLimitExceeded { kind, limit });
            return false;
        };
        if next > limit {
            self.exceeded = Some(AnalysisLimitExceeded { kind, limit });
            return false;
        }
        self.used[index] = next;
        true
    }

    fn observe(&mut self, kind: AnalysisLimitKind, value: u64) -> bool {
        if self.exceeded.is_some() {
            return false;
        }
        let Some(limit) = self.limits.limit(kind) else {
            self.used[kind.index()] = self.used[kind.index()].max(value);
            return true;
        };
        if value > limit {
            self.exceeded = Some(AnalysisLimitExceeded { kind, limit });
            return false;
        }
        self.used[kind.index()] = self.used[kind.index()].max(value);
        true
    }

    fn remaining(&self, kind: AnalysisLimitKind) -> Option<u64> {
        self.limits
            .limit(kind)
            .map(|limit| limit.saturating_sub(self.used[kind.index()]))
    }

    const fn usage(&self) -> AnalysisUsage {
        AnalysisUsage { used: self.used }
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

    /// Reads at most `maximum` bytes for a resource-bounded analysis.
    ///
    /// Host adapters should stop reading at this boundary. The default keeps
    /// compatibility with injected in-memory loaders while ensuring callers
    /// never retain more than the requested amount.
    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        let mut bytes = self.load(module)?;
        bytes.truncate(maximum);
        Ok(bytes)
    }
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

/// One canonical local or compiled-standard module identity and language major.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModuleId {
    path: PathBuf,
    language: LanguageMajor,
    origin: ModuleOrigin,
}

/// The closed module-origin set admitted by the Flash 2 foundation.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ModuleOrigin {
    /// A source module resolved by the injected native-path canonicalizer.
    Local,
    /// A compiled descriptor in the standard namespace.
    Standard { namespace: String, module: String },
}

impl ModuleId {
    /// The canonical native path for a local module, or `std::name` identity
    /// spelling for a compiled standard module.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The explicitly selected language major for this module identity.
    #[must_use]
    pub const fn language(&self) -> LanguageMajor {
        self.language
    }

    /// The canonical provenance class retained by this identity.
    #[must_use]
    pub const fn origin(&self) -> &ModuleOrigin {
        &self.origin
    }

    fn standard(namespace: &str, module: &str, language: LanguageMajor) -> Self {
        Self {
            path: PathBuf::from(format!("{namespace}::{module}")),
            language,
            origin: ModuleOrigin::Standard {
                namespace: namespace.to_owned(),
                module: module.to_owned(),
            },
        }
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
    language: LanguageMajor,
}

impl<'a> ModuleResolver<'a> {
    /// Creates a resolver over `canonicalizer`.
    #[must_use]
    pub const fn new(canonicalizer: &'a dyn ModuleCanonicalizer) -> Self {
        Self::for_language(canonicalizer, LanguageMajor::V1)
    }

    /// Creates a resolver whose canonical identities include `language`.
    #[must_use]
    pub const fn for_language(
        canonicalizer: &'a dyn ModuleCanonicalizer,
        language: LanguageMajor,
    ) -> Self {
        Self {
            canonicalizer,
            language,
        }
    }

    /// The language major retained by every identity this resolver creates.
    #[must_use]
    pub const fn language(&self) -> LanguageMajor {
        self.language
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

        Ok(ModuleId {
            path: canonical,
            language: self.language,
            origin: ModuleOrigin::Local,
        })
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

/// One required Flash 2 alias bound to a singular canonical module identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleAlias {
    name: String,
    importer: ModuleId,
    target: ModuleId,
    requested: Option<PathBuf>,
    declaration_span: Span,
}

impl ModuleAlias {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn importer(&self) -> &ModuleId {
        &self.importer
    }

    #[must_use]
    pub const fn target(&self) -> &ModuleId {
        &self.target
    }

    /// The exact local path request, absent for compiled standard modules.
    #[must_use]
    pub fn requested(&self) -> Option<&Path> {
        self.requested.as_deref()
    }

    #[must_use]
    pub const fn declaration_span(&self) -> Span {
        self.declaration_span
    }
}

/// One explicit alias re-export retaining its original target identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleAliasExport {
    name: String,
    target: ModuleId,
    declaration_span: Span,
    export_span: Span,
}

impl ModuleAliasExport {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn target(&self) -> &ModuleId {
        &self.target
    }

    #[must_use]
    pub const fn declaration_span(&self) -> Span {
        self.declaration_span
    }

    #[must_use]
    pub const fn export_span(&self) -> Span {
        self.export_span
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModuleAliases {
    aliases: BTreeMap<String, ModuleAlias>,
    exports: BTreeMap<String, ModuleAliasExport>,
}

/// Canonical alias and explicit re-export tables for a complete v2 program.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleAliasRegistry {
    by_module: BTreeMap<ModuleId, ModuleAliases>,
}

impl ModuleAliasRegistry {
    pub(crate) fn aliases<'a>(
        &'a self,
        module: &ModuleId,
    ) -> impl Iterator<Item = &'a ModuleAlias> {
        self.by_module
            .get(module)
            .into_iter()
            .flat_map(|aliases| aliases.aliases.values())
    }

    #[must_use]
    pub fn alias(&self, module: &ModuleId, name: &str) -> Option<&ModuleAlias> {
        self.by_module
            .get(module)
            .and_then(|aliases| aliases.aliases.get(name))
    }

    #[must_use]
    pub fn export(&self, module: &ModuleId, name: &str) -> Option<&ModuleAliasExport> {
        self.by_module
            .get(module)
            .and_then(|aliases| aliases.exports.get(name))
    }

    pub(crate) fn exports<'a>(
        &'a self,
        module: &ModuleId,
    ) -> impl Iterator<Item = &'a ModuleAliasExport> {
        self.by_module
            .get(module)
            .into_iter()
            .flat_map(|aliases| aliases.exports.values())
    }

    pub(crate) fn operation_spellings(&self) -> Vec<String> {
        let mut spellings = self
            .by_module
            .values()
            .flat_map(|aliases| aliases.aliases.values())
            .flat_map(|alias| {
                standard_operations(alias.target())
                    .into_iter()
                    .map(move |operation| format!("{}::{}", alias.name(), operation.id().name()))
            })
            .collect::<Vec<_>>();
        spellings.sort();
        spellings.dedup();
        spellings
    }

    pub(crate) fn retain_alias(&mut self, alias: ModuleAlias) {
        self.by_module
            .entry(alias.importer().clone())
            .or_default()
            .aliases
            .insert(alias.name().to_owned(), alias);
    }

    /// Resolves a local alias followed only by explicitly re-exported aliases.
    #[must_use]
    pub fn resolve(&self, module: &ModuleId, segments: &[&str]) -> Option<&ModuleId> {
        let (first, remainder) = segments.split_first()?;
        let mut target = self.alias(module, first)?.target();
        for segment in remainder {
            target = self.export(target, segment)?.target();
        }
        Some(target)
    }

    pub(crate) fn target_at(&self, module: &ModuleId, offset: usize) -> Option<&ModuleAlias> {
        let aliases = self.by_module.get(module)?;
        if let Some(alias) = aliases
            .aliases
            .values()
            .find(|alias| span_contains(alias.declaration_span(), offset))
        {
            return Some(alias);
        }
        aliases
            .exports
            .values()
            .find(|export| span_contains(export.export_span(), offset))
            .and_then(|export| aliases.aliases.get(export.name()))
    }

    fn analyze(
        graph: &ModuleGraph,
        sources: &ModuleSourceRegistry,
        control: &AnalysisControl,
    ) -> Result<Self, Vec<ModuleAliasError>> {
        let mut registry = Self::default();
        let mut errors = Vec::new();
        for entry in sources.entries() {
            registry
                .by_module
                .insert(entry.module().clone(), ModuleAliases::default());
        }

        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
            let mut occupied = BTreeMap::<String, Span>::new();
            for statement in entry.script().statements() {
                for identifier in top_level_declared_identifiers(statement) {
                    let name = entry
                        .source()
                        .slice(identifier.span())
                        .expect("parsed identifiers belong to their source")
                        .to_owned();
                    occupied.entry(name).or_insert(identifier.span());
                }
            }
            for statement in entry.script().statements() {
                let StatementKind::ModuleImport(import) = statement.kind() else {
                    continue;
                };
                let name = entry
                    .source()
                    .slice(import.alias.span())
                    .expect("parsed aliases belong to their source")
                    .to_owned();
                if let Some(first_span) = occupied.get(&name).copied() {
                    errors.push(ModuleAliasError::Conflict {
                        module: entry.module().clone(),
                        name,
                        first_span,
                        duplicate_span: import.alias.span(),
                    });
                    continue;
                }
                occupied.insert(name.clone(), import.alias.span());
                let edge = graph
                    .imports()
                    .iter()
                    .find(|edge| {
                        edge.importer() == entry.module() && edge.span() == import.source.span()
                    })
                    .expect("each validated module alias has one graph edge");
                let requested = match import.source {
                    flash_syntax::ModuleImportSource::Local { path } => {
                        let quoted = entry
                            .source()
                            .slice(path)
                            .expect("local import paths belong to their source");
                        Some(PathBuf::from(&quoted[1..quoted.len() - 1]))
                    }
                    flash_syntax::ModuleImportSource::Standard { .. } => None,
                };
                registry
                    .by_module
                    .get_mut(entry.module())
                    .expect("each source has an alias table")
                    .aliases
                    .insert(
                        name.clone(),
                        ModuleAlias {
                            name,
                            importer: entry.module().clone(),
                            target: edge.target().clone(),
                            requested,
                            declaration_span: import.alias.span(),
                        },
                    );
            }
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
                        .expect("parsed export names belong to their source")
                        .to_owned();
                    let aliases = registry
                        .by_module
                        .get_mut(entry.module())
                        .expect("each source has an alias table");
                    let Some(alias) = aliases.aliases.get(&name) else {
                        continue;
                    };
                    if let Some(first) = aliases.exports.get(&name) {
                        errors.push(ModuleAliasError::DuplicateExport {
                            module: entry.module().clone(),
                            name,
                            first_span: first.export_span(),
                            duplicate_span: identifier.span(),
                        });
                        continue;
                    }
                    aliases.exports.insert(
                        name.clone(),
                        ModuleAliasExport {
                            name,
                            target: alias.target().clone(),
                            declaration_span: alias.declaration_span(),
                            export_span: identifier.span(),
                        },
                    );
                }
            }
        }

        if errors.is_empty() {
            Ok(registry)
        } else {
            Err(errors)
        }
    }
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
        aliases: &ModuleAliasRegistry,
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
                for identifier in top_level_declared_identifiers(statement) {
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
                        if aliases.export(entry.module(), &name).is_some() {
                            continue;
                        }
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
    Error,
    Function,
    Closure,
    TypeParameter(String),
    Nominal {
        id: Box<NominalTypeId>,
        arguments: Vec<Self>,
    },
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
            (Self::Error, Value::Error(_)) => true,
            (Self::Function, Value::Callable(callable)) => callable.family() == "function",
            (Self::Closure, Value::Callable(callable)) => callable.family() == "closure",
            (Self::TypeParameter(_), _) => true,
            (Self::Nominal { id, arguments }, Value::NominalRecord(value)) => {
                nominal_runtime_type_matches(id, arguments, value.id(), value.type_arguments())
            }
            (Self::Nominal { id, arguments }, Value::Variant(value)) => {
                nominal_runtime_type_matches(id, arguments, value.id(), value.type_arguments())
            }
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

fn nominal_runtime_type_matches(
    id: &NominalTypeId,
    arguments: &[ValueType],
    runtime_id: &NominalTypeId,
    runtime_arguments: &[ValueType],
) -> bool {
    id == runtime_id && arguments == runtime_arguments
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
            Self::Error => formatter.write_str("Error"),
            Self::Function => formatter.write_str("Function"),
            Self::Closure => formatter.write_str("Closure"),
            Self::TypeParameter(name) => formatter.write_str(name),
            Self::Nominal { id, arguments } => {
                formatter.write_str(id.name())?;
                if !arguments.is_empty() {
                    formatter.write_str("[")?;
                    for (index, argument) in arguments.iter().enumerate() {
                        if index != 0 {
                            formatter.write_str(", ")?;
                        }
                        argument.fmt(formatter)?;
                    }
                    formatter.write_str("]")?;
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn substitute_type(
    value_type: &ValueType,
    substitutions: &BTreeMap<String, ValueType>,
) -> ValueType {
    match value_type {
        ValueType::TypeParameter(name) => substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| value_type.clone()),
        ValueType::List(element) => {
            ValueType::List(Box::new(substitute_type(element, substitutions)))
        }
        ValueType::Nominal { id, arguments } => ValueType::Nominal {
            id: id.clone(),
            arguments: arguments
                .iter()
                .map(|argument| substitute_type(argument, substitutions))
                .collect(),
        },
        _ => value_type.clone(),
    }
}

fn unify_type(
    expected: &ValueType,
    actual: &ValueType,
    substitutions: &mut BTreeMap<String, ValueType>,
) -> bool {
    match (expected, actual) {
        (ValueType::TypeParameter(_), ValueType::Any) => true,
        (ValueType::TypeParameter(name), actual) => match substitutions.get(name) {
            Some(previous) => previous == actual,
            None => {
                substitutions.insert(name.clone(), actual.clone());
                true
            }
        },
        (ValueType::List(expected), ValueType::List(actual)) => {
            unify_type(expected, actual, substitutions)
        }
        (
            ValueType::Nominal {
                id: expected_id,
                arguments: expected_arguments,
            },
            ValueType::Nominal {
                id: actual_id,
                arguments: actual_arguments,
            },
        ) if expected_id == actual_id && expected_arguments.len() == actual_arguments.len() => {
            expected_arguments
                .iter()
                .zip(actual_arguments)
                .all(|(expected, actual)| unify_type(expected, actual, substitutions))
        }
        (ValueType::Any, _) | (_, ValueType::Any) => true,
        _ => expected == actual,
    }
}

/// Singular nominal identity: defining canonical module plus declared name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NominalTypeId {
    module: ModuleId,
    name: String,
}

impl NominalTypeId {
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        &self.module
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One checked field in a nominal record declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalTypeField {
    name: String,
    value_type: ValueType,
    span: Span,
}

/// One resolved invariant parameter on a generic nominal or callable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTypeParameter {
    name: String,
    constraints: Vec<TypeConstraint>,
    span: Span,
}

impl ResolvedTypeParameter {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn constraints(&self) -> &[TypeConstraint] {
        &self.constraints
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// One checked constructor in a closed nominal variant type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalVariant {
    name: String,
    payload: Vec<ValueType>,
    span: Span,
}

impl NominalVariant {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn payload(&self) -> &[ValueType] {
        &self.payload
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NominalTypeKind {
    Record,
    Variant,
}

impl NominalTypeField {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn value_type(&self) -> &ValueType {
        &self.value_type
    }

    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Immutable declaration metadata shared by checker, help, and editor queries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NominalType {
    id: NominalTypeId,
    declaration_span: Span,
    kind: NominalTypeKind,
    type_parameters: Vec<ResolvedTypeParameter>,
    fields: Vec<NominalTypeField>,
    variants: Vec<NominalVariant>,
}

impl NominalType {
    #[must_use]
    pub const fn id(&self) -> &NominalTypeId {
        &self.id
    }

    #[must_use]
    pub const fn declaration_span(&self) -> Span {
        self.declaration_span
    }

    #[must_use]
    pub const fn kind(&self) -> NominalTypeKind {
        self.kind
    }

    #[must_use]
    pub fn type_parameters(&self) -> &[ResolvedTypeParameter] {
        &self.type_parameters
    }

    #[must_use]
    pub fn fields(&self) -> &[NominalTypeField] {
        &self.fields
    }

    #[must_use]
    pub fn variants(&self) -> &[NominalVariant] {
        &self.variants
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
    type_parameters: Vec<ResolvedTypeParameter>,
    parameters: Vec<FunctionParameterSignature>,
    result: ValueType,
    result_annotation_span: Option<Span>,
    documentation: Option<Documentation>,
    downstream: crate::seam::DownstreamCallMetadata,
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
    pub fn type_parameters(&self) -> &[ResolvedTypeParameter] {
        &self.type_parameters
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

    /// Opaque attachment points reserved for later action/project owners.
    #[must_use]
    pub const fn downstream(&self) -> &crate::seam::DownstreamCallMetadata {
        &self.downstream
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModuleTypes {
    annotations: Vec<ResolvedTypeAnnotation>,
    nominal_references: Vec<ResolvedNominalReference>,
    functions: Vec<FunctionSignature>,
    bindings: Vec<ResolvedBindingType>,
    nominals: BTreeMap<String, NominalType>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedNominalReference {
    span: Span,
    id: NominalTypeId,
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
    annotations_by_source: BTreeMap<SourceId, Vec<ResolvedTypeAnnotation>>,
    modules_by_source: BTreeMap<SourceId, ModuleId>,
    nominals_by_module: BTreeMap<ModuleId, BTreeMap<String, NominalType>>,
    aliases: ModuleAliasRegistry,
}

impl RuntimeBindingTypes {
    pub(crate) fn language(&self, source: SourceId) -> Option<LanguageMajor> {
        self.modules_by_source.get(&source).map(ModuleId::language)
    }
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

    pub(crate) fn qualified_nominal(
        &self,
        source: SourceId,
        segments: &[&str],
    ) -> Option<&NominalType> {
        let (name, modules) = segments.split_last()?;
        let current = self.modules_by_source.get(&source)?;
        let owner = if modules.is_empty() {
            current
        } else {
            self.aliases.resolve(current, modules)?
        };
        self.nominals_by_module.get(owner)?.get(*name)
    }

    pub(crate) fn qualified_variant<'name>(
        &self,
        source: SourceId,
        segments: &'name [&'name str],
    ) -> Option<(&NominalType, &'name str)> {
        let (constructor, nominal) = segments.split_last()?;
        Some((self.qualified_nominal(source, nominal)?, *constructor))
    }

    pub(crate) fn qualified_operation(
        &self,
        source: SourceId,
        segments: &[&str],
    ) -> Option<OperationDescriptor> {
        let (name, modules) = segments.split_last()?;
        if modules.is_empty() {
            return None;
        }
        let current = self.modules_by_source.get(&source)?;
        let owner = self.aliases.resolve(current, modules)?;
        standard_operation(owner, name)
    }

    pub(crate) fn module_alias(&self, source: SourceId, name: &str) -> Option<&ModuleAlias> {
        let current = self.modules_by_source.get(&source)?;
        self.aliases.alias(current, name)
    }

    pub(crate) fn annotation_type(&self, source: SourceId, span: Span) -> Option<&ValueType> {
        self.annotations_by_source
            .get(&source)?
            .iter()
            .find(|annotation| annotation.span() == span)
            .map(ResolvedTypeAnnotation::value_type)
    }

    pub(crate) fn type_satisfies_constraint(
        &self,
        value_type: &ValueType,
        constraint: TypeConstraint,
    ) -> bool {
        self.type_satisfies_constraint_inner(value_type, constraint, &mut BTreeSet::new())
    }

    fn type_satisfies_constraint_inner(
        &self,
        value_type: &ValueType,
        constraint: TypeConstraint,
        visiting: &mut BTreeSet<NominalTypeId>,
    ) -> bool {
        match constraint {
            TypeConstraint::Equal => match value_type {
                ValueType::Any
                | ValueType::Function
                | ValueType::Closure
                | ValueType::TypeParameter(_) => false,
                ValueType::List(element) => {
                    self.type_satisfies_constraint_inner(element, constraint, visiting)
                }
                ValueType::Nominal { id, arguments } => {
                    if !arguments.iter().all(|argument| {
                        self.type_satisfies_constraint_inner(argument, constraint, visiting)
                    }) {
                        return false;
                    }
                    if !visiting.insert(id.as_ref().clone()) {
                        return true;
                    }
                    let result = self
                        .nominals_by_module
                        .get(id.module())
                        .and_then(|nominals| nominals.get(id.name()))
                        .filter(|nominal| nominal.type_parameters().len() == arguments.len())
                        .is_some_and(|nominal| {
                            let substitutions = nominal
                                .type_parameters()
                                .iter()
                                .zip(arguments)
                                .map(|(parameter, argument)| {
                                    (parameter.name().to_owned(), argument.clone())
                                })
                                .collect::<BTreeMap<_, _>>();
                            nominal.fields().iter().all(|field| {
                                self.type_satisfies_constraint_inner(
                                    &substitute_type(field.value_type(), &substitutions),
                                    constraint,
                                    visiting,
                                )
                            }) && nominal.variants().iter().all(|variant| {
                                variant.payload().iter().all(|payload| {
                                    self.type_satisfies_constraint_inner(
                                        &substitute_type(payload, &substitutions),
                                        constraint,
                                        visiting,
                                    )
                                })
                            })
                        });
                    visiting.remove(id.as_ref());
                    result
                }
                _ => true,
            },
            TypeConstraint::Ordered => match value_type {
                ValueType::Int
                | ValueType::Float
                | ValueType::String
                | ValueType::Bytes
                | ValueType::Path
                | ValueType::Duration
                | ValueType::ByteSize => true,
                ValueType::List(element) => {
                    self.type_satisfies_constraint_inner(element, constraint, visiting)
                }
                _ => false,
            },
        }
    }

    pub(crate) fn analyze_source(
        source: &SourceFile,
        script: &Script,
    ) -> Result<Self, Box<ModuleTypeError>> {
        let entry = RegisteredModuleSource {
            module: ModuleId {
                path: PathBuf::from(source.name()),
                language: LanguageMajor::V1,
                origin: ModuleOrigin::Local,
            },
            source: source.clone(),
            script: script.clone(),
        };
        let control = AnalysisControl::never();
        let aliases = ModuleAliasRegistry::default();
        let names = ModuleNameRegistry::default();
        let mut declarations = ModuleTypeRegistry::default();
        declarations.by_module.insert(
            entry.module().clone(),
            TypeCollector::declarations(&entry, &control),
        );
        let (types, errors) =
            TypeCollector::new(&entry, &aliases, &names, &declarations, &control).collect();
        if let Some(error) = errors.into_iter().next() {
            return Err(Box::new(error));
        }
        Ok(Self {
            by_source: BTreeMap::from([(source.id(), types.bindings)]),
            functions_by_source: BTreeMap::from([(source.id(), types.functions)]),
            annotations_by_source: BTreeMap::from([(source.id(), types.annotations)]),
            modules_by_source: BTreeMap::from([(source.id(), entry.module().clone())]),
            nominals_by_module: declarations
                .by_module
                .into_iter()
                .map(|(module, types)| (module, types.nominals))
                .collect(),
            aliases,
        })
    }

    pub(crate) fn analyze_repl_source(
        source: &SourceFile,
        script: &Script,
        inherited_aliases: &ModuleAliasRegistry,
    ) -> Result<Self, Diagnostic> {
        let entry = RegisteredModuleSource {
            module: ModuleId {
                // Submission names remain diagnostic labels. Interactive state
                // needs one stable module identity so aliases retained by an
                // earlier cell resolve from later differently named cells.
                path: PathBuf::from("<interactive>"),
                language: LanguageMajor::V2,
                origin: ModuleOrigin::Local,
            },
            source: source.clone(),
            script: script.clone(),
        };
        let mut aliases = inherited_aliases.clone();
        aliases.by_module.entry(entry.module().clone()).or_default();

        let mut occupied = script
            .statements()
            .iter()
            .flat_map(top_level_declared_identifiers)
            .map(|identifier| {
                source
                    .slice(identifier.span())
                    .expect("parsed identifiers belong to their source")
                    .to_owned()
            })
            .collect::<BTreeSet<_>>();
        for alias in aliases.aliases(entry.module()) {
            occupied.insert(alias.name().to_owned());
        }

        for statement in script.statements() {
            let StatementKind::ModuleImport(import) = statement.kind() else {
                continue;
            };
            let alias_name = source
                .slice(import.alias.span())
                .expect("parsed aliases belong to their source")
                .to_owned();
            if occupied.contains(&alias_name) {
                return Err(Diagnostic::new(
                    Severity::Error,
                    "MOD011",
                    format!("module alias `{alias_name}` conflicts"),
                )
                .with_primary(import.alias.span(), "this alias conflicts"));
            }
            let flash_syntax::ModuleImportSource::Standard {
                namespace,
                module,
                span,
            } = import.source
            else {
                return Err(Diagnostic::new(
                    Severity::Error,
                    "MOD013",
                    "interactive local module loading is not available",
                )
                .with_primary(
                    import.source.span(),
                    "run the versioned module as a source file instead",
                ));
            };
            let namespace = source
                .slice(namespace.span())
                .expect("standard namespace belongs to its source");
            let standard = source
                .slice(module.span())
                .expect("standard module belongs to its source");
            if !is_standard_module(namespace, standard, LanguageMajor::V2) {
                return Err(ModuleAliasError::UnknownStandard {
                    module: entry.module().clone(),
                    name: standard.to_owned(),
                    span,
                }
                .diagnostic());
            }
            let target = ModuleId::standard(namespace, standard, LanguageMajor::V2);
            aliases
                .by_module
                .get_mut(entry.module())
                .expect("the interactive source owns an alias table")
                .aliases
                .insert(
                    alias_name.clone(),
                    ModuleAlias {
                        name: alias_name.clone(),
                        importer: entry.module().clone(),
                        target,
                        requested: None,
                        declaration_span: import.alias.span(),
                    },
                );
            occupied.insert(alias_name);
        }

        let control = AnalysisControl::never();
        let names = ModuleNameRegistry::default();
        let mut declarations = ModuleTypeRegistry::default();
        declarations.by_module.insert(
            entry.module().clone(),
            TypeCollector::declarations(&entry, &control),
        );
        let (types, errors) =
            TypeCollector::new(&entry, &aliases, &names, &declarations, &control).collect();
        if let Some(error) = errors.into_iter().next() {
            return Err(error.diagnostic());
        }
        Ok(Self {
            by_source: BTreeMap::from([(source.id(), types.bindings)]),
            functions_by_source: BTreeMap::from([(source.id(), types.functions)]),
            annotations_by_source: BTreeMap::from([(source.id(), types.annotations)]),
            modules_by_source: BTreeMap::from([(source.id(), entry.module().clone())]),
            nominals_by_module: declarations
                .by_module
                .into_iter()
                .map(|(module, types)| (module, types.nominals))
                .collect(),
            aliases,
        })
    }
}

/// Resolved annotations and named-function signatures by canonical module.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModuleTypeRegistry {
    by_module: BTreeMap<ModuleId, ModuleTypes>,
}

impl ModuleTypeRegistry {
    /// Looks up one nominal declaration by its canonical defining module/name ID.
    #[must_use]
    pub fn nominal(&self, module: &ModuleId, name: &str) -> Option<&NominalType> {
        self.by_module
            .get(module)
            .and_then(|types| types.nominals.get(name))
    }

    pub(crate) fn nominal_at(&self, module: &ModuleId, offset: usize) -> Option<&NominalType> {
        self.by_module
            .get(module)?
            .nominals
            .values()
            .find(|nominal| span_contains(nominal.declaration_span(), offset))
    }

    pub(crate) fn nominal_reference_at(
        &self,
        module: &ModuleId,
        offset: usize,
    ) -> Option<&NominalType> {
        let types = self.by_module.get(module)?;
        let id = types
            .annotations
            .iter()
            .find(|annotation| span_contains(annotation.span(), offset))
            .and_then(|annotation| nominal_type_id(annotation.value_type()))
            .or_else(|| {
                types
                    .nominal_references
                    .iter()
                    .find(|reference| span_contains(reference.span, offset))
                    .map(|reference| &reference.id)
            })?;
        self.nominal(id.module(), id.name())
    }

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
        aliases: &ModuleAliasRegistry,
        names: &ModuleNameRegistry,
        control: &AnalysisControl,
    ) -> Result<Self, Vec<ModuleTypeError>> {
        let mut declarations = Self::default();
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(declarations);
            }
            declarations.by_module.insert(
                entry.module().clone(),
                TypeCollector::declarations(entry, control),
            );
        }
        let mut registry = Self::default();
        let mut errors = Vec::new();
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
            let (types, mut source_errors) =
                TypeCollector::new(entry, aliases, names, &declarations, control).collect();
            errors.append(&mut source_errors);
            registry.by_module.insert(entry.module().clone(), types);
        }
        for entry in sources.entries() {
            if control.is_cancelled() {
                return Ok(registry);
            }
            errors.extend(
                SignatureValidator::new(entry, aliases, names, &registry, control).validate(),
            );
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

fn nominal_type_id(value_type: &ValueType) -> Option<&NominalTypeId> {
    match value_type {
        ValueType::Nominal { id, .. } => Some(id.as_ref()),
        ValueType::List(element) => nominal_type_id(element),
        _ => None,
    }
}

fn pattern_span(pattern: &Pattern) -> Span {
    match pattern {
        Pattern::Binding(identifier) => identifier.span(),
        Pattern::Wildcard(span) => *span,
        Pattern::Literal(literal) => literal.span(),
        Pattern::List(pattern) => pattern.span,
        Pattern::NominalRecord(pattern) => pattern.span,
        Pattern::Variant(pattern) => pattern.span,
    }
}

fn top_level_declared_identifiers(statement: &Statement) -> Vec<Identifier> {
    fn collect_pattern(pattern: &Pattern, identifiers: &mut Vec<Identifier>) {
        match pattern {
            Pattern::Binding(identifier) => identifiers.push(*identifier),
            Pattern::List(pattern) => {
                for element in &pattern.elements {
                    collect_pattern(element, identifiers);
                }
                if let Some(rest) = pattern.rest {
                    identifiers.push(rest);
                }
            }
            Pattern::NominalRecord(pattern) => {
                for field in &pattern.fields {
                    collect_pattern(&field.pattern, identifiers);
                }
            }
            Pattern::Variant(pattern) => {
                for payload in &pattern.payload {
                    collect_pattern(payload, identifiers);
                }
            }
            Pattern::Wildcard(_) | Pattern::Literal(_) => {}
        }
    }

    let mut identifiers = Vec::new();
    match statement.kind() {
        StatementKind::Declaration(declaration) => {
            collect_pattern(&declaration.pattern, &mut identifiers);
        }
        StatementKind::Function(function) => identifiers.push(function.name),
        StatementKind::NominalType(declaration) => identifiers.push(declaration.name),
        StatementKind::VariantType(declaration) => identifiers.push(declaration.name),
        _ => {}
    }
    identifiers
}

struct TypeCollector<'a> {
    entry: &'a RegisteredModuleSource,
    aliases: &'a ModuleAliasRegistry,
    names: &'a ModuleNameRegistry,
    declarations: &'a ModuleTypeRegistry,
    types: ModuleTypes,
    type_parameter_scopes: Vec<BTreeMap<String, ResolvedTypeParameter>>,
    errors: Vec<ModuleTypeError>,
    control: &'a AnalysisControl,
}

impl<'a> TypeCollector<'a> {
    fn new(
        entry: &'a RegisteredModuleSource,
        aliases: &'a ModuleAliasRegistry,
        names: &'a ModuleNameRegistry,
        declarations: &'a ModuleTypeRegistry,
        control: &'a AnalysisControl,
    ) -> Self {
        Self {
            entry,
            aliases,
            names,
            declarations,
            types: declarations
                .by_module
                .get(entry.module())
                .cloned()
                .unwrap_or_default(),
            type_parameter_scopes: Vec::new(),
            errors: Vec::new(),
            control,
        }
    }

    fn collect(mut self) -> (ModuleTypes, Vec<ModuleTypeError>) {
        self.resolve_nominal_schemas(self.entry.script().statements());
        self.statements(self.entry.script().statements())
            .expect("accumulating type collection does not fail fast");
        (self.types, self.errors)
    }

    fn declarations(entry: &RegisteredModuleSource, control: &AnalysisControl) -> ModuleTypes {
        let mut types = ModuleTypes::default();
        for statement in entry.script().statements() {
            if control.is_cancelled() {
                break;
            }
            let (name, parameters, kind) = match statement.kind() {
                StatementKind::NominalType(declaration) => (
                    declaration.name,
                    &declaration.type_parameters,
                    NominalTypeKind::Record,
                ),
                StatementKind::VariantType(declaration) => (
                    declaration.name,
                    &declaration.type_parameters,
                    NominalTypeKind::Variant,
                ),
                _ => continue,
            };
            let name_text = entry
                .source()
                .slice(name.span())
                .expect("parsed nominal names belong to their source")
                .to_owned();
            let type_parameters = parameters
                .iter()
                .map(|parameter| ResolvedTypeParameter {
                    name: entry
                        .source()
                        .slice(parameter.name.span())
                        .expect("parsed type parameters belong to their source")
                        .to_owned(),
                    constraints: parameter.constraints.clone(),
                    span: parameter.span,
                })
                .collect();
            types
                .nominals
                .entry(name_text.clone())
                .or_insert_with(|| NominalType {
                    id: NominalTypeId {
                        module: entry.module().clone(),
                        name: name_text,
                    },
                    declaration_span: name.span(),
                    kind,
                    type_parameters,
                    fields: Vec::new(),
                    variants: Vec::new(),
                });
        }
        types
    }

    fn resolve_nominal_schemas(&mut self, statements: &[Statement]) {
        for statement in statements {
            match statement.kind() {
                StatementKind::NominalType(declaration) => {
                    let parameters = self.resolved_type_parameters(&declaration.type_parameters);
                    self.push_type_parameters(&parameters);
                    let fields = declaration
                        .fields
                        .iter()
                        .map(|field| NominalTypeField {
                            name: self.text(field.name.span()).to_owned(),
                            value_type: self.resolve_type(&field.value_type),
                            span: field.span,
                        })
                        .collect();
                    self.type_parameter_scopes.pop();
                    let name = self.text(declaration.name.span()).to_owned();
                    if let Some(nominal) = self.types.nominals.get_mut(&name) {
                        nominal.fields = fields;
                    }
                }
                StatementKind::VariantType(declaration) => {
                    let parameters = self.resolved_type_parameters(&declaration.type_parameters);
                    self.push_type_parameters(&parameters);
                    let variants = declaration
                        .variants
                        .iter()
                        .map(|variant| NominalVariant {
                            name: self.text(variant.name.span()).to_owned(),
                            payload: variant
                                .payload
                                .iter()
                                .map(|payload| self.resolve_type(payload))
                                .collect(),
                            span: variant.span,
                        })
                        .collect();
                    self.type_parameter_scopes.pop();
                    let name = self.text(declaration.name.span()).to_owned();
                    if let Some(nominal) = self.types.nominals.get_mut(&name) {
                        nominal.variants = variants;
                    }
                }
                _ => {}
            }
        }
    }

    fn resolved_type_parameters(
        &self,
        parameters: &[flash_syntax::TypeParameter],
    ) -> Vec<ResolvedTypeParameter> {
        parameters
            .iter()
            .map(|parameter| ResolvedTypeParameter {
                name: self.text(parameter.name.span()).to_owned(),
                constraints: parameter.constraints.clone(),
                span: parameter.span,
            })
            .collect()
    }

    fn push_type_parameters(&mut self, parameters: &[ResolvedTypeParameter]) {
        self.type_parameter_scopes.push(
            parameters
                .iter()
                .map(|parameter| (parameter.name.clone(), parameter.clone()))
                .collect(),
        );
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
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_) => Ok(()),
            StatementKind::NominalType(_) | StatementKind::VariantType(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                self.collect_nominal_pattern_references(&declaration.pattern);
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
                    self.collect_pattern_bindings(&declaration.pattern, &value_type);
                }
                self.expression(&declaration.value)
            }
            StatementKind::Assignment(assignment) => self.expression(&assignment.value),
            StatementKind::Environment(environment) => match environment {
                flash_syntax::EnvironmentStatement::Export { value, .. } => self.expression(value),
                flash_syntax::EnvironmentStatement::Unset { .. } => Ok(()),
            },
            StatementKind::Function(function) => {
                let type_parameters = self.resolved_type_parameters(&function.type_parameters);
                self.push_type_parameters(&type_parameters);
                let mut parameters = Vec::with_capacity(function.parameters.len());
                for parameter in &function.parameters {
                    self.collect_nominal_pattern_references(&parameter.pattern);
                    let value_type = match &parameter.type_annotation {
                        Some(annotation) => {
                            let value_type = self.resolve_type(annotation);
                            self.collect_pattern_bindings(&parameter.pattern, &value_type);
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
                    type_parameters,
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
                    downstream: crate::seam::DownstreamCallMetadata::foundation(),
                });
                self.type_parameter_scopes.pop();
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
                    self.collect_nominal_pattern_references(&arm.pattern);
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
            StatementKind::Try(statement) => {
                self.statements(&statement.try_block.statements)?;
                self.types.bindings.push(ResolvedBindingType {
                    declaration_span: statement.catch_binding.span(),
                    value_type: ValueType::Error,
                });
                self.statements(&statement.catch_block.statements)
            }
            StatementKind::Throw(expression) => self.expression(expression),
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
            ExpressionKind::Qualified(name) => {
                self.record_nominal_reference(name, true);
                Ok(())
            }
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
            ExpressionKind::NominalRecord(record) => {
                self.record_nominal_reference(&record.name, false);
                for field in &record.fields {
                    self.expression(&field.value)?;
                }
                Ok(())
            }
            ExpressionKind::Closure(closure) => self.closure(closure),
            ExpressionKind::CommandSubstitution(substitution) => self.chain(substitution.chain()),
            ExpressionKind::GroupedJob(chain) => self.chain(chain),
            ExpressionKind::Call(call) => {
                self.expression(&call.callee)?;
                for type_argument in &call.type_arguments {
                    self.resolve_type(type_argument);
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

    fn closure(&mut self, closure: &Closure) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for parameter in &closure.parameters {
            self.collect_nominal_pattern_references(&parameter.pattern);
            if let Some(annotation) = &parameter.type_annotation {
                let value_type = self.resolve_type(annotation);
                self.collect_pattern_bindings(&parameter.pattern, &value_type);
            }
        }
        if let Some(result_type) = &closure.result_type {
            self.resolve_type(result_type);
        }
        self.chain(&closure.body)
    }

    fn collect_pattern_bindings(&mut self, pattern: &Pattern, value_type: &ValueType) {
        match pattern {
            Pattern::Binding(identifier) => self.types.bindings.push(ResolvedBindingType {
                declaration_span: identifier.span(),
                value_type: value_type.clone(),
            }),
            Pattern::Wildcard(_) | Pattern::Literal(_) => {}
            Pattern::List(pattern) => {
                let element_type = match value_type {
                    ValueType::List(element) => element.as_ref().clone(),
                    _ => ValueType::Any,
                };
                for element in &pattern.elements {
                    self.collect_pattern_bindings(element, &element_type);
                }
                if let Some(rest) = pattern.rest {
                    self.types.bindings.push(ResolvedBindingType {
                        declaration_span: rest.span(),
                        value_type: ValueType::List(Box::new(element_type)),
                    });
                }
            }
            Pattern::NominalRecord(pattern) => {
                self.record_nominal_reference(&pattern.name, false);
                let Some((nominal, substitutions)) =
                    self.nominal_for_pattern(&pattern.name, value_type)
                else {
                    for field in &pattern.fields {
                        self.collect_pattern_bindings(&field.pattern, &ValueType::Any);
                    }
                    return;
                };
                let field_types = nominal
                    .fields
                    .iter()
                    .map(|field| {
                        (
                            field.name.clone(),
                            substitute_type(&field.value_type, &substitutions),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                for field in &pattern.fields {
                    let name = self.text(field.name.span());
                    self.collect_pattern_bindings(
                        &field.pattern,
                        field_types.get(name).unwrap_or(&ValueType::Any),
                    );
                }
            }
            Pattern::Variant(pattern) => {
                self.record_nominal_reference(&pattern.constructor, true);
                let Some((nominal, substitutions)) =
                    self.nominal_for_pattern(&pattern.constructor, value_type)
                else {
                    for payload in &pattern.payload {
                        self.collect_pattern_bindings(payload, &ValueType::Any);
                    }
                    return;
                };
                let constructor = pattern
                    .constructor
                    .segments
                    .last()
                    .map(|segment| self.text(segment.span()));
                let payload_types = constructor
                    .and_then(|name| nominal.variants.iter().find(|variant| variant.name == name))
                    .map_or(&[][..], |variant| variant.payload.as_slice());
                let resolved = payload_types
                    .iter()
                    .map(|value_type| substitute_type(value_type, &substitutions))
                    .collect::<Vec<_>>();
                for (index, payload) in pattern.payload.iter().enumerate() {
                    self.collect_pattern_bindings(
                        payload,
                        resolved.get(index).unwrap_or(&ValueType::Any),
                    );
                }
            }
        }
    }

    fn collect_nominal_pattern_references(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::List(pattern) => {
                for element in &pattern.elements {
                    self.collect_nominal_pattern_references(element);
                }
            }
            Pattern::NominalRecord(pattern) => {
                self.record_nominal_reference(&pattern.name, false);
                for field in &pattern.fields {
                    self.collect_nominal_pattern_references(&field.pattern);
                }
            }
            Pattern::Variant(pattern) => {
                self.record_nominal_reference(&pattern.constructor, true);
                for payload in &pattern.payload {
                    self.collect_nominal_pattern_references(payload);
                }
            }
            Pattern::Binding(_) | Pattern::Wildcard(_) | Pattern::Literal(_) => {}
        }
    }

    fn record_nominal_reference(
        &mut self,
        name: &flash_syntax::QualifiedName,
        constructor_member: bool,
    ) {
        let type_index = if constructor_member {
            let Some(index) = name.segments.len().checked_sub(2) else {
                return;
            };
            index
        } else {
            let Some(index) = name.segments.len().checked_sub(1) else {
                return;
            };
            index
        };
        let type_name = self.text(name.segments[type_index].span());
        let owner = if type_index == 0 {
            self.entry.module()
        } else {
            let modules = name.segments[..type_index]
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            let Some(owner) = self.aliases.resolve(self.entry.module(), &modules) else {
                return;
            };
            owner
        };
        if type_index != 0 && self.names.export(owner, type_name).is_none() {
            return;
        }
        let Some(nominal) = self.declarations.nominal(owner, type_name) else {
            return;
        };
        if constructor_member && nominal.kind() != NominalTypeKind::Variant {
            return;
        }
        let reference = ResolvedNominalReference {
            span: name.span,
            id: nominal.id().clone(),
        };
        if !self.types.nominal_references.contains(&reference) {
            self.types.nominal_references.push(reference);
        }
    }

    fn nominal_for_pattern(
        &self,
        name: &flash_syntax::QualifiedName,
        value_type: &ValueType,
    ) -> Option<(NominalType, BTreeMap<String, ValueType>)> {
        let ValueType::Nominal { id, arguments } = value_type else {
            return None;
        };
        let type_name_index = if name.segments.len() > 1 {
            name.segments.len() - 2
        } else {
            0
        };
        let type_name = self.text(name.segments[type_name_index].span());
        let owner = if type_name_index == 0 {
            self.entry.module()
        } else {
            let modules = name.segments[..type_name_index]
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            self.aliases.resolve(self.entry.module(), &modules)?
        };
        if type_name_index != 0 && self.names.export(owner, type_name).is_none() {
            return None;
        }
        let nominal = self.declarations.nominal(owner, type_name)?.clone();
        if nominal.id() != id.as_ref() {
            return None;
        }
        let substitutions = nominal
            .type_parameters
            .iter()
            .zip(arguments)
            .map(|(parameter, argument)| (parameter.name.clone(), argument.clone()))
            .collect();
        Some((nominal, substitutions))
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
        let qualified = self
            .text(reference.span)
            .split('[')
            .next()
            .expect("a type reference begins with its name");
        let segments = qualified.split("::").map(str::trim).collect::<Vec<_>>();
        let name = segments
            .last()
            .copied()
            .expect("a type reference has one name segment");
        if segments.len() == 1
            && let Some(parameter) = self
                .type_parameter_scopes
                .iter()
                .rev()
                .find_map(|scope| scope.get(name))
        {
            if !reference.arguments.is_empty() {
                return Err(Box::new(ModuleTypeError::InvalidTypeArity {
                    module: self.entry.module().clone(),
                    name: name.to_owned(),
                    expected: 0,
                    actual: reference.arguments.len(),
                    span: reference.span,
                }));
            }
            return Ok(ValueType::TypeParameter(parameter.name.clone()));
        }
        let nominal_owner = if segments.len() == 1 {
            Some(self.entry.module())
        } else {
            self.aliases.resolve(
                self.entry.module(),
                &segments[..segments.len().saturating_sub(1)],
            )
        };
        let nominal = nominal_owner.and_then(|owner| {
            (segments.len() == 1 || self.names.export(owner, name).is_some())
                .then(|| self.declarations.nominal(owner, name))
                .flatten()
        });
        let value_type = if segments.len() == 1 && name == "List" {
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
        } else if let Some(nominal) = nominal {
            let expected = nominal.type_parameters().len();
            let id = nominal.id().clone();
            if reference.arguments.len() != expected {
                return Err(Box::new(ModuleTypeError::InvalidTypeArity {
                    module: self.entry.module().clone(),
                    name: name.to_owned(),
                    expected,
                    actual: reference.arguments.len(),
                    span: reference.span,
                }));
            }
            ValueType::Nominal {
                id: Box::new(id),
                arguments: reference
                    .arguments
                    .iter()
                    .map(|argument| self.resolve_type_value(argument))
                    .collect::<Result<_, _>>()?,
            }
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
            match (segments.len() == 1).then_some(name) {
                Some("Any") => ValueType::Any,
                Some("Null") => ValueType::Null,
                Some("Bool") => ValueType::Bool,
                Some("Int") => ValueType::Int,
                Some("Float") => ValueType::Float,
                Some("String") => ValueType::String,
                Some("Bytes") => ValueType::Bytes,
                Some("Path") => ValueType::Path,
                Some("Duration") => ValueType::Duration,
                Some("ByteSize") => ValueType::ByteSize,
                Some("Record") => ValueType::Record,
                Some("Table") => ValueType::Table,
                Some("Range") => ValueType::Range,
                Some("Status") => ValueType::Status,
                Some("Error") => ValueType::Error,
                Some("Function") => ValueType::Function,
                Some("Closure") => ValueType::Closure,
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
    aliases: &'a ModuleAliasRegistry,
    names: &'a ModuleNameRegistry,
    types: &'a ModuleTypeRegistry,
    inferred_bindings: RefCell<BTreeMap<(ModuleId, usize), ValueType>>,
    type_parameter_scopes: RefCell<Vec<BTreeMap<String, Vec<TypeConstraint>>>>,
    errors: RefCell<Vec<ModuleTypeError>>,
    control: &'a AnalysisControl,
}

impl<'a> SignatureValidator<'a> {
    fn new(
        entry: &'a RegisteredModuleSource,
        aliases: &'a ModuleAliasRegistry,
        names: &'a ModuleNameRegistry,
        types: &'a ModuleTypeRegistry,
        control: &'a AnalysisControl,
    ) -> Self {
        Self {
            entry,
            aliases,
            names,
            types,
            inferred_bindings: RefCell::new(BTreeMap::new()),
            type_parameter_scopes: RefCell::new(Vec::new()),
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
        self.type_parameter_scopes.borrow_mut().push(
            signature
                .type_parameters()
                .iter()
                .map(|parameter| {
                    (
                        parameter.name().to_owned(),
                        parameter.constraints().to_vec(),
                    )
                })
                .collect(),
        );
        let result = (|| {
            if self.entry.module().language() == LanguageMajor::V2 {
                for (parameter, resolved) in function.parameters.iter().zip(signature.parameters())
                {
                    self.validate_pattern(&parameter.pattern, resolved.value_type());
                }
            }
            self.function_statements(&function.body.statements, signature)?;

            let Some(StatementKind::Job(job)) =
                function.body.statements.last().map(Statement::kind)
            else {
                return Ok(());
            };
            let Some((span, actual)) =
                self.chain_value_with_expected(&job.chain, Some(signature.result()))?
            else {
                return Ok(());
            };
            self.check_result(signature, span, Some(actual))
        })();
        self.type_parameter_scopes
            .borrow_mut()
            .pop()
            .expect("function validation pushes one type-parameter scope");
        result
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
                    Some(expression) => (
                        expression.span(),
                        self.expression_with_expected(expression, Some(signature.result()))?,
                    ),
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
                let subject_type = self.expression(&statement.value)?;
                if self.entry.module().language() == LanguageMajor::V2 {
                    self.validate_match(statement, subject_type.as_ref())?;
                }
                for arm in &statement.arms {
                    if self.entry.module().language() == LanguageMajor::V2
                        && let Some(subject_type) = &subject_type
                    {
                        self.infer_pattern_bindings(&arm.pattern, subject_type);
                    }
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal)?;
                    }
                    if let Some(guard) = &arm.guard {
                        if self.entry.module().language() == LanguageMajor::V2 {
                            self.validate_guard(guard)?;
                        } else {
                            self.expression(guard)?;
                        }
                    }
                    self.function_statements(&arm.body.statements, signature)?;
                }
                Ok(())
            }
            StatementKind::Try(statement) => {
                self.function_statements(&statement.try_block.statements, signature)?;
                self.function_statements(&statement.catch_block.statements, signature)
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
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_)
            | StatementKind::NominalType(_)
            | StatementKind::VariantType(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                let expected = declaration.type_annotation.as_ref().and_then(|annotation| {
                    self.types
                        .annotation(self.entry.module(), annotation.span)
                        .map(ResolvedTypeAnnotation::value_type)
                });
                let actual = self.expression_with_expected(&declaration.value, expected)?;
                if self.entry.module().language() == LanguageMajor::V2
                    && let Some(actual) = actual
                {
                    if actual != ValueType::Any
                        && let Some(expected) = expected
                        && !expected.accepts_type(&actual)
                    {
                        self.errors
                            .borrow_mut()
                            .push(ModuleTypeError::BindingMismatch {
                                module: self.entry.module().clone(),
                                name: self.text(declaration.name.span()).to_owned(),
                                value_span: declaration.value.span(),
                                expected: expected.clone(),
                                actual: actual.clone(),
                                annotation_span: declaration
                                    .type_annotation
                                    .as_ref()
                                    .expect("an expected declaration type has an annotation")
                                    .span,
                            });
                    }
                    self.validate_pattern(&declaration.pattern, &actual);
                    self.infer_pattern_bindings(&declaration.pattern, &actual);
                }
                Ok(())
            }
            StatementKind::Assignment(assignment) => self.assignment(assignment),
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
                let subject_type = self.expression(&statement.value)?;
                if self.entry.module().language() == LanguageMajor::V2 {
                    self.validate_match(statement, subject_type.as_ref())?;
                }
                for arm in &statement.arms {
                    if self.entry.module().language() == LanguageMajor::V2
                        && let Some(subject_type) = &subject_type
                    {
                        self.infer_pattern_bindings(&arm.pattern, subject_type);
                    }
                    if let Pattern::Literal(literal) = &arm.pattern {
                        self.literal(literal)?;
                    }
                    if let Some(guard) = &arm.guard {
                        if self.entry.module().language() == LanguageMajor::V2 {
                            self.validate_guard(guard)?;
                        } else {
                            self.expression(guard)?;
                        }
                    }
                    self.statements(&arm.body.statements)?;
                }
                Ok(())
            }
            StatementKind::Try(statement) => {
                self.statements(&statement.try_block.statements)?;
                self.statements(&statement.catch_block.statements)
            }
            StatementKind::Throw(expression) => {
                let actual = self.expression(expression)?;
                if let Some(actual) = actual
                    && !matches!(
                        actual,
                        ValueType::Any | ValueType::String | ValueType::Error
                    )
                {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::ThrowMismatch {
                            module: self.entry.module().clone(),
                            span: expression.span(),
                            actual,
                        });
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

    fn assignment(
        &self,
        assignment: &flash_syntax::Assignment,
    ) -> Result<(), Box<ModuleTypeError>> {
        let actual = self.expression(&assignment.value)?;
        let Some(actual) = actual else {
            return Ok(());
        };
        if actual == ValueType::Any {
            return Ok(());
        }
        let reference = self
            .names
            .reference(self.entry.module(), assignment.target.span)
            .expect("clean name analysis resolves every assignment target");
        let (target_module, declaration_span) = match reference.target() {
            ModuleReferenceTarget::Local {
                module,
                declaration_span,
            } => (module, *declaration_span),
            ModuleReferenceTarget::Imported {
                target_module,
                declaration_span,
                ..
            } => (target_module, *declaration_span),
            ModuleReferenceTarget::DynamicStatus | ModuleReferenceTarget::ScriptArguments => {
                return Ok(());
            }
        };
        let Some(expected) = self.types.binding_type(target_module, declaration_span) else {
            return Ok(());
        };
        if expected.accepts_type(&actual) {
            return Ok(());
        }
        self.errors
            .borrow_mut()
            .push(ModuleTypeError::AssignmentMismatch {
                module: self.entry.module().clone(),
                name: reference.name().to_owned(),
                assignment_span: assignment.value.span(),
                expected: expected.clone(),
                actual,
                declaration_span,
            });
        Ok(())
    }

    fn chain(&self, chain: &ConditionalChain) -> Result<(), Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(());
        }
        for and_chain in chain.or_terms() {
            for pipeline in and_chain.and_terms() {
                if pipeline.stages().len() > 1
                    && self.pipeline_value_with_expected(pipeline, None)?.is_some()
                {
                    continue;
                }
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
                                    CommandItemKind::Spread(_) => {
                                        self.validate_spread(item.span());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_spread(&self, span: Span) {
        if self.entry.module().language() != LanguageMajor::V2 {
            return;
        }
        let Some(reference) = self.names.reference(self.entry.module(), span) else {
            return;
        };
        let Some(actual) = self.reference_type(reference.target()) else {
            return;
        };
        match actual {
            ValueType::Any | ValueType::TypeParameter(_) => {}
            ValueType::List(element) => {
                if !matches!(
                    element.as_ref(),
                    ValueType::Any
                        | ValueType::Bool
                        | ValueType::Int
                        | ValueType::Float
                        | ValueType::String
                        | ValueType::Path
                        | ValueType::Duration
                        | ValueType::ByteSize
                        | ValueType::TypeParameter(_)
                ) {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::SpreadElementMismatch {
                            module: self.entry.module().clone(),
                            span,
                            actual: *element,
                        });
                }
            }
            actual => {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::SpreadValueMismatch {
                        module: self.entry.module().clone(),
                        span,
                        actual,
                    });
            }
        }
    }

    fn chain_value_with_expected(
        &self,
        chain: &ConditionalChain,
        expected: Option<&ValueType>,
    ) -> Result<Option<(Span, ValueType)>, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(None);
        }
        if chain.or_terms().len() != 1 || chain.or_terms()[0].and_terms().len() != 1 {
            return Ok(None);
        }
        self.pipeline_value_with_expected(&chain.or_terms()[0].and_terms()[0], expected)
    }

    fn pipeline_value_with_expected(
        &self,
        pipeline: &Pipeline,
        expected: Option<&ValueType>,
    ) -> Result<Option<(Span, ValueType)>, Box<ModuleTypeError>> {
        if pipeline.stages().len() > 1 && self.entry.module().language() != LanguageMajor::V2 {
            return Ok(None);
        }
        let Some(first) = pipeline.stages().first() else {
            return Ok(None);
        };
        let StageKind::Expression(expression) = first.kind() else {
            return Ok(None);
        };
        let first_expected = (pipeline.stages().len() == 1).then_some(expected).flatten();
        let mut value_type = self.expression_with_expected(expression, first_expected)?;
        let mut result_span = expression.span();
        for stage in &pipeline.stages()[1..] {
            let StageKind::Expression(stage_expression) = stage.kind() else {
                return Ok(None);
            };
            let ExpressionKind::Qualified(name) = stage_expression.kind() else {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::InvalidOperationStage {
                        module: self.entry.module().clone(),
                        stage_span: stage.span(),
                    });
                return Ok(Some((stage.span(), ValueType::Any)));
            };
            let Some(operation) = self.operation_for_qualified(name) else {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::InvalidOperationStage {
                        module: self.entry.module().clone(),
                        stage_span: stage.span(),
                    });
                return Ok(Some((stage.span(), ValueType::Any)));
            };
            if !self.control.charge(
                AnalysisLimitKind::OverloadCandidates,
                operation.overloads().len() as u64,
            ) || (!operation.type_parameters().is_empty()
                && !self
                    .control
                    .charge(AnalysisLimitKind::GenericInstantiations, 1))
            {
                return Ok(None);
            }
            let (input, result) = operation
                .overloads()
                .iter()
                .find_map(|overload| match overload.input() {
                    OperationInputType::Value(input) => Some((input, overload.result())),
                    OperationInputType::ValueStream(_) => None,
                })
                .expect("a pipeline-eligible standard operation has a value overload");
            let mut substitutions = BTreeMap::new();
            if let Some(actual) = value_type.as_ref()
                && !unify_type(input, actual, &mut substitutions)
            {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::OperationArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: operation.id().qualified_name(),
                        argument_span: stage.span(),
                        expected: input.clone(),
                        actual: actual.clone(),
                    });
                return Ok(Some((stage.span(), ValueType::Any)));
            }
            for parameter in operation.type_parameters() {
                if !substitutions.contains_key(parameter) {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::AmbiguousOperationGeneric {
                            module: self.entry.module().clone(),
                            name: operation.id().qualified_name(),
                            parameter: parameter.clone(),
                            call_span: stage.span(),
                        });
                    return Ok(Some((stage.span(), ValueType::Any)));
                }
            }
            let resolved_input = substitute_type(input, &substitutions);
            if let Some(actual) = value_type
                && actual != ValueType::Any
                && !resolved_input.accepts_type(&actual)
            {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::OperationArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: operation.id().qualified_name(),
                        argument_span: stage.span(),
                        expected: resolved_input,
                        actual,
                    });
                return Ok(Some((stage.span(), ValueType::Any)));
            }
            value_type = Some(substitute_type(result, &substitutions));
            result_span = stage.span();
        }
        Ok(value_type.map(|value_type| (result_span, value_type)))
    }

    fn expression(
        &self,
        expression: &Expression,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        self.expression_with_expected(expression, None)
    }

    fn expression_with_expected(
        &self,
        expression: &Expression,
        expected: Option<&ValueType>,
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
            ExpressionKind::Qualified(name) => Ok(self.qualified_value_type(name, expected)),
            ExpressionKind::List(elements) => {
                let mut element_type = None;
                let expected_element = match expected {
                    Some(ValueType::List(element)) => Some(element.as_ref()),
                    _ => None,
                };
                for element in elements {
                    let Some(current) = self.expression_with_expected(element, expected_element)?
                    else {
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
            ExpressionKind::NominalRecord(record) => self.nominal_record_type(record, expected),
            ExpressionKind::Closure(closure) => {
                let expected = closure.result_type.as_ref().and_then(|annotation| {
                    self.types
                        .annotation(self.entry.module(), annotation.span)
                        .map(ResolvedTypeAnnotation::value_type)
                });
                let actual = self.chain_value_with_expected(&closure.body, expected)?;
                if actual.is_none() {
                    self.chain(&closure.body)?;
                }
                if self.entry.module().language() == LanguageMajor::V2
                    && let Some(annotation) = &closure.result_type
                    && let Some((result_span, actual)) = actual
                    && actual != ValueType::Any
                {
                    let expected = self
                        .types
                        .annotation(self.entry.module(), annotation.span)
                        .map_or(ValueType::Any, |annotation| annotation.value_type().clone());
                    if !expected.accepts_type(&actual) {
                        self.errors
                            .borrow_mut()
                            .push(ModuleTypeError::ResultMismatch {
                                module: self.entry.module().clone(),
                                name: "closure".to_owned(),
                                result_span,
                                expected,
                                actual,
                                annotation_span: annotation.span,
                            });
                    }
                }
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
            ExpressionKind::Call(call) => self.call(expression.span(), call, expected),
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
                let target = self.expression(&member.target)?;
                let name = self.text(member.member.span());
                Ok(match (target, name) {
                    (Some(ValueType::Status), "ok") => Some(ValueType::Bool),
                    (Some(ValueType::Status), "stages") => {
                        Some(ValueType::List(Box::new(ValueType::Status)))
                    }
                    (Some(ValueType::Status), "duration") => Some(ValueType::Duration),
                    (Some(ValueType::Error), "category" | "message") => Some(ValueType::String),
                    (Some(ValueType::Error), "labels" | "frames") => {
                        Some(ValueType::List(Box::new(ValueType::Record)))
                    }
                    (Some(ValueType::Error), "source" | "cause" | "status") => Some(ValueType::Any),
                    _ => None,
                })
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
        if let Some(value_type) = self
            .inferred_bindings
            .borrow()
            .get(&(module.clone(), declaration_span.start()))
        {
            return Some(value_type.clone());
        }
        if self.types.function(module, declaration_span).is_some() {
            return Some(ValueType::Function);
        }
        self.types.binding_type(module, declaration_span).cloned()
    }

    fn qualified_value_type(
        &self,
        name: &flash_syntax::QualifiedName,
        expected: Option<&ValueType>,
    ) -> Option<ValueType> {
        if let Some((value_name, modules)) = name.segments.split_last()
            && !modules.is_empty()
        {
            let modules = modules
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            if let Some(owner) = self.aliases.resolve(self.entry.module(), &modules) {
                let value_name = self.text(value_name.span());
                if let Some(export) = self.names.export(owner, value_name) {
                    return self.declaration_type(owner, export.declaration_span());
                }
            }
        }
        let nominal = self.nominal_for_qualified_prefix(name)?;
        if !nominal.type_parameters().is_empty()
            && !self
                .control
                .charge(AnalysisLimitKind::GenericInstantiations, 1)
        {
            return None;
        }
        let constructor = self.text(name.segments.last()?.span());
        if nominal.kind() != NominalTypeKind::Variant
            || !nominal
                .variants()
                .iter()
                .any(|variant| variant.name() == constructor && variant.payload().is_empty())
        {
            return None;
        }
        let arguments = if nominal.type_parameters().is_empty() {
            Vec::new()
        } else if let Some(ValueType::Nominal { id, arguments }) = expected
            && id.as_ref() == nominal.id()
            && arguments.len() == nominal.type_parameters().len()
        {
            arguments.clone()
        } else {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::AmbiguousGeneric {
                    module: self.entry.module().clone(),
                    name: nominal.id().name().to_owned(),
                    parameter: nominal.type_parameters()[0].name().to_owned(),
                    call_span: name.span,
                    parameter_span: nominal.type_parameters()[0].span(),
                });
            return Some(ValueType::Any);
        };
        if !self.validate_nominal_constraints(nominal, &arguments, name.span) {
            return Some(ValueType::Any);
        }
        Some(ValueType::Nominal {
            id: Box::new(nominal.id().clone()),
            arguments,
        })
    }

    fn nominal_record_type(
        &self,
        record: &flash_syntax::NominalRecordExpression,
        expected_result: Option<&ValueType>,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        let Some(nominal) = self.nominal_for_qualified(&record.name) else {
            for field in &record.fields {
                self.expression(&field.value)?;
            }
            let name = record
                .name
                .segments
                .last()
                .map(|segment| self.text(segment.span()))
                .unwrap_or("<unknown>")
                .to_owned();
            self.errors.borrow_mut().push(ModuleTypeError::UnknownType {
                module: self.entry.module().clone(),
                name,
                span: record.name.span,
            });
            return Ok(Some(ValueType::Any));
        };
        if !nominal.type_parameters().is_empty()
            && !self
                .control
                .charge(AnalysisLimitKind::GenericInstantiations, 1)
        {
            return Ok(None);
        }
        let mut substitutions = BTreeMap::new();
        if let Some(ValueType::Nominal { id, arguments }) = expected_result
            && id.as_ref() == nominal.id()
            && arguments.len() == nominal.type_parameters().len()
        {
            substitutions.extend(
                nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone())),
            );
        }
        let mut actual_fields = Vec::with_capacity(record.fields.len());
        let mut supplied = BTreeSet::new();
        let mut first_spans = BTreeMap::new();
        let mut invalid = false;
        for field in &record.fields {
            let field_name = self.text(field.name.span());
            if let Some(first_span) = first_spans.get(field_name).copied() {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::DuplicateNominalField {
                        module: self.entry.module().clone(),
                        name: nominal.id().name().to_owned(),
                        field: field_name.to_owned(),
                        first_span,
                        duplicate_span: field.name.span(),
                    });
                invalid = true;
            } else {
                first_spans.insert(field_name.to_owned(), field.name.span());
            }
            let expected = nominal
                .fields()
                .iter()
                .find(|item| item.name() == field_name);
            let expected_value =
                expected.map(|field| substitute_type(field.value_type(), &substitutions));
            let actual = self.expression_with_expected(&field.value, expected_value.as_ref())?;
            if expected.is_some() {
                supplied.insert(field_name.to_owned());
            } else {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::UnknownNominalField {
                        module: self.entry.module().clone(),
                        name: nominal.id().name().to_owned(),
                        field: field_name.to_owned(),
                        field_span: field.name.span(),
                        declaration_span: nominal.declaration_span(),
                    });
                invalid = true;
            }
            if let (Some(expected), Some(actual)) = (expected, actual.as_ref()) {
                unify_type(expected.value_type(), actual, &mut substitutions);
            }
            actual_fields.push((field, actual));
        }
        for expected in nominal.fields() {
            if !supplied.contains(expected.name()) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::MissingNominalField {
                        module: self.entry.module().clone(),
                        name: nominal.id().name().to_owned(),
                        field: expected.name().to_owned(),
                        construction_span: record.name.span,
                        field_span: expected.span(),
                    });
                invalid = true;
            }
        }
        for (field, actual) in &actual_fields {
            let Some(actual) = actual else {
                continue;
            };
            if actual == &ValueType::Any {
                continue;
            }
            let field_name = self.text(field.name.span());
            let Some(expected_field) = nominal
                .fields()
                .iter()
                .find(|item| item.name() == field_name)
            else {
                continue;
            };
            let expected = substitute_type(expected_field.value_type(), &substitutions);
            if !expected.accepts_type(actual) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::ArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: nominal.id().name().to_owned(),
                        parameter: field_name.to_owned(),
                        argument_span: field.value.span(),
                        expected,
                        actual: actual.clone(),
                        parameter_span: expected_field.span(),
                    });
                invalid = true;
            }
        }
        if let Some(parameter) = nominal
            .type_parameters()
            .iter()
            .find(|parameter| !substitutions.contains_key(parameter.name()))
        {
            if actual_fields.iter().any(|(_, actual)| {
                actual
                    .as_ref()
                    .is_none_or(|actual| actual == &ValueType::Any)
            }) {
                return Ok(Some(ValueType::Any));
            }
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::AmbiguousGeneric {
                    module: self.entry.module().clone(),
                    name: nominal.id().name().to_owned(),
                    parameter: parameter.name().to_owned(),
                    call_span: record.name.span,
                    parameter_span: parameter.span(),
                });
            return Ok(Some(ValueType::Any));
        }
        for parameter in nominal.type_parameters() {
            let actual = substitutions[parameter.name()].clone();
            for constraint in parameter.constraints() {
                if !self.type_satisfies_constraint(&actual, *constraint) {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::UnsatisfiedConstraint {
                            module: self.entry.module().clone(),
                            name: nominal.id().name().to_owned(),
                            parameter: parameter.name().to_owned(),
                            constraint: *constraint,
                            actual: actual.clone(),
                            call_span: record.name.span,
                            parameter_span: parameter.span(),
                        });
                    invalid = true;
                }
            }
        }
        if invalid {
            return Ok(Some(ValueType::Any));
        }
        Ok(Some(ValueType::Nominal {
            id: Box::new(nominal.id().clone()),
            arguments: nominal
                .type_parameters()
                .iter()
                .map(|parameter| substitutions[parameter.name()].clone())
                .collect(),
        }))
    }

    fn infer_pattern_bindings(&self, pattern: &Pattern, value_type: &ValueType) {
        match pattern {
            Pattern::Binding(identifier) => {
                self.inferred_bindings.borrow_mut().insert(
                    (self.entry.module().clone(), identifier.span().start()),
                    value_type.clone(),
                );
            }
            Pattern::List(pattern) => {
                let element = match value_type {
                    ValueType::List(element) => element.as_ref().clone(),
                    _ => ValueType::Any,
                };
                for item in &pattern.elements {
                    self.infer_pattern_bindings(item, &element);
                }
                if let Some(rest) = pattern.rest {
                    self.inferred_bindings.borrow_mut().insert(
                        (self.entry.module().clone(), rest.span().start()),
                        ValueType::List(Box::new(element)),
                    );
                }
            }
            Pattern::NominalRecord(pattern) => {
                let ValueType::Nominal { id, arguments } = value_type else {
                    return;
                };
                let Some(nominal) = self.types.nominal(id.module(), id.name()) else {
                    return;
                };
                let substitutions = nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect();
                for field in &pattern.fields {
                    let name = self.text(field.name.span());
                    if let Some(schema) = nominal.fields().iter().find(|item| item.name() == name) {
                        self.infer_pattern_bindings(
                            &field.pattern,
                            &substitute_type(schema.value_type(), &substitutions),
                        );
                    }
                }
            }
            Pattern::Variant(pattern) => {
                let ValueType::Nominal { id, arguments } = value_type else {
                    return;
                };
                let Some(nominal) = self.types.nominal(id.module(), id.name()) else {
                    return;
                };
                let Some(constructor) = pattern.constructor.segments.last() else {
                    return;
                };
                let Some(variant) = nominal
                    .variants()
                    .iter()
                    .find(|variant| variant.name() == self.text(constructor.span()))
                else {
                    return;
                };
                let substitutions = nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect();
                for (pattern, payload) in pattern.payload.iter().zip(variant.payload()) {
                    self.infer_pattern_bindings(pattern, &substitute_type(payload, &substitutions));
                }
            }
            Pattern::Wildcard(_) | Pattern::Literal(_) => {}
        }
    }

    fn validate_guard(&self, guard: &Expression) -> Result<(), Box<ModuleTypeError>> {
        if let Some(actual) = self.expression(guard)?
            && !matches!(actual, ValueType::Any | ValueType::Bool)
        {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::GuardMismatch {
                    module: self.entry.module().clone(),
                    guard_span: guard.span(),
                    actual,
                });
        }
        Ok(())
    }

    fn validate_pattern(&self, pattern: &Pattern, value_type: &ValueType) -> bool {
        let valid = self.pattern_accepts_type(pattern, value_type);
        if !valid {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::PatternTypeMismatch {
                    module: self.entry.module().clone(),
                    pattern_span: pattern_span(pattern),
                    value_type: value_type.clone(),
                });
        }
        valid
    }

    fn pattern_accepts_type(&self, pattern: &Pattern, value_type: &ValueType) -> bool {
        if value_type == &ValueType::Any {
            return true;
        }
        match pattern {
            Pattern::Binding(_) | Pattern::Wildcard(_) => true,
            Pattern::Literal(literal) => {
                let literal_type = match literal.kind() {
                    LiteralKind::Null => ValueType::Null,
                    LiteralKind::Boolean(_) => ValueType::Bool,
                    LiteralKind::Integer => ValueType::Int,
                    LiteralKind::Float => ValueType::Float,
                    LiteralKind::SingleQuoted | LiteralKind::DoubleQuoted(_) => ValueType::String,
                };
                value_type.accepts_type(&literal_type)
            }
            Pattern::List(pattern) => {
                let ValueType::List(element) = value_type else {
                    return false;
                };
                pattern
                    .elements
                    .iter()
                    .all(|pattern| self.pattern_accepts_type(pattern, element))
            }
            Pattern::NominalRecord(pattern) => {
                let ValueType::Nominal { id, arguments } = value_type else {
                    return false;
                };
                let Some(nominal) = self.nominal_for_qualified(&pattern.name) else {
                    return false;
                };
                if nominal.kind() != NominalTypeKind::Record
                    || nominal.id() != id.as_ref()
                    || nominal.type_parameters().len() != arguments.len()
                {
                    return false;
                }
                let substitutions = nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect::<BTreeMap<_, _>>();
                pattern.fields.iter().all(|field| {
                    nominal
                        .fields()
                        .iter()
                        .find(|schema| schema.name() == self.text(field.name.span()))
                        .is_some_and(|schema| {
                            self.pattern_accepts_type(
                                &field.pattern,
                                &substitute_type(schema.value_type(), &substitutions),
                            )
                        })
                })
            }
            Pattern::Variant(pattern) => {
                let ValueType::Nominal { id, arguments } = value_type else {
                    return false;
                };
                let Some(nominal) = self.nominal_for_qualified_prefix(&pattern.constructor) else {
                    return false;
                };
                if nominal.kind() != NominalTypeKind::Variant
                    || nominal.id() != id.as_ref()
                    || nominal.type_parameters().len() != arguments.len()
                {
                    return false;
                }
                let Some(constructor) = pattern.constructor.segments.last() else {
                    return false;
                };
                let Some(variant) = nominal
                    .variants()
                    .iter()
                    .find(|variant| variant.name() == self.text(constructor.span()))
                else {
                    return false;
                };
                if variant.payload().len() != pattern.payload.len() {
                    return false;
                }
                let substitutions = nominal
                    .type_parameters()
                    .iter()
                    .zip(arguments)
                    .map(|(parameter, argument)| (parameter.name().to_owned(), argument.clone()))
                    .collect::<BTreeMap<_, _>>();
                pattern
                    .payload
                    .iter()
                    .zip(variant.payload())
                    .all(|(pattern, payload)| {
                        self.pattern_accepts_type(
                            pattern,
                            &substitute_type(payload, &substitutions),
                        )
                    })
            }
        }
    }

    fn validate_match(
        &self,
        statement: &flash_syntax::MatchStatement,
        subject_type: Option<&ValueType>,
    ) -> Result<(), Box<ModuleTypeError>> {
        let nominal = subject_type
            .and_then(|value_type| match value_type {
                ValueType::Nominal { id, .. } => self.types.nominal(id.module(), id.name()),
                _ => None,
            })
            .filter(|nominal| nominal.kind() == NominalTypeKind::Variant);
        let mut covered = BTreeMap::<String, Span>::new();
        let mut covers_all = None;
        for arm in &statement.arms {
            let compatible = subject_type
                .is_none_or(|value_type| self.validate_pattern(&arm.pattern, value_type));
            let constructor = match &arm.pattern {
                Pattern::Variant(pattern) => pattern
                    .constructor
                    .segments
                    .last()
                    .map(|constructor| self.text(constructor.span()).to_owned()),
                _ => None,
            };
            let covering_span = covers_all.or_else(|| {
                constructor
                    .as_ref()
                    .and_then(|constructor| covered.get(constructor).copied())
            });
            if let Some(covering_span) = covering_span {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::UnreachableMatchArm {
                        module: self.entry.module().clone(),
                        arm_span: arm.span,
                        covering_span,
                    });
            }
            if compatible && arm.guard.is_none() {
                match &arm.pattern {
                    Pattern::Wildcard(_) | Pattern::Binding(_) => covers_all = Some(arm.span),
                    Pattern::Variant(_) => {
                        if let Some(constructor) = constructor {
                            covered.entry(constructor).or_insert(arm.span);
                        }
                    }
                    _ => {}
                }
            }
        }
        let Some(nominal) = nominal else {
            return Ok(());
        };
        if covers_all.is_some() {
            return Ok(());
        }
        let missing = nominal
            .variants()
            .iter()
            .map(|variant| variant.name().to_owned())
            .filter(|name| !covered.contains_key(name))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::NonExhaustiveMatch {
                    module: self.entry.module().clone(),
                    match_span: statement.value.span(),
                    nominal: nominal.id().clone(),
                    missing,
                    declaration_span: nominal.declaration_span(),
                });
        }
        Ok(())
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
        expected_result: Option<&ValueType>,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        if self.control.is_cancelled() {
            return Ok(None);
        }
        if !matches!(
            call.callee.kind(),
            ExpressionKind::Symbol(_) | ExpressionKind::Qualified(_)
        ) {
            self.expression(&call.callee)?;
        }
        if let ExpressionKind::Qualified(name) = call.callee.kind()
            && let Some(value_type) =
                self.variant_call_type(name, call_span, call, expected_result)?
        {
            return Ok(Some(value_type));
        }
        if let ExpressionKind::Qualified(name) = call.callee.kind()
            && let Some(operation) = self.operation_for_qualified(name)
        {
            return self.operation_call_type(&operation, call_span, call);
        }
        if let ExpressionKind::Qualified(name) = call.callee.kind()
            && let Some(unknown) = self.unknown_standard_operation(name)
        {
            for argument in &call.arguments {
                self.expression(argument)?;
            }
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::UnknownOperation {
                    module: self.entry.module().clone(),
                    name: unknown,
                    span: name.span,
                });
            return Ok(Some(ValueType::Any));
        }

        let signature = if let ExpressionKind::Qualified(name) = call.callee.kind() {
            self.function_for_qualified(name)
        } else {
            let Some(reference) = self
                .names
                .reference(self.entry.module(), call.callee.span())
            else {
                if let ExpressionKind::Symbol(identifier) = call.callee.kind()
                    && let Some(intrinsic) =
                        ExpressionIntrinsic::lookup(self.text(identifier.span()))
                {
                    let mut argument_types = Vec::with_capacity(call.arguments.len());
                    for argument in &call.arguments {
                        argument_types.push(self.expression(argument)?);
                    }
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
            self.types.function(target_module, declaration_span)
        };
        let Some(signature) = signature else {
            for argument in &call.arguments {
                self.expression(argument)?;
            }
            return Ok(None);
        };
        if !signature.type_parameters().is_empty()
            && !self
                .control
                .charge(AnalysisLimitKind::GenericInstantiations, 1)
        {
            return Ok(None);
        }
        if !call.type_arguments.is_empty()
            && call.type_arguments.len() != signature.type_parameters().len()
        {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::GenericArity {
                    module: self.entry.module().clone(),
                    name: signature.name().to_owned(),
                    call_span,
                    expected: signature.type_parameters().len(),
                    actual: call.type_arguments.len(),
                    declaration_span: signature.declaration_span(),
                });
            return Ok(Some(ValueType::Any));
        }
        let mut substitutions = BTreeMap::new();
        if call.type_arguments.is_empty() {
            if let Some(expected_result) = expected_result {
                unify_type(signature.result(), expected_result, &mut substitutions);
            }
        } else {
            for (parameter, argument) in
                signature.type_parameters().iter().zip(&call.type_arguments)
            {
                let actual = self
                    .types
                    .annotation(self.entry.module(), argument.span)
                    .map_or(ValueType::Any, |annotation| annotation.value_type().clone());
                substitutions.insert(parameter.name().to_owned(), actual);
            }
        }
        let mut argument_types = Vec::with_capacity(call.arguments.len());
        for (index, argument) in call.arguments.iter().enumerate() {
            let expected = signature
                .parameters()
                .get(index)
                .map(|parameter| substitute_type(parameter.value_type(), &substitutions));
            let actual = self.expression_with_expected(argument, expected.as_ref())?;
            if call.type_arguments.is_empty()
                && let (Some(actual), Some(parameter)) =
                    (actual.as_ref(), signature.parameters().get(index))
            {
                unify_type(parameter.value_type(), actual, &mut substitutions);
            }
            argument_types.push(actual);
        }
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
        for parameter in signature.type_parameters() {
            let Some(actual) = substitutions.get(parameter.name()).cloned() else {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::AmbiguousGeneric {
                        module: self.entry.module().clone(),
                        name: signature.name().to_owned(),
                        parameter: parameter.name().to_owned(),
                        call_span,
                        parameter_span: parameter.span(),
                    });
                return Ok(Some(ValueType::Any));
            };
            for constraint in parameter.constraints() {
                if !self.type_satisfies_constraint(&actual, *constraint) {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::UnsatisfiedConstraint {
                            module: self.entry.module().clone(),
                            name: signature.name().to_owned(),
                            parameter: parameter.name().to_owned(),
                            constraint: *constraint,
                            actual: actual.clone(),
                            call_span,
                            parameter_span: parameter.span(),
                        });
                    return Ok(Some(ValueType::Any));
                }
            }
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
            let expected = substitute_type(parameter.value_type(), &substitutions);
            if !expected.accepts_type(&actual) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::ArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: signature.name().to_owned(),
                        parameter: parameter.name().to_owned(),
                        argument_span: argument.span(),
                        expected,
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
            substitute_type(signature.result(), &substitutions)
        }))
    }

    fn variant_call_type(
        &self,
        name: &flash_syntax::QualifiedName,
        call_span: Span,
        call: &flash_syntax::CallExpression,
        expected_result: Option<&ValueType>,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        if name.segments.len() < 2 {
            return Ok(None);
        }
        let type_name = self.text(name.segments[name.segments.len() - 2].span());
        let constructor = self.text(name.segments[name.segments.len() - 1].span());
        let type_path = &name.segments[..name.segments.len() - 1];
        let type_segment = &type_path[type_path.len() - 1];
        let modules = &type_path[..type_path.len() - 1];
        if !modules.is_empty() {
            let module_names = modules
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            if let Some(owner) = self.aliases.resolve(self.entry.module(), &module_names)
                && self
                    .types
                    .nominal(owner, self.text(type_segment.span()))
                    .is_some()
                && self
                    .names
                    .export(owner, self.text(type_segment.span()))
                    .is_none()
            {
                self.errors.borrow_mut().push(ModuleTypeError::UnknownType {
                    module: self.entry.module().clone(),
                    name: self.text(type_segment.span()).to_owned(),
                    span: type_segment.span(),
                });
                return Ok(Some(ValueType::Any));
            }
        }
        let Some(nominal) = self.nominal_for_qualified_prefix(name) else {
            return Ok(None);
        };
        if !nominal.type_parameters().is_empty()
            && !self
                .control
                .charge(AnalysisLimitKind::GenericInstantiations, 1)
        {
            return Ok(None);
        }
        let Some(variant) = nominal
            .variants()
            .iter()
            .find(|variant| variant.name() == constructor)
        else {
            return Ok(None);
        };
        if !call.type_arguments.is_empty()
            && call.type_arguments.len() != nominal.type_parameters().len()
        {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::GenericArity {
                    module: self.entry.module().clone(),
                    name: nominal.id().name().to_owned(),
                    call_span,
                    expected: nominal.type_parameters().len(),
                    actual: call.type_arguments.len(),
                    declaration_span: nominal.declaration_span(),
                });
            return Ok(Some(ValueType::Any));
        }
        let mut substitutions = BTreeMap::new();
        if call.type_arguments.is_empty() {
            if let Some(ValueType::Nominal { id, arguments }) = expected_result
                && id.as_ref() == nominal.id()
                && arguments.len() == nominal.type_parameters().len()
            {
                for (parameter, argument) in nominal.type_parameters().iter().zip(arguments) {
                    substitutions
                        .entry(parameter.name().to_owned())
                        .or_insert_with(|| argument.clone());
                }
            }
        } else {
            for (parameter, argument) in nominal.type_parameters().iter().zip(&call.type_arguments)
            {
                let actual = self
                    .types
                    .annotation(self.entry.module(), argument.span)
                    .map_or(ValueType::Any, |annotation| annotation.value_type().clone());
                substitutions.insert(parameter.name().to_owned(), actual);
            }
        }
        let mut argument_types = Vec::with_capacity(call.arguments.len());
        for (index, argument) in call.arguments.iter().enumerate() {
            let expected = variant
                .payload()
                .get(index)
                .map(|payload| substitute_type(payload, &substitutions));
            let actual = self.expression_with_expected(argument, expected.as_ref())?;
            if call.type_arguments.is_empty()
                && let (Some(actual), Some(payload)) =
                    (actual.as_ref(), variant.payload().get(index))
            {
                unify_type(payload, actual, &mut substitutions);
            }
            argument_types.push(actual);
        }
        if call.arguments.len() != variant.payload().len() {
            self.errors.borrow_mut().push(ModuleTypeError::CallArity {
                module: self.entry.module().clone(),
                name: format!("{type_name}::{constructor}"),
                call_span,
                expected: variant.payload().len(),
                actual: call.arguments.len(),
                declaration_span: variant.span(),
            });
            return Ok(Some(ValueType::Any));
        }
        let mut invalid = false;
        for ((argument, actual), expected) in call
            .arguments
            .iter()
            .zip(argument_types.iter())
            .zip(variant.payload())
        {
            let Some(actual) = actual else {
                continue;
            };
            if actual == &ValueType::Any {
                continue;
            }
            let expected = substitute_type(expected, &substitutions);
            if !expected.accepts_type(actual) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::ArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: format!("{type_name}::{constructor}"),
                        parameter: "payload".to_owned(),
                        argument_span: argument.span(),
                        expected,
                        actual: actual.clone(),
                        parameter_span: variant.span(),
                    });
                invalid = true;
            }
        }
        if let Some(parameter) = nominal
            .type_parameters()
            .iter()
            .find(|parameter| !substitutions.contains_key(parameter.name()))
        {
            if argument_types.iter().any(|actual| {
                actual
                    .as_ref()
                    .is_none_or(|actual| actual == &ValueType::Any)
            }) {
                return Ok(Some(ValueType::Any));
            }
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::AmbiguousGeneric {
                    module: self.entry.module().clone(),
                    name: nominal.id().name().to_owned(),
                    parameter: parameter.name().to_owned(),
                    call_span,
                    parameter_span: parameter.span(),
                });
            return Ok(Some(ValueType::Any));
        }
        for parameter in nominal.type_parameters() {
            let actual = substitutions[parameter.name()].clone();
            for constraint in parameter.constraints() {
                if !self.type_satisfies_constraint(&actual, *constraint) {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::UnsatisfiedConstraint {
                            module: self.entry.module().clone(),
                            name: nominal.id().name().to_owned(),
                            parameter: parameter.name().to_owned(),
                            constraint: *constraint,
                            actual: actual.clone(),
                            call_span,
                            parameter_span: parameter.span(),
                        });
                    invalid = true;
                }
            }
        }
        if invalid {
            return Ok(Some(ValueType::Any));
        }
        Ok(Some(ValueType::Nominal {
            id: Box::new(nominal.id().clone()),
            arguments: nominal
                .type_parameters()
                .iter()
                .map(|parameter| substitutions[parameter.name()].clone())
                .collect(),
        }))
    }

    fn nominal_for_qualified(&self, name: &flash_syntax::QualifiedName) -> Option<&NominalType> {
        let (last, modules) = name.segments.split_last()?;
        let nominal_name = self.text(last.span());
        let owner = if modules.is_empty() {
            self.entry.module()
        } else {
            let modules = modules
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            self.aliases.resolve(self.entry.module(), &modules)?
        };
        if !modules.is_empty() && self.names.export(owner, nominal_name).is_none() {
            return None;
        }
        self.types.nominal(owner, nominal_name)
    }

    fn function_for_qualified(
        &self,
        name: &flash_syntax::QualifiedName,
    ) -> Option<&FunctionSignature> {
        let (function, modules) = name.segments.split_last()?;
        if modules.is_empty() {
            return None;
        }
        let modules = modules
            .iter()
            .map(|segment| self.text(segment.span()))
            .collect::<Vec<_>>();
        let owner = self.aliases.resolve(self.entry.module(), &modules)?;
        let function = self.text(function.span());
        self.names.export(owner, function)?;
        self.types
            .functions(owner)
            .iter()
            .find(|signature| signature.name() == function)
    }

    fn operation_for_qualified(
        &self,
        name: &flash_syntax::QualifiedName,
    ) -> Option<OperationDescriptor> {
        let (operation, modules) = name.segments.split_last()?;
        if modules.is_empty() {
            return None;
        }
        let modules = modules
            .iter()
            .map(|segment| self.text(segment.span()))
            .collect::<Vec<_>>();
        let owner = self.aliases.resolve(self.entry.module(), &modules)?;
        standard_operation(owner, self.text(operation.span()))
    }

    fn unknown_standard_operation(&self, name: &flash_syntax::QualifiedName) -> Option<String> {
        let (operation, modules) = name.segments.split_last()?;
        if modules.is_empty() {
            return None;
        }
        let modules = modules
            .iter()
            .map(|segment| self.text(segment.span()))
            .collect::<Vec<_>>();
        let owner = self.aliases.resolve(self.entry.module(), &modules)?;
        matches!(owner.origin(), ModuleOrigin::Standard { .. })
            .then(|| self.text(operation.span()).to_owned())
    }

    fn operation_call_type(
        &self,
        operation: &OperationDescriptor,
        call_span: Span,
        call: &flash_syntax::CallExpression,
    ) -> Result<Option<ValueType>, Box<ModuleTypeError>> {
        if !self.control.charge(
            AnalysisLimitKind::OverloadCandidates,
            operation.overloads().len() as u64,
        ) || (!operation.type_parameters().is_empty()
            && !self
                .control
                .charge(AnalysisLimitKind::GenericInstantiations, 1))
        {
            return Ok(None);
        }
        let overload = operation
            .overloads()
            .iter()
            .find_map(|overload| match overload.input() {
                OperationInputType::Value(input) => Some((input, overload.result())),
                OperationInputType::ValueStream(_) => None,
            })
            .expect("a callable standard operation has a value overload");
        if call.arguments.len() != 1 {
            for argument in &call.arguments {
                self.expression(argument)?;
            }
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::OperationCallArity {
                    module: self.entry.module().clone(),
                    name: operation.id().qualified_name(),
                    call_span,
                    expected: 1,
                    actual: call.arguments.len(),
                });
            return Ok(Some(ValueType::Any));
        }
        if !call.type_arguments.is_empty()
            && call.type_arguments.len() != operation.type_parameters().len()
        {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::OperationGenericArity {
                    module: self.entry.module().clone(),
                    name: operation.id().qualified_name(),
                    call_span,
                    expected: operation.type_parameters().len(),
                    actual: call.type_arguments.len(),
                });
            return Ok(Some(ValueType::Any));
        }
        let mut substitutions = BTreeMap::new();
        for (parameter, argument) in operation.type_parameters().iter().zip(&call.type_arguments) {
            let actual = self
                .types
                .annotation(self.entry.module(), argument.span)
                .map_or(ValueType::Any, |annotation| annotation.value_type().clone());
            substitutions.insert(parameter.clone(), actual);
        }
        let expected = substitute_type(overload.0, &substitutions);
        let actual = self.expression_with_expected(&call.arguments[0], Some(&expected))?;
        if call.type_arguments.is_empty()
            && let Some(actual) = actual.as_ref()
            && !unify_type(overload.0, actual, &mut substitutions)
        {
            self.errors
                .borrow_mut()
                .push(ModuleTypeError::OperationArgumentMismatch {
                    module: self.entry.module().clone(),
                    name: operation.id().qualified_name(),
                    argument_span: call.arguments[0].span(),
                    expected: overload.0.clone(),
                    actual: actual.clone(),
                });
            return Ok(Some(ValueType::Any));
        }
        for parameter in operation.type_parameters() {
            if !substitutions.contains_key(parameter) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::AmbiguousOperationGeneric {
                        module: self.entry.module().clone(),
                        name: operation.id().qualified_name(),
                        parameter: parameter.clone(),
                        call_span,
                    });
                return Ok(Some(ValueType::Any));
            }
        }
        if let Some(actual) = actual
            && actual != ValueType::Any
        {
            let expected = substitute_type(overload.0, &substitutions);
            if !expected.accepts_type(&actual) {
                self.errors
                    .borrow_mut()
                    .push(ModuleTypeError::OperationArgumentMismatch {
                        module: self.entry.module().clone(),
                        name: operation.id().qualified_name(),
                        argument_span: call.arguments[0].span(),
                        expected,
                        actual,
                    });
                return Ok(Some(ValueType::Any));
            }
        }
        Ok(Some(substitute_type(overload.1, &substitutions)))
    }

    fn nominal_for_qualified_prefix(
        &self,
        constructor: &flash_syntax::QualifiedName,
    ) -> Option<&NominalType> {
        let (_, type_path) = constructor.segments.split_last()?;
        let (type_name, modules) = type_path.split_last()?;
        let owner = if modules.is_empty() {
            self.entry.module()
        } else {
            let modules = modules
                .iter()
                .map(|segment| self.text(segment.span()))
                .collect::<Vec<_>>();
            self.aliases.resolve(self.entry.module(), &modules)?
        };
        let type_name = self.text(type_name.span());
        if !modules.is_empty() && self.names.export(owner, type_name).is_none() {
            return None;
        }
        self.types.nominal(owner, type_name)
    }

    fn type_satisfies_constraint(
        &self,
        value_type: &ValueType,
        constraint: TypeConstraint,
    ) -> bool {
        self.type_satisfies_constraint_inner(value_type, constraint, &mut BTreeSet::new())
    }

    fn validate_nominal_constraints(
        &self,
        nominal: &NominalType,
        arguments: &[ValueType],
        span: Span,
    ) -> bool {
        let mut valid = true;
        for (parameter, actual) in nominal.type_parameters().iter().zip(arguments) {
            for constraint in parameter.constraints() {
                if !self.type_satisfies_constraint(actual, *constraint) {
                    self.errors
                        .borrow_mut()
                        .push(ModuleTypeError::UnsatisfiedConstraint {
                            module: self.entry.module().clone(),
                            name: nominal.id().name().to_owned(),
                            parameter: parameter.name().to_owned(),
                            constraint: *constraint,
                            actual: actual.clone(),
                            call_span: span,
                            parameter_span: parameter.span(),
                        });
                    valid = false;
                }
            }
        }
        valid
    }

    fn type_satisfies_constraint_inner(
        &self,
        value_type: &ValueType,
        constraint: TypeConstraint,
        visiting: &mut BTreeSet<NominalTypeId>,
    ) -> bool {
        match constraint {
            TypeConstraint::Equal => match value_type {
                ValueType::Any | ValueType::Function | ValueType::Closure => false,
                ValueType::TypeParameter(name) => {
                    self.type_parameter_satisfies_constraint(name, constraint)
                }
                ValueType::List(element) => {
                    self.type_satisfies_constraint_inner(element, constraint, visiting)
                }
                ValueType::Nominal { id, arguments } => {
                    if !arguments.iter().all(|argument| {
                        self.type_satisfies_constraint_inner(argument, constraint, visiting)
                    }) {
                        return false;
                    }
                    if !visiting.insert(id.as_ref().clone()) {
                        return true;
                    }
                    let result = self
                        .types
                        .nominal(id.module(), id.name())
                        .filter(|nominal| nominal.type_parameters().len() == arguments.len())
                        .is_some_and(|nominal| {
                            let substitutions = nominal
                                .type_parameters()
                                .iter()
                                .zip(arguments)
                                .map(|(parameter, argument)| {
                                    (parameter.name().to_owned(), argument.clone())
                                })
                                .collect::<BTreeMap<_, _>>();
                            nominal.fields().iter().all(|field| {
                                self.type_satisfies_constraint_inner(
                                    &substitute_type(field.value_type(), &substitutions),
                                    constraint,
                                    visiting,
                                )
                            }) && nominal.variants().iter().all(|variant| {
                                variant.payload().iter().all(|payload| {
                                    self.type_satisfies_constraint_inner(
                                        &substitute_type(payload, &substitutions),
                                        constraint,
                                        visiting,
                                    )
                                })
                            })
                        });
                    visiting.remove(id.as_ref());
                    result
                }
                _ => true,
            },
            TypeConstraint::Ordered => match value_type {
                ValueType::Int
                | ValueType::Float
                | ValueType::String
                | ValueType::Bytes
                | ValueType::Path
                | ValueType::Duration
                | ValueType::ByteSize => true,
                ValueType::List(element) => {
                    self.type_satisfies_constraint_inner(element, constraint, visiting)
                }
                ValueType::TypeParameter(name) => {
                    self.type_parameter_satisfies_constraint(name, constraint)
                }
                _ => false,
            },
        }
    }

    fn type_parameter_satisfies_constraint(&self, name: &str, constraint: TypeConstraint) -> bool {
        self.type_parameter_scopes
            .borrow()
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .is_some_and(|constraints| {
                constraints.contains(&constraint)
                    || (constraint == TypeConstraint::Equal
                        && constraints.contains(&TypeConstraint::Ordered))
            })
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
    callable_depth: usize,
    references: Vec<ModuleNameReference>,
    visible: Vec<ModuleVisibleBinding>,
    errors: Vec<ModuleNameError>,
    control: &'a AnalysisControl,
}

struct ReferenceScope {
    bindings: BTreeMap<String, Option<ReferenceBinding>>,
    span: Span,
}

#[derive(Clone)]
struct ReferenceBinding {
    target: ModuleReferenceTarget,
    mutable: bool,
    callable_depth: usize,
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
                Some(ReferenceBinding {
                    target: dynamic_target.clone(),
                    mutable: false,
                    callable_depth: 0,
                }),
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
                bindings: BTreeMap::from([(
                    "args".to_owned(),
                    Some(ReferenceBinding {
                        target: target.clone(),
                        mutable: false,
                        callable_depth: 0,
                    }),
                )]),
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
            root.insert(
                import.name().to_owned(),
                Some(ReferenceBinding {
                    target: target.clone(),
                    mutable: false,
                    callable_depth: 0,
                }),
            );
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
            callable_depth: 0,
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
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_)
            | StatementKind::NominalType(_)
            | StatementKind::VariantType(_) => Ok(()),
            StatementKind::Declaration(declaration) => {
                self.expression(&declaration.value)?;
                self.pattern(
                    &declaration.pattern,
                    statement.span().end(),
                    declaration.mutable,
                )
            }
            StatementKind::Assignment(assignment) => {
                self.assignment_target(assignment.target.name.span(), assignment.target.span)?;
                self.expression(&assignment.value)
            }
            StatementKind::Environment(environment) => match environment {
                flash_syntax::EnvironmentStatement::Export { value, .. } => self.expression(value),
                flash_syntax::EnvironmentStatement::Unset { .. } => Ok(()),
            },
            StatementKind::Function(function) => {
                let available = self.ensure_available(function.name.span());
                self.push_scope(function.body.span);
                self.callable_depth += 1;
                self.insert_local(function.name.span(), function.body.span.start(), false);
                self.push_scope(function.body.span);
                let result = (|| {
                    for parameter in &function.parameters {
                        self.pattern(&parameter.pattern, function.body.span.start(), false)?;
                    }
                    self.block(&function.body)
                })();
                self.callable_depth -= 1;
                self.pop_scope();
                self.pop_scope();
                result?;
                if available {
                    self.insert_local(function.name.span(), statement.span().end(), false);
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
                    self.declare(statement.binding.span(), statement.body.span.start(), false)?;
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
            StatementKind::Try(statement) => {
                self.block(&statement.try_block)?;
                self.push_scope(statement.catch_block.span);
                let result = (|| {
                    self.declare(
                        statement.catch_binding.span(),
                        statement.catch_block.span.start(),
                        false,
                    )?;
                    self.statements(&statement.catch_block.statements)
                })();
                self.pop_scope();
                result
            }
            StatementKind::Throw(expression) => self.expression(expression),
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
            self.pattern(&arm.pattern, arm.span.start(), false)?;
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

    fn pattern(
        &mut self,
        pattern: &Pattern,
        visible_from: usize,
        mutable: bool,
    ) -> Result<(), Box<ModuleNameError>> {
        match pattern {
            Pattern::Binding(identifier) => self.declare(identifier.span(), visible_from, mutable),
            Pattern::Literal(literal) => self.literal(literal),
            Pattern::Wildcard(_) => Ok(()),
            Pattern::List(pattern) => {
                for element in &pattern.elements {
                    self.pattern(element, visible_from, mutable)?;
                }
                if let Some(rest) = pattern.rest {
                    self.declare(rest.span(), visible_from, mutable)?;
                }
                Ok(())
            }
            Pattern::NominalRecord(pattern) => {
                for field in &pattern.fields {
                    self.pattern(&field.pattern, visible_from, mutable)?;
                }
                Ok(())
            }
            Pattern::Variant(pattern) => {
                for payload in &pattern.payload {
                    self.pattern(payload, visible_from, mutable)?;
                }
                Ok(())
            }
        }
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
            ExpressionKind::Symbol(_) | ExpressionKind::Qualified(_) => Ok(()),
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
            ExpressionKind::NominalRecord(record) => {
                for field in &record.fields {
                    self.expression(&field.value)?;
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
        self.callable_depth += 1;
        let result = (|| {
            for parameter in &closure.parameters {
                self.pattern(&parameter.pattern, closure.body.span().start(), false)?;
            }
            self.chain(&closure.body)
        })();
        self.callable_depth -= 1;
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
        let Some(binding) = target else {
            return Ok(());
        };
        self.references.push(ModuleNameReference {
            name,
            reference_span,
            target: binding.target,
        });
        Ok(())
    }

    fn assignment_target(
        &mut self,
        name_span: Span,
        reference_span: Span,
    ) -> Result<(), Box<ModuleNameError>> {
        let name = self.text(name_span).to_owned();
        let Some(binding) = self.visible_target(&name) else {
            self.errors.push(ModuleNameError::UnknownReference {
                module: self.entry.module().clone(),
                name,
                reference_span,
            });
            return Ok(());
        };
        let Some(binding) = binding else {
            return Ok(());
        };
        self.references.push(ModuleNameReference {
            name: name.clone(),
            reference_span,
            target: binding.target.clone(),
        });
        let failure = match &binding.target {
            ModuleReferenceTarget::DynamicStatus | ModuleReferenceTarget::ScriptArguments => {
                Some(ModuleNameError::ImmutableAssignment {
                    module: self.entry.module().clone(),
                    name,
                    assignment_span: reference_span,
                    declaration_span: None,
                })
            }
            ModuleReferenceTarget::Imported { import_span, .. } => {
                Some(ModuleNameError::ImportedAssignment {
                    module: self.entry.module().clone(),
                    name,
                    assignment_span: reference_span,
                    import_span: *import_span,
                })
            }
            ModuleReferenceTarget::Local {
                declaration_span, ..
            } if binding.callable_depth < self.callable_depth => {
                Some(ModuleNameError::CapturedAssignment {
                    module: self.entry.module().clone(),
                    name,
                    assignment_span: reference_span,
                    declaration_span: *declaration_span,
                })
            }
            ModuleReferenceTarget::Local {
                declaration_span, ..
            } if !binding.mutable => Some(ModuleNameError::ImmutableAssignment {
                module: self.entry.module().clone(),
                name,
                assignment_span: reference_span,
                declaration_span: Some(*declaration_span),
            }),
            ModuleReferenceTarget::Local { .. } => None,
        };
        if let Some(failure) = failure {
            self.errors.push(failure);
        }
        Ok(())
    }

    fn visible_target(&self, name: &str) -> Option<Option<ReferenceBinding>> {
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
                first_span: binding_span(&first.target),
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
        mutable: bool,
    ) -> Result<(), Box<ModuleNameError>> {
        if self.ensure_available(declaration_span) {
            self.insert_local(declaration_span, visible_from, mutable);
        }
        Ok(())
    }

    fn insert_local(&mut self, declaration_span: Span, visible_from: usize, mutable: bool) {
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
        scope.bindings.insert(
            name.clone(),
            Some(ReferenceBinding {
                target: target.clone(),
                mutable,
                callable_depth: self.callable_depth,
            }),
        );
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
        aliases: &ModuleAliasRegistry,
        names: &ModuleNameRegistry,
        commands: &CommandRegistry,
        control: &AnalysisControl,
    ) -> Self {
        let mut direct = BTreeMap::new();
        let semantics = StaticEffectSemantics { aliases, names };
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
                    &semantics,
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

struct StaticEffectSemantics<'a> {
    aliases: &'a ModuleAliasRegistry,
    names: &'a ModuleNameRegistry,
}

struct StaticEffectAnalyzer<'a> {
    module: ModuleId,
    source: SourceFile,
    sources: &'a ModuleSourceRegistry,
    semantics: &'a StaticEffectSemantics<'a>,
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
        semantics: &'a StaticEffectSemantics<'a>,
        commands: &'a CommandRegistry,
        control: &'a AnalysisControl,
    ) -> ModuleEffectSummary {
        let mut analyzer = Self {
            module: module.clone(),
            source: source.clone(),
            sources,
            semantics,
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
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_)
            | StatementKind::NominalType(_)
            | StatementKind::VariantType(_)
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
            StatementKind::Try(statement) => {
                self.statements(&statement.try_block.statements);
                self.statements(&statement.catch_block.statements);
            }
            StatementKind::Throw(expression) => self.expression(expression),
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
        if self.pure_operation_pipeline(pipeline) {
            for stage in pipeline.stages() {
                let StageKind::Expression(expression) = stage.kind() else {
                    unreachable!("a pure operation pipeline contains only expressions")
                };
                self.expression(expression);
            }
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

    fn pure_operation_pipeline(&self, pipeline: &Pipeline) -> bool {
        if self.module.language() != LanguageMajor::V2 || pipeline.stages().len() < 2 {
            return false;
        }
        matches!(pipeline.stages()[0].kind(), StageKind::Expression(_))
            && pipeline.stages()[1..].iter().all(|stage| {
                let StageKind::Expression(expression) = stage.kind() else {
                    return false;
                };
                let ExpressionKind::Qualified(name) = expression.kind() else {
                    return false;
                };
                self.qualified_operation(name).is_some()
            })
    }

    fn qualified_operation(
        &self,
        name: &flash_syntax::QualifiedName,
    ) -> Option<OperationDescriptor> {
        let (operation, modules) = name.segments.split_last()?;
        if modules.is_empty() {
            return None;
        }
        let modules = modules
            .iter()
            .map(|segment| {
                self.source
                    .slice(segment.span())
                    .expect("qualified operation spans belong to their source")
            })
            .collect::<Vec<_>>();
        let owner = self.semantics.aliases.resolve(&self.module, &modules)?;
        standard_operation(
            owner,
            self.source
                .slice(operation.span())
                .expect("operation spans belong to their source"),
        )
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
                    .semantics
                    .names
                    .reference(&self.module, expression.span())
                    .is_some_and(|reference| {
                        matches!(reference.target(), ModuleReferenceTarget::DynamicStatus)
                    })
                {
                    self.summary.push(ModuleEffect::Status, expression.span());
                }
            }
            ExpressionKind::Symbol(_) | ExpressionKind::Qualified(_) => {}
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
            ExpressionKind::NominalRecord(record) => {
                for field in &record.fields {
                    self.expression(&field.value);
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
        let Some(reference) = self.semantics.names.reference(&self.module, callee.span()) else {
            if let ExpressionKind::Qualified(name) = callee.kind()
                && self.qualified_operation(name).is_some()
            {
                return;
            }
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
    aliases: ModuleAliasRegistry,
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

    /// The structured `CMD001`-`CMD006` or `PIP001`-`PIP004` diagnostic.
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
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_)
            | StatementKind::NominalType(_)
            | StatementKind::VariantType(_) => {}
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
            StatementKind::Try(statement) => {
                self.statements(&statement.try_block.statements);
                self.statements(&statement.catch_block.statements);
            }
            StatementKind::Throw(expression) => self.expression(expression),
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
                StageKind::Command(command) => self.command_contract(command, stage.span()),
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
        let pure_v2_expression_pipeline = self.module.language() == LanguageMajor::V2
            && pipeline
                .stages()
                .iter()
                .all(|stage| matches!(stage.kind(), StageKind::Expression(_)));
        if pipeline.stages().len() > 1 && !pure_v2_expression_pipeline {
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

    fn command_contract(
        &mut self,
        command: &flash_syntax::CommandStage,
        stage_span: Span,
    ) -> StageCarrierContract {
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
                self.command_arguments(command, stage_span, signature);
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

    fn command_arguments(
        &mut self,
        command: &flash_syntax::CommandStage,
        stage_span: Span,
        signature: &CommandSignature,
    ) {
        let shaped = command
            .items
            .iter()
            .filter_map(|item| match item.kind() {
                CommandItemKind::Word(word) => Some((
                    static_word_text(word, self.source)
                        .map(|text| CommandArgumentInput::Word(Some(text.into_bytes())))
                        .unwrap_or(CommandArgumentInput::Word(None)),
                    item.span(),
                )),
                CommandItemKind::Closure(_) => Some((CommandArgumentInput::Closure, item.span())),
                CommandItemKind::Spread(_) => {
                    Some((CommandArgumentInput::DynamicTail, item.span()))
                }
                CommandItemKind::Redirection(_) => None,
            })
            .collect::<Vec<_>>();
        let inputs = shaped
            .iter()
            .map(|(input, _)| input.clone())
            .collect::<Vec<_>>();
        for fault in signature.arguments().validate(&inputs) {
            let span = fault
                .argument_index()
                .and_then(|index| shaped.get(index))
                .map_or(stage_span, |(_, span)| *span);
            self.errors.push(ModulePipelineError {
                module: self.module.clone(),
                diagnostic: self.command_argument_diagnostic(signature.name(), span, &fault),
            });
        }
    }

    fn command_argument_diagnostic(
        &self,
        command: &str,
        span: Span,
        fault: &CommandArgumentFault,
    ) -> Diagnostic {
        match fault.kind() {
            CommandArgumentFaultKind::Arity {
                minimum,
                maximum,
                actual,
            } => {
                let expected = match maximum {
                    Some(maximum) if minimum == maximum => minimum.to_string(),
                    Some(maximum) => format!("{minimum}..={maximum}"),
                    None => format!("at least {minimum}"),
                };
                Diagnostic::new(
                    Severity::Error,
                    "CMD003",
                    format!(
                        "`{command}` expects {expected} positional argument(s), found {actual}"
                    ),
                )
                .with_primary(span, "this invocation has the wrong positional arity")
            }
            CommandArgumentFaultKind::UnknownOption { option } => Diagnostic::new(
                Severity::Error,
                "CMD004",
                format!("`{command}` does not define option `{option}`"),
            )
            .with_primary(span, "unknown built-in option"),
            CommandArgumentFaultKind::MissingOptionValues {
                option,
                expected,
                actual,
            } => Diagnostic::new(
                Severity::Error,
                "CMD004",
                format!("option `{option}` expects {expected} value(s), found {actual}"),
            )
            .with_primary(span, "this option is missing a required value"),
            CommandArgumentFaultKind::RepeatedOption { option } => Diagnostic::new(
                Severity::Error,
                "CMD004",
                format!("option `{option}` cannot be repeated for `{command}`"),
            )
            .with_primary(span, "this option is repeated"),
            CommandArgumentFaultKind::ConflictingOptions { option, conflict } => Diagnostic::new(
                Severity::Error,
                "CMD004",
                format!("options `{option}` and `{conflict}` conflict for `{command}`"),
            )
            .with_primary(span, "this option conflicts with an earlier option"),
            CommandArgumentFaultKind::OptionAfterPositional { option } => Diagnostic::new(
                Severity::Error,
                "CMD004",
                format!("option `{option}` must precede `{command}` job arguments"),
            )
            .with_primary(span, "move this option before every positional argument"),
            CommandArgumentFaultKind::UnexpectedKind {
                position,
                expected,
                actual,
            } => Diagnostic::new(
                Severity::Error,
                "CMD005",
                format!(
                    "`{command}` argument {} expects {expected:?}, found {actual:?}",
                    position + 1
                ),
            )
            .with_primary(span, "this argument has the wrong source form"),
            CommandArgumentFaultKind::DynamicTail => Diagnostic::new(
                Severity::Error,
                "CMD006",
                format!("`{command}` does not accept a dynamic argument tail"),
            )
            .with_primary(span, "this spread has runtime-dependent arity"),
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
            ExpressionKind::Variable(_)
            | ExpressionKind::Symbol(_)
            | ExpressionKind::Qualified(_) => {}
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
            ExpressionKind::NominalRecord(record) => {
                for field in &record.fields {
                    self.expression(&field.value);
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
    usage: AnalysisUsage,
}

/// A controlled analysis either exposes one complete report or no partial state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleAnalysisOutcome {
    Complete(Box<ModuleAnalysisReport>),
    Cancelled,
    BudgetExceeded(AnalysisLimitExceeded),
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

    /// Deterministic resource counters consumed by this complete analysis.
    #[must_use]
    pub const fn usage(&self) -> AnalysisUsage {
        self.usage
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

    /// Qualified module aliases and explicit alias re-exports.
    #[must_use]
    pub const fn aliases(&self) -> &ModuleAliasRegistry {
        &self.aliases
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

    /// Resolves a qualified module path and its terminal nominal type name.
    #[must_use]
    pub fn resolve_nominal_type(
        &self,
        module: &ModuleId,
        qualified: &[&str],
    ) -> Option<&NominalType> {
        let (name, modules) = qualified.split_last()?;
        let owner = if modules.is_empty() {
            module
        } else {
            self.aliases.resolve(module, modules)?
        };
        if !modules.is_empty() && self.names.export(owner, name).is_none() {
            return None;
        }
        self.types.nominal(owner, name)
    }

    /// Resolves a qualified compiled operation through local aliases and
    /// explicit alias re-exports.
    #[must_use]
    pub fn resolve_operation(
        &self,
        module: &ModuleId,
        qualified: &[&str],
    ) -> Option<OperationDescriptor> {
        let (name, modules) = qualified.split_last()?;
        if modules.is_empty() {
            return None;
        }
        let owner = self.aliases.resolve(module, modules)?;
        standard_operation(owner, name)
    }

    pub(crate) fn runtime_binding_types(&self) -> RuntimeBindingTypes {
        let mut by_source = BTreeMap::new();
        let mut functions_by_source = BTreeMap::new();
        let mut annotations_by_source = BTreeMap::new();
        let mut modules_by_source = BTreeMap::new();
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
            annotations_by_source.insert(
                entry.source().id(),
                types.map_or_else(Vec::new, |types| types.annotations.clone()),
            );
            modules_by_source.insert(entry.source().id(), entry.module().clone());
        }
        RuntimeBindingTypes {
            by_source,
            functions_by_source,
            annotations_by_source,
            modules_by_source,
            nominals_by_module: self
                .types
                .by_module
                .iter()
                .map(|(module, types)| (module.clone(), types.nominals.clone()))
                .collect(),
            aliases: self.aliases.clone(),
        }
    }
}

/// Recursively resolves, reads, decodes, parses, and graphs Flash modules.
pub struct ModuleProgramLoader<'a> {
    resolver: ModuleResolver<'a>,
    source_loader: &'a dyn ModuleSourceLoader,
}

enum PendingModuleImport {
    Local {
        requested: PathBuf,
        span: Span,
    },
    Standard {
        namespace: String,
        module: String,
        span: Span,
    },
}

impl<'a> ModuleProgramLoader<'a> {
    /// Creates a program loader over injected path and source capabilities.
    #[must_use]
    pub const fn new(
        canonicalizer: &'a dyn ModuleCanonicalizer,
        source_loader: &'a dyn ModuleSourceLoader,
    ) -> Self {
        Self::for_language(canonicalizer, source_loader, LanguageMajor::V1)
    }

    /// Creates a loader that requires every source in the closure to match the
    /// explicitly selected language major.
    #[must_use]
    pub const fn for_language(
        canonicalizer: &'a dyn ModuleCanonicalizer,
        source_loader: &'a dyn ModuleSourceLoader,
        language: LanguageMajor,
    ) -> Self {
        Self {
            resolver: ModuleResolver::for_language(canonicalizer, language),
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
        self.complete_without_cancellation(
            self.analyze_controlled(requested, &AnalysisControl::never()),
        )
    }

    /// Analyzes with cooperative cancellation and never exposes a partial
    /// diagnostic report or program when cancellation wins.
    #[must_use]
    pub fn analyze_controlled(
        &self,
        requested: &Path,
        control: &AnalysisControl,
    ) -> ModuleAnalysisOutcome {
        self.analyze_with_limits_controlled(requested, control, self.default_limits())
    }

    /// Analyzes with explicit deterministic resource ceilings.
    #[must_use]
    pub fn analyze_with_limits(
        &self,
        requested: &Path,
        limits: AnalysisLimits,
    ) -> ModuleAnalysisReport {
        self.complete_without_cancellation(self.analyze_with_limits_controlled(
            requested,
            &AnalysisControl::never(),
            limits,
        ))
    }

    /// Analyzes with cooperative cancellation and explicit resource ceilings.
    #[must_use]
    pub fn analyze_with_limits_controlled(
        &self,
        requested: &Path,
        control: &AnalysisControl,
        limits: AnalysisLimits,
    ) -> ModuleAnalysisOutcome {
        self.analyze_internal(requested, None, control, limits)
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
        self.complete_without_cancellation(self.analyze_with_commands_controlled(
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
        self.analyze_with_commands_and_limits_controlled(
            requested,
            commands,
            control,
            self.default_limits(),
        )
    }

    /// Performs command-aware analysis with explicit deterministic ceilings.
    #[must_use]
    pub fn analyze_with_commands_and_limits(
        &self,
        requested: &Path,
        commands: &CommandRegistry,
        limits: AnalysisLimits,
    ) -> ModuleAnalysisReport {
        self.complete_without_cancellation(self.analyze_with_commands_and_limits_controlled(
            requested,
            commands,
            &AnalysisControl::never(),
            limits,
        ))
    }

    /// Performs command-aware controlled analysis with explicit ceilings.
    #[must_use]
    pub fn analyze_with_commands_and_limits_controlled(
        &self,
        requested: &Path,
        commands: &CommandRegistry,
        control: &AnalysisControl,
        limits: AnalysisLimits,
    ) -> ModuleAnalysisOutcome {
        self.analyze_internal(requested, Some(commands), control, limits)
    }

    fn default_limits(&self) -> AnalysisLimits {
        match self.resolver.language() {
            LanguageMajor::V1 => AnalysisLimits::unlimited(),
            LanguageMajor::V2 => AnalysisLimits::default(),
        }
    }

    fn complete_without_cancellation(
        &self,
        outcome: ModuleAnalysisOutcome,
    ) -> ModuleAnalysisReport {
        match outcome {
            ModuleAnalysisOutcome::Complete(report) => *report,
            ModuleAnalysisOutcome::Cancelled => {
                unreachable!("a never-cancelled analysis control cannot cancel")
            }
            ModuleAnalysisOutcome::BudgetExceeded(exceeded) => ModuleAnalysisReport {
                sources: Vec::new(),
                issues: vec![ModuleAnalysisIssue::new(
                    ModuleProgramError::BudgetExceeded(exceeded),
                )],
                program: None,
                usage: AnalysisUsage::default(),
            },
        }
    }

    fn stopped_outcome(control: &AnalysisControl) -> ModuleAnalysisOutcome {
        control.limit_exceeded().map_or(
            ModuleAnalysisOutcome::Cancelled,
            ModuleAnalysisOutcome::BudgetExceeded,
        )
    }

    fn analyze_internal(
        &self,
        requested: &Path,
        commands: Option<&CommandRegistry>,
        control: &AnalysisControl,
        limits: AnalysisLimits,
    ) -> ModuleAnalysisOutcome {
        let control = control.for_run(limits);
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        let root_result = self.resolver.resolve_root(requested);
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
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
                    usage: control.usage(),
                }));
            }
        };
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
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
            &control,
            1,
        );
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        let mut pipeline_issues = Vec::new();
        if let Some(commands) = commands {
            for entry in &retained {
                if control.is_cancelled() {
                    return Self::stopped_outcome(&control);
                }
                if let Some(script) = entry.script() {
                    pipeline_issues.extend(
                        StaticPipelineAnalyzer::analyze(
                            entry.module(),
                            entry.source(),
                            script,
                            commands,
                            &control,
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
            return Self::stopped_outcome(&control);
        }

        if !control.charge(
            AnalysisLimitKind::Diagnostics,
            analysis_issue_count(&issues),
        ) || !control.charge(
            AnalysisLimitKind::Diagnostics,
            analysis_issue_count(&pipeline_issues),
        ) {
            return Self::stopped_outcome(&control);
        }

        if !issues.is_empty() {
            issues.extend(pipeline_issues);
            return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                sources: retained,
                issues,
                program: None,
                usage: control.usage(),
            }));
        }

        let aliases_result = ModuleAliasRegistry::analyze(&graph, &sources, &control);
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        let aliases = match aliases_result {
            Ok(aliases) => aliases,
            Err(errors) => {
                if !control.charge(AnalysisLimitKind::Diagnostics, errors.len() as u64) {
                    return Self::stopped_outcome(&control);
                }
                let mut issues = errors
                    .into_iter()
                    .map(|error| {
                        ModuleAnalysisIssue::new(ModuleProgramError::Aliases(Box::new(error)))
                    })
                    .collect::<Vec<_>>();
                issues.extend(pipeline_issues);
                return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                    sources: retained,
                    issues,
                    program: None,
                    usage: control.usage(),
                }));
            }
        };
        let names_result = ModuleNameRegistry::analyze(&graph, &sources, &aliases, &control);
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        let names = match names_result {
            Ok(names) => names,
            Err(errors) => {
                if !control.charge(AnalysisLimitKind::Diagnostics, errors.len() as u64) {
                    return Self::stopped_outcome(&control);
                }
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
                    usage: control.usage(),
                }));
            }
        };
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        let types_result = ModuleTypeRegistry::analyze(&sources, &aliases, &names, &control);
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        let types = match types_result {
            Ok(types) => types,
            Err(errors) => {
                if !control.charge(AnalysisLimitKind::Diagnostics, errors.len() as u64) {
                    return Self::stopped_outcome(&control);
                }
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
                    usage: control.usage(),
                }));
            }
        };
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        if pipeline_issues
            .iter()
            .any(|issue| issue.severity() == Severity::Error)
        {
            return ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
                sources: retained,
                issues: pipeline_issues,
                program: None,
                usage: control.usage(),
            }));
        }
        let effect_commands = commands.cloned().unwrap_or_else(standard_registry);
        let effects = ModuleEffectRegistry::analyze(
            &graph,
            &sources,
            &aliases,
            &names,
            &effect_commands,
            &control,
        );
        if control.is_cancelled() {
            return Self::stopped_outcome(&control);
        }
        ModuleAnalysisOutcome::Complete(Box::new(ModuleAnalysisReport {
            sources: retained,
            issues: pipeline_issues,
            program: Some(ModuleProgram {
                graph,
                sources,
                aliases,
                names,
                types,
                effects,
            }),
            usage: control.usage(),
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
        depth: u64,
    ) {
        if control.is_cancelled() {
            return;
        }
        if !attempted.insert(module.clone()) {
            return;
        }
        if !control.observe(AnalysisLimitKind::Modules, graph.modules().len() as u64)
            || !control.observe(AnalysisLimitKind::ModuleDepth, depth)
        {
            return;
        }

        if control.is_cancelled() {
            return;
        }
        let remaining = control
            .remaining(AnalysisLimitKind::SourceBytes)
            .and_then(|remaining| usize::try_from(remaining).ok())
            .unwrap_or(usize::MAX);
        let read_limit = remaining.saturating_add(1);
        let bytes = if let Some(source) = standard_module_source(&module) {
            source.as_bytes()[..source.len().min(read_limit)].to_vec()
        } else {
            match self.source_loader.load_bounded(&module, read_limit) {
                Ok(bytes) => bytes,
                Err(cause) => {
                    issues.push(ModuleAnalysisIssue::new(ModuleProgramError::SourceRead {
                        module: module.clone(),
                        imported_by: imported_by.map(Box::new),
                        cause,
                    }));
                    return;
                }
            }
        };
        if !control.charge(AnalysisLimitKind::SourceBytes, bytes.len() as u64) {
            return;
        }
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
        let script = match parse_source_for_language(&source, self.resolver.language(), &|| {
            control.is_cancelled()
        }) {
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
        let metrics = SyntaxMetrics::for_script(&script);
        if !control.charge(AnalysisLimitKind::AstNodes, metrics.nodes)
            || !control.observe(AnalysisLimitKind::TypeDepth, metrics.type_depth)
        {
            return;
        }

        let imports = script
            .statements()
            .iter()
            .filter_map(|statement| match statement.kind() {
                StatementKind::Import(import) => Some(PendingModuleImport::Local {
                    requested: {
                        let quoted = source
                            .slice(import.path)
                            .expect("parsed import spans belong to their source");
                        PathBuf::from(&quoted[1..quoted.len() - 1])
                    },
                    span: import.path,
                }),
                StatementKind::ModuleImport(import) => match import.source {
                    flash_syntax::ModuleImportSource::Local { path } => {
                        let quoted = source
                            .slice(path)
                            .expect("parsed import spans belong to their source");
                        Some(PendingModuleImport::Local {
                            requested: PathBuf::from(&quoted[1..quoted.len() - 1]),
                            span: path,
                        })
                    }
                    flash_syntax::ModuleImportSource::Standard {
                        namespace,
                        module: standard,
                        span,
                    } => Some(PendingModuleImport::Standard {
                        namespace: source
                            .slice(namespace.span())
                            .expect("standard namespace belongs to its source")
                            .to_owned(),
                        module: source
                            .slice(standard.span())
                            .expect("standard module belongs to its source")
                            .to_owned(),
                        span,
                    }),
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        sources.register(module.clone(), source.clone(), script.clone());
        retained.push(ModuleAnalysisSource {
            module: module.clone(),
            source,
            script: Some(script),
        });

        for pending in imports {
            if control.is_cancelled() {
                return;
            }
            let (import, recurse) = match pending {
                PendingModuleImport::Local { requested, span } => {
                    let import = match self.resolver.resolve_import(&module, &requested, span) {
                        Ok(import) => import,
                        Err(error) => {
                            issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Resolution(
                                error,
                            )));
                            continue;
                        }
                    };
                    (import, true)
                }
                PendingModuleImport::Standard {
                    namespace,
                    module: standard,
                    span,
                } => {
                    if !is_standard_module(&namespace, &standard, self.resolver.language()) {
                        issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Aliases(
                            Box::new(ModuleAliasError::UnknownStandard {
                                module: module.clone(),
                                name: standard,
                                span,
                            }),
                        )));
                        continue;
                    }
                    let target =
                        ModuleId::standard(&namespace, &standard, self.resolver.language());
                    let recurse = standard_module_source(&target).is_some();
                    (
                        ModuleImport {
                            importer: module.clone(),
                            requested: PathBuf::from(format!("{namespace}::{standard}")),
                            target,
                            span,
                        },
                        recurse,
                    )
                }
            };
            if let Err(error) = graph.add_import(import.clone()) {
                issues.push(ModuleAnalysisIssue::new(ModuleProgramError::Graph(error)));
                continue;
            }
            if !control.observe(AnalysisLimitKind::Modules, graph.modules().len() as u64) {
                return;
            }
            if !control.observe(AnalysisLimitKind::ModuleDepth, depth.saturating_add(1)) {
                return;
            }
            if recurse {
                self.discover_module(
                    import.target().clone(),
                    Some(import),
                    graph,
                    sources,
                    retained,
                    attempted,
                    issues,
                    control,
                    depth.saturating_add(1),
                );
            }
        }
    }
}

const STANDARD_OUTCOME_MODULE: &str = r#"language 2

export { Result, Option }

enum Result[T, E] {
    Ok(T),
    Err(E),
}

enum Option[T] {
    Some(T),
    None,
}
"#;

fn standard_module_source(module: &ModuleId) -> Option<&'static str> {
    (module.language() == LanguageMajor::V2
        && matches!(
            module.origin(),
            ModuleOrigin::Standard {
                namespace,
                module,
            } if namespace == "std" && module == "outcome"
        ))
    .then_some(STANDARD_OUTCOME_MODULE)
}

fn is_standard_module(namespace: &str, module: &str, language: LanguageMajor) -> bool {
    namespace == "std"
        && (module == "value" || (module == "outcome" && language == LanguageMajor::V2))
}

fn parse_source_for_language(
    source: &SourceFile,
    language: LanguageMajor,
    is_cancelled: &dyn Fn() -> bool,
) -> ControlledParseOutcome {
    match language {
        LanguageMajor::V1 => parse_with_control(source, is_cancelled),
        LanguageMajor::V2 => match parse_v2_with_control(source, is_cancelled) {
            ControlledVersionedParseOutcome::Cancelled => ControlledParseOutcome::Cancelled,
            ControlledVersionedParseOutcome::Parsed(VersionedParseOutcome::Complete(script)) => {
                ControlledParseOutcome::Parsed(ParseOutcome::Complete(script.into_script()))
            }
            ControlledVersionedParseOutcome::Parsed(VersionedParseOutcome::Incomplete(input)) => {
                ControlledParseOutcome::Parsed(ParseOutcome::Incomplete(input))
            }
            ControlledVersionedParseOutcome::Parsed(VersionedParseOutcome::Invalid(
                diagnostics,
            )) => ControlledParseOutcome::Parsed(ParseOutcome::Invalid(diagnostics)),
        },
    }
}

fn analysis_issue_count(issues: &[ModuleAnalysisIssue]) -> u64 {
    issues
        .iter()
        .map(|issue| issue.error().diagnostics().len().max(1) as u64)
        .sum()
}

/// Deterministic syntax measurements charged before semantic analysis begins.
///
/// `nodes` counts structural AST owners (statements, expressions, patterns,
/// type references, blocks/chains/pipelines/stages, command items, word parts,
/// and redirections). Identifiers and spans are payload, not separate nodes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SyntaxMetrics {
    nodes: u64,
    type_depth: u64,
}

impl SyntaxMetrics {
    fn for_script(script: &Script) -> Self {
        let mut metrics = Self {
            nodes: 1,
            type_depth: 0,
        };
        metrics.statements(script.statements());
        metrics
    }

    fn statements(&mut self, statements: &[Statement]) {
        for statement in statements {
            self.statement(statement);
        }
    }

    fn statement(&mut self, statement: &Statement) {
        self.nodes += 1;
        match statement.kind() {
            StatementKind::Import(_)
            | StatementKind::ModuleImport(_)
            | StatementKind::ModuleExport(_) => {}
            StatementKind::NominalType(declaration) => {
                for field in &declaration.fields {
                    self.type_reference(&field.value_type, 1);
                }
            }
            StatementKind::VariantType(declaration) => {
                for variant in &declaration.variants {
                    for payload in &variant.payload {
                        self.type_reference(payload, 1);
                    }
                }
            }
            StatementKind::Declaration(declaration) => {
                self.pattern(&declaration.pattern);
                if let Some(annotation) = &declaration.type_annotation {
                    self.type_reference(annotation, 1);
                }
                self.expression(&declaration.value);
            }
            StatementKind::Assignment(assignment) => {
                self.expression(&assignment.value);
            }
            StatementKind::Environment(flash_syntax::EnvironmentStatement::Export {
                value,
                ..
            }) => {
                self.expression(value);
            }
            StatementKind::Environment(flash_syntax::EnvironmentStatement::Unset { .. }) => {}
            StatementKind::Function(function) => {
                for parameter in &function.parameters {
                    self.pattern(&parameter.pattern);
                    if let Some(annotation) = &parameter.type_annotation {
                        self.type_reference(annotation, 1);
                    }
                }
                if let Some(result) = &function.return_type {
                    self.type_reference(result, 1);
                }
                self.block(&function.body);
            }
            StatementKind::If(statement) => {
                self.chain(&statement.condition);
                self.block(&statement.then_block);
                if let Some(branch) = &statement.else_branch {
                    match branch {
                        ElseBranch::Block(block) => self.block(block),
                        ElseBranch::If(statement) => {
                            self.nodes += 1;
                            self.chain(&statement.kind().condition);
                            self.block(&statement.kind().then_block);
                            if let Some(branch) = &statement.kind().else_branch {
                                self.else_branch(branch);
                            }
                        }
                    }
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
                    self.nodes += 1;
                    self.pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.expression(guard);
                    }
                    self.block(&arm.body);
                }
            }
            StatementKind::Try(statement) => {
                self.block(&statement.try_block);
                self.block(&statement.catch_block);
            }
            StatementKind::Throw(expression) => {
                self.expression(expression);
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

    fn else_branch(&mut self, branch: &ElseBranch) {
        match branch {
            ElseBranch::Block(block) => self.block(block),
            ElseBranch::If(statement) => {
                self.nodes += 1;
                self.chain(&statement.kind().condition);
                self.block(&statement.kind().then_block);
                if let Some(branch) = &statement.kind().else_branch {
                    self.else_branch(branch);
                }
            }
        }
    }

    fn block(&mut self, block: &Block) {
        self.nodes += 1;
        self.statements(&block.statements);
    }

    fn type_reference(&mut self, reference: &TypeReference, depth: u64) {
        self.nodes += 1;
        self.type_depth = self.type_depth.max(depth);
        for argument in &reference.arguments {
            self.type_reference(argument, depth.saturating_add(1));
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        self.nodes += 1;
        match pattern {
            Pattern::List(pattern) => {
                for element in &pattern.elements {
                    self.pattern(element);
                }
            }
            Pattern::NominalRecord(pattern) => {
                for field in &pattern.fields {
                    self.pattern(&field.pattern);
                }
            }
            Pattern::Variant(pattern) => {
                for payload in &pattern.payload {
                    self.pattern(payload);
                }
            }
            Pattern::Wildcard(_) | Pattern::Literal(_) | Pattern::Binding(_) => {}
        }
    }

    fn expression(&mut self, expression: &Expression) -> u64 {
        self.nodes += 1;
        match expression.kind() {
            ExpressionKind::Literal(literal) => {
                if let LiteralKind::DoubleQuoted(parts) = literal.kind() {
                    self.word_parts(parts);
                }
                1
            }
            ExpressionKind::List(elements) => {
                let depth = 1_u64.saturating_add(
                    elements
                        .iter()
                        .map(|element| self.expression(element))
                        .max()
                        .unwrap_or(1),
                );
                self.type_depth = self.type_depth.max(depth);
                depth
            }
            ExpressionKind::Record(entries) => {
                for entry in entries {
                    if let RecordKey::DoubleQuoted(part) = &entry.key {
                        self.word_part(part);
                    }
                    self.expression(&entry.value);
                }
                1
            }
            ExpressionKind::NominalRecord(record) => {
                for field in &record.fields {
                    self.expression(&field.value);
                }
                1
            }
            ExpressionKind::Closure(closure) => {
                for parameter in &closure.parameters {
                    self.pattern(&parameter.pattern);
                    if let Some(annotation) = &parameter.type_annotation {
                        self.type_reference(annotation, 1);
                    }
                }
                if let Some(result) = &closure.result_type {
                    self.type_reference(result, 1);
                }
                self.chain(&closure.body);
                1
            }
            ExpressionKind::CommandSubstitution(substitution) => {
                self.chain(substitution.chain());
                1
            }
            ExpressionKind::GroupedJob(chain) => {
                self.chain(chain);
                1
            }
            ExpressionKind::Call(call) => {
                self.expression(&call.callee);
                for argument in &call.type_arguments {
                    self.type_reference(argument, 1);
                }
                for argument in &call.arguments {
                    self.expression(argument);
                }
                1
            }
            ExpressionKind::Index(index) => {
                self.expression(&index.target);
                self.expression(&index.index);
                1
            }
            ExpressionKind::Member(member) => {
                self.expression(&member.target);
                1
            }
            ExpressionKind::Unary(unary) => {
                self.expression(&unary.operand);
                1
            }
            ExpressionKind::Binary(binary) => {
                self.expression(&binary.left);
                self.expression(&binary.right);
                1
            }
            ExpressionKind::Variable(_)
            | ExpressionKind::Symbol(_)
            | ExpressionKind::Qualified(_) => 1,
        }
    }

    fn chain(&mut self, chain: &ConditionalChain) {
        self.nodes += 1;
        for and_chain in chain.or_terms() {
            self.nodes += 1;
            for pipeline in and_chain.and_terms() {
                self.nodes += 1;
                for stage in pipeline.stages() {
                    self.nodes += 1;
                    match stage.kind() {
                        StageKind::Expression(expression) => {
                            self.expression(expression);
                        }
                        StageKind::Command(command) => {
                            self.word(command.head.word());
                            for item in &command.items {
                                self.nodes += 1;
                                match item.kind() {
                                    CommandItemKind::Word(word) => self.word(word),
                                    CommandItemKind::Closure(closure) => {
                                        for parameter in &closure.parameters {
                                            self.pattern(&parameter.pattern);
                                            if let Some(annotation) = &parameter.type_annotation {
                                                self.type_reference(annotation, 1);
                                            }
                                        }
                                        if let Some(result) = &closure.result_type {
                                            self.type_reference(result, 1);
                                        }
                                        self.chain(&closure.body);
                                    }
                                    CommandItemKind::Redirection(redirection) => {
                                        self.redirection(redirection.kind())
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

    fn word(&mut self, word: &Word) {
        self.nodes += 1;
        self.word_parts(word.parts());
    }

    fn word_parts(&mut self, parts: &[WordPart]) {
        for part in parts {
            self.word_part(part);
        }
    }

    fn word_part(&mut self, part: &WordPart) {
        self.nodes += 1;
        match part.kind() {
            WordPartKind::DoubleQuoted(parts) => self.word_parts(parts),
            WordPartKind::BracedInterpolation(expression) => {
                self.expression(expression);
            }
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
        self.nodes += 1;
        match redirection {
            RedirectionKind::Input { target, .. } => self.word(target),
            RedirectionKind::File(file) => self.word(&file.target),
            RedirectionKind::Duplicate { .. } | RedirectionKind::Close { .. } => {}
        }
    }
}

/// A failure in the closed v2 module-alias and re-export namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModuleAliasError {
    UnknownStandard {
        module: ModuleId,
        name: String,
        span: Span,
    },
    Conflict {
        module: ModuleId,
        name: String,
        first_span: Span,
        duplicate_span: Span,
    },
    DuplicateExport {
        module: ModuleId,
        name: String,
        first_span: Span,
        duplicate_span: Span,
    },
}

impl ModuleAliasError {
    #[must_use]
    pub const fn module(&self) -> &ModuleId {
        match self {
            Self::UnknownStandard { module, .. }
            | Self::Conflict { module, .. }
            | Self::DuplicateExport { module, .. } => module,
        }
    }

    #[must_use]
    pub fn diagnostic(&self) -> Diagnostic {
        match self {
            Self::UnknownStandard { span, .. } => {
                Diagnostic::new(Severity::Error, "MOD010", self.to_string()).with_primary(
                    *span,
                    "this standard module is not in the compiled manifest",
                )
            }
            Self::Conflict {
                first_span,
                duplicate_span,
                ..
            } => Diagnostic::new(Severity::Error, "MOD011", self.to_string())
                .with_primary(*duplicate_span, "this alias conflicts")
                .with_secondary(*first_span, "the name is first declared here"),
            Self::DuplicateExport {
                first_span,
                duplicate_span,
                ..
            } => Diagnostic::new(Severity::Error, "MOD012", self.to_string())
                .with_primary(*duplicate_span, "this alias is exported again")
                .with_secondary(*first_span, "the alias is first exported here"),
        }
    }
}

impl fmt::Display for ModuleAliasError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStandard { name, .. } => {
                write!(formatter, "unknown standard module `std::{name}`")
            }
            Self::Conflict { name, .. } => write!(formatter, "module alias `{name}` conflicts"),
            Self::DuplicateExport { name, .. } => {
                write!(
                    formatter,
                    "module alias `{name}` is exported more than once"
                )
            }
        }
    }
}

impl std::error::Error for ModuleAliasError {}

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
    /// An assignment targets a read-only lexical or dynamic binding.
    ImmutableAssignment {
        module: ModuleId,
        name: String,
        assignment_span: Span,
        declaration_span: Option<Span>,
    },
    /// An assignment targets an imported snapshot binding.
    ImportedAssignment {
        module: ModuleId,
        name: String,
        assignment_span: Span,
        import_span: Span,
    },
    /// An assignment crosses a callable's immutable by-value capture boundary.
    CapturedAssignment {
        module: ModuleId,
        name: String,
        assignment_span: Span,
        declaration_span: Span,
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
            | Self::DuplicateBinding { module, .. }
            | Self::ImmutableAssignment { module, .. }
            | Self::ImportedAssignment { module, .. }
            | Self::CapturedAssignment { module, .. } => module,
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
            Self::ImmutableAssignment {
                assignment_span,
                declaration_span,
                ..
            } => match declaration_span {
                Some(declaration_span) => {
                    Diagnostic::new(Severity::Error, "BND001", self.to_string())
                        .with_primary(
                            *assignment_span,
                            "this assignment targets a read-only binding",
                        )
                        .with_secondary(*declaration_span, "the immutable binding is declared here")
                }
                None => Diagnostic::new(Severity::Error, "BND001", self.to_string()).with_primary(
                    *assignment_span,
                    "this assignment targets a read-only binding",
                ),
            },
            Self::ImportedAssignment {
                assignment_span,
                import_span,
                ..
            } => Diagnostic::new(Severity::Error, "BND002", self.to_string())
                .with_primary(*assignment_span, "an imported snapshot cannot be assigned")
                .with_secondary(*import_span, "the snapshot is imported here"),
            Self::CapturedAssignment {
                assignment_span,
                declaration_span,
                ..
            } => Diagnostic::new(Severity::Error, "BND003", self.to_string())
                .with_primary(
                    *assignment_span,
                    "callables capture outer bindings as immutable values",
                )
                .with_secondary(*declaration_span, "the captured binding is declared here"),
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
            Self::ImmutableAssignment { module, name, .. } => write!(
                formatter,
                "module `{}` assigns to immutable binding `{name}`",
                module.path().display()
            ),
            Self::ImportedAssignment { module, name, .. } => write!(
                formatter,
                "module `{}` assigns to imported snapshot `{name}`",
                module.path().display()
            ),
            Self::CapturedAssignment { module, name, .. } => write!(
                formatter,
                "module `{}` assigns to captured binding `{name}`",
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
    OperationCallArity {
        module: ModuleId,
        name: String,
        call_span: Span,
        expected: usize,
        actual: usize,
    },
    OperationGenericArity {
        module: ModuleId,
        name: String,
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
    DuplicateNominalField {
        module: ModuleId,
        name: String,
        field: String,
        first_span: Span,
        duplicate_span: Span,
    },
    UnknownNominalField {
        module: ModuleId,
        name: String,
        field: String,
        field_span: Span,
        declaration_span: Span,
    },
    MissingNominalField {
        module: ModuleId,
        name: String,
        field: String,
        construction_span: Span,
        field_span: Span,
    },
    IntrinsicArgumentMismatch {
        module: ModuleId,
        name: &'static str,
        argument_span: Span,
        expected: &'static str,
        actual: ValueType,
    },
    OperationArgumentMismatch {
        module: ModuleId,
        name: String,
        argument_span: Span,
        expected: ValueType,
        actual: ValueType,
    },
    InvalidOperationStage {
        module: ModuleId,
        stage_span: Span,
    },
    UnknownOperation {
        module: ModuleId,
        name: String,
        span: Span,
    },
    AmbiguousOperationGeneric {
        module: ModuleId,
        name: String,
        parameter: String,
        call_span: Span,
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
    SpreadValueMismatch {
        module: ModuleId,
        span: Span,
        actual: ValueType,
    },
    SpreadElementMismatch {
        module: ModuleId,
        span: Span,
        actual: ValueType,
    },
    ThrowMismatch {
        module: ModuleId,
        span: Span,
        actual: ValueType,
    },
    AssignmentMismatch {
        module: ModuleId,
        name: String,
        assignment_span: Span,
        expected: ValueType,
        actual: ValueType,
        declaration_span: Span,
    },
    BindingMismatch {
        module: ModuleId,
        name: String,
        value_span: Span,
        expected: ValueType,
        actual: ValueType,
        annotation_span: Span,
    },
    GenericArity {
        module: ModuleId,
        name: String,
        call_span: Span,
        expected: usize,
        actual: usize,
        declaration_span: Span,
    },
    AmbiguousGeneric {
        module: ModuleId,
        name: String,
        parameter: String,
        call_span: Span,
        parameter_span: Span,
    },
    UnsatisfiedConstraint {
        module: ModuleId,
        name: String,
        parameter: String,
        constraint: TypeConstraint,
        actual: ValueType,
        call_span: Span,
        parameter_span: Span,
    },
    GuardMismatch {
        module: ModuleId,
        guard_span: Span,
        actual: ValueType,
    },
    PatternTypeMismatch {
        module: ModuleId,
        pattern_span: Span,
        value_type: ValueType,
    },
    UnreachableMatchArm {
        module: ModuleId,
        arm_span: Span,
        covering_span: Span,
    },
    NonExhaustiveMatch {
        module: ModuleId,
        match_span: Span,
        nominal: NominalTypeId,
        missing: Vec<String>,
        declaration_span: Span,
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
            | Self::OperationCallArity { module, .. }
            | Self::OperationGenericArity { module, .. }
            | Self::ArgumentMismatch { module, .. }
            | Self::DuplicateNominalField { module, .. }
            | Self::UnknownNominalField { module, .. }
            | Self::MissingNominalField { module, .. }
            | Self::IntrinsicArgumentMismatch { module, .. }
            | Self::OperationArgumentMismatch { module, .. }
            | Self::InvalidOperationStage { module, .. }
            | Self::UnknownOperation { module, .. }
            | Self::AmbiguousOperationGeneric { module, .. }
            | Self::ResultMismatch { module, .. }
            | Self::ByteCaptureInWord { module, .. }
            | Self::SpreadValueMismatch { module, .. }
            | Self::SpreadElementMismatch { module, .. }
            | Self::ThrowMismatch { module, .. }
            | Self::AssignmentMismatch { module, .. }
            | Self::BindingMismatch { module, .. }
            | Self::GenericArity { module, .. }
            | Self::AmbiguousGeneric { module, .. }
            | Self::UnsatisfiedConstraint { module, .. }
            | Self::GuardMismatch { module, .. }
            | Self::PatternTypeMismatch { module, .. }
            | Self::UnreachableMatchArm { module, .. }
            | Self::NonExhaustiveMatch { module, .. } => module,
        }
    }

    const fn primary_span(&self) -> Span {
        match self {
            Self::UnknownType { span, .. } | Self::InvalidTypeArity { span, .. } => *span,
            Self::CallArity { call_span, .. }
            | Self::IntrinsicCallArity { call_span, .. }
            | Self::OperationCallArity { call_span, .. }
            | Self::OperationGenericArity { call_span, .. } => *call_span,
            Self::ArgumentMismatch { argument_span, .. }
            | Self::IntrinsicArgumentMismatch { argument_span, .. }
            | Self::OperationArgumentMismatch { argument_span, .. } => *argument_span,
            Self::InvalidOperationStage { stage_span, .. } => *stage_span,
            Self::UnknownOperation { span, .. } => *span,
            Self::AmbiguousOperationGeneric { call_span, .. } => *call_span,
            Self::DuplicateNominalField { duplicate_span, .. } => *duplicate_span,
            Self::UnknownNominalField { field_span, .. } => *field_span,
            Self::MissingNominalField {
                construction_span, ..
            } => *construction_span,
            Self::ResultMismatch { result_span, .. } => *result_span,
            Self::ByteCaptureInWord { span, .. }
            | Self::SpreadValueMismatch { span, .. }
            | Self::SpreadElementMismatch { span, .. }
            | Self::ThrowMismatch { span, .. } => *span,
            Self::AssignmentMismatch {
                assignment_span, ..
            } => *assignment_span,
            Self::BindingMismatch { value_span, .. } => *value_span,
            Self::GenericArity { call_span, .. }
            | Self::AmbiguousGeneric { call_span, .. }
            | Self::UnsatisfiedConstraint { call_span, .. } => *call_span,
            Self::GuardMismatch { guard_span, .. } => *guard_span,
            Self::PatternTypeMismatch { pattern_span, .. } => *pattern_span,
            Self::UnreachableMatchArm { arm_span, .. } => *arm_span,
            Self::NonExhaustiveMatch { match_span, .. } => *match_span,
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
            Self::OperationCallArity {
                call_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG003", self.to_string()).with_primary(
                *call_span,
                format!("expected {expected} arguments, found {actual}"),
            ),
            Self::OperationGenericArity {
                call_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG009", self.to_string()).with_primary(
                *call_span,
                format!("expected {expected} type arguments, found {actual}"),
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
            Self::DuplicateNominalField {
                duplicate_span,
                first_span,
                field,
                ..
            } => Diagnostic::new(Severity::Error, "SIG014", self.to_string())
                .with_primary(*duplicate_span, format!("field `{field}` is repeated"))
                .with_secondary(*first_span, "field first supplied here"),
            Self::UnknownNominalField {
                field_span,
                declaration_span,
                field,
                ..
            } => Diagnostic::new(Severity::Error, "SIG015", self.to_string())
                .with_primary(*field_span, format!("unknown field `{field}`"))
                .with_secondary(*declaration_span, "nominal record declared here"),
            Self::MissingNominalField {
                construction_span,
                field_span,
                field,
                ..
            } => Diagnostic::new(Severity::Error, "SIG016", self.to_string())
                .with_primary(
                    *construction_span,
                    format!("required field `{field}` is missing"),
                )
                .with_secondary(*field_span, "field declared here"),
            Self::IntrinsicArgumentMismatch {
                argument_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG004", self.to_string()).with_primary(
                *argument_span,
                format!("this argument is `{actual}`, expected `{expected}`"),
            ),
            Self::OperationArgumentMismatch {
                argument_span,
                expected,
                actual,
                ..
            } => Diagnostic::new(Severity::Error, "SIG004", self.to_string()).with_primary(
                *argument_span,
                format!("this argument is `{actual}`, expected `{expected}`"),
            ),
            Self::InvalidOperationStage { stage_span, .. } => {
                Diagnostic::new(Severity::Error, "OPR001", self.to_string()).with_primary(
                    *stage_span,
                    "a pure value pipeline stage must resolve to a compiled operation",
                )
            }
            Self::UnknownOperation { span, name, .. } => {
                Diagnostic::new(Severity::Error, "OPR002", self.to_string()).with_primary(
                    *span,
                    format!("`{name}` is not exported by this compiled module"),
                )
            }
            Self::AmbiguousOperationGeneric {
                call_span,
                parameter,
                ..
            } => Diagnostic::new(Severity::Error, "SIG010", self.to_string()).with_primary(
                *call_span,
                format!("type parameter `{parameter}` is not uniquely inferred"),
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
            Self::SpreadValueMismatch { span, actual, .. } => {
                Diagnostic::new(Severity::Error, "SIG020", self.to_string()).with_primary(
                    *span,
                    format!("this spread is `{actual}`; expected `List[T]`"),
                )
            }
            Self::SpreadElementMismatch { span, actual, .. } => {
                Diagnostic::new(Severity::Error, "SIG021", self.to_string()).with_primary(
                    *span,
                    format!("list elements are `{actual}` and cannot become command arguments"),
                )
            }
            Self::ThrowMismatch { span, actual, .. } => {
                Diagnostic::new(Severity::Error, "SIG007", self.to_string()).with_primary(
                    *span,
                    format!("this value is `{actual}`; throw requires `String` or `Error`"),
                )
            }
            Self::AssignmentMismatch {
                assignment_span,
                expected,
                actual,
                declaration_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG008", self.to_string())
                .with_primary(
                    *assignment_span,
                    format!("this value is `{actual}`, expected `{expected}`"),
                )
                .with_secondary(*declaration_span, "binding type declared here"),
            Self::BindingMismatch {
                value_span,
                expected,
                actual,
                annotation_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG019", self.to_string())
                .with_primary(
                    *value_span,
                    format!("this value is `{actual}`, expected `{expected}`"),
                )
                .with_secondary(*annotation_span, "binding type declared here"),
            Self::GenericArity {
                call_span,
                expected,
                actual,
                declaration_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG009", self.to_string())
                .with_primary(
                    *call_span,
                    format!("expected {expected} type arguments, found {actual}"),
                )
                .with_secondary(*declaration_span, "generic callable declared here"),
            Self::AmbiguousGeneric {
                call_span,
                parameter,
                parameter_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG010", self.to_string())
                .with_primary(
                    *call_span,
                    format!("type parameter `{parameter}` is not uniquely inferred"),
                )
                .with_secondary(*parameter_span, "type parameter declared here"),
            Self::UnsatisfiedConstraint {
                call_span,
                constraint,
                actual,
                parameter_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG011", self.to_string())
                .with_primary(
                    *call_span,
                    format!("type `{actual}` does not satisfy `{constraint:?}`"),
                )
                .with_secondary(*parameter_span, "constraint declared here"),
            Self::GuardMismatch {
                guard_span, actual, ..
            } => Diagnostic::new(Severity::Error, "SIG012", self.to_string()).with_primary(
                *guard_span,
                format!("match guard is `{actual}`; expected `Bool`"),
            ),
            Self::PatternTypeMismatch {
                pattern_span,
                value_type,
                ..
            } => Diagnostic::new(Severity::Error, "SIG017", self.to_string()).with_primary(
                *pattern_span,
                format!("this pattern cannot match `{value_type}`"),
            ),
            Self::UnreachableMatchArm {
                arm_span,
                covering_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG018", self.to_string())
                .with_primary(*arm_span, "this match arm is unreachable")
                .with_secondary(*covering_span, "an earlier unguarded arm covers it"),
            Self::NonExhaustiveMatch {
                match_span,
                missing,
                declaration_span,
                ..
            } => Diagnostic::new(Severity::Error, "SIG013", self.to_string())
                .with_primary(
                    *match_span,
                    format!("missing unguarded variants: {}", missing.join(", ")),
                )
                .with_secondary(*declaration_span, "closed variant type declared here"),
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
            Self::OperationCallArity {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` calls operation `{name}` with {actual} arguments; expected {expected}",
                module.path().display()
            ),
            Self::OperationGenericArity {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` supplies operation `{name}` {actual} type arguments; expected {expected}",
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
            Self::DuplicateNominalField {
                module,
                name,
                field,
                ..
            } => write!(
                formatter,
                "module `{}` repeats field `{field}` while constructing `{name}`",
                module.path().display()
            ),
            Self::UnknownNominalField {
                module,
                name,
                field,
                ..
            } => write!(
                formatter,
                "module `{}` supplies unknown field `{field}` while constructing `{name}`",
                module.path().display()
            ),
            Self::MissingNominalField {
                module,
                name,
                field,
                ..
            } => write!(
                formatter,
                "module `{}` omits required field `{field}` while constructing `{name}`",
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
            Self::OperationArgumentMismatch {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` passes `{actual}` to operation `{name}`; expected `{expected}`",
                module.path().display()
            ),
            Self::InvalidOperationStage { module, .. } => write!(
                formatter,
                "module `{}` uses a non-operation in a pure value pipeline",
                module.path().display()
            ),
            Self::UnknownOperation { module, name, .. } => write!(
                formatter,
                "module `{}` calls unknown compiled operation `{name}`",
                module.path().display()
            ),
            Self::AmbiguousOperationGeneric {
                module,
                name,
                parameter,
                ..
            } => write!(
                formatter,
                "module `{}` cannot infer operation `{name}` type parameter `{parameter}`",
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
            Self::SpreadValueMismatch { module, actual, .. } => write!(
                formatter,
                "module `{}` spreads `{actual}` as command arguments; expected `List[T]`",
                module.path().display()
            ),
            Self::SpreadElementMismatch { module, actual, .. } => write!(
                formatter,
                "module `{}` spreads `{actual}` list elements that cannot become command arguments",
                module.path().display()
            ),
            Self::ThrowMismatch { module, actual, .. } => write!(
                formatter,
                "module `{}` throws `{actual}`; expected `String` or `Error`",
                module.path().display()
            ),
            Self::AssignmentMismatch {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` assigns `{actual}` to `{name}`; expected `{expected}`",
                module.path().display()
            ),
            Self::BindingMismatch {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` binds `{actual}` to `{name}`; expected `{expected}`",
                module.path().display()
            ),
            Self::GenericArity {
                module,
                name,
                expected,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` calls generic `{name}` with {actual} type arguments; expected {expected}",
                module.path().display()
            ),
            Self::AmbiguousGeneric {
                module,
                name,
                parameter,
                ..
            } => write!(
                formatter,
                "module `{}` cannot infer generic `{name}` parameter `{parameter}` exactly",
                module.path().display()
            ),
            Self::UnsatisfiedConstraint {
                module,
                name,
                parameter,
                constraint,
                actual,
                ..
            } => write!(
                formatter,
                "module `{}` instantiates `{name}` parameter `{parameter}` as `{actual}`, which does not satisfy `{constraint:?}`",
                module.path().display()
            ),
            Self::GuardMismatch { module, actual, .. } => write!(
                formatter,
                "module `{}` uses `{actual}` as a match guard; expected `Bool`",
                module.path().display()
            ),
            Self::PatternTypeMismatch {
                module, value_type, ..
            } => write!(
                formatter,
                "module `{}` uses a pattern that cannot match `{value_type}`",
                module.path().display()
            ),
            Self::UnreachableMatchArm { module, .. } => write!(
                formatter,
                "module `{}` contains an unreachable match arm",
                module.path().display()
            ),
            Self::NonExhaustiveMatch {
                module,
                nominal,
                missing,
                ..
            } => write!(
                formatter,
                "module `{}` has a non-exhaustive match on `{}`; missing {}",
                module.path().display(),
                nominal.name(),
                missing.join(", ")
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
    /// A v2 alias, standard module, or explicit alias re-export was invalid.
    Aliases(Box<ModuleAliasError>),
    /// Static module export/import-name analysis failed.
    Names(Box<ModuleNameError>),
    /// Static type resolution or known-call validation failed.
    Signatures(Box<ModuleTypeError>),
    /// Static command-pipeline carrier analysis failed.
    Pipelines(Box<ModulePipelineError>),
    /// Deterministic Flash 2 analysis resource exhaustion.
    BudgetExceeded(AnalysisLimitExceeded),
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
            Self::Aliases(error) => vec![error.diagnostic()],
            Self::Names(error) => vec![error.diagnostic()],
            Self::Signatures(error) => vec![error.diagnostic()],
            Self::Pipelines(error) => vec![error.diagnostic().clone()],
            Self::Graph(ModuleGraphError::UnknownImporter(_))
            | Self::BudgetExceeded(_)
            | Self::SourceIdentityExhausted => Vec::new(),
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
            Self::Aliases(error) => Some(error.module()),
            Self::Names(error) => Some(error.module()),
            Self::Signatures(error) => Some(error.module()),
            Self::Pipelines(error) => Some(error.module()),
            Self::Resolution(_)
            | Self::Graph(_)
            | Self::BudgetExceeded(_)
            | Self::SourceIdentityExhausted => None,
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
            Self::Aliases(error) => error.fmt(formatter),
            Self::Names(error) => error.fmt(formatter),
            Self::Signatures(error) => error.fmt(formatter),
            Self::Pipelines(error) => error.fmt(formatter),
            Self::BudgetExceeded(error) => error.fmt(formatter),
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
            Self::Aliases(error) => Some(error.as_ref()),
            Self::Names(error) => Some(error),
            Self::Signatures(error) => Some(error),
            Self::Pipelines(error) => Some(error),
            Self::InvalidUtf8 { .. }
            | Self::Syntax { .. }
            | Self::BudgetExceeded(_)
            | Self::SourceIdentityExhausted => None,
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

    /// Every unique canonical module in stable identity order.
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
