//! Canonical persisted artifacts for checked and accepted operation lifecycles.
//!
//! This module owns the closed JSON schemas, digest rules, journal hash chain,
//! and read-only audit projection used by project-task planning and controlled
//! execution. It performs no host observation and grants no authority.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use opaal_platform::operational::supports_exact_process_execution;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use unicode_normalization::UnicodeNormalization as _;

/// Maximum encoded bytes in a check, plan, or audit artifact.
pub const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum encoded bytes in one audit artifact.
pub const MAX_AUDIT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum encoded bytes in one run journal.
pub const MAX_JOURNAL_BYTES: usize = 16 * 1024 * 1024;
/// Maximum complete lines in one run journal, including header and terminal.
pub const MAX_JOURNAL_LINES: usize = 100_000;
/// Capacity reserved for the terminal journal line before task effects begin.
pub const JOURNAL_TERMINAL_RESERVE_BYTES: usize = 64 * 1024;
/// Maximum entries in each bounded artifact collection.
pub const MAX_ARTIFACT_ENTRIES: usize = 1_024;
/// Maximum accepted JSON nesting.
pub const MAX_ARTIFACT_DEPTH: usize = 64;

const CHECK_SCHEMA: &str = "opaal.check.v1";
const PLAN_SCHEMA: &str = "opaal.plan.v1";
const JOURNAL_SCHEMA: &str = "opaal.run-journal.v1";
const AUDIT_SCHEMA: &str = "opaal.audit.v1";

/// SHA-256 identity of exact bytes in the persisted artifact spelling.
#[must_use]
pub fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Canonical JSON representation of one supported-host native path.
#[must_use]
pub fn native_path(path: &Path) -> Value {
    let mut object = Map::new();
    object.insert(
        "encoding".to_owned(),
        Value::String("base64url-nopad".to_owned()),
    );
    object.insert("platform".to_owned(), Value::String("unix".to_owned()));
    object.insert(
        "value".to_owned(),
        Value::String(encode_base64url(path.as_os_str())),
    );
    Value::Object(object)
}

/// Digest the exact canonical JSON representation of a schema subobject.
pub fn digest_value(value: &Value) -> Result<String, WorkflowArtifactError> {
    validate_depth(value, 0)?;
    canonical_bytes(value).map(|bytes| digest_bytes(&bytes))
}

/// Render non-negative Unix nanoseconds as exact UTC RFC 3339 nanoseconds.
pub fn timestamp_from_unix_nanos(nanos: i128) -> Result<String, WorkflowArtifactError> {
    let nanos = u128::try_from(nanos).map_err(|_| {
        WorkflowArtifactError::new("ARTIFACT007", "timestamp precedes the Unix epoch")
    })?;
    let seconds = nanos / 1_000_000_000;
    let fraction = nanos % 1_000_000_000;
    let days = i64::try_from(seconds / 86_400).map_err(|_| {
        WorkflowArtifactError::new("ARTIFACT007", "timestamp exceeds the supported range")
    })?;
    let day_seconds = u32::try_from(seconds % 86_400).expect("one day fits u32");
    let (year, month, day) = civil_date(days)?;
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction:09}Z"
    ))
}

/// Parse one canonical UTC timestamp into Unix nanoseconds.
pub fn unix_nanos_from_timestamp(value: &str) -> Result<i128, WorkflowArtifactError> {
    let TimestampComponents {
        year,
        month,
        day,
        hour,
        minute,
        second,
        fraction,
    } = timestamp_components(value)?;
    let days = days_from_civil(year, month, day)?;
    let seconds = i128::from(days)
        .checked_mul(86_400)
        .and_then(|value| value.checked_add(i128::from(hour) * 3_600))
        .and_then(|value| value.checked_add(i128::from(minute) * 60))
        .and_then(|value| value.checked_add(i128::from(second)))
        .ok_or_else(|| {
            WorkflowArtifactError::new("ARTIFACT007", "timestamp exceeds the supported range")
        })?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(i128::from(fraction)))
        .ok_or_else(|| {
            WorkflowArtifactError::new("ARTIFACT007", "timestamp exceeds the supported range")
        })
}

/// Decode a validated canonical Unix native-path record.
pub fn path_from_native_value(value: &Value) -> Result<PathBuf, WorkflowArtifactError> {
    validate_native_path(value)?;
    let encoded = value["value"]
        .as_str()
        .expect("validated native path value is a string");
    let bytes = decode_base64url(encoded)?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// A fail-closed persisted-lifecycle error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowArtifactError {
    code: &'static str,
    message: String,
}

impl WorkflowArtifactError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Stable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Redaction-safe diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for WorkflowArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WorkflowArtifactError {}

/// One validated canonical `opaal.check.v1` artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckArtifact(CanonicalArtifact);

impl CheckArtifact {
    /// Seal a complete check object whose `digest` field is absent.
    pub fn seal(document: Value) -> Result<Self, WorkflowArtifactError> {
        CanonicalArtifact::seal(ArtifactKind::Check, document).map(Self)
    }

    /// Parse a canonical check artifact and verify its complete closed schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, WorkflowArtifactError> {
        CanonicalArtifact::parse(ArtifactKind::Check, bytes).map(Self)
    }

    /// Exact canonical bytes, without a trailing newline.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The artifact's verified digest.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.0.digest()
    }

    /// The validated JSON value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        self.0.value()
    }

    /// Whether this check's static outcome permits execution.
    #[must_use]
    pub fn is_executable(&self) -> bool {
        self.0.value["outcome"]["class"] == "success"
    }
}

/// One validated canonical `opaal.plan.v1` artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanArtifact(CanonicalArtifact);

impl PlanArtifact {
    /// Seal a complete plan object whose `digest` field is absent.
    pub fn seal(document: Value) -> Result<Self, WorkflowArtifactError> {
        CanonicalArtifact::seal(ArtifactKind::Plan, document).map(Self)
    }

    /// Parse a canonical plan artifact and verify its complete closed schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, WorkflowArtifactError> {
        CanonicalArtifact::parse(ArtifactKind::Plan, bytes).map(Self)
    }

    /// Exact canonical bytes, without a trailing newline.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The immutable accepted-unit digest.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.0.digest()
    }

    /// The validated JSON value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        self.0.value()
    }

    /// Whether this plan's static outcome permits execution.
    #[must_use]
    pub fn is_executable(&self) -> bool {
        self.0.value["outcome"]["class"] == "success"
    }

    /// Canonical creation timestamp.
    #[must_use]
    pub fn created_at(&self) -> &str {
        self.0.value["created_at"]
            .as_str()
            .expect("validated plan timestamps are strings")
    }

    /// Canonical expiry timestamp.
    #[must_use]
    pub fn expires_at(&self) -> &str {
        self.0.value["expires_at"]
            .as_str()
            .expect("validated plan timestamps are strings")
    }
}

/// One validated canonical `opaal.audit.v1` artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditArtifact(CanonicalArtifact);

impl AuditArtifact {
    fn seal(document: Value) -> Result<Self, WorkflowArtifactError> {
        CanonicalArtifact::seal(ArtifactKind::Audit, document).map(Self)
    }

    /// Parse a canonical audit artifact and verify its complete closed schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, WorkflowArtifactError> {
        CanonicalArtifact::parse(ArtifactKind::Audit, bytes).map(Self)
    }

    /// Exact canonical bytes, without a trailing newline.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The artifact's verified digest.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.0.digest()
    }

    /// The validated JSON value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        self.0.value()
    }

    /// Whether the source journal ended in one valid terminal event.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.0.value["completeness"] == "complete"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArtifactKind {
    Check,
    Plan,
    Audit,
}

impl ArtifactKind {
    const fn schema(self) -> &'static str {
        match self {
            Self::Check => CHECK_SCHEMA,
            Self::Plan => PLAN_SCHEMA,
            Self::Audit => AUDIT_SCHEMA,
        }
    }

    const fn max_bytes(self) -> usize {
        match self {
            Self::Audit => MAX_AUDIT_BYTES,
            Self::Check | Self::Plan => MAX_ARTIFACT_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CanonicalArtifact {
    value: Value,
    bytes: Vec<u8>,
    digest: String,
}

impl CanonicalArtifact {
    fn seal(kind: ArtifactKind, mut value: Value) -> Result<Self, WorkflowArtifactError> {
        let object = value.as_object_mut().ok_or_else(|| {
            WorkflowArtifactError::new("ARTIFACT002", "artifact must be a JSON object")
        })?;
        if object.contains_key("digest") {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT003",
                "artifact sealing input must omit digest",
            ));
        }
        object.insert("digest".to_owned(), Value::String(digest_object(object)?));
        let bytes = canonical_bytes(&value)?;
        Self::from_parts(kind, value, bytes)
    }

    fn parse(kind: ArtifactKind, bytes: &[u8]) -> Result<Self, WorkflowArtifactError> {
        if bytes.len() > kind.max_bytes() {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT001",
                format!("artifact exceeds {} bytes", kind.max_bytes()),
            ));
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            WorkflowArtifactError::new("ARTIFACT002", format!("invalid JSON: {error}"))
        })?;
        validate_depth(&value, 0)?;
        let canonical = canonical_bytes(&value)?;
        if canonical != bytes {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT004",
                "artifact is not canonical JSON",
            ));
        }
        Self::from_parts(kind, value, canonical)
    }

    fn from_parts(
        kind: ArtifactKind,
        value: Value,
        bytes: Vec<u8>,
    ) -> Result<Self, WorkflowArtifactError> {
        validate_artifact(kind, &value)?;
        let object = value
            .as_object()
            .expect("artifact schema requires an object");
        let digest = required_digest(object, "digest")?.to_owned();
        if digest_object_without_digest(object)? != digest {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT005",
                "artifact digest does not match its canonical content",
            ));
        }
        if bytes.len() > kind.max_bytes() {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT001",
                format!("artifact exceeds {} bytes", kind.max_bytes()),
            ));
        }
        Ok(Self {
            value,
            bytes,
            digest,
        })
    }

    const fn value(&self) -> &Value {
        &self.value
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn digest(&self) -> &str {
        &self.digest
    }
}

