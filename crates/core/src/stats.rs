//! Study-day arithmetic, totals, streaks with freeze tokens, distraction stats.
//!
//! A "study day" starts at a configurable local hour (default 4am): a session at
//! 01:30 on Tuesday belongs to Monday's study day. All functions are generic
//! over the time zone so DST behaviour can be unit tested.

use crate::models::{Distraction, Id, Session};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use std::collections::BTreeMap;

pub const FREEZE_EVERY_DAYS: u32 = 7;
pub const MAX_FREEZES: u32 = 2;

/// The study day a Unix timestamp belongs to.
pub fn study_day<Tz: TimeZone>(ts: i64, tz: &Tz, day_start_hour: u32) -> NaiveDate {
    let utc: DateTime<Utc> = DateTime::from_timestamp(ts, 0).unwrap_or_default();
    let local = utc.with_timezone(tz).naive_local();
    (local - Duration::hours(day_start_hour as i64)).date()
}

/// Days since the Unix epoch of a study day (used for SRS due dates).
pub fn epoch_day(d: NaiveDate) -> i64 {
    d.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()).num_days()
}

pub fn from_epoch_day(n: i64) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() + Duration::days(n)
}

/// Monday of the week containing `d`.
pub fn week_start(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

/// Focus seconds per study day (a session counts toward the day it started in).
pub fn daily_totals<Tz: TimeZone>(sessions: &[Session], tz: &Tz, day_start_hour: u32) -> BTreeMap<NaiveDate, i64> {
    let mut m = BTreeMap::new();
    for s in sessions {
        *m.entry(study_day(s.started_at, tz, day_start_hour)).or_insert(0) += s.focus_secs;
    }
    m
}

pub fn total_in_range(totals: &BTreeMap<NaiveDate, i64>, from: NaiveDate, to_inclusive: NaiveDate) -> i64 {
    totals.range(from..=to_inclusive).map(|(_, v)| *v).sum()
}

/// Focus seconds per subject (None = untagged).
pub fn by_subject(sessions: &[Session]) -> BTreeMap<Option<Id>, i64> {
    let mut m = BTreeMap::new();
    for s in sessions {
        *m.entry(s.subject_id).or_insert(0) += s.focus_secs;
    }
    m
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreakInfo {
    pub current: u32,
    pub longest: u32,
    /// Freeze tokens currently banked (0..=MAX_FREEZES).
    pub freezes: u32,
    /// Days bridged by a freeze token.
    pub frozen_days: Vec<NaiveDate>,
    /// Whether `today` already reached the minimum.
    pub today_done: bool,
}

/// Replay history to compute streak state.
///
/// * a day qualifies when its total >= `min_secs`;
/// * reaching a streak length that is a multiple of 7 earns one freeze token
///   (max 2 banked);
/// * a missed day (before `today`) automatically consumes a token if one is
///   banked (the streak neither grows nor breaks), otherwise resets to 0;
/// * `today` never breaks a streak - it can still be completed.
pub fn compute_streak(totals: &BTreeMap<NaiveDate, i64>, today: NaiveDate, min_secs: i64) -> StreakInfo {
    let mut info = StreakInfo::default();
    let Some((&first, _)) = totals.iter().find(|(_, v)| **v >= min_secs) else {
        return info;
    };
    let mut d = first;
    while d <= today {
        let ok = totals.get(&d).copied().unwrap_or(0) >= min_secs;
        if ok {
            info.current += 1;
            if info.current % FREEZE_EVERY_DAYS == 0 {
                info.freezes = (info.freezes + 1).min(MAX_FREEZES);
            }
            info.longest = info.longest.max(info.current);
        } else if d < today {
            if info.freezes > 0 {
                info.freezes -= 1;
                info.frozen_days.push(d);
            } else {
                info.current = 0;
            }
        }
        if d == today {
            info.today_done = ok;
        }
        d += Duration::days(1);
    }
    info
}

// ---------------------------------------------------------------------------
// Distractions
// ---------------------------------------------------------------------------

/// Distraction counts per local hour of day (0..24).
pub fn distractions_by_hour<Tz: TimeZone>(items: &[Distraction], tz: &Tz) -> [u32; 24] {
    use chrono::Timelike;
    let mut h = [0u32; 24];
    for d in items {
        if let Some(t) = DateTime::from_timestamp(d.ts, 0) {
            h[t.with_timezone(tz).hour() as usize] += 1;
        }
    }
    h
}

/// Counts per target, most frequent first.
pub fn distractions_by_target(items: &[Distraction]) -> Vec<(String, u32)> {
    let mut m: BTreeMap<String, u32> = BTreeMap::new();
    for d in items {
        *m.entry(d.target.to_lowercase()).or_insert(0) += 1;
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Parse a due date typed by the user: `YYYY-MM-DD`, `today`, `tomorrow`, or
/// `+N` (days from today). Empty input means "no due date" (`Ok(None)`).
pub fn parse_due(input: &str, today: NaiveDate) -> Result<Option<NaiveDate>, String> {
    let t = input.trim().to_lowercase();
    if t.is_empty() {
        return Ok(None);
    }
    match t.as_str() {
        "today" => return Ok(Some(today)),
        "tomorrow" => return Ok(Some(today + Duration::days(1))),
        _ => {}
    }
    if let Some(n) = t.strip_prefix('+') {
        return n
            .parse::<i64>()
            .ok()
            .filter(|n| (0..=3650).contains(n))
            .map(|n| Some(today + Duration::days(n)))
            .ok_or_else(|| format!("Bad day offset '{input}'"));
    }
    NaiveDate::parse_from_str(&t, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| format!("Use YYYY-MM-DD, today, tomorrow or +N (got '{input}')"))
}

pub fn fmt_hm(secs: i64) -> String {
    let m = secs / 60;
    if m >= 60 {
        format!("{}h {:02}m", m / 60, m % 60)
    } else {
        format!("{}m", m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DistractionKind, SessionKind};
    use chrono::{FixedOffset, NaiveDateTime};
    use chrono_tz::Tz;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }
    fn sess(ts: i64, focus: i64, subject: Option<Id>) -> Session {
        Session {
            id: 0,
            subject_id: subject,
            kind: SessionKind::Pomodoro,
            goal: String::new(),
            outcome: None,
            note: String::new(),
            started_at: ts,
            ended_at: None,
            focus_secs: focus,
            cycles: 0,
        }
    }
    fn ts_local(tz: &Tz, y: i32, m: u32, day: u32, h: u32, min: u32) -> i64 {
        let naive: NaiveDateTime = d(y, m, day).and_hms_opt(h, min, 0).unwrap();
        tz.from_local_datetime(&naive).earliest().unwrap().timestamp()
    }
    fn totals_for(days: &[(NaiveDate, i64)]) -> BTreeMap<NaiveDate, i64> {
        days.iter().cloned().collect()
    }
    const MIN: i64 = 25 * 60;

    #[test]
    fn day_boundary_uses_start_hour() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let t = |h, m| {
            DateTime::<Utc>::from_naive_utc_and_offset(d(2025, 3, 11).and_hms_opt(h, m, 0).unwrap(), Utc).timestamp()
        };
        assert_eq!(study_day(t(3, 59), &tz, 4), d(2025, 3, 10));
        assert_eq!(study_day(t(4, 0), &tz, 4), d(2025, 3, 11));
        assert_eq!(study_day(t(0, 0), &tz, 0), d(2025, 3, 11));
        assert_eq!(study_day(t(23, 59), &tz, 4), d(2025, 3, 11));
    }

    #[test]
    fn day_boundary_across_dst_spring_forward() {
        // Berlin: 2025-03-30 02:00 -> 03:00. Day-start 4am must still bucket correctly.
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        let before = ts_local(&tz, 2025, 3, 30, 1, 0); // study day Mar 29
        let after = ts_local(&tz, 2025, 3, 30, 4, 30); // study day Mar 30
        assert_eq!(study_day(before, &tz, 4), d(2025, 3, 29));
        assert_eq!(study_day(after, &tz, 4), d(2025, 3, 30));
        // 03:30 local on the DST day (only 1.5h after the jump) is still the 29th.
        let mid = ts_local(&tz, 2025, 3, 30, 3, 30);
        assert_eq!(study_day(mid, &tz, 4), d(2025, 3, 29));
    }

    #[test]
    fn day_boundary_across_dst_fall_back() {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        // 2025-10-26: 03:00 -> 02:00 (02:00-03:00 happens twice)
        let a = tz.with_ymd_and_hms(2025, 10, 26, 2, 30, 0).earliest().unwrap().timestamp();
        let b = tz.with_ymd_and_hms(2025, 10, 26, 2, 30, 0).latest().unwrap().timestamp();
        assert_ne!(a, b);
        assert_eq!(study_day(a, &tz, 4), d(2025, 10, 25));
        assert_eq!(study_day(b, &tz, 4), d(2025, 10, 25));
    }

    #[test]
    fn epoch_day_roundtrip_and_week_start() {
        assert_eq!(epoch_day(d(1970, 1, 2)), 1);
        assert_eq!(from_epoch_day(epoch_day(d(2025, 3, 11))), d(2025, 3, 11));
        assert_eq!(week_start(d(2025, 3, 12)), d(2025, 3, 10)); // Wed -> Mon
        assert_eq!(week_start(d(2025, 3, 10)), d(2025, 3, 10));
        assert_eq!(week_start(d(2025, 3, 16)), d(2025, 3, 10)); // Sun
    }

    #[test]
    fn totals_and_subjects() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let day = |h| {
            DateTime::<Utc>::from_naive_utc_and_offset(d(2025, 3, 11).and_hms_opt(h, 0, 0).unwrap(), Utc).timestamp()
        };
        let s = vec![sess(day(10), 600, Some(1)), sess(day(12), 300, Some(1)), sess(day(14), 100, None)];
        let t = daily_totals(&s, &tz, 4);
        assert_eq!(t[&d(2025, 3, 11)], 1000);
        assert_eq!(total_in_range(&t, d(2025, 3, 10), d(2025, 3, 11)), 1000);
        assert_eq!(total_in_range(&t, d(2025, 3, 12), d(2025, 3, 13)), 0);
        let bs = by_subject(&s);
        assert_eq!(bs[&Some(1)], 900);
        assert_eq!(bs[&None], 100);
    }

    #[test]
    fn streak_basic_and_today_does_not_break() {
        let days: Vec<_> = (1..=3).map(|i| (d(2025, 3, i), MIN)).collect();
        let t = totals_for(&days);
        let s = compute_streak(&t, d(2025, 3, 4), MIN); // today not done yet
        assert_eq!(s.current, 3);
        assert!(!s.today_done);
        let s = compute_streak(&t, d(2025, 3, 3), MIN);
        assert!(s.today_done);
        assert_eq!(s.current, 3);
    }

    #[test]
    fn streak_threshold_is_inclusive() {
        let t = totals_for(&[(d(2025, 3, 1), MIN - 1), (d(2025, 3, 2), MIN)]);
        let s = compute_streak(&t, d(2025, 3, 2), MIN);
        assert_eq!(s.current, 1);
    }

    #[test]
    fn missed_day_without_freeze_breaks() {
        let t = totals_for(&[(d(2025, 3, 1), MIN), (d(2025, 3, 2), MIN), (d(2025, 3, 4), MIN)]);
        let s = compute_streak(&t, d(2025, 3, 4), MIN);
        assert_eq!(s.current, 1);
        assert_eq!(s.longest, 2);
        assert_eq!(s.freezes, 0);
    }

    #[test]
    fn freeze_earned_at_seven_and_used_on_miss() {
        // 7 days -> 1 token. miss day 8 -> token consumed, streak preserved. day 9 continues.
        let mut days: Vec<_> = (1..=7).map(|i| (d(2025, 3, i), MIN)).collect();
        days.push((d(2025, 3, 9), MIN));
        let t = totals_for(&days);
        let s = compute_streak(&t, d(2025, 3, 9), MIN);
        assert_eq!(s.current, 8);
        assert_eq!(s.freezes, 0);
        assert_eq!(s.frozen_days, vec![d(2025, 3, 8)]);
        // without the miss the token stays banked
        let t2 = totals_for(&(1..=7).map(|i| (d(2025, 3, i), MIN)).collect::<Vec<_>>());
        let s2 = compute_streak(&t2, d(2025, 3, 7), MIN);
        assert_eq!((s2.current, s2.freezes), (7, 1));
    }

    #[test]
    fn two_misses_with_one_freeze_break() {
        let mut days: Vec<_> = (1..=7).map(|i| (d(2025, 3, i), MIN)).collect();
        days.push((d(2025, 3, 11), MIN)); // missed 8, 9, 10
        let t = totals_for(&days);
        let s = compute_streak(&t, d(2025, 3, 11), MIN);
        assert_eq!(s.current, 1);
        assert_eq!(s.longest, 7);
    }

    #[test]
    fn freezes_capped_at_two() {
        let t = totals_for(&(0..28).map(|i| (d(2025, 1, 1) + Duration::days(i), MIN)).collect::<Vec<_>>());
        let s = compute_streak(&t, d(2025, 1, 1) + Duration::days(27), MIN);
        assert_eq!(s.current, 28);
        assert_eq!(s.freezes, MAX_FREEZES);
    }

    #[test]
    fn today_missing_with_freeze_does_not_consume_it() {
        let t = totals_for(&(1..=7).map(|i| (d(2025, 3, i), MIN)).collect::<Vec<_>>());
        let s = compute_streak(&t, d(2025, 3, 8), MIN);
        assert_eq!((s.current, s.freezes), (7, 1));
        assert!(s.frozen_days.is_empty());
    }

    #[test]
    fn empty_and_below_threshold_history() {
        let s = compute_streak(&BTreeMap::new(), d(2025, 3, 1), MIN);
        assert_eq!(s, StreakInfo::default());
        let t = totals_for(&[(d(2025, 3, 1), 10)]);
        assert_eq!(compute_streak(&t, d(2025, 3, 1), MIN).current, 0);
    }

    #[test]
    fn freeze_then_streak_keeps_earning() {
        // 14 straight days -> 2 tokens banked, then 2 misses consume both, streak survives.
        let mut days: Vec<_> = (0..14).map(|i| (d(2025, 2, 1) + Duration::days(i), MIN)).collect();
        days.push((d(2025, 2, 17), MIN));
        let t = totals_for(&days);
        let s = compute_streak(&t, d(2025, 2, 17), MIN);
        assert_eq!(s.current, 15);
        assert_eq!(s.freezes, 0);
        assert_eq!(s.frozen_days.len(), 2);
    }

    #[test]
    fn distraction_stats() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let mk = |ts, target: &str| Distraction {
            id: 0,
            ts,
            target: target.into(),
            kind: DistractionKind::App,
            list_name: String::new(),
            action: "killed".into(),
        };
        // 23:30 UTC = 00:30 local
        let base =
            DateTime::<Utc>::from_naive_utc_and_offset(d(2025, 3, 11).and_hms_opt(23, 30, 0).unwrap(), Utc).timestamp();
        let items = vec![mk(base, "Discord"), mk(base + 60, "discord"), mk(base + 3600, "reddit.com")];
        let h = distractions_by_hour(&items, &tz);
        assert_eq!(h[0], 2);
        assert_eq!(h[1], 1);
        let by = distractions_by_target(&items);
        assert_eq!(by[0], ("discord".to_string(), 2));
    }

    #[test]
    fn due_parsing() {
        let t = d(2025, 3, 11);
        assert_eq!(parse_due("", t), Ok(None));
        assert_eq!(parse_due(" Tomorrow ", t), Ok(Some(d(2025, 3, 12))));
        assert_eq!(parse_due("+3", t), Ok(Some(d(2025, 3, 14))));
        assert_eq!(parse_due("2025-04-01", t), Ok(Some(d(2025, 4, 1))));
        assert!(parse_due("2025-13-01", t).is_err());
        assert!(parse_due("+-1", t).is_err());
        assert!(parse_due("soon", t).is_err());
    }

    #[test]
    fn hm_format() {
        assert_eq!(fmt_hm(59), "0m");
        assert_eq!(fmt_hm(90 * 60), "1h 30m");
    }
}
