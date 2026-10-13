use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use typed_fuse::passthrough::{file_attr_from_metadata, file_kind_from_file_type};
use typed_fuse::{
    Caller, DirBuffer, Errno, NodeAttr, Opened, PathEntry, PathFilesystem, PathNodeRef,
};

/// A minimal read-only passthrough used by the path-listing example and live
/// regression test.
pub struct ReadOnlyPathFs {
    root: PathBuf,
}

impl ReadOnlyPathFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn backing_path(&self, node: PathNodeRef<'_, ()>) -> Result<PathBuf, Errno> {
        let path = node.path().ok_or(Errno::ENOENT)?;
        let relative = path
            .strip_prefix(Path::new("/"))
            .map_err(|_| Errno::EINVAL)?;
        Ok(self.root.join(relative))
    }

    fn attr(path: &Path) -> Result<NodeAttr, Errno> {
        let metadata = std::fs::symlink_metadata(path).map_err(Errno::from)?;
        Ok(file_attr_from_metadata(&metadata, metadata.len()))
    }
}

impl PathFilesystem for ReadOnlyPathFs {
    type NodeState = ();
    type Handle = ();
    type DirHandle = ();

    fn root_state(&mut self) -> Arc<Self::NodeState> {
        Arc::new(())
    }

    fn lookup(
        &self,
        parent: PathNodeRef<'_, Self::NodeState>,
        name: &OsStr,
        _caller: &Caller,
    ) -> Result<Option<PathEntry<Self::NodeState>>, Errno> {
        let path = self.backing_path(parent)?.join(name);
        match Self::attr(&path) {
            Ok(attr) => Ok(Some(PathEntry::new(attr, Arc::new(())))),
            Err(error) if error == Errno::ENOENT => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn getattr(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        _handle: Option<&Self::Handle>,
        _caller: &Caller,
    ) -> Result<NodeAttr, Errno> {
        Self::attr(&self.backing_path(node)?)
    }

    fn opendir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        _flags: i32,
        _caller: &Caller,
    ) -> Result<Opened<Self::DirHandle>, Errno> {
        let metadata = std::fs::metadata(self.backing_path(node)?).map_err(Errno::from)?;
        if !metadata.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        Ok(Opened::new(()))
    }

    fn list_dir(
        &self,
        node: PathNodeRef<'_, Self::NodeState>,
        _handle: &Self::DirHandle,
        _caller: &Caller,
    ) -> Result<DirBuffer<Self::NodeState>, Errno> {
        let is_root = node.path() == Some(Path::new("/"));
        let path = self.backing_path(node)?;
        let parent = if is_root {
            path.as_path()
        } else {
            path.parent().unwrap_or(&path)
        };
        let mut listing = DirBuffer::new();
        listing.push_dots(Self::attr(&path)?, Self::attr(parent)?);
        for entry in std::fs::read_dir(path).map_err(Errno::from)? {
            let entry = entry.map_err(Errno::from)?;
            // Basic readdir needs only the kind, which the directory entry
            // usually carries. An entry removed since `read_dir` started is
            // skipped rather than failing the listing.
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            listing.push(
                entry.file_name(),
                file_kind_from_file_type(file_type),
                Arc::new(()),
            );
        }
        Ok(listing)
    }
}
