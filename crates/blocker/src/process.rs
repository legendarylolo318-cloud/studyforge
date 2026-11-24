//! App / process blocker.
//!
//! All OS access goes through [`ProcessSource`], so the matching and safety
//! logic is tested with a fake process table. Only processes owned by the
//! allowed users are ever candidates; protected/system processes never are.

use crate::steam;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use studyforge_core::models::EnforceMode;
use studyforge_core::protected::{self, Candidate};

#[derive(Debug, Clone, Default)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub cmd0: Option<String>,
    /// uid (Unix) or SID (Windows) as text.
    pub owner: Option<String>,
    pub environ: Vec<String>,
    pub start_time: u64,
}

pub trait ProcessSource {
    fn snapshot(&mut self, want_env: bool) -> Vec<ProcInfo>;
    /// `force` = SIGKILL / TerminateProcess; otherwise a polite SIGTERM where available.
    fn terminate(&mut self, pid: u32, force: bool) -> bool;
    /// Same process still running (pid + start time match)?
    fn same_process_alive(&mut self, pid: u32, start_time: u64) -> bool;
}

#[derive(Debug, Clone, Default)]
pub struct AppRules {
    /// (normalized process name, list name, mode)
    pub processes: Vec<(String, String, EnforceMode)>,
    pub steam: Option<(String, EnforceMode)>,
    pub block_steam_client: bool,
    pub allow_processes: Vec<String>,
    pub allowed_owners: Vec<String>,
    pub steam_commons: Vec<PathBuf>,
}

impl AppRules {
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty() && self.steam.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    Process,
    SteamGame,
    SteamClient,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub owner: Option<String>,
    pub start_time: u64,
    pub list: String,
    pub mode: EnforceMode,
    pub kind: ViolationKind,
}

