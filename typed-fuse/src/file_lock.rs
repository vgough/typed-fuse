//! POSIX record locks: conversions between [`FileLock`] and `libc::flock`,
//! plus `fcntl` helpers for filesystems that pass locking through to a
//! backing file descriptor.
//!
//! The `F_*LCK` constants are declared inconsistently across platforms
//! (`c_int` on Linux, `c_short` on macOS) even though the `flock` fields that
//! hold them are `c_short` everywhere. They are restated here at the fields'
//! own width, so callers never have to cast.

use std::os::fd::{AsFd, AsRawFd};

use typed_fuse_core::{Errno, FileLock, LockKind};

/// Type of the `l_type` and `l_whence` fields of `libc::flock`.
pub type LockType = libc::c_short;

/// A shared (read) lock, at the width of [`LockType`].
pub const F_RDLCK: LockType = libc::F_RDLCK as LockType;
/// An exclusive (write) lock, at the width of [`LockType`].
pub const F_WRLCK: LockType = libc::F_WRLCK as LockType;
/// Releases a lock, at the width of [`LockType`].
pub const F_UNLCK: LockType = libc::F_UNLCK as LockType;
/// The `l_whence` value for ranges measured from the start of the file, at
/// the width of [`LockType`]. It is the only `l_whence` a FUSE lock request
/// uses.
pub const SEEK_SET: LockType = libc::SEEK_SET as LockType;

/// Converts a [`FileLock`] into the `libc::flock` the kernel expects, with a
/// `SEEK_SET`-relative range.
///
/// Fails with `EINVAL` for an inverted range, and `EOVERFLOW` when an offset,
/// length, or pid does not fit the corresponding `flock` field.
pub fn to_flock(lock: FileLock) -> Result<libc::flock, Errno> {
    let FileLock {
        start,
        end,
        kind,
        pid,
    } = lock;
    if end < start {
        return Err(Errno::EINVAL);
    }
    let l_type = match kind {
        LockKind::Read => F_RDLCK,
        LockKind::Write => F_WRLCK,
        LockKind::Unlock => F_UNLCK,
    };

    let l_start = libc::off_t::try_from(start).map_err(|_| Errno::from_raw(libc::EOVERFLOW))?;
    // A zero length means "to end of file", which is how the inclusive
    // `end == u64::MAX` is spelled in `flock`.
    let l_len = if end == u64::MAX {
        0
    } else {
        let length = end
            .checked_sub(start)
            .and_then(|n| n.checked_add(1))
            .ok_or(Errno::from_raw(libc::EOVERFLOW))?;
        libc::off_t::try_from(length).map_err(|_| Errno::from_raw(libc::EOVERFLOW))?
    };

    Ok(libc::flock {
        l_type,
        l_whence: SEEK_SET,
        l_start,
        l_len,
        l_pid: libc::pid_t::try_from(pid).map_err(|_| Errno::from_raw(libc::EOVERFLOW))?,
    })
}

/// Converts a `libc::flock` into a [`FileLock`].
///
/// Fails with `EINVAL` on anything but a non-negative `SEEK_SET`-relative
/// range with a known lock type. A negative `l_pid`, which is what an open
/// file description lock reports, is mapped to pid 0.
pub fn from_flock(raw: &libc::flock) -> Result<FileLock, Errno> {
    if raw.l_whence != SEEK_SET || raw.l_start < 0 || raw.l_len < 0 {
        return Err(Errno::EINVAL);
    }
    let kind = match raw.l_type {
        F_RDLCK => LockKind::Read,
        F_WRLCK => LockKind::Write,
        F_UNLCK => LockKind::Unlock,
        _ => return Err(Errno::EINVAL),
    };

    let start = raw.l_start as u64;
    let end = if raw.l_len == 0 {
        u64::MAX
    } else {
        start
            .checked_add(raw.l_len as u64 - 1)
            .ok_or(Errno::EINVAL)?
    };

    Ok(FileLock {
        kind,
        start,
        end,
        pid: u32::try_from(raw.l_pid).unwrap_or(0),
    })
}

/// Reports the lock on `fd` that would conflict with `requested`, via
/// `F_GETLK`.
///
/// The reply has [`LockKind::Unlock`] when nothing conflicts. This is the
/// whole implementation a filesystem needs for
/// [`NodeFs::getlk`](typed_fuse_core::NodeFs::getlk) when it passes locking
/// through to a backing file.
pub fn getlk(fd: impl AsFd, requested: FileLock) -> Result<FileLock, Errno> {
    let mut lock = to_flock(requested)?;
    // SAFETY: `fd` is a borrowed, open descriptor and `lock` is a valid
    // `flock` for the duration of the call.
    if unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_GETLK, &mut lock) } == -1 {
        return Err(last_errno());
    }
    from_flock(&lock)
}

