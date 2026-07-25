use std::any::Any;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write};
use std::sync::Arc;

/// A finite IEEE 754 binary64 value with both zero spellings normalized.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct FiniteFloat(u64);

impl FiniteFloat {
    pub fn new(value: f64) -> Result<Self, NonFiniteFloat> {
        if !value.is_finite() {
            return Err(NonFiniteFloat);
        }
        let normalized = if value == 0.0 { 0.0 } else { value };
        Ok(Self(normalized.to_bits()))
    }

    #[must_use]
    pub fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl fmt::Debug for FiniteFloat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&float_text(self.get()))
    }
}

impl fmt::Display for FiniteFloat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

/// A rejected NaN or infinite runtime float.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NonFiniteFloat;

impl fmt::Display for NonFiniteFloat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime floats must be finite")
    }
}

impl Error for NonFiniteFloat {}

/// A signed nanosecond count.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub struct Duration(i128);

impl Duration {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn from_nanos(nanoseconds: i128) -> Self {
        Self(nanoseconds)
    }

    #[must_use]
    pub const fn as_nanos(self) -> i128 {
        self.0
    }
}

impl fmt::Debug for Duration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "duration({}ns)", self.0)
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}ns", self.0)
    }
}

/// An unsigned count of bytes.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub struct ByteSize(u64);

impl ByteSize {
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for ByteSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "size({}b)", self.0)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}b", self.0)
    }
}

/// An ascending unit-step range of `Int` endpoints with an inclusive-end flag.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Range {
    start: i64,
    end: i64,
    inclusive_end: bool,
}

impl Range {
    #[must_use]
    pub const fn new(start: i64, end: i64, inclusive_end: bool) -> Self {
        Self {
            start,
            end,
            inclusive_end,
        }
    }

    #[must_use]
    pub const fn start(self) -> i64 {
        self.start
    }

    #[must_use]
    pub const fn end(self) -> i64 {
        self.end
    }

    #[must_use]
    pub const fn includes_end(self) -> bool {
        self.inclusive_end
    }

    #[must_use]
    pub const fn contains(self, value: i64) -> bool {
        value >= self.start
            && if self.inclusive_end {
                value <= self.end
            } else {
                value < self.end
            }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        if self.inclusive_end {
            self.start > self.end
        } else {
            self.start >= self.end
        }
    }
}

impl fmt::Debug for Range {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let separator = if self.inclusive_end { "..=" } else { ".." };
        write!(formatter, "{}{separator}{}", self.start, self.end)
    }
}

impl fmt::Display for Range {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

/// An immutable platform-native path value.
#[derive(Clone, Eq, PartialEq)]
pub struct NativePath(Arc<OsStr>);

impl NativePath {
    #[must_use]
    pub fn new(path: impl Into<OsString>) -> Self {
        Self(Arc::from(path.into().into_boxed_os_str()))
    }

    #[must_use]
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

impl fmt::Debug for NativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("path\"")?;
        write_path_content(formatter, self.as_os_str())?;
        formatter.write_char('"')
    }
}

impl fmt::Display for NativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_path_content(formatter, self.as_os_str())
    }
}

impl From<OsString> for NativePath {
    fn from(path: OsString) -> Self {
        Self::new(path)
    }
}

/// An immutable insertion-ordered mapping from unique string keys to values.
#[derive(Clone, Eq, PartialEq)]
pub struct Record {
    entries: Arc<[(Arc<str>, Value)]>,
}

impl Record {
    pub fn new(entries: Vec<(String, Value)>) -> Result<Self, DuplicateRecordKey> {
        let mut checked: Vec<(Arc<str>, Value)> = Vec::with_capacity(entries.len());
        for (index, (key, value)) in entries.into_iter().enumerate() {
            if checked.iter().any(|(existing, _)| existing.as_ref() == key) {
                return Err(DuplicateRecordKey { key, index });
            }
            checked.push((Arc::from(key), value));
        }
        Ok(Self {
            entries: Arc::from(checked),
        })
    }

    #[must_use]
    pub fn entries(&self) -> &[(Arc<str>, Value)] {
        &self.entries
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find_map(|(candidate, value)| (candidate.as_ref() == key).then_some(value))
    }
}

impl fmt::Debug for Record {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_char('{')?;
        for (index, (key, value)) in self.entries.iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            write_quoted(formatter, key)?;
            write!(formatter, ": {value:?}")?;
        }
        formatter.write_char('}')
    }
}

