//! Versioned open-document ownership and overlay-first module source access.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use opaal_runtime::builtin::standard_registry;
use opaal_runtime::module::{
    AnalysisControl, ModuleAnalysisOutcome, ModuleAnalysisReport, ModuleCanonicalizer, ModuleId,
    ModulePathError, ModuleProgramLoader, ModuleSourceError, ModuleSourceLoader,
};
use opaal_runtime::project::{
    MAX_PROJECT_DOCUMENT_BYTES, ProjectManifest, analyze_project_source_controlled,
    parse_project_manifest, validate_project_compatibility,
};
use opaal_syntax::{
    Diagnostic, LabelStyle, PositionEncoding, PositionError, Severity, SourceFile, SourceId,
    TextPosition, TextRange,
};

use crate::uri::DocumentUri;

/// One currently open editor document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenDocument {
    uri: DocumentUri,
    native_path: PathBuf,
    module_path: PathBuf,
    version: i32,
    text: String,
    provisional: bool,
    generation: u64,
}

impl OpenDocument {
    /// The exact protocol URI that owns this document.
    #[must_use]
    pub const fn uri(&self) -> &DocumentUri {
        &self.uri
    }

    /// The normalized path decoded from the owning URI.
    #[must_use]
    pub fn native_path(&self) -> &Path {
        &self.native_path
    }

    /// The current canonical or provisional module identity.
    #[must_use]
    pub fn module_path(&self) -> &Path {
        &self.module_path
    }

    /// The exact accepted client version.
    #[must_use]
    pub const fn version(&self) -> i32 {
        self.version
    }

    /// The exact current UTF-8 editor text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the module identity has no current regular-file identity.
    #[must_use]
    pub const fn is_provisional(&self) -> bool {
        self.provisional
    }

    /// The workspace generation that last changed this document.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// The result of applying a full-document change notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeOutcome {
    Applied,
    IgnoredInvalidVersion,
}

/// A document mutation that would violate workspace ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentError {
    AlreadyOpen,
    NotOpen,
    ModuleAlreadyOpen { owner: DocumentUri },
    Host(String),
    GenerationExhausted,
}

impl fmt::Display for DocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyOpen => formatter.write_str("document URI is already open"),
            Self::NotOpen => formatter.write_str("document URI is not open"),
            Self::ModuleAlreadyOpen { owner } => {
                write!(formatter, "canonical module is already owned by {owner}")
            }
            Self::Host(message) => formatter.write_str(message),
            Self::GenerationExhausted => formatter.write_str("workspace generation is exhausted"),
        }
    }
}

/// One immutable analysis root in deterministic canonical-path order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotRoot {
    uri: DocumentUri,
    module_path: PathBuf,
}

impl SnapshotRoot {
    /// The exact open URI that owns this root.
    #[must_use]
    pub const fn uri(&self) -> &DocumentUri {
        &self.uri
    }

    /// The canonical or provisional module identity analyzed for this root.
    #[must_use]
    pub fn module_path(&self) -> &Path {
        &self.module_path
    }
}

/// One source location normalized away from analysis-local source identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticLocation {
    uri: DocumentUri,
    range: TextRange,
}

impl DiagnosticLocation {
    #[must_use]
    pub const fn uri(&self) -> &DocumentUri {
        &self.uri
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }
}

/// One ordered secondary label ready for LSP related information.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticRelatedInformation {
    location: DiagnosticLocation,
    message: String,
}

impl DiagnosticRelatedInformation {
    #[must_use]
    pub const fn uri(&self) -> &DocumentUri {
        self.location.uri()
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.location.range()
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One shared diagnostic projected to source-stable LSP data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceDiagnostic {
    range: TextRange,
    severity: Severity,
    code: Option<String>,
    message: String,
    primary_annotation: Option<String>,
    related_information: Vec<DiagnosticRelatedInformation>,
    notes: Vec<String>,
}

impl WorkspaceDiagnostic {
    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }

    #[must_use]
    pub const fn severity(&self) -> Severity {
        self.severity
    }

    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn primary_annotation(&self) -> Option<&str> {
        self.primary_annotation.as_deref()
    }

    #[must_use]
    pub fn related_information(&self) -> &[DiagnosticRelatedInformation] {
        &self.related_information
    }

    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// One URI's complete diagnostic replacement for a workspace generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticDocument {
    uri: DocumentUri,
    version: Option<i32>,
    diagnostics: Vec<WorkspaceDiagnostic>,
}

impl DiagnosticDocument {
    #[must_use]
    pub const fn uri(&self) -> &DocumentUri {
        &self.uri
    }

