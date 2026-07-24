//! Convenience mount configuration and a blocking mount+run helper shared
//! by FUSE daemons: build a [`MountConfig`], turn it into libfuse
//! [`MountOption`]s, and hand a [`PathFilesystem`] off to [`mount_blocking`].
//!
//! Per-OS mount-option rendering (e.g. `volname=` being macOS-only) lives in
//! [`MountOption`] itself, so this module and its callers stay
//! `#[cfg(target_os)]`-free.

use crate::{
    Error, MountOption, PathFilesystem, PathNodeFs, Session, SessionConfig, ThreadPoolConfig,
    ThreadingMode,
};
use std::path::Path;

/// Mount settings for a `PathFilesystem`-based daemon.
pub struct MountConfig {
    pub fs_name: String,
    pub allow_other: bool,
    pub allow_root: bool,
    pub default_permissions: bool,
    pub read_only: bool,
    pub nonempty: bool,
    /// macOS Finder volume name.
    pub volname: Option<String>,
    /// Additional raw mount option words, without any `-o` prefix.
    pub extra_options: Vec<String>,
}

impl MountConfig {
    /// A config with sensible defaults for `fs_name` (`default_permissions`
    /// on, everything else off).
    pub fn new(fs_name: impl Into<String>) -> Self {
        Self {
            fs_name: fs_name.into(),
            allow_other: false,
            allow_root: false,
            default_permissions: true,
            read_only: false,
            nonempty: false,
            volname: None,
            extra_options: Vec::new(),
        }
    }

    /// Applies a batch of comma-separated `-o` option words (e.g. already
    /// split from a `-o allow_other,ro` argument) to this config.
    /// Recognized words set the corresponding field; anything else is
    /// forwarded verbatim as an extra mount option.
    pub fn parse_option_words<'a>(&mut self, words: impl IntoIterator<Item = &'a str>) {
        for word in words {
            match word {
                "allow_other" => self.allow_other = true,
                "allow_root" => self.allow_root = true,
                "nonempty" => self.nonempty = true,
                "ro" => self.read_only = true,
                "default_permissions" => self.default_permissions = true,
                other => self.extra_options.push(other.to_string()),
            }
        }
    }
}

/// Renders a [`MountConfig`] to the [`MountOption`] list `Session` expects.
pub fn build_mount_options(cfg: &MountConfig) -> Vec<MountOption> {
    let mut options = vec![MountOption::FsName(cfg.fs_name.clone())];
    // `uid=`/`gid=` are high-level libfuse mount options (handled by
    // `fuse_main`'s uid/gid translation layer); the low-level session API
    // used here doesn't recognize them and libfuse rejects the mount with
    // "unknown option(s)". They're also unnecessary: `getattr` already
    // reports the backing filesystem's real ownership.
    if cfg.allow_other {
        options.push(MountOption::AllowOther);
    }
    if cfg.allow_root {
        options.push(MountOption::AllowRoot);
    }
    if cfg.default_permissions {
        options.push(MountOption::DefaultPermissions);
    }
    if cfg.read_only {
        options.push(MountOption::ReadOnly);
    }
    if cfg.nonempty {
        options.push(MountOption::NonEmpty);
    }
    if let Some(volname) = &cfg.volname {
        options.push(MountOption::VolName(volname.clone()));
    }
    options.extend(cfg.extra_options.iter().cloned().map(MountOption::Custom));
    options
}

/// Mounts `fs` synchronously and runs libfuse's single- or multi-threaded
/// request loop until unmounted or signalled.
///
/// Call this after any daemonization: forking after libfuse has initialized
/// process state is unsafe.
pub fn mount_blocking<FS>(
    fs: FS,
    mount_point: &Path,
    cfg: &MountConfig,
    single_thread: bool,
) -> Result<(), Error>
where
    FS: PathFilesystem + 'static,
{
    let adapter = PathNodeFs::new(fs);
    let session_config = SessionConfig {
        threading: if single_thread {
            ThreadingMode::SingleThreaded
        } else {
            ThreadingMode::MultiThreaded(ThreadPoolConfig::default())
        },
    };
    let options = build_mount_options(cfg);
    let mut session = Session::new_with_config(adapter, &options, session_config)?;
    session.mount(mount_point)?;
    session.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_mount_policy_and_passthrough_options() {
        let mut config = MountConfig {
            fs_name: "encfs-test".to_string(),
            allow_other: true,
            allow_root: true,
            default_permissions: true,
            read_only: true,
            nonempty: true,
            volname: Some("Encrypted Files".to_string()),
            extra_options: Vec::new(),
        };
        config.parse_option_words(["noatime"]);

        let options = build_mount_options(&config);
        assert!(options.contains(&MountOption::FsName("encfs-test".to_string())));
        assert!(options.contains(&MountOption::AllowOther));
        assert!(options.contains(&MountOption::AllowRoot));
        assert!(options.contains(&MountOption::DefaultPermissions));
        assert!(options.contains(&MountOption::ReadOnly));
        assert!(options.contains(&MountOption::NonEmpty));
        assert!(options.contains(&MountOption::VolName("Encrypted Files".to_string())));
        assert!(options.contains(&MountOption::Custom("noatime".to_string())));
        assert!(!options.iter().any(|option| matches!(
            option,
            MountOption::Custom(value) if value.starts_with("uid=") || value.starts_with("gid=")
        )));
    }

    #[test]
    fn parse_option_words_recognizes_known_words_and_passes_through_others() {
        let mut config = MountConfig::new("fs");
        config.parse_option_words(["allow_other", "allow_root", "nonempty", "ro", "noatime"]);
        assert!(config.allow_other);
        assert!(config.allow_root);
        assert!(config.nonempty);
        assert!(config.read_only);
        assert_eq!(config.extra_options, vec!["noatime".to_string()]);
    }
}
