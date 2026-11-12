//! A lane's card list as rows: workspace boxes, folded subagents and
//! empty workspaces, and the frames that draw them.

use fleet::HostId;
use std::collections::HashSet;

use fleet::lanes::{Family, Lane, LaneCard, card_workspace, needs_user};
use gpui_kit::gpui::{
    Div, Entity, Hsla, InteractiveElement, ParentElement, Rgba, SharedString, Stateful, Styled,
    div, px,
};

use gpui_kit::prelude::FluentBuilder;

use crate::theme::{palette, with_alpha};
use crate::widgets::label_picker::LabelPicker;

/// Lanes show more than this many empty workspaces folded away.
const EMPTY_UNFOLDED: usize = 3;
/// An agent shows more than this many subagents folded away, all but those
/// that need the user.
const SUBAGENTS_UNFOLDED: usize = 2;
pub(super) const BOX_HEADER_HEIGHT: f32 = 24.;
pub(super) const EMPTY_HEIGHT: f32 = 26.;
pub(super) const FOLD_HEIGHT: f32 = 24.;
pub(super) const COMPACT_HEIGHT: f32 = 28.;
pub(super) const SUBAGENT_HEIGHT: f32 = 52.;
/// Indent per level of subagent.
pub(super) const SUBAGENT_INDENT: f32 = 14.;
pub(super) const BOX_RADIUS: f32 = 6.;
/// The least height an expanded card is squeezed to.
pub(super) const EXPANDED_MIN_HEIGHT: f32 = 240.;

/// One row of a lane's card list. Workspace boxes span several rows: a
/// header, then its cards, the last of which closes the box.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Row {
    /// A workspace box's header: the index of its first card, and how many
    /// cards it holds.
    Header { card: usize, count: usize },
    /// A card, inside its workspace's box when `boxed`; `last` closes it.
    /// `depth` is how many parents up its root agent is.
    Card {
        card: usize,
        boxed: bool,
        last: bool,
        depth: usize,
    },
    /// Folds away the subagents of `card`, `count` of them.
    Subagents {
        card: usize,
        count: usize,
        open: bool,
        boxed: bool,
        last: bool,
    },
    /// A workspace's `index`th terminal, in its box.
    Terminal {
        owner: Owner,
        index: usize,
        boxed: bool,
        last: bool,
    },
    /// The toggle that folds away a lane's empty workspaces.
    Fold { count: usize, open: bool },
    /// A workspace without agents: an index into `lane.workspaces`. With
    /// terminals, it heads a box of them (`boxed`); else it is a dashed
    /// line of its own.
    Empty { workspace: usize, boxed: bool },
}

/// The workspace a terminal row belongs to: that of a lane's card, or one
/// of its workspaces without agents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Owner {
    Card(usize),
    Workspace(usize),
}

impl Owner {
    /// Host and workspace id; None for a card with no workspace.
    pub(super) fn workspace(self, lane: &Lane) -> Option<(&HostId, &str)> {
        match self {
            Owner::Card(card) => card_workspace(&lane.cards[card]),
            Owner::Workspace(ix) => {
                let ws = &lane.workspaces[ix];
                Some((&ws.host, ws.workspace_id.as_str()))
            }
        }
    }
}

impl Row {
    /// Whether the gap between boxes follows this row.
    pub(super) fn ends_box(self) -> bool {
        match self {
            Row::Header { .. } => false,
            Row::Card { boxed, last, .. }
            | Row::Subagents { boxed, last, .. }
            | Row::Terminal { boxed, last, .. } => !boxed || last,
            Row::Empty { boxed, .. } => !boxed,
            Row::Fold { .. } => true,
        }
    }

    fn set_last(&mut self) {
        if let Row::Card { last, .. } | Row::Subagents { last, .. } | Row::Terminal { last, .. } =
            self
        {
            *last = true;
        }
    }

