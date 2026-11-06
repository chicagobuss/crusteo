//! crusteo: keyboard. Once the app gives a selection the keyboard, its moving
//! end is a caret: it blinks, keys move it, and the selection is painted grey
//! while the keyboard is somewhere else.

use gpui::{AvailableSpace, ContentMask, FocusHandle, Task, canvas, fill, size};
use instant::{Duration, Instant};

use super::*;

const BLINK: Duration = Duration::from_millis(500);
#[cfg(not(target_os = "macos"))]
const CARET_WIDTH: Pixels = px(2.);
#[cfg(target_os = "macos")]
const CARET_WIDTH: Pixels = px(1.5);

/// Where a key moves a selection's moving end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMotion {
    Left,
    Right,
    /// To the start of this word, or of the one before.
    WordLeft,
    /// To the end of this word, or of the one after.
    WordRight,
    Up,
    Down,
    /// To the start of the row on screen.
    LineStart,
    /// To the end of the row on screen.
    LineEnd,
    /// Over the whole of the selection's group.
    All,
}

#[derive(Default)]
pub(super) struct Keyboard {
    /// The focus that, while it has it, gives the selection the keyboard.
    owner: Option<FocusHandle>,
    /// When the caret last moved: it stays lit for a beat after.
    moved: Option<Instant>,
    /// The x a run of Up and Down keeps to, and where the last left the caret.
    goal: Option<(Pixels, Point<Pixels>)>,
    blink: Option<Task<()>>,
}

impl Keyboard {
    /// A new gesture or a clear takes the selection back from the keyboard.
    pub(super) fn release(&mut self) {
        self.owner = None;
        self.goal = None;
    }
}

/// A run the caret can stop in, with the part of the window that shows it.
struct Stop {
    run: TextSelectionRun,
    view: Bounds<Pixels>,
}

impl Stop {
    /// The top of its first row and the bottom of its last.
    fn rows(&self) -> (Pixels, Pixels) {
        text_rows_extent(&self.run.layout, self.run.layout.line_height())
    }

    fn on_row(&self, y: Pixels) -> bool {
        let (top, bottom) = self.rows();
        top <= y && y < bottom
    }

    fn distance_x(&self, x: Pixels) -> Pixels {
        let bounds = self.run.bounds;
        (bounds.left() - x).max(x - bounds.right()).max(px(0.))
    }

    /// The character boundary nearest `point`.
    fn offset_at(&self, point: Point<Pixels>) -> usize {
        let (text, layout) = (&self.run.text, &self.run.layout);
        let (Ok(offset) | Err(offset)) = layout.index_for_position(point);
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        // The index names the character under the point; past its middle,
        // the caret goes after it.
        if let Some(next) = next_boundary(text, offset)
            && let (Some(at), Some(after)) = (
                layout.position_for_index(offset),
                layout.position_for_index(next),
            )
            && after.y == at.y
            && point.x > at.x + (after.x - at.x).half()
        {
            offset = next;
        }
        offset
    }

    /// Where an end at `offset` goes: mid-row, so its row is never in doubt.
    fn point_at(&self, offset: usize) -> Option<Point<Pixels>> {
        let at = self.run.layout.position_for_index(offset)?;
        Some(point(at.x, at.y + self.run.layout.line_height().half()))
    }
}

fn next_boundary(text: &str, offset: usize) -> Option<usize> {
    text[offset..].chars().next().map(|c| offset + c.len_utf8())
}

fn previous_boundary(text: &str, offset: usize) -> Option<usize> {
    text[..offset]
        .chars()
        .next_back()
        .map(|c| offset - c.len_utf8())
}

/// The stop and offset of a caret at `point`: the nearest boundary on its
/// row, or else the end of the last text above it.
fn caret_at(stops: &[Stop], point: Point<Pixels>) -> Option<(usize, usize)> {
    let on_row = (stops.iter().enumerate())
        .filter(|(_, stop)| stop.on_row(point.y))
        .min_by_key(|(_, stop)| stop.distance_x(point.x));
    if let Some((ix, stop)) = on_row {
        return Some((ix, stop.offset_at(point)));
    }
    match stops.iter().rposition(|stop| stop.rows().1 <= point.y) {
        Some(ix) => Some((ix, stops[ix].run.text.len())),
        None => (!stops.is_empty()).then_some((0, 0)),
    }
}

impl WindowSelectionState {
    fn has_keyboard(&self, window: &Window) -> bool {
        (self.keyboard.owner.as_ref()).is_none_or(|owner| owner.is_focused(window))
    }

