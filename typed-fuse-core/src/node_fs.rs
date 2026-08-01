//! The [`NodeFs`] trait and the value types it exchanges with the runtime.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::attr::{FileKind, NodeAttr, SetAttr, StatFs};
use crate::errno::Errno;
use crate::runtime::Cx;

/// An opaque handle to a node tracked by the runtime. Numerically equal to
/// the FUSE inode number, but filesystems should treat it as opaque and
/// store these (rather than raw `u64`s) in their own directory structures.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NodeId(u64);

impl NodeId {
    /// The filesystem root (`FUSE_ROOT_ID`).
    pub const ROOT: NodeId = NodeId(1);

    /// The underlying inode number.
    pub fn ino(self) -> u64 {
        self.0
    }

    pub(crate) fn from_ino(ino: u64) -> Self {
        NodeId(ino)
    }
}

/// Credentials of the process that issued the current request.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub umask: u32,
}

/// Connection parameters passed to [`NodeFs::init`], read from (and, for the
/// mutable fields, written back into) the kernel connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnInfo {
    pub proto_major: u32,
    pub proto_minor: u32,
    pub max_write: u32,
    pub max_readahead: u32,
    capable: u32,
    want: u32,
}

/// Kernel/libfuse features that a filesystem may negotiate in [`NodeFs::init`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionCapability {
    AsyncRead,
    AsyncDirectIo,
    ParallelDirectoryOperations,
}

impl ConnInfo {
    const ASYNC_READ: u32 = 1 << 0;
    const ASYNC_DIO: u32 = 1 << 15;
    const PARALLEL_DIROPS: u32 = 1 << 18;

    fn bit(capability: ConnectionCapability) -> u32 {
        match capability {
            ConnectionCapability::AsyncRead => Self::ASYNC_READ,
            ConnectionCapability::AsyncDirectIo => Self::ASYNC_DIO,
            ConnectionCapability::ParallelDirectoryOperations => Self::PARALLEL_DIROPS,
        }
    }

    /// Whether the kernel supports `capability`.
    pub fn capable(&self, capability: ConnectionCapability) -> bool {
        self.capable & Self::bit(capability) != 0
    }

    /// Whether this session currently requests `capability`.
    pub fn enabled(&self, capability: ConnectionCapability) -> bool {
        self.want & Self::bit(capability) != 0
    }

    /// Requests or disables a supported capability. Enabling an unsupported
    /// capability returns `false` and leaves the request mask unchanged.
    pub fn set_enabled(&mut self, capability: ConnectionCapability, enabled: bool) -> bool {
        let bit = Self::bit(capability);
        if enabled && self.capable & bit == 0 {
            return false;
        }
        if enabled {
            self.want |= bit
        } else {
            self.want &= !bit
        }
        true
    }

    #[doc(hidden)]
    pub fn from_raw(
        proto_major: u32,
        proto_minor: u32,
        max_write: u32,
        max_readahead: u32,
        capable: u32,
        want: u32,
    ) -> Self {
        Self {
            proto_major,
            proto_minor,
            max_write,
            max_readahead,
            capable,
            want,
        }
    }

    #[doc(hidden)]
    pub fn want_bits(&self) -> u32 {
        self.want
    }
}

/// Kernel caching hints returned alongside an opened handle.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenHints {
    pub direct_io: bool,
    pub keep_cache: bool,
    pub nonseekable: bool,
    pub cache_readdir: bool,
    pub parallel_direct_writes: bool,
}

/// The result of an `open`/`opendir`/`create`: the filesystem's own handle
/// object plus optional caching hints. The runtime assigns the integer file
/// handle; the filesystem never sees it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opened<H> {
    pub handle: H,
    pub hints: OpenHints,
}

impl<H> Opened<H> {
    pub fn new(handle: H) -> Self {
        Opened {
            handle,
            hints: OpenHints::default(),
        }
    }

    pub fn direct_io(mut self, value: bool) -> Self {
        self.hints.direct_io = value;
        self
    }

    pub fn keep_cache(mut self, value: bool) -> Self {
        self.hints.keep_cache = value;
        self
    }

    pub fn nonseekable(mut self, value: bool) -> Self {
        self.hints.nonseekable = value;
        self
    }

    pub fn cache_readdir(mut self, value: bool) -> Self {
        self.hints.cache_readdir = value;
        self
    }

