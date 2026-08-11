//! The command registry and command signatures.
//!
//! A [`CommandSignature`] owns the pipeline-carrier contract and stable
//! documentation an internal command declares: accepted input carriers, its
//! output carrier, advertised flags, invocation spelling, and prose. Typed
//! parameters and option value schemas remain later additive extensions. A
//! [`CommandRegistry`] owns a validated namespace manifest. Core entries own
//! signatures, aliases borrow the signature of one canonical core entry, and
//! reserved names own no executable signature. The registry remains empty by
//! default so focused callers can inject signatures without constructing the
//! standard manifest.

use std::collections::{BTreeMap, BTreeSet};

use crate::documentation::CommandDocumentation;

/// The language major whose namespace contract is being frozen for Flash v1.
pub const V1_LANGUAGE_MAJOR: u16 = 1;

/// One pipeline-edge carrier.
///
/// `Empty`, `ByteStream`, `Value`, and `ValueStream` are distinct payload states
/// that the planner never substitutes for one another.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Carrier {
    /// No payload, distinct from `null` and from an empty stream.
    Empty,
    /// One ordered logical sequence of bytes in arbitrary chunks.
    ByteStream,
    /// Exactly one runtime value.
    Value,
    /// An ordered sequence of zero or more runtime values.
    ValueStream,
}

/// How an internal command determines its output carrier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CommandOutput {
    /// The command always produces one fixed carrier.
    Fixed(Carrier),
    /// The command forwards whichever carrier it accepted unchanged.
    SameAsInput,
}

impl CommandOutput {
    /// Resolve this contract for an actual input carrier.
    #[must_use]
    pub const fn resolve(self, input: Carrier) -> Carrier {
        match self {
            Self::Fixed(output) => output,
            Self::SameAsInput => input,
        }
    }
}

/// An internal command's signature: its name, pipeline contract, flags, and prose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSignature {
    name: String,
    inputs: BTreeSet<Carrier>,
    output: CommandOutput,
    flags: BTreeSet<String>,
    documentation: CommandDocumentation,
}

/// Compatibility metadata shared by invocable namespace entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandLifecycle {
    introduced_major: u16,
    deprecated_since: Option<String>,
    replacement: Option<String>,
}

impl CommandLifecycle {
    /// Metadata for an entry introduced in `language_major`.
    #[must_use]
    pub const fn introduced(language_major: u16) -> Self {
        Self {
            introduced_major: language_major,
            deprecated_since: None,
            replacement: None,
        }
    }

    /// Marks the entry deprecated since one product release.
    #[must_use]
    pub fn deprecated_since(mut self, release: impl Into<String>) -> Self {
        self.deprecated_since = Some(release.into());
        self
    }

    /// Adds a canonical replacement spelling.
    #[must_use]
    pub fn with_replacement(mut self, replacement: impl Into<String>) -> Self {
        self.replacement = Some(replacement.into());
        self
    }

    /// The language major in which the entry first existed or was reserved.
    #[must_use]
    pub const fn introduced_major(&self) -> u16 {
        self.introduced_major
    }

    /// The product release that first deprecated the entry, if any.
    #[must_use]
    pub fn deprecated_since_release(&self) -> Option<&str> {
        self.deprecated_since.as_deref()
    }

    /// The canonical replacement spelling, if one is published.
    #[must_use]
    pub fn replacement(&self) -> Option<&str> {
        self.replacement.as_deref()
    }
}

/// One declarative command-namespace manifest entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandNamespaceEntry {
    /// One canonical executable built-in.
    Core {
        /// The canonical signature and documentation.
        signature: CommandSignature,
        /// Compatibility lifecycle metadata.
        lifecycle: CommandLifecycle,
    },
    /// One executable migration spelling targeting a canonical core entry.
    Alias {
        /// The source spelling.
        name: String,
        /// The canonical core name.
        target: String,
        /// Compatibility lifecycle metadata.
        lifecycle: CommandLifecycle,
    },
    /// One unavailable spelling protected from implicit external fallback.
    Reserved {
        /// The protected source spelling.
        name: String,
        /// The language major in which the reservation first existed.
        introduced_major: u16,
        /// Stable user-facing reason for the reservation.
        purpose: String,
        /// Optional canonical migration target.
        replacement: Option<String>,
    },
}

