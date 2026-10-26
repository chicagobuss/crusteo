//! Reef mode's labels: one fleet catalog, kept by Reef, and labels attached
//! to agents rather than workspaces.
//!
//! - A label is a [`LabelId`] Reef issued, with a name, colour and order in
//!   the [`Catalog`]. Renaming or recolouring changes only the catalog;
//!   deleting leaves a tombstone, so a stale client cannot bring it back.
//! - An agent's labels live in its Paseo `labels` map under one key,
//!   [`KEY`], as a versioned list (`v1:<id>,<id>`), written whole each time:
//!   Paseo merges that map and cannot delete a key.
//! - An agent without the key inherits its workspace's Paseo labels, matched
//!   to the catalog by name through [`Catalog::aliases`]. `v1:` with no ids
//!   is explicitly unlabelled.
//!
//! In Paseo-compatible mode there is no catalog and none of this applies:
//! labels stay on workspaces, as Paseo has them.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::directory::{LabelDef, label_key};
use crate::{Agent, Workspace};

/// The key in an agent's Paseo `labels` map that holds its labels.
pub const KEY: &str = "crusteo.labels";

/// The assignment format [`encode`] writes and [`assignment`] reads.
const VERSION: &str = "v1:";

/// A label, as Reef issued it. Never reused, never renamed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LabelId(String);

impl LabelId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// A fresh id, `lbl_` and 16 hex digits.
    pub fn generate() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        hasher.write_u128(nanos);
        Self(format!("lbl_{:016x}", hasher.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LabelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One label in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub id: LabelId,
    pub name: String,
    /// A colour name as Paseo has them: `red`, `violet`, …
    pub color: String,
    /// Where it sorts, smallest first.
    pub order: u32,
}

/// The fleet's labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Catalog {
    /// The live labels, in order.
    pub labels: Vec<Label>,
    /// Deleted labels: an assignment naming one shows nothing for it.
    pub tombstones: BTreeSet<LabelId>,
    /// A Paseo workspace label, by [`label_key`], to the label it became.
    /// Kept when a label is renamed, so agents that inherit by name keep
    /// it; kept pointing at a tombstone when one is deleted, so making a
    /// label of the same name later does not quietly take them back.
    pub aliases: BTreeMap<String, LabelId>,
    /// Whether the first import from the hosts' labels has run.
    pub imported: bool,
    /// What the import decided, a line per colour conflict.
    pub import_summary: Vec<String>,
    /// Bumped on every change, so readers can tell theirs is stale.
    pub revision: u64,
}

/// What an agent's [`KEY`] says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assignment {
    /// No key: the agent inherits its workspace's labels.
    Unset,
    /// These labels, in this order; none is explicitly unlabelled.
    Labels(Vec<LabelId>),
    /// A format this build does not read; shown as unlabelled, never
    /// overwritten without the user choosing labels.
    Unsupported(String),
}

/// What an agent's `labels` map assigns.
pub fn assignment(labels: &Map<String, Value>) -> Assignment {
    let Some(value) = labels.get(KEY) else {
        return Assignment::Unset;
    };
    let Some(text) = value.as_str() else {
        return Assignment::Unsupported(value.to_string());
    };
    let Some(list) = text.strip_prefix(VERSION) else {
        return Assignment::Unsupported(text.to_string());
    };
    let mut ids: Vec<LabelId> = Vec::new();
    for id in list.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        let id = LabelId::new(id);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Assignment::Labels(ids)
}

/// The [`KEY`] value for `ids`.
pub fn encode(ids: &[LabelId]) -> String {
    let list: Vec<&str> = ids.iter().map(LabelId::as_str).collect();
    format!("{VERSION}{}", list.join(","))
}

/// The `update_agent_request` that sets an agent's labels: only `labels`,
/// and in it only [`KEY`], so every other key (such as
/// `paseo.parent-agent-id`) stays as it is.
pub fn set_request(agent_id: &str, ids: &[LabelId]) -> Value {
    json!({
        "type": "update_agent_request",
        "agentId": agent_id,
        "labels": { KEY: encode(ids) },
    })
}

