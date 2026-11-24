//! StudyForge GUI entry point.

mod audio;
mod daemon_link;
mod backend;
mod sys;

use backend::{Cmd, Shared};
use slint::ComponentHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use studyforge_core::{paths, storage};

slint::include_modules!();

fn init_logging() {
    let dir = paths::logs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("studyforge.log"));
    let builder = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    );
    match file {
        Ok(f) => builder.with_ansi(false).with_writer(std::sync::Mutex::new(f)).init(),
        Err(_) => builder.init(),
    }
}

fn pid_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let p = Pid::from_u32(pid);
    System::new().map(|mut s| {
        s.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
        s.process(p).is_some()
    }).unwrap_or(true) // unknown: assume alive (safer for the data)
}

/// Take the data-folder lock, asking the user when somebody else holds it.
/// Returns `None` if the user chose to quit.
fn acquire_data_lock(dir: &std::path::Path) -> anyhow::Result<Option<storage::DataLock>> {
    use std::cell::Cell;
    use std::rc::Rc;
    match storage::DataLock::acquire(dir, &pid_alive)? {
        storage::LockState::Acquired(l) => Ok(Some(l)),
        storage::LockState::HeldByOther { info, stale, same_host } => {
            let when = chrono::DateTime::from_timestamp(info.started, 0)
                .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            let who = if same_host { format!("another StudyForge window on this computer (process {})", info.pid) } else { format!("StudyForge on the computer \"{}\"", info.host) };
            let msg = format!("{} has had {} open since {}.", who[..1].to_uppercase() + &who[1..], dir.display(), when);
            let win = LockWindow::new()?;
            win.set_message(msg.into());
            win.set_can_take_over(stale && !same_host);
            let choice = Rc::new(Cell::new(false));
            {
                let (c, w) = (choice.clone(), win.as_weak());
                win.on_take_over(move || {
                    c.set(true);
                    if let Some(w) = w.upgrade() {
                        let _ = w.hide();
                    }
                });
                let w = win.as_weak();
                win.on_quit(move || {
                    if let Some(w) = w.upgrade() {
                        let _ = w.hide();
                    }
                });
            }
            win.run()?;
            if choice.get() {
                tracing::warn!("taking over a stale data-folder lock from {}", info.host);
                Ok(Some(storage::DataLock::take_over(dir)?))
            } else {
                Ok(None)
            }
        }
    }
}

/// SIGTERM / SIGINT (logout, `kill`) quit the event loop so the backend can restore
/// Do Not Disturb, checkpoint the database and release the data-folder lock.
#[cfg(unix)]
fn install_signal_handlers() {
    use std::sync::atomic::AtomicBool;
    static GOT_SIGNAL: AtomicBool = AtomicBool::new(false);
    extern "C" fn on_signal(_: libc::c_int) {
        GOT_SIGNAL.store(true, Ordering::SeqCst);
    }
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic (async-signal-safe).
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
    std::thread::spawn(|| {
        while !GOT_SIGNAL.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
}

fn main() -> anyhow::Result<()> {
    init_logging();
    #[cfg(unix)]
    install_signal_handlers();
    let data_dir = paths::data_dir();
    std::fs::create_dir_all(&data_dir)?;
    tracing::info!("StudyForge starting, data dir {}", data_dir.display());

    let Some(lock) = acquire_data_lock(&data_dir)? else {
        tracing::info!("data folder is in use; exiting");
        return Ok(());
    };
    let hb_stop = Arc::new(AtomicBool::new(false));
    {
        let beat = lock.heartbeat_fn();
        let stop = hb_stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(storage::HEARTBEAT_EVERY);
                if !stop.load(Ordering::Relaxed) {
                    beat();
                }
            }
        });
    }
    let start_minimized = std::env::args().any(|a| a == "--minimized");

    let ui = AppWindow::new()?;
    let (tx, rx) = mpsc::channel::<Cmd>();
    backend::wire_callbacks(&ui, &tx);
    let overlay = OverlayWindow::new()?;
    backend::wire_overlay(&overlay, &tx);

    // --- tray ---
    let tray_handler: sys::tray::ActionHandler = {
        let tx = tx.clone();
        let weak = ui.as_weak();
        Arc::new(move |action| match action {
            sys::tray::TrayAction::ToggleTimer => {
                let _ = tx.send(Cmd::TogglePause);
            }
            sys::tray::TrayAction::Skip => {
                let _ = tx.send(Cmd::Skip);
            }
            sys::tray::TrayAction::Open => {
                let _ = weak.upgrade_in_event_loop(|ui| {
                    let _ = ui.show();
                });
            }
            sys::tray::TrayAction::Quit => {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
        })
    };
    let tray = sys::tray::Tray::spawn(tray_handler);
    let have_tray = tray.is_some();

    // --- hotkey (event-loop thread) ---
    let hotkey_status = {
        let tx = tx.clone();
        let trigger: sys::hotkey::Trigger = Arc::new(move || {
            let _ = tx.send(Cmd::TogglePause);
        });
        let combo = studyforge_core::db::Db::open(&paths::db_path(&data_dir))
            .and_then(|d| d.load_settings())
            .map(|s| s.hotkey)
            .unwrap_or_else(|_| "Ctrl+Alt+S".into());
        sys::hotkey::Hotkeys::init(&combo, trigger)
    };
    ui.global::<AppState>().set_hotkey_status(hotkey_status.into());
    let hotkey_rebind: Arc<dyn Fn(String) + Send + Sync> = {
        let weak = ui.as_weak();
        Arc::new(move |combo: String| {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let status = sys::hotkey::Hotkeys::rebind(&combo);
                ui.global::<AppState>().set_hotkey_status(status.into());
            });
        })
    };

    // --- close to tray ---
    let shared = Shared { close_to_tray: Arc::new(AtomicBool::new(true)) };
    {
        let flag = shared.close_to_tray.clone();
        ui.window().on_close_requested(move || {
            if flag.load(Ordering::Relaxed) && have_tray {
                slint::CloseRequestResponse::HideWindow
            } else {
                let _ = slint::quit_event_loop();
                slint::CloseRequestResponse::HideWindow
            }
        });
    }

    if let Some(p) = std::env::var("STUDYFORGE_START_PAGE").ok().and_then(|v| v.parse::<i32>().ok()) {
        ui.set_page(p);
        let _ = tx.send(Cmd::PageChanged(p));
    }
    let handle = backend::spawn(ui.as_weak(), data_dir, rx, tray, shared, hotkey_rebind, tx.clone(), overlay.as_weak());
    if !(start_minimized && have_tray) {
        ui.show()?;
    }
    slint::run_event_loop_until_quit()?;
    let _ = tx.send(Cmd::Shutdown);
    let _ = handle.join();
    hb_stop.store(true, Ordering::Relaxed);
    drop(lock); // releases the data-folder lock
    Ok(())
}
