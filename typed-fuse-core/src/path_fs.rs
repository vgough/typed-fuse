//! A synchronous path-based filesystem interface and its [`NodeFs`] adapter.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// A directory-entry sink used by [`DirBuffer::fill`].
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

/// A directory-entry sink used by [`DirBuffer::fill_plus`].
pub trait PathPlusDirSink<S> {
    /// Adds one entry with its attributes, or `None` for an entry added by
    /// [`DirBuffer::push`] without them. `next_offset` is the resume cookie
    /// the kernel will hand back to continue after this entry. Returns
    /// `false` once the buffer is full, at which point the caller must stop
    /// iterating.
    fn add(
        &mut self,
        name: &OsStr,
        attr: Option<NodeAttr>,
        identity: PathDirIdentity<S>,
        next_offset: u64,
    ) -> bool;
}

/// One entry captured by [`DirBuffer`].
#[derive(Clone, Debug)]
pub struct DirEntry<S> {
    pub name: OsString,
    pub kind: FileKind,
    /// `None` for an entry added by [`DirBuffer::push`].
    pub attr: Option<NodeAttr>,
    pub identity: PathDirIdentity<S>,
}

/// An immutable snapshot of a directory's contents returned by
/// [`PathFilesystem::list_dir`].
///
/// The path adapter retains the buffer for resumed reads and replays it with
/// its own resume cookies, which also identify the listing they came from.
/// [`DirBuffer::fill`]/[`DirBuffer::fill_plus`] serve a buffer with plain
/// `next_offset = index + 1` cookies for [`NodeFs`] implementations that
/// keep one themselves.
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

    /// Appends one entry with only its kind, which is all a basic `readdir`
    /// needs. If the adapter serves the entry through `readdirplus`, it calls
    /// [`PathFilesystem::lookup`] for the attributes, and skips the entry if
    /// the lookup finds nothing.
    pub fn push(&mut self, name: impl Into<OsString>, kind: FileKind, state: Arc<S>) {
        self.entries.push(DirEntry {
            name: name.into(),
            kind,
            attr: None,
            identity: PathDirIdentity::Child(state),
        });
    }

    /// Appends one entry with its attributes, which `readdirplus` sends to
    /// the kernel without a lookup. The kind is taken from `attr`. Use this
    /// when the listing already has the attributes.
    pub fn push_plus(&mut self, name: impl Into<OsString>, attr: NodeAttr, state: Arc<S>) {
        self.entries.push(DirEntry {
            name: name.into(),
            kind: attr.kind,
            attr: Some(attr),
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
            attr: Some(self_attr),
            identity: PathDirIdentity::Current,
        });
        self.entries.push(DirEntry {
            name: OsString::from(".."),
            kind: FileKind::Directory,
            attr: Some(parent_attr),
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
    /// path-adapter `readdir` semantics.
    pub fn fill(&self, offset: u64, sink: &mut dyn PathDirSink<S>) {
        replay(offset, self.entries.len(), |entry, next_offset| {
            let entry = &self.entries[entry];
            sink.add(&entry.name, entry.kind, entry.identity.clone(), next_offset)
        });
    }

    /// Replays entries from `offset` onward into `sink`, per
    /// path-adapter `readdirplus` semantics.
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
    /// Builds a node reference directly; intended for driving a
    /// [`PathFilesystem`] in tests without a mounted session.
    pub fn new(path: Option<&'a Path>, state: &'a Arc<S>) -> Self {
        Self { path, state }
    }

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
    /// Per-open-directory data, such as a backing directory file descriptor.
    /// Use `()` if not needed. The adapter stores directory listings itself.
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
    /// Set to `true` to let the adapter register `readdirplus`, allowing the
    /// kernel to populate its attribute cache from directory listings instead
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
    /// is `None` if the open file's last known name has been unlinked. In
    /// that case `handle` is one of the node's open handles even when the
    /// kernel named none; it may belong to another process and have been
    /// opened with different flags, so it does not show the caller's access.
    fn getattr(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: Option<&Self::Handle>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno>;

    /// Applies the `Some` fields of `set` to `path`, returning the resulting
    /// attributes. `handle` and `path` are as for
    /// [`getattr`](PathFilesystem::getattr).
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

    /// Opens the directory at `path`, performing access checks and creating
    /// any per-open state. Directory contents are obtained lazily through
    /// [`PathFilesystem::list_dir`].
    fn opendir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::DirHandle>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// Lists the directory's current contents.
    ///
    /// Include `.` and `..` when desired, usually through
    /// [`DirBuffer::push_dots`]. Add children with [`DirBuffer::push`], or
    /// with [`DirBuffer::push_plus`] when their attributes are at hand. The
    /// adapter calls this on the first read and
    /// whenever a read starts again at offset zero, then retains the returned
    /// buffer for resumed reads. It is never called directly by the kernel.
    fn list_dir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::DirHandle,
        caller: &Caller,
    ) -> Result<DirBuffer<Self::NodeState>, Errno> {
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
    entry_attr: Mutex<Option<NodeAttr>>,
    /// Counts namespace mutations routed through the adapter that add,
    /// remove, or replace a name in this directory.
    entries_epoch: AtomicU64,
}

impl<S> PathNode<S> {
    fn new(key: u64, state: Arc<S>) -> Self {
        Self {
            key,
            state,
            entry_attr: Mutex::new(None),
            entries_epoch: AtomicU64::new(0),
        }
    }

    /// Records a change to this directory's names. Callers hold the
    /// adapter's `operations` lock exclusively, which orders the bump against
    /// listings taken under the shared lock.
    fn entries_changed(&self) {
        self.entries_epoch.fetch_add(1, Ordering::Relaxed);
    }

    fn entries_epoch(&self) -> u64 {
        self.entries_epoch.load(Ordering::Relaxed)
    }

    fn set_entry_attr(&self, attr: NodeAttr) {
        *mutex(&self.entry_attr) = Some(attr);
    }

    fn take_entry_attr(&self) -> Option<NodeAttr> {
        mutex(&self.entry_attr).take()
    }
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
        let id = cx.insert(PathNode::new(key, state), parent);
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

/// Resume cookies issued by [`PathNodeFs`] hold `index + 1` in the low
/// `COOKIE_INDEX_BITS` bits and the issuing listing's generation in the four
/// bits above, so a resumed read finds the listing it was reading even when
/// another stream has since started a newer one on the same handle. Every
/// cookie stays below 2^31, which keeps it valid for 32-bit `off_t` readers.
const COOKIE_INDEX_BITS: u32 = 27;
const COOKIE_INDEX_MASK: u64 = (1 << COOKIE_INDEX_BITS) - 1;
const COOKIE_GENERATIONS: u8 = 16;
/// The most entries a listing can hold and still fit its cookies.
const MAX_LISTING_LEN: usize = COOKIE_INDEX_MASK as usize;
/// Listings kept per directory handle for resumed reads.
const RETAINED_LISTINGS: usize = 4;

fn listing_cookie(generation: u8, index: usize) -> u64 {
    (u64::from(generation) << COOKIE_INDEX_BITS) | (index as u64 + 1)
}

/// Splits a cookie into the generation that issued it and the index to
/// resume from.
fn split_cookie(offset: u64) -> (u8, u64) {
    let generation = (offset >> COOKIE_INDEX_BITS) & u64::from(COOKIE_GENERATIONS - 1);
    (generation as u8, offset & COOKIE_INDEX_MASK)
}

/// One [`PathFilesystem::list_dir`] result held for resumed reads.
struct DirListing<S> {
    generation: u8,
    /// The directory's `entries_epoch` when the listing was taken.
    epoch: u64,
    buffer: DirBuffer<S>,
}

/// What a directory read continues.
enum DirRead<S> {
    /// Replay `listing` from index `start`. `owned` is false when the
    /// cookie's own listing was evicted and the read borrows the current one.
    Listing {
        listing: Arc<DirListing<S>>,
        start: u64,
        owned: bool,
    },
    /// The cookie's listing already ended at or before this index.
    Ended,
}

struct DirListings<S> {
    next_generation: u8,
    /// Oldest first; the last listing is the current one.
    retained: VecDeque<Arc<DirListing<S>>>,
    /// For each generation, the index at which its dropped listing ended.
    /// The kernel can repeat the read at the end of a stream (FreeBSD's
    /// `getdents` always does), and that read must stay empty rather than
    /// list the directory again.
    ended: [Option<u64>; COOKIE_GENERATIONS as usize],
}

impl<S> DirListings<S> {
    fn new() -> Self {
        Self {
            next_generation: 0,
            retained: VecDeque::with_capacity(RETAINED_LISTINGS),
            ended: [None; COOKIE_GENERATIONS as usize],
        }
    }

    fn has_ended(&self, generation: u8, index: u64) -> bool {
        self.ended[usize::from(generation)].is_some_and(|end| index >= end)
    }

    fn get(&self, generation: u8) -> Option<Arc<DirListing<S>>> {
        self.retained
            .iter()
            .find(|listing| listing.generation == generation)
            .cloned()
    }

    fn current(&self) -> Option<Arc<DirListing<S>>> {
        self.retained.back().cloned()
    }

    /// Drops `listing` after a read at index `end` sent nothing, and
    /// remembers where it ended unless it was already evicted, in which
    /// case its generation may belong to a newer listing.
    fn finish(&mut self, listing: &Arc<DirListing<S>>, end: u64) {
        let before = self.retained.len();
        self.retained
            .retain(|retained| !Arc::ptr_eq(retained, listing));
        if self.retained.len() != before {
            self.ended[usize::from(listing.generation)] = Some(end);
        }
    }

    fn push(&mut self, epoch: u64, buffer: DirBuffer<S>) -> Arc<DirListing<S>> {
        let listing = Arc::new(DirListing {
            generation: self.next_generation,
            epoch,
            buffer,
        });
        self.ended[usize::from(listing.generation)] = None;
        self.next_generation = (self.next_generation + 1) % COOKIE_GENERATIONS;
        if self.retained.len() == RETAINED_LISTINGS {
            self.retained.pop_front();
        }
        self.retained.push_back(Arc::clone(&listing));
        listing
    }
}

/// Adapter-owned state for an open path-based directory.
///
/// The filesystem's own handle is retained in `inner`. The most recent
/// listings are shared by basic and plus directory reads.
pub struct PathDirHandle<P: PathFilesystem> {
    inner: P::DirHandle,
    listings: Mutex<DirListings<P::NodeState>>,
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
        let id = mutex(&self.namespace)
            .insert(cx, parent, name, entry.state)
            .0;
        cx.get(id).ok_or(Errno::ENOENT)?.set_entry_attr(entry.attr);
        Ok(id)
    }

    /// Offers one listed entry to the kernel buffer through `add`. A child
    /// whose dentry this call inserted is removed again if `add` rejects it.
    fn add_listed_entry(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        this: NodeId,
        parent: NodeId,
        entry: &DirEntry<P::NodeState>,
        add: impl FnOnce(NodeId) -> bool,
    ) -> bool {
        let (id, inserted) = match &entry.identity {
            PathDirIdentity::Current => (this, None),
            PathDirIdentity::Parent => (parent, None),
            PathDirIdentity::Child(state) => {
                let (id, inserted) =
                    mutex(&self.namespace).insert(cx, this, &entry.name, Arc::clone(state));
                let dentry = inserted.then(|| Dentry {
                    parent: this,
                    name: entry.name.clone(),
                });
                (id, dentry)
            }
        };
        let accepted = add(id);
        if !accepted && let Some(dentry) = inserted {
            self.rollback_dentry(cx, &dentry);
        }
        accepted
    }

    fn rollback_dentry(&self, cx: &Cx<'_, PathNode<P::NodeState>>, dentry: &Dentry) {
        if let Some(id) = mutex(&self.namespace).remove(dentry) {
            cx.remove_link(id);
        }
    }

    /// Returns what a read at `offset` continues. Callers hold the
    /// `operations` lock shared.
    fn directory_listing(
        &self,
        dir: &PathNode<P::NodeState>,
        path: &Path,
        handle: &PathDirHandle<P>,
        offset: u64,
        caller: &Caller,
    ) -> Result<DirRead<P::NodeState>, Errno> {
        let (generation, start) = split_cookie(offset);
        if offset != 0 {
            let listings = mutex(&handle.listings);
            if let Some(listing) = listings.get(generation) {
                return Ok(DirRead::Listing {
                    listing,
                    start,
                    owned: true,
                });
            }
            if listings.has_ended(generation, start) {
                return Ok(DirRead::Ended);
            }
            // If the cookie's listing has been evicted, resume the current
            // one by index rather than failing the read.
            if let Some(listing) = listings.current() {
                return Ok(DirRead::Listing {
                    listing,
                    start,
                    owned: false,
                });
            }
        }

        // Filesystem I/O deliberately runs outside the handle mutex. An
        // unsuccessful refresh leaves the retained listings untouched. Name
        // changes take the `operations` lock exclusively, so the epoch can't
        // move while `list_dir` runs.
        let epoch = dir.entries_epoch();
        let buffer = self.inner.list_dir(
            PathNodeRef {
                path: Some(path),
                state: &dir.state,
            },
            &handle.inner,
            caller,
        )?;
        if buffer.len() > MAX_LISTING_LEN {
            return Err(Errno::EOVERFLOW);
        }
        Ok(DirRead::Listing {
            listing: mutex(&handle.listings).push(epoch, buffer),
            start,
            owned: true,
        })
    }

    /// Replays `listing` from `start` into `emit`, which returns `false`
    /// once the reply is full, and returns how many entries `emit` accepted.
    /// A `plus` read needs attributes for every entry it sends.
    ///
    /// A child is looked up again before it's sent if a name in the directory
    /// has changed through the adapter since the listing was taken, or if
    /// this is a `plus` read and the child was listed without attributes. A
    /// name the lookup doesn't find (`None` or `ENOENT`) is skipped, so it's
    /// neither reported nor put back into the namespace. A name it finds is
    /// sent with its current attributes and state. If the lookup fails any
    /// other way, the listed entry is sent if it's usable for this read.
    /// Otherwise the reply ends before that entry, and the error is returned
    /// if nothing was accepted, so the read that reaches it reports it.
    #[allow(clippy::too_many_arguments)]
    fn replay_listing(
        &self,
        dir: &PathNode<P::NodeState>,
        path: &Path,
        listing: &DirListing<P::NodeState>,
        start: u64,
        plus: bool,
        caller: &Caller,
        mut emit: impl FnMut(&DirEntry<P::NodeState>, u64) -> bool,
    ) -> Result<usize, Errno> {
        let stale = dir.entries_epoch() != listing.epoch;
        let entries = &listing.buffer.entries;
        let mut accepted = 0;
        let mut failure = None;
        replay(start, entries.len(), |index, _| {
            let listed = &entries[index];
            let next_offset = listing_cookie(listing.generation, index);
            let needs_lookup = matches!(listed.identity, PathDirIdentity::Child(_))
                && (stale || (plus && listed.attr.is_none()));
            let current;
            let entry = if needs_lookup {
                let parent = PathNodeRef {
                    path: Some(path),
                    state: &dir.state,
                };
                match self.inner.lookup(parent, &listed.name, caller) {
                    Ok(None) => return true,
                    Err(error) if error == Errno::ENOENT => return true,
                    Ok(Some(found)) => {
                        current = DirEntry {
                            name: listed.name.clone(),
                            kind: found.attr.kind,
                            attr: Some(found.attr),
                            identity: PathDirIdentity::Child(found.state),
                        };
                        &current
                    }
                    Err(_) if !plus || listed.attr.is_some() => listed,
                    Err(error) => {
                        failure = Some(error);
                        return false;
                    }
                }
            } else {
                listed
            };
            if !emit(entry, next_offset) {
                return false;
            }
            accepted += 1;
            true
        });
        match failure {
            Some(error) if accepted == 0 => Err(error),
            _ => Ok(accepted),
        }
    }
}

impl<P: PathFilesystem> NodeFs for PathNodeFs<P> {
    type Node = PathNode<P::NodeState>;
    type Handle = P::Handle;
    type DirHandle = PathDirHandle<P>;

    const SUPPORTS_POSIX_LOCKS: bool = P::SUPPORTS_POSIX_LOCKS;
    const SUPPORTS_FLOCK: bool = P::SUPPORTS_FLOCK;
    const SUPPORTS_READDIRPLUS: bool = P::SUPPORTS_READDIRPLUS;

    fn root(&mut self) -> Self::Node {
        PathNode::new(1, self.inner.root_state())
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

    fn entry_attr(
        &self,
        node: &PathNode<P::NodeState>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        if let Some(attr) = node.take_entry_attr() {
            Ok(attr)
        } else {
            self.getattr(node, None, caller)
        }
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

    fn wants_fallback_handle(&self, node: &PathNode<P::NodeState>) -> bool {
        self.node_path(node).is_none()
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
        parent_node.entries_changed();
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
        parent_node.entries_changed();
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
        parent_node.entries_changed();
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
        old_parent_node.entries_changed();
        new_parent_node.entries_changed();
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
        parent_node.entries_changed();
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
    ) -> Result<Opened<PathDirHandle<P>>, Errno> {
        let _g = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        let opened = self.inner.opendir(
            PathNodeRef {
                path: Some(&path),
                state: &node.state,
            },
            flags,
            caller,
        )?;
        Ok(Opened {
            handle: PathDirHandle {
                inner: opened.handle,
                listings: Mutex::new(DirListings::new()),
            },
            hints: opened.hints,
        })
    }

    fn readdir(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        node: &PathNode<P::NodeState>,
        this: NodeId,
        parent: NodeId,
        handle: &PathDirHandle<P>,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        let DirRead::Listing {
            listing,
            start,
            owned,
        } = self.directory_listing(node, &path, handle, offset, caller)?
        else {
            return Ok(());
        };
        let accepted = self.replay_listing(
            node,
            &path,
            &listing,
            start,
            false,
            caller,
            |entry, next_offset| {
                self.add_listed_entry(cx, this, parent, entry, |id| {
                    sink.add(&entry.name, id, entry.kind, next_offset)
                })
            },
        )?;
        // An empty reply ends the stream for the kernel, so the listing won't
        // be resumed again. A borrowed listing belongs to another stream.
        if accepted == 0 && owned {
            mutex(&handle.listings).finish(&listing, start);
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        cx: &Cx<'_, PathNode<P::NodeState>>,
        node: &PathNode<P::NodeState>,
        this: NodeId,
        parent: NodeId,
        handle: &PathDirHandle<P>,
        offset: u64,
        sink: &mut dyn PlusDirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _guard = read_lock(&self.operations);
        let path = self.node_path(node).ok_or(Errno::ENOENT)?;
        let DirRead::Listing {
            listing,
            start,
            owned,
        } = self.directory_listing(node, &path, handle, offset, caller)?
        else {
            return Ok(());
        };
        let accepted = self.replay_listing(
            node,
            &path,
            &listing,
            start,
            true,
            caller,
            |entry, next_offset| {
                self.add_listed_entry(cx, this, parent, entry, |id| {
                    let attr = entry
                        .attr
                        .expect("replay_listing supplies attributes for plus reads");
                    sink.add(&entry.name, id, attr, next_offset)
                })
            },
        )?;
        // An empty reply ends the stream for the kernel, so the listing won't
        // be resumed again. A borrowed listing belongs to another stream.
        if accepted == 0 && owned {
            mutex(&handle.listings).finish(&listing, start);
        }
        Ok(())
    }

    fn releasedir(
        &self,
        node: &PathNode<P::NodeState>,
        handle: PathDirHandle<P>,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let _g = read_lock(&self.operations);
        let p = self.node_path(node);
        let PathDirHandle { inner, .. } = handle;
        self.inner.releasedir(
            PathNodeRef {
                path: p.as_deref(),
                state: &node.state,
            },
            inner,
            caller,
        )
    }
    fn fsyncdir(
        &self,
        node: &PathNode<P::NodeState>,
        handle: &PathDirHandle<P>,
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
            &handle.inner,
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
        parent_node.entries_changed();
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
        parent_node.entries_changed();
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;
    use crate::{EntryReply, LookupReply, Runtime, RuntimePlusSink};

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

    #[derive(Clone)]
    struct RecordingFs {
        getattr_paths: Arc<Mutex<Vec<Option<PathBuf>>>>,
        getattr_states: Arc<Mutex<Vec<Arc<usize>>>>,
        getattr_has_handle: Arc<Mutex<Vec<bool>>>,
        setattr_paths: Arc<Mutex<Vec<Option<PathBuf>>>>,
        setattr_has_handle: Arc<Mutex<Vec<bool>>>,
        forgotten_paths: Arc<Mutex<Vec<Option<PathBuf>>>>,
        next_state: Arc<AtomicUsize>,
        removed_names: Arc<Mutex<BTreeSet<OsString>>>,
        directory_names: Arc<Mutex<Vec<OsString>>>,
        list_calls: Arc<AtomicUsize>,
        fail_next_list: Arc<AtomicBool>,
        list_barrier: Option<Arc<Barrier>>,
        include_dots: bool,
        kind_only: bool,
        removed_lookup_fails: bool,
        failing_lookups: Arc<Mutex<BTreeSet<OsString>>>,
        dir_handle: usize,
        fsyncdir_handles: Arc<Mutex<Vec<usize>>>,
        releasedir_handles: Arc<Mutex<Vec<usize>>>,
    }

    impl Default for RecordingFs {
        fn default() -> Self {
            Self {
                getattr_paths: Arc::default(),
                getattr_states: Arc::default(),
                getattr_has_handle: Arc::default(),
                setattr_paths: Arc::default(),
                setattr_has_handle: Arc::default(),
                forgotten_paths: Arc::default(),
                next_state: Arc::default(),
                removed_names: Arc::default(),
                directory_names: Arc::new(Mutex::new(vec![OsString::from("enumerated")])),
                list_calls: Arc::default(),
                fail_next_list: Arc::default(),
                list_barrier: None,
                include_dots: false,
                kind_only: false,
                removed_lookup_fails: false,
                failing_lookups: Arc::default(),
                dir_handle: 41,
                fsyncdir_handles: Arc::default(),
                releasedir_handles: Arc::default(),
            }
        }
    }

    impl PathFilesystem for RecordingFs {
        type NodeState = usize;
        type Handle = ();
        type DirHandle = usize;

        const SUPPORTS_READDIRPLUS: bool = true;

        fn root_state(&mut self) -> Arc<Self::NodeState> {
            Arc::new(0)
        }

        fn lookup(
            &self,
            _parent: PathNodeRef<'_, Self::NodeState>,
            name: &OsStr,
            _caller: &Caller,
        ) -> Result<Option<PathEntry<Self::NodeState>>, Errno> {
            if mutex(&self.failing_lookups).contains(name) {
                return Err(Errno::EIO);
            }
            if mutex(&self.removed_names).contains(name) {
                return if self.removed_lookup_fails {
                    Err(Errno::ENOENT)
                } else {
                    Ok(None)
                };
            }
            let state = self
                .next_state
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            Ok(Some(PathEntry::new(
                NodeAttr {
                    size: state as u64,
                    ..Default::default()
                },
                Arc::new(state),
            )))
        }

        fn getattr(
            &self,
            node: PathNodeRef<'_, Self::NodeState>,
            handle: Option<&Self::Handle>,
            _caller: &Caller,
        ) -> Result<NodeAttr, Errno> {
            mutex(&self.getattr_paths).push(node.path().map(Path::to_path_buf));
            mutex(&self.getattr_states).push(node.state_arc());
            mutex(&self.getattr_has_handle).push(handle.is_some());
            Ok(NodeAttr::default())
        }

        fn setattr(
            &self,
            node: PathNodeRef<'_, Self::NodeState>,
            handle: Option<&Self::Handle>,
            _set: &SetAttr,
            _caller: &Caller,
        ) -> Result<NodeAttr, Errno> {
            mutex(&self.setattr_paths).push(node.path().map(Path::to_path_buf));
            mutex(&self.setattr_has_handle).push(handle.is_some());
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
            name: &OsStr,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            mutex(&self.removed_names).insert(name.to_os_string());
            Ok(())
        }

        fn create(
            &self,
            _parent: PathNodeRef<'_, Self::NodeState>,
            name: &OsStr,
            _mode: u32,
            _umask: u32,
            _flags: i32,
            _caller: &Caller,
        ) -> Result<(PathEntry<Self::NodeState>, Opened<Self::Handle>), Errno> {
            mutex(&self.removed_names).remove(name);
            let state = self.next_state.fetch_add(1, Ordering::Relaxed) + 1;
            Ok((
                PathEntry::new(NodeAttr::default(), Arc::new(state)),
                Opened::new(()),
            ))
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
            Ok(Opened::new(self.dir_handle).cache_readdir(true))
        }

        fn list_dir(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            _handle: &Self::DirHandle,
            _caller: &Caller,
        ) -> Result<DirBuffer<Self::NodeState>, Errno> {
            let call = self.list_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(barrier) = &self.list_barrier {
                barrier.wait();
            }
            if self.fail_next_list.swap(false, Ordering::SeqCst) {
                return Err(Errno::EIO);
            }
            let mut listing = DirBuffer::new();
            if self.include_dots {
                listing.push_dots(NodeAttr::default(), NodeAttr::default());
            }
            for name in mutex(&self.directory_names).iter() {
                let state = Arc::new(10_000 + call);
                if self.kind_only {
                    listing.push(name, FileKind::RegularFile, state);
                } else {
                    let attr = NodeAttr {
                        kind: FileKind::RegularFile,
                        ..Default::default()
                    };
                    listing.push_plus(name, attr, state);
                }
            }
            Ok(listing)
        }

        fn fsyncdir(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            handle: &Self::DirHandle,
            _datasync: bool,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            mutex(&self.fsyncdir_handles).push(*handle);
            Ok(())
        }

        fn releasedir(
            &self,
            _node: PathNodeRef<'_, Self::NodeState>,
            handle: Self::DirHandle,
            _caller: &Caller,
        ) -> Result<(), Errno> {
            mutex(&self.releasedir_handles).push(handle);
            Ok(())
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
    fn lookup_replies_with_path_entry_attributes_without_getattr() {
        let recording = RecordingFs::default();
        let getattr_paths = Arc::clone(&recording.getattr_paths);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();

        let LookupReply::Found(first) = runtime.lookup(1, OsStr::new("entry"), &caller).unwrap()
        else {
            panic!("unexpected negative lookup");
        };
        assert_eq!(first.attr.size, 1);
        assert!(mutex(&getattr_paths).is_empty());

        let LookupReply::Found(second) = runtime.lookup(1, OsStr::new("entry"), &caller).unwrap()
        else {
            panic!("unexpected negative lookup");
        };
        assert_eq!(second.ino, first.ino);
        assert_eq!(second.attr.size, 2);
        assert!(mutex(&getattr_paths).is_empty());
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

    #[test]
    fn unlinked_open_node_receives_open_handle_when_fh_is_omitted() {
        let recording = RecordingFs::default();
        let getattr_paths = Arc::clone(&recording.getattr_paths);
        let getattr_has_handle = Arc::clone(&recording.getattr_has_handle);
        let setattr_paths = Arc::clone(&recording.setattr_paths);
        let setattr_has_handle = Arc::clone(&recording.setattr_has_handle);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();

        let (entry, open) = runtime
            .create(1, OsStr::new("unlinked"), 0o600, 0, 0, &caller)
            .unwrap();
        runtime.unlink(1, OsStr::new("unlinked"), &caller).unwrap();

        runtime.getattr(entry.ino, None, &caller).unwrap();
        runtime
            .setattr(entry.ino, None, &SetAttr::default(), &caller)
            .unwrap();
        runtime.release(entry.ino, open.fh, &caller).unwrap();

        assert_eq!(mutex(&getattr_paths).as_slice(), [None]);
        assert_eq!(mutex(&getattr_has_handle).as_slice(), [true]);
        assert_eq!(mutex(&setattr_paths).as_slice(), [None]);
        assert_eq!(mutex(&setattr_has_handle).as_slice(), [true]);
    }

    #[test]
    fn linked_open_node_receives_no_handle_when_fh_is_omitted() {
        let recording = RecordingFs::default();
        let getattr_has_handle = Arc::clone(&recording.getattr_has_handle);
        let setattr_has_handle = Arc::clone(&recording.setattr_has_handle);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();

        let (entry, open) = runtime
            .create(1, OsStr::new("linked"), 0o600, 0, 0, &caller)
            .unwrap();

        runtime.getattr(entry.ino, None, &caller).unwrap();
        runtime
            .setattr(entry.ino, None, &SetAttr::default(), &caller)
            .unwrap();
        runtime.release(entry.ino, open.fh, &caller).unwrap();

        assert_eq!(mutex(&getattr_has_handle).as_slice(), [false]);
        assert_eq!(mutex(&setattr_has_handle).as_slice(), [false]);
    }

    #[derive(Default)]
    struct CollectingSink {
        seen: Vec<(OsString, NodeId, u64)>,
        accept_limit: Option<usize>,
    }

    impl DirSink for CollectingSink {
        fn add(&mut self, name: &OsStr, id: NodeId, _kind: FileKind, next_offset: u64) -> bool {
            if self.accept_limit == Some(self.seen.len()) {
                return false;
            }
            self.seen.push((name.to_os_string(), id, next_offset));
            true
        }
    }

    #[derive(Default)]
    struct CollectingPlusSink {
        seen: Vec<(OsString, u64, u64)>,
        sizes: Vec<u64>,
        accept_limit: Option<usize>,
    }

    impl RuntimePlusSink for CollectingPlusSink {
        fn add(&mut self, name: &OsStr, entry: EntryReply, next_offset: u64) -> bool {
            if self.accept_limit == Some(self.seen.len()) {
                return false;
            }
            self.seen
                .push((name.to_os_string(), entry.ino, next_offset));
            self.sizes.push(entry.attr.size);
            true
        }
    }

    #[test]
    fn opendir_is_lazy_and_preserves_hints() {
        let recording = RecordingFs::default();
        let list_calls = Arc::clone(&recording.list_calls);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        assert!(open.hints.cache_readdir);
        assert_eq!(list_calls.load(Ordering::SeqCst), 0);

        runtime
            .readdir(1, open.fh, 0, &mut CollectingSink::default(), &caller)
            .unwrap();
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn offset_zero_refreshes_the_listing() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("old")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();

        let mut first = CollectingSink::default();
        runtime.readdir(1, open.fh, 0, &mut first, &caller).unwrap();
        *mutex(&names) = vec![OsString::from("new")];
        let mut second = CollectingSink::default();
        runtime
            .readdir(1, open.fh, 0, &mut second, &caller)
            .unwrap();

        assert_eq!(first.seen[0].0, OsStr::new("old"));
        assert_eq!(second.seen[0].0, OsStr::new("new"));
        assert_eq!(list_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn nonzero_offset_resumes_the_existing_listing() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();

        let mut first = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime.readdir(1, open.fh, 0, &mut first, &caller).unwrap();
        *mutex(&names) = vec![OsString::from("changed")];
        let mut resumed = CollectingSink::default();
        runtime
            .readdir(1, open.fh, 1, &mut resumed, &caller)
            .unwrap();

        assert_eq!(first.seen[0].0, OsStr::new("a"));
        assert_eq!(resumed.seen[0].0, OsStr::new("b"));
        assert_eq!(resumed.seen[0].2, 2);
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nonzero_offset_without_a_listing_lists_once() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut sink = CollectingSink::default();

        runtime.readdir(1, open.fh, 1, &mut sink, &caller).unwrap();

        assert_eq!(sink.seen[0].0, OsStr::new("b"));
        assert_eq!(sink.seen[0].2, 2);
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn readdir_and_readdirplus_share_a_listing() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut basic = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime.readdir(1, open.fh, 0, &mut basic, &caller).unwrap();
        *mutex(&names) = vec![OsString::from("changed")];
        let mut plus = CollectingPlusSink::default();

        runtime
            .readdirplus(1, open.fh, 1, &mut plus, &caller)
            .unwrap();

        assert_eq!(plus.seen[0].0, OsStr::new("b"));
        assert_eq!(plus.seen[0].2, 2);
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_refresh_leaves_the_previous_listing_in_place() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let fail_next = Arc::clone(&recording.fail_next_list);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        runtime
            .readdir(
                1,
                open.fh,
                0,
                &mut CollectingSink {
                    accept_limit: Some(1),
                    ..Default::default()
                },
                &caller,
            )
            .unwrap();
        fail_next.store(true, Ordering::SeqCst);

        assert_eq!(
            runtime.readdir(1, open.fh, 0, &mut CollectingSink::default(), &caller,),
            Err(Errno::EIO)
        );
        let mut resumed = CollectingSink::default();
        runtime
            .readdir(1, open.fh, 1, &mut resumed, &caller)
            .unwrap();
        assert_eq!(resumed.seen[0].0, OsStr::new("b"));
    }

    #[test]
    fn rejected_sink_entry_is_rolled_back() {
        let recording = RecordingFs::default();
        let states = Arc::clone(&recording.getattr_states);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        runtime
            .readdir(
                1,
                open.fh,
                0,
                &mut CollectingSink {
                    accept_limit: Some(0),
                    ..Default::default()
                },
                &caller,
            )
            .unwrap();

        let node = found(
            runtime
                .lookup(1, OsStr::new("enumerated"), &caller)
                .unwrap(),
        );
        runtime.getattr(node, None, &caller).unwrap();
        assert_eq!(**mutex(&states).last().unwrap(), 1);
    }

    #[test]
    fn dot_entries_use_current_and_parent_identities() {
        let runtime = Runtime::new(PathNodeFs::new(RecordingFs {
            include_dots: true,
            ..Default::default()
        }));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut sink = CollectingSink::default();
        runtime.readdir(1, open.fh, 0, &mut sink, &caller).unwrap();
        assert_eq!(sink.seen[0], (OsString::from("."), NodeId::ROOT, 1));
        assert_eq!(sink.seen[1], (OsString::from(".."), NodeId::ROOT, 2));
    }

    #[test]
    fn directory_lifecycle_receives_the_inner_handle() {
        let recording = RecordingFs::default();
        let fsyncdir_handles = Arc::clone(&recording.fsyncdir_handles);
        let releasedir_handles = Arc::clone(&recording.releasedir_handles);
        let expected = recording.dir_handle;
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();

        runtime.fsyncdir(1, open.fh, true, &caller).unwrap();
        runtime.releasedir(1, open.fh, &caller).unwrap();

        assert_eq!(mutex(&fsyncdir_handles).as_slice(), [expected]);
        assert_eq!(mutex(&releasedir_handles).as_slice(), [expected]);
    }

    #[test]
    fn concurrent_offset_zero_reads_each_get_a_complete_listing() {
        let barrier = Arc::new(Barrier::new(2));
        let recording = RecordingFs {
            directory_names: Arc::new(Mutex::new(vec![OsString::from("a"), OsString::from("b")])),
            list_barrier: Some(Arc::clone(&barrier)),
            ..Default::default()
        };
        let list_calls = Arc::clone(&recording.list_calls);
        let runtime = Arc::new(Runtime::new(PathNodeFs::new(recording)));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                thread::spawn(move || {
                    let mut sink = CollectingSink::default();
                    runtime
                        .readdir(1, open.fh, 0, &mut sink, &Caller::default())
                        .unwrap();
                    sink.seen
                        .into_iter()
                        .map(|(name, _, offset)| (name, offset))
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let mut generations = Vec::new();
        for worker in workers {
            let seen = worker.join().unwrap();
            let (generation, _) = split_cookie(seen[0].1);
            assert_eq!(
                seen,
                vec![
                    (OsString::from("a"), listing_cookie(generation, 0)),
                    (OsString::from("b"), listing_cookie(generation, 1)),
                ]
            );
            generations.push(generation);
        }
        assert_ne!(generations[0], generations[1]);
        assert_eq!(list_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cookies_carry_the_generation_and_stay_below_2_31() {
        assert_eq!(listing_cookie(0, 0), 1);
        let largest = listing_cookie(COOKIE_GENERATIONS - 1, MAX_LISTING_LEN - 1);
        assert!(largest < 1 << 31);
        assert_eq!(
            split_cookie(largest),
            (COOKIE_GENERATIONS - 1, MAX_LISTING_LEN as u64)
        );
        assert_eq!(split_cookie(listing_cookie(5, 41)), (5, 42));
    }

    #[test]
    fn listing_generations_wrap_and_only_recent_listings_are_kept() {
        let mut listings = DirListings::<()>::new();
        for _ in 0..COOKIE_GENERATIONS {
            listings.push(0, DirBuffer::new());
        }
        assert_eq!(listings.retained.len(), RETAINED_LISTINGS);
        assert!(listings.get(0).is_none());
        assert_eq!(
            listings.current().unwrap().generation,
            COOKIE_GENERATIONS - 1
        );
        assert_eq!(listings.push(0, DirBuffer::new()).generation, 0);
    }

    #[test]
    fn interleaved_streams_resume_their_own_listing() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();

        let mut first = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime.readdir(1, open.fh, 0, &mut first, &caller).unwrap();
        *mutex(&names) = vec![
            OsString::from("x"),
            OsString::from("y"),
            OsString::from("z"),
        ];
        let mut second = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime
            .readdir(1, open.fh, 0, &mut second, &caller)
            .unwrap();

        let mut first_rest = CollectingSink::default();
        runtime
            .readdir(1, open.fh, first.seen[0].2, &mut first_rest, &caller)
            .unwrap();
        let mut second_rest = CollectingSink::default();
        runtime
            .readdir(1, open.fh, second.seen[0].2, &mut second_rest, &caller)
            .unwrap();

        let names_of = |sink: &CollectingSink| {
            sink.seen
                .iter()
                .map(|(name, _, _)| name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names_of(&first), [OsStr::new("a")]);
        assert_eq!(names_of(&first_rest), [OsStr::new("b")]);
        assert_eq!(names_of(&second), [OsStr::new("x")]);
        assert_eq!(names_of(&second_rest), [OsStr::new("y"), OsStr::new("z")]);
        assert_eq!(list_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn evicted_listing_resumes_the_current_one_by_index() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut first = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime.readdir(1, open.fh, 0, &mut first, &caller).unwrap();

        *mutex(&names) = vec![OsString::from("p"), OsString::from("q")];
        for _ in 0..RETAINED_LISTINGS {
            runtime
                .readdir(1, open.fh, 0, &mut CollectingSink::default(), &caller)
                .unwrap();
        }
        let mut resumed = CollectingSink::default();
        runtime
            .readdir(1, open.fh, first.seen[0].2, &mut resumed, &caller)
            .unwrap();

        assert_eq!(resumed.seen.len(), 1);
        assert_eq!(resumed.seen[0].0, OsStr::new("q"));
    }

    #[test]
    fn entry_unlinked_between_reads_is_not_replayed_or_reinserted() {
        assert_unlinked_entry_is_not_replayed(RecordingFs::default());
    }

    #[test]
    fn entry_whose_lookup_fails_with_enoent_is_not_replayed() {
        assert_unlinked_entry_is_not_replayed(RecordingFs {
            removed_lookup_fails: true,
            ..Default::default()
        });
    }

    fn assert_unlinked_entry_is_not_replayed(recording: RecordingFs) {
        let names = Arc::clone(&recording.directory_names);
        let states = Arc::clone(&recording.getattr_states);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("z")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut first = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime.readdir(1, open.fh, 0, &mut first, &caller).unwrap();

        runtime.unlink(1, OsStr::new("z"), &caller).unwrap();
        let mut resumed = CollectingSink::default();
        runtime
            .readdir(1, open.fh, first.seen[0].2, &mut resumed, &caller)
            .unwrap();
        assert!(resumed.seen.is_empty());

        // A new "z" must get its own state, not the listed one's.
        let (created, _) = runtime
            .create(1, OsStr::new("z"), 0o644, 0, 0, &caller)
            .unwrap();
        runtime.getattr(created.ino, None, &caller).unwrap();
        assert!(**mutex(&states).last().unwrap() < 10_000);
    }

    #[test]
    fn entry_replaced_between_reads_is_replayed_with_current_attributes() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let lookups = Arc::clone(&recording.next_state);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut first = CollectingPlusSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime
            .readdirplus(1, open.fh, 0, &mut first, &caller)
            .unwrap();
        let mut unchanged = CollectingPlusSink::default();
        runtime
            .readdirplus(1, open.fh, first.seen[0].2, &mut unchanged, &caller)
            .unwrap();
        assert_eq!(unchanged.sizes, [0]);
        assert_eq!(lookups.load(Ordering::SeqCst), 0);

        let other = found(runtime.lookup(1, OsStr::new("other"), &caller).unwrap());
        runtime
            .rename(1, OsStr::new("other"), 1, OsStr::new("b"), 0, &caller)
            .unwrap();
        let mut resumed = CollectingPlusSink::default();
        runtime
            .readdirplus(1, open.fh, first.seen[0].2, &mut resumed, &caller)
            .unwrap();

        assert_eq!(resumed.seen[0].0, OsStr::new("b"));
        assert_eq!(resumed.seen[0].1, other);
        assert_eq!(resumed.seen[0].2, first.seen[0].2 + 1);
        assert_eq!(resumed.sizes, [2]);
    }

    fn kind_only(names: &[&str]) -> RecordingFs {
        RecordingFs {
            kind_only: true,
            directory_names: Arc::new(Mutex::new(names.iter().map(OsString::from).collect())),
            ..Default::default()
        }
    }

    #[test]
    fn kind_only_entries_are_looked_up_for_readdirplus() {
        let recording = kind_only(&["a", "b"]);
        let lookups = Arc::clone(&recording.next_state);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut plus = CollectingPlusSink::default();

        runtime
            .readdirplus(1, open.fh, 0, &mut plus, &caller)
            .unwrap();

        assert_eq!(plus.sizes, [1, 2]);
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn kind_only_entry_the_lookup_cannot_find_is_skipped() {
        let recording = kind_only(&["a", "gone", "c"]);
        mutex(&recording.removed_names).insert(OsString::from("gone"));
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut plus = CollectingPlusSink::default();

        runtime
            .readdirplus(1, open.fh, 0, &mut plus, &caller)
            .unwrap();

        let names: Vec<_> = plus.seen.iter().map(|(name, _, _)| name.clone()).collect();
        assert_eq!(names, [OsStr::new("a"), OsStr::new("c")]);
        assert_eq!(plus.seen[1].2, 3);
    }

    #[test]
    fn basic_readdir_of_kind_only_entries_makes_no_lookups() {
        let recording = kind_only(&["a", "b"]);
        let lookups = Arc::clone(&recording.next_state);
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut sink = CollectingSink::default();

        runtime.readdir(1, open.fh, 0, &mut sink, &caller).unwrap();

        assert_eq!(sink.seen.len(), 2);
        assert_eq!(lookups.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn plus_lookup_error_ends_the_page_and_fails_the_next_read() {
        let recording = kind_only(&["a", "b", "c"]);
        let failing = Arc::clone(&recording.failing_lookups);
        let list_calls = Arc::clone(&recording.list_calls);
        mutex(&failing).insert(OsString::from("b"));
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();

        let mut first = CollectingPlusSink::default();
        runtime
            .readdirplus(1, open.fh, 0, &mut first, &caller)
            .unwrap();
        assert_eq!(first.seen.len(), 1);
        assert_eq!(first.seen[0].0, OsStr::new("a"));
        let resume = first.seen[0].2;
        assert_eq!(
            runtime.readdirplus(
                1,
                open.fh,
                resume,
                &mut CollectingPlusSink::default(),
                &caller
            ),
            Err(Errno::EIO)
        );

        // The failed read keeps the listing, so a retry resumes it.
        mutex(&failing).clear();
        let mut retried = CollectingPlusSink::default();
        runtime
            .readdirplus(1, open.fh, resume, &mut retried, &caller)
            .unwrap();
        let names: Vec<_> = retried
            .seen
            .iter()
            .map(|(name, _, _)| name.clone())
            .collect();
        assert_eq!(names, [OsStr::new("b"), OsStr::new("c")]);
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn plus_lookup_error_on_the_first_entry_fails_the_read() {
        let recording = kind_only(&["a", "b"]);
        mutex(&recording.failing_lookups).insert(OsString::from("a"));
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();

        assert_eq!(
            runtime.readdirplus(1, open.fh, 0, &mut CollectingPlusSink::default(), &caller),
            Err(Errno::EIO)
        );
    }

    #[test]
    fn empty_read_at_the_end_releases_the_listing() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut all = CollectingSink::default();
        runtime.readdir(1, open.fh, 0, &mut all, &caller).unwrap();
        let end = all.seen[1].2;

        let mut empty = CollectingSink::default();
        runtime
            .readdir(1, open.fh, end, &mut empty, &caller)
            .unwrap();
        assert!(empty.seen.is_empty());
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);

        // With no listing left, resuming lists the directory again.
        let mut resumed = CollectingSink::default();
        runtime
            .readdir(1, open.fh, all.seen[0].2, &mut resumed, &caller)
            .unwrap();
        assert_eq!(resumed.seen[0].0, OsStr::new("b"));
        assert_eq!(list_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn repeated_empty_reads_at_the_end_do_not_list_again() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![OsString::from("a"), OsString::from("b")];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut all = CollectingSink::default();
        runtime.readdir(1, open.fh, 0, &mut all, &caller).unwrap();
        let end = all.seen[1].2;
        *mutex(&names) = vec![
            OsString::from("a"),
            OsString::from("b"),
            OsString::from("c"),
        ];

        for _ in 0..2 {
            let mut empty = CollectingSink::default();
            runtime
                .readdir(1, open.fh, end, &mut empty, &caller)
                .unwrap();
            assert!(empty.seen.is_empty());
        }
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stream_ended_by_skipped_entries_stays_ended() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![
            OsString::from("a"),
            OsString::from("x"),
            OsString::from("y"),
        ];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut first = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime.readdir(1, open.fh, 0, &mut first, &caller).unwrap();
        runtime.unlink(1, OsStr::new("x"), &caller).unwrap();
        runtime.unlink(1, OsStr::new("y"), &caller).unwrap();

        for _ in 0..2 {
            let mut empty = CollectingSink::default();
            runtime
                .readdir(1, open.fh, first.seen[0].2, &mut empty, &caller)
                .unwrap();
            assert!(empty.seen.is_empty());
        }
        assert_eq!(list_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_read_through_the_fallback_keeps_the_current_listing() {
        let recording = RecordingFs::default();
        let names = Arc::clone(&recording.directory_names);
        let list_calls = Arc::clone(&recording.list_calls);
        *mutex(&names) = vec![
            OsString::from("a"),
            OsString::from("b"),
            OsString::from("c"),
        ];
        let runtime = Runtime::new(PathNodeFs::new(recording));
        let caller = Caller::default();
        let open = runtime.opendir(1, 0, &caller).unwrap();
        let mut evicted = CollectingSink::default();
        runtime
            .readdir(1, open.fh, 0, &mut evicted, &caller)
            .unwrap();
        let evicted_end = evicted.seen[2].2;

        *mutex(&names) = vec![OsString::from("p"), OsString::from("q")];
        for _ in 1..RETAINED_LISTINGS {
            runtime
                .readdir(1, open.fh, 0, &mut CollectingSink::default(), &caller)
                .unwrap();
        }
        *mutex(&names) = vec![OsString::from("r"), OsString::from("s")];
        let mut current = CollectingSink {
            accept_limit: Some(1),
            ..Default::default()
        };
        runtime
            .readdir(1, open.fh, 0, &mut current, &caller)
            .unwrap();
        let calls = list_calls.load(Ordering::SeqCst);

        // The evicted stream's end is past the current listing's end, so
        // its read borrows the current listing and sends nothing.
        let mut empty = CollectingSink::default();
        runtime
            .readdir(1, open.fh, evicted_end, &mut empty, &caller)
            .unwrap();
        assert!(empty.seen.is_empty());

        let mut resumed = CollectingSink::default();
        runtime
            .readdir(1, open.fh, current.seen[0].2, &mut resumed, &caller)
            .unwrap();
        assert_eq!(resumed.seen.len(), 1);
        assert_eq!(resumed.seen[0].0, OsStr::new("s"));
        assert_eq!(list_calls.load(Ordering::SeqCst), calls);
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
        seen: Vec<(OsString, u64, bool)>,
    }

    impl PathPlusDirSink<()> for RecordingPlusDirSink {
        fn add(
            &mut self,
            name: &OsStr,
            attr: Option<NodeAttr>,
            _identity: PathDirIdentity<()>,
            next_offset: u64,
        ) -> bool {
            self.seen
                .push((name.to_os_string(), next_offset, attr.is_some()));
            true
        }
    }

    fn sample_dir_buffer() -> DirBuffer<()> {
        let mut buf = DirBuffer::new();
        buf.push_dots(NodeAttr::default(), NodeAttr::default());
        buf.push_plus("a", NodeAttr::default(), Arc::new(()));
        buf.push("b", FileKind::RegularFile, Arc::new(()));
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
        assert_eq!(sink.seen[2], (OsString::from("a"), 3, true));
        assert_eq!(sink.seen[3], (OsString::from("b"), 4, false));
    }
}
