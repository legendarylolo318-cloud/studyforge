//! Tasks, notes, dashboard and stats parts of the backend.

use super::*;
use crate::{BarItem, DistractionRow, MdBlock, NoteItem, SessionRow, TodoItem};
use chrono::{Datelike, Duration as CDuration, Local, NaiveDate, TimeZone, Timelike};
use studyforge_core::markdown::{self, BlockKind};
use studyforge_core::stats;

pub struct PendingNote {
    pub id: Id,
    pub title: String,
    pub body: String,
    pub subject: Option<Id>,
    pub since: Instant,
}

const GRAY: &str = "#9aa5b1";

fn fmt_date(d: NaiveDate, today: NaiveDate) -> String {
    if d.year() == today.year() {
        d.format("%b %-d").to_string()
    } else {
        d.format("%b %-d, %Y").to_string()
    }
}

fn fmt_when(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|t| t.format("%b %-d %H:%M").to_string()).unwrap_or_default()
}

pub fn blocks_to_ui(blocks: Vec<markdown::Block>) -> Vec<MdBlock> {
    blocks
        .into_iter()
        .map(|b| {
            let (kind, level) = match b.kind {
                BlockKind::Heading(l) => (0, l as i32),
                BlockKind::Paragraph => (1, 0),
                BlockKind::ListItem => (2, 0),
                BlockKind::Code => (3, 0),
                BlockKind::Quote => (4, 0),
                BlockKind::Rule => (5, 0),
            };
            MdBlock { kind, text: b.text.into(), level, bold: b.bold, italic: b.italic, indent: b.indent as i32 }
        })
        .collect()
}

impl Backend {
    pub(super) fn today(&self) -> NaiveDate {
        stats::study_day(chrono::Utc::now().timestamp(), &Local, self.settings.day_start_hour)
    }

