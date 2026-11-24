use super::*;
use chrono::TimeZone;
use std::sync::{Arc, Mutex};
use studyforge_blocker::sites::{SiteReport, SiteStatus};
use studyforge_core::blocking::BlockList;
use studyforge_core::schedule::{ALL_DAYS, WEEKDAYS};

#[derive(Default)]
struct SitesLog {
    applies: Vec<BlockSpec>,
    clears: usize,
    on: bool,
}
#[derive(Clone, Default)]
struct FakeSites(Arc<Mutex<SitesLog>>);
impl SiteBlocker for FakeSites {
    fn apply(&mut self, spec: &BlockSpec) -> SiteReport {
        let mut l = self.0.lock().unwrap();
        l.applies.push(spec.clone());
        l.on = true;
        SiteReport::default()
    }
    fn clear(&mut self) -> SiteReport {
        let mut l = self.0.lock().unwrap();
        l.clears += 1;
        l.on = false;
        SiteReport::default()
    }
    fn status(&self) -> SiteStatus {
        SiteStatus { hosts_applied: self.0.lock().unwrap().on, ..Default::default() }
    }
}

#[derive(Default)]
struct AppsLog {
    violations: Vec<Violation>,
    enforced: Vec<(u32, Action)>,
    rules_nonempty: bool,
    outcome: Option<Outcome>,
}
#[derive(Clone, Default)]
struct FakeApps(Arc<Mutex<AppsLog>>);
impl AppBlocker for FakeApps {
    fn set_rules(&mut self, r: AppRules) {
        self.0.lock().unwrap().rules_nonempty = !r.is_empty();
    }
    fn scan(&mut self) -> Vec<Violation> {
        let l = self.0.lock().unwrap();
        if l.rules_nonempty {
            l.violations.clone()
        } else {
            vec![]
        }
    }
    fn enforce(&mut self, v: &Violation, a: Action) -> Outcome {
        let mut l = self.0.lock().unwrap();
        l.enforced.push((v.pid, a));
        match a {
            Action::Notify => Outcome::Notified,
            Action::Kill => l.outcome.clone().unwrap_or(Outcome::Killed),
        }
    }
}

#[derive(Clone, Default)]
struct FakeNotifier(Arc<Mutex<Vec<String>>>);
impl Notifier for FakeNotifier {
    fn notify(&self, _s: &str, b: &str) {
        self.0.lock().unwrap().push(b.to_string());
    }
}

struct Rig {
    engine: Engine<FakeSites, FakeApps, FakeNotifier>,
    sites: FakeSites,
    apps: FakeApps,
    notes: FakeNotifier,
    dir: tempfile::TempDir,
}

fn rig_in(dir: tempfile::TempDir) -> Rig {
    let sites = FakeSites::default();
    let apps = FakeApps::default();
    let notes = FakeNotifier::default();
    let cfg = EngineConfig {
        state_path: dir.path().join("state.json"),
        emergency_flag: dir.path().join("emergency-unlock"),
        dry_run: false,
        allowed_owners: vec!["1000".into()],
        honor_emergency_flag: true,
    };
    let engine = Engine::new(
        cfg,
        sites.clone(),
        apps.clone(),
        notes.clone(),
        Box::new(|t| t.fixed_offset()),
        Box::new(Vec::new),
    );
    Rig { engine, sites, apps, notes, dir }
}
fn rig() -> Rig {
    rig_in(tempfile::tempdir().unwrap())
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(T0 + secs, 0).unwrap()
}
const T0: i64 = 1_700_000_000;

