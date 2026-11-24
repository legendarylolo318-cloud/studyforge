//! Layer 1: hosts-file blocking.
//!
//! Entries live only between the two marker lines. Everything outside them is
//! preserved byte-for-byte (including line endings). Writes are atomic
//! (temp file in the same directory, fsync, rename) with fallbacks for locked
//! or bind-mounted files.

use std::io::Write;
use std::path::{Path, PathBuf};
use studyforge_core::presets::normalize_domain;

pub const BEGIN: &str = "# >>> StudyForge BEGIN (do not edit) >>>";
pub const END: &str = "# <<< StudyForge END <<<";

/// Dominant line ending of `text` ("\r\n" if CRLF lines are present).
pub fn detect_eol(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

/// Render the managed section (without a trailing newline after END).
pub fn render_section(domains: &[String], eol: &str) -> String {
    let mut lines = vec![BEGIN.to_string()];
    for d in domains {
        // Defense in depth: only well-formed hostnames ever reach the file.
        if normalize_domain(d).as_deref() == Some(d.as_str()) {
            lines.push(format!("0.0.0.0 {d}"));
            lines.push(format!(":: {d}"));
        }
    }
    lines.push(END.to_string());
    lines.join(eol)
}

fn is_our_entry(line: &str) -> bool {
    let l = line.trim();
    let mut it = l.split_whitespace();
    matches!((it.next(), it.next(), it.next()), (Some("0.0.0.0" | "::"), Some(_), None))
}

/// Locate the managed section: `(start_line, end_line_inclusive)` as indexes into `lines`.
fn find_section(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| l.trim() == BEGIN)?;
    if let Some(off) = lines[start + 1..].iter().position(|l| l.trim() == END) {
        return Some((start, start + 1 + off));
    }
    // Corrupt section (END missing): take only the BEGIN line plus the entry lines directly after it.
    let mut end = start;
    while end + 1 < lines.len() && is_our_entry(lines[end + 1]) {
        end += 1;
    }
    Some((start, end))
}

fn split_keep(text: &str) -> Vec<&str> {
    // split on \n, stripping a single trailing \r per line; remembers if text ended with a newline separately
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// Insert / replace the managed section. Empty `domains` removes it.
pub fn apply_to_text(original: &str, domains: &[String]) -> String {
    if domains.is_empty() {
        return remove_from_text(original);
    }
    let eol = detect_eol(original);
    let section = render_section(domains, eol);
    let lines = split_keep(original);
    if let Some((s, e)) = find_section(&lines) {
        let mut out: Vec<String> = lines[..s].iter().map(|l| l.to_string()).collect();
        out.push(section);
        out.extend(lines[e + 1..].iter().map(|l| l.to_string()));
        return out.join(eol);
    }
    let mut out = original.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(&section);
    out.push_str(eol);
    out
}

/// Remove the managed section (and one directly preceding blank separator line we may have added).
pub fn remove_from_text(original: &str) -> String {
    let lines = split_keep(original);
    let Some((s, e)) = find_section(&lines) else { return original.to_string() };
    let eol = detect_eol(original);
    let mut out: Vec<&str> = lines[..s].to_vec();
    out.extend(lines[e + 1..].iter());
    out.join(eol)
}

pub fn has_section(text: &str) -> bool {
    text.lines().any(|l| l.trim() == BEGIN)
}

/// Domains currently in the managed section.
pub fn section_domains(text: &str) -> Vec<String> {
    let lines = split_keep(text);
    let Some((s, e)) = find_section(&lines) else { return vec![] };
    let mut v: Vec<String> = lines[s + 1..e]
        .iter()
        .filter(|l| is_our_entry(l))
        .filter_map(|l| l.split_whitespace().nth(1).map(|d| d.to_string()))
        .collect();
    v.sort();
    v.dedup();
    v
}

#[derive(Debug, thiserror::Error)]
pub enum HostsError {
    #[error("io error on {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

fn io_err(path: &Path) -> impl Fn(std::io::Error) -> HostsError + '_ {
    move |source| HostsError::Io { path: path.to_path_buf(), source }
}

pub struct HostsManager {
    pub path: PathBuf,
    /// Original is copied here once, before the first modification.
    pub backup: PathBuf,
    pub dry_run: bool,
}

impl HostsManager {
    pub fn new(path: PathBuf, backup: PathBuf, dry_run: bool) -> Self {
        Self { path, backup, dry_run }
    }

    fn read(&self) -> Result<String, HostsError> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(io_err(&self.path)(e)),
        }
    }

    pub fn current_domains(&self) -> Vec<String> {
        self.read().map(|t| section_domains(&t)).unwrap_or_default()
    }

    pub fn is_applied(&self) -> bool {
        self.read().map(|t| has_section(&t)).unwrap_or(false)
    }

    /// Apply the given domain set. Returns `true` if the file changed (or would, in dry-run).
    pub fn apply(&self, domains: &[String]) -> Result<bool, HostsError> {
        let original = self.read()?;
        let updated = apply_to_text(&original, domains);
        if updated == original {
            return Ok(false);
        }
        if self.dry_run {
            tracing::info!("[dry-run] would update hosts file {} ({} domains)", self.path.display(), domains.len());
            return Ok(true);
        }
        self.backup_once(&original)?;
        write_atomic(&self.path, updated.as_bytes())?;
        Ok(true)
    }

    pub fn clear(&self) -> Result<bool, HostsError> {
        self.apply(&[])
    }

    fn backup_once(&self, original: &str) -> Result<(), HostsError> {
        if self.backup.exists() {
            return Ok(());
        }
        if let Some(p) = self.backup.parent() {
            std::fs::create_dir_all(p).map_err(io_err(p))?;
        }
        std::fs::write(&self.backup, original).map_err(io_err(&self.backup))
    }
}

