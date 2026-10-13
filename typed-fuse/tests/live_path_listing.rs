#![cfg(any(target_os = "linux", target_os = "freebsd"))]

#[path = "../examples/support/path_passthrough.rs"]
mod path_passthrough;

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use path_passthrough::ReadOnlyPathFs;
use typed_fuse::{PathNodeFs, Session};

fn names(path: &Path) -> Vec<OsString> {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

fn unmount(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    let mut command = Command::new("fusermount3");
    #[cfg(target_os = "freebsd")]
    let mut command = Command::new("umount");
    #[cfg(target_os = "linux")]
    command.arg("-u");
    command
        .arg(path)
        .status()
        .is_ok_and(|status| status.success())
}

struct UnmountGuard<'a> {
    path: &'a Path,
    active: bool,
}

impl Drop for UnmountGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unmount(self.path);
        }
    }
}

#[test]
#[ignore = "requires a working FUSE kernel device and unmount helper"]
fn root_listing_reflects_backing_changes() {
    let backing = tempfile::tempdir().unwrap();
    let mountpoint = tempfile::tempdir().unwrap();
    std::fs::write(backing.path().join("a"), []).unwrap();

    let filesystem = ReadOnlyPathFs::new(backing.path());
    let mounted_at = mountpoint.path().to_path_buf();
    let thread_mountpoint = mounted_at.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut session = match Session::new(PathNodeFs::new(filesystem), &[]) {
            Ok(session) => session,
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return Err(error);
            }
        };
        if let Err(error) = session.mount(&thread_mountpoint) {
            let _ = ready_tx.send(Err(error.to_string()));
            return Err(error);
        }
        ready_tx.send(Ok(())).unwrap();
        session.run()
    });

    // Arm the guard before waiting, so a mount that completes after the
    // wait times out is still unmounted when the test panics.
    let mut guard = UnmountGuard {
        path: &mounted_at,
        active: true,
    };
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("mount worker did not report readiness")
        .expect("mount failed");

    assert_eq!(names(&mounted_at), [OsString::from("a")]);
    std::fs::write(backing.path().join("b"), []).unwrap();
    assert_eq!(
        names(&mounted_at),
        [OsString::from("a"), OsString::from("b")]
    );
    std::fs::remove_file(backing.path().join("a")).unwrap();
    assert_eq!(names(&mounted_at), [OsString::from("b")]);

    assert!(
        unmount(&mounted_at),
        "failed to unmount live test filesystem"
    );
    guard.active = false;
    worker.join().unwrap().unwrap();
}
