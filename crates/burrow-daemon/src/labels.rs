//! Workspace labels: the catalog of names and colours, and their
//! assignments to workspaces.
//!
//! Behaviour follows Paseo 0.12's `workspace-labels/` service:
//!
//! - **No ids.** A label is known by [`labels::key`] of its name. The catalog
//!   keeps the first spelling, in the order labels were made.
//! - **No create request.** Assigning a label the catalog lacks makes it, with
//!   the colour the request carries; a label the catalog has keeps its own
//!   name and colour whatever the request says. Unassigning makes nothing.
//! - **Renames follow through.** A rename rewrites every workspace's
//!   assignment; a recolour touches only the catalog; a delete takes the label
//!   off every workspace.
//! - **Changes are sequenced** in a journal of the last 256, so a client with
//!   a cursor catches up with only what it missed (`synchronize`, Paseo's
//!   `WorkspaceLabelSequence`). Removals are keyed by name.
//!
//! The catalog is saved as `workspace-labels.json`, a bare array as in
//! Paseo; assignments live on the workspace records.

use std::collections::VecDeque;

use burrow_protocol::directory::{SyncCursor, SyncMode};
use burrow_protocol::labels::{self, LabelColor, LabelDefinition};

use crate::store::{Store, StoreEvent};

/// Label changes remembered for catching up (Paseo's `journalLimit`).
const JOURNAL: usize = 256;

/// A change to the catalog, as `workspace.label.update` carries it.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelChange {
    /// A new or edited label; `previous_name` only on a rename.
    Upsert {
        label: LabelDefinition,
        previous_name: Option<String>,
    },
    Remove {
        name: String,
    },
}

/// The catalog and its change journal.
#[derive(Debug, Clone, Default)]
pub struct Labels {
    catalog: Vec<LabelDefinition>,
    head_seq: u64,
    journal: VecDeque<(u64, LabelChange)>,
}

/// What a `workspace.label.list.request` answers.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelSync {
    pub labels: Vec<LabelDefinition>,
    pub mode: SyncMode,
    pub head_seq: u64,
    /// `(name, seq)` of labels gone since the cursor.
    pub removals: Vec<(String, u64)>,
}

/// A refused label request: `rpc_error` with Paseo's code and message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelError {
    pub code: &'static str,
    pub message: &'static str,
}

const NAME_EMPTY: LabelError = LabelError {
    code: "label_name_empty",
    message: "Label name cannot be empty",
};
const NOT_FOUND: LabelError = LabelError {
    code: "label_not_found",
    message: "Label not found",
};
const NAME_TAKEN: LabelError = LabelError {
    code: "label_name_taken",
    message: "A label with that name already exists",
};
const WORKSPACE_NOT_FOUND: LabelError = LabelError {
    code: "workspace_not_found",
    message: "Workspace not found",
};

impl Labels {
    pub fn restore(catalog: Vec<LabelDefinition>) -> Self {
        Self {
            catalog,
            ..Self::default()
        }
    }

