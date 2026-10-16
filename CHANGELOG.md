# Changelog

## 0.8.0

### Breaking changes

- `MountConfig` has a new public `subtype` field, rendered as `subtype=`, so
  a daemon can report its filesystem type (`fuse.<subtype>` on Linux) the way
  `fuse_main`-based daemons do. `MountConfig::new` leaves it unset; struct
  literals that list every field without `..` need to add it.

### Other changes

- `MountConfig::parse_option_words` sets `fs_name`, `subtype` and `volname`
  from `fsname=`, `subtype=` and `volname=` words instead of passing them
  through, so a user-supplied name replaces the default rather than adding a
  second option that libfuse may ignore.
- `fsname=`, `subtype=` and `volname=` values are escaped for libfuse's
  option parser, so a name containing `,` or `\` (such as a path) no longer
  breaks the mount.

## 0.7.0

### Breaking changes

- `PathFilesystem` now exposes `list_dir` instead of `readdir` and
  `readdirplus`. It returns a complete `DirBuffer`; `PathNodeFs` owns resume
  cookies and the current per-handle listing.
- `DirBuffer::push` now takes only a name, kind and state. Use the new
  `DirBuffer::push_plus(name, attr, state)` to record attributes;
  `readdirplus` looks up entries added with `push`. `DirEntry::attr` and the
  `attr` argument of `PathPlusDirSink::add` are now `Option<NodeAttr>`.
- `PathNodeFs` wraps each filesystem directory handle in `PathDirHandle`.
  `opendir`, `releasedir`, and `fsyncdir` still receive the filesystem's own
  `DirHandle` through the `PathFilesystem` trait.

Directory listings are now created lazily on first read and refreshed on
every offset-zero read. This makes `rewinddir` and reused FreeBSD directory
handles observe current contents while preserving one consistent snapshot for
nonzero resume offsets.

Resume cookies name the listing that issued them, and each handle keeps its
four most recent listings, so interleaved listings of one directory on a shared
FreeBSD handle no longer resume each other's snapshot. A resumed read after a
name in the directory was created, removed, or renamed through the mount looks
each entry up again, so removed names are neither reported nor brought back
into the adapter's namespace. Listings are limited to 2^27 - 1 entries. A
listing read to its end is freed by the final empty read rather than held
until `releasedir`.

To migrate, keep access and type checks in `opendir`, move snapshot creation
to `list_dir`, and remove path-level `readdir` and `readdirplus`
implementations. Change `push` calls that pass attributes to `push_plus`.

`typed_fuse::passthrough::file_kind_from_file_type` converts a
`std::fs::FileType` to a `FileKind`.
