//! Concurrent node and handle lifetime management for [`NodeFs`].

use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::attr::{NodeAttr, SetAttr, StatFs};
use crate::errno::Errno;
use crate::node_fs::{
    Caller, ConnInfo, ConnectionCapability, DirSink, FileLock, NodeFs, NodeId, OpenHints,
    PlusDirSink, XattrReply,
};

/// A directory entry for the kernel to cache: the node's inode number and
/// generation, plus its attributes.
#[derive(Clone, Copy, Debug)]
pub struct EntryReply {
    /// Inode number of the node.
    pub ino: u64,
    /// Incremented each time `ino` is reused, so the kernel can tell an
    /// old node from a new one with the same number.
    pub generation: u64,
    /// The node's attributes.
    pub attr: NodeAttr,
}

/// The result of a successful lookup.
#[derive(Clone, Copy, Debug)]
pub enum LookupReply {
    Found(EntryReply),
    Negative,
}

/// The reply to an `open` or `opendir`.
#[derive(Clone, Copy, Debug)]
pub struct OpenReply {
    /// Handle number the kernel passes back on later requests through this
    /// open file or directory.
    pub fh: u64,
    /// Caching and I/O hints the filesystem returned with the handle.
    pub hints: OpenHints,
}

#[doc(hidden)]
pub trait RuntimePlusSink {
    fn add(&mut self, name: &OsStr, entry: EntryReply, next_offset: u64) -> bool;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The filesystem's node, shared between its table slot and any
/// [`NodeRef`] leases on it.
struct NodeRecord<N> {
    payload: N,
}

/// One inode's entry in the [`NodeTable`]. The inode stays allocated while
/// any of its counts is nonzero.
struct Slot<N> {
    record: Arc<NodeRecord<N>>,
    /// Incremented each time the inode number is reused.
    generation: u64,
    /// Directory entries naming the node.
    links: u32,
    /// Kernel lookups not yet returned by `forget`.
    lookups: u64,
    /// Open file and directory handles.
    opens: u32,
    /// Live [`NodeRef`] leases held by in-progress requests.
    leases: u32,
    /// The directory the node was last placed in; readdir reports it as `..`.
    parent: NodeId,
}

impl<N> Slot<N> {
    fn droppable(&self) -> bool {
        self.links == 0 && self.lookups == 0 && self.opens == 0 && self.leases == 0
    }
}

/// Inode allocation and lifetime metadata. It is synchronized internally by
/// [`Runtime`]; the type is public only for compatibility with earlier
/// versions and is not normally used directly.
pub struct NodeTable<N> {
    map: BTreeMap<u64, Slot<N>>,
    next_ino: u64,
    free: Vec<(u64, u64)>,
}

impl<N> NodeTable<N> {
    fn new() -> Self {
        Self {
            map: BTreeMap::new(),
            next_ino: 2,
            free: Vec::new(),
        }
    }

    fn alloc(&mut self) -> (u64, u64) {
        self.free.pop().unwrap_or_else(|| {
            let ino = self.next_ino;
            self.next_ino += 1;
            (ino, 0)
        })
    }

    fn maybe_drop(&mut self, id: NodeId) {
        let ino = id.ino();
        if self.map.get(&ino).is_some_and(Slot::droppable) {
            let generation = self.map.remove(&ino).unwrap().generation;
            self.free.push((ino, generation.wrapping_add(1)));
        }
    }
}

/// State shared by the [`Runtime`], [`Cx`] and every [`NodeRef`], so a
/// lease can update its slot after the request that took it has returned.
struct Shared<N> {
    table: Mutex<NodeTable<N>>,
}

/// A shared node lease. Keeping this value alive prevents the inode from
/// being reclaimed or reused. It dereferences to the filesystem's node.
pub struct NodeRef<N> {
    id: NodeId,
    record: Arc<NodeRecord<N>>,
    shared: Arc<Shared<N>>,
}

impl<N> Deref for NodeRef<N> {
    type Target = N;
    fn deref(&self) -> &N {
        &self.record.payload
    }
}

impl<N> Drop for NodeRef<N> {
    fn drop(&mut self) {
        let mut table = lock(&self.shared.table);
        if let Some(slot) = table.map.get_mut(&self.id.ino()) {
            if Arc::ptr_eq(&slot.record, &self.record) {
                slot.leases = slot.leases.saturating_sub(1);
            }
        }
        table.maybe_drop(self.id);
    }
}

/// Concurrent access to the runtime's node table for structural callbacks.
/// All mutation methods take `&self` and hold the metadata lock only for the
/// bookkeeping operation itself.
pub struct Cx<'a, N> {
    shared: &'a Arc<Shared<N>>,
}

impl<'a, N> Cx<'a, N> {
    pub fn get(&self, id: NodeId) -> Option<NodeRef<N>> {
        let mut table = lock(&self.shared.table);
        let slot = table.map.get_mut(&id.ino())?;
        slot.leases = slot
            .leases
            .checked_add(1)
            .expect("node lease count overflow");
        Some(NodeRef {
            id,
            record: Arc::clone(&slot.record),
            shared: Arc::clone(self.shared),
        })
    }

    pub fn contains(&self, id: NodeId) -> bool {
        lock(&self.shared.table).map.contains_key(&id.ino())
    }

    pub fn insert(&self, payload: N, parent: NodeId) -> NodeId {
        let mut table = lock(&self.shared.table);
        let (ino, generation) = table.alloc();
        table.map.insert(
            ino,
            Slot {
                record: Arc::new(NodeRecord { payload }),
                generation,
                links: 1,
                lookups: 0,
                opens: 0,
                leases: 0,
                parent,
            },
        );
        NodeId::from_ino(ino)
    }

    pub fn reparent(&self, id: NodeId, new_parent: NodeId) {
        if let Some(slot) = lock(&self.shared.table).map.get_mut(&id.ino()) {
            slot.parent = new_parent;
        }
    }

