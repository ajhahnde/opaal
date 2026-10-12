//! The host-free selected image decoder shared by saved and acquired data.

use std::sync::Arc;

use crate::module::{NominalTypeId, ValueType};
use crate::operational::ModuleError;
use crate::{NominalRecordValue, Record, Value, VariantValue};

pub(crate) const API_VERSION: &str = "1.48";
pub(crate) const TOOL_VERSION: &str = "29.9.0";

pub(crate) fn validate_socket(bytes: &[u8]) -> Result<(), ModuleError> {
    let maximum = if cfg!(target_os = "macos") { 103 } else { 107 };
    if bytes.len() > maximum
        || !bytes.starts_with(b"/")
        || bytes.ends_with(b"/")
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'/' | b'.' | b'_' | b'-'))
        || bytes[1..]
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || matches!(part, b"." | b".."))
    {
        return Err(ModuleError::invalid(
            "DOCKER002",
            "socket requires a normalized absolute ASCII path within the native Unix socket limit",
        ));
    }
    Ok(())
}

pub(crate) fn validate_operand(image: &str) -> Result<(), ModuleError> {
    if image.is_empty()
        || image.len() > 512
        || image.starts_with('-')
        || matches!(image, "__complete" | "__completeNoDesc")
        || image.bytes().any(|byte| !byte.is_ascii_graphic())
    {
        return Err(ModuleError::invalid(
            "DOCKER002",
            "image requires one nonempty bounded ASCII operand",
        ));
    }
    Ok(())
}

pub(crate) const MAX_IMAGE_BYTES: usize = 1024 * 1024;
const MAX_DIGESTS: usize = 256;
const MAX_SEGMENT_BYTES: usize = 512;
const MAX_REVISION_BYTES: usize = 4096;

pub(crate) fn decode_image<E>(
    document: &Record,
    visit: &mut impl FnMut(usize, usize) -> Result<(), E>,
    fail: impl Fn(ModuleError) -> E,
) -> Result<Value, E> {
    // Validate ignored data too, without retaining or serializing it.
    let mut bytes = 0;
    document_size(document, 1, &mut bytes, visit, &fail)?;
    let digests = match document.get("RepoDigests") {
        Some(Value::List(values)) => values.len().min(MAX_DIGESTS),
        _ => 0,
    };
    visit(
        7 + digests,
        6 * size_of::<(Arc<str>, Value)>() + (1 + digests) * size_of::<Value>() + 256,
    )?;
    selected_image(document).map_err(fail)
}

fn invalid(message: &'static str) -> ModuleError {
    ModuleError::invalid("DOCKER001", message)
}

fn add_size<E>(
    amount: usize,
    bytes: &mut usize,
    fail: &impl Fn(ModuleError) -> E,
) -> Result<(), E> {
    *bytes = bytes
        .checked_add(amount)
        .filter(|size| *size <= MAX_IMAGE_BYTES)
        .ok_or_else(|| fail(invalid("image document exceeds 1 MiB")))?;
    Ok(())
}

fn string_size<E>(
    text: &str,
    bytes: &mut usize,
    visit: &mut impl FnMut(usize, usize) -> Result<(), E>,
    fail: &impl Fn(ModuleError) -> E,
) -> Result<(), E> {
    add_size(2, bytes, fail)?;
    for chunk in text.as_bytes().chunks(4096) {
        visit(0, 0)?;
        for byte in chunk {
            add_size(
                match byte {
                    b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
                    0..=31 => 6,
                    _ => 1,
                },
                bytes,
                fail,
            )?;
        }
    }
    Ok(())
}

fn document_size<E>(
    record: &Record,
    depth: usize,
    bytes: &mut usize,
    visit: &mut impl FnMut(usize, usize) -> Result<(), E>,
    fail: &impl Fn(ModuleError) -> E,
) -> Result<(), E> {
    if depth > 64 {
        return Err(fail(invalid("image document nesting exceeds 64")));
    }
    visit(0, 0)?;
    add_size(2, bytes, fail)?;
    for (index, (key, value)) in record.entries().iter().enumerate() {
        string_size(key, bytes, visit, fail)?;
        add_size(1 + usize::from(index != 0), bytes, fail)?;
        value_size(value, depth + 1, bytes, visit, fail)?;
    }
    Ok(())
}

fn value_size<E>(
    value: &Value,
    depth: usize,
    bytes: &mut usize,
    visit: &mut impl FnMut(usize, usize) -> Result<(), E>,
    fail: &impl Fn(ModuleError) -> E,
) -> Result<(), E> {
    if depth > 64 {
        return Err(fail(invalid("image document nesting exceeds 64")));
    }
    visit(0, 0)?;
    match value {
        Value::Null => add_size(4, bytes, fail),
        Value::Bool(value) => add_size(if *value { 4 } else { 5 }, bytes, fail),
        Value::Int(value) => add_size(value.to_string().len(), bytes, fail),
        Value::Float(value) => {
            let number = serde_json::Number::from_f64(value.get())
                .ok_or_else(|| fail(invalid("image document contains a nonfinite number")))?;
            add_size(number.to_string().len(), bytes, fail)
        }
        Value::String(text) => string_size(text, bytes, visit, fail),
        Value::Record(record) => document_size(record, depth, bytes, visit, fail),
        Value::List(values) => {
            add_size(2, bytes, fail)?;
            for (index, value) in values.iter().enumerate() {
                add_size(usize::from(index != 0), bytes, fail)?;
                value_size(value, depth + 1, bytes, visit, fail)?;
            }
            Ok(())
        }
        _ => Err(fail(invalid("image document requires JSON data values"))),
    }
}

