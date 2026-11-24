//! The brain of studyd. Pure with respect to time (always injected) and with
//! the OS reached only through traits, so every behaviour is unit tested.

use crate::state::{ActiveSession, Persisted};
use chrono::{DateTime, FixedOffset, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use studyforge_blocker::process::{Action, AppBlocker, AppRules, Outcome, Violation};
use studyforge_blocker::sites::SiteBlocker;
use studyforge_core::blocking::BlockSpec;
use studyforge_core::hardmode::{ExitDecision, HardModeConfig, HardModeState};
use studyforge_core::ipc::*;
use studyforge_core::models::{DistractionKind, EnforceMode};
use studyforge_core::schedule::{self, ScheduleRule};

/// Soft (non-hard) sessions end if no GUI has talked to us for this long.
pub const SOFT_LEASE_SECS: i64 = 600;
/// Longest hard-mode lock we accept (nobody gets locked out for days).
pub const MAX_HARD_SECS: i64 = 24 * 3600;
/// Re-apply blocks this often to heal manual tampering.
pub const HEAL_INTERVAL_SECS: i64 = 30;
/// GUI counts as running if it polled within this many seconds.
pub const GUI_ALIVE_SECS: i64 = 10;
pub const SNOOZE_SECS: i64 = 300;
/// Re-announce an unanswered overlay after this long.
pub const OVERLAY_REPEAT_SECS: i64 = 120;
/// A disagreement between the wall clock and the boot clock larger than this
/// is treated as the system clock being changed.
pub const CLOCK_JUMP_SECS: i64 = 90;
/// How often the "last seen" wall time is persisted (for clock changes while
/// the daemon is not running).
pub const LAST_SEEN_SAVE_SECS: i64 = 60;

/// Seconds since boot, *including* time spent suspended. Used to tell a real
/// clock change (shift the deadline) from time actually passing (keep it).
pub fn boot_clock() -> std::time::Duration {
    #[cfg(target_os = "linux")]
    {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid pointer to a timespec; CLOCK_BOOTTIME exists on every supported kernel.
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut t) } == 0 {
            return std::time::Duration::new(t.tv_sec as u64, t.tv_nsec as u32);
        }
    }
    #[cfg(windows)]
    {
        // GetTickCount64 keeps counting while the machine sleeps.
        // SAFETY: no arguments, cannot fail.
        return std::time::Duration::from_millis(unsafe {
            windows_sys::Win32::System::SystemInformation::GetTickCount64()
        });
    }
    #[allow(unreachable_code)]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }
}

pub trait Notifier {
    fn notify(&self, summary: &str, body: &str);
}

#[derive(Debug, Clone)]
enum Gentle {
    Pending { id: u64, since: i64 },
    Snoozed { until: i64 },
    Dismissed,
}

struct GentleEntry {
    start_time: u64,
    state: Gentle,
    violation: Violation,
}

pub struct EngineConfig {
    pub state_path: PathBuf,
    /// If this file exists the emergency unlock is executed (written by `studyd emergency-unlock`).
    pub emergency_flag: PathBuf,
    pub dry_run: bool,
    pub allowed_owners: Vec<String>,
    /// Only honour the flag file when the state directory is writable by
    /// administrators alone (otherwise any user could skip the cooldown).
    pub honor_emergency_flag: bool,
}

pub type LocalTime = Box<dyn Fn(DateTime<Utc>) -> DateTime<FixedOffset> + Send>;

pub struct Engine<S: SiteBlocker, A: AppBlocker, N: Notifier> {
    pub cfg: EngineConfig,
    pub state: Persisted,
    sites: S,
    apps: A,
    notifier: N,
    local: LocalTime,
    steam_commons: Box<dyn Fn() -> Vec<PathBuf> + Send>,
    applied: Option<BlockSpec>,
    last_heal: i64,
    last_poll: i64,
    last_report: studyforge_blocker::sites::SiteReport,
    schedules_active: Vec<String>,
    gentle: HashMap<u32, GentleEntry>,
    killed_logged: HashSet<(u32, u64)>,
    commons_cache: (i64, Vec<PathBuf>),
    /// (wall seconds, boot clock) at the previous tick.
    clock_ref: Option<(i64, std::time::Duration)>,
}

