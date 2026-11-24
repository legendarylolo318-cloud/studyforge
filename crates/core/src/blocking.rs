//! What should be blocked: the data model shared by the GUI and studyd.

use crate::models::{Blocklist, EnforceMode};
use crate::presets::{normalize_domain, normalize_process};
use serde::{Deserialize, Serialize};

/// One list as enforced (a flattened [`Blocklist`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockList {
    pub name: String,
    pub domains: Vec<String>,
    pub processes: Vec<String>,
    pub steam_games: bool,
    pub mode: EnforceMode,
}

impl From<&Blocklist> for BlockList {
    fn from(b: &Blocklist) -> Self {
        BlockList {
            name: b.name.clone(),
            domains: b.domains.clone(),
            processes: b.processes.clone(),
            steam_games: b.steam_games,
            mode: b.mode,
        }
    }
}

/// Allowlist mode: everything NOT listed is blocked.
/// * websites: through browser policies only (URLBlocklist `*` + URLAllowlist);
/// * apps: only processes of the selected lists (the user's "distraction
///   category") that are not allowlisted are touched - never arbitrary ones.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Allowlist {
    pub enabled: bool,
    pub domains: Vec<String>,
    pub processes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockSpec {
    pub lists: Vec<BlockList>,
    pub allowlist: Allowlist,
    /// Also kill the Steam client (only meaningful with a steam_games list).
    pub block_steam_client: bool,
    /// Apply layer-2 browser policies in addition to the hosts file.
    pub browser_policies: bool,
}

impl BlockSpec {
    pub fn is_empty(&self) -> bool {
        self.lists.is_empty() && !self.allowlist.enabled
    }

    /// Normalized, de-duplicated domains to block (allowlisted ones removed).
    pub fn blocked_domains(&self) -> Vec<String> {
        let allow: Vec<String> = self.allowlist.domains.iter().filter_map(|d| normalize_domain(d)).collect();
        let mut out: Vec<String> = Vec::new();
        for l in &self.lists {
            for d in &l.domains {
                if let Some(n) = normalize_domain(d) {
                    if !is_covered_by(&n, &allow) && !out.contains(&n) {
                        out.push(n);
                    }
                }
            }
        }
        out.sort();
        out
    }

    pub fn allowed_domains(&self) -> Vec<String> {
        let mut v: Vec<String> = self.allowlist.domains.iter().filter_map(|d| normalize_domain(d)).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Lowercase process names (no `.exe`) mapped to the list they came from,
    /// strongest enforcement mode first.
    pub fn process_rules(&self) -> Vec<(String, String, EnforceMode)> {
        let allow: Vec<String> = self.allowlist.processes.iter().map(|p| normalize_process(p)).collect();
        let mut out: Vec<(String, String, EnforceMode)> = Vec::new();
        for l in &self.lists {
            for p in &l.processes {
                let n = normalize_process(p);
                if n.is_empty() || allow.contains(&n) {
                    continue;
                }
                match out.iter_mut().find(|(name, _, _)| *name == n) {
                    Some(existing) => {
                        if l.mode == EnforceMode::Hard {
                            existing.2 = EnforceMode::Hard; // strongest wins
                        }
                    }
                    None => out.push((n, l.name.clone(), l.mode)),
                }
            }
        }
        out
    }

    /// Mode and list name for Steam games, if any list enables them.
    pub fn steam_rule(&self) -> Option<(String, EnforceMode)> {
        let mut found: Option<(String, EnforceMode)> = None;
        for l in self.lists.iter().filter(|l| l.steam_games) {
            match &found {
                Some((_, EnforceMode::Hard)) => {}
                _ => {
                    if found.is_none() || l.mode == EnforceMode::Hard {
                        found = Some((l.name.clone(), l.mode));
                    }
                }
            }
        }
        found
    }

    pub fn allowlist_processes(&self) -> Vec<String> {
        self.allowlist.processes.iter().map(|p| normalize_process(p)).collect()
    }

    /// Union of several specs (used for session + schedules active together).
    pub fn merge(specs: &[&BlockSpec]) -> BlockSpec {
        let mut out = BlockSpec::default();
        let mut allow_sets: Vec<&Allowlist> = vec![];
        for s in specs {
            out.lists.extend(s.lists.iter().cloned());
            out.block_steam_client |= s.block_steam_client;
            out.browser_policies |= s.browser_policies;
            if s.allowlist.enabled {
                allow_sets.push(&s.allowlist);
            }
        }
        if !allow_sets.is_empty() {
            out.allowlist.enabled = true;
            for a in allow_sets {
                for d in &a.domains {
                    if !out.allowlist.domains.contains(d) {
                        out.allowlist.domains.push(d.clone());
                    }
                }
                for p in &a.processes {
                    if !out.allowlist.processes.contains(p) {
                        out.allowlist.processes.push(p.clone());
                    }
                }
            }
        }
        out
    }

    pub fn list_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lists.iter().map(|l| l.name.clone()).collect();
        v.dedup();
        v
    }
}