    #[must_use]
    pub const fn version(&self) -> Option<i32> {
        self.version
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[WorkspaceDiagnostic] {
        &self.diagnostics
    }
}

/// A complete normalized all-root result for one immutable generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticAnalysis {
    generation: u64,
    documents: Vec<DiagnosticDocument>,
}

impl DiagnosticAnalysis {
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn documents(&self) -> &[DiagnosticDocument] {
        &self.documents
    }
}

/// The atomic diagnostic replacements emitted for one current generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticPublication {
    generation: u64,
    documents: Vec<DiagnosticDocument>,
}

impl DiagnosticPublication {
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn documents(&self) -> &[DiagnosticDocument] {
        &self.documents
    }
}

/// Whether a completed analysis crossed the current-generation barrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticPublishOutcome {
    Published(DiagnosticPublication),
    Stale,
}

/// A shared diagnostic that cannot be projected through its retained sources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticProjectionError {
    MissingPrimaryLabel,
    MissingSource(SourceId),
    InvalidPosition(PositionError),
    InvalidFileUri(String),
}

impl fmt::Display for DiagnosticProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPrimaryLabel => formatter.write_str("diagnostic has no primary label"),
            Self::MissingSource(source) => {
                write!(
                    formatter,
                    "diagnostic source {} is not retained",
                    source.get()
                )
            }
            Self::InvalidPosition(error) => {
                write!(formatter, "invalid diagnostic position: {error}")
            }
            Self::InvalidFileUri(error) => {
                write!(formatter, "cannot encode diagnostic file URI: {error}")
            }
        }
    }
}

impl std::error::Error for DiagnosticProjectionError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedDocument {
    version: Option<i32>,
    diagnostics: Vec<WorkspaceDiagnostic>,
}

impl std::error::Error for DocumentError {}

/// A requested immutable LSP project selection that cannot be established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectSelectionError {
    AlreadySelected,
    WrongManifestName,
    InvalidManifest(String),
    Host(String),
}

impl fmt::Display for ProjectSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadySelected => formatter.write_str("a project is already selected"),
            Self::WrongManifestName => {
                formatter.write_str("project manifest must be named opaal.toml")
            }
            Self::InvalidManifest(message) | Self::Host(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ProjectSelectionError {}

/// One accepted transition from a provisional or stale path identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityChange {
    uri: DocumentUri,
    previous: PathBuf,
    current: PathBuf,
}

impl IdentityChange {
    #[must_use]
    pub const fn uri(&self) -> &DocumentUri {
        &self.uri
    }

    #[must_use]
    pub fn previous(&self) -> &Path {
        &self.previous
    }

    #[must_use]
    pub fn current(&self) -> &Path {
        &self.current
    }
}

/// A read-only regular-file adapter beneath the editor overlay.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostFileSystem;

impl HostFileSystem {
    fn identity_for_open(path: &Path) -> Result<(PathBuf, bool), ModulePathError> {
        match fs::canonicalize(path) {
            Ok(canonical) => ensure_regular_file(&canonical).map(|()| (canonical, false)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                provisional_identity(path).map(|identity| (identity, true))
            }
            Err(error) => Err(ModulePathError::new(error.to_string())),
        }
    }
}

impl ModuleCanonicalizer for HostFileSystem {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let canonical =
            fs::canonicalize(candidate).map_err(|error| ModulePathError::new(error.to_string()))?;
        ensure_regular_file(&canonical)?;
        Ok(canonical)
    }
}

impl ModuleSourceLoader for HostFileSystem {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        let file =
            File::open(module.path()).map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(ModuleSourceError::new("path is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(maximum as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        Ok(bytes)
    }
}

#[derive(Clone, Debug)]
struct SelectedProject {
    manifest: ProjectManifest,
    root_directory: Arc<File>,
}

impl SelectedProject {
    fn load(uri: &DocumentUri) -> Result<Self, ProjectSelectionError> {
        let manifest_path = normalize_absolute(
            &uri.to_file_path()
                .map_err(|error| ProjectSelectionError::Host(error.to_string()))?,
        )
        .map_err(|error| ProjectSelectionError::Host(error.to_string()))?;
        if manifest_path.file_name().and_then(OsStr::to_str) != Some("opaal.toml") {
            return Err(ProjectSelectionError::WrongManifestName);
        }
        let (manifest_parent, manifest_file) = open_absolute_file_nofollow(&manifest_path)?;
        let manifest_bytes = read_regular_bounded(manifest_file, MAX_PROJECT_DOCUMENT_BYTES)?;
        let manifest = parse_project_manifest(&manifest_path, &manifest_bytes)
            .map_err(|error| ProjectSelectionError::InvalidManifest(error.to_string()))?;
        validate_project_compatibility(&manifest)
            .map_err(|error| ProjectSelectionError::InvalidManifest(error.to_string()))?;
        let manifest_directory = manifest_path
            .parent()
            .expect("an absolute manifest path has a parent");
        let root_relative = manifest
            .root()
            .strip_prefix(manifest_directory)
            .map_err(|_| {
                ProjectSelectionError::InvalidManifest(
                    "project root escapes the retained manifest directory".to_owned(),
                )
            })?;
        let root_directory =
            open_relative_directory_nofollow(manifest_parent, root_relative, manifest.root())?;
        Ok(Self {
            manifest,
            root_directory: Arc::new(root_directory),
        })
    }

