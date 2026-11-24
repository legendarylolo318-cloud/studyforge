//! Layer 2: browser policies.
//!
//! Browsers with DNS-over-HTTPS bypass the hosts file, so while a block is
//! active we additionally push managed policies:
//!
//! * Firefox: `policies.json` -> `WebsiteFilter` {Block, Exceptions} and
//!   `DNSOverHTTPS` {Enabled:false, Locked:true}. The user's existing policies
//!   are merged, and we remember exactly what we added so removal restores the
//!   previous state (and deletes the file only if we created it).
//! * Chromium family (Chrome/Chromium/Brave/Edge): a dedicated managed policy
//!   file `zz-studyforge.json` (URLBlocklist / URLAllowlist /
//!   DnsOverHttpsMode "off"). Other policy files in the directory are never
//!   touched; their list policies are folded into ours so they keep working.
//!   On Windows the same is done in the registry via the [`Registry`] trait.

use crate::hosts::write_atomic;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use studyforge_core::blocking::BlockSpec;

pub const MARKER_KEY: &str = "_studyforge_managed";
pub const CHROMIUM_FILE: &str = "zz-studyforge.json";

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("io error on {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{0}")]
    Other(String),
}

fn io<'a>(p: &'a Path) -> impl Fn(std::io::Error) -> PolicyError + 'a {
    move |source| PolicyError::Io { path: p.to_path_buf(), source }
}

/// Everything the policy layer needs from a [`BlockSpec`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PolicySpec {
    pub blocked: Vec<String>,
    pub allowlist: bool,
    pub allowed: Vec<String>,
}

