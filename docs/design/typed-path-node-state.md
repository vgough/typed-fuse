# Typed per-node state for `PathFilesystem`

**Status:** Proposed  
**Date:** 2026-07-31

## Summary

`NodeFs::Node` is already typed, runtime-owned data associated with one FUSE
node. `PathNodeFs` should expose that same capability to a
`PathFilesystem` implementation instead of retaining an opaque
`Arc<dyn Any + Send + Sync>` which no later callback can read.

This design makes the path adapter's node payload a composition of:

- an adapter-private key used to recover the node's current path; and
- an `Arc<P::NodeState>` created by the implementing filesystem.

Every operation concerning an existing node receives a typed context with the
current path (when one exists) and the same node state. The runtime continues
to own node identity and lifetime; this design does not introduce a second
inode or node table.

## Motivation

The current `PathFilesystem::node_state` hook constructs an
`Arc<dyn Any + Send + Sync>` and stores it in the private `PathNode` payload.
No `PathFilesystem` callback receives a `PathNode`, and there is no accessor
or downcast path. The value is therefore unusable as cached data.

The hook also introduces failures after successful filesystem operations. For
example, `create` can mutate the backing filesystem, then fail while the
adapter separately calls `node_state`. Repeated or concurrent lookup can also
construct state for an already-known dentry and immediately discard it.

`NodeId` and node payloads have separate jobs:

| Concept | Owner | Purpose |
| --- | --- | --- |
| `NodeId` | Runtime | Opaque FUSE inode identity used in structural operations. |
| `NodeFs::Node` | Filesystem/adapter | Typed payload resolved by the runtime for existing-node callbacks. |
| `PathNode.key` | Path adapter | Private stable key for reverse lookup in the adapter namespace. |
| `NodeState` | `PathFilesystem` implementation | Typed data shared by operations on one runtime node. |

`Handle` and `DirHandle` remain per-open objects. `NodeState` is for data that
must be shared by all opens and remain associated with the node across rename,
hard links, and unlink while open.

## Goals and non-goals

Goals:

- Let a `PathFilesystem` create typed data when it discovers or creates a
  node, and read that same data in later callbacks for that node.
- Preserve a node's state across rename and hard-link aliases.
- Keep state available to operations on an unlinked-but-open node even when
  its path is unavailable.
- Avoid a separate adapter-originated, fallible state-construction step after
  a filesystem mutation.
- Support reusable directory snapshots without requiring `NodeState: Clone`.

Non-goals:

- Expose raw inode numbers as filesystem identity.
- Add automatic locking or mutation semantics to node state. Implementations
  choose `Mutex`, `RwLock`, atomics, or immutable state as appropriate.
- Preserve source compatibility with the current `PathFilesystem` trait. The
  `node_state` hook is unreleased and this is an intentional breaking change.
- Replace direct `NodeFs` implementations. Filesystems that need complete
  control over identity and namespace management should implement `NodeFs`
  directly.

## Public API

### Node state and node context

`PathFilesystem` gains a required associated type and an infallible root
constructor:

```rust
pub trait PathFilesystem: Send + Sync + Sized {
    type NodeState: Send + Sync + 'static;
    type Handle: Send + Sync;
    type DirHandle: Send + Sync;

    fn root_state(&mut self) -> Arc<Self::NodeState>;
    // ...
}
```

The root constructor is infallible because `NodeFs::root` is called while
constructing `Runtime` and is itself infallible. A root whose initialization
can fail should store that result or a lazy initializer inside `NodeState` and
return the operational error from the relevant request callback.

Existing-node callbacks receive this public borrowed context:

```rust
pub struct PathNodeRef<'a, S> {
    path: Option<&'a Path>,
    state: &'a Arc<S>,
}

impl<'a, S> PathNodeRef<'a, S> {
    pub fn path(&self) -> Option<&'a Path>;
    pub fn state(&self) -> &'a S;
    pub fn state_arc(&self) -> Arc<S>;
}
```

The fields remain private. `path()` is `None` only for callbacks that can
operate on an unlinked-but-open node. Naming callbacks such as `lookup`,
`mkdir`, `rename`, and `unlink` receive contexts whose paths are present.

All callbacks operating on an existing node use `PathNodeRef` in place of the
corresponding path argument. For example:

```rust
fn getattr(
    &self,
    node: PathNodeRef<'_, Self::NodeState>,
    handle: Option<&Self::Handle>,
    caller: &Caller,
) -> Result<NodeAttr, Errno>;

fn lookup(
    &self,
    parent: PathNodeRef<'_, Self::NodeState>,
    name: &OsStr,
    caller: &Caller,
) -> Result<Option<PathEntry<Self::NodeState>>, Errno>;

fn rename(
    &self,
    parent: PathNodeRef<'_, Self::NodeState>,
    name: &OsStr,
    new_parent: PathNodeRef<'_, Self::NodeState>,
    new_name: &OsStr,
    flags: u32,
    caller: &Caller,
) -> Result<(), Errno>;
```

