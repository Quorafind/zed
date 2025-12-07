//! Centralized branding constants for the application.
//!
//! This crate contains all brand-related names and identifiers to make it easy
//! to maintain a fork with different branding. When merging upstream changes,
//! conflicts will be isolated to this single crate.

#![deny(missing_docs)]

/// The base name of the application (e.g., "Zed-C").
pub const APP_NAME: &str = "Zed-C";

/// The lowercase base name (e.g., "zed-c").
pub const APP_NAME_LOWERCASE: &str = "zed-c";

/// The display name with space (e.g., "Zed C").
pub const APP_DISPLAY_NAME: &str = "Zed C";

/// The CLI binary name (e.g., "zed-c").
pub const CLI_NAME: &str = "zed-c";

/// The editor binary name (e.g., "zed-c-editor").
pub const EDITOR_BINARY_NAME: &str = "zed-c-editor";

/// The app ID prefix (e.g., "dev.zed.Zed-C").
pub const APP_ID_PREFIX: &str = "dev.zed.Zed-C";

/// The socket file prefix (e.g., "zed-c-").
pub const SOCKET_PREFIX: &str = "zed-c-";

/// Display names for each release channel.
pub mod display_names {
    use super::APP_DISPLAY_NAME;

    /// Display name for the Dev channel.
    pub const DEV: &str = concat!("Zed C", " Dev");
    /// Display name for the Nightly channel.
    pub const NIGHTLY: &str = concat!("Zed C", " Nightly");
    /// Display name for the Preview channel.
    pub const PREVIEW: &str = concat!("Zed C", " Preview");
    /// Display name for the Stable channel.
    pub const STABLE: &str = "Zed C";
}

/// App identifiers for each release channel.
pub mod app_ids {
    /// App ID for the Dev channel.
    pub const DEV: &str = "dev.zed.Zed-C-Dev";
    /// App ID for the Nightly channel.
    pub const NIGHTLY: &str = "dev.zed.Zed-C-Nightly";
    /// App ID for the Preview channel.
    pub const PREVIEW: &str = "dev.zed.Zed-C-Preview";
    /// App ID for the Stable channel.
    pub const STABLE: &str = "dev.zed.Zed-C";
}

/// Windows app identifiers.
#[cfg(target_os = "windows")]
pub mod windows_app_ids {
    /// Windows app ID for the Dev channel.
    pub const DEV: &str = "Zed-C-Dev";
    /// Windows app ID for the Nightly channel.
    pub const NIGHTLY: &str = "Zed-C-Nightly";
    /// Windows app ID for the Preview channel.
    pub const PREVIEW: &str = "Zed-C-Preview";
    /// Windows app ID for the Stable channel.
    pub const STABLE: &str = "Zed-C-Stable";
}

/// Platform-specific binary paths.
pub mod binary_paths {
    /// Linux binary locations relative to CLI.
    pub mod linux {
        /// Possible locations for the editor binary on Linux.
        pub const EDITOR_LOCATIONS: [&str; 3] = [
            "../libexec/zed-c-editor",
            "../lib/zed-c/zed-c-editor",
            "./zed-c",
        ];
    }

    /// Windows binary locations relative to CLI.
    pub mod windows {
        /// Possible locations for the editor binary on Windows.
        pub const EDITOR_LOCATIONS: [&str; 3] = [
            "../Zed-C.exe",
            "../lib/zed-c/zed-c-editor.exe",
            "./zed-c.exe",
        ];
    }

    /// macOS CLI installation path.
    #[cfg(target_os = "macos")]
    pub const MACOS_CLI_PATH: &str = "/usr/local/bin/zed-c";
}

/// Directory names for configuration and data storage.
pub mod directories {
    /// Directory name for macOS Application Support.
    pub const MACOS_APP_SUPPORT: &str = "Zed-C";
    /// Directory name for macOS logs.
    pub const MACOS_LOGS: &str = "Zed-C";
    /// Directory name for macOS cache.
    pub const MACOS_CACHE: &str = "Zed-C";

    /// Directory name for Windows config (APPDATA).
    pub const WINDOWS_CONFIG: &str = "Zed-C";
    /// Directory name for Windows data (LOCALAPPDATA).
    pub const WINDOWS_DATA: &str = "Zed-C";
    /// Directory name for Windows cache.
    pub const WINDOWS_CACHE: &str = "Zed-C";

    /// Directory name for Linux/FreeBSD config.
    pub const LINUX_CONFIG: &str = "zed-c";
    /// Directory name for Linux/FreeBSD data.
    pub const LINUX_DATA: &str = "zed-c";
    /// Directory name for Linux/FreeBSD cache.
    pub const LINUX_CACHE: &str = "zed-c";
}
