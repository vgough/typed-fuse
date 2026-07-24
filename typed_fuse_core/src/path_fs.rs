//! A synchronous path-based filesystem interface and its [`NodeFs`] adapter.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::{
    Caller, ConnInfo, Cx, DirSink, Errno, FileKind, FileLock, NodeAttr, NodeFs, NodeId, Opened,
    PlusDirSink, SetAttr, StatFs, XattrReply,
};

fn mutex<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_lock(value: &RwLock<()>) -> RwLockReadGuard<'_, ()> {
    value
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock(value: &RwLock<()>) -> RwLockWriteGuard<'_, ()> {
    value
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A directory-entry sink used by [`PathFilesystem::readdir`].
pub trait PathDirSink {
    /// Adds one entry. `next_offset` is the resume cookie the kernel will
    /// hand back to continue after this entry. Returns `false` once the
    /// buffer is full, at which point the caller must stop iterating.
    fn add(&mut self, name: &OsStr, kind: FileKind, next_offset: u64) -> bool;
}

/// A directory-entry sink used by [`PathFilesystem::readdirplus`].
pub trait PathPlusDirSink {
    /// Adds one entry with its attributes. `next_offset` is the resume
    /// cookie the kernel will hand back to continue after this entry.
    /// Returns `false` once the buffer is full, at which point the caller
    /// must stop iterating.
    fn add(&mut self, name: &OsStr, attr: NodeAttr, next_offset: u64) -> bool;
}

/// One entry captured by [`DirBuffer`].
#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: OsString,
    pub kind: FileKind,
    pub attr: NodeAttr,
}

/// An immutable snapshot of a directory's contents, suitable for use as a
/// [`PathFilesystem::DirHandle`].
///
/// Filesystems typically build one in `opendir` (so all readers of that
/// handle see a consistent view even if the directory mutates afterward)
/// and replay it into the kernel's sink from `readdir`/`readdirplus` via
/// [`DirBuffer::fill`]/[`DirBuffer::fill_plus`], which own the resume-offset
/// bookkeeping (`next_offset = index + 1`) that the FUSE protocol requires
/// filesystems to get right themselves.
#[derive(Clone, Debug, Default)]
pub struct DirBuffer {
    entries: Vec<DirEntry>,
}

impl DirBuffer {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Appends one entry.
    pub fn push(&mut self, name: impl Into<OsString>, kind: FileKind, attr: NodeAttr) {
        self.entries.push(DirEntry {
            name: name.into(),
            kind,
            attr,
        });
    }

    /// Appends the conventional `.` and `..` entries with the given
    /// attributes (self and parent respectively). `read_dir`-style
    /// directory listings omit these, so callers building a buffer from one
    /// typically call this first.
    pub fn push_dots(&mut self, self_attr: NodeAttr, parent_attr: NodeAttr) {
        self.push(".", FileKind::Directory, self_attr);
        self.push("..", FileKind::Directory, parent_attr);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DirEntry> {
        self.entries.iter()
    }

    /// Replays entries from `offset` onward into `sink`, per
    /// [`PathFilesystem::readdir`] semantics.
    pub fn fill(&self, offset: u64, sink: &mut dyn PathDirSink) {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (index, entry) in self.entries.iter().enumerate().skip(start) {
            if !sink.add(&entry.name, entry.kind, index as u64 + 1) {
                break;
            }
        }
    }

    /// Replays entries from `offset` onward into `sink`, per
    /// [`PathFilesystem::readdirplus`] semantics.
    pub fn fill_plus(&self, offset: u64, sink: &mut dyn PathPlusDirSink) {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        for (index, entry) in self.entries.iter().enumerate().skip(start) {
            if !sink.add(&entry.name, entry.attr, index as u64 + 1) {
                break;
            }
        }
    }
}

/// A synchronous path-based filesystem interface.
///
/// Paths are absolute virtual paths rooted at `/`. An optional path is absent
/// after the last known name of an open file has been unlinked; the typed
/// handle remains available so backing filesystems can continue operating on
/// the open object.
#[allow(unused_variables)]
pub trait PathFilesystem: Send + Sync + Sized {
    /// Per-open-file data. Use `()` if the filesystem is stateless per open.
    type Handle: Send + Sync;
    /// Per-open-directory data. Use `()` if not needed.
    type DirHandle: Send + Sync;

