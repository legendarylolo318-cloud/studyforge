//! Global start/pause hotkey.
//!
//! * Windows / X11: `global-hotkey` (must live on the Slint event-loop thread).
//! * Wayland: the XDG GlobalShortcuts portal through `ashpd`. If the portal is
//!   missing, the feature is disabled with an explanatory status message.

use std::cell::RefCell;
use std::sync::Arc;

pub type Trigger = Arc<dyn Fn() + Send + Sync>;

thread_local! {
    static MANAGER: RefCell<Option<global_hotkey::GlobalHotKeyManager>> = const { RefCell::new(None) };
    static CURRENT: RefCell<Option<global_hotkey::hotkey::HotKey>> = const { RefCell::new(None) };
}

#[cfg(target_os = "linux")]
fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").map(|v| v.eq_ignore_ascii_case("wayland")).unwrap_or(false)
        || (std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none())
}

#[cfg(not(target_os = "linux"))]
fn is_wayland() -> bool {
    false
}

/// Convert "Ctrl+Alt+S" into the portal's trigger syntax ("CTRL+ALT+s").
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn portal_trigger(combo: &str) -> String {
    combo
        .split('+')
        .map(|p| {
            let p = p.trim();
            match p.to_lowercase().as_str() {
                "ctrl" | "control" => "CTRL".to_string(),
                "alt" => "ALT".to_string(),
                "shift" => "SHIFT".to_string(),
                "super" | "meta" | "win" | "cmd" => "LOGO".to_string(),
                _ => p.to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Normalize user text for global-hotkey's parser ("Ctrl+Alt+S" works as-is,
/// but accept spaces and case differences).
fn parse(combo: &str) -> Result<global_hotkey::hotkey::HotKey, String> {
    let cleaned: String = combo.split('+').map(|p| p.trim()).collect::<Vec<_>>().join("+");
    cleaned.parse::<global_hotkey::hotkey::HotKey>().map_err(|e| format!("Invalid hotkey '{combo}': {e}"))
}

#[cfg(not(target_os = "linux"))]
mod wayland {
    use super::Trigger;
    pub fn start(_combo: &str, _trigger: Trigger) -> String {
        String::new()
    }
    pub fn rebind(_combo: &str) -> String {
        String::new()
    }
}

pub struct Hotkeys;

impl Hotkeys {
    /// Initialize. Returns a human-readable status string for Settings.
    /// Call on the event-loop thread.
    pub fn init(combo: &str, trigger: Trigger) -> String {
        if is_wayland() {
            return wayland::start(combo, trigger);
        }
        match global_hotkey::GlobalHotKeyManager::new() {
            Ok(m) => {
                MANAGER.with(|c| *c.borrow_mut() = Some(m));
                global_hotkey::GlobalHotKeyEvent::set_event_handler(Some(
                    move |e: global_hotkey::GlobalHotKeyEvent| {
                        if e.state == global_hotkey::HotKeyState::Pressed {
                            trigger();
                        }
                    },
                ));
                Self::rebind(combo)
            }
            Err(e) => format!("Global hotkey unavailable: {e}"),
        }
    }

    /// Rebind on the event-loop thread. Returns status text.
    pub fn rebind(combo: &str) -> String {
        if is_wayland() {
            return wayland::rebind(combo);
        }
        let hk = match parse(combo) {
            Ok(h) => h,
            Err(e) => return e,
        };
        MANAGER.with(|m| {
            let m = m.borrow();
            let Some(m) = m.as_ref() else { return "Global hotkey unavailable".to_string() };
            CURRENT.with(|c| {
                if let Some(old) = c.borrow_mut().take() {
                    let _ = m.unregister(old);
                }
            });
            match m.register(hk) {
                Ok(()) => {
                    CURRENT.with(|c| *c.borrow_mut() = Some(hk));
                    format!("Hotkey active: {combo}")
                }
                Err(e) => format!("Could not register '{combo}': {e}"),
            }
        })
    }
}

#[cfg(target_os = "linux")]
mod wayland {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static STATUS: OnceLock<Mutex<String>> = OnceLock::new();

    fn status() -> &'static Mutex<String> {
        STATUS.get_or_init(|| Mutex::new(String::new()))
    }

    pub fn start(combo: &str, trigger: Trigger) -> String {
        let combo = combo.to_string();
        *status().lock().unwrap() = "Waiting for the GlobalShortcuts portal…".into();
        std::thread::Builder::new()
            .name("hotkey-portal".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(r) => r,
                    Err(e) => {
                        *status().lock().unwrap() = format!("Hotkey disabled: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    if let Err(e) = run(&combo, trigger).await {
                        tracing::warn!("GlobalShortcuts portal unavailable: {e}");
                        *status().lock().unwrap() = format!(
                            "Global hotkey disabled: this Wayland desktop has no GlobalShortcuts portal ({e}). \
                             Use the tray menu instead, or bind a key in your compositor to `studyforge --toggle`."
                        );
                    }
                });
            })
            .ok();
        status().lock().unwrap().clone()
    }

    async fn run(combo: &str, trigger: Trigger) -> Result<(), ashpd::Error> {
        use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
        use futures_util::StreamExt;
        let gs = GlobalShortcuts::new().await?;
        let session = gs.create_session(Default::default()).await?;
        let sc = NewShortcut::new("toggle-timer", "Start or pause the focus timer")
            .preferred_trigger(Some(portal_trigger(combo).as_str()));
        gs.bind_shortcuts(&session, &[sc], None, Default::default()).await?;
        *status().lock().unwrap() = format!("Hotkey bound through the desktop portal (preferred: {combo}). Change it in your desktop's shortcut settings.");
        let mut stream = gs.receive_activated().await?;
        while let Some(a) = stream.next().await {
            if a.shortcut_id() == "toggle-timer" {
                trigger();
            }
        }
        Ok(())
    }

    pub fn rebind(_combo: &str) -> String {
        "On Wayland, change the shortcut in your desktop's shortcut settings (portal-managed).".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portal_syntax() {
        assert_eq!(portal_trigger("Ctrl+Alt+S"), "CTRL+ALT+s");
        assert_eq!(portal_trigger("Super + Shift + F5"), "LOGO+SHIFT+f5");
    }
}
