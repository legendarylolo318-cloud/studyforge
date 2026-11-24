//! Steam game detection helpers.
//!
//! * library folders come from `libraryfolders.vdf` (every Steam root);
//! * Linux roots: `~/.steam/steam`, `~/.local/share/Steam`, and the Flatpak
//!   locations under `~/.var/app/com.valvesoftware.Steam/`;
//! * Windows: `SteamPath` from the user's registry hive (see `windows_roots`).

use std::path::{Path, PathBuf};

/// Extract library paths from a `libraryfolders.vdf` (new `"path"` format and
/// the old `"1" "/path"` format). Backslash escapes (`D:\\Games`) are decoded.
pub fn parse_libraryfolders(vdf: &str) -> Vec<PathBuf> {
    // tokenise quoted strings
    let mut tokens: Vec<String> = Vec::new();
    let mut chars = vdf.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut s = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next() {
                        Some('\\') => s.push('\\'),
                        Some('"') => s.push('"'),
                        Some('n') => s.push('\n'),
                        Some(o) => {
                            s.push('\\');
                            s.push(o)
                        }
                        None => {}
                    },
                    '"' => break,
                    _ => s.push(c),
                }
            }
            tokens.push(s);
        }
    }
    let looks_like_path = |s: &str| s.contains('/') || s.contains('\\') || s.chars().nth(1) == Some(':');
    let mut out: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i + 1 < tokens.len() {
        let (k, v) = (&tokens[i], &tokens[i + 1]);
        let is_path_key = k.eq_ignore_ascii_case("path") || (k.chars().all(|c| c.is_ascii_digit()) && !k.is_empty());
        if is_path_key && looks_like_path(v) {
            let p = PathBuf::from(v);
            if !out.contains(&p) {
                out.push(p);
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Candidate Steam installation roots for the given home directories (Linux).
pub fn linux_roots(homes: &[PathBuf]) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for h in homes {
        for rel in [
            ".steam/steam",
            ".steam/root",
            ".local/share/Steam",
            ".var/app/com.valvesoftware.Steam/.local/share/Steam",
            ".var/app/com.valvesoftware.Steam/data/Steam",
        ] {
            let p = h.join(rel);
            if p.is_dir() {
                let canon = std::fs::canonicalize(&p).unwrap_or(p);
                if !v.contains(&canon) {
                    v.push(canon);
                }
            }
        }
    }
    v
}

/// All `steamapps/common` directories reachable from the given Steam roots.
pub fn common_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut libs: Vec<PathBuf> = Vec::new();
    for r in roots {
        if !libs.contains(r) {
            libs.push(r.clone());
        }
        for vdf in [r.join("steamapps").join("libraryfolders.vdf"), r.join("config").join("libraryfolders.vdf")] {
            if let Ok(text) = std::fs::read_to_string(&vdf) {
                for p in parse_libraryfolders(&text) {
                    if !libs.contains(&p) {
                        libs.push(p);
                    }
                }
            }
        }
    }
    let mut out: Vec<PathBuf> = libs.iter().map(|l| l.join("steamapps").join("common")).collect();
    out.sort();
    out.dedup();
    out
}

/// Is `path` inside one of the `steamapps/common` directories?
/// Case-insensitive on Windows; separators normalised.
pub fn is_under_common(path: &Path, commons: &[PathBuf]) -> bool {
    if cfg!(windows) {
        let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
        let p = norm(path);
        commons.iter().any(|c| {
            let c = norm(c);
            let c = c.trim_end_matches('\\');
            p == c || p.starts_with(&format!("{c}\\"))
        })
    } else {
        commons.iter().any(|c| path.starts_with(c))
    }
}

/// `SteamAppId=<nonzero>` or `SteamGameId=...` in an environment block.
pub fn env_says_steam_game(env: &[String]) -> bool {
    env.iter().any(|e| {
        if let Some(v) = e.strip_prefix("SteamAppId=") {
            let v = v.trim();
            !v.is_empty() && v != "0"
        } else if let Some(v) = e.strip_prefix("SteamGameId=") {
            let v = v.trim();
            !v.is_empty() && v != "0"
        } else {
            false
        }
    })
}

/// Home directories of the given uids, from `/etc/passwd`-formatted text.
pub fn homes_for_uids(passwd: &str, uids: &[String]) -> Vec<PathBuf> {
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 6 && uids.iter().any(|u| u == f[2])).then(|| PathBuf::from(f[5]))
        })
        .collect()
}

