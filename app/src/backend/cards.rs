//! Flashcards: decks, review sessions, CSV / Anki import.

use super::*;
use crate::{CardRow, CsvRow, DeckRow};
use std::collections::VecDeque;
use studyforge_core::import::apkg::{self, ApkgImport, ApkgLimits};
use studyforge_core::import::csv_import::{self, CsvOptions};
use studyforge_core::import::ImportCard;
use studyforge_core::srs::{self, CardState, Rating};
use studyforge_core::stats;

pub struct ReviewSession {
    pub deck_id: Id,
    pub deck_name: String,
    pub queue: VecDeque<Card>,
    pub current: Option<Card>,
    pub reviewed: u32,
    pub again: u32,
}

pub enum ImportDraft {
    Csv { file: String, rows: Vec<Vec<String>>, opts: CsvOptions },
    Apkg { file: String, data: ApkgImport },
}

fn row(cols: Vec<String>) -> CsvRow {
    CsvRow { cols: ModelRc::new(VecModel::from(cols.into_iter().map(SharedString::from).collect::<Vec<_>>())) }
}

fn short(s: &str, n: usize) -> String {
    let one: String = s.replace('\n', " ⏎ ");
    if one.chars().count() > n {
        format!("{}…", one.chars().take(n).collect::<String>())
    } else {
        one
    }
}

impl Backend {
    fn today_epoch(&self) -> i64 {
        stats::epoch_day(self.today())
    }

