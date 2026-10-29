//! Data-only display into a single capped, charged buffer before any transfer.

use std::fmt::{self, Write};

use crate::Value;
use crate::context::OperationalContext;
use crate::eval::ResourceBudget;
use crate::lifetime::Deadline;
use crate::module::{ValueType, is_opaque_operational_nominal};
use crate::value::{Status, write_bytes, write_string_content, write_utf8_with_invalid_bytes};

use super::super::standard::{charge, limit, poll};
use super::{ModuleError, domain};

const BLOCK: usize = 4096;
const MAX_DEPTH: usize = 128;

pub(super) fn prepare(
    value: &Value,
    cap: usize,
    context: &OperationalContext,
    budget: &mut ResourceBudget,
    deadline: Deadline,
) -> Result<Vec<u8>, ModuleError> {
    let mut display = Display {
        output: Vec::new(),
        cap,
        context,
        budget,
        deadline,
        input_bytes: 0,
        error: None,
    };
    if display.value(value, false, 1).is_err() {
        return Err(display.error.expect("bounded writer records its failure"));
    }
    Ok(display.output)
}

struct Display<'a> {
    output: Vec<u8>,
    cap: usize,
    context: &'a OperationalContext,
    budget: &'a mut ResourceBudget,
    deadline: Deadline,
    input_bytes: usize,
    error: Option<ModuleError>,
}

