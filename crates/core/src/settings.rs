//! Typed application settings, stored as one JSON blob in the DB.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Dark,
    Light,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // timer
    pub work_mins: u32,
    pub short_break_mins: u32,
    pub long_break_mins: u32,
    pub cycles_before_long: u32,
    pub auto_start_next: bool,
    pub flow_reminder_mins: u32, // 0 = off
    // look
    pub theme: ThemeMode,
    pub accent: usize,
    // goals / streaks
    pub daily_goal_mins: u32,
    pub weekly_goal_mins: u32,
    pub streak_min_mins: u32,
    pub day_start_hour: u32,
    // cards
    pub new_cards_per_day: u32,
    // sounds
    pub master_volume: f32,
    pub sound_during_work: bool,
    pub notify_sound: bool,
    // system
    pub notifications: bool,
    pub close_to_tray: bool,
    pub autostart: bool,
    pub hotkey: String,
    pub dnd_during_work: bool,
    // blocking
    pub browser_policies: bool,
    pub block_steam_client: bool,
    pub allowlist_mode: bool,
    /// One domain per line.
    pub allowlist_domains: String,
    /// One process name per line.
    pub allowlist_processes: String,
    pub hard_mode_cooldown_mins: u32,
    pub emergency_cooldown_mins: u32,
    pub early_exit_challenge: bool,
    pub backup_daily: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            work_mins: 25,
            short_break_mins: 5,
            long_break_mins: 15,
            cycles_before_long: 4,
            auto_start_next: true,
            flow_reminder_mins: 0,
            theme: ThemeMode::Dark,
            accent: 0,
            daily_goal_mins: 240,
            weekly_goal_mins: 1200,
            streak_min_mins: 25,
            day_start_hour: 4,
            new_cards_per_day: 20,
            master_volume: 0.8,
            sound_during_work: false,
            notify_sound: true,
            notifications: true,
            close_to_tray: true,
            autostart: false,
            hotkey: "Ctrl+Alt+S".into(),
            dnd_during_work: false,
            browser_policies: true,
            block_steam_client: false,
            allowlist_mode: false,
            allowlist_domains: String::new(),
            allowlist_processes: String::new(),
            hard_mode_cooldown_mins: 20,
            emergency_cooldown_mins: 60,
            early_exit_challenge: true,
            backup_daily: true,
        }
    }
}

impl Settings {
    /// Clamp values to sane ranges (e.g. emergency cooldown >= 10 minutes).
    pub fn sanitized(mut self) -> Self {
        self.work_mins = self.work_mins.clamp(1, 240);
        self.short_break_mins = self.short_break_mins.clamp(1, 120);
        self.long_break_mins = self.long_break_mins.clamp(1, 240);
        self.cycles_before_long = self.cycles_before_long.clamp(1, 12);
        self.day_start_hour = self.day_start_hour.min(12);
        self.emergency_cooldown_mins = self.emergency_cooldown_mins.max(crate::hardmode::MIN_EMERGENCY_COOLDOWN_MINS);
        self.hard_mode_cooldown_mins = self.hard_mode_cooldown_mins.clamp(1, 600);
        self.master_volume = self.master_volume.clamp(0.0, 1.0);
        self
    }
}

pub const ACCENTS: [&str; 6] = ["#6c8cff", "#2fc5a0", "#ef6f6c", "#f2a93b", "#b57cf0", "#e77bb5"];
