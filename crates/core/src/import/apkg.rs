//! Anki `.apkg` import.
//!
//! An `.apkg` is a zip containing a SQLite collection (`collection.anki21b` is
//! zstd-compressed; `collection.anki21` / `collection.anki2` are plain) plus
//! numbered media files and a `media` map. Only the collection is read.
//!
//! Safety:
//! * only entries with exactly the known collection names are ever extracted,
//!   and only into a fresh temp dir -> zip-slip is impossible by construction
//!   (entries whose names are not "enclosed" are additionally ignored);
//! * extraction is size-capped while copying (declared sizes are not trusted),
//!   which also covers zstd-compressed bombs;
//! * the number of entries is capped.

use super::text::{cloze_to_front_back, has_cloze, html_to_text};
use super::{ImportCard, ImportError};
use rusqlite::{Connection, OpenFlags};
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ApkgLimits {
    /// Maximum uncompressed size of the extracted collection database.
    pub max_collection_bytes: u64,
    pub max_entries: usize,
}

impl Default for ApkgLimits {
    fn default() -> Self {
        Self { max_collection_bytes: 512 * 1024 * 1024, max_entries: 500_000 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApkgImport {
    pub cards: Vec<ImportCard>,
    pub notes_total: usize,
    pub skipped_empty: usize,
    pub media_skipped: usize,
    /// "anki21b", "anki21" or "anki2".
    pub collection: String,
}

const COLLECTIONS: [&str; 3] = ["collection.anki21b", "collection.anki21", "collection.anki2"];

fn copy_capped<R: Read, W: Write>(mut r: R, mut w: W, cap: u64) -> Result<u64, ImportError> {
    let mut limited = (&mut r).take(cap + 1);
    let n = std::io::copy(&mut limited, &mut w)?;
    if n > cap {
        return Err(ImportError::Invalid(format!(
            "The deck's collection expands to more than {} MB; refusing to import (possible zip bomb).",
            cap / 1024 / 1024
        )));
    }
    Ok(n)
}

pub fn read_apkg(path: &Path, limits: &ApkgLimits) -> Result<ApkgImport, ImportError> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    if zip.len() > limits.max_entries {
        return Err(ImportError::Invalid("Archive has too many entries".into()));
    }

    // Survey entries (never extracting them).
    let mut chosen: Option<(usize, &'static str)> = None;
    let mut media = 0usize;
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i)?;
        if entry.is_dir() {
            continue;
        }
        let Some(enclosed) = entry.enclosed_name() else { continue }; // unsafe path: ignore entirely
        let name = enclosed.to_string_lossy().replace('\\', "/");
        if let Some(known) = COLLECTIONS.iter().position(|c| *c == name) {
            if chosen.is_none_or(|(_, n)| COLLECTIONS.iter().position(|c| *c == n).unwrap() > known) {
                chosen = Some((i, COLLECTIONS[known]));
            }
        } else if name != "media" && name != "meta" && !name.contains('/') {
            media += 1;
        }
    }
    let Some((idx, kind)) = chosen else {
        return Err(ImportError::Invalid("Not an Anki package: no collection database found".into()));
    };

    let tmp = tempfile::tempdir()?;
    let db_path = tmp.path().join("collection.sqlite");
    {
        let entry = zip.by_index(idx)?;
        let out = std::fs::File::create(&db_path)?;
        if kind == "collection.anki21b" {
            let dec = zstd::stream::read::Decoder::new(entry)?;
            copy_capped(dec, out, limits.max_collection_bytes)?;
        } else {
            copy_capped(entry, out, limits.max_collection_bytes)?;
        }
    }

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut result = read_notes(&conn)?;
    result.media_skipped = media;
    result.collection = kind.trim_start_matches("collection.").to_string();
    if result.notes_total == 1
        && result.cards.first().is_some_and(|c| c.front.to_lowercase().contains("update to the latest anki"))
    {
        return Err(ImportError::Invalid(
            "This package only contains the legacy placeholder deck. Re-export it from Anki with 'Support older Anki versions' enabled.".into(),
        ));
    }
    Ok(result)
}

fn read_notes(conn: &Connection) -> Result<ApkgImport, ImportError> {
    let mut st = conn
        .prepare("SELECT flds, tags FROM notes ORDER BY id")
        .map_err(|_| ImportError::Invalid("Collection has no notes table".into()))?;
    let mut rows = st.query([])?;
    let mut out = ApkgImport::default();
    while let Some(r) = rows.next()? {
        out.notes_total += 1;
        let flds: String = r.get(0)?;
        let tags: String = r.get(1).unwrap_or_default();
        let mut fields = flds.split('\u{1f}');
        let f0 = html_to_text(fields.next().unwrap_or(""));
        let f1 = html_to_text(fields.next().unwrap_or(""));
        let (front, back) = if has_cloze(&f0) && f1.is_empty() { cloze_to_front_back(&f0) } else { (f0, f1) };
        if front.is_empty() || back.is_empty() {
            out.skipped_empty += 1;
            continue;
        }
        out.cards.push(ImportCard { front, back, tags: tags.split_whitespace().collect::<Vec<_>>().join(" ") });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn make_collection(dir: &Path, notes: &[(&str, &str)]) -> Vec<u8> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let p = dir.join(format!("c{}.sqlite", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, guid TEXT, mid INTEGER, mod INTEGER, usn INTEGER, tags TEXT, flds TEXT, sfld TEXT, csum INTEGER, flags INTEGER, data TEXT);",
        )
        .unwrap();
        for (i, (flds, tags)) in notes.iter().enumerate() {
            c.execute(
                "INSERT INTO notes VALUES(?1,'g',1,0,0,?2,?3,'',0,0,'')",
                rusqlite::params![i as i64 + 1, tags, flds],
            )
            .unwrap();
        }
        drop(c);
        std::fs::read(&p).unwrap()
    }

    fn write_zip(path: &Path, entries: &[(&str, Vec<u8>)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        for (name, data) in entries {
            z.start_file(*name, SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    fn sample_notes() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Capital of France?\u{1f}<b>Paris</b><br>(city)", " geo europe "),
            ("{{c1::Mitochondria}} make ATP\u{1f}", ""),
            ("image only <img src=\"a.jpg\">\u{1f}", ""),
            ("single field\u{1f}", ""),
            ("Q\u{1f}A\u{1f}extra field ignored", ""),
        ]
    }

    #[test]
    fn imports_anki2_plain() {
        let d = tempfile::tempdir().unwrap();
        let db = make_collection(d.path(), &sample_notes());
        let z = d.path().join("deck.apkg");
        write_zip(
            &z,
            &[("collection.anki2", db), ("0", b"jpg".to_vec()), ("1", b"mp3".to_vec()), ("media", b"{}".to_vec())],
        );
        let r = read_apkg(&z, &ApkgLimits::default()).unwrap();
        assert_eq!(r.collection, "anki2");
        assert_eq!(r.notes_total, 5);
        assert_eq!(r.media_skipped, 2);
        assert_eq!(r.cards.len(), 3);
        assert_eq!(r.skipped_empty, 2);
        assert_eq!(
            r.cards[0],
            ImportCard { front: "Capital of France?".into(), back: "Paris\n(city)".into(), tags: "geo europe".into() }
        );
        assert_eq!(r.cards[1].front, "[...] make ATP");
        assert_eq!(r.cards[1].back, "Mitochondria make ATP");
        assert_eq!(r.cards[2].back, "A");
    }

    #[test]
    fn prefers_anki21b_zstd() {
        let d = tempfile::tempdir().unwrap();
        let db_new = make_collection(d.path(), &[("New\u{1f}Deck", "")]);
        let compressed = zstd::encode_all(&db_new[..], 3).unwrap();
        let dummy = make_collection(d.path(), &[("Please update to the latest Anki version\u{1f}x", "")]);
        let z = d.path().join("deck.apkg");
        write_zip(&z, &[("collection.anki2", dummy), ("collection.anki21b", compressed), ("meta", vec![8, 3])]);
        let r = read_apkg(&z, &ApkgLimits::default()).unwrap();
        assert_eq!(r.collection, "anki21b");
        assert_eq!(r.cards[0].front, "New");
        assert_eq!(r.media_skipped, 0);
    }

    #[test]
    fn legacy_placeholder_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let dummy = make_collection(d.path(), &[("Please update to the latest Anki version\u{1f}x", "")]);
        let z = d.path().join("deck.apkg");
        write_zip(&z, &[("collection.anki2", dummy)]);
        assert!(read_apkg(&z, &ApkgLimits::default()).is_err());
    }

    #[test]
    fn zip_slip_entries_are_ignored_and_nothing_escapes() {
        let d = tempfile::tempdir().unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let work = d.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let db = make_collection(&work, &[("Q\u{1f}A", "")]);
        let z = work.join("deck.apkg");
        write_zip(
            &z,
            &[
                ("../outside/evil.txt", b"pwn".to_vec()),
                ("../collection.anki2", b"evil".to_vec()),
                ("collection.anki2", db),
                ("/abs/evil", b"x".to_vec()),
            ],
        );
        let r = read_apkg(&z, &ApkgLimits::default()).unwrap();
        assert_eq!(r.cards.len(), 1);
        assert_eq!(r.media_skipped, 0, "unsafe names are not even counted as media");
        assert!(!outside.join("evil.txt").exists());
        assert!(!d.path().join("collection.anki2").exists());
    }

    #[test]
    fn zip_bomb_is_capped() {
        let d = tempfile::tempdir().unwrap();
        let big = vec![0u8; 5 * 1024 * 1024]; // compresses to a few KB
        let z = d.path().join("bomb.apkg");
        write_zip(&z, &[("collection.anki2", big)]);
        // zip stores deflate? SimpleFileOptions default is deflated if feature on, else stored; either way cap triggers.
        let lim = ApkgLimits { max_collection_bytes: 1024 * 1024, ..Default::default() };
        let err = read_apkg(&z, &lim).unwrap_err();
        assert!(err.to_string().contains("zip bomb"), "{err}");
    }

    #[test]
    fn zstd_bomb_is_capped() {
        let d = tempfile::tempdir().unwrap();
        let c = zstd::encode_all(&vec![0u8; 4 * 1024 * 1024][..], 19).unwrap();
        assert!(c.len() < 10_000);
        let z = d.path().join("bomb.apkg");
        write_zip(&z, &[("collection.anki21b", c)]);
        let lim = ApkgLimits { max_collection_bytes: 1024 * 1024, ..Default::default() };
        assert!(read_apkg(&z, &lim).is_err());
    }

    #[test]
    fn not_an_apkg() {
        let d = tempfile::tempdir().unwrap();
        let z = d.path().join("x.apkg");
        write_zip(&z, &[("readme.txt", b"hi".to_vec())]);
        assert!(read_apkg(&z, &ApkgLimits::default()).is_err());
        let junk = d.path().join("junk.apkg");
        std::fs::write(&junk, b"not a zip").unwrap();
        assert!(read_apkg(&junk, &ApkgLimits::default()).is_err());
    }

    #[test]
    fn entry_count_cap() {
        let d = tempfile::tempdir().unwrap();
        let z = d.path().join("many.apkg");
        let entries: Vec<(String, Vec<u8>)> = (0..20).map(|i| (i.to_string(), vec![1])).collect();
        let refs: Vec<(&str, Vec<u8>)> = entries.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
        write_zip(&z, &refs);
        let lim = ApkgLimits { max_entries: 10, ..Default::default() };
        assert!(read_apkg(&z, &lim).is_err());
    }
}
