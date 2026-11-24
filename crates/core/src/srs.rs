//! Spaced repetition. The [`Scheduler`] trait isolates the algorithm so FSRS
//! can be added later; [`Sm2`] is the SuperMemo-2 style implementation.

use crate::models::{Card, CardKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rating {
    Again = 1,
    Hard = 2,
    Good = 3,
    Easy = 4,
}

impl Rating {
    pub const ALL: [Rating; 4] = [Rating::Again, Rating::Hard, Rating::Good, Rating::Easy];
    pub fn from_i32(v: i32) -> Rating {
        match v {
            1 => Rating::Again,
            2 => Rating::Hard,
            4 => Rating::Easy,
            _ => Rating::Good,
        }
    }
}

/// The scheduling-relevant part of a card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardState {
    pub kind: CardKind,
    /// Days since the Unix epoch.
    pub due_day: i64,
    pub interval_days: i64,
    pub ease: f64,
    pub reps: i64,
    pub lapses: i64,
}

impl CardState {
    pub fn new_card() -> Self {
        CardState { kind: CardKind::New, due_day: 0, interval_days: 0, ease: 2.5, reps: 0, lapses: 0 }
    }
    pub fn of(c: &Card) -> Self {
        CardState {
            kind: c.kind,
            due_day: c.due_day,
            interval_days: c.interval_days,
            ease: c.ease,
            reps: c.reps,
            lapses: c.lapses,
        }
    }
    pub fn apply(&self, c: &mut Card) {
        c.kind = self.kind;
        c.due_day = self.due_day;
        c.interval_days = self.interval_days;
        c.ease = self.ease;
        c.reps = self.reps;
        c.lapses = self.lapses;
    }
}

pub trait Scheduler: Send {
    fn name(&self) -> &'static str;
    /// New state after answering `rating` on `today` (epoch day).
    fn review(&self, state: &CardState, rating: Rating, today: i64) -> CardState;
    /// Interval in days each rating would give, in `Rating::ALL` order.
    fn preview(&self, state: &CardState, today: i64) -> [i64; 4] {
        let mut out = [0; 4];
        for (i, r) in Rating::ALL.iter().enumerate() {
            out[i] = self.review(state, *r, today).interval_days;
        }
        out
    }
}

pub const MIN_EASE: f64 = 1.3;
pub const START_EASE: f64 = 2.5;

#[derive(Debug, Clone, Default)]
pub struct Sm2;

impl Scheduler for Sm2 {
    fn name(&self) -> &'static str {
        "SM-2"
    }

    fn review(&self, s: &CardState, rating: Rating, today: i64) -> CardState {
        let mut n = *s;
        let prev = s.interval_days.max(0);
        match rating {
            Rating::Again => {
                // Relearn: due again today (the review screen re-queues it).
                n.kind = CardKind::Learning;
                n.reps = 0;
                n.interval_days = 0;
                n.due_day = today;
                if s.kind == CardKind::Review {
                    n.lapses += 1;
                }
                n.ease = (s.ease - 0.20).max(MIN_EASE);
                return n;
            }
            Rating::Hard => {
                n.interval_days = if s.reps == 0 { 1 } else { ((prev as f64) * 1.2).round() as i64 };
                n.ease = (s.ease - 0.15).max(MIN_EASE);
            }
            Rating::Good => {
                n.interval_days = match s.reps {
                    0 => 1,
                    1 => 6,
                    _ => ((prev as f64) * s.ease).round() as i64,
                };
            }
            Rating::Easy => {
                n.interval_days = match s.reps {
                    0 => 4,
                    1 => 8,
                    _ => ((prev as f64) * s.ease * 1.3).round() as i64,
                };
                n.ease = s.ease + 0.15;
            }
        }
        // Hard must not shrink below 1 day; Good/Easy must always grow.
        n.interval_days = n.interval_days.max(1);
        if matches!(rating, Rating::Good | Rating::Easy) {
            n.interval_days = n.interval_days.max(prev + 1);
        }
        n.reps = s.reps + 1;
        n.kind = CardKind::Review;
        n.due_day = today + n.interval_days;
        n
    }
}