/// Incremental state for one exclusively created, synchronously persisted run
/// journal. Each returned line must be written and synced by the caller before
/// the state transition or effect it describes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalChain {
    run_id: String,
    next_sequence: u64,
    previous: Option<String>,
    lines: usize,
    bytes: usize,
    terminal: bool,
    lifecycle: JournalLifecycle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OpenEffect {
    action_node_id: String,
    effect: String,
    scope: Value,
    attempt: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct JournalLifecycle {
    action_stack: Vec<String>,
    effect_stack: Vec<OpenEffect>,
    cleanup: Vec<Value>,
    cleanup_ordinals: Vec<u64>,
    cleanup_resource_ids: BTreeSet<String>,
    next_attempt: u64,
    action_phase_started: bool,
    cleanup_started: bool,
    terminal: bool,
}

impl JournalChain {
    /// Begin a journal and return its canonical header line, including newline.
    pub fn begin(
        run_id: &str,
        header_payload: Value,
    ) -> Result<(Self, Vec<u8>), WorkflowArtifactError> {
        validate_run_id(run_id)?;
        validate_journal_payload("header", &header_payload)?;
        let (line, digest) = seal_journal_line(run_id, 0, "header", None, header_payload)?;
        if line.len() > MAX_JOURNAL_BYTES - JOURNAL_TERMINAL_RESERVE_BYTES {
            return Err(journal_limit());
        }
        let state = Self {
            run_id: run_id.to_owned(),
            next_sequence: 1,
            previous: Some(digest),
            lines: 1,
            bytes: line.len(),
            terminal: false,
            lifecycle: JournalLifecycle::default(),
        };
        Ok((state, line))
    }

    /// Build the next canonical line. Nonterminal events leave the fixed line
    /// and byte reserve untouched; a terminal event may consume that reserve.
    pub fn append(&mut self, kind: &str, payload: Value) -> Result<Vec<u8>, WorkflowArtifactError> {
        if self.terminal {
            return Err(WorkflowArtifactError::new(
                "JOURNAL008",
                "journal content cannot follow terminal",
            ));
        }
        if kind == "header" {
            return Err(WorkflowArtifactError::new(
                "JOURNAL004",
                "journal header is unique and must be first",
            ));
        }
        validate_journal_payload(kind, &payload)?;
        let previous = self
            .previous
            .as_deref()
            .expect("a begun journal has a previous digest");
        let (line, digest) = seal_journal_line(
            &self.run_id,
            self.next_sequence,
            kind,
            Some(previous),
            payload.clone(),
        )?;
        let proposed_lines = self.lines.checked_add(1).ok_or_else(|| {
            WorkflowArtifactError::new("JOURNAL001", "journal line count overflow")
        })?;
        let proposed_bytes = self.bytes.checked_add(line.len()).ok_or_else(|| {
            WorkflowArtifactError::new("JOURNAL001", "journal byte count overflow")
        })?;
        if kind == "terminal" {
            if proposed_lines > MAX_JOURNAL_LINES || proposed_bytes > MAX_JOURNAL_BYTES {
                return Err(journal_limit());
            }
        } else if proposed_lines >= MAX_JOURNAL_LINES
            || proposed_bytes > MAX_JOURNAL_BYTES - JOURNAL_TERMINAL_RESERVE_BYTES
        {
            return Err(journal_limit());
        }
        let mut lifecycle = self.lifecycle.clone();
        lifecycle.admit(kind, &payload)?;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| WorkflowArtifactError::new("JOURNAL001", "journal sequence overflow"))?;
        self.previous = Some(digest);
        self.lines = proposed_lines;
        self.bytes = proposed_bytes;
        self.terminal = kind == "terminal";
        self.lifecycle = lifecycle;
        Ok(line)
    }

    /// Complete lines admitted so far.
    #[must_use]
    pub const fn lines(&self) -> usize {
        self.lines
    }

    /// Synced bytes represented by the chain so far.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    /// Whether the unique terminal line has been admitted.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }
}

impl JournalLifecycle {
    fn admit(&mut self, kind: &str, payload: &Value) -> Result<(), WorkflowArtifactError> {
        if self.terminal {
            return Err(journal_corrupt("journal content follows terminal"));
        }
        let object = payload
            .as_object()
            .expect("journal payload schema requires an object");
        match kind {
            "action-start" => {
                if self.cleanup_started {
                    return Err(journal_corrupt("action starts after cleanup began"));
                }
                if !self.effect_stack.is_empty() {
                    return Err(journal_corrupt("action starts inside an open effect"));
                }
                let node = required_string(object, "action_node_id")?;
                let contract = required_digest(object, "contract_digest")?;
                let ordinal = node
                    .strip_prefix(contract)
                    .and_then(|suffix| suffix.strip_prefix('#'));
                if !ordinal.is_some_and(|ordinal| {
                    ordinal.len() == 6 && ordinal.bytes().all(|byte| byte.is_ascii_digit())
                }) {
                    return Err(journal_corrupt(
                        "action-start node identity disagrees with its contract digest",
                    ));
                }
                self.action_phase_started = true;
                self.action_stack.push(node.to_owned());
            }
            "action-end" => {
                if !self.effect_stack.is_empty() {
                    return Err(journal_corrupt("action ends inside an open effect"));
                }
                let node = required_string(object, "action_node_id")?;
                if self.action_stack.pop().as_deref() != Some(node) {
                    return Err(journal_corrupt("action boundaries are not properly nested"));
                }
            }
            "effect-before" => {
                if self.cleanup_started {
                    return Err(journal_corrupt("effect starts after cleanup began"));
                }
                let node = required_string(object, "action_node_id")?;
                if let Some(active) = self.action_stack.last() {
                    if active != node {
                        return Err(journal_corrupt("effect is not owned by the active action"));
                    }
                } else if self.action_phase_started {
                    return Err(journal_corrupt("effect occurs outside an action boundary"));
                } else if required_string(object, "effect")? != "process.run" {
                    return Err(journal_corrupt(
                        "only maintained process probes may precede the action lifecycle",
                    ));
                }
                let attempt = required_u64(object, "attempt")?;
                if attempt != self.next_attempt {
                    return Err(journal_corrupt(
                        "effect attempt identities are not contiguous",
                    ));
                }
                self.next_attempt = self
                    .next_attempt
                    .checked_add(1)
                    .ok_or_else(|| journal_corrupt("effect attempt identity overflow"))?;
                self.effect_stack.push(OpenEffect {
                    action_node_id: node.to_owned(),
                    effect: required_string(object, "effect")?.to_owned(),
                    scope: required(object, "scope")?.clone(),
                    attempt,
                });
            }
            "effect-after" => {
                let Some(open) = self.effect_stack.pop() else {
                    return Err(journal_corrupt("effect-after has no matching before event"));
                };
                if open.action_node_id != required_string(object, "action_node_id")?
                    || open.effect != required_string(object, "effect")?
                    || open.scope != *required(object, "scope")?
                    || open.attempt != required_u64(object, "attempt")?
                {
                    return Err(journal_corrupt("effect before/after identities disagree"));
                }
            }
            "cleanup" => {
                if !self.action_stack.is_empty() || !self.effect_stack.is_empty() {
                    return Err(journal_corrupt(
                        "cleanup occurs before action and effect boundaries close",
                    ));
                }
                self.cleanup_started = true;
                let resource_id = required_string(object, "resource_id")?;
                if !self.cleanup_resource_ids.insert(resource_id.to_owned()) {
                    return Err(journal_corrupt(
                        "cleanup resource identities are not unique",
                    ));
                }
                let ordinal = required_u64(object, "ordinal")?;
                if self
                    .cleanup_ordinals
                    .last()
                    .is_some_and(|previous| previous.checked_sub(1) != Some(ordinal))
                {
                    return Err(journal_corrupt(
                        "cleanup resource ordinals are not contiguous in LIFO order",
                    ));
                }
                self.cleanup_ordinals.push(ordinal);
                self.cleanup.push(required(object, "outcome")?.clone());
            }
            "terminal" => {
                if !self.action_stack.is_empty() || !self.effect_stack.is_empty() {
                    return Err(journal_corrupt(
                        "terminal occurs before action and effect boundaries close",
                    ));
                }
                if required(object, "cleanup")? != &Value::Array(self.cleanup.clone()) {
                    return Err(journal_corrupt(
                        "terminal cleanup disagrees with preceding cleanup events",
                    ));
                }
                if self
                    .cleanup_ordinals
                    .last()
                    .is_some_and(|ordinal| *ordinal != 0)
                {
                    return Err(journal_corrupt(
                        "complete cleanup does not reach the first registered resource",
                    ));
                }
                self.terminal = true;
            }
            _ => return Err(journal_corrupt("unknown journal event kind")),
        }
        Ok(())
    }
}

