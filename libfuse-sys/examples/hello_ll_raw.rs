//! Port of libfuse's `example/hello_ll.c` to the lowlevel FUSE 3.x API.
//!
//! A read-only filesystem with a single file `hello` containing "Hello World!\n".
//!
//! Usage: `cargo run --example hello_ll_raw -- <mountpoint>`

mod imp {
    use libfuse_sys::fuse_lowlevel::*;
    use std::ffi::{CStr, CString};
    use std::mem::size_of;
    use std::os::raw::{c_char, c_int, c_void};
    use std::ptr;

    const HELLO_STR: &[u8] = b"Hello World!\n";
    const HELLO_NAME: &[u8] = b"hello\0";

    type Attr = stat;
    type EntryParam = fuse_entry_param;

    #[derive(Clone, Copy)]
    struct MountTime {
        seconds: i64,
        nanoseconds: i64,
    }

    impl MountTime {
        fn now() -> Self {
            let duration = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Self {
                seconds: duration.as_secs() as i64,
                nanoseconds: duration.subsec_nanos() as i64,
            }
        }
    }

    // On macOS, libfuse's headers declare fuse_reply_entry()/fuse_reply_attr()/
    // fuse_add_direntry() with "Darwin extensions" enabled by default, which aliases
    // them (via an asm symbol rename) to `<name>$DARWIN` and switches their argument
    // structs to the extended Darwin layouts. This example sticks to the portable
    // vanilla structs, so bind directly to the plain symbol names instead of going
    // through libfuse_sys's aliased declarations.
    #[cfg(target_os = "macos")]
    extern "C" {
        #[link_name = "fuse_reply_entry"]
        fn fuse_reply_entry_vanilla(req: fuse_req_t, e: *const EntryParam) -> c_int;
        #[link_name = "fuse_reply_attr"]
        fn fuse_reply_attr_vanilla(req: fuse_req_t, attr: *const Attr, attr_timeout: f64) -> c_int;
        #[link_name = "fuse_add_direntry"]
        fn fuse_add_direntry_vanilla(
            req: fuse_req_t,
            buf: *mut c_char,
            bufsize: usize,
            name: *const c_char,
            stbuf: *const Attr,
            off: off_t,
        ) -> usize;
    }
    #[cfg(target_os = "macos")]
    use fuse_add_direntry_vanilla as fuse_add_direntry;
    #[cfg(target_os = "macos")]
    use fuse_reply_attr_vanilla as fuse_reply_attr;
    #[cfg(target_os = "macos")]
    use fuse_reply_entry_vanilla as fuse_reply_entry;