    fn contains(&self, path: &Path) -> bool {
        normalize_absolute(path).is_ok_and(|path| path.starts_with(self.manifest.root()))
    }

    fn resolve_existing_file(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let candidate = normalize_absolute(candidate)?;
        let relative = candidate
            .strip_prefix(self.manifest.root())
            .map_err(|_| ModulePathError::new("module path escapes the explicit project root"))?;
        open_relative_file_nofollow(&self.root_directory, relative, &candidate)
            .map_err(|error| ModulePathError::new(error.to_string()))?;
        Ok(candidate)
    }

    fn read_existing_bounded(
        &self,
        candidate: &Path,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        let candidate = normalize_absolute(candidate)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let relative = candidate
            .strip_prefix(self.manifest.root())
            .map_err(|_| ModuleSourceError::new("module path escapes the explicit project root"))?;
        let file = open_relative_file_nofollow(&self.root_directory, relative, &candidate)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take(maximum as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        Ok(bytes)
    }
}

/// The sole versioned document table and module source overlay.
#[derive(Debug, Default)]
pub struct Workspace {
    documents: BTreeMap<DocumentUri, OpenDocument>,
    owners: BTreeMap<PathBuf, DocumentUri>,
    host: HostFileSystem,
    project: Option<SelectedProject>,
    generation: u64,
    published: BTreeMap<DocumentUri, PublishedDocument>,
}

impl Workspace {
    /// Creates an empty document workspace over the read-only host adapter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of open document roots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Whether there are no open documents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// The current monotonically increasing workspace generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Selects one immutable, explicit project manifest for this server.
    pub fn select_project(&mut self, uri: &DocumentUri) -> Result<(), ProjectSelectionError> {
        if self.project.is_some() {
            return Err(ProjectSelectionError::AlreadySelected);
        }
        self.project = Some(SelectedProject::load(uri)?);
        Ok(())
    }

    /// Returns the current editor-owned document for `uri`.
    #[must_use]
    pub fn document(&self, uri: &DocumentUri) -> Option<&OpenDocument> {
        self.documents.get(uri)
    }

    /// Opens one independent root after resolving its unique module identity.
    pub fn open(
        &mut self,
        uri: DocumentUri,
        version: i32,
        text: String,
    ) -> Result<(), DocumentError> {
        if self.documents.contains_key(&uri) {
            return Err(DocumentError::AlreadyOpen);
        }
        let native_path = normalize_absolute(
            &uri.to_file_path()
                .map_err(|error| DocumentError::Host(error.to_string()))?,
        )
        .map_err(|error| DocumentError::Host(error.to_string()))?;
        let (module_path, provisional) = HostFileSystem::identity_for_open(&native_path)
            .map_err(|error| DocumentError::Host(error.to_string()))?;
        if let Some(owner) = self.owners.get(&module_path) {
            return Err(DocumentError::ModuleAlreadyOpen {
                owner: owner.clone(),
            });
        }
        let generation = self.next_generation(1)?;

        self.owners.insert(module_path.clone(), uri.clone());
        self.documents.insert(
            uri.clone(),
            OpenDocument {
                uri,
                native_path,
                module_path,
                version,
                text,
                provisional,
                generation,
            },
        );
        self.generation = generation;
        Ok(())
    }

    /// Applies one full-text replacement only when its version increases.
    pub fn change(
        &mut self,
        uri: &DocumentUri,
        version: Option<i32>,
        text: String,
    ) -> Result<ChangeOutcome, DocumentError> {
        let current_version = self
            .documents
            .get(uri)
            .ok_or(DocumentError::NotOpen)?
            .version;
        let Some(version) = version.filter(|version| *version > current_version) else {
            return Ok(ChangeOutcome::IgnoredInvalidVersion);
        };
        let generation = self.next_generation(1)?;
        let document = self
            .documents
            .get_mut(uri)
            .expect("the validated document remains open");
        document.version = version;
        document.text = text;
        document.generation = generation;
        self.generation = generation;
        Ok(ChangeOutcome::Applied)
    }

    /// Removes one document root and its source overlay.
    pub fn close(&mut self, uri: &DocumentUri) -> Result<OpenDocument, DocumentError> {
        if !self.documents.contains_key(uri) {
            return Err(DocumentError::NotOpen);
        }
        let generation = self.next_generation(1)?;
        let document = self.documents.remove(uri).ok_or(DocumentError::NotOpen)?;
        self.owners.remove(&document.module_path);
        self.generation = generation;
        Ok(document)
    }