    pub(super) fn push_decks(&self) {
        let sums = self.db.deck_summaries(self.today_epoch(), self.settings.new_cards_per_day).unwrap_or_default();
        let rows: Vec<DeckRow> = sums
            .iter()
            .map(|s| DeckRow {
                id: s.deck.id as i32,
                name: s.deck.name.clone().into(),
                subject: s.deck.subject_id.and_then(|id| self.subjects.iter().find(|x| x.id == id)).map(|x| x.name.clone()).unwrap_or_default().into(),
                total: s.total as i32,
                due: s.due as i32,
                new_count: s.new_available as i32,
            })
            .collect();
        let names: Vec<SharedString> = std::iter::once(SharedString::from("New deck…")).chain(sums.iter().map(|s| SharedString::from(s.deck.name.as_str()))).collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_decks(ModelRc::new(VecModel::from(rows)));
            st.set_import_deck_names(ModelRc::new(VecModel::from(names)));
        });
    }

    fn deck_ids(&self) -> Vec<Deck> {
        self.db.deck_summaries(self.today_epoch(), 0).unwrap_or_default().into_iter().map(|s| s.deck).collect()
    }

    pub(super) fn add_deck(&mut self, name: String, subject_idx: i32) {
        if name.trim().is_empty() {
            return;
        }
        let subject = if subject_idx > 0 { self.subjects.get(subject_idx as usize - 1).map(|s| s.id) } else { None };
        let _ = self.db.add_deck(&name, subject);
        self.push_decks();
    }

    pub(super) fn delete_deck(&mut self, id: Id) {
        let _ = self.db.delete_deck(id);
        self.push_decks();
        self.push_dashboard();
    }

    // ------------------------------------------------------------ review
    pub(super) fn study_deck(&mut self, id: Id) {
        let name = self.deck_ids().into_iter().find(|d| d.id == id).map(|d| d.name).unwrap_or_default();
        let queue: VecDeque<Card> = self.db.review_queue(id, self.today_epoch(), self.settings.new_cards_per_day).unwrap_or_default().into();
        if queue.is_empty() {
            return;
        }
        self.review = Some(ReviewSession { deck_id: id, deck_name: name, queue, current: None, reviewed: 0, again: 0 });
        let _ = self.ui.upgrade_in_event_loop(|ui| {
            let st = ui.global::<AppState>();
            st.set_fc_mode(1);
            st.set_review_finished(false);
        });
        self.next_card();
    }

    fn next_card(&mut self) {
        let Some(r) = &mut self.review else { return };
        r.current = r.queue.pop_front();
        let remaining = r.queue.len() as i32 + i32::from(r.current.is_some());
        match &r.current {
            Some(c) => {
                let state = CardState::of(c);
                let intervals = self.sched.preview(&state, stats::epoch_day(stats::study_day(chrono::Utc::now().timestamp(), &chrono::Local, self.settings.day_start_hour)));
                let labels: Vec<SharedString> = intervals.iter().map(|d| SharedString::from(srs::fmt_interval(*d))).collect();
                let (f, b, deck) = (c.front.clone(), c.back.clone(), r.deck_name.clone());
                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                    let st = ui.global::<AppState>();
                    st.set_review_deck(deck.into());
                    st.set_review_front(f.into());
                    st.set_review_back(b.into());
                    st.set_review_answer_shown(false);
                    st.set_review_remaining(remaining);
                    st.set_review_labels(ModelRc::new(VecModel::from(labels)));
                    st.set_review_finished(false);
                });
            }
            None => {
                let summary = format!("{} reviews, {} marked Again.", r.reviewed, r.again);
                let _ = self.ui.upgrade_in_event_loop(move |ui| {
                    let st = ui.global::<AppState>();
                    st.set_review_finished(true);
                    st.set_review_summary(summary.into());
                    st.set_review_remaining(0);
                });
            }
        }
    }

    pub(super) fn show_answer(&mut self) {
        let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_review_answer_shown(true));
    }

    pub(super) fn rate_card(&mut self, rating: i32) {
        let rating = Rating::from_i32(rating);
        let today = self.today_epoch();
        let Some(r) = &mut self.review else { return };
        let Some(mut card) = r.current.take() else { return };
        let was_new = card.kind == CardKind::New;
        let new_state = self.sched.review(&CardState::of(&card), rating, today);
        new_state.apply(&mut card);
        if let Err(e) = self.db.record_review(&card, rating as i64, was_new, chrono::Utc::now().timestamp(), today) {
            tracing::error!("record review: {e}");
        }
        r.reviewed += 1;
        if rating == Rating::Again {
            r.again += 1;
            // Show again later in this session.
            let pos = r.queue.len().min(3);
            r.queue.insert(pos, card);
        }
        self.next_card();
    }

    pub(super) fn end_review(&mut self) {
        self.review = None;
        let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_fc_mode(0));
        self.push_decks();
        self.push_dashboard();
    }

    // ------------------------------------------------------------ browse
    pub(super) fn browse_deck(&mut self, id: Id) {
        self.browse = Some(id);
        let name = self.deck_ids().into_iter().find(|d| d.id == id).map(|d| d.name).unwrap_or_default();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_browse_deck_name(name.into());
            st.set_fc_mode(2);
        });
        self.push_browse();
    }

    fn push_browse(&self) {
        let Some(id) = self.browse else { return };
        let today = self.today_epoch();
        let rows: Vec<CardRow> = self
            .db
            .cards_in_deck(id, 500)
            .unwrap_or_default()
            .into_iter()
            .map(|c| CardRow {
                id: c.id as i32,
                front: short(&c.front, 120).into(),
                back: short(&c.back, 120).into(),
                status: match c.kind {
                    CardKind::New => "new".to_string(),
                    _ if c.due_day <= today => "due".to_string(),
                    _ => format!("in {}", srs::fmt_interval(c.due_day - today)),
                }
                .into(),
            })
            .collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_browse_cards(ModelRc::new(VecModel::from(rows))));
    }

    pub(super) fn close_browse(&mut self) {
        self.browse = None;
        let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_fc_mode(0));
        self.push_decks();
    }

    pub(super) fn add_card(&mut self, front: String, back: String) {
        if let Some(id) = self.browse {
            let _ = self.db.add_card(id, front.trim(), back.trim(), "");
            self.push_browse();
        }
    }

    pub(super) fn delete_card(&mut self, id: Id) {
        let _ = self.db.delete_card(id);
        self.push_browse();
    }

    // ------------------------------------------------------------ import
    pub(super) fn pick_import(&mut self, kind: i32) {
        let tx = self.self_tx.clone();
        std::thread::spawn(move || {
            let dlg = rfd::FileDialog::new();
            let dlg = if kind == 0 {
                dlg.set_title("Choose a CSV file").add_filter("CSV / text", &["csv", "tsv", "txt"])
            } else {
                dlg.set_title("Choose an Anki package").add_filter("Anki package", &["apkg"])
            };
            if let Some(p) = dlg.pick_file() {
                let _ = tx.send(Cmd::ImportFileChosen { kind, path: p });
            }
        });
    }

    fn import_error(&self, msg: String) {
        let _ = self.ui.upgrade_in_event_loop(move |ui| ui.global::<AppState>().set_fc_message(msg.into()));
    }

    pub(super) fn import_file_chosen(&mut self, kind: i32, path: std::path::PathBuf) {
        let file = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Imported").to_string();
        if kind == 0 {
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return self.import_error(format!("Cannot read file: {e}")),
            };
            let text = match csv_import::decode(&bytes) {
                Ok(t) => t,
                Err(e) => return self.import_error(e.to_string()),
            };
            let delim = csv_import::sniff_delimiter(&text);
            let rows = match csv_import::read_rows(&text, delim) {
                Ok(r) => r,
                Err(e) => return self.import_error(format!("CSV parse error: {e}")),
            };
            if rows.is_empty() {
                return self.import_error("The CSV file is empty.".into());
            }
            let ncols = csv_import::column_count(&rows);
            let opts = CsvOptions {
                delimiter: delim,
                has_header: false,
                front_col: 0,
                back_col: if ncols > 1 { 1 } else { 0 },
                tags_col: if ncols > 2 { Some(2) } else { None },
            };
            self.import = Some(ImportDraft::Csv { file, rows, opts });
        } else {
            match apkg::read_apkg(&path, &ApkgLimits::default()) {
                Ok(data) => self.import = Some(ImportDraft::Apkg { file, data }),
                Err(e) => return self.import_error(format!("Anki import failed: {e}")),
            }
        }
        self.push_import(true);
    }

    pub(super) fn csv_update(&mut self, front: i32, back: i32, tags: i32, header: bool) {
        if let Some(ImportDraft::Csv { opts, .. }) = &mut self.import {
            opts.front_col = front.max(0) as usize;
            opts.back_col = back.max(0) as usize;
            opts.tags_col = if tags > 0 { Some(tags as usize - 1) } else { None };
            opts.has_header = header;
        }
        self.push_import(false);
    }

    fn push_import(&self, opening: bool) {
        let Some(draft) = &self.import else { return };
        let (kind, file, info, cards, raw, cols, opts_view): (i32, String, String, Vec<ImportCard>, Vec<Vec<String>>, usize, Option<CsvOptions>) = match draft {
            ImportDraft::Csv { file, rows, opts } => {
                let (cards, skipped) = csv_import::map_rows(rows, opts);
                let info = format!("{} rows, {} cards will be imported, {} skipped (empty front/back)", rows.len(), cards.len(), skipped);
                (0, file.clone(), info, cards, rows.iter().take(5).cloned().collect(), csv_import::column_count(rows), Some(opts.clone()))
            }
            ImportDraft::Apkg { file, data } => {
                let info = format!(
                    "{} notes, {} cards will be imported, {} skipped (empty), {} media files skipped (media is not imported)",
                    data.notes_total,
                    data.cards.len(),
                    data.skipped_empty,
                    data.media_skipped
                );
                (1, file.clone(), info, data.cards.clone(), vec![], 0, None)
            }
        };
        let preview: Vec<Vec<String>> = cards.iter().take(8).map(|c| vec![short(&c.front, 80), short(&c.back, 80)]).collect();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_import_kind(kind);
            st.set_import_file(file.into());
            st.set_import_info(info.into());
            st.set_import_error("".into());
            st.set_import_preview(ModelRc::new(VecModel::from(preview.into_iter().map(row).collect::<Vec<_>>())));
            if let Some(o) = opts_view {
                let col_names: Vec<SharedString> = (0..cols).map(|i| SharedString::from(format!("Column {}", i + 1))).collect();
                let mut tag_names: Vec<SharedString> = vec!["(none)".into()];
                tag_names.extend(col_names.iter().cloned());
                st.set_import_cols(ModelRc::new(VecModel::from(col_names)));
                st.set_import_tag_cols(ModelRc::new(VecModel::from(tag_names)));
                st.set_import_raw(ModelRc::new(VecModel::from(raw.into_iter().map(|r| row(r.iter().map(|c| short(c, 40)).collect())).collect::<Vec<_>>())));
                if opening {
                    st.set_import_map_front(o.front_col as i32);
                    st.set_import_map_back(o.back_col as i32);
                    st.set_import_map_tags(o.tags_col.map(|c| c as i32 + 1).unwrap_or(0));
                    st.set_import_header(o.has_header);
                }
            }
            if opening {
                st.set_import_open(true);
            }
        });
    }

    pub(super) fn import_confirm(&mut self, deck_idx: i32, new_name: String) {
        let Some(draft) = self.import.take() else { return };
        let cards: Vec<ImportCard> = match &draft {
            ImportDraft::Csv { rows, opts, .. } => csv_import::map_rows(rows, opts).0,
            ImportDraft::Apkg { data, .. } => data.cards.clone(),
        };
        let deck_id = if deck_idx > 0 {
            self.deck_ids().get(deck_idx as usize - 1).map(|d| d.id)
        } else {
            let name = if new_name.trim().is_empty() { "Imported".to_string() } else { new_name };
            self.db.add_deck(&name, None).ok()
        };
        let msg = match deck_id {
            Some(id) => {
                let tuples: Vec<_> = cards.iter().map(|c| c.as_tuple()).collect();
                match self.db.add_cards_bulk(id, &tuples) {
                    Ok(n) => format!("Imported {n} cards."),
                    Err(e) => format!("Import failed: {e}"),
                }
            }
            None => "Import failed: deck not found".into(),
        };
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            let st = ui.global::<AppState>();
            st.set_import_open(false);
            st.set_fc_message(msg.into());
        });
        self.push_decks();
        self.push_dashboard();
    }

    pub(super) fn import_cancel(&mut self) {
        self.import = None;
        let _ = self.ui.upgrade_in_event_loop(|ui| ui.global::<AppState>().set_import_open(false));
    }
}
