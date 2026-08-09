//! Host-free documentation metadata, help snapshots, and deterministic rendering.

use flash_syntax::SourceFile;

use crate::ScopeStack;
use crate::command::{Carrier, CommandOutput, CommandRegistry, CommandSignature};
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
    Function,
}

/// Structured signature metadata retained in one immutable help entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HelpSignature {
    Builtin(CommandSignature),
    Function(FunctionSignature),
}

/// One immutable built-in or visible named-function help result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpEntry {
    kind: HelpKind,
    name: String,
    signature: HelpSignature,
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
    /// Snapshots standard commands and currently visible named callables.
    #[must_use]
    pub fn snapshot(registry: &CommandRegistry, scope: &ScopeStack) -> Self {
        let mut entries = registry
            .signatures()
            .map(|signature| HelpEntry {
                kind: HelpKind::Builtin,
                name: signature.name().to_owned(),
                signature: HelpSignature::Builtin(signature.clone()),
                documentation: signature.documentation().documentation().clone(),
                definition: None,
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
            let summary = summary(entry);
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
            let flags = signature.flags().collect::<Vec<_>>();
            if !flags.is_empty() {
                output.push_str(&format!("  flags: {}\n", flags.join(", ")));
            }
        }
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

fn signature_text(signature: &HelpSignature) -> String {
    match signature {
        HelpSignature::Builtin(signature) => signature.documentation().invocation().to_owned(),
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
}