/// Is `domain` equal to, or a subdomain of, any entry of `allow`?
pub fn is_covered_by(domain: &str, allow: &[String]) -> bool {
    allow.iter().any(|a| domain == a || domain.ends_with(&format!(".{a}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(name: &str, domains: &[&str], procs: &[&str], mode: EnforceMode) -> BlockList {
        BlockList {
            name: name.into(),
            domains: domains.iter().map(|s| s.to_string()).collect(),
            processes: procs.iter().map(|s| s.to_string()).collect(),
            steam_games: false,
            mode,
        }
    }

    #[test]
    fn domains_normalized_deduped_and_allowlist_removed() {
        let spec = BlockSpec {
            lists: vec![
                list("a", &["Reddit.com", "https://www.reddit.com/x", "1.2.3.4", "youtube.com"], &[], EnforceMode::Hard),
                list("b", &["reddit.com", "music.youtube.com"], &[], EnforceMode::Hard),
            ],
            allowlist: Allowlist { enabled: false, domains: vec!["youtube.com".into()], processes: vec![] },
            ..Default::default()
        };
        assert_eq!(spec.blocked_domains(), vec!["reddit.com", "www.reddit.com"]);
    }

    #[test]
    fn process_rules_strongest_mode_wins_and_allowlist_exempts() {
        let spec = BlockSpec {
            lists: vec![
                list("soft", &[], &["Discord.exe", "steam"], EnforceMode::Gentle),
                list("hard", &[], &["discord"], EnforceMode::Hard),
            ],
            allowlist: Allowlist { enabled: true, domains: vec![], processes: vec!["Steam".into()] },
            ..Default::default()
        };
        let r = spec.process_rules();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "discord");
        assert_eq!(r[0].2, EnforceMode::Hard);
    }

    #[test]
    fn merge_unions_lists_and_allowlists() {
        let a = BlockSpec { lists: vec![list("a", &["a.com"], &[], EnforceMode::Hard)], browser_policies: true, ..Default::default() };
        let b = BlockSpec {
            lists: vec![list("b", &["b.com"], &[], EnforceMode::Gentle)],
            allowlist: Allowlist { enabled: true, domains: vec!["wiki.org".into()], processes: vec![] },
            block_steam_client: true,
            ..Default::default()
        };
        let m = BlockSpec::merge(&[&a, &b]);
        assert_eq!(m.lists.len(), 2);
        assert!(m.browser_policies && m.block_steam_client && m.allowlist.enabled);
        assert_eq!(m.allowed_domains(), vec!["wiki.org"]);
        assert!(BlockSpec::merge(&[]).is_empty());
    }

    #[test]
    fn steam_rule_prefers_hard() {
        let mut g = list("g1", &[], &[], EnforceMode::Gentle);
        g.steam_games = true;
        let mut h = list("g2", &[], &[], EnforceMode::Hard);
        h.steam_games = true;
        let spec = BlockSpec { lists: vec![g, h], ..Default::default() };
        assert_eq!(spec.steam_rule(), Some(("g2".into(), EnforceMode::Hard)));
        assert_eq!(BlockSpec::default().steam_rule(), None);
    }
}
