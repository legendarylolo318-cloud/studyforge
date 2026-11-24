//! Website blocking = hosts file (layer 1) + browser policies (layer 2) + DNS flush.

use crate::hosts::HostsManager;
use crate::paths::Paths;
use crate::policies::{self, PolicySpec};
use studyforge_core::blocking::BlockSpec;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteStatus {
    pub hosts_applied: bool,
    pub policies_applied: bool,
    pub domains: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteReport {
    pub changed: bool,
    pub status: SiteStatus,
    pub warnings: Vec<String>,
}

pub trait SiteBlocker {
    fn apply(&mut self, spec: &BlockSpec) -> SiteReport;
    fn clear(&mut self) -> SiteReport;
    fn status(&self) -> SiteStatus;
}

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), String>;
}

/// Runs real commands; failures are reported but never fatal.
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<(), String> {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| e.to_string())
            .and_then(|s| if s.success() { Ok(()) } else { Err(format!("{program} exited with {s}")) })
    }
}

pub struct SystemSiteBlocker<R: CommandRunner> {
    pub paths: Paths,
    pub dry_run: bool,
    pub runner: R,
    /// Skip DNS flush entirely (tests / dry-run).
    pub flush_dns: bool,
}

impl<R: CommandRunner> SystemSiteBlocker<R> {
    pub fn new(paths: Paths, dry_run: bool, runner: R) -> Self {
        Self { paths, dry_run, runner, flush_dns: true }
    }

    fn hosts(&self) -> HostsManager {
        HostsManager::new(self.paths.hosts.clone(), self.paths.hosts_backup(), self.dry_run)
    }

    fn flush(&self) {
        if self.dry_run || !self.flush_dns {
            return;
        }
        #[cfg(windows)]
        {
            let _ = self.runner.run("ipconfig", &["/flushdns"]);
        }
        #[cfg(not(windows))]
        {
            // No systemd-resolved? Ignore the failure.
            let _ = self.runner.run("resolvectl", &["flush-caches"]);
            let _ = self.runner.run("nscd", &["-i", "hosts"]);
        }
    }

    fn do_apply(&mut self, spec: &BlockSpec, policy_spec: PolicySpec) -> SiteReport {
        let mut report = SiteReport::default();
        let domains = spec.blocked_domains();
        match self.hosts().apply(&domains) {
            Ok(changed) => report.changed |= changed,
            Err(e) => report.warnings.push(format!("hosts file: {e}")),
        }
        let was = policies::is_applied(&self.paths);
        let problems = policies::apply_all(&self.paths, &policy_spec, self.dry_run);
        report.warnings.extend(problems);
        let now = policies::is_applied(&self.paths);
        report.changed |= was != now || (now && !self.dry_run);
        if spec.allowlist.enabled && !spec.browser_policies {
            report.warnings.push(
                "Website allowlisting needs browser policies; they are turned off, so websites are not restricted."
                    .into(),
            );
        }
        if report.changed {
            self.flush();
        }
        report.status = self.status();
        report
    }
}

impl<R: CommandRunner> SiteBlocker for SystemSiteBlocker<R> {
    fn apply(&mut self, spec: &BlockSpec) -> SiteReport {
        let ps = if spec.browser_policies { PolicySpec::from_spec(spec) } else { PolicySpec::default() };
        self.do_apply(spec, ps)
    }

    fn clear(&mut self) -> SiteReport {
        self.do_apply(&BlockSpec::default(), PolicySpec::default())
    }

    fn status(&self) -> SiteStatus {
        SiteStatus {
            hosts_applied: self.hosts().is_applied(),
            policies_applied: policies::is_applied(&self.paths),
            domains: self.hosts().current_domains().len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use studyforge_core::blocking::{Allowlist, BlockList};
    use studyforge_core::models::EnforceMode;

    #[derive(Default)]
    struct Rec(RefCell<Vec<String>>);
    impl CommandRunner for &Rec {
        fn run(&self, p: &str, a: &[&str]) -> Result<(), String> {
            self.0.borrow_mut().push(format!("{p} {}", a.join(" ")));
            Err("not available".into()) // failures must be harmless
        }
    }

    fn spec(domains: &[&str], policies: bool) -> BlockSpec {
        BlockSpec {
            lists: vec![BlockList {
                name: "L".into(),
                domains: domains.iter().map(|s| s.to_string()).collect(),
                processes: vec![],
                steam_games: false,
                mode: EnforceMode::Hard,
            }],
            browser_policies: policies,
            ..Default::default()
        }
    }

    fn blocker<'a>(root: &std::path::Path, rec: &'a Rec) -> SystemSiteBlocker<&'a Rec> {
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/hosts"), "127.0.0.1 localhost\n").unwrap();
        SystemSiteBlocker::new(Paths::under(root), false, rec)
    }

    #[test]
    fn apply_and_clear_leave_system_as_before() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        let r = b.apply(&spec(&["reddit.com"], true));
        assert!(r.changed && r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(r.status.hosts_applied && r.status.policies_applied);
        let hosts = std::fs::read_to_string(root.path().join("etc/hosts")).unwrap();
        assert!(hosts.contains("0.0.0.0 reddit.com") && hosts.contains(":: reddit.com"));
        let r = b.clear();
        assert!(r.changed);
        assert!(!r.status.hosts_applied && !r.status.policies_applied);
        assert_eq!(std::fs::read_to_string(root.path().join("etc/hosts")).unwrap(), "127.0.0.1 localhost\n");
        assert!(!rec.0.borrow().is_empty(), "dns flush attempted (and its failure ignored)");
    }

    #[test]
    fn policies_toggle_off_means_hosts_only() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        let r = b.apply(&spec(&["a.com"], false));
        assert!(r.status.hosts_applied && !r.status.policies_applied);
    }

    #[test]
    fn disabling_policies_removes_previously_applied_ones() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        b.apply(&spec(&["a.com"], true));
        let r = b.apply(&spec(&["a.com"], false));
        assert!(!r.status.policies_applied);
    }

    #[test]
    fn allowlist_without_policies_warns() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        let mut s = spec(&[], false);
        s.allowlist = Allowlist { enabled: true, domains: vec!["wikipedia.org".into()], processes: vec![] };
        let r = b.apply(&s);
        assert!(r.warnings.iter().any(|w| w.contains("allowlist")));
    }

    #[test]
    fn identical_reapply_does_not_flush_dns_again() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        b.apply(&spec(&["a.com"], false));
        let n = rec.0.borrow().len();
        let r = b.apply(&spec(&["a.com"], false));
        assert!(!r.changed);
        assert_eq!(rec.0.borrow().len(), n);
    }

    #[test]
    fn dry_run_touches_nothing_and_does_not_run_commands() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        b.dry_run = true;
        b.apply(&spec(&["a.com"], true));
        assert_eq!(std::fs::read_to_string(root.path().join("etc/hosts")).unwrap(), "127.0.0.1 localhost\n");
        assert!(rec.0.borrow().is_empty());
        assert!(!root.path().join("state").exists());
    }

    #[test]
    fn ip_addresses_are_never_blocked() {
        let root = tempfile::tempdir().unwrap();
        let rec = Rec::default();
        let mut b = blocker(root.path(), &rec);
        b.apply(&spec(&["1.1.1.1", "8.8.8.8", "ok.com"], false));
        let hosts = std::fs::read_to_string(root.path().join("etc/hosts")).unwrap();
        assert!(!hosts.contains("1.1.1.1") && !hosts.contains("8.8.8.8") && hosts.contains("ok.com"));
    }
}