fn spec(mode: EnforceMode) -> BlockSpec {
    BlockSpec {
        lists: vec![BlockList {
            name: "Social".into(),
            domains: vec!["reddit.com".into()],
            processes: vec!["discord".into()],
            steam_games: false,
            mode,
        }],
        browser_policies: true,
        ..Default::default()
    }
}
fn start(goal: &str, spec: BlockSpec, hard: Option<HardModeConfig>) -> Request {
    Request::StartSession(StartSession { goal: goal.into(), spec, hard })
}
fn viol(pid: u32, mode: EnforceMode) -> Violation {
    Violation {
        pid,
        name: "discord".into(),
        exe: None,
        owner: Some("1000".into()),
        start_time: 5,
        list: "Social".into(),
        mode,
        kind: studyforge_blocker::ViolationKind::Process,
    }
}
fn is_ok(r: &Response) -> bool {
    matches!(r, Response::Ok)
}
fn events(r: &mut Rig, secs: i64) -> Vec<DaemonEvent> {
    match r.engine.handle(Request::PollEvents, at(secs)) {
        Response::Events { events } => events,
        other => panic!("{other:?}"),
    }
}

#[test]
fn handshake_checks_protocol_version() {
    let mut r = rig();
    assert!(matches!(
        r.engine.handle(Request::Hello { protocol: PROTOCOL_VERSION, client: "t".into() }, at(0)),
        Response::Hello { .. }
    ));
    assert!(matches!(
        r.engine.handle(Request::Hello { protocol: 999, client: "t".into() }, at(0)),
        Response::Error { .. }
    ));
}

#[test]
fn soft_session_applies_and_stops() {
    let mut r = rig();
    assert!(is_ok(&r.engine.handle(start("study", spec(EnforceMode::Hard), None), at(0))));
    assert_eq!(r.sites.0.lock().unwrap().applies.len(), 1);
    assert!(r.apps.0.lock().unwrap().rules_nonempty);
    assert!(
        matches!(r.engine.handle(Request::Status, at(1)), Response::Status(s) if s.active.is_some() && s.hosts_applied)
    );
    assert!(is_ok(&r.engine.handle(Request::StopSession, at(5))));
    assert!(!r.sites.0.lock().unwrap().on);
    assert!(!r.apps.0.lock().unwrap().rules_nonempty);
}

#[test]
fn soft_session_lapses_without_contact_but_polling_keeps_it_alive() {
    let mut r = rig();
    r.engine.handle(start("s", spec(EnforceMode::Hard), None), at(0));
    for i in 1..=5 {
        r.engine.handle(Request::Status, at(i * 300));
        r.engine.tick(at(i * 300));
    }
    assert!(r.engine.state.active.is_some(), "kept alive by contact");
    r.engine.tick(at(1500 + SOFT_LEASE_SECS + 1));
    assert!(r.engine.state.active.is_none());
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn hard_session_cannot_be_stopped_or_replaced() {
    let mut r = rig();
    let h = HardModeConfig::new(T0 + 3600, true, 20, 60);
    assert!(is_ok(&r.engine.handle(start("g", spec(EnforceMode::Hard), Some(h)), at(0))));
    assert!(matches!(r.engine.handle(Request::StopSession, at(10)), Response::Error { .. }));
    assert!(matches!(r.engine.handle(start("other", BlockSpec::default(), None), at(10)), Response::Error { .. }));
    assert!(r.engine.state.active.is_some());
    // even without any GUI contact it keeps running far beyond the soft lease
    r.engine.tick(at(3000));
    assert!(r.engine.state.active.is_some());
    assert!(r.sites.0.lock().unwrap().on);
}

#[test]
fn hard_session_ends_at_deadline() {
    let mut r = rig();
    let h = HardModeConfig::new(T0 + 600, false, 20, 60);
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(h)), at(0));
    r.engine.tick(at(599));
    assert!(r.engine.state.active.is_some());
    r.engine.tick(at(600));
    assert!(r.engine.state.active.is_none());
    assert!(!r.sites.0.lock().unwrap().on);
    let ev = events(&mut r, 601);
    assert!(ev.iter().any(|e| matches!(e, DaemonEvent::Notice { message, .. } if message.contains("finished"))));
}

