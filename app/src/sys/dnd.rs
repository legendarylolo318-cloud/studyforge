#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
//! Do Not Disturb during focus phases (best effort, never fatal).
//!
//! * GNOME: `gsettings org.gnome.desktop.notifications show-banners` is set to
//!   false and the previous value is restored afterwards. The previous value is
//!   also returned as a *token* the caller persists, so a crash can be repaired
//!   on the next start.
//! * KDE: `org.freedesktop.Notifications.Inhibit` over D-Bus. The inhibition is
//!   tied to our bus connection, so it disappears by itself if we crash.
//! * Anything else (incl. Windows): reported as unsupported. Windows has no
//!   public Focus Assist API and we do not use undocumented hacks.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Kde,
    Other,
}

pub fn detect_desktop(xdg_current_desktop: &str) -> Desktop {
    let d = xdg_current_desktop.to_uppercase();
    if d.split(':').any(|p| p == "GNOME" || p == "UNITY" || p == "X-CINNAMON-NOPE") {
        Desktop::Gnome
    } else if d.split(':').any(|p| p == "KDE") {
        Desktop::Kde
    } else {
        Desktop::Other
    }
}

pub trait Dnd: Send {
    fn name(&self) -> &'static str;
    fn supported(&self) -> bool;
    /// Turn DND on. Returns a restore token to persist (if the method needs one).
    fn enable(&mut self) -> Result<Option<String>, String>;
    fn restore(&mut self) -> Result<(), String>;
    /// Repair state left behind by a crashed run.
    fn restore_from_token(&mut self, token: &str) -> Result<(), String>;
}

