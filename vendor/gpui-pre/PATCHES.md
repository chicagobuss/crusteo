# gpui-pre, patched for crusteo

A copy of `gpui-pre` 0.3.7 (a snapshot of Zed's GPUI at zed@1a28cff) with the
changes below, wired in through `[patch.crates-io]` in the workspace manifest.
The examples and tests of the published crate are left out. Every change is
marked with a `crusteo:` comment.

To move to a newer `gpui-pre`, copy its `src`, `docs`, `resources` and
`build.rs` over this one, strip the `[[example]]`/`[[test]]` targets from its
`Cargo.toml`, and reapply these changes.

## A window that only moved does not redraw

`Window::bounds_changed` (window.rs) ran on every `WM_MOVE` and called
`refresh()`, which redraws every view with every cache ignored, for each step
of a window drag. It now refreshes only when the window's size or scale
changed. A move changes nothing drawn: the system compositor places the
window.

## Cached views can be reused where they moved

A cached view was reused only at exactly the bounds it was drawn at, so
scrolling the Lanes board sideways redrew every visible column every frame.
A view can now opt in with `ViewElement::relocatable(true)`
(`AnyView::cached(style).relocatable(true)`):

- It is recorded in its own space (`Window::with_local_recording`): inside it
  the clip starts at its own bounds, not at its ancestors' clip. That clip
  becomes a display clip which what is drawn (`Scene::display_clip`) and
  hit-tested (`Hitbox::content_mask`) is cut to, while the scene's paint
  operations and `Frame::hitbox_local_masks` keep the clips as recorded.
- When only its position changed, its recorded hitboxes, primitives, layers
  and clips are replayed moved by the difference
  (`Window::reuse_prepaint_shifted`, `Window::reuse_paint_shifted`,
  `Scene::replay_shifted`), then cut to the current display clip.
- It is drawn afresh instead when the move is not a whole number of device
  pixels (glyphs are rasterized for their subpixel offset), or it recorded
  tooltips, deferred draws, text input handlers, rotated or scaled sprites,
  or platform surfaces (`Window::can_relocate`). Local recordings do not
  nest.

Only opt in a view whose elements do not remember where they were drawn:
element state or closures holding absolute bounds from prepaint (a canvas
recording its bounds, list or scroll handles asked for window positions)
would be off by the move until the view next redraws.

Relocation does not rewrite positions captured by event-listener closures.
Do not opt in a view with drag-start offsets or tooltip source bounds stored
in such closures. The runtime guard detects registered tooltip draws, but
cannot discover every position a caller retained. Such interactions must
disable relocation or resolve geometry from the current frame. Hover styling is
re-evaluated when its input changes; a moved view must be notified if the
move itself changes state that its drawing depends on.


## Replayed views remain reactive

Cached view prepaint records the view's own entity as accessed even when
replaying its drawing. The window's live-entity tracking otherwise forgets
the view on a cached frame, so a later list scroll or observer notification
does not invalidate its drawing. Native Mac QA reproduced an inert Fleet
list with incoming fe421fe and verifies wheel scrolling after this patch.

This fixes tracking the cached view itself. The upstream accessed-entity
collector still subtracts entities already read by an ancestor from a child
view's cached dependency set. Dependencies read by both parent and child can
therefore also be missing. Current streamed lane previews use explicit
observers, rather than relying on that collector; future cached views need
observers for such shared dependencies until the collector tracks per-view
accesses independently.


## Window zoom

`Window::set_zoom` scales everything a window draws, as a browser's Ctrl+/-
does. The zoom multiplies the platform's scale factor and divides its content
size, so layout, text rasterization and hit testing all see a smaller logical
viewport drawn at a larger scale, and text stays crisp. `bounds_changed`
applies it on every resize or display change. Platform input is divided by the
zoom on entry (`unzoom_input`): mouse, wheel (and pixel scroll deltas), pinch,
pressure and file-drop positions, and the window's tracked mouse position.
`show_window_menu` multiplies its position back out.

Not converted: IME candidate bounds the input handler reports to the platform
(`bounds_for_range`, `update_ime_position`). With a zoom other than 1, an IME
composition window can open offset from the caret.