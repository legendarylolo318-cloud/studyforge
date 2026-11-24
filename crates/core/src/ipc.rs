//! GUI <-> studyd protocol. JSON lines over a local socket / named pipe.
//! One request line, one response line. The first request on a connection
//! must be `Hello` (protocol-version handshake).

use crate::blocking::BlockSpec;
use crate::hardmode::HardModeConfig;
use crate::models::DistractionKind;
use crate::schedule::ScheduleRule;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartSession {
    pub goal: String,
    pub spec: BlockSpec,
    /// Hard mode: locked in until the deadline in `hard.deadline`.
    pub hard: Option<HardModeConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayChoice {
    /// Close (terminate) the offending app.
    CloseIt,
    /// Leave it for 5 more minutes.
    FiveMore,
    /// Ignore for the rest of the session.
    Dismiss,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    Hello {
        protocol: u32,
        client: String,
    },
    Ping,
    Status,
    StartSession(StartSession),
    /// Refused in hard mode.
    StopSession,
    /// Hard mode: attempt early exit by typing the shown challenge.
    ExitChallenge {
        typed: String,
    },
    /// Hard mode: start (or cancel) the early-exit cooldown.
    ExitCooldown {
        start: bool,
    },
    /// Emergency unlock: start (or cancel) its (long) cooldown. Always works.
    Emergency {
        start: bool,
    },
    SetSchedules {
        rules: Vec<ScheduleRule>,
    },
    PollEvents,
    OverlayAction {
        id: u64,
        choice: OverlayChoice,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardStatus {
    pub deadline: i64,
    pub challenge_enabled: bool,
    pub challenge: Option<String>,
    pub exit_cooldown_mins: u32,
    pub exit_cooldown_remaining: Option<i64>,
    pub emergency_cooldown_mins: u32,
    pub emergency_remaining: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveInfo {
    pub goal: String,
    pub started_at: i64,
    pub lists: Vec<String>,
    pub hard: Option<HardStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DaemonStatus {
    pub protocol: u32,
    pub version: String,
    pub dry_run: bool,
    pub active: Option<ActiveInfo>,
    /// Names of currently active schedule rules.
    pub schedules_active: Vec<String>,
    pub schedules_total: usize,
    pub blocked_domains: usize,
    pub hosts_applied: bool,
    pub policies_applied: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonEvent {
    /// A blocked/killed/overlayed attempt (goes into the distraction log).
    Distraction { ts: i64, target: String, kind: DistractionKind, list: String, action: String },
    /// Ask the GUI to show the gentle-mode overlay.
    Overlay { id: u64, ts: i64, target: String, list: String, goal: String },
    /// Informational message (session ended by deadline, etc.).
    Notice { ts: i64, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)] // short-lived messages; boxing would only complicate every match
pub enum Response {
    Hello { protocol: u32, version: String },
    Pong,
    Ok,
    Status(DaemonStatus),
    Events { events: Vec<DaemonEvent> },
    Error { message: String },
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("serializable");
    s.push('\n');
    s
}

pub fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// Maximum accepted request line (guards the daemon against memory abuse).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_requests() {
        let reqs = vec![
            Request::Hello { protocol: PROTOCOL_VERSION, client: "gui".into() },
            Request::Status,
            Request::StartSession(StartSession {
                goal: "g".into(),
                spec: BlockSpec::default(),
                hard: Some(HardModeConfig::new(5, true, 20, 60)),
            }),
            Request::ExitChallenge { typed: "abc".into() },
            Request::OverlayAction { id: 3, choice: OverlayChoice::FiveMore },
            Request::Emergency { start: true },
        ];
        for r in reqs {
            let line = encode(&r);
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
            let back: Request = decode(&line).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn roundtrip_responses_and_events() {
        let r = Response::Events {
            events: vec![
                DaemonEvent::Distraction {
                    ts: 1,
                    target: "discord".into(),
                    kind: DistractionKind::App,
                    list: "Social".into(),
                    action: "killed".into(),
                },
                DaemonEvent::Overlay {
                    id: 9,
                    ts: 2,
                    target: "steam game".into(),
                    list: "Gaming".into(),
                    goal: "study".into(),
                },
            ],
        };
        assert_eq!(decode::<Response>(&encode(&r)).unwrap(), r);
        let s = Response::Status(DaemonStatus { protocol: 1, ..Default::default() });
        assert_eq!(decode::<Response>(&encode(&s)).unwrap(), s);
    }

    #[test]
    fn unknown_request_is_an_error_not_a_panic() {
        assert!(decode::<Request>("{\"type\":\"Nope\"}").is_err());
        assert!(decode::<Request>("garbage").is_err());
    }
}
