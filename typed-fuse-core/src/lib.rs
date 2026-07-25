//! Backend-neutral, node-tracking core for building FUSE filesystems.
//!
//! This crate owns everything that filesystem authors used to re-implement
//! by hand: inode identity and allocation, node lifetime (lookup / hard-link
//! / open-handle refcounts and deferred deletion), and the file-handle
//! table. Authors implement the [`NodeFs`] trait against their own node and
//! handle payload types; operations receive shared node / handle
//! objects instead of raw inode numbers and integer file handles.
//!
//! It contains no raw C types and does not depend on `libfuse-sys`; the
//! `fuse3` crate bridges this core to libfuse's low-level C API. Because the
//! [`Runtime`] is pure Rust, its identity/refcount/deferred-delete logic can
//! be unit-tested without mounting a filesystem.

mod attr;
mod errno;
mod node_fs;
mod path_fs;
mod runtime;

pub use attr::{FileKind, NodeAttr, SetAttr, StatFs, TimeOrNow};
pub use errno::Errno;
pub use node_fs::{
    Caller, ConnInfo, ConnectionCapability, DirSink, FileLock, LockKind, NodeFs, NodeId, OpenHints,
    Opened, PlusDirSink, XattrReply,
};
pub use path_fs::{
    replay, DirBuffer, DirEntry, PathDirSink, PathFilesystem, PathNode, PathNodeFs, PathPlusDirSink,
};
pub use runtime::{
    Cx, EntryReply, LookupReply, NodeRef, NodeTable, OpenReply, Runtime, RuntimePlusSink,
};
