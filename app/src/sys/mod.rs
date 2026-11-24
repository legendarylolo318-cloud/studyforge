//! Platform integration (tray, hotkey, notifications, DND). Everything here is
//! best-effort: failures are logged and never crash the app.

pub mod dnd;
pub mod hotkey;
pub mod notify;
pub mod tray;