/// Verify a run journal and produce one redacted ordered audit artifact.
///
/// An optional truncated final line is ignored only after every prior complete
/// line has passed schema, sequence, and digest-chain validation. Any earlier
/// corruption refuses the complete audit.
pub fn audit_journal(bytes: &[u8]) -> Result<AuditArtifact, WorkflowArtifactError> {
    if bytes.len() > MAX_JOURNAL_BYTES {
        return Err(journal_limit());
    }
    let complete_end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let complete = &bytes[..complete_end];
    let truncated = complete_end < bytes.len();
    let complete = complete.strip_suffix(b"\n").unwrap_or(complete);
    let lines = complete.split(|byte| *byte == b'\n');
    let mut previous = None;
    let mut run_id = None;
    let mut header = None;
    let mut events = Vec::new();
    let mut primary = Value::Null;
    let mut cleanup = Vec::new();
    let mut terminal_digest = Value::Null;
    let mut terminal_seen = false;
    let mut lifecycle = JournalLifecycle::default();
    for (sequence, line) in lines.enumerate() {
        if sequence >= MAX_JOURNAL_LINES {
            return Err(journal_limit());
        }
        let value = parse_journal_line(line)?;
        let object = value
            .as_object()
            .expect("journal line validation requires object");
        let line_run_id = required_string(object, "run_id")?;
        match run_id.as_deref() {
            None => run_id = Some(line_run_id.to_owned()),
            Some(expected) if expected == line_run_id => {}
            Some(_) => return Err(journal_corrupt("run_id changed within the journal")),
        }
        if required_u64(object, "seq")? != sequence as u64 {
            return Err(journal_corrupt("journal sequence is not contiguous"));
        }
        let line_previous = object.get("previous").expect("closed journal field exists");
        match (&previous, line_previous) {
            (None, Value::Null) => {}
            (Some(expected), Value::String(actual)) if expected == actual => {}
            _ => return Err(journal_corrupt("journal previous digest is invalid")),
        }
        let kind = required_string(object, "kind")?;
        if sequence == 0 && kind != "header" {
            return Err(journal_corrupt("journal must begin with header"));
        }
        if sequence > 0 && kind == "header" {
            return Err(journal_corrupt("journal contains multiple headers"));
        }
        if terminal_seen {
            return Err(journal_corrupt("journal content follows terminal"));
        }
        let payload = object.get("payload").expect("closed journal field exists");
        if kind == "header" {
            header = Some(payload.clone());
        } else {
            let root_action_end = kind == "action-end" && lifecycle.action_stack.len() == 1;
            lifecycle.admit(kind, payload)?;
            let event_digest = required_digest(object, "digest")?.to_owned();
            let mut event = Map::new();
            event.insert("seq".to_owned(), Value::from(sequence as u64));
            event.insert("kind".to_owned(), Value::String(kind.to_owned()));
            event.insert("payload".to_owned(), payload.clone());
            event.insert("digest".to_owned(), Value::String(event_digest.clone()));
            events.push(Value::Object(event));
            match kind {
                "action-end" if root_action_end => {
                    if let Some(outcome) = payload.get("outcome") {
                        primary = outcome.clone();
                    }
                }
                "cleanup" => {
                    cleanup.push(payload["outcome"].clone());
                }
                "terminal" => {
                    terminal_seen = true;
                    primary = payload["primary"].clone();
                    cleanup = payload["cleanup"]
                        .as_array()
                        .expect("terminal cleanup is validated")
                        .clone();
                    terminal_digest = Value::String(event_digest);
                }
                _ => {}
            }
        }
        previous = Some(required_digest(object, "digest")?.to_owned());
    }
    let header = header.ok_or_else(|| journal_corrupt("journal has no complete header"))?;
    if terminal_seen && truncated {
        return Err(journal_corrupt("content follows the terminal line"));
    }
    let header = header.as_object().expect("header payload is validated");
    if let Some(terminal) = events
        .last()
        .filter(|event| event["kind"] == "terminal")
        .and_then(|event| event["payload"].as_object())
    {
        let started = unix_nanos_from_timestamp(required_string(header, "started_at")?)?;
        let finished = unix_nanos_from_timestamp(required_string(terminal, "finished_at")?)?;
        if finished < started {
            return Err(journal_corrupt("terminal time precedes journal start"));
        }
    }
    let mut audit = Map::new();
    audit.insert("schema".to_owned(), Value::String(AUDIT_SCHEMA.to_owned()));
    audit.insert("schema_version".to_owned(), Value::from(1_u64));
    audit.insert(
        "run_id".to_owned(),
        Value::String(run_id.expect("header supplies run id")),
    );
    for name in ["plan_digest", "accepted_plan_digest", "authority_digest"] {
        audit.insert(name.to_owned(), header[name].clone());
    }
    audit.insert("events".to_owned(), Value::Array(events));
    audit.insert("primary".to_owned(), primary);
    audit.insert("cleanup".to_owned(), Value::Array(cleanup));
    audit.insert("journal_terminal_digest".to_owned(), terminal_digest);
    audit.insert(
        "completeness".to_owned(),
        Value::String(
            if terminal_seen {
                "complete"
            } else {
                "incomplete"
            }
            .to_owned(),
        ),
    );
    AuditArtifact::seal(Value::Object(audit))
}

fn validate_artifact(kind: ArtifactKind, value: &Value) -> Result<(), WorkflowArtifactError> {
    validate_depth(value, 0)?;
    let object = as_object(value, "artifact")?;
    match kind {
        ArtifactKind::Check => validate_check(object)?,
        ArtifactKind::Plan => validate_plan(object)?,
        ArtifactKind::Audit => validate_audit(object)?,
    }
    if required_string(object, "schema")? != kind.schema()
        || required_u64(object, "schema_version")? != 1
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT006",
            format!("artifact must use {} schema version 1", kind.schema()),
        ));
    }
    Ok(())
}

fn validate_check(object: &Map<String, Value>) -> Result<(), WorkflowArtifactError> {
    exact_keys(
        object,
        &[
            "schema",
            "schema_version",
            "toolchain",
            "project",
            "task",
            "inputs",
            "sources",
            "authority",
            "tools",
            "findings",
            "outcome",
            "digest",
        ],
        "check artifact",
    )?;
    validate_shared(object, false)?;
    bounded_array(object, "findings", validate_finding)?;
    require_ordered_unique(required_array(object, "findings")?, "findings", finding_key)?;
    validate_outcome(required(object, "outcome")?)?;
    validate_shared_cross_fields(object, None)?;
    let has_error = object["findings"]
        .as_array()
        .expect("validated findings are an array")
        .iter()
        .any(|finding| finding["severity"] == "error");
    let requests_executable = object["authority"]["requests"]
        .as_array()
        .expect("validated authority requests are an array")
        .iter()
        .all(|request| {
            matches!(
                request["verdict"].as_str(),
                Some("granted-enforced" | "granted-unenforced")
            )
        });
    let class = required_string(
        object["outcome"]
            .as_object()
            .expect("validated outcome is an object"),
        "class",
    )?;
    if ((has_error || !requests_executable) && class != "refused")
        || (!has_error && requests_executable && class != "success")
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "check findings, authority requests, and outcome disagree",
        ));
    }
    Ok(())
}

fn validate_plan(object: &Map<String, Value>) -> Result<(), WorkflowArtifactError> {
    exact_keys(
        object,
        &[
            "schema",
            "schema_version",
            "created_at",
            "expires_at",
            "toolchain",
            "platform",
            "project",
            "task",
            "inputs",
            "sources",
            "authority",
            "tools",
            "observations",
            "actions",
            "outcome",
            "digest",
        ],
        "plan artifact",
    )?;
    validate_timestamp(required_string(object, "created_at")?)?;
    validate_timestamp(required_string(object, "expires_at")?)?;
    validate_shared(object, true)?;
    let platform = required_object(object, "platform")?;
    exact_keys(platform, &["triple"], "platform")?;
    validate_id(required_string(platform, "triple")?)?;
    bounded_array(object, "observations", validate_observation)?;
    bounded_array(object, "actions", validate_action_node)?;
    validate_outcome(required(object, "outcome")?)?;
    let created = unix_nanos_from_timestamp(required_string(object, "created_at")?)?;
    let expires = unix_nanos_from_timestamp(required_string(object, "expires_at")?)?;
    if expires <= created || expires - created > 900_000_000_000 {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan expiry must be later than creation by at most 900 seconds",
        ));
    }
    validate_shared_cross_fields(object, Some(required_object(object, "platform")?))?;
    validate_plan_observations(object)?;
    validate_plan_actions(object)?;
    Ok(())
}

fn validate_shared(object: &Map<String, Value>, plan: bool) -> Result<(), WorkflowArtifactError> {
    let toolchain = required_object(object, "toolchain")?;
    exact_keys(toolchain, &["version"], "toolchain")?;
    validate_id(required_string(toolchain, "version")?)?;
    validate_project(required(object, "project")?)?;
    validate_task(required(object, "task")?)?;
    bounded_array(object, "inputs", validate_input)?;
    bounded_array(object, "sources", validate_source)?;
    validate_authority(required(object, "authority")?)?;
    bounded_array(object, "tools", |value| validate_tool(value, plan))?;
    Ok(())
}

