//! Plain data models shared by the DB layer, the GUI and the daemon.

use serde::{Deserialize, Serialize};

pub type Id = i64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subject {
    pub id: Id,
    pub name: String,
    /// `#rrggbb`
    pub color: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    Pomodoro,
    Flow,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::Pomodoro => "pomodoro",
            SessionKind::Flow => "flow",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s == "flow" {
            SessionKind::Flow
        } else {
            SessionKind::Pomodoro
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Done,
    Partly,
    NotDone,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Partly => "partly",
            Outcome::NotDone => "not_done",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "done" => Some(Outcome::Done),
            "partly" => Some(Outcome::Partly),
            "not_done" => Some(Outcome::NotDone),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: Id,
    pub subject_id: Option<Id>,
    pub kind: SessionKind,
    pub goal: String,
    pub outcome: Option<Outcome>,
    pub note: String,
    /// Unix seconds (UTC).
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// Seconds spent in work phases.
    pub focus_secs: i64,
    pub cycles: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deck {
    pub id: Id,
    pub name: String,
    pub subject_id: Option<Id>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardKind {
    New,
    Learning,
    Review,
}

impl CardKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CardKind::New => "new",
            CardKind::Learning => "learning",
            CardKind::Review => "review",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "learning" => CardKind::Learning,
            "review" => CardKind::Review,
            _ => CardKind::New,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: Id,
    pub deck_id: Id,
    pub front: String,
    pub back: String,
    pub tags: String,
    pub kind: CardKind,
    /// Days since the Unix epoch on which the card is due.
    pub due_day: i64,
    pub interval_days: i64,
    pub ease: f64,
    pub reps: i64,
    pub lapses: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeckSummary {
    pub deck: Deck,
    pub total: i64,
    pub due: i64,
    pub new_available: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: Id,
    pub subject_id: Option<Id>,
    pub title: String,
    pub body: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Low = 0,
    Medium = 1,
    High = 2,
}

impl Priority {
    pub fn from_i64(v: i64) -> Self {
        match v {
            0 => Priority::Low,
            2 => Priority::High,
            _ => Priority::Medium,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Priority::Low => "Low",
            Priority::Medium => "Medium",
            Priority::High => "High",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: Id,
    pub title: String,
    pub subject_id: Option<Id>,
    /// ISO date `YYYY-MM-DD`.
    pub due: Option<String>,
    pub priority: Priority,
    pub done: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DistractionKind {
    App,
    Site,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distraction {
    pub id: Id,
    pub ts: i64,
    pub target: String,
    pub kind: DistractionKind,
    pub list_name: String,
    /// "killed", "overlay", "notified", "dismissed", "snoozed", ...
    pub action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EnforceMode {
    /// Terminate the offending process.
    #[default]
    Hard,
    /// Show an overlay / notification, never kill.
    Gentle,
}

/// A user (or preset) blocklist as stored by the GUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blocklist {
    pub id: Id,
    pub name: String,
    /// Set for built-in presets (they are read-only; duplicate to edit).
    pub preset_key: Option<String>,
    pub domains: Vec<String>,
    pub processes: Vec<String>,
    pub steam_games: bool,
    pub mode: EnforceMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BreakTip {
    pub id: Id,
    pub text: String,
}
