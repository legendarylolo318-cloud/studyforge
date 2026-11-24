//! The worker thread: owns the database and the timer, never touches the UI
//! directly. UI updates are pushed with `upgrade_in_event_loop`.

use crate::audio::{self, Audio, AudioCmd, TrackInfo};
use crate::sys::tray::{Tray, TrayState};
use crate::{AppState, AppWindow, SettingsData, SubjectItem, TipItem, TrackItem};
use std::collections::HashMap;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use studyforge_core::db::Db;
use studyforge_core::models::*;
use studyforge_core::settings::{Settings, ThemeMode};
use studyforge_core::timer::{fmt_clock, Mode, Phase, Stamp, Timer, TimerConfig, TimerEvent};

pub enum Cmd {
    Start { mode: i32, subject_idx: i32, goal: String },
    TogglePause,
    Skip,
    Stop,
    SubmitOutcome { outcome: i32, note: String },
    SetSetting { key: String, value: serde_json::Value },
    AddSubject { name: String, color: String },
    DeleteSubject(i64),
    AddTip(String),
    DeleteTip(i64),
    SetTrackVolume { id: String, volume: f32 },
    ToggleSounds,
    OpenSoundsFolder,
    RescanSounds,
    Shutdown,
}

/// Shared flags read on the UI thread.
#[derive(Clone)]
pub struct Shared {
    pub close_to_tray: Arc<AtomicBool>,
}

pub struct Backend {
    ui: Weak<AppWindow>,
    db: Db,
    settings: Settings,
    timer: Timer,
    session_id: Option<Id>,
    session_goal: String,
    pending_outcome: Option<Id>,
    subjects: Vec<Subject>,
    tips: Vec<BreakTip>,
    tip_idx: usize,
    tray: Option<Tray>,
    shared: Shared,
    last_progress_save: Instant,
    last_tray: (TrayState, String),
    hotkey_rebind: Arc<dyn Fn(String) + Send + Sync>,
    audio: Audio,
    tracks: Vec<TrackInfo>,
    mix: HashMap<String, f32>,
    sounds_dir: PathBuf,
    sounds_playing: bool,
}

pub fn stamp() -> Stamp {
    Stamp { mono: Instant::now(), wall: chrono::Utc::now().timestamp() }
}

fn timer_cfg(s: &Settings) -> TimerConfig {
    TimerConfig {
        work: Duration::from_secs(s.work_mins as u64 * 60),
        short_break: Duration::from_secs(s.short_break_mins as u64 * 60),
        long_break: Duration::from_secs(s.long_break_mins as u64 * 60),
        cycles_before_long: s.cycles_before_long,
        auto_start_next: s.auto_start_next,
        flow_reminder: (s.flow_reminder_mins > 0).then(|| Duration::from_secs(s.flow_reminder_mins as u64 * 60)),
    }
}

pub fn parse_color(s: &str) -> slint::Color {
    let h = s.trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8);
        }
    }
    slint::Color::from_rgb_u8(108, 140, 255)
}

fn color_hex(c: slint::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

pub fn spawn(
    ui: Weak<AppWindow>,
    data_dir: PathBuf,
    rx: Receiver<Cmd>,
    tray: Option<Tray>,
    shared: Shared,
    hotkey_rebind: Arc<dyn Fn(String) + Send + Sync>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("backend".into())
        .spawn(move || match Db::open(&studyforge_core::paths::db_path(&data_dir)) {
            Ok(db) => {
                let mut b = Backend::new(ui, db, tray, shared, hotkey_rebind, data_dir.clone());
                b.run(rx);
            }
            Err(e) => {
                tracing::error!("cannot open database: {e}");
                let msg = format!("Database error: {e}");
                let _ = ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_status_line(msg.into()));
            }
        })
        .expect("spawn backend")
}

