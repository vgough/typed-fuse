# typed-fuse

**A safe, Rust-friendly low-level FUSE API, built on top of [`libfuse-sys`](..)**

`libfuse-sys` exposes only raw bindgen bindings to libfuse: writing a filesystem against
it means writing `unsafe` callbacks, using C types (`*const c_char`, `stat`,
`mem::zeroed()`), and sprinkling `#[cfg(target_os = "macos")]` around to handle libfuse's
Darwin-specific quirks. `typed-fuse` centralizes all of that here so filesystem authors write
none of it.

## What you get

- A `NodeFs` trait with methods like `lookup`, `getattr`, `read`, `write`, `readdir`,
  ... that take and return plain `std` types (`&OsStr`, `Vec<u8>`, `SystemTime`,
  `Result<T, Errno>`). Every method has a sensible default, so you only implement the
  operations your filesystem actually supports.
- A `Session` type that owns mounting, the libfuse event loop, and unmounting.
- No `unsafe`, no C types (`stat`, `c_char`, raw pointers, ...), and no `target_os` cfgs in
  your code - all per-OS differences (timestamp field layout, Darwin's aliased
  `fuse_reply_*`/`fuse_add_direntry` symbols, mode_t width, ...) are handled internally.
- Filenames are `&OsStr`/`OsString`, so Unix filesystems can preserve non-UTF-8 names.
- Multi-threaded by default. Callbacks take shared references and may overlap on the same
  filesystem, node, or handle. Mutable state uses `Mutex`, `RwLock`, or atomics at the
  filesystem's chosen granularity. `ThreadingMode::SingleThreaded` is available when
  callback overlap must be disabled.

## Minimal usage

```rust
use std::borrow::Cow;
use std::path::Path;
use typed_fuse::{Caller, Errno, FileKind, NodeAttr, NodeFs, Opened, Session};

const HELLO_CONTENT: &[u8] = b"Hello World!\n";

struct HelloFs;

impl NodeFs for HelloFs {
    type Node = &'static [u8];
    type Handle = ();
    type DirHandle = ();

    fn root(&mut self) -> Self::Node { HELLO_CONTENT }

    fn getattr(
        &self,
        _node: &&'static [u8],
        _handle: Option<&()>,
        _caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Ok(NodeAttr {
            kind: FileKind::RegularFile,
            perm: 0o444,
            nlink: 1,
            size: HELLO_CONTENT.len() as u64,
            ..Default::default()
        })
    }

    fn open(&self, _node: &&'static [u8], _flags: i32, _caller: &Caller) -> Result<Opened<()>, Errno> {
        Ok(Opened::new(()))
    }

    fn read<'a>(&'a self, node: &'a &'static [u8], _handle: &'a (), offset: u64, size: usize, _caller: &Caller) -> Result<Cow<'a, [u8]>, Errno> {
        let offset = offset as usize;
        let end = (offset + size).min(node.len());
        Ok(Cow::Borrowed(&node[offset.min(end)..end]))
    }
}

fn main() {
    let mountpoint = std::env::args().nth(1).expect("usage: <mountpoint>");
    Session::mount_and_run(HelloFs, Path::new(&mountpoint), &[]).expect("mount failed");
}
```

This minimal example shows the callback shapes. A mountable filesystem also needs a
directory root plus the relevant lookup and directory-reading operations; see
[`examples/hello_ll.rs`](examples/hello_ll.rs) for a complete implementation.

`SessionConfig` controls dispatch. Its default is a ten-worker pool with no idle
retirement and `clone_fd` disabled. Libfuse limits `max_threads` to 100,000:

```rust
use typed_fuse::{SessionConfig, ThreadingMode};
let config = SessionConfig { threading: ThreadingMode::SingleThreaded };
// Session::new_with_config(fs, &options, config)?;
```

In multi-threaded mode there is no callback ordering or automatic per-node or
per-handle serialization. For operations locking multiple nodes, acquire application
locks in ascending `NodeId` order. `release`/`releasedir` are delayed until existing
leases drain and consume their handle exactly once.

## Running the `hello_ll` example

`examples/hello_ll.rs` is the full safe-API port of libfuse's classic `hello_ll.c`: a
read-only filesystem exposing a single file, `hello`, containing `Hello World!\n`.

```sh
mkdir /tmp/hello_mnt
cargo run -p typed-fuse --example hello_ll -- /tmp/hello_mnt
```

In another terminal:

```sh
cat /tmp/hello_mnt/hello
```

Unmount it with `umount /tmp/hello_mnt` on macOS or `fusermount3 -u /tmp/hello_mnt` on
Linux.

## Filesystem benchmark

The workspace includes an automated filesystem benchmark based on `mdtest`,
distributed as part of IOR. It mounts the release build of the `memory_fs`
example, runs mdtest's standard directory and file phases on that filesystem,
unmounts it, and removes its temporary mount directory. It requires a working
FUSE installation, `mdtest` on `PATH` (or `MDTEST_BIN`), and `mpirun` only for
multi-rank runs.

Save a baseline for the current machine, then compare later runs with it:

```sh
make benchmark-save-baseline
make benchmark
```

The default workload uses one rank, 1,000 items per iteration, five iterations,
and 4,096-byte writes and reads. Results are mean operations per second for each
operation in mdtest's summary. Deltas are informational; command, parse, mount,
and unmount failures still fail the benchmark.

The workload and tools can be customized with `BENCH_BASELINE`, `BENCH_ITEMS`,
`BENCH_ITERATIONS`, `BENCH_PROCS`, `BENCH_BYTES`, `MDTEST_BIN`, `MPIRUN_BIN`,
and `BENCH_MOUNT_TIMEOUT_SECS`. For example:

```sh
make benchmark-save-baseline BENCH_ITEMS=100 BENCH_ITERATIONS=2
make benchmark BENCH_ITEMS=100 BENCH_ITERATIONS=2
make benchmark BENCH_PROCS=4 MPIRUN_BIN=/opt/mpi/bin/mpirun
```

The default baseline is `.benchmarks/filesystem-baseline.json`, which is ignored
by Git. Baselines are machine-specific, and comparisons warn when host context
or the mdtest version differs.

## License

This crate itself is published under the MIT license while libfuse is published under
LGPL2+. Take special care to ensure the terms of the LGPL2+ are honored when using this
crate.
