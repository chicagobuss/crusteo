//! Watching the coding-agent CLIs on every host: when one upgrades itself,
//! or a model appears or goes, the user is told once for the whole fleet.
//!
//! A [`Watch`] is fed each host's providers snapshot as it is read or
//! pushed ([`Watch::observe`]). A host's first snapshot is a baseline; later
//! ones are compared with it. Changes gather for a while ([`DEBOUNCE`]) so
//! ten hosts upgrading together are one notice, "Claude Code 2.1.320 on 7 of
//! 10 hosts", and the notice is sent again, under the same id, as more
//! hosts catch up ([`Watch::flush`]). A model that appears with no version
//! change is a notice of its own.
//!
//! No clock and no I/O: callers pass the time, and send the notices. Reef
//! and the desktop app share it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

use crate::providers::Provider;

/// How long changes gather before they are told.
pub const DEBOUNCE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq)]
struct Seen {
    label: String,
    version: Option<String>,
    install_kind: Option<String>,
    /// Model id to label.
    models: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A CLI version, on as many hosts as have it so far.
    CliUpdate,
    /// Models that appeared or went, with no version to name.
    Models,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::CliUpdate => "cli_update",
            Kind::Models => "models",
        }
    }
}

/// Something to tell the user. The same `id` again replaces what was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub id: String,
    pub kind: Kind,
    pub provider: String,
    pub title: String,
    pub body: String,
    pub version: Option<String>,
    pub hosts_on: usize,
    pub hosts_total: usize,
    pub new_models: Vec<String>,
    pub gone_models: Vec<String>,
}

/// One host's provider, as a hosts screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub provider: String,
    pub label: String,
    pub version: Option<String>,
    pub install_kind: Option<String>,
    pub models: usize,
    /// The fleet has a newer version of this CLI than this host.
    pub behind: bool,
}

#[derive(Default)]
struct Digest {
    due: Option<Instant>,
    versions: BTreeSet<String>,
    /// Model id to its label and the hosts it appeared on.
    added: BTreeMap<String, (String, BTreeSet<String>)>,
    removed: BTreeMap<String, (String, BTreeSet<String>)>,
}

struct Announced {
    id: String,
    hosts_on: usize,
    new: Vec<String>,
    gone: Vec<String>,
}

pub struct Watch {
    pub debounce: Duration,
    /// By host, then provider.
    seen: HashMap<(String, String), Seen>,
    digests: BTreeMap<String, Digest>,
    /// By provider and version.
    announced: BTreeMap<(String, String), Announced>,
    /// Models already told as new, and as gone, by provider.
    told_new: HashMap<String, BTreeSet<String>>,
    told_gone: HashMap<String, BTreeSet<String>>,
    counter: u64,
}

impl Default for Watch {
    fn default() -> Self {
        Self::new(DEBOUNCE)
    }
}

/// `2.1.320` against `2.1.9`: by number, part by part.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> {
        v.split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    parts(a).cmp(&parts(b)).then_with(|| a.cmp(b))
}