    /// The runs a caret can stop in, those of the selection's group, in
    /// reading order: down the window, then across. Participants'
    /// document orders are only a tiebreak, as plain text leaves them all 0.
    fn stops(&self, cx: &App) -> Vec<Stop> {
        let mut stops = Vec::new();
        for registration in self.participants.values() {
            let geometry = &registration.registration;
            if geometry.scope != self.active_scope
                || self
                    .fence
                    .is_some_and(|group| geometry.group != Some(group))
            {
                continue;
            }
            let Some(participant) = registration.participant.upgrade() else {
                continue;
            };
            let view = geometry.hitbox.content_mask.bounds;
            stops.extend(
                (participant.read(cx).runs.iter())
                    .filter(|run| !run.text.is_empty() && run.text.len() == run.layout.len())
                    .map(|run| {
                        let place = (run.bounds.top(), run.bounds.left());
                        let order = (place, geometry.document_order, run.document_order);
                        (
                            order,
                            Stop {
                                run: run.clone(),
                                view,
                            },
                        )
                    }),
            );
        }
        stops.sort_by_key(|(order, _)| *order);
        stops.into_iter().map(|(_, stop)| stop).collect()
    }

    /// Moves the selection's moving end, or its both ends for
    /// [`SelectionMotion::All`]. False when there is nowhere to go.
    fn move_caret(&mut self, motion: SelectionMotion, cx: &mut App) -> bool {
        use SelectionMotion::*;
        let stops = self.stops(cx);
        let Some(cursor) = (self.cursor.as_ref()).and_then(|end| end.resolve(&self.participants))
        else {
            return false;
        };
        if motion == All {
            let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
                return false;
            };
            let (Some(start), Some(end)) = (first.point_at(0), last.point_at(last.run.text.len()))
            else {
                return false;
            };
            self.anchor = Some(self.endpoint(start, None, cx));
            return self.place_caret(end, None, cx);
        }
        let Some((ix, offset)) = caret_at(&stops, cursor) else {
            return false;
        };
        let text = &stops[ix].run.text;
        let right = |(ix, offset): (usize, usize)| {
            next_boundary(&stops[ix].run.text, offset)
                .map(|next| (ix, next))
                .or_else(|| (ix + 1 < stops.len()).then_some((ix + 1, 0)))
        };
        let left = |(ix, offset): (usize, usize)| {
            previous_boundary(&stops[ix].run.text, offset)
                .map(|previous| (ix, previous))
                .or_else(|| ix.checked_sub(1).map(|ix| (ix, stops[ix].run.text.len())))
        };
        let row = |x: Pixels| {
            let here = stops[ix].point_at(offset)?;
            Some((ix, stops[ix].offset_at(point(x, here.y))))
        };
        let mut goal = None;
        let target = match motion {
            Right => right((ix, offset)),
            Left => left((ix, offset)),
            WordRight if offset < text.len() => {
                let start = text.len() - text[offset..].trim_start().len();
                Some((
                    ix,
                    word_range_at(text, start).map_or(text.len(), |word| word.end),
                ))
            }
            WordRight => right((ix, offset)),
            WordLeft if offset > 0 => {
                let end = text[..offset].trim_end().len();
                let word = previous_boundary(text, end).and_then(|last| word_range_at(text, last));
                Some((ix, word.map_or(0, |word| word.start)))
            }
            WordLeft => left((ix, offset)),
            LineStart => row(stops[ix].run.bounds.left()),
            LineEnd => row(stops[ix].run.bounds.right()),
            Up | Down => {
                let Some(here) = stops[ix].point_at(offset) else {
                    return false;
                };
                // A run of Up and Down keeps to the x it began at.
                let x = (self.keyboard.goal)
                    .filter(|(_, at)| {
                        (at.x - cursor.x).abs() < px(0.5) && (at.y - cursor.y).abs() < px(0.5)
                    })
                    .map_or(here.x, |(x, _)| x);
                goal = Some(x);
                let line = stops[ix].run.layout.line_height();
                let y = if motion == Down {
                    here.y + line
                } else {
                    here.y - line
                };
                let found = (stops.iter().enumerate())
                    .filter(|(_, stop)| stop.on_row(y))
                    .min_by_key(|(_, stop)| stop.distance_x(x))
                    .map(|(jx, _)| (jx, y))
                    // In the gap between two blocks: the nearest row past it.
                    .or_else(|| {
                        let half = |stop: &Stop| stop.run.layout.line_height().half();
                        let candidates = stops.iter().enumerate();
                        if motion == Down {
                            (candidates.filter(|(_, stop)| stop.rows().0 > here.y))
                                .min_by_key(|(_, stop)| stop.rows().0)
                                .map(|(jx, stop)| (jx, stop.rows().0 + half(stop)))
                        } else {
                            (candidates.filter(|(_, stop)| stop.rows().1 < here.y))
                                .max_by_key(|(_, stop)| stop.rows().1)
                                .map(|(jx, stop)| (jx, stop.rows().1 - half(stop)))
                        }
                    });
                found.map(|(jx, y)| (jx, stops[jx].offset_at(point(x, y))))
            }
            All => unreachable!(),
        };
        let Some(to) = target.and_then(|(jx, offset)| stops[jx].point_at(offset)) else {
            return false;
        };
        self.place_caret(to, goal, cx)
    }

    fn place_caret(&mut self, to: Point<Pixels>, goal: Option<Pixels>, cx: &mut App) -> bool {
        self.cursor = Some(self.endpoint(to, None, cx));
        self.did_hit_text = true;
        self.keyboard.goal = goal.map(|x| (x, to));
        self.keyboard.moved = Some(Instant::now());
        self.publish_snapshots(cx);
        true
    }

    /// Where the caret is, and the part of the window it may be seen in;
    /// only while the selection has the keyboard of an owner that gave it.
    fn caret(&self, window: &Window, cx: &App) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        self.keyboard.owner.as_ref()?;
        if !self.has_keyboard(window) || !self.did_hit_text {
            return None;
        }
        let cursor = self.cursor.as_ref()?.resolve(&self.participants)?;
        let stops = self.stops(cx);
        let (ix, offset) = caret_at(&stops, cursor)?;
        let stop = &stops[ix];
        let at = stop.run.layout.position_for_index(offset)?;
        let caret = Bounds::new(at, size(CARET_WIDTH, stop.run.layout.line_height()));
        Some((caret, stop.view))
    }
}