impl<S: SiteBlocker, A: AppBlocker, N: Notifier> Engine<S, A, N> {
    pub fn new(
        cfg: EngineConfig,
        sites: S,
        apps: A,
        notifier: N,
        local: LocalTime,
        steam_commons: Box<dyn Fn() -> Vec<PathBuf> + Send>,
    ) -> Self {
        let state = Persisted::load(&cfg.state_path);
        Engine {
            cfg,
            state,
            sites,
            apps,
            notifier,
            local,
            steam_commons,
            applied: None,
            last_heal: 0,
            last_poll: 0,
            last_report: Default::default(),
            schedules_active: vec![],
            gentle: HashMap::new(),
            killed_logged: HashSet::new(),
            commons_cache: (0, vec![]),
            clock_ref: None,
        }
    }

    fn save(&self) {
        if self.cfg.dry_run {
            return;
        }
        if let Err(e) = self.state.save(&self.cfg.state_path) {
            tracing::error!("cannot persist state: {e}");
        }
    }

    fn next_id(&mut self) -> u64 {
        self.state.next_id += 1;
        self.state.next_id
    }

    fn notice(&mut self, now: i64, message: impl Into<String>) {
        let message = message.into();
        tracing::info!("{message}");
        self.state.push_event(DaemonEvent::Notice { ts: now, message });
    }

    // ------------------------------------------------------------------
    // startup / crash recovery
    // ------------------------------------------------------------------
    /// Called once at start: end expired sessions, then make the system match
    /// what *should* be active (this removes leftovers from a crash).
    pub fn reconcile_on_start(&mut self, now: DateTime<Utc>) {
        let ts = now.timestamp();
        // a restart counts as contact so soft sessions don't lapse immediately
        if let Some(a) = &mut self.state.active {
            a.last_contact = ts;
        }
        // The clock went backwards while we were not running (e.g. RTC reset,
        // manual change before reboot): keep the remaining time instead of
        // extending the lock by the jump. A forward jump cannot be told apart
        // from time really passing, so it is honoured.
        let seen = self.state.last_seen;
        if seen > 0 && ts < seen - CLOCK_JUMP_SECS {
            self.shift_times(ts - seen);
            self.notice(ts, "System clock is earlier than when studyd last ran: hard-mode times adjusted");
        }
        self.clamp_lock(ts);
        self.state.last_seen = ts;
        self.check_emergency_flag(ts);
        self.expire_session(ts);
        let report = self.sites.clear(); // always clean first
        self.last_report = report;
        self.applied = None;
        self.apply_desired(now, true);
        self.save();
    }

    // ------------------------------------------------------------------
    // periodic work (call about every 2 seconds)
    // ------------------------------------------------------------------
    pub fn tick(&mut self, now: DateTime<Utc>) {
        self.tick_with_clock(now, None);
    }

    /// `boot` = [`boot_clock`] reading (None disables clock-change detection).
    pub fn tick_with_clock(&mut self, now: DateTime<Utc>, boot: Option<std::time::Duration>) {
        let ts = now.timestamp();
        if let Some(b) = boot {
            self.detect_clock_change(ts, b);
        }
        if ts - self.state.last_seen >= LAST_SEEN_SAVE_SECS || ts < self.state.last_seen {
            self.state.last_seen = ts;
            self.save();
        }
        self.check_emergency_flag(ts);
        self.expire_session(ts);
        self.apply_desired(now, false);
        self.enforce_apps(ts);
    }

