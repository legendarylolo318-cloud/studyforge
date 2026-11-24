//! Scheduled blocking rules ("Social Media, Mon-Fri 09:00-17:00").
//!
//! Rules are evaluated against *local wall-clock time*, so "09:00" means 09:00
//! on the clock on the wall, also on days where DST shifts the offset.
//! Ranges may cross midnight (22:00-02:00); the weekday set refers to the day
//! the range *starts* on.

use crate::blocking::BlockSpec;
use chrono::{DateTime, Datelike, TimeZone, Timelike};
use serde::{Deserialize, Serialize};

pub const MINUTES_PER_DAY: u16 = 24 * 60;

/// Weekday bits: Monday = bit 0 ... Sunday = bit 6.
pub const WEEKDAYS: u8 = 0b001_1111;
pub const WEEKEND: u8 = 0b110_0000;
pub const ALL_DAYS: u8 = 0b111_1111;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub days: u8,
    /// Minutes after midnight, local time. 0..1440.
    pub start_min: u16,
    pub end_min: u16,
    pub spec: BlockSpec,
}

impl ScheduleRule {
    /// `start == end` means the whole day on each listed weekday.
    pub fn is_active<Tz: TimeZone>(&self, now: &DateTime<Tz>) -> bool {
        if !self.enabled || self.days & ALL_DAYS == 0 {
            return false;
        }
        let local = now.naive_local();
        let minute = (local.hour() * 60 + local.minute()) as u16;
        let today = local.weekday().num_days_from_monday() as u8;
        let yesterday = (today + 6) % 7;
        let has = |d: u8| self.days & (1 << d) != 0;
        let (s, e) = (self.start_min.min(MINUTES_PER_DAY), self.end_min.min(MINUTES_PER_DAY));
        if s == e {
            return has(today);
        }
        if s < e {
            has(today) && minute >= s && minute < e
        } else {
            // overnight: evening part belongs to today, morning part to the range that started yesterday
            (has(today) && minute >= s) || (has(yesterday) && minute < e)
        }
    }
}

/// A schedule as the GUI stores it (references lists by id so that editing a
/// list updates the schedule). [`resolve_defs`] turns these into rules for studyd.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleDef {
    pub id: i64,
    pub enabled: bool,
    pub days: u8,
    pub start_min: u16,
    pub end_min: u16,
    pub list_ids: Vec<i64>,
}

impl ScheduleDef {
    pub fn list_names(&self, lists: &[crate::models::Blocklist]) -> String {
        let names: Vec<&str> = self.list_ids.iter().filter_map(|id| lists.iter().find(|l| l.id == *id)).map(|l| l.name.as_str()).collect();
        if names.is_empty() {
            "(deleted list)".into()
        } else {
            names.join(", ")
        }
    }
    pub fn time_text(&self) -> String {
        format!("{}–{}", fmt_hhmm(self.start_min), fmt_hhmm(self.end_min))
    }
}

pub fn resolve_defs(defs: &[ScheduleDef], lists: &[crate::models::Blocklist], browser_policies: bool, block_steam_client: bool) -> Vec<ScheduleRule> {
    defs.iter()
        .map(|d| {
            let chosen: Vec<crate::blocking::BlockList> =
                lists.iter().filter(|l| d.list_ids.contains(&l.id)).map(crate::blocking::BlockList::from).collect();
            ScheduleRule {
                id: d.id,
                name: format!("{} · {} {}", d.list_names(lists), fmt_days(d.days), d.time_text()),
                // a schedule whose lists were all deleted must not run as an empty block
                enabled: d.enabled && !chosen.is_empty(),
                days: d.days,
                start_min: d.start_min,
                end_min: d.end_min,
                spec: BlockSpec { lists: chosen, allowlist: Default::default(), block_steam_client, browser_policies },
            }
        })
        .collect()
}

pub fn active_rules<'a, Tz: TimeZone>(rules: &'a [ScheduleRule], now: &DateTime<Tz>) -> Vec<&'a ScheduleRule> {
    rules.iter().filter(|r| r.is_active(now)).collect()
}