fn validate_shared_cross_fields(
    object: &Map<String, Value>,
    platform: Option<&Map<String, Value>>,
) -> Result<(), WorkflowArtifactError> {
    let project = required_object(object, "project")?;
    require_ordered_unique(required_array(project, "tls")?, "TLS bindings", |value| {
        Ok(
            required_string(as_object(value, "TLS binding")?, "endpoint")?
                .as_bytes()
                .to_vec(),
        )
    })?;
    for binding in required_array(project, "tls")? {
        let binding = as_object(binding, "TLS binding")?;
        require_ordered_unique(
            required_array(binding, "methods")?,
            "TLS methods",
            string_key,
        )?;
        require_ordered_unique(
            required_array(binding, "secret_headers")?,
            "TLS secret headers",
            string_key,
        )?;
    }
    require_ordered_unique(required_array(object, "inputs")?, "inputs", |value| {
        Ok(required_string(as_object(value, "input")?, "name")?
            .as_bytes()
            .to_vec())
    })?;
    require_ordered_unique(required_array(object, "sources")?, "sources", |value| {
        Ok(required_string(as_object(value, "source")?, "module")?
            .as_bytes()
            .to_vec())
    })?;
    require_ordered_unique(required_array(object, "tools")?, "tools", |value| {
        Ok(required_string(as_object(value, "tool")?, "id")?
            .as_bytes()
            .to_vec())
    })?;
    let child_environment_digest = required_digest(project, "child_environment_digest")?;
    for tool in required_array(object, "tools")? {
        let tool = as_object(tool, "tool")?;
        if required_digest(tool, "child_environment_digest")? != child_environment_digest {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "tool and project child-environment digests disagree",
            ));
        }
        if let Some(platform) = platform
            && required_string(tool, "platform")? != required_string(platform, "triple")?
        {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "tool and plan platforms disagree",
            ));
        }
    }
    let authority = required_object(object, "authority")?;
    let rules = required_array(authority, "rules")?;
    let requests = required_array(authority, "requests")?;
    require_ordered_unique(rules, "authority rules", authority_key)?;
    require_ordered_unique(requests, "authority requests", authority_key)?;
    let mut rule_by_request = BTreeMap::new();
    for rule in rules {
        let rule = as_object(rule, "authority rule")?;
        let effect = required_string(rule, "effect")?;
        validate_effect_scope(effect, required(rule, "scope")?)?;
        let decision = required_string(rule, "decision")?;
        let enforcement = required(rule, "required_enforcement")?;
        if (decision == "deny" && !enforcement.is_null())
            || (decision == "grant" && enforcement.is_null())
        {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "authority decision and required enforcement disagree",
            ));
        }
        rule_by_request.insert(authority_map_key(rule)?, rule);
    }
    for request in requests {
        let request = as_object(request, "authority request")?;
        let effect = required_string(request, "effect")?;
        validate_effect_scope(effect, required(request, "scope")?)?;
        let process_platform = if effect == "process.run" {
            let tool_id = required_string(required_object(request, "scope")?, "tool")?;
            let tool = required_array(object, "tools")?
                .iter()
                .find(|tool| tool["id"] == tool_id)
                .ok_or_else(|| {
                    WorkflowArtifactError::new(
                        "ARTIFACT007",
                        "process request refers to an absent tool",
                    )
                })?;
            Some(required_string(as_object(tool, "tool")?, "platform")?)
        } else {
            None
        };
        let expected = match rule_by_request.get(&authority_map_key(request)?) {
            None => "denied",
            Some(rule) if required_string(rule, "decision")? == "deny" => "denied",
            Some(rule)
                if effect == "process.run"
                    && rule["required_enforcement"] == "acknowledged-unenforced"
                    && process_platform.is_some_and(|platform| {
                        matches!(platform, "aarch64-apple-darwin" | "x86_64-apple-darwin")
                    }) =>
            {
                "unsupported"
            }
            Some(rule)
                if effect == "process.run"
                    && rule["required_enforcement"] == "acknowledged-unenforced"
                    && process_platform.is_some_and(supports_exact_process_execution) =>
            {
                "granted-unenforced"
            }
            Some(rule)
                if effect == "process.run"
                    && rule["required_enforcement"] == "acknowledged-unenforced" =>
            {
                "unknown"
            }
            Some(rule) if effect != "process.run" && rule["required_enforcement"] == "enforced" => {
                "granted-enforced"
            }
            Some(_) => "denied",
        };
        if required_string(request, "verdict")? != expected {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "authority request verdict disagrees with its exact rule and adapter",
            ));
        }
    }
    Ok(())
}

fn validate_plan_observations(object: &Map<String, Value>) -> Result<(), WorkflowArtifactError> {
    let project = required_object(object, "project")?;
    let authority = required_object(object, "authority")?;
    let observations = required_array(object, "observations")?;
    let mut index = 0usize;
    expect_observation(
        observations,
        &mut index,
        "manifest",
        &project["name"],
        &project["manifest_path"],
        &project["manifest_digest"],
        None,
        &Value::Null,
    )?;
    require_observation_size(observations, index - 1, "manifest")?;
    for source in required_array(object, "sources")? {
        expect_observation(
            observations,
            &mut index,
            "source",
            &source["module"],
            &source["path"],
            &source["digest"],
            Some(&source["size"]),
            &Value::Null,
        )?;
    }
    expect_observation(
        observations,
        &mut index,
        "authority",
        &project["environment"],
        &authority["path"],
        &authority["digest"],
        None,
        &Value::Null,
    )?;
    require_observation_size(observations, index - 1, "authority")?;
    let tool_lock = observations.get(index).ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", "plan is missing tool-lock observation")
    })?;
    if tool_lock["kind"] != "tool-lock"
        || tool_lock["id"] != project["environment"]
        || !tool_lock["path"].is_object()
        || tool_lock["digest"] != project["tool_lock_digest"]
        || !tool_lock["size"].is_u64()
        || !tool_lock["observed_at"].is_null()
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "tool-lock observation disagrees with its bound identity",
        ));
    }
    index += 1;
    for input in required_array(object, "inputs")?
        .iter()
        .filter(|input| input["path"].is_object())
    {
        expect_observation(
            observations,
            &mut index,
            "input",
            &input["name"],
            &input["path"],
            &input["digest"],
            Some(&input["size"]),
            &Value::Null,
        )?;
    }
    for tls in required_array(project, "tls")? {
        expect_observation(
            observations,
            &mut index,
            "tls-ca",
            &tls["endpoint"],
            &tls["ca_path"],
            &tls["ca_digest"],
            None,
            &Value::Null,
        )?;
        require_observation_size(observations, index - 1, "tls-ca")?;
    }
    for tool in required_array(object, "tools")? {
        expect_observation(
            observations,
            &mut index,
            "tool-executable",
            &tool["id"],
            &tool["path"],
            &tool["digest"],
            None,
            &Value::Null,
        )?;
        require_observation_size(observations, index - 1, "tool-executable")?;
    }
    expect_observation(
        observations,
        &mut index,
        "child-environment",
        &project["environment"],
        &Value::Null,
        &project["child_environment_digest"],
        Some(&Value::Null),
        &Value::Null,
    )?;
    expect_observation(
        observations,
        &mut index,
        "wall-clock",
        &Value::String("created-at".to_owned()),
        &Value::Null,
        &Value::Null,
        Some(&Value::Null),
        required(object, "created_at")?,
    )?;
    if index != observations.len() {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan has extra or misordered observations",
        ));
    }
    Ok(())
}

fn require_observation_size(
    observations: &[Value],
    index: usize,
    kind: &str,
) -> Result<(), WorkflowArtifactError> {
    if observations[index]["size"].is_u64() {
        Ok(())
    } else {
        Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            format!("`{kind}` observation must record its byte size"),
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn expect_observation(
    observations: &[Value],
    index: &mut usize,
    kind: &str,
    id: &Value,
    path: &Value,
    digest: &Value,
    size: Option<&Value>,
    observed_at: &Value,
) -> Result<(), WorkflowArtifactError> {
    let observation = observations.get(*index).ok_or_else(|| {
        WorkflowArtifactError::new(
            "ARTIFACT007",
            format!("plan is missing `{kind}` observation {}", *index),
        )
    })?;
    *index += 1;
    if observation["kind"] != kind
        || &observation["id"] != id
        || &observation["path"] != path
        || &observation["digest"] != digest
        || size.is_some_and(|size| &observation["size"] != size)
        || &observation["observed_at"] != observed_at
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            format!("`{kind}` observation disagrees with its bound identity"),
        ));
    }
    Ok(())
}