pub struct NoDnd;
impl Dnd for NoDnd {
    fn name(&self) -> &'static str {
        "unsupported"
    }
    fn supported(&self) -> bool {
        false
    }
    fn enable(&mut self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn restore(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn restore_from_token(&mut self, _t: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Runs `gsettings`; injectable for tests.
pub trait Gsettings: Send {
    fn get(&self) -> Result<String, String>;
    fn set(&self, value: bool) -> Result<(), String>;
}

pub struct RealGsettings;
impl Gsettings for RealGsettings {
    fn get(&self) -> Result<String, String> {
        let out = std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.notifications", "show-banners"])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("gsettings failed".into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    fn set(&self, value: bool) -> Result<(), String> {
        let st = std::process::Command::new("gsettings")
            .args(["set", "org.gnome.desktop.notifications", "show-banners", if value { "true" } else { "false" }])
            .status()
            .map_err(|e| e.to_string())?;
        st.success().then_some(()).ok_or_else(|| "gsettings set failed".into())
    }
}

pub struct GnomeDnd<G: Gsettings> {
    g: G,
    previous: Option<bool>,
}

impl<G: Gsettings> GnomeDnd<G> {
    pub fn new(g: G) -> Self {
        Self { g, previous: None }
    }
}

impl<G: Gsettings> Dnd for GnomeDnd<G> {
    fn name(&self) -> &'static str {
        "GNOME"
    }
    fn supported(&self) -> bool {
        self.g.get().is_ok()
    }
    fn enable(&mut self) -> Result<Option<String>, String> {
        if let Some(p) = self.previous {
            return Ok(Some(p.to_string()));
        }
        let prev = self.g.get()? == "true";
        self.g.set(false)?;
        self.previous = Some(prev);
        Ok(Some(prev.to_string()))
    }
    fn restore(&mut self) -> Result<(), String> {
        if let Some(p) = self.previous.take() {
            self.g.set(p)?;
        }
        Ok(())
    }
    fn restore_from_token(&mut self, token: &str) -> Result<(), String> {
        self.g.set(token != "false")
    }
}

#[cfg(target_os = "linux")]
pub struct KdeDnd {
    rt: tokio::runtime::Runtime,
    conn: Option<zbus::Connection>,
    cookie: Option<u32>,
}

#[cfg(target_os = "linux")]
impl KdeDnd {
    pub fn new() -> Option<Self> {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        Some(Self { rt, conn: None, cookie: None })
    }

    fn call(
        &mut self,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> Result<zbus::Message, String> {
        let conn = match &self.conn {
            Some(c) => c.clone(),
            None => {
                let c = self.rt.block_on(zbus::Connection::session()).map_err(|e| e.to_string())?;
                self.conn = Some(c.clone());
                c
            }
        };
        self.rt
            .block_on(conn.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                method,
                body,
            ))
            .map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "linux")]
impl Dnd for KdeDnd {
    fn name(&self) -> &'static str {
        "KDE"
    }
    fn supported(&self) -> bool {
        true
    }
    fn enable(&mut self) -> Result<Option<String>, String> {
        if self.cookie.is_some() {
            return Ok(None);
        }
        let hints: std::collections::HashMap<&str, zbus::Variant> = std::collections::HashMap::new();
        let reply = self.call("Inhibit", &("StudyForge", "Focus session", hints))?;
        let cookie: u32 = reply.body().deserialize().map_err(|e| e.to_string())?;
        self.cookie = Some(cookie);
        Ok(None) // tied to our connection: nothing to persist
    }
    fn restore(&mut self) -> Result<(), String> {
        if let Some(c) = self.cookie.take() {
            self.call("UnInhibit", &(c,))?;
        }
        Ok(())
    }
    fn restore_from_token(&mut self, _t: &str) -> Result<(), String> {
        Ok(())
    }
}

pub fn detect() -> Box<dyn Dnd> {
    #[cfg(target_os = "linux")]
    {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        match detect_desktop(&desktop) {
            Desktop::Gnome => {
                let g = GnomeDnd::new(RealGsettings);
                if g.supported() {
                    return Box::new(g);
                }
            }
            Desktop::Kde => {
                if let Some(k) = KdeDnd::new() {
                    return Box::new(k);
                }
            }
            Desktop::Other => {}
        }
    }
    Box::new(NoDnd)
}

pub fn status_text(d: &dyn Dnd) -> String {
    if d.supported() {
        format!("Do Not Disturb is available on this desktop ({}).", d.name())
    } else if cfg!(windows) {
        "Not supported on this desktop (Windows has no public Focus Assist API).".to_string()
    } else {
        "Not supported on this desktop (only GNOME and KDE are handled).".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Fake(Arc<Mutex<String>>, Arc<Mutex<Vec<bool>>>);
    impl Gsettings for Fake {
        fn get(&self) -> Result<String, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set(&self, v: bool) -> Result<(), String> {
            *self.0.lock().unwrap() = v.to_string();
            self.1.lock().unwrap().push(v);
            Ok(())
        }
    }
    fn fake(initial: &str) -> Fake {
        Fake(Arc::new(Mutex::new(initial.into())), Arc::new(Mutex::new(vec![])))
    }

    #[test]
    fn desktop_detection() {
        assert_eq!(detect_desktop("ubuntu:GNOME"), Desktop::Gnome);
        assert_eq!(detect_desktop("KDE"), Desktop::Kde);
        assert_eq!(detect_desktop("Hyprland"), Desktop::Other);
        assert_eq!(detect_desktop(""), Desktop::Other);
        assert_eq!(detect_desktop("X-Cinnamon"), Desktop::Other);
    }

    #[test]
    fn gnome_restores_previous_value_true() {
        let f = fake("true");
        let mut d = GnomeDnd::new(f.clone());
        assert_eq!(d.enable().unwrap().as_deref(), Some("true"));
        assert_eq!(*f.0.lock().unwrap(), "false");
        d.restore().unwrap();
        assert_eq!(*f.0.lock().unwrap(), "true");
    }

    #[test]
    fn gnome_restores_previous_value_false() {
        // the user already had banners off: must stay off afterwards
        let f = fake("false");
        let mut d = GnomeDnd::new(f.clone());
        d.enable().unwrap();
        d.restore().unwrap();
        assert_eq!(*f.0.lock().unwrap(), "false");
    }

    #[test]
    fn gnome_enable_is_idempotent_and_restore_without_enable_is_noop() {
        let f = fake("true");
        let mut d = GnomeDnd::new(f.clone());
        d.restore().unwrap();
        assert!(f.1.lock().unwrap().is_empty());
        d.enable().unwrap();
        d.enable().unwrap();
        assert_eq!(f.1.lock().unwrap().len(), 1, "second enable must not clobber the saved original");
        d.restore().unwrap();
        assert_eq!(*f.0.lock().unwrap(), "true");
    }

    #[test]
    fn crash_recovery_from_token() {
        let f = fake("false"); // left over from a crashed run
        let mut d = GnomeDnd::new(f.clone());
        d.restore_from_token("true").unwrap();
        assert_eq!(*f.0.lock().unwrap(), "true");
    }

    #[test]
    fn unsupported_is_harmless() {
        let mut d = NoDnd;
        assert!(!d.supported());
        assert!(d.enable().unwrap().is_none());
        d.restore().unwrap();
        assert!(status_text(&d).contains("Not supported"));
    }
}
