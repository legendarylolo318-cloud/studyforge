//! Blocking from the GUI's point of view: lists, starting/ending blocks with a
//! session, polling the helper, overlay (gentle mode) and the install action.

use super::*;
use crate::{ListItem, OverlayWindow};
use std::collections::VecDeque;
use studyforge_core::blocking::{BlockList, BlockSpec};
use studyforge_core::ipc::*;
use studyforge_core::presets::{normalize_domain, normalize_process};

#[derive(Default)]
pub struct DaemonView {
    pub connected: bool,
    pub status: Option<DaemonStatus>,
    pub error: String,
}

#[derive(Default)]
pub struct OverlayQueue {
    pub current: Option<u64>,
    pub queue: VecDeque<(u64, String, String)>, // id, target, goal
}

pub fn endpoint() -> String {
    std::env::var("STUDYFORGE_SOCKET").unwrap_or_else(|_| studyforge_core::paths::ipc_endpoint())
}

pub fn parse_lines(text: &str) -> Vec<String> {
    text.split(['\n', ',', ';']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

impl Backend {
    // ------------------------------------------------------------ lists
    pub(super) fn reload_blocklists(&mut self) {
        self.blocklists = self.db.blocklists().unwrap_or_default();
        self.selected_lists.retain(|id| self.blocklists.iter().any(|b| b.id == *id));
    }

    pub(super) fn push_blocklists(&self) {
        let items: Vec<ListItem> = self
            .blocklists
            .iter()
            .map(|b| ListItem {
                id: b.id as i32,
                name: b.name.clone().into(),
                preset: b.preset_key.is_some(),
                domains: b.domains.len() as i32,
                processes: b.processes.len() as i32,
                mode: if b.mode == EnforceMode::Gentle { 1 } else { 0 },
                selected: self.selected_lists.contains(&b.id),
                steam: b.steam_games,
                editing: Some(b.id) == self.edit_list,
            })
            .collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_blocklists(ModelRc::new(VecModel::from(items))));
    }

    fn persist_selection(&self) {
        if let Ok(j) = serde_json::to_string(&self.selected_lists) {
            let _ = self.db.set_kv("selected_lists", &j);
        }
    }

    pub(super) fn load_selection(&mut self) {
        self.selected_lists = self.db.get_kv("selected_lists").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    }

    pub(super) fn toggle_list_selected(&mut self, id: Id, on: bool) {
        self.selected_lists.retain(|x| *x != id);
        if on {
            self.selected_lists.push(id);
        }
        self.persist_selection();
        self.push_blocklists();
    }

    pub(super) fn set_list_mode(&mut self, id: Id, mode: i32) {
        let m = if mode == 1 { EnforceMode::Gentle } else { EnforceMode::Hard };
        let _ = self.db.set_blocklist_mode(id, m);
        self.reload_blocklists();
        self.push_blocklists();
    }

    pub(super) fn edit_list(&mut self, id: Id) {
        self.edit_list = Some(id);
        let Some(b) = self.blocklists.iter().find(|b| b.id == id).cloned() else { return };
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_edit_list_id(b.id as i32);
            st.set_edit_name(b.name.clone().into());
            st.set_edit_domains(b.domains.join("\n").into());
            st.set_edit_processes(b.processes.join("\n").into());
            st.set_edit_steam(b.steam_games);
            st.set_edit_mode(if b.mode == EnforceMode::Gentle { 1 } else { 0 });
            st.set_edit_preset(b.preset_key.is_some());
            st.set_edit_error("".into());
            st.set_edit_seq(st.get_edit_seq() + 1);
        });
        self.push_blocklists();
    }

    pub(super) fn new_list(&mut self) {
        let b = Blocklist { id: 0, name: "New list".into(), preset_key: None, domains: vec![], processes: vec![], steam_games: false, mode: EnforceMode::Hard };
        if let Ok(id) = self.db.save_blocklist(&b) {
            self.reload_blocklists();
            self.edit_list(id);
        }
    }

    pub(super) fn save_list(&mut self, id: Id, name: String, domains: String, processes: String, steam: bool, mode: i32) {
        let Some(existing) = self.blocklists.iter().find(|b| b.id == id).cloned() else { return };
        if existing.preset_key.is_some() {
            return;
        }
        let raw = parse_lines(&domains);
        let mut doms: Vec<String> = vec![];
        let mut rejected: Vec<String> = vec![];
        for r in &raw {
            match normalize_domain(r) {
                Some(d) => {
                    if !doms.contains(&d) {
                        doms.push(d)
                    }
                }
                None => rejected.push(r.clone()),
            }
        }
        let mut procs: Vec<String> = vec![];
        for p in parse_lines(&processes) {
            let n = normalize_process(&p);
            if !n.is_empty() && !procs.contains(&n) {
                procs.push(n);
            }
        }
        let b = Blocklist {
            id,
            name: if name.trim().is_empty() { "Untitled".into() } else { name.trim().into() },
            preset_key: None,
            domains: doms,
            processes: procs,
            steam_games: steam,
            mode: if mode == 1 { EnforceMode::Gentle } else { EnforceMode::Hard },
        };
        if let Err(e) = self.db.save_blocklist(&b) {
            tracing::error!("save list: {e}");
        }
        self.reload_blocklists();
        self.edit_list(id);
        if !rejected.is_empty() {
            let msg = format!("Saved. Ignored (not valid domain names; IP addresses are never blocked): {}", rejected.join(", "));
            let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_edit_error(msg.into()));
        }
    }

    pub(super) fn duplicate_list(&mut self, id: Id) {
        if let Ok(Some(new)) = self.db.duplicate_blocklist(id) {
            self.reload_blocklists();
            self.edit_list(new);
        }
    }

    pub(super) fn delete_list(&mut self, id: Id) {
        if self.db.delete_blocklist(id).unwrap_or(false) {
            if self.edit_list == Some(id) {
                self.edit_list = None;
                let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_edit_list_id(-1));
            }
            self.reload_blocklists();
            self.persist_selection();
            self.push_blocklists();
        }
    }

    // ------------------------------------------------------------ spec
    pub(super) fn build_spec(&self, ids: &[Id]) -> BlockSpec {
        let lists: Vec<BlockList> = self.blocklists.iter().filter(|b| ids.contains(&b.id)).map(BlockList::from).collect();
        BlockSpec {
            lists,
            allowlist: Default::default(),
            block_steam_client: self.settings.block_steam_client,
            browser_policies: self.settings.browser_policies,
        }
    }

    // ------------------------------------------------------------ session hooks
    pub(super) fn begin_blocking(&mut self, goal: &str) {
        self.blocking_session = false;
        if self.selected_lists.is_empty() {
            return;
        }
        if !self.daemon.connected {
            self.flash("Blocking helper not available — session started without blocking.");
            return;
        }
        let spec = self.build_spec(&self.selected_lists.clone());
        self.link.send(Request::StartSession(StartSession { goal: goal.to_string(), spec, hard: None }), "start");
        self.blocking_session = true;
    }

    pub(super) fn end_blocking(&mut self) {
        if self.blocking_session {
            self.link.send(Request::StopSession, "stop");
            self.blocking_session = false;
        }
    }

    pub(super) fn flash(&self, msg: &str) {
        let m = msg.to_string();
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_flash(m.into()));
    }

    // ------------------------------------------------------------ daemon traffic
    pub(super) fn on_daemon_status(&mut self, r: Result<DaemonStatus, String>) {
        match r {
            Ok(s) => {
                let was = self.daemon.connected;
                self.daemon = DaemonView { connected: true, status: Some(s), error: String::new() };
                if !was {
                    tracing::info!("connected to studyd {}", self.daemon.status.as_ref().map(|s| s.version.as_str()).unwrap_or("?"));
                    self.on_helper_connected();
                }
            }
            Err(e) => {
                if self.daemon.connected {
                    tracing::info!("lost connection to studyd: {e}");
                }
                self.daemon = DaemonView { connected: false, status: None, error: e };
            }
        }
        self.refresh_helper_ui();
    }

    pub(super) fn on_helper_connected(&mut self) {}

    pub(super) fn on_daemon_events(&mut self, events: Vec<DaemonEvent>) {
        for e in events {
            match e {
                DaemonEvent::Distraction { ts, target, kind, list, action } => {
                    let _ = self.db.log_distraction(ts, &target, kind, &list, &action);
                }
                DaemonEvent::Overlay { id, target, goal, .. } => {
                    self.overlay.queue.push_back((id, target, goal));
                    self.show_next_overlay();
                }
                DaemonEvent::Notice { message, .. } => self.flash(&message),
            }
        }
    }

    pub(super) fn on_daemon_reply(&mut self, tag: &'static str, result: Result<Response, String>) {
        match result {
            Ok(Response::Error { message }) => {
                tracing::warn!("helper refused {tag}: {message}");
                self.flash(&message);
            }
            Err(e) => tracing::warn!("helper request {tag} failed: {e}"),
            Ok(_) => {}
        }
    }

    // ------------------------------------------------------------ overlay
    fn show_next_overlay(&mut self) {
        if self.overlay.current.is_some() {
            return;
        }
        let Some((id, target, goal)) = self.overlay.queue.pop_front() else { return };
        self.overlay.current = Some(id);
        let theme = (self.settings.theme, self.settings.accent);
        let _ = self.overlay_win.upgrade_in_event_loop(move |w: OverlayWindow| {
            w.set_goal(goal.into());
            w.set_target(target.into());
            let t = w.global::<crate::Theme>();
            t.set_mode(match theme.0 {
                ThemeMode::Dark => 0,
                ThemeMode::Light => 1,
                ThemeMode::System => 2,
            });
            t.set_accent_index(theme.1 as i32);
            let _ = w.show();
        });
    }

    pub(super) fn overlay_choice(&mut self, choice: i32) {
        let Some(id) = self.overlay.current.take() else { return };
        let c = match choice {
            0 => OverlayChoice::CloseIt,
            1 => OverlayChoice::FiveMore,
            _ => OverlayChoice::Dismiss,
        };
        self.link.send(Request::OverlayAction { id, choice: c }, "overlay");
        if self.overlay.queue.is_empty() {
            let _ = self.overlay_win.upgrade_in_event_loop(|w| {
                let _ = w.hide();
            });
        } else {
            self.show_next_overlay();
        }
    }

    // ------------------------------------------------------------ helper status UI
    pub(super) fn refresh_helper_ui(&mut self) {
        let (connected, status_line, detail, dash) = match (&self.daemon.status, self.daemon.connected) {
            (Some(s), true) => {
                let mut parts = vec![format!("studyd {}", s.version)];
                if let Some(a) = &s.active {
                    parts.push(format!("session block active ({})", a.lists.join(", ")));
                    if a.hard.is_some() {
                        parts.push("hard mode".into());
                    }
                }
                if !s.schedules_active.is_empty() {
                    parts.push(format!("schedule: {}", s.schedules_active.join(", ")));
                }
                if s.dry_run {
                    parts.push("DRY RUN".into());
                }
                let dash = if s.active.is_some() || !s.schedules_active.is_empty() {
                    let mut v = vec![];
                    if let Some(a) = &s.active {
                        v.push(format!("Session block: {}", if a.lists.is_empty() { "custom".into() } else { a.lists.join(", ") }));
                    }
                    if !s.schedules_active.is_empty() {
                        v.push(format!("Schedule: {}", s.schedules_active.join(", ")));
                    }
                    v.join(" · ")
                } else {
                    format!("No block active · {} schedule rule(s)", s.schedules_total)
                };
                (true, parts.join(" · "), s.warnings.join(" "), dash)
            }
            _ => (false, String::new(), String::new(), "Blocking helper not installed".to_string()),
        };
        self.block_status = dash.clone();
        let sidebar = if connected { "Helper: connected" } else { "Helper: not installed" };
        let note = install_note();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_helper_connected(connected);
            st.set_helper_status(status_line.into());
            st.set_helper_detail(detail.into());
            st.set_helper_install_note(note.into());
            st.set_status_line(sidebar.into());
            st.set_dash_block_status(dash.into());
        });
    }

    pub(super) fn install_helper(&mut self) {
        let msg = try_install_helper();
        self.flash(&msg);
    }
}

