//! End-to-end test of the real daemon wiring in sandbox mode: real sockets,
//! real process table, but hosts/policies/state live in a temp dir and the only
//! process ever killed is a copy of `sleep` spawned by this test.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use studyd_lib::daemon::{self, Options};
use studyd_lib::transport::request;
use studyforge_core::blocking::{BlockList, BlockSpec};
use studyforge_core::hardmode::HardModeConfig;
use studyforge_core::ipc::*;
use studyforge_core::models::EnforceMode;

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn spawn_victim(dir: &Path, name: &str) -> Child {
    let sleep = String::from_utf8(Command::new("which").arg("sleep").output().unwrap().stdout).unwrap();
    let victim: PathBuf = dir.join(name);
    std::fs::copy(sleep.trim(), &victim).unwrap();
    Command::new(&victim).arg("300").spawn().unwrap()
}

fn spec(victim: &str) -> BlockSpec {
    BlockSpec {
        lists: vec![BlockList {
            name: "E2E".into(),
            domains: vec!["e2e-blocked.example.com".into(), "www.e2e-blocked.example.com".into()],
            processes: vec![victim.into()],
            steam_games: false,
            mode: EnforceMode::Hard,
        }],
        browser_policies: true,
        ..Default::default()
    }
}

#[test]
fn full_session_lifecycle_in_sandbox() {
    let root = tempfile::tempdir().unwrap();
    let sb = root.path().to_path_buf();
    std::fs::create_dir_all(sb.join("etc")).unwrap();
    let original_hosts = "127.0.0.1 localhost\n# e2e\n";
    std::fs::write(sb.join("etc/hosts"), original_hosts).unwrap();
    let opts = Options { dry_run: false, sandbox: Some(sb.clone()) };
    let l = daemon::layout(&opts);

    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (o, s) = (opts.clone(), stop.clone());
        std::thread::spawn(move || daemon::run_daemon(&o, s).unwrap())
    };
    wait_for("socket", 10, || std::path::Path::new(&l.endpoint).exists());

    assert!(
        matches!(request(&l.endpoint, &Request::Status).unwrap(), Response::Status(s) if s.active.is_none() && !s.hosts_applied)
    );

    // ---- soft session: hosts + policies + process kill ----
    let mut victim = spawn_victim(&sb, "e2evictimproc");
    let r = request(
        &l.endpoint,
        &Request::StartSession(StartSession { goal: "e2e".into(), spec: spec("e2evictimproc"), hard: None }),
    )
    .unwrap();
    assert_eq!(r, Response::Ok);
    let hosts = std::fs::read_to_string(sb.join("etc/hosts")).unwrap();
    assert!(hosts.starts_with(original_hosts), "original content untouched");
    assert!(hosts.contains("0.0.0.0 e2e-blocked.example.com") && hosts.contains(":: e2e-blocked.example.com"));
    assert!(sb.join("etc/chromium/policies/managed/zz-studyforge.json").exists());
    assert!(sb.join("etc/firefox/policies/policies.json").exists());

    wait_for("victim to be terminated by the daemon", 12, || victim.try_wait().unwrap().is_some());

    let Response::Events { events } = request(&l.endpoint, &Request::PollEvents).unwrap() else { panic!() };
    assert!(events.iter().any(|e| matches!(e, DaemonEvent::Distraction { target, action, .. } if target == "e2evictimproc" && action == "killed")), "{events:?}");

    assert_eq!(request(&l.endpoint, &Request::StopSession).unwrap(), Response::Ok);
    assert_eq!(std::fs::read_to_string(sb.join("etc/hosts")).unwrap(), original_hosts, "hosts restored byte-for-byte");
    assert!(!sb.join("etc/chromium/policies/managed/zz-studyforge.json").exists());
    assert!(!sb.join("etc/firefox/policies/policies.json").exists());

    // ---- hard mode: cannot stop; emergency unlock always works ----
    let hard = HardModeConfig::new(chrono::Utc::now().timestamp() + 3600, true, 20, 60);
    let r = request(
        &l.endpoint,
        &Request::StartSession(StartSession { goal: "locked".into(), spec: spec("e2enothing"), hard: Some(hard) }),
    )
    .unwrap();
    assert_eq!(r, Response::Ok);
    assert!(matches!(request(&l.endpoint, &Request::StopSession).unwrap(), Response::Error { .. }));
    assert!(std::fs::read_to_string(sb.join("etc/hosts")).unwrap().contains("e2e-blocked"));

    daemon::emergency_unlock(&opts).unwrap(); // the root/admin CLI escape
    wait_for(
        "daemon to notice the emergency unlock",
        8,
        || matches!(request(&l.endpoint, &Request::Status), Ok(Response::Status(s)) if s.active.is_none()),
    );
    assert_eq!(std::fs::read_to_string(sb.join("etc/hosts")).unwrap(), original_hosts);

    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
    let _ = victim.kill();
}

#[test]
fn hard_session_survives_daemon_restart() {
    let root = tempfile::tempdir().unwrap();
    let sb = root.path().to_path_buf();
    std::fs::create_dir_all(sb.join("etc")).unwrap();
    std::fs::write(sb.join("etc/hosts"), "127.0.0.1 localhost\n").unwrap();
    let opts = Options { dry_run: false, sandbox: Some(sb.clone()) };
    let l = daemon::layout(&opts);

    let run = |stop: Arc<AtomicBool>| {
        let o = opts.clone();
        std::thread::spawn(move || daemon::run_daemon(&o, stop).unwrap())
    };
    let stop1 = Arc::new(AtomicBool::new(false));
    let h1 = run(stop1.clone());
    wait_for("socket", 10, || std::path::Path::new(&l.endpoint).exists());
    let hard = HardModeConfig::new(chrono::Utc::now().timestamp() + 3600, true, 20, 60);
    request(
        &l.endpoint,
        &Request::StartSession(StartSession { goal: "locked".into(), spec: spec("nothing-x"), hard: Some(hard) }),
    )
    .unwrap();
    stop1.store(true, Ordering::Relaxed);
    h1.join().unwrap();

    // someone removes the hosts section while the daemon is down...
    std::fs::write(sb.join("etc/hosts"), "127.0.0.1 localhost\n").unwrap();
    // ...the restarted daemon restores the lock from disk
    let stop2 = Arc::new(AtomicBool::new(false));
    let h2 = run(stop2.clone());
    wait_for("socket", 10, || request(&l.endpoint, &Request::Ping).is_ok());
    let Response::Status(s) = request(&l.endpoint, &Request::Status).unwrap() else { panic!() };
    assert!(s.active.as_ref().is_some_and(|a| a.hard.is_some()), "hard mode persisted");
    assert!(std::fs::read_to_string(sb.join("etc/hosts")).unwrap().contains("e2e-blocked"), "blocks re-applied");
    stop2.store(true, Ordering::Relaxed);
    h2.join().unwrap();
}
