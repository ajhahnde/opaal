#![forbid(unsafe_code)]

//! Deterministic, read-only analysis of explicit Flash v1 source graphs.

mod scan;
mod sha256;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use flash_syntax::{
    ControlledParseOutcome, ControlledVersionedParseOutcome, Delimiter, Keyword, LanguageDetection,
    ModuleImportSource, Operator, ParseOutcome, Script, SourceFile, SourceId, Span, StatementKind,
    TokenKind, VersionedParseOutcome, detect_source_language, lex, parse_v2_with_control,
    parse_with_control,
};

use crate::scan::{ReferenceKind, SourceScan, scan};

pub const SCHEMA_VERSION: u16 = 1;

/// Deterministic ceilings for one migration analysis and its rendered report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MigrationLimits {
    /// Distinct canonical sources across every explicit root.
    pub max_files: usize,
    /// Aggregate bytes read from distinct canonical sources.
    pub max_source_bytes: usize,
    /// Aggregate findings in the completed report.
    pub max_findings: usize,
    /// Aggregate UTF-8 replacement bytes in suggested edits.
    pub max_edit_bytes: usize,
    /// UTF-8 bytes in the selected complete rendered report.
    pub max_output_bytes: usize,
    /// Maximum of static-import depth and lexical/recursive syntax depth.
    pub max_nesting: usize,
    /// Aggregate path bytes, source bytes, lexer tokens, parser control polls,
    /// imports, analysis bytes, findings, edit bytes, and rendered bytes.
    pub max_work_units: usize,
}