fn validate_plan_actions(object: &Map<String, Value>) -> Result<(), WorkflowArtifactError> {
    let actions = required_array(object, "actions")?;
    if actions.is_empty() {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan must contain its selected action",
        ));
    }
    let task = required_object(object, "task")?;
    let authority = required_object(object, "authority")?;
    let mut nodes = BTreeMap::<String, Vec<String>>::new();
    let mut node_ordinals = BTreeMap::<String, usize>::new();
    let mut action_ids = BTreeSet::new();
    let mut union = BTreeMap::<Vec<u8>, Value>::new();
    for (ordinal, action) in actions.iter().enumerate() {
        let action = as_object(action, "action node")?;
        if required_u64(action, "ordinal")? != ordinal as u64 {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "action ordinals are not contiguous from zero",
            ));
        }
        let node_id = required_string(action, "id")?.to_owned();
        if !action_ids.insert(required_string(action, "action_id")?.to_owned()) {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "plan contains a duplicate action identity",
            ));
        }
        let requests = required_array(action, "requests")?;
        require_ordered_unique(requests, "action requests", authority_key)?;
        let action_executable = requests.iter().all(|request| {
            matches!(
                request["verdict"].as_str(),
                Some("granted-enforced" | "granted-unenforced")
            )
        });
        let action_class = required_string(
            action["outcome"]
                .as_object()
                .expect("validated action outcome is an object"),
            "class",
        )?;
        if (action_executable && action_class != "success")
            || (!action_executable && action_class != "refused")
        {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "action outcome disagrees with its authority requests",
            ));
        }
        for request in requests {
            union.insert(authority_key(request)?, request.clone());
        }
        let dependencies = required_array(action, "dependencies")?;
        require_ordered_unique(dependencies, "action dependencies", string_key)?;
        node_ordinals.insert(node_id.clone(), ordinal);
        nodes.insert(
            node_id,
            dependencies
                .iter()
                .map(|dependency| {
                    dependency
                        .as_str()
                        .expect("validated dependency is an id")
                        .to_owned()
                })
                .collect(),
        );
    }
    let root = actions[0]
        .as_object()
        .expect("validated action node is an object");
    if root["action_id"] != task["action_id"] || root["contract_digest"] != task["contract_digest"]
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan root action disagrees with the selected task",
        ));
    }
    for (node, dependencies) in &nodes {
        for dependency in dependencies {
            if dependency == node || !nodes.contains_key(dependency) {
                return Err(WorkflowArtifactError::new(
                    "ARTIFACT007",
                    "action dependency is self-referential or absent",
                ));
            }
        }
    }
    let root_id = required_string(root, "id")?;
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut traversal = Vec::new();
    visit_action_graph(
        root_id,
        &nodes,
        &node_ordinals,
        &mut visiting,
        &mut visited,
        &mut traversal,
        1,
    )?;
    if visited.len() != nodes.len() {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan contains an action outside the selected closure",
        ));
    }
    let declared_order = actions
        .iter()
        .map(|action| required_string(as_object(action, "action node")?, "id"))
        .collect::<Result<Vec<_>, _>>()?;
    if traversal.iter().map(String::as_str).ne(declared_order) {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan actions are not in depth-first dependency order",
        ));
    }
    let requests = required_array(authority, "requests")?;
    let union = union.into_values().collect::<Vec<_>>();
    if requests != union.as_slice() {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan authority requests disagree with the action closure",
        ));
    }
    let executable = requests.iter().all(|request| {
        matches!(
            request["verdict"].as_str(),
            Some("granted-enforced" | "granted-unenforced")
        )
    }) && actions
        .iter()
        .all(|action| action["outcome"]["class"] == "success");
    let class = required_string(
        object["outcome"]
            .as_object()
            .expect("validated outcome is an object"),
        "class",
    )?;
    if (executable && class != "success") || (!executable && class != "refused") {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan outcome disagrees with its requests and action outcomes",
        ));
    }
    Ok(())
}

fn visit_action_graph(
    node: &str,
    nodes: &BTreeMap<String, Vec<String>>,
    ordinals: &BTreeMap<String, usize>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    traversal: &mut Vec<String>,
    depth: usize,
) -> Result<(), WorkflowArtifactError> {
    if visited.contains(node) {
        return Ok(());
    }
    if !visiting.insert(node.to_owned()) {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan action dependency graph is cyclic",
        ));
    }
    if depth > 64 {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "plan action dependency depth exceeds 64",
        ));
    }
    traversal.push(node.to_owned());
    let mut dependencies = nodes[node].iter().collect::<Vec<_>>();
    dependencies.sort_by_key(|dependency| ordinals[*dependency]);
    for dependency in dependencies {
        visit_action_graph(
            dependency,
            nodes,
            ordinals,
            visiting,
            visited,
            traversal,
            depth + 1,
        )?;
    }
    visiting.remove(node);
    visited.insert(node.to_owned());
    Ok(())
}

fn validate_effect_scope(effect: &str, scope: &Value) -> Result<(), WorkflowArtifactError> {
    let kind = required_string(as_object(scope, "scope")?, "kind")?;
    let valid = matches!(
        (effect, kind),
        ("filesystem.read" | "filesystem.write", "project-path")
            | ("process.run", "tool")
            | ("network.http", "endpoint")
            | ("secret.reveal", "secret-sink")
            | ("clock.wall" | "clock.monotonic", "clock")
    );
    if !valid
        || (effect == "clock.wall" && scope["clock"] != "wall")
        || (effect == "clock.monotonic" && scope["clock"] != "monotonic")
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "effect and scope families disagree",
        ));
    }
    Ok(())
}

fn authority_key(value: &Value) -> Result<Vec<u8>, WorkflowArtifactError> {
    authority_map_key(as_object(value, "authority entry")?)
}

fn authority_map_key(object: &Map<String, Value>) -> Result<Vec<u8>, WorkflowArtifactError> {
    let mut key = required_string(object, "effect")?.as_bytes().to_vec();
    key.push(0);
    key.extend(canonical_bytes(required(object, "scope")?)?);
    Ok(key)
}

fn string_key(value: &Value) -> Result<Vec<u8>, WorkflowArtifactError> {
    value
        .as_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or_else(|| WorkflowArtifactError::new("ARTIFACT007", "array entry must be a string"))
}

fn finding_key(value: &Value) -> Result<Vec<u8>, WorkflowArtifactError> {
    let finding = as_object(value, "finding")?;
    canonical_bytes(&json!([
        required(finding, "severity")?,
        required(finding, "code")?,
        required(finding, "source")?,
        required(finding, "path")?,
        required(finding, "start_byte")?,
        required(finding, "end_byte")?,
        required(finding, "message")?
    ]))
}

fn require_ordered_unique(
    values: &[Value],
    context: &str,
    key: impl Fn(&Value) -> Result<Vec<u8>, WorkflowArtifactError>,
) -> Result<(), WorkflowArtifactError> {
    let mut previous: Option<Vec<u8>> = None;
    for value in values {
        let current = key(value)?;
        if previous
            .as_ref()
            .is_some_and(|previous| previous >= &current)
        {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                format!("{context} are not strictly sorted and unique"),
            ));
        }
        previous = Some(current);
    }
    Ok(())
}

fn validate_project(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "project")?;
    exact_keys(
        object,
        &[
            "name",
            "root",
            "manifest_path",
            "manifest_digest",
            "environment",
            "environment_digest",
            "tool_lock_digest",
            "child_environment_digest",
            "tls",
        ],
        "project",
    )?;
    for name in ["name", "environment"] {
        validate_id(required_string(object, name)?)?;
    }
    for name in ["root", "manifest_path"] {
        validate_native_path(required(object, name)?)?;
    }
    for name in [
        "manifest_digest",
        "environment_digest",
        "tool_lock_digest",
        "child_environment_digest",
    ] {
        required_digest(object, name)?;
    }
    bounded_array(object, "tls", validate_tls)?;
    Ok(())
}

fn validate_tls(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "TLS binding")?;
    exact_keys(
        object,
        &[
            "endpoint",
            "origin",
            "methods",
            "secret_headers",
            "ca_path",
            "ca_digest",
            "server_name",
        ],
        "TLS binding",
    )?;
    for name in ["endpoint", "origin", "server_name"] {
        validate_id(required_string(object, name)?)?;
    }
    for name in ["methods", "secret_headers"] {
        bounded_id_array(object, name)?;
    }
    if required_array(object, "methods")?.is_empty() {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "TLS binding methods must not be empty",
        ));
    }
    validate_native_path(required(object, "ca_path")?)?;
    required_digest(object, "ca_digest")?;
    Ok(())
}

fn validate_task(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "task")?;
    exact_keys(object, &["id", "action_id", "contract_digest"], "task")?;
    validate_id(required_string(object, "id")?)?;
    validate_id(required_string(object, "action_id")?)?;
    required_digest(object, "contract_digest")?;
    Ok(())
}

fn validate_input(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "input")?;
    exact_keys(
        object,
        &["name", "type", "value", "path", "digest", "size"],
        "input",
    )?;
    validate_id(required_string(object, "name")?)?;
    let value_type = required_string(object, "type")?;
    validate_id(value_type)?;
    required_string(object, "value")?;
    nullable_native_path(required(object, "path")?)?;
    nullable_digest(required(object, "digest")?)?;
    nullable_u64(required(object, "size")?)?;
    let has_path = required(object, "path")?.is_object();
    let has_digest = required(object, "digest")?.is_string();
    let has_size = required(object, "size")?.is_u64();
    if (value_type == "Path") != has_path || has_path != has_digest || has_path != has_size {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "only Path inputs carry a path, digest, and size observation",
        ));
    }
    Ok(())
}

fn validate_source(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "source")?;
    exact_keys(object, &["module", "path", "digest", "size"], "source")?;
    validate_id(required_string(object, "module")?)?;
    validate_native_path(required(object, "path")?)?;
    required_digest(object, "digest")?;
    required_u64(object, "size")?;
    Ok(())
}

fn validate_authority(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "authority")?;
    exact_keys(
        object,
        &["path", "digest", "rules", "requests"],
        "authority",
    )?;
    validate_native_path(required(object, "path")?)?;
    required_digest(object, "digest")?;
    bounded_array(object, "rules", validate_authority_rule)?;
    bounded_array(object, "requests", validate_authority_request)?;
    Ok(())
}

