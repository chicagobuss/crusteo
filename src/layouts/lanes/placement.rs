//! Expansion placement and acknowledgement decisions after directory changes.
use crate::app::store::LabelChange;
use fleet::{
    directory::label_key,
    lanes::{Lane, LaneKind, is_combination_key},
};
use std::borrow::Borrow;
use std::collections::BTreeMap;

pub enum Progress {
    Writing,
    Acknowledged(Vec<LabelChange>),
}
impl Progress {
    pub fn can_finish(&self, labels: Option<&[String]>, timed_out: bool) -> bool {
        match self {
            Self::Writing => false,
            Self::Acknowledged(changes) => {
                timed_out
                    || changes.is_empty()
                    || labels.is_some_and(|labels| landed(labels, changes))
            }
        }
    }
    pub fn changes(&self) -> &[LabelChange] {
        match self {
            Self::Writing => &[],
            Self::Acknowledged(changes) => changes,
        }
    }
}

pub fn landed(labels: &[String], changes: &[LabelChange]) -> bool {
    changes.iter().all(|change| {
        labels
            .iter()
            .any(|label| label_key(label) == label_key(&change.name))
            == change.assigned
    })
}

/// Placement takes plain lanes (tests) or the board's shared ones.
fn view<L: Borrow<Lane>>(lanes: &[L]) -> Vec<&Lane> {
    lanes.iter().map(Borrow::borrow).collect()
}

pub fn destination(
    lanes: &[impl Borrow<Lane>],
    source: &str,
    host: &str,
    agent: &str,
    changes: &[LabelChange],
    label_grouping: bool,
) -> Option<usize> {
    let lanes = view(lanes);
    let contains = |lane: &Lane| {
        lane.cards
            .iter()
            .any(|card| card.host == host && card.agent_id == agent)
    };
    // A conversation opened in an exact-set lane stays in an exact-set lane
    // after relabeling when that view is available, including in Both mode.
    if is_combination_key(source)
        && let Some(index) = lanes
            .iter()
            .position(|lane| lane.is_combination() && contains(lane))
    {
        return Some(index);
    }
    if label_grouping {
        for change in changes.iter().filter(|change| change.assigned) {
            if let Some(index) = lanes.iter().position(|lane| {
                lane.kind == LaneKind::Label
                    && lane.label_keys == [label_key(&change.name)]
                    && contains(lane)
            }) {
                return Some(index);
            }
        }
    }
    lanes
        .iter()
        .position(|lane| lane.key == source && contains(lane))
        .or_else(|| lanes.iter().position(|lane| contains(lane)))
}

/// A draft's selected workspace must remain a member of its visible lane.
/// A surviving single-label lane may no longer contain it in Combinations mode.
pub fn workspace_destination(
    lanes: &[impl Borrow<Lane>],
    source: &str,
    host: &str,
    workspace: &str,
) -> Option<usize> {
    let lanes = view(lanes);
    let contains = |lane: &Lane| {
        lane.workspaces
            .iter()
            .any(|item| item.host == host && item.workspace_id == workspace)
    };
    if is_combination_key(source)
        && let Some(index) = lanes
            .iter()
            .position(|lane| lane.is_combination() && contains(lane))
    {
        return Some(index);
    }
    lanes
        .iter()
        .position(|lane| lane.key == source && contains(lane))
        .or_else(|| lanes.iter().position(|lane| contains(lane)))
}

