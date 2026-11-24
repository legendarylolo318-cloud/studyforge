//! Pomodoro / flow timer as a pure state machine.
//!
//! All time is injected via [`Stamp`] (monotonic `Instant` + wall-clock
//! seconds). Countdowns use the monotonic clock, so wall-clock changes never
//! shorten or lengthen a phase. A large wall-clock jump *forward* relative to
//! the monotonic clock is interpreted as the machine having been suspended
//! (CLOCK_MONOTONIC does not advance during suspend on Linux) and the gap is
//! credited, i.e. the elapsed time is recomputed from the stored start time.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Gap (wall minus monotonic) above which we assume suspend/resume.
pub const SUSPEND_THRESHOLD: Duration = Duration::from_secs(5);
/// Flow mode credits at most this much time for a suspend gap.
pub const FLOW_MAX_GAP_CREDIT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy)]
pub struct Stamp {
    pub mono: Instant,
    /// Unix seconds, wall clock.
    pub wall: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Pomodoro,
    Flow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Work,
    ShortBreak,
    LongBreak,
    Flow,
}

impl Phase {
    pub fn is_break(&self) -> bool {
        matches!(self, Phase::ShortBreak | Phase::LongBreak)
    }
    pub fn is_focus(&self) -> bool {
        matches!(self, Phase::Work | Phase::Flow)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Idle => "Ready",
            Phase::Work => "Focus",
            Phase::ShortBreak => "Short break",
            Phase::LongBreak => "Long break",
            Phase::Flow => "Flow",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerConfig {
    pub work: Duration,
    pub short_break: Duration,
    pub long_break: Duration,
    pub cycles_before_long: u32,
    pub auto_start_next: bool,
    /// Flow mode: remind after this long (None = never).
    pub flow_reminder: Option<Duration>,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            work: Duration::from_secs(25 * 60),
            short_break: Duration::from_secs(5 * 60),
            long_break: Duration::from_secs(15 * 60),
            cycles_before_long: 4,
            auto_start_next: true,
            flow_reminder: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerEvent {
    /// A phase began (including the first one).
    PhaseStarted(Phase),
    /// A phase ended. `completed` is false when it was skipped.
    PhaseEnded { phase: Phase, completed: bool },
    /// Auto-start is off: `Phase` is queued and waiting for the user.
    AwaitingStart(Phase),
    FlowReminder,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub focus_secs: i64,
    pub cycles: u32,
}

#[derive(Debug, Clone)]
pub struct Timer {
    cfg: TimerConfig,
    mode: Mode,
    phase: Phase,
    /// Counting time right now.
    running: bool,
    /// A phase is queued but hasn't been started (auto-start off).
    awaiting: bool,
    phase_elapsed: Duration,
    last: Option<Stamp>,
    focus: Duration,
    cycles: u32,
    flow_reminded: bool,
}

impl Timer {
    pub fn new(cfg: TimerConfig) -> Self {
        Self {
            cfg,
            mode: Mode::Pomodoro,
            phase: Phase::Idle,
            running: false,
            awaiting: false,
            phase_elapsed: Duration::ZERO,
            last: None,
            focus: Duration::ZERO,
            cycles: 0,
            flow_reminded: false,
        }
    }

    pub fn config(&self) -> &TimerConfig {
        &self.cfg
    }
    /// Replace the config; applies from the next phase on.
    pub fn set_config(&mut self, cfg: TimerConfig) {
        self.cfg = cfg;
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn is_running(&self) -> bool {
        self.running
    }
    pub fn is_active(&self) -> bool {
        self.phase != Phase::Idle
    }
    pub fn is_awaiting_start(&self) -> bool {
        self.awaiting
    }
    pub fn is_paused(&self) -> bool {
        self.is_active() && !self.running && !self.awaiting
    }
    pub fn cycles(&self) -> u32 {
        self.cycles
    }
    pub fn focus(&self) -> Duration {
        self.focus
    }
    pub fn phase_elapsed(&self) -> Duration {
        self.phase_elapsed
    }

    pub fn phase_len(&self) -> Option<Duration> {
        match self.phase {
            Phase::Work => Some(self.cfg.work),
            Phase::ShortBreak => Some(self.cfg.short_break),
            Phase::LongBreak => Some(self.cfg.long_break),
            Phase::Idle | Phase::Flow => None,
        }
    }

    /// Time left in a countdown phase; `None` in flow / idle.
    pub fn remaining(&self) -> Option<Duration> {
        self.phase_len().map(|l| l.saturating_sub(self.phase_elapsed))
    }

    /// 0..=1 progress through a countdown phase.
    pub fn progress(&self) -> f32 {
        match self.phase_len() {
            Some(l) if !l.is_zero() => (self.phase_elapsed.as_secs_f32() / l.as_secs_f32()).min(1.0),
            _ => 0.0,
        }
    }

    pub fn summary(&self) -> Summary {
        Summary { focus_secs: self.focus.as_secs() as i64, cycles: self.cycles }
    }

    /// Begin a new session. Ignored (empty events) if one is already active.
    pub fn start(&mut self, mode: Mode, now: Stamp) -> Vec<TimerEvent> {
        if self.is_active() {
            return vec![];
        }
        self.mode = mode;
        self.focus = Duration::ZERO;
        self.cycles = 0;
        self.flow_reminded = false;
        self.phase_elapsed = Duration::ZERO;
        self.phase = if mode == Mode::Flow { Phase::Flow } else { Phase::Work };
        self.begin_counting(now);
        vec![TimerEvent::PhaseStarted(self.phase)]
    }

    fn begin_counting(&mut self, now: Stamp) {
        self.running = true;
        self.awaiting = false;
        self.last = Some(now);
    }

    pub fn pause(&mut self, now: Stamp) {
        if self.running {
            self.advance(now, &mut vec![]);
            self.running = false;
            self.last = None;
        }
    }

    pub fn resume(&mut self, now: Stamp) -> Vec<TimerEvent> {
        if !self.is_active() || self.running {
            return vec![];
        }
        let was_awaiting = self.awaiting;
        self.begin_counting(now);
        if was_awaiting {
            vec![TimerEvent::PhaseStarted(self.phase)]
        } else {
            vec![]
        }
    }

    pub fn toggle(&mut self, now: Stamp) -> Vec<TimerEvent> {
        if !self.is_active() {
            return vec![];
        }
        if self.running {
            self.pause(now);
            vec![]
        } else {
            self.resume(now)
        }
    }

    /// Skip the current phase (Pomodoro only). Skipped work does not count as
    /// a completed cycle.
    pub fn skip(&mut self, now: Stamp) -> Vec<TimerEvent> {
        let mut ev = vec![];
        if self.mode != Mode::Pomodoro || !self.is_active() {
            return ev;
        }
        if self.running {
            self.advance(now, &mut ev);
        }
        self.finish_phase(false, now, &mut ev);
        ev
    }

    /// End the session and return its totals.
    pub fn stop(&mut self, now: Stamp) -> (Summary, Vec<TimerEvent>) {
        if self.running {
            self.advance(now, &mut vec![]);
        }
        let s = self.summary();
        let mut ev = vec![];
        if self.is_active() {
            ev.push(TimerEvent::PhaseEnded { phase: self.phase, completed: false });
            ev.push(TimerEvent::Stopped);
        }
        self.phase = Phase::Idle;
        self.running = false;
        self.awaiting = false;
        self.last = None;
        self.phase_elapsed = Duration::ZERO;
        (s, ev)
    }

    /// Advance the clock. Call frequently (e.g. 4x per second) and after resume.
    pub fn tick(&mut self, now: Stamp) -> Vec<TimerEvent> {
        let mut ev = vec![];
        if !self.running {
            return ev;
        }
        self.advance(now, &mut ev);
        if let Some(len) = self.phase_len() {
            if self.phase_elapsed >= len {
                self.finish_phase(true, now, &mut ev);
            }
        }
        ev
    }

    fn advance(&mut self, now: Stamp, ev: &mut Vec<TimerEvent>) {
        let Some(last) = self.last else { return };
        let d_mono = now.mono.saturating_duration_since(last.mono);
        let d_wall = Duration::from_secs((now.wall - last.wall).max(0) as u64);
        let mut delta = d_mono;
        if d_wall > d_mono + SUSPEND_THRESHOLD {
            // Machine slept: monotonic clock stood still but wall time moved.
            delta = if self.phase == Phase::Flow { d_wall.min(d_mono + FLOW_MAX_GAP_CREDIT) } else { d_wall };
        }
        self.last = Some(now);
        self.phase_elapsed += delta;
        if self.phase.is_focus() {
            self.focus += delta;
        }
        if self.phase == Phase::Flow && !self.flow_reminded {
            if let Some(r) = self.cfg.flow_reminder {
                if self.phase_elapsed >= r {
                    self.flow_reminded = true;
                    ev.push(TimerEvent::FlowReminder);
                }
            }
        }
    }

    fn finish_phase(&mut self, completed: bool, now: Stamp, ev: &mut Vec<TimerEvent>) {
        let ended = self.phase;
        let len = self.phase_len().unwrap_or(self.phase_elapsed);
        if ended == Phase::Work {
            // Don't credit overshoot (e.g. long suspend) as focus time.
            let overshoot = self.phase_elapsed.saturating_sub(len);
            self.focus = self.focus.saturating_sub(overshoot);
            if completed {
                self.cycles += 1;
            }
        }
        ev.push(TimerEvent::PhaseEnded { phase: ended, completed });
        let next = match ended {
            Phase::Work => {
                if completed && self.cycles > 0 && self.cycles % self.cfg.cycles_before_long.max(1) == 0 {
                    Phase::LongBreak
                } else {
                    Phase::ShortBreak
                }
            }
            _ => Phase::Work,
        };
        self.phase = next;
        self.phase_elapsed = Duration::ZERO;
        if self.cfg.auto_start_next {
            self.begin_counting(now);
            ev.push(TimerEvent::PhaseStarted(next));
        } else {
            self.running = false;
            self.awaiting = true;
            self.last = None;
            ev.push(TimerEvent::AwaitingStart(next));
        }
    }
}

pub fn fmt_clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Clk {
        base: Instant,
        mono: Duration,
        wall: i64,
    }
    impl Clk {
        fn new() -> Self {
            Clk { base: Instant::now(), mono: Duration::ZERO, wall: 1_700_000_000 }
        }
        fn now(&self) -> Stamp {
            Stamp { mono: self.base + self.mono, wall: self.wall }
        }
        /// Normal passage of time.
        fn adv(&mut self, secs: u64) -> Stamp {
            self.mono += Duration::from_secs(secs);
            self.wall += secs as i64;
            self.now()
        }
        /// Only wall clock changes (user set the clock).
        fn jump_wall(&mut self, secs: i64) -> Stamp {
            self.wall += secs;
            self.now()
        }
        /// Suspend: wall advances, monotonic does not.
        fn suspend(&mut self, secs: i64) -> Stamp {
            self.wall += secs;
            self.now()
        }
    }

    fn cfg() -> TimerConfig {
        TimerConfig {
            work: Duration::from_secs(100),
            short_break: Duration::from_secs(20),
            long_break: Duration::from_secs(50),
            cycles_before_long: 2,
            auto_start_next: true,
            flow_reminder: None,
        }
    }

    #[test]
    fn starts_in_work() {
        let c = Clk::new();
        let mut t = Timer::new(cfg());
        let ev = t.start(Mode::Pomodoro, c.now());
        assert_eq!(ev, vec![TimerEvent::PhaseStarted(Phase::Work)]);
        assert!(t.is_running());
        assert_eq!(t.remaining(), Some(Duration::from_secs(100)));
        // second start ignored
        assert!(t.start(Mode::Flow, c.now()).is_empty());
        assert_eq!(t.mode(), Mode::Pomodoro);
    }

    #[test]
    fn work_to_short_to_work_to_long() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        let ev = t.tick(c.adv(100));
        assert_eq!(
            ev,
            vec![
                TimerEvent::PhaseEnded { phase: Phase::Work, completed: true },
                TimerEvent::PhaseStarted(Phase::ShortBreak)
            ]
        );
        assert_eq!(t.cycles(), 1);
        let ev = t.tick(c.adv(20));
        assert_eq!(ev[0], TimerEvent::PhaseEnded { phase: Phase::ShortBreak, completed: true });
        assert_eq!(t.phase(), Phase::Work);
        let ev = t.tick(c.adv(100));
        assert_eq!(ev[1], TimerEvent::PhaseStarted(Phase::LongBreak));
        assert_eq!(t.cycles(), 2);
        t.tick(c.adv(50));
        assert_eq!(t.phase(), Phase::Work);
        assert_eq!(t.summary().focus_secs, 200);
    }

    #[test]
    fn auto_start_off_waits() {
        let mut c = Clk::new();
        let mut cf = cfg();
        cf.auto_start_next = false;
        let mut t = Timer::new(cf);
        t.start(Mode::Pomodoro, c.now());
        let ev = t.tick(c.adv(100));
        assert_eq!(ev.last(), Some(&TimerEvent::AwaitingStart(Phase::ShortBreak)));
        assert!(!t.is_running());
        assert!(t.is_awaiting_start());
        assert!(!t.is_paused());
        // time passing while waiting does nothing
        assert!(t.tick(c.adv(500)).is_empty());
        assert_eq!(t.remaining(), Some(Duration::from_secs(20)));
        let ev = t.resume(c.now());
        assert_eq!(ev, vec![TimerEvent::PhaseStarted(Phase::ShortBreak)]);
        assert!(t.is_running());
    }

    #[test]
    fn pause_resume_excludes_paused_time() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        t.tick(c.adv(30));
        t.pause(c.now());
        assert!(t.is_paused());
        c.adv(1000);
        assert!(t.tick(c.now()).is_empty());
        t.resume(c.now());
        t.tick(c.adv(10));
        assert_eq!(t.phase_elapsed(), Duration::from_secs(40));
        assert_eq!(t.focus(), Duration::from_secs(40));
    }

    #[test]
    fn pause_credits_time_up_to_pause() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        // no tick before pause: pause itself must account for elapsed time.
        t.pause(c.adv(25));
        assert_eq!(t.phase_elapsed(), Duration::from_secs(25));
    }