impl fmt::Display for Record {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

/// The later occurrence of a duplicate record key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateRecordKey {
    key: String,
    index: usize,
}

impl DuplicateRecordKey {
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }
}

impl fmt::Display for DuplicateRecordKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "duplicate record key {:?} at entry {}",
            self.key, self.index
        )
    }
}

impl Error for DuplicateRecordKey {}

/// A platform signal identity with at least a number or a name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Signal {
    number: Option<i64>,
    name: Option<Arc<str>>,
}

impl Signal {
    pub fn new(number: Option<i64>, name: Option<String>) -> Result<Self, MissingSignalIdentity> {
        if number.is_none() && name.is_none() {
            return Err(MissingSignalIdentity);
        }
        Ok(Self {
            number,
            name: name.map(Arc::from),
        })
    }

    #[must_use]
    pub const fn number(&self) -> Option<i64> {
        self.number
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// A signal without either portable identity field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingSignalIdentity;

impl fmt::Display for MissingSignalIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a signal requires a number or name")
    }
}

impl Error for MissingSignalIdentity {}

/// An immutable completed stage or aggregate pipeline status.
#[derive(Clone, Eq, PartialEq)]
pub struct Status {
    code: Option<i64>,
    signal: Option<Signal>,
    stages: Arc<[Self]>,
    duration: Duration,
}

impl Status {
    pub fn exit(code: i64, duration: Duration) -> Result<Self, InvalidStatus> {
        validate_status_duration(duration)?;
        Ok(Self {
            code: Some(code),
            signal: None,
            stages: Arc::from([]),
            duration,
        })
    }

    pub fn signaled(signal: Signal, duration: Duration) -> Result<Self, InvalidStatus> {
        validate_status_duration(duration)?;
        Ok(Self {
            code: None,
            signal: Some(signal),
            stages: Arc::from([]),
            duration,
        })
    }

    pub fn aggregate(
        stages: Vec<Self>,
        selected_stage: usize,
        duration: Duration,
    ) -> Result<Self, InvalidStatus> {
        validate_status_duration(duration)?;
        if stages.is_empty() {
            return Err(InvalidStatus::EmptyAggregate);
        }
        let Some(selected) = stages.get(selected_stage) else {
            return Err(InvalidStatus::SelectedStageOutOfBounds {
                selected: selected_stage,
                stages: stages.len(),
            });
        };
        if stages.iter().any(|stage| !stage.stages.is_empty()) {
            return Err(InvalidStatus::NestedAggregate);
        }
        let code = selected.code;
        let signal = selected.signal.clone();
        Ok(Self {
            code,
            signal,
            stages: Arc::from(stages),
            duration,
        })
    }

    #[must_use]
    pub const fn code(&self) -> Option<i64> {
        self.code
    }

    #[must_use]
    pub const fn signal(&self) -> Option<&Signal> {
        self.signal.as_ref()
    }

    #[must_use]
    pub fn stages(&self) -> &[Self] {
        &self.stages
    }

    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self.code, Some(0))
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("status(code: ")?;
        write_optional(formatter, self.code)?;
        formatter.write_str(", signal: ")?;
        match &self.signal {
            Some(signal) => {
                formatter.write_str("signal(number: ")?;
                write_optional(formatter, signal.number)?;
                formatter.write_str(", name: ")?;
                match &signal.name {
                    Some(name) => write_quoted(formatter, name)?,
                    None => formatter.write_str("null")?,
                }
                formatter.write_char(')')?;
            }
            None => formatter.write_str("null")?,
        }
        formatter.write_str(", stages: [")?;
        for (index, stage) in self.stages.iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "{stage:?}")?;
        }
        write!(formatter, "], duration: {}ns)", self.duration.as_nanos())
    }
}

impl fmt::Display for Status {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.code, &self.signal) {
            (Some(0), None) => formatter.write_str("success"),
            (Some(code), None) => write!(formatter, "exit {code}"),
            (None, Some(signal)) => match (signal.name(), signal.number()) {
                (Some(name), Some(number)) => write!(formatter, "signal {name} ({number})"),
                (Some(name), None) => write!(formatter, "signal {name}"),
                (None, Some(number)) => write!(formatter, "signal {number}"),
                (None, None) => unreachable!("signal construction requires an identity"),
            },
            _ => unreachable!("status construction requires exactly one completion field"),
        }
    }
}

/// A violation of completed-status structural invariants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidStatus {
    NegativeDuration,
    EmptyAggregate,
    SelectedStageOutOfBounds { selected: usize, stages: usize },
    NestedAggregate,
}

