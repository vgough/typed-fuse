# Architecture and API overview

This document is a map of the `typed-fuse` workspace for two audiences:

- filesystem authors choosing and implementing one of the safe APIs; and
- contributors changing the runtime, libfuse bridge, platform support, or raw bindings.

The public Rustdoc on each type remains the source for exact signatures. This guide focuses on
how the pieces fit together, who owns each piece of state, and the contracts that span multiple
callbacks.

## Workspace at a glance

| Crate | Role | Typical user |
| --- | --- | --- |
| [`typed-fuse`](typed-fuse/) | Safe FUSE 3 session, C callback bridge, mount configuration, and passthrough helpers | Filesystem implementations and daemons |
| [`typed-fuse-core`](typed-fuse-core/) | Backend-neutral traits, value types, node/handle runtime, and path-to-node adapter | Filesystem implementations, tests, and runtime contributors |
| [`libfuse-sys`](libfuse-sys/) | Bindgen-generated FUSE 3 and optional CUSE bindings | The `typed-fuse` bridge, or users who explicitly need the raw C API |

`typed-fuse` re-exports the author-facing types from `typed-fuse-core`, so a mounted filesystem
normally needs only a dependency on `typed-fuse`. Tests that drive the pure-Rust runtime directly
also depend on `typed-fuse-core` because `Runtime` itself is not re-exported by `typed-fuse`.

The dependency and request flow is:

```text
filesystem process
    implements NodeFs                    implements PathFilesystem
            |                                      |
            |                                PathNodeFs adapter
            |                                      |
            +------------------+-------------------+
                               v
                   typed-fuse-core::Runtime
              node identity, lifetime, handle table
                               |
                               v
                   typed-fuse session shims
             C conversion, replies, panic boundary
                               |
                               v
              libfuse-sys generated low-level API
                               |
                               v
                         libfuse 3 <-> kernel
```

The dependency arrows point down (`typed-fuse` depends on both other crates), while a request
travels up from libfuse through the session and runtime to the filesystem callback, then back down
as a reply.

## Choosing an authoring API

There are two safe authoring surfaces.

| Choose | When it fits | What you manage |
| --- | --- | --- |
| [`NodeFs`](typed-fuse-core/src/node_fs.rs) | In-memory filesystems, databases/object stores, or designs where stable object identity is natural | Directory maps, node payloads and metadata, hard-link counts, and application synchronization |
| [`PathFilesystem`](typed-fuse-core/src/path_fs.rs) through `PathNodeFs` | Passthrough and transforming filesystems whose backing operations are naturally path-oriented | Backing paths and handles, attributes, typed node state, and application synchronization; the adapter maintains the FUSE node namespace |

Prefer `NodeFs` when the filesystem already has an object/inode model. It exposes the full safe
operation set, including `copy_file_range`, and makes lifetime changes explicit through `Cx`.

Prefer `PathFilesystem` when nearly every operation starts by resolving a virtual path. Its adapter
handles inode allocation, dentry aliases, renames, hard links, and paths of unlinked-but-open
objects. The tradeoff is adapter bookkeeping and a slightly smaller API: rename flags other than
zero are rejected, and `copy_file_range` is not exposed by `PathFilesystem`.

The complete node-based examples are:

- [`hello_ll.rs`](typed-fuse/examples/hello_ll.rs), a minimal static read-only filesystem; and
- [`memory_fs.rs`](typed-fuse/examples/memory_fs.rs), a read-write filesystem with namespace
  mutation, metadata, symlinks, I/O, and xattrs.

## Shared author-facing types

These types are defined by `typed-fuse-core` and re-exported by `typed-fuse`.

### Attributes and errors

- `FileKind` represents the `S_IFMT` portion of a mode: regular file, directory, symlink,
  character/block device, FIFO, or socket.
- `NodeAttr` is the safe equivalent of `stat`, without an inode number. `kind` carries the file
  type and `perm` carries permission/special bits. The filesystem must report an accurate `nlink`;
  runtime link accounting does not fill it in.
- `SetAttr` contains only requested changes as `Option` fields. Apply every `Some` field and leave
  every `None` field unchanged. `TimeOrNow` preserves the distinction between an explicit time and
  `UTIME_NOW`.
