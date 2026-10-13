# ADR 0001: The path adapter owns directory listings

## Status

Accepted and implemented for 0.7.0. Breaking change to `PathFilesystem`.

## Context

### How path filesystems list directories today

`PathFilesystem` exposes the FUSE directory callbacks almost unchanged:
`opendir` returns a `DirHandle` of the filesystem's choosing, and `readdir`
/ `readdirplus` emit entries from it at a kernel-supplied `offset`. The
documented way to implement them (the `DirBuffer` rustdoc in
`typed-fuse-core/src/path_fs.rs`, and "Path directory identities and
snapshots" in `ARCHITECTURE.md`) is:

1. build a `DirBuffer` in `opendir` and return it as the `DirHandle`;
2. replay it with `DirBuffer::fill` / `fill_plus` from `readdir` /
   `readdirplus`, which own the `index + 1` cookie scheme.

The snapshot gives a reader that resumes at a later offset a consistent view
even if the directory changes between calls. Both known implementors (EncFS's
forward and reverse filesystems) follow this pattern exactly.

### The pattern is wrong on FreeBSD

The pattern assumes one `FUSE_OPENDIR` per `opendir(3)` and one
`FUSE_RELEASEDIR` per `closedir(3)`. Linux behaves that way. FreeBSD's fusefs
does not (`sys/fs/fuse/fuse_vnops.c`, `fuse_file.c`):

- `fuse_vnop_close` returns immediately for directories, so closing a
  directory never sends `FUSE_RELEASEDIR`.
- `fuse_vnop_open` first calls `fuse_filehandle_validrw`, and if the vnode
  already has a handle with the same access type, uid, gid and pid, it reuses
  that handle instead of sending `FUSE_OPENDIR`.
- Handles are closed only in `fuse_vnop_inactive`. The mount holds a
  reference on the root vnode, so the root never goes inactive and its
  handle lives until unmount.
- `fuse_internal_readdir` sends the file's own position as the offset
  (`fri->offset = uio_offset(uio)`), so a fresh `opendir(3)` that reuses a
  handle reads from offset 0.

As a result, after a process first lists the root, every later listing of the
root by that process replays the first snapshot. EncFS hit this in CI (vgough/encfs PR #710):
after `rename("a", "z/moved")` and `rename("z/moved", "target")`,
listing `/` still returned `["a", "z"]`. Non-root directories usually go
inactive after close and get a new handle, which is why the bug showed up only
on the root.

The same root cause breaks `rewinddir(3)` on every platform: POSIX expects a
rewound stream to show the directory's current contents, but a handle
snapshotted at `opendir` cannot.

### Why the adapter should fix it

This is a kernel quirk that every path filesystem following the documented
pattern hits, and the fix needs no knowledge of the filesystem. EncFS's interim
fix (`src/dir_snapshot.rs`: retake the listing when a read restarts at offset
0) had to be copied into both of its filesystems. libfuse's high-level API
already does this for its users (`fuse_readdir_common` in `lib/fuse.c`: "According
to SUS, directory contents need to be refreshed on rewinddir()"). It fills the
listing on the first read and clears it whenever `off == 0`.

## Decision

The path adapter, `PathNodeFs`, owns directory listing state. A path
filesystem only says what a directory contains. The adapter decides when to
ask, holds the result, and serves `readdir` / `readdirplus` from it.

### Trait changes

```rust
pub trait PathFilesystem: Send + Sync + Sized {
    /// Per-open-directory state (e.g. a backing directory fd for
    /// `fsyncdir`). Use `()` if not needed. No longer holds the listing.
    type DirHandle: Send + Sync;

    /// Opens the directory: access checks, per-open state, open hints.
    /// Unchanged signature. It should not list the directory.
    fn opendir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        flags: i32,
        caller: &Caller,
    ) -> Result<Opened<Self::DirHandle>, Errno> {
        Err(Errno::ENOSYS)
    }

    /// New. Lists the directory's current contents, including `.` and `..`
    /// if the filesystem wants them (`DirBuffer::push_dots`). Called by the
    /// adapter, never directly by the kernel; see "When the adapter lists".
    fn list_dir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        handle: &Self::DirHandle,
        caller: &Caller,
    ) -> Result<DirBuffer<Self::NodeState>, Errno> {
        Err(Errno::ENOSYS)
    }

    // Removed: readdir, readdirplus.
    // Unchanged: releasedir, fsyncdir, SUPPORTS_READDIRPLUS.
}
```

`list_dir` gets the per-open handle so a filesystem can list through state
it pinned at `opendir` (such as a directory fd) instead of re-resolving the
path. The path in `node` is the adapter's current path, so it follows
renames of the open directory.

### Adapter handle

`PathNodeFs` stops passing `P::DirHandle` through as its
`NodeFs::DirHandle` and wraps it:

```rust
pub struct PathDirHandle<P: PathFilesystem> {
    inner: P::DirHandle,
    /// Up to four recent listings, each tagged with a generation number;
    /// empty until the first read.
    listings: Mutex<DirListings<P::NodeState>>,
}
```

- `opendir`: call `P::opendir`, wrap the result with `listing: None`, and
  pass the open hints through unchanged.
- `readdir` / `readdirplus`: get a listing (below), then replay it. Each
  child is inserted into the namespace as it is sent, and removed again if
  the kernel buffer rejects it.
- `releasedir`: drop the listing, then pass `inner` to `P::releasedir`.
- `fsyncdir`: pass `&inner`.

### When the adapter lists

The rule is the one libfuse uses:

- A read at offset 0 always takes a new listing (`P::list_dir`), gives it the
  next generation number, and makes it the handle's current listing.
- A read at any other offset resumes the listing its cookie names (see
  "Resume cookies"). If that listing has been evicted, it resumes the current
  listing by index. If the handle has none yet (a resumed cookie on a handle
  that was never read from 0), the adapter takes one first.

So a single read-through still sees one consistent snapshot, while every
`opendir(3)` + read, every `rewinddir(3)` and every reuse of a FreeBSD handle
starts from current contents. `readdir` and `readdirplus` share the current
listing, so a kernel that chooses between them per request on one handle
(Linux's adaptive readdirplus can) gets consistent cookies.

### Locking

The handle mutex guards only reading or swapping the `Arc`. `list_dir` and
`fill` run outside it, with the adapter's `operations` read lock held as
`readdir` does today. That lock keeps listings from overlapping namespace
callbacks routed through the adapter. Two concurrent offset-0 reads on one
handle each take a listing under its own generation, and both are retained,
so each stream resumes its own copy. This differs from libfuse, which
holds its per-handle lock across the fill.

### Entries with and without attributes

`DirBuffer::push(name, kind, state)` records only what a basic `readdir`
needs, so a filesystem listing a backing directory can use the kind from
`readdir(3)` without a `stat` per entry. `push_plus(name, attr, state)`
records full attributes for filesystems that already have them, and
`DirEntry::attr` is an `Option`. When `readdirplus` reaches an entry without
attributes, the adapter calls `P::lookup` for it, using the same rules as
"Resuming after a name change" below: `None` or `ENOENT` skips the entry, and
`Some` supplies the attributes and state. Any other error ends the reply
before that entry, or fails the read if no entry was sent yet, so the read
that reaches the entry reports the error. A failed read never needs to undo
entries already accepted, which is why the sinks have no error rollback.

### Resume cookies

Each cookie is `(generation << 27) | (index + 1)`. The generation is 4 bits
and rotates through 16 values, and the handle retains the 4 most recent
listings. Every cookie stays below 2^31, so it fits a 32-bit `off_t`, and is
never 0. A listing is limited to 2^27 - 1 entries; a larger one fails the read
with `EOVERFLOW`. A cookie whose generation has been reused by a newer listing
resumes that listing by index, which is the pre-generation behavior. That only
happens if a stream stays paused across 16 offset-0 reads on its handle.

### Resuming after a name change

A retained listing can be replayed long after it was taken. Replaying a name
that has since been removed would send the kernel a stale entry and put the
name back into the adapter's namespace, where a later `create` or `lookup` of
the same name would get the removed node and its `NodeState` back.

Each directory node therefore counts the namespace mutations routed through
the adapter (`create`, `mknod`, `mkdir`, `symlink`, `link`, `unlink`,
`rmdir`, and both parents of `rename`), and each listing records the count
when it is taken. These mutators hold the `operations` lock exclusively, so
the count can't change while `list_dir` runs under the shared lock. When a
resumed read finds the count has moved, it calls `P::lookup` for each child
before sending it:

- `None` or `ENOENT`: the entry is skipped. Cookies are index-based, so later entries
  keep theirs.
- `Some(entry)`: the entry is sent with the current attributes and state.
- Any other error: the listed entry is sent unchanged.

This costs one lookup per entry sent, and only on resumed pages after a
change through the mount. Attribute-only changes (`write`, `setattr`) don't
count, so a resumed `readdirplus` page can still carry an older size or time,
as with libfuse's high-level API. Changes made to the backing store outside
the mount aren't detected, consistent with the adapter having no namespace
invalidation.

### Errors

- An error from `list_dir` fails that `readdir` and leaves the previous
  listing in place, so a later read of the handle still works.
- `opendir` no longer surfaces listing errors, because it no longer lists. A
  filesystem that wants `opendir(3)` to fail early (`ENOTDIR`, `EACCES`,
  `ENOENT` for a vanished backing directory) checks that in `opendir`.

## Consequences

### For filesystem authors

- One method to implement instead of three, and no cookie bookkeeping to get
  wrong.
- A directory that is opened but never read (`openat`-based traversal,
  `fchdir`, `O_DIRECTORY` opens in `fts(3)`) no longer pays for a listing.
- Entries created between `opendir(3)` and the first read now appear in it.
  POSIX allows either behavior. EncFS's
  `reading_from_the_start_again_relists` test, which expects them not to
  appear, needs to change.
- Path filesystems can no longer stream a directory without materializing it.
  No current implementor does. A filesystem that needs to should use
  `NodeFs`, or see open question 2.
- A read that sends no entries ends the stream for the kernel, so the
  adapter drops the listing that read used, unless the read borrowed the
  current listing because its own was evicted. A listing read to the end is
  therefore freed by the final empty read, even on a FreeBSD handle that is
  never released. The handle records, per generation, the index where the
  listing ended, so a repeated read there stays empty instead of listing
  again. FreeBSD's `fuse_internal_readdir` sends READDIR until a reply is
  empty, so every `getdents` after the end makes such a read. Up to four listings that were abandoned partway hold every
  child's `Arc<NodeState>` until `releasedir`, which on a shared FreeBSD
  handle, such as the root's, can be unmount. A `seekdir(3)` back into a
  stream that already reached its end resumes the current listing by index,
  as with an evicted listing.

### Shared FreeBSD handles

Opens by the same process and credentials share one handle, and a FUSE
`READDIR` carries only the handle and an offset. If a process is partway
through listing a directory and starts a second listing of it, the cookie's
generation lets the first stream keep resuming its own listing. libfuse has
no equivalent and resumes the newest listing. The remaining limit is
retention: a stream paused across more than four newer listings on its handle
falls back to resuming the current one by index.

### Unchanged

- `NodeFs` and its runtime. `NodeFs` filesystems that read live state in
  `readdir` (like `typed-fuse/examples/memory_fs.rs`) are unaffected. Ones that snapshot
  at `opendir` have the same FreeBSD exposure. The "Directory iteration"
  section of `ARCHITECTURE.md` should say so.
- `OpenHints::cache_readdir`. With it set, Linux may answer reads from its
  page cache without reaching the adapter. Invalidation remains the
  filesystem's job.
- `DirBuffer`, `DirEntry`, `PathDirIdentity` and `replay`. Only their docs
  change: `DirBuffer` becomes "what `list_dir` returns" rather than "an
  immutable snapshot built in `opendir`".

## Alternatives considered

1. **Ship a helper type, not an adapter change.** Move EncFS's `DirSnapshot`
   (listing plus a relist callback) into typed-fuse-core next to `DirBuffer`
   and point the docs at it. This is non-breaking and small, but authors must
   know to use it, and the broken pattern stays the obvious one. It is the
   fallback if this ADR is rejected, and a reasonable stopgap while it lands.
2. **Keep `readdir` / `readdirplus` as an opt-out**, selected by an
   associated const such as `ADAPTER_LISTS_DIRECTORIES`. This gives two code
   paths and two contracts to document and test, and no current user needs
   the opt-out. It can be added later without breaking anyone: a new
   defaulted method plus a const defaulting to the managed mode.
3. **List eagerly at `opendir` and relist only on a repeated offset-0 read**
   (EncFS's interim fix). This keeps listing errors at `opendir(3)` and the
   "listing as of open" semantics, but every directory open pays for a
   listing even when it's never read, and it needs a "has been read" flag.
   Lazy listing matches libfuse and has a one-line rule.
4. **Relist when the directory's mtime changes.** This costs a `getattr` per
   read, misses changes within the backing filesystem's timestamp granularity,
   and depends on the filesystem reporting a meaningful directory mtime.
   Rejected.
5. **Work around FreeBSD by making the kernel release handles.** The adapter
   has no request that forces `FUSE_RELEASEDIR`, and the behavior is
   deliberate kernel policy. Rejected.

## Open questions

1. **Generation-tagged cookies.** *Resolved: implemented as described in
   "Resume cookies", with a 4-bit generation above a 27-bit index so cookies
   fit in 31 bits. The FreeBSD mounted check below is still outstanding.*
   Encode a listing generation in the cookie and keep the last few listings
   per handle. A resumed read whose cookie names a retained
   generation is served from that listing, so interleaved streams on a shared
   FreeBSD handle each see their own snapshot. If the generation was evicted,
   the read falls back to the current listing (today's behavior). Costs:
   retained memory, a retention bound to choose, and cookies that are no
   longer small integers. Both kernels pass `next_offset` back verbatim, so
   it should work, but it needs a FreeBSD check before relying on it. Worth
   doing only if the limitation shows up in practice.
2. **Streaming escape hatch.** Is there a path filesystem that needs to list
   directories too large to materialize? If so, alternative 2 or an
   iterator-returning `list_dir` variant becomes worth its cost.
3. **Default `SUPPORTS_READDIRPLUS` to true.** The adapter can serve
   `readdirplus` for any listing, looking up entries added without
   attributes. That costs a lookup per such entry, so it is only a clear win
   for filesystems that use `push_plus`. This is separate from this ADR.
4. **Name.** `list_dir` vs `read_dir` (shadows `std::fs::read_dir` in
   implementations) vs `enumerate`.

## Migration

### typed-fuse

- `path_fs.rs`: add `list_dir`, remove `readdir` / `readdirplus` from
  `PathFilesystem`, add `PathDirHandle`, and rewrite the adapter's
  `opendir` / `readdir` / `readdirplus` / `releasedir` / `fsyncdir`. Port
  the test `PathFilesystem` in `path_fs.rs`'s `mod tests` to `list_dir`.
- Docs: the `DirBuffer` rustdoc, "Path directory identities and snapshots"
  and "Implementation checklist" in `ARCHITECTURE.md`, plus a note under
  "Directory iteration" about FreeBSD handle reuse for `NodeFs` authors.
- Version 0.7.0, with the trait change called out in the changelog or README.

### EncFS

- `src/fs.rs` / `src/reverse_fs.rs`: `type DirHandle = ()`. `opendir`
  resolves the path and checks the backing directory exists (keeping
  `ENOENT` / `ENOTDIR` at open). `list_dir` is the existing
  `directory_snapshot` / `build_dir_buffer`, with each `push` changed to
  `push_plus` since it already has attributes. Delete the `readdir` /
  `readdirplus` impls and `src/dir_snapshot.rs`.
- `tests/directory_snapshot_test.rs` calls `readdir` on the trait directly.
  It moves to driving `Runtime<PathNodeFs<EncFs>>` (or calls `list_dir` for
  listing-content checks), and the first-read expectation changes as noted
  above.

## Verification

### Adapter unit tests (`path_fs.rs`, driven through `Runtime`, no mount)

- `opendir` does not call `list_dir`. The first read from offset 0 does.
- A second offset-0 read on the same handle calls `list_dir` again and shows
  a mutation made in between.
- A read at a later offset after a mutation resumes the earlier listing:
  no duplicates, no missing entries, nothing new.
- A nonzero-offset read on a never-read handle lists once and serves from
  that offset.
- `readdir` and `readdirplus` on one handle share a listing and cookies.
- A `list_dir` error fails the read, and the next read on the handle still
  succeeds from the previous listing.
- Sink rejection rolls back the rejected newly inserted dentry. A
  `list_dir` error happens before enumeration and therefore inserts none.
- `readdirplus` looks up entries listed without attributes and skips one the
  lookup doesn't find; basic `readdir` makes no lookups. A lookup error ends
  the page, or fails the read when it is the first entry, and the next read
  resumes the same listing.
- An empty read at the end drops the listing, and a repeated read there
  stays empty without listing again, including when the stream ended
  because its remaining entries were skipped. A read earlier in the dropped
  listing lists again. An empty read that borrowed the current listing
  leaves it in place for its own stream.
- `releasedir` and `fsyncdir` receive the filesystem's own handle.
- Concurrent offset-0 reads on one handle each return a consistent listing
  under different generations.
- Two streams interleaved on one handle each resume their own listing, and a
  cookie for an evicted listing resumes the current one by index.
- Cookies are nonzero and below 2^31, and generations wrap.
- A name unlinked between pages is not replayed, and a later `create` of it
  gets new state. A name replaced between pages is replayed with current
  attributes.

### Mounted tests

The implementation adds:

- a small path-based example (a passthrough over a temp directory) and a
  mounted test, ignored by default like EncFS's live tests, that lists the
  root, creates an entry, lists again, removes one, and lists again;
- `kldload fusefs` and that test in the FreeBSD job, and the same test in the
  Linux job.

EncFS's `live_root_listing_reflects_changes` and `live_directory_iv_rename*`
remain downstream end-to-end checks after its 0.7 migration.
