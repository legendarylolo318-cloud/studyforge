//! Windows service integration (compiled on Windows only; verified by CI type-check).

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use studyd_lib::daemon::{self, Options};
use windows_service::service::*;
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const NAME: &str = "studyd";

static OPTIONS: OnceLock<Options> = OnceLock::new();

windows_service::define_windows_service!(ffi_service_main, service_main);

pub fn run_as_service(opts: Options) -> Result<()> {
    let _ = OPTIONS.set(opts);
    service_dispatcher::start(NAME, ffi_service_main).context("not started by the service manager")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service failed: {e:#}");
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    }
}

fn run_service() -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = service_control_handler::register(NAME, move |ev| match ev {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop2.store(true, Ordering::SeqCst);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
    ))?;
    let opts = OPTIONS.get().cloned().unwrap_or_default();
    let result = daemon::run_daemon(&opts, stop);
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()))?;
    result
}

/// SID of the user running this (elevated) terminal.
fn current_user_sid() -> Result<String> {
    let out =
        std::process::Command::new("whoami").args(["/user", "/fo", "csv", "/nh"]).output().context("running whoami")?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split(',')
        .nth(1)
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| s.starts_with("S-1-"))
        .context("could not determine your SID")
}

pub fn install() -> Result<()> {
    let sid = current_user_sid()?;
    let cfg_dir = studyforge_core::paths::daemon_config_dir();
    std::fs::create_dir_all(&cfg_dir)?;
    // Lock the directory down *before* writing allowed_users into it.
    anyhow::ensure!(daemon::secure_state_dir(&cfg_dir, false), "could not restrict access to {}", cfg_dir.display());
    std::fs::write(cfg_dir.join("allowed_users"), format!("# written by `studyd install`\n{sid}\n"))?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)?;
    let info = ServiceInfo {
        name: OsString::from(NAME),
        display_name: OsString::from("StudyForge blocking helper"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description("Enforces StudyForge website/app blocking and hard-mode sessions.")?;
    service.start::<OsString>(&[])?;
    println!("Service '{NAME}' installed and started. Allowed user SID: {sid}");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service =
        manager.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..50 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    service.delete()?;
    // undo hosts / policy changes
    for w in daemon::cleanup(&Options::default())? {
        eprintln!("warning: {w}");
    }
    println!("Service '{NAME}' removed; hosts file and browser policies restored.");
    Ok(())
}