impl PolicySpec {
    pub fn from_spec(spec: &BlockSpec) -> PolicySpec {
        PolicySpec { blocked: spec.blocked_domains(), allowlist: spec.allowlist.enabled, allowed: spec.allowed_domains() }
    }
    pub fn is_empty(&self) -> bool {
        self.blocked.is_empty() && !self.allowlist
    }
    fn firefox_block(&self) -> Vec<String> {
        if self.allowlist {
            vec!["<all_urls>".into()]
        } else {
            self.blocked.iter().map(|d| format!("*://*.{d}/*")).collect()
        }
    }
    fn firefox_exceptions(&self) -> Vec<String> {
        if self.allowlist {
            self.allowed.iter().map(|d| format!("*://*.{d}/*")).collect()
        } else {
            vec![]
        }
    }
    fn chromium_block(&self) -> Vec<String> {
        if self.allowlist {
            vec!["*".into()]
        } else {
            self.blocked.clone()
        }
    }
    fn chromium_allow(&self) -> Vec<String> {
        if self.allowlist {
            let mut v = self.allowed.clone();
            v.push("chrome://*".into());
            v.push("about:blank".into());
            v
        } else {
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------
// bookkeeping
// ---------------------------------------------------------------------------

/// A value we may have overwritten. (An `Option<Option<T>>` would not survive
/// a JSON round trip: `Some(None)` serializes as `null` and reads back as `None`.)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum Saved<T> {
    /// We never touched it.
    #[default]
    Untouched,
    /// The key did not exist before we set it.
    Absent,
    /// The key held this value before.
    Present(T),
}

impl<T> Saved<T> {
    fn of(v: Option<T>) -> Self {
        match v {
            Some(v) => Saved::Present(v),
            None => Saved::Absent,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileRecord {
    pub created_file: bool,
    pub created_dirs: Vec<PathBuf>,
    pub added_block: Vec<String>,
    pub added_exceptions: Vec<String>,
    pub website_filter_existed: bool,
    pub had_block_key: bool,
    pub had_exceptions_key: bool,
    pub doh_before: Saved<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RegRecord {
    pub added_block: Vec<String>,
    pub added_allow: Vec<String>,
    pub doh_before: Saved<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PolicyState {
    pub firefox: BTreeMap<String, FileRecord>,
    pub chromium_dirs_created: BTreeMap<String, Vec<PathBuf>>,
    pub registry: BTreeMap<String, RegRecord>,
}

impl PolicyState {
    pub fn load(path: &Path) -> PolicyState {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    pub fn save(&self, path: &Path) -> Result<(), PolicyError> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(io(p))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| PolicyError::Other(e.to_string()))?;
        write_atomic(path, &json).map_err(|e| PolicyError::Other(e.to_string()))
    }
    pub fn is_empty(&self) -> bool {
        self.firefox.is_empty() && self.chromium_dirs_created.is_empty() && self.registry.is_empty()
    }
}

fn read_json_object(path: &Path) -> Result<Option<Map<String, Value>>, PolicyError> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(Some(Map::new())),
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => Ok(Some(m)),
            Ok(_) | Err(_) => Err(PolicyError::Other(format!("{} is not a JSON object; leaving it untouched", path.display()))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path)(e)),
    }
}

fn str_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn write_json(path: &Path, v: &Value, dry: bool) -> Result<(), PolicyError> {
    if dry {
        tracing::info!("[dry-run] would write {}", path.display());
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(io(p))?;
    }
    let mut bytes = serde_json::to_vec_pretty(v).map_err(|e| PolicyError::Other(e.to_string()))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes).map_err(|e| PolicyError::Other(e.to_string()))?;
    // policy files must be world-readable for the (unprivileged) browser
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Firefox
// ---------------------------------------------------------------------------

/// Undo `rec` on the in-memory document.
fn firefox_revert(doc: &mut Map<String, Value>, rec: &FileRecord) {
    let Some(Value::Object(policies)) = doc.get_mut("policies") else { return };
    if let Some(Value::Object(wf)) = policies.get_mut("WebsiteFilter") {
        for (key, added, had) in [("Block", &rec.added_block, rec.had_block_key), ("Exceptions", &rec.added_exceptions, rec.had_exceptions_key)] {
            if let Some(Value::Array(arr)) = wf.get_mut(key) {
                arr.retain(|x| x.as_str().is_none_or(|s| !added.iter().any(|a| a == s)));
                if arr.is_empty() && !had {
                    wf.remove(key);
                }
            }
        }
        if wf.is_empty() && !rec.website_filter_existed {
            policies.remove("WebsiteFilter");
        }
    }
    match &rec.doh_before {
        Saved::Absent => {
            policies.remove("DNSOverHTTPS");
        }
        Saved::Present(v) => {
            policies.insert("DNSOverHTTPS".into(), v.clone());
        }
        Saved::Untouched => {}
    }
}

/// Apply (or, with an empty spec, remove) Firefox policy for one file.
pub fn firefox_apply(path: &Path, spec: &PolicySpec, state: &mut PolicyState, dry: bool) -> Result<(), PolicyError> {
    let key = path.to_string_lossy().to_string();
    let prev = state.firefox.get(&key).cloned();
    let existing = read_json_object(path)?;
    let file_existed = existing.is_some();
    let mut doc = existing.unwrap_or_default();

    // Start from the user's own policies: strip our previous contribution.
    if let Some(rec) = &prev {
        firefox_revert(&mut doc, rec);
    }
    let created_before = prev.as_ref().is_some_and(|r| r.created_file) || !file_existed;

    if spec.is_empty() {
        // finish removal
        let empty = doc.get("policies").and_then(|p| p.as_object()).is_none_or(|p| p.is_empty()) && doc.keys().all(|k| k == "policies");
        if prev.is_some() {
            if empty && created_before && file_existed {
                if dry {
                    tracing::info!("[dry-run] would delete {}", path.display());
                } else {
                    std::fs::remove_file(path).map_err(io(path))?;
                    for d in prev.as_ref().map(|r| r.created_dirs.clone()).unwrap_or_default().iter() {
                        let _ = std::fs::remove_dir(d); // only removes if empty
                    }
                }
            } else if file_existed {
                write_json(path, &Value::Object(doc), dry)?;
            }
            if !dry {
                state.firefox.remove(&key);
            }
        }
        return Ok(());
    }

    // Create parent dirs, remembering which ones are new.
    let mut created_dirs = prev.as_ref().map(|r| r.created_dirs.clone()).unwrap_or_default();
    if !file_existed {
        let mut missing = vec![];
        let mut cur = path.parent();
        while let Some(d) = cur {
            if d.exists() {
                break;
            }
            missing.push(d.to_path_buf());
            cur = d.parent();
        }
        if !dry {
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).map_err(io(p))?;
            }
        }
        for d in missing {
            if !created_dirs.contains(&d) {
                created_dirs.push(d);
            }
        }
    }

    let policies = doc.entry("policies").or_insert_with(|| json!({}));
    let Value::Object(policies) = policies else {
        return Err(PolicyError::Other(format!("{}: \"policies\" is not an object", path.display())));
    };
    let mut rec = FileRecord { created_file: created_before, created_dirs, ..Default::default() };
    rec.website_filter_existed = policies.contains_key("WebsiteFilter");
    let wf = policies.entry("WebsiteFilter").or_insert_with(|| json!({}));
    let Value::Object(wf) = wf else {
        return Err(PolicyError::Other(format!("{}: \"WebsiteFilter\" is not an object", path.display())));
    };
    rec.had_block_key = wf.contains_key("Block");
    rec.had_exceptions_key = wf.contains_key("Exceptions");
    for (k, ours, added) in [("Block", spec.firefox_block(), &mut rec.added_block), ("Exceptions", spec.firefox_exceptions(), &mut rec.added_exceptions)] {
        if ours.is_empty() {
            continue;
        }
        let mut cur = str_array(wf.get(k));
        for o in ours {
            if !cur.contains(&o) {
                added.push(o.clone());
                cur.push(o);
            }
        }
        wf.insert(k.into(), json!(cur));
    }
    rec.doh_before = Saved::of(policies.get("DNSOverHTTPS").cloned());
    policies.insert("DNSOverHTTPS".into(), json!({"Enabled": false, "Locked": true}));

    write_json(path, &Value::Object(doc), dry)?;
    if !dry {
        state.firefox.insert(key, rec);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Chromium family (managed policy files)
// ---------------------------------------------------------------------------

pub fn chromium_apply(dir: &Path, spec: &PolicySpec, state: &mut PolicyState, dry: bool) -> Result<(), PolicyError> {
    let file = dir.join(CHROMIUM_FILE);
    let key = dir.to_string_lossy().to_string();

    if spec.is_empty() {
        if let Some(Value::Object(m)) = read_json_object(&file).ok().flatten().map(Value::Object) {
            if m.get(MARKER_KEY) == Some(&Value::Bool(true)) {
                if dry {
                    tracing::info!("[dry-run] would delete {}", file.display());
                } else {
                    std::fs::remove_file(&file).map_err(io(&file))?;
                }
            }
        }
        if !dry {
            for d in state.chromium_dirs_created.remove(&key).unwrap_or_default().iter() {
                let _ = std::fs::remove_dir(d);
            }
        }
        return Ok(());
    }

    // Fold list policies of other files into ours so they keep working.
    let mut block = vec![];
    let mut allow = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p == file || p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(Some(m)) = read_json_object(&p) {
                block.extend(str_array(m.get("URLBlocklist")));
                allow.extend(str_array(m.get("URLAllowlist")));
            }
        }
    }
    for b in spec.chromium_block() {
        if !block.contains(&b) {
            block.push(b);
        }
    }
    for a in spec.chromium_allow() {
        if !allow.contains(&a) {
            allow.push(a);
        }
    }

    if !dir.exists() && !dry {
        let mut missing = vec![];
        let mut cur = Some(dir);
        while let Some(d) = cur {
            if d.exists() {
                break;
            }
            missing.push(d.to_path_buf());
            cur = d.parent();
        }
        std::fs::create_dir_all(dir).map_err(io(dir))?;
        state.chromium_dirs_created.entry(key).or_default().extend(missing);
    }
    let mut doc = json!({ MARKER_KEY: true, "URLBlocklist": block, "DnsOverHttpsMode": "off" });
    if !allow.is_empty() {
        doc["URLAllowlist"] = json!(allow);
    }
    write_json(&file, &doc, dry)
}

// ---------------------------------------------------------------------------
// Windows registry (logic is platform independent; backend is pluggable)
// ---------------------------------------------------------------------------

pub trait Registry {
    fn values(&self, key: &str) -> Vec<(String, String)>;
    fn get(&self, key: &str, name: &str) -> Option<String>;
    fn set(&mut self, key: &str, name: &str, value: &str) -> std::io::Result<()>;
    fn delete_value(&mut self, key: &str, name: &str) -> std::io::Result<()>;
    /// Delete the key if it has no values and no subkeys.
    fn delete_key_if_empty(&mut self, key: &str);
}

fn next_index(existing: &[(String, String)]) -> u32 {
    existing.iter().filter_map(|(n, _)| n.parse::<u32>().ok()).max().unwrap_or(0) + 1
}

pub fn registry_apply<R: Registry>(reg: &mut R, root: &str, spec: &PolicySpec, state: &mut PolicyState) -> std::io::Result<()> {
    let block_key = format!(r"{root}\URLBlocklist");
    let allow_key = format!(r"{root}\URLAllowlist");
    let prev = state.registry.get(root).cloned();

    // revert previous contribution first
    if let Some(rec) = &prev {
        for (k, names) in [(&block_key, &rec.added_block), (&allow_key, &rec.added_allow)] {
            for n in names {
                let _ = reg.delete_value(k, n);
            }
            reg.delete_key_if_empty(k);
        }
        match &rec.doh_before {
            Saved::Absent => {
                let _ = reg.delete_value(root, "DnsOverHttpsMode");
            }
            Saved::Present(v) => reg.set(root, "DnsOverHttpsMode", v)?,
            Saved::Untouched => {}
        }
        reg.delete_key_if_empty(root);
    }
    if spec.is_empty() {
        state.registry.remove(root);
        return Ok(());
    }

    let mut rec = RegRecord::default();
    for (key, entries, added) in [(&block_key, spec.chromium_block(), &mut rec.added_block), (&allow_key, spec.chromium_allow(), &mut rec.added_allow)] {
        let mut existing = reg.values(key);
        for e in entries {
            if existing.iter().any(|(_, v)| *v == e) {
                continue; // already present (user's own): leave it, don't claim it
            }
            let name = next_index(&existing).to_string();
            reg.set(key, &name, &e)?;
            existing.push((name.clone(), e));
            added.push(name);
        }
    }
    rec.doh_before = Saved::of(reg.get(root, "DnsOverHttpsMode"));
    reg.set(root, "DnsOverHttpsMode", "off")?;
    state.registry.insert(root.to_string(), rec);
    Ok(())
}

#[cfg(windows)]
pub mod winreg_backend {
    use super::Registry;
    use winreg::enums::*;
    use winreg::RegKey;

