//! StudyForge core: platform-independent models, storage, timer, SRS,
//! stats, schedules, hard-mode rules, IPC messages and blocklist presets.
//! This crate intentionally contains no UI and no platform enforcement code.

pub mod db;
pub mod hardmode;
pub mod import;
pub mod ipc;
pub mod markdown;
pub mod models;
pub mod paths;
pub mod presets;
pub mod protected;
pub mod schedule;
pub mod settings;
pub mod srs;
pub mod stats;
pub mod storage;
pub mod timer;

pub use models::*;