impl fmt::Display for InvalidStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeDuration => formatter.write_str("status duration cannot be negative"),
            Self::EmptyAggregate => formatter.write_str("aggregate status requires stages"),
            Self::SelectedStageOutOfBounds { selected, stages } => write!(
                formatter,
                "selected stage {selected} is outside {stages} aggregate stages"
            ),
            Self::NestedAggregate => {
                formatter.write_str("aggregate status stages must all be leaves")
            }
        }
    }
}

impl Error for InvalidStatus {}

/// An immutable runtime value.
/// A runtime callable value: a named function or an anonymous closure.
///
/// Callables have runtime identity: two callables are equal only when they are
/// the same allocation. The concrete implementation is owned by the evaluator, so
/// the value model stays free of any syntax dependency and stores the callable
/// behind this object-safe trait.
pub trait Callable: fmt::Debug + Send + Sync {
    /// Returns the stable family name, `"function"` or `"closure"`.
    fn family(&self) -> &'static str;
    /// Writes the deterministic human display form.
    fn display(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result;
    /// Returns this callable as `&dyn Any` for evaluator-internal downcasting.
    fn as_any(&self) -> &dyn Any;
}

#[non_exhaustive]
#[derive(Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(FiniteFloat),
    String(Arc<str>),
    Bytes(Arc<[u8]>),
    Path(NativePath),
    Duration(Duration),
    ByteSize(ByteSize),
    List(Arc<[Self]>),
    Record(Record),
    Range(Range),
    Status(Status),
    Callable(Arc<dyn Callable>),
}

impl Value {
    #[must_use]
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(Arc::from(value.into()))
    }

    #[must_use]
    pub fn bytes(value: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(Arc::from(value.into()))
    }

    #[must_use]
    pub fn list(values: Vec<Self>) -> Self {
        Self::List(Arc::from(values))
    }

    #[must_use]
    pub fn as_list(&self) -> Option<&[Self]> {
        match self {
            Self::List(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the stable lowercase name of this value's family for diagnostics.
    #[must_use]
    pub fn family_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::String(_) => "string",
            Self::Bytes(_) => "bytes",
            Self::Path(_) => "path",
            Self::Duration(_) => "duration",
            Self::ByteSize(_) => "byte size",
            Self::List(_) => "list",
            Self::Record(_) => "record",
            Self::Range(_) => "range",
            Self::Status(_) => "status",
            Self::Callable(value) => value.family(),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Int(integer), Self::Float(float)) | (Self::Float(float), Self::Int(integer)) => {
                int_float_equal(*integer, *float)
            }
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Bytes(left), Self::Bytes(right)) => left == right,
            (Self::Path(left), Self::Path(right)) => left == right,
            (Self::Duration(left), Self::Duration(right)) => left == right,
            (Self::ByteSize(left), Self::ByteSize(right)) => left == right,
            (Self::List(left), Self::List(right)) => left == right,
            (Self::Record(left), Self::Record(right)) => left == right,
            (Self::Range(left), Self::Range(right)) => left == right,
            (Self::Status(left), Self::Status(right)) => left == right,
            (Self::Callable(left), Self::Callable(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl Eq for Value {}

impl fmt::Debug for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("null"),
            Self::Bool(value) => value.fmt(formatter),
            Self::Int(value) => value.fmt(formatter),
            Self::Float(value) => value.fmt(formatter),
            Self::String(value) => write_quoted(formatter, value),
            Self::Bytes(value) => {
                formatter.write_str("bytes\"")?;
                write_bytes(formatter, value)?;
                formatter.write_char('"')
            }
            Self::Path(value) => value.fmt(formatter),
            Self::Duration(value) => value.fmt(formatter),
            Self::ByteSize(value) => value.fmt(formatter),
            Self::List(values) => {
                formatter.write_char('[')?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{value:?}")?;
                }
                formatter.write_char(']')
            }
            Self::Record(value) => value.fmt(formatter),
            Self::Range(value) => value.fmt(formatter),
            Self::Status(value) => value.fmt(formatter),
            Self::Callable(value) => fmt::Debug::fmt(value, formatter),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("null"),
            Self::Bool(value) => value.fmt(formatter),
            Self::Int(value) => value.fmt(formatter),
            Self::Float(value) => value.fmt(formatter),
            Self::String(value) => formatter.write_str(value),
            Self::Bytes(value) => write_bytes(formatter, value),
            Self::Path(value) => value.fmt(formatter),
            Self::Duration(value) => value.fmt(formatter),
            Self::ByteSize(value) => value.fmt(formatter),
            Self::List(_) | Self::Record(_) => fmt::Debug::fmt(self, formatter),
            Self::Range(value) => value.fmt(formatter),
            Self::Status(value) => value.fmt(formatter),
            Self::Callable(value) => value.display(formatter),
        }
    }
}