- `StatFs` is the backend-neutral `statvfs` result.
- `Errno` is a small POSIX errno wrapper. Common values are constants; arbitrary values use
  `Errno::from_raw`. Conversion from `std::io::Error` preserves its OS errno and otherwise uses
  `EIO`.

`crtime` and BSD flags are meaningful on macOS and ignored on Linux. Names use `OsStr`/`OsString`,
not UTF-8 strings, so implementations should not call `to_str()` as part of normal path handling.

### Requests, opens, and xattrs

- `Caller` contains the requester's uid, gid, pid, and umask as reported by libfuse.
- `ConnInfo` exposes negotiated protocol limits and capabilities to `init`. The currently modeled
  capabilities are asynchronous reads, asynchronous direct I/O, and parallel directory
  operations.
- `Opened<H>` returns an implementation-defined handle plus `OpenHints`: `direct_io`,
  `keep_cache`, `nonseekable`, `cache_readdir`, and (when supported by installed headers)
  `parallel_direct_writes`. Parallel direct writes are only applied with direct I/O.
- `XattrReply` implements the two-stage xattr protocol. `XattrReply::sized(data, requested_size)`
  is the easiest correct way to answer a materialized value or name list.
- `FileLock` uses an inclusive `[start, end]` range; `u64::MAX` means through end-of-file.

## The node-based API

Implementing `NodeFs` requires three thread-safe payload types:

```rust
type Node: Send + Sync;       // one payload per runtime node
type Handle: Send + Sync;     // one payload per open file
type DirHandle: Send + Sync;  // one payload per open directory
```

Only `root` and `getattr` lack default implementations. Most unsupported operations default to
`ENOSYS`; cleanup callbacks (`flush`, `release`, `fsync`, `releasedir`, and `fsyncdir`) default to
success, `access` allows access, and `statfs` returns a minimal valid response.

### API groups

| Area | `NodeFs` methods | Important result or side effect |
| --- | --- | --- |
| Lifecycle | `root`, `populate`, `init`, `destroy`, `forget` | Seed nodes; negotiate connection features; observe lookup-count transitions |
| Metadata | `getattr`, `entry_attr`, `setattr`, `statfs`, `access` | Return complete attributes; apply only requested changes |
| Namespace | `lookup`, `mknod`, `mkdir`, `symlink`, `link`, `unlink`, `rmdir`, `rename`, `readlink` | Maintain directory maps and mirror link/parent changes into `Cx` |
| File I/O | `open`, `read`, `write`, `flush`, `fsync`, `release` | Exchange typed handles; `release` consumes the handle |
| Directory I/O | `opendir`, `readdir`, `readdirplus`, `fsyncdir`, `releasedir` | Emit size-limited entries with valid resume cookies |
| Extended attributes | `setxattr`, `getxattr`, `listxattr`, `removexattr` | Honor size probes and `ERANGE` behavior |
| Allocation/seeking | `fallocate`, `lseek`, `copy_file_range` | Implement optional kernel fast paths |
| Locking | `getlk`, `setlk`, `flock` | Opt in with associated constants before callbacks are registered |
| Atomic creation | `create` | Return both the inserted `NodeId` and an `Opened<Handle>` |

`SUPPORTS_READDIRPLUS`, `SUPPORTS_POSIX_LOCKS`, and `SUPPORTS_FLOCK` are `false` by default.
Setting one to `true` makes the corresponding low-level callback visible to the kernel. Leaving a
flag false is different from registering a callback that returns `ENOSYS`, especially for locking,
where the kernel may provide its own behavior when the callback is absent.

### Namespace ownership and `Cx`

`Cx` is a synchronized view of the runtime's node table, provided to callbacks that may discover
or mutate nodes:

- `insert(payload, parent)` creates a fresh identity with one logical link and returns a `NodeId`.
- `get(id)` returns a `NodeRef` lease, or `None` if the node is gone. A live lease prevents inode
  reclamation and reuse.
- `add_link` and `remove_link` mirror creation/removal of additional names.
- `reparent` updates the parent used for directory `..` reporting after a move.
- `contains` only tests whether an identity is live.

The runtime does **not** store directory entries or deduplicate payloads. A node-based filesystem
normally stores `NodeId`s in its directory payloads. A typical create sequence is:

1. validate the parent and name;
2. build the payload and call `cx.insert(payload, parent)`;
3. record the returned ID under the name in the parent directory; and
4. return the ID.

