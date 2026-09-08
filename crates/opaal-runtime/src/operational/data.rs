//! Deterministic bounded TOML decoding and canonical JSON encoding.

use std::io::{self, Write};

use crate::{FiniteFloat, Record, Value};

use super::ModuleError;

pub const MAX_DATA_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DATA_DEPTH: usize = 64;

pub fn toml_decode(bytes: &[u8]) -> Result<Value, ModuleError> {
    if bytes.len() > MAX_DATA_BYTES {
        return Err(ModuleError::invalid("DATA001", "TOML input exceeds 16 MiB"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|error| ModuleError::invalid("DATA002", format!("TOML is not UTF-8: {error}")))?;
    let value = text
        .parse::<toml::Value>()
        .map_err(|error| ModuleError::invalid("DATA003", format!("invalid TOML: {error}")))?;
    from_toml(value, 1)
}

fn from_toml(value: toml::Value, depth: usize) -> Result<Value, ModuleError> {
    if depth > MAX_DATA_DEPTH {
        return Err(ModuleError::invalid("DATA004", "TOML nesting exceeds 64"));
    }
    match value {
        toml::Value::String(value) => Ok(Value::string(value)),
        toml::Value::Integer(value) => Ok(Value::Int(value)),
        toml::Value::Float(value) => FiniteFloat::new(value)
            .map(Value::Float)
            .map_err(|_| ModuleError::invalid("DATA005", "TOML float is not finite")),
        toml::Value::Boolean(value) => Ok(Value::Bool(value)),
        toml::Value::Datetime(_) => Err(ModuleError::invalid(
            "DATA006",
            "TOML datetime values require an explicit time conversion",
        )),
        toml::Value::Array(values) => values
            .into_iter()
            .map(|value| from_toml(value, depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::list),
        toml::Value::Table(table) => {
            let mut entries = table.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let entries = entries
                .into_iter()
                .map(|(key, value)| Ok((key, from_toml(value, depth + 1)?)))
                .collect::<Result<Vec<_>, ModuleError>>()?;
            Record::new(entries)
                .map(Value::Record)
                .map_err(|error| ModuleError::invalid("DATA007", error.to_string()))
        }
    }
}

pub fn get<'a>(value: &'a Value, keys: &[&str]) -> Result<&'a Value, ModuleError> {
    let mut current = value;
    for key in keys {
        current = match current {
            Value::Record(record) => record.get(key).ok_or_else(|| {
                ModuleError::invalid("DATA008", format!("record has no key `{key}`"))
            })?,
            other => {
                return Err(ModuleError::invalid(
                    "DATA009",
                    format!("cannot read key `{key}` from {}", other.family_name()),
                ));
            }
        };
    }
    Ok(current)
}

pub fn json_encode(value: &Value) -> Result<Vec<u8>, ModuleError> {
    let mut output = LimitedJson::default();
    encode_json(value, &mut output, 1)?;
    Ok(output.bytes)
}

fn encode_json(value: &Value, output: &mut LimitedJson, depth: usize) -> Result<(), ModuleError> {
    if depth > MAX_DATA_DEPTH {
        return Err(ModuleError::invalid("DATA011", "JSON nesting exceeds 64"));
    }
    match value {
        Value::Null => output.append(b"null")?,
        Value::Bool(value) => output.append(if *value { b"true" } else { b"false" })?,
        Value::Int(value) => output.append(value.to_string().as_bytes())?,
        Value::Float(value) => {
            let number = serde_json::Number::from_f64(value.get())
                .ok_or_else(|| ModuleError::invalid("DATA012", "JSON float is not finite"))?;
            output.append(number.to_string().as_bytes())?;
        }
        Value::String(value) => output.serialize_string(value.as_ref())?,
        Value::List(values) => {
            output.append(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.append(b",")?;
                }
                encode_json(value, output, depth + 1)?;
            }
            output.append(b"]")?;
        }
        Value::Record(record) => {
            output.append(b"{")?;
            let mut entries = record.entries().iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.append(b",")?;
                }
                output.serialize_string(key.as_ref())?;
                output.append(b":")?;
                encode_json(value, output, depth + 1)?;
            }
            output.append(b"}")?;
        }
        other => {
            return Err(ModuleError::invalid(
                "DATA013",
                format!(
                    "{} has no canonical JSON representation",
                    other.family_name()
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct LimitedJson {
    bytes: Vec<u8>,
}

impl LimitedJson {
    fn append(&mut self, bytes: &[u8]) -> Result<(), ModuleError> {
        let proposed = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| ModuleError::invalid("DATA010", "JSON output size overflow"))?;
        if proposed > MAX_DATA_BYTES {
            return Err(ModuleError::invalid(
                "DATA010",
                "JSON output exceeds 16 MiB",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn serialize_string(&mut self, value: &str) -> Result<(), ModuleError> {
        serde_json::to_writer(self, value).map_err(|error| {
            if error.is_io() {
                ModuleError::invalid("DATA010", "JSON output exceeds 16 MiB")
            } else {
                ModuleError::invalid("DATA012", format!("cannot encode JSON string: {error}"))
            }
        })
    }
}

impl Write for LimitedJson {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let proposed = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("JSON output size overflow"))?;
        if proposed > MAX_DATA_BYTES {
            return Err(io::Error::other("JSON output exceeds 16 MiB"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
