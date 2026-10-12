//! Toolbars: rows of items the user arranges, as in early Firefox.
//!
//! Every control in a bar is a [`ToolbarItem`] registered by id with the
//! [`Registry`]: the shell's own (layout switcher, attention counts, usage
//! meters, hosts), the "Needs you" strip, and those a layout provides, such
//! as Lanes' grouping controls. An [`Arrangement`] says which bar each item
//! sits in, in its left or right group, in order; it is saved with the rest
//! of the UI state. Items the arrangement does not mention yet go to the bar
//! and side they ask for, so a new item, or one from an extension, appears
//! without the user placing it.
//!
//! A bar shows only the items that apply to the layout on screen, and a bar
//! with none of them takes no room, except the title bar, which always shows.

use std::rc::Rc;

use gpui_kit::gpui::{
    AnyElement, App, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, Window,
    div, px,
};
use gpui_kit::prelude::FluentBuilder as _;
use serde::{Deserialize, Serialize};

pub mod builtin;
pub mod customize;

/// The window's title bar.
pub const TITLE: &str = "title";
/// The bar under it, which holds "Needs you" by default.
pub const STRIP: &str = "strip";
/// The bar over the layout, which holds the layout's own controls.
pub const LAYOUT: &str = "layout";
/// Bars in the order they stack.
pub const BARS: [&str; 3] = [TITLE, STRIP, LAYOUT];

/// Items that no longer exist; arrangements drop them.
const RETIRED: [&str; 1] = ["lanes.keys-hint"];

/// Whether a bar is one the user made, rather than one of [`BARS`].
pub fn is_custom(bar: &str) -> bool {
    !BARS.contains(&bar)
}

/// An empty gap of fixed width. It sets an item off from the bar's edge or
/// from its neighbours; an arrangement may hold any number of them.
pub const SPACER: &str = "spacer";
pub const SPACER_WIDTH: f32 = 24.;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

/// A control that can sit in any bar.
pub trait ToolbarItem {
    /// Stable, unique; saved in arrangements.
    fn id(&self) -> &str;
    /// What customizing calls it.
    fn title(&self) -> SharedString;
    /// Where it goes until the user moves it.
    fn home(&self) -> (&'static str, Side);
    /// Where among the other new items in its home it goes: lower first.
    fn order(&self) -> i32 {
        0
    }
    /// Whether it shows while the layout `layout` is on screen.
    fn shows_in(&self, _layout: &str, _cx: &App) -> bool {
        true
    }
    /// Whether it takes the room left in its bar, as the "Needs you" strip
    /// does.
    fn grows(&self) -> bool {
        false
    }
    /// Whether it gives way, clipped, when its bar runs out of room. Items
    /// that do not keep their width.
    fn shrinks(&self) -> bool {
        false
    }
    fn render(&self, window: &mut Window, cx: &mut App) -> Option<AnyElement>;
}

/// Every toolbar item there is, by id.
#[derive(Clone, Default)]
pub struct Registry {
    items: Vec<Rc<dyn ToolbarItem>>,
}

impl Registry {
    /// Adds an item, replacing one with the same id.
    pub fn register(&mut self, item: Rc<dyn ToolbarItem>) {
        self.items.retain(|known| known.id() != item.id());
        self.items.push(item);
    }

    pub fn get(&self, id: &str) -> Option<Rc<dyn ToolbarItem>> {
        self.items.iter().find(|item| item.id() == id).cloned()
    }

    pub fn items(&self) -> impl Iterator<Item = &Rc<dyn ToolbarItem>> {
        self.items.iter()
    }
}

/// One bar's items, by id, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarArrangement {
    pub id: String,
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub hidden: bool,
}

/// Where every item sits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Arrangement {
    pub bars: Vec<BarArrangement>,
    /// Items the user took out of every bar.
    pub removed: Vec<String>,
}

impl Arrangement {
    pub fn bar(&self, id: &str) -> Option<&BarArrangement> {
        self.bars.iter().find(|bar| bar.id == id)
    }

    fn bar_mut(&mut self, id: &str) -> &mut BarArrangement {
        if let Some(at) = self.bars.iter().position(|bar| bar.id == id) {
            return &mut self.bars[at];
        }
        self.bars.push(BarArrangement {
            id: id.to_string(),
            ..Default::default()
        });
        self.bars.last_mut().expect("just pushed")
    }

    /// Whether the arrangement places or removed `item`.
    pub fn knows(&self, item: &str) -> bool {
        self.removed.iter().any(|id| id == item)
            || self
                .bars
                .iter()
                .any(|bar| bar.left.iter().chain(&bar.right).any(|id| id == item))
    }

    fn group_mut(&mut self, bar: &str, side: Side) -> &mut Vec<String> {
        let bar = self.bar_mut(bar);
        match side {
            Side::Left => &mut bar.left,
            Side::Right => &mut bar.right,
        }
    }