impl Catalog {
    pub fn get(&self, id: &LabelId) -> Option<&Label> {
        self.labels.iter().find(|label| &label.id == id)
    }

    /// The live label a Paseo workspace label stands for, if any.
    pub fn by_paseo_name(&self, name: &str) -> Option<&Label> {
        self.get(self.aliases.get(&label_key(name))?)
    }

    /// The live label with this name, by [`label_key`].
    pub fn by_name(&self, name: &str) -> Option<&Label> {
        let key = label_key(name);
        self.labels
            .iter()
            .find(|label| label_key(&label.name) == key)
    }

    /// Only the live ones of `ids`, in their order.
    pub fn live(&self, ids: &[LabelId]) -> Vec<LabelId> {
        ids.iter()
            .filter(|id| self.get(id).is_some())
            .cloned()
            .collect()
    }

    /// A workspace's Paseo labels as catalog labels: those with a live alias.
    pub fn from_paseo(&self, names: &[String]) -> Vec<LabelId> {
        let mut ids: Vec<LabelId> = Vec::new();
        for name in names {
            if let Some(label) = self.by_paseo_name(name)
                && !ids.contains(&label.id)
            {
                ids.push(label.id.clone());
            }
        }
        ids
    }

    /// Makes a label. A name is used once among live labels.
    pub fn create(&mut self, name: &str, color: &str) -> Result<LabelId, String> {
        let name = clean_name(name)?;
        if self.by_name(&name).is_some() {
            return Err(format!("There is already a label {name}"));
        }
        let id = std::iter::repeat_with(LabelId::generate)
            .find(|id| self.get(id).is_none() && !self.tombstones.contains(id))
            .expect("a fresh id");
        let order = self
            .labels
            .iter()
            .map(|label| label.order + 1)
            .max()
            .unwrap_or(0);
        self.labels.push(Label {
            id: id.clone(),
            name,
            color: color.to_string(),
            order,
        });
        self.revision += 1;
        Ok(id)
    }

    pub fn rename(&mut self, id: &LabelId, name: &str) -> Result<(), String> {
        let name = clean_name(name)?;
        if self.by_name(&name).is_some_and(|other| &other.id != id) {
            return Err(format!("There is already a label {name}"));
        }
        let label =
            (self.labels.iter_mut().find(|label| &label.id == id)).ok_or("That label is gone")?;
        label.name = name;
        self.revision += 1;
        Ok(())
    }

    pub fn recolor(&mut self, id: &LabelId, color: &str) -> Result<(), String> {
        let label =
            (self.labels.iter_mut().find(|label| &label.id == id)).ok_or("That label is gone")?;
        label.color = color.to_string();
        self.revision += 1;
        Ok(())
    }

    /// Deletes a label, leaving a tombstone. Aliases to it stay, pointing
    /// at the tombstone.
    pub fn delete(&mut self, id: &LabelId) -> Result<(), String> {
        let before = self.labels.len();
        self.labels.retain(|label| &label.id != id);
        if self.labels.len() == before {
            return Err("That label is gone".into());
        }
        self.tombstones.insert(id.clone());
        self.revision += 1;
        Ok(())
    }

    /// Makes `name`, a Paseo workspace label seen after the import, a
    /// catalog label: the live one of that name if there is one, else a new
    /// one in `color`. Does nothing for a name already aliased.
    pub fn adopt(&mut self, name: &str, color: &str) {
        let key = label_key(name);
        if key.is_empty() || self.aliases.contains_key(&key) {
            return;
        }
        let id = match self.by_name(name) {
            Some(label) => label.id.clone(),
            None => match self.create(name, color) {
                Ok(id) => id,
                Err(_) => return,
            },
        };
        self.aliases.insert(key, id);
        self.revision += 1;
    }