    pub fn add_link(&self, id: NodeId) {
        if let Some(slot) = lock(&self.shared.table).map.get_mut(&id.ino()) {
            slot.links = slot.links.checked_add(1).expect("node link count overflow");
        }
    }

    pub fn remove_link(&self, id: NodeId) {
        let mut table = lock(&self.shared.table);
        if let Some(slot) = table.map.get_mut(&id.ino()) {
            slot.links = slot.links.saturating_sub(1);
        }
        table.maybe_drop(id);
    }
}

/// Lease bookkeeping for a [`HandleRecord`].
struct HandleState {
    /// Live [`HandleLease`]s.
    active: u32,
    /// Set by `release` so no new leases are taken while it waits for
    /// `active` to drain.
    closing: bool,
    /// Set once the payload has been moved out, so `Drop` doesn't drop it
    /// again.
    taken: bool,
}

/// An open file or directory handle and the node it was opened on.
/// Requests borrow the payload through [`HandleLease`]s; `release` waits
/// for them to finish and then moves the payload out.
struct HandleRecord<H> {
    node: NodeId,
    payload: UnsafeCell<ManuallyDrop<H>>,
    state: Mutex<HandleState>,
    drained: Condvar,
}

// The payload is only moved after closing prevents new leases and all prior
// leases have drained. Shared access is valid because `H: Sync`.
unsafe impl<H: Send + Sync> Send for HandleRecord<H> {}
unsafe impl<H: Send + Sync> Sync for HandleRecord<H> {}

impl<H> HandleRecord<H> {
    fn new(node: NodeId, payload: H) -> Self {
        Self {
            node,
            payload: UnsafeCell::new(ManuallyDrop::new(payload)),
            state: Mutex::new(HandleState {
                active: 0,
                closing: false,
                taken: false,
            }),
            drained: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Option<HandleLease<H>> {
        let mut state = lock(&self.state);
        if state.closing {
            return None;
        }
        state.active = state
            .active
            .checked_add(1)
            .expect("handle lease count overflow");
        drop(state);
        Some(HandleLease {
            record: Arc::clone(self),
        })
    }

    fn close_and_take(&self) -> H {
        let mut state = lock(&self.state);
        state.closing = true;
        while state.active != 0 {
            state = self.drained.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        state.taken = true;
        drop(state);
        // SAFETY: the record is closed, no leases exist, and this method is
        // called only by the thread that removed the record from the table.
        unsafe { ManuallyDrop::take(&mut *self.payload.get()) }
    }
}

impl<H> Drop for HandleRecord<H> {
    fn drop(&mut self) {
        if !lock(&self.state).taken {
            // SAFETY: `drop` has exclusive access to the record, so no lease
            // can still refer to the payload.
            unsafe { ManuallyDrop::drop(&mut *self.payload.get()) };
        }
    }
}

/// Shared access to an open handle for the length of one request. It
/// dereferences to the filesystem's handle.
struct HandleLease<H> {
    record: Arc<HandleRecord<H>>,
}

impl<H> Deref for HandleLease<H> {
    type Target = H;
    fn deref(&self) -> &H {
        // SAFETY: acquiring the lease increments `active`; close waits for it.
        unsafe { &*self.record.payload.get() }
    }
}

impl<H> Drop for HandleLease<H> {
    fn drop(&mut self) {
        let mut state = lock(&self.record.state);
        state.active = state.active.saturating_sub(1);
        if state.active == 0 && state.closing {
            self.record.drained.notify_all();
        }
    }
}

/// Open handles by handle number. Files and directories share one number
/// sequence, so a handle number names at most one open handle.
struct HandleTable<F: NodeFs> {
    files: BTreeMap<u64, Arc<HandleRecord<F::Handle>>>,
    dirs: BTreeMap<u64, Arc<HandleRecord<F::DirHandle>>>,
    next_fh: u64,
}

impl<F: NodeFs> HandleTable<F> {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            dirs: BTreeMap::new(),
            next_fh: 1,
        }
    }
    fn next(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }
    fn add_file(&mut self, node: NodeId, handle: F::Handle) -> u64 {
        let fh = self.next();
        self.files
            .insert(fh, Arc::new(HandleRecord::new(node, handle)));
        fh
    }
    fn add_dir(&mut self, node: NodeId, handle: F::DirHandle) -> u64 {
        let fh = self.next();
        self.dirs
            .insert(fh, Arc::new(HandleRecord::new(node, handle)));
        fh
    }
}

/// Runs a [`NodeFs`] behind FUSE's numeric interface. It assigns inode and
/// handle numbers, tracks the kernel's lookup counts and the open handles
/// on each node, and keeps nodes and handles alive while requests are using
/// them, so callbacks for different requests can run at the same time.
pub struct Runtime<F: NodeFs> {
    fs: F,
    shared: Arc<Shared<F::Node>>,
    handles: Mutex<HandleTable<F>>,
    ttl: Duration,
    negative_ttl: Duration,
    parallel_dirops: bool,
}

impl<F: NodeFs> Runtime<F> {
    pub fn new(mut fs: F) -> Self {
        let root = fs.root();
        let mut table = NodeTable::new();
        table.map.insert(
            1,
            Slot {
                record: Arc::new(NodeRecord { payload: root }),
                generation: 0,
                links: 1,
                lookups: 0,
                opens: 0,
                leases: 0,
                parent: NodeId::ROOT,
            },
        );
        let shared = Arc::new(Shared {
            table: Mutex::new(table),
        });
        {
            let cx = Cx { shared: &shared };
            fs.populate(&cx);
        }
        Self {
            fs,
            shared,
            handles: Mutex::new(HandleTable::new()),
            ttl: Duration::from_secs(1),
            negative_ttl: Duration::from_secs(1),
            parallel_dirops: false,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
    pub fn negative_ttl(&self) -> Duration {
        self.negative_ttl
    }
    pub fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }
    pub fn set_negative_ttl(&mut self, ttl: Duration) {
        self.negative_ttl = ttl;
    }
    #[doc(hidden)]
    pub fn set_parallel_dirops(&mut self, enabled: bool) {
        self.parallel_dirops = enabled;
    }
    fn cx(&self) -> Cx<'_, F::Node> {
        Cx {
            shared: &self.shared,
        }
    }
    fn node(&self, ino: u64) -> Result<NodeRef<F::Node>, Errno> {
        self.cx().get(NodeId::from_ino(ino)).ok_or(Errno::ENOENT)
    }

