//! Trampolines from the raw `fuse_lowlevel_ops` C callback table to a
//! [`Runtime`] driving a [`NodeFs`] implementation, plus the [`Session`]
//! type that owns a mounted FUSE session end to end.
//!
//! # Threading
//!
//! Trampolines recover a shared [`Runtime`] reference. Its metadata is
//! synchronized internally, and [`NodeFs`] requires thread-safe payloads.
//!
//! # Panics
//!
//! Every replying shim reaches the runtime through [`call_fs`], which wraps
//! the call in [`catch_unwind`]; a panicking filesystem method results in
//! `EIO` being sent to the kernel instead of unwinding across the
//! `extern "C"` boundary (undefined behavior). `forget`/`forget_multi` have
//! no error reply, so a panic there is caught, logged, and swallowed.
//!
//! # Darwin symbol aliasing
//!
//! See `darwin.rs`: on macOS `fuse_reply_entry`/`_attr`/`_create`/`_statfs`
//! and the direntry builders are aliased to non-existent `<name>$DARWIN`
//! symbols. This module (and `ffi.rs`) always go through the cfg'd
//! `raw_reply_*`/`raw_add_direntry*` wrappers so the rest of the code stays
//! `#[cfg]`-free. `setattr`'s callback is typed `*mut fuse_darwin_attr` by
//! bindgen on macOS; since Darwin extensions are disabled at the session
//! level, libfuse actually passes a vanilla `stat*`, which `setattr_shim`
//! reinterprets.

use std::ffi::{CStr, CString, OsStr};
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::ptr;
use std::rc::Rc;
use std::time::Duration;

use libfuse_sys::fuse_lowlevel::{
    dev_t, flock, fuse_args, fuse_conn_info, fuse_ctx, fuse_entry_param, fuse_file_info,
    fuse_forget_data, fuse_ino_t, fuse_lowlevel_ops, fuse_opt_free_args,
    fuse_remove_signal_handlers, fuse_reply_buf, fuse_reply_err, fuse_reply_lock, fuse_reply_lseek,
    fuse_reply_none, fuse_reply_open, fuse_reply_readlink, fuse_reply_write, fuse_reply_xattr,
    fuse_req_ctx, fuse_req_t, fuse_req_userdata, fuse_session, fuse_session_destroy,
    fuse_session_loop, fuse_session_mount, fuse_session_unmount, fuse_set_signal_handlers, mode_t,
    off_t, stat,
};
use libfuse_sys::fuse_lowlevel::{
    fuse_loop_config, loop_cfg_create_312, loop_cfg_destroy_312, loop_cfg_set_clone_fd_312,
    loop_cfg_set_idle_threads_312, loop_cfg_set_max_threads_312, session_loop_mt_312,
};

// macOS's macFUSE ships a patched libfuse with an ABI-versioned
// `fuse_session_new_versioned()` (and the `libfuse_version` struct it
// takes); vanilla Linux libfuse only has the plain `fuse_session_new()`.
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::fuse_session_new;
#[cfg(target_os = "macos")]
use libfuse_sys::fuse_lowlevel::{
    fuse_session_new_versioned, libfuse_version, FUSE_HOTFIX_VERSION, FUSE_MAJOR_VERSION,
    FUSE_MINOR_VERSION,
};

#[cfg(target_os = "macos")]
use libfuse_sys::fuse_lowlevel::{fuse_darwin_attr, statfs};
#[cfg(not(target_os = "macos"))]
use libfuse_sys::fuse_lowlevel::{
    fuse_reply_attr, fuse_reply_create, fuse_reply_entry, fuse_reply_statfs, statvfs,
};

#[cfg(target_os = "macos")]
use crate::darwin::{
    fuse_reply_attr_vanilla, fuse_reply_create_vanilla, fuse_reply_entry_vanilla,
    fuse_reply_statfs_vanilla,
};

use typed_fuse_core::{Caller, Errno, FileLock, LookupReply, NodeFs, Runtime, XattrReply};

use crate::conv::{
    attr_to_stat, entry_to_entry_param, negative_entry_param, setattr_from_raw, statfs_to_raw,
};
use crate::ffi::{apply_open, conn_apply, conn_read, fi_fh, fi_fh_opt, fi_flags, DirBuffer};
use crate::file_lock;
use typed_fuse_core::{EntryReply, NodeAttr};

