//! Conversions between `typed_fuse_core`'s backend-neutral types and the
//! raw C types from `libfuse_sys::fuse_lowlevel`.
//!
//! This is where all per-OS layout differences (`mode_t` width, `stat`
//! timestamp field names, Darwin's extra `st_flags`/`st_birthtimespec`,
//! ...) are handled; the typed core crate and the `Filesystem` trait never
//! see raw C types.
#![allow(clippy::unnecessary_cast)]

use std::os::raw::c_int;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libfuse_sys::fuse_lowlevel::{
    fuse_entry_param, stat, FUSE_SET_ATTR_ATIME, FUSE_SET_ATTR_ATIME_NOW, FUSE_SET_ATTR_GID,
    FUSE_SET_ATTR_MODE, FUSE_SET_ATTR_MTIME, FUSE_SET_ATTR_MTIME_NOW, FUSE_SET_ATTR_SIZE,
    FUSE_SET_ATTR_UID,
};
#[cfg(target_os = "macos")]
use libfuse_sys::fuse_lowlevel::{statfs, timespec, FUSE_SET_ATTR_BTIME, FUSE_SET_ATTR_CTIME};
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::{statvfs, FUSE_SET_ATTR_CTIME};

use typed_fuse_core::{FileKind, NodeAttr, SetAttr, StatFs, TimeOrNow};

/// Returns the `S_IFMT` bits corresponding to `kind`.
pub(crate) fn kind_to_mode_bits(kind: FileKind) -> u32 {
    (match kind {
        FileKind::RegularFile => libc::S_IFREG,
        FileKind::Directory => libc::S_IFDIR,
        FileKind::Symlink => libc::S_IFLNK,
        FileKind::CharDevice => libc::S_IFCHR,
        FileKind::BlockDevice => libc::S_IFBLK,
        FileKind::NamedPipe => libc::S_IFIFO,
        FileKind::Socket => libc::S_IFSOCK,
    }) as u32
}

/// Decodes a file kind from the `S_IFMT` bits of a mode value. Currently
/// only exercised by this module's round-trip tests; kept alongside
/// [`kind_to_mode_bits`] as the natural inverse.
#[allow(dead_code)]
pub(crate) fn mode_bits_to_kind(mode: u32) -> Option<FileKind> {
    match mode & (libc::S_IFMT as u32) {
        m if m == libc::S_IFREG as u32 => Some(FileKind::RegularFile),
        m if m == libc::S_IFDIR as u32 => Some(FileKind::Directory),
        m if m == libc::S_IFLNK as u32 => Some(FileKind::Symlink),
        m if m == libc::S_IFCHR as u32 => Some(FileKind::CharDevice),
        m if m == libc::S_IFBLK as u32 => Some(FileKind::BlockDevice),
        m if m == libc::S_IFIFO as u32 => Some(FileKind::NamedPipe),
        m if m == libc::S_IFSOCK as u32 => Some(FileKind::Socket),
        _ => None,
    }
}

/// Splits a `SystemTime` into `(seconds, nanoseconds)` relative to the Unix
/// epoch, gracefully handling times before the epoch (negative seconds).
pub(crate) fn system_time_to_secs_nsecs(t: SystemTime) -> (i64, i64) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i64),
        Err(e) => {
            let d = e.duration();
            let secs = d.as_secs() as i64;
            let nsecs = d.subsec_nanos() as i64;
            if nsecs == 0 {
                (-secs, 0)
            } else {
                (-secs - 1, 1_000_000_000 - nsecs)
            }
        }
    }
}

/// Inverse of [`system_time_to_secs_nsecs`].
pub(crate) fn secs_nsecs_to_system_time(secs: i64, nsecs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32)
    } else {
        UNIX_EPOCH - Duration::new((-secs) as u64, 0) + Duration::new(0, nsecs as u32)
    }
}

