//! A safe, node-based FUSE wrapper over the raw low-level bindings in
//! `libfuse_sys::fuse_lowlevel`.
//!
//! Filesystem authors implement the [`NodeFs`] trait (re-exported from
//! [`typed_fuse_core`]) against their own node and handle types, and drive it
//! with [`Session`]. The base layer owns inode identity, node lifetime
//! (lookup/link/open refcounts and deferred deletion), and file handles, so
//! filesystems never manage inode numbers or integer file handles
//! themselves.
//!
//! This crate is the FFI bridge: it decodes raw C callbacks into the
//! backend-neutral types of `typed_fuse_core`, drives the
//! [`Runtime`](typed_fuse_core::Runtime), and encodes results back into
//! `fuse_reply_*`. All per-OS layout handling lives in the `conv` module;
//! all `unsafe` lives here.
//!
//! Filesystems that pass POSIX record locks through to a backing file
//! descriptor can implement [`NodeFs::getlk`] and [`NodeFs::setlk`] with the
//! helpers in [`file_lock`], which also owns the `libc::flock` conversions
//! and the per-OS widths of the `F_*LCK` constants.

mod conv;
mod ffi;
mod session;

pub mod file_lock;
pub mod mount;
pub mod passthrough;

#[cfg(target_os = "macos")]
mod darwin;

pub use session::{Error, MountOption, Session, SessionConfig, ThreadPoolConfig, ThreadingMode};

// The user-facing API is the node-based core.
pub use typed_fuse_core::{
    replay, Caller, ConnInfo, ConnectionCapability, Cx, DirBuffer, DirEntry, DirSink, Errno,
    FileKind, FileLock, LockKind, NodeAttr, NodeFs, NodeId, NodeRef, OpenHints, Opened,
    PathDirSink, PathFilesystem, PathNode, PathNodeFs, PathPlusDirSink, PlusDirSink, SetAttr,
    StatFs, TimeOrNow, XattrReply,
};