    pub struct WinRegistry;

    fn hklm() -> RegKey {
        RegKey::predef(HKEY_LOCAL_MACHINE)
    }

    impl Registry for WinRegistry {
        fn values(&self, key: &str) -> Vec<(String, String)> {
            hklm()
                .open_subkey(key)
                .map(|k| k.enum_values().filter_map(|r| r.ok()).filter_map(|(n, v)| k.get_value::<String, _>(&n).ok().map(|s| (n, s)).filter(|_| v.vtype == REG_SZ)).collect())
                .unwrap_or_default()
        }
        fn get(&self, key: &str, name: &str) -> Option<String> {
            hklm().open_subkey(key).ok()?.get_value::<String, _>(name).ok()
        }
        fn set(&mut self, key: &str, name: &str, value: &str) -> std::io::Result<()> {
            let (k, _) = hklm().create_subkey(key)?;
            k.set_value(name, &value.to_string())
        }
        fn delete_value(&mut self, key: &str, name: &str) -> std::io::Result<()> {
            hklm().open_subkey_with_flags(key, KEY_SET_VALUE)?.delete_value(name)
        }
        fn delete_key_if_empty(&mut self, key: &str) {
            if let Ok(k) = hklm().open_subkey(key) {
                let empty = k.enum_values().next().is_none() && k.enum_keys().next().is_none();
                drop(k);
                if empty {
                    let _ = hklm().delete_subkey(key);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Apply `spec` to every browser layer reachable through `paths`. Returns a
/// list of non-fatal problems.
pub fn apply_all(paths: &crate::paths::Paths, spec: &PolicySpec, dry: bool) -> Vec<String> {
    let state_path = paths.policy_state();
    let mut state = PolicyState::load(&state_path);
    let mut problems = vec![];
    for f in &paths.firefox_policy_files {
        if let Err(e) = firefox_apply(f, spec, &mut state, dry) {
            problems.push(e.to_string());
        }
    }
    for d in &paths.chromium_policy_dirs {
        if let Err(e) = chromium_apply(d, spec, &mut state, dry) {
            problems.push(e.to_string());
        }
    }
    #[cfg(windows)]
    {
        if !dry {
            let mut reg = winreg_backend::WinRegistry;
            for root in &paths.registry_roots {
                if let Err(e) = registry_apply(&mut reg, root, spec, &mut state) {
                    problems.push(format!("registry {root}: {e}"));
                }
            }
        }
    }
    if !dry {
        if state.is_empty() {
            let _ = std::fs::remove_file(&state_path);
        } else if let Err(e) = state.save(&state_path) {
            problems.push(e.to_string());
        }
    }
    problems
}

pub fn clear_all(paths: &crate::paths::Paths, dry: bool) -> Vec<String> {
    apply_all(paths, &PolicySpec::default(), dry)
}

/// True if any managed policy from us is currently in place.
pub fn is_applied(paths: &crate::paths::Paths) -> bool {
    let st = PolicyState::load(&paths.policy_state());
    !st.firefox.is_empty()
        || !st.registry.is_empty()
        || paths.chromium_policy_dirs.iter().any(|d| d.join(CHROMIUM_FILE).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    fn spec(blocked: &[&str]) -> PolicySpec {
        PolicySpec { blocked: blocked.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }
    fn allow_spec(allowed: &[&str]) -> PolicySpec {
        PolicySpec { allowlist: true, allowed: allowed.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }
    fn read(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn firefox_creates_and_fully_removes_file_and_dirs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        let p = Paths::under(root.path());
        let ff = &p.firefox_policy_files[0];
        let mut st = PolicyState::default();
        firefox_apply(ff, &spec(&["reddit.com"]), &mut st, false).unwrap();
        let v = read(ff);
        assert_eq!(v["policies"]["WebsiteFilter"]["Block"], json!(["*://*.reddit.com/*"]));
        assert_eq!(v["policies"]["DNSOverHTTPS"], json!({"Enabled": false, "Locked": true}));
        firefox_apply(ff, &PolicySpec::default(), &mut st, false).unwrap();
        assert!(!ff.exists());
        assert!(!root.path().join("etc/firefox").exists(), "dirs we created are removed");
        assert!(st.firefox.is_empty());
        assert!(root.path().join("etc").exists(), "pre-existing dir kept");
    }

    #[test]
    fn firefox_merges_with_user_policies_and_restores_them() {
        let root = tempfile::tempdir().unwrap();
        let ff = root.path().join("policies.json");
        let user = json!({"policies": {
            "DisableTelemetry": true,
            "DNSOverHTTPS": {"Enabled": true, "ProviderURL": "https://dns.example/q"},
            "WebsiteFilter": {"Block": ["*://*.userblocked.com/*"], "Exceptions": ["*://ok.userblocked.com/*"]}
        }});
        std::fs::write(&ff, serde_json::to_string_pretty(&user).unwrap()).unwrap();
        let mut st = PolicyState::default();
        firefox_apply(&ff, &spec(&["a.com"]), &mut st, false).unwrap();
        let v = read(&ff);
        assert_eq!(v["policies"]["DisableTelemetry"], json!(true));
        assert_eq!(v["policies"]["WebsiteFilter"]["Block"], json!(["*://*.userblocked.com/*", "*://*.a.com/*"]));
        assert_eq!(v["policies"]["DNSOverHTTPS"]["Locked"], json!(true));
        // changing the spec swaps our entries without duplicating
        firefox_apply(&ff, &spec(&["b.com"]), &mut st, false).unwrap();
        assert_eq!(read(&ff)["policies"]["WebsiteFilter"]["Block"], json!(["*://*.userblocked.com/*", "*://*.b.com/*"]));
        // removal restores the user's file exactly
        firefox_apply(&ff, &PolicySpec::default(), &mut st, false).unwrap();
        assert_eq!(read(&ff), user);
    }

    #[test]
    fn firefox_user_entry_equal_to_ours_is_not_removed() {
        let root = tempfile::tempdir().unwrap();
        let ff = root.path().join("p.json");
        let user = json!({"policies": {"WebsiteFilter": {"Block": ["*://*.a.com/*"]}}});
        std::fs::write(&ff, user.to_string()).unwrap();
        let mut st = PolicyState::default();
        firefox_apply(&ff, &spec(&["a.com"]), &mut st, false).unwrap();
        firefox_apply(&ff, &PolicySpec::default(), &mut st, false).unwrap();
        assert_eq!(read(&ff), user);
    }

    #[test]
    fn firefox_allowlist_mode() {
        let root = tempfile::tempdir().unwrap();
        let ff = root.path().join("p.json");
        let mut st = PolicyState::default();
        firefox_apply(&ff, &allow_spec(&["wikipedia.org"]), &mut st, false).unwrap();
        let v = read(&ff);
        assert_eq!(v["policies"]["WebsiteFilter"]["Block"], json!(["<all_urls>"]));
        assert_eq!(v["policies"]["WebsiteFilter"]["Exceptions"], json!(["*://*.wikipedia.org/*"]));
    }

    #[test]
    fn firefox_state_survives_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let ff = root.path().join("p.json");
        let sp = root.path().join("state.json");
        let user = json!({"policies": {"DisableTelemetry": true}});
        std::fs::write(&ff, user.to_string()).unwrap();
        let mut st = PolicyState::default();
        firefox_apply(&ff, &spec(&["a.com"]), &mut st, false).unwrap();
        st.save(&sp).unwrap();
        // "daemon restart": fresh state loaded from disk
        let mut st2 = PolicyState::load(&sp);
        firefox_apply(&ff, &PolicySpec::default(), &mut st2, false).unwrap();
        assert_eq!(read(&ff), user);
    }

    #[test]
    fn firefox_refuses_to_touch_invalid_json() {
        let root = tempfile::tempdir().unwrap();
        let ff = root.path().join("p.json");
        std::fs::write(&ff, "{ not json").unwrap();
        let mut st = PolicyState::default();
        assert!(firefox_apply(&ff, &spec(&["a.com"]), &mut st, false).is_err());
        assert_eq!(std::fs::read_to_string(&ff).unwrap(), "{ not json");
    }

    #[test]
    fn chromium_writes_dedicated_file_and_folds_user_lists() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("managed");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("corp.json"), json!({"URLBlocklist": ["corp-blocked.com"], "HomepageLocation": "x"}).to_string()).unwrap();
        let mut st = PolicyState::default();
        chromium_apply(&dir, &spec(&["a.com", "b.com"]), &mut st, false).unwrap();
        let v = read(&dir.join(CHROMIUM_FILE));
        assert_eq!(v["URLBlocklist"], json!(["corp-blocked.com", "a.com", "b.com"]));
        assert_eq!(v["DnsOverHttpsMode"], json!("off"));
        assert_eq!(v[MARKER_KEY], json!(true));
        assert!(dir.join("corp.json").exists());
        chromium_apply(&dir, &PolicySpec::default(), &mut st, false).unwrap();
        assert!(!dir.join(CHROMIUM_FILE).exists());
        assert!(dir.join("corp.json").exists(), "foreign file untouched");
    }

    #[test]
    fn chromium_allowlist_and_cleanup_of_created_dirs() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("etc/chromium/policies/managed");
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        let mut st = PolicyState::default();
        chromium_apply(&dir, &allow_spec(&["school.edu"]), &mut st, false).unwrap();
        let v = read(&dir.join(CHROMIUM_FILE));
        assert_eq!(v["URLBlocklist"], json!(["*"]));
        assert!(v["URLAllowlist"].as_array().unwrap().contains(&json!("school.edu")));
        chromium_apply(&dir, &PolicySpec::default(), &mut st, false).unwrap();
        assert!(!root.path().join("etc/chromium").exists());
    }

    #[test]
    fn chromium_never_deletes_foreign_file_with_our_name() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("m");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CHROMIUM_FILE), "{\"mine\":1}").unwrap();
        let mut st = PolicyState::default();
        chromium_apply(&dir, &PolicySpec::default(), &mut st, false).unwrap();
        assert!(dir.join(CHROMIUM_FILE).exists());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let p = Paths::under(root.path());
        let problems = apply_all(&p, &spec(&["a.com"]), true);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(!root.path().join("etc").exists() && !root.path().join("state").exists());
    }

