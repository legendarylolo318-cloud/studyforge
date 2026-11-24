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

// ---------------------------------------------------------------------------
// Text layer. Works on raw bytes, line by line, keeping every line's own
// terminator, so everything outside the markers survives byte-for-byte:
// non-UTF-8 (e.g. ANSI) comments, a BOM, mixed line endings and a missing
// trailing newline. Only ASCII is ever inserted.
// ---------------------------------------------------------------------------

/// Lines including their terminator (`\n`, `\r\n`, or none for the last line).
fn split_lines(b: &[u8]) -> Vec<&[u8]> {
    b.split_inclusive(|&c| c == b'\n').collect()
}

/// Line without its terminator.
fn content(line: &[u8]) -> &[u8] {
    let l = line.strip_suffix(b"\n").unwrap_or(line);
    l.strip_suffix(b"\r").unwrap_or(l)
}

/// The terminator of a line (possibly empty).
fn terminator(line: &[u8]) -> &[u8] {
    &line[content(line).len()..]
}

fn trimmed(line: &[u8]) -> &[u8] {
    content(line).trim_ascii()
}

fn is_begin(line: &[u8]) -> bool {
    // A BOM may precede the marker if the section is the first line.
    let t = trimmed(line);
    t.strip_prefix("\u{feff}".as_bytes()).unwrap_or(t) == BEGIN.as_bytes()
}

fn is_end(line: &[u8]) -> bool {
    trimmed(line) == END.as_bytes()
}

fn is_our_entry(line: &[u8]) -> bool {
    let Ok(l) = std::str::from_utf8(trimmed(line)) else { return false };
    let mut it = l.split_whitespace();
    matches!((it.next(), it.next(), it.next()), (Some("0.0.0.0" | "::"), Some(_), None))
}

/// Dominant line ending of `text` (CRLF only if most lines use it).
pub fn detect_eol_bytes(text: &[u8]) -> &'static [u8] {
    let lines = split_lines(text);
    let crlf = lines.iter().filter(|l| l.ends_with(b"\r\n")).count();
    let lf = lines.iter().filter(|l| l.ends_with(b"\n")).count() - crlf;
    if crlf > lf {
        b"\r\n"
    } else {
        b"\n"
    }
}

pub fn detect_eol(text: &str) -> &'static str {
    if detect_eol_bytes(text.as_bytes()) == b"\r\n" {
        "\r\n"
    } else {
        "\n"
    }
}

/// Render the managed section, every line terminated by `eol` except END,
/// which gets `end_term` (empty when the section is the last thing in the file).
fn render_section_bytes(domains: &[String], eol: &[u8], end_term: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(BEGIN.as_bytes());
    out.extend_from_slice(eol);
    for d in domains {
        // Defense in depth: only well-formed hostnames ever reach the file.
        if normalize_domain(d).as_deref() == Some(d.as_str()) {
            for prefix in ["0.0.0.0 ", ":: "] {
                out.extend_from_slice(prefix.as_bytes());
                out.extend_from_slice(d.as_bytes());
                out.extend_from_slice(eol);
            }
        }
    }
    out.extend_from_slice(END.as_bytes());
    out.extend_from_slice(end_term);
    out
}

/// Render the managed section (without a trailing newline after END).
pub fn render_section(domains: &[String], eol: &str) -> String {
    String::from_utf8(render_section_bytes(domains, eol.as_bytes(), b"")).expect("ASCII")
}

