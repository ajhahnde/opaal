//! Host-free documentation metadata, help snapshots, and deterministic rendering.

use flash_syntax::SourceFile;

use crate::ScopeStack;
use crate::command::{
    Carrier, CommandClassification, CommandLifecycle, CommandOutput, CommandRegistry,
    CommandSignature,
};
pub use crate::documentation::{CommandDocumentation, Documentation};
use crate::module::FunctionSignature;

/// A named callable's immutable defining metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionInspection {
    signature: FunctionSignature,
    source: String,
    line: usize,
    column: usize,
}

impl FunctionInspection {
    pub(crate) fn new(signature: FunctionSignature, source: &SourceFile) -> Self {
        let location = source
            .location(signature.declaration_span().start())
            .expect("function signatures address their defining source");
        Self {
            signature,
            source: source.name().to_owned(),
            line: location.line(),
            column: location.column(),
        }
    }

    #[must_use]
    pub const fn signature(&self) -> &FunctionSignature {
        &self.signature
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    #[must_use]
    pub const fn column(&self) -> usize {
        self.column
    }
}

/// The namespace owning one help result.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum HelpKind {
    Builtin,
    Alias,
    Reserved,
    Function,
}

/// Structured signature metadata retained in one immutable help entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HelpSignature {
    Builtin(CommandSignature),
    Reserved,
    Function(FunctionSignature),
}

/// Namespace metadata retained by one help result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HelpNamespace {
    /// A canonical executable built-in.
    Core { lifecycle: CommandLifecycle },
    /// An executable migration spelling backed by a canonical built-in.
    Alias {
        canonical_name: String,
        lifecycle: CommandLifecycle,
    },
    /// A spelling protected from implicit external fallback.
    Reserved {
        introduced_major: u16,
        purpose: String,
        replacement: Option<String>,
    },
    /// A visible lexical function, outside the command registry.
    Function,
}

/// One immutable command-namespace or visible named-function help result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    kind: HelpKind,
    name: String,
    signature: HelpSignature,
    namespace: HelpNamespace,
    documentation: Documentation,
    definition: Option<FunctionInspection>,
}

impl HelpEntry {
    #[must_use]
    pub const fn kind(&self) -> HelpKind {
        self.kind
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn signature(&self) -> &HelpSignature {
        &self.signature
    }

    #[must_use]
    pub const fn namespace(&self) -> &HelpNamespace {
        &self.namespace
    }

    #[must_use]
    pub const fn documentation(&self) -> &Documentation {
        &self.documentation
    }

    #[must_use]
    pub const fn definition(&self) -> Option<&FunctionInspection> {
        self.definition.as_ref()
    }
}

/// A deterministic, immutable catalog derived from canonical signature owners.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HelpCatalog {
    entries: Vec<HelpEntry>,
}

