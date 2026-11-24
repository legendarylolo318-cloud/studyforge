//! SQLite storage: WAL, foreign keys, versioned migrations, CRUD helpers.

use crate::models::*;
use crate::settings::Settings;
use rusqlite::{params, Connection, OptionalExtension};
use rusqlite_migration::{Migrations, M};
use std::path::Path;

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

const MIGRATIONS_SQL: &[&str] = &[
    // v1: core schema
    "CREATE TABLE subjects(
        id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, color TEXT NOT NULL DEFAULT '#6c8cff',
        created_at INTEGER NOT NULL DEFAULT 0);
     CREATE TABLE sessions(
        id INTEGER PRIMARY KEY, subject_id INTEGER REFERENCES subjects(id) ON DELETE SET NULL,
        kind TEXT NOT NULL, goal TEXT NOT NULL DEFAULT '', outcome TEXT, note TEXT NOT NULL DEFAULT '',
        started_at INTEGER NOT NULL, ended_at INTEGER, focus_secs INTEGER NOT NULL DEFAULT 0,
        cycles INTEGER NOT NULL DEFAULT 0);
     CREATE INDEX idx_sessions_started ON sessions(started_at);
     CREATE TABLE decks(
        id INTEGER PRIMARY KEY, name TEXT NOT NULL, subject_id INTEGER REFERENCES subjects(id) ON DELETE SET NULL);
     CREATE TABLE cards(
        id INTEGER PRIMARY KEY, deck_id INTEGER NOT NULL REFERENCES decks(id) ON DELETE CASCADE,
        front TEXT NOT NULL, back TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '',
        kind TEXT NOT NULL DEFAULT 'new', due_day INTEGER NOT NULL DEFAULT 0,
        interval_days INTEGER NOT NULL DEFAULT 0, ease REAL NOT NULL DEFAULT 2.5,
        reps INTEGER NOT NULL DEFAULT 0, lapses INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL DEFAULT 0);
     CREATE INDEX idx_cards_deck_due ON cards(deck_id, kind, due_day);
     CREATE TABLE review_log(
        id INTEGER PRIMARY KEY, card_id INTEGER NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
        ts INTEGER NOT NULL, day INTEGER NOT NULL, rating INTEGER NOT NULL, was_new INTEGER NOT NULL DEFAULT 0,
        interval_after INTEGER NOT NULL DEFAULT 0);
     CREATE TABLE notes(
        id INTEGER PRIMARY KEY, subject_id INTEGER REFERENCES subjects(id) ON DELETE SET NULL,
        title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL);
     CREATE TABLE todos(
        id INTEGER PRIMARY KEY, title TEXT NOT NULL, subject_id INTEGER REFERENCES subjects(id) ON DELETE SET NULL,
        due TEXT, priority INTEGER NOT NULL DEFAULT 1, done INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL);
     CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE distractions(
        id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, target TEXT NOT NULL, kind TEXT NOT NULL,
        list_name TEXT NOT NULL DEFAULT '', action TEXT NOT NULL);
     CREATE INDEX idx_distractions_ts ON distractions(ts);
     CREATE TABLE blocklists(
        id INTEGER PRIMARY KEY, name TEXT NOT NULL, preset_key TEXT UNIQUE,
        domains TEXT NOT NULL DEFAULT '[]', processes TEXT NOT NULL DEFAULT '[]',
        steam_games INTEGER NOT NULL DEFAULT 0, mode TEXT NOT NULL DEFAULT 'hard');
     CREATE TABLE break_tips(id INTEGER PRIMARY KEY, text TEXT NOT NULL);
     CREATE TABLE schedules(id INTEGER PRIMARY KEY, json TEXT NOT NULL);",
    // v2: link review_log queries & notes search helper index
    "CREATE INDEX idx_review_log_day ON review_log(day);
     CREATE INDEX idx_notes_updated ON notes(updated_at);",
];

pub fn migrations() -> Migrations<'static> {
    Migrations::new(MIGRATIONS_SQL.iter().map(|s| M::up(s)).collect())
}