    /// Set to `true` to enable [`PathFilesystem::getlk`]/[`PathFilesystem::setlk`].
    /// Left disabled by default since most filesystems delegate POSIX
    /// locking to the kernel.
    const SUPPORTS_POSIX_LOCKS: bool = false;
    /// Set to `true` to enable [`PathFilesystem::readdirplus`], letting the
    /// kernel populate its attribute cache from directory listings instead
    /// of a follow-up `lookup` per entry.
    const SUPPORTS_READDIRPLUS: bool = false;

    /// Called once when libfuse establishes communication with the kernel.
    fn init(&self, conn: &mut ConnInfo) {}
    /// Called on filesystem exit.
    fn destroy(&self) {}

    /// Looks up `name` in directory `parent`. Return `Ok(Some(attr))` for a
    /// hit, `Ok(None)` to populate the kernel's negative-lookup cache, or
    /// `Err` for a hard failure.
    fn lookup(
        &self,
        parent: &Path,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<Option<NodeAttr>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns the attributes of `path`, including its current link count.
    /// `handle` is the open handle if the call arrived through one; `path`
    /// is `None` if the open file's last known name has been unlinked.
    fn getattr(
        &self,
        path: Option<&Path>,
        handle: Option<&Self::Handle>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno>;

    /// Applies the `Some` fields of `set` to `path`, returning the resulting
    /// attributes.
    fn setattr(
        &self,
        path: Option<&Path>,
        handle: Option<&Self::Handle>,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads the target of the symbolic link at `path`.
    fn readlink(&self, path: &Path, caller: &Caller) -> Result<PathBuf, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a non-directory, non-symlink node named `name` in `parent`.
    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &self,
        parent: &Path,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a directory named `name` in `parent`.
    fn mkdir(
        &self,
        parent: &Path,
        name: &OsStr,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a symbolic link named `name` in `parent` pointing at `target`.
    fn symlink(
        &self,
        parent: &Path,
        name: &OsStr,
        target: &Path,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the non-directory entry `name` from `parent`.
    fn unlink(&self, parent: &Path, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the empty directory `name` from `parent`.
    fn rmdir(&self, parent: &Path, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Renames `name` in `parent` to `newname` in `newparent`.
    fn rename(
        &self,
        parent: &Path,
        name: &OsStr,
        newparent: &Path,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a hard link to `path` named `newname` in `newparent`.
    fn link(
        &self,
        path: &Path,
        newparent: &Path,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Opens `path`, returning the filesystem's handle object.
    fn open(
        &self,
        path: &Path,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::Handle>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads up to `size` bytes from `handle` at `offset`. The returned data
    /// may borrow from `self`/`handle` for a zero-copy reply. `path` is
    /// `None` if the open file's last known name has been unlinked.
    fn read<'a>(
        &'a self,
        path: Option<&Path>,
        handle: &'a Self::Handle,
        offset: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Writes `data` to `handle` at `offset`, returning the count written.
    fn write(
        &self,
        path: Option<&Path>,
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
        path: Option<&Path>,
        handle: &Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Called when the last reference to an open file is dropped; consumes
    /// the handle.
    fn release(
        &self,
        path: Option<&Path>,
        handle: Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes file contents (and metadata unless `datasync`).
    fn fsync(
        &self,
        path: Option<&Path>,
        handle: &Self::Handle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Opens the directory at `path`.
    fn opendir(
        &self,
        path: &Path,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::DirHandle>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Emits the entries of directory `path` into `sink`, starting at
    /// `offset`. Push each entry with a strictly increasing `next_offset`
    /// cookie and stop as soon as [`PathDirSink::add`] returns `false`.
    fn readdir(
        &self,
        path: &Path,
        handle: &Self::DirHandle,
        offset: u64,
        sink: &mut dyn PathDirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Like [`PathFilesystem::readdir`], but emits attributes alongside each
    /// entry via [`PathPlusDirSink`]. Only called when
    /// [`PathFilesystem::SUPPORTS_READDIRPLUS`] is `true`.
    fn readdirplus(
        &self,
        path: &Path,
        handle: &Self::DirHandle,
        offset: u64,
        sink: &mut dyn PathPlusDirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Releases a directory handle; consumes it.
    fn releasedir(
        &self,
        path: Option<&Path>,
        handle: Self::DirHandle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes directory contents.
    fn fsyncdir(
        &self,
        path: Option<&Path>,
        handle: &Self::DirHandle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Returns filesystem-wide statistics. Defaults to a minimal-but-valid
    /// value (as libfuse does when the callback is unset).
    fn statfs(&self, path: &Path, caller: &Caller) -> Result<StatFs, Errno> {
        Ok(StatFs {
            bsize: 512,
            namelen: 255,
            ..Default::default()
        })
    }

    /// Sets extended attribute `name` on `path`.
    fn setxattr(
        &self,
        path: &Path,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }
    /// Returns extended attribute `name` on `path`. A `size` of zero is a
    /// length query (return [`XattrReply::Size`]).
    fn getxattr(
        &self,
        path: &Path,
        name: &OsStr,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }
    /// Returns the NUL-separated extended attribute names on `path`. Same
    /// size-query protocol as [`PathFilesystem::getxattr`].
    fn listxattr(&self, path: &Path, size: usize, caller: &Caller) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }
    /// Removes extended attribute `name` from `path`.
    fn removexattr(&self, path: &Path, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }
    /// Checks access to `path` per the `access(2)` `mask`.
    fn access(&self, path: &Path, mask: i32, caller: &Caller) -> Result<(), Errno> {
        Ok(())
    }

    /// Atomically creates and opens a regular file named `name` in `parent`.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        parent: &Path,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(NodeAttr, Opened<Self::Handle>), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Pre-allocates/punches `length` bytes at `offset` in `handle`.
    fn fallocate(
        &self,
        path: Option<&Path>,
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
        path: Option<&Path>,
        handle: &Self::Handle,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        Err(Errno::ENOSYS)
    }
    /// Tests whether `lock` could be acquired on `handle` by `owner`,
    /// returning the conflicting lock (or `lock` itself with
    /// [`crate::LockKind::Unlock`] if it would succeed). Only called when
    /// [`PathFilesystem::SUPPORTS_POSIX_LOCKS`] is `true`.
    fn getlk(
        &self,
        path: Option<&Path>,
        handle: &Self::Handle,
        owner: u64,
        lock: FileLock,
        caller: &Caller,
    ) -> Result<FileLock, Errno> {
        Err(Errno::ENOSYS)
    }
    /// Acquires or releases `lock` on `handle` for `owner`. If `sleep` is
    /// `true`, block until the lock is available rather than failing
    /// immediately. Only called when
    /// [`PathFilesystem::SUPPORTS_POSIX_LOCKS`] is `true`.
    fn setlk(
        &self,
        path: Option<&Path>,
        handle: &Self::Handle,
        owner: u64,
        lock: FileLock,
        sleep: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }
}

/// The [`NodeFs::Node`] payload used by [`PathNodeFs`]. Opaque; filesystems
/// interact with [`PathFilesystem`] purely in terms of paths.
#[derive(Debug)]
pub struct PathNode {
    key: u64,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Dentry {
    parent: NodeId,
    name: OsString,
}

struct Namespace {
    by_name: BTreeMap<Dentry, NodeId>,
    aliases: BTreeMap<NodeId, BTreeSet<Dentry>>,
    keys: BTreeMap<u64, NodeId>,
    next_key: u64,
}

impl Namespace {
    fn new() -> Self {
        let mut keys = BTreeMap::new();
        keys.insert(1, NodeId::ROOT);
        Self {
            by_name: BTreeMap::new(),
            aliases: BTreeMap::new(),
            keys,
            next_key: 2,
        }
    }

    fn id_for_node(&self, node: &PathNode) -> Option<NodeId> {
        self.keys.get(&node.key).copied()
    }

    fn path_for(&self, id: NodeId) -> Option<PathBuf> {
        if id == NodeId::ROOT {
            return Some(PathBuf::from("/"));
        }
        let alias = self.aliases.get(&id)?.iter().next()?;
        let mut parent = self.path_for(alias.parent)?;
        parent.push(&alias.name);
        Some(parent)
    }

    fn insert(&mut self, cx: &Cx<'_, PathNode>, parent: NodeId, name: &OsStr) -> (NodeId, bool) {
        let dentry = Dentry {
            parent,
            name: name.to_os_string(),
        };
        if let Some(id) = self.by_name.get(&dentry) {
            return (*id, false);
        }
        let key = self.next_key;
        self.next_key = self
            .next_key
            .checked_add(1)
            .expect("path node key overflow");
        let id = cx.insert(PathNode { key }, parent);
        self.keys.insert(key, id);
        self.by_name.insert(dentry.clone(), id);
        self.aliases.entry(id).or_default().insert(dentry);
        (id, true)
    }

    fn remove(&mut self, dentry: &Dentry) -> Option<NodeId> {
        let id = self.by_name.remove(dentry)?;
        if let Some(aliases) = self.aliases.get_mut(&id) {
            aliases.remove(dentry);
            if aliases.is_empty() {
                self.aliases.remove(&id);
            }
        }
        Some(id)
    }

    fn add_alias(&mut self, id: NodeId, dentry: Dentry) {
        self.by_name.insert(dentry.clone(), id);
        self.aliases.entry(id).or_default().insert(dentry);
    }
}

/// Adapts a [`PathFilesystem`] to the node-based runtime.
pub struct PathNodeFs<P> {
    inner: P,
    operations: RwLock<()>,
    namespace: Mutex<Namespace>,
}

impl<P> PathNodeFs<P> {
    /// Wraps `inner` for use with the node-based [`Runtime`](crate::Runtime).
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            operations: RwLock::new(()),
            namespace: Mutex::new(Namespace::new()),
        }
    }

    /// Unwraps the adapter, discarding the path namespace it built up.
    pub fn into_inner(self) -> P {
        self.inner
    }

    fn node_id(&self, node: &PathNode) -> Option<NodeId> {
        mutex(&self.namespace).id_for_node(node)
    }
    fn node_path(&self, node: &PathNode) -> Option<PathBuf> {
        self.node_id(node)
            .and_then(|id| mutex(&self.namespace).path_for(id))
    }
    fn path(&self, id: NodeId) -> Result<PathBuf, Errno> {
        mutex(&self.namespace).path_for(id).ok_or(Errno::ENOENT)
    }
    fn add_node(&self, cx: &Cx<'_, PathNode>, parent: NodeId, name: &OsStr) -> NodeId {
        mutex(&self.namespace).insert(cx, parent, name).0
    }

    fn add_enumerated_node(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
    ) -> (NodeId, Option<Dentry>) {
        let (id, inserted) = mutex(&self.namespace).insert(cx, parent, name);
        let dentry = inserted.then(|| Dentry {
            parent,
            name: name.to_os_string(),
        });
        (id, dentry)
    }

    fn rollback_dentry(&self, cx: &Cx<'_, PathNode>, dentry: &Dentry) {
        if let Some(id) = mutex(&self.namespace).remove(dentry) {
            cx.remove_link(id);
        }
    }
}

impl<P: PathFilesystem> NodeFs for PathNodeFs<P> {
    type Node = PathNode;
    type Handle = P::Handle;
    type DirHandle = P::DirHandle;

    const SUPPORTS_POSIX_LOCKS: bool = P::SUPPORTS_POSIX_LOCKS;
    const SUPPORTS_READDIRPLUS: bool = P::SUPPORTS_READDIRPLUS;

    fn root(&mut self) -> Self::Node {
        PathNode { key: 1 }
    }
    fn init(&self, conn: &mut ConnInfo) {
        self.inner.init(conn)
    }
    fn destroy(&self) {
        self.inner.destroy()
    }

    fn getattr(
        &self,
        node: &PathNode,
        handle: Option<&P::Handle>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.getattr(path.as_deref(), handle, caller)
    }

    fn setattr(
        &self,
        node: &PathNode,
        handle: Option<&P::Handle>,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.setattr(path.as_deref(), handle, set, caller)
    }

    fn lookup(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        let _guard = read_lock(&self.operations);
        let parent_path = self.path(parent)?;
        if self.inner.lookup(&parent_path, name, caller)?.is_none() {
            return Ok(None);
        }
        Ok(Some(self.add_node(cx, parent, name)))
    }

    fn readlink(&self, node: &PathNode, caller: &Caller) -> Result<PathBuf, Errno> {
        let _guard = read_lock(&self.operations);
        self.inner
            .readlink(&self.node_path(node).ok_or(Errno::ENOENT)?, caller)
    }

    fn mknod(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        self.inner
            .mknod(&self.path(parent)?, name, mode, rdev, umask, caller)?;
        Ok(self.add_node(cx, parent, name))
    }

    fn mkdir(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        self.inner
            .mkdir(&self.path(parent)?, name, mode, umask, caller)?;
        Ok(self.add_node(cx, parent, name))
    }

    fn symlink(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        target: &Path,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        self.inner
            .symlink(&self.path(parent)?, name, target, caller)?;
        Ok(self.add_node(cx, parent, name))
    }

    fn unlink(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        self.remove_entry(cx, parent, name, caller, false)
    }

    fn rmdir(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        self.remove_entry(cx, parent, name, caller, true)
    }

    fn rename(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        newparent: NodeId,
        newname: &OsStr,
        flags: u32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        if flags != 0 {
            return Err(Errno::EOPNOTSUPP);
        }
        let _guard = write_lock(&self.operations);
        let old_parent_path = self.path(parent)?;
        let new_parent_path = self.path(newparent)?;
        self.inner
            .rename(&old_parent_path, name, &new_parent_path, newname, caller)?;
        let old = Dentry {
            parent,
            name: name.to_os_string(),
        };
        let new = Dentry {
            parent: newparent,
            name: newname.to_os_string(),
        };
        let mut ns = mutex(&self.namespace);
        let Some(source) = ns.by_name.get(&old).copied() else {
            return Ok(());
        };
        if ns.by_name.get(&new).copied() == Some(source) {
            return Ok(());
        }
        if let Some(replaced) = ns.remove(&new) {
            cx.remove_link(replaced);
        }
        ns.remove(&old);
        ns.add_alias(source, new);
        cx.reparent(source, newparent);
        Ok(())
    }

    fn link(
        &self,
        cx: &Cx<'_, PathNode>,
        id: NodeId,
        newparent: NodeId,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        let source = self.path(id)?;
        let parent = self.path(newparent)?;
        self.inner.link(&source, &parent, newname, caller)?;
        let dentry = Dentry {
            parent: newparent,
            name: newname.to_os_string(),
        };
        mutex(&self.namespace).add_alias(id, dentry);
        cx.add_link(id);
        Ok(id)
    }

    fn open(
        &self,
        node: &PathNode,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<P::Handle>, Errno> {
        let _guard = read_lock(&self.operations);
        self.inner
            .open(&self.node_path(node).ok_or(Errno::ENOENT)?, flags, caller)
    }

    fn read<'a>(
        &'a self,
        node: &'a PathNode,
        handle: &'a P::Handle,
        offset: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner
            .read(path.as_deref(), handle, offset, size, caller)
    }

    fn write(
        &self,
        node: &PathNode,
        handle: &P::Handle,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner
            .write(path.as_deref(), handle, data, offset, caller)
    }

    fn flush(&self, node: &PathNode, handle: &P::Handle, caller: &Caller) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.flush(p.as_deref(), handle, caller)
    }
    fn release(&self, node: &PathNode, handle: P::Handle, caller: &Caller) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.release(p.as_deref(), handle, caller)
    }
    fn fsync(
        &self,
        node: &PathNode,
        handle: &P::Handle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.fsync(p.as_deref(), handle, datasync, caller)
    }

    fn opendir(
        &self,
        node: &PathNode,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<P::DirHandle>, Errno> {
        let _g = read_lock(&self.operations);
        self.inner
            .opendir(&self.node_path(node).ok_or(Errno::ENOENT)?, flags, caller)
    }

    fn readdir(
        &self,
        cx: &Cx<'_, PathNode>,
        node: &PathNode,
        this: NodeId,
        parent: NodeId,
        handle: &P::DirHandle,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        let mut adapter = BasicSink {
            owner: self,
            cx,
            this,
            parent,
            output: sink,
            inserted: Vec::new(),
        };
        let result = self
            .inner
            .readdir(&path, handle, offset, &mut adapter, caller);
        if result.is_err() {
            adapter.rollback();
        }
        result
    }

    fn readdirplus(
        &self,
        cx: &Cx<'_, PathNode>,
        node: &PathNode,
        this: NodeId,
        parent: NodeId,
        handle: &P::DirHandle,
        offset: u64,
        sink: &mut dyn PlusDirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        let mut adapter = PlusSink {
            owner: self,
            cx,
            this,
            parent,
            output: sink,
            inserted: Vec::new(),
        };
        let result = self
            .inner
            .readdirplus(&path, handle, offset, &mut adapter, caller);
        if result.is_err() {
            adapter.rollback();
        }
        result
    }

    fn releasedir(
        &self,
        node: &PathNode,
        handle: P::DirHandle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.releasedir(p.as_deref(), handle, caller)
    }
    fn fsyncdir(
        &self,
        node: &PathNode,
        handle: &P::DirHandle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.fsyncdir(p.as_deref(), handle, datasync, caller)
    }
    fn statfs(&self, caller: &Caller) -> Result<StatFs, Errno> {
        let _g = read_lock(&self.operations);
        self.inner.statfs(Path::new("/"), caller)
    }

    fn setxattr(
        &self,
        node: &PathNode,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        self.inner.setxattr(
            &self.node_path(node).ok_or(Errno::ENOENT)?,
            name,
            value,
            flags,
            caller,
        )
    }
    fn getxattr(
        &self,
        node: &PathNode,
        name: &OsStr,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let _g = read_lock(&self.operations);
        self.inner.getxattr(
            &self.node_path(node).ok_or(Errno::ENOENT)?,
            name,
            size,
            caller,
        )
    }
    fn listxattr(
        &self,
        node: &PathNode,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let _g = read_lock(&self.operations);
        self.inner
            .listxattr(&self.node_path(node).ok_or(Errno::ENOENT)?, size, caller)
    }
    fn removexattr(&self, node: &PathNode, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        self.inner
            .removexattr(&self.node_path(node).ok_or(Errno::ENOENT)?, name, caller)
    }
    fn access(&self, node: &PathNode, mask: i32, caller: &Caller) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        self.inner
            .access(&self.node_path(node).ok_or(Errno::ENOENT)?, mask, caller)
    }

    fn create(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(NodeId, Opened<P::Handle>), Errno> {
        let _g = write_lock(&self.operations);
        let (_, opened) =
            self.inner
                .create(&self.path(parent)?, name, mode, umask, flags, caller)?;
        Ok((self.add_node(cx, parent, name), opened))
    }
    fn fallocate(
        &self,
        node: &PathNode,
        handle: &P::Handle,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner
            .fallocate(p.as_deref(), handle, mode, offset, length, caller)
    }
    fn lseek(
        &self,
        node: &PathNode,
        handle: &P::Handle,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner
            .lseek(p.as_deref(), handle, offset, whence, caller)
    }
    fn getlk(
        &self,
        node: &PathNode,
        handle: &P::Handle,
        owner: u64,
        lock: FileLock,
        caller: &Caller,
    ) -> Result<FileLock, Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.getlk(p.as_deref(), handle, owner, lock, caller)
    }
    fn setlk(
        &self,
        node: &PathNode,
        handle: &P::Handle,
        owner: u64,
        lock: FileLock,
        sleep: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner
            .setlk(p.as_deref(), handle, owner, lock, sleep, caller)
    }
}

impl<P: PathFilesystem> PathNodeFs<P> {
    fn remove_entry(
        &self,
        cx: &Cx<'_, PathNode>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
        directory: bool,
    ) -> Result<(), Errno> {
        let _guard = write_lock(&self.operations);
        let parent_path = self.path(parent)?;
        if directory {
            self.inner.rmdir(&parent_path, name, caller)?;
        } else {
            self.inner.unlink(&parent_path, name, caller)?;
        }
        let dentry = Dentry {
            parent,
            name: name.to_os_string(),
        };
        if let Some(id) = mutex(&self.namespace).remove(&dentry) {
            cx.remove_link(id);
        }
        Ok(())
    }
}

struct BasicSink<'a, 'b, P> {
    owner: &'a PathNodeFs<P>,
    cx: &'a Cx<'b, PathNode>,
    this: NodeId,
    parent: NodeId,
    output: &'a mut dyn DirSink,
    inserted: Vec<Dentry>,
}
impl<P: PathFilesystem> BasicSink<'_, '_, P> {
    fn rollback(&mut self) {
        for dentry in self.inserted.drain(..).rev() {
            self.owner.rollback_dentry(self.cx, &dentry);
        }
    }
}
impl<P: PathFilesystem> PathDirSink for BasicSink<'_, '_, P> {
    fn add(&mut self, name: &OsStr, kind: FileKind, next_offset: u64) -> bool {
        let (id, inserted) = if name == OsStr::new(".") {
            (self.this, None)
        } else if name == OsStr::new("..") {
            (self.parent, None)
        } else {
            self.owner.add_enumerated_node(self.cx, self.this, name)
        };
        let accepted = self.output.add(name, id, kind, next_offset);
        if let Some(dentry) = inserted {
            if accepted {
                self.inserted.push(dentry);
            } else {
                self.owner.rollback_dentry(self.cx, &dentry);
            }
        }
        accepted
    }
}
struct PlusSink<'a, 'b, P> {
    owner: &'a PathNodeFs<P>,
    cx: &'a Cx<'b, PathNode>,
    this: NodeId,
    parent: NodeId,
    output: &'a mut dyn PlusDirSink,
    inserted: Vec<Dentry>,
}
impl<P: PathFilesystem> PlusSink<'_, '_, P> {
    fn rollback(&mut self) {
        for dentry in self.inserted.drain(..).rev() {
            self.owner.rollback_dentry(self.cx, &dentry);
        }
    }
}
impl<P: PathFilesystem> PathPlusDirSink for PlusSink<'_, '_, P> {
    fn add(&mut self, name: &OsStr, attr: NodeAttr, next_offset: u64) -> bool {
        let (id, inserted) = if name == OsStr::new(".") {
            (self.this, None)
        } else if name == OsStr::new("..") {
            (self.parent, None)
        } else {
            self.owner.add_enumerated_node(self.cx, self.this, name)
        };
        let accepted = self.output.add(name, id, attr, next_offset);
        if let Some(dentry) = inserted {
            if accepted {
                self.inserted.push(dentry);
            } else {
                self.owner.rollback_dentry(self.cx, &dentry);
            }
        }
        accepted
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::{LookupReply, Runtime};

    #[derive(Clone, Default)]
    struct RecordingFs {
        getattr_paths: Arc<Mutex<Vec<Option<PathBuf>>>>,
    }

    impl PathFilesystem for RecordingFs {
        type Handle = ();
        type DirHandle = ();

        fn lookup(
            &self,
            _parent: &Path,
            _name: &OsStr,
            _caller: &Caller,
        ) -> Result<Option<NodeAttr>, Errno> {
            Ok(Some(NodeAttr::default()))
        }

        fn getattr(
            &self,
            path: Option<&Path>,
            _handle: Option<&Self::Handle>,
            _caller: &Caller,
        ) -> Result<NodeAttr, Errno> {
            mutex(&self.getattr_paths).push(path.map(Path::to_path_buf));
            Ok(NodeAttr::default())
        }

        fn rename(
            &self,
            _parent: &Path,
            _name: &OsStr,
            _newparent: &Path,
            _newname: &OsStr,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn link(
            &self,
            _path: &Path,
            _newparent: &Path,
            _newname: &OsStr,
            _caller: &Caller,
        ) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }

        fn unlink(&self, _parent: &Path, _name: &OsStr, _caller: &Caller) -> Result<(), Errno> {
            Ok(())
        }

        fn open(
            &self,
            _path: &Path,
            _flags: i32,
            _caller: &Caller,
        ) -> Result<Opened<Self::Handle>, Errno> {
            Ok(Opened::new(()))
        }

        fn opendir(
            &self,
            _path: &Path,
            _flags: i32,
            _caller: &Caller,
        ) -> Result<Opened<Self::DirHandle>, Errno> {
            Ok(Opened::new(()))
        }

        fn readdir(
            &self,
            _path: &Path,
            _handle: &Self::DirHandle,
            _offset: u64,
            sink: &mut dyn PathDirSink,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            assert!(sink.add(OsStr::new("enumerated"), FileKind::RegularFile, 1));
            Err(Errno::EIO)
        }
    }

    fn found(reply: LookupReply) -> u64 {
        match reply {
            LookupReply::Found(entry) => entry.ino,
            LookupReply::Negative => panic!("unexpected negative lookup"),
        }
    }

    #[test]
    fn concurrent_lookup_deduplicates_node_identity() {
        let runtime = Arc::new(Runtime::new(PathNodeFs::new(RecordingFs::default())));
        let barrier = Arc::new(Barrier::new(12));
        let mut workers = Vec::new();
        for _ in 0..12 {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                found(
                    runtime
                        .lookup(1, OsStr::new("same"), &Caller::default())
                        .unwrap(),
                )
            }));
        }
        let ids: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
    }

    #[test]
    fn moving_directory_updates_descendant_paths_without_rewriting_children() {
        let recording = RecordingFs::default();
        let paths = Arc::clone(&recording.getattr_paths);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let directory = found(runtime.lookup(1, OsStr::new("old"), &caller).unwrap());
        let child = found(
            runtime
                .lookup(directory, OsStr::new("child"), &caller)
                .unwrap(),
        );

        runtime
            .rename(1, OsStr::new("old"), 1, OsStr::new("new"), 0, &caller)
            .unwrap();
        runtime.getattr(child, None, &caller).unwrap();

        assert_eq!(
            mutex(&paths).last().unwrap().as_deref(),
            Some(Path::new("/new/child"))
        );
    }

    #[test]
    fn overwritten_open_destination_continues_without_a_path() {
        let recording = RecordingFs::default();
        let paths = Arc::clone(&recording.getattr_paths);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let source = found(runtime.lookup(1, OsStr::new("source"), &caller).unwrap());
        let destination = found(
            runtime
                .lookup(1, OsStr::new("destination"), &caller)
                .unwrap(),
        );
        let open = runtime.open(destination, 0, &caller).unwrap();

        runtime
            .rename(
                1,
                OsStr::new("source"),
                1,
                OsStr::new("destination"),
                0,
                &caller,
            )
            .unwrap();
        runtime
            .getattr(destination, Some(open.fh), &caller)
            .unwrap();

        assert_eq!(mutex(&paths).last().unwrap(), &None);
        assert_eq!(
            found(
                runtime
                    .lookup(1, OsStr::new("destination"), &caller)
                    .unwrap()
            ),
            source
        );
    }

    #[test]
    fn hard_link_alias_survives_unlink_of_original_name() {
        let recording = RecordingFs::default();
        let paths = Arc::clone(&recording.getattr_paths);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let original = found(runtime.lookup(1, OsStr::new("original"), &caller).unwrap());
        runtime
            .link(original, 1, OsStr::new("alias"), &caller)
            .unwrap();
        runtime.unlink(1, OsStr::new("original"), &caller).unwrap();
        runtime.getattr(original, None, &caller).unwrap();
        assert_eq!(
            mutex(&paths).last().unwrap().as_deref(),
            Some(Path::new("/alias"))
        );
    }

    struct AcceptingSink;
    impl DirSink for AcceptingSink {
        fn add(&mut self, _name: &OsStr, _id: NodeId, _kind: FileKind, _next_offset: u64) -> bool {
            true
        }
    }

    #[test]
    fn failed_readdir_rolls_back_new_namespace_entries() {
        let runtime = Runtime::new(PathNodeFs::new(RecordingFs::default()));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        assert_eq!(
            runtime.readdir(1, open.fh, 0, &mut AcceptingSink, &caller),
            Err(Errno::EIO)
        );

        // A stale dentry would point at the node that was retired during
        // rollback, causing entry construction to fail with ENOENT.
        assert!(matches!(
            runtime.lookup(1, OsStr::new("enumerated"), &caller),
            Ok(LookupReply::Found(_))
        ));
    }

    #[derive(Default)]
    struct RecordingDirSink {
        seen: Vec<(OsString, u64)>,
        stop_after: Option<usize>,
    }

    impl PathDirSink for RecordingDirSink {
        fn add(&mut self, name: &OsStr, _kind: FileKind, next_offset: u64) -> bool {
            self.seen.push((name.to_os_string(), next_offset));
            self.stop_after != Some(self.seen.len())
        }
    }

    #[derive(Default)]
    struct RecordingPlusDirSink {
        seen: Vec<(OsString, u64)>,
    }

    impl PathPlusDirSink for RecordingPlusDirSink {
        fn add(&mut self, name: &OsStr, _attr: NodeAttr, next_offset: u64) -> bool {
            self.seen.push((name.to_os_string(), next_offset));
            true
        }
    }

    fn sample_dir_buffer() -> DirBuffer {
        let mut buf = DirBuffer::new();
        buf.push_dots(NodeAttr::default(), NodeAttr::default());
        buf.push("a", FileKind::RegularFile, NodeAttr::default());
        buf.push("b", FileKind::RegularFile, NodeAttr::default());
        buf
    }

    #[test]
    fn dir_buffer_fill_assigns_sequential_resume_offsets() {
        let buf = sample_dir_buffer();
        let mut sink = RecordingDirSink::default();
        buf.fill(0, &mut sink);
        assert_eq!(
            sink.seen,
            vec![
                (OsString::from("."), 1),
                (OsString::from(".."), 2),
                (OsString::from("a"), 3),
                (OsString::from("b"), 4),
            ]
        );
    }

    #[test]
    fn dir_buffer_fill_resumes_from_given_offset() {
        let buf = sample_dir_buffer();
        let mut sink = RecordingDirSink::default();
        // Resume after the entry that returned next_offset = 2 (i.e. "..").
        buf.fill(2, &mut sink);
        assert_eq!(
            sink.seen,
            vec![(OsString::from("a"), 3), (OsString::from("b"), 4)]
        );
    }

    #[test]
    fn dir_buffer_fill_stops_when_sink_is_full() {
        let buf = sample_dir_buffer();
        let mut sink = RecordingDirSink {
            stop_after: Some(2),
            ..Default::default()
        };
        buf.fill(0, &mut sink);
        assert_eq!(
            sink.seen,
            vec![(OsString::from("."), 1), (OsString::from(".."), 2)]
        );
    }

    #[test]
    fn dir_buffer_fill_plus_carries_attrs_and_offsets() {
        let buf = sample_dir_buffer();
        let mut sink = RecordingPlusDirSink::default();
        buf.fill_plus(0, &mut sink);
        assert_eq!(sink.seen.len(), 4);
        assert_eq!(sink.seen[3], (OsString::from("b"), 4));
    }
}
