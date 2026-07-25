//! Node-API port of a full read-write in-memory filesystem.
//!
//! Contrast with a raw low-level implementation: there is no inode table, no
//! inode allocator, no `get(&ino).ok_or(ENOENT)` on every call, no file-handle
//! table, and no `forget`/lifetime handling. The runtime owns identity and
//! lifetime; this filesystem stores per-node data, link counts, and directory
//! entries, and gets correct unlink-while-open behavior for free.
//!
//! Usage: `cargo run -p fuse3 --example memory_fs -- <mountpoint>`

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

use typed_fuse::{
    Caller, Cx, DirSink, Errno, FileKind, NodeAttr, NodeFs, NodeId, Opened, Session, SetAttr,
    TimeOrNow,
};

#[derive(Clone, Copy)]
struct DirEntry {
    id: NodeId,
    kind: FileKind,
}

enum Content {
    Dir(BTreeMap<OsString, DirEntry>),
    File(Vec<u8>),
    Symlink(PathBuf),
    /// FIFO, socket, or device node; the kernel handles I/O directly.
    Special,
}

struct Node {
    inner: RwLock<NodeData>,
}

struct NodeData {
    kind: FileKind,
    content: Content,
    perm: u16,
    uid: u32,
    gid: u32,
    rdev: u32,
    nlink: u32,
    atime: SystemTime,
    mtime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
    xattrs: BTreeMap<OsString, Vec<u8>>,
}

impl Node {
    fn new(kind: FileKind, content: Content, perm: u16, uid: u32, gid: u32, rdev: u32) -> Self {
        let now = SystemTime::now();
        Node {
            inner: RwLock::new(NodeData {
                kind,
                content,
                perm,
                uid,
                gid,
                rdev,
                nlink: if kind == FileKind::Directory { 2 } else { 1 },
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                xattrs: BTreeMap::new(),
            }),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, NodeData> {
        self.inner.read().unwrap()
    }
    fn write(&self) -> RwLockWriteGuard<'_, NodeData> {
        self.inner.write().unwrap()
    }
}

impl NodeData {
    fn size(&self) -> u64 {
        match &self.content {
            Content::File(v) => v.len() as u64,
            Content::Symlink(s) => s.as_os_str().as_bytes().len() as u64,
            Content::Dir(entries) => entries.len() as u64,
            Content::Special => 0,
        }
    }

    fn attr(&self) -> NodeAttr {
        NodeAttr {
            size: self.size(),
            kind: self.kind,
            perm: self.perm,
            uid: self.uid,
            gid: self.gid,
            rdev: self.rdev,
            nlink: self.nlink,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            crtime: self.crtime,
            ..Default::default()
        }
    }