`insert` already accounts for the first link, so do not call `add_link` for that first name. A hard
link records the existing ID under another name and calls `add_link`. Unlink removes the name and
calls `remove_link`. Keep the user-visible `NodeAttr::nlink` in sync separately.

After `lookup`, `mknod`, `mkdir`, `symlink`, `link`, or `create` returns an ID, the runtime calls
`entry_attr` (which defaults to `getattr`) before it can reply. It does not roll back an
application-level mutation if that attribute step fails. Make attributes available as part of the
same logical operation; override `entry_attr` only when the node payload already carries a more
direct answer.

For a directory moved to a different parent, update the namespace and call `reparent`. For
operations that lock several node payloads, use a consistent global order such as ascending
`NodeId`; the runtime cannot prevent application-level deadlocks.

### Node identity and lifetime

The runtime assigns inode numbers starting at 2; root is always `NodeId::ROOT`/inode 1. It tracks
five values per node:

```text
links       names that keep the node present in the filesystem namespace
lookups     references issued to the kernel in entry replies
opens       live file or directory handles
leases      in-process NodeRef users currently executing
generation  incremented before a reclaimed inode number is reused
```

A node payload is reclaimed only when `links == lookups == opens == leases == 0`. This is what
makes unlink-while-open safe and prevents an inode from being reused while a callback still has a
reference. Generation numbers let the kernel distinguish a reused inode number from its previous
occupant.

Every successful `lookup`, create-style entry reply, link reply, and accepted non-dot
`readdirplus` entry increments the lookup count. Kernel `forget` requests subtract from it. The
filesystem's `forget` hook runs only when a positive lookup count transitions to zero; it is a
notification, not permission to destroy payload state, because links or handles may still retain
the node.

Positive and negative lookup replies are cached for one second by default. `Runtime` exposes
`set_ttl` and `set_negative_ttl` for direct users; `Session` currently constructs its runtime with
those defaults and has no TTL field in `SessionConfig`.

### File-handle lifetime

The filesystem returns typed handles and never sees libfuse's numeric `fh`. The runtime assigns
numeric handles, checks that every handle is used with the node that opened it, and returns
`EBADF` for mismatches or closing handles.

Callbacks may overlap on the same handle. `release`/`releasedir` remove the handle from the table,
prevent new users, wait for existing handle leases to drain, and then pass the payload by value to
the consuming callback exactly once. Handle payloads therefore need interior synchronization if
they contain mutable shared state.

`flush` is not the final close notification: it can run more than once for duplicated descriptors.
Use `release` for final per-open cleanup.

### Directory iteration

`readdir` pushes entries into `DirSink`; `readdirplus` pushes attributes into `PlusDirSink` as well.
For both forms:

- include `.` and `..` if the intended filesystem semantics require them, using the `this` and
  `parent` IDs supplied by the runtime;
- treat `offset` as an opaque resume cookie, not a byte position;
- give each emitted entry a strictly increasing `next_offset` that resumes *after* it;
- stop immediately when `sink.add(...)` returns `false`; and
- only emit live `NodeId`s.

A directory-handle snapshot makes cookies stable when the namespace changes between calls. The
path API's `DirBuffer` and the general `replay` helper encode the common `index + 1` cookie scheme.

For `readdirplus`, the runtime increments lookup counts only after the underlying libfuse buffer
accepts a non-dot entry. If the filesystem callback later returns an error, it rolls those
increments back. This accounting is part of node lifetime correctness; contributors changing the
sink path should preserve it.

### Reads and borrowed data

`read` returns `Cow<'a, [u8]>`. It can borrow from the filesystem, node, or handle. The runtime
invokes the reply continuation before dropping their leases, and the session sends the reply in
that continuation, so the borrowed buffer remains valid without an intermediate allocation.

## The path-based API and adapter

`PathFilesystem` offers the same general callback families using absolute virtual paths rooted at
`/`. Mount it by wrapping it in `PathNodeFs::new`, or use `mount::mount_blocking`, which performs
that wrapping for you.

It has three associated payloads:

```rust
type NodeState: Send + Sync + 'static;
type Handle: Send + Sync;
type DirHandle: Send + Sync;
```

`root_state` and `getattr` are the only required methods. Discovery and creation methods return a
`PathEntry<NodeState>`, which packages both the discovered attributes and `Arc<NodeState>`. The
adapter saves the attributes for the immediate entry reply, avoiding a duplicate `getattr`, and
retains the typed state for the node's runtime lifetime.

