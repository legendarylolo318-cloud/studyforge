//! Data-directory housekeeping: the cross-machine lock file and daily backups.
//!
//! Folder sync (Syncthing, Dropbox, ...) is deliberately minimal: a lock file
//! containing `hostname + pid` prevents two instances/machines from opening the
//! same SQLite database at once. It is *not* a merge mechanism.

use crate::db::Db;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const LOCK_FILE: &str = "studyforge.lock";
/// A lock nobody refreshed for this long belongs to a crashed/sleeping instance.
pub const STALE_AFTER: Duration = Duration::from_secs(120);
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub host: String,
    pub pid: u32,
    /// Unix seconds when the lock was taken.
    pub started: i64,
}

#[derive(Debug)]
pub enum LockState {
    Acquired(DataLock),
    /// Somebody else holds it. `stale` = safe to take over.
    HeldByOther {
        info: LockInfo,
        stale: bool,
        same_host: bool,
    },
}

#[derive(Debug)]
pub struct DataLock {
    path: PathBuf,
    info: LockInfo,
}

pub fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

impl DataLock {
    pub fn info(&self) -> &LockInfo {
        &self.info
    }

    pub fn acquire(dir: &Path, pid_alive: &dyn Fn(u32) -> bool) -> std::io::Result<LockState> {
        Self::acquire_as(dir, &hostname(), std::process::id(), SystemTime::now(), pid_alive)
    }

