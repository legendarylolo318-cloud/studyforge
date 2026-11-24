//! studyd: the privileged StudyForge blocking helper.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use studyd_lib::daemon::{self, Options};
use studyforge_core::ipc::{Request, Response};

#[derive(Parser)]
#[command(name = "studyd", version, about = "StudyForge blocking helper (runs as root / SYSTEM)")]
struct Cli {
    /// Compute and log everything but do not modify hosts files, policies or processes.
    #[arg(long, global = true)]
    dry_run: bool,
    /// Work entirely inside DIR (own socket, state and a fake hosts file). For development; needs no root.
    #[arg(long, global = true, value_name = "DIR")]
    sandbox: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run in the foreground (what the systemd unit executes).
    Run,
    /// Windows service entry point (started by the service manager).
    Service,
    /// Windows: register the service (run from an elevated terminal).
    Install,
    /// Windows: stop and remove the service, then undo hosts/policy changes.
    Uninstall,
    /// Remove the StudyForge hosts section and browser policies.
    Cleanup,
    /// Admin escape: end any hard-mode lock and lift all blocking immediately.
    EmergencyUnlock,
    /// Print the running daemon's status.
    Status,
}

fn init_logging(to_file: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if to_file {
        let dir = studyforge_core::paths::daemon_state_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("studyd.log")) {
            tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(f)).init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

#[cfg(windows)]
mod winsvc;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let opts = Options { dry_run: cli.dry_run, sandbox: cli.sandbox };
    match cli.cmd {
        Cmd::Run => {
            init_logging(false);
            let stop = Arc::new(AtomicBool::new(false));
            #[cfg(unix)]
            install_signal_handlers(stop.clone());
            daemon::run_daemon(&opts, stop)
        }
        Cmd::Service => {
            #[cfg(windows)]
            {
                init_logging(true);
                winsvc::run_as_service(opts)
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("`service` is only available on Windows; use `studyd run` (systemd unit: packaging/linux/studyd.service)")
            }
        }
        Cmd::Install => {
            #[cfg(windows)]
            {
                winsvc::install()
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("On Linux use packaging/linux/install.sh (installs binaries, writes /etc/studyd/allowed_users, enables the systemd unit)")
            }
        }
        Cmd::Uninstall => {
            #[cfg(windows)]
            {
                winsvc::uninstall()
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("On Linux use packaging/linux/uninstall.sh")
            }
        }
        Cmd::Cleanup => {
            init_logging(false);
            for w in daemon::cleanup(&opts)? {
                eprintln!("warning: {w}");
            }
            println!("StudyForge hosts section and browser policies removed.");
            Ok(())
        }
        Cmd::EmergencyUnlock => daemon::emergency_unlock(&opts),
        Cmd::Status => {
            let l = daemon::layout(&opts);
            match studyd_lib::transport::request(&l.endpoint, &Request::Status) {
                Ok(Response::Status(s)) => {
                    println!("{}", serde_json::to_string_pretty(&s)?);
                    Ok(())
                }
                Ok(other) => anyhow::bail!("unexpected response: {other:?}"),
                Err(e) => anyhow::bail!("cannot reach studyd at {}: {e}", l.endpoint),
            }
        }
    }
}

#[cfg(unix)]
static STOP_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// SIGTERM / SIGINT stop the loop. Blocks stay in place on purpose: stopping the
/// service must not end a hard-mode lock (the next start re-applies it).
#[cfg(unix)]
fn install_signal_handlers(stop: Arc<AtomicBool>) {
    use std::sync::atomic::Ordering;
    extern "C" fn on_signal(_: libc::c_int) {
        STOP_FLAG.store(true, Ordering::SeqCst);
    }
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: installing a handler that only stores to an atomic is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
    std::thread::spawn(move || {
        while !STOP_FLAG.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        stop.store(true, Ordering::SeqCst);
    });
}