    /// Whether `self` in `lane` shows the same as `other` in `other_lane`.
    pub(super) fn same_as(self, lane: &Lane, other: Row, other_lane: &Lane) -> bool {
        let same_card = |a: usize, b: usize| lane.cards[a] == other_lane.cards[b];
        match (self, other) {
            (Row::Header { card: a, count: m }, Row::Header { card: b, count: n }) => {
                m == n
                    && card_workspace(&lane.cards[a]) == card_workspace(&other_lane.cards[b])
                    && lane.cards[a].workspace_name == other_lane.cards[b].workspace_name
            }
            (
                Row::Card {
                    card: a,
                    boxed,
                    last,
                    depth,
                },
                Row::Card {
                    card: b,
                    boxed: boxed_b,
                    last: last_b,
                    depth: depth_b,
                },
            ) => boxed == boxed_b && last == last_b && depth == depth_b && same_card(a, b),
            (
                Row::Subagents {
                    card: a,
                    count,
                    open,
                    boxed,
                    last,
                },
                Row::Subagents {
                    card: b,
                    count: count_b,
                    open: open_b,
                    boxed: boxed_b,
                    last: last_b,
                },
            ) => {
                (count, open, boxed, last) == (count_b, open_b, boxed_b, last_b) && same_card(a, b)
            }
            (
                Row::Empty {
                    workspace: a,
                    boxed,
                },
                Row::Empty {
                    workspace: b,
                    boxed: boxed_b,
                },
            ) => boxed == boxed_b && lane.workspaces[a] == other_lane.workspaces[b],
            (
                Row::Terminal {
                    owner,
                    index,
                    boxed,
                    last,
                },
                Row::Terminal {
                    owner: owner_b,
                    index: index_b,
                    boxed: boxed_b,
                    last: last_b,
                },
            ) => {
                (index, boxed, last) == (index_b, boxed_b, last_b)
                    && owner.workspace(lane) == owner_b.workspace(other_lane)
            }
            (a, b) => a == b,
        }
    }
}

/// The rows of a lane in [`group_workspaces`] order. A workspace with one
/// card is boxed only when `box_single`. An agent's subagents follow it,
/// folded away past [`SUBAGENTS_UNFOLDED`] unless `subagents_open` says it
/// is open, and its workspace's `terminals` close its box. Workspaces
/// without agents follow the cards: those with terminals boxed with them,
/// the empty ones folded away past [`EMPTY_UNFOLDED`] unless `empty_open`.
pub(super) fn lane_rows(
    lane: &Lane,
    box_single: bool,
    empty_open: bool,
    subagents_open: &dyn Fn(&LaneCard) -> bool,
    terminals: &dyn Fn(&HostId, &str) -> usize,
) -> Vec<Row> {
    let family = Family::of(lane);
    let cards = &lane.cards;
    let mut rows = Vec::with_capacity(cards.len() + 4);
    let mut start = 0;
    while start < cards.len() {
        let key = card_workspace(&cards[start]);
        let end = match key {
            Some(key) => {
                start
                    + cards[start..]
                        .iter()
                        .take_while(|card| card_workspace(card) == Some(key))
                        .count()
            }
            None => start + 1,
        };
        let count = end - start;
        let shells = key.map_or(0, |(host, workspace)| terminals(host, workspace));
        let boxed = key.is_some() && (count + shells > 1 || box_single);
        if boxed {
            rows.push(Row::Header { card: start, count });
        }
        let first_row = rows.len();
        let mut at = start;
        while at < end {
            rows.push(Row::Card {
                card: at,
                boxed,
                last: false,
                depth: 0,
            });
            // Its subagents follow it, up to the next root.
            let below = cards[at + 1..end]
                .iter()
                .enumerate()
                .take_while(|(offset, _)| !family.root[at + 1 + offset])
                .count();
            let folds = below > SUBAGENTS_UNFOLDED;
            let open = !folds || subagents_open(&cards[at]);
            if folds {
                rows.push(Row::Subagents {
                    card: at,
                    count: below,
                    open,
                    boxed,
                    last: false,
                });
            }
            for (sub, card) in cards.iter().enumerate().skip(at + 1).take(below) {
                if open || needs_user(card) {
                    rows.push(Row::Card {
                        card: sub,
                        boxed,
                        last: false,
                        depth: family.depth(sub),
                    });
                }
            }
            at += 1 + below;
        }
        rows.extend((0..shells).map(|index| Row::Terminal {
            owner: Owner::Card(start),
            index,
            boxed,
            last: false,
        }));
        if rows.len() > first_row {
            rows.last_mut().expect("just pushed").set_last();
        }
        start = end;
    }
    let occupied: HashSet<(&HostId, &str)> = cards.iter().filter_map(card_workspace).collect();
    let mut empty: Vec<usize> = Vec::new();
    for (ix, ws) in lane.workspaces.iter().enumerate() {
        if occupied.contains(&(&ws.host, ws.workspace_id.as_str())) {
            continue;
        }
        let shells = terminals(&ws.host, &ws.workspace_id);
        if shells == 0 {
            empty.push(ix);
            continue;
        }
        rows.push(Row::Empty {
            workspace: ix,
            boxed: true,
        });
        rows.extend((0..shells).map(|index| Row::Terminal {
            owner: Owner::Workspace(ix),
            index,
            boxed: true,
            last: index + 1 == shells,
        }));
    }
    let folds = empty.len() > EMPTY_UNFOLDED;
    if folds {
        rows.push(Row::Fold {
            count: empty.len(),
            open: empty_open,
        });
    }
    if !folds || empty_open {
        rows.extend(empty.into_iter().map(|workspace| Row::Empty {
            workspace,
            boxed: false,
        }));
    }
    rows
}

