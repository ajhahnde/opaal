//! The command registry and command signatures.
//!
//! A [`CommandSignature`] owns the pipeline-carrier contract and stable
//! documentation an internal command declares: accepted input carriers, its
//! output carrier, positional/option validation, invocation spelling, and prose. A
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
    arguments: CommandArgumentSchema,
    documentation: CommandDocumentation,
}

/// The source/runtime form accepted by one positional slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandArgumentKind {
    /// One native command word.
    Word,
    /// One typed closure value.
    Closure,
    /// Either a word or a typed closure value.
    Any,
}

/// How static checking treats an argument spread whose final length is unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandDynamicTail {
    /// Defer the final arity and option answer to expanded planning.
    DeferredToRuntime,
    /// Reject a dynamic tail because the command requires a fixed source shape.
    Rejected,
}

/// How a command treats the conventional `--` option terminator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOptionTerminator {
    /// Remove the first `--` before execution and treat later words as positionals.
    Accepted,
    /// Treat `--` as an ordinary positional word.
    Literal,
}

/// One declarative long-option contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOptionSchema {
    name: String,
    value_arity: usize,
    repeatable: bool,
    conflicts: BTreeSet<String>,
}

impl CommandOptionSchema {
    /// A non-repeatable flag that consumes no following value.
    #[must_use]
    pub fn flag(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value_arity: 0,
            repeatable: false,
            conflicts: BTreeSet::new(),
        }
    }

    /// Sets the exact number of following words consumed by this option.
    #[must_use]
    pub const fn with_value_arity(mut self, value_arity: usize) -> Self {
        self.value_arity = value_arity;
        self
    }

    /// Allows the option to occur more than once.
    #[must_use]
    pub const fn repeatable(mut self) -> Self {
        self.repeatable = true;
        self
    }

    /// Declares mutually exclusive option spellings.
    #[must_use]
    pub fn conflicts_with(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.conflicts.extend(names.into_iter().map(Into::into));
        self
    }

    /// The exact option spelling.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The exact count of following value words.
    #[must_use]
    pub const fn value_arity(&self) -> usize {
        self.value_arity
    }

    /// Whether the option may repeat.
    #[must_use]
    pub const fn is_repeatable(&self) -> bool {
        self.repeatable
    }

    /// Mutually exclusive option spellings, in deterministic order.
    pub fn conflicts(&self) -> impl Iterator<Item = &str> {
        self.conflicts.iter().map(String::as_str)
    }
}

/// Complete positional, option, terminator, and dynamic-tail metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandArgumentSchema {
    minimum: usize,
    maximum: Option<usize>,
    positional_kinds: Vec<CommandArgumentKind>,
    variadic_kind: CommandArgumentKind,
    options: BTreeMap<String, CommandOptionSchema>,
    options_before_positionals: bool,
    terminator: CommandOptionTerminator,
    dynamic_tail: CommandDynamicTail,
}

impl Default for CommandArgumentSchema {
    fn default() -> Self {
        Self {
            minimum: 0,
            maximum: None,
            positional_kinds: Vec::new(),
            variadic_kind: CommandArgumentKind::Any,
            options: BTreeMap::new(),
            options_before_positionals: false,
            terminator: CommandOptionTerminator::Accepted,
            dynamic_tail: CommandDynamicTail::DeferredToRuntime,
        }
    }
}

impl CommandArgumentSchema {
    /// An exact positional range with word-valued slots by default.
    #[must_use]
    pub fn positional(minimum: usize, maximum: Option<usize>) -> Self {
        Self {
            minimum,
            maximum,
            positional_kinds: Vec::new(),
            variadic_kind: CommandArgumentKind::Word,
            ..Self::default()
        }
    }

    /// Assigns exact kinds to the leading positional slots.
    #[must_use]
    pub fn with_positional_kinds(
        mut self,
        kinds: impl IntoIterator<Item = CommandArgumentKind>,
    ) -> Self {
        self.positional_kinds = kinds.into_iter().collect();
        self
    }

    /// Assigns the kind used after the explicitly described leading slots.
    #[must_use]
    pub const fn with_variadic_kind(mut self, kind: CommandArgumentKind) -> Self {
        self.variadic_kind = kind;
        self
    }

    /// Adds declarative long-option schemas.
    #[must_use]
    pub fn with_options(mut self, options: impl IntoIterator<Item = CommandOptionSchema>) -> Self {
        for option in options {
            self.options.insert(option.name.clone(), option);
        }
        self
    }

    /// Requires options to precede every positional argument.
    #[must_use]
    pub const fn options_before_positionals(mut self) -> Self {
        self.options_before_positionals = true;
        self
    }