/// Builds a zeroed raw `stat` struct for `ino`/`attr`, handling all per-OS
/// layout differences.
pub(crate) fn attr_to_stat(ino: u64, attr: &NodeAttr) -> stat {
    let mut st: stat = unsafe { std::mem::zeroed() };

    st.st_ino = ino as _;
    st.st_mode = (kind_to_mode_bits(attr.kind) | (attr.perm as u32 & 0o7777)) as _;
    st.st_nlink = attr.nlink as _;
    st.st_uid = attr.uid as _;
    st.st_gid = attr.gid as _;
    st.st_rdev = attr.rdev as _;
    st.st_size = attr.size as _;
    st.st_blocks = attr.blocks as _;
    st.st_blksize = attr.blksize as _;

    let (atime_sec, atime_nsec) = system_time_to_secs_nsecs(attr.atime);
    let (mtime_sec, mtime_nsec) = system_time_to_secs_nsecs(attr.mtime);
    let (ctime_sec, ctime_nsec) = system_time_to_secs_nsecs(attr.ctime);

    #[cfg(target_os = "macos")]
    {
        let (crtime_sec, crtime_nsec) = system_time_to_secs_nsecs(attr.crtime);
        st.st_atimespec = timespec {
            tv_sec: atime_sec as _,
            tv_nsec: atime_nsec as _,
        };
        st.st_mtimespec = timespec {
            tv_sec: mtime_sec as _,
            tv_nsec: mtime_nsec as _,
        };
        st.st_ctimespec = timespec {
            tv_sec: ctime_sec as _,
            tv_nsec: ctime_nsec as _,
        };
        st.st_birthtimespec = timespec {
            tv_sec: crtime_sec as _,
            tv_nsec: crtime_nsec as _,
        };
        st.st_flags = attr.flags;
    }
    #[cfg(not(target_os = "macos"))]
    {
        st.st_atim.tv_sec = atime_sec as _;
        st.st_atim.tv_nsec = atime_nsec as _;
        st.st_mtim.tv_sec = mtime_sec as _;
        st.st_mtim.tv_nsec = mtime_nsec as _;
        st.st_ctim.tv_sec = ctime_sec as _;
        st.st_ctim.tv_nsec = ctime_nsec as _;
        // crtime/flags have no vanilla `stat` field on Linux.
    }

    st
}

/// Builds the `fuse_entry_param` reply for `ino`/`generation`/`attr`/`ttl`.
pub(crate) fn entry_to_entry_param(
    ino: u64,
    generation: u64,
    attr: &NodeAttr,
    ttl: Duration,
) -> fuse_entry_param {
    fuse_entry_param {
        ino,
        generation,
        attr: attr_to_stat(ino, attr),
        attr_timeout: ttl.as_secs_f64(),
        entry_timeout: ttl.as_secs_f64(),
    }
}

/// Builds a negative-cache `fuse_entry_param` (`ino: 0`) for a failed
/// `lookup` that carries a negative TTL.
pub(crate) fn negative_entry_param(ttl: Duration) -> fuse_entry_param {
    fuse_entry_param {
        ino: 0,
        generation: 0,
        attr: unsafe { std::mem::zeroed() },
        attr_timeout: 0.0,
        entry_timeout: ttl.as_secs_f64(),
    }
}

