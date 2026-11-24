//! Schedules, hard mode, Do Not Disturb, autostart, data folder and backups.

use super::*;
use crate::sys::{autostart, dnd};
use crate::ScheduleItem;
use chrono::Local;
use std::path::PathBuf;
use studyforge_core::hardmode::HardModeConfig;
use studyforge_core::ipc::{ActiveInfo, Request};
use studyforge_core::schedule::{self, ScheduleDef};
use studyforge_core::storage;

pub fn fmt_secs(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

impl Backend {
    // ------------------------------------------------------------ schedules
    fn schedule_defs(&self) -> Vec<ScheduleDef> {
        self.db
            .schedules()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, json)| serde_json::from_str::<ScheduleDef>(&json).ok().map(|mut d| {
                d.id = id;
                d
            }))
            .collect()
    }

    pub(super) fn push_schedules_ui(&self) {
        let defs = self.schedule_defs();
        let items: Vec<ScheduleItem> = defs
            .iter()
            .map(|d| ScheduleItem {
                id: d.id as i32,
                name: d.list_names(&self.blocklists).into(),
                days: schedule::fmt_days(d.days).into(),
                time: d.time_text().into(),
                lists: d.list_names(&self.blocklists).into(),
                enabled: d.enabled,
            })
            .collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_schedules(ModelRc::new(VecModel::from(items))));
    }

    /// Send the resolved rules to studyd (on change, on reconnect, when lists change).
    pub(super) fn push_schedules_to_daemon(&self) {
        if !self.daemon.connected {
            return;
        }
        let rules = schedule::resolve_defs(&self.schedule_defs(), &self.blocklists, self.settings.browser_policies, self.settings.block_steam_client);
        self.link.send(Request::SetSchedules { rules }, "schedules");
    }

    pub(super) fn add_schedule(&mut self, list_idx: i32, days: i32, start: String, end: String) {
        let err = |me: &Self, m: &str| {
            let m = m.to_string();
            let _ = me.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_schedule_error(m.into()));
        };
        let Some(list) = self.blocklists.get(list_idx.max(0) as usize) else { return err(self, "Pick a list first.") };
        if days & 0x7f == 0 {
            return err(self, "Pick at least one day.");
        }
        let (Some(s), Some(e)) = (schedule::parse_hhmm(&start), schedule::parse_hhmm(&end)) else {
            return err(self, "Times must look like 09:00 (24-hour).");
        };
        let def = ScheduleDef { id: 0, enabled: true, days: (days & 0x7f) as u8, start_min: s, end_min: e, list_ids: vec![list.id] };
        if let Ok(json) = serde_json::to_string(&def) {
            let _ = self.db.add_schedule_json(&json);
        }
        err(self, "");
        self.push_schedules_ui();
        self.push_schedules_to_daemon();
    }

    pub(super) fn toggle_schedule(&mut self, id: Id, on: bool) {
        if let Some(mut d) = self.schedule_defs().into_iter().find(|d| d.id == id) {
            d.enabled = on;
            if let Ok(json) = serde_json::to_string(&d) {
                let _ = self.db.update_schedule_json(id, &json);
            }
        }
        self.push_schedules_ui();
        self.push_schedules_to_daemon();
    }

    pub(super) fn delete_schedule(&mut self, id: Id) {
        let _ = self.db.delete_schedule(id);
        self.push_schedules_ui();
        self.push_schedules_to_daemon();
    }

    // ------------------------------------------------------------ hard mode
    pub(super) fn hard_config(&self, mins: i32) -> HardModeConfig {
        let deadline = chrono::Utc::now().timestamp() + mins.clamp(5, 1440) as i64 * 60;
        HardModeConfig::new(deadline, self.settings.early_exit_challenge, self.settings.hard_mode_cooldown_mins, self.settings.emergency_cooldown_mins)
    }

    /// Mirror the helper's hard-mode state into the UI and notice when it ends.
    pub(super) fn push_hard(&mut self) {
        let now = chrono::Utc::now().timestamp();
        let hard = self.daemon.status.as_ref().and_then(|s| s.active.as_ref().and_then(|a: &ActiveInfo| a.hard.clone()));
        let active = hard.is_some();
        if self.hard_was_active && !active && self.daemon.connected && self.timer.is_active() {
            // The lock ended (deadline, cooldown or emergency unlock): the focus session is over too.
            self.stop();
        }
        self.hard_was_active = active;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            match hard {
                Some(h) => {
                    let until = chrono::DateTime::from_timestamp(h.deadline, 0).map(|t| t.with_timezone(&Local).format("%H:%M").to_string()).unwrap_or_default();
                    st.set_hard_active(true);
                    st.set_hard_until(until.into());
                    st.set_hard_left(fmt_secs(h.deadline - now).into());
                    st.set_hard_challenge_enabled(h.challenge_enabled);
                    st.set_hard_challenge(h.challenge.clone().unwrap_or_default().into());
                    st.set_hard_cooldown(h.exit_cooldown_remaining.map(|r| format!("{} left", fmt_secs(r))).unwrap_or_default().into());
                    st.set_hard_emergency(h.emergency_remaining.map(|r| format!("{} left", fmt_secs(r))).unwrap_or_default().into());
                    st.set_hard_cooldown_mins(h.exit_cooldown_mins as i32);
                    st.set_hard_emergency_mins(h.emergency_cooldown_mins as i32);
                }
                None => {
                    st.set_hard_active(false);
                    st.set_hard_error("".into());
                }
            }
        });
    }

    pub(super) fn hard_request(&self, req: Request, tag: &'static str) {
        self.link.send(req, tag);
    }

    pub(super) fn set_hard_error(&self, msg: String) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_hard_error(msg.into()));
    }

    // ------------------------------------------------------------ DND
    pub(super) fn init_dnd(&mut self) {
        if let Ok(Some(token)) = self.db.get_kv("dnd_restore") {
            // A previous run crashed while DND was on: put the user's setting back.
            if let Err(e) = self.dnd.restore_from_token(&token) {
                tracing::warn!("DND crash recovery failed: {e}");
            }
            let _ = self.db.del_kv("dnd_restore");
        }
        let text = dnd::status_text(self.dnd.as_ref());
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_dnd_status(text.into()));
    }

    pub(super) fn dnd_enable(&mut self) {
        if !self.settings.dnd_during_work || !self.dnd.supported() {
            return;
        }
        match self.dnd.enable() {
            Ok(Some(token)) => {
                let _ = self.db.set_kv("dnd_restore", &token);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("cannot enable Do Not Disturb: {e}"),
        }
    }

    pub(super) fn dnd_restore(&mut self) {
        if let Err(e) = self.dnd.restore() {
            tracing::warn!("cannot restore Do Not Disturb: {e}");
        }
        let _ = self.db.del_kv("dnd_restore");
    }

    // ------------------------------------------------------------ autostart
    pub(super) fn sync_autostart(&mut self) {
        if self.settings.autostart != autostart::is_enabled() {
            if let Err(e) = autostart::set(self.settings.autostart) {
                tracing::warn!("autostart: {e}");
            }
        }
    }

    // ------------------------------------------------------------ data folder
    pub(super) fn push_data_dir(&self) {
        let cur = self.data_dir.display().to_string();
        let custom = studyforge_core::paths::data_dir() != studyforge_core::paths::default_data_dir();
        let text = if custom { format!("{cur}  (custom)") } else { format!("{cur}  (default)") };
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_data_dir(text.into()));
    }

    fn data_note(&self, msg: String) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_data_note(msg.into()));
    }

    pub(super) fn choose_data_dir(&mut self) {
        let tx = self.self_tx.clone();
        std::thread::spawn(move || {
            if let Some(p) = rfd::FileDialog::new().set_title("Choose the StudyForge data folder").pick_folder() {
                let _ = tx.send(Cmd::SetDataDir(Some(p)));
            }
        });
    }

    pub(super) fn set_data_dir(&mut self, target: Option<PathBuf>) {
        match target {
            Some(p) => {
                if p == self.data_dir {
                    return;
                }
                if let Err(e) = std::fs::create_dir_all(&p) {
                    return self.data_note(format!("Cannot use that folder: {e}"));
                }
                let target_db = studyforge_core::paths::db_path(&p);
                if !target_db.exists() {
                    if let Err(e) = self.db.backup_to(&target_db) {
                        return self.data_note(format!("Could not copy your data there: {e}"));
                    }
                }
                match studyforge_core::paths::set_custom_data_dir(Some(&p)) {
                    Ok(()) => self.data_note(format!("Data folder set to {}. Restart StudyForge to use it (your current data was copied if the folder had none).", p.display())),
                    Err(e) => self.data_note(format!("Could not save the setting: {e}")),
                }
            }
            None => match studyforge_core::paths::set_custom_data_dir(None) {
                Ok(()) => self.data_note("Data folder reset to the default. Restart StudyForge to use it.".into()),
                Err(e) => self.data_note(format!("Could not reset: {e}")),
            },
        }
        self.push_data_dir();
    }

    // ------------------------------------------------------------ backups
    pub(super) fn run_daily_backup(&mut self) {
        self.last_backup_check = Instant::now();
        if !self.settings.backup_daily {
            return;
        }
        let dir = studyforge_core::paths::backups_dir(&self.data_dir);
        match storage::run_backup(&self.db, &dir, Local::now().date_naive(), storage::KEEP_BACKUPS) {
            Ok(Some(p)) => tracing::info!("daily backup written: {}", p.display()),
            Ok(None) => {}
            Err(e) => tracing::warn!("backup failed: {e}"),
        }
    }

    pub(super) fn backup_now(&mut self) {
        let dir = studyforge_core::paths::backups_dir(&self.data_dir);
        let today = Local::now().date_naive();
        let _ = std::fs::remove_file(dir.join(storage::backup_file_name(today)));
        match storage::run_backup(&self.db, &dir, today, storage::KEEP_BACKUPS) {
            Ok(Some(p)) => self.data_note(format!("Backup saved: {}", p.display())),
            Ok(None) => self.data_note("Backup already exists for today.".into()),
            Err(e) => self.data_note(format!("Backup failed: {e}")),
        }
    }
}