impl CommandNamespaceEntry {
    /// A canonical core entry.
    #[must_use]
    pub const fn core(signature: CommandSignature, lifecycle: CommandLifecycle) -> Self {
        Self::Core {
            signature,
            lifecycle,
        }
    }

    /// An alias that reuses one canonical core entry.
    #[must_use]
    pub fn alias(
        name: impl Into<String>,
        target: impl Into<String>,
        lifecycle: CommandLifecycle,
    ) -> Self {
        Self::Alias {
            name: name.into(),
            target: target.into(),
            lifecycle,
        }
    }

    /// A name protected from implicit external fallback.
    #[must_use]
    pub fn reserved(
        name: impl Into<String>,
        introduced_major: u16,
        purpose: impl Into<String>,
        replacement: Option<&str>,
    ) -> Self {
        Self::Reserved {
            name: name.into(),
            introduced_major,
            purpose: purpose.into(),
            replacement: replacement.map(str::to_owned),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Core { signature, .. } => signature.name(),
            Self::Alias { name, .. } | Self::Reserved { name, .. } => name,
        }
    }
}

/// The stable class of one namespace spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceClass {
    /// A canonical executable built-in.
    Core,
    /// An executable migration spelling.
    Alias,
    /// A spelling protected from external fallback.
    Reserved,
}

/// The classification result for one command spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandClassification<'a> {
    /// The spelling has no namespace entry.
    Unknown,
    /// A canonical executable built-in.
    Core {
        /// The canonical signature.
        signature: &'a CommandSignature,
        /// Compatibility lifecycle metadata.
        lifecycle: &'a CommandLifecycle,
    },
    /// An executable spelling that reuses one canonical core entry.
    Alias {
        /// The canonical executor and signature name.
        canonical_name: &'a str,
        /// The canonical signature.
        signature: &'a CommandSignature,
        /// Compatibility lifecycle metadata for the alias spelling.
        lifecycle: &'a CommandLifecycle,
    },
    /// A spelling protected from implicit external fallback.
    Reserved {
        /// Stable user-facing reason for the reservation.
        purpose: &'a str,
        /// Optional canonical migration target.
        replacement: Option<&'a str>,
        /// The language major in which the reservation first existed.
        introduced_major: u16,
    },
}

/// A borrowed entry from deterministic namespace iteration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamespaceEntryRef<'a> {
    name: &'a str,
    class: NamespaceClass,
}

impl<'a> NamespaceEntryRef<'a> {
    /// The source spelling.
    #[must_use]
    pub const fn name(self) -> &'a str {
        self.name
    }

    /// The entry's stable namespace class.
    #[must_use]
    pub const fn class(self) -> NamespaceClass {
        self.class
    }
}

/// A manifest validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandRegistryError {
    /// The registry language major must be positive.
    InvalidLanguageMajor { language_major: u16 },
    /// Namespace spellings cannot be empty.
    EmptyName,
    /// One spelling occurred in more than one manifest entry.
    DuplicateName { name: String },
    /// An entry claims introduction outside the registry's language-major range.
    InvalidIntroducedMajor {
        name: String,
        introduced_major: u16,
        language_major: u16,
    },
    /// A deprecation release spelling was empty.
    EmptyDeprecation { name: String },
    /// Replacement guidance requires an explicit deprecation.
    ReplacementWithoutDeprecation { name: String },
    /// A reservation must explain why the spelling is unavailable.
    EmptyReservationPurpose { name: String },
    /// An alias targeted itself.
    SelfAlias { name: String },
    /// An alias target had no manifest entry.
    MissingAliasTarget { name: String, target: String },
    /// An alias target was another alias or a reservation rather than a core.
    AliasTargetNotCore { name: String, target: String },
    /// Replacement guidance did not name a different canonical core entry.
    InvalidReplacementTarget { name: String, replacement: String },
}