    fn mount_time(req: fuse_req_t) -> &'static MountTime {
        unsafe { &*(fuse_req_userdata(req) as *const MountTime) }
    }

    fn set_times(attr: &mut Attr, mounted_at: MountTime) {
        #[cfg(target_os = "macos")]
        {
            attr.st_atimespec.tv_sec = mounted_at.seconds as _;
            attr.st_atimespec.tv_nsec = mounted_at.nanoseconds as _;
            attr.st_mtimespec = attr.st_atimespec;
            attr.st_ctimespec = attr.st_atimespec;
            attr.st_birthtimespec = attr.st_atimespec;
        }
        #[cfg(not(target_os = "macos"))]
        {
            attr.st_atim.tv_sec = mounted_at.seconds as _;
            attr.st_atim.tv_nsec = mounted_at.nanoseconds as _;
            attr.st_mtim.tv_sec = mounted_at.seconds as _;
            attr.st_mtim.tv_nsec = mounted_at.nanoseconds as _;
            attr.st_ctim.tv_sec = mounted_at.seconds as _;
            attr.st_ctim.tv_nsec = mounted_at.nanoseconds as _;
        }
    }

    fn hello_stat(ino: fuse_ino_t, mounted_at: MountTime) -> Option<Attr> {
        let mut attr: Attr = unsafe { std::mem::zeroed() };
        attr.st_ino = ino;
        match ino {
            1 => {
                attr.st_mode = libc::S_IFDIR as mode_t | 0o755;
                attr.st_nlink = 2;
            }
            2 => {
                attr.st_mode = libc::S_IFREG as mode_t | 0o444;
                attr.st_nlink = 1;
                attr.st_size = HELLO_STR.len() as off_t;
            }
            _ => return None,
        }
        set_times(&mut attr, mounted_at);
        Some(attr)
    }

    unsafe extern "C" fn hello_ll_lookup(req: fuse_req_t, parent: fuse_ino_t, name: *const c_char) {
        let name = unsafe { CStr::from_ptr(name) };
        if parent != FUSE_ROOT_ID as fuse_ino_t || name.to_bytes_with_nul() != HELLO_NAME {
            unsafe { fuse_reply_err(req, libc::ENOENT) };
            return;
        }

        let mut e: EntryParam = unsafe { std::mem::zeroed() };
        e.ino = 2;
        e.attr_timeout = 1.0;
        e.entry_timeout = 1.0;
        e.attr = hello_stat(2, *mount_time(req)).unwrap();
        unsafe { fuse_reply_entry(req, &e) };
    }

    unsafe extern "C" fn hello_ll_getattr(
        req: fuse_req_t,
        ino: fuse_ino_t,
        _fi: *mut fuse_file_info,
    ) {
        match hello_stat(ino, *mount_time(req)) {
            Some(attr) => {
                unsafe { fuse_reply_attr(req, &attr, 1.0) };
            }
            None => {
                unsafe { fuse_reply_err(req, libc::ENOENT) };
            }
        }
    }

    unsafe fn dirbuf_add(req: fuse_req_t, buf: &mut Vec<u8>, name: &CStr, ino: fuse_ino_t) {
        let old_size = buf.len();
        let entry_size =
            unsafe { fuse_add_direntry(req, ptr::null_mut(), 0, name.as_ptr(), ptr::null(), 0) };
        buf.resize(old_size + entry_size, 0);

        let attr = hello_stat(ino, *mount_time(req)).unwrap();
        unsafe {
            fuse_add_direntry(
                req,
                buf.as_mut_ptr().add(old_size) as *mut c_char,
                entry_size,
                name.as_ptr(),
                &attr,
                buf.len() as off_t,
            );
        }
    }

    unsafe fn reply_buf_limited(req: fuse_req_t, buf: &[u8], off: off_t, maxsize: usize) -> c_int {
        let off = off as usize;
        if off < buf.len() {
            let end = std::cmp::min(buf.len(), off + maxsize);
            unsafe { fuse_reply_buf(req, buf[off..end].as_ptr() as *const c_char, end - off) }
        } else {
            unsafe { fuse_reply_buf(req, ptr::null(), 0) }
        }
    }

    unsafe extern "C" fn hello_ll_readdir(
        req: fuse_req_t,
        ino: fuse_ino_t,
        size: usize,
        off: off_t,
        _fi: *mut fuse_file_info,
    ) {
        if ino != FUSE_ROOT_ID as fuse_ino_t {
            unsafe { fuse_reply_err(req, libc::ENOTDIR) };
            return;
        }

        let mut buf = Vec::new();
        unsafe {
            dirbuf_add(req, &mut buf, CStr::from_bytes_with_nul(b".\0").unwrap(), 1);
            dirbuf_add(
                req,
                &mut buf,
                CStr::from_bytes_with_nul(b"..\0").unwrap(),
                1,
            );
            dirbuf_add(
                req,
                &mut buf,
                CStr::from_bytes_with_nul(HELLO_NAME).unwrap(),
                2,
            );
            reply_buf_limited(req, &buf, off, size);
        }
    }

    unsafe extern "C" fn hello_ll_open(req: fuse_req_t, ino: fuse_ino_t, fi: *mut fuse_file_info) {
        if ino != 2 {
            unsafe { fuse_reply_err(req, libc::EISDIR) };
            return;
        }
        let flags = unsafe { (*fi).flags };
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            unsafe { fuse_reply_err(req, libc::EACCES) };
        } else {
            unsafe { fuse_reply_open(req, fi) };
        }
    }

    unsafe extern "C" fn hello_ll_read(
        req: fuse_req_t,
        ino: fuse_ino_t,
        size: usize,
        off: off_t,
        _fi: *mut fuse_file_info,
    ) {
        debug_assert_eq!(ino, 2);
        unsafe { reply_buf_limited(req, HELLO_STR, off, size) };
    }

    fn hello_ll_oper() -> fuse_lowlevel_ops {
        fuse_lowlevel_ops {
            lookup: Some(hello_ll_lookup),
            getattr: Some(hello_ll_getattr),
            readdir: Some(hello_ll_readdir),
            open: Some(hello_ll_open),
            read: Some(hello_ll_read),
            ..Default::default()
        }
    }

    unsafe fn run_session(args: &mut fuse_args, opts: &fuse_cmdline_opts) -> c_int {
        let ops = hello_ll_oper();
        let mut mounted_at = MountTime::now();

        // macOS's macFUSE ships a patched libfuse with an ABI-versioned
        // `fuse_session_new_versioned()`; vanilla Linux libfuse only has the
        // plain `fuse_session_new()`.
        #[cfg(target_os = "macos")]
        let se = unsafe {
            let mut version = libfuse_version {
                major: FUSE_MAJOR_VERSION as _,
                minor: FUSE_MINOR_VERSION as _,
                hotfix: FUSE_HOTFIX_VERSION as _,
                ..Default::default()
            };
            // Darwin extensions stay disabled: the reply functions above are bound to the
            // vanilla symbols and use the vanilla struct layouts.
            version.set_darwin_extensions_enabled(0);
            fuse_session_new_versioned(
                args,
                &ops,
                size_of::<fuse_lowlevel_ops>(),
                &mut version,
                &mut mounted_at as *mut MountTime as *mut c_void,
            )
        };
        #[cfg(not(target_os = "macos"))]
        let se = unsafe {
            fuse_session_new(
                args,
                &ops,
                size_of::<fuse_lowlevel_ops>(),
                &mut mounted_at as *mut MountTime as *mut c_void,
            )
        };
        if se.is_null() {
            return 1;
        }

        let mut ret: c_int = 1;
        if unsafe { fuse_set_signal_handlers(se) } == 0 {
            if unsafe { fuse_session_mount(se, opts.mountpoint) } == 0 {
                unsafe { fuse_daemonize(opts.foreground) };
                ret = unsafe { fuse_session_loop(se) };
                unsafe { fuse_session_unmount(se) };
            }
            unsafe { fuse_remove_signal_handlers(se) };
        }
        unsafe { fuse_session_destroy(se) };
        ret
    }

    pub fn run() -> i32 {
        let prog_args: Vec<CString> = std::env::args()
            .map(|a| CString::new(a).expect("argument contains a NUL byte"))
            .collect();
        let mut argv: Vec<*mut c_char> = prog_args
            .iter()
            .map(|a| a.as_ptr() as *mut c_char)
            .collect();
        let mut args = fuse_args {
            argc: argv.len() as c_int,
            argv: argv.as_mut_ptr(),
            allocated: 0,
        };

        let mut opts: fuse_cmdline_opts = unsafe { std::mem::zeroed() };
        let prog_name = prog_args
            .first()
            .map(|a| a.to_string_lossy().into_owned())
            .unwrap_or_else(|| "hello_ll".to_string());

        // Linux's versioned-symbol libfuse build exposes the unsuffixed
        // `fuse_parse_cmdline` regardless of the requested API version;
        // macOS's macFUSE needs the FUSE_USE_VERSION-suffixed name.
        #[cfg(feature = "fuse_312")]
        let parse_result = unsafe { parse_cmdline_312(&mut args, &mut opts) };
        #[cfg(all(not(feature = "fuse_312"), target_os = "macos"))]
        let parse_result = unsafe { fuse_parse_cmdline_30(&mut args, &mut opts) };
        #[cfg(all(not(feature = "fuse_312"), not(target_os = "macos")))]
        let parse_result = unsafe { fuse_parse_cmdline(&mut args, &mut opts) };
        if parse_result != 0 {
            return 1;
        }

        let ret = if opts.show_help != 0 {
            println!("usage: {} [options] <mountpoint>\n", prog_name);
            unsafe { fuse_cmdline_help() };
            unsafe { fuse_lowlevel_help() };
            0
        } else if opts.show_version != 0 {
            let version = unsafe { CStr::from_ptr(fuse_pkgversion()) };
            println!("FUSE library version {}", version.to_string_lossy());
            unsafe { fuse_lowlevel_version() };
            0
        } else if opts.mountpoint.is_null() {
            eprintln!("usage: {} [options] <mountpoint>", prog_name);
            eprintln!("       {} --help", prog_name);
            1
        } else {
            unsafe { run_session(&mut args, &opts) }
        };

        if !opts.mountpoint.is_null() {
            unsafe { libc::free(opts.mountpoint as *mut c_void) };
        }
        unsafe { fuse_opt_free_args(&mut args) };

        if ret != 0 {
            1
        } else {
            0
        }
    }
}

fn main() {
    std::process::exit(imp::run());
}