impl From<FiniteFloat> for Value {
    fn from(value: FiniteFloat) -> Self {
        Self::Float(value)
    }
}

impl From<Duration> for Value {
    fn from(value: Duration) -> Self {
        Self::Duration(value)
    }
}

impl From<ByteSize> for Value {
    fn from(value: ByteSize) -> Self {
        Self::ByteSize(value)
    }
}

impl From<NativePath> for Value {
    fn from(value: NativePath) -> Self {
        Self::Path(value)
    }
}

impl From<Record> for Value {
    fn from(value: Record) -> Self {
        Self::Record(value)
    }
}

impl From<Range> for Value {
    fn from(value: Range) -> Self {
        Self::Range(value)
    }
}

impl From<Status> for Value {
    fn from(value: Status) -> Self {
        Self::Status(value)
    }
}

fn validate_status_duration(duration: Duration) -> Result<(), InvalidStatus> {
    if duration.as_nanos() < 0 {
        Err(InvalidStatus::NegativeDuration)
    } else {
        Ok(())
    }
}

fn int_float_equal(integer: i64, float: FiniteFloat) -> bool {
    let float = float.get();
    float.fract() == 0.0 && (float as i128) == i128::from(integer)
}

fn float_text(value: f64) -> String {
    if value == 0.0 {
        return "0.0".to_owned();
    }
    let mut text = value.to_string();
    if !text.contains(['.', 'e', 'E']) {
        text.push_str(".0");
    }
    text
}

fn write_optional<T: fmt::Display>(
    formatter: &mut fmt::Formatter<'_>,
    value: Option<T>,
) -> fmt::Result {
    match value {
        Some(value) => value.fmt(formatter),
        None => formatter.write_str("null"),
    }
}

fn write_quoted(formatter: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    formatter.write_char('"')?;
    write_string_content(formatter, value)?;
    formatter.write_char('"')
}

fn write_string_content(formatter: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    for character in value.chars() {
        match character {
            '"' => formatter.write_str("\\\"")?,
            '\\' => formatter.write_str("\\\\")?,
            '\n' => formatter.write_str("\\n")?,
            '\r' => formatter.write_str("\\r")?,
            '\t' => formatter.write_str("\\t")?,
            '\0' => formatter.write_str("\\0")?,
            character if character.is_control() => {
                write!(formatter, "\\u{{{:X}}}", u32::from(character))?;
            }
            character => formatter.write_char(character)?,
        }
    }
    Ok(())
}

fn write_bytes(formatter: &mut fmt::Formatter<'_>, value: &[u8]) -> fmt::Result {
    for byte in value {
        match byte {
            b'"' => formatter.write_str("\\\"")?,
            b'\\' => formatter.write_str("\\\\")?,
            0x20..=0x7e => formatter.write_char(char::from(*byte))?,
            byte => write!(formatter, "\\x{byte:02X}")?,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_path_content(formatter: &mut fmt::Formatter<'_>, value: &OsStr) -> fmt::Result {
    use std::os::unix::ffi::OsStrExt;

    write_utf8_with_invalid_bytes(formatter, value.as_bytes())
}

#[cfg(not(unix))]
fn write_path_content(formatter: &mut fmt::Formatter<'_>, value: &OsStr) -> fmt::Result {
    write_string_content(formatter, &value.to_string_lossy())
}

fn write_utf8_with_invalid_bytes(
    formatter: &mut fmt::Formatter<'_>,
    mut bytes: &[u8],
) -> fmt::Result {
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                write_string_content(formatter, text)?;
                break;
            }
            Err(error) => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                write_string_content(
                    formatter,
                    std::str::from_utf8(valid).expect("valid_up_to identifies UTF-8"),
                )?;
                let invalid_len = error.error_len().unwrap_or(rest.len());
                let (invalid, remaining) = rest.split_at(invalid_len);
                for byte in invalid {
                    write!(formatter, "\\x{byte:02X}")?;
                }
                bytes = remaining;
            }
        }
    }
    Ok(())
}
