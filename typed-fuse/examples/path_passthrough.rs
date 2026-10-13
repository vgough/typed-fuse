//! Minimal read-only `PathFilesystem` passthrough.
//!
//! Usage: `cargo run -p typed-fuse --example path_passthrough -- <backing> <mountpoint>`

#[path = "support/path_passthrough.rs"]
mod path_passthrough;

use std::path::Path;

use path_passthrough::ReadOnlyPathFs;
use typed_fuse::mount::{MountConfig, mount_blocking};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(backing) = args.next() else {
        eprintln!("usage: path_passthrough <backing> <mountpoint>");
        std::process::exit(2);
    };
    let Some(mountpoint) = args.next() else {
        eprintln!("usage: path_passthrough <backing> <mountpoint>");
        std::process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("usage: path_passthrough <backing> <mountpoint>");
        std::process::exit(2);
    }

    let filesystem = ReadOnlyPathFs::new(backing);
    let config = MountConfig::new("typed-fuse-path-passthrough");
    if let Err(error) = mount_blocking(filesystem, Path::new(&mountpoint), &config, true) {
        eprintln!("mount failed: {error}");
        std::process::exit(1);
    }
}