impl Display<'_> {
    fn checked(&mut self, result: Result<(), ModuleError>) -> fmt::Result {
        result.map_err(|error| {
            self.error = Some(error);
            fmt::Error
        })
    }

    fn step(&mut self) -> fmt::Result {
        let result = poll(self.context, self.deadline).and_then(|()| charge(self.budget));
        self.checked(result)
    }

    fn node(&mut self, depth: usize) -> fmt::Result {
        self.step()?;
        if depth > MAX_DEPTH {
            return self.checked(Err(limit()));
        }
        Ok(())
    }

    fn input(&mut self, length: usize) -> fmt::Result {
        if length > 0 {
            let blocks =
                (self.input_bytes + length).div_ceil(BLOCK) - self.input_bytes.div_ceil(BLOCK);
            // Also poll within a previously charged block at each new source slice.
            let result = poll(self.context, self.deadline);
            self.checked(result)?;
            for _ in 0..blocks {
                self.step()?;
            }
            self.input_bytes += length;
        }
        Ok(())
    }

    fn text(&mut self, mut text: &str, quoted_content: bool) -> fmt::Result {
        while !text.is_empty() {
            let mut end = text.len().min(BLOCK);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, rest) = text.split_at(end);
            self.input(chunk.len())?;
            if quoted_content {
                write_string_content(self, chunk)?;
            } else {
                self.write_str(chunk)?;
            }
            text = rest;
        }
        Ok(())
    }

    fn quoted(&mut self, text: &str) -> fmt::Result {
        self.write_char('"')?;
        self.text(text, true)?;
        self.write_char('"')
    }

    fn types(&mut self, arguments: &[ValueType], depth: usize) -> fmt::Result {
        if !arguments.is_empty() {
            self.write_char('[')?;
            for (index, argument) in arguments.iter().enumerate() {
                if index != 0 {
                    self.write_str(", ")?;
                }
                self.value_type(argument, depth)?;
            }
            self.write_char(']')?;
        }
        Ok(())
    }

    fn value_type(&mut self, value: &ValueType, depth: usize) -> fmt::Result {
        self.node(depth)?;
        match value {
            ValueType::List(element) => {
                self.write_str("List[")?;
                self.value_type(element, depth + 1)?;
                self.write_char(']')
            }
            ValueType::Nominal { id, arguments } => {
                self.text(id.name(), false)?;
                self.types(arguments, depth + 1)
            }
            ValueType::TypeParameter(name) => self.text(name, false),
            // These cases have fixed, scalar spellings in the shared type formatter.
            ValueType::Any
            | ValueType::Null
            | ValueType::Bool
            | ValueType::Int
            | ValueType::Float
            | ValueType::String
            | ValueType::Bytes
            | ValueType::Path
            | ValueType::Duration
            | ValueType::ByteSize
            | ValueType::Record
            | ValueType::Table
            | ValueType::Range
            | ValueType::Status
            | ValueType::Error
            | ValueType::Function
            | ValueType::Closure => write!(self, "{value}"),
        }
    }

    fn values(&mut self, values: &[Value], depth: usize) -> fmt::Result {
        for (index, value) in values.iter().enumerate() {
            if index != 0 {
                self.write_str(", ")?;
            }
            self.value(value, true, depth)?;
        }
        Ok(())
    }

    fn optional(&mut self, value: Option<i64>) -> fmt::Result {
        match value {
            Some(value) => write!(self, "{value}"),
            None => self.write_str("null"),
        }
    }

    fn status(&mut self, status: &Status, debug: bool, depth: usize) -> fmt::Result {
        if !debug {
            // Display omits stages; still bound and charge all status metadata.
            self.status_metadata(status, depth)?;
            if let Some(signal) = status.signal() {
                self.write_str("signal ")?;
                if let Some(name) = signal.name() {
                    self.text(name, false)?;
                    if let Some(number) = signal.number() {
                        write!(self, " ({number})")?;
                    }
                } else {
                    write!(self, "{}", signal.number().expect("signal identity"))?;
                }
                return Ok(());
            }
            return match status.code().expect("completion identity") {
                0 => self.write_str("success"),
                code => write!(self, "exit {code}"),
            };
        }
        self.write_str("status(code: ")?;
        self.optional(status.code())?;
        self.write_str(", signal: ")?;
        if let Some(signal) = status.signal() {
            self.step()?;
            self.write_str("signal(number: ")?;
            self.optional(signal.number())?;
            self.write_str(", name: ")?;
            match signal.name() {
                Some(name) => self.quoted(name)?,
                None => self.write_str("null")?,
            }
            self.write_char(')')?;
        } else {
            self.write_str("null")?;
        }
        self.write_str(", stages: [")?;
        for (index, stage) in status.stages().iter().enumerate() {
            if index != 0 {
                self.write_str(", ")?;
            }
            self.node(depth + 1)?;
            self.status(stage, true, depth + 1)?;
        }
        write!(self, "], duration: {}ns)", status.duration().as_nanos())
    }

    fn status_metadata(&mut self, status: &Status, depth: usize) -> fmt::Result {
        if let Some(signal) = status.signal() {
            self.step()?;
            if let Some(name) = signal.name() {
                for chunk in name.as_bytes().chunks(BLOCK) {
                    self.input(chunk.len())?;
                }
            }
        }
        for stage in status.stages() {
            self.node(depth + 1)?;
            self.status_metadata(stage, depth + 1)?;
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, debug: bool, depth: usize) -> fmt::Result {
        self.node(depth)?;
        match value {
            Value::Error(_) | Value::Callable(_) => self.checked(Err(ModuleError::invalid(
                "IO005",
                "value is not supported for standard printing",
            ))),
            Value::String(text) => {
                if debug {
                    self.quoted(text)
                } else {
                    self.text(text, false)
                }
            }
            Value::Bytes(bytes) => {
                if debug {
                    self.write_str("bytes\"")?;
                }
                for chunk in bytes.chunks(BLOCK) {
                    self.input(chunk.len())?;
                    write_bytes(self, chunk)?;
                }
                if debug {
                    self.write_char('"')?;
                }
                Ok(())
            }
            Value::Path(path) => {
                if debug {
                    self.write_str("path\"")?;
                }
                let mut bytes = path.as_os_str().as_encoded_bytes();
                while !bytes.is_empty() {
                    let mut end = bytes.len().min(BLOCK);
                    // Keep a possible split UTF8 sequence together without a full scan.
                    if end < bytes.len() {
                        for _ in 0..3 {
                            if !(0x80..=0xbf).contains(&bytes[end]) {
                                break;
                            }
                            end -= 1;
                        }
                    }
                    self.input(end)?;
                    write_utf8_with_invalid_bytes(self, &bytes[..end])?;
                    bytes = &bytes[end..];
                }
                if debug {
                    self.write_char('"')?;
                }
                Ok(())
            }
            Value::List(values) => {
                self.write_char('[')?;
                self.values(values, depth + 1)?;
                self.write_char(']')
            }
            Value::Record(record) => {
                self.write_char('{')?;
                for (index, (key, value)) in record.entries().iter().enumerate() {
                    if index != 0 {
                        self.write_str(", ")?;
                    }
                    self.quoted(key)?;
                    self.write_str(": ")?;
                    self.value(value, true, depth + 1)?;
                }
                self.write_char('}')
            }
            Value::NominalRecord(record) => {
                if is_opaque_operational_nominal(record.id()) {
                    return self.checked(Err(ModuleError::invalid(
                        "IO005",
                        "value is not supported for standard printing",
                    )));
                }
                self.text(record.id().name(), false)?;
                self.types(record.type_arguments(), depth + 1)?;
                self.write_str(" {")?;
                for (index, (name, value)) in record.fields().iter().enumerate() {
                    if index != 0 {
                        self.write_char(',')?;
                    }
                    self.write_char(' ')?;
                    self.text(name, false)?;
                    self.write_str(": ")?;
                    self.value(value, true, depth + 1)?;
                }
                if !record.fields().is_empty() {
                    self.write_char(' ')?;
                }
                self.write_char('}')
            }
            Value::Variant(variant) => {
                self.text(variant.id().name(), false)?;
                self.types(variant.type_arguments(), depth + 1)?;
                self.write_str("::")?;
                self.text(variant.constructor(), false)?;
                if !variant.payload().is_empty() {
                    self.write_char('(')?;
                    self.values(variant.payload(), depth + 1)?;
                    self.write_char(')')?;
                }
                Ok(())
            }
            Value::Table(table) => {
                self.write_str("table(columns: [")?;
                for (index, column) in table.columns().iter().enumerate() {
                    if index != 0 {
                        self.write_str(", ")?;
                    }
                    self.quoted(column)?;
                }
                self.write_str("], rows: [")?;
                for (index, row) in table.rows().iter().enumerate() {
                    self.step()?;
                    if index != 0 {
                        self.write_str(", ")?;
                    }
                    self.write_char('[')?;
                    self.values(row, depth + 1)?;
                    self.write_char(']')?;
                }
                self.write_str("])")
            }
            Value::Status(status) => self.status(status, debug, depth),
            // Scalar formatters have no callbacks, recursive data or unbounded input.
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Duration(_)
            | Value::ByteSize(_)
            | Value::Range(_) => {
                if debug {
                    write!(self, "{value:?}")
                } else {
                    write!(self, "{value}")
                }
            }
        }
    }
}

