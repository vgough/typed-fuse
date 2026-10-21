//! Helpers for passthrough-style filesystems that serve a view of a backing
//! directory tree: attribute conversions from `std::fs::Metadata`, no-follow
//! extended-attribute syscalls, and POSIX permission checks.
//!
//! Everything here is platform portable across Linux, macOS and FreeBSD;
//! per-OS differences in the xattr syscalls are hidden behind the
//! [`setxattr_nofollow`], [`getxattr_nofollow`], [`listxattr_nofollow`], and
//! [`removexattr_nofollow`] wrappers, which never follow symlinks. Linux uses
//! the `l*xattr` syscalls, macOS the `*xattr` ones with `XATTR_NOFOLLOW`, and
//! FreeBSD the `extattr_*_link` family; see [`freebsd_xattr`] for the last.

use std::ffi::{CStr, CString, OsStr};
use std::fs::Metadata;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::{Duration, SystemTime};

use typed_fuse_core::{Caller, Errno, FileKind, NodeAttr, StatFs};

// ---------------------------------------------------------------------------
// Attribute conversions
// ---------------------------------------------------------------------------

/// Map `std::fs::Metadata` file type to a FUSE [`FileKind`].
pub fn file_type_from_metadata(metadata: &Metadata) -> FileKind {
    file_kind_from_file_type(metadata.file_type())
}

/// Map a [`std::fs::FileType`], such as one from
/// [`std::fs::DirEntry::file_type`], to a FUSE [`FileKind`].
pub fn file_kind_from_file_type(ft: std::fs::FileType) -> FileKind {
    if ft.is_dir() {
        FileKind::Directory
    } else if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_block_device() {
        FileKind::BlockDevice
    } else if ft.is_char_device() {
        FileKind::CharDevice
    } else if ft.is_fifo() {
        FileKind::NamedPipe
    } else if ft.is_socket() {
        FileKind::Socket
    } else {
        FileKind::RegularFile
    }
}