    /// Re-resolves every open path and atomically applies identity transitions.
    pub fn refresh_identities(&mut self) -> Result<Vec<IdentityChange>, DocumentError> {
        let mut next = Vec::with_capacity(self.documents.len());
        let mut next_owners = BTreeMap::new();
        for (uri, document) in &self.documents {
            let (module_path, provisional) =
                HostFileSystem::identity_for_open(&document.native_path)
                    .map_err(|error| DocumentError::Host(error.to_string()))?;
            if let Some(owner) = next_owners.insert(module_path.clone(), uri.clone()) {
                return Err(DocumentError::ModuleAlreadyOpen { owner });
            }
            next.push((uri.clone(), module_path, provisional));
        }

        let change_count = next
            .iter()
            .filter(|(uri, module_path, provisional)| {
                let document = self
                    .documents
                    .get(uri)
                    .expect("identity refresh only contains current documents");
                document.module_path != *module_path || document.provisional != *provisional
            })
            .count();
        let final_generation = self.next_generation(
            u64::try_from(change_count).map_err(|_| DocumentError::GenerationExhausted)?,
        )?;

        let mut changes = Vec::new();
        let mut transition_generation = self.generation;
        for (uri, module_path, provisional) in next {
            let document = self
                .documents
                .get_mut(&uri)
                .expect("identity refresh only contains current documents");
            if document.module_path != module_path || document.provisional != provisional {
                transition_generation += 1;
                changes.push(IdentityChange {
                    uri,
                    previous: std::mem::replace(&mut document.module_path, module_path),
                    current: document.module_path.clone(),
                });
                document.provisional = provisional;
                document.generation = transition_generation;
            }
        }
        self.owners = next_owners;
        self.generation = final_generation;
        Ok(changes)
    }

    /// Clones all source and document state required for one immutable analysis.
    #[must_use]
    pub fn diagnostic_snapshot(&self) -> WorkspaceSnapshot {
        let mut roots = self
            .documents
            .values()
            .map(|document| SnapshotRoot {
                uri: document.uri.clone(),
                module_path: document.module_path.clone(),
            })
            .collect::<Vec<_>>();
        roots.sort_by(|left, right| {
            left.module_path
                .cmp(&right.module_path)
                .then_with(|| left.uri.cmp(&right.uri))
        });
        WorkspaceSnapshot {
            generation: self.generation,
            documents: self.documents.clone(),
            owners: self.owners.clone(),
            roots,
            host: self.host,
            project: self.project.clone(),
        }
    }