    pub fn list(&self) -> &[LabelDefinition] {
        &self.catalog
    }

    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.catalog
            .iter()
            .position(|l| labels::key(&l.name) == key)
    }

    pub fn find(&self, name: &str) -> Option<&LabelDefinition> {
        self.position(&labels::key(name))
            .map(|at| &self.catalog[at])
    }

    fn publish(&mut self, change: LabelChange) -> StoreEvent {
        self.head_seq += 1;
        self.journal.push_back((self.head_seq, change.clone()));
        if self.journal.len() > JOURNAL {
            self.journal.pop_front();
        }
        StoreEvent::Label {
            seq: self.head_seq,
            change,
        }
    }

    /// The catalog, or only what changed after the cursor when the journal
    /// still reaches back that far.
    pub fn synchronize(&self, generation: &str, cursor: &SyncCursor) -> LabelSync {
        let oldest = self
            .journal
            .front()
            .map_or(self.head_seq + 1, |(seq, _)| *seq);
        let after = match (&cursor.generation, cursor.after_seq) {
            (Some(theirs), Some(after))
                if theirs == generation && after <= self.head_seq && after + 1 >= oldest =>
            {
                after
            }
            _ => {
                return LabelSync {
                    labels: self.catalog.clone(),
                    mode: SyncMode::Snapshot,
                    head_seq: self.head_seq,
                    removals: Vec::new(),
                };
            }
        };
        // Folded as Paseo folds them: the last upsert per name; a label
        // upserted and then removed within the window leaves nothing.
        let mut upserts: Vec<(String, LabelDefinition)> = Vec::new();
        let mut removals: Vec<(String, (String, u64))> = Vec::new();
        for (seq, change) in self.journal.iter().filter(|(seq, _)| *seq > after) {
            let gone = match change {
                LabelChange::Remove { name } => Some(name),
                LabelChange::Upsert { previous_name, .. } => previous_name.as_ref(),
            };
            if let Some(name) = gone {
                let key = name.to_lowercase();
                let before = upserts.len();
                upserts.retain(|(k, _)| *k != key);
                if upserts.len() == before {
                    set(&mut removals, key, (name.clone(), *seq));
                }
            }
            if let LabelChange::Upsert { label, .. } = change {
                let key = label.name.to_lowercase();
                removals.retain(|(k, _)| *k != key);
                set(&mut upserts, key, label.clone());
            }
        }
        LabelSync {
            labels: upserts.into_iter().map(|(_, label)| label).collect(),
            mode: SyncMode::Changes,
            head_seq: self.head_seq,
            removals: removals.into_iter().map(|(_, removal)| removal).collect(),
        }
    }
}

/// `Map.set`: replaces the value in place, or appends.
fn set<T>(entries: &mut Vec<(String, T)>, key: String, value: T) {
    match entries.iter_mut().find(|(k, _)| *k == key) {
        Some((_, existing)) => *existing = value,
        None => entries.push((key, value)),
    }
}

/// `requireName`: the normalised name, or `label_name_empty`.
fn require_name(raw: &str) -> Result<String, LabelError> {
    let name = labels::normalize_name(raw);
    if name.is_empty() {
        Err(NAME_EMPTY)
    } else {
        Ok(name)
    }
}

type Outcome<T> = (Result<T, LabelError>, Vec<StoreEvent>);

impl Store {
    fn save_labels(&self) {
        if let Some(persist) = self.persist() {
            persist.labels(self.labels.list().to_vec());
        }
    }

    /// Rewrites every workspace that has the label `from`: to `to`, or off.
    /// Returns the workspaces changed.
    fn rewrite_assignments(&mut self, from: &str, to: Option<&str>) -> Vec<String> {
        let changed: Vec<String> = self
            .workspaces
            .iter()
            .filter(|(w, _)| w.labels.iter().any(|l| labels::key(l) == from))
            .map(|(w, _)| w.workspace_id.clone())
            .collect();
        for id in &changed {
            if let Some(workspace) = self.workspaces.get_mut(id) {
                workspace.labels = std::mem::take(&mut workspace.labels)
                    .into_iter()
                    .filter_map(|l| match labels::key(&l) == from {
                        true => to.map(str::to_string),
                        false => Some(l),
                    })
                    .collect();
            }
        }
        changed
    }

    fn workspaces_changed(&mut self, ids: &[String]) -> Vec<StoreEvent> {
        if ids.is_empty() {
            return Vec::new();
        }
        self.save_workspaces();
        ids.iter()
            .filter_map(|id| self.workspace_changed(id))
            .collect()
    }

