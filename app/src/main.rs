//! StudyForge GUI entry point.

mod audio;
mod daemon_link;
mod backend;
mod sys;

use backend::{Cmd, Shared};
use slint::ComponentHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use studyforge_core::paths;

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

fn main() -> anyhow::Result<()> {
    init_logging();
    let data_dir = paths::data_dir();
    std::fs::create_dir_all(&data_dir)?;
    tracing::info!("StudyForge starting, data dir {}", data_dir.display());

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
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    let _ = tx.send(Cmd::Shutdown);
    let _ = handle.join();
    Ok(())
}