    /// Atomically accepts one complete current-generation diagnostic result.
    #[must_use]
    pub fn publish_diagnostics(
        &mut self,
        analysis: DiagnosticAnalysis,
    ) -> DiagnosticPublishOutcome {
        if analysis.generation != self.generation {
            return DiagnosticPublishOutcome::Stale;
        }

        let next = analysis
            .documents
            .into_iter()
            .map(|document| {
                (
                    document.uri,
                    PublishedDocument {
                        version: document.version,
                        diagnostics: document.diagnostics,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let uris = self
            .published
            .keys()
            .chain(next.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut documents = Vec::new();
        for uri in uris {
            match next.get(&uri) {
                Some(current) if self.published.get(&uri) != Some(current) => {
                    documents.push(DiagnosticDocument {
                        uri,
                        version: current.version,
                        diagnostics: current.diagnostics.clone(),
                    });
                }
                None if self.published.contains_key(&uri) => {
                    documents.push(DiagnosticDocument {
                        version: self.documents.get(&uri).map(OpenDocument::version),
                        uri,
                        diagnostics: Vec::new(),
                    });
                }
                Some(_) | None => {}
            }
        }
        self.published = next;
        DiagnosticPublishOutcome::Published(DiagnosticPublication {
            generation: self.generation,
            documents,
        })
    }

    fn overlay_for_native_path(&self, candidate: &Path) -> Option<&OpenDocument> {
        self.documents
            .values()
            .find(|document| document.native_path == candidate)
    }

    fn next_generation(&self, amount: u64) -> Result<u64, DocumentError> {
        self.generation
            .checked_add(amount)
            .ok_or(DocumentError::GenerationExhausted)
    }
}

/// Immutable open-document and source state for one workspace generation.
#[derive(Clone, Debug)]
pub struct WorkspaceSnapshot {
    generation: u64,
    documents: BTreeMap<DocumentUri, OpenDocument>,
    owners: BTreeMap<PathBuf, DocumentUri>,
    roots: Vec<SnapshotRoot>,
    host: HostFileSystem,
    project: Option<SelectedProject>,
}

impl WorkspaceSnapshot {
    /// The exact workspace generation captured by this snapshot.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Every open root in deterministic canonical-module order.
    #[must_use]
    pub fn roots(&self) -> &[SnapshotRoot] {
        &self.roots
    }

    /// Returns one document as it existed in this generation.
    #[must_use]
    pub fn document(&self, uri: &DocumentUri) -> Option<&OpenDocument> {
        self.documents.get(uri)
    }

    /// Runs complete command-aware analysis for every immutable root and
    /// normalizes all shared issues without retaining analysis-local source IDs.
    pub fn analyze_diagnostics(
        &self,
        encoding: PositionEncoding,
    ) -> Result<DiagnosticAnalysis, DiagnosticProjectionError> {
        self.analyze_diagnostics_controlled(encoding, &AnalysisControl::never())
            .map(|analysis| {
                let mut analysis = analysis.expect("never-cancelled analysis must complete");
                analysis
                    .documents
                    .retain(|document| !document.diagnostics.is_empty());
                analysis
            })
    }

    /// Runs complete diagnostics with cooperative worker cancellation.
    pub(crate) fn analyze_diagnostics_controlled(
        &self,
        encoding: PositionEncoding,
        control: &AnalysisControl,
    ) -> Result<Option<DiagnosticAnalysis>, DiagnosticProjectionError> {
        let commands = standard_registry();
        let mut by_uri = BTreeMap::<DocumentUri, Vec<WorkspaceDiagnostic>>::new();
        let mut project_documents = BTreeSet::new();

        if let Some(project) = &self.project {
            let sources = ProjectWorkspaceSources {
                snapshot: self,
                project,
            };
            let mut project_roots = vec![SnapshotRoot {
                uri: DocumentUri::from_absolute_path(project.manifest.root_module()).map_err(
                    |error| DiagnosticProjectionError::InvalidFileUri(error.to_string()),
                )?,
                module_path: project.manifest.root_module().to_path_buf(),
            }];
            for document in self
                .documents
                .values()
                .filter(|document| project.contains(document.native_path()))
            {
                project_documents.insert(document.uri().clone());
                project_roots.push(SnapshotRoot {
                    uri: document.uri().clone(),
                    module_path: document.native_path().to_path_buf(),
                });
            }
            project_roots.sort_by(|left, right| {
                left.module_path
                    .cmp(&right.module_path)
                    .then_with(|| left.uri.cmp(&right.uri))
            });
            project_roots.dedup_by(|left, right| left.module_path == right.module_path);

            let mut project_modules = BTreeSet::new();
            for root in project_roots {
                if project_modules.contains(root.module_path()) {
                    by_uri.entry(root.uri().clone()).or_default();
                    continue;
                }
                match analyze_project_source_controlled(
                    &project.manifest,
                    root.module_path(),
                    &sources,
                    &sources,
                    control,
                ) {
                    ModuleAnalysisOutcome::Complete(report) => {
                        project_modules.extend(
                            report
                                .sources()
                                .iter()
                                .filter(|entry| {
                                    matches!(
                                        entry.module().origin(),
                                        opaal_runtime::module::ModuleOrigin::Local
                                    )
                                })
                                .map(|entry| entry.module().path().to_path_buf()),
                        );
                        for document in self
                            .documents
                            .values()
                            .filter(|document| project_modules.contains(document.native_path()))
                        {
                            by_uri.entry(document.uri().clone()).or_default();
                        }
                        self.normalize_report(&root, &report, encoding, &mut by_uri)?;
                    }
                    ModuleAnalysisOutcome::Cancelled => return Ok(None),
                    ModuleAnalysisOutcome::BudgetExceeded(exceeded) => {
                        by_uri
                            .entry(root.uri().clone())
                            .or_default()
                            .push(WorkspaceDiagnostic {
                                range: zero_range(),
                                severity: Severity::Error,
                                code: Some("ANL001".to_owned()),
                                message: exceeded.to_string(),
                                primary_annotation: None,
                                related_information: Vec::new(),
                                notes: vec![
                                    "analysis produced no partial executable program".to_owned(),
                                ],
                            });
                    }
                }
            }
        }

        let loader = ModuleProgramLoader::new(self, self);
        for root in &self.roots {
            if project_documents.contains(root.uri()) {
                continue;
            }
            by_uri.entry(root.uri.clone()).or_default();
            let report = match loader.analyze_with_commands_controlled(
                root.module_path(),
                &commands,
                control,
            ) {
                ModuleAnalysisOutcome::Complete(report) => report,
                ModuleAnalysisOutcome::Cancelled => return Ok(None),
                ModuleAnalysisOutcome::BudgetExceeded(exceeded) => {
                    by_uri
                        .entry(root.uri.clone())
                        .or_default()
                        .push(WorkspaceDiagnostic {
                            range: zero_range(),
                            severity: Severity::Error,
                            code: Some("ANL001".to_owned()),
                            message: exceeded.to_string(),
                            primary_annotation: None,
                            related_information: Vec::new(),
                            notes: vec![
                                "analysis produced no partial executable program".to_owned(),
                            ],
                        });
                    continue;
                }
            };
            self.normalize_report(root, &report, encoding, &mut by_uri)?;
        }
        let documents = by_uri
            .into_iter()
            .map(|(uri, diagnostics)| DiagnosticDocument {
                version: self.documents.get(&uri).map(OpenDocument::version),
                uri,
                diagnostics,
            })
            .collect();
        Ok(Some(DiagnosticAnalysis {
            generation: self.generation,
            documents,
        }))
    }

    pub(crate) fn analyze_project_controlled(
        &self,
        requested: &Path,
        control: &AnalysisControl,
    ) -> Option<ModuleAnalysisOutcome> {
        let project = self.project.as_ref()?;
        let sources = ProjectWorkspaceSources {
            snapshot: self,
            project,
        };
        Some(analyze_project_source_controlled(
            &project.manifest,
            requested,
            &sources,
            &sources,
            control,
        ))
    }

    pub(crate) fn document_may_belong_to_project(&self, document: &OpenDocument) -> bool {
        self.project
            .as_ref()
            .is_some_and(|project| project.contains(document.native_path()))
    }

    pub(crate) fn project_root_module(&self) -> Option<&Path> {
        self.project
            .as_ref()
            .map(|project| project.manifest.root_module())
    }

    fn normalize_report(
        &self,
        root: &SnapshotRoot,
        report: &ModuleAnalysisReport,
        encoding: PositionEncoding,
        by_uri: &mut BTreeMap<DocumentUri, Vec<WorkspaceDiagnostic>>,
    ) -> Result<(), DiagnosticProjectionError> {
        let sources = report
            .sources()
            .iter()
            .map(|entry| (entry.source().id(), (entry.module(), entry.source())))
            .collect::<BTreeMap<_, _>>();
        for issue in report.issues() {
            let diagnostics = issue.error().diagnostics();
            if diagnostics.is_empty() {
                let diagnostic = WorkspaceDiagnostic {
                    range: zero_range(),
                    severity: issue.severity(),
                    code: None,
                    message: issue.error().to_string(),
                    primary_annotation: None,
                    related_information: Vec::new(),
                    notes: Vec::new(),
                };
                push_unique(by_uri, root.uri.clone(), diagnostic);
                continue;
            }
            for diagnostic in &diagnostics {
                let (uri, normalized) =
                    self.normalize_diagnostic(diagnostic, &sources, encoding)?;
                push_unique(by_uri, uri, normalized);
            }
        }
        Ok(())
    }

    fn normalize_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        sources: &BTreeMap<SourceId, (&ModuleId, &SourceFile)>,
        encoding: PositionEncoding,
    ) -> Result<(DocumentUri, WorkspaceDiagnostic), DiagnosticProjectionError> {
        let primary = diagnostic
            .labels()
            .iter()
            .find(|label| label.style() == LabelStyle::Primary)
            .ok_or(DiagnosticProjectionError::MissingPrimaryLabel)?;
        let primary_location = self.normalize_span(primary.span(), sources, encoding)?;
        let mut related_information = Vec::new();
        for label in diagnostic
            .labels()
            .iter()
            .filter(|label| label.style() == LabelStyle::Secondary)
        {
            related_information.push(DiagnosticRelatedInformation {
                location: self.normalize_span(label.span(), sources, encoding)?,
                message: label.message().to_owned(),
            });
        }
        let uri = primary_location.uri.clone();
        Ok((
            uri,
            WorkspaceDiagnostic {
                range: primary_location.range,
                severity: diagnostic.severity(),
                code: Some(diagnostic.code().to_owned()),
                message: diagnostic.message().to_owned(),
                primary_annotation: Some(primary.message().to_owned()),
                related_information,
                notes: diagnostic.notes().to_vec(),
            },
        ))
    }

    fn normalize_span(
        &self,
        span: opaal_syntax::Span,
        sources: &BTreeMap<SourceId, (&ModuleId, &SourceFile)>,
        encoding: PositionEncoding,
    ) -> Result<DiagnosticLocation, DiagnosticProjectionError> {
        let (module, source) = sources
            .get(&span.source_id())
            .copied()
            .ok_or(DiagnosticProjectionError::MissingSource(span.source_id()))?;
        let uri = self.uri_for_module(module)?;
        let range = source
            .text_range(span, encoding)
            .map_err(DiagnosticProjectionError::InvalidPosition)?;
        Ok(DiagnosticLocation { uri, range })
    }

    pub(crate) fn uri_for_module(
        &self,
        module: &ModuleId,
    ) -> Result<DocumentUri, DiagnosticProjectionError> {
        if let Some(uri) = self.owners.get(module.path()) {
            return Ok(uri.clone());
        }
        DocumentUri::from_absolute_path(module.path())
            .map_err(|error| DiagnosticProjectionError::InvalidFileUri(error.to_string()))
    }

    fn overlay_for_native_path(&self, candidate: &Path) -> Option<&OpenDocument> {
        self.documents
            .values()
            .find(|document| document.native_path == candidate)
    }
}

impl ModuleCanonicalizer for WorkspaceSnapshot {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let normalized = normalize_absolute(candidate)?;
        if let Some(document) = self.overlay_for_native_path(&normalized) {
            return Ok(document.module_path.clone());
        }
        if self.owners.contains_key(&normalized) {
            return Ok(normalized);
        }
        self.host.canonicalize(&normalized)
    }
}

impl ModuleSourceLoader for WorkspaceSnapshot {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        if let Some(uri) = self.owners.get(module.path()) {
            let document = self
                .documents
                .get(uri)
                .expect("every snapshot overlay owner has an open document");
            let bytes = document.text.as_bytes();
            return Ok(bytes[..bytes.len().min(maximum)].to_vec());
        }
        self.host.load_bounded(module, maximum)
    }
}

struct ProjectWorkspaceSources<'a> {
    snapshot: &'a WorkspaceSnapshot,
    project: &'a SelectedProject,
}

impl ModuleCanonicalizer for ProjectWorkspaceSources<'_> {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let candidate = normalize_absolute(candidate)?;
        if let Some(document) = self.snapshot.overlay_for_native_path(&candidate) {
            if !self.project.contains(document.native_path()) {
                return Err(ModulePathError::new(
                    "module path escapes the explicit project root",
                ));
            }
            if !document.is_provisional() {
                self.project.resolve_existing_file(&candidate)?;
            }
            return Ok(candidate);
        }
        self.project.resolve_existing_file(&candidate)
    }
}

impl ModuleSourceLoader for ProjectWorkspaceSources<'_> {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        if let Some(document) = self.snapshot.overlay_for_native_path(module.path()) {
            let bytes = document.text.as_bytes();
            return Ok(bytes[..bytes.len().min(maximum)].to_vec());
        }
        self.project.read_existing_bounded(module.path(), maximum)
    }
}

