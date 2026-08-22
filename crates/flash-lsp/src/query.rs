//! Current-generation protocol request projection over shared language data.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use flash_runtime::builtin::standard_registry;
use flash_runtime::command::{
    CommandArgumentKind, CommandArgumentSchema, CommandDynamicTail, CommandOptionTerminator,
    NamespaceClass,
};
use flash_runtime::module::{
    AnalysisControl, FunctionSignature, ModuleAnalysisOutcome, ModuleEffect, ModuleProgram,
    ModuleProgramLoader,
};
use flash_runtime::query::{NameKind, SemanticHover, SourceLocation};
use flash_syntax::{
    CompletionContext, FormatOutcome, PositionEncoding, SourceFile, SourceId, TextPosition,
    TextRange, completion_target, format_source,
};
use serde_json::{Value, json};

use crate::uri::DocumentUri;
use crate::workspace::{OpenDocument, Workspace, WorkspaceSnapshot};

/// Cooperative cancellation state owned by one protocol request ID.
#[derive(Clone, Debug, Default)]
pub struct RequestControl {
    cancelled: Arc<AtomicBool>,
    invalidated: Arc<AtomicBool>,
}

impl RequestControl {
    /// Creates an active request control.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the request as explicitly cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Stops analysis because a newer workspace generation superseded it.
    pub(crate) fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
    }

    /// Whether explicit cancellation or workspace invalidation has been observed.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.invalidated.load(Ordering::Acquire)
    }

    pub(crate) fn was_explicitly_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn analysis_control(&self) -> AnalysisControl {
        let cancelled = Arc::clone(&self.cancelled);
        let invalidated = Arc::clone(&self.invalidated);
        AnalysisControl::cooperative(move || {
            cancelled.load(Ordering::Acquire) || invalidated.load(Ordering::Acquire)
        })
    }
}

/// The only protocol errors produced by a supported language request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestError {
    RequestCancelled,
    ContentModified,
    InvalidParams,
}

impl RequestError {
    /// The standard JSON-RPC/LSP error code for this request outcome.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::RequestCancelled => -32_800,
            Self::ContentModified => -32_801,
            Self::InvalidParams => -32_602,
        }
    }

    /// The stable standard error message paired with [`Self::code`].
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::RequestCancelled => "Request cancelled",
            Self::ContentModified => "Content modified",
            Self::InvalidParams => "Invalid params",
        }
    }
}

/// One computed response waiting at the current-generation publication barrier.
#[derive(Clone, Debug)]
pub struct PreparedResponse {
    generation: u64,
    control: RequestControl,
    outcome: Result<Value, RequestError>,
}

impl PreparedResponse {
    /// Applies cancellation and workspace-generation precedence before reply.
    pub fn finish(&self, workspace: &Workspace) -> Result<Value, RequestError> {
        if self.control.was_explicitly_cancelled() {
            return Err(RequestError::RequestCancelled);
        }
        if self.generation != workspace.generation() {
            return Err(RequestError::ContentModified);
        }
        self.outcome.clone()
    }
}

/// Computes one supported request against an immutable workspace generation.
///
/// Worker/coordinator scheduling is deliberately outside this module. The
/// returned value cannot cross the output boundary until [`PreparedResponse::finish`]
/// validates it against the live workspace.
#[must_use]
pub fn prepare_request(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: RequestControl,
    method: &str,
    params: &Value,
) -> PreparedResponse {
    let outcome = if control.is_cancelled() {
        Err(RequestError::RequestCancelled)
    } else {
        dispatch(snapshot, encoding, &control, method, params)
    };
    PreparedResponse {
        generation: snapshot.generation(),
        control,
        outcome,
    }
}

fn dispatch(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    method: &str,
    params: &Value,
) -> Result<Value, RequestError> {
    match method {
        "textDocument/completion" => completion(snapshot, encoding, control, params),
        "textDocument/hover" => hover(snapshot, encoding, control, params),
        "textDocument/signatureHelp" => signature_help(snapshot, encoding, control, params),
        "textDocument/definition" => definition(snapshot, encoding, control, params),
        "textDocument/references" => references(snapshot, encoding, control, params),
        "textDocument/formatting" => formatting(snapshot, encoding, control, params),
        _ => Err(RequestError::InvalidParams),
    }
}