    /// Allows direct writes issued through this open handle to overlap.
    pub fn parallel_direct_writes(mut self, value: bool) -> Self {
        self.hints.parallel_direct_writes = value;
        self
    }
}

/// The reply to `getxattr`/`listxattr`. The kernel's xattr protocol is
/// two-phase (`size == 0` asks only for the length); [`XattrReply::Size`]
/// answers the length without materializing the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XattrReply {
    Size(usize),
    Data(Vec<u8>),
}

impl From<Vec<u8>> for XattrReply {
    fn from(data: Vec<u8>) -> Self {
        XattrReply::Data(data)
    }
}

impl XattrReply {
    /// Applies the kernel's getxattr/listxattr size protocol to a fully
    /// materialized value: `size == 0` probes the length; otherwise the
    /// value must fit in the caller's buffer or the request fails with
    /// `ERANGE`.
    pub fn sized(data: Vec<u8>, size: usize) -> Result<XattrReply, Errno> {
        if size == 0 {
            Ok(XattrReply::Size(data.len()))
        } else if data.len() > size {
            Err(Errno::ERANGE)
        } else {
            Ok(XattrReply::Data(data))
        }
    }
}

/// A sink [`NodeFs::readdir`] pushes directory entries into. The runtime
/// (via `fuse3`) implements this over libfuse's size-limited buffer
/// protocol.
pub trait DirSink {
    /// Adds one entry. `next_offset` is the resume cookie the kernel will
    /// hand back to continue after this entry. Returns `false` once the
    /// buffer is full, at which point the caller must stop iterating.
    fn add(&mut self, name: &OsStr, id: NodeId, kind: FileKind, next_offset: u64) -> bool;
}

/// A sink [`NodeFs::readdirplus`] pushes directory entries (with attributes)
/// into. Like [`DirSink`], but each entry carries a [`NodeAttr`] so the
/// kernel can populate its attribute cache and skip a follow-up `lookup`.
pub trait PlusDirSink {
    /// Adds one entry with its attributes. `next_offset` is the resume
    /// cookie the kernel will hand back to continue after this entry.
    /// Returns `false` once the buffer is full, at which point the caller
    /// must stop iterating.
    fn add(&mut self, name: &OsStr, id: NodeId, attr: NodeAttr, next_offset: u64) -> bool;
}

/// The kind of a POSIX record lock, as used by [`NodeFs::getlk`] and
/// [`NodeFs::setlk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    Read,
    Write,
    /// Releases a previously held lock over the range.
    Unlock,
}

/// A POSIX record lock request or reply, covering the byte range
/// `[start, end]` inclusive (`end == u64::MAX` means "to end of file").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileLock {
    pub kind: LockKind,
    pub start: u64,
    pub end: u64,
    /// The process id holding or requesting the lock.
    pub pid: u32,
}

/// The safe, node-based FUSE filesystem interface.
///
/// The runtime owns node identity, lifetime (lookup/link/open refcounts and
/// deferred deletion), and the file-handle table. Filesystem authors work
/// with their own [`NodeFs::Node`] and [`NodeFs::Handle`] payloads:
///
/// * Operations on a single existing node receive `&Self::Node` directly
///   (the runtime resolves it, replying `ENOENT` if it is gone).
/// * I/O operations additionally receive `&Self::Handle`.
/// * Structural / naming operations receive a [`Cx`] to resolve, insert, and
///   link other nodes.
///
/// Every method has a default (fallible ones default to `Err(ENOSYS)`;
/// no-op-style ones to `Ok`), so a filesystem overrides only what it
/// supports. Request callbacks take `&self` and may overlap, including on the
/// same node and handle. Implementations choose their own interior locking.
#[allow(unused_variables)]
pub trait NodeFs: Sized + Send + Sync {
    /// Per-node data owned and stored by the runtime.
    type Node: Send + Sync;
    /// Per-open-file data. Use `()` if the filesystem is stateless per open.
    type Handle: Send + Sync;
    /// Per-open-directory data. Use `()` if not needed.
    type DirHandle: Send + Sync;

