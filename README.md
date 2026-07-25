# typed-fuse

A safe, Rust-friendly FUSE API built on top of `libfuse-sys`. 

This repository is a workspace containing the following crates:
* **[`typed-fuse`](typed-fuse/)**: A safe, high-level wrapper over the low-level FUSE 3.x API.
* **[`typed-fuse-core`](typed-fuse-core/)**: A backend-neutral, node-tracking core for building FUSE filesystems.
* **[`libfuse-sys`](libfuse-sys/)**: Raw FFI bindings to libfuse. 

## Acknowledgments

This project started as a fork of the excellent [libfuse-sys](https://github.com/richard-w/libfuse-sys) crate by Richard Wiedenhöft, before expanding to include higher-level, safe abstractions for building filesystems. The raw FFI bindings remain in the `libfuse-sys` sub-crate.

### Changes to `libfuse-sys` vs Upstream

Compared to the upstream [`richard-w/libfuse-sys`](https://github.com/richard-w/libfuse-sys) repository, key changes in this fork's `libfuse-sys` sub-crate include:

* **FUSE 3.x Exclusivity**: Dropped support for legacy FUSE 2.x API feature flags (`fuse_11` through `fuse_30`). FUSE 3 (`fuse3` pkg-config module) is now required, and the default API version was raised from 30 to 35.
* **FUSE 3.12+ Support**: Added the `fuse_312` feature flag (requiring `libfuse >= 3.12.0`).
* **macOS / macFUSE Compatibility**:
  * Added support for macFUSE ABI extensions (`fuse_main_real_versioned`).
  * Provided platform-portable wrappers (`session_loop_mt_312`, `parse_cmdline_312`, `loop_cfg_*`) to reconcile symbol naming differences between macFUSE on macOS (which appends `_312` suffixes) and Linux libfuse (which uses symbol versioning).
* **Robust Build & Header Resolution**: Added compiler resource path probing (locating `stdarg.h` via `CLANG_PATH`, `CC`, `gcc`, or `cc`) to resolve `bindgen` errors on systems where `libclang` fails to locate compiler-internal include paths.
* **Updated Tooling & Bindgen**: Upgraded `bindgen` to 0.72 with refined symbol allowlist filtering and clippy lint suppressions for generated bindings.
* **Workspace Restructuring**: Reorganized into a sub-crate within the `typed-fuse` Cargo workspace and added raw FFI examples (`hello_ll_raw.rs`).

---

## The `typed-fuse` crate: a safe wrapper

If you're writing a new filesystem, prefer the [`typed-fuse`](typed-fuse/) crate. It's a safe, Rust-friendly low-level FUSE API built on top of `libfuse-sys` and `typed-fuse-core`: implement the concurrent `NodeFs` trait with standard Rust types such as `&OsStr` and `Result<T, Errno>`, then hand it to `Session` - no `unsafe`, no C types, no `#[cfg(target_os = ...)]` required in your code.

```rust
use std::path::Path;
use typed_fuse::{Caller, Errno, NodeAttr, NodeFs, Session};

struct HelloFs;
struct Node;

impl NodeFs for HelloFs {
    type Node = Node;
    type Handle = ();
    type DirHandle = ();

    fn root(&mut self) -> Node { Node }
    fn getattr(
        &self,
        _node: &Node,
        _handle: Option<&()>,
        _caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Ok(NodeAttr::default())
    }
    // ... lookup, open, read, readdir
}

Session::mount_and_run(HelloFs, Path::new(&mountpoint), &[])?;
```

Sessions dispatch concurrently by default. Node and handle payloads are `Send + Sync`, and implementations use interior synchronization for mutable state. A single-threaded runtime mode is available through `SessionConfig`.

See `typed-fuse/README.md` for details and `typed-fuse/examples/hello_ll.rs` for a full example.

## Using `libfuse-sys` directly

If you only need the raw bindings, add the dependency to your `Cargo.toml`:
```toml
[dependencies]
libfuse-sys = { version = "0.4", features = ["fuse_312"] }
libc = "0.2"
```
You can select a FUSE API version. Currently supported are:
* `fuse_31`
* `fuse_35`
* `fuse_312` (requires libfuse 3.12 or later)

If no version is selected the crate defaults to version 35.

`libfuse-sys/examples/hello_ll_raw.rs` is a Rust port of libfuse's classic `hello_ll.c`.

## License

This project is published under the MIT license, while libfuse itself is published under LGPL2+. Take special care to ensure the terms of the LGPL2+ are honored when using these crates.