fn completion(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    params: &Value,
) -> Result<Value, RequestError> {
    let position = parse_position(params)?;
    let Some((_, document)) = open_document(snapshot, params)? else {
        return Ok(json!([]));
    };
    let source = request_source(document);
    let cursor = source
        .byte_offset(position, encoding)
        .map_err(|_| RequestError::InvalidParams)?;
    let Some(target) = completion_target(source.text(), cursor) else {
        return Ok(json!([]));
    };
    let replacement = TextRange::new(
        source
            .text_position(target.replacement().start, encoding)
            .map_err(|_| RequestError::InvalidParams)?,
        source
            .text_position(target.replacement().end, encoding)
            .map_err(|_| RequestError::InvalidParams)?,
    );
    let commands = standard_registry();
    let mut candidates = Vec::new();

    match target.context() {
        CompletionContext::CommandSubstitutionModifier => {
            for modifier in ["bytes:", "text:"] {
                if modifier.starts_with(target.prefix()) {
                    candidates.push(CompletionCandidate::new(modifier, modifier, 0, 14));
                }
            }
        }
        CompletionContext::Command {
            forced_external: false,
        } => {
            for entry in commands.namespace_entries() {
                if !entry.name().starts_with(target.prefix()) {
                    continue;
                }
                if matches!(entry.class(), NamespaceClass::Core | NamespaceClass::Alias) {
                    candidates.push(CompletionCandidate::new(entry.name(), entry.name(), 0, 3));
                }
            }
        }
        CompletionContext::Flag { command } => {
            if let Some(signature) = commands.lookup(command) {
                for flag in signature
                    .flags()
                    .filter(|flag| flag.starts_with(target.prefix()))
                {
                    candidates.push(CompletionCandidate::new(flag, flag, 2, 14));
                }
            }
        }
        CompletionContext::Command {
            forced_external: true,
        }
        | CompletionContext::Expression
        | CompletionContext::Variable
        | CompletionContext::Path
        | CompletionContext::None => {}
    }

    if matches!(
        target.context(),
        CompletionContext::Command {
            forced_external: false
        } | CompletionContext::Expression
            | CompletionContext::Variable
    ) && let Some(report) = analyze(snapshot, document, &commands, control)?
        && let Some(program) = report.program()
        && let Some(module) = module_for_document(program, document)
        && let Some(names) = program
            .semantic_queries(&commands)
            .visible_names(module, cursor)
    {
        let prefix = target.prefix().strip_prefix('$').unwrap_or(target.prefix());
        for name in names
            .into_iter()
            .filter(|name| name.name().starts_with(prefix))
        {
            match target.context() {
                CompletionContext::Command { .. }
                    if matches!(
                        name.kind(),
                        NameKind::Intrinsic | NameKind::Function | NameKind::ImportedFunction
                    ) =>
                {
                    candidates.push(CompletionCandidate::new(name.name(), name.name(), 1, 3));
                }
                CompletionContext::Expression
                    if matches!(
                        name.kind(),
                        NameKind::Intrinsic | NameKind::Function | NameKind::ImportedFunction
                    ) =>
                {
                    candidates.push(CompletionCandidate::new(name.name(), name.name(), 1, 3));
                }
                CompletionContext::Variable if name.kind() != NameKind::Intrinsic => {
                    candidates.push(CompletionCandidate::new(
                        format!("${}", name.name()),
                        name.name(),
                        1,
                        if matches!(name.kind(), NameKind::Function | NameKind::ImportedFunction) {
                            3
                        } else {
                            6
                        },
                    ));
                }
                _ => {}
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.class
            .cmp(&right.class)
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut seen = BTreeSet::new();
    let items = candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.insert.clone()))
        .map(|candidate| {
            json!({
                "label": candidate.insert,
                "kind": candidate.kind,
                "sortText": format!("{}:{}", candidate.class, candidate.name),
                "textEdit": {
                    "range": range_value(replacement),
                    "newText": candidate.insert
                }
            })
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(items))
}

fn hover(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    params: &Value,
) -> Result<Value, RequestError> {
    let Some((document, cursor)) = positional_document(snapshot, encoding, params)? else {
        return Ok(Value::Null);
    };
    let commands = standard_registry();
    let Some(report) = analyze(snapshot, document, &commands, control)? else {
        return Ok(Value::Null);
    };
    let Some(program) = report.program() else {
        return Ok(Value::Null);
    };
    let Some(module) = module_for_document(program, document) else {
        return Ok(Value::Null);
    };
    let queries = program.semantic_queries(&commands);
    let markdown = if let Some(effects) = queries.import_effects_at(module, cursor) {
        render_effects(effects.direct(), effects.transitive())
    } else {
        match queries.hover_at(module, cursor) {
            Some(SemanticHover::Intrinsic(hover)) => {
                let intrinsic = hover.intrinsic();
                format!(
                    "```flash\n{}({}: {}) -> {}\n```\n\n{}",
                    intrinsic.name(),
                    intrinsic.parameter_name(),
                    intrinsic.parameter_type_label(),
                    intrinsic.result_type(),
                    intrinsic.documentation(),
                )
            }
            Some(SemanticHover::DynamicBinding(hover)) => {
                let binding = hover.binding();
                format!(
                    "```flash\ndynamic ${}: {}\n```\n\n{}",
                    binding.name(),
                    binding.result_type(),
                    binding.documentation(),
                )
            }
            Some(SemanticHover::Binding(binding)) => {
                format!(
                    "```flash\nlet {}: {}\n```",
                    binding.name(),
                    binding.value_type()
                )
            }
            Some(SemanticHover::Function(function)) => {
                let mut markdown = format!(
                    "```flash\ndef {}\n```",
                    function_label(function.signature())
                );
                append_documentation(
                    &mut markdown,
                    function.signature().documentation().map(|docs| docs.text()),
                );
                markdown
            }
            Some(SemanticHover::Command(command)) => {
                let documentation = command.signature().documentation();
                let mut markdown = format!("```flash\n{}\n```", documentation.invocation());
                append_command_schema(&mut markdown, command.signature().arguments());
                append_documentation(&mut markdown, Some(documentation.documentation().text()));
                markdown
            }
            None => return Ok(Value::Null),
        }
    };
    Ok(json!({"contents": {"kind": "markdown", "value": markdown}}))
}

fn signature_help(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    params: &Value,
) -> Result<Value, RequestError> {
    let Some((document, cursor)) = positional_document(snapshot, encoding, params)? else {
        return Ok(Value::Null);
    };
    let commands = standard_registry();
    let Some(report) = analyze(snapshot, document, &commands, control)? else {
        return Ok(Value::Null);
    };
    let Some(program) = report.program() else {
        return Ok(Value::Null);
    };
    let Some(module) = module_for_document(program, document) else {
        return Ok(Value::Null);
    };
    let queries = program.semantic_queries(&commands);
    if let Some(signature) = queries.signature_at(module, cursor) {
        let parameters = signature
            .signature()
            .parameters()
            .iter()
            .map(|parameter| {
                json!({"label": format!("{}: {}", parameter.name(), parameter.value_type())})
            })
            .collect::<Vec<_>>();
        let documentation = signature
            .signature()
            .documentation()
            .filter(|documentation| !documentation.is_empty())
            .map(|documentation| json!({"kind": "markdown", "value": documentation.text()}));
        return Ok(json!({
            "signatures": [{
                "label": function_label(signature.signature()),
                "documentation": documentation,
                "parameters": parameters
            }],
            "activeSignature": 0,
            "activeParameter": signature.active_parameter()
        }));
    }
    if let Some(signature) = queries.intrinsic_signature_at(module, cursor) {
        let intrinsic = signature.intrinsic();
        return Ok(json!({
            "signatures": [{
                "label": format!(
                    "{}({}: {}) -> {}",
                    intrinsic.name(),
                    intrinsic.parameter_name(),
                    intrinsic.parameter_type_label(),
                    intrinsic.result_type(),
                ),
                "documentation": {
                    "kind": "markdown",
                    "value": intrinsic.documentation(),
                },
                "parameters": [{
                    "label": format!(
                        "{}: {}",
                        intrinsic.parameter_name(),
                        intrinsic.parameter_type_label(),
                    )
                }]
            }],
            "activeSignature": 0,
            "activeParameter": signature.active_parameter(),
        }));
    }
    let Some(signature) = queries.command_signature_at(module, cursor) else {
        return Ok(Value::Null);
    };
    let command = signature.command();
    let schema = command.signature().arguments();
    let parameters = command_parameters(schema);
    let active_parameter = signature
        .active_parameter()
        .min(parameters.len().saturating_sub(1));
    let mut documentation = String::new();
    append_command_schema(&mut documentation, schema);
    append_documentation(
        &mut documentation,
        Some(command.signature().documentation().documentation().text()),
    );
    Ok(json!({
        "signatures": [{
            "label": command.signature().documentation().invocation(),
            "documentation": {"kind": "markdown", "value": documentation},
            "parameters": parameters
        }],
        "activeSignature": 0,
        "activeParameter": active_parameter
    }))
}

fn definition(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    params: &Value,
) -> Result<Value, RequestError> {
    let Some((document, cursor)) = positional_document(snapshot, encoding, params)? else {
        return Ok(Value::Null);
    };
    let commands = standard_registry();
    let Some(report) = analyze(snapshot, document, &commands, control)? else {
        return Ok(Value::Null);
    };
    let Some(program) = report.program() else {
        return Ok(Value::Null);
    };
    let Some(module) = module_for_document(program, document) else {
        return Ok(Value::Null);
    };
    let Some(location) = program
        .semantic_queries(&commands)
        .definition_at(module, cursor)
    else {
        return Ok(Value::Null);
    };
    Ok(location_value(snapshot, program, &location, encoding).unwrap_or(Value::Null))
}

fn references(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    params: &Value,
) -> Result<Value, RequestError> {
    let include_declaration = params
        .get("context")
        .and_then(Value::as_object)
        .and_then(|context| context.get("includeDeclaration"))
        .and_then(Value::as_bool)
        .ok_or(RequestError::InvalidParams)?;
    let Some((document, cursor)) = positional_document(snapshot, encoding, params)? else {
        return Ok(json!([]));
    };
    let commands = standard_registry();
    let reports = analyze_all(snapshot, &commands, control)?;
    let Some(requested_index) = reports.iter().position(|report| {
        report
            .program()
            .is_some_and(|program| program.graph().root().path() == document.module_path())
    }) else {
        return Ok(json!([]));
    };
    let requested = reports[requested_index]
        .program()
        .expect("the selected request report has a complete program");
    let module = requested.graph().root();
    let Some(definition) = requested
        .semantic_queries(&commands)
        .definition_at(module, cursor)
    else {
        return Ok(json!([]));
    };

    let mut locations = Vec::new();
    let report_order = std::iter::once(&reports[requested_index]).chain(
        reports
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != requested_index)
            .map(|(_, report)| report),
    );
    for report in report_order {
        let Some(program) = report.program() else {
            continue;
        };
        let Some(module) = program
            .sources()
            .entries()
            .find(|entry| entry.module().path() == definition.module().path())
            .map(|entry| entry.module())
        else {
            continue;
        };
        let queries = program.semantic_queries(&commands);
        let Some(local_definition) = queries.definition_at(module, definition.span().start())
        else {
            continue;
        };
        if local_definition.module().path() != definition.module().path()
            || local_definition.span().start() != definition.span().start()
            || local_definition.span().end() != definition.span().end()
        {
            continue;
        }
        for location in queries.references_to(&local_definition, include_declaration) {
            let Some(location) = location_value(snapshot, program, &location, encoding) else {
                continue;
            };
            if !locations.contains(&location) {
                locations.push(location);
            }
        }
    }
    Ok(Value::Array(locations))
}

fn formatting(
    snapshot: &WorkspaceSnapshot,
    encoding: PositionEncoding,
    control: &RequestControl,
    params: &Value,
) -> Result<Value, RequestError> {
    if !params
        .get("options")
        .is_some_and(|options| options.is_object())
    {
        return Err(RequestError::InvalidParams);
    }
    let Some((_, document)) = open_document(snapshot, params)? else {
        return Ok(json!([]));
    };
    if control.is_cancelled() {
        return Err(RequestError::RequestCancelled);
    }
    let source = request_source(document);
    let FormatOutcome::Complete(formatted) = format_source(&source) else {
        return Ok(json!([]));
    };
    if control.is_cancelled() {
        return Err(RequestError::RequestCancelled);
    }
    if formatted == source.text() {
        return Ok(json!([]));
    }
    let range = TextRange::new(
        TextPosition::new(0, 0),
        source
            .text_position(source.len(), encoding)
            .map_err(|_| RequestError::InvalidParams)?,
    );
    Ok(json!([{"range": range_value(range), "newText": formatted}]))
}

fn positional_document<'a>(
    snapshot: &'a WorkspaceSnapshot,
    encoding: PositionEncoding,
    params: &Value,
) -> Result<Option<(&'a OpenDocument, usize)>, RequestError> {
    let position = parse_position(params)?;
    let Some((_, document)) = open_document(snapshot, params)? else {
        return Ok(None);
    };
    let source = request_source(document);
    let cursor = source
        .byte_offset(position, encoding)
        .map_err(|_| RequestError::InvalidParams)?;
    Ok(Some((document, cursor)))
}