/// Convert metadata timestamps (seconds + nanoseconds since epoch) to a
/// [`SystemTime`].
pub fn system_time_from_secs(secs: i64, nanos: i64) -> SystemTime {
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nanos as u32)
    } else {
        // POSIX timestamps use floor-based seconds, so (-1, 500_000_000)
        // denotes half a second before the epoch, not one-and-a-half seconds.
        let duration = if nanos == 0 {
            Duration::new(secs.unsigned_abs(), 0)
        } else {
            Duration::new(secs.unsigned_abs() - 1, 1_000_000_000 - nanos as u32)
        };
        SystemTime::UNIX_EPOCH
            .checked_sub(duration)
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

/// Build a FUSE [`NodeAttr`] from file metadata, with an explicit size
/// override (passthrough filesystems that transform contents report a
/// different size than the backing file's length).
pub fn file_attr_from_metadata(metadata: &Metadata, size: u64) -> NodeAttr {
    NodeAttr {
        size,
        blocks: metadata.blocks(),
        atime: system_time_from_secs(metadata.atime(), metadata.atime_nsec()),
        mtime: system_time_from_secs(metadata.mtime(), metadata.mtime_nsec()),
        ctime: system_time_from_secs(metadata.ctime(), metadata.ctime_nsec()),
        crtime: metadata.created().unwrap_or(SystemTime::UNIX_EPOCH),
        kind: file_type_from_metadata(metadata),
        // Mask to permission bits only; the file type is carried in `kind`
        // and the base layer ORs them together when building the reply.
        perm: (metadata.mode() & 0o7777) as u16,
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        flags: 0,
        blksize: metadata.blksize() as u32,
    }
}

/// Build a [`NodeAttr`] for a virtual (in-memory) regular file, such as a
/// synthesized config file presented by a passthrough filesystem.
pub fn synthetic_file_attr(
    size: u64,
    mtime: SystemTime,
    perm: u16,
    uid: u32,
    gid: u32,
) -> NodeAttr {
    NodeAttr {
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: FileKind::RegularFile,
        perm,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

/// Query filesystem-wide statistics for the filesystem backing `path`
/// (`statvfs(2)`). The field-width casts here absorb the Linux/macOS
/// `statvfs` layout differences.
pub fn statfs_path(path: &Path) -> Result<StatFs, Errno> {
    let c_path = c_path(path)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if ret != 0 {
        return Err(last_errno());
    }
    Ok(StatFs {
        blocks: stat.f_blocks as u64,
        bfree: stat.f_bfree as u64,
        bavail: stat.f_bavail as u64,
        files: stat.f_files as u64,
        ffree: stat.f_ffree as u64,
        bsize: stat.f_bsize as u32,
        namelen: stat.f_namemax as u32,
        frsize: stat.f_frsize as u32,
    })
}

// ---------------------------------------------------------------------------
// No-follow extended attribute syscalls
// ---------------------------------------------------------------------------

/// Extended attributes on FreeBSD.
///
/// FreeBSD has no `l*xattr` syscalls. Extended attributes are reached through
/// `extattr_*_link(2)`, which differs from the Linux API in three ways:
///
/// * The namespace is a separate argument rather than a prefix on the name,
///   so `user.foo` becomes `(EXTATTR_NAMESPACE_USER, "foo")`. A name with no
///   known prefix is rejected rather than silently aliased onto the user
///   namespace: Linux rejects those too, with ENOTSUP, "The namespace prefix
///   of name is not valid" (setxattr(2)).
/// * `extattr_list_link` returns "a single byte containing the length of the
///   attribute name, followed by the attribute name ... not terminated by
///   ASCII 0 (nul)" (extattr(2)), while callers here expect NUL-terminated
///   names, so the list is re-encoded and the prefix put back.
/// * `extattr_set_link` has no flags argument, so `XATTR_CREATE` and
///   `XATTR_REPLACE` are emulated with a preceding existence probe. That
///   probe is not atomic; FreeBSD offers no atomic equivalent.
#[cfg(target_os = "freebsd")]
pub mod freebsd_xattr {
    use std::ffi::{CStr, CString};

    use typed_fuse_core::Errno;

    // The FUSE protocol carries the Linux values, which FreeBSD's libc does
    // not define. From linux/include/uapi/linux/xattr.h:
    //   #define XATTR_CREATE  0x1  /* set value, fail if attr already exists */
    //   #define XATTR_REPLACE 0x2  /* set value, fail if attr does not exist */
    const XATTR_CREATE: libc::c_int = 0x1;
    const XATTR_REPLACE: libc::c_int = 0x2;

    // Same header: XATTR_USER_PREFIX "user.", XATTR_SYSTEM_PREFIX "system.".
    const USER_PREFIX: &[u8] = b"user.";
    const SYSTEM_PREFIX: &[u8] = b"system.";

    /// Split a Linux-style namespaced attribute name into the FreeBSD
    /// namespace id and the bare name.
    ///
    /// Returns `EOPNOTSUPP` for a name in no known namespace, matching what
    /// Linux does with the same input.
    pub fn split_namespace(name: &[u8]) -> Result<(libc::c_int, CString), Errno> {
        let (namespace, bare) = if let Some(rest) = name.strip_prefix(USER_PREFIX) {
            (libc::EXTATTR_NAMESPACE_USER, rest)
        } else if let Some(rest) = name.strip_prefix(SYSTEM_PREFIX) {
            (libc::EXTATTR_NAMESPACE_SYSTEM, rest)
        } else {
            return Err(Errno::EOPNOTSUPP);
        };
        // An empty bare name is not an attribute, and CString::new would
        // happily build one.
        if bare.is_empty() {
            return Err(Errno::EOPNOTSUPP);
        }
        Ok((namespace, CString::new(bare).map_err(|_| Errno::EINVAL)?))
    }

    /// True when the attribute is present. Used only to emulate the
    /// create/replace flags, so a failure to look it up reads as absent.
    fn exists(path: &CStr, namespace: libc::c_int, bare: &CStr) -> bool {
        // extattr(2): a NULL data pointer returns the size instead of copying.
        let ret = unsafe {
            libc::extattr_get_link(
                path.as_ptr(),
                namespace,
                bare.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        ret >= 0
    }

    /// `extattr_set_link`, with the Linux create/replace flags emulated.
    pub fn setxattr(
        path: &CStr,
        name: &CStr,
        value: &[u8],
        flags: libc::c_int,
    ) -> Result<(), Errno> {
        let (namespace, bare) = split_namespace(name.to_bytes())?;
        if flags & (XATTR_CREATE | XATTR_REPLACE) != 0 {
            let present = exists(path, namespace, &bare);
            if flags & XATTR_CREATE != 0 && present {
                return Err(Errno::EEXIST);
            }
            if flags & XATTR_REPLACE != 0 && !present {
                return Err(Errno::ENODATA);
            }
        }
        let ret = unsafe {
            libc::extattr_set_link(
                path.as_ptr(),
                namespace,
                bare.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
            )
        };
        // Unlike lsetxattr, this returns the number of bytes written.
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    /// `extattr_get_link`. An empty `buf` probes the size.
    pub fn getxattr(path: &CStr, name: &CStr, buf: &mut [u8]) -> Result<usize, Errno> {
        let (namespace, bare) = split_namespace(name.to_bytes())?;
        let (ptr, size) = if buf.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        let ret = unsafe {
            libc::extattr_get_link(path.as_ptr(), namespace, bare.as_ptr(), ptr, size)
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(ret as usize)
        }
    }

    /// `extattr_delete_link`.
    pub fn removexattr(path: &CStr, name: &CStr) -> Result<(), Errno> {
        let (namespace, bare) = split_namespace(name.to_bytes())?;
        let ret =
            unsafe { libc::extattr_delete_link(path.as_ptr(), namespace, bare.as_ptr()) };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    }

    /// Re-encode one `extattr_list_link` buffer into NUL-terminated names
    /// carrying `prefix`.
    ///
    /// The input is a run of `[len][name]` entries with no terminators. A
    /// trailing entry that runs past the end means the buffer was cut short,
    /// which extattr(2) does not define a behaviour for, so it is reported
    /// rather than guessed at.
    fn decode_list(raw: &[u8], prefix: &[u8], out: &mut Vec<u8>) -> Result<(), Errno> {
        let mut i = 0;
        while i < raw.len() {
            let len = raw[i] as usize;
            i += 1;
            let end = i.checked_add(len).ok_or(Errno::EIO)?;
            if end > raw.len() {
                return Err(Errno::EIO);
            }
            out.extend_from_slice(prefix);
            out.extend_from_slice(&raw[i..end]);
            out.push(0);
            i = end;
        }
        Ok(())
    }

    /// Read one namespace's list and append it to `out`, prefixed.
    fn append_namespace(path: &CStr, namespace: libc::c_int, prefix: &[u8], out: &mut Vec<u8>)
        -> Result<(), Errno> {
        let size = unsafe {
            libc::extattr_list_link(path.as_ptr(), namespace, std::ptr::null_mut(), 0)
        };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if size == 0 {
            return Ok(());
        }
        let mut raw = vec![0u8; size as usize];
        let len = unsafe {
            libc::extattr_list_link(
                path.as_ptr(),
                namespace,
                raw.as_mut_ptr() as *mut libc::c_void,
                raw.len(),
            )
        };
        if len < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // The list can shrink between the sizing call and this one.
        raw.truncate(len as usize);
        decode_list(&raw, prefix, out)
    }

    /// `extattr_list_link`, re-encoded as the NUL-terminated list the callers
    /// here expect. An empty `buf` probes the size of that encoding, which is
    /// not the size `extattr_list_link` reports.
    ///
    /// Only the user namespace is listed. `extattr_list_link` takes one
    /// namespace per call, and the system namespace is privileged, so
    /// enumerating it would fail for the unprivileged callers a passthrough
    /// filesystem serves. Named attributes in it stay reachable through
    /// get/set/remove.
    pub fn listxattr(path: &CStr, buf: &mut [libc::c_char]) -> Result<usize, Errno> {
        let mut names = Vec::new();
        append_namespace(path, libc::EXTATTR_NAMESPACE_USER, USER_PREFIX, &mut names)?;
        if buf.is_empty() {
            return Ok(names.len());
        }
        if buf.len() < names.len() {
            return Err(Errno::ERANGE);
        }
        for (slot, byte) in buf.iter_mut().zip(names.iter()) {
            *slot = *byte as libc::c_char;
        }
        Ok(names.len())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn splits_known_namespaces() {
            let (ns, bare) = split_namespace(b"user.encfs.abc").unwrap();
            assert_eq!(ns, libc::EXTATTR_NAMESPACE_USER);
            assert_eq!(bare.to_bytes(), b"encfs.abc");

            let (ns, bare) = split_namespace(b"system.acl").unwrap();
            assert_eq!(ns, libc::EXTATTR_NAMESPACE_SYSTEM);
            assert_eq!(bare.to_bytes(), b"acl");
        }

        #[test]
        fn rejects_unknown_and_empty_namespaces() {
            // Linux answers ENOTSUP here too, so "user.x" and a bare "x"
            // cannot collide on the same stored attribute.
            assert_eq!(split_namespace(b"bare"), Err(Errno::EOPNOTSUPP));
            assert_eq!(split_namespace(b"trusted.x"), Err(Errno::EOPNOTSUPP));
            assert_eq!(split_namespace(b"user."), Err(Errno::EOPNOTSUPP));
        }

        #[test]
        fn decodes_length_prefixed_list() {
            // Two entries: "ab" and "cde".
            let raw = [2u8, b'a', b'b', 3, b'c', b'd', b'e'];
            let mut out = Vec::new();
            decode_list(&raw, USER_PREFIX, &mut out).unwrap();
            assert_eq!(out, b"user.ab\0user.cde\0");
        }

        #[test]
        fn decodes_empty_list() {
            let mut out = Vec::new();
            decode_list(&[], USER_PREFIX, &mut out).unwrap();
            assert!(out.is_empty());
        }

        #[test]
        fn rejects_a_truncated_entry() {
            // Claims 9 bytes but supplies 2.
            let raw = [9u8, b'a', b'b'];
            let mut out = Vec::new();
            assert_eq!(decode_list(&raw, USER_PREFIX, &mut out), Err(Errno::EIO));
        }
    }
}

/// Set an extended attribute without following symlinks.
///
/// On Linux this is `lsetxattr`, on macOS `setxattr` with `XATTR_NOFOLLOW`,
/// and on FreeBSD `extattr_set_link` (see [`freebsd_xattr`]).
pub fn setxattr_nofollow(
    path: &CStr,
    name: &CStr,
    value: &[u8],
    flags: libc::c_int,
) -> Result<(), Errno> {
    #[cfg(target_os = "freebsd")]
    {
        freebsd_xattr::setxattr(path, name, value, flags)
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let ret = unsafe {
            #[cfg(target_os = "macos")]
            {
                libc::setxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    0,
                    flags | libc::XATTR_NOFOLLOW,
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                libc::lsetxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr() as *const libc::c_void,
                    value.len(),
                    flags,
                )
            }
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    }
}

/// Get an extended attribute without following symlinks.
///
/// Pass an empty slice for `buf` to probe the value length. Returns the
/// number of bytes stored (or that would be stored when probing).
pub fn getxattr_nofollow(path: &CStr, name: &CStr, buf: &mut [u8]) -> Result<usize, Errno> {
    #[cfg(target_os = "freebsd")]
    {
        freebsd_xattr::getxattr(path, name, buf)
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let (ptr, size) = if buf.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        let ret = unsafe {
            #[cfg(target_os = "macos")]
            {
                libc::getxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    ptr,
                    size,
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                libc::lgetxattr(path.as_ptr(), name.as_ptr(), ptr, size)
            }
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(ret as usize)
        }
    }
}

/// Read an extended attribute into a fresh `Vec`, probing the size first.
pub fn getxattr_value_nofollow(path: &CStr, name: &CStr) -> Result<Vec<u8>, Errno> {
    let size = getxattr_nofollow(path, name, &mut [])?;
    let mut value = vec![0u8; size];
    let len = getxattr_nofollow(path, name, &mut value)?;
    value.truncate(len);
    Ok(value)
}

/// List extended attribute names without following symlinks.
///
/// Pass an empty slice for `buf` to probe the list length. Returns the
/// number of bytes stored (or that would be stored when probing). The list
/// is a sequence of NUL-terminated names.
pub fn listxattr_nofollow(path: &CStr, buf: &mut [libc::c_char]) -> Result<usize, Errno> {
    #[cfg(target_os = "freebsd")]
    {
        freebsd_xattr::listxattr(path, buf)
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let (ptr, size) = if buf.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (buf.as_mut_ptr(), buf.len())
        };
        let ret = unsafe {
            #[cfg(target_os = "macos")]
            {
                libc::listxattr(path.as_ptr(), ptr, size, libc::XATTR_NOFOLLOW)
            }
            #[cfg(not(target_os = "macos"))]
            {
                libc::llistxattr(path.as_ptr(), ptr, size)
            }
        };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(ret as usize)
        }
    }
}

/// Read the NUL-separated extended attribute name list into a fresh `Vec`,
/// probing the size first. Each name is a `Vec<u8>` without the terminator.
pub fn listxattr_names_nofollow(path: &CStr) -> Result<Vec<Vec<u8>>, Errno> {
    let size = listxattr_nofollow(path, &mut [])?;
    let mut buf = vec![0 as libc::c_char; size];
    let len = listxattr_nofollow(path, &mut buf)?;
    buf.truncate(len);
    Ok(buf
        .split(|&c| c == 0)
        .filter(|name| !name.is_empty())
        .map(|name| name.iter().map(|&c| c as u8).collect())
        .collect())
}

/// Remove an extended attribute without following symlinks.
pub fn removexattr_nofollow(path: &CStr, name: &CStr) -> Result<(), Errno> {
    #[cfg(target_os = "freebsd")]
    {
        freebsd_xattr::removexattr(path, name)
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let ret = unsafe {
            #[cfg(target_os = "macos")]
            {
                libc::removexattr(path.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW)
            }
            #[cfg(not(target_os = "macos"))]
            {
                libc::lremovexattr(path.as_ptr(), name.as_ptr())
            }
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    }
}

/// Build a `CString` from a filesystem path, for use with the libc xattr
/// functions above.
pub fn c_path(path: &Path) -> Result<CString, Errno> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)
}

/// Build a `CString` from an extended attribute name.
pub fn c_name(name: &OsStr) -> Result<CString, Errno> {
    CString::new(name.as_bytes()).map_err(|_| Errno::EINVAL)
}

/// Returns true for macOS-resident metadata attributes (e.g. `com.apple.*`)
/// that the underlying filesystem may attach to backing files. Passthrough
/// filesystems usually hide these from the mounted view.
pub fn is_apple_xattr(name: &str) -> bool {
    name.starts_with("com.apple.")
}

// ---------------------------------------------------------------------------
// POSIX permission checks
// ---------------------------------------------------------------------------

/// POSIX `access(2)` permission check: selects the owner/group/other mode
/// triplet based on the caller's uid/gid versus the file's, and verifies the
/// requested `mask` (`R_OK`/`W_OK`/`X_OK` bits) against it. Root always
/// passes. A `mask` of 0 (`F_OK`, existence probe) trivially passes: callers
/// are expected to have already checked existence while resolving `file_*`
/// metadata, so reaching here at all means the file exists.
pub fn access_check(
    caller: &Caller,
    file_uid: u32,
    file_gid: u32,
    mode: u32,
    mask: u32,
) -> Result<(), Errno> {
    debug_assert!(
        mask & !0o7 == 0,
        "access_check mask contains non-RWX bits: {mask:#o}"
    );
    if mask & 0o7 == 0 || caller.uid == 0 {
        return Ok(());
    }
    let effective = if caller.uid == file_uid {
        (mode >> 6) & 0o7
    } else if caller.gid == file_gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    let need = mask & 0o7;
    if (effective & need) == need {
        Ok(())
    } else {
        Err(Errno::EACCES)
    }
}

/// POSIX utimens permission check: the owner and root may always set times;
/// others may set both times to the current time only if they have write
/// access; setting an explicit time requires the owner or root.
///
/// `atime`/`mtime` are `Some` when the caller requested an explicit or
/// current time (as opposed to leaving the timestamp untouched).
pub fn utimens_permission_check(
    caller: &Caller,
    file_uid: u32,
    file_gid: u32,
    mode: u32,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
) -> Result<(), Errno> {
    if caller.uid == 0 {
        return Ok(());
    }
    if caller.uid == file_uid {
        return Ok(());
    }
    let setting_atime = atime.is_some();
    let setting_mtime = mtime.is_some();
    if !setting_atime && !setting_mtime {
        return Ok(());
    }
    let now = SystemTime::now();
    // FUSE passes UTIME_NOW as SystemTime::now() at callback time; explicit
    // times (e.g. `utime $now $now`) can be tens of ms in the past. Use a
    // small window to distinguish "now" from an explicit timestamp.
    let near_now = |t: SystemTime| {
        now.duration_since(t).unwrap_or(Duration::MAX) < Duration::from_millis(10)
            || t.duration_since(now).unwrap_or(Duration::MAX) < Duration::from_millis(10)
    };
    let setting_to_current = atime.is_none_or(near_now) && mtime.is_none_or(near_now);
    if setting_to_current {
        let has_write = (caller.uid == file_uid && (mode & 0o200) != 0)
            || (caller.gid == file_gid && (mode & 0o020) != 0)
            || (mode & 0o002) != 0;
        if has_write {
            return Ok(());
        }
        return Err(Errno::EACCES);
    }
    Err(Errno::EPERM)
}

/// Whether an entry this process creates already has `caller`'s ownership:
/// the kernel gives new entries the process's effective uid and gid, not the
/// real ones (which differ under a setuid or setgid launcher).
fn owned_by_caller(caller: &Caller) -> bool {
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    caller.uid == uid && caller.gid == gid
}

/// Set ownership of an open file descriptor to the caller's uid/gid when it
/// differs from the current process's effective ids, which a new entry gets.
/// Skips the `fchown` when already correct and ignores `EPERM` so
/// unprivileged mounts continue to work.
pub fn set_ownership_fd(fd: RawFd, caller: &Caller) -> Result<(), Errno> {
    if owned_by_caller(caller) {
        return Ok(());
    }
    if unsafe { libc::fchown(fd, caller.uid as libc::uid_t, caller.gid as libc::gid_t) } == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EPERM) {
            return Err(err.into());
        }
    }
    Ok(())
}

/// Set ownership of a path to the caller's uid/gid when it differs from the
/// current process's effective ids, which a new entry gets. Skips the
/// `chown` when already correct and ignores `EPERM` so unprivileged mounts
/// continue to work.
pub fn set_ownership_path(path: &Path, caller: &Caller) -> Result<(), Errno> {
    if owned_by_caller(caller) {
        return Ok(());
    }
    let c_path = c_path(path)?;
    if unsafe {
        libc::chown(
            c_path.as_ptr(),
            caller.uid as libc::uid_t,
            caller.gid as libc::gid_t,
        )
    } == -1
    {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EPERM) {
            return Err(err.into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mode / ownership / timestamps
// ---------------------------------------------------------------------------

fn last_errno() -> Errno {
    std::io::Error::last_os_error().into()
}

/// Change permissions of an open file descriptor (`fchmod`).
pub fn chmod_fd(fd: RawFd, mode: u32) -> Result<(), Errno> {
    if unsafe { libc::fchmod(fd, mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// Change permissions of a path without following symlinks.
pub fn chmod_path(path: &Path, mode: u32) -> Result<(), Errno> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(Into::into)
}

/// Change owner/group of an open file descriptor (`fchown`). `None` leaves
/// the id unchanged (the chown(2) `-1` convention).
pub fn chown_fd(fd: RawFd, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
    let ret = unsafe { libc::fchown(fd, uid.unwrap_or(u32::MAX), gid.unwrap_or(u32::MAX)) };
    if ret == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// Change owner/group of a path without following symlinks (`lchown`).
/// `None` leaves the id unchanged.
pub fn chown_path(path: &Path, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno> {
    let c_path = c_path(path)?;
    let ret = unsafe {
        libc::lchown(
            c_path.as_ptr(),
            uid.unwrap_or(u32::MAX),
            gid.unwrap_or(u32::MAX),
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

fn to_timespec(t: Option<SystemTime>) -> libc::timespec {
    match t {
        Some(ts) => {
            let d = ts
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            libc::timespec {
                tv_sec: d.as_secs() as i64,
                tv_nsec: d.subsec_nanos() as i64,
            }
        }
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
    }
}

/// Set access/modification times on an open file descriptor (`futimens`).
/// `None` leaves the timestamp untouched (`UTIME_OMIT`).
pub fn utimens_fd(
    fd: RawFd,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
) -> Result<(), Errno> {
    let times = [to_timespec(atime), to_timespec(mtime)];
    if unsafe { libc::futimens(fd, times.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// Set access/modification times on a path without following symlinks
/// (`utimensat` with `AT_SYMLINK_NOFOLLOW`). `None` = `UTIME_OMIT`.
pub fn utimens_path(
    path: &Path,
    atime: Option<SystemTime>,
    mtime: Option<SystemTime>,
) -> Result<(), Errno> {
    let c_path = c_path(path)?;
    let times = [to_timespec(atime), to_timespec(mtime)];
    let ret = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// Create a filesystem node: FIFO, character/block device, or socket,
/// dispatched on the `S_IFMT` bits of `mode` (`mkfifo`/`mknod`). Regular
/// files are rejected with `EINVAL` — open them with `create` semantics so
/// callers can write payload metadata (headers) atomically.
pub fn mknod(path: &Path, mode: u32, rdev: u32) -> Result<(), Errno> {
    let c_path = c_path(path)?;
    let mode_t = mode as libc::mode_t;
    let mode_bits = mode_t & libc::S_IFMT;
    let ret = if mode_bits == libc::S_IFIFO {
        unsafe { libc::mkfifo(c_path.as_ptr(), mode_t) }
    } else if mode_bits == libc::S_IFCHR
        || mode_bits == libc::S_IFBLK
        || mode_bits == libc::S_IFSOCK
    {
        unsafe { libc::mknod(c_path.as_ptr(), mode_t, rdev as libc::dev_t) }
    } else {
        return Err(Errno::EINVAL);
    };
    if ret == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// Create a symlink at `link` pointing to `target`, an arbitrary byte string
/// (not necessarily valid UTF-8/`Path` syntax). Passthrough filesystems that
/// transform symlink targets (e.g. encrypt them) need this instead of
/// `std::os::unix::fs::symlink`, which requires target to look like a path.
pub fn symlink(target: &OsStr, link: &Path) -> Result<(), Errno> {
    let c_target = CString::new(target.as_bytes()).map_err(|_| Errno::EINVAL)?;
    let c_link = c_path(link)?;
    let ret = unsafe { libc::symlink(c_target.as_ptr(), c_link.as_ptr()) };
    if ret == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

// ---------------------------------------------------------------------------
// Renames
// ---------------------------------------------------------------------------

/// Rename `from` (relative to `fromfd`) to `to` (relative to `tofd`), but
/// only if nothing is at `to`: unlike rename(2), which replaces an existing
/// file or empty directory, this fails with `EEXIST`. The check and the
/// rename are one atomic step.
///
/// Returns `Ok(true)` once renamed, and `Ok(false)`, having done nothing,
/// where the platform or the filesystem can't refuse to replace: the caller
/// then needs a fallback of its own, such as a check and rename(2) under a
/// lock. `fromfd` and `tofd` may be `libc::AT_FDCWD`.
///
/// - Linux: `renameat2(RENAME_NOREPLACE)`, called through `syscall(2)` so it
///   needs no particular C library version. A kernel before 3.15 answers
///   `ENOSYS`, a filesystem without the flag `EINVAL`.
/// - macOS: `renameatx_np(RENAME_EXCL)`. A filesystem without it answers
///   `ENOTSUP`.
/// - FreeBSD: `renameat2(AT_RENAME_NOREPLACE)`, new in FreeBSD 15. It is
///   looked up at run time, so the same binary runs on FreeBSD 14, where
///   this returns `Ok(false)`.
///
/// Those answers, and `EINVAL` generally, count as unsupported. A rename
/// that is invalid for another reason, such as a directory into itself,
/// then gets its error from the fallback instead.
pub fn rename_noreplace_at(
    fromfd: RawFd,
    from: &CStr,
    tofd: RawFd,
    to: &CStr,
) -> Result<bool, Errno> {
    let Some(ret) = noreplace::renameat(fromfd, from, tofd, to) else {
        return Ok(false);
    };
    if ret == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    let unsupported = [libc::EINVAL, libc::ENOSYS, libc::ENOTSUP, libc::EOPNOTSUPP];
    match err.raw_os_error() {
        Some(n) if unsupported.contains(&n) => Ok(false),
        _ => Err(err.into()),
    }
}

/// The platform's no-replace rename, returning its result (0 or -1 with
/// `errno`), or `None` where there is none.
mod noreplace {
    use std::ffi::CStr;
    use std::os::unix::io::RawFd;

    #[cfg(target_os = "linux")]
    pub(super) fn renameat(fromfd: RawFd, from: &CStr, tofd: RawFd, to: &CStr) -> Option<i32> {
        // SAFETY: both names are valid C strings for the duration of the call.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                fromfd,
                from.as_ptr(),
                tofd,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        Some(ret as i32)
    }

    #[cfg(target_os = "macos")]
    pub(super) fn renameat(fromfd: RawFd, from: &CStr, tofd: RawFd, to: &CStr) -> Option<i32> {
        // SAFETY: both names are valid C strings for the duration of the call.
        Some(unsafe {
            libc::renameatx_np(fromfd, from.as_ptr(), tofd, to.as_ptr(), libc::RENAME_EXCL)
        })
    }

    #[cfg(target_os = "freebsd")]
    pub(super) fn renameat(fromfd: RawFd, from: &CStr, tofd: RawFd, to: &CStr) -> Option<i32> {
        use std::sync::OnceLock;
        type Renameat2 = unsafe extern "C" fn(
            libc::c_int,
            *const libc::c_char,
            libc::c_int,
            *const libc::c_char,
            libc::c_uint,
        ) -> libc::c_int;
        // Linking `renameat2` directly would keep the binary from loading on
        // FreeBSD 14, whose libc lacks it.
        static RENAMEAT2: OnceLock<Option<Renameat2>> = OnceLock::new();
        let renameat2 = (*RENAMEAT2.get_or_init(|| {
            // `RTLD_DEFAULT`, which the libc crate doesn't define for FreeBSD.
            let default = -2isize as *mut libc::c_void;
            // SAFETY: the name is a valid C string.
            let sym = unsafe { libc::dlsym(default, c"renameat2".as_ptr()) };
            // SAFETY: FreeBSD 15 declares `renameat2` with this signature.
            (!sym.is_null())
                .then(|| unsafe { std::mem::transmute::<*mut libc::c_void, Renameat2>(sym) })
        }))?;
        // SAFETY: both names are valid C strings for the duration of the call.
        Some(unsafe {
            renameat2(
                fromfd,
                from.as_ptr(),
                tofd,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
    pub(super) fn renameat(_: RawFd, _: &CStr, _: RawFd, _: &CStr) -> Option<i32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An existing entry, a file or an empty directory, is never replaced; a
    /// free name is renamed to. Linux and macOS test filesystems support it;
    /// FreeBSD 14 doesn't, and then nothing at all is renamed.
    #[test]
    fn rename_noreplace_never_replaces() {
        use std::os::unix::io::AsRawFd;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("from")).unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        std::fs::write(root.join("file"), b"kept").unwrap();
        let dir = std::fs::File::open(root).unwrap();
        let fd = dir.as_raw_fd();
        let supported = rename_noreplace_at(fd, c"from", fd, c"to").unwrap();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(supported);
        }
        let from = if supported { c"to" } else { c"from" };
        assert!(root.join(from.to_str().unwrap()).is_dir());

        for existing in [c"empty", c"file"] {
            let result = rename_noreplace_at(fd, from, fd, existing);
            if supported {
                assert_eq!(result, Err(Errno::EEXIST), "{existing:?}");
            } else {
                assert_eq!(result, Ok(false), "{existing:?}");
            }
        }
        assert!(root.join(from.to_str().unwrap()).is_dir());
        assert!(root.join("empty").is_dir());
        assert_eq!(std::fs::read(root.join("file")).unwrap(), b"kept");
    }

    /// A caller with the process's effective ids needs no ownership change,
    /// so none is attempted: a chown of this missing path would fail.
    #[test]
    fn set_ownership_skips_a_caller_with_the_effective_ids() {
        let caller = Caller {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            pid: 0,
            umask: 0,
        };
        assert!(owned_by_caller(&caller));
        set_ownership_path(Path::new("/nonexistent/typed-fuse"), &caller).unwrap();
    }

    #[test]
    fn system_time_preserves_fractional_pre_epoch_timestamp() {
        let timestamp = system_time_from_secs(-1, 500_000_000);

        assert_eq!(
            SystemTime::UNIX_EPOCH.duration_since(timestamp).unwrap(),
            Duration::new(0, 500_000_000)
        );
    }

    #[test]
    fn system_time_converts_whole_second_pre_epoch_timestamp() {
        let timestamp = system_time_from_secs(-1, 0);

        assert_eq!(
            SystemTime::UNIX_EPOCH.duration_since(timestamp).unwrap(),
            Duration::new(1, 0)
        );
    }

    #[test]
    fn utimens_allows_owner_and_root() {
        let t = SystemTime::UNIX_EPOCH;
        let owner = Caller {
            uid: 1000,
            ..Default::default()
        };
        // Owner may set explicit times.
        assert!(utimens_permission_check(&owner, 1000, 1000, 0o644, Some(t), Some(t)).is_ok());
        let root = Caller::default();
        assert!(utimens_permission_check(&root, 1000, 1000, 0o644, Some(t), Some(t)).is_ok());
    }

    #[test]
    fn utimens_rejects_explicit_time_from_non_owner() {
        let t = SystemTime::UNIX_EPOCH;
        let other = Caller {
            uid: 2000,
            gid: 2000,
            ..Default::default()
        };
        assert_eq!(
            utimens_permission_check(&other, 1000, 1000, 0o666, Some(t), Some(t)),
            Err(Errno::EPERM)
        );
    }

    #[test]
    fn utimens_allows_now_from_writer() {
        let now = SystemTime::now();
        let other = Caller {
            uid: 2000,
            gid: 2000,
            ..Default::default()
        };
        // World-writable file: setting times to "now" is allowed.
        assert!(utimens_permission_check(&other, 1000, 1000, 0o666, Some(now), Some(now)).is_ok());
        // Not writable: rejected with EACCES.
        assert_eq!(
            utimens_permission_check(&other, 1000, 1000, 0o444, Some(now), Some(now)),
            Err(Errno::EACCES)
        );
        // Leaving both untouched is always fine.
        assert!(utimens_permission_check(&other, 1000, 1000, 0o444, None, None).is_ok());
    }

    #[test]
    fn synthetic_attr_counts_blocks() {
        let attr = synthetic_file_attr(1, SystemTime::UNIX_EPOCH, 0o644, 0, 0);
        assert_eq!(attr.kind, FileKind::RegularFile);
        assert_eq!(attr.blocks, 1);
        assert_eq!(attr.perm, 0o644);
    }

    #[test]
    fn access_check_root_always_allowed() {
        let root = Caller::default();
        assert!(access_check(&root, 1000, 1000, 0o000, libc::R_OK as u32).is_ok());
    }

    #[test]
    fn access_check_f_ok_only_checks_existence() {
        let other = Caller {
            uid: 2000,
            gid: 2000,
            ..Default::default()
        };
        assert!(access_check(&other, 1000, 1000, 0o000, 0).is_ok());
    }

    #[test]
    fn access_check_selects_owner_group_other_triplet() {
        let owner = Caller {
            uid: 1000,
            gid: 1000,
            ..Default::default()
        };
        let group = Caller {
            uid: 3000,
            gid: 2000,
            ..Default::default()
        };
        let other = Caller {
            uid: 4000,
            gid: 4000,
            ..Default::default()
        };
        let mode = 0o640; // owner rw, group r, other none
        assert!(access_check(&owner, 1000, 2000, mode, libc::W_OK as u32).is_ok());
        assert!(access_check(&group, 1000, 2000, mode, libc::R_OK as u32).is_ok());
        assert_eq!(
            access_check(&group, 1000, 2000, mode, libc::W_OK as u32),
            Err(Errno::EACCES)
        );
        assert_eq!(
            access_check(&other, 1000, 2000, mode, libc::R_OK as u32),
            Err(Errno::EACCES)
        );
    }

    #[test]
    fn symlink_roundtrips_through_readlink() {
        let dir = std::env::temp_dir().join(format!("fuse3-symlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("link");
        let target = OsStr::new("/some/arbitrary/target");
        symlink(target, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap().as_os_str(), target);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn statfs_path_reports_root_filesystem() {
        let stat = statfs_path(Path::new("/")).unwrap();
        assert!(stat.bsize > 0);
        assert!(stat.blocks > 0);
    }
}
