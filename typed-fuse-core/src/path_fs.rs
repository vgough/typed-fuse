//! A synchronous path-based filesystem interface and its [`NodeFs`] adapter.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

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

/// The identity represented by a path-directory entry.
pub enum PathDirIdentity<S> {
    Current,
    Parent,
    Child(Arc<S>),
}

impl<S> Clone for PathDirIdentity<S> {
    fn clone(&self) -> Self {
        match self {
            Self::Current => Self::Current,
            Self::Parent => Self::Parent,
            Self::Child(state) => Self::Child(Arc::clone(state)),
        }
    }
}

impl<S: std::fmt::Debug> std::fmt::Debug for PathDirIdentity<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current => f.write_str("Current"),
            Self::Parent => f.write_str("Parent"),
            Self::Child(state) => f.debug_tuple("Child").field(state).finish(),
        }
    }
}

/// A directory-entry sink used by [`PathFilesystem::readdir`].
pub trait PathDirSink<S> {
    /// Adds one entry. `next_offset` is the resume cookie the kernel will
    /// hand back to continue after this entry. Returns `false` once the
    /// buffer is full, at which point the caller must stop iterating.
    fn add(
        &mut self,
        name: &OsStr,
        kind: FileKind,
        identity: PathDirIdentity<S>,
        next_offset: u64,
    ) -> bool;
}

/// A directory-entry sink used by [`PathFilesystem::readdirplus`].
pub trait PathPlusDirSink<S> {
    /// Adds one entry with its attributes. `next_offset` is the resume
    /// cookie the kernel will hand back to continue after this entry.
    /// Returns `false` once the buffer is full, at which point the caller
    /// must stop iterating.
    fn add(
        &mut self,
        name: &OsStr,
        attr: NodeAttr,
        identity: PathDirIdentity<S>,
        next_offset: u64,
    ) -> bool;
}

/// One entry captured by [`DirBuffer`].
#[derive(Clone, Debug)]
pub struct DirEntry<S> {
    pub name: OsString,
    pub kind: FileKind,
    pub attr: NodeAttr,
    pub identity: PathDirIdentity<S>,
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
pub struct DirBuffer<S> {
    entries: Vec<DirEntry<S>>,
}

impl<S> DirBuffer<S> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Appends one entry.
    pub fn push(
        &mut self,
        name: impl Into<OsString>,
        kind: FileKind,
        attr: NodeAttr,
        state: Arc<S>,
    ) {
        self.entries.push(DirEntry {
            name: name.into(),
            kind,
            attr,
            identity: PathDirIdentity::Child(state),
        });
    }

    /// Appends the conventional `.` and `..` entries with the given
    /// attributes (self and parent respectively). `read_dir`-style
    /// directory listings omit these, so callers building a buffer from one
    /// typically call this first.
    pub fn push_dots(&mut self, self_attr: NodeAttr, parent_attr: NodeAttr) {
        self.entries.push(DirEntry {
            name: OsString::from("."),
            kind: FileKind::Directory,
            attr: self_attr,
            identity: PathDirIdentity::Current,
        });
        self.entries.push(DirEntry {
            name: OsString::from(".."),
            kind: FileKind::Directory,
            attr: parent_attr,
            identity: PathDirIdentity::Parent,
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &DirEntry<S>> {
        self.entries.iter()
    }

    /// Replays entries from `offset` onward into `sink`, per
    /// [`PathFilesystem::readdir`] semantics.
    pub fn fill(&self, offset: u64, sink: &mut dyn PathDirSink<S>) {
        replay(offset, self.entries.len(), |entry, next_offset| {
            let entry = &self.entries[entry];
            sink.add(&entry.name, entry.kind, entry.identity.clone(), next_offset)
        });
    }

    /// Replays entries from `offset` onward into `sink`, per
    /// [`PathFilesystem::readdirplus`] semantics.
    pub fn fill_plus(&self, offset: u64, sink: &mut dyn PathPlusDirSink<S>) {
        replay(offset, self.entries.len(), |entry, next_offset| {
            let entry = &self.entries[entry];
            sink.add(&entry.name, entry.attr, entry.identity.clone(), next_offset)
        });
    }
}

/// Typed state and the adapter's current path for an existing node.
pub struct PathNodeRef<'a, S> {
    path: Option<&'a Path>,
    state: &'a Arc<S>,
}

impl<'a, S> PathNodeRef<'a, S> {
    pub fn path(&self) -> Option<&'a Path> {
        self.path
    }

    pub fn state(&self) -> &'a S {
        self.state
    }