/// What a provider is called to the user: Claude's id is `claude`, but it
/// is Claude Code that upgrades.
fn product(id: &str, label: &str) -> String {
    match id {
        "claude" => "Claude Code".into(),
        _ if label.is_empty() => id.into(),
        _ => label.into(),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl Watch {
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            seen: HashMap::new(),
            digests: BTreeMap::new(),
            announced: BTreeMap::new(),
            told_new: HashMap::new(),
            told_gone: HashMap::new(),
            counter: 0,
        }
    }

    /// Takes a host's providers snapshot. Providers not ready (loading, or
    /// unavailable) say nothing about models, and are left as they were.
    pub fn observe(&mut self, host: &str, providers: &[Provider], now: Instant) {
        for provider in providers.iter().filter(|p| p.ready()) {
            let current = Seen {
                label: provider.label.clone(),
                version: provider.version.clone(),
                install_kind: provider.install_kind.clone(),
                models: provider
                    .models
                    .iter()
                    .map(|m| (m.id.clone(), m.label.clone()))
                    .collect(),
            };
            let key = (host.to_string(), provider.id.clone());
            let Some(before) = self.seen.insert(key, current.clone()) else {
                continue;
            };
            let digest = self.digests.entry(provider.id.clone()).or_default();
            let mut changed = false;
            if let Some(version) = &current.version
                && before.version.as_ref() != Some(version)
            {
                digest.versions.insert(version.clone());
                changed = true;
            }
            for (id, label) in &current.models {
                if !before.models.contains_key(id) {
                    let entry = digest.added.entry(id.clone());
                    entry
                        .or_insert_with(|| (label.clone(), BTreeSet::new()))
                        .1
                        .insert(host.to_string());
                    changed = true;
                }
            }
            for (id, label) in &before.models {
                if !current.models.contains_key(id) {
                    let entry = digest.removed.entry(id.clone());
                    entry
                        .or_insert_with(|| (label.clone(), BTreeSet::new()))
                        .1
                        .insert(host.to_string());
                    changed = true;
                }
            }
            if changed && digest.due.is_none() {
                digest.due = Some(now + self.debounce);
            } else if !changed
                && digest.versions.is_empty()
                && digest.added.is_empty()
                && digest.removed.is_empty()
            {
                self.digests.remove(&provider.id);
            }
        }
    }

    /// Forgets a host that was removed.
    pub fn forget_host(&mut self, host: &str) {
        self.seen.retain(|(h, _), _| h != host);
    }

    /// The notices whose changes have gathered long enough.
    pub fn flush(&mut self, now: Instant) -> Vec<Notice> {
        let due: Vec<String> = (self.digests.iter())
            .filter(|(_, d)| d.due.is_some_and(|at| at <= now))
            .map(|(p, _)| p.clone())
            .collect();
        let mut out = Vec::new();
        for provider in due {
            let Some(digest) = self.digests.remove(&provider) else {
                continue;
            };
            out.extend(self.notice(&provider, digest));
        }
        out
    }

    /// When the next notice falls due, for a caller that sleeps until then.
    pub fn next_due(&self) -> Option<Instant> {
        self.digests.values().filter_map(|d| d.due).min()
    }

    fn hosts_with(&self, provider: &str) -> Vec<&Seen> {
        (self.seen.iter())
            .filter(|((_, p), _)| p == provider)
            .map(|(_, seen)| seen)
            .collect()
    }

    /// The newest version of a provider's CLI any host runs.
    pub fn newest(&self, provider: &str) -> Option<String> {
        self.hosts_with(provider)
            .iter()
            .filter_map(|s| s.version.clone())
            .max_by(|a, b| compare_versions(a, b))
    }

    /// A host's providers, for a hosts screen: each with whether the host
    /// is behind the newest version in the fleet.
    pub fn rows(&self, host: &str) -> Vec<Row> {
        let mut rows: Vec<Row> = (self.seen.iter())
            .filter(|((h, _), _)| h == host)
            .map(|((_, provider), seen)| {
                let newest = self.newest(provider);
                let behind = match (&seen.version, &newest) {
                    (Some(mine), Some(newest)) => {
                        compare_versions(mine, newest) == std::cmp::Ordering::Less
                    }
                    _ => false,
                };
                Row {
                    provider: provider.clone(),
                    label: product(provider, &seen.label),
                    version: seen.version.clone(),
                    install_kind: seen.install_kind.clone(),
                    models: seen.models.len(),
                    behind,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.provider.cmp(&b.provider));
        rows
    }

    fn notice(&mut self, provider: &str, digest: Digest) -> Vec<Notice> {
        let present: BTreeSet<&String> = (self.seen.iter())
            .filter(|((_, p), _)| p == provider)
            .flat_map(|(_, seen)| seen.models.keys())
            .collect();
        let told_new = self.told_new.entry(provider.to_string()).or_default();
        let told_gone = self.told_gone.entry(provider.to_string()).or_default();
        // A model back after it went is no longer gone.
        for id in digest.added.keys() {
            told_gone.remove(id);
        }
        let new: Vec<(String, BTreeSet<String>)> = (digest.added.iter())
            .filter(|(id, _)| present.contains(id) && !told_new.contains(*id))
            .map(|(id, (label, hosts))| {
                let _ = id;
                (label.clone(), hosts.clone())
            })
            .collect();
        let new_ids: Vec<String> = (digest.added.keys())
            .filter(|id| present.contains(id) && !told_new.contains(*id))
            .cloned()
            .collect();
        let gone: Vec<(String, BTreeSet<String>)> = (digest.removed.iter())
            .filter(|(id, _)| !present.contains(id) && !told_gone.contains(*id))
            .map(|(_, (label, hosts))| (label.clone(), hosts.clone()))
            .collect();
        let gone_ids: Vec<String> = (digest.removed.keys())
            .filter(|id| !present.contains(id) && !told_gone.contains(*id))
            .cloned()
            .collect();
        told_new.extend(new_ids);
        told_gone.extend(gone_ids);
        let new_labels: Vec<String> = new.iter().map(|(l, _)| l.clone()).collect();
        let gone_labels: Vec<String> = gone.iter().map(|(l, _)| l.clone()).collect();

        let hosts = self.hosts_with(provider);
        let total = hosts.len();
        let name = hosts
            .iter()
            .map(|s| product(provider, &s.label))
            .next()
            .unwrap_or_else(|| provider.to_string());
        let newest = self.newest(provider);
        let version = newest.filter(|n| digest.versions.contains(n));
        if let Some(version) = version {
            let on = hosts
                .iter()
                .filter(|s| s.version.as_deref() == Some(version.as_str()))
                .count();
            let key = (provider.to_string(), version.clone());
            let fresh = !self.announced.contains_key(&key);
            if fresh {
                self.counter += 1;
            }
            let id = format!("cli-{provider}-{version}");
            let entry = self.announced.entry(key).or_insert_with(|| Announced {
                id: id.clone(),
                hosts_on: 0,
                new: Vec::new(),
                gone: Vec::new(),
            });
            let changed =
                fresh || entry.hosts_on != on || !new_labels.is_empty() || !gone_labels.is_empty();
            entry.hosts_on = on;
            entry.new.extend(new_labels);
            entry.gone.extend(gone_labels);
            if !changed {
                return Vec::new();
            }
            let mut body = format!("on {on} of {}", plural(total, "host", "hosts"));
            if !entry.new.is_empty() {
                body += &format!(" \u{b7} new: {}", entry.new.join(", "));
            }
            if !entry.gone.is_empty() {
                body += &format!(" \u{b7} gone: {}", entry.gone.join(", "));
            }
            return vec![Notice {
                id: entry.id.clone(),
                kind: Kind::CliUpdate,
                provider: provider.to_string(),
                title: format!("{name} {version}"),
                body,
                version: Some(version),
                hosts_on: on,
                hosts_total: total,
                new_models: entry.new.clone(),
                gone_models: entry.gone.clone(),
            }];
        }
        if new.is_empty() && gone.is_empty() {
            return Vec::new();
        }
        let describe = |list: &[(String, BTreeSet<String>)]| -> String {
            list.iter()
                .map(|(label, hosts)| {
                    let names: Vec<&str> = hosts.iter().map(String::as_str).collect();
                    match names.len() {
                        0 => label.clone(),
                        1..=2 => format!("{label} on {}", names.join(", ")),
                        n => format!("{label} on {n} hosts"),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !new.is_empty() {
            parts.push(format!("new: {}", describe(&new)));
        }
        if !gone.is_empty() {
            parts.push(format!("gone: {}", describe(&gone)));
        }
        let title = match (new.len(), gone.len()) {
            (1, 0) => format!("New {name} model"),
            (_, 0) => format!("New {name} models"),
            (0, _) => format!("{name} models gone"),
            _ => format!("{name} models changed"),
        };
        self.counter += 1;
        vec![Notice {
            id: format!("models-{provider}-{}", self.counter),
            kind: Kind::Models,
            provider: provider.to_string(),
            title,
            body: parts.join(" \u{b7} "),
            version: None,
            hosts_on: 0,
            hosts_total: total,
            new_models: new_labels,
            gone_models: gone_labels,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Model;

    fn claude(version: &str, models: &[(&str, &str)]) -> Vec<Provider> {
        vec![Provider {
            id: "claude".into(),
            label: "Claude".into(),
            status: "ready".into(),
            error: None,
            enabled: true,
            default_mode: None,
            modes: Vec::new(),
            models: models
                .iter()
                .map(|(id, label)| Model {
                    id: (*id).into(),
                    label: (*label).into(),
                    is_default: false,
                    thinking: Vec::new(),
                    default_thinking: None,
                })
                .collect(),
            version: Some(version.into()),
            install_kind: Some("native".into()),
        }]
    }

    const OLD: &[(&str, &str)] = &[("opus-5", "Opus 5")];
    const NEW: &[(&str, &str)] = &[("opus-5", "Opus 5"), ("fable-5-2", "Fable 5.2")];

    fn fleet_of(n: usize, watch: &mut Watch, t: Instant) {
        for i in 0..n {
            watch.observe(&format!("box-{i}"), &claude("2.1.317", OLD), t);
        }
    }

    #[test]
    fn versions_compare_by_number() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("2.1.320", "2.1.9"), Greater);
        assert_eq!(compare_versions("2.1.9", "2.1.9"), Equal);
        assert_eq!(compare_versions("2.0.999", "2.1.0"), Less);
    }

    #[test]
    fn first_sight_of_a_host_is_a_baseline_not_news() {
        let t = Instant::now();
        let mut watch = Watch::new(Duration::from_secs(10));
        fleet_of(3, &mut watch, t);
        assert!(watch.flush(t + Duration::from_secs(60)).is_empty());
        assert_eq!(watch.next_due(), None);
    }

    #[test]
    fn a_fleet_upgrading_is_one_notice_that_fills_in() {
        let t = Instant::now();
        let mut watch = Watch::new(Duration::from_secs(10));
        fleet_of(10, &mut watch, t);
        for i in 0..7 {
            watch.observe(&format!("box-{i}"), &claude("2.1.320", NEW), t);
        }
        // Nothing yet: the changes are still gathering.
        assert!(watch.flush(t + Duration::from_secs(5)).is_empty());
        let notices = watch.flush(t + Duration::from_secs(11));
        assert_eq!(notices.len(), 1);
        let n = &notices[0];
        assert_eq!(n.kind, Kind::CliUpdate);
        assert_eq!(n.title, "Claude Code 2.1.320");
        assert_eq!(n.body, "on 7 of 10 hosts \u{b7} new: Fable 5.2");
        assert_eq!((n.hosts_on, n.hosts_total), (7, 10));
        let id = n.id.clone();

        // The rest catch up: the same notice, with new counts, and the model
        // is not told twice.
        let later = t + Duration::from_secs(600);
        for i in 7..10 {
            watch.observe(&format!("box-{i}"), &claude("2.1.320", NEW), later);
        }
        assert!(watch.flush(later).is_empty(), "debounced");
        let notices = watch.flush(later + Duration::from_secs(11));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].id, id);
        assert_eq!(notices[0].body, "on 10 of 10 hosts \u{b7} new: Fable 5.2");

        // Nothing more happens while nothing changes.
        assert!(watch.flush(later + Duration::from_secs(900)).is_empty());
    }

    #[test]
    fn a_host_behind_the_newest_is_marked() {
        let t = Instant::now();
        let mut watch = Watch::new(Duration::from_secs(1));
        fleet_of(2, &mut watch, t);
        watch.observe("box-0", &claude("2.1.320", NEW), t);
        let rows = watch.rows("box-0");
        assert_eq!(rows[0].version.as_deref(), Some("2.1.320"));
        assert!(!rows[0].behind);
        assert_eq!(rows[0].label, "Claude Code");
        assert!(watch.rows("box-1")[0].behind);
        assert!(watch.rows("nowhere").is_empty());
    }

    #[test]
    fn a_model_with_no_version_change_is_told_on_its_own() {
        let t = Instant::now();
        let mut watch = Watch::new(Duration::from_secs(1));
        fleet_of(4, &mut watch, t);
        watch.observe("box-0", &claude("2.1.317", NEW), t);
        watch.observe("box-1", &claude("2.1.317", NEW), t);
        watch.observe("box-2", &claude("2.1.317", NEW), t);
        let notices = watch.flush(t + Duration::from_secs(2));
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].kind, Kind::Models);
        assert_eq!(notices[0].title, "New Claude Code model");
        assert_eq!(notices[0].body, "new: Fable 5.2 on 3 hosts");
        // A fourth host getting it later is not news again.
        watch.observe(
            "box-3",
            &claude("2.1.317", NEW),
            t + Duration::from_secs(60),
        );
        assert!(watch.flush(t + Duration::from_secs(120)).is_empty());
    }

    #[test]
    fn a_model_gone_from_every_host_is_told() {
        let t = Instant::now();
        let mut watch = Watch::new(Duration::from_secs(1));
        fleet_of(2, &mut watch, t);
        let none: &[(&str, &str)] = &[];
        watch.observe("box-0", &claude("2.1.317", none), t);
        // Still on box-1: not gone.
        assert!(watch.flush(t + Duration::from_secs(2)).is_empty());
        watch.observe(
            "box-1",
            &claude("2.1.317", none),
            t + Duration::from_secs(3),
        );
        let notices = watch.flush(t + Duration::from_secs(5));
        assert_eq!(notices[0].title, "Claude Code models gone");
        assert_eq!(notices[0].gone_models, ["Opus 5"]);
    }

    #[test]
    fn providers_not_ready_and_hosts_gone_are_left_alone() {
        let t = Instant::now();
        let mut watch = Watch::new(Duration::from_secs(1));
        fleet_of(1, &mut watch, t);
        let mut loading = claude("2.1.400", &[]);
        loading[0].status = "loading".into();
        watch.observe("box-0", &loading, t);
        assert!(watch.flush(t + Duration::from_secs(5)).is_empty());
        watch.forget_host("box-0");
        assert!(watch.rows("box-0").is_empty());
    }
}