fn parse_position(params: &Value) -> Result<TextPosition, RequestError> {
    let position = params
        .get("position")
        .and_then(Value::as_object)
        .ok_or(RequestError::InvalidParams)?;
    let line = usize::try_from(
        position
            .get("line")
            .and_then(Value::as_u64)
            .ok_or(RequestError::InvalidParams)?,
    )
    .map_err(|_| RequestError::InvalidParams)?;
    let character = usize::try_from(
        position
            .get("character")
            .and_then(Value::as_u64)
            .ok_or(RequestError::InvalidParams)?,
    )
    .map_err(|_| RequestError::InvalidParams)?;
    Ok(TextPosition::new(line, character))
}

fn open_document<'a>(
    snapshot: &'a WorkspaceSnapshot,
    params: &Value,
) -> Result<Option<(DocumentUri, &'a OpenDocument)>, RequestError> {
    let uri = params
        .get("textDocument")
        .and_then(Value::as_object)
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
        .ok_or(RequestError::InvalidParams)?;
    let Ok(uri) = DocumentUri::parse(uri) else {
        return Ok(None);
    };
    Ok(snapshot.document(&uri).map(|document| (uri, document)))
}

fn request_source(document: &OpenDocument) -> SourceFile {
    SourceFile::new(SourceId::new(0), document.uri().as_str(), document.text())
}