/// Decodes a `setattr` request from the raw `stat`/`to_set` bitmask pair.
pub(crate) fn setattr_from_raw(attr: *const stat, to_set: c_int) -> SetAttr {
    let to_set = to_set as u32;
    let mut out = SetAttr::default();

    if attr.is_null() {
        return out;
    }
    let st = unsafe { &*attr };

    if to_set & FUSE_SET_ATTR_MODE != 0 {
        out.mode = Some(st.st_mode as u32);
    }
    if to_set & FUSE_SET_ATTR_UID != 0 {
        out.uid = Some(st.st_uid as u32);
    }
    if to_set & FUSE_SET_ATTR_GID != 0 {
        out.gid = Some(st.st_gid as u32);
    }
    if to_set & FUSE_SET_ATTR_SIZE != 0 {
        out.size = Some(st.st_size as u64);
    }

    #[cfg(target_os = "macos")]
    {
        if to_set & FUSE_SET_ATTR_ATIME_NOW != 0 {
            out.atime = Some(TimeOrNow::Now);
        } else if to_set & FUSE_SET_ATTR_ATIME != 0 {
            out.atime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                st.st_atimespec.tv_sec as i64,
                st.st_atimespec.tv_nsec as i64,
            )));
        }
        if to_set & FUSE_SET_ATTR_MTIME_NOW != 0 {
            out.mtime = Some(TimeOrNow::Now);
        } else if to_set & FUSE_SET_ATTR_MTIME != 0 {
            out.mtime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                st.st_mtimespec.tv_sec as i64,
                st.st_mtimespec.tv_nsec as i64,
            )));
        }
        if to_set & FUSE_SET_ATTR_CTIME != 0 {
            out.ctime = Some(secs_nsecs_to_system_time(
                st.st_ctimespec.tv_sec as i64,
                st.st_ctimespec.tv_nsec as i64,
            ));
        }
        if to_set & FUSE_SET_ATTR_BTIME != 0 {
            out.crtime = Some(secs_nsecs_to_system_time(
                st.st_birthtimespec.tv_sec as i64,
                st.st_birthtimespec.tv_nsec as i64,
            ));
        }
        if to_set & libfuse_sys::fuse_lowlevel::FUSE_SET_ATTR_FLAGS != 0 {
            out.flags = Some(st.st_flags);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        if to_set & FUSE_SET_ATTR_ATIME_NOW != 0 {
            out.atime = Some(TimeOrNow::Now);
        } else if to_set & FUSE_SET_ATTR_ATIME != 0 {
            out.atime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                st.st_atim.tv_sec as i64,
                st.st_atim.tv_nsec as i64,
            )));
        }
        if to_set & FUSE_SET_ATTR_MTIME_NOW != 0 {
            out.mtime = Some(TimeOrNow::Now);
        } else if to_set & FUSE_SET_ATTR_MTIME != 0 {
            out.mtime = Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                st.st_mtim.tv_sec as i64,
                st.st_mtim.tv_nsec as i64,
            )));
        }
        if to_set & FUSE_SET_ATTR_CTIME != 0 {
            out.ctime = Some(secs_nsecs_to_system_time(
                st.st_ctim.tv_sec as i64,
                st.st_ctim.tv_nsec as i64,
            ));
        }
        // crtime/flags have no vanilla `stat` field on Linux.
    }

    out
}

