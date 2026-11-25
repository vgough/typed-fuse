#![cfg(any(target_os = "linux", target_os = "freebsd"))]

#[path = "../examples/support/path_passthrough.rs"]
mod path_passthrough;

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU64;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use path_passthrough::ReadOnlyPathFs;
use typed_fuse::{
    Admission, Caller, Errno, NodeAttr, PathEntry, PathFilesystem, PathNodeFs, PathNodeRef, PathOp,
    Session, SessionConfig, ThreadingMode,
};

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

static LIVE_MOUNT_LOCK: Mutex<()> = Mutex::new(());

fn with_mounted(
    filesystem: impl PathFilesystem + 'static,
    config: SessionConfig,
    check: impl FnOnce(&Path),
) {
    let _lock = LIVE_MOUNT_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let mountpoint = tempfile::tempdir().unwrap();
    let mounted_at = mountpoint.path().to_path_buf();
    let thread_mountpoint = mounted_at.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut session = match Session::new_with_config(PathNodeFs::new(filesystem), &[], config) {
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

    check(&mounted_at);

    assert!(
        unmount(&mounted_at),
        "failed to unmount live test filesystem"
    );
    guard.active = false;
    worker.join().unwrap().unwrap();
}

#[test]
#[ignore = "requires a working FUSE kernel device and unmount helper"]
fn root_listing_reflects_backing_changes() {
    let backing = tempfile::tempdir().unwrap();
    std::fs::write(backing.path().join("a"), []).unwrap();
    with_mounted(
        ReadOnlyPathFs::new(backing.path()),
        SessionConfig::default(),
        |mounted_at| {
            assert_eq!(names(mounted_at), [OsString::from("a")]);
            std::fs::write(backing.path().join("b"), []).unwrap();
            assert_eq!(
                names(mounted_at),
                [OsString::from("a"), OsString::from("b")]
            );
            std::fs::remove_file(backing.path().join("a")).unwrap();
            assert_eq!(names(mounted_at), [OsString::from("b")]);
        },
    );
}

#[derive(Default)]
struct LookupIds {
    admitted: HashSet<NonZeroU64>,
    handled: Vec<NonZeroU64>,
}

struct CorrelatingFs {
    inner: ReadOnlyPathFs,
    ids: Arc<Mutex<LookupIds>>,
}

impl PathFilesystem for CorrelatingFs {
    type NodeState = ();
    type Handle = ();
    type DirHandle = ();

    fn root_state(&mut self) -> Arc<()> {
        self.inner.root_state()
    }

    fn admit(&self, request: &Admission<'_>) -> Result<(), Errno> {
        if request.op() == PathOp::Lookup {
            let id = request.caller().correlation_id.ok_or(Errno::EIO)?;
            self.ids.lock().unwrap().admitted.insert(id);
        }
        Ok(())
    }

    fn lookup(
        &self,
        parent: PathNodeRef<'_, ()>,
        name: &OsStr,
        caller: &Caller,
    ) -> Result<Option<PathEntry<()>>, Errno> {
        let id = caller.correlation_id.ok_or(Errno::EIO)?;
        self.ids.lock().unwrap().handled.push(id);
        self.inner.lookup(parent, name, caller)
    }

    fn getattr(
        &self,
        node: PathNodeRef<'_, ()>,
        handle: Option<&()>,
        caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        self.inner.getattr(node, handle, caller)
    }
}

fn check_mounted_request_correlation(threading: ThreadingMode) {
    let backing = tempfile::tempdir().unwrap();
    let ids = Arc::new(Mutex::new(LookupIds::default()));
    let filesystem = CorrelatingFs {
        inner: ReadOnlyPathFs::new(backing.path()),
        ids: Arc::clone(&ids),
    };
    for file in ["first", "second"] {
        std::fs::write(backing.path().join(file), []).unwrap();
    }

    with_mounted(filesystem, SessionConfig { threading }, |mounted_at| {
        thread::scope(|scope| {
            for file in ["first", "second"] {
                scope.spawn(move || std::fs::metadata(mounted_at.join(file)).unwrap());
            }
        });
        let ids = ids.lock().unwrap();
        assert!(ids.handled.len() >= 2);
        let handled: HashSet<_> = ids.handled.iter().copied().collect();
        assert_eq!(handled.len(), ids.handled.len());
        assert_eq!(handled, ids.admitted);
    });
}

#[test]
#[ignore = "requires a working FUSE kernel device and unmount helper"]
fn single_threaded_requests_share_ids_across_callbacks() {
    check_mounted_request_correlation(ThreadingMode::SingleThreaded);
}

#[test]
#[ignore = "requires a working FUSE kernel device and unmount helper"]
fn multithreaded_requests_share_ids_across_callbacks() {
    check_mounted_request_correlation(SessionConfig::default().threading);
}