impl Backend {
    fn new(
        ui: Weak<AppWindow>,
        db: Db,
        tray: Option<Tray>,
        shared: Shared,
        hotkey_rebind: Arc<dyn Fn(String) + Send + Sync>,
        data_dir: PathBuf,
    ) -> Self {
        let settings = db.load_settings().unwrap_or_default();
        if let Ok(n) = db.close_dangling_sessions() {
            if n > 0 {
                tracing::info!("closed {n} dangling session(s) from a previous crash");
            }
        }
        let timer = Timer::new(timer_cfg(&settings));
        let ui_for_audio = ui.clone();
        let mut b = Backend {
            ui,
            db,
            settings,
            timer,
            session_id: None,
            session_goal: String::new(),
            pending_outcome: None,
            subjects: vec![],
            tips: vec![],
            tip_idx: 0,
            tray,
            shared,
            last_progress_save: Instant::now(),
            last_tray: (TrayState::Idle, String::new()),
            hotkey_rebind,
            audio: {
                let w = ui_for_audio;
                Audio::spawn(move |msg| {
                    let _ = w.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_audio_status(msg.into()));
                })
            },
            tracks: vec![],
            mix: HashMap::new(),
            sounds_dir: studyforge_core::paths::sounds_dir(&data_dir),
            sounds_playing: false,
        };
        let _ = std::fs::create_dir_all(&b.sounds_dir);
        b.mix = b.db.get_kv("sound_mix").ok().flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        b.audio.send(AudioCmd::Master(b.settings.master_volume));
        b.rescan_sounds();
        b.reload_lists();
        b.push_settings();
        b.push_lists();
        b.push_timer();
        b
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Cmd::Shutdown) => {
                    self.shutdown();
                    return;
                }
                Ok(cmd) => {
                    self.handle(cmd);
                    // drain any queued commands
                    while let Ok(c) = rx.try_recv() {
                        if matches!(c, Cmd::Shutdown) {
                            self.shutdown();
                            return;
                        }
                        self.handle(c);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown();
                    return;
                }
            }
            self.tick();
        }
    }

    fn shutdown(&mut self) {
        self.audio.send(AudioCmd::Quit);
        if self.timer.is_active() {
            let (s, _) = self.timer.stop(stamp());
            if let Some(id) = self.session_id.take() {
                let _ = self.db.finish_session(id, chrono::Utc::now().timestamp(), s.focus_secs, s.cycles as i64, None, "");
            }
        }
    }

    // ------------------------------------------------------------------
    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Start { mode, subject_idx, goal } => self.start(mode, subject_idx, goal),
            Cmd::TogglePause => {
                if !self.timer.is_active() {
                    self.start(0, 0, String::new());
                } else {
                    let ev = self.timer.toggle(stamp());
                    self.handle_events(ev);
                }
            }
            Cmd::Skip => {
                let ev = self.timer.skip(stamp());
                self.handle_events(ev);
            }
            Cmd::Stop => self.stop(),
            Cmd::SubmitOutcome { outcome, note } => {
                if let Some(id) = self.pending_outcome.take() {
                    let o = match outcome {
                        0 => Outcome::Done,
                        1 => Outcome::Partly,
                        _ => Outcome::NotDone,
                    };
                    if let Err(e) = self.db.set_session_outcome(id, Some(o), note.trim()) {
                        tracing::error!("saving outcome: {e}");
                    }
                }
                let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_outcome_open(false));
            }
            Cmd::SetSetting { key, value } => self.set_setting(&key, value),
            Cmd::AddSubject { name, color } => {
                if !name.trim().is_empty() {
                    if let Err(e) = self.db.add_subject(&name, &color) {
                        tracing::warn!("add subject: {e}");
                    }
                    self.reload_lists();
                    self.push_lists();
                }
            }
            Cmd::DeleteSubject(id) => {
                let _ = self.db.delete_subject(id);
                self.reload_lists();
                self.push_lists();
            }
            Cmd::AddTip(t) => {
                let _ = self.db.add_tip(&t);
                self.reload_lists();
                self.push_lists();
            }
            Cmd::DeleteTip(id) => {
                let _ = self.db.delete_tip(id);
                self.reload_lists();
                self.push_lists();
            }
            Cmd::SetTrackVolume { id, volume } => {
                self.mix.insert(id.clone(), volume);
                if let Ok(j) = serde_json::to_string(&self.mix) {
                    let _ = self.db.set_kv("sound_mix", &j);
                }
                if let Some(t) = self.tracks.iter().find(|t| t.id == id) {
                    self.audio.send(AudioCmd::Track { id, spec: t.spec.clone(), volume });
                }
                // Moving a slider up implies the user wants to hear it.
                if volume > 0.0 && !self.sounds_playing {
                    self.set_sounds_playing(true);
                }
            }
            Cmd::ToggleSounds => self.set_sounds_playing(!self.sounds_playing),
            Cmd::OpenSoundsFolder => {
                let _ = std::fs::create_dir_all(&self.sounds_dir);
                open_folder(&self.sounds_dir);
            }
            Cmd::RescanSounds => self.rescan_sounds(),
            Cmd::Shutdown => {}
        }
    }

    fn set_sounds_playing(&mut self, on: bool) {
        self.sounds_playing = on;
        self.audio.send(AudioCmd::SetPlaying(on));
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_sounds_playing(on));
    }

    fn rescan_sounds(&mut self) {
        self.tracks = audio::scan_tracks(&self.sounds_dir);
        for t in &self.tracks {
            let volume = self.mix.get(&t.id).copied().unwrap_or(0.0);
            self.audio.send(AudioCmd::Track { id: t.id.clone(), spec: t.spec.clone(), volume });
        }
        let items: Vec<(String, String, f32)> =
            self.tracks.iter().map(|t| (t.id.clone(), t.name.clone(), self.mix.get(&t.id).copied().unwrap_or(0.0))).collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let v: Vec<TrackItem> =
                items.into_iter().map(|(id, name, volume)| TrackItem { id: id.into(), name: name.into(), volume }).collect();
            ui.global::<AppState>().set_tracks(ModelRc::new(VecModel::from(v)));
        });
    }

    fn start(&mut self, mode: i32, subject_idx: i32, goal: String) {
        if self.timer.is_active() {
            return;
        }
        let kind = if mode == 1 { SessionKind::Flow } else { SessionKind::Pomodoro };
        let subject = if subject_idx > 0 { self.subjects.get(subject_idx as usize - 1).map(|s| s.id) } else { None };
        let now = stamp();
        match self.db.start_session(subject, kind, goal.trim(), now.wall) {
            Ok(id) => self.session_id = Some(id),
            Err(e) => tracing::error!("start session: {e}"),
        }
        self.session_goal = goal.trim().to_string();
        let ev = self.timer.start(if mode == 1 { Mode::Flow } else { Mode::Pomodoro }, now);
        self.last_progress_save = Instant::now();
        self.handle_events(ev);
    }

    fn stop(&mut self) {
        let now = stamp();
        let (s, ev) = self.timer.stop(now);
        self.handle_events(ev);
        if let Some(id) = self.session_id.take() {
            if s.focus_secs < 10 {
                let _ = self.db.delete_session(id); // accidental start
            } else {
                let _ = self.db.finish_session(id, now.wall, s.focus_secs, s.cycles as i64, None, "");
                self.pending_outcome = Some(id);
                let summary = format!("{} focused · {} cycle(s)", fmt_clock(Duration::from_secs(s.focus_secs as u64)), s.cycles);
                let goal = self.session_goal.clone();
                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                    let st = ui.global::<AppState>();
                    st.set_outcome_goal(goal.into());
                    st.set_outcome_summary(summary.into());
                    st.set_outcome_open(true);
                });
            }
        }
        self.session_goal.clear();
        self.push_timer();
    }

    fn tick(&mut self) {
        let ev = self.timer.tick(stamp());
        if !ev.is_empty() {
            self.handle_events(ev);
        }
        if self.timer.is_active() && self.last_progress_save.elapsed() > Duration::from_secs(15) {
            self.last_progress_save = Instant::now();
            if let Some(id) = self.session_id {
                let s = self.timer.summary();
                let _ = self.db.update_session_progress(id, s.focus_secs, s.cycles as i64);
            }
        }
        self.push_timer();
    }

    fn handle_events(&mut self, events: Vec<TimerEvent>) {
        for e in events {
            match e {
                TimerEvent::PhaseEnded { phase, completed: true } => {
                    let (title, body) = match phase {
                        Phase::Work => ("Focus phase complete", "Nice work — time for a break."),
                        Phase::ShortBreak | Phase::LongBreak => ("Break over", "Ready to focus again?"),
                        _ => continue,
                    };
                    self.announce(title, body);
                    if self.settings.notify_sound {
                        self.audio.send(AudioCmd::Chime(self.settings.master_volume.max(0.3)));
                    }
                }
                TimerEvent::PhaseStarted(p) => {
                    if self.settings.sound_during_work {
                        self.set_sounds_playing(p.is_focus());
                    }
                    if p.is_break() {
                        self.next_tip();
                    }
                }
                TimerEvent::Stopped => {
                    if self.settings.sound_during_work {
                        self.set_sounds_playing(false);
                    }
                }
                TimerEvent::AwaitingStart(p) => {
                    self.announce("Next phase ready", &format!("{} is waiting — press Start next.", p.label()));
                    if p.is_break() {
                        self.next_tip();
                    }
                }
                TimerEvent::FlowReminder => self.announce("Time for a break?", "You've been in flow for a while."),
                _ => {}
            }
        }
    }

    fn announce(&self, title: &str, body: &str) {
        if self.settings.notifications {
            crate::sys::notify::send(title, body);
        }
    }

    fn next_tip(&mut self) {
        if self.tips.is_empty() {
            return;
        }
        self.tip_idx = (self.tip_idx + 1) % self.tips.len();
    }

    fn current_tip(&self) -> String {
        self.tips.get(self.tip_idx % self.tips.len().max(1)).map(|t| t.text.clone()).unwrap_or_default()
    }

    // ------------------------------------------------------------------
    fn reload_lists(&mut self) {
        self.subjects = self.db.subjects().unwrap_or_default();
        self.tips = self.db.tips().unwrap_or_default();
    }

    fn push_lists(&self) {
        let subjects = self.subjects.clone();
        let tips = self.tips.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            let items: Vec<SubjectItem> = subjects
                .iter()
                .map(|s| SubjectItem { id: s.id as i32, name: s.name.clone().into(), color: parse_color(&s.color) })
                .collect();
            let mut names: Vec<SharedString> = vec!["No subject".into()];
            names.extend(subjects.iter().map(|s| SharedString::from(s.name.as_str())));
            st.set_subjects(ModelRc::new(VecModel::from(items)));
            st.set_subject_names(ModelRc::new(VecModel::from(names)));
            if st.get_focus_subject() as usize > subjects.len() {
                st.set_focus_subject(0);
            }
            st.set_tips(ModelRc::new(VecModel::from(
                tips.iter().map(|t| TipItem { id: t.id as i32, text: t.text.clone().into() }).collect::<Vec<_>>(),
            )));
        });
    }

    fn settings_data(&self) -> SettingsData {
        let s = &self.settings;
        SettingsData {
            work_mins: s.work_mins as i32,
            short_break_mins: s.short_break_mins as i32,
            long_break_mins: s.long_break_mins as i32,
            cycles_before_long: s.cycles_before_long as i32,
            auto_start_next: s.auto_start_next,
            flow_reminder_mins: s.flow_reminder_mins as i32,
            theme: match s.theme {
                ThemeMode::Dark => 0,
                ThemeMode::Light => 1,
                ThemeMode::System => 2,
            },
            accent: s.accent as i32,
            daily_goal_mins: s.daily_goal_mins as i32,
            weekly_goal_mins: s.weekly_goal_mins as i32,
            streak_min_mins: s.streak_min_mins as i32,
            day_start_hour: s.day_start_hour as i32,
            new_cards_per_day: s.new_cards_per_day as i32,
            master_volume: s.master_volume,
            sound_during_work: s.sound_during_work,
            notify_sound: s.notify_sound,
            notifications: s.notifications,
            close_to_tray: s.close_to_tray,
            autostart: s.autostart,
            hotkey: s.hotkey.clone().into(),
            dnd_during_work: s.dnd_during_work,
            browser_policies: s.browser_policies,
            block_steam_client: s.block_steam_client,
            hard_mode_cooldown_mins: s.hard_mode_cooldown_mins as i32,
            emergency_cooldown_mins: s.emergency_cooldown_mins as i32,
            early_exit_challenge: s.early_exit_challenge,
            backup_daily: s.backup_daily,
        }
    }

    pub fn push_settings(&self) {
        let data = self.settings_data();
        let theme = data.theme;
        let accent = data.accent;
        self.shared.close_to_tray.store(self.settings.close_to_tray, Ordering::Relaxed);
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.global::<AppState>().set_settings(data);
            let t = ui.global::<crate::Theme>();
            t.set_mode(theme);
            t.set_accent_index(accent);
        });
    }

    fn set_setting(&mut self, key: &str, value: serde_json::Value) {
        let Ok(mut j) = serde_json::to_value(&self.settings) else { return };
        if j.get(key).is_none() {
            tracing::warn!("unknown setting {key}");
            return;
        }
        // theme is stored as an enum name
        let value = if key == "theme" {
            match value.as_i64() {
                Some(1) => "Light".into(),
                Some(2) => "System".into(),
                _ => "Dark".into(),
            }
        } else {
            value
        };
        j[key] = value;
        match serde_json::from_value::<Settings>(j) {
            Ok(s) => {
                let old = std::mem::replace(&mut self.settings, s.sanitized());
                if let Err(e) = self.db.save_settings(&self.settings) {
                    tracing::error!("save settings: {e}");
                }
                self.after_settings_change(&old);
                self.push_settings();
            }
            Err(e) => tracing::warn!("bad value for {key}: {e}"),
        }
    }

    fn after_settings_change(&mut self, old: &Settings) {
        self.timer.set_config(timer_cfg(&self.settings));
        self.audio.send(AudioCmd::Master(self.settings.master_volume));
        if old.hotkey != self.settings.hotkey {
            (self.hotkey_rebind)(self.settings.hotkey.clone());
        }
    }

    pub fn push_timer(&mut self) {
        let t = &self.timer;
        let active = t.is_active();
        let text = match t.phase() {
            Phase::Idle => fmt_clock(Duration::from_secs(self.settings.work_mins as u64 * 60)),
            Phase::Flow => fmt_clock(t.phase_elapsed()),
            _ => fmt_clock(t.remaining().unwrap_or_default() + Duration::from_millis(900)),
        };
        let label = if t.is_awaiting_start() {
            format!("{} — ready", t.phase().label())
        } else if t.is_paused() {
            format!("{} (paused)", t.phase().label())
        } else {
            t.phase().label().to_string()
        };
        let info = if active {
            format!("Cycle {} · {} focused", t.cycles() + u32::from(t.phase() == Phase::Work), fmt_clock(t.focus()))
        } else {
            String::new()
        };
        let tip = if t.phase().is_break() { self.current_tip() } else { String::new() };
        let progress = t.progress();
        let running = t.is_running();
        let awaiting = t.is_awaiting_start();
        let is_break = t.phase().is_break();
        let mode = if t.mode() == Mode::Flow { 1 } else { 0 };
        let goal = self.session_goal.clone();
        let mode_if_active = active.then_some(mode);

        // tray
        let tray_state = if !active {
            TrayState::Idle
        } else if !running {
            TrayState::Paused
        } else if is_break {
            TrayState::Break
        } else {
            TrayState::Work
        };
        let tip_text = if active { format!("StudyForge — {} {}", label, text) } else { "StudyForge — idle".to_string() };
        if let Some(tray) = &self.tray {
            if self.last_tray != (tray_state, tip_text.clone()) {
                tray.set(tray_state, &tip_text);
                self.last_tray = (tray_state, tip_text);
            }
        }

        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_timer_text(text.into());
            st.set_phase_label(label.into());
            st.set_timer_progress(progress);
            st.set_timer_active(active);
            st.set_timer_running(running);
            st.set_timer_awaiting(awaiting);
            st.set_timer_is_break(is_break);
            if let Some(m) = mode_if_active {
                st.set_timer_mode(m);
            }
            st.set_session_info(info.into());
            st.set_break_tip(tip.into());
            st.set_active_goal(goal.into());
        });
    }
}

