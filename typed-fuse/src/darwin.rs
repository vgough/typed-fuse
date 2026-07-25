//! macOS-only workaround for the "Darwin extensions" symbol aliasing.
//!
//! On macOS, libfuse's headers declare `fuse_reply_entry()`, `fuse_reply_attr()`,
//! `fuse_reply_create()`, `fuse_reply_statfs()`, `fuse_add_direntry()` and
//! `fuse_add_direntry_plus()` with Darwin extensions enabled by default. That
//! aliases the symbols (via an asm symbol rename) to `<name>$DARWIN` and
//! switches their argument structs to Darwin-extended layouts
//! (`fuse_darwin_attr` / `fuse_darwin_entry_param`). The full list of
//! `$DARWIN`-aliased functions was confirmed by grepping the generated
//! bindings for `link_name`; those six are all of them.
//!
//! This crate keeps Darwin extensions disabled (`set_darwin_extensions_enabled(0)`,
//! done in `session.rs`) and only ever works with the portable vanilla structs
//! (`stat` / `fuse_entry_param` / `statfs`), so it must bind directly to the
//! plain symbol names instead of going through libfuse-sys's aliased
//! declarations.
//!
//! See the root crate's `examples/hello_ll.rs` (lines 44-65) for the original
//! version of this workaround, and the `darwin-symbol-aliasing` memory note.

use libfuse_sys::fuse_lowlevel::{
    fuse_entry_param, fuse_file_info, fuse_req_t, off_t, stat, statfs,
};
use std::os::raw::{c_char, c_int};

type Attr = stat;
type EntryParam = fuse_entry_param;

extern "C" {
    #[link_name = "fuse_reply_entry"]
    pub(crate) fn fuse_reply_entry_vanilla(req: fuse_req_t, e: *const EntryParam) -> c_int;

    #[link_name = "fuse_reply_attr"]
    pub(crate) fn fuse_reply_attr_vanilla(
        req: fuse_req_t,
        attr: *const Attr,
        attr_timeout: f64,
    ) -> c_int;

    #[link_name = "fuse_reply_create"]
    pub(crate) fn fuse_reply_create_vanilla(
        req: fuse_req_t,
        e: *const EntryParam,
        fi: *const fuse_file_info,
    ) -> c_int;

    #[link_name = "fuse_reply_statfs"]
    pub(crate) fn fuse_reply_statfs_vanilla(req: fuse_req_t, stbuf: *const statfs) -> c_int;

    #[link_name = "fuse_add_direntry"]
    pub(crate) fn fuse_add_direntry_vanilla(
        req: fuse_req_t,
        buf: *mut c_char,
        bufsize: usize,
        name: *const c_char,
        stbuf: *const Attr,
        off: off_t,
    ) -> usize;

    #[link_name = "fuse_add_direntry_plus"]
    pub(crate) fn fuse_add_direntry_plus_vanilla(
        req: fuse_req_t,
        buf: *mut c_char,
        bufsize: usize,
        name: *const c_char,
        e: *const EntryParam,
        off: off_t,
    ) -> usize;
}
