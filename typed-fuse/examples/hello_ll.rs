//! Node-API port of libfuse's classic `hello_ll.c`: a read-only filesystem
//! with a single file `hello` containing "Hello World!\n".
//!
//! Compared to the raw port, this manages no inode numbers, no file handles,
//! no lifetime, and no C types. The two nodes are seeded in `populate`; the
//! runtime assigns their inodes and tracks everything else.
//!
//! Usage: `cargo run -p fuse3 --example hello_ll -- <mountpoint>`

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::RwLock;
use std::time::SystemTime;

use typed_fuse::{Caller, Cx, Errno, FileKind, NodeAttr, NodeFs, NodeId, Opened, Session};

const HELLO_CONTENT: &[u8] = b"Hello World!\n";

enum Node {
    Dir {
        entries: RwLock<BTreeMap<String, NodeId>>,
    },
    File {
        content: &'static [u8],
    },
}

struct HelloFs {
    mounted_at: SystemTime,
}

impl HelloFs {
    fn attr_of(&self, node: &Node) -> NodeAttr {
        let (kind, perm, size, nlink) = match node {
            Node::Dir { .. } => (FileKind::Directory, 0o755, 0, 2),
            Node::File { content } => (FileKind::RegularFile, 0o444, content.len() as u64, 1),
        };
        NodeAttr {
            kind,
            perm,
            size,
            nlink,
            atime: self.mounted_at,
            mtime: self.mounted_at,
            ctime: self.mounted_at,
            crtime: self.mounted_at,
            ..Default::default()
        }
    }
}

impl NodeFs for HelloFs {
    type Node = Node;
    type Handle = ();
    type DirHandle = ();

    fn root(&mut self) -> Node {
        Node::Dir {
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    fn populate(&mut self, cx: &Cx<'_, Node>) {
        let hello = cx.insert(
            Node::File {
                content: HELLO_CONTENT,
            },
            NodeId::ROOT,
        );
        if let Some(root) = cx.get(NodeId::ROOT) {
            if let Node::Dir { entries } = &*root {
                entries.write().unwrap().insert("hello".to_string(), hello);
            }
        }
    }

    fn getattr(&self, node: &Node, _h: Option<&()>, _c: &Caller) -> Result<NodeAttr, Errno> {
        Ok(self.attr_of(node))
    }

    fn lookup(
        &self,
        cx: &Cx<'_, Node>,
        parent: NodeId,
        name: &OsStr,
        _c: &Caller,
    ) -> Result<Option<NodeId>, Errno> {
        match cx.get(parent).as_deref() {
            Some(Node::Dir { entries }) => Ok(entries
                .read()
                .unwrap()
                .get(&name.to_string_lossy().into_owned())
                .copied()),
            Some(_) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    fn open(&self, node: &Node, _flags: i32, _c: &Caller) -> Result<Opened<()>, Errno> {
        match node {
            Node::File { .. } => Ok(Opened::new(())),
            Node::Dir { .. } => Err(Errno::EISDIR),
        }
    }

    fn read<'a>(
        &'a self,
        node: &'a Node,
        _h: &'a (),
        offset: u64,
        size: usize,
        _c: &Caller,
    ) -> Result<Cow<'a, [u8]>, Errno> {
        let Node::File { content } = node else {
            return Err(Errno::EISDIR);
        };
        let offset = offset as usize;
        if offset >= content.len() {
            return Ok(Cow::Borrowed(&[]));
        }
        let end = (offset + size).min(content.len());
        Ok(Cow::Borrowed(&content[offset..end]))
    }

    fn readdir(
        &self,
        _cx: &Cx<'_, Node>,
        node: &Node,
        this: NodeId,
        parent: NodeId,
        _dh: &(),
        offset: u64,
        sink: &mut dyn typed_fuse::DirSink,
        _c: &Caller,
    ) -> Result<(), Errno> {
        let Node::Dir { entries } = node else {
            return Err(Errno::ENOTDIR);
        };

        // Offsets: 1 => ".", 2 => "..", 3.. => real entries.
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
        let entries = entries.read().unwrap();
        for (i, (name, &id)) in entries.iter().enumerate().skip(skip) {
            let next_offset = (i + 3) as u64;
            if !sink.add(OsStr::new(name), id, FileKind::RegularFile, next_offset) {
                break;
            }
        }
        Ok(())
    }
}

fn main() {
    let mountpoint = match std::env::args().nth(1) {
        Some(mountpoint) => mountpoint,
        None => {
            eprintln!("usage: hello_ll <mountpoint>");
            std::process::exit(1);
        }
    };

    let fs = HelloFs {
        mounted_at: SystemTime::now(),
    };

    if let Err(e) = Session::mount_and_run(fs, Path::new(&mountpoint), &[]) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