#[test]
fn deadline_is_clamped_and_emergency_floor_enforced_server_side() {
    let mut r = rig();
    // client asks for a year and a 1-minute emergency cooldown
    let h = HardModeConfig {
        deadline: T0 + 365 * 86400,
        early_exit_challenge: true,
        exit_cooldown_mins: 20,
        emergency_cooldown_mins: 1,
    };
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(h)), at(0));
    let hs = r.engine.state.active.as_ref().unwrap().hard.clone().unwrap();
    assert_eq!(hs.deadline, T0 + MAX_HARD_SECS);
    assert_eq!(hs.emergency_cooldown_mins, studyforge_core::hardmode::MIN_EMERGENCY_COOLDOWN_MINS);
    // a deadline in the past is still at least a minute
    let mut r2 = rig();
    let h = HardModeConfig {
        deadline: T0 - 5,
        early_exit_challenge: false,
        exit_cooldown_mins: 1,
        emergency_cooldown_mins: 60,
    };
    r2.engine.handle(start("g", spec(EnforceMode::Hard), Some(h)), at(0));
    assert_eq!(r2.engine.state.active.as_ref().unwrap().hard.as_ref().unwrap().deadline, T0 + 60);
}

#[test]
fn hard_mode_survives_restart_and_ends_if_deadline_passed_meanwhile() {
    let r = rig();
    let dir = r.dir;
    let mut e = r.engine;
    let h = HardModeConfig::new(T0 + 1000, true, 20, 60);
    e.handle(start("g", spec(EnforceMode::Hard), Some(h)), at(0));
    drop(e);
    // reboot 500 s later: lock is still in force and blocks come back
    let mut r2 = rig_in(dir);
    r2.engine.reconcile_on_start(at(500));
    assert!(r2.engine.state.active.is_some());
    assert!(r2.sites.0.lock().unwrap().on, "blocks re-applied after reboot");
    let dir = r2.dir;
    drop(r2.engine);
    // long outage: machine was off past the deadline
    let mut r3 = rig_in(dir);
    r3.engine.reconcile_on_start(at(5000));
    assert!(r3.engine.state.active.is_none());
    assert!(!r3.sites.0.lock().unwrap().on);
}

#[test]
fn startup_reconcile_clears_leftovers_when_nothing_should_be_active() {
    let mut r = rig();
    r.engine.reconcile_on_start(at(0));
    assert!(r.sites.0.lock().unwrap().clears >= 1, "crash recovery clears stale sections");
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn challenge_flow_and_failure_rotation() {
    let mut r = rig();
    let h = HardModeConfig::new(T0 + 3600, true, 20, 60);
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(h)), at(0));
    let Response::Status(s) = r.engine.handle(Request::Status, at(1)) else { panic!() };
    let first = s.active.unwrap().hard.unwrap().challenge.unwrap();
    assert_eq!(first.len(), studyforge_core::hardmode::CHALLENGE_LEN);
    assert!(matches!(r.engine.handle(Request::ExitChallenge { typed: "wrong".into() }, at(2)), Response::Error { .. }));
    let Response::Status(s) = r.engine.handle(Request::Status, at(3)) else { panic!() };
    let second = s.active.unwrap().hard.unwrap().challenge.unwrap();
    assert_ne!(first, second);
    assert!(is_ok(&r.engine.handle(Request::ExitChallenge { typed: second }, at(4))));
    assert!(r.engine.state.active.is_none());
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn challenge_disabled_means_no_challenge_exit() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, false, 20, 60))), at(0));
    let Response::Status(s) = r.engine.handle(Request::Status, at(1)) else { panic!() };
    assert!(s.active.unwrap().hard.unwrap().challenge.is_none());
    assert!(matches!(r.engine.handle(Request::ExitChallenge { typed: "x".into() }, at(2)), Response::Error { .. }));
}

#[test]
fn exit_cooldown_ends_session() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 7200, true, 20, 60))), at(0));
    assert!(is_ok(&r.engine.handle(Request::ExitCooldown { start: true }, at(10))));
    r.engine.tick(at(10 + 20 * 60 - 1));
    assert!(r.engine.state.active.is_some());
    r.engine.tick(at(10 + 20 * 60));
    assert!(r.engine.state.active.is_none());
}

#[test]
fn exit_cooldown_can_be_cancelled() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 7200, true, 20, 60))), at(0));
    r.engine.handle(Request::ExitCooldown { start: true }, at(10));
    r.engine.handle(Request::ExitCooldown { start: false }, at(100));
    r.engine.tick(at(5000));
    assert!(r.engine.state.active.is_some());
}

