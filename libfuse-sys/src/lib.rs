#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::cognitive_complexity)]
#![allow(clippy::missing_safety_doc)]
// Generated bindgen pointer arithmetic and bitfield constructors intentionally
// use these shapes; suppress them at the raw-binding crate boundary.
#![allow(clippy::ptr_offset_with_cast)]
#![allow(clippy::too_many_arguments)]

#[cfg(feature = "fuse_highlevel")]
use libc::*;

#[cfg(feature = "fuse_highlevel")]
pub mod fuse {
    use super::*;
    include!(concat!(env!("OUT_DIR"), "/fuse.rs"));

    /// Main function of FUSE
    ///
    /// Implemented as a macro in the original fuse header.
    ///
    /// macOS's macFUSE ships a patched libfuse with an ABI-versioned
    /// `fuse_main_real_versioned()` entry point (and the `libfuse_version`
    /// struct it takes); vanilla Linux libfuse only has the plain
    /// `fuse_main_real()`.
    pub unsafe fn fuse_main(
        argc: c_int,
        argv: *mut *mut c_char,
        op: *const fuse_operations,
        user_data: *mut c_void,
    ) -> c_int {
        #[cfg(target_os = "macos")]
        {
            let mut version = libfuse_version {
                major: FUSE_MAJOR_VERSION as _,
                minor: FUSE_MINOR_VERSION as _,
                hotfix: FUSE_HOTFIX_VERSION as _,
                ..Default::default()
            };
            version.set_darwin_extensions_enabled(FUSE_DARWIN_ENABLE_EXTENSIONS as _);
            fuse_main_real_versioned(
                argc,
                argv,
                op,
                std::mem::size_of_val(&*op),
                &mut version,
                user_data,
            )
        }
        #[cfg(not(target_os = "macos"))]
        {
            fuse_main_real(argc, argv, op, std::mem::size_of_val(&*op), user_data)
        }
    }
}

#[cfg(feature = "fuse_lowlevel")]
#[allow(clashing_extern_declarations)]
pub mod fuse_lowlevel {
    include!(concat!(env!("OUT_DIR"), "/fuse_lowlevel.rs"));

    /// Stable names for the libfuse 3.12 multi-thread loop API.
    ///
    /// `fuse_session_loop_mt` resolves to the correct platform-specific
    /// symbol while bindgen processes the libfuse headers: macOS uses the
    /// `_312` alias, while Linux's versioned-symbol build exposes the
    /// unsuffixed name.
    #[cfg(feature = "fuse_312")]
    pub unsafe fn session_loop_mt_312(
        session: *mut fuse_session,
        config: *mut fuse_loop_config,
    ) -> ::std::os::raw::c_int {
        #[cfg(target_os = "macos")]
        {
            unsafe { fuse_session_loop_mt_312(session, config) }
        }
        #[cfg(not(target_os = "macos"))]
        {
            unsafe { fuse_session_loop_mt(session, config) }
        }
    }

    /// Stable name for the libfuse 3.12 command-line parser API.
    ///
    /// As with the multi-thread loop, libfuse's macOS headers use a versioned
    /// alias while Linux's versioned-symbol build exposes the unsuffixed name.
    #[cfg(feature = "fuse_312")]
    pub unsafe fn parse_cmdline_312(
        args: *mut fuse_args,
        opts: *mut fuse_cmdline_opts,
    ) -> ::std::os::raw::c_int {
        #[cfg(target_os = "macos")]
        {
            unsafe { fuse_parse_cmdline_312(args, opts) }
        }
        #[cfg(not(target_os = "macos"))]
        {
            unsafe { fuse_parse_cmdline(args, opts) }
        }
    }

    #[cfg(feature = "fuse_312")]
    pub unsafe fn loop_cfg_create_312() -> *mut fuse_loop_config {
        unsafe { fuse_loop_cfg_create() }
    }

    #[cfg(feature = "fuse_312")]
    pub unsafe fn loop_cfg_destroy_312(config: *mut fuse_loop_config) {
        unsafe { fuse_loop_cfg_destroy(config) }
    }

    #[cfg(feature = "fuse_312")]
    pub unsafe fn loop_cfg_set_idle_threads_312(config: *mut fuse_loop_config, value: u32) {
        unsafe { fuse_loop_cfg_set_idle_threads(config, value) }
    }

    #[cfg(feature = "fuse_312")]
    pub unsafe fn loop_cfg_set_max_threads_312(config: *mut fuse_loop_config, value: u32) {
        unsafe { fuse_loop_cfg_set_max_threads(config, value) }
    }

    #[cfg(feature = "fuse_312")]
    pub unsafe fn loop_cfg_set_clone_fd_312(config: *mut fuse_loop_config, value: u32) {
        unsafe { fuse_loop_cfg_set_clone_fd(config, value) }
    }
}

#[cfg(feature = "cuse_lowlevel")]
pub mod cuse_lowlevel {
    include!(concat!(env!("OUT_DIR"), "/cuse_lowlevel.rs"));
}