#[cfg(windows)]
pub mod windows_roots {
    use std::path::PathBuf;
    use winreg::enums::*;
    use winreg::RegKey;

    /// Steam roots for the given user SIDs. The service runs as SYSTEM, so
    /// `HKCU` would be the wrong hive: read `HKEY_USERS\<sid>` instead.
    pub fn roots(sids: &[String]) -> Vec<PathBuf> {
        let mut v = Vec::new();
        for sid in sids {
            if let Ok(k) = RegKey::predef(HKEY_USERS).open_subkey(format!(r"{sid}\Software\Valve\Steam")) {
                if let Ok(p) = k.get_value::<String, _>("SteamPath") {
                    v.push(PathBuf::from(p.replace('/', "\\")));
                }
            }
        }
        if let Ok(k) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\WOW6432Node\Valve\Steam") {
            if let Ok(p) = k.get_value::<String, _>("InstallPath") {
                v.push(PathBuf::from(p));
            }
        }
        v.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        v.retain(|p| p.exists());
        v.sort();
        v.dedup();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW_FORMAT: &str = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"label"		""
		"apps"
		{
			"228980"		"12345"
		}
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
	}
}"#;

    #[test]
    fn parses_new_format() {
        let p = parse_libraryfolders(NEW_FORMAT);
        assert_eq!(p, vec![PathBuf::from("/home/u/.local/share/Steam"), PathBuf::from("/mnt/games/SteamLibrary")]);
    }

    #[test]
    fn parses_old_format_and_windows_escapes() {
        let old = "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\"1700000000\"\n\t\"1\"\t\"D:\\\\SteamLibrary\"\n\t\"2\"\t\"/mnt/b\"\n}";
        let p = parse_libraryfolders(old);
        assert_eq!(p, vec![PathBuf::from(r"D:\SteamLibrary"), PathBuf::from("/mnt/b")]);
    }

    #[test]
    fn garbage_is_ignored() {
        assert!(parse_libraryfolders("").is_empty());
        assert!(parse_libraryfolders("\"unterminated").is_empty());
        assert!(parse_libraryfolders("\"path\" \"notapath\"").is_empty());
    }

    #[test]
    fn env_detection() {
        let e = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(env_says_steam_game(&e(&["PATH=/bin", "SteamAppId=730"])));
        assert!(env_says_steam_game(&e(&["SteamGameId=12345"])));
        assert!(!env_says_steam_game(&e(&["SteamAppId=0"])));
        assert!(!env_says_steam_game(&e(&["SteamAppId="])));
        assert!(!env_says_steam_game(&e(&["XSteamAppId=730", "PATH=x"])));
    }

    #[cfg(not(windows))]
    #[test]
    fn under_common_unix() {
        let commons = vec![PathBuf::from("/mnt/g/steamapps/common")];
        assert!(is_under_common(Path::new("/mnt/g/steamapps/common/Game/game.x86_64"), &commons));
        assert!(!is_under_common(Path::new("/mnt/g/steamapps/common2/x"), &commons));
        assert!(!is_under_common(Path::new("/mnt/g/steamapps/workshop/x"), &commons));
    }

    #[test]
    fn discovers_libraries_from_disk_and_proton() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home/u");
        let root = home.join(".local/share/Steam");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        let extra = d.path().join("lib2");
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                root.display().to_string().replace('\\', "\\\\"),
                extra.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        // flatpak root as well
        let fp = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");
        std::fs::create_dir_all(&fp).unwrap();
        let roots = linux_roots(std::slice::from_ref(&home));
        assert_eq!(roots.len(), 2);
        let commons = common_dirs(&roots);
        assert!(commons.contains(&root.join("steamapps/common")));
        assert!(commons.contains(&extra.join("steamapps/common")));
        assert!(commons.contains(&fp.join("steamapps/common")));
        // proton binaries live under steamapps/common too
        let proton = root.join("steamapps/common/Proton - Experimental/files/bin/wine64-preloader");
        assert!(is_under_common(&proton, &commons));
    }

    #[test]
    fn passwd_homes() {
        let pw = "root:x:0:0::/root:/bin/bash\nstudent:x:1000:1000:L:/home/student:/bin/zsh\n";
        assert_eq!(homes_for_uids(pw, &["1000".into()]), vec![PathBuf::from("/home/student")]);
        assert!(homes_for_uids(pw, &["5".into()]).is_empty());
    }
}