/// Converts to the raw type expected by `fuse_reply_statfs` on this
/// platform (`statfs` on macOS, `statvfs` on Linux).
#[cfg(target_os = "macos")]
pub(crate) fn statfs_to_raw(s: &StatFs) -> statfs {
    let mut out: statfs = unsafe { std::mem::zeroed() };
    out.f_bsize = s.bsize;
    out.f_blocks = s.blocks;
    out.f_bfree = s.bfree;
    out.f_bavail = s.bavail;
    out.f_files = s.files;
    out.f_ffree = s.ffree;
    // macOS's `statfs` has no filename-length-limit field; `f_iosize`
    // (preferred I/O size) is the closest analog to `frsize`.
    out.f_iosize = s.frsize as i32;
    out
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn statfs_to_raw(s: &StatFs) -> statvfs {
    let mut out: statvfs = unsafe { std::mem::zeroed() };
    out.f_bsize = s.bsize as _;
    out.f_frsize = s.frsize as _;
    out.f_blocks = s.blocks as _;
    out.f_bfree = s.bfree as _;
    out.f_bavail = s.bavail as _;
    out.f_files = s.files as _;
    out.f_ffree = s.ffree as _;
    out.f_namemax = s.namelen as _;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_mode_round_trip() {
        for kind in [
            FileKind::RegularFile,
            FileKind::Directory,
            FileKind::Symlink,
            FileKind::CharDevice,
            FileKind::BlockDevice,
            FileKind::NamedPipe,
            FileKind::Socket,
        ] {
            let bits = kind_to_mode_bits(kind);
            assert_eq!(mode_bits_to_kind(bits), Some(kind));
            assert_eq!(mode_bits_to_kind(bits | 0o644), Some(kind));
        }
    }

    #[test]
    fn mode_bits_to_kind_unknown() {
        assert_eq!(mode_bits_to_kind(0), None);
    }

    #[test]
    fn attr_to_stat_basic_fields() {
        let attr = NodeAttr {
            size: 1234,
            kind: FileKind::RegularFile,
            perm: 0o644,
            nlink: 3,
            ..Default::default()
        };
        let st = attr_to_stat(42, &attr);
        assert_eq!(st.st_ino as u64, 42);
        assert_eq!(st.st_size as u64, 1234);
        assert_eq!(st.st_nlink as u64, 3);
        assert_eq!(
            mode_bits_to_kind(st.st_mode as u32),
            Some(FileKind::RegularFile)
        );
        assert_eq!((st.st_mode as u32) & 0o777, 0o644);
    }

    #[test]
    fn secs_nsecs_round_trip_pre_and_post_epoch() {
        for t in [
            UNIX_EPOCH,
            UNIX_EPOCH + Duration::new(100, 250),
            UNIX_EPOCH - Duration::new(1, 0),
            UNIX_EPOCH - Duration::new(1, 500_000_000),
            UNIX_EPOCH - Duration::new(100, 999_999_999),
        ] {
            let (secs, nsecs) = system_time_to_secs_nsecs(t);
            let back = secs_nsecs_to_system_time(secs, nsecs);
            assert_eq!(back, t, "round trip failed for {:?}", t);
        }
    }

    #[test]
    fn entry_to_entry_param_timeouts() {
        let attr = NodeAttr::default();
        let param = entry_to_entry_param(7, 1, &attr, Duration::from_millis(1500));
        assert_eq!(param.ino, 7);
        assert_eq!(param.generation, 1);
        assert!((param.attr_timeout - 1.5).abs() < 1e-9);
        assert!((param.entry_timeout - 1.5).abs() < 1e-9);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn set_attr_from_raw_decodes_bitmask() {
        let mut st: stat = unsafe { std::mem::zeroed() };
        st.st_mode = 0o644;
        st.st_size = 4096;
        st.st_mtimespec = timespec {
            tv_sec: 12345,
            tv_nsec: 6789,
        };

        let to_set = (FUSE_SET_ATTR_MODE
            | FUSE_SET_ATTR_SIZE
            | FUSE_SET_ATTR_ATIME_NOW
            | FUSE_SET_ATTR_MTIME) as c_int;

        let attrs = setattr_from_raw(&st, to_set);
        assert_eq!(attrs.mode, Some(0o644));
        assert_eq!(attrs.size, Some(4096));
        assert_eq!(attrs.atime, Some(TimeOrNow::Now));
        assert_eq!(
            attrs.mtime,
            Some(TimeOrNow::SpecificTime(secs_nsecs_to_system_time(
                12345, 6789
            )))
        );
        assert_eq!(attrs.uid, None);
        assert_eq!(attrs.gid, None);
        assert_eq!(attrs.ctime, None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn statfs_conversion_spot_check() {
        let sfs = StatFs {
            blocks: 1000,
            bfree: 500,
            bavail: 400,
            files: 100,
            ffree: 50,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        };
        let raw = statfs_to_raw(&sfs);
        assert_eq!(raw.f_blocks, 1000);
        assert_eq!(raw.f_bfree, 500);
        assert_eq!(raw.f_bavail, 400);
        assert_eq!(raw.f_files, 100);
        assert_eq!(raw.f_ffree, 50);
        assert_eq!(raw.f_bsize, 4096);
    }
}