    fn entry_for(&self, id: NodeId, caller: &Caller) -> Result<EntryReply, Errno> {
        let node = self.cx().get(id).ok_or(Errno::ENOENT)?;
        let attr = self.fs.entry_attr(&node, caller)?;
        let mut table = lock(&self.shared.table);
        let slot = table.map.get_mut(&id.ino()).ok_or(Errno::ENOENT)?;
        let generation = slot.generation;
        slot.lookups = slot.lookups.checked_add(1).expect("lookup count overflow");
        Ok(EntryReply {
            ino: id.ino(),
            generation,
            attr,
        })
    }

    fn retain_lookup(&self, id: NodeId) -> Result<(), Errno> {
        let mut table = lock(&self.shared.table);
        let slot = table.map.get_mut(&id.ino()).ok_or(Errno::ENOENT)?;
        slot.lookups = slot.lookups.checked_add(1).expect("lookup count overflow");
        Ok(())
    }

    fn plus_entry(&self, id: NodeId, attr: NodeAttr) -> Result<EntryReply, Errno> {
        let table = lock(&self.shared.table);
        let slot = table.map.get(&id.ino()).ok_or(Errno::ENOENT)?;
        Ok(EntryReply {
            ino: id.ino(),
            generation: slot.generation,
            attr,
        })
    }

    pub fn init(&self, conn: &mut ConnInfo) {
        if self.parallel_dirops {
            conn.set_enabled(ConnectionCapability::ParallelDirectoryOperations, true);
        }
        self.fs.init(conn);
    }
    pub fn destroy(&self) {
        self.fs.destroy();
    }
    pub fn forget(&self, ino: u64, nlookup: u64) {
        let id = NodeId::from_ino(ino);
        let forgotten = {
            let mut table = lock(&self.shared.table);
            let Some(slot) = table.map.get_mut(&ino) else {
                return;
            };
            let had_lookups = slot.lookups != 0;
            slot.lookups = slot.lookups.saturating_sub(nlookup);
            if had_lookups && slot.lookups == 0 {
                slot.leases = slot
                    .leases
                    .checked_add(1)
                    .expect("node lease count overflow");
                Some(NodeRef {
                    id,
                    record: Arc::clone(&slot.record),
                    shared: Arc::clone(&self.shared),
                })
            } else {
                None
            }
        };
        if let Some(node) = forgotten {
            self.fs.forget(&node);
        } else {
            lock(&self.shared.table).maybe_drop(id);
        }
    }
    pub fn statfs(&self, _ino: u64, caller: &Caller) -> Result<StatFs, Errno> {
        self.fs.statfs(caller)
    }
    pub fn getattr(&self, ino: u64, fh: Option<u64>, caller: &Caller) -> Result<NodeAttr, Errno> {
        let n = self.node(ino)?;
        let h = self.optional_file(ino, &n, fh)?;
        self.fs.getattr(&n, h.as_deref(), caller)
    }
    pub fn setattr(
        &self,
        ino: u64,
        fh: Option<u64>,
        set: &SetAttr,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let n = self.node(ino)?;
        let h = self.optional_file(ino, &n, fh)?;
        self.fs.setattr(&n, h.as_deref(), set, caller)
    }
    pub fn readlink(&self, ino: u64, caller: &Caller) -> Result<PathBuf, Errno> {
        let n = self.node(ino)?;
        self.fs.readlink(&n, caller)
    }
    pub fn access(&self, ino: u64, mask: i32, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        self.fs.access(&n, mask, caller)
    }
    pub fn setxattr(
        &self,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let n = self.node(ino)?;
        self.fs.setxattr(&n, name, value, flags, caller)
    }
    pub fn getxattr(
        &self,
        ino: u64,
        name: &OsStr,
        size: usize,
        caller: &Caller,
    ) -> Result<XattrReply, Errno> {
        let n = self.node(ino)?;
        self.fs.getxattr(&n, name, size, caller)
    }
    pub fn listxattr(&self, ino: u64, size: usize, caller: &Caller) -> Result<XattrReply, Errno> {
        let n = self.node(ino)?;
        self.fs.listxattr(&n, size, caller)
    }
    pub fn removexattr(&self, ino: u64, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        self.fs.removexattr(&n, name, caller)
    }