    fn detect_clock_change(&mut self, ts: i64, boot: std::time::Duration) {
        if let Some((w0, b0)) = self.clock_ref {
            let elapsed = boot.saturating_sub(b0).as_secs() as i64;
            let drift = (ts - w0) - elapsed;
            if drift.abs() > CLOCK_JUMP_SECS {
                self.shift_times(drift);
                self.clamp_lock(ts);
                self.notice(ts, format!("System clock changed by {drift} s: hard-mode deadline adjusted so the remaining time stays the same"));
                self.save();
            }
        }
        self.clock_ref = Some((ts, boot));
    }

    /// Move every absolute timestamp of the session by `delta` seconds.
    fn shift_times(&mut self, delta: i64) {
        if let Some(a) = &mut self.state.active {
            a.started_at += delta;
            a.last_contact += delta;
            if let Some(h) = &mut a.hard {
                h.deadline += delta;
            }
            for t in
                [&mut a.hard_state.exit_requested_at, &mut a.hard_state.emergency_requested_at].into_iter().flatten()
            {
                *t += delta;
            }
        }
    }

    /// Never more than `MAX_HARD_SECS` of lock left, whatever happened to the clock.
    fn clamp_lock(&mut self, ts: i64) {
        if let Some(h) = self.state.active.as_mut().and_then(|a| a.hard.as_mut()) {
            h.deadline = h.deadline.min(ts + MAX_HARD_SECS);
        }
    }

    fn check_emergency_flag(&mut self, ts: i64) {
        if self.cfg.emergency_flag.exists() && !self.cfg.honor_emergency_flag {
            tracing::warn!("ignoring emergency-unlock flag: the state directory is not admin-only");
            let _ = std::fs::remove_file(&self.cfg.emergency_flag);
            return;
        }
        if self.cfg.emergency_flag.exists() {
            let _ = std::fs::remove_file(&self.cfg.emergency_flag);
            let had_session = self.state.active.take().is_some();
            let had_rules = !self.state.schedules.is_empty();
            // The emergency escape lifts everything, scheduled rules included.
            self.state.schedules.clear();
            if had_session || had_rules {
                self.notice(ts, "Emergency unlock requested from the command line: all blocking lifted");
            }
            self.save();
        }
    }

    fn expire_session(&mut self, ts: i64) {
        let Some(a) = &mut self.state.active else { return };
        let mut end: Option<&str> = None;
        match (&a.hard, ts) {
            (Some(h), now) => {
                if HardModeState::deadline_passed(h, now) {
                    end = Some("Hard-mode session finished");
                } else if a.hard_state.try_emergency(h, now) == ExitDecision::Allowed {
                    end = Some("Emergency unlock completed: session ended early");
                } else if a.hard_state.try_cooldown_exit(h, now) == ExitDecision::Allowed {
                    end = Some("Early-exit cooldown completed: session ended early");
                }
            }
            (None, now) => {
                if now - a.last_contact > SOFT_LEASE_SECS {
                    end = Some("Session ended: the app stopped responding");
                }
            }
        }
        if let Some(msg) = end {
            self.state.active = None;
            self.notice(ts, msg);
            self.save();
        }
    }

    fn desired(&mut self, now: DateTime<Utc>) -> Option<BlockSpec> {
        let local = (self.local)(now);
        let active: Vec<&ScheduleRule> = schedule::active_rules(&self.state.schedules, &local);
        self.schedules_active = active.iter().map(|r| r.name.clone()).collect();
        let mut specs: Vec<&BlockSpec> = vec![];
        if let Some(a) = &self.state.active {
            specs.push(&a.spec);
        }
        specs.extend(active.iter().map(|r| &r.spec));
        let specs: Vec<&BlockSpec> = specs.into_iter().filter(|s| !s.is_empty()).collect();
        if specs.is_empty() {
            None
        } else {
            Some(BlockSpec::merge(&specs))
        }
    }

    fn commons(&mut self, ts: i64) -> Vec<PathBuf> {
        if ts - self.commons_cache.0 > 60 || self.commons_cache.0 == 0 {
            self.commons_cache = (ts, (self.steam_commons)());
        }
        self.commons_cache.1.clone()
    }

