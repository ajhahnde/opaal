//! Editor-neutral, parser-aware completion over immutable candidate snapshots.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use flash_runtime::command::{CommandClassification, CommandRegistry};
use flash_runtime::intrinsic::{DynamicBinding, ExpressionIntrinsic};
use flash_runtime::{Environment, ScopeStack};
use flash_syntax::{
    CompletionContext, CompletionTarget, ParseOutcome, SourceFile, SourceId, completion_target,
    parse,
};

/// The semantic source of one completion candidate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CompletionKind {
    /// A built-in callable available in expression position.
    Intrinsic,
    /// A command registered inside Flash.
    InternalCommand,
    /// A named callable visible in the lexical scope.
    Function,
    /// An executable supplied by a host snapshot.
    ExternalCommand,
    /// A visible lexical binding.
    Variable,
    /// A flag advertised by an internal command signature.
    Flag,
    /// A UTF-8 path spelling supplied by a host snapshot.
    Path,
}

/// Fixed ceilings for host candidate collection at one prompt boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionSnapshotLimits {
    max_path_directories: usize,
    max_entries: usize,
}

impl CompletionSnapshotLimits {
    pub const DEFAULT_MAX_PATH_DIRECTORIES: usize = 256;
    pub const DEFAULT_MAX_ENTRIES: usize = 4_096;

    #[must_use]
    pub const fn new(max_path_directories: usize, max_entries: usize) -> Self {
        Self {
            max_path_directories,
            max_entries,
        }
    }
}

impl Default for CompletionSnapshotLimits {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_MAX_PATH_DIRECTORIES,
            Self::DEFAULT_MAX_ENTRIES,
        )
    }
}

/// One editor-neutral replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    value: String,
    replacement: Range<usize>,
    kind: CompletionKind,
    append_whitespace: bool,
}

impl Completion {
    /// The exact text to insert.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The half-open UTF-8 byte range replaced in the edit buffer.
    #[must_use]
    pub fn replacement(&self) -> Range<usize> {
        self.replacement.clone()
    }

    /// The source category used for deterministic ordering and presentation.
    #[must_use]
    pub const fn kind(&self) -> CompletionKind {
        self.kind
    }

    /// Whether the editor should add a separating space after insertion.
    #[must_use]
    pub const fn append_whitespace(&self) -> bool {
        self.append_whitespace
    }
}

/// Immutable candidates used by [`CompletionEngine`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionCatalog {
    intrinsics: BTreeSet<String>,
    internal: BTreeMap<String, BTreeSet<String>>,
    functions: BTreeSet<String>,
    variables: BTreeSet<String>,
    external: BTreeSet<String>,
    paths: BTreeSet<String>,
}

impl CompletionCatalog {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshots authoritative runtime registry and lexical-scope names.
    #[must_use]
    pub fn from_runtime(registry: &CommandRegistry, scope: &ScopeStack) -> Self {
        let internal = registry
            .namespace_entries()
            .filter_map(|entry| {
                let signature = match registry.classify(entry.name()) {
                    CommandClassification::Core { signature, .. }
                    | CommandClassification::Alias { signature, .. } => signature,
                    CommandClassification::Reserved { .. } | CommandClassification::Unknown => {
                        return None;
                    }
                };
                let flags = signature.flags().map(str::to_owned).collect();
                Some((entry.name().to_owned(), flags))
            })
            .collect();
        let mut functions = BTreeSet::new();
        let mut variables = BTreeSet::new();
        for (name, value) in scope.visible_bindings() {
            variables.insert(name.to_owned());
            if matches!(value, flash_runtime::Value::Callable(callable) if callable.family() == "function")
            {
                functions.insert(name.to_owned());
            }
        }
        variables.extend(
            DynamicBinding::ALL
                .into_iter()
                .map(DynamicBinding::name)
                .map(str::to_owned),
        );
        let intrinsics = ExpressionIntrinsic::ALL
            .into_iter()
            .map(ExpressionIntrinsic::name)
            .filter(|name| !variables.contains(*name))
            .map(str::to_owned)
            .collect();
        Self {
            intrinsics,
            internal,
            functions,
            variables,
            external: BTreeSet::new(),
            paths: BTreeSet::new(),
        }
    }

    /// Replaces the external-command snapshot.
    #[must_use]
    pub fn with_external_commands(
        mut self,
        commands: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.external = commands.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces the UTF-8 path snapshot.
    #[must_use]
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }
}

/// Builds one immutable prompt-boundary catalog from live session and host state.
///
/// Filesystem and environment work completes here; [`CompletionEngine`] retains
/// only UTF-8 candidates and performs no host I/O during a keypress.
#[must_use]
pub fn live_completion_catalog(
    registry: &CommandRegistry,
    scope: &ScopeStack,
    cwd: &Path,
    environment: &Environment,
    limits: CompletionSnapshotLimits,
) -> CompletionCatalog {
    CompletionCatalog::from_runtime(registry, scope)
        .with_external_commands(external_commands(environment, limits))
        .with_paths(cwd_paths(cwd, limits))
}