    /// Selects whether `--` is an option terminator or a literal positional.
    #[must_use]
    pub const fn with_terminator(mut self, terminator: CommandOptionTerminator) -> Self {
        self.terminator = terminator;
        self
    }

    /// Selects how an unresolved spread is handled by static checking.
    #[must_use]
    pub const fn with_dynamic_tail(mut self, dynamic_tail: CommandDynamicTail) -> Self {
        self.dynamic_tail = dynamic_tail;
        self
    }

    /// Minimum positional count after option parsing.
    #[must_use]
    pub const fn minimum(&self) -> usize {
        self.minimum
    }

    /// Maximum positional count after option parsing.
    #[must_use]
    pub const fn maximum(&self) -> Option<usize> {
        self.maximum
    }

    /// Expected form for one zero-based positional slot.
    #[must_use]
    pub fn positional_kind(&self, position: usize) -> CommandArgumentKind {
        self.positional_kinds
            .get(position)
            .copied()
            .unwrap_or(self.variadic_kind)
    }

    /// One exact option contract.
    #[must_use]
    pub fn option(&self, name: &str) -> Option<&CommandOptionSchema> {
        self.options.get(name)
    }

    /// Options in deterministic spelling order.
    pub fn options(&self) -> impl Iterator<Item = &CommandOptionSchema> {
        self.options.values()
    }

    /// Whether options must occur before every positional.
    #[must_use]
    pub const fn require_options_before_positionals(&self) -> bool {
        self.options_before_positionals
    }

    /// The command's `--` policy.
    #[must_use]
    pub const fn terminator(&self) -> CommandOptionTerminator {
        self.terminator
    }

    /// The command's unresolved-spread policy.
    #[must_use]
    pub const fn dynamic_tail(&self) -> CommandDynamicTail {
        self.dynamic_tail
    }

    /// Validate one source or expanded argument sequence against this schema.
    ///
    /// Static callers represent interpolation-dependent words with `Word(None)`
    /// and unresolved spreads with `DynamicTail`; only facts independent of
    /// those values are reported. Expanded planning supplies exact word bytes.
    #[must_use]
    pub fn validate(&self, inputs: &[CommandArgumentInput]) -> Vec<CommandArgumentFault> {
        let mut faults = Vec::new();
        let mut options_active = true;
        let mut positional_started = false;
        let mut definite_positionals = 0usize;
        let mut definite_positional_indices = Vec::new();
        let mut possible_positionals = 0usize;
        let mut unbounded_positionals = false;
        let mut uncertain_option_classification = false;
        let mut seen = BTreeMap::<String, usize>::new();
        let mut index = 0usize;

        while index < inputs.len() {
            match &inputs[index] {
                CommandArgumentInput::DynamicTail => {
                    if self.dynamic_tail == CommandDynamicTail::Rejected {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::DynamicTail,
                        ));
                    } else {
                        unbounded_positionals = true;
                        uncertain_option_classification = true;
                    }
                    index += 1;
                }
                CommandArgumentInput::Word(Some(bytes))
                    if options_active
                        && self.terminator == CommandOptionTerminator::Accepted
                        && bytes.as_slice() == b"--" =>
                {
                    options_active = false;
                    index += 1;
                }
                CommandArgumentInput::Word(Some(bytes))
                    if options_active
                        && bytes.starts_with(b"--")
                        && !(self.terminator == CommandOptionTerminator::Literal
                            && bytes.as_slice() == b"--") =>
                {
                    let option_name = String::from_utf8_lossy(bytes).into_owned();
                    let Some(option) = self.options.get(&option_name) else {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::UnknownOption {
                                option: option_name,
                            },
                        ));
                        index += 1;
                        continue;
                    };
                    if self.options_before_positionals && positional_started {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::OptionAfterPositional {
                                option: option_name.clone(),
                            },
                        ));
                    }
                    if !option.repeatable && seen.contains_key(&option_name) {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::RepeatedOption {
                                option: option_name.clone(),
                            },
                        ));
                    }
                    if let Some(conflict) = option
                        .conflicts
                        .iter()
                        .find(|conflict| seen.contains_key(*conflict))
                    {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::ConflictingOptions {
                                option: option_name.clone(),
                                conflict: conflict.clone(),
                            },
                        ));
                    }
                    seen.insert(option_name.clone(), index);
                    let available = inputs.len().saturating_sub(index + 1);
                    if available < option.value_arity {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::MissingOptionValues {
                                option: option_name,
                                expected: option.value_arity,
                                actual: available,
                            },
                        ));
                        break;
                    }
                    index += option.value_arity + 1;
                }
                CommandArgumentInput::Word(None) if options_active => {
                    possible_positionals += 1;
                    uncertain_option_classification = true;
                    index += 1;
                }
                input => {
                    positional_started = true;
                    let position = definite_positionals;
                    definite_positionals += 1;
                    definite_positional_indices.push(index);
                    possible_positionals += 1;
                    let actual = match input {
                        CommandArgumentInput::Word(_) => CommandArgumentKind::Word,
                        CommandArgumentInput::Closure => CommandArgumentKind::Closure,
                        CommandArgumentInput::DynamicTail => unreachable!("handled above"),
                    };
                    let expected = self.positional_kind(position);
                    if !uncertain_option_classification
                        && expected != CommandArgumentKind::Any
                        && expected != actual
                    {
                        faults.push(CommandArgumentFault::new(
                            Some(index),
                            CommandArgumentFaultKind::UnexpectedKind {
                                position,
                                expected,
                                actual,
                            },
                        ));
                    }
                    index += 1;
                }
            }
        }

        let too_few = !unbounded_positionals
            && !uncertain_option_classification
            && possible_positionals < self.minimum;
        let too_many = self
            .maximum
            .is_some_and(|maximum| definite_positionals > maximum);
        if too_few || too_many {
            faults.push(CommandArgumentFault::new(
                self.maximum
                    .filter(|_| too_many)
                    .and_then(|maximum| definite_positional_indices.get(maximum).copied()),
                CommandArgumentFaultKind::Arity {
                    minimum: self.minimum,
                    maximum: self.maximum,
                    actual: definite_positionals,
                },
            ));
        }
        faults
    }
}