#[allow(dead_code)]
pub fn subject_color_hex(c: slint::Color) -> String {
    color_hex(c)
}

#[allow(dead_code)]
fn _assert_model_trait<T: Model>() {}

pub fn open_folder(p: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let prog = "explorer";
    #[cfg(not(target_os = "windows"))]
    let prog = "xdg-open";
    if let Err(e) = std::process::Command::new(prog).arg(p).spawn() {
        tracing::warn!("open folder: {e}");
    }
}

pub fn wire_callbacks(ui: &AppWindow, tx: &Sender<Cmd>) {
    let st = ui.global::<AppState>();
    let t = tx.clone();
    st.on_set_track_volume(move |id, v| {
        let _ = t.send(Cmd::SetTrackVolume { id: id.to_string(), volume: v });
    });
    let t = tx.clone();
    st.on_toggle_sounds(move || {
        let _ = t.send(Cmd::ToggleSounds);
    });
    let t = tx.clone();
    st.on_open_sounds_folder(move || {
        let _ = t.send(Cmd::OpenSoundsFolder);
    });
    let t = tx.clone();
    st.on_rescan_sounds(move || {
        let _ = t.send(Cmd::RescanSounds);
    });
    let t = tx.clone();
    st.on_start_session(move |mode, subj, goal| {
        let _ = t.send(Cmd::Start { mode, subject_idx: subj, goal: goal.to_string() });
    });
    let t = tx.clone();
    st.on_toggle_pause(move || {
        let _ = t.send(Cmd::TogglePause);
    });
    let t = tx.clone();
    st.on_skip_phase(move || {
        let _ = t.send(Cmd::Skip);
    });
    let t = tx.clone();
    st.on_stop_session(move || {
        let _ = t.send(Cmd::Stop);
    });
    let t = tx.clone();
    st.on_submit_outcome(move |o, note| {
        let _ = t.send(Cmd::SubmitOutcome { outcome: o, note: note.to_string() });
    });
    let t = tx.clone();
    st.on_set_int(move |k, v| {
        let _ = t.send(Cmd::SetSetting { key: k.replace('-', "_"), value: v.into() });
    });
    let t = tx.clone();
    st.on_set_bool(move |k, v| {
        let _ = t.send(Cmd::SetSetting { key: k.replace('-', "_"), value: v.into() });
    });
    let t = tx.clone();
    st.on_set_float(move |k, v| {
        let _ = t.send(Cmd::SetSetting { key: k.replace('-', "_"), value: serde_json::json!(v) });
    });
    let t = tx.clone();
    st.on_set_str(move |k, v| {
        let _ = t.send(Cmd::SetSetting { key: k.replace('-', "_"), value: v.to_string().into() });
    });
    let t = tx.clone();
    st.on_add_subject(move |n, c| {
        let _ = t.send(Cmd::AddSubject { name: n.to_string(), color: color_hex(c) });
    });
    let t = tx.clone();
    st.on_delete_subject(move |id| {
        let _ = t.send(Cmd::DeleteSubject(id as i64));
    });
    let t = tx.clone();
    st.on_add_tip(move |s| {
        let _ = t.send(Cmd::AddTip(s.to_string()));
    });
    let t = tx.clone();
    st.on_delete_tip(move |id| {
        let _ = t.send(Cmd::DeleteTip(id as i64));
    });
}