impl Write for Display<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            if self.output.len().is_multiple_of(BLOCK) {
                self.step()?;
            }
            let count = bytes.len().min(BLOCK - self.output.len() % BLOCK);
            let next = self.output.len() + count;
            if next > self.cap {
                return self.checked(Err(domain()));
            }
            if next > self.output.capacity() {
                let capacity = self.cap.min(self.output.capacity().saturating_add(BLOCK));
                if !self
                    .budget
                    .charge_collection_bytes(capacity - self.output.capacity())
                {
                    return self.checked(Err(limit()));
                }
                if self
                    .output
                    .try_reserve_exact(capacity - self.output.len())
                    .is_err()
                {
                    return self.checked(Err(limit()));
                }
            }
            self.output.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NominalRecordValue;
    use crate::authority::{AuthorityContext, EvaluationContextId};
    use crate::eval::{CancellationToken, FakeClock};
    use crate::module::{ModuleId, NominalTypeId};
    use std::sync::Arc;

    #[test]
    fn canonical_opaque_nominals_refuse_before_names_or_fields() {
        let context = OperationalContext::new(
            AuthorityContext::empty(EvaluationContextId::new(1).unwrap()),
            CancellationToken::never(),
            Arc::new(FakeClock::new()),
            None,
        );
        for (namespace, module, name) in [
            ("project", "tools", "Tool"),
            ("project", "endpoints", "Endpoint"),
            ("project", "secrets", "Secret"),
            ("std", "http", "SecretHeader"),
            ("std", "docker", "TargetIdentity"),
        ] {
            let value = Value::from(NominalRecordValue::new(
                NominalTypeId::new(ModuleId::standard(namespace, module), name.into()),
                vec![],
                vec![(Arc::from("secret"), Value::string("sensitive"))],
            ));
            let mut budget = ResourceBudget::steps(1);
            let deadline = context
                .cancellation()
                .operation_deadline(std::time::Duration::from_secs(30));
            assert_eq!(
                prepare(&value, 1024, &context, &mut budget, deadline)
                    .unwrap_err()
                    .code(),
                "IO005"
            );
            assert_eq!(budget.collection_bytes(), 0);
            let mut budget = ResourceBudget::opaal();
            assert_eq!(
                prepare(
                    &Value::list(vec![value]),
                    1024,
                    &context,
                    &mut budget,
                    deadline
                )
                .unwrap_err()
                .code(),
                "IO005"
            );
        }
    }
}
