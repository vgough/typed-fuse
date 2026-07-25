//! Backend-neutral attribute types. These carry no inode number (the
//! runtime supplies it separately) and no raw C types; `fuse3`'s `conv`
//! module is responsible for turning them into `stat`/`statvfs`/etc.

use std::time::{SystemTime, UNIX_EPOCH};

/// The type of a filesystem entry, corresponding to the `S_IFMT` bits of
/// `st_mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum FileKind {
    #[default]
    RegularFile,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    NamedPipe,
    Socket,
}

/// Filesystem entry attributes (the backend-neutral equivalent of
/// `struct stat`), minus the inode number.
///
/// The filesystem is responsible for supplying an accurate `nlink` value.
#[derive(Clone, Copy, Debug)]
pub struct NodeAttr {
    pub size: u64,
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    /// Creation time. Only meaningful on macOS; ignored on Linux.
    pub crtime: SystemTime,
    pub kind: FileKind,
    pub perm: u16,
    /// Number of hard links to this node.
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    /// BSD file flags (`chflags(2)`). Only meaningful on macOS; ignored on
    /// Linux.
    pub flags: u32,
}

impl Default for NodeAttr {
    fn default() -> Self {
        NodeAttr {
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileKind::default(),
            perm: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 0,
            flags: 0,
        }
    }
}

/// Either a specific point in time, or "now" (as requested by e.g.
/// `utimes(2)` with `UTIME_NOW`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TimeOrNow {
    SpecificTime(SystemTime),
    Now,
}

/// The decoded `setattr` request: which fields the caller wants changed,
/// and to what. Only the `Some` fields should be applied.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<TimeOrNow>,
    pub mtime: Option<TimeOrNow>,
    pub ctime: Option<SystemTime>,
    /// Creation time (macOS only; the `FUSE_SET_ATTR_BTIME` bit).
    pub crtime: Option<SystemTime>,
    /// BSD file flags (macOS only; the `FUSE_SET_ATTR_FLAGS` bit).
    pub flags: Option<u32>,
}

/// Filesystem-wide statistics, the backend-neutral equivalent of
/// `statvfs`/`statfs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}