pub const DEFAULT_TIPS: &[&str] = &[
    "Stand up and walk around for a minute.",
    "Drink a glass of water.",
    "20-20-20: look at something 20 feet away for 20 seconds.",
    "Stretch your neck, shoulders and wrists.",
    "Take five slow, deep breaths.",
    "Rest your eyes: close them for 30 seconds.",
];

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        // journal_mode returns a row, so use query_row.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations().to_latest(&mut conn)?;
        let db = Db { conn };
        db.seed_defaults()?;
        Ok(db)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    fn seed_defaults(&self) -> Result<()> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM break_tips", [], |r| r.get(0))?;
        if n == 0 {
            for t in DEFAULT_TIPS {
                self.conn.execute("INSERT INTO break_tips(text) VALUES(?)", [t])?;
            }
        }
        // Presets are (re)synced so new domains in updates arrive; custom lists untouched.
        for p in crate::presets::presets() {
            let domains = serde_json::to_string(&p.expanded_domains())?;
            let procs = serde_json::to_string(&p.processes)?;
            self.conn.execute(
                "INSERT INTO blocklists(name, preset_key, domains, processes, steam_games, mode)
                 VALUES(?1, ?2, ?3, ?4, ?5, 'hard')
                 ON CONFLICT(preset_key) DO UPDATE SET name=?1, domains=?3, processes=?4, steam_games=?5",
                params![p.name, p.key, domains, procs, p.steam_games],
            )?;
        }
        Ok(())
    }

    // ---------- settings ----------
    pub fn load_settings(&self) -> Result<Settings> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key='app'", [], |r| r.get(0))
            .optional()?;
        Ok(match v {
            Some(s) => serde_json::from_str::<Settings>(&s).unwrap_or_default().sanitized(),
            None => Settings::default(),
        })
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        let json = serde_json::to_string(&s.clone().sanitized())?;
        self.conn.execute(
            "INSERT INTO settings(key,value) VALUES('app',?1) ON CONFLICT(key) DO UPDATE SET value=?1",
            [json],
        )?;
        Ok(())
    }

    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key=?", [key], |r| r.get(0))
            .optional()?)
    }
    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    // ---------- subjects ----------
    pub fn add_subject(&self, name: &str, color: &str) -> Result<Id> {
        self.conn.execute(
            "INSERT INTO subjects(name,color,created_at) VALUES(?1,?2,strftime('%s','now'))",
            params![name.trim(), color],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn update_subject(&self, id: Id, name: &str, color: &str) -> Result<()> {
        self.conn
            .execute("UPDATE subjects SET name=?1,color=?2 WHERE id=?3", params![name.trim(), color, id])?;
        Ok(())
    }
    pub fn delete_subject(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM subjects WHERE id=?", [id])?;
        Ok(())
    }
    pub fn subjects(&self) -> Result<Vec<Subject>> {
        let mut st = self.conn.prepare("SELECT id,name,color FROM subjects ORDER BY name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| Ok(Subject { id: r.get(0)?, name: r.get(1)?, color: r.get(2)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---------- sessions ----------
    pub fn start_session(&self, subject: Option<Id>, kind: SessionKind, goal: &str, started_at: i64) -> Result<Id> {
        self.conn.execute(
            "INSERT INTO sessions(subject_id,kind,goal,started_at) VALUES(?1,?2,?3,?4)",
            params![subject, kind.as_str(), goal, started_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    /// Update running totals (called periodically and at the end).
    pub fn update_session_progress(&self, id: Id, focus_secs: i64, cycles: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET focus_secs=?1, cycles=?2 WHERE id=?3",
            params![focus_secs, cycles, id],
        )?;
        Ok(())
    }
    pub fn finish_session(
        &self,
        id: Id,
        ended_at: i64,
        focus_secs: i64,
        cycles: i64,
        outcome: Option<Outcome>,
        note: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at=?1, focus_secs=?2, cycles=?3, outcome=?4, note=?5 WHERE id=?6",
            params![ended_at, focus_secs, cycles, outcome.map(|o| o.as_str()), note, id],
        )?;
        Ok(())
    }
    /// Sessions that were left open (crash) get closed with their stored progress.
    pub fn close_dangling_sessions(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE sessions SET ended_at = started_at + focus_secs WHERE ended_at IS NULL",
            [],
        )?)
    }
    fn session_from(r: &rusqlite::Row) -> rusqlite::Result<Session> {
        let outcome: Option<String> = r.get(4)?;
        Ok(Session {
            id: r.get(0)?,
            subject_id: r.get(1)?,
            kind: SessionKind::parse(&r.get::<_, String>(2)?),
            goal: r.get(3)?,
            outcome: outcome.as_deref().and_then(Outcome::parse),
            note: r.get(5)?,
            started_at: r.get(6)?,
            ended_at: r.get(7)?,
            focus_secs: r.get(8)?,
            cycles: r.get(9)?,
        })
    }
    pub fn sessions_since(&self, since: i64) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(
            "SELECT id,subject_id,kind,goal,outcome,note,started_at,ended_at,focus_secs,cycles
             FROM sessions WHERE started_at>=? ORDER BY started_at",
        )?;
        let rows = st.query_map([since], Self::session_from)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn recent_sessions(&self, limit: i64) -> Result<Vec<Session>> {
        let mut st = self.conn.prepare(
            "SELECT id,subject_id,kind,goal,outcome,note,started_at,ended_at,focus_secs,cycles
             FROM sessions ORDER BY started_at DESC LIMIT ?",
        )?;
        let rows = st.query_map([limit], Self::session_from)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---------- decks / cards ----------
    pub fn add_deck(&self, name: &str, subject: Option<Id>) -> Result<Id> {
        self.conn
            .execute("INSERT INTO decks(name,subject_id) VALUES(?1,?2)", params![name.trim(), subject])?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn delete_deck(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM decks WHERE id=?", [id])?;
        Ok(())
    }
    pub fn rename_deck(&self, id: Id, name: &str) -> Result<()> {
        self.conn.execute("UPDATE decks SET name=?1 WHERE id=?2", params![name.trim(), id])?;
        Ok(())
    }
    pub fn deck_summaries(&self, today_day: i64, new_per_day: u32) -> Result<Vec<DeckSummary>> {
        let mut st = self.conn.prepare("SELECT id,name,subject_id FROM decks ORDER BY name COLLATE NOCASE")?;
        let decks: Vec<Deck> = st
            .query_map([], |r| Ok(Deck { id: r.get(0)?, name: r.get(1)?, subject_id: r.get(2)? }))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = vec![];
        for d in decks {
            let total: i64 = self.conn.query_row("SELECT COUNT(*) FROM cards WHERE deck_id=?", [d.id], |r| r.get(0))?;
            let due: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM cards WHERE deck_id=? AND kind!='new' AND due_day<=?",
                params![d.id, today_day],
                |r| r.get(0),
            )?;
            let new_total: i64 =
                self.conn.query_row("SELECT COUNT(*) FROM cards WHERE deck_id=? AND kind='new'", [d.id], |r| r.get(0))?;
            let new_done = self.new_introduced_today(d.id, today_day)?;
            let new_available = new_total.min((new_per_day as i64 - new_done).max(0));
            out.push(DeckSummary { deck: d, total, due, new_available });
        }
        Ok(out)
    }
    pub fn total_due(&self, today_day: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM cards WHERE kind!='new' AND due_day<=?",
            [today_day],
            |r| r.get(0),
        )?)
    }
    fn new_introduced_today(&self, deck_id: Id, today_day: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM review_log l JOIN cards c ON c.id=l.card_id
             WHERE c.deck_id=?1 AND l.day=?2 AND l.was_new=1",
            params![deck_id, today_day],
            |r| r.get(0),
        )?)
    }
    pub fn add_card(&self, deck: Id, front: &str, back: &str, tags: &str) -> Result<Id> {
        self.conn.execute(
            "INSERT INTO cards(deck_id,front,back,tags,created_at) VALUES(?1,?2,?3,?4,strftime('%s','now'))",
            params![deck, front, back, tags],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    /// Bulk insert in one transaction. Returns number inserted.
    pub fn add_cards_bulk(&mut self, deck: Id, cards: &[(String, String, String)]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO cards(deck_id,front,back,tags,created_at) VALUES(?1,?2,?3,?4,strftime('%s','now'))",
            )?;
            for (f, b, t) in cards {
                st.execute(params![deck, f, b, t])?;
            }
        }
        tx.commit()?;
        Ok(cards.len())
    }
    pub fn update_card_text(&self, id: Id, front: &str, back: &str) -> Result<()> {
        self.conn.execute("UPDATE cards SET front=?1, back=?2 WHERE id=?3", params![front, back, id])?;
        Ok(())
    }
    pub fn delete_card(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM cards WHERE id=?", [id])?;
        Ok(())
    }
    fn card_from(r: &rusqlite::Row) -> rusqlite::Result<Card> {
        Ok(Card {
            id: r.get(0)?,
            deck_id: r.get(1)?,
            front: r.get(2)?,
            back: r.get(3)?,
            tags: r.get(4)?,
            kind: CardKind::parse(&r.get::<_, String>(5)?),
            due_day: r.get(6)?,
            interval_days: r.get(7)?,
            ease: r.get(8)?,
            reps: r.get(9)?,
            lapses: r.get(10)?,
        })
    }
    const CARD_COLS: &'static str = "id,deck_id,front,back,tags,kind,due_day,interval_days,ease,reps,lapses";
    pub fn cards_in_deck(&self, deck: Id, limit: i64) -> Result<Vec<Card>> {
        let sql = format!("SELECT {} FROM cards WHERE deck_id=? ORDER BY id DESC LIMIT ?", Self::CARD_COLS);
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params![deck, limit], Self::card_from)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn get_card(&self, id: Id) -> Result<Option<Card>> {
        let sql = format!("SELECT {} FROM cards WHERE id=?", Self::CARD_COLS);
        Ok(self.conn.query_row(&sql, [id], Self::card_from).optional()?)
    }
    /// Build a review queue: due cards (oldest due first) followed by new
    /// cards limited by the per-day budget.
    pub fn review_queue(&self, deck: Id, today_day: i64, new_per_day: u32) -> Result<Vec<Card>> {
        let sql = format!(
            "SELECT {} FROM cards WHERE deck_id=? AND kind!='new' AND due_day<=? ORDER BY due_day, id",
            Self::CARD_COLS
        );
        let mut st = self.conn.prepare(&sql)?;
        let mut cards: Vec<Card> =
            st.query_map(params![deck, today_day], Self::card_from)?.collect::<std::result::Result<_, _>>()?;
        let budget = (new_per_day as i64 - self.new_introduced_today(deck, today_day)?).max(0);
        let sql = format!("SELECT {} FROM cards WHERE deck_id=? AND kind='new' ORDER BY id LIMIT ?", Self::CARD_COLS);
        let mut st = self.conn.prepare(&sql)?;
        let new: Vec<Card> =
            st.query_map(params![deck, budget], Self::card_from)?.collect::<std::result::Result<_, _>>()?;
        cards.extend(new);
        Ok(cards)
    }
    pub fn record_review(&self, card: &Card, rating: i64, was_new: bool, ts: i64, day: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE cards SET kind=?1,due_day=?2,interval_days=?3,ease=?4,reps=?5,lapses=?6 WHERE id=?7",
            params![card.kind.as_str(), card.due_day, card.interval_days, card.ease, card.reps, card.lapses, card.id],
        )?;
        self.conn.execute(
            "INSERT INTO review_log(card_id,ts,day,rating,was_new,interval_after) VALUES(?1,?2,?3,?4,?5,?6)",
            params![card.id, ts, day, rating, was_new, card.interval_days],
        )?;
        Ok(())
    }

    // ---------- notes ----------
    pub fn add_note(&self, subject: Option<Id>, title: &str, now: i64) -> Result<Id> {
        self.conn.execute(
            "INSERT INTO notes(subject_id,title,body,updated_at) VALUES(?1,?2,'',?3)",
            params![subject, title, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn save_note(&self, id: Id, title: &str, body: &str, subject: Option<Id>, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE notes SET title=?1, body=?2, subject_id=?3, updated_at=?4 WHERE id=?5",
            params![title, body, subject, now, id],
        )?;
        Ok(())
    }
    pub fn delete_note(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM notes WHERE id=?", [id])?;
        Ok(())
    }
    fn note_from(r: &rusqlite::Row) -> rusqlite::Result<Note> {
        Ok(Note { id: r.get(0)?, subject_id: r.get(1)?, title: r.get(2)?, body: r.get(3)?, updated_at: r.get(4)? })
    }
    pub fn get_note(&self, id: Id) -> Result<Option<Note>> {
        Ok(self
            .conn
            .query_row("SELECT id,subject_id,title,body,updated_at FROM notes WHERE id=?", [id], Self::note_from)
            .optional()?)
    }
    /// List (or search) notes. Search is a case-insensitive substring match on
    /// title and body, with LIKE wildcards escaped.
    pub fn notes(&self, subject: Option<Id>, query: &str) -> Result<Vec<Note>> {
        let like = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut st = self.conn.prepare(
            "SELECT id,subject_id,title,body,updated_at FROM notes
             WHERE (?1 IS NULL OR subject_id=?1)
               AND (title LIKE ?2 ESCAPE '\\' OR body LIKE ?2 ESCAPE '\\')
             ORDER BY updated_at DESC",
        )?;
        let rows = st.query_map(params![subject, like], Self::note_from)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---------- todos ----------
    pub fn add_todo(&self, title: &str, subject: Option<Id>, due: Option<&str>, prio: Priority, now: i64) -> Result<Id> {
        self.conn.execute(
            "INSERT INTO todos(title,subject_id,due,priority,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![title.trim(), subject, due, prio as i64, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn set_todo_done(&self, id: Id, done: bool) -> Result<()> {
        self.conn.execute("UPDATE todos SET done=?1 WHERE id=?2", params![done, id])?;
        Ok(())
    }
    pub fn delete_todo(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM todos WHERE id=?", [id])?;
        Ok(())
    }
    pub fn todos(&self) -> Result<Vec<Todo>> {
        let mut st = self
            .conn
            .prepare("SELECT id,title,subject_id,due,priority,done,created_at FROM todos ORDER BY done, due IS NULL, due, priority DESC")?;
        let rows = st.query_map([], |r| {
            Ok(Todo {
                id: r.get(0)?,
                title: r.get(1)?,
                subject_id: r.get(2)?,
                due: r.get(3)?,
                priority: Priority::from_i64(r.get(4)?),
                done: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---------- distractions ----------
    pub fn log_distraction(&self, ts: i64, target: &str, kind: DistractionKind, list: &str, action: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO distractions(ts,target,kind,list_name,action) VALUES(?1,?2,?3,?4,?5)",
            params![ts, target, if kind == DistractionKind::App { "app" } else { "site" }, list, action],
        )?;
        Ok(())
    }
    pub fn distractions_since(&self, since: i64, limit: i64) -> Result<Vec<Distraction>> {
        let mut st = self.conn.prepare(
            "SELECT id,ts,target,kind,list_name,action FROM distractions WHERE ts>=? ORDER BY ts DESC LIMIT ?",
        )?;
        let rows = st.query_map(params![since, limit], |r| {
            Ok(Distraction {
                id: r.get(0)?,
                ts: r.get(1)?,
                target: r.get(2)?,
                kind: if r.get::<_, String>(3)? == "app" { DistractionKind::App } else { DistractionKind::Site },
                list_name: r.get(4)?,
                action: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---------- blocklists ----------
    pub fn blocklists(&self) -> Result<Vec<Blocklist>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,preset_key,domains,processes,steam_games,mode FROM blocklists
             ORDER BY preset_key IS NULL, name COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| {
            let d: String = r.get(3)?;
            let p: String = r.get(4)?;
            Ok(Blocklist {
                id: r.get(0)?,
                name: r.get(1)?,
                preset_key: r.get(2)?,
                domains: serde_json::from_str(&d).unwrap_or_default(),
                processes: serde_json::from_str(&p).unwrap_or_default(),
                steam_games: r.get(5)?,
                mode: if r.get::<_, String>(6)? == "gentle" { EnforceMode::Gentle } else { EnforceMode::Hard },
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn save_blocklist(&self, b: &Blocklist) -> Result<Id> {
        let d = serde_json::to_string(&b.domains)?;
        let p = serde_json::to_string(&b.processes)?;
        let mode = if b.mode == EnforceMode::Gentle { "gentle" } else { "hard" };
        if b.id > 0 {
            self.conn.execute(
                "UPDATE blocklists SET name=?1,domains=?2,processes=?3,steam_games=?4,mode=?5 WHERE id=?6",
                params![b.name, d, p, b.steam_games, mode, b.id],
            )?;
            Ok(b.id)
        } else {
            self.conn.execute(
                "INSERT INTO blocklists(name,domains,processes,steam_games,mode) VALUES(?1,?2,?3,?4,?5)",
                params![b.name, d, p, b.steam_games, mode],
            )?;
            Ok(self.conn.last_insert_rowid())
        }
    }
    /// Presets cannot be deleted; only the mode of a preset may change.
    pub fn set_blocklist_mode(&self, id: Id, mode: EnforceMode) -> Result<()> {
        let mode = if mode == EnforceMode::Gentle { "gentle" } else { "hard" };
        self.conn.execute("UPDATE blocklists SET mode=?1 WHERE id=?2", params![mode, id])?;
        Ok(())
    }
    pub fn delete_blocklist(&self, id: Id) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM blocklists WHERE id=?1 AND preset_key IS NULL", [id])? > 0)
    }
    pub fn duplicate_blocklist(&self, id: Id) -> Result<Option<Id>> {
        let all = self.blocklists()?;
        let Some(b) = all.into_iter().find(|b| b.id == id) else { return Ok(None) };
        let copy = Blocklist { id: 0, name: format!("{} (copy)", b.name), preset_key: None, ..b };
        Ok(Some(self.save_blocklist(&copy)?))
    }

    // ---------- schedules (JSON blobs, typed in schedule.rs) ----------
    pub fn schedules(&self) -> Result<Vec<(Id, String)>> {
        let mut st = self.conn.prepare("SELECT id,json FROM schedules ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn add_schedule_json(&self, json: &str) -> Result<Id> {
        self.conn.execute("INSERT INTO schedules(json) VALUES(?)", [json])?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn delete_schedule(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM schedules WHERE id=?", [id])?;
        Ok(())
    }

    // ---------- break tips ----------
    pub fn tips(&self) -> Result<Vec<BreakTip>> {
        let mut st = self.conn.prepare("SELECT id,text FROM break_tips ORDER BY id")?;
        let rows = st.query_map([], |r| Ok(BreakTip { id: r.get(0)?, text: r.get(1)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
    pub fn add_tip(&self, text: &str) -> Result<()> {
        self.conn.execute("INSERT INTO break_tips(text) VALUES(?)", [text.trim()])?;
        Ok(())
    }
    pub fn delete_tip(&self, id: Id) -> Result<()> {
        self.conn.execute("DELETE FROM break_tips WHERE id=?", [id])?;
        Ok(())
    }

    /// Consistent online backup into `dest` (uses VACUUM INTO).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        self.conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_with_wal_fk_and_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let mode: String = db.conn().query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = db.conn().query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS_SQL.len() as i64);
        migrations().validate().unwrap();
    }

    #[test]
    fn subjects_and_cascade() {
        let db = Db::open_in_memory().unwrap();
        let s = db.add_subject("Math", "#ff0000").unwrap();
        let d = db.add_deck("Algebra", Some(s)).unwrap();
        db.add_card(d, "x", "y", "").unwrap();
        db.delete_deck(d).unwrap();
        let n: i64 = db.conn().query_row("SELECT COUNT(*) FROM cards", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        db.delete_subject(s).unwrap();
        assert!(db.subjects().unwrap().is_empty());
    }

    #[test]
    fn settings_roundtrip_and_sanitize() {
        let db = Db::open_in_memory().unwrap();
        let mut s = db.load_settings().unwrap();
        assert_eq!(s.work_mins, 25);
        s.emergency_cooldown_mins = 1;
        s.work_mins = 50;
        db.save_settings(&s).unwrap();
        let s2 = db.load_settings().unwrap();
        assert_eq!(s2.work_mins, 50);
        assert_eq!(s2.emergency_cooldown_mins, crate::hardmode::MIN_EMERGENCY_COOLDOWN_MINS);
    }

    #[test]
    fn presets_seeded_and_protected() {
        let db = Db::open_in_memory().unwrap();
        let lists = db.blocklists().unwrap();
        assert!(lists.iter().any(|l| l.preset_key.as_deref() == Some("social")));
        let social = lists.iter().find(|l| l.preset_key.as_deref() == Some("social")).unwrap();
        assert!(!db.delete_blocklist(social.id).unwrap());
        let copy = db.duplicate_blocklist(social.id).unwrap().unwrap();
        assert!(db.delete_blocklist(copy).unwrap());
    }

    #[test]
    fn notes_search_escapes_wildcards() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add_note(None, "Alpha", 1).unwrap();
        db.save_note(a, "Alpha", "100% sure", None, 2).unwrap();
        let b = db.add_note(None, "Beta", 1).unwrap();
        db.save_note(b, "Beta", "other", None, 2).unwrap();
        assert_eq!(db.notes(None, "100%").unwrap().len(), 1);
        assert_eq!(db.notes(None, "%").unwrap().len(), 1);
        assert_eq!(db.notes(None, "").unwrap().len(), 2);
        assert_eq!(db.notes(None, "BETA").unwrap().len(), 1);
    }

    #[test]
    fn dangling_sessions_closed() {
        let db = Db::open_in_memory().unwrap();
        let id = db.start_session(None, SessionKind::Flow, "g", 1000).unwrap();
        db.update_session_progress(id, 60, 0).unwrap();
        assert_eq!(db.close_dangling_sessions().unwrap(), 1);
        assert_eq!(db.recent_sessions(1).unwrap()[0].ended_at, Some(1060));
    }

    #[test]
    fn backup_works() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("a.db")).unwrap();
        db.add_subject("S", "#fff").unwrap();
        let dest = dir.path().join("b.db");
        db.backup_to(&dest).unwrap();
        let db2 = Db::open(&dest).unwrap();
        assert_eq!(db2.subjects().unwrap().len(), 1);
    }
}