fn analyze(
    snapshot: &WorkspaceSnapshot,
    document: &OpenDocument,
    commands: &flash_runtime::command::CommandRegistry,
    control: &RequestControl,
) -> Result<Option<Box<flash_runtime::module::ModuleAnalysisReport>>, RequestError> {
    let loader = ModuleProgramLoader::new(snapshot, snapshot);
    match loader.analyze_with_commands_controlled(
        document.module_path(),
        commands,
        &control.analysis_control(),
    ) {
        ModuleAnalysisOutcome::Complete(report) => Ok(Some(report)),
        ModuleAnalysisOutcome::Cancelled => Err(RequestError::RequestCancelled),
    }
}

fn analyze_all(
    snapshot: &WorkspaceSnapshot,
    commands: &flash_runtime::command::CommandRegistry,
    control: &RequestControl,
) -> Result<Vec<flash_runtime::module::ModuleAnalysisReport>, RequestError> {
    let loader = ModuleProgramLoader::new(snapshot, snapshot);
    let mut reports = Vec::with_capacity(snapshot.roots().len());
    for root in snapshot.roots() {
        match loader.analyze_with_commands_controlled(
            root.module_path(),
            commands,
            &control.analysis_control(),
        ) {
            ModuleAnalysisOutcome::Complete(report) => reports.push(*report),
            ModuleAnalysisOutcome::Cancelled => return Err(RequestError::RequestCancelled),
        }
    }
    Ok(reports)
}