    fn apply_desired(&mut self, now: DateTime<Utc>, force: bool) {
        let ts = now.timestamp();
        let want = self.desired(now);
        let heal_due = ts - self.last_heal >= HEAL_INTERVAL_SECS;
        if want == self.applied && !force && !(want.is_some() && heal_due) {
            return;
        }
        match &want {
            Some(spec) => {
                self.last_report = self.sites.apply(spec);
                let commons = self.commons(ts);
                self.apps.set_rules(AppRules {
                    processes: spec.process_rules(),
                    steam: spec.steam_rule(),
                    block_steam_client: spec.block_steam_client,
                    allow_processes: spec.allowlist_processes(),
                    allowed_owners: self.cfg.allowed_owners.clone(),
                    steam_commons: commons,
                });
            }
            None => {
                if self.applied.is_some() || force {
                    self.last_report = self.sites.clear();
                }
                self.apps.set_rules(AppRules::default());
                self.gentle.clear();
                self.killed_logged.clear();
            }
        }
        if want != self.applied {
            tracing::info!("blocking {}", if want.is_some() { "active" } else { "inactive" });
        }
        self.applied = want;
        self.last_heal = ts;
    }

    fn goal(&self) -> String {
        self.state.active.as_ref().map(|a| a.goal.clone()).unwrap_or_default()
    }

    fn gui_alive(&self, ts: i64) -> bool {
        ts - self.last_poll <= GUI_ALIVE_SECS
    }