    pub fn state_arc(&self) -> Arc<S> {
        Arc::clone(self.state)
    }
}

/// Attributes and typed state discovered or created for a path node.
pub struct PathEntry<S> {
    pub attr: NodeAttr,
    pub state: Arc<S>,
}

impl<S> PathEntry<S> {
    pub fn new(attr: NodeAttr, state: Arc<S>) -> Self {
        Self { attr, state }
    }
}

/// Shared readdir resume-offset bookkeeping: `offset` is the cookie the
/// kernel handed back (0 = from the start), each entry at index `i` is
/// emitted with `next_offset = i + 1`, and emission stops as soon as the
/// sink reports its buffer full (`add` returns `false`). An offset beyond
/// `len` simply emits nothing. This is the single place the FUSE readdir
/// cookie protocol is encoded for snapshot-style listings; use it for any
/// new `readdir` implementation backed by a materialized entry list.
pub fn replay(offset: u64, len: usize, mut emit: impl FnMut(usize, u64) -> bool) {
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
    for index in start..len {
        if !emit(index, index as u64 + 1) {
            break;
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
    /// Per-runtime-node data. Use `()` for stateless nodes.
    type NodeState: Send + Sync + 'static;
    /// Per-open-file data. Use `()` if the filesystem is stateless per open.
    type Handle: Send + Sync;
    /// Per-open-directory data. Use `()` if not needed.
    type DirHandle: Send + Sync;

    /// Builds state for the root node. This is infallible because runtime
    /// construction itself cannot report an error.
    fn root_state(&mut self) -> Arc<Self::NodeState>;

    /// Set to `true` to enable [`PathFilesystem::getlk`]/[`PathFilesystem::setlk`].
    /// Left disabled by default since most filesystems delegate POSIX
    /// locking to the kernel.
    const SUPPORTS_POSIX_LOCKS: bool = false;
    /// Set to `true` to enable [`PathFilesystem::flock`]. Left disabled by
    /// default since most filesystems delegate BSD locking to the kernel.
    const SUPPORTS_FLOCK: bool = false;
    /// Set to `true` to enable [`PathFilesystem::readdirplus`], letting the
    /// kernel populate its attribute cache from directory listings instead
    /// of a follow-up `lookup` per entry.
    const SUPPORTS_READDIRPLUS: bool = false;

    /// Called once when libfuse establishes communication with the kernel.
    fn init(&self, conn: &mut ConnInfo) {}
    /// Called on filesystem exit.
    fn destroy(&self) {}

    /// Called when the kernel's lookup count for a node reaches zero.
    ///
    /// `path` is the adapter's current canonical path for the node, or
    /// `None` if its last known name has already been removed. This is a
    /// notification only: it may be delivered while the node still has links
    /// or open handles, and it cannot report an error.
    fn forget(&self, node: PathNodeRef<'_, Self::NodeState>) {}

    /// Looks up `name` in directory `parent`. Return `Ok(Some(attr))` for a
    /// hit, `Ok(None)` to populate the kernel's negative-lookup cache, or
    /// `Err` for a hard failure.
    fn lookup(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<Option<PathEntry<Self::NodeState>>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Returns the attributes of `path`, including its current link count.
    /// `handle` is the open handle if the call arrived through one; `path`
    /// is `None` if the open file's last known name has been unlinked.
    fn getattr(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: Option<&Self::Handle>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno>;

    /// Applies the `Some` fields of `set` to `path`, returning the resulting
    /// attributes.
    fn setattr(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: Option<&Self::Handle>,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Reads the target of the symbolic link at `path`.
    fn readlink(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        caller: &Caller,
    ) -> Result<PathBuf, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a non-directory, non-symlink node named `name` in `parent`.
    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<PathEntry<Self::NodeState>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a directory named `name` in `parent`.
    fn mkdir(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<PathEntry<Self::NodeState>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a symbolic link named `name` in `parent` pointing at `target`.
    fn symlink(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        target: &Path,
        caller: &Caller,
    ) -> Result<PathEntry<Self::NodeState>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the non-directory entry `name` from `parent`.
    fn unlink(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Removes the empty directory `name` from `parent`.
    fn rmdir(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Renames `name` in `parent` to `newname` in `newparent`.
    fn rename(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        newparent: PathNodeRef<'_, Self::NodeState>,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Creates a hard link to `path` named `newname` in `newparent`.
    fn link(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        newparent: PathNodeRef<'_, Self::NodeState>,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Opens `path`, returning the filesystem's handle object.
    fn open(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Called when the last reference to an open file is dropped; consumes
    /// the handle.
    fn release(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: Self::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes file contents (and metadata unless `datasync`).
    fn fsync(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::Handle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Opens the directory at `path`.
    fn opendir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::DirHandle,
        offset: u64,
        sink: &mut dyn PathDirSink<Self::NodeState>,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Like [`PathFilesystem::readdir`], but emits attributes alongside each
    /// entry via [`PathPlusDirSink`]. Only called when
    /// [`PathFilesystem::SUPPORTS_READDIRPLUS`] is `true`.
    fn readdirplus(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::DirHandle,
        offset: u64,
        sink: &mut dyn PathPlusDirSink<Self::NodeState>,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Releases a directory handle; consumes it.
    fn releasedir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: Self::DirHandle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Flushes directory contents.
    fn fsyncdir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }
    /// Returns the NUL-separated extended attribute names on `path`. Same
    /// size-query protocol as [`PathFilesystem::getxattr`].
    fn listxattr(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        Err(Errno::ENOSYS)
    }
    /// Removes extended attribute `name` from `path`.
    fn removexattr(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }
    /// Checks access to `path` per the `access(2)` `mask`.
    fn access(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        mask: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Ok(())
    }

    /// Atomically creates and opens a regular file named `name` in `parent`.
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(PathEntry<Self::NodeState>, Opened<Self::Handle>), Errno> {
        Err(Errno::ENOSYS)
    }

    /// Pre-allocates/punches `length` bytes at `offset` in `handle`.
    fn fallocate(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
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
        node: PathNodeRef<'_, Self::NodeState>,
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
    /// with `LOCK_NB`. Only called when
    /// [`PathFilesystem::SUPPORTS_FLOCK`] is `true`.
    fn flock(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::Handle,
        operation: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        Err(Errno::ENOSYS)
    }
}

/// The [`NodeFs::Node`] payload used by [`PathNodeFs`]. Opaque; filesystems
/// interact with [`PathFilesystem`] purely in terms of paths.
pub struct PathNode<S> {
    key: u64,
    state: Arc<S>,
}

impl<S> std::fmt::Debug for PathNode<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathNode").field("key", &self.key).finish()
    }
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

    fn id_for_node<S>(&self, node: &PathNode<S>) -> Option<NodeId> {
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

    fn insert<S>(
        &mut self,
        cx: &Cx<'_, PathNode<S>>,
        parent: NodeId,
        name: &OsStr,
        state: Arc<S>,
    ) -> (NodeId, bool) {
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
        let id = cx.insert(PathNode { key, state }, parent);
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

impl<P: PathFilesystem> PathNodeFs<P> {
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

    fn node_id(&self, node: &PathNode<P::NodeState>) -> Option<NodeId> {
        mutex(&self.namespace).id_for_node(node)
    }
    fn node_path(&self, node: &PathNode<P::NodeState>) -> Option<PathBuf> {
        self.node_id(node)
            .and_then(|id| mutex(&self.namespace).path_for(id))
    }
    fn path(&self, id: NodeId) -> Result<PathBuf, Errno> {
        mutex(&self.namespace).path_for(id).ok_or(Errno::ENOENT)
    }
    fn add_node(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        entry: PathEntry<P::NodeState>,
    ) -> Result<NodeId, Errno>
    where
        P: PathFilesystem,
    {
        Ok(mutex(&self.namespace)
            .insert(cx, parent, name, entry.state)
            .0)
    }

    fn add_enumerated_node(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        state: Arc<P::NodeState>,
    ) -> Result<(NodeId, Option<Dentry>), Errno>
    where
        P: PathFilesystem,
    {
        let (id, inserted) = mutex(&self.namespace).insert(cx, parent, name, state);
        let dentry = inserted.then(|| Dentry {
            parent,
            name: name.to_os_string(),
        });
        Ok((id, dentry))
    }

    fn rollback_dentry(&self, cx: &Cx<'_, PathNode<P::NodeState>>, dentry: &Dentry) {
        if let Some(id) = mutex(&self.namespace).remove(dentry) {
            cx.remove_link(id);
        }
    }
}

impl<P: PathFilesystem> NodeFs for PathNodeFs<P> {
    type Node = PathNode<P::NodeState>;
    type Handle = P::Handle;
    type DirHandle = P::DirHandle;

    const SUPPORTS_POSIX_LOCKS: bool = P::SUPPORTS_POSIX_LOCKS;
    const SUPPORTS_FLOCK: bool = P::SUPPORTS_FLOCK;
    const SUPPORTS_READDIRPLUS: bool = P::SUPPORTS_READDIRPLUS;

    fn root(&mut self) -> Self::Node {
        PathNode {
            key: 1,
            state: self.inner.root_state(),
        }
    }
    fn init(&self, conn: &mut ConnInfo) {
        self.inner.init(conn)
    }
    fn destroy(&self) {
        self.inner.destroy()
    }
    fn forget(&self, node: &PathNode<P::NodeState>) {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.forget(PathNodeRef {
            path: path.as_deref(),
            state: &node.state,
        });
    }

    fn getattr(
        &self,
        node: &PathNode<P::NodeState>,
        handle: Option<&P::Handle>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.getattr(
            PathNodeRef {
                path: path.as_deref(),
                state: &node.state,
            },
            handle,
            caller,
        )
    }

    fn setattr(
        &self,
        node: &PathNode<P::NodeState>,
        handle: Option<&P::Handle>,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.setattr(
            PathNodeRef {
                path: path.as_deref(),
                state: &node.state,
            },
            handle,
            set,
            caller,
        )
    }

    fn lookup(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        let _guard = read_lock(&self.operations);
        let parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let parent_path = self.path(parent)?;
        let Some(entry) = self.inner.lookup(
            PathNodeRef {
                path: Some(&parent_path),
                state: &parent_node.state,
            },
            name,
            caller,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(self.add_node(cx, parent, name, entry)?))
    }

    fn readlink(&self, node: &PathNode<P::NodeState>, caller: &Caller) -> Result<PathBuf, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.readlink(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            caller,
        )
    }

    fn mknod(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        let parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let parent_path = self.path(parent)?;
        let entry = self.inner.mknod(
            PathNodeRef {
                path: Some(&parent_path),
                state: &parent_node.state,
            },
            name,
            mode,
            rdev,
            umask,
            caller,
        )?;
        Ok(self.add_node(cx, parent, name, entry)?)
    }

    fn mkdir(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        let parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let parent_path = self.path(parent)?;
        let entry = self.inner.mkdir(
            PathNodeRef {
                path: Some(&parent_path),
                state: &parent_node.state,
            },
            name,
            mode,
            umask,
            caller,
        )?;
        Ok(self.add_node(cx, parent, name, entry)?)
    }

    fn symlink(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        target: &Path,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        let parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let parent_path = self.path(parent)?;
        let entry = self.inner.symlink(
            PathNodeRef {
                path: Some(&parent_path),
                state: &parent_node.state,
            },
            name,
            target,
            caller,
        )?;
        Ok(self.add_node(cx, parent, name, entry)?)
    }

    fn unlink(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        self.remove_entry(cx, parent, name, caller, false)
    }

    fn rmdir(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        self.remove_entry(cx, parent, name, caller, true)
    }

    fn rename(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
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
        let old_parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let new_parent_node = cx.get(newparent).ok_or(Errno::ENOENT)?;
        let old_parent_path = self.path(parent)?;
        let new_parent_path = self.path(newparent)?;
        self.inner.rename(
            PathNodeRef {
                path: Some(&old_parent_path),
                state: &old_parent_node.state,
            },
            name,
            PathNodeRef {
                path: Some(&new_parent_path),
                state: &new_parent_node.state,
            },
            newname,
            caller,
        )?;
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
        cx: &Cx<'_, PathNode<P::NodeState>>,
        id: NodeId,
        newparent: NodeId,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<NodeId, Errno> {
        let _guard = write_lock(&self.operations);
        let source_node = cx.get(id).ok_or(Errno::ENOENT)?;
        let parent_node = cx.get(newparent).ok_or(Errno::ENOENT)?;
        let source = self.path(id)?;
        let parent = self.path(newparent)?;
        self.inner.link(
            PathNodeRef {
                path: Some(&source),
                state: &source_node.state,
            },
            PathNodeRef {
                path: Some(&parent),
                state: &parent_node.state,
            },
            newname,
            caller,
        )?;
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
        node: &PathNode<P::NodeState>,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<P::Handle>, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.open(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            flags,
            caller,
        )
    }

    fn read<'a>(
        &'a self,
        node: &'a PathNode<P::NodeState>,
        handle: &'a P::Handle,
        offset: u64,
        size: usize,
        caller: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.read(
            PathNodeRef {
                path: path.as_deref(),
                state: &node.state,
            },
            handle,
            offset,
            size,
            caller,
        )
    }

    fn write(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node);
        self.inner.write(
            PathNodeRef {
                path: path.as_deref(),
                state: &node.state,
            },
            handle,
            data,
            offset,
            caller,
        )
    }

    fn flush(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.flush(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            caller,
        )
    }
    fn release(
        &self,
        node: &PathNode<P::NodeState>,
        handle: P::Handle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.release(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            caller,
        )
    }
    fn fsync(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.fsync(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            datasync,
            caller,
        )
    }

    fn opendir(
        &self,
        node: &PathNode<P::NodeState>,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<P::DirHandle>, Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.opendir(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            flags,
            caller,
        )
    }

    fn readdir(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        node: &PathNode<P::NodeState>,
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
            error: None,
        };
        let result = self.inner.readdir(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            handle,
            offset,
            &mut adapter,
            caller,
        );
        if result.is_err() || adapter.error.is_some() {
            adapter.rollback();
        }
        result?;
        adapter.error.map_or(Ok(()), Err)
    }

    fn readdirplus(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        node: &PathNode<P::NodeState>,
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
            error: None,
        };
        let result = self.inner.readdirplus(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            handle,
            offset,
            &mut adapter,
            caller,
        );
        if result.is_err() || adapter.error.is_some() {
            adapter.rollback();
        }
        result?;
        adapter.error.map_or(Ok(()), Err)
    }

    fn releasedir(
        &self,
        node: &PathNode<P::NodeState>,
        handle: P::DirHandle,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.releasedir(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            caller,
        )
    }
    fn fsyncdir(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::DirHandle,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.fsyncdir(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            datasync,
            caller,
        )
    }
    fn statfs(&self, caller: &Caller) -> Result<StatFs, Errno> {
        let _g = read_lock(&self.operations);
        self.inner.statfs(Path::new("/"), caller)
    }

    fn setxattr(
        &self,
        node: &PathNode<P::NodeState>,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.setxattr(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            name,
            value,
            flags,
            caller,
        )
    }
    fn getxattr(
        &self,
        node: &PathNode<P::NodeState>,
        name: &OsStr,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.getxattr(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            name,
            size,
            caller,
        )
    }
    fn listxattr(
        &self,
        node: &PathNode<P::NodeState>,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.listxattr(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            size,
            caller,
        )
    }
    fn removexattr(
        &self,
        node: &PathNode<P::NodeState>,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.removexattr(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            name,
            caller,
        )
    }
    fn access(
        &self,
        node: &PathNode<P::NodeState>,
        mask: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        self.inner.access(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            mask,
            caller,
        )
    }

    fn create(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(NodeId, Opened<P::Handle>), Errno> {
        let _g = write_lock(&self.operations);
        let parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let parent_path = self.path(parent)?;
        let (entry, opened) = self.inner.create(
            PathNodeRef {
                path: Some(&parent_path),
                state: &parent_node.state,
            },
            name,
            mode,
            umask,
            flags,
            caller,
        )?;
        Ok((self.add_node(cx, parent, name, entry)?, opened))
    }
    fn fallocate(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.fallocate(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            mode,
            offset,
            length,
            caller,
        )
    }
    fn lseek(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.lseek(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            offset,
            whence,
            caller,
        )
    }
    fn getlk(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        owner: u64,
        lock: FileLock,
        caller: &Caller,
    ) -> Result<FileLock, Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.getlk(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            owner,
            lock,
            caller,
        )
    }
    fn setlk(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        owner: u64,
        lock: FileLock,
        sleep: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.setlk(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            owner,
            lock,
            sleep,
            caller,
        )
    }
    fn flock(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &P::Handle,
        operation: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        self.inner.flock(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            handle,
            operation,
            caller,
        )
    }
}

impl<P: PathFilesystem> PathNodeFs<P> {
    fn remove_entry(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        parent: NodeId,
        name: &OsStr,
        caller: &Caller,
        directory: bool,
    ) -> Result<(), Errno> {
        let _guard = write_lock(&self.operations);
        let parent_node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let parent_path = self.path(parent)?;
        if directory {
            self.inner.rmdir(
                PathNodeRef {
                    path: Some(&parent_path),
                    state: &parent_node.state,
                },
                name,
                caller,
            )?;
        } else {
            self.inner.unlink(
                PathNodeRef {
                    path: Some(&parent_path),
                    state: &parent_node.state,
                },
                name,
                caller,
            )?;
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

struct BasicSink<'a, 'b, P: PathFilesystem> {
    owner: &'a PathNodeFs<P>,
    cx: &'a Cx<'b, PathNode<P::NodeState>>,
    this: NodeId,
    parent: NodeId,
    output: &'a mut dyn DirSink,
    inserted: Vec<Dentry>,
    error: Option<Errno>,
}
impl<P: PathFilesystem> BasicSink<'_, '_, P> {
    fn rollback(&mut self) {
        for dentry in self.inserted.drain(..).rev() {
            self.owner.rollback_dentry(self.cx, &dentry);
        }
    }
}
impl<P: PathFilesystem> PathDirSink<P::NodeState> for BasicSink<'_, '_, P> {
    fn add(
        &mut self,
        name: &OsStr,
        kind: FileKind,
        identity: PathDirIdentity<P::NodeState>,
        next_offset: u64,
    ) -> bool {
        let (id, inserted) = match identity {
            PathDirIdentity::Current => (self.this, None),
            PathDirIdentity::Parent => (self.parent, None),
            PathDirIdentity::Child(state) => match self
                .owner
                .add_enumerated_node(self.cx, self.this, name, state)
            {
                Ok(node) => node,
                Err(error) => {
                    self.error = Some(error);
                    return false;
                }
            },
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
struct PlusSink<'a, 'b, P: PathFilesystem> {
    owner: &'a PathNodeFs<P>,
    cx: &'a Cx<'b, PathNode<P::NodeState>>,
    this: NodeId,
    parent: NodeId,
    output: &'a mut dyn PlusDirSink,
    inserted: Vec<Dentry>,
    error: Option<Errno>,
}
impl<P: PathFilesystem> PlusSink<'_, '_, P> {
    fn rollback(&mut self) {
        for dentry in self.inserted.drain(..).rev() {
            self.owner.rollback_dentry(self.cx, &dentry);
        }
    }
}
impl<P: PathFilesystem> PathPlusDirSink<P::NodeState> for PlusSink<'_, '_, P> {
    fn add(
        &mut self,
        name: &OsStr,
        attr: NodeAttr,
        identity: PathDirIdentity<P::NodeState>,
        next_offset: u64,
    ) -> bool {
        let (id, inserted) = match identity {
            PathDirIdentity::Current => (self.this, None),
            PathDirIdentity::Parent => (self.parent, None),
            PathDirIdentity::Child(state) => match self
                .owner
                .add_enumerated_node(self.cx, self.this, name, state)
            {
                Ok(node) => node,
                Err(error) => {
                    self.error = Some(error);
                    return false;
                }
            },
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

    #[test]
    fn replay_emits_index_plus_one_cookies_from_offset() {
        let mut emitted: Vec<(usize, u64)> = Vec::new();
        replay(0, 3, |index, next_offset| {
            emitted.push((index, next_offset));
            true
        });
        assert_eq!(emitted, vec![(0, 1), (1, 2), (2, 3)]);

        emitted.clear();
        replay(2, 3, |index, next_offset| {
            emitted.push((index, next_offset));
            true
        });
        assert_eq!(emitted, vec![(2, 3)]);
    }

    #[test]
    fn replay_stops_when_sink_is_full() {
        let mut emitted = Vec::new();
        replay(0, 5, |index, next_offset| {
            emitted.push((index, next_offset));
            index < 1 // full after the second entry
        });
        assert_eq!(emitted, vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn replay_tolerates_out_of_range_and_huge_offsets() {
        let mut emitted = Vec::new();
        replay(10, 3, |index, next_offset| {
            emitted.push((index, next_offset));
            true
        });
        replay(u64::MAX, 3, |index, next_offset| {
            emitted.push((index, next_offset));
            true
        });
        assert!(emitted.is_empty());
    }

    #[derive(Clone, Default)]
    struct RecordingFs {
        getattr_paths: Arc<Mutex<Vec<Option<PathBuf>>>>,
        getattr_states: Arc<Mutex<Vec<Arc<usize>>>>,
        forgotten_paths: Arc<Mutex<Vec<Option<PathBuf>>>>,
        next_state: Arc<std::sync::atomic::AtomicUsize>,
        alternate_dir_identity: bool,
    }

    impl PathFilesystem for RecordingFs {
        type NodeState = usize;
        type Handle = ();
        type DirHandle = ();

        fn root_state(&mut self) -> Arc<Self::NodeState> {
            Arc::new(0)
        }

        fn lookup(
            &self,
            _parent: PathNodeRef<'_, Self::NodeState>,
            _name: &OsStr,
            _caller: &Caller,
        ) -> Result<Option<PathEntry<Self::NodeState>>, Errno> {
            Ok(Some(PathEntry::new(
                NodeAttr::default(),
                Arc::new(
                    self.next_state
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1,
                ),
            )))
        }

        fn getattr(
            &self,
            node: PathNodeRef<'_, Self::NodeState>,
            _handle: Option<&Self::Handle>,
            _caller: &Caller,
        ) -> Result<NodeAttr, Errno> {
            mutex(&self.getattr_paths).push(node.path().map(Path::to_path_buf));
            mutex(&self.getattr_states).push(node.state_arc());
            Ok(NodeAttr::default())
        }

        fn forget(&self, node: PathNodeRef<'_, Self::NodeState>) {
            mutex(&self.forgotten_paths).push(node.path().map(Path::to_path_buf));
        }

        fn rename(
            &self,
            _parent: PathNodeRef<'_, Self::NodeState>,
            _name: &OsStr,
            _newparent: PathNodeRef<'_, Self::NodeState>,
            _newname: &OsStr,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn link(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            _newparent: PathNodeRef<'_, Self::NodeState>,
            _newname: &OsStr,
            _caller: &Caller,
        ) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }

        fn unlink(
            &self,
            _parent: PathNodeRef<'_, Self::NodeState>,
            _name: &OsStr,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            Ok(())
        }

        fn open(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            _flags: i32,
            _caller: &Caller,
        ) -> Result<Opened<Self::Handle>, Errno> {
            Ok(Opened::new(()))
        }

        fn opendir(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            _flags: i32,
            _caller: &Caller,
        ) -> Result<Opened<Self::DirHandle>, Errno> {
            Ok(Opened::new(()))
        }

        fn readdir(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            _handle: &Self::DirHandle,
            _offset: u64,
            sink: &mut dyn PathDirSink<Self::NodeState>,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            assert!(sink.add(
                OsStr::new("enumerated"),
                FileKind::RegularFile,
                PathDirIdentity::Child(Arc::new(1)),
                1,
            ));
            if self.alternate_dir_identity {
                assert!(sink.add(
                    OsStr::new("self"),
                    FileKind::Directory,
                    PathDirIdentity::Current,
                    2,
                ));
                return Ok(());
            }
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
    fn forget_reports_the_current_path_for_each_zero_lookup_transition() {
        let recording = RecordingFs::default();
        let forgotten = Arc::clone(&recording.forgotten_paths);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let node = found(runtime.lookup(1, OsStr::new("old"), &caller).unwrap());

        runtime
            .rename(1, OsStr::new("old"), 1, OsStr::new("new"), 0, &caller)
            .unwrap();
        runtime.forget(node, 1);
        runtime.forget(node, 1);

        assert_eq!(mutex(&forgotten).as_slice(), [Some(PathBuf::from("/new"))]);

        assert_eq!(
            found(runtime.lookup(1, OsStr::new("new"), &caller).unwrap()),
            node
        );
        runtime.forget(node, 1);
        assert_eq!(
            mutex(&forgotten).as_slice(),
            [Some(PathBuf::from("/new")), Some(PathBuf::from("/new"))]
        );
    }

    #[test]
    fn forget_reports_none_after_the_last_name_is_unlinked() {
        let recording = RecordingFs::default();
        let forgotten = Arc::clone(&recording.forgotten_paths);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let node = found(runtime.lookup(1, OsStr::new("gone"), &caller).unwrap());

        runtime.unlink(1, OsStr::new("gone"), &caller).unwrap();
        runtime.forget(node, 1);

        assert_eq!(mutex(&forgotten).as_slice(), [None]);
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

    #[test]
    fn typed_node_state_is_retained_across_aliases_and_unlink() {
        let recording = RecordingFs::default();
        let states = Arc::clone(&recording.getattr_states);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();

        runtime.getattr(1, None, &caller).unwrap();
        assert_eq!(**mutex(&states).last().unwrap(), 0);

        let node = found(runtime.lookup(1, OsStr::new("file"), &caller).unwrap());
        runtime.getattr(node, None, &caller).unwrap();
        let original = mutex(&states).last().unwrap().clone();
        assert_eq!(*original, 1);

        runtime.link(node, 1, OsStr::new("alias"), &caller).unwrap();
        runtime
            .rename(1, OsStr::new("alias"), 1, OsStr::new("renamed"), 0, &caller)
            .unwrap();
        runtime.getattr(node, None, &caller).unwrap();
        assert!(Arc::ptr_eq(&original, mutex(&states).last().unwrap()));

        let open = runtime.open(node, 0, &caller).unwrap();
        runtime.unlink(1, OsStr::new("file"), &caller).unwrap();
        runtime.unlink(1, OsStr::new("renamed"), &caller).unwrap();
        runtime.getattr(node, Some(open.fh), &caller).unwrap();
        assert!(Arc::ptr_eq(&original, mutex(&states).last().unwrap()));
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

    #[test]
    fn current_directory_identity_accepts_a_nonstandard_name() {
        let runtime = Runtime::new(PathNodeFs::new(RecordingFs {
            alternate_dir_identity: true,
            ..Default::default()
        }));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        assert_eq!(
            runtime.readdir(1, open.fh, 0, &mut AcceptingSink, &caller),
            Ok(())
        );
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

    impl PathDirSink<()> for RecordingDirSink {
        fn add(
            &mut self,
            name: &OsStr,
            _kind: FileKind,
            _identity: PathDirIdentity<()>,
            next_offset: u64,
        ) -> bool {
            self.seen.push((name.to_os_string(), next_offset));
            self.stop_after != Some(self.seen.len())
        }
    }

    #[derive(Default)]
    struct RecordingPlusDirSink {
        seen: Vec<(OsString, u64)>,
    }

    impl PathPlusDirSink<()> for RecordingPlusDirSink {
        fn add(
            &mut self,
            name: &OsStr,
            _attr: NodeAttr,
            _identity: PathDirIdentity<()>,
            next_offset: u64,
        ) -> bool {
            self.seen.push((name.to_os_string(), next_offset));
            true
        }
    }

    fn sample_dir_buffer() -> DirBuffer<()> {
        let mut buf = DirBuffer::new();
        buf.push_dots(NodeAttr::default(), NodeAttr::default());
        buf.push(
            "a",
            FileKind::RegularFile,
            NodeAttr::default(),
            Arc::new(()),
        );
        buf.push(
            "b",
            FileKind::RegularFile,
            NodeAttr::default(),
            Arc::new(()),
        );
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