impl std::fmt::Display for CommandRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid command namespace manifest: {self:?}")
    }
}

impl std::error::Error for CommandRegistryError {}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StoredNamespaceEntry {
    Core {
        signature: CommandSignature,
        lifecycle: CommandLifecycle,
    },
    Alias {
        target: String,
        lifecycle: CommandLifecycle,
    },
    Reserved {
        introduced_major: u16,
        purpose: String,
        replacement: Option<String>,
    },
}

impl CommandSignature {
    /// Builds a signature from a name, the input carriers it accepts, and the
    /// carrier it produces. Duplicate input carriers collapse to one.
    pub fn new(
        name: impl Into<String>,
        inputs: impl IntoIterator<Item = Carrier>,
        output: Carrier,
    ) -> Self {
        Self {
            name: name.into(),
            inputs: inputs.into_iter().collect(),
            output: CommandOutput::Fixed(output),
            flags: BTreeSet::new(),
            documentation: CommandDocumentation::default(),
        }
    }

    /// Builds a signature that forwards its accepted input carrier unchanged.
    pub fn passthrough(name: impl Into<String>, inputs: impl IntoIterator<Item = Carrier>) -> Self {
        Self {
            name: name.into(),
            inputs: inputs.into_iter().collect(),
            output: CommandOutput::SameAsInput,
            flags: BTreeSet::new(),
            documentation: CommandDocumentation::default(),
        }
    }

    /// Adds the flags this command accepts. Duplicate spellings collapse to one.
    #[must_use]
    pub fn with_flags(mut self, flags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.flags.extend(flags.into_iter().map(Into::into));
        self
    }

    /// Attaches the command's stable invocation spelling and prose.
    #[must_use]
    pub fn with_documentation(mut self, documentation: CommandDocumentation) -> Self {
        self.documentation = documentation;
        self
    }

    /// The command name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the command accepts `carrier` as input.
    #[must_use]
    pub fn accepts(&self, carrier: Carrier) -> bool {
        self.inputs.contains(&carrier)
    }

    /// The accepted input carriers, in a deterministic order.
    pub fn inputs(&self) -> impl Iterator<Item = Carrier> + '_ {
        self.inputs.iter().copied()
    }

    /// The carrier the command produces.
    #[must_use]
    pub fn output(&self) -> CommandOutput {
        self.output
    }

    /// The advertised flags, in sorted order.
    pub fn flags(&self) -> impl Iterator<Item = &str> {
        self.flags.iter().map(String::as_str)
    }

    /// Required user-facing invocation and prose metadata.
    #[must_use]
    pub const fn documentation(&self) -> &CommandDocumentation {
        &self.documentation
    }
}

/// A validated command namespace. Empty at language major one by default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRegistry {
    language_major: u16,
    entries: BTreeMap<String, StoredNamespaceEntry>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self {
            language_major: V1_LANGUAGE_MAJOR,
            entries: BTreeMap::new(),
        }
    }
}

