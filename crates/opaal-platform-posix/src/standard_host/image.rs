//! Loader admission against the running image, before lazy native launch.
use super::*;
#[cfg(any(target_os = "linux", test))]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::PathBuf;

pub(super) struct Image {
    #[cfg(target_os = "linux")]
    pub file: File,
    #[cfg(target_os = "macos")]
    pub path: PathBuf,
}

impl Image {
    pub fn capture() -> Result<Self, OperationalError> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let file = File::open("/proc/self/exe").map_err(io_error)?;
            verify_elf(&file, true)?;
            verify_linux_libraries(&file)?;
            Ok(Self { file })
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            verify_mach(&native::running_mach_header()?)?;
            if native::running_mach_libraries()?
                .iter()
                .any(|name| !system_mach_library(name))
            {
                return Err(unsupported());
            }
            Ok(Self {
                path: std::env::current_exe().map_err(io_error)?,
            })
        }
        #[cfg(not(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64")
        )))]
        {
            Err(unsupported())
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn verify_mach(bytes: &[u8]) -> Result<(), OperationalError> {
    if bytes.get(..4) != Some(&[0xcf, 0xfa, 0xed, 0xfe])
        || number32(bytes, 4)? != 0x100000c
        || number32(bytes, 12)? != 2
    {
        return Err(unsupported());
    }
    let count = number32(bytes, 16)? as usize;
    let size = number32(bytes, 20)? as usize;
    if count > 4096 || size > 65536 || bytes.len() != 32 + size {
        return Err(unsupported());
    }
    let mut offset = 32;
    let mut linker = false;
    for _ in 0..count {
        let command = number32(bytes, offset)?;
        let length = number32(bytes, offset + 4)? as usize;
        if length < 8 || !length.is_multiple_of(4) {
            return Err(unsupported());
        }
        let body = bytes
            .get(offset..offset.checked_add(length).ok_or_else(unsupported)?)
            .ok_or_else(unsupported)?;
        // Relative loader search or loader-injected environment is never safe.
        if matches!(command, 0x8000001c | 0x27 | 0x6 | 0x10) {
            return Err(unsupported());
        }
        if matches!(
            command,
            0xc | 0x80000018 | 0x8000001f | 0x20 | 0x80000023 | 0xe
        ) {
            if body.len() < 12 {
                return Err(unsupported());
            }
            let name_offset = number32(body, 8)? as usize;
            if name_offset < if command == 0xe { 12 } else { 24 } {
                return Err(unsupported());
            }
            let name = cstring(body, name_offset)?;
            if command == 0xe {
                if name != "/usr/lib/dyld" || linker {
                    return Err(unsupported());
                }
                linker = true;
            } else if !system_mach_library(name) {
                return Err(unsupported());
            }
        }
        offset += length;
    }
    if offset != bytes.len() || !linker {
        return Err(unsupported());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn system_mach_library(name: &str) -> bool {
    (name.starts_with("/usr/lib/") || name.starts_with("/System/Library/"))
        && name
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(any(target_os = "linux", test))]
fn verify_elf(file: &File, executable: bool) -> Result<(), OperationalError> {
    use std::os::unix::fs::FileExt;
    fn read(file: &File, offset: u64, count: usize) -> Result<Vec<u8>, OperationalError> {
        if count > 65536 {
            return Err(unsupported());
        }
        let mut bytes = vec![0; count];
        file.read_exact_at(&mut bytes, offset).map_err(io_error)?;
        Ok(bytes)
    }
    let header = read(file, 0, 64)?;
    if header[..7] != [0x7f, b'E', b'L', b'F', 2, 1, 1]
        || !matches!(
            u16::from_le_bytes(header[16..18].try_into().expect("ELF type")),
            2 | 3
        )
        || u16::from_le_bytes(header[18..20].try_into().expect("ELF machine")) != 62
        || number32(&header, 20)? != 1
    {
        return Err(unsupported());
    }
    let table_offset = number64(&header, 32)?;
    let entry_size = u16::from_le_bytes(header[54..56].try_into().expect("ELF phentsize")) as usize;
    let count = u16::from_le_bytes(header[56..58].try_into().expect("ELF phnum")) as usize;
    if entry_size != 56 || count == 0 || count > 1024 {
        return Err(unsupported());
    }
    let table = read(file, table_offset, entry_size * count)?;
    let entries = table.chunks_exact(56).collect::<Vec<_>>();
    let mut dynamic = None;
    let mut interpreter = false;
    for entry in &entries {
        match number32(entry, 0)? {
            2 => {
                if dynamic.is_some() {
                    return Err(unsupported());
                }
                let count = usize::try_from(number64(entry, 32)?).map_err(|_| unsupported())?;
                dynamic = Some(read(file, number64(entry, 8)?, count)?);
            }
            3 => {
                if interpreter {
                    return Err(unsupported());
                }
                let count = usize::try_from(number64(entry, 32)?).map_err(|_| unsupported())?;
                let name = read(file, number64(entry, 8)?, count)?;
                if !matches!(
                    cstring(&name, 0)?,
                    "/lib64/ld-linux-x86-64.so.2" | "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"
                ) {
                    return Err(unsupported());
                }
                interpreter = true;
            }
            _ => {}
        }
    }
    let dynamic = dynamic.ok_or_else(unsupported)?;
    if (executable && !interpreter) || !dynamic.len().is_multiple_of(16) {
        return Err(unsupported());
    }
    let mut address = None;
    let mut size = None;
    let mut needed = Vec::new();
    let mut soname = None;
    let mut terminated = false;
    for entry in dynamic.chunks_exact(16) {
        let tag = number64(entry, 0)?;
        let value = number64(entry, 8)?;
        match tag {
            0 => {
                terminated = true;
                break;
            }
            1 => needed.push(value),
            5 => {
                if address.replace(value).is_some() {
                    return Err(unsupported());
                }
            }
            10 => {
                if size.replace(value).is_some() {
                    return Err(unsupported());
                }
            }
            14 => {
                if soname.replace(value).is_some() {
                    return Err(unsupported());
                }
            }
            15 | 29 | 0x7fffffff | 0x7ffffffe | 0x6ffffefc | 0x6ffffefb => {
                return Err(unsupported());
            }
            _ => {}
        }
    }
    if !terminated {
        return Err(unsupported());
    }
    let address = address.ok_or_else(unsupported)?;
    let size = size.ok_or_else(unsupported)?;
    let mut offset = None;
    for entry in &entries {
        let base = number64(entry, 16)?;
        if number32(entry, 0)? == 1 && address >= base {
            let delta = address - base;
            if delta
                .checked_add(size)
                .is_some_and(|end| end <= number64(entry, 32).unwrap_or(0))
            {
                offset = number64(entry, 8)?.checked_add(delta);
                break;
            }
        }
    }
    let strings = read(
        file,
        offset.ok_or_else(unsupported)?,
        usize::try_from(size).map_err(|_| unsupported())?,
    )?;
    if !executable
        && !system_elf_library(cstring(
            &strings,
            usize::try_from(soname.ok_or_else(unsupported)?).map_err(|_| unsupported())?,
        )?)
    {
        return Err(unsupported());
    }
    for offset in needed {
        if !system_elf_library(cstring(
            &strings,
            usize::try_from(offset).map_err(|_| unsupported())?,
        )?) {
            return Err(unsupported());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_linux_libraries(image: &File) -> Result<(), OperationalError> {
    use std::os::unix::fs::MetadataExt;
    let mut maps = String::new();
    File::open("/proc/self/maps")
        .map_err(io_error)?
        .take(65537)
        .read_to_string(&mut maps)
        .map_err(io_error)?;
    if maps.len() > 65536 {
        return Err(unsupported());
    }
    let identity = image.metadata().map_err(io_error)?;
    let paths = mapped_libraries(
        &maps,
        (libc::major(identity.dev()), libc::minor(identity.dev())),
        identity.ino(),
    )?;
    for path in paths {
        let path = path.canonicalize().map_err(io_error)?;
        if !["/usr/lib", "/usr/lib64", "/lib", "/lib64"]
            .iter()
            .any(|root| path.starts_with(root))
        {
            return Err(unsupported());
        }
        require_root_owned(&path)?;
        // Validate the actual transitive images, not just their filenames.
        verify_elf(&File::open(&path).map_err(io_error)?, false)?;
    }
    verify_linux_preload()?;
    verify_linux_cache()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_linux_cache() -> Result<(), OperationalError> {
    use std::os::unix::fs::{FileExt, MetadataExt};
    let file = File::open("/etc/ld.so.cache").map_err(io_error)?;
    require_root_owned(
        &PathBuf::from("/etc/ld.so.cache")
            .canonicalize()
            .map_err(io_error)?,
    )?;
    let metadata = file.metadata().map_err(io_error)?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(unsupported());
    }
    let mut header = [0; 48];
    file.read_exact_at(&mut header, 0).map_err(io_error)?;
    if &header[..20] != b"glibc-ld.so.cache1.1" {
        return Err(unsupported());
    }
    let count = number32(&header, 20)?;
    if count > 65536 {
        return Err(unsupported());
    }
    fn read_name(file: &File, offset: u32) -> Result<String, OperationalError> {
        let mut bytes = Vec::new();
        for index in 0..4096 {
            let mut byte = [0];
            file.read_exact_at(&mut byte, u64::from(offset) + index)
                .map_err(io_error)?;
            if byte[0] == 0 {
                return String::from_utf8(bytes).map_err(|_| unsupported());
            }
            bytes.push(byte[0]);
        }
        Err(unsupported())
    }
    for index in 0..count {
        let mut entry = [0; 24];
        file.read_exact_at(&mut entry, 48 + u64::from(index) * 24)
            .map_err(io_error)?;
        // glibc's ELF libc6 + x86_64 cache identity; other ABIs cannot be
        // selected by this worker and may legitimately coexist on the host.
        if number32(&entry, 0)? != 0x0303 {
            continue;
        }
        let name = read_name(&file, number32(&entry, 4)?)?;
        if !system_elf_library(&name) {
            continue;
        }
        let path = PathBuf::from(read_name(&file, number32(&entry, 8)?)?)
            .canonicalize()
            .map_err(io_error)?;
        if !["/usr/lib", "/usr/lib64", "/lib", "/lib64"]
            .iter()
            .any(|root| path.starts_with(root))
        {
            return Err(unsupported());
        }
        require_root_owned(&path)?;
        verify_elf(&File::open(&path).map_err(io_error)?, false)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_root_owned(path: &std::path::Path) -> Result<(), OperationalError> {
    use std::os::unix::fs::MetadataExt;
    for ancestor in path.ancestors() {
        let metadata = ancestor.metadata().map_err(io_error)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(unsupported());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_linux_preload() -> Result<(), OperationalError> {
    // Empty environment does not disable the loader's system preload file.
    // Refuse configured preloads; a trusted directory protects its absence.
    require_root_owned(std::path::Path::new("/etc"))?;
    match std::fs::symlink_metadata("/etc/ld.so.preload") {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
        Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {
            require_root_owned(std::path::Path::new("/etc/ld.so.preload"))
        }
        Ok(_) => Err(unsupported()),
    }
}

#[cfg(any(target_os = "linux", test))]
fn system_elf_library(name: &str) -> bool {
    matches!(
        name,
        "libc.so.6"
            | "libm.so.6"
            | "libgcc_s.so.1"
            | "libpthread.so.0"
            | "libdl.so.2"
            | "librt.so.1"
            | "ld-linux-x86-64.so.2"
    )
}

#[cfg(any(target_os = "linux", test))]
fn mapped_libraries(
    maps: &str,
    image_device: (u32, u32),
    image_inode: u64,
) -> Result<std::collections::BTreeSet<PathBuf>, OperationalError> {
    let mut paths = std::collections::BTreeSet::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let _range = fields.next().ok_or_else(unsupported)?;
        let permissions = fields.next().ok_or_else(unsupported)?;
        let _offset = fields.next().ok_or_else(unsupported)?;
        let (major, minor) = fields
            .next()
            .and_then(|dev| dev.split_once(':'))
            .ok_or_else(unsupported)?;
        let device = (
            u32::from_str_radix(major, 16).map_err(|_| unsupported())?,
            u32::from_str_radix(minor, 16).map_err(|_| unsupported())?,
        );
        let inode: u64 = fields
            .next()
            .ok_or_else(unsupported)?
            .parse()
            .map_err(|_| unsupported())?;
        if !permissions.contains('x') || (device == image_device && inode == image_inode) {
            continue;
        }
        let path = fields.next().ok_or_else(unsupported)?;
        if matches!(path, "[vdso]" | "[vsyscall]") {
            continue;
        }
        if !path.starts_with('/') || fields.next().is_some() {
            return Err(unsupported());
        }
        paths.insert(PathBuf::from(path));
    }
    if paths.is_empty() {
        return Err(unsupported());
    }
    Ok(paths)
}

fn number32(bytes: &[u8], offset: usize) -> Result<u32, OperationalError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset.checked_add(4).ok_or_else(unsupported)?)
            .ok_or_else(unsupported)?
            .try_into()
            .expect("four-byte field"),
    ))
}
#[cfg(any(target_os = "linux", test))]
fn number64(bytes: &[u8], offset: usize) -> Result<u64, OperationalError> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset.checked_add(8).ok_or_else(unsupported)?)
            .ok_or_else(unsupported)?
            .try_into()
            .expect("eight-byte field"),
    ))
}
fn cstring(bytes: &[u8], offset: usize) -> Result<&str, OperationalError> {
    let tail = bytes.get(offset..).ok_or_else(unsupported)?;
    let end = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(unsupported)?;
    std::str::from_utf8(&tail[..end]).map_err(|_| unsupported())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loader_paths_refuse_relative_override_and_traversal() {
        for path in [
            "@rpath/libfoo.dylib",
            "libc.dylib",
            "/usr/lib/../local/evil.dylib",
            "/System/Library//evil.dylib",
            "/tmp/evil.dylib",
        ] {
            assert!(!system_mach_library(path));
        }
        assert!(system_mach_library("/usr/lib/libSystem.B.dylib"));
        assert!(verify_mach(&[]).is_err());
        assert!(cstring(b"unterminated", 0).is_err());
        assert!(cstring(b"valid\0", usize::MAX).is_err());
        assert!(number32(&[], usize::MAX).is_err());
        assert!(number64(&[], usize::MAX).is_err());
    }

    #[test]
    fn loaded_images_do_not_depend_on_filename_suffix_and_keep_deleted_main_identity() {
        let maps = "100-200 r-xp 0 00:01 17 /tmp/opaal (deleted)\n\
                    200-300 r-xp 0 00:01 18 /tmp/injected\n\
                    300-400 r-xp 0 00:02 17 /usr/lib/libc.so.6\n\
                    400-500 r--p 0 00:01 19 /tmp/data\n\
                    500-600 r-xp 0 00:00 0 [vdso]\n";
        assert_eq!(
            mapped_libraries(maps, (0, 1), 17).unwrap(),
            [
                PathBuf::from("/tmp/injected"),
                PathBuf::from("/usr/lib/libc.so.6")
            ]
            .into()
        );
        assert!(
            mapped_libraries("100-200 r-xp 0 00:01 18 /tmp/deleted (deleted)", (0, 1), 17).is_err()
        );
        assert!(mapped_libraries("100-200 r-xp 0 00:00 0", (0, 1), 17).is_err());
        assert!(!system_elf_library("libc.extra.so"));
        assert!(!system_elf_library("/tmp/libc.so.6"));
        assert!(system_elf_library("libc.so.6"));
    }

    #[test]
    fn elf_library_closure_refuses_non_system_soname_dependency_and_search_overrides() {
        use std::io::Write;
        use std::os::unix::fs::FileExt;
        let path = std::env::temp_dir().join(format!("opaal-loader-review-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        // One PT_LOAD and PT_DYNAMIC, a closed dynamic string table and SONAME.
        let mut bytes = vec![0; 512];
        bytes[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
        bytes[16..18].copy_from_slice(&3_u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62_u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64_u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&56_u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&2_u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&1_u32.to_le_bytes());
        bytes[96..104].copy_from_slice(&512_u64.to_le_bytes());
        bytes[120..124].copy_from_slice(&2_u32.to_le_bytes());
        bytes[128..136].copy_from_slice(&176_u64.to_le_bytes());
        bytes[152..160].copy_from_slice(&80_u64.to_le_bytes());
        for (index, (tag, value)) in [(5_u64, 256_u64), (10, 32), (14, 0), (1, 0)]
            .into_iter()
            .enumerate()
        {
            bytes[176 + index * 16..184 + index * 16].copy_from_slice(&tag.to_le_bytes());
            bytes[184 + index * 16..192 + index * 16].copy_from_slice(&value.to_le_bytes());
        }
        bytes[256..266].copy_from_slice(b"libc.so.6\0");
        file.write_all(&bytes).unwrap();
        assert!(verify_elf(&file, false).is_ok());
        assert!(verify_elf(&file, true).is_err());
        file.write_all_at(b"evil.so.6\0", 256).unwrap();
        assert!(verify_elf(&file, false).is_err());
        file.write_all_at(b"libc.so.6\0", 256).unwrap();
        for tag in [15_u64, 29, 0x6ffffefc, 0x6ffffefb] {
            file.write_all_at(&tag.to_le_bytes(), 224).unwrap();
            assert!(verify_elf(&file, false).is_err());
        }
    }
}