pub fn install_note() -> String {
    if cfg!(windows) {
        "Install: open an administrator terminal in the StudyForge folder and run `studyd.exe install`.".to_string()
    } else {
        "Install: run `sudo ./packaging/linux/install.sh` from the StudyForge source/package folder (installs studyd to /usr/local/bin, writes /etc/studyd/allowed_users and enables the systemd service).".to_string()
    }
}

fn try_install_helper() -> String {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("studyd.exe")));
        match exe {
            Some(e) if e.exists() => {
                let arg = format!("Start-Process -FilePath '{}' -ArgumentList 'install' -Verb RunAs", e.display());
                match std::process::Command::new("powershell").args(["-NoProfile", "-Command", &arg]).spawn() {
                    Ok(_) => "Approve the administrator prompt to install the helper.".into(),
                    Err(e) => format!("Could not start the installer: {e}"),
                }
            }
            _ => install_note(),
        }
    }
    #[cfg(not(windows))]
    {
        let mut candidates: Vec<std::path::PathBuf> = vec![];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                candidates.push(d.join("install.sh"));
                candidates.push(d.join("../share/studyforge/install.sh"));
                candidates.push(d.join("../../packaging/linux/install.sh"));
            }
        }
        candidates.push("/usr/share/studyforge/install.sh".into());
        candidates.push("packaging/linux/install.sh".into());
        // SAFETY: getuid cannot fail
        let uid = unsafe { libc_getuid() };
        match candidates.into_iter().find(|p| p.exists()) {
            Some(script) => match std::process::Command::new("pkexec").arg("sh").arg(&script).arg(uid.to_string()).spawn() {
                Ok(_) => "Approve the authentication prompt to install the helper. This page updates when it is running.".into(),
                Err(_) => install_note(),
            },
            None => install_note(),
        }
    }
}

#[cfg(not(windows))]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    getuid()
}