    /// Set to `true` to register the `getlk`/`setlk` callbacks and enable
    /// [`NodeFs::getlk`]/[`NodeFs::setlk`]. Left disabled by default since
    /// most filesystems delegate POSIX locking to the kernel.
    const SUPPORTS_POSIX_LOCKS: bool = false;
    /// Set to `true` to register the `flock` callback and enable
    /// [`NodeFs::flock`]. Left disabled by default since most filesystems
    /// delegate BSD locking to the kernel.
    const SUPPORTS_FLOCK: bool = false;
    /// Set to `true` to register the `readdirplus` callback and enable
    /// [`NodeFs::readdirplus`], letting the kernel populate its attribute
    /// cache from directory listings instead of a follow-up `lookup` per
    /// entry.
    const SUPPORTS_READDIRPLUS: bool = false;

    /// Builds the payload for the root directory. Called once, when the
    /// runtime is constructed.
    fn root(&mut self) -> Self::Node;

    /// Populates the initial tree beneath the (already-inserted) root, for
    /// filesystems with statically-known contents. Called once, right after
    /// [`NodeFs::root`], with a [`Cx`] so children can be inserted and
    /// recorded. Default: no-op (an empty root).
    fn populate(&mut self, cx: &Cx<'_, Self::Node>) {}

    /// Called once when libfuse establishes communication with the kernel.
    fn init(&self, conn: &mut ConnInfo) {}

    /// Called on filesystem exit.
    fn destroy(&self) {}

    /// Called when the kernel's lookup count for `node` reaches zero.
    ///
    /// This notification is independent of the node's link and open-handle
    /// counts: the runtime may retain the node after this callback returns.
    /// It has no result because FUSE forget requests do not receive replies.
    fn forget(&self, node: &Self::Node) {}

    /// Returns the attributes of `node`, including its current link count.
    /// `handle` is the open file/dir handle if the call arrived through one,
    /// or `None` (e.g. an `fstat` on a path rather than a descriptor).
    fn getattr(
        &self,
        node: &Self::Node,
        handle: Option<&Self::Handle>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno>;

    /// Returns attributes for a newly looked-up or created entry.
    ///
    /// The default obtains the attributes normally. Adapters which already
    /// received attributes while discovering the node can override this to
    /// avoid issuing an immediate, duplicate `getattr` request.
    fn entry_attr(&self, node: &Self::Node, caller: &Caller) -> Result<NodeAttr, Errno> {
        self.getattr(node, None, caller)
    }

    /// Applies the `Some` fields of `set` to `node`, returning the resulting
    /// attributes. `handle` is the open handle if the call arrived through
    /// one, or `None` otherwise.
    fn setattr(
        &self,
        node: &Self::Node,
        handle: Option<&Self::Handle>,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Looks up `name` in directory `parent`. Return `Ok(Some(id))` for a
    /// hit, `Ok(None)` to populate the kernel's negative-lookup cache, or
    /// `Err` for a hard failure.
    fn lookup(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads the target of the symbolic link `node`.
    fn readlink(&self, node: &Self::Node, caller: &Caller) -> Result<PathBuf, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a non-directory, non-symlink node named `name` in `parent`.
    /// Insert it via `cx.insert(..)`, record its [`NodeId`] in the parent,
    /// and return the id.
    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a directory named `name` in `parent`.
    fn mkdir(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a symbolic link named `name` in `parent` pointing at `target`.
    fn symlink(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        target: &Path,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the non-directory entry `name` from `parent`. Remove the name
    /// from the parent and call `cx.remove_link(id)`; the runtime frees the
    /// node once it is also un-looked-up and closed.
    fn unlink(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the empty directory `name` from `parent`.
    fn rmdir(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Renames `name` in `parent` to `newname` in `newparent`.
    #[allow(clippy::too_many_arguments)]
    fn rename(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        newparent: NodeId,
        newname: &OsStr,
        flags: u32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a hard link to `id` named `newname` in `newparent`. Record
    /// the name and call `cx.add_link(id)`.
    fn link(
        &self,
        cx: &Cx<'_, Self::Node>,
        id: NodeId,
        newparent: NodeId,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Opens `node`, returning the filesystem's handle object.
    fn open(
        &self,
        node: &Self::Node,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::Handle>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads up to `size` bytes from `node` at `offset`. The returned data
    /// may borrow from `self`/`node`/`handle` for a zero-copy reply (they all
    /// share the lifetime `'a`).
    fn read<'a>(
        &'a self,
        node: &'a Self::Node,
        handle: &'a Self::Handle,
        offset: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Writes `data` to `node` at `offset`, returning the count written.
    fn write(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Called on each `close()` of an open file.
    fn flush(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Called when the last reference to an open file is dropped; consumes
    /// the handle.
    fn release(
        &self,
        node: &Self::Node,
        handle: Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes file contents (and metadata unless `datasync`).
    fn fsync(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Opens the directory `node`.
    fn opendir(
        &self,
        node: &Self::Node,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::DirHandle>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Emits the entries of directory `node` into `sink`, starting at
    /// `offset`. `this`/`parent` are provided so the filesystem can emit
    /// `.` and `..` correctly. Push each entry with a strictly increasing
    /// `next_offset` cookie and stop as soon as [`DirSink::add`] returns
    /// `false`.
    #[allow(clippy::too_many_arguments)]
    fn readdir(
        &self,
        cx: &Cx<'_, Self::Node>,
        node: &Self::Node,
        this: NodeId,
        parent: NodeId,
        handle: &Self::DirHandle,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Like [`NodeFs::readdir`], but emits attributes alongside each entry
    /// via [`PlusDirSink`]. Only called when [`NodeFs::SUPPORTS_READDIRPLUS`]
    /// is `true`.
    #[allow(clippy::too_many_arguments)]
    fn readdirplus(
        &self,
        cx: &Cx<'_, Self::Node>,
        node: &Self::Node,
        this: NodeId,
        parent: NodeId,
        handle: &Self::DirHandle,
        offset: u64,
        sink: &mut dyn PlusDirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Releases a directory handle; consumes it.
    fn releasedir(
        &self,
        node: &Self::Node,
        handle: Self::DirHandle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes directory contents.
    fn fsyncdir(
        &self,
        node: &Self::Node,
        handle: &Self::DirHandle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Returns filesystem-wide statistics. Defaults to a minimal-but-valid
    /// value (as libfuse does when the callback is unset).
    fn statfs(&self, caller: &Caller) -> Result<StatFs, Errno> {
        Ok(StatFs {
            bsize: 512,
            namelen: 255,
            ..Default::default()
        })
    }

    /// Sets extended attribute `name` on `node`.
    fn setxattr(
        &self,
        node: &Self::Node,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns extended attribute `name` on `node`. A `size` of zero is a
    /// length query (return [`XattrReply::Size`]).
    fn getxattr(
        &self,
        node: &Self::Node,
        name: &OsStr,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns the NUL-separated extended attribute names on `node`. Same
    /// size-query protocol as [`NodeFs::getxattr`].
    fn listxattr(
        &self,
        node: &Self::Node,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes extended attribute `name` from `node`.
    fn removexattr(&self, node: &Self::Node, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Checks access to `node` per the `access(2)` `mask`.
    fn access(&self, node: &Self::Node, mask: i32, caller: &Caller) -> Result<(), Errno> {
        Ok(())
    }

    /// Atomically creates and opens a regular file named `name` in `parent`.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        cx: &Cx<'_, Self::Node>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(NodeId, Opened<Self::Handle>), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Pre-allocates/punches `length` bytes at `offset` in `node`.
    fn fallocate(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Finds the next data region or hole at or after `offset`.
    fn lseek(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Tests whether `lock` could be acquired on `node` by `owner`, returning
    /// the conflicting lock (or `lock` itself with [`LockKind::Unlock`] if it
    /// would succeed). Only called when [`NodeFs::SUPPORTS_POSIX_LOCKS`] is
    /// `true`.
    fn getlk(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        owner: u64,
        lock: FileLock,
        caller: &Caller,
    ) -> Result<FileLock, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Acquires or releases `lock` on `node` for `owner`. If `sleep` is
    /// `true`, block until the lock is available rather than failing
    /// immediately. Only called when [`NodeFs::SUPPORTS_POSIX_LOCKS`] is
    /// `true`.
    fn setlk(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        owner: u64,
        lock: FileLock,
        sleep: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Acquires, modifies, or releases a BSD lock on `handle`. `operation` is
    /// the platform's `LOCK_SH`, `LOCK_EX`, or `LOCK_UN`, optionally combined
    /// with `LOCK_NB`. Only called when [`NodeFs::SUPPORTS_FLOCK`] is `true`.
    fn flock(
        &self,
        node: &Self::Node,
        handle: &Self::Handle,
        operation: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Copies `len` bytes between two nodes (resolvable via `cx`).
    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        cx: &Cx<'_, Self::Node>,
        id_in: NodeId,
        off_in: u64,
        id_out: NodeId,
        off_out: u64,
        len: u64,
        flags: i32,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        Err(Errno::ENOSYS)
    }
}