/// Atomically replace `path` with `data`.
///
/// 1. resolve symlinks (so a symlinked hosts file is updated in place);
/// 2. write a temp file in the same directory, fsync it, copy permissions;
/// 3. rename over the target; on Windows clear the read-only attribute first,
///    retry on transient locks and restore the attribute afterwards;
/// 4. if rename is impossible (bind mount, EBUSY) fall back to an in-place
///    truncate+write+fsync.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), HostsError> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.studyforge.tmp", target.file_name().and_then(|n| n.to_str()).unwrap_or("hosts")));
    let existing_meta = std::fs::metadata(&target).ok();
    let was_readonly = existing_meta.as_ref().is_some_and(|m| m.permissions().readonly());

    let result = (|| -> std::io::Result<()> {
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        // Keep the file's permissions exactly as they were. (Never use
        // `set_readonly(false)` on Unix: it would make the file world-writable.)
        #[cfg(unix)]
        if let Some(m) = &existing_meta {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        // Windows refuses to replace a read-only file: clear the attribute for
        // the swap, it is restored below.
        #[cfg(windows)]
        if was_readonly {
            let mut p = std::fs::metadata(&target)?.permissions();
            p.set_readonly(false);
            std::fs::set_permissions(&target, p)?;
        }
        rename_with_retry(&tmp, &target)
    })();

    let result = match result {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("atomic replace of {} failed ({e}); falling back to in-place write", target.display());
            let _ = std::fs::remove_file(&tmp);
            (|| -> std::io::Result<()> {
                #[cfg(windows)]
                if was_readonly {
                    let mut p = std::fs::metadata(&target)?.permissions();
                    p.set_readonly(false);
                    std::fs::set_permissions(&target, p)?;
                }
                let mut f = std::fs::OpenOptions::new().write(true).truncate(true).create(true).open(&target)?;
                f.write_all(data)?;
                f.sync_all()
            })()
        }
    };
    let _ = std::fs::remove_file(&tmp);
    #[cfg(windows)]
    if was_readonly {
        if let Ok(m) = std::fs::metadata(&target) {
            let mut p = m.permissions();
            p.set_readonly(true);
            let _ = std::fs::set_permissions(&target, p);
        }
    }
    let _ = was_readonly; // used on Windows only
    result.map_err(io_err(&target))
}

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let attempts = if cfg!(windows) { 8 } else { 1 };
    let mut last = None;
    for i in 0..attempts {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                if i + 1 < attempts {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                }
            }
        }
    }
    Err(last.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const ORIGINAL: &str = "127.0.0.1 localhost\n::1 localhost\n# my comment\n10.0.0.5 nas.lan\n";

    #[test]
    fn append_and_remove_roundtrip_is_byte_exact() {
        let with = apply_to_text(ORIGINAL, &d(&["reddit.com", "www.reddit.com"]));
        assert!(with.starts_with(ORIGINAL));
        assert!(with.contains("0.0.0.0 reddit.com\n:: reddit.com\n"));
        assert!(with.contains(BEGIN) && with.contains(END));
        assert_eq!(remove_from_text(&with), ORIGINAL);
    }

    #[test]
    fn replaces_existing_section_only() {
        let a = apply_to_text(ORIGINAL, &d(&["a.com"]));
        let b = apply_to_text(&a, &d(&["b.com", "c.com"]));
        assert!(!b.contains("a.com") && b.contains("b.com") && b.contains("c.com"));
        assert_eq!(b.matches(BEGIN).count(), 1);
        assert_eq!(remove_from_text(&b), ORIGINAL);
        // content after the section is preserved too
        let tail = format!("{a}192.168.1.1 router\n");
        let c = apply_to_text(&tail, &d(&["z.com"]));
        assert!(c.ends_with("192.168.1.1 router\n"));
        assert!(c.contains("z.com") && !c.contains("a.com"));
    }

    #[test]
    fn preserves_crlf() {
        let orig = "127.0.0.1 localhost\r\n# x\r\n";
        let with = apply_to_text(orig, &d(&["a.com"]));
        assert!(with.contains("\r\n0.0.0.0 a.com\r\n:: a.com\r\n"));
        assert!(!with.replace("\r\n", "").contains('\n'), "no bare LF introduced");
        assert_eq!(remove_from_text(&with), orig);
    }

    #[test]
    fn no_trailing_newline_original() {
        let orig = "127.0.0.1 localhost";
        let with = apply_to_text(orig, &d(&["a.com"]));
        assert!(with.starts_with("127.0.0.1 localhost\n# >>>"));
        assert_eq!(remove_from_text(&with), "127.0.0.1 localhost\n".trim_end_matches('\n').to_string() + "\n");
    }

    #[test]
    fn empty_and_missing_files() {
        let with = apply_to_text("", &d(&["a.com"]));
        assert!(with.starts_with(BEGIN));
        assert_eq!(remove_from_text(&with), "");
        assert_eq!(remove_from_text(ORIGINAL), ORIGINAL, "nothing to remove");
        assert_eq!(apply_to_text(ORIGINAL, &[]), ORIGINAL);
    }

    #[test]
    fn invalid_domains_never_written() {
        let with =
            apply_to_text(ORIGINAL, &d(&["ok.com", "1.2.3.4", "evil.com\n0.0.0.0 bank.com", "*.x.com", "Upper.COM"]));
        assert!(with.contains("0.0.0.0 ok.com"));
        for bad in ["1.2.3.4", "bank.com", "*.x.com", "Upper"] {
            assert!(!with.contains(bad), "{bad} leaked");
        }
    }

    #[test]
    fn marker_without_end_removes_only_our_entries() {
        let corrupt = format!("127.0.0.1 localhost\n{BEGIN}\n0.0.0.0 a.com\n:: a.com\n10.0.0.1 keepme\n");
        let cleaned = remove_from_text(&corrupt);
        assert_eq!(cleaned, "127.0.0.1 localhost\n10.0.0.1 keepme\n");
    }

    #[test]
    fn section_domains_listing() {
        let with = apply_to_text(ORIGINAL, &d(&["b.com", "a.com"]));
        assert_eq!(section_domains(&with), d(&["a.com", "b.com"]));
        assert!(section_domains(ORIGINAL).is_empty());
        assert!(has_section(&with) && !has_section(ORIGINAL));
    }

    #[test]
    fn manager_roundtrip_backup_and_idempotence() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        let backup = dir.path().join("state").join("hosts.backup");
        std::fs::write(&hosts, ORIGINAL).unwrap();
        let m = HostsManager::new(hosts.clone(), backup.clone(), false);
        assert!(m.apply(&d(&["a.com"])).unwrap());
        assert!(!m.apply(&d(&["a.com"])).unwrap(), "idempotent");
        assert!(m.is_applied());
        assert_eq!(m.current_domains(), d(&["a.com"]));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), ORIGINAL);
        // second change must not overwrite the original backup
        m.apply(&d(&["b.com"])).unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), ORIGINAL);
        assert!(m.clear().unwrap());
        assert_eq!(std::fs::read_to_string(&hosts).unwrap(), ORIGINAL);
        assert!(!m.is_applied());
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.iter().all(|n| !n.contains("studyforge.tmp")), "{leftovers:?}");
    }

    #[test]
    fn manager_dry_run_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        std::fs::write(&hosts, ORIGINAL).unwrap();
        let m = HostsManager::new(hosts.clone(), dir.path().join("b"), true);
        assert!(m.apply(&d(&["a.com"])).unwrap());
        assert_eq!(std::fs::read_to_string(&hosts).unwrap(), ORIGINAL);
        assert!(!dir.path().join("b").exists());
    }

    #[test]
    fn manager_missing_hosts_file_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        let m = HostsManager::new(hosts.clone(), dir.path().join("b"), false);
        m.apply(&d(&["a.com"])).unwrap();
        assert!(std::fs::read_to_string(&hosts).unwrap().contains("a.com"));
    }

    #[test]
    fn readonly_attribute_is_handled_and_restored() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        std::fs::write(&hosts, ORIGINAL).unwrap();
        let mut p = std::fs::metadata(&hosts).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&hosts, p).unwrap();
        let m = HostsManager::new(hosts.clone(), dir.path().join("b"), false);
        m.apply(&d(&["a.com"])).unwrap();
        assert!(std::fs::read_to_string(&hosts).unwrap().contains("a.com"));
        assert!(std::fs::metadata(&hosts).unwrap().permissions().readonly(), "read-only attribute restored");
        m.clear().unwrap();
        assert_eq!(std::fs::read_to_string(&hosts).unwrap(), ORIGINAL);
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_preserved_never_world_writable() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o644u32, 0o600, 0o444] {
            let dir = tempfile::tempdir().unwrap();
            let hosts = dir.path().join("hosts");
            std::fs::write(&hosts, ORIGINAL).unwrap();
            std::fs::set_permissions(&hosts, std::fs::Permissions::from_mode(mode)).unwrap();
            let m = HostsManager::new(hosts.clone(), dir.path().join("b"), false);
            m.apply(&d(&["a.com"])).unwrap();
            let got = std::fs::metadata(&hosts).unwrap().permissions().mode() & 0o777;
            assert_eq!(got, mode, "mode after apply");
            m.clear().unwrap();
            assert_eq!(std::fs::metadata(&hosts).unwrap().permissions().mode() & 0o777, mode, "mode after clear");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_hosts_is_updated_through_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real_hosts");
        let link = dir.path().join("hosts");
        std::fs::write(&real, ORIGINAL).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let m = HostsManager::new(link.clone(), dir.path().join("b"), false);
        m.apply(&d(&["a.com"])).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "link preserved");
        assert!(std::fs::read_to_string(&real).unwrap().contains("a.com"));
    }
}