    /// The one-time import: a label for each distinct Paseo workspace label
    /// name across `hosts` (each host's definitions, then any name its
    /// workspaces use without one), in the hosts' order. Where hosts colour
    /// a name differently, the colour most hosts use wins, each host counted
    /// once; ties go to the host first in order. A name no host defines
    /// takes the colour `undefined` picks for it, the one Paseo shows it in.
    /// Each conflict leaves a line in [`Self::import_summary`]. Does nothing
    /// once [`Self::imported`].
    pub fn import(&mut self, hosts: &[ImportHost<'_>], undefined: impl Fn(&str) -> String) {
        if self.imported {
            return;
        }
        // Each name's spelling (the first seen) and colour per host.
        let mut names: Vec<(String, String)> = Vec::new();
        let mut votes: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
        for (index, host) in hosts.iter().enumerate() {
            let mut seen = BTreeSet::new();
            let defined = host
                .labels
                .iter()
                .map(|def| (def.name.as_str(), Some(def.color.as_str())));
            let used =
                (host.workspaces.iter()).flat_map(|w| w.labels.iter().map(|n| (n.as_str(), None)));
            for (name, color) in defined.chain(used) {
                let key = label_key(name);
                if key.is_empty() || !seen.insert(key.clone()) {
                    continue;
                }
                if !names.iter().any(|(k, _)| *k == key) {
                    names.push((key.clone(), name.trim().to_string()));
                }
                if let Some(color) = color {
                    votes
                        .entry(key)
                        .or_default()
                        .push((index, color.to_string()));
                }
            }
        }
        for (key, name) in names {
            let ballots = votes.get(&key).cloned().unwrap_or_default();
            let color = winner(&ballots).unwrap_or_else(|| undefined(&name));
            let distinct: BTreeSet<&str> = ballots.iter().map(|(_, c)| c.as_str()).collect();
            if distinct.len() > 1 {
                let mut counts: Vec<String> = distinct
                    .iter()
                    .map(|c| {
                        let n = ballots.iter().filter(|(_, b)| b == c).count();
                        format!("{n} {}", if n == 1 { "host" } else { "hosts" }) + " used " + c
                    })
                    .collect();
                counts.sort();
                self.import_summary
                    .push(format!("{name}: {}; using {color}", counts.join(", ")));
            }
            let id = match self.by_name(&name) {
                Some(label) => label.id.clone(),
                None => match self.create(&name, &color) {
                    Ok(id) => id,
                    Err(_) => continue,
                },
            };
            self.aliases.entry(key).or_insert(id);
        }
        self.imported = true;
        self.revision += 1;
    }
}

/// One host, as the import reads it.
pub struct ImportHost<'a> {
    pub labels: &'a [LabelDef],
    pub workspaces: Vec<&'a Workspace>,
}

/// The colour most ballots name; a tie goes to the earliest host's.
fn winner(ballots: &[(usize, String)]) -> Option<String> {
    let mut best: Option<(usize, usize, &str)> = None;
    for (index, color) in ballots {
        let count = ballots.iter().filter(|(_, c)| c == color).count();
        let first = ballots
            .iter()
            .filter(|(_, c)| c == color)
            .map(|(i, _)| *i)
            .min()
            .unwrap_or(*index);
        let better = match best {
            None => true,
            Some((n, at, _)) => count > n || (count == n && first < at),
        };
        if better {
            best = Some((count, first, color));
        }
    }
    best.map(|(_, _, color)| color.to_string())
}

fn clean_name(name: &str) -> Result<String, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.chars().count() > 64 {
        return Err("A label needs a name, up to 64 characters".into());
    }
    Ok(name)
}

/// An agent's labels as the views place it: its own, or, with none set,
/// its workspace's Paseo labels as catalog labels. Only live labels; an
/// assignment this build cannot read shows as none.
pub fn effective(agent: &Agent, workspace: Option<&Workspace>, catalog: &Catalog) -> Vec<LabelId> {
    match assignment(&agent.labels) {
        Assignment::Labels(ids) => catalog.live(&ids),
        Assignment::Unset => workspace.map_or_else(Vec::new, |w| catalog.from_paseo(&w.labels)),
        Assignment::Unsupported(_) => Vec::new(),
    }
}

