//! Wiring of the real implementations: process table, hosts/policy files,
//! sockets. Shared by the console `run` command and the Windows service.

use crate::engine::{Engine, EngineConfig, Notifier};
use crate::state::{load_allowed_users, Persisted};
use crate::transport::{self, Handler};
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use studyforge_blocker::paths::Paths;
use studyforge_blocker::sites::{SiteBlocker, SystemCommandRunner, SystemSiteBlocker};
use studyforge_blocker::steam;
use studyforge_blocker::{ProcessBlocker, SysinfoSource};
use studyforge_core::ipc::{Request, Response};

pub const TICK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    /// Run entirely inside this directory (own socket, state, fake hosts file):
    /// for development and demos; needs no root and touches nothing else.
    pub sandbox: Option<PathBuf>,
}

pub struct Layout {
    pub paths: Paths,
    pub endpoint: String,
    pub allowed_users_file: PathBuf,
    pub state_file: PathBuf,
    pub emergency_flag: PathBuf,
}

pub fn layout(opts: &Options) -> Layout {
    match &opts.sandbox {
        Some(dir) => {
            let paths = Paths::under(dir);
            Layout {
                endpoint: dir.join("studyd.sock").to_string_lossy().into_owned(),
                allowed_users_file: dir.join("allowed_users"),
                state_file: paths.state_dir.join("state.json"),
                emergency_flag: paths.state_dir.join("emergency-unlock"),
                paths,
            }
        }
        None => {
            let paths = Paths::system();
            Layout {
                endpoint: studyforge_core::paths::ipc_endpoint(),
                allowed_users_file: studyforge_core::paths::daemon_config_dir().join("allowed_users"),
                state_file: paths.state_dir.join("state.json"),
                emergency_flag: paths.state_dir.join("emergency-unlock"),
                paths,
            }
        }
    }
}

fn current_identity() -> Vec<String> {
    #[cfg(unix)]
    {
        // SAFETY: getuid cannot fail
        vec![unsafe { libc::getuid() }.to_string()]
    }
    #[cfg(not(unix))]
    {
        vec![]
    }
}

fn allowed_users(l: &Layout, opts: &Options) -> Vec<String> {
    let mut v = load_allowed_users(&l.allowed_users_file);
    if v.is_empty() && opts.sandbox.is_some() {
        v = current_identity();
    }
    v
}

/// Shows a desktop notification in each allowed user's session (best effort).
pub struct DesktopNotifier {
    pub users: Arc<Mutex<Vec<String>>>,
}

impl Notifier for DesktopNotifier {
    fn notify(&self, summary: &str, body: &str) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            for uid in self.users.lock().unwrap().iter() {
                let Ok(n) = uid.parse::<u32>() else { continue };
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "StudyForge", summary, body])
                    .uid(n)
                    .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path=/run/user/{n}/bus"))
                    .env("XDG_RUNTIME_DIR", format!("/run/user/{n}"))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
        }
        #[cfg(not(unix))]
        {
            // A Windows service runs in session 0 and cannot show UI; the GUI overlay is the primary path.
            tracing::info!("(no desktop session reachable) {summary}: {body}");
        }
    }
}

type RealEngine = Engine<SystemSiteBlocker<SystemCommandRunner>, ProcessBlocker<SysinfoSource>, DesktopNotifier>;

struct Shared(Mutex<RealEngine>);

impl Handler for Shared {
    fn handle(&self, req: Request) -> Response {
        self.0.lock().unwrap().handle(req, Utc::now())
    }
}

fn steam_commons(users: Arc<Mutex<Vec<String>>>) -> Box<dyn Fn() -> Vec<PathBuf> + Send> {
    Box::new(move || {
        let ids = users.lock().unwrap().clone();
        #[cfg(unix)]
        {
            let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
            let homes = steam::homes_for_uids(&passwd, &ids);
            steam::common_dirs(&steam::linux_roots(&homes))
        }
        #[cfg(windows)]
        {
            steam::common_dirs(&steam::windows_roots::roots(&ids))
        }
    })
}