#[test]
fn emergency_unlock_always_works() {
    let mut r = rig();
    // even with the challenge disabled and a far deadline
    r.engine
        .handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 20 * 3600, false, 20, 60))), at(0));
    assert!(is_ok(&r.engine.handle(Request::Emergency { start: true }, at(100))));
    let Response::Status(s) = r.engine.handle(Request::Status, at(200)) else { panic!() };
    assert_eq!(s.active.unwrap().hard.unwrap().emergency_remaining, Some(3600 - 100));
    r.engine.tick(at(100 + 3599));
    assert!(r.engine.state.active.is_some());
    r.engine.tick(at(100 + 3600));
    assert!(r.engine.state.active.is_none());
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn emergency_request_survives_restart() {
    let mut r = rig();
    r.engine
        .handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 20 * 3600, false, 20, 60))), at(0));
    r.engine.handle(Request::Emergency { start: true }, at(100));
    let dir = r.dir;
    drop(r.engine);
    let mut r2 = rig_in(dir);
    r2.engine.reconcile_on_start(at(100 + 3700));
    assert!(r2.engine.state.active.is_none(), "cooldown kept counting while the daemon was down");
}

#[test]
fn cli_emergency_flag_file_ends_session_immediately() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    std::fs::write(&r.engine.cfg.emergency_flag, "").unwrap();
    r.engine.tick(at(5));
    assert!(r.engine.state.active.is_none());
    assert!(!r.engine.cfg.emergency_flag.exists());
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn cli_emergency_flag_also_lifts_schedules() {
    let mut r = rig();
    let rule = ScheduleRule {
        id: 1,
        name: "S".into(),
        enabled: true,
        days: ALL_DAYS,
        start_min: 0,
        end_min: 0,
        spec: spec(EnforceMode::Hard),
    };
    r.engine.handle(Request::SetSchedules { rules: vec![rule] }, at(0));
    assert!(r.sites.0.lock().unwrap().on);
    std::fs::write(&r.engine.cfg.emergency_flag, "").unwrap();
    r.engine.tick(at(2));
    assert!(r.engine.state.schedules.is_empty());
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn hard_requests_without_hard_session_error() {
    let mut r = rig();
    for req in [
        Request::ExitChallenge { typed: "x".into() },
        Request::Emergency { start: true },
        Request::ExitCooldown { start: true },
    ] {
        assert!(matches!(r.engine.handle(req, at(0)), Response::Error { .. }));
    }
    r.engine.handle(start("g", spec(EnforceMode::Hard), None), at(0));
    assert!(matches!(r.engine.handle(Request::Emergency { start: true }, at(1)), Response::Error { .. }));
}

#[test]
fn schedules_apply_without_a_session_and_follow_the_clock() {
    let mut r = rig();
    // 2023-11-14 is a Tuesday; T0 is 22:13 UTC
    let rule = ScheduleRule {
        id: 1,
        name: "Evenings".into(),
        enabled: true,
        days: WEEKDAYS,
        start_min: 22 * 60,
        end_min: 23 * 60 + 30,
        spec: spec(EnforceMode::Hard),
    };
    assert!(is_ok(&r.engine.handle(Request::SetSchedules { rules: vec![rule] }, at(0))));
    assert!(r.sites.0.lock().unwrap().on, "active at 22:13");
    let Response::Status(s) = r.engine.handle(Request::Status, at(1)) else { panic!() };
    assert_eq!(s.schedules_active, vec!["Evenings"]);
    assert!(s.active.is_none());
    r.engine.tick(at(77 * 60 + 1)); // 23:30:21
    assert!(!r.sites.0.lock().unwrap().on, "inactive after the window");
    // overnight rule starting at 23:00 covers the rest of the night
    let overnight = ScheduleRule {
        id: 2,
        name: "Night".into(),
        enabled: true,
        days: ALL_DAYS,
        start_min: 23 * 60,
        end_min: 2 * 60,
        spec: spec(EnforceMode::Hard),
    };
    r.engine.handle(Request::SetSchedules { rules: vec![overnight] }, at(77 * 60 + 2));
    assert!(r.sites.0.lock().unwrap().on);
    r.engine.tick(at(3 * 3600 + 60)); // 01:13 next day
    assert!(r.sites.0.lock().unwrap().on);
    r.engine.tick(at(4 * 3600)); // 02:13
    assert!(!r.sites.0.lock().unwrap().on);
}

#[test]
fn schedule_union_with_session_and_no_flapping() {
    let mut r = rig();
    let rule = ScheduleRule {
        id: 1,
        name: "S".into(),
        enabled: true,
        days: ALL_DAYS,
        start_min: 0,
        end_min: 0,
        spec: spec(EnforceMode::Hard),
    };
    r.engine.handle(Request::SetSchedules { rules: vec![rule] }, at(0));
    let n = r.sites.0.lock().unwrap().applies.len();
    // same desired state again: no re-apply until the heal interval
    for i in 1..10 {
        r.engine.tick(at(i * 2));
    }
    assert_eq!(r.sites.0.lock().unwrap().applies.len(), n);
    r.engine.tick(at(HEAL_INTERVAL_SECS + 1));
    assert_eq!(r.sites.0.lock().unwrap().applies.len(), n + 1, "periodic self-heal");
    // stopping the session keeps the schedule's block active
    r.engine.handle(start("g", spec(EnforceMode::Hard), None), at(100));
    r.engine.handle(Request::StopSession, at(101));
    assert!(r.sites.0.lock().unwrap().on);
}

#[test]
fn hard_kills_are_logged_once_per_process() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), None), at(0));
    r.apps.0.lock().unwrap().violations = vec![viol(42, EnforceMode::Hard)];
    for i in 1..4 {
        r.engine.tick(at(i * 2));
    }
    assert_eq!(r.apps.0.lock().unwrap().enforced.len(), 3, "kept enforcing (escalation)");
    let ev = events(&mut r, 10);
    let d: Vec<_> = ev.iter().filter(|e| matches!(e, DaemonEvent::Distraction { .. })).collect();
    assert_eq!(d.len(), 1);
    assert!(
        matches!(d[0], DaemonEvent::Distraction { action, target, .. } if action == "killed" && target == "discord")
    );
}