    fn subject_name_color(&self, id: Option<Id>) -> (String, String) {
        id.and_then(|id| self.subjects.iter().find(|s| s.id == id))
            .map(|s| (s.name.clone(), s.color.clone()))
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ todos
    pub(super) fn add_todo(&mut self, title: String, subject_idx: i32, due: String, prio: i32) {
        let due = match stats::parse_due(&due, self.today()) {
            Ok(d) => d,
            Err(e) => {
                let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_task_error(e.into()));
                return;
            }
        };
        let subject = if subject_idx > 0 { self.subjects.get(subject_idx as usize - 1).map(|s| s.id) } else { None };
        let due_s = due.map(|d| d.format("%Y-%m-%d").to_string());
        if let Err(e) = self.db.add_todo(&title, subject, due_s.as_deref(), Priority::from_i64(prio as i64), chrono::Utc::now().timestamp()) {
            tracing::error!("add todo: {e}");
        }
        let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_task_error("".into()));
        self.push_todos();
        self.push_dashboard();
    }

    fn todo_item(&self, t: &Todo, today: NaiveDate) -> TodoItem {
        let due_date = t.due.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        let (name, color) = self.subject_name_color(t.subject_id);
        TodoItem {
            id: t.id as i32,
            title: t.title.clone().into(),
            subject: name.into(),
            subject_color: parse_color(if color.is_empty() { GRAY } else { &color }),
            due: due_date.map(|d| fmt_date(d, today)).unwrap_or_default().into(),
            priority: t.priority as i32,
            done: t.done,
            overdue: !t.done && due_date.is_some_and(|d| d < today),
        }
    }

    pub(super) fn push_todos(&self) {
        let today = self.today();
        let mut all = self.db.todos().unwrap_or_default();
        let is_overdue = |t: &Todo| !t.done && t.due.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()).is_some_and(|d| d < today);
        all.retain(|t| match self.todo_filter {
            1 => !t.done,
            2 => t.done,
            3 => is_overdue(t),
            _ => true,
        });
        match self.todo_sort {
            1 => all.sort_by(|a, b| a.done.cmp(&b.done).then(b.priority.cmp(&a.priority)).then(a.due.cmp(&b.due))),
            2 => all.sort_by(|a, b| b.id.cmp(&a.id)),
            _ => all.sort_by(|a, b| {
                a.done
                    .cmp(&b.done)
                    .then(a.due.is_none().cmp(&b.due.is_none()))
                    .then(a.due.cmp(&b.due))
                    .then(b.priority.cmp(&a.priority))
            }),
        }
        let items: Vec<TodoItem> = all.iter().map(|t| self.todo_item(t, today)).collect();
        let (f, s) = (self.todo_filter, self.todo_sort);
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_todos(ModelRc::new(VecModel::from(items)));
            st.set_task_filter(f);
            st.set_task_sort(s);
        });
    }

    // ------------------------------------------------------------ notes
    fn filter_subject(&self) -> Option<Id> {
        if self.note_filter > 0 {
            self.subjects.get(self.note_filter as usize - 1).map(|s| s.id)
        } else {
            None
        }
    }

    pub(super) fn push_note_filter_names(&self) {
        let mut names: Vec<SharedString> = vec!["All subjects".into()];
        names.extend(self.subjects.iter().map(|s| SharedString::from(s.name.as_str())));
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.global::<AppState>().set_note_filter_names(ModelRc::new(VecModel::from(names)));
        });
    }

    pub(super) fn push_notes(&self) {
        let notes = self.db.notes(self.filter_subject(), &self.note_query).unwrap_or_default();
        let cur = self.current_note;
        let items: Vec<NoteItem> = notes
            .iter()
            .map(|n| NoteItem {
                id: n.id as i32,
                title: n.title.clone().into(),
                snippet: n.body.chars().take(90).collect::<String>().replace('\n', " ").into(),
                subject: self.subject_name_color(n.subject_id).0.into(),
                updated: fmt_when(n.updated_at).into(),
                selected: Some(n.id) == cur,
            })
            .collect();
        let filter = self.note_filter;
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_notes(ModelRc::new(VecModel::from(items)));
            st.set_note_filter(filter);
        });
    }

    pub(super) fn search_notes(&mut self, filter: i32, query: String) {
        self.note_filter = filter;
        self.note_query = query;
        self.push_notes();
    }

    pub(super) fn flush_note(&mut self) {
        if let Some(p) = self.pending_note.take() {
            let now = chrono::Utc::now().timestamp();
            match self.db.save_note(p.id, &p.title, &p.body, p.subject, now) {
                Ok(()) => {
                    let msg = format!("Saved {}", Local::now().format("%H:%M:%S"));
                    let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_note_saved(msg.into()));
                }
                Err(e) => tracing::error!("save note: {e}"),
            }
            self.push_notes();
        }
    }

    pub(super) fn new_note(&mut self) {
        self.flush_note();
        let subject = self.filter_subject();
        if let Ok(id) = self.db.add_note(subject, "", chrono::Utc::now().timestamp()) {
            self.select_note(id);
        }
    }

    pub(super) fn select_note(&mut self, id: Id) {
        self.flush_note();
        let Ok(Some(n)) = self.db.get_note(id) else { return };
        self.current_note = Some(id);
        let subj_idx = n.subject_id.and_then(|sid| self.subjects.iter().position(|s| s.id == sid)).map(|i| i as i32 + 1).unwrap_or(0);
        let blocks = blocks_to_ui(markdown::render_blocks(&n.body));
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_current_note_id(id as i32);
            st.set_note_title(n.title.into());
            st.set_note_body(n.body.into());
            st.set_note_subject(subj_idx);
            st.set_note_blocks(ModelRc::new(VecModel::from(blocks)));
            st.set_note_saved("".into());
            st.set_note_load_seq(st.get_note_load_seq() + 1);
        });
        self.push_notes();
    }

    pub(super) fn edit_note(&mut self, title: String, body: String, subject_idx: i32) {
        let Some(id) = self.current_note else { return };
        let subject = if subject_idx > 0 { self.subjects.get(subject_idx as usize - 1).map(|s| s.id) } else { None };
        self.pending_note = Some(PendingNote { id, title, body, subject, since: Instant::now() });
        self.preview_dirty = Some(Instant::now());
    }

    pub(super) fn delete_note(&mut self) {
        if let Some(id) = self.current_note.take() {
            self.pending_note = None;
            let _ = self.db.delete_note(id);
            let _ = self.ui.upgrade_in_event_loop(|ui| {
                let st = ui.global::<AppState>();
                st.set_current_note_id(-1);
                st.set_note_blocks(ModelRc::new(VecModel::from(Vec::<MdBlock>::new())));
            });
            self.push_notes();
        }
    }

    /// Called from the worker tick: debounced autosave + throttled preview.
    pub(super) fn tick_notes(&mut self) {
        if let Some(t) = self.preview_dirty {
            if t.elapsed() >= Duration::from_millis(120) {
                self.preview_dirty = None;
                if let Some(p) = &self.pending_note {
                    let blocks = blocks_to_ui(markdown::render_blocks(&p.body));
                    let _ = self.ui.upgrade_in_event_loop(move |ui| {
                        ui.global::<AppState>().set_note_blocks(ModelRc::new(VecModel::from(blocks)));
                    });
                }
            }
        }
        if self.pending_note.as_ref().is_some_and(|p| p.since.elapsed() >= Duration::from_millis(800)) {
            self.flush_note();
        }
    }

    // ------------------------------------------------------------ dashboard / stats
    fn bars_for_subjects(&self, sessions: &[Session]) -> Vec<BarItem> {
        let by = stats::by_subject(sessions);
        let mut v: Vec<_> = by.into_iter().filter(|(_, s)| *s > 0).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let max = v.first().map(|x| x.1).unwrap_or(1).max(1) as f32;
        v.into_iter()
            .take(8)
            .map(|(id, secs)| {
                let (name, color) = self.subject_name_color(id);
                BarItem {
                    label: if name.is_empty() { "No subject".into() } else { name.into() },
                    value: secs as f32 / max,
                    text: stats::fmt_hm(secs).into(),
                    color: parse_color(if color.is_empty() { GRAY } else { &color }),
                }
            })
            .collect()
    }

    pub(super) fn push_dashboard(&self) {
        let today = self.today();
        let sessions = self.db.sessions_since(0).unwrap_or_default();
        let totals = stats::daily_totals(&sessions, &Local, self.settings.day_start_hour);
        let today_secs = totals.get(&today).copied().unwrap_or(0);
        let week_secs = stats::total_in_range(&totals, stats::week_start(today), today);
        let streak = stats::compute_streak(&totals, today, self.settings.streak_min_mins as i64 * 60);
        let daily_goal = self.settings.daily_goal_mins as i64 * 60;
        let weekly_goal = self.settings.weekly_goal_mins as i64 * 60;
        let ratio = |a: i64, b: i64| if b > 0 { (a as f32 / b as f32).min(1.0) } else { 0.0 };
        let min_secs = self.settings.streak_min_mins as i64 * 60;
        let note = if streak.today_done {
            "Today counts ✓".to_string()
        } else {
            format!("{} more to extend it today", stats::fmt_hm((min_secs - today_secs).max(60)))
        };
        let recent: Vec<Session> = sessions.iter().filter(|s| s.started_at >= chrono::Utc::now().timestamp() - 30 * 86400).cloned().collect();
        let bars = self.bars_for_subjects(&recent);
        let mut todos = self.db.todos().unwrap_or_default();
        todos.retain(|t| !t.done && t.due.is_some());
        todos.sort_by(|a, b| a.due.cmp(&b.due).then(b.priority.cmp(&a.priority)));
        let next: Vec<TodoItem> = todos.iter().take(3).map(|t| self.todo_item(t, today)).collect();
        let due_cards = self.db.total_due(stats::epoch_day(today)).unwrap_or(0);
        let (today_text, week_text) = (stats::fmt_hm(today_secs), stats::fmt_hm(week_secs));
        let today_sub = format!("{} of {}", today_text, stats::fmt_hm(daily_goal));
        let week_sub = format!("{} of {}", week_text, stats::fmt_hm(weekly_goal));
        let block_status = self.block_status.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_dash_today_text(today_text.into());
            st.set_dash_today_progress(ratio(today_secs, daily_goal));
            st.set_dash_today_sub(today_sub.into());
            st.set_dash_week_text(week_text.into());
            st.set_dash_week_progress(ratio(week_secs, weekly_goal));
            st.set_dash_week_sub(week_sub.into());
            st.set_dash_streak(streak.current as i32);
            st.set_dash_freezes(streak.freezes as i32);
            st.set_dash_streak_note(note.into());
            st.set_dash_cards_due(due_cards as i32);
            st.set_dash_block_status(block_status.into());
            st.set_next_tasks(ModelRc::new(VecModel::from(next)));
            st.set_stats_subject_bars(ModelRc::new(VecModel::from(bars)));
        });
    }

    pub(super) fn push_stats(&self) {
        let today = self.today();
        let now = chrono::Utc::now().timestamp();
        let sessions = self.db.sessions_since(0).unwrap_or_default();
        let totals = stats::daily_totals(&sessions, &Local, self.settings.day_start_hour);
        let today_secs = totals.get(&today).copied().unwrap_or(0);
        let week = stats::total_in_range(&totals, stats::week_start(today), today);
        let month = stats::total_in_range(&totals, today - CDuration::days(29), today);
        let all: i64 = totals.values().sum();
        // last 14 days
        let days: Vec<(NaiveDate, i64)> = (0..14).rev().map(|i| today - CDuration::days(i)).map(|d| (d, totals.get(&d).copied().unwrap_or(0))).collect();
        let max = days.iter().map(|d| d.1).max().unwrap_or(1).max(1) as f32;
        let min_secs = self.settings.streak_min_mins as i64 * 60;
        let day_bars: Vec<BarItem> = days
            .iter()
            .map(|(d, s)| BarItem {
                label: d.format("%-d").to_string().into(),
                value: *s as f32 / max,
                text: if *s > 0 { format!("{}m", s / 60).into() } else { "".into() },
                color: if *s >= min_secs { parse_color("#3ec28f") } else { parse_color("#6c8cff") },
            })
            .collect();
        let recent30: Vec<Session> = sessions.iter().filter(|s| s.started_at >= now - 30 * 86400).cloned().collect();
        let subject_bars = self.bars_for_subjects(&recent30);
        let rows: Vec<SessionRow> = self
            .db
            .recent_sessions(25)
            .unwrap_or_default()
            .iter()
            .map(|s| SessionRow {
                when: fmt_when(s.started_at).into(),
                subject: self.subject_name_color(s.subject_id).0.into(),
                goal: s.goal.clone().into(),
                duration: stats::fmt_hm(s.focus_secs).into(),
                outcome: match s.outcome {
                    Some(Outcome::Done) => "Done",
                    Some(Outcome::Partly) => "Partly",
                    Some(Outcome::NotDone) => "Not done",
                    None => "",
                }
                .into(),
            })
            .collect();
        let dist = self.db.distractions_since(now - 30 * 86400, 500).unwrap_or_default();
        let hours = stats::distractions_by_hour(&dist, &Local);
        let hmax = hours.iter().copied().max().unwrap_or(1).max(1) as f32;
        let hour_bars: Vec<BarItem> = hours
            .iter()
            .enumerate()
            .map(|(h, c)| BarItem {
                label: if h % 3 == 0 { format!("{h}").into() } else { "".into() },
                value: *c as f32 / hmax,
                text: if *c > 0 { format!("{c}").into() } else { "".into() },
                color: parse_color("#ef6f6c"),
            })
            .collect();
        let by_target = stats::distractions_by_target(&dist);
        let tmax = by_target.first().map(|x| x.1).unwrap_or(1).max(1) as f32;
        let app_bars: Vec<BarItem> = by_target
            .iter()
            .take(8)
            .map(|(t, c)| BarItem { label: t.clone().into(), value: *c as f32 / tmax, text: format!("{c}").into(), color: parse_color("#ef6f6c") })
            .collect();
        let dist_rows: Vec<DistractionRow> = dist
            .iter()
            .take(60)
            .map(|d| DistractionRow { when: fmt_when(d.ts).into(), target: d.target.clone().into(), list: d.list_name.clone().into(), action: d.action.clone().into() })
            .collect();
        let _ = Timelike::hour(&Local::now());
        let (t, w, m, a) = (stats::fmt_hm(today_secs), stats::fmt_hm(week), stats::fmt_hm(month), stats::fmt_hm(all));
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_stats_today(t.into());
            st.set_stats_week(w.into());
            st.set_stats_month(m.into());
            st.set_stats_all(a.into());
            st.set_stats_day_bars(ModelRc::new(VecModel::from(day_bars)));
            st.set_stats_subject_bars(ModelRc::new(VecModel::from(subject_bars)));
            st.set_stats_sessions(ModelRc::new(VecModel::from(rows)));
            st.set_stats_hour_bars(ModelRc::new(VecModel::from(hour_bars)));
            st.set_stats_app_bars(ModelRc::new(VecModel::from(app_bars)));
            st.set_stats_distractions(ModelRc::new(VecModel::from(dist_rows)));
        });
    }

    pub(super) fn refresh_for_page(&mut self) {
        match self.page {
            0 => self.push_dashboard(),
            2 => self.push_decks(),
            3 => {
                self.push_notes();
            }
            4 => self.push_todos(),
            6 => self.push_stats(),
            _ => {}
        }
        self.last_refresh = Instant::now();
    }
}