fn validate_authority_rule(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "authority rule")?;
    exact_keys(
        object,
        &["decision", "effect", "scope", "required_enforcement"],
        "authority rule",
    )?;
    one_of(
        required_string(object, "decision")?,
        &["grant", "deny"],
        "authority decision",
    )?;
    validate_id(required_string(object, "effect")?)?;
    validate_scope(required(object, "scope")?)?;
    nullable_one_of(
        required(object, "required_enforcement")?,
        &["enforced", "acknowledged-unenforced"],
        "required enforcement",
    )
}

fn validate_authority_request(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "authority request")?;
    exact_keys(object, &["effect", "scope", "verdict"], "authority request")?;
    validate_id(required_string(object, "effect")?)?;
    validate_scope(required(object, "scope")?)?;
    one_of(
        required_string(object, "verdict")?,
        &[
            "granted-enforced",
            "granted-unenforced",
            "denied",
            "unsupported",
            "unknown",
        ],
        "authority verdict",
    )
}

fn validate_scope(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "scope")?;
    match required_string(object, "kind")? {
        "project-path" => {
            exact_keys(object, &["kind", "path"], "project path scope")?;
            validate_native_path(required(object, "path")?)
        }
        "tool" => {
            exact_keys(object, &["kind", "tool"], "tool scope")?;
            validate_id(required_string(object, "tool")?)
        }
        "endpoint" => {
            exact_keys(object, &["kind", "endpoint", "method"], "endpoint scope")?;
            validate_id(required_string(object, "endpoint")?)?;
            validate_id(required_string(object, "method")?)
        }
        "secret-sink" => {
            exact_keys(
                object,
                &["kind", "secret", "endpoint", "header"],
                "secret sink scope",
            )?;
            for name in ["secret", "endpoint", "header"] {
                validate_id(required_string(object, name)?)?;
            }
            Ok(())
        }
        "clock" => {
            exact_keys(object, &["kind", "clock"], "clock scope")?;
            one_of(
                required_string(object, "clock")?,
                &["wall", "monotonic"],
                "clock",
            )
        }
        _ => Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "unknown scope kind",
        )),
    }
}

fn validate_tool(value: &Value, _plan: bool) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "tool")?;
    exact_keys(
        object,
        &[
            "id",
            "adapter",
            "path",
            "required_version",
            "locked_version",
            "digest",
            "platform",
            "child_environment_digest",
            "version_verified",
        ],
        "tool",
    )?;
    for name in ["id", "required_version", "locked_version", "platform"] {
        validate_id(required_string(object, name)?)?;
    }
    one_of(
        required_string(object, "adapter")?,
        &["git", "cargo"],
        "tool adapter",
    )?;
    validate_native_path(required(object, "path")?)?;
    required_digest(object, "digest")?;
    required_digest(object, "child_environment_digest")?;
    let verified = required_bool(object, "version_verified")?;
    if verified {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "check and plan artifacts cannot report a verified tool version",
        ));
    }
    Ok(())
}

fn validate_finding(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "finding")?;
    exact_keys(
        object,
        &[
            "severity",
            "code",
            "message",
            "source",
            "path",
            "start_byte",
            "end_byte",
        ],
        "finding",
    )?;
    one_of(
        required_string(object, "severity")?,
        &["info", "warning", "error"],
        "finding severity",
    )?;
    validate_id(required_string(object, "code")?)?;
    required_string(object, "message")?;
    nullable_id(required(object, "source")?)?;
    nullable_native_path(required(object, "path")?)?;
    nullable_u64(required(object, "start_byte")?)?;
    nullable_u64(required(object, "end_byte")?)
}

fn validate_observation(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "observation")?;
    exact_keys(
        object,
        &["kind", "id", "path", "digest", "size", "observed_at"],
        "observation",
    )?;
    one_of(
        required_string(object, "kind")?,
        &[
            "manifest",
            "source",
            "authority",
            "tool-lock",
            "input",
            "tls-ca",
            "tool-executable",
            "child-environment",
            "wall-clock",
        ],
        "observation kind",
    )?;
    validate_id(required_string(object, "id")?)?;
    nullable_native_path(required(object, "path")?)?;
    nullable_digest(required(object, "digest")?)?;
    nullable_u64(required(object, "size")?)?;
    nullable_timestamp(required(object, "observed_at")?)
}

fn validate_action_node(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "action node")?;
    exact_keys(
        object,
        &[
            "id",
            "ordinal",
            "action_id",
            "contract_digest",
            "requests",
            "dependencies",
            "outcome",
        ],
        "action node",
    )?;
    let id = required_string(object, "id")?;
    validate_id(id)?;
    let ordinal = required_u64(object, "ordinal")?;
    let contract = required_digest(object, "contract_digest")?;
    let expected = format!("{contract}#{ordinal:06}");
    if ordinal > 999_999 || id != expected {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "action node id is not the contract digest plus six-digit ordinal",
        ));
    }
    validate_id(required_string(object, "action_id")?)?;
    bounded_array(object, "requests", validate_authority_request)?;
    bounded_id_array(object, "dependencies")?;
    validate_outcome(required(object, "outcome")?)
}

fn validate_outcome(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "outcome")?;
    exact_keys(
        object,
        &[
            "class",
            "code",
            "message",
            "status",
            "value_digest",
            "partial",
        ],
        "outcome",
    )?;
    one_of(
        required_string(object, "class")?,
        &[
            "success",
            "status",
            "error",
            "result",
            "cancelled",
            "refused",
            "partial",
            "cleanup-failed",
        ],
        "outcome class",
    )?;
    validate_id(required_string(object, "code")?)?;
    required_string(object, "message")?;
    nullable_u64(required(object, "status")?)?;
    nullable_digest(required(object, "value_digest")?)?;
    required_bool(object, "partial")?;
    Ok(())
}

fn validate_audit(object: &Map<String, Value>) -> Result<(), WorkflowArtifactError> {
    exact_keys(
        object,
        &[
            "schema",
            "schema_version",
            "run_id",
            "plan_digest",
            "accepted_plan_digest",
            "authority_digest",
            "events",
            "primary",
            "cleanup",
            "journal_terminal_digest",
            "completeness",
            "digest",
        ],
        "audit artifact",
    )?;
    validate_run_id(required_string(object, "run_id")?)?;
    for name in ["plan_digest", "accepted_plan_digest", "authority_digest"] {
        required_digest(object, name)?;
    }
    bounded_array_limit(
        object,
        "events",
        MAX_JOURNAL_LINES - 1,
        validate_audit_event,
    )?;
    if !required(object, "primary")?.is_null() {
        validate_outcome(required(object, "primary")?)?;
    }
    bounded_array(object, "cleanup", validate_outcome)?;
    nullable_digest(required(object, "journal_terminal_digest")?)?;
    let completeness = required_string(object, "completeness")?;
    one_of(
        completeness,
        &["complete", "incomplete"],
        "audit completeness",
    )?;
    if (completeness == "complete") != required(object, "journal_terminal_digest")?.is_string() {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "audit completeness and terminal digest disagree",
        ));
    }
    let events = required(object, "events").and_then(|value| {
        value
            .as_array()
            .ok_or_else(|| WorkflowArtifactError::new("ARTIFACT007", "`events` must be an array"))
    })?;
    let mut lifecycle = JournalLifecycle::default();
    let mut derived_primary = Value::Null;
    let mut terminal_digest = Value::Null;
    for (index, event) in events.iter().enumerate() {
        let event = event
            .as_object()
            .expect("validated audit event is an object");
        if required_u64(event, "seq")? != (index + 1) as u64 {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "audit event sequences are not contiguous from one",
            ));
        }
        let kind = required_string(event, "kind")?;
        let root_action_end = kind == "action-end" && lifecycle.action_stack.len() == 1;
        let payload = required(event, "payload")?;
        lifecycle.admit(kind, payload)?;
        if root_action_end {
            derived_primary = payload["outcome"].clone();
        }
        if kind == "terminal" {
            derived_primary = payload["primary"].clone();
            terminal_digest = event["digest"].clone();
        }
    }
    if (completeness == "complete") != lifecycle.terminal {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "audit completeness disagrees with its event lifecycle",
        ));
    }
    if required(object, "primary")? != &derived_primary
        || required(object, "cleanup")? != &Value::Array(lifecycle.cleanup)
        || required(object, "journal_terminal_digest")? != &terminal_digest
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "audit summary disagrees with its verified event lifecycle",
        ));
    }
    Ok(())
}

fn validate_audit_event(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "audit event")?;
    exact_keys(object, &["seq", "kind", "payload", "digest"], "audit event")?;
    required_u64(object, "seq")?;
    let kind = required_string(object, "kind")?;
    if kind == "header" {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "audit events exclude the journal header",
        ));
    }
    validate_journal_payload(kind, required(object, "payload")?)?;
    required_digest(object, "digest")?;
    Ok(())
}