// ---------------------------------------------------------------------
// Darwin-aliased reply wrappers (see module docs / darwin.rs)
// ---------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn raw_reply_entry(req: fuse_req_t, e: *const fuse_entry_param) -> c_int {
    unsafe { fuse_reply_entry_vanilla(req, e) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_entry(req: fuse_req_t, e: *const fuse_entry_param) -> c_int {
    unsafe { fuse_reply_entry(req, e) }
}

#[cfg(target_os = "macos")]
fn raw_reply_attr(req: fuse_req_t, attr: *const stat, timeout: f64) -> c_int {
    unsafe { fuse_reply_attr_vanilla(req, attr, timeout) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_attr(req: fuse_req_t, attr: *const stat, timeout: f64) -> c_int {
    unsafe { fuse_reply_attr(req, attr, timeout) }
}

#[cfg(target_os = "macos")]
fn raw_reply_create(
    req: fuse_req_t,
    e: *const fuse_entry_param,
    fi: *const fuse_file_info,
) -> c_int {
    unsafe { fuse_reply_create_vanilla(req, e, fi) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_create(
    req: fuse_req_t,
    e: *const fuse_entry_param,
    fi: *const fuse_file_info,
) -> c_int {
    unsafe { fuse_reply_create(req, e, fi) }
}

#[cfg(target_os = "macos")]
fn raw_reply_statfs(req: fuse_req_t, stbuf: *const statfs) -> c_int {
    unsafe { fuse_reply_statfs_vanilla(req, stbuf) }
}
#[cfg(not(target_os = "macos"))]
fn raw_reply_statfs(req: fuse_req_t, stbuf: *const statvfs) -> c_int {
    unsafe { fuse_reply_statfs(req, stbuf) }
}

// ---------------------------------------------------------------------
// Holder recovery + shared helpers
// ---------------------------------------------------------------------

/// Recovers the `Runtime<F>` associated with an in-flight request. Sound
/// because `fuse_session_loop` processes requests sequentially (see module
/// docs).
fn holder_of<'a, F: NodeFs>(req: fuse_req_t) -> &'a Runtime<F> {
    unsafe { &*(fuse_req_userdata(req) as *const Runtime<F>) }
}

/// Extracts the calling process's credentials from a request.
fn caller_of(req: fuse_req_t) -> Caller {
    let ctx: &fuse_ctx = unsafe { &*fuse_req_ctx(req) };
    Caller {
        uid: ctx.uid as u32,
        gid: ctx.gid as u32,
        pid: ctx.pid as u32,
        umask: ctx.umask as u32,
    }
}

fn c_os_str<'a>(ptr: *const c_char) -> &'a OsStr {
    OsStr::from_bytes(unsafe { CStr::from_ptr(ptr) }.to_bytes())
}

/// Decodes a name argument as an OS string, preserving non-UTF-8 Unix bytes.
macro_rules! try_name {
    ($req:expr, $ptr:expr) => {
        c_os_str($ptr)
    };
}

fn catch_unwind<T>(f: impl FnOnce() -> Result<T, Errno>) -> Result<T, Errno> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(Errno::EIO),
    }
}

fn catch_unwind_unit(f: impl FnOnce()) {
    if panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        eprintln!("fuse3: filesystem callback panicked; no reply is possible for this operation");
    }
}

/// Recovers the runtime behind `req`, builds a [`Caller`], and runs `f`
/// under [`catch_unwind`]. The runtime reference is handed to `f` at the
/// caller-chosen lifetime `'a` so that borrowing return values (`read`'s
/// `Cow`) can flow out through `R`.
fn call_fs<F: NodeFs, R>(
    req: fuse_req_t,
    f: impl FnOnce(&Runtime<F>, &Caller) -> Result<R, Errno>,
) -> Result<R, Errno> {
    let rt = holder_of::<F>(req);
    let caller = caller_of(req);
    catch_unwind(move || f(rt, &caller))
}

fn reply_err(req: fuse_req_t, errno: Errno) {
    unsafe { fuse_reply_err(req, errno.raw()) };
}

fn reply_ok(req: fuse_req_t) {
    unsafe { fuse_reply_err(req, 0) };
}

fn reply_entry(req: fuse_req_t, entry: &EntryReply, ttl: Duration) {
    let param = entry_to_entry_param(entry.ino, entry.generation, &entry.attr, ttl);
    raw_reply_entry(req, &param);
}

fn reply_negative(req: fuse_req_t, ttl: Duration) {
    let param = negative_entry_param(ttl);
    raw_reply_entry(req, &param);
}

fn reply_attr(req: fuse_req_t, ino: u64, attr: &NodeAttr, ttl: Duration) {
    let st = attr_to_stat(ino, attr);
    raw_reply_attr(req, &st, ttl.as_secs_f64());
}

/// Sends a `readdir` buffer, translating an empty buffer to the null/zero
/// reply that signals end-of-stream.
fn reply_dir_buf(req: fuse_req_t, data: &[u8]) -> c_int {
    if data.is_empty() {
        unsafe { fuse_reply_buf(req, ptr::null(), 0) }
    } else {
        unsafe { fuse_reply_buf(req, data.as_ptr() as *const c_char, data.len()) }
    }
}

/// Implements the `getxattr`/`listxattr` size-query protocol.
fn reply_xattr(req: fuse_req_t, reply: &XattrReply, requested_size: usize) {
    match reply {
        XattrReply::Size(len) => {
            if requested_size == 0 {
                unsafe { fuse_reply_xattr(req, *len) };
            } else {
                reply_err(req, Errno::EIO);
            }
        }
        XattrReply::Data(data) => {
            if requested_size == 0 {
                unsafe { fuse_reply_xattr(req, data.len()) };
            } else if data.len() > requested_size {
                reply_err(req, Errno::ERANGE);
            } else {
                unsafe { fuse_reply_buf(req, data.as_ptr() as *const c_char, data.len()) };
            }
        }
    }
}

// ---------------------------------------------------------------------
// Trampolines
// ---------------------------------------------------------------------

unsafe extern "C" fn init_shim<F: NodeFs>(userdata: *mut c_void, conn: *mut fuse_conn_info) {
    let rt = unsafe { &*(userdata as *const Runtime<F>) };
    let mut info = conn_read(conn);
    catch_unwind_unit(|| rt.init(&mut info));
    conn_apply(conn, &info);
}

unsafe extern "C" fn destroy_shim<F: NodeFs>(userdata: *mut c_void) {
    let rt = unsafe { &*(userdata as *const Runtime<F>) };
    catch_unwind_unit(|| rt.destroy());
}

unsafe extern "C" fn lookup_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.lookup(parent, name, c)) {
        Ok(LookupReply::Found(entry)) => reply_entry(req, &entry, holder_of::<F>(req).ttl()),
        Ok(LookupReply::Negative) => reply_negative(req, holder_of::<F>(req).negative_ttl()),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn forget_shim<F: NodeFs>(req: fuse_req_t, ino: fuse_ino_t, nlookup: u64) {
    let rt = holder_of::<F>(req);
    catch_unwind_unit(|| rt.forget(ino, nlookup));
    unsafe { fuse_reply_none(req) };
}

unsafe extern "C" fn forget_multi_shim<F: NodeFs>(
    req: fuse_req_t,
    count: usize,
    forgets: *mut fuse_forget_data,
) {
    let rt = holder_of::<F>(req);
    let entries = unsafe { std::slice::from_raw_parts(forgets, count) };
    catch_unwind_unit(|| {
        for entry in entries {
            rt.forget(entry.ino, entry.nlookup);
        }
    });
    unsafe { fuse_reply_none(req) };
}

unsafe extern "C" fn getattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    match call_fs::<F, _>(req, |rt, c| rt.getattr(ino, fi_fh_opt(fi), c)) {
        Ok(attr) => reply_attr(req, ino, &attr, holder_of::<F>(req).ttl()),
        Err(e) => reply_err(req, e),
    }
}

fn setattr_shim_impl<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    attr: *const stat,
    to_set: c_int,
    fi: *mut fuse_file_info,
) {
    let set = setattr_from_raw(attr, to_set);
    match call_fs::<F, _>(req, |rt, c| rt.setattr(ino, fi_fh_opt(fi), &set, c)) {
        Ok(a) => reply_attr(req, ino, &a, holder_of::<F>(req).ttl()),
        Err(e) => reply_err(req, e),
    }
}