impl Default for MigrationLimits {
    fn default() -> Self {
        Self {
            max_files: 4_096,
            max_source_bytes: 16 * 1024 * 1024,
            max_findings: 131_072,
            max_edit_bytes: 16 * 1024 * 1024,
            max_output_bytes: 64 * 1024 * 1024,
            max_nesting: 256,
            max_work_units: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationResource {
    Files,
    SourceBytes,
    Findings,
    EditBytes,
    OutputBytes,
    Nesting,
    WorkUnits,
}

impl MigrationResource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Files => "file-count",
            Self::SourceBytes => "source-byte",
            Self::Findings => "finding-count",
            Self::EditBytes => "edit-byte",
            Self::OutputBytes => "output-byte",
            Self::Nesting => "nesting",
            Self::WorkUnits => "work-unit",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MigrationFormat {
    #[default]
    Human,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FindingSeverity {
    Required,
    Suggested,
    Information,
}

impl FindingSeverity {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Suggested => "suggested",
            Self::Information => "information",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MigrationEdit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MigrationFinding {
    pub code: String,
    pub severity: FindingSeverity,
    pub start: usize,
    pub end: usize,
    pub message: String,
    pub edit: Option<MigrationEdit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationSource {
    pub source_uri: String,
    pub digest: String,
    pub detected_language: u16,
    pub target_language: u16,
    pub findings: Vec<MigrationFinding>,
    pub unresolved: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationReport {
    pub schema: u16,
    pub sources: Vec<MigrationSource>,
    work_units: usize,
}

impl MigrationReport {
    #[must_use = "migration report rendering can exceed its configured output budget"]
    pub fn render(&self, format: MigrationFormat) -> Result<String, MigrationError> {
        self.render_with_limits(format, &MigrationLimits::default())
    }

    #[must_use]
    pub fn exit_status(&self) -> u8 {
        if self.sources.iter().any(|source| {
            source.unresolved
                || source
                    .findings
                    .iter()
                    .any(|finding| finding.severity == FindingSeverity::Required)
        }) {
            1
        } else {
            0
        }
    }

    #[must_use = "migration report rendering can exceed its configured output budget"]
    pub fn render_with_limits(
        &self,
        format: MigrationFormat,
        limits: &MigrationLimits,
    ) -> Result<String, MigrationError> {
        let remaining_work = limits.max_work_units.saturating_sub(self.work_units);
        let mut counter = CountingWriter::new(limits.max_output_bytes.min(remaining_work));
        let _ = self.write_render(format, &mut counter);
        ensure_limit(
            MigrationResource::OutputBytes,
            limits.max_output_bytes,
            counter.len,
            "report",
        )?;
        checked_usage(
            MigrationResource::WorkUnits,
            self.work_units,
            counter.len,
            limits.max_work_units,
            "report",
        )?;
        let mut output = String::with_capacity(counter.len);
        self.write_render(format, &mut output)
            .expect("writing to a pre-sized String cannot fail");
        Ok(output)
    }

    fn write_render(&self, format: MigrationFormat, output: &mut impl fmt::Write) -> fmt::Result {
        match format {
            MigrationFormat::Human => self.write_human(output),
            MigrationFormat::Json => self.write_json(output),
        }
    }

    fn write_human(&self, output: &mut impl fmt::Write) -> fmt::Result {
        let mut first = true;
        for source in &self.sources {
            if source.findings.is_empty() {
                if !first {
                    output.write_char('\n')?;
                }
                write!(output, "{}: clean", source.source_uri)?;
                first = false;
                continue;
            }
            for finding in &source.findings {
                if !first {
                    output.write_char('\n')?;
                }
                write!(
                    output,
                    "{}:{}:{}: {} {}: {}",
                    source.source_uri,
                    finding.start,
                    finding.end,
                    finding.severity.as_str(),
                    finding.code,
                    finding.message
                )?;
                first = false;
            }
        }
        Ok(())
    }

    fn write_json(&self, output: &mut impl fmt::Write) -> fmt::Result {
        write!(output, "{{\"schema\":{},\"sources\":[", self.schema)?;
        for (source_index, source) in self.sources.iter().enumerate() {
            if source_index != 0 {
                output.write_char(',')?;
            }
            output.write_str("{\"source_uri\":")?;
            write_json_string(output, &source.source_uri)?;
            output.write_str(",\"digest\":")?;
            write_json_string(output, &source.digest)?;
            write!(
                output,
                ",\"detected_language\":{},\"target_language\":{},\"findings\":[",
                source.detected_language, source.target_language
            )?;
            for (finding_index, finding) in source.findings.iter().enumerate() {
                if finding_index != 0 {
                    output.write_char(',')?;
                }
                output.write_str("{\"code\":")?;
                write_json_string(output, &finding.code)?;
                output.write_str(",\"severity\":")?;
                write_json_string(output, finding.severity.as_str())?;
                write!(
                    output,
                    ",\"start\":{},\"end\":{},\"message\":",
                    finding.start, finding.end
                )?;
                write_json_string(output, &finding.message)?;
                if let Some(edit) = &finding.edit {
                    write!(
                        output,
                        ",\"edit\":{{\"start\":{},\"end\":{},\"replacement\":",
                        edit.start, edit.end
                    )?;
                    write_json_string(output, &edit.replacement)?;
                    output.write_char('}')?;
                }
                output.write_char('}')?;
            }
            write!(output, "],\"unresolved\":{}}}", source.unresolved)?;
        }
        output.write_str("]}")
    }
}

struct CountingWriter {
    len: usize,
    ceiling: usize,
}

impl CountingWriter {
    const fn new(ceiling: usize) -> Self {
        Self { len: 0, ceiling }
    }
}

impl fmt::Write for CountingWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let observed = self.len.saturating_add(value.len());
        if observed > self.ceiling {
            self.len = self.ceiling.saturating_add(1);
            Err(fmt::Error)
        } else {
            self.len = observed;
            Ok(())
        }
    }
}

fn write_json_string(output: &mut impl fmt::Write, value: &str) -> fmt::Result {
    let mut adapter = JsonWriter { output };
    serde_json::to_writer(&mut adapter, value).map_err(|_| fmt::Error)
}

struct JsonWriter<'writer, Writer> {
    output: &'writer mut Writer,
}

impl<Writer: fmt::Write> io::Write for JsonWriter<'_, Writer> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let value = std::str::from_utf8(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.output
            .write_str(value)
            .map_err(|_| io::Error::other("JSON output rejected rendered UTF-8"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub trait SourceReader {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String>;
    /// Reads at most `max_bytes + 1` bytes so the analyzer can distinguish an
    /// exact boundary from the first excess without allocating the full input.
    fn read(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSourceReader;

impl SourceReader for NativeSourceReader {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
        fs::canonicalize(path).map_err(|error| error.to_string())
    }

    fn read(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
        let file = fs::File::open(path).map_err(|error| error.to_string())?;
        let limit = u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX);
        let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrationError {
    Source {
        source_uri: String,
        operation: &'static str,
        detail: String,
    },
    Limit {
        source_uri: String,
        resource: MigrationResource,
        configured: usize,
        observed: usize,
    },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source {
                source_uri,
                operation,
                detail,
            } => write!(
                formatter,
                "cannot {operation} migration source `{source_uri}`: {detail}"
            ),
            Self::Limit {
                source_uri,
                resource,
                configured,
                observed,
            } => write!(
                formatter,
                "migration {} limit exceeded for `{source_uri}`: configured {configured}, observed {observed}",
                resource.as_str()
            ),
        }
    }
}

impl std::error::Error for MigrationError {}

impl MigrationError {
    #[must_use]
    pub fn render(&self, format: MigrationFormat) -> String {
        match format {
            MigrationFormat::Human => self.to_string(),
            MigrationFormat::Json => {
                let mut output =
                    format!("{{\"schema\":{SCHEMA_VERSION},\"complete\":false,\"error\":{{");
                match self {
                    Self::Source {
                        source_uri,
                        operation,
                        detail,
                    } => {
                        output.push_str("\"kind\":\"source\",\"source_uri\":");
                        write_json_string(&mut output, source_uri)
                            .expect("writing JSON error text to a String cannot fail");
                        output.push_str(",\"operation\":");
                        write_json_string(&mut output, operation)
                            .expect("writing JSON error text to a String cannot fail");
                        output.push_str(",\"detail\":");
                        write_json_string(&mut output, detail)
                            .expect("writing JSON error text to a String cannot fail");
                    }
                    Self::Limit {
                        source_uri,
                        resource,
                        configured,
                        observed,
                    } => {
                        output.push_str("\"kind\":\"limit\",\"source_uri\":");
                        write_json_string(&mut output, source_uri)
                            .expect("writing JSON error text to a String cannot fail");
                        output.push_str(",\"resource\":");
                        write_json_string(&mut output, resource.as_str())
                            .expect("writing JSON error text to a String cannot fail");
                        write!(
                            output,
                            ",\"configured\":{configured},\"observed\":{observed}"
                        )
                        .expect("writing JSON error text to a String cannot fail");
                    }
                }
                output.push_str("}}");
                output
            }
        }
    }
}

#[must_use = "migration analysis failures must be handled"]
pub fn analyze_roots(
    reader: &impl SourceReader,
    roots: &[PathBuf],
) -> Result<MigrationReport, MigrationError> {
    analyze_roots_with_limits(reader, roots, &MigrationLimits::default())
}

#[must_use = "migration analysis failures must be handled"]
pub fn analyze_roots_with_limits(
    reader: &impl SourceReader,
    roots: &[PathBuf],
    limits: &MigrationLimits,
) -> Result<MigrationReport, MigrationError> {
    let mut graph = Graph::new(reader, *limits);
    for root in roots {
        graph.visit(root.clone(), root.clone(), 1)?;
    }
    graph.into_report()
}

struct Graph<'reader, Reader> {
    reader: &'reader Reader,
    limits: MigrationLimits,
    usage: MigrationUsage,
    nodes: Vec<SourceNode>,
    identities: BTreeMap<PathBuf, usize>,
}

#[derive(Default)]
struct MigrationUsage {
    source_bytes: usize,
    work_units: usize,
    findings: usize,
    edit_bytes: usize,
}

struct FindingCollector<'usage> {
    findings: BTreeSet<MigrationFinding>,
    usage: &'usage mut MigrationUsage,
    limits: &'usage MigrationLimits,
    source_uri: &'usage str,
}

impl<'usage> FindingCollector<'usage> {
    fn new(
        usage: &'usage mut MigrationUsage,
        limits: &'usage MigrationLimits,
        source_uri: &'usage str,
    ) -> Self {
        Self {
            findings: BTreeSet::new(),
            usage,
            limits,
            source_uri,
        }
    }

    fn push(&mut self, finding: MigrationFinding) -> Result<(), MigrationError> {
        if self.findings.contains(&finding) {
            return Ok(());
        }
        let edit_bytes = finding
            .edit
            .as_ref()
            .map_or(0, |edit| edit.replacement.len());
        let findings = checked_usage(
            MigrationResource::Findings,
            self.usage.findings,
            1,
            self.limits.max_findings,
            self.source_uri,
        )?;
        let aggregate_edit_bytes = checked_usage(
            MigrationResource::EditBytes,
            self.usage.edit_bytes,
            edit_bytes,
            self.limits.max_edit_bytes,
            self.source_uri,
        )?;
        let work_units = checked_usage(
            MigrationResource::WorkUnits,
            self.usage.work_units,
            1_usize.saturating_add(edit_bytes),
            self.limits.max_work_units,
            self.source_uri,
        )?;
        self.usage.findings = findings;
        self.usage.edit_bytes = aggregate_edit_bytes;
        self.usage.work_units = work_units;
        self.findings.insert(finding);
        Ok(())
    }

    fn finish(self) -> Vec<MigrationFinding> {
        self.findings.into_iter().collect()
    }
}

impl<'reader, Reader: SourceReader> Graph<'reader, Reader> {
    fn new(reader: &'reader Reader, limits: MigrationLimits) -> Self {
        Self {
            reader,
            limits,
            usage: MigrationUsage::default(),
            nodes: Vec::new(),
            identities: BTreeMap::new(),
        }
    }

    fn visit(
        &mut self,
        requested: PathBuf,
        logical: PathBuf,
        depth: usize,
    ) -> Result<usize, MigrationError> {
        self.charge_work(path_bytes(&requested), "path")?;
        let source_uri = encode_path(&logical);
        ensure_limit(
            MigrationResource::Nesting,
            self.limits.max_nesting,
            depth,
            &source_uri,
        )?;
        self.charge_work(1, &source_uri)?;
        let canonical =
            self.reader
                .canonicalize(&requested)
                .map_err(|detail| MigrationError::Source {
                    source_uri: source_uri.clone(),
                    operation: "resolve",
                    detail,
                })?;
        self.charge_work(path_bytes(&canonical), &source_uri)?;
        if let Some(index) = self.identities.get(&canonical) {
            return Ok(*index);
        }

        ensure_limit(
            MigrationResource::Files,
            self.limits.max_files,
            self.nodes.len().saturating_add(1),
            &source_uri,
        )?;

        self.charge_work(1, &source_uri)?;
        let remaining_source_bytes = self
            .limits
            .max_source_bytes
            .saturating_sub(self.usage.source_bytes);
        let remaining_work_units = self
            .limits
            .max_work_units
            .saturating_sub(self.usage.work_units);
        let read_limit = remaining_source_bytes.min(remaining_work_units);
        let bytes =
            self.reader
                .read(&canonical, read_limit)
                .map_err(|detail| MigrationError::Source {
                    source_uri: source_uri.clone(),
                    operation: "read",
                    detail,
                })?;
        self.usage.source_bytes = checked_usage(
            MigrationResource::SourceBytes,
            self.usage.source_bytes,
            bytes.len(),
            self.limits.max_source_bytes,
            &source_uri,
        )?;
        self.charge_work(bytes.len(), &source_uri)?;
        let digest = sha256::digest(&bytes);
        let text = String::from_utf8(bytes).map_err(|error| MigrationError::Source {
            source_uri: source_uri.clone(),
            operation: "decode as UTF-8",
            detail: error.to_string(),
        })?;
        let source_id =
            u32::try_from(self.nodes.len() + 1).map_err(|_| MigrationError::Source {
                source_uri: source_uri.clone(),
                operation: "identify",
                detail: "source graph exceeds the supported identity range".to_owned(),
            })?;
        let source = SourceFile::new(SourceId::new(source_id), source_uri.clone(), text);
        let is_v2 = matches!(
            detect_source_language(&source),
            LanguageDetection::Complete(_)
        );
        let (nesting, token_count) = source_metrics(&source);
        ensure_limit(
            MigrationResource::Nesting,
            self.limits.max_nesting,
            nesting,
            &source_uri,
        )?;
        self.charge_work(token_count.saturating_mul(2), &source_uri)?;
        let parsed = ParsedSource::parse(&source, is_v2, &mut self.usage, &self.limits)?;
        let imports = parsed.imports(&source);
        self.charge_work(imports.len(), &source_uri)?;
        let index = self.nodes.len();
        self.nodes.push(SourceNode {
            canonical: canonical.clone(),
            logical,
            source,
            digest,
            parsed,
            imports,
        });
        self.identities.insert(canonical.clone(), index);

        let parent = canonical.parent().unwrap_or(Path::new(""));
        let logical_parent = self.nodes[index]
            .logical
            .parent()
            .unwrap_or(Path::new(""))
            .to_owned();
        for import_index in 0..self.nodes[index].imports.len() {
            let relative = self.nodes[index].imports[import_index].path.clone();
            let requested_import = if relative.is_absolute() {
                relative.clone()
            } else {
                parent.join(&relative)
            };
            let logical_import = if relative.is_absolute() {
                relative
            } else {
                logical_parent.join(relative)
            };
            let target = self.visit(requested_import, logical_import, depth.saturating_add(1))?;
            self.nodes[index].imports[import_index].target = Some(target);
        }
        Ok(index)
    }

    fn into_report(mut self) -> Result<MigrationReport, MigrationError> {
        let mut sources = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            self.usage.work_units = checked_usage(
                MigrationResource::WorkUnits,
                self.usage.work_units,
                node.source.len(),
                self.limits.max_work_units,
                node.source.name(),
            )?;
            let migrated = analyze_source(node, &mut self.usage, &self.limits)?;
            sources.push(migrated);
        }
        Ok(MigrationReport {
            schema: SCHEMA_VERSION,
            sources,
            work_units: self.usage.work_units,
        })
    }

    fn charge_work(&mut self, amount: usize, source_uri: &str) -> Result<(), MigrationError> {
        self.usage.work_units = checked_usage(
            MigrationResource::WorkUnits,
            self.usage.work_units,
            amount,
            self.limits.max_work_units,
            source_uri,
        )?;
        Ok(())
    }
}

struct SourceNode {
    #[allow(dead_code)]
    canonical: PathBuf,
    logical: PathBuf,
    source: SourceFile,
    digest: String,
    parsed: ParsedSource,
    imports: Vec<StaticImport>,
}

enum ParsedSource {
    V1(Script),
    V2(Script),
    Invalid(MigrationFinding),
}

impl ParsedSource {
    fn parse(
        source: &SourceFile,
        is_v2: bool,
        usage: &mut MigrationUsage,
        limits: &MigrationLimits,
    ) -> Result<Self, MigrationError> {
        let starting_work = usage.work_units;
        let remaining = limits.max_work_units.saturating_sub(starting_work);
        let polls = Cell::new(0_usize);
        let is_exhausted = || {
            let observed = polls.get().saturating_add(1);
            polls.set(observed);
            observed > remaining
        };
        let parsed = if is_v2 {
            match parse_v2_with_control(source, &is_exhausted) {
                ControlledVersionedParseOutcome::Parsed(outcome) => match outcome {
                    VersionedParseOutcome::Complete(script) => Self::V2(script.into_script()),
                    VersionedParseOutcome::Incomplete(incomplete) => Self::Invalid(parse_finding(
                        incomplete.span(),
                        "incomplete explicitly versioned source",
                    )),
                    VersionedParseOutcome::Invalid(diagnostics) => {
                        Self::Invalid(diagnostic_finding(&diagnostics))
                    }
                },
                ControlledVersionedParseOutcome::Cancelled => {
                    return Err(limit_error(
                        MigrationResource::WorkUnits,
                        limits.max_work_units,
                        starting_work.saturating_add(polls.get()),
                        source.name(),
                    ));
                }
            }
        } else {
            match parse_with_control(source, &is_exhausted) {
                ControlledParseOutcome::Parsed(outcome) => match outcome {
                    ParseOutcome::Complete(script) => Self::V1(script),
                    ParseOutcome::Incomplete(incomplete) => {
                        Self::Invalid(parse_finding(incomplete.span(), "incomplete v1 source"))
                    }
                    ParseOutcome::Invalid(diagnostics) => {
                        Self::Invalid(diagnostic_finding(&diagnostics))
                    }
                },
                ControlledParseOutcome::Cancelled => {
                    return Err(limit_error(
                        MigrationResource::WorkUnits,
                        limits.max_work_units,
                        starting_work.saturating_add(polls.get()),
                        source.name(),
                    ));
                }
            }
        };
        usage.work_units = checked_usage(
            MigrationResource::WorkUnits,
            starting_work,
            polls.get(),
            limits.max_work_units,
            source.name(),
        )?;
        Ok(parsed)
    }

    fn script(&self) -> Option<&Script> {
        match self {
            Self::V1(script) | Self::V2(script) => Some(script),
            Self::Invalid(_) => None,
        }
    }

    fn imports(&self, source: &SourceFile) -> Vec<StaticImport> {
        let Some(script) = self.script() else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        for statement in script.statements() {
            match statement.kind() {
                StatementKind::Import(import) => {
                    let path = quoted_path(source, import.path);
                    imports.push(StaticImport {
                        statement_span: statement.span(),
                        path,
                        names: import
                            .names
                            .iter()
                            .map(|name| {
                                source
                                    .slice(name.span())
                                    .expect("an import name belongs to its source")
                                    .to_owned()
                            })
                            .collect(),
                        target: None,
                    });
                }
                StatementKind::ModuleImport(import) => {
                    if let ModuleImportSource::Local { path } = import.source {
                        imports.push(StaticImport {
                            statement_span: statement.span(),
                            path: quoted_path(source, path),
                            names: Vec::new(),
                            target: None,
                        });
                    }
                }
                _ => {}
            }
        }
        imports
    }
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn source_metrics(source: &SourceFile) -> (usize, usize) {
    let tokens = lex(source);
    let mut delimiter_depth = 0_usize;
    let mut maximum = 0_usize;
    let mut openings = Vec::new();
    let mut expression_chains = vec![0_usize];
    let mut brace_depth = 0_usize;
    let mut if_chains = BTreeMap::<usize, usize>::new();
    let mut pending_else = None;
    let mut last_significant = None;

    for token in &tokens {
        match token.kind() {
            TokenKind::Whitespace | TokenKind::Comment | TokenKind::DocumentationComment => {
                continue;
            }
            TokenKind::Newline => {
                if !last_significant.is_some_and(continues_expression) {
                    *expression_chains
                        .last_mut()
                        .expect("syntax depth always has one expression level") = 0;
                }
                last_significant = Some(token.kind());
                continue;
            }
            TokenKind::BracedExpansionStart
            | TokenKind::CommandSubstitutionStart
            | TokenKind::Delimiter(
                Delimiter::LeftParenthesis | Delimiter::LeftBrace | Delimiter::LeftBracket,
            ) => {
                delimiter_depth = delimiter_depth.saturating_add(1);
                maximum = maximum.max(delimiter_depth);
                match token.kind() {
                    TokenKind::Delimiter(Delimiter::LeftParenthesis | Delimiter::LeftBracket) => {
                        let chain = expression_chains
                            .last_mut()
                            .expect("syntax depth always has one expression level");
                        *chain = chain.saturating_add(1);
                        maximum = maximum.max(*chain);
                    }
                    TokenKind::Delimiter(Delimiter::LeftBrace) => {
                        *expression_chains
                            .last_mut()
                            .expect("syntax depth always has one expression level") = 0;
                        brace_depth = brace_depth.saturating_add(1);
                    }
                    TokenKind::BracedExpansionStart | TokenKind::CommandSubstitutionStart => {}
                    _ => unreachable!("the outer match selects only opening syntax"),
                }
                openings.push(token.kind());
                expression_chains.push(0);
            }
            TokenKind::Delimiter(
                Delimiter::RightParenthesis | Delimiter::RightBrace | Delimiter::RightBracket,
            ) => {
                let opening = openings.pop();
                if opening.is_some() {
                    delimiter_depth = delimiter_depth.saturating_sub(1);
                    expression_chains.pop();
                }
                if opening == Some(TokenKind::Delimiter(Delimiter::LeftBrace)) {
                    if_chains.remove(&brace_depth);
                    brace_depth = brace_depth.saturating_sub(1);
                }
            }
            TokenKind::Keyword(Keyword::Else) => pending_else = Some(brace_depth),
            TokenKind::Keyword(Keyword::If) => {
                let chain = if pending_else == Some(brace_depth) {
                    if_chains
                        .get(&brace_depth)
                        .copied()
                        .unwrap_or(1)
                        .saturating_add(1)
                } else {
                    1
                };
                if_chains.insert(brace_depth, chain);
                maximum = maximum.max(chain);
                pending_else = None;
            }
            TokenKind::Operator(operator) if recursive_expression_operator(operator) => {
                let chain = expression_chains
                    .last_mut()
                    .expect("syntax depth always has one expression level");
                *chain = chain.saturating_add(1);
                maximum = maximum.max(*chain);
                pending_else = None;
            }
            TokenKind::Operator(operator) if expression_separator(operator) => {
                *expression_chains
                    .last_mut()
                    .expect("syntax depth always has one expression level") = 0;
                pending_else = None;
            }
            _ => pending_else = None,
        }
        last_significant = Some(token.kind());
    }
    (maximum, tokens.len())
}

fn recursive_expression_operator(operator: Operator) -> bool {
    matches!(
        operator,
        Operator::Plus
            | Operator::Minus
            | Operator::Star
            | Operator::Slash
            | Operator::Percent
            | Operator::Bang
            | Operator::Dot
    )
}

fn expression_separator(operator: Operator) -> bool {
    matches!(
        operator,
        Operator::Assign
            | Operator::Semicolon
            | Operator::Pipe
            | Operator::PipeBoth
            | Operator::And
            | Operator::Or
            | Operator::Append
            | Operator::Duplicate
            | Operator::Arrow
            | Operator::MatchArrow
            | Operator::Comma
            | Operator::Spread
            | Operator::Caret
            | Operator::Colon
    )
}

fn continues_expression(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Operator(operator) if recursive_expression_operator(operator))
}

fn checked_usage(
    resource: MigrationResource,
    current: usize,
    added: usize,
    configured: usize,
    source_uri: &str,
) -> Result<usize, MigrationError> {
    let observed = current.saturating_add(added);
    ensure_limit(resource, configured, observed, source_uri)?;
    Ok(observed)
}

fn ensure_limit(
    resource: MigrationResource,
    configured: usize,
    observed: usize,
    source_uri: &str,
) -> Result<(), MigrationError> {
    if observed > configured {
        Err(limit_error(resource, configured, observed, source_uri))
    } else {
        Ok(())
    }
}

fn limit_error(
    resource: MigrationResource,
    configured: usize,
    observed: usize,
    source_uri: &str,
) -> MigrationError {
    MigrationError::Limit {
        source_uri: source_uri.to_owned(),
        resource,
        configured,
        observed,
    }
}

fn diagnostic_finding(diagnostics: &[flash_syntax::Diagnostic]) -> MigrationFinding {
    let diagnostic = diagnostics
        .first()
        .expect("an invalid parse has at least one diagnostic");
    let span = diagnostic
        .labels()
        .first()
        .map_or(SpanBounds { start: 0, end: 0 }, |label| SpanBounds {
            start: label.span().start(),
            end: label.span().end(),
        });
    MigrationFinding {
        code: "MIG1001".to_owned(),
        severity: FindingSeverity::Required,
        start: span.start,
        end: span.end,
        message: format!("v1 source cannot be classified: {}", diagnostic.message()),
        edit: None,
    }
}

fn parse_finding(span: Span, message: &str) -> MigrationFinding {
    MigrationFinding {
        code: "MIG1001".to_owned(),
        severity: FindingSeverity::Required,
        start: span.start(),
        end: span.end(),
        message: message.to_owned(),
        edit: None,
    }
}

struct SpanBounds {
    start: usize,
    end: usize,
}

struct StaticImport {
    statement_span: Span,
    path: PathBuf,
    names: Vec<String>,
    #[allow(dead_code)]
    target: Option<usize>,
}

fn quoted_path(source: &SourceFile, span: Span) -> PathBuf {
    let quoted = source
        .slice(span)
        .expect("an import path belongs to its source");
    PathBuf::from(&quoted[1..quoted.len() - 1])
}

fn analyze_source(
    node: &SourceNode,
    usage: &mut MigrationUsage,
    limits: &MigrationLimits,
) -> Result<MigrationSource, MigrationError> {
    let detected_language = match node.parsed {
        ParsedSource::V2(_) => 2,
        ParsedSource::V1(_) | ParsedSource::Invalid(_) => 1,
    };
    let mut findings = FindingCollector::new(usage, limits, node.source.name());
    let mut unresolved = false;
    match &node.parsed {
        ParsedSource::Invalid(finding) => {
            findings.push(finding.clone())?;
            unresolved = true;
        }
        ParsedSource::V2(_) => {}
        ParsedSource::V1(script) => {
            let (insertion, replacement) = language_insertion(node.source.text());
            findings.push(MigrationFinding {
                code: "MIG2001".to_owned(),
                severity: FindingSeverity::Required,
                start: insertion,
                end: insertion,
                message: "add 'language 2' before the first statement".to_owned(),
                edit: Some(MigrationEdit {
                    start: insertion,
                    end: insertion,
                    replacement,
                }),
            })?;
            let scan = scan(&node.source, script);
            analyze_reserved_names(&scan, &node.imports, &mut findings)?;
            unresolved |= analyze_imports(&node.source, &scan, &node.imports, &mut findings)?;
            analyze_length_operation(&node.source, &scan, &node.imports, &mut findings)?;
            analyze_known_argv_transport(&node.source, &mut findings)?;
            unresolved |= analyze_effects(&scan, &node.imports, &mut findings)?;
        }
    }
    let mut findings = findings.finish();
    findings.sort_by(|left, right| {
        (
            left.start,
            left.end,
            left.code.as_str(),
            left.message.as_str(),
        )
            .cmp(&(
                right.start,
                right.end,
                right.code.as_str(),
                right.message.as_str(),
            ))
    });
    Ok(MigrationSource {
        source_uri: node.source.name().to_owned(),
        digest: node.digest.clone(),
        detected_language,
        target_language: 2,
        findings,
        unresolved,
    })
}

fn language_insertion(source: &str) -> (usize, String) {
    if !source.starts_with("#!") {
        return (0, "language 2\n\n".to_owned());
    }
    match source.find('\n') {
        Some(end) => (end + 1, "language 2\n\n".to_owned()),
        None => (source.len(), "\nlanguage 2\n".to_owned()),
    }
}

const NEW_RESERVED_WORDS: [&str; 5] = ["action", "enum", "language", "task", "type"];
const V2_RESERVED_WORDS: [&str; 26] = [
    "action", "break", "catch", "continue", "def", "else", "enum", "export", "false", "for", "if",
    "import", "in", "language", "let", "match", "mut", "null", "return", "task", "throw", "true",
    "try", "type", "unset", "while",
];

fn analyze_reserved_names(
    scan: &SourceScan,
    imports: &[StaticImport],
    findings: &mut FindingCollector<'_>,
) -> Result<(), MigrationError> {
    let imported = imports
        .iter()
        .flat_map(|import| import.names.iter())
        .collect::<BTreeSet<_>>();
    for reserved in NEW_RESERVED_WORDS {
        if imported.contains(&reserved.to_owned()) {
            continue;
        }
        let bindings = scan
            .bindings
            .iter()
            .filter(|(name, _)| name == reserved)
            .collect::<Vec<_>>();
        if bindings.is_empty() {
            continue;
        }
        let replacement = collision_free_name(reserved, &scan.identifiers);
        for (_, span) in bindings {
            findings.push(rename_finding(*span, reserved, &replacement))?;
        }
        for reference in scan
            .references
            .iter()
            .filter(|reference| reference.name == reserved)
        {
            findings.push(rename_finding(reference.name_span, reserved, &replacement))?;
        }
    }
    for reserved in &scan.reserved_uses {
        findings.push(MigrationFinding {
            code: "MIG2002".to_owned(),
            severity: FindingSeverity::Required,
            start: reserved.start,
            end: reserved.end,
            message: format!(
                "preserve v1 data name `{}` across the Flash 2 reserved-word boundary",
                reserved.spelling
            ),
            edit: Some(MigrationEdit {
                start: reserved.start,
                end: reserved.end,
                replacement: reserved.replacement.clone(),
            }),
        })?;
    }
    Ok(())
}

fn rename_finding(span: Span, name: &str, replacement: &str) -> MigrationFinding {
    MigrationFinding {
        code: "MIG2002".to_owned(),
        severity: FindingSeverity::Required,
        start: span.start(),
        end: span.end(),
        message: format!("rename v1 binding `{name}` because it is reserved by Flash 2"),
        edit: Some(MigrationEdit {
            start: span.start(),
            end: span.end(),
            replacement: replacement.to_owned(),
        }),
    }
}

fn collision_free_name(name: &str, occupied: &BTreeSet<String>) -> String {
    let base = format!("{name}_v1");
    if !occupied.contains(&base) {
        return base;
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}_{suffix}");
        if !occupied.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("the finite occupied set cannot contain every numeric suffix")
}

fn analyze_imports(
    source: &SourceFile,
    scan: &SourceScan,
    imports: &[StaticImport],
    findings: &mut FindingCollector<'_>,
) -> Result<bool, MigrationError> {
    let mut occupied = scan.identifiers.clone();
    let mut imported_counts = BTreeMap::<&str, usize>::new();
    for import in imports {
        for name in &import.names {
            *imported_counts.entry(name).or_default() += 1;
        }
    }
    let local_bindings = scan
        .top_level_bindings
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut unresolved = false;
    for import in imports {
        let alias = import_alias(&import.path, &occupied);
        occupied.insert(alias.clone());
        let conflict = import.names.iter().find_map(|name| {
            if NEW_RESERVED_WORDS.contains(&name.as_str()) {
                Some(format!("imported name `{name}` is reserved by Flash 2"))
            } else if local_bindings.contains(name.as_str()) {
                Some(format!("imported name `{name}` is shadowed locally"))
            } else if imported_counts
                .get(name.as_str())
                .copied()
                .unwrap_or_default()
                > 1
            {
                Some(format!("imported name `{name}` has multiple sources"))
            } else if scan.exports.contains(name) {
                Some(format!("imported name `{name}` is publicly re-exported"))
            } else if scan.references.iter().any(|reference| {
                reference.name == *name
                    && reference.kind == ReferenceKind::Assignment
                    && !scan.reference_is_shadowed(name, reference.full_span)
            }) {
                Some(format!("imported name `{name}` is assigned"))
            } else if scan.references.iter().any(|reference| {
                reference.name == *name
                    && reference.kind == ReferenceKind::Spread
                    && !scan.reference_is_shadowed(name, reference.full_span)
            }) {
                Some(format!(
                    "imported name `{name}` is used as a command spread"
                ))
            } else {
                None
            }
        });
        let unsafe_names = conflict.is_some();
        let original_path = source
            .slice(import.statement_span)
            .expect("an import statement belongs to its source");
        let quoted = original_path
            .find('\'')
            .and_then(|start| {
                original_path
                    .rfind('\'')
                    .map(|end| &original_path[start..=end])
            })
            .unwrap_or("''");
        let replacement = format!("import {quoted} as {alias}");
        findings.push(MigrationFinding {
            code: "MIG2005".to_owned(),
            severity: FindingSeverity::Required,
            start: import.statement_span.start(),
            end: import.statement_span.end(),
            message: if unsafe_names {
                format!(
                    "v1 import needs a qualified alias, but {}",
                    conflict.expect("an unsafe import has a conflict")
                )
            } else {
                format!("rewrite v1 import through module alias `{alias}`")
            },
            edit: (!unsafe_names).then_some(MigrationEdit {
                start: import.statement_span.start(),
                end: import.statement_span.end(),
                replacement,
            }),
        })?;
        if unsafe_names {
            unresolved = true;
            continue;
        }
        for name in &import.names {
            for reference in scan.references.iter().filter(|reference| {
                reference.name == *name && !scan.reference_is_shadowed(name, reference.full_span)
            }) {
                let span = match reference.kind {
                    ReferenceKind::ExpressionValue | ReferenceKind::WordValue => {
                        reference.full_span
                    }
                    ReferenceKind::Name | ReferenceKind::Command => reference.name_span,
                    ReferenceKind::Spread | ReferenceKind::Assignment => continue,
                };
                let replacement = match reference.kind {
                    ReferenceKind::WordValue => format!("${{{alias}::{name}}}"),
                    ReferenceKind::ExpressionValue
                    | ReferenceKind::Name
                    | ReferenceKind::Command => format!("{alias}::{name}"),
                    ReferenceKind::Spread | ReferenceKind::Assignment => continue,
                };
                findings.push(MigrationFinding {
                    code: "MIG2005".to_owned(),
                    severity: FindingSeverity::Required,
                    start: span.start(),
                    end: span.end(),
                    message: format!("qualify imported name `{name}` through `{alias}`"),
                    edit: Some(MigrationEdit {
                        start: span.start(),
                        end: span.end(),
                        replacement,
                    }),
                })?;
            }
        }
    }
    Ok(unresolved)
}

fn import_alias(path: &Path, occupied: &BTreeSet<String>) -> String {
    let raw = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("module");
    let mut base = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if base.is_empty() || base.as_bytes()[0].is_ascii_digit() {
        base.insert_str(0, "module_");
    }
    if V2_RESERVED_WORDS.contains(&base.as_str()) {
        base.push_str("_module");
    }
    if !occupied.contains(&base) {
        return base;
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}_{suffix}");
        if !occupied.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("the finite occupied set cannot contain every numeric suffix")
}

fn analyze_length_operation(
    source: &SourceFile,
    scan: &SourceScan,
    imports: &[StaticImport],
    findings: &mut FindingCollector<'_>,
) -> Result<(), MigrationError> {
    let imported_names = imports
        .iter()
        .flat_map(|import| import.names.iter())
        .collect::<BTreeSet<_>>();
    if imported_names.contains(&"length".to_owned()) {
        return Ok(());
    }
    let uses = scan
        .commands
        .iter()
        .filter(|command| {
            !command.forced_external
                && command.name.as_deref() == Some("length")
                && !scan.command_is_callable("length", command.span)
        })
        .collect::<Vec<_>>();
    if uses.is_empty() {
        return Ok(());
    }
    let alias = import_alias(Path::new("value"), &scan.identifiers);
    for command in uses {
        findings.push(MigrationFinding {
            code: "MIG2003".to_owned(),
            severity: FindingSeverity::Required,
            start: command.span.start(),
            end: command.span.end(),
            message: "qualify the v1 `length` operation through `std::value`".to_owned(),
            edit: Some(MigrationEdit {
                start: command.span.start(),
                end: command.span.end(),
                replacement: format!("{alias}::length"),
            }),
        })?;
    }
    let insertion = source.len();
    let prefix = if source.text().ends_with('\n') {
        ""
    } else {
        "\n"
    };
    findings.push(MigrationFinding {
        code: "MIG2003".to_owned(),
        severity: FindingSeverity::Required,
        start: insertion,
        end: insertion,
        message: "import the qualified Flash 2 value operation module".to_owned(),
        edit: Some(MigrationEdit {
            start: insertion,
            end: insertion,
            replacement: format!("{prefix}import std::value as {alias}\n"),
        }),
    })?;
    Ok(())
}

fn analyze_known_argv_transport(
    source: &SourceFile,
    findings: &mut FindingCollector<'_>,
) -> Result<(), MigrationError> {
    const BUILD_ARGV_TRANSPORT: &str = "^sh -c 'remaining=$1; shift; while [ \"$remaining\" -gt 0 ]; do shift; remaining=$((remaining - 1)); done; exec make \"$@\"' flash-build-argv $argument_index ...$args";
    let Some(start) = source.text().find(BUILD_ARGV_TRANSPORT) else {
        return Ok(());
    };
    findings.push(MigrationFinding {
        code: "MIG2004".to_owned(),
        severity: FindingSeverity::Suggested,
        start,
        end: start + BUILD_ARGV_TRANSPORT.len(),
        message:
            "replace the recognized build argv transport with the reviewed list-rest transformation"
                .to_owned(),
        edit: None,
    })?;
    Ok(())
}

fn analyze_effects(
    scan: &SourceScan,
    imports: &[StaticImport],
    findings: &mut FindingCollector<'_>,
) -> Result<bool, MigrationError> {
    let imported_names = imports
        .iter()
        .flat_map(|import| import.names.iter())
        .collect::<BTreeSet<_>>();
    let mut effect_spans = scan.effect_spans.clone();
    for (name, span) in &scan.intrinsic_calls {
        if !imported_names.contains(name) && !scan.reference_is_shadowed(name, *span) {
            effect_spans.push(*span);
        }
    }
    for command in &scan.commands {
        let is_language_callable = command.name.as_ref().is_some_and(|name| {
            scan.command_is_callable(name, command.span) || imported_names.contains(name)
        });
        let permitted = !command.forced_external
            && command.name.as_deref().is_some_and(|name| {
                is_language_callable || matches!(name, "exit" | "help" | "length")
            });
        if !permitted {
            effect_spans.push(command.span);
        }
    }
    effect_spans.sort_by_key(|span| (span.start(), span.end()));
    effect_spans.dedup();
    for span in &effect_spans {
        findings.push(MigrationFinding {
            code: "AUTH2001".to_owned(),
            severity: FindingSeverity::Required,
            start: span.start(),
            end: span.end(),
            message: "ambient effectful execution is unavailable in the Flash 2 pure foundation"
                .to_owned(),
            edit: None,
        })?;
    }
    Ok(!effect_spans.is_empty())
}

fn encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'.' | b'_' | b'~' | b':')
        {
            encoded.push(char::from(*byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Default)]
    struct FakeReader {
        sources: BTreeMap<PathBuf, Vec<u8>>,
        aliases: BTreeMap<PathBuf, PathBuf>,
    }

    impl FakeReader {
        fn source(mut self, path: &str, text: &str) -> Self {
            self.sources
                .insert(PathBuf::from(path), text.as_bytes().to_vec());
            self
        }

        fn source_path(mut self, path: PathBuf, text: &str) -> Self {
            self.sources.insert(path, text.as_bytes().to_vec());
            self
        }

        fn alias(mut self, path: &str, canonical: &str) -> Self {
            self.aliases
                .insert(PathBuf::from(path), PathBuf::from(canonical));
            self
        }
    }

    impl SourceReader for FakeReader {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
            let normalized = normalize(path);
            Ok(self.aliases.get(&normalized).cloned().unwrap_or(normalized))
        }

        fn read(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
            let bytes = self
                .sources
                .get(path)
                .ok_or_else(|| "not found".to_owned())?;
            Ok(bytes[..bytes.len().min(max_bytes.saturating_add(1))].to_vec())
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum ReadAccess {
        Resolve(PathBuf),
        Read { path: PathBuf, max_bytes: usize },
    }

    #[derive(Default)]
    struct RecordingReader {
        sources: BTreeMap<PathBuf, Vec<u8>>,
        accesses: RefCell<Vec<ReadAccess>>,
    }

    impl RecordingReader {
        fn source(mut self, path: &str, text: &str) -> Self {
            self.sources
                .insert(PathBuf::from(path), text.as_bytes().to_vec());
            self
        }
    }

    impl SourceReader for RecordingReader {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf, String> {
            let path = normalize(path);
            self.accesses
                .borrow_mut()
                .push(ReadAccess::Resolve(path.clone()));
            Ok(path)
        }

        fn read(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
            self.accesses.borrow_mut().push(ReadAccess::Read {
                path: path.to_owned(),
                max_bytes,
            });
            let bytes = self
                .sources
                .get(path)
                .ok_or_else(|| "not found".to_owned())?;
            Ok(bytes[..bytes.len().min(max_bytes.saturating_add(1))].to_vec())
        }
    }

    fn normalize(path: &Path) -> PathBuf {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                component => normalized.push(component.as_os_str()),
            }
        }
        normalized
    }

    #[test]
    fn graph_order_alias_collapse_and_import_edits_are_stable() {
        let reader = FakeReader::default()
            .source(
                "/project/root.fsh",
                "import { answer } from './lib.fsh'\nanswer()\nimport './alias.fsh'\n",
            )
            .source(
                "/project/lib.fsh",
                "def answer() { return 42 }\nexport { answer }\n",
            )
            .alias("/project/alias.fsh", "/project/lib.fsh");
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();

        assert_eq!(
            report
                .sources
                .iter()
                .map(|source| source.source_uri.as_str())
                .collect::<Vec<_>>(),
            ["/project/root.fsh", "/project/./lib.fsh"]
        );
        let root = &report.sources[0];
        assert!(root.findings.iter().any(|finding| {
            finding.code == "MIG2005"
                && finding
                    .edit
                    .as_ref()
                    .is_some_and(|edit| edit.replacement == "import './lib.fsh' as lib")
        }));
        assert!(root.findings.iter().any(|finding| {
            finding.code == "MIG2005"
                && finding
                    .edit
                    .as_ref()
                    .is_some_and(|edit| edit.replacement == "lib::answer")
        }));
    }

    #[test]
    fn import_alias_avoids_every_v2_keyword() {
        let reader = FakeReader::default()
            .source(
                "/project/root.fsh",
                "import { answer } from './let.fsh'\nanswer()\n",
            )
            .source(
                "/project/let.fsh",
                "def answer() { return 42 }\nexport { answer }\n",
            );
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();

        assert!(report.sources[0].findings.iter().any(|finding| {
            finding
                .edit
                .as_ref()
                .is_some_and(|edit| edit.replacement == "import './let.fsh' as let_module")
        }));
        assert!(report.sources[0].findings.iter().any(|finding| {
            finding
                .edit
                .as_ref()
                .is_some_and(|edit| edit.replacement == "let_module::answer")
        }));
    }

    #[test]
    fn explicit_root_order_follows_each_depth_first_closure() {
        let reader = FakeReader::default()
            .source("/project/first.fsh", "import './shared.fsh'\n")
            .source("/project/shared.fsh", "let shared = 1\n")
            .source("/project/second.fsh", "let second = 2\n");
        let report = analyze_roots(
            &reader,
            &[
                PathBuf::from("/project/first.fsh"),
                PathBuf::from("/project/second.fsh"),
            ],
        )
        .unwrap();
        assert_eq!(
            report
                .sources
                .iter()
                .map(|source| source.source_uri.as_str())
                .collect::<Vec<_>>(),
            [
                "/project/first.fsh",
                "/project/./shared.fsh",
                "/project/second.fsh"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_non_utf8_root_units_are_percent_encoded_losslessly() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(OsString::from_vec(b"/project/\xff.fsh".to_vec()));
        let reader = FakeReader::default().source_path(path.clone(), "let value = 1\n");
        let report = analyze_roots(&reader, &[path]).unwrap();
        assert_eq!(report.sources[0].source_uri, "/project/%FF.fsh");
    }

    #[test]
    fn reserved_binding_and_references_share_one_collision_free_rename() {
        let reader = FakeReader::default().source(
            "/project/root.fsh",
            "let action = 1\nlet action_v1 = 2\n$action\n",
        );
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();
        let edits = report.sources[0]
            .findings
            .iter()
            .filter(|finding| finding.code == "MIG2002")
            .map(|finding| finding.edit.as_ref().unwrap().replacement.as_str())
            .collect::<Vec<_>>();
        assert_eq!(edits, ["action_v1_2", "action_v1_2"]);
    }

    #[test]
    fn json_schema_has_stable_field_order_and_digest() {
        let reader = FakeReader::default().source("/project/root.fsh", "let value = 1\n");
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();
        let rendered = report.render(MigrationFormat::Json).unwrap();
        assert_eq!(
            rendered,
            "{\"schema\":1,\"sources\":[{\"source_uri\":\"/project/root.fsh\",\"digest\":\"sha256:ccdab916a2664126bae77709d63891fea8e32421e01f33fa9814de8abe9ba3e3\",\"detected_language\":1,\"target_language\":2,\"findings\":[{\"code\":\"MIG2001\",\"severity\":\"required\",\"start\":0,\"end\":0,\"message\":\"add 'language 2' before the first statement\",\"edit\":{\"start\":0,\"end\":0,\"replacement\":\"language 2\\n\\n\"}}],\"unresolved\":false}]}"
        );
        assert_eq!(
            report.render(MigrationFormat::Human).unwrap(),
            "/project/root.fsh:0:0: required MIG2001: add 'language 2' before the first statement"
        );
        assert_eq!(report.exit_status(), 1);
    }

    #[test]
    fn imported_references_follow_source_order_before_a_nested_shadow() {
        let reader = FakeReader::default()
            .source(
                "/project/root.fsh",
                "import { answer } from './support.fsh'\n\
                 def use() {\n\
                     answer()\n\
                     let answer = 7\n\
                     answer()\n\
                 }\n",
            )
            .source(
                "/project/support.fsh",
                "def answer() { return 42 }\nexport { answer }\n",
            );
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();
        let qualified = report.sources[0]
            .findings
            .iter()
            .filter_map(|finding| finding.edit.as_ref())
            .filter(|edit| edit.replacement == "support::answer")
            .collect::<Vec<_>>();

        assert_eq!(qualified.len(), 1);
        assert_eq!(
            &reader.sources[Path::new("/project/root.fsh")][qualified[0].start..qualified[0].end],
            b"answer"
        );
    }

    #[test]
    fn imported_values_preserve_expression_and_word_contexts() {
        let reader = FakeReader::default()
            .source(
                "/project/root.fsh",
                "import { answer } from './support.fsh'\n\
                 let copy = $answer\n\
                 ^echo \"$answer\" $answer\n",
            )
            .source(
                "/project/support.fsh",
                "let answer = 42\nexport { answer }\n",
            );
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();
        let replacements = report.sources[0]
            .findings
            .iter()
            .filter_map(|finding| finding.edit.as_ref())
            .map(|edit| edit.replacement.as_str())
            .filter(|replacement| replacement.contains("support::answer"))
            .collect::<Vec<_>>();

        assert_eq!(
            replacements,
            [
                "support::answer",
                "${support::answer}",
                "${support::answer}"
            ]
        );
    }

    #[test]
    fn imported_command_spread_is_unresolved_without_an_unsafe_edit() {
        let reader = FakeReader::default()
            .source(
                "/project/root.fsh",
                "import { values } from './support.fsh'\n^echo ...$values\n",
            )
            .source(
                "/project/support.fsh",
                "let values = ['one']\nexport { values }\n",
            );
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();
        let root = &report.sources[0];
        let import = root
            .findings
            .iter()
            .find(|finding| {
                finding.code == "MIG2005" && finding.message.contains("used as a command spread")
            })
            .expect("the unrepresentable spread must be reported");

        assert!(import.edit.is_none());
        assert!(root.unresolved);
        assert!(!root.findings.iter().any(|finding| {
            finding
                .edit
                .as_ref()
                .is_some_and(|edit| edit.replacement.contains("support::values"))
        }));
    }

    #[test]
    fn host_intrinsics_and_commands_follow_source_order_and_authority() {
        let reader = FakeReader::default().source(
            "/project/root.fsh",
            "let inherited = env('HOME')\n\
             let matches = glob('*.fsh')\n\
             length\n\
             def length() { return 0 }\n\
             length\n\
             def env(value) { return $value }\n\
             let local = env('safe')\n\
             unknown\n",
        );
        let report = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();
        let root = &report.sources[0];
        let authority = root
            .findings
            .iter()
            .filter(|finding| finding.code == "AUTH2001")
            .collect::<Vec<_>>();
        let length_edits = root
            .findings
            .iter()
            .filter(|finding| {
                finding.code == "MIG2003"
                    && finding
                        .edit
                        .as_ref()
                        .is_some_and(|edit| edit.replacement.ends_with("::length"))
            })
            .count();

        assert_eq!(authority.len(), 3);
        assert_eq!(length_edits, 1);
        assert!(root.unresolved);
    }

    #[test]
    fn injected_reader_observes_only_explicit_resolution_and_reads() {
        let reader = RecordingReader::default()
            .source("/project/root.fsh", "import './support.fsh'\n")
            .source("/project/support.fsh", "let answer = 42\n");
        let original_sources = reader.sources.clone();

        analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap();

        assert_eq!(reader.sources, original_sources);
        assert_eq!(
            reader.accesses.into_inner(),
            [
                ReadAccess::Resolve(PathBuf::from("/project/root.fsh")),
                ReadAccess::Read {
                    path: PathBuf::from("/project/root.fsh"),
                    max_bytes: MigrationLimits::default().max_source_bytes,
                },
                ReadAccess::Resolve(PathBuf::from("/project/support.fsh")),
                ReadAccess::Read {
                    path: PathBuf::from("/project/support.fsh"),
                    max_bytes: MigrationLimits::default().max_source_bytes
                        - "import './support.fsh'\n".len(),
                },
            ]
        );
    }

    #[test]
    fn source_reader_receives_the_remaining_byte_ceiling_before_loading() {
        let reader = RecordingReader::default().source("/project/root.fsh", "0123456789");
        let error = analyze_roots_with_limits(
            &reader,
            &[PathBuf::from("/project/root.fsh")],
            &MigrationLimits {
                max_source_bytes: 4,
                ..MigrationLimits::default()
            },
        )
        .unwrap_err();

        assert!(matches!(
            error,
            MigrationError::Limit {
                resource: MigrationResource::SourceBytes,
                configured: 4,
                observed: 5,
                ..
            }
        ));
        assert_eq!(
            reader.accesses.into_inner(),
            [
                ReadAccess::Resolve(PathBuf::from("/project/root.fsh")),
                ReadAccess::Read {
                    path: PathBuf::from("/project/root.fsh"),
                    max_bytes: 4,
                },
            ]
        );
    }

    #[test]
    fn recursive_syntax_chains_share_the_nesting_ceiling() {
        let fixtures = [
            "let value = !!!true\n",
            "let value = 1 + 1 + 1 + 1\n",
            "let value = item[0][0][0]\n",
            "if true {} else if true {} else if true {}\n",
        ];
        for source in fixtures {
            let reader = FakeReader::default().source("/project/root.fsh", source);
            analyze_roots_with_limits(
                &reader,
                &[PathBuf::from("/project/root.fsh")],
                &MigrationLimits {
                    max_nesting: 3,
                    ..MigrationLimits::default()
                },
            )
            .unwrap();
            assert_limit(
                analyze_roots_with_limits(
                    &reader,
                    &[PathBuf::from("/project/root.fsh")],
                    &MigrationLimits {
                        max_nesting: 2,
                        ..MigrationLimits::default()
                    },
                )
                .unwrap_err(),
                MigrationResource::Nesting,
            );
        }
    }

    #[test]
    fn deep_and_wide_import_limits_stop_before_the_excess_read() {
        let deep_reader = RecordingReader::default()
            .source("/project/root.fsh", "import './a.fsh'\n")
            .source("/project/a.fsh", "import './b.fsh'\n")
            .source("/project/b.fsh", "import './c.fsh'\n")
            .source("/project/c.fsh", "language 2\n");
        analyze_roots_with_limits(
            &deep_reader,
            &[PathBuf::from("/project/root.fsh")],
            &MigrationLimits {
                max_nesting: 4,
                ..MigrationLimits::default()
            },
        )
        .unwrap();

        let deep_reader = RecordingReader::default()
            .source("/project/root.fsh", "import './a.fsh'\n")
            .source("/project/a.fsh", "import './b.fsh'\n")
            .source("/project/b.fsh", "import './c.fsh'\n")
            .source("/project/c.fsh", "language 2\n");
        assert_limit(
            analyze_roots_with_limits(
                &deep_reader,
                &[PathBuf::from("/project/root.fsh")],
                &MigrationLimits {
                    max_nesting: 3,
                    ..MigrationLimits::default()
                },
            )
            .unwrap_err(),
            MigrationResource::Nesting,
        );
        assert!(!deep_reader.accesses.borrow().iter().any(|access| {
            matches!(access, ReadAccess::Resolve(path) if path == Path::new("/project/c.fsh"))
        }));

        let wide_source = "import './a.fsh'\nimport './b.fsh'\nimport './c.fsh'\n";
        let wide_reader = RecordingReader::default()
            .source("/project/root.fsh", wide_source)
            .source("/project/a.fsh", "language 2\n")
            .source("/project/b.fsh", "language 2\n")
            .source("/project/c.fsh", "language 2\n");
        analyze_roots_with_limits(
            &wide_reader,
            &[PathBuf::from("/project/root.fsh")],
            &MigrationLimits {
                max_files: 4,
                ..MigrationLimits::default()
            },
        )
        .unwrap();

        let wide_reader = RecordingReader::default()
            .source("/project/root.fsh", wide_source)
            .source("/project/a.fsh", "language 2\n")
            .source("/project/b.fsh", "language 2\n")
            .source("/project/c.fsh", "language 2\n");
        assert_limit(
            analyze_roots_with_limits(
                &wide_reader,
                &[PathBuf::from("/project/root.fsh")],
                &MigrationLimits {
                    max_files: 3,
                    ..MigrationLimits::default()
                },
            )
            .unwrap_err(),
            MigrationResource::Files,
        );
        let accesses = wide_reader.accesses.into_inner();
        assert!(accesses.iter().any(|access| {
            matches!(access, ReadAccess::Resolve(path) if path == Path::new("/project/c.fsh"))
        }));
        assert!(!accesses.iter().any(|access| {
            matches!(access, ReadAccess::Read { path, .. } if path == Path::new("/project/c.fsh"))
        }));
    }

    #[test]
    fn every_migration_resource_limit_accepts_the_boundary_and_refuses_first_excess() {
        let root = PathBuf::from("/project/root.fsh");
        let one_source = FakeReader::default().source("/project/root.fsh", "let value = 1\n");

        let source_bytes = one_source.sources[&root].len();
        analyze_roots_with_limits(
            &one_source,
            std::slice::from_ref(&root),
            &MigrationLimits {
                max_source_bytes: source_bytes,
                ..MigrationLimits::default()
            },
        )
        .unwrap();
        assert_limit(
            analyze_roots_with_limits(
                &one_source,
                std::slice::from_ref(&root),
                &MigrationLimits {
                    max_source_bytes: source_bytes - 1,
                    ..MigrationLimits::default()
                },
            )
            .unwrap_err(),
            MigrationResource::SourceBytes,
        );

        analyze_roots_with_limits(
            &one_source,
            std::slice::from_ref(&root),
            &MigrationLimits {
                max_findings: 1,
                max_edit_bytes: "language 2\n\n".len(),
                ..MigrationLimits::default()
            },
        )
        .unwrap();
        for (resource, limits) in [
            (
                MigrationResource::Findings,
                MigrationLimits {
                    max_findings: 0,
                    ..MigrationLimits::default()
                },
            ),
            (
                MigrationResource::EditBytes,
                MigrationLimits {
                    max_edit_bytes: "language 2\n\n".len() - 1,
                    ..MigrationLimits::default()
                },
            ),
        ] {
            assert_limit(
                analyze_roots_with_limits(&one_source, std::slice::from_ref(&root), &limits)
                    .unwrap_err(),
                resource,
            );
        }

        let two_sources = FakeReader::default()
            .source("/project/root.fsh", "import './support.fsh'\n")
            .source("/project/support.fsh", "language 2\n");
        let aggregate_source_bytes = two_sources.sources.values().map(Vec::len).sum();
        analyze_roots_with_limits(
            &two_sources,
            std::slice::from_ref(&root),
            &MigrationLimits {
                max_files: 2,
                max_source_bytes: aggregate_source_bytes,
                max_nesting: 2,
                ..MigrationLimits::default()
            },
        )
        .unwrap();
        for (resource, limits) in [
            (
                MigrationResource::Files,
                MigrationLimits {
                    max_files: 1,
                    ..MigrationLimits::default()
                },
            ),
            (
                MigrationResource::Nesting,
                MigrationLimits {
                    max_nesting: 1,
                    ..MigrationLimits::default()
                },
            ),
            (
                MigrationResource::SourceBytes,
                MigrationLimits {
                    max_source_bytes: aggregate_source_bytes - 1,
                    ..MigrationLimits::default()
                },
            ),
        ] {
            assert_limit(
                analyze_roots_with_limits(&two_sources, std::slice::from_ref(&root), &limits)
                    .unwrap_err(),
                resource,
            );
        }

        let nested = FakeReader::default().source("/project/root.fsh", "let value = [[[1]]]\n");
        analyze_roots_with_limits(
            &nested,
            std::slice::from_ref(&root),
            &MigrationLimits {
                max_nesting: 3,
                ..MigrationLimits::default()
            },
        )
        .unwrap();
        assert_limit(
            analyze_roots_with_limits(
                &nested,
                std::slice::from_ref(&root),
                &MigrationLimits {
                    max_nesting: 2,
                    ..MigrationLimits::default()
                },
            )
            .unwrap_err(),
            MigrationResource::Nesting,
        );

        let report = analyze_roots(&one_source, std::slice::from_ref(&root)).unwrap();
        let json_bytes = report.render(MigrationFormat::Json).unwrap().len();
        report
            .render_with_limits(
                MigrationFormat::Json,
                &MigrationLimits {
                    max_output_bytes: json_bytes,
                    ..MigrationLimits::default()
                },
            )
            .unwrap();
        assert_limit(
            report
                .render_with_limits(
                    MigrationFormat::Json,
                    &MigrationLimits {
                        max_output_bytes: json_bytes - 1,
                        ..MigrationLimits::default()
                    },
                )
                .unwrap_err(),
            MigrationResource::OutputBytes,
        );
        report
            .render_with_limits(
                MigrationFormat::Json,
                &MigrationLimits {
                    max_work_units: report.work_units + json_bytes,
                    ..MigrationLimits::default()
                },
            )
            .unwrap();
        assert_limit(
            report
                .render_with_limits(
                    MigrationFormat::Json,
                    &MigrationLimits {
                        max_work_units: report.work_units + json_bytes - 1,
                        ..MigrationLimits::default()
                    },
                )
                .unwrap_err(),
            MigrationResource::WorkUnits,
        );

        let first_passing_work_limit = (0..10_000)
            .find(|max_work_units| {
                analyze_roots_with_limits(
                    &one_source,
                    std::slice::from_ref(&root),
                    &MigrationLimits {
                        max_work_units: *max_work_units,
                        ..MigrationLimits::default()
                    },
                )
                .is_ok()
            })
            .expect("the small fixture must fit within the search ceiling");
        assert!(first_passing_work_limit > 0);
        assert_limit(
            analyze_roots_with_limits(
                &one_source,
                &[root],
                &MigrationLimits {
                    max_work_units: first_passing_work_limit - 1,
                    ..MigrationLimits::default()
                },
            )
            .unwrap_err(),
            MigrationResource::WorkUnits,
        );
    }

    #[test]
    fn hostile_non_utf8_source_is_rejected_without_a_partial_report() {
        let reader = FakeReader {
            sources: BTreeMap::from([(PathBuf::from("/project/root.fsh"), vec![0xff])]),
            aliases: BTreeMap::new(),
        };
        let error = analyze_roots(&reader, &[PathBuf::from("/project/root.fsh")]).unwrap_err();
        assert!(matches!(
            &error,
            MigrationError::Source {
                operation: "decode as UTF-8",
                ..
            }
        ));
        let json = error.render(MigrationFormat::Json);
        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded["complete"], false);
        assert_eq!(decoded["error"]["kind"], "source");
    }

    fn assert_limit(error: MigrationError, expected: MigrationResource) {
        match error {
            MigrationError::Limit {
                resource,
                configured,
                observed,
                ..
            } => {
                assert_eq!(resource, expected);
                assert!(observed > configured);
            }
            MigrationError::Source { .. } => {
                panic!("expected {expected:?} exhaustion, got {error}")
            }
        }
    }
}