fn external_commands(
    environment: &Environment,
    limits: CompletionSnapshotLimits,
) -> BTreeSet<String> {
    let Some(path) = environment.get("PATH") else {
        return BTreeSet::new();
    };
    let directories: Vec<_> = std::env::split_paths(path)
        .take(limits.max_path_directories.saturating_add(1))
        .collect();
    if directories.len() > limits.max_path_directories {
        return BTreeSet::new();
    }

    let mut observed = 0_usize;
    let mut commands = BTreeSet::new();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            observed = observed.saturating_add(1);
            if observed > limits.max_entries {
                return BTreeSet::new();
            }
            let Some(name) = insertable_native_name(entry.file_name()) else {
                continue;
            };
            if fs::metadata(entry.path()).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }) {
                commands.insert(name);
            }
        }
    }
    commands
}

fn cwd_paths(cwd: &Path, limits: CompletionSnapshotLimits) -> BTreeSet<String> {
    let Ok(entries) = fs::read_dir(cwd) else {
        return BTreeSet::new();
    };
    let mut observed = 0_usize;
    let mut paths = BTreeSet::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        observed = observed.saturating_add(1);
        if observed > limits.max_entries {
            return BTreeSet::new();
        }
        let Some(mut name) = insertable_native_name(entry.file_name()) else {
            continue;
        };
        if fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir()) {
            name.push('/');
        }
        paths.insert(name.clone());
        paths.insert(format!("./{name}"));
    }
    paths
}

fn insertable_native_name(value: OsString) -> Option<String> {
    value
        .into_string()
        .ok()
        .filter(|value| insertable_unquoted(value))
}

fn insertable_unquoted(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            !character.is_whitespace()
                && !character.is_control()
                && !matches!(
                    character,
                    '|' | '&'
                        | ';'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '['
                        | ']'
                        | '$'
                        | '\''
                        | '"'
                        | '\\'
                        | '#'
                )
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    #[test]
    fn native_names_require_lossless_utf8_and_unquoted_spelling() {
        assert_eq!(
            insertable_native_name(OsString::from("plain-name")),
            Some("plain-name".to_owned())
        );
        assert_eq!(insertable_native_name(OsString::from("two words")), None);
        assert_eq!(
            insertable_native_name(OsString::from_vec(vec![b'n', 0xff])),
            None
        );
    }
}

/// Pure completion over a fixed catalog.
#[derive(Clone, Debug, Default)]
pub struct CompletionEngine {
    catalog: CompletionCatalog,
}

impl CompletionEngine {
    /// Builds an engine over one immutable candidate snapshot.
    #[must_use]
    pub const fn new(catalog: CompletionCatalog) -> Self {
        Self { catalog }
    }

    /// Completes the source at one UTF-8 byte cursor.
    #[must_use]
    pub fn complete(&self, source: &str, cursor: usize) -> Vec<Completion> {
        if cursor > source.len() || !source.is_char_boundary(cursor) {
            return Vec::new();
        }

        let source_file = SourceFile::new(SourceId::new(0), "<interactive>", source);
        if matches!(parse(&source_file), ParseOutcome::Invalid(_)) {
            return Vec::new();
        }
        let Some(target) = completion_target(source, cursor) else {
            return Vec::new();
        };
        self.candidates(&target)
    }

    fn candidates(&self, target: &CompletionTarget) -> Vec<Completion> {
        let mut completions = Vec::new();
        let mut seen = HashSet::new();
        let replacement = target.replacement();
        let mut add = |values: &BTreeSet<String>,
                       kind,
                       prefix: &str,
                       decorate: bool,
                       append_whitespace: bool| {
            for value in values.iter().filter(|value| value.starts_with(prefix)) {
                let replacement_value = if decorate {
                    format!("${value}")
                } else {
                    value.clone()
                };
                if seen.insert(replacement_value.clone()) {
                    completions.push(Completion {
                        value: replacement_value,
                        replacement: replacement.clone(),
                        kind,
                        append_whitespace,
                    });
                }
            }
        };

        match target.context() {
            CompletionContext::Command { forced_external } => {
                if !*forced_external {
                    let names = self.catalog.internal.keys().cloned().collect();
                    add(
                        &names,
                        CompletionKind::InternalCommand,
                        target.prefix(),
                        false,
                        true,
                    );
                    add(
                        &self.catalog.functions,
                        CompletionKind::Function,
                        target.prefix(),
                        false,
                        true,
                    );
                }
                add(
                    &self.catalog.external,
                    CompletionKind::ExternalCommand,
                    target.prefix(),
                    false,
                    true,
                );
            }
            CompletionContext::Expression => {
                add(
                    &self.catalog.functions,
                    CompletionKind::Function,
                    target.prefix(),
                    false,
                    false,
                );
                add(
                    &self.catalog.intrinsics,
                    CompletionKind::Intrinsic,
                    target.prefix(),
                    false,
                    false,
                );
            }
            CompletionContext::Variable => add(
                &self.catalog.variables,
                CompletionKind::Variable,
                target.prefix().strip_prefix('$').unwrap_or(target.prefix()),
                true,
                false,
            ),
            CompletionContext::Flag { command } => {
                if let Some(flags) = self.catalog.internal.get(command.as_str()) {
                    add(flags, CompletionKind::Flag, target.prefix(), false, true);
                }
            }
            CompletionContext::Path => add(
                &self.catalog.paths,
                CompletionKind::Path,
                target.prefix(),
                false,
                false,
            ),
            CompletionContext::None => {}
        }
        completions
    }
}
