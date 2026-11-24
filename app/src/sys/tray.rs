//! System tray icon. Shows state through the icon colour and remaining time
//! in the tooltip. On Linux the tray lives on its own GTK thread (tray-icon
//! needs a GTK main loop, which Slint's winit loop does not provide); on
//! Windows it is driven from the Slint (Win32) event loop thread.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Work,
    Break,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ToggleTimer,
    Skip,
    Open,
    Quit,
}

pub type ActionHandler = Arc<dyn Fn(TrayAction) + Send + Sync>;

fn color_for(state: TrayState) -> [u8; 3] {
    match state {
        TrayState::Idle => [150, 156, 170],
        TrayState::Work => [239, 111, 108],
        TrayState::Break => [62, 194, 143],
        TrayState::Paused => [242, 169, 59],
    }
}

/// Procedurally generated 32x32 round icon.
pub fn make_icon(state: TrayState) -> tray_icon::Icon {
    const N: u32 = 32;
    let [r, g, b] = color_for(state);
    let mut rgba = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let d = (dx * dx + dy * dy).sqrt();
            if d <= 14.0 {
                if d > 7.0 && d < 10.0 && dy < 0.0 && dx > -2.0 {
                    rgba.extend_from_slice(&[255, 255, 255, 255]); // clock hand hint
                } else {
                    rgba.extend_from_slice(&[r, g, b, 255]);
                }
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, N, N).expect("valid icon")
}

fn build_menu() -> (muda::Menu, [muda::MenuId; 4]) {
    use muda::{Menu, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    let toggle = MenuItem::new("Start / Pause", true, None);
    let skip = MenuItem::new("Skip phase", true, None);
    let open = MenuItem::new("Open StudyForge", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let ids = [toggle.id().clone(), skip.id().clone(), open.id().clone(), quit.id().clone()];
    let _ = menu.append_items(&[&toggle, &skip, &PredefinedMenuItem::separator(), &open, &quit]);
    (menu, ids)
}

fn install_handlers(ids: [muda::MenuId; 4], handler: ActionHandler) {
    let h = handler.clone();
    muda::MenuEvent::set_event_handler(Some(move |e: muda::MenuEvent| {
        let a = if e.id == ids[0] {
            TrayAction::ToggleTimer
        } else if e.id == ids[1] {
            TrayAction::Skip
        } else if e.id == ids[2] {
            TrayAction::Open
        } else if e.id == ids[3] {
            TrayAction::Quit
        } else {
            return;
        };
        h(a);
    }));
    let h = handler;
    tray_icon::TrayIconEvent::set_event_handler(Some(move |e: tray_icon::TrayIconEvent| {
        if let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = e
        {
            h(TrayAction::Open);
        }
    }));
}

// ---------------- Linux: dedicated GTK thread ----------------
#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::sync::mpsc;

    pub enum Msg {
        Set(TrayState, String),
    }

    #[derive(Clone)]
    pub struct Tray {
        tx: mpsc::Sender<Msg>,
    }

    impl Tray {
        pub fn spawn(handler: ActionHandler) -> Option<Tray> {
            let (tx, rx) = mpsc::channel::<Msg>();
            let (ready_tx, ready_rx) = mpsc::channel::<bool>();
            std::thread::Builder::new()
                .name("tray-gtk".into())
                .spawn(move || {
                    if gtk::init().is_err() {
                        tracing::warn!("GTK init failed; tray disabled");
                        let _ = ready_tx.send(false);
                        return;
                    }
                    let (menu, ids) = build_menu();
                    install_handlers(ids, handler);
                    let icon = tray_icon::TrayIconBuilder::new()
                        .with_menu(Box::new(menu))
                        .with_tooltip("StudyForge")
                        .with_icon(make_icon(TrayState::Idle))
                        .build();
                    let tray = match icon {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::warn!("tray unavailable: {e}");
                            let _ = ready_tx.send(false);
                            return;
                        }
                    };
                    let _ = ready_tx.send(true);
                    let mut last_state = TrayState::Idle;
                    gtk::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                        while let Ok(Msg::Set(state, tip)) = rx.try_recv() {
                            if state != last_state {
                                let _ = tray.set_icon(Some(make_icon(state)));
                                last_state = state;
                            }
                            let _ = tray.set_tooltip(Some(tip));
                            // Some hosts (AppIndicator) show the title instead of a tooltip.
                        }
                        gtk::glib::ControlFlow::Continue
                    });
                    gtk::main();
                })
                .ok()?;
            if ready_rx.recv().unwrap_or(false) {
                Some(Tray { tx })
            } else {
                None
            }
        }

        pub fn set(&self, state: TrayState, tooltip: &str) {
            let _ = self.tx.send(Msg::Set(state, tooltip.to_string()));
        }
    }
}

// ---------------- Windows (and other): main thread ----------------
#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static TRAY: RefCell<Option<tray_icon::TrayIcon>> = const { RefCell::new(None) };
    }

    #[derive(Clone)]
    pub struct Tray;

    impl Tray {
        /// Must be called on the thread running the Slint event loop.
        pub fn spawn(handler: ActionHandler) -> Option<Tray> {
            let (menu, ids) = build_menu();
            install_handlers(ids, handler);
            let t = tray_icon::TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("StudyForge")
                .with_icon(make_icon(TrayState::Idle))
                .build()
                .map_err(|e| tracing::warn!("tray unavailable: {e}"))
                .ok()?;
            TRAY.with(|c| *c.borrow_mut() = Some(t));
            Some(Tray)
        }

        pub fn set(&self, state: TrayState, tooltip: &str) {
            let tip = tooltip.to_string();
            let _ = slint::invoke_from_event_loop(move || {
                TRAY.with(|c| {
                    if let Some(t) = c.borrow().as_ref() {
                        let _ = t.set_icon(Some(make_icon(state)));
                        let _ = t.set_tooltip(Some(tip));
                    }
                });
            });
        }
    }
}

pub use imp::Tray;
