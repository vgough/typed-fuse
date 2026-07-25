//! POSIX error numbers returned by fallible [`NodeFs`](crate::NodeFs)
//! methods.

/// A POSIX error number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(i32);

impl Errno {
    pub const EPERM: Errno = Errno(libc::EPERM);
    pub const ENOENT: Errno = Errno(libc::ENOENT);
    pub const EIO: Errno = Errno(libc::EIO);
    pub const EACCES: Errno = Errno(libc::EACCES);
    pub const EEXIST: Errno = Errno(libc::EEXIST);
    pub const ENOTDIR: Errno = Errno(libc::ENOTDIR);
    pub const EISDIR: Errno = Errno(libc::EISDIR);
    pub const EINVAL: Errno = Errno(libc::EINVAL);
    pub const ENOSYS: Errno = Errno(libc::ENOSYS);
    pub const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
    pub const ERANGE: Errno = Errno(libc::ERANGE);
    /// "No data available" - used for missing extended attributes.
    pub const ENODATA: Errno = Errno(libc::ENODATA);
    /// The error used for missing extended attributes.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const ENOATTR: Errno = Errno(libc::ENODATA);
    /// The error used for missing extended attributes.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub const ENOATTR: Errno = Errno(libc::ENOATTR);
    pub const EILSEQ: Errno = Errno(libc::EILSEQ);
    pub const ENOSPC: Errno = Errno(libc::ENOSPC);
    pub const EROFS: Errno = Errno(libc::EROFS);
    pub const EBADF: Errno = Errno(libc::EBADF);
    pub const ENAMETOOLONG: Errno = Errno(libc::ENAMETOOLONG);
    pub const ENXIO: Errno = Errno(libc::ENXIO);
    pub const EOPNOTSUPP: Errno = Errno(libc::EOPNOTSUPP);

    /// Wraps a raw `errno` value.
    pub const fn from_raw(errno: i32) -> Self {
        Errno(errno)
    }

    /// Returns the raw `errno` value.
    pub fn raw(self) -> i32 {
        self.0
    }
}

impl From<i32> for Errno {
    fn from(value: i32) -> Self {
        Errno(value)
    }
}

impl From<std::io::Error> for Errno {
    fn from(err: std::io::Error) -> Self {
        Errno(err.raw_os_error().unwrap_or(libc::EIO))
    }
}

impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "errno {}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_from_io_error() {
        let io_err = std::io::Error::from_raw_os_error(libc::ENOENT);
        let errno: Errno = io_err.into();
        assert_eq!(errno, Errno::ENOENT);

        let other_err = std::io::Error::other("boom");
        let errno: Errno = other_err.into();
        assert_eq!(errno, Errno::EIO);
    }

    #[test]
    fn errno_raw_and_from_raw() {
        let e = Errno::from_raw(5);
        assert_eq!(e.raw(), 5);
        assert_eq!(Errno::from(5), e);
    }
}