/// How a card sits in its lane.
#[derive(Clone, Copy)]
pub(super) enum Frame {
    /// A card of its own, outlined and rounded.
    Alone,
    /// A slice of its workspace's box; `last` closes the box. Unless
    /// `framed`, the box draws its own outline.
    Boxed { last: bool, framed: bool },
}

/// The colours of a workspace's box in one lane.
#[derive(Clone, Copy)]
pub(super) struct BoxLook {
    pub(super) border: Hsla,
    pub(super) bg: Rgba,
    pub(super) hover: Rgba,
    /// Selected in this lane: outlined in the accent.
    pub(super) outlined: bool,
    /// Selected in another lane: tinted with the accent.
    pub(super) tinted: bool,
}

/// Wraps a card's `content` in its frame. Alone, it is its own rounded card;
/// boxed, a slice of its workspace's box, divided from the row above by a
/// hairline. Focus outlines it in the accent either way.
pub(super) fn card_frame(
    id: SharedString,
    height: f32,
    focused: bool,
    frame: Frame,
    look: BoxLook,
    content: Div,
) -> Stateful<Div> {
    let p = palette();
    let accent: Hsla = p.accent.into();
    let hover = look.hover;
    let shell = div()
        .id(id)
        .h(px(height))
        .flex_shrink_0()
        .min_w(px(0.))
        .overflow_hidden();
    match frame {
        Frame::Alone => shell
            .rounded(px(BOX_RADIUS))
            .border_1()
            .border_color(if focused || look.outlined {
                accent
            } else {
                look.border
            })
            .bg(look.bg)
            .hover(move |style| style.bg(hover))
            .child(content.size_full()),
        Frame::Boxed { last, framed } => {
            let closes = framed && last;
            shell
                .when(framed, |shell| shell.border_l_1().border_r_1())
                .when(closes, |shell| shell.border_b_1().rounded_b(px(BOX_RADIUS)))
                .border_color(if focused { accent } else { look.border })
                .child(
                    // GPUI tracks hover only on elements with an id; without
                    // one, hovering repaints only when something else does.
                    content
                        .id("card-inner")
                        .size_full()
                        .border_t_1()
                        .when(focused && !closes, |content| content.border_b_1())
                        .border_color(if focused {
                            accent
                        } else {
                            with_alpha(p.border, 0.7)
                        })
                        .when(closes, |content| content.rounded_b(px(BOX_RADIUS - 1.)))
                        .bg(if focused { look.hover } else { look.bg })
                        .hover(move |style| style.bg(hover)),
                )
        }
    }
}

/// A keyboard or click target in a lane other than a card.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) enum RowFocus {
    /// An empty workspace, by host and workspace id.
    Workspace(HostId, String),
    /// The empty workspaces' fold toggle.
    Fold,
    /// An agent's subagents' fold toggle, by host and agent id.
    Subagents(HostId, String),
    /// A terminal, by host and terminal id.
    Terminal(HostId, String),
}

/// A workspace box's label picker, opened from its header.
pub(super) struct BoxPicker {
    pub(super) lane: String,
    pub(super) host: HostId,
    pub(super) workspace: String,
    pub(super) picker: Entity<LabelPicker>,
}

#[cfg(test)]
mod box_tests {
    use super::*;
    use fleet::Bucket;
    use fleet::lanes::{LaneKind, LaneWorkspace, Placement, group_workspaces};
    use std::collections::{BTreeMap, BTreeSet};

    fn card(agent: &str, workspace: Option<&str>) -> LaneCard {
        LaneCard {
            host: HostId::new("h"),
            agent_id: agent.into(),
            workspace_id: workspace.map(str::to_string),
            workspace_name: workspace.unwrap_or_default().into(),
            title: agent.into(),
            provider: "claude".into(),
            model: None,
            updated_at: None,
            bucket: Bucket::Done,
            attention_reason: None,
            parent: None,
        }
    }

    fn lane(cards: Vec<LaneCard>, workspaces: &[&str]) -> Lane {
        Lane {
            key: "l".into(),
            kind: LaneKind::Other,
            title: "l".into(),
            color: None,
            label_keys: Vec::new(),
            hosts: vec![HostId::new("h")],
            cards,
            workspaces: workspaces
                .iter()
                .map(|id| LaneWorkspace {
                    host: HostId::new("h"),
                    workspace_id: (*id).into(),
                    name: (*id).into(),
                    background: false,
                })
                .collect(),
            collapsed: false,
            band: None,
        }
    }