// SAFETY (macOS): bindgen types `attr` as `*mut fuse_darwin_attr` because the
// header was parsed with Darwin extensions enabled; the session disables them
// at runtime, so libfuse passes a vanilla `stat*`. See module docs.
#[cfg(target_os = "macos")]
unsafe extern "C" fn setattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    attr: *mut fuse_darwin_attr,
    to_set: c_int,
    fi: *mut fuse_file_info,
) {
    setattr_shim_impl::<F>(req, ino, attr as *const stat, to_set, fi);
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn setattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    attr: *mut stat,
    to_set: c_int,
    fi: *mut fuse_file_info,
) {
    setattr_shim_impl::<F>(req, ino, attr as *const stat, to_set, fi);
}

unsafe extern "C" fn readlink_shim<F: NodeFs>(req: fuse_req_t, ino: fuse_ino_t) {
    match call_fs::<F, _>(req, |rt, c| rt.readlink(ino, c)) {
        Ok(target) => match CString::new(target.as_os_str().as_bytes()) {
            Ok(c) => {
                unsafe { fuse_reply_readlink(req, c.as_ptr()) };
            }
            Err(_) => reply_err(req, Errno::EINVAL),
        },
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn mknod_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    mode: mode_t,
    rdev: dev_t,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| {
        rt.mknod(parent, name, mode as u32, rdev as u32, c.umask, c)
    }) {
        Ok(entry) => reply_entry(req, &entry, holder_of::<F>(req).ttl()),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn mkdir_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    mode: mode_t,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.mkdir(parent, name, mode as u32, c.umask, c)) {
        Ok(entry) => reply_entry(req, &entry, holder_of::<F>(req).ttl()),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn unlink_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.unlink(parent, name, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn rmdir_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.rmdir(parent, name, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn symlink_shim<F: NodeFs>(
    req: fuse_req_t,
    link: *const c_char,
    parent: fuse_ino_t,
    name: *const c_char,
) {
    let link = try_name!(req, link);
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.symlink(parent, name, Path::new(link), c)) {
        Ok(entry) => reply_entry(req, &entry, holder_of::<F>(req).ttl()),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn rename_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    newparent: fuse_ino_t,
    newname: *const c_char,
    flags: c_uint,
) {
    let name = try_name!(req, name);
    let newname = try_name!(req, newname);
    match call_fs::<F, _>(req, |rt, c| {
        rt.rename(parent, name, newparent, newname, flags, c)
    }) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn link_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    newparent: fuse_ino_t,
    newname: *const c_char,
) {
    let newname = try_name!(req, newname);
    match call_fs::<F, _>(req, |rt, c| rt.link(ino, newparent, newname, c)) {
        Ok(entry) => reply_entry(req, &entry, holder_of::<F>(req).ttl()),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn open_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let flags = fi_flags(fi);
    match call_fs::<F, _>(req, |rt, c| rt.open(ino, flags, c)) {
        Ok(reply) => {
            apply_open(fi, reply.fh, reply.hints);
            unsafe { fuse_reply_open(req, fi as *const fuse_file_info) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn read_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    let rt = holder_of::<F>(req);
    let caller = caller_of(req);
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        rt.read(ino, fh, off as u64, size, &caller, |result| match result {
            Ok(data) => {
                let len = data.len().min(size);
                unsafe { fuse_reply_buf(req, data[..len].as_ptr() as *const c_char, len) };
            }
            Err(e) => reply_err(req, e),
        })
    }));
    if result.is_err() {
        reply_err(req, Errno::EIO);
    }
}

unsafe extern "C" fn write_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    buf: *const c_char,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    let data = unsafe { std::slice::from_raw_parts(buf as *const u8, size) };
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.write(ino, fh, data, off as u64, c)) {
        Ok(count) => {
            unsafe { fuse_reply_write(req, count) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn flush_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.flush(ino, fh, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn release_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.release(ino, fh, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn fsync_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    datasync: c_int,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.fsync(ino, fh, datasync != 0, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn opendir_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let flags = fi_flags(fi);
    match call_fs::<F, _>(req, |rt, c| rt.opendir(ino, flags, c)) {
        Ok(reply) => {
            apply_open(fi, reply.fh, reply.hints);
            unsafe { fuse_reply_open(req, fi as *const fuse_file_info) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn readdir_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    if off < 0 {
        reply_err(req, Errno::EINVAL);
        return;
    }
    let fh = fi_fh(fi);
    let mut buf = DirBuffer::new(req, size);
    match call_fs::<F, _>(req, |rt, c| rt.readdir(ino, fh, off as u64, &mut buf, c)) {
        Ok(()) => {
            reply_dir_buf(req, buf.as_slice());
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn readdirplus_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    size: usize,
    off: off_t,
    fi: *mut fuse_file_info,
) {
    if off < 0 {
        reply_err(req, Errno::EINVAL);
        return;
    }
    let fh = fi_fh(fi);
    let mut buf = DirBuffer::new_plus(req, size, holder_of::<F>(req).ttl());
    match call_fs::<F, _>(req, |rt, c| {
        rt.readdirplus(ino, fh, off as u64, &mut buf, c)
    }) {
        Ok(retained) => {
            if reply_dir_buf(req, buf.as_slice()) != 0 {
                let rt = holder_of::<F>(req);
                for id in retained {
                    rt.forget(id.ino(), 1);
                }
            }
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn releasedir_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.releasedir(ino, fh, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn fsyncdir_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    datasync: c_int,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.fsyncdir(ino, fh, datasync != 0, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn statfs_shim<F: NodeFs>(req: fuse_req_t, ino: fuse_ino_t) {
    match call_fs::<F, _>(req, |rt, c| rt.statfs(ino, c)) {
        Ok(stats) => {
            let raw = statfs_to_raw(&stats);
            raw_reply_statfs(req, &raw);
        }
        Err(e) => reply_err(req, e),
    }
}

fn setxattr_shim_impl<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    value: *const c_char,
    size: usize,
    flags: c_int,
) {
    let name = try_name!(req, name);
    let value = unsafe { std::slice::from_raw_parts(value as *const u8, size) };
    match call_fs::<F, _>(req, |rt, c| rt.setxattr(ino, name, value, flags, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn setxattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    value: *const c_char,
    size: usize,
    flags: c_int,
    _position: u32,
) {
    setxattr_shim_impl::<F>(req, ino, name, value, size, flags);
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn setxattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    value: *const c_char,
    size: usize,
    flags: c_int,
) {
    setxattr_shim_impl::<F>(req, ino, name, value, size, flags);
}

fn getxattr_shim_impl<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    size: usize,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.getxattr(ino, name, size, c)) {
        Ok(reply) => reply_xattr(req, &reply, size),
        Err(e) => reply_err(req, e),
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn getxattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    size: usize,
    _position: u32,
) {
    getxattr_shim_impl::<F>(req, ino, name, size);
}
#[cfg(not(target_os = "macos"))]
unsafe extern "C" fn getxattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
    size: usize,
) {
    getxattr_shim_impl::<F>(req, ino, name, size);
}

unsafe extern "C" fn listxattr_shim<F: NodeFs>(req: fuse_req_t, ino: fuse_ino_t, size: usize) {
    match call_fs::<F, _>(req, |rt, c| rt.listxattr(ino, size, c)) {
        Ok(reply) => reply_xattr(req, &reply, size),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn removexattr_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    name: *const c_char,
) {
    let name = try_name!(req, name);
    match call_fs::<F, _>(req, |rt, c| rt.removexattr(ino, name, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn access_shim<F: NodeFs>(req: fuse_req_t, ino: fuse_ino_t, mask: c_int) {
    match call_fs::<F, _>(req, |rt, c| rt.access(ino, mask, c)) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn create_shim<F: NodeFs>(
    req: fuse_req_t,
    parent: fuse_ino_t,
    name: *const c_char,
    mode: mode_t,
    fi: *mut fuse_file_info,
) {
    let name = try_name!(req, name);
    let flags = fi_flags(fi);
    match call_fs::<F, _>(req, |rt, c| {
        rt.create(parent, name, mode as u32, c.umask, flags, c)
    }) {
        Ok((entry, reply)) => {
            apply_open(fi, reply.fh, reply.hints);
            let param = entry_to_entry_param(
                entry.ino,
                entry.generation,
                &entry.attr,
                holder_of::<F>(req).ttl(),
            );
            raw_reply_create(req, &param, fi as *const fuse_file_info);
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn fallocate_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    mode: c_int,
    offset: off_t,
    length: off_t,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| {
        rt.fallocate(ino, fh, mode, offset as u64, length as u64, c)
    }) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn lseek_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    off: off_t,
    whence: c_int,
    fi: *mut fuse_file_info,
) {
    let fh = fi_fh(fi);
    match call_fs::<F, _>(req, |rt, c| rt.lseek(ino, fh, off as u64, whence, c)) {
        Ok(new_off) => {
            unsafe { fuse_reply_lseek(req, new_off as off_t) };
        }
        Err(e) => reply_err(req, e),
    }
}

// libfuse's bindgen `flock` and `libc::flock` are separate Rust types over
// the same C struct, so the two bridge below field by field and the range
// arithmetic stays in `file_lock`.
fn lock_from_raw(raw: &flock) -> Result<FileLock, Errno> {
    let mut owned: libc::flock = unsafe { std::mem::zeroed() };
    owned.l_type = raw.l_type;
    owned.l_whence = raw.l_whence;
    owned.l_start = raw.l_start;
    owned.l_len = raw.l_len;
    owned.l_pid = raw.l_pid;
    file_lock::from_flock(&owned)
}

fn lock_to_raw(lock: FileLock) -> Result<flock, Errno> {
    let owned = file_lock::to_flock(lock)?;
    Ok(flock {
        l_start: owned.l_start,
        l_len: owned.l_len,
        l_pid: owned.l_pid,
        l_type: owned.l_type,
        l_whence: owned.l_whence,
    })
}

unsafe extern "C" fn getlk_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
    raw: *mut flock,
) {
    let requested = match lock_from_raw(unsafe { &*raw }) {
        Ok(v) => v,
        Err(e) => {
            reply_err(req, e);
            return;
        }
    };
    let fh = fi_fh(fi);
    let owner = if fi.is_null() {
        0
    } else {
        unsafe { (*fi).lock_owner }
    };
    match call_fs::<F, _>(req, |rt, c| rt.getlk(ino, fh, owner, requested, c)).and_then(lock_to_raw)
    {
        Ok(reply) => {
            unsafe { fuse_reply_lock(req, &reply) };
        }
        Err(e) => reply_err(req, e),
    }
}

unsafe extern "C" fn setlk_shim<F: NodeFs>(
    req: fuse_req_t,
    ino: fuse_ino_t,
    fi: *mut fuse_file_info,
    raw: *mut flock,
    sleep: c_int,
) {
    let requested = match lock_from_raw(unsafe { &*raw }) {
        Ok(v) => v,
        Err(e) => {
            reply_err(req, e);
            return;
        }
    };
    let fh = fi_fh(fi);
    let owner = if fi.is_null() {
        0
    } else {
        unsafe { (*fi).lock_owner }
    };
    match call_fs::<F, _>(req, |rt, c| {
        rt.setlk(ino, fh, owner, requested, sleep != 0, c)
    }) {
        Ok(()) => reply_ok(req),
        Err(e) => reply_err(req, e),
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn copy_file_range_shim<F: NodeFs>(
    req: fuse_req_t,
    ino_in: fuse_ino_t,
    off_in: off_t,
    _fi_in: *mut fuse_file_info,
    ino_out: fuse_ino_t,
    off_out: off_t,
    _fi_out: *mut fuse_file_info,
    len: usize,
    flags: c_int,
) {
    match call_fs::<F, _>(req, |rt, c| {
        rt.copy_file_range(
            ino_in,
            off_in as u64,
            ino_out,
            off_out as u64,
            len as u64,
            flags,
            c,
        )
    }) {
        Ok(count) => {
            unsafe { fuse_reply_write(req, count) };
        }
        Err(e) => reply_err(req, e),
    }
}

// ---------------------------------------------------------------------
// Ops table
// ---------------------------------------------------------------------

/// Builds a `fuse_lowlevel_ops` table wiring every operation the runtime
/// covers to its trampoline. Uncovered ops (`getlk`/`setlk`/`flock`,
/// `ioctl`, `poll`, `bmap`, `write_buf`, `retrieve_reply`, `statx`,
/// `tmpfile`, macOS-only extensions, ...) are left `None`.
pub(crate) fn make_ops<F: NodeFs>() -> fuse_lowlevel_ops {
    fuse_lowlevel_ops {
        init: Some(init_shim::<F>),
        destroy: Some(destroy_shim::<F>),
        lookup: Some(lookup_shim::<F>),
        forget: Some(forget_shim::<F>),
        getattr: Some(getattr_shim::<F>),
        setattr: Some(setattr_shim::<F>),
        readlink: Some(readlink_shim::<F>),
        mknod: Some(mknod_shim::<F>),
        mkdir: Some(mkdir_shim::<F>),
        unlink: Some(unlink_shim::<F>),
        rmdir: Some(rmdir_shim::<F>),
        symlink: Some(symlink_shim::<F>),
        rename: Some(rename_shim::<F>),
        link: Some(link_shim::<F>),
        open: Some(open_shim::<F>),
        read: Some(read_shim::<F>),
        write: Some(write_shim::<F>),
        flush: Some(flush_shim::<F>),
        release: Some(release_shim::<F>),
        fsync: Some(fsync_shim::<F>),
        opendir: Some(opendir_shim::<F>),
        readdir: Some(readdir_shim::<F>),
        releasedir: Some(releasedir_shim::<F>),
        fsyncdir: Some(fsyncdir_shim::<F>),
        statfs: Some(statfs_shim::<F>),
        setxattr: Some(setxattr_shim::<F>),
        getxattr: Some(getxattr_shim::<F>),
        listxattr: Some(listxattr_shim::<F>),
        removexattr: Some(removexattr_shim::<F>),
        access: Some(access_shim::<F>),
        create: Some(create_shim::<F>),
        forget_multi: Some(forget_multi_shim::<F>),
        fallocate: Some(fallocate_shim::<F>),
        readdirplus: if F::SUPPORTS_READDIRPLUS {
            Some(readdirplus_shim::<F>)
        } else {
            None
        },
        copy_file_range: Some(copy_file_range_shim::<F>),
        lseek: Some(lseek_shim::<F>),
        getlk: if F::SUPPORTS_POSIX_LOCKS {
            Some(getlk_shim::<F>)
        } else {
            None
        },
        setlk: if F::SUPPORTS_POSIX_LOCKS {
            Some(setlk_shim::<F>)
        } else {
            None
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------
// MountOption
// ---------------------------------------------------------------------

/// A libfuse mount option, rendered to a `-o key[=value]` argument.
///
/// Some variants only apply on one platform (noted per-variant); on the
/// others [`MountOption::render`] silently drops them instead of forwarding
/// a word libfuse there would reject outright. This is the "system-specific
/// changes handled in the wrapper" seam: callers building a portable
/// [`MountOption`] list don't need their own `#[cfg(target_os)]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountOption {
    ReadOnly,
    AllowOther,
    /// Allow root to access the mount even without `allow_other`. Unlike
    /// `uid=`/`gid=`, this is recognized by the low-level session API on
    /// both platforms, not just `fuse_main`.
    AllowRoot,
    AutoUnmount,
    DefaultPermissions,
    /// Allow mounting over a non-empty directory. Meaningful on macFUSE;
    /// libfuse3 on Linux removed this option and always allows it.
    NonEmpty,
    FsName(String),
    Subtype(String),
    /// macOS Finder display name for the mounted volume. Linux libfuse has
    /// no equivalent and rejects unknown options, so this is dropped there.
    VolName(String),
    Custom(String),
}

impl MountOption {
    /// Renders this option to its libfuse `-o` word, or `None` if it does
    /// not apply on the current platform.
    fn render(&self) -> Option<String> {
        match self {
            MountOption::ReadOnly => Some("ro".to_string()),
            MountOption::AllowOther => Some("allow_other".to_string()),
            MountOption::AllowRoot => Some("allow_root".to_string()),
            MountOption::AutoUnmount => Some("auto_unmount".to_string()),
            MountOption::DefaultPermissions => Some("default_permissions".to_string()),
            MountOption::NonEmpty => {
                #[cfg(target_os = "macos")]
                {
                    Some("nonempty".to_string())
                }
                #[cfg(not(target_os = "macos"))]
                {
                    None
                }
            }
            MountOption::FsName(name) => Some(format!("fsname={name}")),
            MountOption::Subtype(name) => Some(format!("subtype={name}")),
            MountOption::VolName(name) => {
                #[cfg(target_os = "macos")]
                {
                    Some(format!("volname={name}"))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    None
                }
            }
            MountOption::Custom(opt) => Some(opt.clone()),
        }
    }
}

// ---------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------

/// Errors returned by [`Session`] setup/teardown.
#[derive(Debug)]
pub enum Error {
    SessionNew,
    Mount,
    MountPointNotFound(String),
    MountPointNotDir(String),
    SignalHandlers,
    Loop(i32),
    InvalidMountpoint(String),
    InvalidThreadPool(String),
    LoopConfigAllocation,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::SessionNew => write!(f, "failed to create the FUSE session"),
            Error::Mount => write!(f, "failed to mount the FUSE session"),
            Error::MountPointNotFound(mp) => write!(f, "mount point does not exist: {mp}"),
            Error::MountPointNotDir(mp) => write!(f, "mount point is not a directory: {mp}"),
            Error::SignalHandlers => write!(f, "failed to install FUSE signal handlers"),
            Error::Loop(rc) => write!(f, "FUSE session loop exited with code {rc}"),
            Error::InvalidMountpoint(mp) => {
                write!(f, "invalid mountpoint (contains a NUL byte): {mp:?}")
            }
            Error::InvalidThreadPool(message) => write!(f, "invalid thread pool: {message}"),
            Error::LoopConfigAllocation => write!(f, "failed to allocate libfuse loop config"),
        }
    }
}

impl std::error::Error for Error {}

/// Runtime dispatch configuration for a FUSE session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    pub threading: ThreadingMode,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            threading: ThreadingMode::MultiThreaded(ThreadPoolConfig::default()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadingMode {
    SingleThreaded,
    MultiThreaded(ThreadPoolConfig),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadPoolConfig {
    pub max_threads: NonZeroU32,
    pub max_idle_threads: Option<u32>,
    pub clone_fd: bool,
}

impl Default for ThreadPoolConfig {
    fn default() -> Self {
        Self {
            max_threads: NonZeroU32::new(10).unwrap(),
            max_idle_threads: None,
            clone_fd: false,
        }
    }
}

impl ThreadPoolConfig {
    const MAX_THREADS: u32 = 100_000;
    fn validate(self) -> Result<Self, Error> {
        if self.max_threads.get() > Self::MAX_THREADS {
            return Err(Error::InvalidThreadPool(format!(
                "max_threads exceeds {}",
                Self::MAX_THREADS
            )));
        }
        Ok(self)
    }
}

struct LoopConfig(*mut fuse_loop_config);
impl LoopConfig {
    fn new(pool: ThreadPoolConfig) -> Result<Self, Error> {
        let pool = pool.validate()?;
        let raw = unsafe { loop_cfg_create_312() };
        if raw.is_null() {
            return Err(Error::LoopConfigAllocation);
        }
        unsafe {
            loop_cfg_set_max_threads_312(raw, pool.max_threads.get());
            if let Some(idle) = pool.max_idle_threads {
                loop_cfg_set_idle_threads_312(raw, idle);
            }
            loop_cfg_set_clone_fd_312(raw, pool.clone_fd as u32);
        }
        Ok(Self(raw))
    }
}
impl Drop for LoopConfig {
    fn drop(&mut self) {
        unsafe { loop_cfg_destroy_312(self.0) }
    }
}

// ---------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------

/// A mounted (or mountable) FUSE session driving a single [`NodeFs`]
/// implementation with configurable dispatch. The session is intentionally
/// `!Send`; libfuse worker threads only share its synchronized runtime.
pub struct Session<F: NodeFs> {
    session: *mut fuse_session,
    runtime: *mut Runtime<F>,
    mounted: bool,
    _arg_storage: Vec<CString>,
    config: SessionConfig,
    _not_send: PhantomData<Rc<()>>,
}

impl<F: NodeFs> Session<F> {
    /// Creates a new (not-yet-mounted) session for `fs`, configured with
    /// `options`. The root node is seeded from [`NodeFs::root`].
    pub fn new(fs: F, options: &[MountOption]) -> Result<Self, Error> {
        Self::new_with_config(fs, options, SessionConfig::default())
    }

    pub fn new_with_config(
        fs: F,
        options: &[MountOption],
        config: SessionConfig,
    ) -> Result<Self, Error> {
        if let ThreadingMode::MultiThreaded(pool) = config.threading {
            pool.validate()?;
        }
        let mut arg_strings = vec!["fuse3".to_string()];
        let rendered_options: Vec<String> =
            options.iter().filter_map(MountOption::render).collect();
        if !rendered_options.is_empty() {
            arg_strings.push("-o".to_string());
            arg_strings.push(rendered_options.join(","));
        }
        let arg_cstrings: Vec<CString> = arg_strings
            .into_iter()
            .map(|s| CString::new(s).expect("mount option string contains a NUL byte"))
            .collect();
        let mut argv: Vec<*mut c_char> = arg_cstrings
            .iter()
            .map(|s| s.as_ptr() as *mut c_char)
            .collect();
        let mut args = fuse_args {
            argc: argv.len() as c_int,
            argv: argv.as_mut_ptr(),
            allocated: 0,
        };

        let ops = make_ops::<F>();

        let mut runtime_value = Runtime::new(fs);
        runtime_value
            .set_parallel_dirops(matches!(config.threading, ThreadingMode::MultiThreaded(_)));
        let runtime: *mut Runtime<F> = Box::into_raw(Box::new(runtime_value));

        #[cfg(target_os = "macos")]
        let session = unsafe {
            let mut version = libfuse_version {
                major: FUSE_MAJOR_VERSION as _,
                minor: FUSE_MINOR_VERSION as _,
                hotfix: FUSE_HOTFIX_VERSION as _,
                ..Default::default()
            };
            // The reply/trampoline code always works with vanilla structs.
            version.set_darwin_extensions_enabled(0);
            fuse_session_new_versioned(
                &mut args,
                &ops,
                std::mem::size_of::<fuse_lowlevel_ops>(),
                &mut version,
                runtime as *mut c_void,
            )
        };
        #[cfg(not(target_os = "macos"))]
        let session = unsafe {
            fuse_session_new(
                &mut args,
                &ops,
                std::mem::size_of::<fuse_lowlevel_ops>(),
                runtime as *mut c_void,
            )
        };

        unsafe { fuse_opt_free_args(&mut args) };

        if session.is_null() {
            unsafe { drop(Box::from_raw(runtime)) };
            return Err(Error::SessionNew);
        }

        Ok(Session {
            session,
            runtime,
            mounted: false,
            _arg_storage: arg_cstrings,
            config,
            _not_send: PhantomData,
        })
    }

    /// Mounts the session at `mountpoint`.
    ///
    /// Validates that `mountpoint` exists and is a directory before handing
    /// off to libfuse. On macOS, `fuse_session_mount` dispatches to the
    /// macFUSE mount helper and returns success even when the mount point
    /// does not exist, leaving the session loop blocked forever instead of
    /// reporting the failure — checking here gives every caller a fast,
    /// clear error on both platforms instead.
    pub fn mount(&mut self, mountpoint: &Path) -> Result<(), Error> {
        if !mountpoint.exists() {
            return Err(Error::MountPointNotFound(mountpoint.display().to_string()));
        }
        if !mountpoint.is_dir() {
            return Err(Error::MountPointNotDir(mountpoint.display().to_string()));
        }
        let c_mountpoint = CString::new(mountpoint.as_os_str().as_bytes())
            .map_err(|_| Error::InvalidMountpoint(mountpoint.display().to_string()))?;
        let rc = unsafe { fuse_session_mount(self.session, c_mountpoint.as_ptr()) };
        if rc != 0 {
            return Err(Error::Mount);
        }
        self.mounted = true;
        Ok(())
    }

    /// Runs the configured event loop until unmounted or signalled.
    pub fn run(&mut self) -> Result<(), Error> {
        if unsafe { fuse_set_signal_handlers(self.session) } != 0 {
            return Err(Error::SignalHandlers);
        }
        let rc = match self.config.threading {
            ThreadingMode::SingleThreaded => unsafe { fuse_session_loop(self.session) },
            ThreadingMode::MultiThreaded(pool) => match LoopConfig::new(pool) {
                Ok(config) => unsafe { session_loop_mt_312(self.session, config.0) },
                Err(error) => {
                    unsafe { fuse_remove_signal_handlers(self.session) };
                    return Err(error);
                }
            },
        };
        unsafe { fuse_remove_signal_handlers(self.session) };
        if rc != 0 {
            return Err(Error::Loop(rc));
        }
        Ok(())
    }

    /// Convenience: [`Session::new`] + [`Session::mount`] + [`Session::run`].
    pub fn mount_and_run(fs: F, mountpoint: &Path, options: &[MountOption]) -> Result<(), Error> {
        let mut session = Session::new(fs, options)?;
        session.mount(mountpoint)?;
        session.run()
    }

    pub fn mount_and_run_with_config(
        fs: F,
        mountpoint: &Path,
        options: &[MountOption],
        config: SessionConfig,
    ) -> Result<(), Error> {
        let mut session = Session::new_with_config(fs, options, config)?;
        session.mount(mountpoint)?;
        session.run()
    }
}

impl<F: NodeFs> Drop for Session<F> {
    fn drop(&mut self) {
        unsafe {
            if self.mounted {
                fuse_session_unmount(self.session);
            }
            fuse_session_destroy(self.session);
            drop(Box::from_raw(self.runtime));
        }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use typed_fuse_core::{Caller, LockKind, NodeAttr};

    #[test]
    fn c_names_preserve_non_utf8_bytes() {
        let name = CString::new([0xff, b'x']).unwrap();
        assert_eq!(c_os_str(name.as_ptr()).as_bytes(), &[0xff, b'x']);
    }

    struct NullFs;
    impl NodeFs for NullFs {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(&self, _n: &(), _h: Option<&()>, _c: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }
    }

    #[test]
    fn make_ops_registers_covered_callbacks_and_skips_the_rest() {
        let ops = make_ops::<NullFs>();

        assert!(ops.init.is_some());
        assert!(ops.destroy.is_some());
        assert!(ops.lookup.is_some());
        assert!(ops.forget.is_some());
        assert!(ops.getattr.is_some());
        assert!(ops.setattr.is_some());
        assert!(ops.readlink.is_some());
        assert!(ops.mknod.is_some());
        assert!(ops.mkdir.is_some());
        assert!(ops.unlink.is_some());
        assert!(ops.rmdir.is_some());
        assert!(ops.symlink.is_some());
        assert!(ops.rename.is_some());
        assert!(ops.link.is_some());
        assert!(ops.open.is_some());
        assert!(ops.read.is_some());
        assert!(ops.write.is_some());
        assert!(ops.flush.is_some());
        assert!(ops.release.is_some());
        assert!(ops.fsync.is_some());
        assert!(ops.opendir.is_some());
        assert!(ops.readdir.is_some());
        assert!(ops.releasedir.is_some());
        assert!(ops.fsyncdir.is_some());
        assert!(ops.statfs.is_some());
        assert!(ops.setxattr.is_some());
        assert!(ops.getxattr.is_some());
        assert!(ops.listxattr.is_some());
        assert!(ops.removexattr.is_some());
        assert!(ops.access.is_some());
        assert!(ops.create.is_some());
        assert!(ops.forget_multi.is_some());
        assert!(ops.fallocate.is_some());
        assert!(ops.readdirplus.is_none());
        assert!(ops.copy_file_range.is_some());
        assert!(ops.lseek.is_some());

        assert!(ops.getlk.is_none());
        assert!(ops.setlk.is_none());
        assert!(ops.bmap.is_none());
        assert!(ops.ioctl.is_none());
        assert!(ops.poll.is_none());
        assert!(ops.write_buf.is_none());
        assert!(ops.retrieve_reply.is_none());
        assert!(ops.flock.is_none());
        #[cfg(has_tmpfile_op)]
        assert!(ops.tmpfile.is_none());
        #[cfg(target_os = "macos")]
        {
            assert!(ops.setvolname.is_none());
            assert!(ops.monitor.is_none());
        }
        #[cfg(has_statx_op)]
        assert!(ops.statx.is_none());
    }

    struct OptInFs;
    impl NodeFs for OptInFs {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        const SUPPORTS_POSIX_LOCKS: bool = true;
        const SUPPORTS_READDIRPLUS: bool = true;
        fn root(&mut self) {}
        fn getattr(&self, _: &(), _: Option<&()>, _: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }
    }

    #[test]
    fn optional_callbacks_are_registered_only_on_opt_in() {
        let ops = make_ops::<OptInFs>();
        assert!(ops.readdirplus.is_some());
        assert!(ops.getlk.is_some());
        assert!(ops.setlk.is_some());
    }

    #[test]
    fn file_lock_conversion_preserves_ranges_and_eof() {
        for lock in [
            FileLock {
                kind: LockKind::Read,
                start: 7,
                end: 19,
                pid: 42,
            },
            FileLock {
                kind: LockKind::Unlock,
                start: 20,
                end: u64::MAX,
                pid: 0,
            },
        ] {
            assert_eq!(lock_from_raw(&lock_to_raw(lock).unwrap()).unwrap(), lock);
        }
    }

    #[test]
    fn mount_option_render() {
        assert_eq!(MountOption::ReadOnly.render().unwrap(), "ro");
        assert_eq!(MountOption::AllowOther.render().unwrap(), "allow_other");
        assert_eq!(MountOption::AllowRoot.render().unwrap(), "allow_root");
        assert_eq!(MountOption::AutoUnmount.render().unwrap(), "auto_unmount");
        assert_eq!(
            MountOption::DefaultPermissions.render().unwrap(),
            "default_permissions"
        );
        assert_eq!(
            MountOption::FsName("myfs".to_string()).render().unwrap(),
            "fsname=myfs"
        );
        assert_eq!(
            MountOption::Subtype("fuse.myfs".to_string())
                .render()
                .unwrap(),
            "subtype=fuse.myfs"
        );
        assert_eq!(
            MountOption::Custom("noatime".to_string()).render().unwrap(),
            "noatime"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn mount_option_render_macos_only_options() {
        assert_eq!(MountOption::NonEmpty.render().unwrap(), "nonempty");
        assert_eq!(
            MountOption::VolName("Encrypted Files".to_string())
                .render()
                .unwrap(),
            "volname=Encrypted Files"
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn mount_option_render_drops_macos_only_options() {
        assert_eq!(MountOption::NonEmpty.render(), None);
        assert_eq!(
            MountOption::VolName("Encrypted Files".to_string()).render(),
            None
        );
    }

    #[test]
    fn session_config_defaults_to_libfuse_pool_defaults() {
        assert_eq!(
            SessionConfig::default().threading,
            ThreadingMode::MultiThreaded(ThreadPoolConfig {
                max_threads: NonZeroU32::new(10).unwrap(),
                max_idle_threads: None,
                clone_fd: false,
            })
        );
    }

    #[test]
    fn rejects_pool_above_libfuse_ceiling() {
        let pool = ThreadPoolConfig {
            max_threads: NonZeroU32::new(100_001).unwrap(),
            ..Default::default()
        };
        assert!(matches!(pool.validate(), Err(Error::InvalidThreadPool(_))));
    }
}