    /// Takes what is being dragged out of where it was: a placed item from
    /// its slot, a removed item from the removed list. A spacer from the
    /// palette is new and comes from nowhere.
    fn take(&mut self, dragged: &Dragged) {
        match &dragged.from {
            Some(slot) => {
                let group = self.group_mut(&slot.bar, slot.side);
                if group.get(slot.index) == Some(&dragged.id) {
                    group.remove(slot.index);
                }
            }
            None => self.removed.retain(|id| id != &dragged.id),
        }
    }

    /// Moves what is dragged to `bar`'s `side`, before its `before`th slot,
    /// or last.
    pub fn place(&mut self, dragged: &Dragged, bar: &str, side: Side, before: Option<usize>) {
        let mut before = before;
        // Taking it out of the same group shifts what follows it.
        if let (Some(from), Some(at)) = (&dragged.from, before)
            && from.bar == bar
            && from.side == side
            && from.index < at
        {
            before = Some(at - 1);
        }
        self.take(dragged);
        let group = self.group_mut(bar, side);
        let at = before.map_or(group.len(), |at| at.min(group.len()));
        group.insert(at, dragged.id.clone());
    }

    /// Takes a placed item out of every bar. A spacer simply goes.
    pub fn remove(&mut self, dragged: &Dragged) {
        if dragged.from.is_none() {
            return;
        }
        self.take(dragged);
        if dragged.id != SPACER && !self.removed.contains(&dragged.id) {
            self.removed.push(dragged.id.clone());
        }
    }

    pub fn set_hidden(&mut self, bar: &str, hidden: bool) {
        self.bar_mut(bar).hidden = hidden;
    }

    /// Adds an empty bar of the user's own, under the others. Returns its id.
    pub fn add_bar(&mut self) -> String {
        let id = (1..)
            .map(|n| format!("bar-{n}"))
            .find(|id| self.bar(id).is_none())
            .expect("a free bar id");
        self.bar_mut(&id);
        id
    }

    /// Removes a bar of the user's own; its items are taken out, ready to
    /// place again.
    pub fn remove_bar(&mut self, bar: &str) {
        if !is_custom(bar) {
            return;
        }
        let Some(at) = self.bars.iter().position(|placed| placed.id == bar) else {
            return;
        };
        let placed = self.bars.remove(at);
        for id in placed.left.into_iter().chain(placed.right) {
            if id != SPACER && !self.removed.contains(&id) {
                self.removed.push(id);
            }
        }
    }

    /// The bars the user made, in the order they were made.
    pub fn custom_bars(&self) -> impl Iterator<Item = &BarArrangement> {
        self.bars.iter().filter(|bar| is_custom(&bar.id))
    }

    /// Places each item not yet placed at its home, before the first item
    /// there that orders after it ([`ToolbarItem::order`]), else last.
    /// Returns whether anything moved.
    pub fn place_new(&mut self, homes: &[Home]) -> bool {
        let mut changed = false;
        for bar in &mut self.bars {
            for group in [&mut bar.left, &mut bar.right] {
                let before = group.len();
                group.retain(|id| !RETIRED.contains(&id.as_str()));
                changed |= group.len() != before;
            }
        }
        let order_of = |id: &str| {
            homes
                .iter()
                .find(|home| home.id == id)
                .map_or(0, |home| home.order)
        };
        for home in homes {
            if self.knows(&home.id) {
                continue;
            }
            let bar = self.bar_mut(home.bar);
            let group = match home.side {
                Side::Left => &mut bar.left,
                Side::Right => &mut bar.right,
            };
            let at = group
                .iter()
                .position(|other| other != SPACER && order_of(other) > home.order)
                .unwrap_or(group.len());
            group.insert(at, home.id.clone());
            changed = true;
        }
        changed
    }
}

/// A slot in a bar: which bar, which side, which position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotRef {
    pub bar: String,
    pub side: Side,
    pub index: usize,
}

/// What is being dragged while customizing: an item, or a spacer, from a
/// slot, or from the palette when `from` is None.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dragged {
    pub id: String,
    pub title: SharedString,
    pub from: Option<SlotRef>,
}

/// Where an item goes until the user moves it, for [`Arrangement::place_new`].
pub struct Home {
    pub id: String,
    pub bar: &'static str,
    pub side: Side,
    pub order: i32,
}

impl Home {
    pub fn of(item: &dyn ToolbarItem) -> Self {
        let (bar, side) = item.home();
        Self {
            id: item.id().to_string(),
            bar,
            side,
            order: item.order(),
        }
    }
}