/// Only membership-relevant data for active views; token/status pushes do not
/// require a full lane rebuild just to reconcile focus or an unsent draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewMembership {
    focused: Option<AgentMembership>,
    draft: Option<WorkspaceMembership>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentMembership {
    agent: (String, String),
    live: bool,
    workspace: Option<WorkspaceMembership>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceMembership {
    workspace: (String, String),
    present: bool,
    labels: Vec<String>,
}
pub fn view_membership(
    fleet: &fleet::Fleet,
    focused: Option<&(String, String)>,
    draft: Option<&(String, String)>,
) -> ViewMembership {
    let workspace = |host: &str, id: &str| {
        let found = fleet.workspace(host, id);
        let labels = found
            .map(|workspace| fleet::lanes::label_set(workspace).into_keys().collect())
            .unwrap_or_default();
        WorkspaceMembership {
            workspace: (host.into(), id.into()),
            present: found.is_some(),
            labels,
        }
    };
    ViewMembership {
        focused: focused.map(|key| {
            let agent = fleet.agent(&key.0, &key.1);
            AgentMembership {
                agent: key.clone(),
                live: agent.is_some_and(|agent| agent.archived_at.is_none()),
                workspace: agent
                    .and_then(|agent| agent.workspace_id.as_ref())
                    .map(|id| workspace(&key.0, id)),
            }
        }),
        draft: draft.map(|(host, id)| workspace(host, id)),
    }
}
pub fn focus_destination(
    lanes: &[impl Borrow<Lane>],
    source: Option<&str>,
    focused: Option<&(String, String)>,
    focused_draft: Option<&str>,
) -> Option<usize> {
    let lanes = view(lanes);
    let found = if let Some(key) = focused_draft {
        lanes.iter().position(|lane| lane.key == key)
    } else if let Some((host, agent)) = focused {
        destination(&lanes, source.unwrap_or_default(), host, agent, &[], false)
    } else {
        source.and_then(|key| lanes.iter().position(|lane| lane.key == key))
    };
    found.or_else(|| (!lanes.is_empty()).then_some(0))
}

pub fn focus_lane_missing(lanes: &[impl Borrow<Lane>], source: Option<&str>) -> bool {
    let lanes = view(lanes);
    source.is_some_and(|key| !lanes.iter().any(|lane| lane.key == key))
}

pub struct ViewInputs<'a> {
    pub source: Option<&'a str>,
    pub preferred: Option<&'a str>,
    pub focused: Option<&'a (String, String)>,
    pub draft: Option<(&'a str, Option<&'a (String, String)>)>,
    pub draft_focused: bool,
    pub follow: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub struct ViewOutcome {
    pub draft_lane: Option<String>,
    pub draft_focused: bool,
    pub focused_lane: Option<String>,
    pub focused_agent: Option<(String, String)>,
    pub scroll_to: Option<usize>,
}
pub fn reconcile(lanes: &[impl Borrow<Lane>], input: ViewInputs<'_>) -> ViewOutcome {
    let lanes = view(lanes);
    let draft_lane = input.draft.map(|(source, workspace)| {
        workspace
            .and_then(|(host, id)| workspace_destination(&lanes, source, host, id))
            .or_else(|| lanes.iter().position(|lane| lane.key == source))
            .or_else(|| (!lanes.is_empty()).then_some(0))
            .map_or_else(|| source.to_string(), |index| lanes[index].key.clone())
    });
    let preferred = input
        .preferred
        .filter(|_| !input.draft_focused)
        .and_then(|key| lanes.iter().position(|lane| lane.key == key));
    let target = preferred.or_else(|| {
        focus_destination(
            &lanes,
            input.source,
            input.focused,
            draft_lane.as_deref().filter(|_| input.draft_focused),
        )
    });
    let focused_lane = target.map(|index| lanes[index].key.clone());
    let focused_agent = input
        .focused
        .filter(|(host, agent)| {
            target.is_some_and(|index| {
                input.draft_focused
                    || lanes[index]
                        .cards
                        .iter()
                        .any(|card| &card.host == host && &card.agent_id == agent)
            })
        })
        .cloned();
    let scroll_to =
        target.filter(|index| input.follow && input.source != Some(lanes[*index].key.as_str()));
    ViewOutcome {
        draft_lane,
        draft_focused: input.draft_focused && target.is_some(),
        focused_lane,
        focused_agent,
        scroll_to,
    }
}

/// An explicit move wins a collision, then the focused expansion, then an agent already in that lane,
/// then source-key order. Swaps must be computed from an immutable old map.
pub fn relocate(
    expanded: &BTreeMap<String, (String, String)>,
    lanes: &[impl Borrow<Lane>],
    protected: &[(String, String)],
    focused: Option<&(String, String)>,
    requested: Option<(&(String, String), &str)>,
) -> BTreeMap<String, (String, String)> {
    let lanes = view(lanes);
    let mut candidates: Vec<_> = expanded
        .iter()
        .map(|(source, agent)| {
            let target =
                if let Some((_, target)) = requested.filter(|(requested, _)| *requested == agent) {
                    target.to_string()
                } else if protected.contains(agent) {
                    source.clone()
                } else {
                    destination(&lanes, source, &agent.0, &agent.1, &[], false)
                        .map_or_else(|| source.clone(), |index| lanes[index].key.clone())
                };
            (target, source, agent)
        })
        .collect();
    candidates.sort_by_key(|(target, source, agent)| {
        (
            requested.is_none_or(|(requested, _)| requested != *agent),
            focused != Some(*agent),
            target != *source,
            *source,
        )
    });
    let mut relocated = BTreeMap::new();
    for (target, _, agent) in candidates {
        relocated.entry(target).or_insert_with(|| agent.clone());
    }
    relocated
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet::lanes::LaneCard;
    fn lane(key: &str) -> Lane {
        Lane {
            key: key.into(),
            kind: LaneKind::Label,
            title: key.into(),
            color: None,
            label_keys: vec![key.into()],
            hosts: vec!["mac".into()],
            workspaces: vec![],
            collapsed: false,
            cards: vec![LaneCard {
                host: "mac".into(),
                agent_id: "a".into(),
                workspace_id: Some("w".into()),
                workspace_name: "w".into(),
                title: "a".into(),
                provider: "codex".into(),
                model: None,
                updated_at: None,
                bucket: fleet::Bucket::Done,
                attention_reason: None,
                parent: None,
            }],
        }
    }

    #[test]
    fn membership_tracks_external_relabels_without_following_status_pushes() {
        let workspace = serde_json::from_value(serde_json::json!({
            "id": "w", "projectId": "p", "name": "w", "labels": ["A", "B"]
        }))
        .unwrap();
        let agent = paseo_client::proto::Agent::from_value(serde_json::json!({
            "id": "a", "workspaceId": "w", "status": "done",
            "updatedAt": "2026-10-01T00:00:00Z", "provider": "claude"
        }))
        .unwrap();
        let mut fleet = fleet::Fleet {
            hosts: vec![fleet::Host {
                config: fleet::HostConfig {
                    name: "mac".into(),
                    ..Default::default()
                },
                directory: fleet::Directory {
                    workspaces: BTreeMap::from([("w".into(), workspace)]),
                    agents: std::collections::HashMap::from([("a".into(), agent)]),
                    ..Default::default()
                },
                ..Default::default()
            }],
        };
        let focused = ("mac".into(), "a".into());
        let draft = ("mac".into(), "w".into());
        let before = view_membership(&fleet, Some(&focused), Some(&draft));
        fleet.hosts[0]
            .directory
            .workspaces
            .get_mut("w")
            .unwrap()
            .labels = vec![" b ".into(), "a".into(), "A".into(), " ".into()];
        fleet.hosts[0].directory.agents.insert(
            "a".into(),
            paseo_client::proto::Agent::from_value(serde_json::json!({
                "id": "a", "workspaceId": "w", "status": "running",
                "updatedAt": "2026-10-04T12:00:00Z", "provider": "claude",
                "activeTurn": {"phase": "open"}, "title": "new title"
            }))
            .unwrap(),
        );
        assert_eq!(
            view_membership(&fleet, Some(&focused), Some(&draft)),
            before
        );
        fleet.hosts[0]
            .directory
            .workspaces
            .get_mut("w")
            .unwrap()
            .labels
            .push("C".into());
        let relabelled = view_membership(&fleet, Some(&focused), Some(&draft));
        assert_ne!(relabelled.focused, before.focused);
        assert_ne!(relabelled.draft, before.draft);
        fleet.hosts[0].directory.workspaces.remove("w");
        let removed = view_membership(&fleet, Some(&focused), Some(&draft));
        assert_ne!(removed, relabelled);
        fleet.hosts[0].directory.agents.remove("a");
        assert_ne!(
            view_membership(&fleet, Some(&focused), Some(&draft)),
            removed
        );
        assert_eq!(
            view_membership(&fleet, None, None),
            ViewMembership {
                focused: None,
                draft: None
            }
        );
    }

    #[test]
    fn focus_follows_its_card_or_draft_and_falls_back_when_removed() {
        let mut combo = lane("\u{0}label-set:[\"b\",\"c\"]");
        combo.kind = LaneKind::Combination;
        combo.label_keys = vec!["b".into(), "c".into()];
        let lanes = vec![lane("b"), combo];
        let focused = ("mac".into(), "a".into());
        assert_eq!(
            focus_destination(
                &lanes,
                Some("\u{0}label-set:[\"a\",\"b\"]"),
                Some(&focused),
                None
            ),
            Some(1)
        );
        assert_eq!(
            focus_destination(&lanes, Some("b"), Some(&focused), Some(&lanes[1].key)),
            Some(1)
        );
        assert_eq!(
            focus_destination(&lanes, Some(&lanes[1].key), Some(&focused), Some("b")),
            Some(0)
        );
        assert_eq!(
            focus_destination(&lanes, Some("removed"), None, None),
            Some(0)
        );
        let missing = ("other-host".into(), "a".into());
        assert_eq!(
            focus_destination(&lanes, Some("removed"), Some(&missing), None),
            Some(0)
        );
        assert_eq!(
            focus_destination(&[] as &[Lane], Some("b"), Some(&focused), None),
            None
        );
    }

    #[test]
    fn view_outcome_relocates_drafts_and_honors_follow_without_hidden_focus() {
        let mut old = lane("b");
        old.cards.clear();
        let mut combo = lane("\u{0}label-set:[\"b\",\"c\"]");
        combo.kind = LaneKind::Combination;
        combo.label_keys = vec!["b".into(), "c".into()];
        combo.workspaces.push(fleet::LaneWorkspace {
            host: "mac".into(),
            workspace_id: "w".into(),
            name: "w".into(),
        });
        let lanes = vec![old, combo];
        let agent = ("mac".into(), "a".into());
        let workspace = ("mac".into(), "w".into());
        for follow in [false, true] {
            let outcome = reconcile(
                &lanes,
                ViewInputs {
                    preferred: None,
                    source: Some("b"),
                    focused: Some(&agent),
                    draft: Some(("b", Some(&workspace))),
                    draft_focused: true,
                    follow,
                },
            );
            assert_eq!(outcome.draft_lane, Some(lanes[1].key.clone()));
            assert_eq!(outcome.focused_lane, Some(lanes[1].key.clone()));
            assert_eq!(outcome.focused_agent, Some(agent.clone()));
            assert_eq!(outcome.scroll_to, follow.then_some(1));
        }
        let missing = ("mac".into(), "removed".into());
        let outcome = reconcile(
            &lanes,
            ViewInputs {
                preferred: None,
                source: Some("missing"),
                focused: Some(&missing),
                draft: None,
                draft_focused: false,
                follow: false,
            },
        );
        assert_eq!(outcome.focused_lane.as_deref(), Some("b"));
        assert_eq!(outcome.focused_agent, None);
        assert_eq!(outcome.scroll_to, None);
        assert!(focus_lane_missing(&lanes, Some("missing")));
        assert!(!focus_lane_missing(&lanes, Some("b")));
        assert!(!focus_lane_missing(&lanes, None));
        assert!(focus_lane_missing(&[] as &[Lane], Some("b")));
        let gone_workspace = ("mac".into(), "deleted-workspace".into());
        for workspace in [None, Some(&gone_workspace)] {
            let orphaned = reconcile(
                &lanes,
                ViewInputs {
                    source: Some("removed"),
                    preferred: None,
                    focused: None,
                    draft: Some(("removed", workspace)),
                    draft_focused: true,
                    follow: true,
                },
            );
            assert_eq!(orphaned.draft_lane.as_deref(), Some("b"));
            assert_eq!(orphaned.focused_lane.as_deref(), Some("b"));
            assert!(orphaned.draft_focused);
        }
        let choices = [lane("y"), lane("z")];
        let moved = reconcile(
            &choices,
            ViewInputs {
                source: Some("x"),
                preferred: Some("z"),
                focused: Some(&agent),
                draft: None,
                draft_focused: false,
                follow: false,
            },
        );
        assert_eq!(moved.focused_lane.as_deref(), Some("z"));
        assert_eq!(moved.focused_agent, Some(agent.clone()));
        assert_eq!(moved.scroll_to, None);
        let empty = reconcile(
            &[] as &[Lane],
            ViewInputs {
                preferred: None,
                source: Some("b"),
                focused: Some(&agent),
                draft: Some(("b", Some(&workspace))),
                draft_focused: true,
                follow: true,
            },
        );
        assert_eq!(empty.focused_lane, None);
        assert_eq!(empty.focused_agent, None);
        assert_eq!(empty.scroll_to, None);
        assert_eq!(empty.draft_lane.as_deref(), Some("b"));
        assert!(!empty.draft_focused);
    }
    #[test]
    fn combinations_keep_expansion_kind_and_drafts_follow_workspace_membership() {
        let mut combo = lane("\u{0}label-set:[\"b\",\"c\"]");
        combo.kind = LaneKind::Combination;
        combo.label_keys = vec!["b".into(), "c".into()];
        combo.workspaces = vec![fleet::LaneWorkspace {
            host: "mac".into(),
            workspace_id: "multi".into(),
            name: "multi".into(),
        }];
        let mut individual = lane("b");
        individual.workspaces = vec![fleet::LaneWorkspace {
            host: "mac".into(),
            workspace_id: "single".into(),
            name: "single".into(),
        }];
        let lanes = vec![individual.clone(), combo.clone()];
        let added = vec![LabelChange {
            name: "b".into(),
            color: "red".into(),
            assigned: true,
        }];
        let old_combo = "\u{0}label-set:[\"a\",\"c\"]";
        assert_eq!(
            destination(&lanes, old_combo, "mac", "a", &added, true),
            Some(1)
        );
        assert_eq!(
            destination(&lanes, "old", "mac", "a", &added, true),
            Some(0)
        );
        assert_eq!(
            destination(&lanes[1..], "b", "mac", "a", &[], false),
            Some(0)
        );
        // A b lane still exists, but no longer contains this selected workspace.
        assert_eq!(workspace_destination(&lanes, "b", "mac", "multi"), Some(1));
        assert_eq!(workspace_destination(&lanes, "b", "mac", "single"), Some(0));
        assert_eq!(
            workspace_destination(&lanes, old_combo, "mac", "multi"),
            Some(1)
        );
        assert_eq!(
            workspace_destination(&lanes, "b", "other-host", "multi"),
            None
        );
        assert_eq!(workspace_destination(&lanes, "b", "mac", "missing"), None);
        let a = ("mac".into(), "a".into());
        let old = BTreeMap::from([("b".into(), a.clone()), (old_combo.into(), a.clone())]);
        let relocated = relocate(&old, &[combo.clone()], &[], Some(&a), None);
        assert_eq!(relocated.len(), 1);
        assert_eq!(relocated.get(&combo.key), Some(&a));
        assert_eq!(
            relocate(&relocated, &[individual], &[], Some(&a), None).get("b"),
            Some(&a)
        );
    }

    #[test]
    fn prefer_added_label_and_preserve_source_in_other_groupings() {
        let lanes = vec![lane("old"), lane("new")];
        let changes = vec![LabelChange {
            name: " NEW ".into(),
            color: "teal".into(),
            assigned: true,
        }];
        assert_eq!(
            destination(&lanes, "old", "mac", "a", &changes, true),
            Some(1)
        );
        assert_eq!(
            destination(&lanes, "old", "mac", "a", &changes, false),
            Some(0)
        );
        assert_eq!(
            destination(&lanes[1..], "old", "mac", "a", &[], true),
            Some(0)
        );
        assert_eq!(
            destination(&lanes, "old", "mac", "missing", &changes, true),
            None
        );
        assert!(landed(
            &["new".into(), "concurrent unrelated label".into()],
            &changes
        ));
        assert!(!landed(&["old".into()], &changes));
    }
    #[test]
    fn simultaneous_swaps_and_collisions_are_deterministic() {
        let a = ("mac".into(), "a".into());
        let b = ("mac".into(), "b".into());
        let old = BTreeMap::from([("x".into(), a.clone()), ("y".into(), b.clone())]);
        let mut x = lane("x");
        x.cards[0].agent_id = "b".into();
        let y = lane("y");
        let swapped = relocate(&old, &[x, y], &[], None, None);
        assert_eq!(
            swapped,
            BTreeMap::from([("x".into(), b.clone()), ("y".into(), a.clone())])
        );
        let mut y = lane("y");
        let mut second = y.cards[0].clone();
        second.agent_id = "b".into();
        y.cards.push(second);
        assert_eq!(
            relocate(&old, &[y.clone()], &[], None, None).get("y"),
            Some(&b)
        );
        assert_eq!(
            relocate(&old, &[y.clone()], &[], Some(&a), None).get("y"),
            Some(&a)
        );
        assert_eq!(
            relocate(&old, &[y.clone()], std::slice::from_ref(&a), None, None),
            old
        );
    }
    #[test]
    fn acknowledgement_waits_for_push_and_can_time_out_without_waiting_forever() {
        assert!(!Progress::Writing.can_finish(None, true));
        let changes = vec![LabelChange {
            name: "new".into(),
            color: "teal".into(),
            assigned: true,
        }];
        let progress = Progress::Acknowledged(changes);
        assert!(!progress.can_finish(None, false));
        assert!(!progress.can_finish(Some(&["old".into()]), false));
        assert!(progress.can_finish(Some(&["new".into(), "concurrent".into()]), false));
        assert!(progress.can_finish(None, true));
        assert!(Progress::Acknowledged(vec![]).can_finish(None, false));
        let a = ("mac".into(), "a".into());
        let b = ("mac".into(), "b".into());
        let old = BTreeMap::from([("x".into(), a.clone()), ("y".into(), b.clone())]);
        let mut y = lane("y");
        let mut second = y.cards[0].clone();
        second.agent_id = "b".into();
        y.cards.push(second);
        assert_eq!(
            relocate(&old, &[y], &[], Some(&b), Some((&a, "y"))).get("y"),
            Some(&a)
        );
    }
}