pub fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else if h == 24 && m == 0 {
        Some(MINUTES_PER_DAY)
    } else {
        None
    }
}

pub fn fmt_hhmm(min: u16) -> String {
    format!("{:02}:{:02}", (min / 60) % 25, min % 60)
}

pub fn fmt_days(mask: u8) -> String {
    const N: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    if mask & ALL_DAYS == ALL_DAYS {
        return "Every day".into();
    }
    if mask & ALL_DAYS == WEEKDAYS {
        return "Mon–Fri".into();
    }
    if mask & ALL_DAYS == WEEKEND {
        return "Sat–Sun".into();
    }
    (0..7).filter(|i| mask & (1 << i) != 0).map(|i| N[i]).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use chrono_tz::Tz;

    fn rule(days: u8, s: &str, e: &str) -> ScheduleRule {
        ScheduleRule {
            id: 1,
            name: "r".into(),
            enabled: true,
            days,
            start_min: parse_hhmm(s).unwrap(),
            end_min: parse_hhmm(e).unwrap(),
            spec: BlockSpec::default(),
        }
    }
    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }
    // 2025-03-10 is a Monday.

    #[test]
    fn daytime_range_weekdays() {
        let r = rule(WEEKDAYS, "09:00", "17:00");
        assert!(!r.is_active(&utc(2025, 3, 10, 8, 59)));
        assert!(r.is_active(&utc(2025, 3, 10, 9, 0)));
        assert!(r.is_active(&utc(2025, 3, 10, 16, 59)));
        assert!(!r.is_active(&utc(2025, 3, 10, 17, 0)), "end is exclusive");
        assert!(!r.is_active(&utc(2025, 3, 15, 12, 0)), "Saturday");
        assert!(r.is_active(&utc(2025, 3, 14, 12, 0)), "Friday");
    }

    #[test]
    fn overnight_range_uses_start_day() {
        // Mon-Fri 22:00-02:00
        let r = rule(WEEKDAYS, "22:00", "02:00");
        assert!(r.is_active(&utc(2025, 3, 10, 22, 0)), "Mon 22:00");
        assert!(r.is_active(&utc(2025, 3, 11, 1, 59)), "Tue 01:59 (range began Mon)");
        assert!(!r.is_active(&utc(2025, 3, 11, 2, 0)));
        assert!(r.is_active(&utc(2025, 3, 14, 23, 0)), "Fri night");
        assert!(r.is_active(&utc(2025, 3, 15, 1, 0)), "Sat 01:00 belongs to Friday's range");
        assert!(!r.is_active(&utc(2025, 3, 15, 23, 0)), "Sat evening: Saturday not selected");
        assert!(!r.is_active(&utc(2025, 3, 16, 1, 0)), "Sun 01:00: Sat range does not exist");
        assert!(!r.is_active(&utc(2025, 3, 10, 1, 0)), "Mon 01:00: Sunday range does not exist");
        assert!(!r.is_active(&utc(2025, 3, 10, 12, 0)));
    }

    #[test]
    fn midnight_edges() {
        let r = rule(ALL_DAYS, "00:00", "24:00");
        assert!(r.is_active(&utc(2025, 3, 10, 0, 0)) && r.is_active(&utc(2025, 3, 10, 23, 59)));
        let all_day = rule(1, "08:00", "08:00");
        assert!(all_day.is_active(&utc(2025, 3, 10, 3, 0)));
        assert!(!all_day.is_active(&utc(2025, 3, 11, 3, 0)));
    }

    #[test]
    fn disabled_and_empty_day_masks() {
        let mut r = rule(WEEKDAYS, "09:00", "17:00");
        r.enabled = false;
        assert!(!r.is_active(&utc(2025, 3, 10, 12, 0)));
        let r = rule(0, "09:00", "17:00");
        assert!(!r.is_active(&utc(2025, 3, 10, 12, 0)));
    }

    #[test]
    fn local_time_is_used_not_utc() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let r = rule(WEEKDAYS, "09:00", "17:00");
        // 13:30 UTC on Mon Mar 10 2025 = 09:30 EDT (DST started Mar 9)
        let t = utc(2025, 3, 10, 13, 30).with_timezone(&tz);
        assert!(r.is_active(&t));
        let t = utc(2025, 3, 10, 12, 30).with_timezone(&tz); // 08:30 EDT
        assert!(!r.is_active(&t));
    }

    #[test]
    fn dst_spring_forward_overnight() {
        // Europe/Berlin 2025-03-30 (Sunday): 02:00 -> 03:00. Rule Sat+Sun 22:00-04:00.
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        let r = rule(WEEKEND, "22:00", "04:00");
        let at = |h, m| tz.with_ymd_and_hms(2025, 3, 30, h, m, 0).unwrap();
        assert!(r.is_active(&at(1, 59)), "Sat range still running");
        assert!(r.is_active(&at(3, 0)), "03:00 exists; still before 04:00 wall clock");
        assert!(r.is_active(&at(3, 59)));
        assert!(!r.is_active(&at(4, 0)));
        assert!(r.is_active(&at(22, 0)), "Sunday evening starts a new range");
    }

    #[test]
    fn dst_fall_back_repeated_hour_is_active_both_times() {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        let r = rule(WEEKEND, "22:00", "04:00");
        let first = tz.with_ymd_and_hms(2025, 10, 26, 2, 30, 0).earliest().unwrap();
        let second = tz.with_ymd_and_hms(2025, 10, 26, 2, 30, 0).latest().unwrap();
        assert_ne!(first, second);
        assert!(r.is_active(&first) && r.is_active(&second));
        // a rule ending exactly in the repeated hour
        let r2 = rule(WEEKEND, "22:00", "02:30");
        assert!(r2.is_active(&tz.with_ymd_and_hms(2025, 10, 26, 2, 29, 0).earliest().unwrap()));
        assert!(!r2.is_active(&tz.with_ymd_and_hms(2025, 10, 26, 2, 30, 0).earliest().unwrap()));
        // second 02:29 (after fall back) is evaluated again on the wall clock -> active again
        assert!(r2.is_active(&tz.with_ymd_and_hms(2025, 10, 26, 2, 29, 0).latest().unwrap()));
    }

    #[test]
    fn defs_resolve_against_current_lists() {
        use crate::models::{Blocklist, EnforceMode};
        let lists = vec![Blocklist { id: 3, name: "Social".into(), preset_key: None, domains: vec!["a.com".into()], processes: vec!["x".into()], steam_games: false, mode: EnforceMode::Gentle }];
        let def = ScheduleDef { id: 1, enabled: true, days: WEEKDAYS, start_min: 540, end_min: 1020, list_ids: vec![3] };
        let rules = resolve_defs(&[def.clone()], &lists, true, false);
        assert_eq!(rules[0].name, "Social · Mon–Fri 09:00–17:00");
        assert!(rules[0].enabled && rules[0].spec.browser_policies);
        assert_eq!(rules[0].spec.lists[0].mode, EnforceMode::Gentle);
        // editing the list is reflected
        let mut lists2 = lists.clone();
        lists2[0].domains.push("b.com".into());
        assert_eq!(resolve_defs(&[def.clone()], &lists2, true, false)[0].spec.lists[0].domains.len(), 2);
        // deleted list -> rule disabled instead of silently blocking nothing / everything
        let none = resolve_defs(&[def], &[], true, false);
        assert!(!none[0].enabled);
        assert!(none[0].name.contains("deleted"));
    }

    #[test]
    fn rule_filtering_and_formatting() {
        let rules = vec![rule(WEEKDAYS, "09:00", "17:00"), rule(WEEKEND, "09:00", "17:00")];
        let t = utc(2025, 3, 10, 12, 0);
        assert_eq!(active_rules(&rules, &t).len(), 1);
        assert_eq!(parse_hhmm("7:05"), Some(425));
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        assert_eq!(parse_hhmm("25:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("noon"), None);
        assert_eq!(fmt_hhmm(1440), "24:00");
        assert_eq!(fmt_days(WEEKDAYS), "Mon–Fri");
        assert_eq!(fmt_days(0b101), "Mon, Wed");
    }
}