/// Paints the caret over everything else, after every participant has
/// painted this frame and said where its text is.
pub(super) fn defer_caret(
    state: &Entity<WindowSelectionState>,
    window: &mut Window,
    cx: &mut App,
) {
    let state = state.clone();
    let mut caret = canvas(
        |_, _, _| {},
        move |_, _, window, cx| paint_caret(&state, window, cx),
    )
    .into_any_element();
    caret.layout_as_root(AvailableSpace::min_size(), window, cx);
    window.defer_draw(caret, Point::default(), 0, None);
}

fn paint_caret(state: &Entity<WindowSelectionState>, window: &mut Window, cx: &mut App) {
    let Some((caret, view)) = state.read(cx).caret(window, cx) else {
        state.update(cx, |state, _| state.keyboard.blink = None);
        return;
    };
    let lit_for = (state.read(cx).keyboard.moved).map_or(Duration::ZERO, |moved| moved.elapsed());
    let phase = lit_for.as_millis() % (2 * BLINK.as_millis());
    if phase < BLINK.as_millis() {
        let color = crate::Theme::global(cx).tokens.colors.foreground;
        window.with_content_mask(Some(ContentMask { bounds: view }), |window| {
            window.paint_quad(fill(caret, color));
        });
    }
    // Drawn again at the next blink. The window draws for this entity's
    // notification, having read it this frame.
    let wait = BLINK - Duration::from_millis((phase % BLINK.as_millis()) as u64);
    let weak = state.downgrade();
    let blink = cx.spawn(async move |cx| {
        cx.background_executor().timer(wait).await;
        let _ = weak.update(cx, |_, cx| cx.notify());
    });
    state.update(cx, |state, _| state.keyboard.blink = Some(blink));
}

/// The color selected text is painted in: `color`, or a grey of it while
/// the keyboard is somewhere else.
pub(crate) fn selection_color(color: Hsla, window: &Window, cx: &App) -> Hsla {
    let away = WindowSelectionState::existing(window, cx)
        .is_some_and(|state| !state.read(cx).has_keyboard(window));
    if away { Hsla { s: 0., ..color } } else { color }
}

impl TextSelection {
    /// crusteo: keyboard. Gives the selection the keyboard while `owner` has
    /// focus: it gets a caret and is painted in the selection color, and is
    /// grey while `owner` doesn't. The next gesture or clear takes it back.
    pub fn give_keyboard(owner: &FocusHandle, window: &mut Window, cx: &mut App) {
        if let Some(state) = live_text_selection_state(window, cx) {
            state.update(cx, |state, _| {
                state.keyboard.owner = Some(owner.clone());
                state.keyboard.moved = Some(Instant::now());
            });
        }
    }

