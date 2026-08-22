//! Flash-owned policy and storage boundary for interactive history.

use std::collections::VecDeque;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, Write};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fd_lock::RwLock;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use reedline::{
    FileBackedHistory, History, HistoryItem, HistoryItemId, HistorySessionId, SearchQuery,
};

use crate::editor::EditorError;
use crate::terminal_editor::history::HistoryPersistence;

/// Maximum number of entries retained by the built-in history policy.
pub const DEFAULT_HISTORY_CAPACITY: usize = 1_000;

const HISTORY_DIRECTORY_MODE: u32 = 0o700;
const HISTORY_FILE_MODE: u32 = 0o600;

/// Host path convention used while selecting the history file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryPlatform {
    Linux,
    MacOs,
    FlashOs,
}

impl HistoryPlatform {
    /// Returns the path convention for the current supported host.
    #[must_use]
    pub const fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Linux
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(target_os = "redox")]
        {
            Self::FlashOs
        }
    }
}

/// Environment lookup seam used to prove that disabled history performs no discovery.
pub trait HistoryEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString>;
}

/// Process environment adapter for later interactive CLI startup wiring.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessHistoryEnvironment;

impl HistoryEnvironment for ProcessHistoryEnvironment {
    fn value(&self, name: &OsStr) -> Option<OsString> {
        std::env::var_os(name)
    }
}

/// Selected history behavior after applying the `--no-history` policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistorySelection {
    Disabled,
    Persistent { primary: PathBuf, legacy: PathBuf },
}

/// History initialization or persistence failure.
#[derive(Debug)]
pub struct HistoryError {
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl HistoryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    fn with_source(message: impl Into<String>, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for HistoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Selects disabled or persistent history without touching the filesystem.
pub fn select_history(
    no_history: bool,
    platform: HistoryPlatform,
    environment: &dyn HistoryEnvironment,
) -> Result<HistorySelection, HistoryError> {
    if no_history {
        return Ok(HistorySelection::Disabled);
    }

    let state_root = environment
        .value(OsStr::new("XDG_STATE_HOME"))
        .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            environment
                .value(OsStr::new("HOME"))
                .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
                .map(PathBuf::from)
                .map(|home| match platform {
                    HistoryPlatform::Linux | HistoryPlatform::FlashOs => home.join(".local/state"),
                    HistoryPlatform::MacOs => home.join("Library/Application Support"),
                })
        })
        .ok_or_else(|| {
            HistoryError::new("history path is unavailable: no absolute state root or home")
        })?;

    Ok(HistorySelection::Persistent {
        primary: state_root.join("flash/history"),
        legacy: state_root.join("flashshell/history"),
    })
}

/// History backend exposed without leaking Reedline types through its API.
#[derive(Debug)]
pub struct EditorHistory {
    entries: VecDeque<String>,
    pending: VecDeque<String>,
    capacity: usize,
    file: Option<PathBuf>,
}