#[test]
fn gentle_overlay_flow_close_snooze_dismiss() {
    let mut r = rig();
    r.engine.handle(start("finish chapter 4", spec(EnforceMode::Gentle), None), at(0));
    r.apps.0.lock().unwrap().violations = vec![viol(7, EnforceMode::Gentle)];
    r.engine.handle(Request::PollEvents, at(1)); // GUI alive
    r.engine.tick(at(2));
    let ev = events(&mut r, 3);
    let (id, goal) = ev
        .iter()
        .find_map(|e| if let DaemonEvent::Overlay { id, goal, .. } = e { Some((*id, goal.clone())) } else { None })
        .expect("overlay event");
    assert_eq!(goal, "finish chapter 4");
    assert!(r.apps.0.lock().unwrap().enforced.is_empty(), "gentle mode never kills by itself");
    assert!(r.notes.0.lock().unwrap().is_empty(), "GUI is running: no fallback notification");
    // still running, still pending: no duplicate overlay
    r.engine.tick(at(4));
    assert!(events(&mut r, 5).iter().all(|e| !matches!(e, DaemonEvent::Overlay { .. })));
    // snooze 5 minutes
    r.engine.handle(Request::OverlayAction { id, choice: OverlayChoice::FiveMore }, at(6));
    r.engine.tick(at(10));
    assert!(events(&mut r, 11).iter().all(|e| !matches!(e, DaemonEvent::Overlay { .. })));
    r.engine.tick(at(6 + SNOOZE_SECS));
    let ev = events(&mut r, 6 + SNOOZE_SECS + 1);
    let id2 = ev
        .iter()
        .find_map(|e| if let DaemonEvent::Overlay { id, .. } = e { Some(*id) } else { None })
        .expect("overlay returns after snooze");
    assert_ne!(id, id2);
    // close it
    r.engine.handle(Request::OverlayAction { id: id2, choice: OverlayChoice::CloseIt }, at(400));
    assert_eq!(r.apps.0.lock().unwrap().enforced, vec![(7, Action::Kill)]);
    let ev = events(&mut r, 401);
    assert!(ev.iter().any(|e| matches!(e, DaemonEvent::Distraction { action, .. } if action.starts_with("closed"))));
}