    /// Moves the selection's moving end within its group, as Shift with an
    /// arrow key does in a text box. False when there is nowhere to go.
    pub fn move_caret(motion: SelectionMotion, window: &mut Window, cx: &mut App) -> bool {
        let Some(state) = live_text_selection_state(window, cx) else {
            return false;
        };
        let moved = state.update(cx, |state, cx| state.move_caret(motion, cx));
        if moved {
            WindowSelectionState::resolve_content_keys(&state, cx);
            window.refresh();
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, Modifiers, MouseDownEvent, MouseUpEvent, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, div,
    };

    use super::*;
    use crate::SelectableText;

    struct TwoParagraphs;

    impl Render for TwoParagraphs {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(TextSelectionLayer)
                .child(div().w(px(400.)).child(SelectableText::new("one", "alpha beta gamma")))
                .child(div().w(px(400.)).mt(px(8.)).child(SelectableText::new("two", "delta epsilon")))
        }
    }

    /// A window with the two paragraphs drawn, and "alpha" double-clicked.
    fn alpha_selected(cx: &mut TestAppContext) -> &mut VisualTestContext {
        let (_, cx) = cx.add_window_view(|_, _| TwoParagraphs);
        let position = cx.update(|window, cx| {
            let _ = window.draw(cx);
            let state = WindowSelectionState::existing(window, cx).unwrap();
            state.read(cx).stops(cx)[0].point_at(1).unwrap()
        });
        let modifiers = Modifiers::default();
        let button = MouseButton::Left;
        cx.simulate_event(MouseDownEvent { position, modifiers, button, click_count: 2, first_mouse: false });
        cx.simulate_event(MouseUpEvent { position, modifiers, button, click_count: 2 });
        assert_eq!(selected(cx), "alpha");
        cx
    }

    fn selected(cx: &mut VisualTestContext) -> String {
        cx.update(|window, cx| {
            let _ = window.draw(cx);
            TextSelection::selected_text(window, cx)
        })
    }

    fn press(motion: SelectionMotion, cx: &mut VisualTestContext) -> String {
        cx.update(|window, cx| assert!(TextSelection::move_caret(motion, window, cx)));
        selected(cx)
    }

    #[gpui::test]
    fn keys_move_the_moving_end_by_character_word_row_and_paragraph(cx: &mut TestAppContext) {
        use SelectionMotion::*;
        let cx = alpha_selected(cx);
        assert_eq!(press(Right, cx), "alpha ");
        assert_eq!(press(WordRight, cx), "alpha beta");
        assert_eq!(press(WordLeft, cx), "alpha ");
        assert_eq!(press(LineEnd, cx), "alpha beta gamma");
        // Past the end of one paragraph, on into the next.
        assert_eq!(press(Right, cx), "alpha beta gamma");
        assert_eq!(press(Right, cx), "alpha beta gamma\nd");
        // Up keeps to where it was across: one character in.
        assert_eq!(press(Up, cx), "a");
        assert_eq!(press(Down, cx), "alpha beta gamma\nd");
        assert_eq!(press(LineStart, cx), "alpha beta gamma");
        assert_eq!(press(All, cx), "alpha beta gamma\ndelta epsilon");
        cx.update(|window, cx| assert!(!TextSelection::move_caret(Down, window, cx)));
    }

    #[gpui::test]
    fn the_caret_and_color_follow_the_owner_of_the_keyboard(cx: &mut TestAppContext) {
        let cx = alpha_selected(cx);
        cx.update(|window, cx| {
            let state = WindowSelectionState::existing(window, cx).unwrap();
            let color = crate::Theme::global(cx).tokens.colors.selection;
            let grey = Hsla { s: 0., ..color };
            // Not given the keyboard: no caret, the usual color.
            assert!(state.read(cx).caret(window, cx).is_none());
            assert_eq!(selection_color(color, window, cx), color);

            let owner = cx.focus_handle();
            TextSelection::give_keyboard(&owner, window, cx);
            assert!(state.read(cx).caret(window, cx).is_none());
            assert_eq!(selection_color(color, window, cx), grey);

            owner.focus(window, cx);
            assert!(state.read(cx).caret(window, cx).is_some());
            assert_eq!(selection_color(color, window, cx), color);

            // A clear takes the keyboard back.
            TextSelection::clear(window, cx);
            assert!(state.read(cx).keyboard.owner.is_none());
        });
    }
}
