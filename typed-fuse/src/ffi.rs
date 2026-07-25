//! FFI-side helpers that wrap raw libfuse structs: the directory-buffer sink
//! and small accessors for `fuse_file_info`/`fuse_conn_info`. Everything
//! attribute/stat-shaped is handled by the `conv` module instead.

#![allow(clippy::unnecessary_cast)]

use std::ffi::OsStr;
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::ptr;

use libfuse_sys::fuse_lowlevel::{
    fuse_conn_info, fuse_entry_param, fuse_file_info, fuse_req_t, stat,
};
use typed_fuse_core::{
    ConnInfo, DirSink, EntryReply, FileKind, NodeId, OpenHints, RuntimePlusSink,
};

use crate::conv::kind_to_mode_bits;

#[cfg(target_os = "macos")]
use crate::darwin::{fuse_add_direntry_plus_vanilla, fuse_add_direntry_vanilla};
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::{fuse_add_direntry, fuse_add_direntry_plus};

// --- Darwin-aliased direntry builders (see darwin.rs) ---

#[cfg(target_os = "macos")]
fn raw_add_direntry(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    stbuf: *const stat,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry_vanilla(req, buf, bufsize, name, stbuf, off) }
}
#[cfg(not(target_os = "macos"))]
fn raw_add_direntry(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    stbuf: *const stat,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry(req, buf, bufsize, name, stbuf, off) }
}

#[cfg(target_os = "macos")]
fn raw_add_direntry_plus(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    e: *const fuse_entry_param,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry_plus_vanilla(req, buf, bufsize, name, e, off) }
}
#[cfg(not(target_os = "macos"))]
fn raw_add_direntry_plus(
    req: fuse_req_t,
    buf: *mut c_char,
    bufsize: usize,
    name: *const c_char,
    e: *const fuse_entry_param,
    off: i64,
) -> usize {
    unsafe { fuse_add_direntry_plus(req, buf, bufsize, name, e, off) }
}

// --- fuse_file_info accessors ---

/// The file handle carried in a (possibly null) `fuse_file_info`.
pub(crate) fn fi_fh(fi: *mut fuse_file_info) -> u64 {
    if fi.is_null() {
        0
    } else {
        unsafe { (*fi).fh }
    }
}

pub(crate) fn fi_fh_opt(fi: *mut fuse_file_info) -> Option<u64> {
    (!fi.is_null()).then(|| unsafe { (*fi).fh })
}

/// The open flags carried in a (possibly null) `fuse_file_info`.
pub(crate) fn fi_flags(fi: *mut fuse_file_info) -> i32 {
    if fi.is_null() {
        0
    } else {
        unsafe { (*fi).flags }
    }
}

/// Writes the runtime-assigned handle and caching hints back into the
/// kernel's `fuse_file_info`. No-op if `fi` is null.
pub(crate) fn apply_open(fi: *mut fuse_file_info, fh: u64, hints: OpenHints) {
    if fi.is_null() {
        return;
    }
    unsafe {
        (*fi).fh = fh;
        (*fi).set_direct_io(hints.direct_io as u32);
        (*fi).set_keep_cache(hints.keep_cache as u32);
        (*fi).set_nonseekable(hints.nonseekable as u32);
        (*fi).set_cache_readdir(hints.cache_readdir as u32);
        #[cfg(has_parallel_direct_writes)]
        (*fi).set_parallel_direct_writes((hints.direct_io && hints.parallel_direct_writes) as u32);
    }
}

// --- fuse_conn_info bridging ---

/// Reads the connection parameters libfuse negotiated into a plain
/// [`ConnInfo`].
pub(crate) fn conn_read(conn: *mut fuse_conn_info) -> ConnInfo {
    unsafe {
        ConnInfo::from_raw(
            (*conn).proto_major,
            (*conn).proto_minor,
            (*conn).max_write,
            (*conn).max_readahead,
            (*conn).capable,
            (*conn).want,
        )
    }
}

/// Writes back the fields of [`ConnInfo`] the filesystem is allowed to tune.
pub(crate) fn conn_apply(conn: *mut fuse_conn_info, info: &ConnInfo) {
    unsafe {
        (*conn).max_write = info.max_write;
        (*conn).max_readahead = info.max_readahead;
        (*conn).want = info.want_bits();
    }
}

// --- RawDirentBuf: the DirSink implementation ---

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// Plain `READDIR` entries via `fuse_add_direntry`.
    Plain,
    /// `READDIRPLUS`-shaped entries with a synthesized, non-caching
    /// `fuse_entry_param` (nodeid 0, so the kernel performs no lookup and
    /// needs no matching `forget`). Used to answer READDIRPLUS from the
    /// filesystem's plain `readdir` output.
    Plus,
}