fn validate_journal_payload(kind: &str, value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "journal payload")?;
    match kind {
        "header" => {
            exact_keys(
                object,
                &[
                    "plan_digest",
                    "accepted_plan_digest",
                    "authority_digest",
                    "project_digest",
                    "environment_digest",
                    "tool_lock_digest",
                    "child_environment_digest",
                    "started_at",
                ],
                "journal header",
            )?;
            for name in [
                "plan_digest",
                "accepted_plan_digest",
                "authority_digest",
                "project_digest",
                "environment_digest",
                "tool_lock_digest",
                "child_environment_digest",
            ] {
                required_digest(object, name)?;
            }
            validate_timestamp(required_string(object, "started_at")?)
        }
        "action-start" => {
            exact_keys(
                object,
                &["action_node_id", "action_id", "contract_digest"],
                "action-start",
            )?;
            validate_id(required_string(object, "action_node_id")?)?;
            validate_id(required_string(object, "action_id")?)?;
            required_digest(object, "contract_digest")?;
            Ok(())
        }
        "action-end" => {
            exact_keys(object, &["action_node_id", "outcome"], "action-end")?;
            validate_id(required_string(object, "action_node_id")?)?;
            validate_outcome(required(object, "outcome")?)
        }
        "effect-before" => {
            exact_keys(
                object,
                &["action_node_id", "effect", "scope", "verdict", "attempt"],
                "effect-before",
            )?;
            validate_id(required_string(object, "action_node_id")?)?;
            validate_id(required_string(object, "effect")?)?;
            validate_scope(required(object, "scope")?)?;
            validate_effect_scope(
                required_string(object, "effect")?,
                required(object, "scope")?,
            )
            .map_err(|_| journal_corrupt("effect and scope families disagree"))?;
            one_of(
                required_string(object, "verdict")?,
                &[
                    "granted-enforced",
                    "granted-unenforced",
                    "denied",
                    "unsupported",
                    "unknown",
                ],
                "authority verdict",
            )?;
            if !matches!(
                required_string(object, "verdict")?,
                "granted-enforced" | "granted-unenforced"
            ) {
                return Err(journal_corrupt(
                    "an attempted effect must have an executable verdict",
                ));
            }
            required_u64(object, "attempt")?;
            Ok(())
        }
        "effect-after" => {
            exact_keys(
                object,
                &[
                    "action_node_id",
                    "effect",
                    "scope",
                    "attempt",
                    "outcome",
                    "evidence_digest",
                ],
                "effect-after",
            )?;
            validate_id(required_string(object, "action_node_id")?)?;
            validate_id(required_string(object, "effect")?)?;
            validate_scope(required(object, "scope")?)?;
            validate_effect_scope(
                required_string(object, "effect")?,
                required(object, "scope")?,
            )
            .map_err(|_| journal_corrupt("effect and scope families disagree"))?;
            required_u64(object, "attempt")?;
            validate_outcome(required(object, "outcome")?)?;
            nullable_digest(required(object, "evidence_digest")?)
        }
        "cleanup" => {
            exact_keys(
                object,
                &["action_node_id", "resource_id", "ordinal", "outcome"],
                "cleanup",
            )?;
            nullable_id(required(object, "action_node_id")?)?;
            validate_id(required_string(object, "resource_id")?)?;
            required_u64(object, "ordinal")?;
            validate_outcome(required(object, "outcome")?)
        }
        "terminal" => {
            exact_keys(
                object,
                &["finished_at", "primary", "cleanup", "complete"],
                "terminal",
            )?;
            validate_timestamp(required_string(object, "finished_at")?)?;
            validate_outcome(required(object, "primary")?)?;
            bounded_array(object, "cleanup", validate_outcome)?;
            if !required_bool(object, "complete")? {
                return Err(WorkflowArtifactError::new(
                    "JOURNAL003",
                    "terminal complete must be true",
                ));
            }
            Ok(())
        }
        _ => Err(WorkflowArtifactError::new(
            "JOURNAL003",
            "unknown journal event kind",
        )),
    }
}

fn seal_journal_line(
    run_id: &str,
    seq: u64,
    kind: &str,
    previous: Option<&str>,
    payload: Value,
) -> Result<(Vec<u8>, String), WorkflowArtifactError> {
    let mut object = Map::new();
    object.insert(
        "schema".to_owned(),
        Value::String(JOURNAL_SCHEMA.to_owned()),
    );
    object.insert("schema_version".to_owned(), Value::from(1_u64));
    object.insert("run_id".to_owned(), Value::String(run_id.to_owned()));
    object.insert("seq".to_owned(), Value::from(seq));
    object.insert("kind".to_owned(), Value::String(kind.to_owned()));
    object.insert(
        "previous".to_owned(),
        previous.map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
    object.insert("payload".to_owned(), payload);
    let digest = digest_object(&object)?;
    object.insert("digest".to_owned(), Value::String(digest.clone()));
    let mut line = canonical_bytes(&Value::Object(object))?;
    line.push(b'\n');
    Ok((line, digest))
}

fn parse_journal_line(bytes: &[u8]) -> Result<Value, WorkflowArtifactError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| journal_corrupt(format!("invalid JSON: {error}")))?;
    validate_depth(&value, 0)?;
    if canonical_bytes(&value)? != bytes {
        return Err(journal_corrupt("journal line is not canonical JSON"));
    }
    let object = as_object(&value, "journal line")?;
    exact_keys(
        object,
        &[
            "schema",
            "schema_version",
            "run_id",
            "seq",
            "kind",
            "previous",
            "payload",
            "digest",
        ],
        "journal line",
    )?;
    if required_string(object, "schema")? != JOURNAL_SCHEMA
        || required_u64(object, "schema_version")? != 1
    {
        return Err(journal_corrupt("wrong journal schema or version"));
    }
    validate_run_id(required_string(object, "run_id")?)?;
    required_u64(object, "seq")?;
    let kind = required_string(object, "kind")?;
    match required(object, "previous")? {
        Value::Null => {}
        Value::String(value) => {
            validate_digest(value)?;
        }
        _ => return Err(journal_corrupt("previous must be null or a digest")),
    }
    validate_journal_payload(kind, required(object, "payload")?)?;
    let digest = required_digest(object, "digest")?;
    if digest_object_without_digest(object)? != digest {
        return Err(journal_corrupt("journal line digest mismatch"));
    }
    Ok(value)
}

fn canonical_bytes(value: &Value) -> Result<Vec<u8>, WorkflowArtifactError> {
    let mut bytes = Vec::new();
    write_canonical_json(value, &mut bytes)?;
    Ok(bytes)
}

fn write_canonical_json(value: &Value, bytes: &mut Vec<u8>) -> Result<(), WorkflowArtifactError> {
    match value {
        Value::Null => bytes.extend_from_slice(b"null"),
        Value::Bool(true) => bytes.extend_from_slice(b"true"),
        Value::Bool(false) => bytes.extend_from_slice(b"false"),
        Value::Number(number) => {
            let value = number.as_u64().ok_or_else(|| {
                WorkflowArtifactError::new(
                    "ARTIFACT007",
                    "artifact numbers must be non-negative integers",
                )
            })?;
            bytes.extend_from_slice(value.to_string().as_bytes());
        }
        Value::String(value) => serde_json::to_writer(bytes, value)
            .map_err(|error| WorkflowArtifactError::new("ARTIFACT002", error.to_string()))?,
        Value::Array(values) => {
            bytes.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    bytes.push(b',');
                }
                write_canonical_json(value, bytes)?;
            }
            bytes.push(b']');
        }
        Value::Object(values) => {
            let mut fields = values.iter().collect::<Vec<_>>();
            fields.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            bytes.push(b'{');
            for (index, (name, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    bytes.push(b',');
                }
                serde_json::to_writer(&mut *bytes, name).map_err(|error| {
                    WorkflowArtifactError::new("ARTIFACT002", error.to_string())
                })?;
                bytes.push(b':');
                write_canonical_json(value, bytes)?;
            }
            bytes.push(b'}');
        }
    }
    Ok(())
}

fn digest_object(object: &Map<String, Value>) -> Result<String, WorkflowArtifactError> {
    let bytes = canonical_bytes(&Value::Object(object.clone()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn digest_object_without_digest(
    object: &Map<String, Value>,
) -> Result<String, WorkflowArtifactError> {
    let mut unsigned = object.clone();
    unsigned.remove("digest");
    digest_object(&unsigned)
}

fn validate_depth(value: &Value, depth: usize) -> Result<(), WorkflowArtifactError> {
    if depth > MAX_ARTIFACT_DEPTH {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT001",
            "artifact nesting exceeds its limit",
        ));
    }
    match value {
        Value::Array(values) => {
            for value in values {
                validate_depth(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_depth(value, depth + 1)?;
            }
        }
        Value::Number(number) if !number.is_u64() => {
            return Err(WorkflowArtifactError::new(
                "ARTIFACT007",
                "artifact numbers must be non-negative integers",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn exact_keys(
    object: &Map<String, Value>,
    expected: &[&str],
    context: &str,
) -> Result<(), WorkflowArtifactError> {
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            format!("{context} has missing or unknown fields"),
        ));
    }
    Ok(())
}

fn required<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Value, WorkflowArtifactError> {
    object
        .get(name)
        .ok_or_else(|| WorkflowArtifactError::new("ARTIFACT007", format!("missing field `{name}`")))
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Map<String, Value>, WorkflowArtifactError> {
    as_object(required(object, name)?, name)
}
fn required_array<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Vec<Value>, WorkflowArtifactError> {
    required(object, name)?.as_array().ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", format!("`{name}` must be an array"))
    })
}
fn as_object<'a>(
    value: &'a Value,
    context: &str,
) -> Result<&'a Map<String, Value>, WorkflowArtifactError> {
    value.as_object().ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", format!("{context} must be an object"))
    })
}
fn required_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, WorkflowArtifactError> {
    required(object, name)?.as_str().ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", format!("`{name}` must be a string"))
    })
}
fn required_u64(object: &Map<String, Value>, name: &str) -> Result<u64, WorkflowArtifactError> {
    required(object, name)?.as_u64().ok_or_else(|| {
        WorkflowArtifactError::new(
            "ARTIFACT007",
            format!("`{name}` must be a non-negative integer"),
        )
    })
}
fn required_bool(object: &Map<String, Value>, name: &str) -> Result<bool, WorkflowArtifactError> {
    required(object, name)?.as_bool().ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", format!("`{name}` must be a boolean"))
    })
}
fn required_digest<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, WorkflowArtifactError> {
    let value = required_string(object, name)?;
    validate_digest(value)?;
    Ok(value)
}

