//! CSV import with delimiter sniffing and column mapping.

use super::{ImportCard, ImportError};

pub const MAX_CSV_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub has_header: bool,
    pub front_col: usize,
    pub back_col: usize,
    pub tags_col: Option<usize>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self { delimiter: b',', has_header: false, front_col: 0, back_col: 1, tags_col: None }
    }
}

/// Remove a UTF-8 BOM and decode lossily.
pub fn decode(bytes: &[u8]) -> Result<String, ImportError> {
    if bytes.len() > MAX_CSV_BYTES {
        return Err(ImportError::Invalid(format!("CSV file is larger than {} MB", MAX_CSV_BYTES / 1024 / 1024)));
    }
    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    Ok(String::from_utf8_lossy(b).into_owned())
}

/// Pick the delimiter (comma, semicolon, tab) that gives the most consistent
/// column count over the first lines.
pub fn sniff_delimiter(text: &str) -> u8 {
    let mut best = (b',', 0usize);
    for d in *b",;\t" {
        let mut rdr =
            csv::ReaderBuilder::new().delimiter(d).has_headers(false).flexible(true).from_reader(text.as_bytes());
        let counts: Vec<usize> = rdr.records().take(20).filter_map(|r| r.ok()).map(|r| r.len()).collect();
        if counts.is_empty() {
            continue;
        }
        let first = counts[0];
        let consistent = counts.iter().filter(|&&c| c == first).count();
        let score = if first > 1 { consistent * 100 + first } else { 0 };
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

pub fn read_rows(text: &str, delimiter: u8) -> Result<Vec<Vec<String>>, ImportError> {
    let mut rdr =
        csv::ReaderBuilder::new().delimiter(delimiter).has_headers(false).flexible(true).from_reader(text.as_bytes());
    let mut rows = Vec::new();
    for r in rdr.records() {
        let r = r?;
        rows.push(r.iter().map(|s| s.to_string()).collect());
    }
    Ok(rows)
}

pub fn column_count(rows: &[Vec<String>]) -> usize {
    rows.iter().take(50).map(|r| r.len()).max().unwrap_or(0)
}

/// Apply the mapping. Rows with an empty front or back are skipped; returns
/// (cards, skipped count).
pub fn map_rows(rows: &[Vec<String>], opts: &CsvOptions) -> (Vec<ImportCard>, usize) {
    let mut cards = Vec::new();
    let mut skipped = 0;
    for (i, r) in rows.iter().enumerate() {
        if i == 0 && opts.has_header {
            continue;
        }
        let get = |c: usize| r.get(c).map(|s| s.trim()).unwrap_or("");
        let (f, b) = (get(opts.front_col), get(opts.back_col));
        if f.is_empty() || b.is_empty() {
            skipped += 1;
            continue;
        }
        let tags = opts.tags_col.map(|c| get(c).replace(',', " ")).unwrap_or_default();
        cards.push(ImportCard { front: f.to_string(), back: b.to_string(), tags });
    }
    (cards, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_delimiters() {
        assert_eq!(sniff_delimiter("a,b\nc,d\n"), b',');
        assert_eq!(sniff_delimiter("a;b;c\nd;e;f\n"), b';');
        assert_eq!(sniff_delimiter("a\tb\nc\td\n"), b'\t');
        assert_eq!(sniff_delimiter("just one column\nanother\n"), b',');
    }

    #[test]
    fn quoted_fields_and_newlines() {
        let rows = read_rows("front,back\n\"Hello, world\",\"line1\nline2\"\n", b',').unwrap();
        assert_eq!(rows[1][0], "Hello, world");
        assert_eq!(rows[1][1], "line1\nline2");
    }

    #[test]
    fn mapping_header_tags_and_skips() {
        let rows = read_rows("front,back,tags\nq1,a1,\"bio,cell\"\n,a2,\nq3,,\nq4,a4,x\n", b',').unwrap();
        let opts = CsvOptions { has_header: true, tags_col: Some(2), ..Default::default() };
        let (cards, skipped) = map_rows(&rows, &opts);
        assert_eq!(cards.len(), 2);
        assert_eq!(skipped, 2);
        assert_eq!(cards[0], ImportCard { front: "q1".into(), back: "a1".into(), tags: "bio cell".into() });
        // swapped columns
        let swapped = CsvOptions { front_col: 1, back_col: 0, has_header: true, ..Default::default() };
        let (c, _) = map_rows(&rows, &swapped);
        assert_eq!(c[0].front, "a1");
    }

    #[test]
    fn ragged_rows_and_bad_column() {
        let rows = read_rows("a,b\nonly\nc,d,e\n", b',').unwrap();
        assert_eq!(column_count(&rows), 3);
        let (c, s) = map_rows(&rows, &CsvOptions { back_col: 9, ..Default::default() });
        assert!(c.is_empty());
        assert_eq!(s, 3);
    }

    #[test]
    fn bom_empty_lines_quotes_and_crlf_together() {
        let bytes = b"\xef\xbb\xbf\"front, with comma\",\"back \"\"quoted\"\"\"\r\n\r\n\r\nq2,a2\r\n\n";
        let text = decode(bytes).unwrap();
        let rows = read_rows(&text, sniff_delimiter(&text)).unwrap();
        let (cards, skipped) = map_rows(&rows, &CsvOptions::default());
        assert_eq!(skipped, 0, "blank lines are not rows");
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].front, "front, with comma");
        assert_eq!(cards[0].back, "back \"quoted\"");
        assert_eq!(cards[1].front, "q2");
    }

    #[test]
    fn bom_and_size_limit() {
        let s = decode(&[0xEF, 0xBB, 0xBF, b'a', b',', b'b']).unwrap();
        assert_eq!(s, "a,b");
        assert!(decode(&vec![b'x'; MAX_CSV_BYTES + 1]).is_err());
    }
}