fn push_unique(
    by_uri: &mut BTreeMap<DocumentUri, Vec<WorkspaceDiagnostic>>,
    uri: DocumentUri,
    diagnostic: WorkspaceDiagnostic,
) {
    let diagnostics = by_uri.entry(uri).or_default();
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}

const fn zero_range() -> TextRange {
    let start = TextPosition::new(0, 0);
    TextRange::new(start, start)
}

impl ModuleCanonicalizer for Workspace {
    fn canonicalize(&self, candidate: &Path) -> Result<PathBuf, ModulePathError> {
        let normalized = normalize_absolute(candidate)?;
        if let Some(document) = self.overlay_for_native_path(&normalized) {
            return Ok(document.module_path.clone());
        }
        if self.owners.contains_key(&normalized) {
            return Ok(normalized);
        }
        self.host.canonicalize(&normalized)
    }
}

impl ModuleSourceLoader for Workspace {
    fn load(&self, module: &ModuleId) -> Result<Vec<u8>, ModuleSourceError> {
        self.load_bounded(module, usize::MAX)
    }

    fn load_bounded(
        &self,
        module: &ModuleId,
        maximum: usize,
    ) -> Result<Vec<u8>, ModuleSourceError> {
        if let Some(uri) = self.owners.get(module.path()) {
            let document = self
                .documents
                .get(uri)
                .expect("every overlay owner has an open document");
            let bytes = document.text.as_bytes();
            return Ok(bytes[..bytes.len().min(maximum)].to_vec());
        }
        self.host.load_bounded(module, maximum)
    }
}