    /// `workspace.label.assignment.set.request`. Returns the label as the
    /// catalog has it and the workspace's labels after the change.
    pub fn set_label_assignment(
        &mut self,
        workspace_id: &str,
        label: LabelDefinition,
        assigned: bool,
    ) -> Outcome<(LabelDefinition, Vec<String>)> {
        let name = match require_name(&label.name) {
            Ok(name) => name,
            Err(error) => return (Err(error), Vec::new()),
        };
        let Some(current) = self.workspaces.get(workspace_id).map(|w| w.labels.clone()) else {
            return (Err(WORKSPACE_NOT_FOUND), Vec::new());
        };
        let key = labels::key(&name);
        let existing = self.labels.find(&name).cloned();
        let created = existing.is_none() && assigned;
        let definition = existing.unwrap_or(LabelDefinition {
            name,
            color: label.color,
        });
        let at = current.iter().position(|l| labels::key(l) == key);
        let assignment = match (assigned, at) {
            (true, None) => Some([current.clone(), vec![definition.name.clone()]].concat()),
            (false, Some(at)) => {
                let mut labels = current.clone();
                labels.remove(at);
                Some(labels)
            }
            _ => None,
        };
        let mut events = Vec::new();
        // The catalog first, so a workspace never names a label it lacks.
        if created {
            self.labels.catalog.push(definition.clone());
            self.save_labels();
            events.push(self.labels.publish(LabelChange::Upsert {
                label: definition.clone(),
                previous_name: None,
            }));
        }
        let labels = assignment.clone().unwrap_or(current);
        if let Some(labels) = assignment {
            if let Some(workspace) = self.workspaces.get_mut(workspace_id) {
                workspace.labels = labels;
            }
            events.extend(self.workspaces_changed(&[workspace_id.to_string()]));
        }
        (Ok((definition, labels)), events)
    }

    /// `workspace.label.update.request`: a new name, a new colour, or both.
    /// Returns the label and how many workspaces a rename rewrote.
    pub fn update_label(
        &mut self,
        name: &str,
        new_name: Option<&str>,
        color: Option<LabelColor>,
    ) -> Outcome<(LabelDefinition, usize)> {
        let checked = require_name(name).and_then(|name| {
            let new_name = new_name.map(require_name).transpose()?;
            let from = labels::key(&name);
            let at = self.labels.position(&from).ok_or(NOT_FOUND)?;
            // Only a case change keeps the key; any other must be free.
            if let Some(new_name) = &new_name {
                let to = labels::key(new_name);
                if to != from && self.labels.position(&to).is_some() {
                    return Err(NAME_TAKEN);
                }
            }
            Ok((from, at, new_name))
        });
        let (from, at, new_name) = match checked {
            Ok(checked) => checked,
            Err(error) => return (Err(error), Vec::new()),
        };
        let existing = self.labels.catalog[at].clone();
        let renamed = new_name.filter(|n| *n != existing.name);
        let recolored = color.filter(|c| *c != existing.color);
        if renamed.is_none() && recolored.is_none() {
            return (Ok((existing, 0)), Vec::new());
        }
        let label = LabelDefinition {
            name: renamed.clone().unwrap_or_else(|| existing.name.clone()),
            color: recolored.unwrap_or_else(|| existing.color.clone()),
        };
        self.labels.catalog[at] = label.clone();
        self.save_labels();
        let mut events = vec![self.labels.publish(LabelChange::Upsert {
            label: label.clone(),
            previous_name: renamed.as_ref().map(|_| existing.name.clone()),
        })];
        // Workspaces hold names, so only a rename reaches them.
        let rewritten = match &renamed {
            Some(to) => self.rewrite_assignments(&from, Some(to)),
            None => Vec::new(),
        };
        events.extend(self.workspaces_changed(&rewritten));
        (Ok((label, rewritten.len())), events)
    }

    /// `workspace.label.delete.request`. A label the catalog lacks is not an
    /// error. Returns how many workspaces it was taken off.
    pub fn delete_label(&mut self, name: &str) -> Outcome<usize> {
        let key = match require_name(name) {
            Ok(name) => labels::key(&name),
            Err(error) => return (Err(error), Vec::new()),
        };
        let Some(at) = self.labels.position(&key) else {
            return (Ok(0), Vec::new());
        };
        let removed = self.labels.catalog.remove(at);
        let rewritten = self.rewrite_assignments(&key, None);
        let mut events = self.workspaces_changed(&rewritten);
        self.save_labels();
        events.push(
            self.labels
                .publish(LabelChange::Remove { name: removed.name }),
        );
        (Ok(rewritten.len()), events)
    }