Unlike `NodeFs`, the path API has no `populate` hook. Nodes beneath root are learned from lookup,
creation, linking, and directory enumeration.

Every node callback receives `PathNodeRef`:

- `path()` is the adapter's current path, if the node still has a known name;
- `state()` borrows its typed state; and
- `state_arc()` clones the stored `Arc` for longer-lived work.

State belongs to node identity, not to an individual pathname. Hard-link aliases share it, and it
survives rename and unlink while the node remains open. If the final known name is unlinked,
handle-oriented callbacks such as `read`, `write`, and `release` receive `path() == None` but still
receive state and the handle. Implement handles so these operations do not require re-resolving the
removed path (an open backing file descriptor is the usual answer).

### Adapter namespace

`PathNodeFs` maps `(parent NodeId, OsString)` dentries to runtime nodes and keeps all aliases for a
node. Paths are reconstructed from current parent/name relationships, so renaming a directory also
changes descendant paths without rewriting every child.

Important consequences:

- concurrent discoveries of the same dentry reuse one runtime identity;
- the first retained node state is shared across later discoveries and hard-link aliases;
- when several aliases exist, one sorted alias is used as the current canonical path;
- replacing or unlinking an alias decrements the runtime link count, but an open node continues
  with no path after its last alias disappears; and
- there is no external namespace-invalidation API. A path filesystem should route namespace
  changes through its callbacks or otherwise ensure that externally changed backing state remains
  consistent with previously discovered dentries.

The adapter serializes namespace-changing callbacks (`mknod`, `mkdir`, `symlink`, `create`,
`unlink`, `rmdir`, `rename`, and `link`) with a write lock. Ordinary lookup, data, metadata,
directory, and forget callbacks hold a shared read lock, so they may overlap with one another but
not with those structural changes. Long reads or backing I/O can consequently delay a
rename/unlink, and payloads/handles still need their own interior locking.

`PathFilesystem::rename` has no flags argument. `PathNodeFs` returns `EOPNOTSUPP` when the kernel
passes nonzero flags. `statfs` receives `/` through the adapter. The path trait has no
`copy_file_range`; the adapted `NodeFs` default therefore returns `ENOSYS` for it.

### Path directory identities and snapshots

A path listing cannot supply a `NodeId` directly. Each entry supplies `PathDirIdentity`:

- `Current` for `.`;
- `Parent` for `..`; or
- `Child(Arc<NodeState>)` for a real child.

This lets the adapter reuse the current/parent identities and create or deduplicate child runtime
nodes. If a listing fails or the downstream buffer rejects a newly inserted child, the adapter
rolls back the corresponding dentry and runtime link.

`DirBuffer<NodeState>` is a ready-made immutable directory snapshot. Build it in `opendir`, usually
with `push_dots` followed by `push`, store it as `DirHandle`, and call `fill` or `fill_plus` from the
listing callback. It provides stable `index + 1` offsets and stops when the sink is full.

For tests that call a `PathFilesystem` without the adapter, construct a `PathNodeRef` directly with
`PathNodeRef::new`.

## Sessions, mounting, and the libfuse bridge

`Session<F: NodeFs>` owns the raw `fuse_session`, the `Runtime<F>`, mount state, signal handlers,
and event loop. Its normal lifecycle is:

```rust
let mut session = Session::new_with_config(fs, &options, config)?;
session.mount(mountpoint)?;
session.run()?;
```

`mount_and_run` and `mount_and_run_with_config` are convenience forms. Dropping a mounted session
unmounts it, destroys the libfuse session, and then drops the runtime. A `Session` is deliberately
not `Send`; libfuse worker threads share only its synchronized runtime.

`SessionConfig` controls dispatch:

- the default is `MultiThreaded` with 10 maximum workers, no explicit idle-worker retirement, and
  `clone_fd == false`;
- libfuse's supported maximum is 100,000 workers; and
- `SingleThreaded` prevents callback overlap at the session level.

In multi-threaded mode there is no callback ordering or automatic per-node/per-handle
serialization. The session also requests the kernel's parallel-directory-operations capability
when available; the filesystem can inspect or change it in `init`.