fn module_for_document<'a>(
    program: &'a ModuleProgram,
    document: &OpenDocument,
) -> Option<&'a flash_runtime::module::ModuleId> {
    program
        .sources()
        .entries()
        .find(|entry| entry.module().path() == document.module_path())
        .map(|entry| entry.module())
}

fn location_value(
    snapshot: &WorkspaceSnapshot,
    program: &ModuleProgram,
    location: &SourceLocation,
    encoding: PositionEncoding,
) -> Option<Value> {
    let source = program.sources().source(location.module())?;
    let range = source.text_range(location.span(), encoding).ok()?;
    let uri = snapshot.uri_for_module(location.module()).ok()?;
    Some(json!({"uri": uri.as_str(), "range": range_value(range)}))
}

fn range_value(range: TextRange) -> Value {
    json!({
        "start": {
            "line": range.start().line(),
            "character": range.start().character()
        },
        "end": {
            "line": range.end().line(),
            "character": range.end().character()
        }
    })
}

fn function_label(signature: &FunctionSignature) -> String {
    let parameters = signature
        .parameters()
        .iter()
        .map(|parameter| format!("{}: {}", parameter.name(), parameter.value_type()))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}({parameters}) -> {}",
        signature.name(),
        signature.result()
    )
}

fn command_parameters(schema: &CommandArgumentSchema) -> Vec<Value> {
    let count = schema.maximum().unwrap_or_else(|| schema.minimum().max(1));
    (0..count)
        .map(|position| {
            let variadic = schema.maximum().is_none() && position + 1 == count;
            let suffix = if variadic { "..." } else { "" };
            json!({
                "label": format!(
                    "argument {}{}: {}",
                    position + 1,
                    suffix,
                    command_argument_kind_label(schema.positional_kind(position)),
                )
            })
        })
        .collect()
}