impl EditorHistory {
    /// Initializes disabled or persistent history according to the selected policy.
    pub fn open(selection: HistorySelection) -> Result<Self, HistoryError> {
        match selection {
            HistorySelection::Disabled => Ok(Self {
                entries: VecDeque::new(),
                pending: VecDeque::new(),
                capacity: 0,
                file: None,
            }),
            HistorySelection::Persistent { primary, legacy } => {
                let path = match fs::symlink_metadata(&primary) {
                    Ok(_) => primary,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        match fs::symlink_metadata(&legacy) {
                            Ok(_) => legacy,
                            Err(error) if error.kind() == io::ErrorKind::NotFound => primary,
                            Err(error) => {
                                return Err(HistoryError::with_source(
                                    format!(
                                        "cannot initialize history at {}: {error}",
                                        legacy.display()
                                    ),
                                    error,
                                ));
                            }
                        }
                    }
                    Err(error) => {
                        return Err(HistoryError::with_source(
                            format!(
                                "cannot initialize history at {}: {error}",
                                primary.display()
                            ),
                            error,
                        ));
                    }
                };
                prepare_history_path(&path).map_err(|error| {
                    HistoryError::with_source(
                        format!("cannot initialize history at {}: {error}", path.display()),
                        error,
                    )
                })?;
                let mut history = Self {
                    entries: VecDeque::new(),
                    pending: VecDeque::new(),
                    capacity: DEFAULT_HISTORY_CAPACITY,
                    file: Some(path),
                };
                history.sync().map_err(|error| {
                    HistoryError::with_source("cannot load persistent history", error)
                })?;
                Ok(history)
            }
        }
    }

    /// Records one submitted source buffer and synchronizes persistent state.
    pub fn record(&mut self, source: &str) -> Result<bool, HistoryError> {
        if self.capacity == 0
            || source.is_empty()
            || self.entries.back().is_some_and(|entry| entry == source)
        {
            return Ok(false);
        }
        self.pending.push_back(source.to_owned());
        self.sync().map_err(|error| {
            HistoryError::with_source("cannot synchronize persistent history", error)
        })?;
        Ok(true)
    }

    /// Returns all retained source buffers from oldest to newest.
    pub fn entries(&self) -> Result<Vec<String>, HistoryError> {
        Ok(self.entries.iter().cloned().collect())
    }

    /// Returns newest-first entries containing the exact substring.
    pub fn search_substring(&self, needle: &str) -> Result<Vec<String>, HistoryError> {
        Ok(self
            .entries
            .iter()
            .rev()
            .filter(|entry| entry.contains(needle))
            .cloned()
            .collect())
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn into_backend(self) -> Box<dyn History> {
        Box::new(ReedlineHistory::new(self))
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        self.pending.clear();
        let Some(path) = &self.file else {
            return Ok(());
        };
        validate_history_path(path)?;
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        validate_file(&file, path)?;
        let mut lock = RwLock::new(file);
        let mut guard = lock.write()?;
        guard.rewind().and_then(|()| guard.set_len(0))
    }

    fn sync(&mut self) -> io::Result<()> {
        let Some(path) = self.file.clone() else {
            return Ok(());
        };
        validate_history_path(&path)?;
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        validate_file(&file, &path)?;
        let mut lock = RwLock::new(file);
        let mut guard = lock.write()?;
        let mut merged = {
            let reader = BufReader::new(guard.deref());
            reader
                .lines()
                .map(|line| line.and_then(|line| decode_entry(&line)))
                .collect::<io::Result<VecDeque<_>>>()?
        };
        merged.extend(self.pending.iter().cloned());
        let mut deduplicated = VecDeque::with_capacity(merged.len());
        for entry in merged {
            if deduplicated.back() != Some(&entry) {
                deduplicated.push_back(entry);
            }
        }
        let mut merged = deduplicated;
        while merged.len() > self.capacity {
            merged.pop_front();
        }

        guard.rewind()?;
        {
            let mut writer = BufWriter::new(guard.deref_mut());
            for entry in &merged {
                writer.write_all(encode_entry(entry).as_bytes())?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
        }
        let position = guard.stream_position()?;
        guard.set_len(position)?;
        drop(guard);

        self.entries = merged;
        self.pending.clear();
        Ok(())
    }
}

impl HistoryPersistence for EditorHistory {
    fn entries(&mut self) -> Result<Vec<String>, EditorError> {
        EditorHistory::entries(self)
            .map_err(|error| EditorError::with_source("cannot load interactive history", error))
    }

    fn record(&mut self, source: &str) -> Result<(), EditorError> {
        EditorHistory::record(self, source)
            .map(|_| ())
            .map_err(|error| {
                EditorError::with_source("cannot synchronize interactive history", error)
            })
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug)]
struct ReedlineHistory {
    memory: FileBackedHistory,
    storage: EditorHistory,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ReedlineHistory {
    fn new(storage: EditorHistory) -> Self {
        let mut history = Self {
            memory: FileBackedHistory::new(storage.capacity)
                .expect("validated history capacity initializes Reedline memory"),
            storage,
        };
        history.rebuild().expect("loaded history entries are valid");
        history
    }

    fn rebuild(&mut self) -> reedline::Result<()> {
        let mut memory = FileBackedHistory::new(self.storage.capacity)?;
        for entry in &self.storage.entries {
            memory.save(HistoryItem::from_command_line(entry))?;
        }
        self.memory = memory;
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl History for ReedlineHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        let saved = self.memory.save(item.clone())?;
        if saved.id.is_some() {
            self.storage.record(&item.command_line).map_err(|error| {
                reedline::ReedlineError::from(io::Error::other(error.to_string()))
            })?;
            self.rebuild()?;
        }
        Ok(saved)
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.memory.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.memory.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.memory.search(query)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.memory.update(id, updater)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        self.storage
            .clear()
            .map_err(reedline::ReedlineError::from)?;
        self.rebuild()
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.memory.delete(id)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.storage.sync()?;
        self.rebuild()
            .map_err(|error| io::Error::other(error.to_string()))
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.memory.session()
    }
}

fn encode_entry(entry: &str) -> String {
    let mut encoded = String::with_capacity(entry.len());
    for character in entry.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\0' => encoded.push_str("\\0"),
            character => encoded.push(character),
        }
    }
    encoded
}

fn decode_entry(entry: &str) -> io::Result<String> {
    let mut decoded = String::with_capacity(entry.len());
    let mut characters = entry.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => decoded.push('\\'),
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some('0') => decoded.push('\0'),
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid history escape \\{other}"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unterminated history escape",
                ));
            }
        }
    }
    Ok(decoded)
}

fn prepare_history_path(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "history path has no parent"))?;
    match fs::symlink_metadata(parent) {
        Ok(_) => validate_directory(parent)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(HISTORY_DIRECTORY_MODE);
            builder.create(parent)?;
            // Redox currently does not preserve the requested DirBuilder mode
            // for recursively created directories. Finalize the private leaf
            // before creating the history file; existing directories still go
            // through validation above and are never silently repaired.
            fs::set_permissions(parent, fs::Permissions::from_mode(HISTORY_DIRECTORY_MODE))?;
            validate_directory(parent)?;
        }
        Err(error) => return Err(error),
    }

    match fs::symlink_metadata(path) {
        Ok(_) => validate_history_path(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(HISTORY_FILE_MODE)
                .open(path)?;
            validate_file(&file, path)
        }
        Err(error) => Err(error),
    }
}

fn validate_history_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must not be a symlink", path.display()),
        ));
    }
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    validate_file(&file, path)
}

fn validate_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be a nonsymlink directory", path.display()),
        ));
    }
    validate_owner_and_mode(&metadata, path, HISTORY_DIRECTORY_MODE, "directory")
}

fn validate_file(file: &File, path: &Path) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must be a regular file", path.display()),
        ));
    }
    validate_owner_and_mode(&metadata, path, HISTORY_FILE_MODE, "file")
}

fn validate_owner_and_mode(
    metadata: &fs::Metadata,
    path: &Path,
    expected_mode: u32,
    kind: &str,
) -> io::Result<()> {
    let effective_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != effective_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} {kind} must be owned by the effective user",
                path.display()
            ),
        ));
    }
    if metadata.mode() & 0o777 != expected_mode {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} {kind} must have mode {expected_mode:04o}",
                path.display()
            ),
        ));
    }
    Ok(())
}