/// One argument shape presented to shared source/runtime validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandArgumentInput {
    /// A word with exact expanded bytes, or an interpolation-dependent static word.
    Word(Option<Vec<u8>>),
    /// A typed closure syntax item.
    Closure,
    /// A spread whose final number of words is unknown to static checking.
    DynamicTail,
}

/// One shared command-schema validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandArgumentFault {
    argument_index: Option<usize>,
    kind: CommandArgumentFaultKind,
}

impl CommandArgumentFault {
    fn new(argument_index: Option<usize>, kind: CommandArgumentFaultKind) -> Self {
        Self {
            argument_index,
            kind,
        }
    }

    /// Zero-based source/runtime argument index, when one argument owns the fault.
    #[must_use]
    pub const fn argument_index(&self) -> Option<usize> {
        self.argument_index
    }

    /// The exact schema rule that failed.
    #[must_use]
    pub const fn kind(&self) -> &CommandArgumentFaultKind {
        &self.kind
    }
}

/// Stable classes of shared command-schema validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandArgumentFaultKind {
    Arity {
        minimum: usize,
        maximum: Option<usize>,
        actual: usize,
    },
    UnknownOption {
        option: String,
    },
    MissingOptionValues {
        option: String,
        expected: usize,
        actual: usize,
    },
    RepeatedOption {
        option: String,
    },
    ConflictingOptions {
        option: String,
        conflict: String,
    },
    OptionAfterPositional {
        option: String,
    },
    UnexpectedKind {
        position: usize,
        expected: CommandArgumentKind,
        actual: CommandArgumentKind,
    },
    DynamicTail,
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
            arguments: CommandArgumentSchema::default(),
            documentation: CommandDocumentation::default(),
        }
    }

    /// Builds a signature that forwards its accepted input carrier unchanged.
    pub fn passthrough(name: impl Into<String>, inputs: impl IntoIterator<Item = Carrier>) -> Self {
        Self {
            name: name.into(),
            inputs: inputs.into_iter().collect(),
            output: CommandOutput::SameAsInput,
            arguments: CommandArgumentSchema::default(),
            documentation: CommandDocumentation::default(),
        }
    }

    /// Adds the flags this command accepts. Duplicate spellings collapse to one.
    #[must_use]
    pub fn with_flags(mut self, flags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.arguments = self
            .arguments
            .with_options(flags.into_iter().map(CommandOptionSchema::flag));
        self
    }

    /// Attaches the canonical positional and option schema.
    #[must_use]
    pub fn with_arguments(mut self, arguments: CommandArgumentSchema) -> Self {
        self.arguments = arguments;
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
        self.arguments.options().map(CommandOptionSchema::name)
    }

    /// The canonical positional and option schema.
    #[must_use]
    pub const fn arguments(&self) -> &CommandArgumentSchema {
        &self.arguments
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