/// One bar's items for the layout on screen: left group, right group. None
/// when the bar is hidden or none of its items apply.
pub fn bar_items(
    bar: &str,
    layout: &str,
    arrangement: &Arrangement,
    registry: &Registry,
    cx: &App,
) -> Option<(Vec<Slot>, Vec<Slot>)> {
    let placed = arrangement.bar(bar)?;
    if placed.hidden {
        return None;
    }
    let resolve = |ids: &[String]| -> Vec<Slot> {
        let mut slots: Vec<Slot> = ids
            .iter()
            .filter_map(|id| {
                if id == SPACER {
                    return Some(Slot::Spacer);
                }
                let item = registry.get(id)?;
                item.shows_in(layout, cx).then_some(Slot::Item(item))
            })
            .collect();
        // Spacers only set items apart: without an item they would hold an
        // empty bar open.
        if !slots.iter().any(|slot| matches!(slot, Slot::Item(_))) {
            slots.clear();
        }
        slots
    };
    let (left, right) = (resolve(&placed.left), resolve(&placed.right));
    (!left.is_empty() || !right.is_empty()).then_some((left, right))
}

/// An item or a spacer in a bar.
pub enum Slot {
    Item(Rc<dyn ToolbarItem>),
    Spacer,
}

/// A bar's contents: its left group, the room between, and its right group.
/// The caller wraps it in the bar's frame.
pub fn bar_contents(
    left: Vec<Slot>,
    right: Vec<Slot>,
    occlude: bool,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let render = |slots: Vec<Slot>, window: &mut Window, cx: &mut App| {
        let mut grew = false;
        let elements = slots
            .into_iter()
            .filter_map(|slot| match slot {
                Slot::Spacer => Some(div().w(px(SPACER_WIDTH)).flex_shrink_0().into_any_element()),
                Slot::Item(item) => {
                    // Whether it grows is known only once it draws: an item
                    // with nothing to show takes no room.
                    let grows = item.grows();
                    let shrinks = item.shrinks();
                    let element = item.render(window, cx)?;
                    grew |= grows;
                    let wrapper = div()
                        .id(SharedString::from(format!("toolbar-{}", item.id())))
                        .flex()
                        .items_center()
                        .min_w(px(0.))
                        .when(grows, |wrapper| wrapper.flex_1())
                        .when(shrinks, |wrapper| wrapper.overflow_hidden())
                        .when(!grows && !shrinks, |wrapper| wrapper.flex_shrink_0())
                        // In the title bar, an item is not a place to drag
                        // the window from.
                        .when(occlude, |wrapper| wrapper.occlude())
                        .child(element);
                    Some(wrapper.into_any_element())
                }
            })
            .collect::<Vec<_>>();
        (elements, grew)
    };
    let (left, left_grows) = render(left, window, cx);
    let (right, _) = render(right, window, cx);
    div()
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .overflow_hidden()
        .flex()
        .items_center()
        .gap(px(10.))
        .children(left)
        // The room between the groups; an item that grows takes it instead.
        .when(!left_grows, |bar| bar.child(div().flex_1()))
        .children(right)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(id: &str, bar: &'static str, side: Side, order: i32) -> Home {
        Home {
            id: id.into(),
            bar,
            side,
            order,
        }
    }

    fn homes() -> Vec<Home> {
        vec![
            home("switcher", TITLE, Side::Left, 0),
            home("hosts", TITLE, Side::Right, 100),
            home("needs", STRIP, Side::Left, 0),
        ]
    }

    fn dragged(id: &str, from: Option<(&str, Side, usize)>) -> Dragged {
        Dragged {
            id: id.into(),
            title: id.to_string().into(),
            from: from.map(|(bar, side, index)| SlotRef {
                bar: bar.into(),
                side,
                index,
            }),
        }
    }

    #[test]
    fn items_move_between_bars_and_within_a_group() {
        let mut arrangement = Arrangement::default();
        arrangement.place_new(&homes());
        // To the strip, before "needs".
        arrangement.place(
            &dragged("hosts", Some((TITLE, Side::Right, 0))),
            STRIP,
            Side::Left,
            Some(0),
        );
        assert!(arrangement.bar(TITLE).unwrap().right.is_empty());
        assert_eq!(arrangement.bar(STRIP).unwrap().left, ["hosts", "needs"]);
        // Later in its own group: the slot it left shifts the target.
        arrangement.place(
            &dragged("hosts", Some((STRIP, Side::Left, 0))),
            STRIP,
            Side::Left,
            Some(2),
        );
        assert_eq!(arrangement.bar(STRIP).unwrap().left, ["needs", "hosts"]);
        // To the end of another group.
        arrangement.place(
            &dragged("needs", Some((STRIP, Side::Left, 0))),
            TITLE,
            Side::Right,
            None,
        );
        assert_eq!(arrangement.bar(TITLE).unwrap().right, ["needs"]);
    }

    #[test]
    fn removed_items_return_from_the_palette_and_spacers_multiply() {
        let mut arrangement = Arrangement::default();
        arrangement.place_new(&homes());
        arrangement.remove(&dragged("switcher", Some((TITLE, Side::Left, 0))));
        assert_eq!(arrangement.removed, ["switcher"]);
        assert!(arrangement.bar(TITLE).unwrap().left.is_empty());
        // Back from the palette.
        arrangement.place(&dragged("switcher", None), TITLE, Side::Left, None);
        assert!(arrangement.removed.is_empty());
        // Spacers come from the palette as often as wanted, and simply go.
        arrangement.place(&dragged(SPACER, None), TITLE, Side::Left, Some(0));
        arrangement.place(&dragged(SPACER, None), TITLE, Side::Left, Some(0));
        assert_eq!(
            arrangement.bar(TITLE).unwrap().left,
            [SPACER, SPACER, "switcher"]
        );
        arrangement.remove(&dragged(SPACER, Some((TITLE, Side::Left, 1))));
        assert_eq!(arrangement.bar(TITLE).unwrap().left, [SPACER, "switcher"]);
        assert!(arrangement.removed.is_empty());
    }

    #[test]
    fn own_bars_come_and_go_and_give_their_items_back() {
        let mut arrangement = Arrangement::default();
        arrangement.place_new(&homes());
        let bar = arrangement.add_bar();
        assert_eq!(bar, "bar-1");
        assert!(is_custom(&bar) && !is_custom(TITLE));
        arrangement.place(
            &dragged("hosts", Some((TITLE, Side::Right, 0))),
            &bar,
            Side::Left,
            None,
        );
        arrangement.place(&dragged(SPACER, None), &bar, Side::Left, None);
        assert_eq!(arrangement.add_bar(), "bar-2");
        arrangement.remove_bar(&bar);
        assert!(arrangement.bar(&bar).is_none());
        assert_eq!(arrangement.removed, ["hosts"]);
        // The built-in bars stay.
        arrangement.remove_bar(TITLE);
        assert!(arrangement.bar(TITLE).is_some());
        assert_eq!(arrangement.add_bar(), "bar-1");
    }

    #[test]
    fn retired_items_drop_out() {
        let mut arrangement = Arrangement {
            bars: vec![BarArrangement {
                id: LAYOUT.into(),
                left: vec!["lanes.keys-hint".into(), "lanes.grouping".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(arrangement.place_new(&[]));
        assert_eq!(arrangement.bar(LAYOUT).unwrap().left, ["lanes.grouping"]);
    }

    #[test]
    fn a_late_item_goes_before_those_ordered_after_it() {
        let mut arrangement = Arrangement::default();
        arrangement.place_new(&homes());
        let mut later = homes();
        later.push(home("clock", TITLE, Side::Right, 50));
        arrangement.place_new(&later);
        assert_eq!(arrangement.bar(TITLE).unwrap().right, ["clock", "hosts"]);
    }

    #[test]
    fn new_items_go_home_after_what_is_there() {
        let mut arrangement = Arrangement::default();
        assert!(arrangement.place_new(&homes()));
        assert_eq!(arrangement.bar(TITLE).unwrap().left, ["switcher"]);
        assert_eq!(arrangement.bar(TITLE).unwrap().right, ["hosts"]);
        assert_eq!(arrangement.bar(STRIP).unwrap().left, ["needs"]);
        assert!(
            !arrangement.place_new(&homes()),
            "placing twice moves nothing"
        );
    }

    #[test]
    fn moved_and_removed_items_stay_where_the_user_put_them() {
        let mut arrangement = Arrangement {
            bars: vec![BarArrangement {
                id: STRIP.into(),
                left: vec![SPACER.into(), "hosts".into()],
                ..Default::default()
            }],
            removed: vec!["needs".into()],
        };
        arrangement.place_new(&homes());
        assert_eq!(arrangement.bar(STRIP).unwrap().left, [SPACER, "hosts"]);
        assert_eq!(arrangement.bar(TITLE).unwrap().left, ["switcher"]);
        assert!(arrangement.bar(TITLE).unwrap().right.is_empty());
    }

    #[test]
    fn arrangements_round_trip_and_tolerate_missing_fields() {
        let mut arrangement = Arrangement::default();
        arrangement.place_new(&homes());
        let json = serde_json::to_string(&arrangement).unwrap();
        assert_eq!(
            serde_json::from_str::<Arrangement>(&json).unwrap(),
            arrangement
        );
        let sparse: Arrangement =
            serde_json::from_str(r#"{"bars":[{"id":"title","left":["switcher"]}]}"#).unwrap();
        assert!(sparse.bar(TITLE).unwrap().right.is_empty());
        assert!(!sparse.bar(TITLE).unwrap().hidden);
    }
}