    /// Grouped as the user has said nothing about where cards go.
    fn group(lane: &Lane) -> Option<Lane> {
        group_workspaces(
            lane,
            &Placement {
                pinned: &BTreeSet::new(),
                order: &BTreeMap::new(),
            },
        )
    }

    #[test]
    fn pinned_workspaces_lead_and_given_card_order_holds() {
        let lane = lane(
            vec![
                card("a1", Some("a")),
                card("a2", Some("a")),
                card("a3", Some("a")),
                card("b1", Some("b")),
            ],
            &["a", "b"],
        );
        let pinned = BTreeSet::from(["h:b".to_string()]);
        let order = BTreeMap::from([("h:a".to_string(), vec!["a3".to_string(), "a1".to_string()])]);
        let grouped = group_workspaces(
            &lane,
            &Placement {
                pinned: &pinned,
                order: &order,
            },
        )
        .unwrap();
        // b is pinned; in a, the given order first, then a2 as it sorts.
        assert_eq!(agents(&grouped), ["b1", "a3", "a1", "a2"]);
    }

    fn agents(lane: &Lane) -> Vec<&str> {
        lane.cards
            .iter()
            .map(|card| card.agent_id.as_str())
            .collect()
    }

    #[test]
    fn a_workspace_gathers_where_its_first_card_sorts() {
        let interleaved = lane(
            vec![
                card("a1", Some("a")),
                card("b1", Some("b")),
                card("loose", None),
                card("a2", Some("a")),
                card("b2", Some("b")),
            ],
            &["a", "b"],
        );
        let grouped = group(&interleaved).unwrap();
        assert_eq!(agents(&grouped), ["a1", "a2", "b1", "b2", "loose"]);
        assert!(group(&grouped).is_none());
    }

