//! Locations of data / config / log directories (via the `directories` crate).

use directories::ProjectDirs;
use std::path::PathBuf;

pub const APP_QUALIFIER: &str = "";
pub const APP_ORG: &str = "";
pub const APP_NAME: &str = "StudyForge";

/// Default per-user data directory.
pub fn default_data_dir() -> PathBuf {
    ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".studyforge"))
}

/// Per-user config directory (holds the small bootstrap file that records a
/// custom data directory).
pub fn config_dir() -> PathBuf {
    ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".studyforge-config"))
}

fn bootstrap_file() -> PathBuf {
    config_dir().join("data_dir.txt")
}

/// The data directory in use: a user-chosen one if set, else the default.
pub fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("STUDYFORGE_DATA_DIR").filter(|p| !p.is_empty()) {
        return PathBuf::from(p); // portable mode / tests
    }
    if let Ok(s) = std::fs::read_to_string(bootstrap_file()) {
        let s = s.trim();
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    default_data_dir()
}

/// Persist a custom data directory (empty path resets to default).
pub fn set_custom_data_dir(p: Option<&std::path::Path>) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir())?;
    match p {
        Some(p) => std::fs::write(bootstrap_file(), p.to_string_lossy().as_bytes()),
        None => {
            let _ = std::fs::remove_file(bootstrap_file());
            Ok(())
        }
    }
}

pub fn db_path(data: &std::path::Path) -> PathBuf {
    data.join("studyforge.sqlite3")
}
pub fn sounds_dir(data: &std::path::Path) -> PathBuf {
    data.join("sounds")
}
pub fn backups_dir(data: &std::path::Path) -> PathBuf {
    data.join("backups")
}
pub fn logs_dir() -> PathBuf {
    // Logs stay local even when the data dir is synced.
    ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Daemon state directory (root-owned).
pub fn daemon_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base =
            std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("StudyForge")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/var/lib/studyd")
    }
}

/// Daemon config directory (allowed_users lives here on Linux).
pub fn daemon_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        daemon_state_dir()
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/studyd")
    }
}

/// IPC endpoint name / path.
pub fn ipc_endpoint() -> String {
    #[cfg(windows)]
    {
        r"\\.\pipe\studyd".to_string()
    }
    #[cfg(not(windows))]
    {
        "/run/studyd/studyd.sock".to_string()
    }
}
