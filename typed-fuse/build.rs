//! Probes the system libfuse headers for a handful of API additions that
//! post-date the widely-packaged 3.14 release (`parallel_direct_writes`,
//! the `tmpfile`/`statx` lowlevel ops, the versioned session constructor).
//! `libfuse-sys`'s bindgen output only exposes what the installed headers
//! actually declare, so code here that calls into those symbols must be
//! compiled out when they're absent.

use std::fs;

fn header_contains(fuse_lib: &pkg_config::Library, header: &str, needle: &str) -> bool {
    fuse_lib
        .include_paths
        .iter()
        .map(|dir| dir.join(header))
        .filter_map(|path| fs::read_to_string(path).ok())
        .any(|contents| contents.contains(needle))
}

fn main() {
    let mut pkgcfg = pkg_config::Config::new();
    pkgcfg.cargo_metadata(false);
    let fuse_lib = pkgcfg
        .probe("fuse3")
        .expect("fuse3 pkg-config module not found");

    println!("cargo:rustc-check-cfg=cfg(has_parallel_direct_writes)");
    println!("cargo:rustc-check-cfg=cfg(has_tmpfile_op)");
    println!("cargo:rustc-check-cfg=cfg(has_statx_op)");
    println!("cargo:rustc-check-cfg=cfg(has_session_new_versioned)");

    if header_contains(&fuse_lib, "fuse_common.h", "parallel_direct_writes") {
        println!("cargo:rustc-cfg=has_parallel_direct_writes");
    }
    if header_contains(&fuse_lib, "fuse_lowlevel.h", "*tmpfile)") {
        println!("cargo:rustc-cfg=has_tmpfile_op");
    }
    if header_contains(&fuse_lib, "fuse_lowlevel.h", "*statx)") {
        println!("cargo:rustc-cfg=has_statx_op");
    }
    // Since libfuse 3.17, fuse_session_new() is a macro over a static inline
    // wrapper around fuse_session_new_versioned(). bindgen emits neither, so
    // the plain symbol is absent from the bindings against those headers.
    if header_contains(&fuse_lib, "fuse_lowlevel.h", "fuse_session_new_versioned") {
        println!("cargo:rustc-cfg=has_session_new_versioned");
    }
}