impl HelpCatalog {
    /// Snapshots command-namespace entries and currently visible named callables.
    #[must_use]
    pub fn snapshot(registry: &CommandRegistry, scope: &ScopeStack) -> Self {
        let mut entries = registry
            .namespace_entries()
            .map(|entry| match registry.classify(entry.name()) {
                CommandClassification::Core {
                    signature,
                    lifecycle,
                } => HelpEntry {
                    kind: HelpKind::Builtin,
                    name: entry.name().to_owned(),
                    signature: HelpSignature::Builtin(signature.clone()),
                    namespace: HelpNamespace::Core {
                        lifecycle: lifecycle.clone(),
                    },
                    documentation: signature.documentation().documentation().clone(),
                    definition: None,
                },
                CommandClassification::Alias {
                    canonical_name,
                    signature,
                    lifecycle,
                } => HelpEntry {
                    kind: HelpKind::Alias,
                    name: entry.name().to_owned(),
                    signature: HelpSignature::Builtin(signature.clone()),
                    namespace: HelpNamespace::Alias {
                        canonical_name: canonical_name.to_owned(),
                        lifecycle: lifecycle.clone(),
                    },
                    documentation: signature.documentation().documentation().clone(),
                    definition: None,
                },
                CommandClassification::Reserved {
                    purpose,
                    replacement,
                    introduced_major,
                } => HelpEntry {
                    kind: HelpKind::Reserved,
                    name: entry.name().to_owned(),
                    signature: HelpSignature::Reserved,
                    namespace: HelpNamespace::Reserved {
                        introduced_major,
                        purpose: purpose.to_owned(),
                        replacement: replacement.map(str::to_owned),
                    },
                    documentation: Documentation::new(purpose),
                    definition: None,
                },
                CommandClassification::Unknown => {
                    unreachable!("namespace iteration yields classified entries")
                }
            })
            .collect::<Vec<_>>();
        entries.extend(
            scope
                .visible_bindings()
                .into_iter()
                .filter_map(|(_, value)| {
                    let crate::Value::Callable(callable) = value else {
                        return None;
                    };
                    let inspection = callable.inspection()?.clone();
                    let signature = inspection.signature().clone();
                    Some(HelpEntry {
                        kind: HelpKind::Function,
                        name: signature.name().to_owned(),
                        documentation: signature.documentation().cloned().unwrap_or_default(),
                        signature: HelpSignature::Function(signature),
                        namespace: HelpNamespace::Function,
                        definition: Some(inspection),
                    })
                }),
        );
        entries.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.kind.cmp(&right.kind))
        });
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }

    /// Selects exact, case-sensitive matches while preserving catalog order.
    #[must_use]
    pub fn query(&self, name: Option<&str>) -> Vec<HelpEntry> {
        match name {
            Some(name) => self
                .entries
                .iter()
                .filter(|entry| entry.name() == name)
                .cloned()
                .collect(),
            None => self.entries.clone(),
        }
    }
}

/// The immutable list/detail payload owned by a planned `help` stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpSnapshot {
    entries: Vec<HelpEntry>,
    detailed: bool,
}

impl HelpSnapshot {
    #[must_use]
    pub fn new(entries: Vec<HelpEntry>, detailed: bool) -> Self {
        Self { entries, detailed }
    }

    #[must_use]
    pub fn entries(&self) -> &[HelpEntry] {
        &self.entries
    }

    #[must_use]
    pub const fn detailed(&self) -> bool {
        self.detailed
    }
}

/// Renders a planned help snapshot as stable UTF-8 ending in one newline.
#[must_use]
pub fn render_help(snapshot: &HelpSnapshot) -> Vec<u8> {
    let mut output = String::new();
    if snapshot.detailed {
        for (index, entry) in snapshot.entries.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            render_detail(&mut output, entry);
        }
    } else {
        for entry in &snapshot.entries {
            let kind = kind_name(entry.kind);
            let signature = signature_text(&entry.signature);
            let summary = list_summary(entry);
            output.push_str(&format!("{}\t{kind}\t{signature}\t{summary}\n", entry.name));
        }
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.into_bytes()
}

