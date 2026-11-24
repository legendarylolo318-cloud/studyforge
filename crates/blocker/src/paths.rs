//! Every filesystem location blocker code touches is injected through [`Paths`],
//! so tests run entirely inside a temp directory.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub hosts: PathBuf,
    /// Daemon state dir: hosts backup, policy bookkeeping.
    pub state_dir: PathBuf,
    /// Full paths of Firefox `policies.json` files to manage.
    pub firefox_policy_files: Vec<PathBuf>,
    /// Chromium-family managed policy directories (Linux).
    pub chromium_policy_dirs: Vec<PathBuf>,
    /// Registry roots for Chromium-family policies (Windows), e.g. `SOFTWARE\Policies\Google\Chrome`.
    pub registry_roots: Vec<String>,
}

impl Paths {
    pub fn hosts_backup(&self) -> PathBuf {
        self.state_dir.join("hosts.backup")
    }
    pub fn policy_state(&self) -> PathBuf {
        self.state_dir.join("policy_state.json")
    }

    /// The real system locations.
    pub fn system() -> Paths {
        #[cfg(windows)]
        {
            let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            let mut firefox = vec![];
            for var in ["ProgramFiles", "ProgramFiles(x86)"] {
                if let Some(pf) = std::env::var_os(var) {
                    let base = PathBuf::from(pf).join("Mozilla Firefox");
                    if base.exists() {
                        firefox.push(base.join("distribution").join("policies.json"));
                    }
                }
            }
            Paths {
                hosts: root.join(r"System32\drivers\etc\hosts"),
                state_dir: studyforge_core::paths::daemon_state_dir(),
                firefox_policy_files: firefox,
                chromium_policy_dirs: vec![],
                registry_roots: vec![
                    r"SOFTWARE\Policies\Google\Chrome".into(),
                    r"SOFTWARE\Policies\Microsoft\Edge".into(),
                    r"SOFTWARE\Policies\BraveSoftware\Brave".into(),
                ],
            }
        }
        #[cfg(not(windows))]
        {
            let mut firefox = vec![PathBuf::from("/etc/firefox/policies/policies.json")];
            for base in ["/usr/lib/firefox", "/usr/lib64/firefox", "/usr/lib/firefox-esr", "/opt/firefox"] {
                if Path::new(base).is_dir() {
                    firefox.push(Path::new(base).join("distribution").join("policies.json"));
                }
            }
            Paths {
                hosts: PathBuf::from("/etc/hosts"),
                state_dir: studyforge_core::paths::daemon_state_dir(),
                firefox_policy_files: firefox,
                chromium_policy_dirs: [
                    "/etc/opt/chrome/policies/managed",
                    "/etc/chromium/policies/managed",
                    "/etc/brave/policies/managed",
                    "/etc/opt/edge/policies/managed",
                ]
                .iter()
                .map(PathBuf::from)
                .collect(),
                registry_roots: vec![],
            }
        }
    }

    /// Everything under `root` (for tests and `--dry-run` experiments).
    pub fn under(root: &Path) -> Paths {
        Paths {
            hosts: root.join("etc/hosts"),
            state_dir: root.join("state"),
            firefox_policy_files: vec![
                root.join("etc/firefox/policies/policies.json"),
                root.join("usr/lib/firefox/distribution/policies.json"),
            ],
            chromium_policy_dirs: vec![root.join("etc/chromium/policies/managed"), root.join("etc/opt/chrome/policies/managed")],
            registry_roots: vec![],
        }
    }
}
