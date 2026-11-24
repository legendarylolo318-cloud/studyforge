//! Platform-specific enforcement behind traits. Every path touched is
//! injectable ([`paths::Paths`]); tests only ever use temp directories.

pub mod hosts;
pub mod paths;
pub mod policies;
pub mod process;
pub mod sites;
pub mod steam;

pub use process::{Action, AppBlocker, AppRules, Outcome, ProcessBlocker, SysinfoSource, Violation, ViolationKind};
pub use sites::{SiteBlocker, SiteStatus, SystemCommandRunner, SystemSiteBlocker};