    #[test]
    fn apply_all_and_clear_all_roundtrip_leaves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let p = Paths::under(root.path());
        assert!(apply_all(&p, &spec(&["a.com"]), false).is_empty());
        assert!(is_applied(&p));
        for f in &p.firefox_policy_files {
            assert!(f.exists());
        }
        assert!(clear_all(&p, false).is_empty());
        assert!(!is_applied(&p));
        let remaining: Vec<_> = walk(root.path());
        assert!(remaining.is_empty(), "left behind: {remaining:?}");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    // ---- registry logic with a fake registry ----
    #[derive(Default)]
    struct Fake(BTreeMap<String, BTreeMap<String, String>>);
    impl Registry for Fake {
        fn values(&self, key: &str) -> Vec<(String, String)> {
            self.0.get(key).map(|m| m.iter().map(|(a, b)| (a.clone(), b.clone())).collect()).unwrap_or_default()
        }
        fn get(&self, key: &str, name: &str) -> Option<String> {
            self.0.get(key)?.get(name).cloned()
        }
        fn set(&mut self, key: &str, name: &str, value: &str) -> std::io::Result<()> {
            self.0.entry(key.into()).or_default().insert(name.into(), value.into());
            Ok(())
        }
        fn delete_value(&mut self, key: &str, name: &str) -> std::io::Result<()> {
            self.0.get_mut(key).and_then(|m| m.remove(name)).map(|_| ()).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
        fn delete_key_if_empty(&mut self, key: &str) {
            if self.0.get(key).is_some_and(|m| m.is_empty()) {
                self.0.remove(key);
            }
        }
    }