fn read_regular_bounded(file: File, maximum: usize) -> Result<Vec<u8>, ProjectSelectionError> {
    let metadata = file
        .metadata()
        .map_err(|error| ProjectSelectionError::Host(error.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(ProjectSelectionError::Host(
            "project manifest is not a regular file".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(maximum.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| ProjectSelectionError::Host(error.to_string()))?;
    if bytes.len() > maximum {
        return Err(ProjectSelectionError::Host(format!(
            "project manifest exceeds its {maximum}-byte read limit"
        )));
    }
    Ok(bytes)
}

fn open_absolute_file_nofollow(path: &Path) -> Result<(File, File), ProjectSelectionError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProjectSelectionError::Host("manifest path has no parent".to_owned()))?;
    let name = path
        .file_name()
        .ok_or_else(|| ProjectSelectionError::Host("manifest path has no file name".to_owned()))?;
    let directory = open_absolute_directory_nofollow(parent)?;
    let file = open_file_at_nofollow(&directory, name, path)?;
    Ok((directory, file))
}

fn open_absolute_directory_nofollow(path: &Path) -> Result<File, ProjectSelectionError> {
    if !path.is_absolute() {
        return Err(ProjectSelectionError::Host(
            "project directory path is not absolute".to_owned(),
        ));
    }
    let descriptor = rustix::fs::open(
        Path::new("/"),
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, Path::new("/")))?;
    let mut directory = File::from(descriptor);
    let mut traversed = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                traversed.push(name);
                directory = open_directory_at_nofollow(&directory, name, &traversed)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(ProjectSelectionError::Host(
                    "project directory path is not lexically normalized".to_owned(),
                ));
            }
        }
    }
    Ok(directory)
}