    fn touch(&mut self, now: SystemTime) {
        self.mtime = now;
        self.ctime = now;
    }
}

struct MemoryFs {
    uid: u32,
    gid: u32,
}

impl MemoryFs {
    fn new() -> Self {
        MemoryFs {
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    /// Validates that `parent` is a directory not already containing `name`.
    fn check_new_entry(cx: &Cx<'_, Node>, parent: NodeId, name: &OsStr) -> Result<(), Errno> {
        match cx.get(parent) {
            Some(node) => {
                let node = node.read();
                let Content::Dir(entries) = &node.content else {
                    return Err(Errno::ENOTDIR);
                };
                if entries.contains_key(name) {
                    Err(Errno::EEXIST)
                } else {
                    Ok(())
                }
            }
            None => Err(Errno::ENOENT),
        }
    }

    fn entry(cx: &Cx<'_, Node>, parent: NodeId, name: &OsStr) -> Result<DirEntry, Errno> {
        let node = cx.get(parent).ok_or(Errno::ENOENT)?;
        let node = node.read();
        match &node.content {
            Content::Dir(entries) => entries.get(name).copied().ok_or(Errno::ENOENT),
            _ => Err(Errno::ENOTDIR),
        }
    }

    fn kind(cx: &Cx<'_, Node>, id: NodeId) -> Result<FileKind, Errno> {
        let node = cx.get(id).ok_or(Errno::ENOENT)?;
        let kind = node.read().kind;
        Ok(kind)
    }

    /// Records `id` under `name` in directory `parent` and bumps its times.
    fn link_into(
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        entry: DirEntry,
        now: SystemTime,
    ) {
        if let Some(node) = cx.get(parent) {
            let mut node = node.write();
            if let Content::Dir(entries) = &mut node.content {
                entries.insert(name.to_os_string(), entry);
            }
            if entry.kind == FileKind::Directory {
                node.nlink += 1;
            }
            node.touch(now);
        }
    }

    /// Inserts a fresh node, records it in `parent`, and returns its id.
    #[allow(clippy::too_many_arguments)]
    fn create_child(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        kind: FileKind,
        content: Content,
        perm: u16,
        rdev: u32,
    ) -> Result<NodeId, Errno> {
        Self::check_new_entry(cx, parent, name)?;
        let node = Node::new(kind, content, perm, self.uid, self.gid, rdev);
        let id = cx.insert(node, parent);
        Self::link_into(cx, parent, name, DirEntry { id, kind }, SystemTime::now());
        Ok(id)
    }
}

impl NodeFs for MemoryFs {
    type Node = Node;
    type Handle = ();
    type DirHandle = ();

    fn root(&mut self) -> Node {
        Node::new(
            FileKind::Directory,
            Content::Dir(BTreeMap::new()),
            0o755,
            self.uid,
            self.gid,
            0,
        )
    }

    fn getattr(&self, node: &Node, _h: Option<&()>, _c: &Caller) -> Result<NodeAttr, Errno> {
        Ok(node.read().attr())
    }

    fn opendir(&self, node: &Node, _flags: i32, _c: &Caller) -> Result<Opened<()>, Errno> {
        if node.read().kind != FileKind::Directory {
            return Err(Errno::ENOTDIR);
        }
        Ok(Opened::new(()))
    }

    fn setattr(
        &self,
        node: &Node,
        _h: Option<&()>,
        set: &SetAttr,
        _c: &Caller,
    ) -> Result<NodeAttr, Errno> {
        let mut node = node.write();
        let now = SystemTime::now();
        let mut changed = false;

        if let Some(mode) = set.mode {
            node.perm = (mode & 0o7777) as u16;
            changed = true;
        }
        if let Some(uid) = set.uid {
            node.uid = uid;
            changed = true;
        }
        if let Some(gid) = set.gid {
            node.gid = gid;
            changed = true;
        }
        if let Some(size) = set.size {
            if let Content::File(content) = &mut node.content {
                content.resize(size as usize, 0);
            }
            changed = true;
        }
        if let Some(atime) = set.atime {
            node.atime = match atime {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            changed = true;
        }
        if let Some(mtime) = set.mtime {
            node.mtime = match mtime {
                TimeOrNow::SpecificTime(t) => t,
                TimeOrNow::Now => now,
            };
            changed = true;
        }
        if let Some(ctime) = set.ctime {
            node.ctime = ctime;
        } else if changed {
            node.ctime = now;
        }

        Ok(node.attr())
    }

    fn readlink(&self, node: &Node, _c: &Caller) -> Result<PathBuf, Errno> {
        let node = node.read();
        match &node.content {
            Content::Symlink(target) => Ok(PathBuf::from(target)),
            _ => Err(Errno::EINVAL),
        }
    }

    fn lookup(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        _c: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        match Self::entry(cx, parent, name) {
            Ok(entry) => Ok(Some(entry.id)),
            Err(Errno::ENOENT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn mknod(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        _umask: u32,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        let (kind, content) = match mode & (libc::S_IFMT as u32) {
            x if x == libc::S_IFREG as u32 => (FileKind::RegularFile, Content::File(Vec::new())),
            x if x == libc::S_IFIFO as u32 => (FileKind::NamedPipe, Content::Special),
            x if x == libc::S_IFSOCK as u32 => (FileKind::Socket, Content::Special),
            x if x == libc::S_IFCHR as u32 => (FileKind::CharDevice, Content::Special),
            x if x == libc::S_IFBLK as u32 => (FileKind::BlockDevice, Content::Special),
            _ => return Err(Errno::EINVAL),
        };
        self.create_child(
            cx,
            parent,
            name,
            kind,
            content,
            (mode & 0o7777) as u16,
            rdev,
        )
    }

    fn mkdir(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        self.create_child(
            cx,
            parent,
            name,
            FileKind::Directory,
            Content::Dir(BTreeMap::new()),
            (mode & 0o7777) as u16,
            0,
        )
    }

    fn symlink(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        target: &Path,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        self.create_child(
            cx,
            parent,
            name,
            FileKind::Symlink,
            Content::Symlink(target.to_path_buf()),
            0o777,
            0,
        )
    }

    fn create(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        _c: &Caller,
    ) -> Result<(NodeId, Opened<()>), Errno> {
        let id = self.create_child(
            cx,
            parent,
            name,
            FileKind::RegularFile,
            Content::File(Vec::new()),
            (mode & 0o7777) as u16,
            0,
        )?;
        Ok((id, Opened::new(())))
    }

    fn unlink(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let id = Self::entry(cx, parent, name)?.id;
        if Self::kind(cx, id)? == FileKind::Directory {
            return Err(Errno::EISDIR);
        }
        let now = SystemTime::now();
        if let Some(node) = cx.get(parent) {
            let mut node = node.write();
            if let Content::Dir(entries) = &mut node.content {
                entries.remove(name);
            }
            node.touch(now);
        }
        if let Some(node) = cx.get(id) {
            let mut node = node.write();
            node.nlink = node.nlink.saturating_sub(1);
            node.ctime = now;
        }
        cx.remove_link(id);
        Ok(())
    }

    fn rmdir(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let id = Self::entry(cx, parent, name)?.id;
        let child = cx.get(id).ok_or(Errno::ENOENT)?;
        match &child.read().content {
            Content::Dir(entries) if entries.is_empty() => {}
            Content::Dir(_) => return Err(Errno::ENOTEMPTY),
            _ => return Err(Errno::ENOTDIR),
        }
        let now = SystemTime::now();
        if let Some(node) = cx.get(parent) {
            let mut node = node.write();
            if let Content::Dir(entries) = &mut node.content {
                entries.remove(name);
            }
            node.nlink = node.nlink.saturating_sub(1);
            node.touch(now);
        }
        if let Some(node) = cx.get(id) {
            let mut node = node.write();
            node.nlink = 0;
            node.ctime = now;
        }
        cx.remove_link(id);
        Ok(())
    }

    fn rename(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        newparent: NodeId,
        newname: &OsStr,
        _flags: u32,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let target = Self::entry(cx, parent, name)?.id;
        let target_kind = Self::kind(cx, target)?;
        let target_is_dir = target_kind == FileKind::Directory;

        // Inspect any entry being replaced at the destination.
        let existing = match Self::entry(cx, newparent, newname) {
            Ok(v) => Some(v),
            Err(Errno::ENOENT) => None,
            Err(e) => return Err(e),
        };
        if let Some(existing_entry) = existing {
            if existing_entry.id == target {
                return Ok(());
            }
            let existing_node = cx.get(existing_entry.id).ok_or(Errno::ENOENT)?;
            let existing_node = existing_node.read();
            match &existing_node.content {
                Content::Dir(entries) => {
                    if !target_is_dir {
                        return Err(Errno::EISDIR);
                    }
                    if !entries.is_empty() {
                        return Err(Errno::ENOTEMPTY);
                    }
                }
                _ => {
                    if target_is_dir {
                        return Err(Errno::ENOTDIR);
                    }
                }
            }
        }

        let now = SystemTime::now();
        if let Some(node) = cx.get(parent) {
            let mut node = node.write();
            if let Content::Dir(entries) = &mut node.content {
                entries.remove(name);
            }
            node.touch(now);
        }
        let replaced = if let Some(node) = cx.get(newparent) {
            let mut node = node.write();
            if let Content::Dir(entries) = &mut node.content {
                entries.insert(
                    newname.to_os_string(),
                    DirEntry {
                        id: target,
                        kind: target_kind,
                    },
                )
            } else {
                return Err(Errno::ENOTDIR);
            }
        } else {
            None
        };
        if let Some(node) = cx.get(newparent) {
            let mut node = node.write();
            node.touch(now);
        }
        if target_is_dir && parent != newparent {
            cx.reparent(target, newparent);
            if let Some(node) = cx.get(parent) {
                let mut node = node.write();
                node.nlink = node.nlink.saturating_sub(1);
            }
            if let Some(node) = cx.get(newparent) {
                let mut node = node.write();
                node.nlink += 1;
            }
        }
        if let Some(replaced_entry) = replaced {
            if replaced_entry.kind == FileKind::Directory {
                if let Some(node) = cx.get(newparent) {
                    let mut node = node.write();
                    node.nlink = node.nlink.saturating_sub(1);
                }
            }
            if let Some(node) = cx.get(replaced_entry.id) {
                let mut node = node.write();
                if replaced_entry.kind == FileKind::Directory {
                    node.nlink = 0;
                } else {
                    node.nlink = node.nlink.saturating_sub(1);
                }
                node.ctime = now;
            }
            cx.remove_link(replaced_entry.id);
        }
        Ok(())
    }

    fn link(
        &self,
        cx: &Cx<'_, Node>,
        id: NodeId,
        newparent: NodeId,
        newname: &OsStr,
        _c: &Caller,
    ) -> Result<NodeId, Errno> {
        if Self::kind(cx, id)? == FileKind::Directory {
            return Err(Errno::EPERM);
        }
        Self::check_new_entry(cx, newparent, newname)?;
        let now = SystemTime::now();
        let kind = Self::kind(cx, id)?;
        Self::link_into(cx, newparent, newname, DirEntry { id, kind }, now);
        cx.add_link(id);
        if let Some(node) = cx.get(id) {
            let mut node = node.write();
            node.nlink += 1;
            node.ctime = now;
        }
        Ok(id)
    }

    fn open(&self, node: &Node, _flags: i32, _c: &Caller) -> Result<Opened<()>, Errno> {
        if node.read().kind == FileKind::Directory {
            return Err(Errno::EISDIR);
        }
        Ok(Opened::new(()))
    }

    fn read<'a>(
        &'a self,
        node: &'a Node,
        _h: &'a (),
        offset: u64,
        size: usize,
        _c: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        let node = node.read();
        let Content::File(content) = &node.content else {
            return Err(Errno::EISDIR);
        };
        let offset = offset as usize;
        if offset >= content.len() {
            return Ok(Cow::Borrowed(&[]));
        }
        let end = (offset + size).min(content.len());
        Ok(Cow::Owned(content[offset..end].to_vec()))
    }

    fn write(
        &self,
        node: &Node,
        _h: &(),
        data: &[u8],
        offset: u64,
        _c: &Caller,
    ) -> Result<usize, Errno> {
        let now = SystemTime::now();
        let mut node = node.write();
        let Content::File(content) = &mut node.content else {
            return Err(Errno::EISDIR);
        };
        let offset = offset as usize;
        let end = offset + data.len();
        if end > content.len() {
            content.resize(end, 0);
        }
        content[offset..end].copy_from_slice(data);
        node.touch(now);
        Ok(data.len())
    }

    fn readdir(
        &self,
        _cx: &Cx<'_, Node>,
        node: &Node,
        this: NodeId,
        parent: NodeId,
        _dh: &(),
        offset: u64,
        sink: &mut dyn DirSink,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let node = node.read();
        let Content::Dir(entries) = &node.content else {
            return Err(Errno::ENOTDIR);
        };

        let mut cursor = offset;
        if cursor < 1 {
            if !sink.add(OsStr::new("."), this, FileKind::Directory, 1) {
                return Ok(());
            }
            cursor = 1;
        }
        if cursor < 2 {
            if !sink.add(OsStr::new(".."), parent, FileKind::Directory, 2) {
                return Ok(());
            }
            cursor = 2;
        }

        let skip = (cursor - 2) as usize;
        for (i, (name, entry)) in entries.iter().enumerate().skip(skip) {
            let next_offset = (i + 3) as u64;
            if !sink.add(name.as_os_str(), entry.id, entry.kind, next_offset) {
                break;
            }
        }
        Ok(())
    }

    // --- extended attributes (backed for real: on macOS an unsupported
    // xattr op makes the kernel spill to AppleDouble "._*" sidecar files). ---

    fn setxattr(
        &self,
        node: &Node,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let mut node = node.write();
        let exists = node.xattrs.contains_key(name);
        if flags & libc::XATTR_CREATE != 0 && exists {
            return Err(Errno::EEXIST);
        }
        if flags & libc::XATTR_REPLACE != 0 && !exists {
            return Err(Errno::ENODATA);
        }
        node.xattrs.insert(name.to_os_string(), value.to_vec());
        node.ctime = SystemTime::now();
        Ok(())
    }

    fn getxattr(
        &self,
        node: &Node,
        name: &OsStr,
        size: usize,
        _c: &Caller,
    ) -> Result<typed_fuse::XattrReply, Errno> {
        let node = node.read();
        let value = node.xattrs.get(name).ok_or(Errno::ENODATA)?;
        if size == 0 {
            Ok(typed_fuse::XattrReply::Size(value.len()))
        } else {
            Ok(typed_fuse::XattrReply::Data(value.clone()))
        }
    }

    fn listxattr(&self, node: &Node, size: usize, _c: &Caller) -> Result<typed_fuse::XattrReply, Errno> {
        let node = node.read();
        let mut names = Vec::new();
        for name in node.xattrs.keys() {
            names.extend_from_slice(name.as_os_str().as_bytes());
            names.push(0);
        }
        if size == 0 {
            Ok(typed_fuse::XattrReply::Size(names.len()))
        } else {
            Ok(typed_fuse::XattrReply::Data(names))
        }
    }

    fn removexattr(&self, node: &Node, name: &OsStr, _c: &Caller) -> Result<(), Errno> {
        let mut node = node.write();
        node.xattrs.remove(name).ok_or(Errno::ENODATA)?;
        node.ctime = SystemTime::now();
        Ok(())
    }
}

fn main() {
    let mountpoint = match std::env::args().nth(1) {
        Some(mountpoint) => mountpoint,
        None => {
            eprintln!("usage: memory_fs <mountpoint>");
            std::process::exit(1);
        }
    };

    if let Err(e) = Session::mount_and_run(MemoryFs::new(), Path::new(&mountpoint), &[]) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typed_fuse_core::Runtime;

    #[derive(Default)]
    struct Entries(Vec<(String, FileKind)>);

    impl DirSink for Entries {
        fn add(&mut self, name: &OsStr, _id: NodeId, kind: FileKind, _next_offset: u64) -> bool {
            self.0.push((name.to_string_lossy().into_owned(), kind));
            true
        }
    }

    #[test]
    fn directory_link_counts_follow_child_directories() {
        let rt = Runtime::new(MemoryFs::new());
        let caller = Caller::default();

        assert_eq!(
            rt.getattr(NodeId::ROOT.ino(), None, &caller).unwrap().nlink,
            2
        );
        let child = rt
            .mkdir(NodeId::ROOT.ino(), OsStr::new("child"), 0o755, 0, &caller)
            .unwrap();
        assert_eq!(child.attr.nlink, 2);
        assert_eq!(
            rt.getattr(NodeId::ROOT.ino(), None, &caller).unwrap().nlink,
            3
        );

        rt.rmdir(NodeId::ROOT.ino(), OsStr::new("child"), &caller)
            .unwrap();
        assert_eq!(
            rt.getattr(NodeId::ROOT.ino(), None, &caller).unwrap().nlink,
            2
        );
    }

    #[test]
    fn readdir_reports_each_entry_kind() {
        let rt = Runtime::new(MemoryFs::new());
        let caller = Caller::default();
        rt.mkdir(NodeId::ROOT.ino(), OsStr::new("dir"), 0o755, 0, &caller)
            .unwrap();
        rt.symlink(
            NodeId::ROOT.ino(),
            OsStr::new("link"),
            Path::new("target"),
            &caller,
        )
        .unwrap();
        rt.create(NodeId::ROOT.ino(), OsStr::new("file"), 0o644, 0, 0, &caller)
            .unwrap();

        let open = rt.opendir(NodeId::ROOT.ino(), 0, &caller).unwrap();
        let mut entries = Entries::default();
        rt.readdir(NodeId::ROOT.ino(), open.fh, 0, &mut entries, &caller)
            .unwrap();

        assert!(entries.0.contains(&("dir".into(), FileKind::Directory)));
        assert!(entries.0.contains(&("link".into(), FileKind::Symlink)));
        assert!(entries.0.contains(&("file".into(), FileKind::RegularFile)));
    }
}