    /// `workspace.label.delete.inspect.request`: the workspaces a delete
    /// would change.
    pub fn count_label(&self, name: &str) -> Result<usize, LabelError> {
        let key = labels::key(&require_name(name)?);
        Ok(self
            .workspaces
            .iter()
            .filter(|(w, _)| w.labels.iter().any(|l| labels::key(l) == key))
            .count())
    }

    /// Adds an imported label the catalog lacks. Nothing happens when the
    /// catalog has one by that name already, which then wins.
    pub fn import_label(&mut self, label: LabelDefinition) -> Option<StoreEvent> {
        let name = require_name(&label.name).ok()?;
        if self.labels.find(&name).is_some() {
            return None;
        }
        let label = LabelDefinition {
            name,
            color: label.color,
        };
        self.labels.catalog.push(label.clone());
        self.save_labels();
        Some(self.labels.publish(LabelChange::Upsert {
            label,
            previous_name: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, color: &str) -> LabelDefinition {
        LabelDefinition {
            name: name.into(),
            color: LabelColor::try_from(color.to_string()).unwrap(),
        }
    }

    fn cursor(generation: &str, after: u64) -> SyncCursor {
        SyncCursor {
            generation: Some(generation.into()),
            after_seq: Some(after),
        }
    }

    fn upsert(labels: &mut Labels, label: LabelDefinition, previous: Option<&str>) {
        labels.publish(LabelChange::Upsert {
            label,
            previous_name: previous.map(str::to_string),
        });
    }

    #[test]
    fn catching_up_folds_the_window_as_paseo_does() {
        let mut labels = Labels::default();
        upsert(&mut labels, def("infra", "teal"), None);
        upsert(&mut labels, def("blocked", "red"), None);
        let seen = labels.head_seq();
        // A rename of a label the client knows: the old name is removed.
        upsert(&mut labels, def("Stuck", "red"), Some("blocked"));
        // Made and deleted inside the window: nothing.
        upsert(&mut labels, def("temp", "sky"), None);
        labels.publish(LabelChange::Remove {
            name: "temp".into(),
        });
        // Recoloured twice: the last one.
        upsert(&mut labels, def("infra", "blue"), None);
        upsert(&mut labels, def("infra", "amber"), None);

        let sync = labels.synchronize("g", &cursor("g", seen));
        assert_eq!(sync.mode, SyncMode::Changes);
        assert_eq!(sync.labels, [def("Stuck", "red"), def("infra", "amber")]);
        assert_eq!(sync.removals, [("blocked".to_string(), seen + 1)]);

        let fresh = labels.synchronize("g", &SyncCursor::default());
        assert_eq!(fresh.mode, SyncMode::Snapshot);
        let other = labels.synchronize("g", &cursor("other", seen));
        assert_eq!(other.mode, SyncMode::Snapshot);
        let ahead = labels.synchronize("g", &cursor("g", labels.head_seq() + 1));
        assert_eq!(ahead.mode, SyncMode::Snapshot);
        let current = labels.synchronize("g", &cursor("g", labels.head_seq()));
        assert_eq!(current.mode, SyncMode::Changes);
        assert!(current.labels.is_empty() && current.removals.is_empty());
    }

    #[test]
    fn a_cursor_older_than_the_journal_gets_a_snapshot() {
        let mut labels = Labels::default();
        for n in 0..(JOURNAL as u64 + 10) {
            upsert(&mut labels, def(&format!("l{n}"), "violet"), None);
        }
        assert_eq!(
            labels.synchronize("g", &cursor("g", 5)).mode,
            SyncMode::Snapshot
        );
        let oldest_kept = labels.head_seq() - JOURNAL as u64;
        assert_eq!(
            labels.synchronize("g", &cursor("g", oldest_kept)).mode,
            SyncMode::Changes
        );
    }
}