    fn enforce_apps(&mut self, ts: i64) {
        if self.applied.is_none() {
            return;
        }
        let violations = self.apps.scan();
        let live: HashSet<u32> = violations.iter().map(|v| v.pid).collect();
        self.gentle.retain(|pid, _| live.contains(pid));
        self.killed_logged.retain(|(pid, _)| live.contains(pid));
        let goal = self.goal();
        let mut changed = false;
        for v in violations {
            match v.mode {
                EnforceMode::Hard => {
                    let outcome = self.apps.enforce(&v, Action::Kill);
                    let action = match &outcome {
                        Outcome::Killed | Outcome::Terminating => Some("killed"),
                        Outcome::DryRun => Some("would kill (dry run)"),
                        Outcome::Failed(e) => {
                            tracing::warn!("could not stop {} ({}): {e}", v.name, v.pid);
                            None
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        if self.killed_logged.insert((v.pid, v.start_time)) {
                            self.state.push_event(DaemonEvent::Distraction {
                                ts,
                                target: v.display_name(),
                                kind: DistractionKind::App,
                                list: v.list.clone(),
                                action: action.into(),
                            });
                            changed = true;
                        }
                    }
                }
                EnforceMode::Gentle => changed |= self.gentle_violation(v, ts, &goal),
            }
        }
        if changed {
            self.save();
        }
    }

    /// Returns true if state changed (events queued).
    fn gentle_violation(&mut self, v: Violation, ts: i64, goal: &str) -> bool {
        let announce = match self.gentle.get(&v.pid) {
            Some(e) if e.start_time == v.start_time => match e.state {
                Gentle::Pending { since, .. } => ts - since >= OVERLAY_REPEAT_SECS,
                Gentle::Snoozed { until } => ts >= until,
                Gentle::Dismissed => false,
            },
            _ => true,
        };
        if !announce {
            return false;
        }
        let id = self.next_id();
        let target = v.display_name();
        self.state.push_event(DaemonEvent::Overlay {
            id,
            ts,
            target: target.clone(),
            list: v.list.clone(),
            goal: goal.to_string(),
        });
        self.state.push_event(DaemonEvent::Distraction {
            ts,
            target: target.clone(),
            kind: DistractionKind::App,
            list: v.list.clone(),
            action: "overlay".into(),
        });
        if !self.gui_alive(ts) {
            let body = if goal.is_empty() {
                format!("{target} is on your block list.")
            } else {
                format!("You said you'd study: {goal}. Close {target}?")
            };
            self.notifier.notify("StudyForge", &body);
        }
        self.gentle.insert(
            v.pid,
            GentleEntry { start_time: v.start_time, state: Gentle::Pending { id, since: ts }, violation: v },
        );
        true
    }

    // ------------------------------------------------------------------
    // requests
    // ------------------------------------------------------------------
    pub fn handle(&mut self, req: Request, now: DateTime<Utc>) -> Response {
        let ts = now.timestamp();
        // any real request is "contact" for the soft-session lease
        if !matches!(req, Request::Hello { .. } | Request::Ping) {
            if let Some(a) = &mut self.state.active {
                a.last_contact = ts;
            }
        }
        match req {
            Request::Hello { protocol, .. } => {
                if protocol != PROTOCOL_VERSION {
                    Response::Error { message: format!("protocol mismatch: daemon speaks {PROTOCOL_VERSION}, client {protocol}. Update both StudyForge and studyd.") }
                } else {
                    Response::Hello { protocol: PROTOCOL_VERSION, version: env!("CARGO_PKG_VERSION").into() }
                }
            }
            Request::Ping => Response::Pong,
            Request::Status => Response::Status(self.status(ts)),
            Request::PollEvents => {
                self.last_poll = ts;
                let events = std::mem::take(&mut self.state.events);
                if !events.is_empty() {
                    self.save();
                }
                Response::Events { events }
            }
            Request::SetSchedules { rules } => {
                self.state.schedules = rules;
                self.save();
                self.apply_desired(now, false);
                Response::Ok
            }
            Request::StartSession(s) => self.start_session(s, now),
            Request::StopSession => {
                match &self.state.active {
                    Some(a) if a.hard.is_some() => Response::Error { message: "This session is locked (hard mode). It ends at the deadline, via the early-exit challenge, or the emergency unlock.".into() },
                    Some(_) => {
                        self.state.active = None;
                        self.save();
                        self.apply_desired(now, false);
                        Response::Ok
                    }
                    None => Response::Ok,
                }
            }
            Request::ExitChallenge { typed } => self.hard_action(now, |h, st| st.try_challenge(h, &typed)),
            Request::ExitCooldown { start } => {
                self.with_hard(ts, |_, st| {
                    if start {
                        st.request_exit_cooldown(ts)
                    } else {
                        st.cancel_exit_cooldown()
                    }
                })
            }
            Request::Emergency { start } => {
                self.with_hard(ts, |_, st| {
                    if start {
                        st.request_emergency(ts)
                    } else {
                        st.cancel_emergency()
                    }
                })
            }
            Request::OverlayAction { id, choice } => self.overlay_action(id, choice, now),
        }
    }

    fn start_session(&mut self, s: StartSession, now: DateTime<Utc>) -> Response {
        let ts = now.timestamp();
        if let Some(a) = &self.state.active {
            if a.hard.is_some() {
                return Response::Error {
                    message: "A hard-mode session is already running and cannot be replaced.".into(),
                };
            }
        }
        let hard = s.hard.map(|h| {
            // Never trust the client: re-normalise (emergency floor) and clamp the lock length.
            let deadline = h.deadline.clamp(ts + 60, ts + MAX_HARD_SECS);
            HardModeConfig::new(deadline, h.early_exit_challenge, h.exit_cooldown_mins, h.emergency_cooldown_mins)
        });
        let mut hard_state = HardModeState::default();
        if hard.as_ref().is_some_and(|h| h.early_exit_challenge) {
            hard_state.ensure_challenge();
        }
        self.state.active =
            Some(ActiveSession { goal: s.goal, started_at: ts, spec: s.spec, hard, hard_state, last_contact: ts });
        self.gentle.clear();
        self.save();
        self.apply_desired(now, false);
        Response::Ok
    }

    fn with_hard(&mut self, ts: i64, f: impl FnOnce(&HardModeConfig, &mut HardModeState)) -> Response {
        match &mut self.state.active {
            Some(ActiveSession { hard: Some(h), hard_state, .. }) => {
                f(h, hard_state);
                self.save();
                Response::Ok
            }
            _ => {
                let _ = ts;
                Response::Error { message: "No hard-mode session is active".into() }
            }
        }
    }

    fn hard_action(
        &mut self,
        now: DateTime<Utc>,
        f: impl FnOnce(&HardModeConfig, &mut HardModeState) -> ExitDecision,
    ) -> Response {
        let decision = match &mut self.state.active {
            Some(ActiveSession { hard: Some(h), hard_state, .. }) => f(h, hard_state),
            _ => return Response::Error { message: "No hard-mode session is active".into() },
        };
        match decision {
            ExitDecision::Allowed => {
                self.state.active = None;
                self.notice(now.timestamp(), "Hard-mode session ended early (challenge completed)");
                self.save();
                self.apply_desired(now, false);
                Response::Ok
            }
            ExitDecision::Denied { reason } => {
                self.save();
                Response::Error { message: reason }
            }
        }
    }

    fn overlay_action(&mut self, id: u64, choice: OverlayChoice, now: DateTime<Utc>) -> Response {
        let ts = now.timestamp();
        let Some(pid) = self
            .gentle
            .iter()
            .find(|(_, e)| matches!(e.state, Gentle::Pending { id: i, .. } if i == id))
            .map(|(p, _)| *p)
        else {
            return Response::Ok; // stale overlay: nothing to do
        };
        let entry = self.gentle.get_mut(&pid).unwrap();
        let v = entry.violation.clone();
        let action = match choice {
            OverlayChoice::CloseIt => {
                // Escalates through repeated scans like a hard kill.
                let outcome = self.apps.enforce(&v, Action::Kill);
                entry.state = Gentle::Dismissed;
                match outcome {
                    Outcome::Killed | Outcome::Terminating | Outcome::Gone => "closed (your choice)",
                    Outcome::DryRun => "would close (dry run)",
                    _ => "close failed",
                }
            }
            OverlayChoice::FiveMore => {
                entry.state = Gentle::Snoozed { until: ts + SNOOZE_SECS };
                "snoozed 5 min"
            }
            OverlayChoice::Dismiss => {
                entry.state = Gentle::Dismissed;
                "dismissed"
            }
        };
        self.state.push_event(DaemonEvent::Distraction {
            ts,
            target: v.display_name(),
            kind: DistractionKind::App,
            list: v.list,
            action: action.into(),
        });
        self.save();
        Response::Ok
    }

    pub fn status(&self, ts: i64) -> DaemonStatus {
        let active = self.state.active.as_ref().map(|a| ActiveInfo {
            goal: a.goal.clone(),
            started_at: a.started_at,
            lists: a.spec.list_names(),
            hard: a.hard.as_ref().map(|h| HardStatus {
                deadline: h.deadline,
                challenge_enabled: h.early_exit_challenge,
                challenge: if h.early_exit_challenge { a.hard_state.challenge.clone() } else { None },
                exit_cooldown_mins: h.exit_cooldown_mins,
                exit_cooldown_remaining: a.hard_state.exit_cooldown_remaining(h, ts),
                emergency_cooldown_mins: h.emergency_cooldown_mins,
                emergency_remaining: a.hard_state.emergency_remaining(h, ts),
            }),
        });
        let st = self.sites.status();
        DaemonStatus {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").into(),
            dry_run: self.cfg.dry_run,
            active,
            schedules_active: self.schedules_active.clone(),
            schedules_total: self.state.schedules.len(),
            blocked_domains: st.domains,
            hosts_applied: st.hosts_applied,
            policies_applied: st.policies_applied,
            warnings: self.last_report.warnings.clone(),
        }
    }

    /// Remove every trace of blocking (used by `cleanup` and emergency unlock).
    pub fn force_clear(&mut self) {
        self.state.active = None;
        self.sites.clear();
        self.apps.set_rules(AppRules::default());
        self.applied = None;
        self.save();
    }
}

#[cfg(test)]
mod tests;