#[test]
fn gentle_dismiss_is_for_the_rest_of_the_session_for_that_process() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Gentle), None), at(0));
    r.apps.0.lock().unwrap().violations = vec![viol(7, EnforceMode::Gentle)];
    r.engine.tick(at(1));
    let ev = events(&mut r, 2);
    let id = ev.iter().find_map(|e| if let DaemonEvent::Overlay { id, .. } = e { Some(*id) } else { None }).unwrap();
    r.engine.handle(Request::OverlayAction { id, choice: OverlayChoice::Dismiss }, at(3));
    for i in 0..200 {
        r.engine.tick(at(10 + i * 2));
        r.engine.handle(Request::Status, at(10 + i * 2)); // keep the soft session alive
    }
    assert!(events(&mut r, 500).iter().all(|e| !matches!(e, DaemonEvent::Overlay { .. })));
    // a different process (new pid) is announced again
    r.apps.0.lock().unwrap().violations = vec![viol(8, EnforceMode::Gentle)];
    r.engine.tick(at(600));
    assert!(events(&mut r, 601).iter().any(|e| matches!(e, DaemonEvent::Overlay { .. })));
}

#[test]
fn gentle_falls_back_to_desktop_notification_when_gui_is_not_polling() {
    let mut r = rig();
    r.engine.handle(start("write essay", spec(EnforceMode::Gentle), None), at(0));
    r.apps.0.lock().unwrap().violations = vec![viol(7, EnforceMode::Gentle)];
    r.engine.tick(at(100)); // GUI never polled
    let n = r.notes.0.lock().unwrap().clone();
    assert_eq!(n.len(), 1);
    assert!(n[0].contains("write essay") && n[0].contains("discord"));
}

#[test]
fn stale_overlay_actions_are_harmless() {
    let mut r = rig();
    assert!(is_ok(&r.engine.handle(Request::OverlayAction { id: 999, choice: OverlayChoice::CloseIt }, at(0))));
    assert!(r.apps.0.lock().unwrap().enforced.is_empty());
}

#[test]
fn nothing_is_scanned_when_no_block_is_active() {
    let mut r = rig();
    r.apps.0.lock().unwrap().violations = vec![viol(1, EnforceMode::Hard)];
    r.engine.tick(at(0));
    assert!(r.apps.0.lock().unwrap().enforced.is_empty());
}

#[test]
fn dry_run_does_not_persist() {
    let mut r = rig();
    r.engine.cfg.dry_run = true;
    r.engine.handle(start("g", spec(EnforceMode::Hard), None), at(0));
    assert!(!r.dir.path().join("state.json").exists());
}

#[test]
fn force_clear_removes_everything() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    r.engine.force_clear();
    assert!(r.engine.state.active.is_none());
    assert!(!r.sites.0.lock().unwrap().on);
}

// ---------------------------------------------------------------- clock changes
fn boot(secs: u64) -> Option<std::time::Duration> {
    Some(std::time::Duration::from_secs(secs))
}

#[test]
fn clock_set_backwards_does_not_extend_the_lock() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    r.engine.tick_with_clock(at(0), boot(1000));
    r.engine.tick_with_clock(at(600), boot(1600)); // normal: 10 min passed
                                                   // user sets the clock back by a full day
    r.engine.tick_with_clock(at(602 - 86400), boot(1602));
    let deadline = r.engine.state.active.as_ref().unwrap().hard.as_ref().unwrap().deadline;
    assert_eq!(deadline, T0 + 3600 - 86400, "deadline moved with the clock: remaining time unchanged");
    // 50 minutes of real time later the lock ends as planned
    r.engine.tick_with_clock(at(602 - 86400 + 2998), boot(1602 + 2998));
    assert!(r.engine.state.active.is_none());
}

