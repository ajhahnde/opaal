//! Linux executable snapshots; source metadata is admission, never copied.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, PermissionsExt};

use opaal_platform::operational::{OperationalError, OperationalErrorKind};
use rustix::fs::{MemfdFlags, SealFlags};

use super::error;

#[allow(unsafe_code)]
pub(super) fn snapshot(
    source: &File,
    max_bytes: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<File, OperationalError> {
    let unavailable = || {
        error(
            OperationalErrorKind::Unsupported,
            "native snapshot is unavailable",
        )
    };
    let poll = || {
        if cancelled() {
            Err(error(
                OperationalErrorKind::Cancelled,
                "native acquisition was cancelled",
            ))
        } else {
            Ok(())
        }
    };
    poll()?;
    let metadata = source.metadata().map_err(|_| unavailable())?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o6000 != 0 {
        return Err(unavailable());
    }
    if metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Err(error(
            OperationalErrorKind::LimitExceeded,
            "native image exceeds its byte limit",
        ));
    }
    // SAFETY: source is live; the empty C string is valid for AT_EMPTY_PATH.
    let admitted = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            source.as_raw_fd(),
            c"".as_ptr(),
            libc::X_OK,
            libc::AT_EMPTY_PATH | libc::AT_EACCESS,
        )
    };
    if admitted != 0 {
        return Err(unavailable());
    }
    // SAFETY: this is a size-only query on a live descriptor; no output buffer.
    let capabilities = unsafe {
        libc::fgetxattr(
            source.as_raw_fd(),
            c"security.capability".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if capabilities > 0
        || (capabilities < 0
            && !matches!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENODATA | libc::EOPNOTSUPP)
            ))
    {
        return Err(unavailable());
    }
    poll()?;
    let mut snapshot = File::from(
        rustix::fs::memfd_create(
            c"opaal-native",
            MemfdFlags::EXEC | MemfdFlags::ALLOW_SEALING | MemfdFlags::CLOEXEC,
        )
        .map_err(|_| unavailable())?,
    );
    rustix::fs::fchmod(&snapshot, rustix::fs::Mode::RWXU).map_err(|_| unavailable())?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0_usize;
    loop {
        poll()?;
        let chunk = buffer
            .len()
            .min(max_bytes.saturating_sub(offset).saturating_add(1));
        let read = source
            .read_at(
                &mut buffer[..chunk],
                u64::try_from(offset).map_err(|_| unavailable())?,
            )
            .map_err(|_| unavailable())?;
        if read == 0 {
            break;
        }
        offset = offset.checked_add(read).ok_or_else(unavailable)?;
        if offset > max_bytes {
            return Err(error(
                OperationalErrorKind::LimitExceeded,
                "native image exceeds its byte limit",
            ));
        }
        snapshot
            .write_all(&buffer[..read])
            .map_err(|_| unavailable())?;
    }
    poll()?;
    let seals = SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL;
    rustix::fs::fcntl_add_seals(&snapshot, seals).map_err(|_| unavailable())?;
    if !rustix::fs::fcntl_get_seals(&snapshot)
        .map_err(|_| unavailable())?
        .contains(seals)
    {
        return Err(unavailable());
    }
    poll()?;
    Ok(snapshot)
}