    const ROOT: &str = r"SOFTWARE\Policies\Google\Chrome";

    #[test]
    fn registry_merge_and_restore() {
        let mut reg = Fake::default();
        reg.set(&format!(r"{ROOT}\URLBlocklist"), "1", "corp.com").unwrap();
        reg.set(ROOT, "DnsOverHttpsMode", "secure").unwrap();
        reg.set(ROOT, "HomepageLocation", "http://x").unwrap();
        let before = reg.0.clone();
        let mut st = PolicyState::default();
        registry_apply(&mut reg, ROOT, &spec(&["a.com", "corp.com"]), &mut st).unwrap();
        let vals = reg.values(&format!(r"{ROOT}\URLBlocklist"));
        assert_eq!(vals.len(), 2, "corp.com not duplicated: {vals:?}");
        assert!(vals.contains(&("2".into(), "a.com".into())));
        assert_eq!(reg.get(ROOT, "DnsOverHttpsMode").as_deref(), Some("off"));
        // re-apply with different spec replaces our values only
        registry_apply(&mut reg, ROOT, &spec(&["b.com"]), &mut st).unwrap();
        assert!(!reg.values(&format!(r"{ROOT}\URLBlocklist")).iter().any(|(_, v)| v == "a.com"));
        registry_apply(&mut reg, ROOT, &PolicySpec::default(), &mut st).unwrap();
        assert_eq!(reg.0, before);
        assert!(st.registry.is_empty());
    }

    #[test]
    fn registry_created_keys_are_removed() {
        let mut reg = Fake::default();
        let mut st = PolicyState::default();
        registry_apply(&mut reg, ROOT, &allow_spec(&["school.edu"]), &mut st).unwrap();
        assert_eq!(reg.get(&format!(r"{ROOT}\URLBlocklist"), "1").as_deref(), Some("*"));
        registry_apply(&mut reg, ROOT, &PolicySpec::default(), &mut st).unwrap();
        assert!(reg.0.is_empty() || reg.0.values().all(|m| m.is_empty()), "{:?}", reg.0);
    }
}