fn bounded_array(
    object: &Map<String, Value>,
    name: &str,
    validate: impl Fn(&Value) -> Result<(), WorkflowArtifactError>,
) -> Result<(), WorkflowArtifactError> {
    bounded_array_limit(object, name, MAX_ARTIFACT_ENTRIES, validate)
}

fn bounded_array_limit(
    object: &Map<String, Value>,
    name: &str,
    maximum: usize,
    validate: impl Fn(&Value) -> Result<(), WorkflowArtifactError>,
) -> Result<(), WorkflowArtifactError> {
    let values = required(object, name)?.as_array().ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", format!("`{name}` must be an array"))
    })?;
    if values.len() > maximum {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT001",
            format!("`{name}` exceeds {maximum} entries"),
        ));
    }
    for value in values {
        validate(value)?;
    }
    Ok(())
}

fn bounded_id_array(object: &Map<String, Value>, name: &str) -> Result<(), WorkflowArtifactError> {
    bounded_array(object, name, |value| {
        value
            .as_str()
            .ok_or_else(|| {
                WorkflowArtifactError::new(
                    "ARTIFACT007",
                    format!("`{name}` entries must be strings"),
                )
            })
            .and_then(validate_id)
    })
}
fn validate_id(value: &str) -> Result<(), WorkflowArtifactError> {
    if value.is_empty() || value.nfc().ne(value.chars()) {
        Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "identity must be non-empty NFC text",
        ))
    } else {
        Ok(())
    }
}
fn validate_digest(value: &str) -> Result<(), WorkflowArtifactError> {
    if value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        Ok(())
    } else {
        Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "digest must be sha256 plus 64 lowercase hexadecimal digits",
        ))
    }
}
fn validate_run_id(value: &str) -> Result<(), WorkflowArtifactError> {
    if value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(WorkflowArtifactError::new(
            "JOURNAL002",
            "run id must be 32 lowercase hexadecimal digits",
        ))
    }
}
fn validate_native_path(value: &Value) -> Result<(), WorkflowArtifactError> {
    let object = as_object(value, "native path")?;
    exact_keys(object, &["encoding", "platform", "value"], "native path")?;
    if required_string(object, "encoding")? != "base64url-nopad"
        || required_string(object, "platform")? != "unix"
        || !is_base64url_nopad(required_string(object, "value")?)
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "native path must use canonical Unix base64url without padding",
        ));
    }
    Ok(())
}
fn nullable_native_path(value: &Value) -> Result<(), WorkflowArtifactError> {
    if value.is_null() {
        Ok(())
    } else {
        validate_native_path(value)
    }
}
fn nullable_id(value: &Value) -> Result<(), WorkflowArtifactError> {
    if value.is_null() {
        Ok(())
    } else {
        value
            .as_str()
            .ok_or_else(|| {
                WorkflowArtifactError::new(
                    "ARTIFACT007",
                    "nullable identity must be null or string",
                )
            })
            .and_then(validate_id)
    }
}
fn nullable_digest(value: &Value) -> Result<(), WorkflowArtifactError> {
    if value.is_null() {
        Ok(())
    } else {
        value
            .as_str()
            .ok_or_else(|| {
                WorkflowArtifactError::new("ARTIFACT007", "nullable digest must be null or string")
            })
            .and_then(validate_digest)
    }
}
fn nullable_u64(value: &Value) -> Result<(), WorkflowArtifactError> {
    if value.is_null() || value.as_u64().is_some() {
        Ok(())
    } else {
        Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "nullable integer must be null or non-negative integer",
        ))
    }
}
fn nullable_timestamp(value: &Value) -> Result<(), WorkflowArtifactError> {
    if value.is_null() {
        Ok(())
    } else {
        value
            .as_str()
            .ok_or_else(|| {
                WorkflowArtifactError::new(
                    "ARTIFACT007",
                    "nullable timestamp must be null or string",
                )
            })
            .and_then(validate_timestamp)
    }
}
fn nullable_one_of(
    value: &Value,
    allowed: &[&str],
    context: &str,
) -> Result<(), WorkflowArtifactError> {
    if value.is_null() {
        Ok(())
    } else {
        one_of(
            value.as_str().ok_or_else(|| {
                WorkflowArtifactError::new(
                    "ARTIFACT007",
                    format!("{context} must be null or string"),
                )
            })?,
            allowed,
            context,
        )
    }
}
fn one_of(value: &str, allowed: &[&str], context: &str) -> Result<(), WorkflowArtifactError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            format!("invalid {context}"),
        ))
    }
}

fn validate_timestamp(value: &str) -> Result<(), WorkflowArtifactError> {
    timestamp_components(value).map(|_| ())
}

struct TimestampComponents {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    fraction: u32,
}

fn timestamp_components(value: &str) -> Result<TimestampComponents, WorkflowArtifactError> {
    let bytes = value.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
        (29, b'Z'),
    ];
    if bytes.len() != 30
        || punctuation
            .iter()
            .any(|(index, byte)| bytes[*index] != *byte)
        || bytes.iter().enumerate().any(|(index, byte)| {
            !punctuation.iter().any(|(position, _)| *position == index) && !byte.is_ascii_digit()
        })
    {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "timestamp must be RFC 3339 UTC with exactly nanosecond precision",
        ));
    }
    let number = |start: usize, end: usize| {
        std::str::from_utf8(&bytes[start..end])
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
    };
    let (
        Some(year),
        Some(month),
        Some(day),
        Some(hour),
        Some(minute),
        Some(second),
        Some(fraction),
    ) = (
        number(0, 4),
        number(5, 7),
        number(8, 10),
        number(11, 13),
        number(14, 16),
        number(17, 19),
        number(20, 29),
    )
    else {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "timestamp contains an invalid number",
        ));
    };
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        _ => 0,
    };
    if day == 0 || day > month_days || hour > 23 || minute > 59 || second > 59 {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "timestamp components are out of range",
        ));
    }
    Ok(TimestampComponents {
        year: year as i32,
        month,
        day,
        hour,
        minute,
        second,
        fraction,
    })
}

fn is_base64url_nopad(value: &str) -> bool {
    let Some(digits) = value
        .bytes()
        .map(base64url_digit)
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    match digits.len() % 4 {
        0 => true,
        2 => digits.last().is_some_and(|digit| digit & 0b00_1111 == 0),
        3 => digits.last().is_some_and(|digit| digit & 0b00_0011 == 0),
        _ => false,
    }
}

fn base64url_digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn decode_base64url(value: &str) -> Result<Vec<u8>, WorkflowArtifactError> {
    if !is_base64url_nopad(value) {
        return Err(WorkflowArtifactError::new(
            "ARTIFACT007",
            "native path uses invalid base64url bytes",
        ));
    }
    let mut decoded = Vec::with_capacity(value.len() / 4 * 3 + 2);
    for chunk in value.as_bytes().chunks(4) {
        let digits = chunk
            .iter()
            .map(|byte| base64url_digit(*byte).expect("encoding was validated"))
            .collect::<Vec<_>>();
        decoded.push((digits[0] << 2) | (digits[1] >> 4));
        if digits.len() >= 3 {
            decoded.push((digits[1] << 4) | (digits[2] >> 2));
        }
        if digits.len() == 4 {
            decoded.push((digits[2] << 6) | digits[3]);
        }
    }
    Ok(decoded)
}
fn encode_base64url(value: &OsStr) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = value.as_bytes();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        let second = chunk.get(1).copied();
        encoded.push(ALPHABET[(((first & 0x03) << 4) | second.unwrap_or(0) >> 4) as usize] as char);
        if let Some(second) = second {
            let third = chunk.get(2).copied();
            encoded.push(
                ALPHABET[(((second & 0x0f) << 2) | third.unwrap_or(0) >> 6) as usize] as char,
            );
            if let Some(third) = third {
                encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
            }
        }
    }
    encoded
}

fn civil_date(days_since_epoch: i64) -> Result<(i32, u32, u32), WorkflowArtifactError> {
    let z = days_since_epoch.checked_add(719_468).ok_or_else(|| {
        WorkflowArtifactError::new("ARTIFACT007", "timestamp exceeds the supported range")
    })?;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_piece = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_piece + 2) / 5 + 1;
    let month = month_piece + if month_piece < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    let year = i32::try_from(year)
        .ok()
        .filter(|year| (0..=9999).contains(year))
        .ok_or_else(|| {
            WorkflowArtifactError::new("ARTIFACT007", "timestamp exceeds four-digit UTC years")
        })?;
    Ok((year, month as u32, day as u32))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Result<i64, WorkflowArtifactError> {
    let adjusted_year = i64::from(year) - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)
        .and_then(|value| value.checked_add(day_of_era))
        .and_then(|value| value.checked_sub(719_468))
        .ok_or_else(|| {
            WorkflowArtifactError::new("ARTIFACT007", "timestamp exceeds the supported range")
        })
}
fn journal_limit() -> WorkflowArtifactError {
    WorkflowArtifactError::new("JOURNAL001", "journal exceeds its line or byte limit")
}
fn journal_corrupt(message: impl Into<String>) -> WorkflowArtifactError {
    WorkflowArtifactError::new("JOURNAL003", message)
}