#[test]
fn clock_set_forwards_does_not_end_the_lock_instantly() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    r.engine.tick_with_clock(at(0), boot(1000));
    r.engine.tick_with_clock(at(2 + 7 * 86400), boot(1002)); // jumped a week ahead
    assert!(r.engine.state.active.is_some(), "still locked");
    let a = r.engine.state.active.as_ref().unwrap();
    assert_eq!(a.hard.as_ref().unwrap().deadline - (T0 + 2 + 7 * 86400), 3598);
}

#[test]
fn suspend_counts_as_real_time() {
    let mut r = rig();
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    r.engine.tick_with_clock(at(0), boot(1000));
    // laptop slept 2 hours: wall and boot clocks both advanced
    r.engine.tick_with_clock(at(7200), boot(8200));
    assert!(r.engine.state.active.is_none(), "the lock ended while asleep");
}

#[test]
fn cooldowns_shift_with_the_clock_too() {
    let mut r = rig();
    r.engine
        .handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 20 * 3600, false, 20, 60))), at(0));
    r.engine.tick_with_clock(at(0), boot(0));
    r.engine.handle(Request::Emergency { start: true }, at(10));
    r.engine.tick_with_clock(at(12 + 10_000), boot(12)); // clock forward ~2.8 h
    assert!(r.engine.state.active.is_some(), "a clock jump must not complete the emergency cooldown");
    r.engine.tick_with_clock(at(12 + 10_000 + 3600), boot(12 + 3600));
    assert!(r.engine.state.active.is_none(), "but real elapsed time does");
}

#[test]
fn clock_moved_back_while_daemon_was_down() {
    let r = rig();
    let dir = r.dir;
    let mut e = r.engine;
    e.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    e.tick_with_clock(at(1200), boot(1200)); // last seen at T0+1200 (persisted)
    drop(e);
    let mut r2 = rig_in(dir);
    r2.engine.reconcile_on_start(at(1200 - 30 * 86400)); // RTC reset a month back
    let h = r2.engine.state.active.as_ref().unwrap().hard.clone().unwrap();
    assert_eq!(h.deadline - (T0 + 1200 - 30 * 86400), 2400, "remaining time preserved, not a month extra");
}

#[test]
fn lock_is_never_longer_than_the_maximum_after_restart() {
    let r = rig();
    let dir = r.dir;
    // hand-craft a persisted state with an absurd deadline (corrupt/edited file)
    let mut st = crate::state::Persisted::default();
    st.active = Some(crate::state::ActiveSession {
        goal: "x".into(),
        started_at: T0,
        spec: spec(EnforceMode::Hard),
        hard: Some(HardModeConfig {
            deadline: T0 + 400 * 86400,
            early_exit_challenge: false,
            exit_cooldown_mins: 20,
            emergency_cooldown_mins: 60,
        }),
        hard_state: Default::default(),
        last_contact: T0,
    });
    st.save(&dir.path().join("state.json")).unwrap();
    drop(r.engine);
    let mut r2 = rig_in(dir);
    r2.engine.reconcile_on_start(at(0));
    assert_eq!(r2.engine.state.active.as_ref().unwrap().hard.as_ref().unwrap().deadline, T0 + MAX_HARD_SECS);
}

#[test]
fn emergency_flag_ignored_when_state_dir_is_not_admin_only() {
    let mut r = rig();
    r.engine.cfg.honor_emergency_flag = false;
    r.engine.handle(start("g", spec(EnforceMode::Hard), Some(HardModeConfig::new(T0 + 3600, true, 20, 60))), at(0));
    std::fs::write(&r.engine.cfg.emergency_flag, "").unwrap();
    r.engine.tick(at(5));
    assert!(r.engine.state.active.is_some(), "a planted flag must not end the lock");
    assert!(!r.engine.cfg.emergency_flag.exists(), "and is removed");
}