impl Violation {
    /// Human-friendly target for logs / overlay.
    pub fn display_name(&self) -> String {
        match self.kind {
            ViolationKind::SteamGame => format!("{} (Steam game)", self.name),
            _ => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Terminate the process.
    Kill,
    /// Do nothing to the process (the caller shows an overlay / notification).
    Notify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Killed,
    /// SIGTERM sent; will be escalated if still alive on the next scan.
    Terminating,
    Notified,
    Gone,
    Protected,
    DryRun,
    Failed(String),
}

pub trait AppBlocker {
    fn set_rules(&mut self, rules: AppRules);
    fn scan(&mut self) -> Vec<Violation>;
    fn enforce(&mut self, v: &Violation, action: Action) -> Outcome;
}

pub struct ProcessBlocker<S: ProcessSource> {
    source: S,
    rules: AppRules,
    self_pid: u32,
    dry_run: bool,
    /// pid -> when a polite terminate was sent
    termed: HashMap<u32, Instant>,
}

fn stem(name: &str) -> String {
    protected::normalize(name)
}

impl<S: ProcessSource> ProcessBlocker<S> {
    pub fn new(source: S, dry_run: bool) -> Self {
        Self { source, rules: AppRules::default(), self_pid: std::process::id(), dry_run, termed: HashMap::new() }
    }

    pub fn rules(&self) -> &AppRules {
        &self.rules
    }

    /// Candidate names for a process: comm name, exe file stem, argv[0] basename.
    fn names_of(p: &ProcInfo) -> Vec<String> {
        let mut v = vec![stem(&p.name)];
        if let Some(e) = &p.exe {
            if let Some(f) = e.file_name().and_then(|f| f.to_str()) {
                v.push(stem(f));
            }
        }
        if let Some(c) = &p.cmd0 {
            let base = c.rsplit(['/', '\\']).next().unwrap_or(c);
            v.push(stem(base));
        }
        v.retain(|s| !s.is_empty());
        v.dedup();
        v
    }

    fn matches_name(candidates: &[String], raw_comm: &str, rule: &str) -> bool {
        if candidates.iter().any(|c| c == rule) {
            return true;
        }
        // Linux `comm` is truncated to 15 bytes.
        raw_comm.len() == 15 && rule.len() > 15 && rule.starts_with(&stem(raw_comm))
    }

    fn classify(&self, p: &ProcInfo) -> Option<Violation> {
        let names = Self::names_of(p);
        let mk = |list: &str, mode, kind| Violation {
            pid: p.pid,
            name: stem(&p.name),
            exe: p.exe.clone(),
            owner: p.owner.clone(),
            start_time: p.start_time,
            list: list.to_string(),
            mode,
            kind,
        };
        // Steam client: only with the explicit option.
        if names.iter().any(|n| protected::is_steam_client(n)) {
            if self.rules.block_steam_client {
                if let Some((list, mode)) = &self.rules.steam {
                    return Some(mk(list, *mode, ViolationKind::SteamClient));
                }
            }
            return None;
        }
        if names.iter().any(|n| self.rules.allow_processes.contains(n)) {
            return None;
        }
        for (rule, list, mode) in &self.rules.processes {
            if Self::matches_name(&names, &p.name, rule) {
                return Some(mk(list, *mode, ViolationKind::Process));
            }
        }
        if let Some((list, mode)) = &self.rules.steam {
            let by_env = steam::env_says_steam_game(&p.environ);
            let by_path = p.exe.as_deref().is_some_and(|e| steam::is_under_common(e, &self.rules.steam_commons))
                || p.cwd.as_deref().is_some_and(|c| steam::is_under_common(c, &self.rules.steam_commons));
            if by_env || by_path {
                return Some(mk(list, *mode, ViolationKind::SteamGame));
            }
        }
        None
    }
}

impl<S: ProcessSource> AppBlocker for ProcessBlocker<S> {
    fn set_rules(&mut self, rules: AppRules) {
        self.rules = rules;
    }

    fn scan(&mut self) -> Vec<Violation> {
        if self.rules.is_empty() {
            self.termed.clear();
            return vec![];
        }
        let snap = self.source.snapshot(self.rules.steam.is_some());
        let mut out = Vec::new();
        for p in &snap {
            let cand = Candidate { pid: p.pid, name: &p.name, owner: p.owner.as_deref(), self_pid: self.self_pid };
            // Steam client names are "protected" by default; classify() decides, and
            // we re-check ownership here. Protected-list names (other than steam client) never pass.
            let steam_client = protected::is_steam_client(&p.name);
            if !steam_client && !protected::may_kill(&cand, &self.rules.allowed_owners) {
                continue;
            }
            if steam_client {
                let owner_ok = p.owner.as_deref().is_some_and(|o| !protected::is_system_owner(o) && self.rules.allowed_owners.iter().any(|a| a == o));
                if !owner_ok || p.pid <= 1 || p.pid == self.self_pid {
                    continue;
                }
            }
            if let Some(v) = self.classify(p) {
                out.push(v);
            }
        }
        let alive: Vec<u32> = snap.iter().map(|p| p.pid).collect();
        self.termed.retain(|pid, _| alive.contains(pid));
        out
    }

    fn enforce(&mut self, v: &Violation, action: Action) -> Outcome {
        if action == Action::Notify {
            return Outcome::Notified;
        }
        // Re-verify against the safety rules at the moment of the kill.
        let cand = Candidate { pid: v.pid, name: &v.name, owner: v.owner.as_deref(), self_pid: self.self_pid };
        let steam_client = v.kind == ViolationKind::SteamClient;
        let allowed = if steam_client {
            v.owner.as_deref().is_some_and(|o| !protected::is_system_owner(o) && self.rules.allowed_owners.iter().any(|a| a == o)) && v.pid > 1 && v.pid != self.self_pid
        } else {
            protected::may_kill(&cand, &self.rules.allowed_owners)
        };
        if !allowed {
            return Outcome::Protected;
        }
        if self.dry_run {
            tracing::info!("[dry-run] would terminate {} (pid {})", v.name, v.pid);
            return Outcome::DryRun;
        }
        if !self.source.same_process_alive(v.pid, v.start_time) {
            return Outcome::Gone;
        }
        let escalate = self.termed.get(&v.pid).is_some_and(|t| t.elapsed() >= Duration::from_millis(1200));
        if escalate || !cfg!(unix) {
            return if self.source.terminate(v.pid, true) { Outcome::Killed } else { Outcome::Failed("kill failed".into()) };
        }
        if self.termed.contains_key(&v.pid) {
            return Outcome::Terminating; // wait a bit before escalating
        }
        self.termed.insert(v.pid, Instant::now());
        if self.source.terminate(v.pid, false) {
            Outcome::Terminating
        } else {
            Outcome::Failed("terminate failed".into())
        }
    }
}

// ---------------------------------------------------------------------------
// sysinfo-backed source
// ---------------------------------------------------------------------------

pub struct SysinfoSource {
    sys: sysinfo::System,
}

impl SysinfoSource {
    pub fn new() -> Result<Self, sysinfo::Error> {
        Ok(Self { sys: sysinfo::System::new()? })
    }
}

impl ProcessSource for SysinfoSource {
    fn snapshot(&mut self, want_env: bool) -> Vec<ProcInfo> {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
        let mut kind = ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_user(UpdateKind::OnlyIfNotSet);
        if want_env {
            kind = kind.with_cwd(UpdateKind::OnlyIfNotSet).with_environ(UpdateKind::Always);
        }
        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        self.sys
            .processes()
            .values()
            .map(|p| ProcInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                exe: p.exe().map(|e| e.to_path_buf()),
                cwd: p.cwd().map(|c| c.to_path_buf()),
                cmd0: p.cmd().first().map(|c| c.to_string_lossy().into_owned()),
                owner: p.user_id().map(|u| u.to_string()),
                environ: if want_env { p.environ().iter().map(|e| e.to_string_lossy().into_owned()).collect() } else { vec![] },
                start_time: p.start_time(),
            })
            .collect()
    }

    fn terminate(&mut self, pid: u32, force: bool) -> bool {
        let Some(p) = self.sys.process(sysinfo::Pid::from_u32(pid)) else { return false };
        if force || !cfg!(unix) {
            p.kill()
        } else {
            p.kill_with(sysinfo::Signal::Term).unwrap_or_else(|| p.kill())
        }
    }

    fn same_process_alive(&mut self, pid: u32, start_time: u64) -> bool {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
        let pid = sysinfo::Pid::from_u32(pid);
        self.sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, ProcessRefreshKind::nothing());
        self.sys.process(pid).is_some_and(|p| p.start_time() == start_time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Table {
        procs: Vec<ProcInfo>,
        killed: Vec<(u32, bool)>,
    }
    struct Fake(Rc<RefCell<Table>>);
    impl ProcessSource for Fake {
        fn snapshot(&mut self, _env: bool) -> Vec<ProcInfo> {
            self.0.borrow().procs.clone()
        }
        fn terminate(&mut self, pid: u32, force: bool) -> bool {
            self.0.borrow_mut().killed.push((pid, force));
            true
        }
        fn same_process_alive(&mut self, pid: u32, start: u64) -> bool {
            self.0.borrow().procs.iter().any(|p| p.pid == pid && p.start_time == start)
        }
    }

    fn proc(pid: u32, name: &str, owner: &str) -> ProcInfo {
        ProcInfo { pid, name: name.into(), owner: Some(owner.into()), start_time: 100 + pid as u64, ..Default::default() }
    }

    fn setup(procs: Vec<ProcInfo>, rules: AppRules) -> (ProcessBlocker<Fake>, Rc<RefCell<Table>>) {
        let t = Rc::new(RefCell::new(Table { procs, killed: vec![] }));
        let mut b = ProcessBlocker::new(Fake(t.clone()), false);
        b.set_rules(rules);
        (b, t)
    }

    fn rules(names: &[&str]) -> AppRules {
        AppRules {
            processes: names.iter().map(|n| (n.to_string(), "Social".to_string(), EnforceMode::Hard)).collect(),
            allowed_owners: vec!["1000".into()],
            ..Default::default()
        }
    }

    #[test]
    fn matches_case_insensitive_and_exe_suffix() {
        let procs = vec![proc(10, "Discord", "1000"), proc(11, "DISCORD.EXE", "1000"), proc(12, "firefox", "1000")];
        let (mut b, _) = setup(procs, rules(&["discord"]));
        let v = b.scan();
        assert_eq!(v.iter().map(|v| v.pid).collect::<Vec<_>>(), vec![10, 11]);
    }

    #[test]
    fn matches_by_exe_stem_and_cmdline_and_truncated_comm() {
        let mut a = proc(10, "electron", "1000");
        a.exe = Some(PathBuf::from("/opt/Slack/slack"));
        let mut b2 = proc(11, "MainThread", "1000");
        b2.cmd0 = Some("/usr/lib/telegram-desktop/Telegram".into());
        let c = proc(12, "telegram-deskto", "1000"); // 15-char comm truncation
        let (mut b, _) = setup(vec![a, b2, c], rules(&["slack", "telegram", "telegram-desktop"]));
        let v = b.scan();
        assert_eq!(v.len(), 3, "{v:?}");
    }

    #[test]
    fn only_allowed_owners_never_root_or_others() {
        let procs = vec![proc(10, "discord", "1000"), proc(11, "discord", "1001"), proc(12, "discord", "0")];
        let (mut b, _) = setup(procs, rules(&["discord"]));
        assert_eq!(b.scan().iter().map(|v| v.pid).collect::<Vec<_>>(), vec![10]);
    }

    #[test]
    fn protected_names_are_never_candidates_even_if_listed() {
        let procs = vec![proc(10, "Xorg", "1000"), proc(11, "bash", "1000"), proc(12, "kwin_wayland", "1000"), proc(13, "studyd", "1000"), proc(14, "explorer.exe", "1000")];
        let (mut b, _) = setup(procs, rules(&["xorg", "bash", "kwin_wayland", "studyd", "explorer"]));
        assert!(b.scan().is_empty());
    }

    #[test]
    fn allowlist_exempts() {
        let mut r = rules(&["discord"]);
        r.allow_processes = vec!["discord".into()];
        let (mut b, _) = setup(vec![proc(10, "discord", "1000")], r);
        assert!(b.scan().is_empty());
    }

    #[test]
    fn steam_games_by_env_and_path_but_not_client() {
        let mut game = proc(20, "hl2_linux", "1000");
        game.environ = vec!["SteamAppId=220".into()];
        let mut proton = proc(21, "wine64-preloader", "1000");
        proton.exe = Some(PathBuf::from("/mnt/g/steamapps/common/Proton 9/files/bin/wine64-preloader"));
        let client = proc(22, "steam", "1000");
        let web = proc(23, "steamwebhelper", "1000");
        let unrelated = proc(24, "firefox", "1000");
        let r = AppRules {
            steam: Some(("Gaming".into(), EnforceMode::Hard)),
            allowed_owners: vec!["1000".into()],
            steam_commons: vec![PathBuf::from("/mnt/g/steamapps/common")],
            ..Default::default()
        };
        let (mut b, _) = setup(vec![game.clone(), proton.clone(), client.clone(), web.clone(), unrelated.clone()], r.clone());
        let v = b.scan();
        assert_eq!(v.iter().map(|v| v.pid).collect::<Vec<_>>(), vec![20, 21]);
        assert!(v.iter().all(|v| v.kind == ViolationKind::SteamGame));
        // with "block steam client too"
        let mut r2 = r;
        r2.block_steam_client = true;
        let (mut b, _) = setup(vec![game, proton, client, web, unrelated], r2);
        let v = b.scan();
        assert_eq!(v.iter().map(|v| v.pid).collect::<Vec<_>>(), vec![20, 21, 22, 23]);
        assert_eq!(v[2].kind, ViolationKind::SteamClient);
    }

    #[test]
    fn steam_client_is_not_killed_when_owned_by_someone_else() {
        let r = AppRules { steam: Some(("G".into(), EnforceMode::Hard)), block_steam_client: true, allowed_owners: vec!["1000".into()], ..Default::default() };
        let (mut b, _) = setup(vec![proc(5, "steam", "1001"), proc(6, "steam", "0")], r);
        assert!(b.scan().is_empty());
    }

    #[test]
    fn enforce_escalates_term_then_kill_and_verifies_identity() {
        let (mut b, t) = setup(vec![proc(10, "discord", "1000")], rules(&["discord"]));
        let v = b.scan().remove(0);
        assert_eq!(b.enforce(&v, Action::Notify), Outcome::Notified);
        assert!(t.borrow().killed.is_empty());
        if cfg!(unix) {
            assert_eq!(b.enforce(&v, Action::Kill), Outcome::Terminating);
            assert_eq!(b.enforce(&v, Action::Kill), Outcome::Terminating, "waits before escalating");
            b.termed.insert(10, Instant::now() - Duration::from_secs(5));
            assert_eq!(b.enforce(&v, Action::Kill), Outcome::Killed);
            assert_eq!(t.borrow().killed, vec![(10, false), (10, true)]);
        }
        // pid reuse: start time changed -> Gone, never killed
        let mut stale = v.clone();
        stale.start_time += 1;
        t.borrow_mut().killed.clear();
        b.termed.clear();
        assert_eq!(b.enforce(&stale, Action::Kill), Outcome::Gone);
        assert!(t.borrow().killed.is_empty());
    }

    #[test]
    fn enforce_refuses_protected_even_if_handed_a_forged_violation() {
        let (mut b, t) = setup(vec![proc(1, "systemd", "1000"), proc(10, "discord", "0")], rules(&["discord"]));
        for (pid, name, owner) in [(1u32, "systemd", "1000"), (10, "discord", "0"), (11, "bash", "1000")] {
            let v = Violation { pid, name: name.into(), exe: None, owner: Some(owner.into()), start_time: 0, list: "x".into(), mode: EnforceMode::Hard, kind: ViolationKind::Process };
            assert_eq!(b.enforce(&v, Action::Kill), Outcome::Protected);
        }
        assert!(t.borrow().killed.is_empty());
    }

    #[test]
    fn dry_run_never_kills() {
        let t = Rc::new(RefCell::new(Table { procs: vec![proc(10, "discord", "1000")], killed: vec![] }));
        let mut b = ProcessBlocker::new(Fake(t.clone()), true);
        b.set_rules(rules(&["discord"]));
        let v = b.scan().remove(0);
        assert_eq!(b.enforce(&v, Action::Kill), Outcome::DryRun);
        assert!(t.borrow().killed.is_empty());
    }

    #[test]
    fn empty_rules_scan_nothing() {
        let (mut b, _) = setup(vec![proc(10, "discord", "1000")], AppRules::default());
        assert!(b.scan().is_empty());
    }

    #[test]
    fn real_source_snapshot_sees_this_process() {
        let mut s = SysinfoSource::new().unwrap();
        let me = std::process::id();
        let snap = s.snapshot(false);
        assert!(snap.iter().any(|p| p.pid == me));
        let mine = snap.iter().find(|p| p.pid == me).unwrap();
        assert!(s.same_process_alive(me, mine.start_time));
        assert!(!s.same_process_alive(me, mine.start_time + 12345));
    }
}