/// All managed sections as inclusive line ranges. Normally there is at most
/// one; duplicates (hand edits, old bugs) are all found so they can be removed.
/// A BEGIN without END ("half-written") covers only the BEGIN line plus the
/// entry lines that directly follow it, so user lines are never swallowed.
fn find_sections(lines: &[&[u8]]) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        if is_begin(lines[i]) {
            let end = match lines[i + 1..].iter().position(|l| is_end(l) || is_begin(l)) {
                Some(off) if is_end(lines[i + 1 + off]) => i + 1 + off,
                _ => {
                    let mut e = i;
                    while e + 1 < lines.len() && is_our_entry(lines[e + 1]) {
                        e += 1;
                    }
                    e
                }
            };
            out.push((i, end));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Remove every managed section. When a section was the last thing in the file
/// and its final line had no terminator, the newline before it was added by us
/// (see `apply_bytes`) and is removed too, so the original is restored exactly.
pub fn remove_bytes(original: &[u8]) -> Vec<u8> {
    let lines = split_lines(original);
    let sections = find_sections(&lines);
    if sections.is_empty() {
        return original.to_vec();
    }
    let mut kept: Vec<Vec<u8>> = Vec::with_capacity(lines.len());
    let mut next = 0;
    for (s, e) in sections {
        kept.extend(lines[next..s].iter().map(|l| l.to_vec()));
        if e + 1 == lines.len() && terminator(lines[e]).is_empty() {
            if let Some(last) = kept.last_mut() {
                let n = content(last).len();
                last.truncate(n);
            }
        }
        next = e + 1;
    }
    kept.extend(lines[next..].iter().map(|l| l.to_vec()));
    kept.concat()
}

/// Insert / replace the managed section. Empty `domains` removes it.
pub fn apply_bytes(original: &[u8], domains: &[String]) -> Vec<u8> {
    if domains.is_empty() {
        return remove_bytes(original);
    }
    let eol = detect_eol_bytes(original);
    let lines = split_lines(original);
    let sections = find_sections(&lines);
    if let Some(&(s, e)) = sections.first() {
        // Replace in place; END keeps the old END line's terminator so the text
        // after it is untouched. Any duplicate sections further down are dropped.
        let mut out: Vec<u8> = lines[..s].concat();
        out.extend(render_section_bytes(domains, eol, terminator(lines[e])));
        out.extend(remove_bytes(&lines[e + 1..].concat()));
        return out;
    }
    let mut out = original.to_vec();
    if out.is_empty() || out.ends_with(b"\n") {
        out.extend(render_section_bytes(domains, eol, eol));
    } else {
        // No trailing newline: the separator belongs to us and END stays
        // unterminated, which tells `remove_bytes` to take the separator back.
        out.extend_from_slice(eol);
        out.extend(render_section_bytes(domains, eol, b""));
    }
    out
}

pub fn apply_to_text(original: &str, domains: &[String]) -> String {
    String::from_utf8(apply_bytes(original.as_bytes(), domains)).expect("only ASCII is spliced into valid UTF-8")
}

pub fn remove_from_text(original: &str) -> String {
    String::from_utf8(remove_bytes(original.as_bytes())).expect("only whole lines are removed")
}

pub fn has_section_bytes(text: &[u8]) -> bool {
    split_lines(text).iter().any(|l| is_begin(l))
}

pub fn has_section(text: &str) -> bool {
    has_section_bytes(text.as_bytes())
}

/// Domains currently in the managed section(s).
pub fn section_domains_bytes(text: &[u8]) -> Vec<String> {
    let lines = split_lines(text);
    let mut v: Vec<String> = vec![];
    for (s, e) in find_sections(&lines) {
        for l in &lines[s + 1..=e] {
            if is_our_entry(l) {
                if let Some(d) = std::str::from_utf8(trimmed(l)).ok().and_then(|t| t.split_whitespace().nth(1)) {
                    v.push(d.to_string());
                }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

pub fn section_domains(text: &str) -> Vec<String> {
    section_domains_bytes(text.as_bytes())
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

    fn read(&self) -> Result<Vec<u8>, HostsError> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_err(&self.path)(e)),
        }
    }

    pub fn current_domains(&self) -> Vec<String> {
        self.read().map(|t| section_domains_bytes(&t)).unwrap_or_default()
    }

    pub fn is_applied(&self) -> bool {
        self.read().map(|t| has_section_bytes(&t)).unwrap_or(false)
    }

    /// Apply the given domain set. Returns `true` if the file changed (or would, in dry-run).
    pub fn apply(&self, domains: &[String]) -> Result<bool, HostsError> {
        let original = self.read()?;
        let updated = apply_bytes(&original, domains);
        if updated == original {
            return Ok(false);
        }
        if self.dry_run {
            tracing::info!("[dry-run] would update hosts file {} ({} domains)", self.path.display(), domains.len());
            return Ok(true);
        }
        self.backup_once(&original)?;
        write_atomic(&self.path, &updated)?;
        Ok(true)
    }

    pub fn clear(&self) -> Result<bool, HostsError> {
        self.apply(&[])
    }

    /// Raw-byte copy of the file as it was before StudyForge first touched it.
    fn backup_once(&self, original: &[u8]) -> Result<(), HostsError> {
        if self.backup.exists() {
            return Ok(());
        }
        if let Some(p) = self.backup.parent() {
            std::fs::create_dir_all(p).map_err(io_err(p))?;
        }
        std::fs::write(&self.backup, remove_bytes(original)).map_err(io_err(&self.backup))
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
        assert!(with.ends_with(END), "END stays unterminated to remember the separator is ours");
        assert_eq!(remove_from_text(&with), orig, "no newline is left behind");
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

/// Randomised ("property") tests of the text layer. Deterministic PRNG so
/// failures are reproducible; thousands of generated hosts files per run.
#[cfg(test)]
mod prop_tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A random hosts file *without* StudyForge markers.
    fn gen_file(r: &mut Rng) -> Vec<u8> {
        const LINES: &[&[u8]] = &[
            b"127.0.0.1 localhost",
            b"::1 localhost ip6-localhost",
            b"# a comment",
            b"# commentaire \xc3\xa9t\xc3\xa9 (UTF-8)",
            b"# ANSI \xe9t\xe9 \xff\xfe (not UTF-8)",
            b"",
            b"   ",
            b"\t# indented",
            b"0.0.0.0 user-blocked.example",
            b":: user-blocked.example",
            b"10.0.0.5 nas.lan nas",
            b"# >>> not our marker",
        ];
        let mut out = Vec::new();
        if r.below(5) == 0 {
            out.extend_from_slice("\u{feff}".as_bytes());
        }
        let n = r.below(12);
        for i in 0..n {
            out.extend_from_slice(LINES[r.below(LINES.len())]);
            let last = i + 1 == n;
            match r.below(if last { 3 } else { 2 }) {
                0 => out.extend_from_slice(b"\n"),
                1 => out.extend_from_slice(b"\r\n"),
                _ => {} // last line without newline
            }
        }
        out
    }

    fn gen_domains(r: &mut Rng) -> Vec<String> {
        const D: &[&str] = &[
            "reddit.com",
            "www.reddit.com",
            "x.com",
            "1.2.3.4",
            "bad domain",
            "youtube.com",
            "m.youtube.com",
            "a.b.c.example",
        ];
        let n = r.below(5);
        (0..n).map(|_| D[r.below(D.len())].to_string()).collect()
    }

    fn valid(d: &[String]) -> Vec<String> {
        let mut v: Vec<String> =
            d.iter().filter(|x| normalize_domain(x).as_deref() == Some(x.as_str())).cloned().collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn apply_then_clear_restores_every_byte() {
        let mut r = Rng(0x5eed_1234_abcd_ef01);
        for case in 0..5000 {
            let orig = gen_file(&mut r);
            let d = gen_domains(&mut r);
            let applied = apply_bytes(&orig, &d);
            if valid(&d).is_empty() && d.is_empty() {
                assert_eq!(applied, orig, "case {case}: empty spec must not touch the file");
            }
            assert_eq!(remove_bytes(&applied), orig, "case {case}: {:?}", String::from_utf8_lossy(&orig));
            if !d.is_empty() {
                assert!(
                    applied.starts_with(&orig) || orig.last() != Some(&b'\n') && !orig.is_empty(),
                    "case {case}: prefix kept"
                );
                let begins = split_lines(&applied).iter().filter(|l| is_begin(l)).count();
                assert_eq!(begins, 1, "case {case}: exactly one section");
                assert_eq!(section_domains_bytes(&applied), valid(&d), "case {case}");
            }
        }
    }

    #[test]
    fn idempotent_and_replace_semantics() {
        let mut r = Rng(0x0123_4567_89ab_cdef);
        for case in 0..3000 {
            let orig = gen_file(&mut r);
            let (d1, d2) = (gen_domains(&mut r), gen_domains(&mut r));
            let a1 = apply_bytes(&orig, &d1);
            assert_eq!(apply_bytes(&a1, &d1), a1, "case {case}: idempotent");
            assert_eq!(apply_bytes(&a1, &d2), apply_bytes(&orig, &d2), "case {case}: re-apply == fresh apply");
        }
    }

    #[test]
    fn long_random_apply_clear_sequences_never_drift() {
        let mut r = Rng(0xdead_beef_cafe_f00d);
        for case in 0..500 {
            let orig = gen_file(&mut r);
            let mut cur = orig.clone();
            for _ in 0..20 {
                cur = if r.below(3) == 0 { remove_bytes(&cur) } else { apply_bytes(&cur, &gen_domains(&mut r)) };
            }
            assert_eq!(remove_bytes(&cur), orig, "case {case}");
        }
    }

    #[test]
    fn duplicate_and_half_written_sections_are_repaired() {
        let mut r = Rng(42);
        for case in 0..1000 {
            let user = gen_file(&mut r);
            let mut user_nl = user.clone();
            if !user_nl.is_empty() && !user_nl.ends_with(b"\n") {
                user_nl.push(b'\n');
            }
            // two sections (e.g. an old bug), the second one half-written
            let mut f = user_nl.clone();
            f.extend_from_slice(format!("{BEGIN}\n0.0.0.0 old.com\n:: old.com\n{END}\n").as_bytes());
            f.extend_from_slice(b"# keep me\n");
            f.extend_from_slice(format!("{BEGIN}\n0.0.0.0 stale.com\n").as_bytes());
            let mut expect = user_nl.clone();
            expect.extend_from_slice(b"# keep me\n");
            assert_eq!(remove_bytes(&f), expect, "case {case}");
            let applied = apply_bytes(&f, &["new.com".to_string()]);
            assert_eq!(section_domains_bytes(&applied), vec!["new.com"], "case {case}");
            assert_eq!(remove_bytes(&applied), expect, "case {case}");
        }
    }

    #[test]
    fn hand_edited_section_content_is_replaced_but_nothing_outside() {
        let f = format!("127.0.0.1 localhost\n{BEGIN}\n# user scribbled here\n0.0.0.0 a.com\n{END}\n10.0.0.1 nas\n");
        let out = apply_to_text(&f, &["b.com".to_string()]);
        assert_eq!(out, format!("127.0.0.1 localhost\n{BEGIN}\n0.0.0.0 b.com\n:: b.com\n{END}\n10.0.0.1 nas\n"));
    }

    #[test]
    fn bom_and_non_utf8_survive_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        let orig: &[u8] = b"\xef\xbb\xbf# \xe9t\xe9 ANSI\r\n127.0.0.1 localhost\r\n# no newline at end";
        std::fs::write(&hosts, orig).unwrap();
        let m = HostsManager::new(hosts.clone(), dir.path().join("backup"), false);
        m.apply(&["a.com".to_string()]).unwrap();
        let mid = std::fs::read(&hosts).unwrap();
        assert!(mid.starts_with(orig));
        assert!(mid.windows(2).filter(|w| w == b"\r\n").count() >= 4, "CRLF used for our lines too");
        m.clear().unwrap();
        assert_eq!(std::fs::read(&hosts).unwrap(), orig);
        assert_eq!(std::fs::read(dir.path().join("backup")).unwrap(), orig, "backup is raw bytes");
    }
}
