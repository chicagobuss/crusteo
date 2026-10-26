# gpui-base, patched for crusteo

A copy of `gpui-base` 0.7.0 (the core of `gpui-component` 0.7) with the changes
below, wired in through `[patch.crates-io]` in the workspace manifest. The
benches and tests of the published crate are left out. Every change is marked
with a `crusteo:` comment.

To move to a newer `gpui-base`, copy its `src` over this one, strip the
`[[test]]`/`[[bench]]` targets from its `Cargo.toml`, and reapply these changes.

## Home and End go to the visual line in a wrapped text box

`start_of_line_at` and `end_of_line_at` (input/base/state.rs) went to the start
or end of the visual line only in a code editor. In a soft-wrapped text box,
such as the composer, Home and End went to the ends of the whole paragraph,
which in a narrow Lanes card is several lines away. Every soft-wrapped input
now goes to the visual line first and the paragraph on a second press, as code
editors already did.

## Notifications: how long, how they sit together, never past the window

Toasts (toast.rs), which gpui-component's notifications are, take app-wide
preferences set with `set_toast_preferences`: a layout and the most kept at
once.

- `Stack` keeps the Sonner pile but closer: 6px slivers instead of 14px, five
  shown, so more fit in about the room of one.
- `List` is always spread out. `Newest` keeps one, a new toast dismissing it.
- Spread out, as a list or a stack on hover, a stack shows only the newest
  toasts that fit before the window's edge, from where it was last drawn; the
  rest wait hidden behind the last of them.
- Past the most kept, pushing a toast dismisses the oldest rather than hiding
  it, so a toast that never times out can't pile up out of sight.

gpui-component gives every notification a fixed 5 s. `set_next_toast_timeout`
gives the next toast pushed its own, `None` being until closed, so the app can
time its notifications without changing its other toasts.

## A lost mouse release no longer leaves text selecting

The window's text selection (text_selection.rs) ended a drag only on a left
button release seen in the bubble phase. A release stopped on its way, or let
go outside the window, never arrived, and the drag stayed live: every later
mouse move, with no button held, moved the selection across whatever text the
pointer passed, in every card, and the next press skipped clearing the old
selections. The release is now taken in the capture phase, before anything
can stop it, and a move without the left button ends a drag still running
(a long press held by touch excepted).

## Selection groups: a selection stays in the group it began in

`TextSelectionGroupId` with `ElementExt::text_selection_group` marks a
subtree's text as one group, as `text_selection_scope` marks scopes. A
gesture that starts in a grouped participant only hit-tests that group's
participants: past them, the pointer resolves to the nearest end of the
group, so the selection stops there. Unlike scopes, every group stays
selectable. Text in no group selects across the window as before. crusteo
groups each conversation turn, so a drag never runs on into the next turn or
across other cards.

## Up and Down at the first or last line go to the start or end

`move_vertical` (input/base/movement.rs) kept the cursor where it was when
there was no row above or below. It now goes to the start of the text on the
first row and the end on the last, as text boxes do and as extending a
selection with Shift already did (`vertical_selection_target`).