Methods with a source node and a destination parent, such as `link`, receive
one context for each. `statfs` remains path-based (`/`) because the underlying
`NodeFs::statfs` operation has no node argument.

### Discovery and creation results

New nodes are described by a typed entry result:

```rust
pub struct PathEntry<S> {
    pub attr: NodeAttr,
    pub state: Arc<S>,
}

impl<S> PathEntry<S> {
    pub fn new(attr: NodeAttr, state: Arc<S>) -> Self;
}
```

`lookup`, `mknod`, `mkdir`, and `symlink` return `PathEntry<Self::NodeState>`
instead of `NodeAttr`. `create` returns
`(PathEntry<Self::NodeState>, Opened<Self::Handle>)`. The state is produced as
part of the implementation's discovery or creation work; `PathNodeFs` makes
no separate callback to construct it.

`link` retains the source node's existing state because it creates another
name for the same node, not a new node.

An implementation may use `Arc<()>` for stateless nodes. It may put a
`Mutex`, `RwLock`, `OnceLock`, immutable backing object, or error-caching
container inside its `NodeState` according to its own needs.

### Directory entries

Directory enumeration must be able to replay a snapshot while retaining typed
child state. The directory APIs therefore become generic over `NodeState` and
use an explicit identity enum:

```rust
pub enum PathDirIdentity<S> {
    Current,
    Parent,
    Child(Arc<S>),
}

pub trait PathDirSink<S> {
    fn add(
        &mut self,
        name: &OsStr,
        kind: FileKind,
        identity: PathDirIdentity<S>,
        next_offset: u64,
    ) -> bool;
}

pub trait PathPlusDirSink<S> {
    fn add(
        &mut self,
        name: &OsStr,
        attr: NodeAttr,
        identity: PathDirIdentity<S>,
        next_offset: u64,
    ) -> bool;
}
```

`Current` is valid only for `.`, `Parent` only for `..`, and `Child` only for
all other names. The adapter rejects an invalid pairing with `EINVAL`, stops
the enumeration, and rolls back new namespace entries already created during
that call.

`DirEntry` and `DirBuffer` become generic over `S` and retain a
`PathDirIdentity<S>`. A buffer clones `Arc<S>` while replaying child entries;
it does not require state values themselves to be cloneable. `push_dots`
creates `Current` and `Parent` entries, while ordinary `push` accepts a child
state `Arc<S>`.

## Adapter implementation

Internally, replace the type-erased field with the typed composite payload:

```rust
pub struct PathNode<S> {
    key: u64,
    state: Arc<S>,
}

impl<P: PathFilesystem> NodeFs for PathNodeFs<P> {
    type Node = PathNode<P::NodeState>;
    // ...
}
```

`PathNodeFs::root` stores `inner.root_state()` in the root node. When a
filesystem operation returns a new `PathEntry`, the adapter inserts its
`state` into the corresponding `PathNode`. When it invokes a later callback,
it derives the current path from the namespace and constructs `PathNodeRef`
from that path and the stored `Arc`.

The first successfully inserted node state is canonical. If a lookup or
directory enumeration discovers a dentry that is already present, the adapter
keeps the existing node and drops the newly supplied candidate state. Under
concurrent discovery, more than one candidate may therefore be constructed,
but every callback for the resolved node receives the single retained state.
The adapter must not replace state merely because a later lookup returns a
different candidate.

The runtime's existing node records already retain payloads until their link,
lookup, open-handle, and active-lease counts all reach zero. Consequently, the
node state's `Arc` is dropped at the correct node-lifetime boundary without a
second lifetime manager.

## Migration

- Remove `PathFilesystem::node_state` and imports of `Any`.
- Add `type NodeState` and `root_state` to every `PathFilesystem`
  implementation.
- Convert path-taking node callbacks to `PathNodeRef`; retain ordinary
  `&OsStr`, request data, handles, and callers unchanged.
- Return `PathEntry` from discovery and creation operations.
- Convert `PathDirSink`, `PathPlusDirSink`, `DirEntry`, and `DirBuffer` to
  their generic forms and supply explicit dot identities.
- Export `PathNodeRef`, `PathEntry`, and `PathDirIdentity` from
  `typed-fuse-core` and the facade crate alongside the existing path API.
- Keep `mount_blocking` and `PathNodeFs::new` infallible; only filesystem trait
  implementations require migration.

## Verification

The implementation should add tests covering:

- root and child state are visible with the expected concrete type;
- state is shared by repeated operations, hard-link aliases, and open handles;
- rename changes `PathNodeRef::path()` but preserves state identity;
- an unlinked-but-open node has `path() == None` while retaining its state;
- separate nodes receive separate state values and state drops only after the
  runtime releases the node;
- concurrent lookup resolves one node and keeps a stable canonical state;
- create-family operations have no adapter-originated post-mutation state
  failure;
- `readdir` and `readdirplus` attach child state, replay a `DirBuffer`, handle
  `.` and `..`, and reject invalid identities with rollback;
- stateless `NodeState = ()` implementations remain straightforward; and
- the workspace has no remaining `Any`/downcast-based path-node state API.

