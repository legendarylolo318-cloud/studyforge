//! State persisted by studyd (`<state_dir>/state.json`): the active session
//! (so a reboot or crash does not end a hard-mode lock), schedules, and the
//! queue of events the GUI has not collected yet.

use serde::{Deserialize, Serialize};
use std::path::Path;
use studyforge_core::blocking::BlockSpec;
use studyforge_core::hardmode::{HardModeConfig, HardModeState};
use studyforge_core::ipc::DaemonEvent;
use studyforge_core::schedule::ScheduleRule;

pub const MAX_QUEUED_EVENTS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveSession {
    pub goal: String,
    pub started_at: i64,
    pub spec: BlockSpec,
    pub hard: Option<HardModeConfig>,
    #[serde(default)]
    pub hard_state: HardModeState,
    /// Last time a GUI talked to us (soft sessions lapse without contact).
    pub last_contact: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub active: Option<ActiveSession>,
    pub schedules: Vec<ScheduleRule>,
    pub events: Vec<DaemonEvent>,
    pub next_id: u64,
    /// Wall-clock time (unix s) at which studyd last ran; detects the clock
    /// going backwards across a restart.
    #[serde(default)]
    pub last_seen: i64,
}

impl Persisted {
    pub fn load(path: &Path) -> Persisted {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::error!("state file {} is corrupt ({e}); starting empty", path.display());
                Persisted::default()
            }),
            Err(_) => Persisted::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    pub fn push_event(&mut self, e: DaemonEvent) {
        self.events.push(e);
        if self.events.len() > MAX_QUEUED_EVENTS {
            let drop = self.events.len() - MAX_QUEUED_EVENTS;
            self.events.drain(..drop);
        }
    }
}

/// Allowed identities from `allowed_users` (one uid / SID per line, `#` comments).
pub fn load_allowed_users(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines().map(|l| l.split('#').next().unwrap_or("").trim().to_string()).filter(|l| !l.is_empty()).collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_users_parsing() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("allowed_users");
        std::fs::write(&f, "# installer\n1000\n\n 1001  # second user\n").unwrap();
        assert_eq!(load_allowed_users(&f), vec!["1000", "1001"]);
        assert!(load_allowed_users(&d.path().join("missing")).is_empty());
    }

    #[test]
    fn persisted_roundtrip_and_corrupt_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("s/state.json");
        let p = Persisted { next_id: 7, ..Default::default() };
        p.save(&f).unwrap();
        assert_eq!(Persisted::load(&f).next_id, 7);
        std::fs::write(&f, "{ nope").unwrap();
        assert_eq!(Persisted::load(&f).next_id, 0);
    }

    #[test]
    fn event_queue_is_bounded() {
        let mut p = Persisted::default();
        for i in 0..(MAX_QUEUED_EVENTS + 50) {
            p.push_event(DaemonEvent::Notice { ts: i as i64, message: "x".into() });
        }
        assert_eq!(p.events.len(), MAX_QUEUED_EVENTS);
        assert!(matches!(p.events[0], DaemonEvent::Notice { ts: 50, .. }));
    }
}