fn render_detail(output: &mut String, entry: &HelpEntry) {
    output.push_str(&format!("{} {}\n", kind_name(entry.kind), entry.name));
    match &entry.namespace {
        HelpNamespace::Alias { canonical_name, .. } => {
            output.push_str(&format!("  target: {canonical_name}\n"));
        }
        HelpNamespace::Reserved {
            introduced_major,
            purpose,
            replacement,
        } => {
            output.push_str(&format!("  introduced major: {introduced_major}\n"));
            output.push_str(&format!("  purpose: {purpose}\n"));
            if let Some(replacement) = replacement {
                output.push_str(&format!("  replacement: {replacement}\n"));
            }
        }
        HelpNamespace::Core { .. } | HelpNamespace::Function => {}
    }
    match &entry.signature {
        HelpSignature::Function(_) => {
            output.push_str(&format!(
                "  signature: {}\n",
                signature_text(&entry.signature)
            ));
            if let Some(definition) = &entry.definition {
                output.push_str(&format!(
                    "  defined at: {}:{}:{}\n",
                    definition.source, definition.line, definition.column
                ));
            }
        }
        HelpSignature::Builtin(signature) => {
            output.push_str(&format!(
                "  invocation: {}\n",
                signature.documentation().invocation()
            ));
            output.push_str(&format!(
                "  input: {}\n",
                signature
                    .inputs()
                    .map(carrier_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            output.push_str(&format!(
                "  output: {}\n",
                match signature.output() {
                    CommandOutput::Fixed(carrier) => carrier_name(carrier),
                    CommandOutput::SameAsInput => "same as input",
                }
            ));
            let arguments = signature.arguments();
            output.push_str(&format!(
                "  positionals: {}\n",
                positional_range(arguments.minimum(), arguments.maximum())
            ));
            let flags = signature.flags().collect::<Vec<_>>();
            if !flags.is_empty() {
                output.push_str(&format!("  flags: {}\n", flags.join(", ")));
            }
        }
        HelpSignature::Reserved => {}
    }
    match &entry.namespace {
        HelpNamespace::Core { lifecycle } | HelpNamespace::Alias { lifecycle, .. } => {
            if let Some(release) = lifecycle.deprecated_since_release() {
                output.push_str(&format!("  deprecated since: {release}\n"));
                if let Some(replacement) = lifecycle.replacement() {
                    output.push_str(&format!("  replacement: {replacement}\n"));
                }
            }
        }
        HelpNamespace::Reserved { .. } | HelpNamespace::Function => {}
    }
    output.push_str(&format!("  summary: {}\n", summary(entry)));
    if !entry.documentation.is_empty() {
        output.push_str("  details:\n");
        for line in entry.documentation.text().lines() {
            output.push_str("    ");
            output.push_str(line);
            output.push('\n');
        }
    }
}

fn positional_range(minimum: usize, maximum: Option<usize>) -> String {
    match maximum {
        Some(maximum) if minimum == maximum => minimum.to_string(),
        Some(maximum) => format!("{minimum}..={maximum}"),
        None => format!("{minimum}.."),
    }
}

fn signature_text(signature: &HelpSignature) -> String {
    match signature {
        HelpSignature::Builtin(signature) => signature.documentation().invocation().to_owned(),
        HelpSignature::Reserved => "-".to_owned(),
        HelpSignature::Function(signature) => {
            let parameters = signature
                .parameters()
                .iter()
                .map(|parameter| format!("{}: {}", parameter.name(), parameter.value_type()))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "def {}({parameters}) -> {}",
                signature.name(),
                signature.result()
            )
        }
    }
}

fn list_summary(entry: &HelpEntry) -> String {
    let mut rendered = summary(entry).to_owned();
    match &entry.namespace {
        HelpNamespace::Core { lifecycle } => append_lifecycle(&mut rendered, lifecycle),
        HelpNamespace::Alias {
            canonical_name,
            lifecycle,
        } => {
            rendered.push_str(&format!(" (alias for {canonical_name})"));
            append_lifecycle(&mut rendered, lifecycle);
        }
        HelpNamespace::Reserved {
            replacement: Some(replacement),
            ..
        } => rendered.push_str(&format!(" (replacement: {replacement})")),
        HelpNamespace::Reserved {
            replacement: None, ..
        }
        | HelpNamespace::Function => {}
    }
    rendered
}

fn append_lifecycle(rendered: &mut String, lifecycle: &CommandLifecycle) {
    let Some(release) = lifecycle.deprecated_since_release() else {
        return;
    };
    rendered.push_str(&format!(" (deprecated since {release}"));
    if let Some(replacement) = lifecycle.replacement() {
        rendered.push_str(&format!("; use {replacement}"));
    }
    rendered.push(')');
}

fn summary(entry: &HelpEntry) -> &str {
    let summary = entry.documentation.summary();
    if summary.is_empty() {
        "undocumented"
    } else {
        summary
    }
}

const fn kind_name(kind: HelpKind) -> &'static str {
    match kind {
        HelpKind::Builtin => "builtin",
        HelpKind::Alias => "alias",
        HelpKind::Reserved => "reserved",
        HelpKind::Function => "function",
    }
}

const fn carrier_name(carrier: Carrier) -> &'static str {
    match carrier {
        Carrier::Empty => "Empty",
        Carrier::ByteStream => "ByteStream",
        Carrier::Value => "Value",
        Carrier::ValueStream => "ValueStream",
    }
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::fmt;
    use std::sync::Arc;

    use flash_syntax::{ParseOutcome, SourceId, parse};

    use super::*;
    use crate::command::{CommandLifecycle, CommandNamespaceEntry};
    use crate::module::RuntimeBindingTypes;
    use crate::{BindingMutability, Callable, Value};

    #[derive(Debug)]
    struct InspectedCallable(FunctionInspection);

    impl Callable for InspectedCallable {
        fn family(&self) -> &'static str {
            "function"
        }

        fn display(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("<test function>")
        }

        fn inspection(&self) -> Option<&FunctionInspection> {
            Some(&self.0)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug)]
    struct OpaqueCallable;

    impl Callable for OpaqueCallable {
        fn family(&self) -> &'static str {
            "closure"
        }

        fn display(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("<test closure>")
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn inspected(source: &SourceFile, name: &str) -> FunctionInspection {
        let ParseOutcome::Complete(script) = parse(source) else {
            panic!("test function source must parse");
        };
        let types = RuntimeBindingTypes::analyze_source(source, &script).expect("analyze types");
        let name_start = source
            .text()
            .find(&format!("def {name}"))
            .expect("function definition")
            + "def ".len();
        let span = source
            .span(name_start..name_start + name.len())
            .expect("name span");
        FunctionInspection::new(
            types
                .function_signature(source.id(), span)
                .expect("function signature")
                .clone(),
            source,
        )
    }

    #[test]
    fn catalog_uses_visible_binding_shadowing_and_omits_opaque_callables() {
        let target_source = SourceFile::new(
            SourceId::new(801),
            "library.fsh",
            "## Imported target.\ndef target() -> Int { 1 }\n",
        );
        let other_source =
            SourceFile::new(SourceId::new(802), "main.fsh", "def other() { null }\n");
        let mut scope = ScopeStack::new();
        scope
            .declare(
                "target",
                BindingMutability::Immutable,
                Value::Callable(Arc::new(InspectedCallable(inspected(
                    &target_source,
                    "target",
                )))),
            )
            .unwrap();
        scope
            .declare(
                "other",
                BindingMutability::Immutable,
                Value::Callable(Arc::new(InspectedCallable(inspected(
                    &other_source,
                    "other",
                )))),
            )
            .unwrap();
        scope
            .declare(
                "closure",
                BindingMutability::Immutable,
                Value::Callable(Arc::new(OpaqueCallable)),
            )
            .unwrap();

        let visible = HelpCatalog::snapshot(&CommandRegistry::new(), &scope);
        assert_eq!(
            visible
                .entries()
                .iter()
                .map(HelpEntry::name)
                .collect::<Vec<_>>(),
            ["other", "target"]
        );
        let target = visible.query(Some("target"));
        assert_eq!(target[0].documentation().summary(), "Imported target.");
        assert_eq!(target[0].definition().unwrap().source(), "library.fsh");

        scope.push();
        scope
            .declare("target", BindingMutability::Immutable, Value::Int(7))
            .unwrap();
        let shadowed = HelpCatalog::snapshot(&CommandRegistry::new(), &scope);
        assert!(shadowed.query(Some("target")).is_empty());
        assert_eq!(
            shadowed
                .entries()
                .iter()
                .map(HelpEntry::name)
                .collect::<Vec<_>>(),
            ["other"]
        );
    }

    #[test]
    fn namespace_help_exposes_classes_lifecycle_targets_order_and_exact_queries() {
        let signature = |name: &str, invocation: &str, summary: &str| {
            CommandSignature::new(name, [Carrier::Empty], Carrier::Value)
                .with_flags(["--all"])
                .with_documentation(CommandDocumentation::new(
                    invocation,
                    Documentation::new(summary),
                ))
        };
        let registry = CommandRegistry::try_from_entries(
            1,
            [
                CommandNamespaceEntry::core(
                    signature("inspect", "inspect [--all]", "Inspect values."),
                    CommandLifecycle::introduced(1),
                ),
                CommandNamespaceEntry::core(
                    signature("old", "old [--all]", "Legacy inspection."),
                    CommandLifecycle::introduced(1)
                        .deprecated_since("0.9.0")
                        .with_replacement("inspect"),
                ),
                CommandNamespaceEntry::alias(
                    "show",
                    "inspect",
                    CommandLifecycle::introduced(1)
                        .deprecated_since("0.9.0")
                        .with_replacement("inspect"),
                ),
                CommandNamespaceEntry::reserved(
                    "future",
                    1,
                    "reserved for a future inspector",
                    Some("inspect"),
                ),
            ],
        )
        .expect("valid help namespace");

        let catalog = HelpCatalog::snapshot(&registry, &ScopeStack::new());
        assert_eq!(
            catalog
                .entries()
                .iter()
                .map(|entry| (entry.name(), entry.kind()))
                .collect::<Vec<_>>(),
            [
                ("future", HelpKind::Reserved),
                ("inspect", HelpKind::Builtin),
                ("old", HelpKind::Builtin),
                ("show", HelpKind::Alias),
            ]
        );

        let show = catalog.query(Some("show"));
        assert_eq!(show.len(), 1);
        assert!(matches!(
            show[0].namespace(),
            HelpNamespace::Alias {
                canonical_name,
                lifecycle,
            } if canonical_name == "inspect"
                && lifecycle.deprecated_since_release() == Some("0.9.0")
                && lifecycle.replacement() == Some("inspect")
        ));
        let HelpSignature::Builtin(alias_signature) = show[0].signature() else {
            panic!("an alias reuses its canonical built-in signature");
        };
        assert_eq!(alias_signature.name(), "inspect");
        assert_eq!(alias_signature.flags().collect::<Vec<_>>(), ["--all"]);

        let old = catalog.query(Some("old"));
        assert!(matches!(
            old[0].namespace(),
            HelpNamespace::Core { lifecycle }
                if lifecycle.deprecated_since_release() == Some("0.9.0")
                    && lifecycle.replacement() == Some("inspect")
        ));

        let rendered = String::from_utf8(render_help(&HelpSnapshot::new(
            catalog.entries().to_vec(),
            false,
        )))
        .expect("help is UTF-8");
        assert!(rendered.contains("future\treserved\t"));
        assert!(rendered.contains("show\talias\t"));
        assert!(rendered.contains("alias for inspect"));
        assert!(rendered.contains("deprecated since 0.9.0; use inspect"));

        let future = catalog.query(Some("future"));
        assert_eq!(future.len(), 1);
        assert!(matches!(future[0].signature(), HelpSignature::Reserved));
        assert!(matches!(
            future[0].namespace(),
            HelpNamespace::Reserved {
                introduced_major: 1,
                purpose,
                replacement: Some(replacement),
            } if purpose == "reserved for a future inspector" && replacement == "inspect"
        ));
        let detail = String::from_utf8(render_help(&HelpSnapshot::new(future, true)))
            .expect("help is UTF-8");
        assert!(detail.contains("reserved future\n"));
        assert!(detail.contains("purpose: reserved for a future inspector\n"));
        assert!(detail.contains("replacement: inspect\n"));
    }
}