    fn add_open(&self, ino: u64) -> Result<(), Errno> {
        let mut t = lock(&self.shared.table);
        let s = t.map.get_mut(&ino).ok_or(Errno::ENOENT)?;
        s.opens = s.opens.checked_add(1).expect("open count overflow");
        Ok(())
    }
    fn remove_open(&self, ino: u64) {
        let mut t = lock(&self.shared.table);
        if let Some(s) = t.map.get_mut(&ino) {
            s.opens = s.opens.saturating_sub(1);
        }
        t.maybe_drop(NodeId::from_ino(ino));
    }
    fn file(&self, ino: u64, fh: u64) -> Result<HandleLease<F::Handle>, Errno> {
        let record = lock(&self.handles)
            .files
            .get(&fh)
            .cloned()
            .ok_or(Errno::EBADF)?;
        if record.node.ino() != ino {
            return Err(Errno::EBADF);
        }
        record.acquire().ok_or(Errno::EBADF)
    }
    /// Leases the file handle `getattr` or `setattr` should pass for `ino`:
    /// the one `fh` names, or else, if the filesystem
    /// [wants one](NodeFs::wants_fallback_handle), the handle most recently
    /// opened on `ino`.
    fn optional_file(
        &self,
        ino: u64,
        node: &F::Node,
        fh: Option<u64>,
    ) -> Result<Option<HandleLease<F::Handle>>, Errno> {
        if let Some(fh) = fh {
            return self.file(ino, fh).map(Some);
        }
        // `opens` also counts directory handles, so it only rules out the
        // scan. The table lock is released before calling the filesystem,
        // which may take its own locks.
        let open = lock(&self.shared.table)
            .map
            .get(&ino)
            .is_some_and(|slot| slot.opens > 0);
        if !open || !self.fs.wants_fallback_handle(node) {
            return Ok(None);
        }
        Ok(lock(&self.handles)
            .files
            .values()
            .rev()
            .filter(|record| record.node.ino() == ino)
            .find_map(HandleRecord::acquire))
    }
    fn dir(&self, ino: u64, fh: u64) -> Result<HandleLease<F::DirHandle>, Errno> {
        let record = lock(&self.handles)
            .dirs
            .get(&fh)
            .cloned()
            .ok_or(Errno::EBADF)?;
        if record.node.ino() != ino {
            return Err(Errno::EBADF);
        }
        record.acquire().ok_or(Errno::EBADF)
    }

    pub fn open(&self, ino: u64, flags: i32, caller: &Caller) -> Result<OpenReply, Errno> {
        let node = self.node(ino)?;
        let opened = self.fs.open(&node, flags, caller)?;
        self.add_open(ino)?;
        let fh = lock(&self.handles).add_file(NodeId::from_ino(ino), opened.handle);
        Ok(OpenReply {
            fh,
            hints: opened.hints,
        })
    }

    /// Runs the reply continuation before releasing node and handle leases,
    /// allowing a borrowed `Cow` to be sent without copying.
    pub fn read<R>(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        size: usize,
        caller: &Caller,
        reply: impl FnOnce(Result<Cow<'_, [u8]>, Errno>) -> R,
    ) -> R {
        let node = match self.node(ino) {
            Ok(v) => v,
            Err(e) => return reply(Err(e)),
        };
        let handle = match self.file(ino, fh) {
            Ok(v) => v,
            Err(e) => return reply(Err(e)),
        };
        reply(self.fs.read(&node, &handle, offset, size, caller))
    }

    pub fn write(
        &self,
        ino: u64,
        fh: u64,
        data: &[u8],
        offset: u64,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.write(&n, &h, data, offset, caller)
    }
    pub fn flush(&self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.flush(&n, &h, caller)
    }
    pub fn fsync(&self, ino: u64, fh: u64, datasync: bool, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.fsync(&n, &h, datasync, caller)
    }
    pub fn fallocate(
        &self,
        ino: u64,
        fh: u64,
        mode: i32,
        offset: u64,
        length: u64,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.fallocate(&n, &h, mode, offset, length, caller)
    }
    pub fn lseek(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        whence: i32,
        caller: &Caller,
    ) -> Result<u64, Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.lseek(&n, &h, offset, whence, caller)
    }

    pub fn getlk(
        &self,
        ino: u64,
        fh: u64,
        owner: u64,
        requested: FileLock,
        caller: &Caller,
    ) -> Result<FileLock, Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.getlk(&n, &h, owner, requested, caller)
    }

    pub fn setlk(
        &self,
        ino: u64,
        fh: u64,
        owner: u64,
        lock: FileLock,
        sleep: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.setlk(&n, &h, owner, lock, sleep, caller)
    }

    pub fn flock(&self, ino: u64, fh: u64, operation: i32, caller: &Caller) -> Result<(), Errno> {
        let n = self.node(ino)?;
        let h = self.file(ino, fh)?;
        self.fs.flock(&n, &h, operation, caller)
    }

    pub fn release(&self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let mut handles = lock(&self.handles);
        if handles.files.get(&fh).is_none_or(|r| r.node.ino() != ino) {
            return Err(Errno::EBADF);
        }
        let record = handles.files.remove(&fh).unwrap();
        drop(handles);
        let node = self.node(ino)?;
        self.remove_open(ino);
        let handle = record.close_and_take();
        self.fs.release(&node, handle, caller)
    }

    pub fn opendir(&self, ino: u64, flags: i32, caller: &Caller) -> Result<OpenReply, Errno> {
        let node = self.node(ino)?;
        let opened = self.fs.opendir(&node, flags, caller)?;
        self.add_open(ino)?;
        let fh = lock(&self.handles).add_dir(NodeId::from_ino(ino), opened.handle);
        Ok(OpenReply {
            fh,
            hints: opened.hints,
        })
    }

    pub fn readdir(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        sink: &mut dyn DirSink,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let node = self.node(ino)?;
        let parent = lock(&self.shared.table)
            .map
            .get(&ino)
            .map(|s| s.parent)
            .ok_or(Errno::ENOENT)?;
        let h = self.dir(ino, fh)?;
        self.fs.readdir(
            &self.cx(),
            &node,
            NodeId::from_ino(ino),
            parent,
            &h,
            offset,
            sink,
            caller,
        )
    }