/// Acquires or releases `requested` on `fd`, via `F_SETLK` -- or `F_SETLKW`
/// when `block` is set, which waits for a conflicting lock to be released.
///
/// Fails with `EACCES` or `EAGAIN` when a non-blocking request conflicts with
/// a lock held elsewhere. This is the whole implementation a filesystem needs
/// for [`NodeFs::setlk`](typed_fuse_core::NodeFs::setlk) when it passes
/// locking through to a backing file.
pub fn setlk(fd: impl AsFd, requested: FileLock, block: bool) -> Result<(), Errno> {
    let lock = to_flock(requested)?;
    let command = if block { libc::F_SETLKW } else { libc::F_SETLK };
    // SAFETY: as in `getlk`.
    if unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), command, &lock) } == -1 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

fn last_errno() -> Errno {
    Errno::from(std::io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn lock(kind: LockKind, start: u64, end: u64, pid: u32) -> FileLock {
        FileLock {
            kind,
            start,
            end,
            pid,
        }
    }

    #[test]
    fn narrowed_constants_match_libc() {
        // Guards the `as LockType` narrowing above, since libc declares these
        // as c_int on Linux.
        assert_eq!(i64::from(F_RDLCK), i64::from(libc::F_RDLCK));
        assert_eq!(i64::from(F_WRLCK), i64::from(libc::F_WRLCK));
        assert_eq!(i64::from(F_UNLCK), i64::from(libc::F_UNLCK));
        assert_eq!(i64::from(SEEK_SET), i64::from(libc::SEEK_SET));
    }

    #[test]
    fn converts_inclusive_ranges() {
        let one = to_flock(lock(LockKind::Read, 7, 7, 12)).unwrap();
        assert_eq!((one.l_start, one.l_len), (7, 1));
        assert_eq!((one.l_type, one.l_whence), (F_RDLCK, SEEK_SET));

        let finite = to_flock(lock(LockKind::Write, 7, 16, 12)).unwrap();
        assert_eq!((finite.l_start, finite.l_len), (7, 10));
        assert_eq!(finite.l_type, F_WRLCK);

        let eof = to_flock(lock(LockKind::Unlock, 7, u64::MAX, 12)).unwrap();
        assert_eq!((eof.l_start, eof.l_len), (7, 0));
        assert_eq!(eof.l_type, F_UNLCK);
    }

    #[test]
    fn round_trips_ranges_and_eof() {
        for original in [
            lock(LockKind::Read, 7, 19, 42),
            lock(LockKind::Write, 0, 0, 1),
            lock(LockKind::Unlock, 20, u64::MAX, 0),
        ] {
            assert_eq!(from_flock(&to_flock(original).unwrap()).unwrap(), original);
        }
    }

    #[test]
    fn rejects_invalid_ranges_and_types() {
        assert_eq!(
            to_flock(lock(LockKind::Read, 8, 7, 0)).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            to_flock(lock(LockKind::Read, u64::MAX, u64::MAX, 0)).unwrap_err(),
            Errno::from_raw(libc::EOVERFLOW)
        );

        let mut raw = to_flock(lock(LockKind::Read, 0, 0, 0)).unwrap();
        raw.l_type = 999;
        assert_eq!(from_flock(&raw).unwrap_err(), Errno::EINVAL);

        let mut raw = to_flock(lock(LockKind::Read, 0, 0, 0)).unwrap();
        raw.l_whence = libc::SEEK_END as LockType;
        assert_eq!(from_flock(&raw).unwrap_err(), Errno::EINVAL);
    }

    #[test]
    fn locks_and_unlocks_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked");
        let mut file = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(b"payload").unwrap();

        setlk(&file, lock(LockKind::Write, 0, u64::MAX, 0), false).unwrap();
        // The lock is ours, so nothing conflicts with a further request on
        // the same descriptor.
        let held = getlk(&file, lock(LockKind::Write, 0, u64::MAX, 0)).unwrap();
        assert_eq!(held.kind, LockKind::Unlock);

        setlk(&file, lock(LockKind::Unlock, 0, u64::MAX, 0), false).unwrap();
    }
}
