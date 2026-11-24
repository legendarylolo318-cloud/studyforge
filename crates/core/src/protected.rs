//! Hard-coded safety rules: processes that must never be terminated.

/// Lowercase names (without `.exe`). Matching is case-insensitive.
pub const PROTECTED_NAMES: &[&str] = &[
    // init / service managers
    "init",
    "systemd",
    "systemd-journald",
    "systemd-logind",
    "dbus-daemon",
    "dbus-broker",
    "dbus-broker-launch",
    "login",
    "sshd",
    "udevd",
    "systemd-udevd",
    "polkitd",
    "networkmanager",
    // display servers / compositors / shells of the desktop
    "xorg",
    "x",
    "xwayland",
    "xwayland-satellite",
    "wayland",
    "kwin",
    "kwin_x11",
    "kwin_wayland",
    "gnome-shell",
    "mutter",
    "hyprland",
    "sway",
    "river",
    "labwc",
    "wayfire",
    "weston",
    "plasmashell",
    "gnome-session",
    "gnome-session-binary",
    "sddm",
    "gdm",
    "gdm3",
    "lightdm",
    "xfwm4",
    "openbox",
    "i3",
    "awesome",
    "mutter-x11-frames",
    "pipewire",
    "wireplumber",
    "pulseaudio",
    // terminals & shells
    "bash",
    "zsh",
    "fish",
    "sh",
    "dash",
    "tcsh",
    "nu",
    "pwsh",
    "powershell",
    "cmd",
    "conhost",
    "windowsterminal",
    "wt",
    "konsole",
    "gnome-terminal",
    "gnome-terminal-server",
    "alacritty",
    "kitty",
    "wezterm",
    "wezterm-gui",
    "foot",
    "xterm",
    "urxvt",
    "terminator",
    "tilix",
    "xfce4-terminal",
    "tmux",
    "screen",
    "ghostty",
    // windows system
    "explorer",
    "dwm",
    "winlogon",
    "csrss",
    "lsass",
    "services",
    "svchost",
    "smss",
    "wininit",
    "system",
    "registry",
    "fontdrvhost",
    "taskmgr",
    "sihost",
    "ctfmon",
    "runtimebroker",
    "searchhost",
    "startmenuexperiencehost",
    "shellexperiencehost",
    "audiodg",
    "spoolsv",
    "dllhost",
    "taskhostw",
    "securityhealthservice",
    // privilege / session plumbing
    "sudo",
    "su",
    "doas",
    "pkexec",
    "ssh",
    "ssh-agent",
    "gpg-agent",
    "dbus-launch",
    // ourselves
    "studyd",
    "studyforge",
];

/// Names that are Steam client components; only killable when the user
/// explicitly enabled "Block Steam client too".
pub const STEAM_CLIENT_NAMES: &[&str] = &["steam", "steamwebhelper", "steamservice", "steamerrorreporter", "steam.sh"];

pub fn normalize(name: &str) -> String {
    let n = name.trim().to_lowercase();
    n.strip_suffix(".exe").map(str::to_string).unwrap_or(n)
}

/// Prefixes of session-critical helpers (systemd-*, portals, polkit agents, ...).
pub const PROTECTED_PREFIXES: &[&str] = &[
    "systemd",
    "xdg-desktop-portal",
    "xdg-document-portal",
    "xdg-permission-store",
    "polkit",
    "gnome-keyring",
    "kwalletd",
    "ksmserver",
    "kded",
    "startplasma",
    "plasma_session",
    "gsd-",
    "at-spi",
    "xwayland",
    "hyprland",
];

pub fn is_protected_name(name: &str) -> bool {
    let n = normalize(name);
    PROTECTED_NAMES.contains(&n.as_str()) || PROTECTED_PREFIXES.iter().any(|p| n.starts_with(p))
}

pub fn is_steam_client(name: &str) -> bool {
    let n = normalize(name);
    STEAM_CLIENT_NAMES.contains(&n.as_str())
}

/// Everything the killer needs to know about a candidate.
pub struct Candidate<'a> {
    pub pid: u32,
    pub name: &'a str,
    /// Owner identity (uid string on Unix, SID on Windows). `None` = unknown.
    pub owner: Option<&'a str>,
    pub self_pid: u32,
}

/// True if the process may be killed given the allowed owner identities.
/// Unknown owner => not killable. Root/SYSTEM owners => never killable.
pub fn may_kill(c: &Candidate, allowed_owners: &[String]) -> bool {
    if c.pid <= 1 || c.pid == c.self_pid {
        return false;
    }
    if is_protected_name(c.name) {
        return false;
    }
    let Some(owner) = c.owner else { return false };
    if is_system_owner(owner) {
        return false;
    }
    allowed_owners.iter().any(|a| a == owner)
}

pub fn is_system_owner(owner: &str) -> bool {
    matches!(owner, "0" | "S-1-5-18" | "S-1-5-19" | "S-1-5-20")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c<'a>(pid: u32, name: &'a str, owner: Option<&'a str>) -> Candidate<'a> {
        Candidate { pid, name, owner, self_pid: 999 }
    }
    #[test]
    fn protected_list() {
        for n in ["Xorg", "KWin_Wayland", "EXPLORER.EXE", "systemd", "bash", "studyd", "Hyprland", "lsass.exe"] {
            assert!(is_protected_name(n), "{n}");
        }
        assert!(!is_protected_name("discord"));
        for n in [
            "systemd-resolved",
            "xdg-desktop-portal-hyprland",
            "polkit-kde-authentication-agent-1",
            "gsd-power",
            "sudo",
        ] {
            assert!(is_protected_name(n), "{n}");
        }
    }
    #[test]
    fn owner_rules() {
        let allowed = vec!["1000".to_string()];
        assert!(may_kill(&c(500, "discord", Some("1000")), &allowed));
        assert!(!may_kill(&c(1, "discord", Some("1000")), &allowed));
        assert!(!may_kill(&c(999, "discord", Some("1000")), &allowed));
        assert!(!may_kill(&c(500, "discord", Some("0")), &["0".into()]));
        assert!(!may_kill(&c(500, "discord", Some("1001")), &allowed));
        assert!(!may_kill(&c(500, "discord", None), &allowed));
        assert!(!may_kill(&c(500, "bash", Some("1000")), &allowed));
        assert!(!may_kill(&c(500, "x", Some("S-1-5-18")), &["S-1-5-18".into()]));
    }
    #[test]
    fn steam_client_names() {
        assert!(is_steam_client("steamwebhelper.exe"));
        assert!(!is_steam_client("hl2"));
    }
}
