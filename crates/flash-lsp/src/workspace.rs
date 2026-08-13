//! Versioned open-document ownership and overlay-first module source access.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flash_runtime::module::{
    ModuleCanonicalizer, ModuleId, ModulePathError, ModuleSourceError, ModuleSourceLoader,
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
        }
    }
}

impl std::error::Error for DocumentError {}

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
        let mut file =
            File::open(module.path()).map_err(|error| ModuleSourceError::new(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| ModuleSourceError::new(error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(ModuleSourceError::new("path is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
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
            },
        );
        Ok(())
    }

    /// Applies one full-text replacement only when its version increases.
    pub fn change(
        &mut self,
        uri: &DocumentUri,
        version: Option<i32>,
        text: String,
    ) -> Result<ChangeOutcome, DocumentError> {
        let document = self.documents.get_mut(uri).ok_or(DocumentError::NotOpen)?;
        let Some(version) = version.filter(|version| *version > document.version) else {
            return Ok(ChangeOutcome::IgnoredInvalidVersion);
        };
        document.version = version;
        document.text = text;
        Ok(ChangeOutcome::Applied)
    }

    /// Removes one document root and its source overlay.
    pub fn close(&mut self, uri: &DocumentUri) -> Result<OpenDocument, DocumentError> {
        let document = self.documents.remove(uri).ok_or(DocumentError::NotOpen)?;
        self.owners.remove(&document.module_path);
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

        let mut changes = Vec::new();
        for (uri, module_path, provisional) in next {
            let document = self
                .documents
                .get_mut(&uri)
                .expect("identity refresh only contains current documents");
            if document.module_path != module_path || document.provisional != provisional {
                changes.push(IdentityChange {
                    uri,
                    previous: std::mem::replace(&mut document.module_path, module_path),
                    current: document.module_path.clone(),
                });
                document.provisional = provisional;
            }
        }
        self.owners = next_owners;
        Ok(changes)
    }

    fn overlay_for_native_path(&self, candidate: &Path) -> Option<&OpenDocument> {
        self.documents
            .values()
            .find(|document| document.native_path == candidate)
    }
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
        if let Some(uri) = self.owners.get(module.path()) {
            let document = self
                .documents
                .get(uri)
                .expect("every overlay owner has an open document");
            return Ok(document.text.as_bytes().to_vec());
        }
        self.host.load(module)
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