impl CommandRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds and validates one complete namespace manifest.
    pub fn try_from_entries(
        language_major: u16,
        entries: impl IntoIterator<Item = CommandNamespaceEntry>,
    ) -> Result<Self, CommandRegistryError> {
        if language_major == 0 {
            return Err(CommandRegistryError::InvalidLanguageMajor { language_major });
        }

        let mut registry = Self {
            language_major,
            entries: BTreeMap::new(),
        };
        for entry in entries {
            let name = entry.name().to_owned();
            if name.is_empty() {
                return Err(CommandRegistryError::EmptyName);
            }
            if registry.entries.contains_key(&name) {
                return Err(CommandRegistryError::DuplicateName { name });
            }

            let stored = match entry {
                CommandNamespaceEntry::Core {
                    signature,
                    lifecycle,
                } => {
                    validate_lifecycle(&name, &lifecycle, language_major)?;
                    StoredNamespaceEntry::Core {
                        signature,
                        lifecycle,
                    }
                }
                CommandNamespaceEntry::Alias {
                    target, lifecycle, ..
                } => {
                    validate_lifecycle(&name, &lifecycle, language_major)?;
                    StoredNamespaceEntry::Alias { target, lifecycle }
                }
                CommandNamespaceEntry::Reserved {
                    introduced_major,
                    purpose,
                    replacement,
                    ..
                } => {
                    validate_introduced_major(&name, introduced_major, language_major)?;
                    if purpose.is_empty() {
                        return Err(CommandRegistryError::EmptyReservationPurpose { name });
                    }
                    StoredNamespaceEntry::Reserved {
                        introduced_major,
                        purpose,
                        replacement,
                    }
                }
            };
            registry.entries.insert(name, stored);
        }

        registry.validate_targets()?;
        Ok(registry)
    }

    /// Registers `signature` under its name. Returns `true` if it was inserted, or
    /// `false` if a command of that name already exists, in which case the earlier
    /// signature is kept unchanged.
    pub fn register(&mut self, signature: CommandSignature) -> bool {
        if signature.name().is_empty() || self.entries.contains_key(signature.name()) {
            return false;
        }
        self.entries.insert(
            signature.name.clone(),
            StoredNamespaceEntry::Core {
                signature,
                lifecycle: CommandLifecycle::introduced(self.language_major),
            },
        );
        true
    }

    /// The signature registered under `name`, if any.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<&CommandSignature> {
        match self.entries.get(name)? {
            StoredNamespaceEntry::Core { signature, .. } => Some(signature),
            StoredNamespaceEntry::Alias { target, .. } => match self.entries.get(target) {
                Some(StoredNamespaceEntry::Core { signature, .. }) => Some(signature),
                _ => unreachable!("validated aliases target core entries"),
            },
            StoredNamespaceEntry::Reserved { .. } => None,
        }
    }

    /// Whether a command of `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        matches!(
            self.entries.get(name),
            Some(StoredNamespaceEntry::Core { .. } | StoredNamespaceEntry::Alias { .. })
        )
    }

    /// The number of registered commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has no commands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The registered command names, in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Registered signatures in name order.
    pub fn signatures(&self) -> impl Iterator<Item = &CommandSignature> {
        self.entries.values().filter_map(|entry| match entry {
            StoredNamespaceEntry::Core { signature, .. } => Some(signature),
            StoredNamespaceEntry::Alias { .. } | StoredNamespaceEntry::Reserved { .. } => None,
        })
    }

    /// The language major whose namespace policy this registry validates.
    #[must_use]
    pub const fn language_major(&self) -> u16 {
        self.language_major
    }

    /// Classify one source spelling through the shared namespace seam.
    #[must_use]
    pub fn classify(&self, name: &str) -> CommandClassification<'_> {
        match self.entries.get(name) {
            None => CommandClassification::Unknown,
            Some(StoredNamespaceEntry::Core {
                signature,
                lifecycle,
            }) => CommandClassification::Core {
                signature,
                lifecycle,
            },
            Some(StoredNamespaceEntry::Alias { target, lifecycle }) => {
                let Some(StoredNamespaceEntry::Core { signature, .. }) = self.entries.get(target)
                else {
                    unreachable!("validated aliases target core entries");
                };
                CommandClassification::Alias {
                    canonical_name: target,
                    signature,
                    lifecycle,
                }
            }
            Some(StoredNamespaceEntry::Reserved {
                introduced_major,
                purpose,
                replacement,
            }) => CommandClassification::Reserved {
                purpose,
                replacement: replacement.as_deref(),
                introduced_major: *introduced_major,
            },
        }
    }

    /// Every namespace entry in spelling order.
    pub fn namespace_entries(&self) -> impl Iterator<Item = NamespaceEntryRef<'_>> {
        self.entries.iter().map(|(name, entry)| NamespaceEntryRef {
            name,
            class: match entry {
                StoredNamespaceEntry::Core { .. } => NamespaceClass::Core,
                StoredNamespaceEntry::Alias { .. } => NamespaceClass::Alias,
                StoredNamespaceEntry::Reserved { .. } => NamespaceClass::Reserved,
            },
        })
    }

    /// Canonical core names in spelling order.
    pub fn core_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|(name, entry)| {
            matches!(entry, StoredNamespaceEntry::Core { .. }).then_some(name.as_str())
        })
    }

    /// Alias names in spelling order.
    pub fn alias_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|(name, entry)| {
            matches!(entry, StoredNamespaceEntry::Alias { .. }).then_some(name.as_str())
        })
    }

    /// Reserved names in spelling order.
    pub fn reserved_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|(name, entry)| {
            matches!(entry, StoredNamespaceEntry::Reserved { .. }).then_some(name.as_str())
        })
    }

    fn validate_targets(&self) -> Result<(), CommandRegistryError> {
        for (name, entry) in &self.entries {
            match entry {
                StoredNamespaceEntry::Core { lifecycle, .. } => {
                    self.validate_replacement(name, lifecycle.replacement())?;
                }
                StoredNamespaceEntry::Alias { target, lifecycle } => {
                    if target == name {
                        return Err(CommandRegistryError::SelfAlias { name: name.clone() });
                    }
                    match self.entries.get(target) {
                        None => {
                            return Err(CommandRegistryError::MissingAliasTarget {
                                name: name.clone(),
                                target: target.clone(),
                            });
                        }
                        Some(StoredNamespaceEntry::Core { .. }) => {}
                        Some(
                            StoredNamespaceEntry::Alias { .. }
                            | StoredNamespaceEntry::Reserved { .. },
                        ) => {
                            return Err(CommandRegistryError::AliasTargetNotCore {
                                name: name.clone(),
                                target: target.clone(),
                            });
                        }
                    }
                    self.validate_replacement(name, lifecycle.replacement())?;
                }
                StoredNamespaceEntry::Reserved { replacement, .. } => {
                    self.validate_replacement(name, replacement.as_deref())?;
                }
            }
        }
        Ok(())
    }

    fn validate_replacement(
        &self,
        name: &str,
        replacement: Option<&str>,
    ) -> Result<(), CommandRegistryError> {
        let Some(replacement) = replacement else {
            return Ok(());
        };
        if replacement == name
            || !matches!(
                self.entries.get(replacement),
                Some(StoredNamespaceEntry::Core { .. })
            )
        {
            return Err(CommandRegistryError::InvalidReplacementTarget {
                name: name.to_owned(),
                replacement: replacement.to_owned(),
            });
        }
        Ok(())
    }
}

fn validate_lifecycle(
    name: &str,
    lifecycle: &CommandLifecycle,
    language_major: u16,
) -> Result<(), CommandRegistryError> {
    validate_introduced_major(name, lifecycle.introduced_major, language_major)?;
    if lifecycle
        .deprecated_since
        .as_ref()
        .is_some_and(String::is_empty)
    {
        return Err(CommandRegistryError::EmptyDeprecation {
            name: name.to_owned(),
        });
    }
    if lifecycle.replacement.is_some() && lifecycle.deprecated_since.is_none() {
        return Err(CommandRegistryError::ReplacementWithoutDeprecation {
            name: name.to_owned(),
        });
    }
    Ok(())
}

fn validate_introduced_major(
    name: &str,
    introduced_major: u16,
    language_major: u16,
) -> Result<(), CommandRegistryError> {
    if introduced_major == 0 || introduced_major > language_major {
        return Err(CommandRegistryError::InvalidIntroducedMajor {
            name: name.to_owned(),
            introduced_major,
            language_major,
        });
    }
    Ok(())
}
