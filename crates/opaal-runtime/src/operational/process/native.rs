//! Bounded structural admission for the supported native image format.

use std::fs::File;
use std::os::unix::fs::FileExt;

use opaal_platform::operational::OperationalAdapter;
use sha2::{Digest, Sha256};

use super::{LockedTool, MAX_TOOL_EXECUTABLE_BYTES, ModuleError, ToolLock};

pub(crate) fn validate_arguments<S: AsRef<str>>(
    lock: &ToolLock,
    tool_id: &str,
    arguments: &[S],
) -> Result<(), ModuleError> {
    let Some(tool) = lock
        .tools()
        .get(tool_id)
        .filter(|tool| tool.contract().adapter().is_none())
    else {
        return Ok(());
    };
    let invalid = || ModuleError::invalid("NATIVE004", "native process request exceeds its limits");
    if arguments.len() >= 4096 {
        return Err(invalid());
    }
    let mut total = 0_usize;
    for argument in std::iter::once(tool.path().bytes()).chain(
        arguments
            .iter()
            .map(|argument| argument.as_ref().as_bytes()),
    ) {
        let length = argument.len().checked_add(1).ok_or_else(invalid)?;
        total = total.checked_add(length).ok_or_else(invalid)?;
        if argument.contains(&0) || length > 32 * 1024 || total > 128 * 1024 {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(super) fn acquire(
    adapter: &dyn OperationalAdapter,
    source: &File,
    tool: &LockedTool,
    poll: &dyn Fn() -> Result<(), ModuleError>,
) -> Result<File, ModuleError> {
    poll()?;
    let result =
        adapter.snapshot_executable(source, MAX_TOOL_EXECUTABLE_BYTES, &|| poll().is_err());
    poll()?;
    let snapshot = result.map_err(|_| {
        ModuleError::invalid("NATIVE002", "native executable snapshot is unavailable")
    })?;
    let (length, digest) = hash_snapshot(&snapshot, poll)?;
    if digest != tool.digest() {
        return Err(ModuleError::invalid(
            "NATIVE003",
            "native snapshot digest changed",
        ));
    }
    validate_image_file(&snapshot, length, poll)?;
    Ok(snapshot)
}

fn hash_snapshot(
    snapshot: &File,
    poll: &dyn Fn() -> Result<(), ModuleError>,
) -> Result<(usize, String), ModuleError> {
    let invalid = || ModuleError::invalid("NATIVE001", "native image exceeds its byte limit");
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_usize;
    let mut hash = Sha256::new();
    loop {
        poll()?;
        let remaining = MAX_TOOL_EXECUTABLE_BYTES + 1 - total;
        let chunk = remaining.min(buffer.len());
        let offset = u64::try_from(total).map_err(|_| invalid())?;
        let read = snapshot
            .read_at(&mut buffer[..chunk], offset)
            .map_err(|_| ModuleError::invalid("NATIVE002", "native snapshot read failed"))?;
        if read == 0 {
            break;
        }
        let next = total.checked_add(read).ok_or_else(invalid)?;
        if next > MAX_TOOL_EXECUTABLE_BYTES {
            return Err(invalid());
        }
        hash.update(&buffer[..read]);
        total = next;
    }
    poll()?;
    Ok((total, format!("sha256:{:x}", hash.finalize())))
}

fn validate_image_file(
    file: &File,
    length: usize,
    poll: &dyn Fn() -> Result<(), ModuleError>,
) -> Result<(), ModuleError> {
    let length = u64::try_from(length)
        .map_err(|_| ModuleError::invalid("NATIVE001", "unsupported native executable image"))?;
    validate_image_reader(
        length,
        |offset, target| {
            file.read_exact_at(target, offset)
                .map_err(|_| ModuleError::invalid("NATIVE002", "native snapshot read failed"))
        },
        poll,
    )
}

/// Inspect bounded native ELF structure without acquiring a snapshot or launching it.
pub fn validate_image(
    bytes: &[u8],
    poll: &dyn Fn() -> Result<(), ModuleError>,
) -> Result<(), ModuleError> {
    let invalid = || ModuleError::invalid("NATIVE001", "unsupported native executable image");
    let length = u64::try_from(bytes.len()).map_err(|_| invalid())?;
    validate_image_reader(
        length,
        |offset, target| {
            let start = usize::try_from(offset).map_err(|_| invalid())?;
            let end = start.checked_add(target.len()).ok_or_else(invalid)?;
            let source = bytes.get(start..end).ok_or_else(invalid)?;
            target.copy_from_slice(source);
            Ok(())
        },
        poll,
    )
}

fn validate_image_reader(
    length: u64,
    mut read_at: impl FnMut(u64, &mut [u8]) -> Result<(), ModuleError>,
    poll: &dyn Fn() -> Result<(), ModuleError>,
) -> Result<(), ModuleError> {
    let invalid = || ModuleError::invalid("NATIVE001", "unsupported native executable image");
    poll()?;
    if length < 64 {
        return Err(invalid());
    }
    let mut header = [0_u8; 64];
    read_at(0, &mut header)?;
    if header[..7] != *b"\x7fELF\x02\x01\x01" {
        return Err(invalid());
    }
    let u16_at = |offset| {
        u16::from_le_bytes(
            header[offset..offset + 2]
                .try_into()
                .expect("header bounds"),
        )
    };
    let u32_at = |offset| {
        u32::from_le_bytes(
            header[offset..offset + 4]
                .try_into()
                .expect("header bounds"),
        )
    };
    let u64_at = |offset| {
        u64::from_le_bytes(
            header[offset..offset + 8]
                .try_into()
                .expect("header bounds"),
        )
    };
    if !matches!(u16_at(16), 2 | 3)
        || u16_at(18) != native_elf_machine()
        || u32_at(20) != 1
        || u16_at(52) != 64
    {
        return Err(invalid());
    }
    let range = |offset: u64, size: u64| -> Result<std::ops::Range<u64>, ModuleError> {
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= length)
            .ok_or_else(invalid)?;
        Ok(offset..end)
    };
    let phnum = u16_at(56);
    if phnum == 0 || phnum == u16::MAX || u16_at(54) != 56 || u64_at(32) < 64 {
        return Err(invalid());
    }
    let programs = range(u64_at(32), u64::from(phnum) * 56)?;
    let shnum = u16_at(60);
    let mut scratch = [0_u8; 64 * 1024];
    if shnum != 0 {
        if u16_at(58) != 64 || u64_at(40) < 64 || u16_at(62) >= shnum {
            return Err(invalid());
        }
        let sections = range(u64_at(40), u64::from(shnum) * 64)?;
        if programs.start < sections.end && sections.start < programs.end {
            return Err(invalid());
        }
        let chunk_capacity = scratch.len() - scratch.len() % 64;
        let mut offset = sections.start;
        while offset < sections.end {
            let remaining = sections.end - offset;
            let chunk =
                usize::try_from(remaining.min(chunk_capacity as u64)).map_err(|_| invalid())?;
            read_at(offset, &mut scratch[..chunk])?;
            for section in scratch[..chunk].chunks_exact(64) {
                poll()?;
                let kind = u32::from_le_bytes(section[4..8].try_into().expect("section size"));
                if kind != 8 {
                    // SHT_NOBITS occupies memory, not file bytes.
                    range(
                        u64::from_le_bytes(section[24..32].try_into().expect("section size")),
                        u64::from_le_bytes(section[32..40].try_into().expect("section size")),
                    )?;
                }
            }
            offset = offset
                .checked_add(u64::try_from(chunk).map_err(|_| invalid())?)
                .ok_or_else(invalid)?;
        }
    } else if u64_at(40) != 0 || u16_at(62) != 0 {
        return Err(invalid());
    }
    let mut loads = 0;
    let mut interpreter = false;
    let chunk_capacity = scratch.len() - scratch.len() % 56;
    let mut header_offset = programs.start;
    while header_offset < programs.end {
        let remaining = programs.end - header_offset;
        let chunk = usize::try_from(remaining.min(chunk_capacity as u64)).map_err(|_| invalid())?;
        read_at(header_offset, &mut scratch[..chunk])?;
        for program in scratch[..chunk].chunks_exact(56) {
            poll()?;
            let kind = u32::from_le_bytes(program[..4].try_into().expect("program size"));
            let offset = u64::from_le_bytes(program[8..16].try_into().expect("program size"));
            let vaddr = u64::from_le_bytes(program[16..24].try_into().expect("program size"));
            let filesz = u64::from_le_bytes(program[32..40].try_into().expect("program size"));
            let memsz = u64::from_le_bytes(program[40..48].try_into().expect("program size"));
            let align = u64::from_le_bytes(program[48..56].try_into().expect("program size"));
            range(offset, filesz)?;
            if align > 1 && !align.is_power_of_two() {
                return Err(invalid());
            }
            if kind == 1 {
                loads += 1;
                if filesz > memsz
                    || vaddr.checked_add(memsz).is_none()
                    || (align > 1 && offset % align != vaddr % align)
                {
                    return Err(invalid());
                }
            } else if kind == 3 {
                if filesz > 4096 {
                    return Err(invalid());
                }
                let path_length = usize::try_from(filesz).map_err(|_| invalid())?;
                let mut path = [0_u8; 4096];
                read_at(offset, &mut path[..path_length])?;
                if interpreter
                    || path_length < 2
                    || path[0] != b'/'
                    || path[path_length - 1] != 0
                    || path[..path_length - 1].contains(&0)
                {
                    return Err(invalid());
                }
                interpreter = true;
            }
        }
        header_offset = header_offset
            .checked_add(u64::try_from(chunk).map_err(|_| invalid())?)
            .ok_or_else(invalid)?;
    }
    if loads == 0 {
        return Err(invalid());
    }
    poll()
}

#[cfg(target_arch = "x86_64")]
const fn native_elf_machine() -> u16 {
    62
}
#[cfg(target_arch = "aarch64")]
const fn native_elf_machine() -> u16 {
    183
}
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const fn native_elf_machine() -> u16 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Vec<u8> {
        let mut image = vec![0; 120];
        image[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        image[16..18].copy_from_slice(&2_u16.to_le_bytes());
        image[18..20].copy_from_slice(&native_elf_machine().to_le_bytes());
        image[20..24].copy_from_slice(&1_u32.to_le_bytes());
        image[32..40].copy_from_slice(&64_u64.to_le_bytes());
        image[52..54].copy_from_slice(&64_u16.to_le_bytes());
        image[54..56].copy_from_slice(&56_u16.to_le_bytes());
        image[56..58].copy_from_slice(&1_u16.to_le_bytes());
        image[64..68].copy_from_slice(&1_u32.to_le_bytes());
        image
    }

    #[test]
    fn image_frozen_format_cases_have_independent_expected_admission() {
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/golden/controlled-external-tools/image-cases.json"
        ));
        let inventory: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        let inventory_digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let host = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            ("macos", "aarch64") => "aarch64-apple-darwin",
            _ => "unsupported",
        };
        let mut checked = 0;
        for case in inventory["cases"].as_array().unwrap() {
            if case["input"]["kind"] != "format" {
                continue;
            }
            let hex = case["input"]["image_hex"].as_str().unwrap();
            assert_eq!(hex.len() % 2, 0);
            let mut bytes = (0..hex.len())
                .step_by(2)
                .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
                .collect::<Vec<_>>();
            if bytes.len() >= 20 {
                let machine: u16 = if case["name"] == "foreign-machine" {
                    if native_elf_machine() == 62 { 183 } else { 62 }
                } else {
                    native_elf_machine()
                };
                bytes[18..20].copy_from_slice(&machine.to_le_bytes());
            }
            let admitted = validate_image(&bytes, &|| Ok(())).is_ok();
            assert_eq!(
                admitted,
                case["expected"]["admitted"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            println!(
                "OPAAL_IMAGE_CASE {}",
                serde_json::json!({
                    "name": case["name"], "observed": { "admitted": admitted },
                    "host": host, "inventory_digest": inventory_digest,
                })
            );
            checked += 1;
        }
        assert_eq!(checked, 42);
    }

    #[test]
    fn image_format_bounds_and_cancellation() {
        let valid = image();
        assert!(validate_image(&valid, &|| Ok(())).is_ok());
        let mut dynamic = valid.clone();
        dynamic[16..18].copy_from_slice(&3_u16.to_le_bytes());
        assert!(validate_image(&dynamic, &|| Ok(())).is_ok());
        for end in 0..valid.len() {
            assert!(
                validate_image(&valid[..end], &|| Ok(())).is_err(),
                "truncation at {end}"
            );
        }
        for offset in [
            0, 4, 5, 6, 16, 18, 20, 32, 40, 52, 54, 56, 60, 62, 64, 72, 96, 112,
        ] {
            let mut bad = valid.clone();
            bad[offset] = 255;
            assert!(
                validate_image(&bad, &|| Ok(())).is_err(),
                "field at {offset}"
            );
        }
        let cancelled = validate_image(&valid, &|| {
            Err(ModuleError::Cancelled(crate::eval::CancelReason::Requested))
        });
        assert!(matches!(cancelled, Err(ModuleError::Cancelled(_))));
    }

    #[test]
    fn image_program_sections_and_interpreter_boundaries() {
        let valid = image();
        for (offset, value) in [
            (32, u64::MAX),
            (72, u64::MAX),
            (96, u64::MAX),
            (80, u64::MAX),
            (104, u64::MAX),
            (112, 3),
        ] {
            let mut bad = valid.clone();
            bad[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            if offset == 80 {
                bad[104..112].copy_from_slice(&1_u64.to_le_bytes());
            } else if offset == 104 {
                bad[80..88].copy_from_slice(&1_u64.to_le_bytes());
            }
            assert!(
                validate_image(&bad, &|| Ok(())).is_err(),
                "field at {offset}"
            );
        }
        let mut sections = valid.clone();
        sections.resize(184, 0);
        sections[40..48].copy_from_slice(&120_u64.to_le_bytes());
        sections[58..60].copy_from_slice(&64_u16.to_le_bytes());
        sections[60..62].copy_from_slice(&1_u16.to_le_bytes());
        assert!(validate_image(&sections, &|| Ok(())).is_ok());
        let mut overlap = sections.clone();
        overlap[40..48].copy_from_slice(&64_u64.to_le_bytes());
        assert!(validate_image(&overlap, &|| Ok(())).is_err());
        sections[144..152].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(validate_image(&sections, &|| Ok(())).is_err());
        sections[124..128].copy_from_slice(&8_u32.to_le_bytes());
        assert!(
            validate_image(&sections, &|| Ok(())).is_ok(),
            "NOBITS has no file contents"
        );

        let mut dynamic = valid;
        dynamic.resize(176, 0);
        dynamic[56..58].copy_from_slice(&2_u16.to_le_bytes());
        dynamic[120..124].copy_from_slice(&3_u32.to_le_bytes());
        dynamic[128..136].copy_from_slice(&176_u64.to_le_bytes());
        for path in [
            b"/lib/loader\0".as_slice(),
            b"relative\0",
            b"/not-terminated",
            b"/interior\0nul\0",
            b"/\0",
        ] {
            let mut candidate = dynamic.clone();
            candidate[152..160].copy_from_slice(&(path.len() as u64).to_le_bytes());
            candidate.extend_from_slice(path);
            assert_eq!(
                validate_image(&candidate, &|| Ok(())).is_ok(),
                path == b"/lib/loader\0" || path == b"/\0"
            );
        }
        let polls = std::cell::Cell::new(0);
        let mut candidate = dynamic;
        candidate[152..160].copy_from_slice(&2_u64.to_le_bytes());
        candidate.extend_from_slice(b"/\0");
        validate_image(&candidate, &|| {
            polls.set(polls.get() + 1);
            Ok(())
        })
        .unwrap();
        for boundary in 0..polls.get() {
            let calls = std::cell::Cell::new(0);
            let result = validate_image(&candidate, &|| {
                let call = calls.get();
                calls.set(call + 1);
                if call >= boundary {
                    Err(ModuleError::Cancelled(crate::eval::CancelReason::Timeout))
                } else {
                    Ok(())
                }
            });
            assert!(
                matches!(
                    result,
                    Err(ModuleError::Cancelled(crate::eval::CancelReason::Timeout))
                ),
                "poll {boundary}"
            );
        }
    }

    #[test]
    fn maximum_native_image_hash_and_elf_validation_use_bounded_reads() {
        struct RemoveFile(std::path::PathBuf);

        impl Drop for RemoveFile {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        let path = std::env::temp_dir().join(format!(
            "opaal-native-image-{}-{}.elf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _remove = RemoveFile(path.clone());
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let length = MAX_TOOL_EXECUTABLE_BYTES;
        let content = image();
        file.set_len(u64::try_from(length).unwrap()).unwrap();
        file.write_all_at(&content, 0).unwrap();

        let mut expected = Sha256::new();
        expected.update(&content);
        let zeros = [0_u8; 64 * 1024];
        let mut remaining = length - content.len();
        while remaining > 0 {
            let chunk = remaining.min(zeros.len());
            expected.update(&zeros[..chunk]);
            remaining -= chunk;
        }
        let (hashed_length, digest) = hash_snapshot(&file, &|| Ok(())).unwrap();
        assert_eq!(hashed_length, length);
        assert_eq!(digest, format!("sha256:{:x}", expected.finalize()));
        validate_image_file(&file, hashed_length, &|| Ok(())).unwrap();

        file.set_len(u64::try_from(length + 1).unwrap()).unwrap();
        assert_eq!(
            hash_snapshot(&file, &|| Ok(())).unwrap_err().code(),
            "NATIVE001"
        );
    }
}