    pub fn readdirplus(
        &self,
        ino: u64,
        fh: u64,
        offset: u64,
        sink: &mut dyn RuntimePlusSink,
        caller: &Caller,
    ) -> Result<Vec<NodeId>, Errno> {
        let node = self.node(ino)?;
        let parent = lock(&self.shared.table)
            .map
            .get(&ino)
            .map(|s| s.parent)
            .ok_or(Errno::ENOENT)?;
        let h = self.dir(ino, fh)?;
        // Forwards entries to `inner` and records the ones the kernel
        // accepted, whose lookups are rolled back if the listing fails.
        struct Accounting<'a, F: NodeFs> {
            rt: &'a Runtime<F>,
            inner: &'a mut dyn RuntimePlusSink,
            accepted: Vec<NodeId>,
        }
        impl<F: NodeFs> PlusDirSink for Accounting<'_, F> {
            fn add(&mut self, name: &OsStr, id: NodeId, attr: NodeAttr, next_offset: u64) -> bool {
                let Ok(entry) = self.rt.plus_entry(id, attr) else {
                    return false;
                };
                if !self.inner.add(name, entry, next_offset) {
                    return false;
                }
                if name != OsStr::new(".")
                    && name != OsStr::new("..")
                    && self.rt.retain_lookup(id).is_ok()
                {
                    self.accepted.push(id);
                }
                true
            }
        }
        let mut accounting = Accounting {
            rt: self,
            inner: sink,
            accepted: Vec::new(),
        };
        match self.fs.readdirplus(
            &self.cx(),
            &node,
            NodeId::from_ino(ino),
            parent,
            &h,
            offset,
            &mut accounting,
            caller,
        ) {
            Ok(()) => Ok(accounting.accepted),
            Err(e) => {
                for id in accounting.accepted {
                    self.forget(id.ino(), 1);
                }
                Err(e)
            }
        }
    }

    pub fn fsyncdir(
        &self,
        ino: u64,
        fh: u64,
        datasync: bool,
        caller: &Caller,
    ) -> Result<(), Errno> {
        let node = self.node(ino)?;
        let h = self.dir(ino, fh)?;
        self.fs.fsyncdir(&node, &h, datasync, caller)
    }

    pub fn releasedir(&self, ino: u64, fh: u64, caller: &Caller) -> Result<(), Errno> {
        let mut handles = lock(&self.handles);
        if handles.dirs.get(&fh).is_none_or(|r| r.node.ino() != ino) {
            return Err(Errno::EBADF);
        }
        let record = handles.dirs.remove(&fh).unwrap();
        drop(handles);
        let node = self.node(ino)?;
        self.remove_open(ino);
        let handle = record.close_and_take();
        self.fs.releasedir(&node, handle, caller)
    }

    pub fn lookup(&self, parent: u64, name: &OsStr, caller: &Caller) -> Result<LookupReply, Errno> {
        match self
            .fs
            .lookup(&self.cx(), NodeId::from_ino(parent), name, caller)?
        {
            Some(id) => Ok(LookupReply::Found(self.entry_for(id, caller)?)),
            None => Ok(LookupReply::Negative),
        }
    }
    pub fn mknod(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self.fs.mknod(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            mode,
            rdev,
            umask,
            caller,
        )?;
        self.entry_for(id, caller)
    }
    pub fn mkdir(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self.fs.mkdir(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            mode,
            umask,
            caller,
        )?;
        self.entry_for(id, caller)
    }
    pub fn symlink(
        &self,
        parent: u64,
        name: &OsStr,
        target: &Path,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self
            .fs
            .symlink(&self.cx(), NodeId::from_ino(parent), name, target, caller)?;
        self.entry_for(id, caller)
    }
    pub fn link(
        &self,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        caller: &Caller,
    ) -> Result<EntryReply, Errno> {
        let id = self.fs.link(
            &self.cx(),
            NodeId::from_ino(ino),
            NodeId::from_ino(newparent),
            newname,
            caller,
        )?;
        self.entry_for(id, caller)
    }
    pub fn unlink(&self, parent: u64, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        self.fs
            .unlink(&self.cx(), NodeId::from_ino(parent), name, caller)
    }
    pub fn rmdir(&self, parent: u64, name: &OsStr, caller: &Caller) -> Result<(), Errno> {
        self.fs
            .rmdir(&self.cx(), NodeId::from_ino(parent), name, caller)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        caller: &Caller,
    ) -> Result<(), Errno> {
        self.fs.rename(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            NodeId::from_ino(newparent),
            newname,
            flags,
            caller,
        )
    }

    pub fn create(
        &self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        caller: &Caller,
    ) -> Result<(EntryReply, OpenReply), Errno> {
        let (id, opened) = self.fs.create(
            &self.cx(),
            NodeId::from_ino(parent),
            name,
            mode,
            umask,
            flags,
            caller,
        )?;
        let entry = self.entry_for(id, caller)?;
        self.add_open(id.ino())?;
        let fh = lock(&self.handles).add_file(id, opened.handle);
        Ok((
            entry,
            OpenReply {
                fh,
                hints: opened.hints,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_file_range(
        &self,
        ino_in: u64,
        off_in: u64,
        ino_out: u64,
        off_out: u64,
        len: u64,
        flags: i32,
        caller: &Caller,
    ) -> Result<usize, Errno> {
        self.fs.copy_file_range(
            &self.cx(),
            NodeId::from_ino(ino_in),
            off_in,
            NodeId::from_ino(ino_out),
            off_out,
            len,
            flags,
            caller,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileKind, Opened};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Barrier, RwLock};
    use std::thread;

    /// Contents of a [`Mini`] node.
    enum Data {
        Dir(BTreeMap<String, NodeId>),
        File(Vec<u8>),
    }
    /// A [`Mini`] node.
    struct Node(RwLock<Data>);
    /// A minimal in-memory filesystem with create, unlink and read, for
    /// testing node and handle lifetimes.
    struct Mini;
    impl NodeFs for Mini {
        type Node = Node;
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) -> Node {
            Node(RwLock::new(Data::Dir(BTreeMap::new())))
        }
        fn getattr(&self, n: &Node, _: Option<&()>, _: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr {
                kind: match &*n.0.read().unwrap() {
                    Data::Dir(_) => FileKind::Directory,
                    Data::File(_) => FileKind::RegularFile,
                },
                nlink: 1,
                ..Default::default()
            })
        }
        fn create(
            &self,
            cx: &Cx<'_, Node>,
            parent: NodeId,
            name: &OsStr,
            _: u32,
            _: u32,
            _: i32,
            _: &Caller,
        ) -> Result<(NodeId, Opened<()>), Errno> {
            let id = cx.insert(Node(RwLock::new(Data::File(Vec::new()))), parent);
            let p = cx.get(parent).ok_or(Errno::ENOENT)?;
            if let Data::Dir(e) = &mut *p.0.write().unwrap() {
                e.insert(name.to_string_lossy().into_owned(), id);
            }
            Ok((id, Opened::new(())))
        }
        fn unlink(
            &self,
            cx: &Cx<'_, Node>,
            parent: NodeId,
            name: &OsStr,
            _: &Caller,
        ) -> Result<(), Errno> {
            let p = cx.get(parent).ok_or(Errno::ENOENT)?;
            let id = match &mut *p.0.write().unwrap() {
                Data::Dir(e) => e
                    .remove(&name.to_string_lossy().into_owned())
                    .ok_or(Errno::ENOENT)?,
                _ => return Err(Errno::ENOTDIR),
            };
            cx.remove_link(id);
            Ok(())
        }
        fn read<'a>(
            &'a self,
            n: &'a Node,
            _: &'a (),
            off: u64,
            size: usize,
            _: &Caller,
        ) -> Result<Cow<'a, [u8]>, Errno> {
            match &*n.0.read().unwrap() {
                Data::File(v) => Ok(Cow::Owned(
                    v.get(off as usize..).unwrap_or_default()
                        [..size.min(v.len().saturating_sub(off as usize))]
                        .to_vec(),
                )),
                _ => Err(Errno::EISDIR),
            }
        }
    }
    fn caller() -> Caller {
        Caller::default()
    }
    fn counts(rt: &Runtime<Mini>, ino: u64) -> Option<(u32, u64, u32, u32)> {
        lock(&rt.shared.table)
            .map
            .get(&ino)
            .map(|s| (s.links, s.lookups, s.opens, s.leases))
    }

    #[test]
    fn lifetime_and_generation() {
        let rt = Runtime::new(Mini);
        let (e, o) = rt.create(1, OsStr::new("f"), 0, 0, 0, &caller()).unwrap();
        assert_eq!(counts(&rt, e.ino), Some((1, 1, 1, 0)));
        rt.unlink(1, OsStr::new("f"), &caller()).unwrap();
        rt.forget(e.ino, 1);
        rt.release(e.ino, o.fh, &caller()).unwrap();
        assert_eq!(counts(&rt, e.ino), None);
        let (e2, _) = rt.create(1, OsStr::new("g"), 0, 0, 0, &caller()).unwrap();
        assert_eq!(e2.ino, e.ino);
        assert_eq!(e2.generation, 1);
    }

    /// A root with one child that every lookup finds, counting `forget`
    /// notifications.
    struct ForgetFs {
        /// The child, inserted by `populate`.
        child: Mutex<Option<NodeId>>,
        /// Number of `forget` callbacks received.
        notifications: AtomicUsize,
    }
    impl NodeFs for ForgetFs {
        type Node = ();
        type Handle = ();
        type DirHandle = ();

        fn root(&mut self) {}
        fn populate(&mut self, cx: &Cx<'_, ()>) {
            *lock(&self.child) = Some(cx.insert((), NodeId::ROOT));
        }
        fn getattr(&self, _: &(), _: Option<&()>, _: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }
        fn lookup(
            &self,
            _: &Cx<'_, ()>,
            _: NodeId,
            _: &OsStr,
            _: &Caller,
        ) -> Result<Option<NodeId>, Errno> {
            Ok(*lock(&self.child))
        }
        fn forget(&self, _: &()) {
            self.notifications.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn forget_notifies_only_on_zero_lookup_transitions() {
        let rt = Runtime::new(ForgetFs {
            child: Mutex::new(None),
            notifications: AtomicUsize::new(0),
        });
        let LookupReply::Found(entry) = rt.lookup(1, OsStr::new("child"), &caller()).unwrap()
        else {
            panic!("unexpected negative lookup");
        };
        let child = entry.ino;

        rt.forget(child, 0);
        rt.forget(child, 1);
        rt.forget(child, 1);
        rt.forget(999, 1);
        assert_eq!(rt.fs.notifications.load(Ordering::SeqCst), 1);

        let LookupReply::Found(entry) = rt.lookup(1, OsStr::new("child"), &caller()).unwrap()
        else {
            panic!("unexpected negative lookup");
        };
        assert_eq!(entry.ino, child);
        rt.forget(child, u64::MAX);
        assert_eq!(rt.fs.notifications.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn handles_are_bound_to_their_opening_node() {
        let rt = Runtime::new(Mini);
        let (entry, open) = rt.create(1, OsStr::new("f"), 0, 0, 0, &caller()).unwrap();
        assert_eq!(
            rt.getattr(1, Some(open.fh), &caller()).unwrap_err(),
            Errno::EBADF
        );
        assert_eq!(rt.release(1, open.fh, &caller()).unwrap_err(), Errno::EBADF);
        rt.release(entry.ino, open.fh, &caller()).unwrap();
    }

    /// Two nodes under the root (2 and 3) whose file handles are numbered
    /// 1, 2, ... in open order, recording which handle each `getattr` and
    /// `setattr` received.
    #[derive(Default)]
    struct HandleFallbackFs {
        /// Returned from `wants_fallback_handle`.
        wants_fallback: bool,
        /// The last handle number given out.
        next_handle: AtomicUsize,
        /// The handle passed to each `getattr`, in call order.
        getattr_handles: Mutex<Vec<Option<usize>>>,
        /// The handle passed to each `setattr`, in call order.
        setattr_handles: Mutex<Vec<Option<usize>>>,
    }
    impl NodeFs for HandleFallbackFs {
        type Node = ();
        type Handle = usize;
        type DirHandle = ();

        fn root(&mut self) {}
        fn populate(&mut self, cx: &Cx<'_, ()>) {
            cx.insert((), NodeId::ROOT);
            cx.insert((), NodeId::ROOT);
        }
        fn getattr(&self, _: &(), handle: Option<&usize>, _: &Caller) -> Result<NodeAttr, Errno> {
            lock(&self.getattr_handles).push(handle.copied());
            Ok(NodeAttr::default())
        }
        fn setattr(
            &self,
            _: &(),
            handle: Option<&usize>,
            _: &SetAttr,
            _: &Caller,
        ) -> Result<NodeAttr, Errno> {
            lock(&self.setattr_handles).push(handle.copied());
            Ok(NodeAttr::default())
        }
        fn wants_fallback_handle(&self, _: &()) -> bool {
            self.wants_fallback
        }
        fn open(&self, _: &(), _: i32, _: &Caller) -> Result<Opened<usize>, Errno> {
            let id = self.next_handle.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(Opened::new(id))
        }
        fn opendir(&self, _: &(), _: i32, _: &Caller) -> Result<Opened<()>, Errno> {
            Ok(Opened::new(()))
        }
    }

    #[test]
    fn getattr_and_setattr_without_fh_get_no_handle_by_default() {
        let rt = Runtime::new(HandleFallbackFs::default());
        let open = rt.open(2, 0, &caller()).unwrap();
        rt.getattr(2, None, &caller()).unwrap();
        rt.setattr(2, None, &SetAttr::default(), &caller()).unwrap();
        rt.getattr(2, Some(open.fh), &caller()).unwrap();
        rt.release(2, open.fh, &caller()).unwrap();

        assert_eq!(*lock(&rt.fs.getattr_handles), vec![None, Some(1)]);
        assert_eq!(*lock(&rt.fs.setattr_handles), vec![None]);
    }

    #[test]
    fn getattr_and_setattr_fall_back_to_newest_open_file_handle() {
        let rt = Runtime::new(HandleFallbackFs {
            wants_fallback: true,
            ..HandleFallbackFs::default()
        });
        let set = SetAttr::default();

        // No open handles yet -> None.
        rt.getattr(2, None, &caller()).unwrap();
        rt.setattr(2, None, &set, &caller()).unwrap();

        // An open directory handle on node 2 does not supply a file handle.
        let dir = rt.opendir(2, 0, &caller()).unwrap();
        rt.getattr(2, None, &caller()).unwrap();
        rt.releasedir(2, dir.fh, &caller()).unwrap();

        // An open file handle on node 3 is never supplied for node 2.
        let other = rt.open(3, 0, &caller()).unwrap();
        rt.getattr(2, None, &caller()).unwrap();

        // Opening two handles on node 2 supplies the newer one when fh is None,
        // while an explicit fh still selects the requested handle.
        let first = rt.open(2, 0, &caller()).unwrap();
        let second = rt.open(2, 0, &caller()).unwrap();
        rt.getattr(2, None, &caller()).unwrap();
        rt.setattr(2, None, &set, &caller()).unwrap();
        rt.getattr(2, Some(first.fh), &caller()).unwrap();
        rt.setattr(2, Some(first.fh), &set, &caller()).unwrap();

        // Releasing the newer handle falls back to the remaining older handle.
        rt.release(2, second.fh, &caller()).unwrap();
        rt.getattr(2, None, &caller()).unwrap();
        rt.setattr(2, None, &set, &caller()).unwrap();

        // Releasing the last handle on node 2 returns to None.
        rt.release(2, first.fh, &caller()).unwrap();
        rt.getattr(2, None, &caller()).unwrap();
        rt.setattr(2, None, &set, &caller()).unwrap();
        rt.release(3, other.fh, &caller()).unwrap();

        assert_eq!(
            *lock(&rt.fs.getattr_handles),
            vec![None, None, None, Some(3), Some(2), Some(2), None]
        );
        assert_eq!(
            *lock(&rt.fs.setattr_handles),
            vec![None, Some(3), Some(2), Some(2), None]
        );
    }

    /// A root whose readdirplus lists one child, optionally failing after
    /// emitting it.
    struct PlusFs {
        /// The child, inserted by `populate`.
        child: Mutex<Option<NodeId>>,
        /// Fail the listing with `EIO` after the kernel accepts the child.
        fail_after_emit: bool,
    }
    impl NodeFs for PlusFs {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        const SUPPORTS_READDIRPLUS: bool = true;
        fn root(&mut self) {}
        fn populate(&mut self, cx: &Cx<'_, ()>) {
            *lock(&self.child) = Some(cx.insert((), NodeId::ROOT));
        }
        fn getattr(&self, _: &(), _: Option<&()>, _: &Caller) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }
        fn opendir(&self, _: &(), _: i32, _: &Caller) -> Result<Opened<()>, Errno> {
            Ok(Opened::new(()))
        }
        fn readdirplus(
            &self,
            _: &Cx<'_, ()>,
            _: &(),
            _: NodeId,
            _: NodeId,
            _: &(),
            _: u64,
            sink: &mut dyn PlusDirSink,
            _: &Caller,
        ) -> Result<(), Errno> {
            let id = lock(&self.child).unwrap();
            if !sink.add(OsStr::new("\u{ff}name"), id, NodeAttr::default(), 1) {
                return Ok(());
            }
            if self.fail_after_emit {
                Err(Errno::EIO)
            } else {
                Ok(())
            }
        }
    }
    /// A readdirplus sink that accepts every entry if its flag is set and
    /// rejects every entry otherwise.
    struct CollectPlus(bool);
    impl RuntimePlusSink for CollectPlus {
        fn add(&mut self, _: &OsStr, _: EntryReply, _: u64) -> bool {
            self.0
        }
    }
    #[test]
    fn readdirplus_retains_only_accepted_entries_and_rolls_back_errors() {
        for (accept, fail, expected) in [(true, false, 1), (false, false, 0), (true, true, 0)] {
            let rt = Runtime::new(PlusFs {
                child: Mutex::new(None),
                fail_after_emit: fail,
            });
            let child = lock(&rt.fs.child).unwrap();
            let dir = rt.opendir(1, 0, &caller()).unwrap();
            let result = rt.readdirplus(1, dir.fh, 0, &mut CollectPlus(accept), &caller());
            assert_eq!(result.is_ok(), !fail);
            assert_eq!(
                lock(&rt.shared.table)
                    .map
                    .get(&child.ino())
                    .unwrap()
                    .lookups,
                expected
            );
        }
    }

    /// A filesystem whose `getattr` waits on a barrier, so a test passes
    /// only if two `getattr` calls run at the same time.
    struct Overlap {
        barrier: Arc<Barrier>,
    }
    impl NodeFs for Overlap {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(&self, _: &(), _: Option<&()>, _: &Caller) -> Result<NodeAttr, Errno> {
            self.barrier.wait();
            Ok(NodeAttr::default())
        }
    }
    #[test]
    fn callbacks_really_overlap() {
        let rt = Arc::new(Runtime::new(Overlap {
            barrier: Arc::new(Barrier::new(2)),
        }));
        let a = Arc::clone(&rt);
        let b = Arc::clone(&rt);
        let t1 = thread::spawn(move || a.getattr(1, None, &caller()).unwrap());
        let t2 = thread::spawn(move || b.getattr(1, None, &caller()).unwrap());
        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn node_lease_defers_reuse() {
        let rt = Runtime::new(Mini);
        let (e, o) = rt.create(1, OsStr::new("f"), 0, 0, 0, &caller()).unwrap();
        let lease = rt.node(e.ino).unwrap();
        rt.unlink(1, OsStr::new("f"), &caller()).unwrap();
        rt.forget(e.ino, 1);
        rt.release(e.ino, o.fh, &caller()).unwrap();
        assert!(counts(&rt, e.ino).is_some());
        drop(lease);
        assert!(counts(&rt, e.ino).is_none());
    }

    /// A [`HandleOverlap`] file handle whose `read` blocks until the test
    /// lets it finish.
    struct BlockingHandle {
        /// Reached when `read` starts.
        entered: Arc<Barrier>,
        /// Waited on before `read` returns.
        finish: Arc<Barrier>,
    }
    impl Default for BlockingHandle {
        fn default() -> Self {
            Self {
                entered: Arc::new(Barrier::new(1)),
                finish: Arc::new(Barrier::new(1)),
            }
        }
    }
    /// A filesystem with blocking reads, for checking that `release` waits
    /// for a `read` on the same handle and consumes the handle once.
    struct HandleOverlap {
        /// Shared with each opened [`BlockingHandle`].
        entered: Arc<Barrier>,
        /// Shared with each opened [`BlockingHandle`].
        finish: Arc<Barrier>,
        /// Number of `release` callbacks received.
        releases: Arc<AtomicUsize>,
    }
    impl NodeFs for HandleOverlap {
        type Node = ();
        type Handle = BlockingHandle;
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(
            &self,
            _: &(),
            _: Option<&BlockingHandle>,
            _: &Caller,
        ) -> Result<NodeAttr, Errno> {
            Ok(NodeAttr::default())
        }
        fn open(&self, _: &(), _: i32, _: &Caller) -> Result<crate::Opened<BlockingHandle>, Errno> {
            Ok(crate::Opened::new(BlockingHandle {
                entered: Arc::clone(&self.entered),
                finish: Arc::clone(&self.finish),
            }))
        }
        fn read<'a>(
            &'a self,
            _: &'a (),
            h: &'a BlockingHandle,
            _: u64,
            _: usize,
            _: &Caller,
        ) -> Result<Cow<'a, [u8]>, Errno> {
            h.entered.wait();
            h.finish.wait();
            Ok(Cow::Borrowed(&[]))
        }
        fn release(&self, _: &(), _: BlockingHandle, _: &Caller) -> Result<(), Errno> {
            self.releases.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[test]
    fn release_waits_for_same_handle_callback_and_consumes_once() {
        let entered = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let releases = Arc::new(AtomicUsize::new(0));
        let rt = Arc::new(Runtime::new(HandleOverlap {
            entered: Arc::clone(&entered),
            finish: Arc::clone(&finish),
            releases: Arc::clone(&releases),
        }));
        let open = rt.open(1, 0, &caller()).unwrap();
        let reader = Arc::clone(&rt);
        let read_thread = thread::spawn(move || {
            reader
                .read(1, open.fh, 0, 1, &caller(), |r| r.map(|_| ()))
                .unwrap()
        });
        entered.wait();
        let closer = Arc::clone(&rt);
        let (tx, rx) = mpsc::channel();
        let close_thread = thread::spawn(move || {
            tx.send(closer.release(1, open.fh, &caller())).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        finish.wait();
        read_thread.join().unwrap();
        rx.recv().unwrap().unwrap();
        close_thread.join().unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(rt.release(1, open.fh, &caller()).unwrap_err(), Errno::EBADF);
    }

    /// A filesystem whose first `getattr` panics if the flag starts set,
    /// for checking that a panic doesn't leak a node lease.
    struct PanicOnce(AtomicBool);
    impl NodeFs for PanicOnce {
        type Node = ();
        type Handle = ();
        type DirHandle = ();
        fn root(&mut self) {}
        fn getattr(&self, _: &(), _: Option<&()>, _: &Caller) -> Result<NodeAttr, Errno> {
            if self.0.swap(false, Ordering::SeqCst) {
                panic!("boom")
            }
            Ok(NodeAttr::default())
        }
    }
    #[test]
    fn panic_drops_node_lease_and_runtime_remains_usable() {
        let rt = Runtime::new(PanicOnce(AtomicBool::new(true)));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.getattr(
                1,
                None,
                &caller()
            )))
            .is_err()
        );
        assert!(rt.getattr(1, None, &caller()).is_ok());
        assert_eq!(lock(&rt.shared.table).map.get(&1).unwrap().leases, 0);
    }
}
