//! Editor-neutral, parser-aware completion over immutable candidate snapshots.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ops::Range;

use flash_runtime::ScopeStack;
use flash_runtime::command::{CommandClassification, CommandRegistry};
use flash_syntax::{
    CompletionContext, CompletionTarget, ParseOutcome, SourceFile, SourceId, completion_target,
    parse,
};

/// The semantic source of one completion candidate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CompletionKind {
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
        Self {
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
        let mut add = |values: &BTreeSet<String>, kind, prefix: &str, decorate: bool| {
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
                        append_whitespace: !matches!(
                            kind,
                            CompletionKind::Variable | CompletionKind::Path
                        ),
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
                    );
                    add(
                        &self.catalog.functions,
                        CompletionKind::Function,
                        target.prefix(),
                        false,
                    );
                }
                add(
                    &self.catalog.external,
                    CompletionKind::ExternalCommand,
                    target.prefix(),
                    false,
                );
            }
            CompletionContext::Variable => add(
                &self.catalog.variables,
                CompletionKind::Variable,
                target.prefix().strip_prefix('$').unwrap_or(target.prefix()),
                true,
            ),
            CompletionContext::Flag { command } => {
                if let Some(flags) = self.catalog.internal.get(command.as_str()) {
                    add(flags, CompletionKind::Flag, target.prefix(), false);
                }
            }
            CompletionContext::Path => add(
                &self.catalog.paths,
                CompletionKind::Path,
                target.prefix(),
                false,
            ),
            CompletionContext::None => {}
        }
        completions
    }
}