fn required_string<'a>(record: &'a Record, key: &str) -> Result<&'a Arc<str>, ModuleError> {
    match record.get(key) {
        Some(Value::String(text)) => Ok(text),
        _ => Err(invalid(
            "image document has a missing or invalid selected String",
        )),
    }
}

fn is_digest(text: &str) -> bool {
    text.len() == 71
        && text.starts_with("sha256:")
        && text.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn is_segment(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_SEGMENT_BYTES
        && text.as_bytes()[0].is_ascii_alphanumeric()
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        })
}

fn is_repository_digest(text: &str) -> bool {
    if text.len() > MAX_SEGMENT_BYTES {
        return false;
    }
    let Some((repository, digest)) = text.split_once('@') else {
        return false;
    };
    !repository.is_empty()
        && repository.as_bytes()[0].is_ascii_alphanumeric()
        && repository.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
        && is_digest(digest)
}

fn optional_record<'a>(record: &'a Record, key: &str) -> Result<Option<&'a Record>, ModuleError> {
    match record.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Record(value)) => Ok(Some(value)),
        _ => Err(invalid("image document has an invalid selected object")),
    }
}

fn selected_image(document: &Record) -> Result<Value, ModuleError> {
    let id = required_string(document, "Id")?;
    if !is_digest(id) {
        return Err(invalid("image Id requires a full lowercase sha256 digest"));
    }
    let Some(Value::Record(descriptor)) = document.get("Descriptor") else {
        return Err(invalid("image requires a single-manifest Descriptor"));
    };
    let media_type = required_string(descriptor, "mediaType")?;
    let digest = required_string(descriptor, "digest")?;
    if !matches!(
        media_type.as_ref(),
        "application/vnd.oci.image.manifest.v1+json"
            | "application/vnd.docker.distribution.manifest.v2+json"
    ) || !is_digest(digest)
        || digest != id
    {
        return Err(invalid(
            "Descriptor requires a supported single manifest and digest equal to Id",
        ));
    }
    let os = required_string(document, "Os")?;
    let architecture = required_string(document, "Architecture")?;
    let variant = match document.get("Variant") {
        None => Arc::from(""),
        Some(Value::String(text)) => Arc::clone(text),
        _ => return Err(invalid("image Variant requires a String when present")),
    };
    if !is_segment(os)
        || !is_segment(architecture)
        || (!variant.is_empty() && !is_segment(&variant))
        || os.len()
            + architecture.len()
            + 1
            + if variant.is_empty() {
                0
            } else {
                variant.len() + 1
            }
            > MAX_SEGMENT_BYTES
    {
        return Err(invalid(
            "image platform requires bounded lowercase ASCII segments",
        ));
    }
    let revision = optional_record(document, "Config")?
        .map(|config| optional_record(config, "Labels"))
        .transpose()?
        .flatten()
        .and_then(|labels| labels.get("org.opencontainers.image.revision"));
    let revision = match revision {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.len() <= MAX_REVISION_BYTES => Some(text),
        _ => {
            return Err(invalid(
                "image revision requires a String of at most 4096 UTF8 bytes",
            ));
        }
    };
    let repo_digests = match document.get("RepoDigests") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::List(values)) if values.len() <= MAX_DIGESTS => {
            for value in values.iter() {
                if !matches!(value, Value::String(text) if is_repository_digest(text)) {
                    return Err(invalid(
                        "image repository digest requires a bounded repository and full digest",
                    ));
                }
            }
            values.to_vec()
        }
        _ => {
            return Err(invalid(
                "image repository digests require at most 256 Strings",
            ));
        }
    };
    Ok(Value::NominalRecord(Box::new(NominalRecordValue::new(
        NominalTypeId::standard("docker", "Image"),
        Vec::new(),
        vec![
            (Arc::from("id"), Value::String(Arc::clone(id))),
            (Arc::from("os"), Value::String(Arc::clone(os))),
            (
                Arc::from("architecture"),
                Value::String(Arc::clone(architecture)),
            ),
            (Arc::from("variant"), Value::String(variant)),
            (
                Arc::from("revision"),
                Value::Variant(Box::new(VariantValue::new(
                    NominalTypeId::standard("outcome", "Option"),
                    vec![ValueType::String],
                    if revision.is_some() { "Some" } else { "None" },
                    revision
                        .map(|text| vec![Value::String(Arc::clone(text))])
                        .unwrap_or_default(),
                ))),
            ),
            (Arc::from("repo_digests"), Value::list(repo_digests)),
        ],
    ))))
}