`MountOption` renders safe variants for read-only, access policy, default permission checking,
filesystem names, subtypes, and automatic unmounting, plus raw custom words. `NonEmpty` and
`VolName` are macOS-specific and are dropped (with debug logging) on unsupported platforms.

For path-based daemons, [`mount.rs`](typed-fuse/src/mount.rs) provides:

- `MountConfig`, with `default_permissions` enabled by default;
- `parse_option_words` and `build_mount_options`; and
- `mount_blocking`, which wraps a `PathFilesystem`, selects single/multi-threaded dispatch, mounts,
  and runs.

Call `mount_blocking` after daemonization. Forking after libfuse has initialized process state is
unsafe.

### Callback and safety boundary

[`session.rs`](typed-fuse/src/session.rs) builds the low-level callback table. Each trampoline:

1. decodes request credentials, names, flags, offsets, and C structures;
2. calls the backend-neutral `Runtime`;
3. converts the result to the platform's libfuse structures; and
4. sends exactly one reply.

Replying callbacks are protected with `catch_unwind`; a panic becomes `EIO` instead of unwinding
through an `extern "C"` boundary. Panics in no-reply callbacks such as `forget` are caught and
logged. Filesystems should still treat panics as bugs: the conversion prevents undefined behavior,
not partial application-level mutation.

The safe bridge currently covers the `NodeFs` operations listed above. Raw low-level operations
such as `ioctl`, `poll`, `bmap`, `write_buf`, `retrieve_reply`, `statx`, `tmpfile`, and macOS-only
extensions are left unregistered. Adding one requires coordinated changes to the core trait and
runtime, session trampoline, conversion/FFI layer, platform handling, and tests.

## Helper modules in `typed-fuse`

### `passthrough`

[`passthrough.rs`](typed-fuse/src/passthrough.rs) centralizes portable building blocks for a view of
a backing Unix tree:

- `Metadata` to `FileKind`/`NodeAttr` conversion, including pre-epoch timestamps;
- synthetic-file attributes and `statvfs` conversion;
- no-follow set/get/list/remove xattr wrappers for Linux and macOS;
- byte-preserving `Path`/`OsStr` to `CString` conversion;
- access and timestamp-permission checks;
- ownership, chmod/chown, and timestamp mutation by fd or path; and
- portable special-node and arbitrary-byte symlink creation.

Use these helpers instead of duplicating platform-specific libc calls in a filesystem crate. Their
APIs return `Errno`, matching trait callbacks directly.

### `file_lock`

[`file_lock.rs`](typed-fuse/src/file_lock.rs) converts between safe `FileLock` values and the
platform's `libc::flock`, normalizing constant widths and the inclusive range representation. Its
`getlk` and `setlk` functions implement pass-through POSIX record locking for an `AsFd` backing
handle. Set the filesystem's `SUPPORTS_POSIX_LOCKS` constant when using these callbacks.

## `typed-fuse-core` internals

The core has no dependency on libfuse and contains no raw C types. Its source is divided into:

| File | Responsibility |
| --- | --- |
| [`attr.rs`](typed-fuse-core/src/attr.rs) | Attributes, file kinds, setattr values, filesystem statistics |
| [`errno.rs`](typed-fuse-core/src/errno.rs) | Portable errno wrapper |
| [`node_fs.rs`](typed-fuse-core/src/node_fs.rs) | `NodeFs` and its request/reply value types |
| [`runtime.rs`](typed-fuse-core/src/runtime.rs) | Concurrent node/handle tables and operation dispatch |
| [`path_fs.rs`](typed-fuse-core/src/path_fs.rs) | `PathFilesystem`, directory snapshots, adapter namespace |

`Runtime` is public primarily so the identity and lifetime machinery can be unit-tested without a
mount. It translates inode/file-handle operations into borrowed payload callbacks. `NodeTable` is
public for compatibility but is internally managed and has no public constructor or mutation API.

Runtime locks protect metadata, not the filesystem implementation. Node-table locks are held for
short accounting steps and released before author callbacks. `NodeRef` and handle leases bridge
those unlocked callback periods safely. Mutex poisoning is recovered by taking the poisoned inner
value, because abandoning lifetime bookkeeping would be less safe than continuing after a caught
callback panic.

When changing runtime code, pay particular attention to these invariants:

- an entry reply increments `lookups` only after attributes are available;
- a `NodeRef` increments/decrements `leases` around every payload borrow;
- a numeric handle is bound to exactly one node and one file/directory table;
- close removes the handle before waiting for active callbacks and consumes it once;
- `readdirplus` retains only entries accepted by the output buffer and rolls back on error; and
- an inode enters the free list only when all four retention counts are zero, with its next
  generation incremented.

## `libfuse-sys` internals and features

`libfuse-sys` discovers `fuse3` with `pkg-config`, runs bindgen against installed headers, and
includes the generated output from `OUT_DIR`. The surface is selected with features:

- `fuse_highlevel` generates `fuse.h` bindings;
- `fuse_lowlevel` generates `fuse_lowlevel.h` bindings;
- `cuse_lowlevel` generates `cuse_lowlevel.h` bindings; and
- exactly one of `fuse_31`, `fuse_35`, or `fuse_312` may select the API version. With none selected,
  version 35 is used.

The raw crate defaults to high- and low-level FUSE bindings. `typed-fuse` disables those defaults
and requests only `fuse_lowlevel` plus `fuse_312`, so building the full workspace requires libfuse
3.12 or newer.

[`libfuse-sys/build.rs`](libfuse-sys/build.rs) also locates compiler-provided headers such as
`stdarg.h` for libclang. The wrapper functions in [`lib.rs`](libfuse-sys/src/lib.rs) normalize FUSE
3.12 symbol differences: macFUSE uses `_312` aliases while Linux commonly uses symbol versioning.

The higher-level [`typed-fuse/build.rs`](typed-fuse/build.rs) probes installed headers for optional
fields/operations. Platform-specific C layouts and symbol aliases belong in `conv.rs`, `ffi.rs`,
`darwin.rs`, or the raw wrapper—not in filesystem implementations.

## Implementation checklist

For a new filesystem:

1. Choose `NodeFs` or `PathFilesystem`; use `OsStr` throughout the namespace.
2. Define node state and separate per-open file/directory handles. Prefer open backing fds over
   re-opening paths for unlink-while-open behavior.
3. Implement root construction, `getattr`, lookup, file open/read or write, and directory
   open/listing for the minimum mountable tree.
4. Keep namespace links and `NodeAttr::nlink` consistent. For `NodeFs`, mirror identity changes in
   `Cx`; for the path API, return `PathEntry`/`PathDirIdentity` state consistently.
5. Make directory offsets resumable and stop on a full sink. Snapshot mutable listings per open
   when practical.
6. Apply `SetAttr` field-by-field and implement the xattr size-probe protocol.
7. Decide whether the kernel (`DefaultPermissions`) or the filesystem enforces access policy; do
   not accidentally rely on the default permissive `access` callback.
8. Add interior synchronization for every mutable payload. Test concurrent operations, rename,
   hard links, and unlink while open.
9. Opt in to readdir-plus or locking only after implementing the full callback contract.
10. Mount with explicit `MountOption`s and test with both the single- and multi-threaded loops.

Common correctness traps are returning UTF-8-normalized names, using byte offsets as directory
cookies, forgetting to stop on a full sink, returning stale `nlink` or size values, destroying a
node in `forget`, requiring a path during final handle cleanup, and assuming callbacks on one node
cannot overlap.

## Building, testing, and examples

The native prerequisites are `pkg-config`, libfuse 3.12+ headers and library, a working C compiler,
and libclang for bindgen. CI installs `libfuse3-dev` on Linux and runs workspace examples and tests.

Useful local checks are:

```sh
cargo fmt --all -- --check
cargo build --workspace --examples
cargo test --workspace
cargo clippy --workspace --all-targets
```

Most core behavior can and should be tested without a mount:

```rust
use typed_fuse_core::Runtime;

let runtime = Runtime::new(MyNodeFs::new());
// Call runtime.lookup(...), runtime.open(...), and so on with a test Caller.
```

Run the examples against a real mount point with:

```sh
mkdir -p /tmp/typed-fuse-mnt
cargo run -p typed-fuse --example hello_ll -- /tmp/typed-fuse-mnt
# or: cargo run -p typed-fuse --example memory_fs -- /tmp/typed-fuse-mnt
```

Unmount with `fusermount3 -u` on Linux or `umount` on macOS. The mdtest-based filesystem benchmark
is documented in [`typed-fuse/README.md`](typed-fuse/README.md) and exposed through
`make benchmark-save-baseline` / `make benchmark`.