/// The labels a new agent in `workspace` starts with when it was made
/// outside a lane and has no parent: those every current (not archived)
/// agent there has, else, with no such agent, the workspace's Paseo labels.
/// The creation form shows them, and the user may change them.
pub fn workspace_default<'a>(
    agents: impl IntoIterator<Item = &'a Agent>,
    workspace: &Workspace,
    catalog: &Catalog,
) -> Vec<LabelId> {
    let mut shared: Option<Vec<LabelId>> = None;
    for agent in agents {
        if agent.archived_at.is_some() {
            continue;
        }
        let mine = effective(agent, Some(workspace), catalog);
        shared = Some(match shared {
            None => mine,
            Some(shared) => shared.into_iter().filter(|id| mine.contains(id)).collect(),
        });
    }
    shared.unwrap_or_else(|| catalog.from_paseo(&workspace.labels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(labels: Value) -> Agent {
        Agent::from_value(json!({"id": "a", "status": "idle", "labels": labels})).unwrap()
    }

    fn workspace(labels: &[&str]) -> Workspace {
        serde_json::from_value(json!({"id": "w", "labels": labels})).unwrap()
    }

    fn def(name: &str, color: &str) -> LabelDef {
        LabelDef {
            name: name.into(),
            color: color.into(),
        }
    }

    #[test]
    fn assignments_read_their_version_and_write_only_their_key() {
        let a = LabelId::new("lbl_a");
        let b = LabelId::new("lbl_b");
        let labels = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(assignment(&labels(json!({}))), Assignment::Unset);
        assert_eq!(
            assignment(&labels(json!({KEY: "v1:lbl_a, lbl_b,lbl_a"}))),
            Assignment::Labels(vec![a.clone(), b.clone()])
        );
        assert_eq!(
            assignment(&labels(json!({KEY: "v1:"}))),
            Assignment::Labels(vec![])
        );
        assert!(matches!(
            assignment(&labels(json!({KEY: "v2:x"}))),
            Assignment::Unsupported(_)
        ));
        assert_eq!(encode(&[a.clone(), b.clone()]), "v1:lbl_a,lbl_b");
        assert_eq!(encode(&[]), "v1:");
        assert_eq!(
            set_request("agt", &[a]),
            json!({"type": "update_agent_request", "agentId": "agt",
                   "labels": {KEY: "v1:lbl_a"}})
        );
    }

    #[test]
    fn the_import_votes_colours_per_host_and_says_so() {
        let mut catalog = Catalog::default();
        let one = [def("UX", "blue"), def("infra", "red")];
        let two = [def("ux", "blue")];
        let three = [def("ux", "red"), def("Ops", "green"), def("infra", "teal")];
        let used = workspace(&["adhoc", " "]);
        catalog.import(
            &[
                ImportHost {
                    labels: &one,
                    workspaces: vec![&used],
                },
                ImportHost {
                    labels: &two,
                    workspaces: vec![],
                },
                ImportHost {
                    labels: &three,
                    workspaces: vec![],
                },
            ],
            |_| "violet".into(),
        );
        let names: Vec<(&str, &str)> = catalog
            .labels
            .iter()
            .map(|l| (l.name.as_str(), l.color.as_str()))
            .collect();
        // First spelling seen; most hosts' colour; a 1-1 tie goes to the
        // first host; a name only used gets the one picked for it.
        assert_eq!(
            names,
            [
                ("UX", "blue"),
                ("infra", "red"),
                ("adhoc", "violet"),
                ("Ops", "green")
            ]
        );
        assert_eq!(
            catalog.import_summary,
            [
                "UX: 1 host used red, 2 hosts used blue; using blue",
                "infra: 1 host used red, 1 host used teal; using red"
            ]
        );
        assert!(catalog.imported);
        // It runs once.
        let before = catalog.clone();
        catalog.import(
            &[ImportHost {
                labels: &[def("new", "red")],
                workspaces: vec![],
            }],
            |_| "violet".into(),
        );
        assert_eq!(catalog, before);
        assert_eq!(catalog.by_paseo_name("ux").unwrap().name, "UX");
    }

    #[test]
    fn agents_inherit_by_name_until_they_choose_and_renames_keep_them() {
        let mut catalog = Catalog::default();
        catalog.import(
            &[ImportHost {
                labels: &[def("ux", "blue"), def("ops", "red")],
                workspaces: vec![],
            }],
            |_| "violet".into(),
        );
        let ux = catalog.by_name("ux").unwrap().id.clone();
        let ops = catalog.by_name("ops").unwrap().id.clone();
        let ws = workspace(&["UX", "unknown"]);

        let inherits = agent(json!({}));
        assert_eq!(effective(&inherits, Some(&ws), &catalog), vec![ux.clone()]);
        let chose = agent(json!({KEY: encode(std::slice::from_ref(&ops))}));
        assert_eq!(effective(&chose, Some(&ws), &catalog), vec![ops.clone()]);
        let none = agent(json!({KEY: "v1:"}));
        assert!(effective(&none, Some(&ws), &catalog).is_empty());
        let newer = agent(json!({KEY: "v9:whatever"}));
        assert!(effective(&newer, Some(&ws), &catalog).is_empty());

        // Renamed, the inheriting agent keeps it.
        catalog.rename(&ux, "Design").unwrap();
        assert_eq!(effective(&inherits, Some(&ws), &catalog), vec![ux.clone()]);
        // Deleted, nobody shows it, and a new "ux" does not take them back.
        catalog.delete(&ux).unwrap();
        assert!(effective(&inherits, Some(&ws), &catalog).is_empty());
        catalog.create("ux", "green").unwrap();
        assert!(effective(&inherits, Some(&ws), &catalog).is_empty());
        let stale = agent(json!({KEY: encode(&[ux.clone(), ops.clone()])}));
        assert_eq!(effective(&stale, Some(&ws), &catalog), [ops]);
    }

    #[test]
    fn names_are_unique_and_later_paseo_labels_are_adopted() {
        let mut catalog = Catalog::default();
        let ux = catalog.create(" ux ", "blue").unwrap();
        assert_eq!(catalog.get(&ux).unwrap().name, "ux");
        assert!(catalog.create("UX", "red").is_err());
        assert!(catalog.create("  ", "red").is_err());
        let ops = catalog.create("ops", "red").unwrap();
        assert!(catalog.rename(&ops, "Ux").is_err());
        assert_eq!(catalog.get(&ops).unwrap().order, 1);
        // A Paseo name seen later: the label of that name, or a new one.
        catalog.adopt("UX", "green");
        assert_eq!(catalog.by_paseo_name("ux").unwrap().id, ux);
        catalog.adopt("brand-new", "teal");
        assert_eq!(catalog.by_paseo_name("Brand-New").unwrap().color, "teal");
    }

    #[test]
    fn a_new_agent_in_a_workspace_starts_with_what_its_agents_share() {
        let mut catalog = Catalog::default();
        let a = catalog.create("a", "red").unwrap();
        let b = catalog.create("b", "blue").unwrap();
        catalog.adopt("old", "teal");
        let old = catalog.by_name("old").unwrap().id.clone();
        let ws = workspace(&["old"]);
        let both = agent(json!({KEY: encode(&[a.clone(), b.clone()])}));
        let just_a = agent(json!({KEY: encode(std::slice::from_ref(&a))}));
        assert_eq!(
            workspace_default([&both, &just_a], &ws, &catalog),
            vec![a.clone()]
        );
        let none = agent(json!({KEY: "v1:"}));
        assert!(workspace_default([&both, &none], &ws, &catalog).is_empty());
        // Archived agents do not count; with no agents, the Paseo labels.
        let mut gone = agent(json!({KEY: encode(&[b])}));
        gone.archived_at = Some("2026-10-22T00:00:00Z".into());
        assert_eq!(workspace_default([&gone], &ws, &catalog), [old]);
    }
}