    #[test]
    fn rows_box_workspaces_and_leave_loose_cards_alone() {
        let lane = lane(
            vec![
                card("a1", Some("a")),
                card("a2", Some("a")),
                card("b1", Some("b")),
                card("loose", None),
            ],
            &["a", "b"],
        );
        assert_eq!(
            lane_rows(&lane, true, false, &|_| false, &|_, _| 0),
            [
                Row::Header { card: 0, count: 2 },
                Row::Card {
                    card: 0,
                    boxed: true,
                    last: false,
                    depth: 0
                },
                Row::Card {
                    card: 1,
                    boxed: true,
                    last: true,
                    depth: 0
                },
                Row::Header { card: 2, count: 1 },
                Row::Card {
                    card: 2,
                    boxed: true,
                    last: true,
                    depth: 0
                },
                Row::Card {
                    card: 3,
                    boxed: false,
                    last: true,
                    depth: 0
                },
            ]
        );
        // Without boxing single workspaces, only "a" keeps its box.
        let rows = lane_rows(&lane, false, false, &|_| false, &|_, _| 0);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::Header { .. }))
                .count(),
            1
        );
        assert_eq!(
            rows[3],
            Row::Card {
                card: 2,
                boxed: false,
                last: true,
                depth: 0
            }
        );
    }

    #[test]
    fn empty_workspaces_follow_and_fold_beyond_three() {
        let few = lane(vec![card("a1", Some("a"))], &["a", "e1", "e2", "e3"]);
        assert_eq!(
            lane_rows(&few, true, false, &|_| false, &|_, _| 0)[2..],
            [
                Row::Empty {
                    workspace: 1,
                    boxed: false
                },
                Row::Empty {
                    workspace: 2,
                    boxed: false
                },
                Row::Empty {
                    workspace: 3,
                    boxed: false
                },
            ]
        );
        let many = lane(vec![card("a1", Some("a"))], &["a", "e1", "e2", "e3", "e4"]);
        assert_eq!(
            lane_rows(&many, true, false, &|_| false, &|_, _| 0)[2..],
            [Row::Fold {
                count: 4,
                open: false
            }]
        );
        let open = lane_rows(&many, true, true, &|_| false, &|_, _| 0);
        assert_eq!(open.len(), 2 + 1 + 4);
        assert!(open[3..].iter().all(|row| matches!(row, Row::Empty { .. })));
    }

    fn sub(agent: &str, workspace: &str, parent: &str) -> LaneCard {
        LaneCard {
            parent: Some(parent.into()),
            ..card(agent, Some(workspace))
        }
    }

    #[test]
    fn subagents_sit_under_the_agent_that_started_them() {
        let lane = lane(
            vec![
                card("p", Some("a")),
                card("x", Some("b")),
                sub("c2", "a", "c1"),
                sub("c1", "a", "p"),
            ],
            &["a", "b"],
        );
        let grouped = group(&lane).unwrap();
        assert_eq!(agents(&grouped), ["p", "c1", "c2", "x"]);
        let rows = lane_rows(&grouped, true, false, &|_| false, &|_, _| 0);
        let depths: Vec<usize> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Card { depth, .. } => Some(*depth),
                _ => None,
            })
            .collect();
        assert_eq!(depths, [0, 1, 2, 0]);
    }

    #[test]
    fn an_urgent_subagent_lifts_its_parent() {
        let lane = lane(
            vec![
                card("q", Some("a")),
                sub("c", "a", "p"),
                card("p", Some("a")),
            ],
            &["a"],
        );
        assert_eq!(agents(&group(&lane).unwrap()), ["q", "p", "c"]);
    }

    #[test]
    fn many_subagents_fold_but_those_needing_the_user_stay() {
        let mut waiting = sub("c3", "a", "p");
        waiting.bucket = Bucket::NeedsInput;
        let lane = lane(
            vec![
                card("p", Some("a")),
                sub("c1", "a", "p"),
                sub("c2", "a", "p"),
                waiting,
            ],
            &["a"],
        );
        let folded = lane_rows(&lane, true, false, &|_| false, &|_, _| 0);
        assert!(matches!(
            folded[2],
            Row::Subagents {
                card: 0,
                count: 3,
                open: false,
                ..
            }
        ));
        assert!(matches!(
            folded[3],
            Row::Card {
                card: 3,
                depth: 1,
                last: true,
                ..
            }
        ));
        assert_eq!(folded.len(), 4);
        let open = lane_rows(&lane, true, false, &|card| card.agent_id == "p", &|_, _| 0);
        assert_eq!(open.len(), 1 + 1 + 1 + 3);
    }

    #[test]
    fn terminals_close_their_workspaces_box() {
        let lane = lane(vec![card("a1", Some("a"))], &["a", "shell", "bare"]);
        let terminals = |_: &HostId, workspace: &str| match workspace {
            "a" => 2,
            "shell" => 1,
            _ => 0,
        };
        // Alone, "a"'s one card would be unboxed; its terminals box it.
        let rows = lane_rows(&lane, false, false, &|_| false, &terminals);
        assert_eq!(
            rows,
            [
                Row::Header { card: 0, count: 1 },
                Row::Card {
                    card: 0,
                    boxed: true,
                    last: false,
                    depth: 0
                },
                Row::Terminal {
                    owner: Owner::Card(0),
                    index: 0,
                    boxed: true,
                    last: false
                },
                Row::Terminal {
                    owner: Owner::Card(0),
                    index: 1,
                    boxed: true,
                    last: true
                },
                // A workspace with a terminal but no agents heads a box.
                Row::Empty {
                    workspace: 1,
                    boxed: true
                },
                Row::Terminal {
                    owner: Owner::Workspace(1),
                    index: 0,
                    boxed: true,
                    last: true
                },
                Row::Empty {
                    workspace: 2,
                    boxed: false
                },
            ]
        );
        assert!(!rows[4].ends_box());
        assert!(rows[5].ends_box());
        assert_eq!(
            Owner::Workspace(1).workspace(&lane),
            Some((&HostId::new("h"), "shell"))
        );
    }

    #[test]
    fn a_parent_cycle_hides_no_card() {
        let lane = lane(vec![sub("a1", "a", "a2"), sub("a2", "a", "a1")], &["a"]);
        let rows = lane_rows(&lane, true, false, &|_| false, &|_, _| 0);
        let cards = rows
            .iter()
            .filter(|row| matches!(row, Row::Card { .. }))
            .count();
        assert_eq!(cards, 2);
    }

    #[test]
    fn rows_match_by_what_they_show_not_where_they_sit() {
        let before = lane(
            vec![card("a1", Some("a")), card("b1", Some("b"))],
            &["a", "b"],
        );
        let after = lane(
            vec![
                card("new", Some("c")),
                card("a1", Some("a")),
                card("b1", Some("b")),
            ],
            &["a", "b", "c"],
        );
        let (old, new) = (
            lane_rows(&before, true, false, &|_| false, &|_, _| 0),
            lane_rows(&after, true, false, &|_| false, &|_, _| 0),
        );
        // "a"'s header moved down two rows but shows the same.
        assert!(old[0].same_as(&before, new[2], &after));
        assert!(!old[0].same_as(&before, new[0], &after));
    }
}