fn open_relative_directory_nofollow(
    mut directory: File,
    relative: &Path,
    display_path: &Path,
) -> Result<File, ProjectSelectionError> {
    let mut traversed = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                traversed.push(name);
                directory = open_directory_at_nofollow(&directory, name, &traversed)?;
            }
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(ProjectSelectionError::Host(format!(
                    "{} is not relative to the manifest",
                    display_path.display()
                )));
            }
        }
    }
    Ok(directory)
}

fn open_relative_file_nofollow(
    root: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<File, ProjectSelectionError> {
    let components = relative.components().collect::<Vec<_>>();
    let Some((final_component, parent_components)) = components.split_last() else {
        return Err(ProjectSelectionError::Host(
            "project path is not a regular file".to_owned(),
        ));
    };
    let Component::Normal(file_name) = final_component else {
        return Err(ProjectSelectionError::Host(
            "project file path is not lexically normalized".to_owned(),
        ));
    };
    let mut directory = None;
    let mut traversed = PathBuf::new();
    for component in parent_components {
        let Component::Normal(name) = component else {
            return Err(ProjectSelectionError::Host(
                "project file path is not lexically normalized".to_owned(),
            ));
        };
        traversed.push(name);
        let parent = directory.as_ref().unwrap_or(root);
        directory = Some(open_directory_at_nofollow(parent, name, &traversed)?);
    }
    open_file_at_nofollow(directory.as_ref().unwrap_or(root), file_name, display_path)
}

fn open_directory_at_nofollow(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<File, ProjectSelectionError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, display_path))?;
    let directory = File::from(descriptor);
    if !directory
        .metadata()
        .map_err(|error| ProjectSelectionError::Host(error.to_string()))?
        .is_dir()
    {
        return Err(ProjectSelectionError::Host(format!(
            "{} is not a directory",
            display_path.display()
        )));
    }
    Ok(directory)
}

fn open_file_at_nofollow(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<File, ProjectSelectionError> {
    let descriptor = rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| project_open_error(error, display_path))?;
    let file = File::from(descriptor);
    if !file
        .metadata()
        .map_err(|error| ProjectSelectionError::Host(error.to_string()))?
        .is_file()
    {
        return Err(ProjectSelectionError::Host(format!(
            "{} is not a regular file",
            display_path.display()
        )));
    }
    Ok(file)
}

fn project_open_error(error: rustix::io::Errno, path: &Path) -> ProjectSelectionError {
    if error == rustix::io::Errno::LOOP {
        ProjectSelectionError::Host(format!(
            "project path contains symbolic link `{}`",
            path.display()
        ))
    } else {
        ProjectSelectionError::Host(format!("cannot open `{}`: {error}", path.display()))
    }
}

fn ensure_regular_file(path: &Path) -> Result<(), ModulePathError> {
    let metadata = fs::metadata(path).map_err(|error| ModulePathError::new(error.to_string()))?;
    if !metadata.file_type().is_file() {
        return Err(ModulePathError::new("path is not a regular file"));
    }
    Ok(())
}

fn normalize_absolute(path: &Path) -> Result<PathBuf, ModulePathError> {
    if !path.is_absolute() {
        return Err(ModulePathError::new("path is not absolute"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    Ok(normalized)
}

fn provisional_identity(path: &Path) -> Result<PathBuf, ModulePathError> {
    let normalized = normalize_absolute(path)?;
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                if !missing.is_empty() {
                    let metadata = fs::metadata(&canonical)
                        .map_err(|error| ModulePathError::new(error.to_string()))?;
                    if !metadata.file_type().is_dir() {
                        return Err(ModulePathError::new(
                            "provisional file path has a non-directory ancestor",
                        ));
                    }
                }
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name() else {
                    return Err(ModulePathError::new(error.to_string()));
                };
                missing.push(name.to_os_string());
                let Some(parent) = existing.parent() else {
                    return Err(ModulePathError::new(error.to_string()));
                };
                existing = parent;
            }
            Err(error) => return Err(ModulePathError::new(error.to_string())),
        }
    }
}