/// "1d", "3w", "2mo", "1.5y" style label for UI buttons.
pub fn fmt_interval(days: i64) -> String {
    match days {
        0 => "<1d".into(),
        1..=29 => format!("{days}d"),
        30..=364 => format!("{}mo", (days as f64 / 30.0).round() as i64),
        _ => format!("{:.1}y", days as f64 / 365.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: i64 = 20_000;

    fn review_state(interval: i64, reps: i64, ease: f64) -> CardState {
        CardState { kind: CardKind::Review, due_day: TODAY, interval_days: interval, ease, reps, lapses: 0 }
    }

    #[test]
    fn new_card_good_sequence_1_6_then_ease() {
        let s0 = CardState::new_card();
        let s1 = Sm2.review(&s0, Rating::Good, TODAY);
        assert_eq!((s1.interval_days, s1.due_day, s1.kind), (1, TODAY + 1, CardKind::Review));
        let s2 = Sm2.review(&s1, Rating::Good, TODAY + 1);
        assert_eq!(s2.interval_days, 6);
        let s3 = Sm2.review(&s2, Rating::Good, TODAY + 7);
        assert_eq!(s3.interval_days, 15); // 6 * 2.5
        assert!((s3.ease - 2.5).abs() < 1e-9);
        assert_eq!(s3.reps, 3);
    }

    #[test]
    fn again_resets_and_counts_lapse_only_for_review_cards() {
        let s = review_state(30, 5, 2.5);
        let a = Sm2.review(&s, Rating::Again, TODAY);
        assert_eq!((a.kind, a.reps, a.interval_days, a.due_day, a.lapses), (CardKind::Learning, 0, 0, TODAY, 1));
        assert!((a.ease - 2.3).abs() < 1e-9);
        // Again on a new card is not a lapse
        let n = Sm2.review(&CardState::new_card(), Rating::Again, TODAY);
        assert_eq!(n.lapses, 0);
        // learning card then Good -> 1 day
        let g = Sm2.review(&a, Rating::Good, TODAY);
        assert_eq!(g.interval_days, 1);
        assert_eq!(g.kind, CardKind::Review);
    }

    #[test]
    fn hard_grows_slowly_and_lowers_ease() {
        let s = review_state(10, 4, 2.5);
        let h = Sm2.review(&s, Rating::Hard, TODAY);
        assert_eq!(h.interval_days, 12);
        assert!((h.ease - 2.35).abs() < 1e-9);
        // hard on tiny interval still >= prev? at least 1 day
        let h2 = Sm2.review(&review_state(1, 2, 2.5), Rating::Hard, TODAY);
        assert_eq!(h2.interval_days, 1);
    }

    #[test]
    fn easy_bonus() {
        let s = review_state(10, 4, 2.5);
        let e = Sm2.review(&s, Rating::Easy, TODAY);
        assert_eq!(e.interval_days, 33); // 10 * 2.5 * 1.3 = 32.5 -> 33
        assert!((e.ease - 2.65).abs() < 1e-9);
        assert_eq!(Sm2.review(&CardState::new_card(), Rating::Easy, TODAY).interval_days, 4);
    }

    #[test]
    fn ease_has_floor() {
        let mut s = review_state(10, 4, 1.35);
        s = Sm2.review(&s, Rating::Hard, TODAY);
        assert!((s.ease - MIN_EASE).abs() < 1e-9);
        let a = Sm2.review(&s, Rating::Again, TODAY);
        assert!((a.ease - MIN_EASE).abs() < 1e-9);
    }

    #[test]
    fn good_always_grows_even_with_low_ease() {
        let s = review_state(1, 5, 1.3);
        let g = Sm2.review(&s, Rating::Good, TODAY);
        assert!(g.interval_days >= 2);
    }

    #[test]
    fn rating_order_monotonic_intervals() {
        let s = review_state(10, 4, 2.5);
        let p = Sm2.preview(&s, TODAY);
        assert!(p[0] < p[1] && p[1] <= p[2] && p[2] < p[3], "{p:?}");
    }

    #[test]
    fn interval_formatting() {
        assert_eq!(fmt_interval(0), "<1d");
        assert_eq!(fmt_interval(6), "6d");
        assert_eq!(fmt_interval(45), "2mo");
        assert_eq!(fmt_interval(730), "2.0y");
    }

    #[test]
    fn rating_parse() {
        assert_eq!(Rating::from_i32(1), Rating::Again);
        assert_eq!(Rating::from_i32(99), Rating::Good);
    }
}