/// A `readdir` reply builder implementing libfuse's size-limited buffer
/// protocol and [`DirSink`]. The runtime pushes entries in via
/// [`DirSink::add`].
pub(crate) struct RawDirentBuf {
    req: fuse_req_t,
    capacity: usize,
    buf: Vec<u8>,
    format: Format,
    ttl: std::time::Duration,
}

impl RawDirentBuf {
    pub(crate) fn new(req: fuse_req_t, size: usize) -> Self {
        RawDirentBuf {
            req,
            capacity: size,
            buf: Vec::with_capacity(size),
            format: Format::Plain,
            ttl: std::time::Duration::ZERO,
        }
    }

    /// Like [`RawDirentBuf::new`], but emits attribute-carrying READDIRPLUS entries.
    pub(crate) fn new_plus(req: fuse_req_t, size: usize, ttl: std::time::Duration) -> Self {
        RawDirentBuf {
            req,
            capacity: size,
            buf: Vec::with_capacity(size),
            format: Format::Plus,
            ttl,
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

impl DirSink for RawDirentBuf {
    fn add(&mut self, name: &OsStr, id: NodeId, kind: FileKind, next_offset: u64) -> bool {
        // Real filenames never contain interior NULs; silently skip if one
        // somehow does rather than aborting the whole listing.
        let cname = match std::ffi::CString::new(name.as_bytes()) {
            Ok(c) => c,
            Err(_) => return true,
        };

        let mut st: stat = unsafe { std::mem::zeroed() };
        st.st_ino = id.ino() as _;
        st.st_mode = kind_to_mode_bits(kind) as _;

        match self.format {
            Format::Plain => {
                let entry_size =
                    raw_add_direntry(self.req, ptr::null_mut(), 0, cname.as_ptr(), ptr::null(), 0);
                if self.buf.len() + entry_size > self.capacity {
                    return false;
                }
                let old_len = self.buf.len();
                self.buf.resize(old_len + entry_size, 0);
                raw_add_direntry(
                    self.req,
                    unsafe { self.buf.as_mut_ptr().add(old_len) } as *mut c_char,
                    entry_size,
                    cname.as_ptr(),
                    &st,
                    next_offset as _,
                );
            }
            Format::Plus => unreachable!("plus entries require attributes"),
        }
        true
    }
}

impl RuntimePlusSink for RawDirentBuf {
    fn add(&mut self, name: &OsStr, entry: EntryReply, next_offset: u64) -> bool {
        let cname = match std::ffi::CString::new(name.as_bytes()) {
            Ok(c) => c,
            Err(_) => return true,
        };
        let param =
            crate::conv::entry_to_entry_param(entry.ino, entry.generation, &entry.attr, self.ttl);
        let entry_size =
            raw_add_direntry_plus(self.req, ptr::null_mut(), 0, cname.as_ptr(), &param, 0);
        if self.buf.len() + entry_size > self.capacity {
            return false;
        }
        let old_len = self.buf.len();
        self.buf.resize(old_len + entry_size, 0);
        raw_add_direntry_plus(
            self.req,
            unsafe { self.buf.as_mut_ptr().add(old_len) } as *mut c_char,
            entry_size,
            cname.as_ptr(),
            &param,
            next_offset as _,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typed_fuse_core::ConnectionCapability;

    #[test]
    fn optional_file_info_distinguishes_null_from_handle_zero() {
        assert_eq!(fi_fh_opt(std::ptr::null_mut()), None);
        let mut fi: fuse_file_info = unsafe { std::mem::zeroed() };
        fi.fh = 0;
        assert_eq!(fi_fh_opt(&mut fi), Some(0));
    }

    #[test]
    #[cfg(has_parallel_direct_writes)]
    fn parallel_direct_writes_requires_direct_io() {
        let mut fi: fuse_file_info = unsafe { std::mem::zeroed() };
        apply_open(
            &mut fi,
            7,
            OpenHints {
                parallel_direct_writes: true,
                ..Default::default()
            },
        );
        assert_eq!(fi.parallel_direct_writes(), 0);
        apply_open(
            &mut fi,
            8,
            OpenHints {
                direct_io: true,
                parallel_direct_writes: true,
                ..Default::default()
            },
        );
        assert_eq!(fi.fh, 8);
        assert_eq!(fi.parallel_direct_writes(), 1);
    }

    #[test]
    fn capability_update_preserves_unrelated_want_bits() {
        let mut raw: fuse_conn_info = unsafe { std::mem::zeroed() };
        raw.capable = (1 << 0) | (1 << 18) | (1 << 10);
        raw.want = (1 << 0) | (1 << 10);
        let mut info = conn_read(&mut raw);
        assert!(info.set_enabled(ConnectionCapability::ParallelDirectoryOperations, true));
        assert!(info.set_enabled(ConnectionCapability::AsyncRead, false));
        conn_apply(&mut raw, &info);
        assert_eq!(raw.want & (1 << 10), 1 << 10);
        assert_eq!(raw.want & (1 << 18), 1 << 18);
        assert_eq!(raw.want & 1, 0);
    }
}