fn append_command_schema(markdown: &mut String, schema: &CommandArgumentSchema) {
    if !markdown.is_empty() {
        markdown.push_str("\n\n");
    }
    let maximum = schema
        .maximum()
        .map_or_else(|| "unbounded".to_owned(), |maximum| maximum.to_string());
    markdown.push_str(&format!("Positionals: {}..={maximum}", schema.minimum()));

    let options = schema
        .options()
        .map(|option| {
            let mut label = format!("`{}`", option.name());
            for _ in 0..option.value_arity() {
                label.push_str(" VALUE");
            }
            if option.is_repeatable() {
                label.push_str(" (repeatable)");
            }
            let conflicts = option.conflicts().collect::<Vec<_>>();
            if !conflicts.is_empty() {
                label.push_str(&format!(" (conflicts with {})", conflicts.join(", ")));
            }
            label
        })
        .collect::<Vec<_>>();
    if !options.is_empty() {
        markdown.push_str("  \nOptions: ");
        markdown.push_str(&options.join(", "));
    }

    let terminator = match schema.terminator() {
        CommandOptionTerminator::Accepted => "accepted",
        CommandOptionTerminator::Literal => "literal",
    };
    let dynamic_tail = match schema.dynamic_tail() {
        CommandDynamicTail::DeferredToRuntime => "deferred until expansion",
        CommandDynamicTail::Rejected => "rejected",
    };
    markdown.push_str(&format!(
        "  \n`--`: {terminator}; unresolved spread: {dynamic_tail}."
    ));
}

const fn command_argument_kind_label(kind: CommandArgumentKind) -> &'static str {
    match kind {
        CommandArgumentKind::Word => "word",
        CommandArgumentKind::Closure => "closure",
        CommandArgumentKind::Any => "word or closure",
    }
}

fn append_documentation(markdown: &mut String, documentation: Option<&str>) {
    if let Some(documentation) = documentation.filter(|documentation| !documentation.is_empty()) {
        markdown.push_str("\n\n");
        markdown.push_str(documentation);
    }
}

fn render_effects(
    direct: &flash_runtime::module::ModuleEffectSummary,
    transitive: &flash_runtime::module::ModuleEffectSummary,
) -> String {
    format!(
        "**Module initializer effects**\n\nDirect: {}\n\nTransitive: {}",
        effect_list(direct),
        effect_list(transitive)
    )
}

fn effect_list(summary: &flash_runtime::module::ModuleEffectSummary) -> String {
    let mut effects = summary
        .occurrences()
        .iter()
        .map(|occurrence| effect_name(occurrence.effect()))
        .collect::<Vec<_>>();
    effects.sort_unstable();
    effects.dedup();
    if effects.is_empty() {
        "none".to_owned()
    } else {
        effects.join(", ")
    }
}

const fn effect_name(effect: ModuleEffect) -> &'static str {
    match effect {
        ModuleEffect::WorkingDirectory => "working directory",
        ModuleEffect::ChildEnvironment => "child environment",
        ModuleEffect::Status => "status",
        ModuleEffect::Output => "output",
        ModuleEffect::FilesystemRead => "filesystem read",
        ModuleEffect::FilesystemWrite => "filesystem write",
        ModuleEffect::Process => "process",
        ModuleEffect::Job => "job",
        ModuleEffect::ProgramExit => "program exit",
        ModuleEffect::OpaqueExternal => "opaque external",
    }
}

struct CompletionCandidate {
    insert: String,
    name: String,
    class: u8,
    kind: u8,
}

impl CompletionCandidate {
    fn new(insert: impl Into<String>, name: impl Into<String>, class: u8, kind: u8) -> Self {
        Self {
            insert: insert.into(),
            name: name.into(),
            class,
            kind,
        }
    }
}