    #[test]
    fn wall_clock_changes_do_not_matter() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        t.tick(c.adv(10));
        // clock set back by an hour
        assert!(t.tick(c.jump_wall(-3600)).is_empty());
        t.tick(c.adv(10));
        assert_eq!(t.phase_elapsed(), Duration::from_secs(20));
    }

    #[test]
    fn suspend_is_recovered_from_wall_time() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        t.tick(c.adv(10));
        let ev = t.tick(c.suspend(60)); // slept 60 s
        assert!(ev.is_empty());
        assert_eq!(t.phase_elapsed(), Duration::from_secs(70));
    }

    #[test]
    fn long_suspend_ends_phase_without_overcredit() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        t.tick(c.adv(10));
        let ev = t.tick(c.suspend(8 * 3600));
        assert_eq!(ev[0], TimerEvent::PhaseEnded { phase: Phase::Work, completed: true });
        assert_eq!(t.summary().focus_secs, 100, "capped at the phase length");
        assert_eq!(t.phase(), Phase::ShortBreak);
        assert_eq!(t.phase_elapsed(), Duration::ZERO);
    }

    #[test]
    fn skip_work_is_not_a_cycle() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        let ev = t.skip(c.adv(40));
        assert_eq!(ev[0], TimerEvent::PhaseEnded { phase: Phase::Work, completed: false });
        assert_eq!(t.cycles(), 0);
        assert_eq!(t.phase(), Phase::ShortBreak);
        assert_eq!(t.summary().focus_secs, 40);
        // skipping a break goes back to work
        t.skip(c.adv(1));
        assert_eq!(t.phase(), Phase::Work);
    }

    #[test]
    fn stop_returns_summary_and_resets() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        let (s, ev) = t.stop(c.adv(33));
        assert_eq!(s, Summary { focus_secs: 33, cycles: 0 });
        assert!(ev.contains(&TimerEvent::Stopped));
        assert_eq!(t.phase(), Phase::Idle);
        assert!(!t.is_active());
        // can start again, counters reset
        t.start(Mode::Flow, c.now());
        assert_eq!(t.focus(), Duration::ZERO);
    }

    #[test]
    fn flow_counts_up_and_reminds_once() {
        let mut c = Clk::new();
        let mut cf = cfg();
        cf.flow_reminder = Some(Duration::from_secs(60));
        let mut t = Timer::new(cf);
        t.start(Mode::Flow, c.now());
        assert_eq!(t.phase(), Phase::Flow);
        assert_eq!(t.remaining(), None);
        assert!(t.tick(c.adv(59)).is_empty());
        assert_eq!(t.tick(c.adv(1)), vec![TimerEvent::FlowReminder]);
        assert!(t.tick(c.adv(600)).is_empty());
        assert_eq!(t.focus(), Duration::from_secs(660));
        // flow can't be skipped
        assert!(t.skip(c.now()).is_empty());
    }

    #[test]
    fn flow_suspend_credit_is_capped() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Flow, c.now());
        t.tick(c.suspend(5 * 3600));
        assert_eq!(t.focus(), FLOW_MAX_GAP_CREDIT);
    }

    #[test]
    fn long_break_cadence_cycles_before_long_one_means_always_long() {
        let mut c = Clk::new();
        let mut cf = cfg();
        cf.cycles_before_long = 1;
        let mut t = Timer::new(cf);
        t.start(Mode::Pomodoro, c.now());
        t.tick(c.adv(100));
        assert_eq!(t.phase(), Phase::LongBreak);
    }

    #[test]
    fn toggle_and_formatting() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        assert!(t.toggle(c.now()).is_empty());
        t.start(Mode::Pomodoro, c.now());
        t.toggle(c.adv(5));
        assert!(t.is_paused());
        t.toggle(c.adv(5));
        assert!(t.is_running());
        assert_eq!(fmt_clock(Duration::from_secs(65)), "01:05");
        assert_eq!(fmt_clock(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn progress_fraction() {
        let mut c = Clk::new();
        let mut t = Timer::new(cfg());
        t.start(Mode::Pomodoro, c.now());
        t.tick(c.adv(25));
        assert!((t.progress() - 0.25).abs() < 1e-6);
    }
}
