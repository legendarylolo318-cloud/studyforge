//! Card importers: CSV and Anki `.apkg`.

pub mod apkg;
pub mod csv_import;
pub mod text;

/// A card ready to be inserted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCard {
    pub front: String,
    pub back: String,
    pub tags: String,
}

impl ImportCard {
    pub fn as_tuple(&self) -> (String, String, String) {
        (self.front.clone(), self.back.clone(), self.tags.clone())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0}")]
    Invalid(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
}
