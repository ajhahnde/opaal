//! FlashShell-owned policy and storage boundary for interactive history.

use std::collections::VecDeque;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, Write};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use fd_lock::RwLock;
use reedline::{
    FileBackedHistory, History, HistoryItem, HistoryItemId, HistorySessionId, SearchQuery,
};

/// Maximum number of entries retained by the built-in history policy.
pub const DEFAULT_HISTORY_CAPACITY: usize = 1_000;

const HISTORY_DIRECTORY_MODE: u32 = 0o700;
const HISTORY_FILE_MODE: u32 = 0o600;

/// Host path convention used while selecting the history file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryPlatform {
    Linux,
    MacOs,
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
    Persistent(PathBuf),
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
                    HistoryPlatform::Linux => home.join(".local/state"),
                    HistoryPlatform::MacOs => home.join("Library/Application Support"),
                })
        })
        .ok_or_else(|| {
            HistoryError::new("history path is unavailable: no absolute state root or home")
        })?;

    Ok(HistorySelection::Persistent(
        state_root.join("flashshell/history"),
    ))
}

/// History backend exposed without leaking Reedline types through its API.
#[derive(Debug)]
pub struct EditorHistory {
    backend: PrivateHistory,
    capacity: usize,
}

impl EditorHistory {
    /// Initializes disabled or persistent history according to the selected policy.
    pub fn open(selection: HistorySelection) -> Result<Self, HistoryError> {
        match selection {
            HistorySelection::Disabled => Ok(Self {
                backend: PrivateHistory::memory(0)?,
                capacity: 0,
            }),
            HistorySelection::Persistent(path) => {
                prepare_history_path(&path).map_err(|error| {
                    HistoryError::with_source(
                        format!("cannot initialize history at {}: {error}", path.display()),
                        error,
                    )
                })?;
                let mut backend = PrivateHistory::persistent(DEFAULT_HISTORY_CAPACITY, path)
                    .map_err(|error| {
                        HistoryError::with_source("cannot load persistent history", error)
                    })?;
                backend.sync().map_err(|error| {
                    HistoryError::with_source("cannot load persistent history", error)
                })?;
                Ok(Self {
                    backend,
                    capacity: DEFAULT_HISTORY_CAPACITY,
                })
            }
        }
    }

    /// Records one submitted source buffer and synchronizes persistent state.
    pub fn record(&mut self, source: &str) -> Result<bool, HistoryError> {
        let saved = self
            .backend
            .save(HistoryItem::from_command_line(source))
            .map_err(|error| HistoryError::with_source("cannot record history entry", error))?;
        self.backend.sync().map_err(|error| {
            HistoryError::with_source("cannot synchronize persistent history", error)
        })?;
        Ok(saved.id.is_some())
    }

    /// Returns all retained source buffers from oldest to newest.
    pub fn entries(&self) -> Result<Vec<String>, HistoryError> {
        self.backend
            .search(SearchQuery::everything(
                reedline::SearchDirection::Forward,
                None,
            ))
            .map(|items| items.into_iter().map(|item| item.command_line).collect())
            .map_err(|error| HistoryError::with_source("cannot read history entries", error))
    }

    /// Returns newest-first entries containing the exact substring.
    pub fn search_substring(&self, needle: &str) -> Result<Vec<String>, HistoryError> {
        self.backend
            .search(SearchQuery::all_that_contain_rev(needle.to_owned()))
            .map(|items| items.into_iter().map(|item| item.command_line).collect())
            .map_err(|error| HistoryError::with_source("cannot search history entries", error))
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn into_backend(self) -> Box<dyn History> {
        Box::new(self.backend)
    }
}

#[derive(Debug)]
struct PrivateHistory {
    memory: FileBackedHistory,
    entries: VecDeque<String>,
    pending: VecDeque<String>,
    capacity: usize,
    file: Option<PathBuf>,
}

impl PrivateHistory {
    fn memory(capacity: usize) -> Result<Self, HistoryError> {
        let memory = FileBackedHistory::new(capacity)
            .map_err(|error| HistoryError::with_source("cannot initialize history", error))?;
        Ok(Self {
            memory,
            entries: VecDeque::new(),
            pending: VecDeque::new(),
            capacity,
            file: None,
        })
    }

    fn persistent(capacity: usize, file: PathBuf) -> Result<Self, HistoryError> {
        let mut history = Self::memory(capacity)?;
        history.file = Some(file);
        Ok(history)
    }

    fn replace_entries(&mut self, entries: VecDeque<String>) -> io::Result<()> {
        let mut memory = FileBackedHistory::new(self.capacity)
            .map_err(|error| io::Error::other(error.to_string()))?;
        for entry in &entries {
            memory
                .save(HistoryItem::from_command_line(entry))
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        self.memory = memory;
        self.entries = entries;
        self.pending.clear();
        Ok(())
    }
}

impl History for PrivateHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        let command_line = item.command_line.clone();
        let saved = self.memory.save(item)?;
        if saved.id.is_some() {
            if self.entries.len() == self.capacity {
                self.entries.pop_front();
            }
            self.entries.push_back(command_line.clone());
            self.pending.push_back(command_line);
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
        self.memory.clear()?;
        self.entries.clear();
        self.pending.clear();
        let Some(path) = &self.file else {
            return Ok(());
        };
        validate_history_path(path).map_err(reedline::ReedlineError::from)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(reedline::ReedlineError::from)?;
        validate_file(&file, path).map_err(reedline::ReedlineError::from)?;
        let mut lock = RwLock::new(file);
        let mut guard = lock.write().map_err(reedline::ReedlineError::from)?;
        guard
            .rewind()
            .and_then(|()| guard.set_len(0))
            .map_err(reedline::ReedlineError::from)
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.memory.delete(id)
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

        self.replace_entries(merged)
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
