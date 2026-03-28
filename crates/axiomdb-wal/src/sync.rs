use std::fs::File;

use axiomdb_core::error::{classify_io, DbError};

#[cfg(unix)]
use std::os::fd::AsRawFd;

/// Configured WAL durability method for steady-state DML commits.
///
/// `Auto` is resolved once when the WAL writer is opened/created:
/// - macOS: `Fsync`
/// - other Unix: `Fdatasync`
/// - non-Unix: `SyncAll`
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalSyncMethod {
    Auto,
    Fsync,
    Fdatasync,
    FullFsync,
    SyncAll,
}

impl WalSyncMethod {
    pub(crate) fn resolve(self) -> Result<ResolvedWalSyncMethod, DbError> {
        match self {
            Self::Auto => Ok(default_sync_method()),
            Self::Fsync => validate_sync_method(ResolvedWalSyncMethod::Fsync),
            Self::Fdatasync => validate_sync_method(ResolvedWalSyncMethod::Fdatasync),
            Self::FullFsync => validate_sync_method(ResolvedWalSyncMethod::FullFsync),
            Self::SyncAll => Ok(ResolvedWalSyncMethod::SyncAll),
        }
    }
}

/// Concrete sync method after platform resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolvedWalSyncMethod {
    Fsync,
    Fdatasync,
    FullFsync,
    SyncAll,
}

pub(crate) fn sync_wal_data(file: &File, method: ResolvedWalSyncMethod) -> Result<(), DbError> {
    match method {
        ResolvedWalSyncMethod::Fsync => sync_with_fsync(file),
        ResolvedWalSyncMethod::Fdatasync => sync_with_fdatasync(file),
        ResolvedWalSyncMethod::FullFsync => sync_with_fullfsync(file),
        ResolvedWalSyncMethod::SyncAll => file
            .sync_all()
            .map_err(|e| classify_io(e, "wal commit sync_all")),
    }
}

#[cfg(target_os = "macos")]
fn default_sync_method() -> ResolvedWalSyncMethod {
    ResolvedWalSyncMethod::Fsync
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn default_sync_method() -> ResolvedWalSyncMethod {
    ResolvedWalSyncMethod::Fdatasync
}

#[cfg(all(
    unix,
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "android"),
    not(target_os = "freebsd"),
    not(target_os = "dragonfly"),
    not(target_os = "netbsd"),
    not(target_os = "openbsd")
))]
fn default_sync_method() -> ResolvedWalSyncMethod {
    ResolvedWalSyncMethod::Fsync
}

#[cfg(not(unix))]
fn default_sync_method() -> ResolvedWalSyncMethod {
    ResolvedWalSyncMethod::SyncAll
}

fn validate_sync_method(method: ResolvedWalSyncMethod) -> Result<ResolvedWalSyncMethod, DbError> {
    #[cfg(unix)]
    {
        match method {
            ResolvedWalSyncMethod::Fsync | ResolvedWalSyncMethod::SyncAll => Ok(method),
            ResolvedWalSyncMethod::Fdatasync => {
                #[cfg(any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                ))]
                {
                    Ok(method)
                }
                #[cfg(not(any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                )))]
                {
                    Err(DbError::InvalidValue {
                        reason: "WAL sync method fdatasync is unsupported on this platform".into(),
                    })
                }
            }
            ResolvedWalSyncMethod::FullFsync => {
                #[cfg(target_os = "macos")]
                {
                    Ok(method)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Err(DbError::InvalidValue {
                        reason: "WAL sync method fullfsync is only supported on macOS".into(),
                    })
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        match method {
            ResolvedWalSyncMethod::SyncAll => Ok(method),
            _ => Err(DbError::InvalidValue {
                reason: "WAL sync method is unsupported on this platform".into(),
            }),
        }
    }
}

#[cfg(unix)]
fn sync_with_fsync(file: &File) -> Result<(), DbError> {
    let rc = unsafe {
        // SAFETY: `as_raw_fd()` returns a live descriptor owned by `file`.
        // `fsync` only uses the descriptor during this call and does not outlive `file`.
        libc::fsync(file.as_raw_fd())
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(classify_io(
            std::io::Error::last_os_error(),
            "wal commit fsync",
        ))
    }
}

#[cfg(not(unix))]
fn sync_with_fsync(file: &File) -> Result<(), DbError> {
    file.sync_all()
        .map_err(|e| classify_io(e, "wal commit sync_all"))
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn sync_with_fdatasync(file: &File) -> Result<(), DbError> {
    let rc = unsafe {
        // SAFETY: `as_raw_fd()` returns a live descriptor owned by `file`.
        // `fdatasync` only uses the descriptor during this call and does not outlive `file`.
        libc::fdatasync(file.as_raw_fd())
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(classify_io(
            std::io::Error::last_os_error(),
            "wal commit fdatasync",
        ))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn sync_with_fdatasync(file: &File) -> Result<(), DbError> {
    let _ = file;
    Err(DbError::InvalidValue {
        reason: "WAL sync method fdatasync is unsupported on this platform".into(),
    })
}

#[cfg(target_os = "macos")]
fn sync_with_fullfsync(file: &File) -> Result<(), DbError> {
    let rc = unsafe {
        // SAFETY: `as_raw_fd()` returns a live descriptor owned by `file`.
        // `fcntl(F_FULLFSYNC)` only uses the descriptor during this call.
        libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(classify_io(
            std::io::Error::last_os_error(),
            "wal commit fullfsync",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn sync_with_fullfsync(_file: &File) -> Result<(), DbError> {
    Err(DbError::InvalidValue {
        reason: "WAL sync method fullfsync is only supported on macOS".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_resolves_to_platform_default() {
        let method = WalSyncMethod::Auto.resolve().unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!(method, ResolvedWalSyncMethod::Fsync);

        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(method, ResolvedWalSyncMethod::Fdatasync);

        #[cfg(not(unix))]
        assert_eq!(method, ResolvedWalSyncMethod::SyncAll);
    }

    #[test]
    fn test_syncall_is_supported_everywhere() {
        let method = WalSyncMethod::SyncAll.resolve().unwrap();
        assert_eq!(method, ResolvedWalSyncMethod::SyncAll);
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    #[test]
    fn test_fdatasync_supported_on_platforms_that_have_it() {
        assert_eq!(
            WalSyncMethod::Fdatasync.resolve().unwrap(),
            ResolvedWalSyncMethod::Fdatasync
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_fsync_supported_on_unix() {
        assert_eq!(
            WalSyncMethod::Fsync.resolve().unwrap(),
            ResolvedWalSyncMethod::Fsync
        );
    }

    #[cfg(all(
        unix,
        not(target_os = "linux"),
        not(target_os = "android"),
        not(target_os = "freebsd"),
        not(target_os = "dragonfly"),
        not(target_os = "netbsd"),
        not(target_os = "openbsd")
    ))]
    #[test]
    fn test_fdatasync_rejected_when_not_supported() {
        let err = WalSyncMethod::Fdatasync.resolve().unwrap_err();
        assert!(matches!(err, DbError::InvalidValue { .. }));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_fullfsync_supported_on_macos() {
        assert_eq!(
            WalSyncMethod::FullFsync.resolve().unwrap(),
            ResolvedWalSyncMethod::FullFsync
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn test_fullfsync_rejected_off_macos() {
        let err = WalSyncMethod::FullFsync.resolve().unwrap_err();
        assert!(matches!(err, DbError::InvalidValue { .. }));
    }
}