/// Run until `stop` is set.
pub fn run_daemon(opts: &Options, stop: Arc<AtomicBool>) -> Result<()> {
    let l = layout(opts);
    std::fs::create_dir_all(&l.paths.state_dir).with_context(|| format!("creating {}", l.paths.state_dir.display()))?;
    let users = Arc::new(Mutex::new(allowed_users(&l, opts)));
    if users.lock().unwrap().is_empty() {
        tracing::warn!("{} is empty or missing: only root can talk to studyd", l.allowed_users_file.display());
    }
    let sites = SystemSiteBlocker::new(l.paths.clone(), opts.dry_run, SystemCommandRunner);
    let apps = ProcessBlocker::new(SysinfoSource::new().context("cannot read the process table")?, opts.dry_run);
    let cfg = EngineConfig {
        state_path: l.state_file.clone(),
        emergency_flag: l.emergency_flag.clone(),
        dry_run: opts.dry_run,
        allowed_owners: users.lock().unwrap().clone(),
    };
    let mut engine = Engine::new(
        cfg,
        sites,
        apps,
        DesktopNotifier { users: users.clone() },
        Box::new(|t| t.with_timezone(&chrono::Local).fixed_offset()),
        steam_commons(users.clone()),
    );
    engine.reconcile_on_start(Utc::now());
    tracing::info!(
        "studyd {} started{}",
        env!("CARGO_PKG_VERSION"),
        if opts.dry_run { " (DRY RUN: nothing will be modified)" } else { "" }
    );
    let shared = Arc::new(Shared(Mutex::new(engine)));

    let server = {
        let (ep, u, h, s) = (l.endpoint.clone(), users.clone(), shared.clone() as Arc<dyn Handler>, stop.clone());
        std::thread::Builder::new().name("ipc".into()).spawn(move || {
            if let Err(e) = transport::serve(&ep, u, h, s) {
                tracing::error!("IPC server failed: {e}");
            }
        })?
    };

    let mut last_reload = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        let mut e = shared.0.lock().unwrap();
        if last_reload.elapsed() > Duration::from_secs(30) {
            last_reload = Instant::now();
            let fresh = allowed_users(&l, opts);
            e.cfg.allowed_owners = fresh.clone();
            *users.lock().unwrap() = fresh;
        }
        e.tick_with_clock(Utc::now(), Some(crate::engine::boot_clock()));
    }
    tracing::info!("studyd stopping (blocks stay in place if a hard-mode lock is active)");
    let _ = server.join();
    Ok(())
}

/// Remove the hosts section and browser policies (used by uninstall scripts).
pub fn cleanup(opts: &Options) -> Result<Vec<String>> {
    let l = layout(opts);
    let mut sites = SystemSiteBlocker::new(l.paths, opts.dry_run, SystemCommandRunner);
    let r = sites.clear();
    Ok(r.warnings)
}

/// Admin escape hatch. Works with the daemon running or not.
pub fn emergency_unlock(opts: &Options) -> Result<()> {
    let l = layout(opts);
    #[cfg(unix)]
    if opts.sandbox.is_none() && !opts.dry_run {
        // SAFETY: geteuid cannot fail
        anyhow::ensure!(
            unsafe { libc::geteuid() } == 0,
            "emergency-unlock must be run as root (sudo studyd emergency-unlock)"
        );
    }
    std::fs::create_dir_all(&l.paths.state_dir)?;
    // 1. tell a running daemon (it ends the session and lifts schedules on its next tick)
    std::fs::write(&l.emergency_flag, b"unlock")?;
    // 2. and make sure a stopped daemon won't resurrect the lock
    let mut st = Persisted::load(&l.state_file);
    st.active = None;
    st.schedules.clear();
    st.save(&l.state_file)?;
    // 3. remove hosts / policy sections right now
    let warnings = cleanup(opts)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    println!(
        "Emergency unlock done: hard-mode session ended, schedules removed, hosts file and browser policies restored."
    );
    println!("A running studyd will pick this up within a few seconds. Restart your browsers to drop cached policies.");
    Ok(())
}