    /// Fully injectable version (tests).
    pub fn acquire_as(
        dir: &Path,
        host: &str,
        pid: u32,
        now: SystemTime,
        pid_alive: &dyn Fn(u32) -> bool,
    ) -> std::io::Result<LockState> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOCK_FILE);
        let mine = LockInfo {
            host: host.to_string(),
            pid,
            started: now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
        };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(serde_json::to_string(&mine).map_err(std::io::Error::other)?.as_bytes())?;
                f.sync_all()?;
                return Ok(LockState::Acquired(DataLock { path, info: mine }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let Ok(existing) = serde_json::from_str::<LockInfo>(&text) else {
            // unreadable/corrupt lock: treat as stale garbage
            Self::write(&path, &mine)?;
            return Ok(LockState::Acquired(DataLock { path, info: mine }));
        };
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        let same_host = existing.host == host;
        let stale = if same_host { !pid_alive(existing.pid) || age > STALE_AFTER } else { age > STALE_AFTER };
        if same_host && existing.pid == pid {
            // our own leftover (same pid): just take it
            Self::write(&path, &mine)?;
            return Ok(LockState::Acquired(DataLock { path, info: mine }));
        }
        if same_host && stale {
            // our machine, dead owner: no question to ask
            Self::write(&path, &mine)?;
            return Ok(LockState::Acquired(DataLock { path, info: mine }));
        }
        Ok(LockState::HeldByOther { info: existing, stale, same_host })
    }

    fn write(path: &Path, info: &LockInfo) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_string(info).map_err(std::io::Error::other)?)
    }

    /// Forcefully take the lock ("take over" button).
    pub fn take_over(dir: &Path) -> std::io::Result<DataLock> {
        let path = dir.join(LOCK_FILE);
        let info = LockInfo { host: hostname(), pid: std::process::id(), started: chrono::Utc::now().timestamp() };
        Self::write(&path, &info)?;
        Ok(DataLock { path, info })
    }

    /// Refresh the lock's modification time (call every `HEARTBEAT_EVERY`).
    pub fn heartbeat(&self) -> std::io::Result<()> {
        Self::write(&self.path, &self.info)
    }

    /// A closure that refreshes the lock; safe to move to a heartbeat thread.
    pub fn heartbeat_fn(&self) -> impl Fn() + Send + 'static {
        let (path, info) = (self.path.clone(), self.info.clone());
        move || {
            let _ = Self::write(&path, &info);
        }
    }

    pub fn release(&self) {
        if let Ok(text) = std::fs::read_to_string(&self.path) {
            if serde_json::from_str::<LockInfo>(&text).is_ok_and(|i| i == self.info) {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl Drop for DataLock {
    fn drop(&mut self) {
        self.release();
    }
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

pub const BACKUP_PREFIX: &str = "studyforge-";
pub const BACKUP_SUFFIX: &str = ".sqlite3";
pub const KEEP_BACKUPS: usize = 7;

pub fn backup_file_name(d: NaiveDate) -> String {
    format!("{BACKUP_PREFIX}{}{BACKUP_SUFFIX}", d.format("%Y-%m-%d"))
}

pub fn list_backups(dir: &Path) -> Vec<(NaiveDate, PathBuf)> {
    let mut v: Vec<(NaiveDate, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let date = name.strip_prefix(BACKUP_PREFIX)?.strip_suffix(BACKUP_SUFFIX)?;
                    Some((NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?, e.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Create today's backup if missing and prune to the newest `keep`.
/// Returns the new backup's path when one was created.
pub fn run_backup(db: &Db, dir: &Path, today: NaiveDate, keep: usize) -> Result<Option<PathBuf>, crate::db::DbError> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(backup_file_name(today));
    let created = if target.exists() {
        None
    } else {
        let tmp = dir.join(format!("{}.partial", backup_file_name(today)));
        db.backup_to(&tmp)?;
        std::fs::rename(&tmp, &target)?;
        Some(target)
    };
    let all = list_backups(dir);
    if all.len() > keep {
        for (_, p) in &all[..all.len() - keep] {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }
    fn now_real() -> SystemTime {
        SystemTime::now()
    }

    #[test]
    fn fresh_dir_is_acquired_and_released() {
        let d = tempfile::tempdir().unwrap();
        match DataLock::acquire_as(d.path(), "hostA", 10, now_real(), &|_| true).unwrap() {
            LockState::Acquired(l) => {
                assert!(d.path().join(LOCK_FILE).exists());
                assert_eq!(l.info().host, "hostA");
                drop(l);
                assert!(!d.path().join(LOCK_FILE).exists(), "released on drop");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn second_instance_same_host_is_blocked_while_first_lives() {
        let d = tempfile::tempdir().unwrap();
        let _first = match DataLock::acquire_as(d.path(), "hostA", 10, now_real(), &|_| true).unwrap() {
            LockState::Acquired(l) => l,
            _ => panic!(),
        };
        match DataLock::acquire_as(d.path(), "hostA", 11, now_real(), &|pid| pid == 10).unwrap() {
            LockState::HeldByOther { info, stale, same_host } => {
                assert_eq!(info.pid, 10);
                assert!(!stale && same_host);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dead_owner_on_same_host_is_taken_silently() {
        let d = tempfile::tempdir().unwrap();
        let first = match DataLock::acquire_as(d.path(), "hostA", 10, now_real(), &|_| true).unwrap() {
            LockState::Acquired(l) => l,
            _ => panic!(),
        };
        std::mem::forget(first); // simulate crash: lock file stays
        match DataLock::acquire_as(d.path(), "hostA", 11, now_real(), &|_| false).unwrap() {
            LockState::Acquired(l) => assert_eq!(l.info().pid, 11),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn other_machine_fresh_lock_blocks_but_stale_lock_offers_takeover() {
        let d = tempfile::tempdir().unwrap();
        let first = match DataLock::acquire_as(d.path(), "laptop", 5, now_real(), &|_| true).unwrap() {
            LockState::Acquired(l) => l,
            _ => panic!(),
        };
        std::mem::forget(first);
        // fresh
        match DataLock::acquire_as(d.path(), "desktop", 9, now_real(), &|_| false).unwrap() {
            LockState::HeldByOther { stale, same_host, info } => {
                assert!(!stale && !same_host);
                assert_eq!(info.host, "laptop");
            }
            other => panic!("{other:?}"),
        }
        // 10 minutes later with no heartbeat
        let later = now_real() + Duration::from_secs(600);
        match DataLock::acquire_as(d.path(), "desktop", 9, later, &|_| false).unwrap() {
            LockState::HeldByOther { stale, same_host, .. } => assert!(stale && !same_host),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn heartbeat_refreshes_and_keeps_lock_fresh() {
        let d = tempfile::tempdir().unwrap();
        let LockState::Acquired(l) = DataLock::acquire_as(d.path(), "A", 1, now_real(), &|_| true).unwrap() else {
            panic!()
        };
        l.heartbeat().unwrap();
        let text = std::fs::read_to_string(d.path().join(LOCK_FILE)).unwrap();
        assert_eq!(serde_json::from_str::<LockInfo>(&text).unwrap(), *l.info());
    }

    #[test]
    fn corrupt_lock_file_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(LOCK_FILE), "garbage").unwrap();
        assert!(matches!(
            DataLock::acquire_as(d.path(), "A", 1, now_real(), &|_| true).unwrap(),
            LockState::Acquired(_)
        ));
    }

    #[test]
    fn release_does_not_remove_someone_elses_lock() {
        let d = tempfile::tempdir().unwrap();
        let LockState::Acquired(mine) = DataLock::acquire_as(d.path(), "A", 1, now_real(), &|_| true).unwrap() else {
            panic!()
        };
        // another instance took over
        let other = LockInfo { host: "B".into(), pid: 2, started: 0 };
        std::fs::write(d.path().join(LOCK_FILE), serde_json::to_string(&other).unwrap()).unwrap();
        drop(mine);
        assert!(d.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn take_over_rewrites_lock() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(LOCK_FILE),
            serde_json::to_string(&LockInfo { host: "x".into(), pid: 1, started: 0 }).unwrap(),
        )
        .unwrap();
        let l = DataLock::take_over(d.path()).unwrap();
        assert_eq!(l.info().pid, std::process::id());
        assert_eq!(l.info().host, hostname());
    }

    #[test]
    fn t_helper_is_usable() {
        assert!(t(1) < t(2));
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn backups_are_created_once_per_day_and_pruned_to_seven() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        db.add_subject("S", "#fff").unwrap();
        let bdir = dir.path().join("backups");
        for day in 1..=10 {
            let r = run_backup(&db, &bdir, d(2025, 3, day), KEEP_BACKUPS).unwrap();
            assert!(r.is_some());
        }
        let list = list_backups(&bdir);
        assert_eq!(list.len(), 7);
        assert_eq!(list[0].0, d(2025, 3, 4));
        assert_eq!(list[6].0, d(2025, 3, 10));
        // same day again: nothing new
        assert!(run_backup(&db, &bdir, d(2025, 3, 10), KEEP_BACKUPS).unwrap().is_none());
        // the backup is a usable database
        let restored = Db::open(&list[6].1).unwrap();
        assert_eq!(restored.subjects().unwrap().len(), 1);
        // no partial files left, unrelated files untouched
        std::fs::write(bdir.join("notes.txt"), "keep me").unwrap();
        run_backup(&db, &bdir, d(2025, 3, 11), KEEP_BACKUPS).unwrap();
        assert!(bdir.join("notes.txt").exists());
        assert!(std::fs::read_dir(&bdir).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")));
    }

    #[test]
    fn file_name_roundtrip() {
        assert_eq!(backup_file_name(d(2025, 1, 2)), "studyforge-2025-01-02.sqlite3");
    }
}
