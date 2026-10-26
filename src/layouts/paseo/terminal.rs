//! A terminal tab: the cell grid for one daemon PTY.

use fleet::HostId;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, Focusable, Font, FontFeatures, FontStyle,
    FontWeight, Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, Styled, TextAlign, TextRun, UnderlineStyle, Window, canvas,
    div, fill, point, px, rgb,
};
use gpui_kit::prelude::FluentBuilder;
use term::{BufferPoint, PaintColor, Snapshot, SnapshotCell};

use crate::app::{Core, TerminalSession};
use crate::theme::{self, palette, size};

gpui_kit::actions!(terminal, [Copy, Paste]);

pub fn bind_keys(cx: &mut App) {
    use gpui_kit::gpui::{KeyBinding, NoAction};
    let terminal = Some("Terminal");
    // The root binds Tab to move focus. In a terminal Tab completes, so the
    // keystroke goes to the shell instead.
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, terminal),
        KeyBinding::new("shift-tab", NoAction, terminal),
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("secondary-c", Copy, terminal),
            KeyBinding::new("secondary-v", Paste, terminal),
        ]);
    } else {
        cx.bind_keys([
            KeyBinding::new("ctrl-shift-c", Copy, terminal),
            KeyBinding::new("ctrl-shift-v", Paste, terminal),
        ]);
    }
}

#[derive(Clone, Copy)]
struct Metrics {
    origin: Point<Pixels>,
    cell_w: Pixels,
    cell_h: Pixels,
}

pub struct TerminalView {
    core: Core,
    host: HostId,
    terminal_id: String,
    session: Entity<TerminalSession>,
    focus: FocusHandle,
    metrics: Rc<Cell<Metrics>>,
    rows: u32,
    cols: u32,
    resize_gen: u64,
    selecting: bool,
    anchor: Option<BufferPoint>,
    /// The cursor is in the lit half of its blink.
    blink_on: bool,
    /// The blink's timer is running, as it does while the terminal has focus.
    blinking: bool,
    /// When a key was last typed or output last came; the cursor stays lit
    /// for a blink after either.
    last_activity: Instant,
}

/// How long the cursor stays lit, then dark: a slow blink.
const BLINK: Duration = Duration::from_millis(800);

impl TerminalView {
    pub fn new(
        core: Core,
        host: &HostId,
        terminal_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session = core
            .store
            .update(cx, |store, cx| store.open_terminal(host, terminal_id, cx));
        cx.observe(&session, |this, _, cx| {
            this.wake_cursor();
            cx.notify();
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            core,
            host: host.clone(),
            terminal_id: terminal_id.to_string(),
            session,
            focus,
            metrics: Rc::new(Cell::new(Metrics {
                origin: point(px(0.), px(0.)),
                cell_w: px(7.2),
                cell_h: px(16.),
            })),
            rows: 0,
            cols: 0,
            resize_gen: 0,
            selecting: false,
            anchor: None,
            blink_on: true,
            blinking: false,
            last_activity: Instant::now(),
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Lights the cursor and holds it lit for a blink, as typing or output
    /// does, so it never goes dark under the keys.
    fn wake_cursor(&mut self) {
        self.blink_on = true;
        self.last_activity = Instant::now();
    }

    /// Blinks the cursor while the terminal has focus. The timer stops, the
    /// cursor lit, once focus leaves; rendering starts it again on return.
    fn start_blinking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blinking {
            return;
        }
        self.blinking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let going = this
                    .update_in(cx, |view, window, cx| {
                        if !view.focus.is_focused(window) {
                            view.blinking = false;
                            view.blink_on = true;
                            cx.notify();
                            return false;
                        }
                        let lit = view.last_activity.elapsed() < BLINK || !view.blink_on;
                        if lit != view.blink_on {
                            view.blink_on = lit;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !going {
                    break;
                }
            }
        })
        .detach();
    }

    fn note_size(&mut self, rows: u32, cols: u32, cx: &mut Context<Self>) {
        if rows < 2 || cols < 2 || (rows == self.rows && cols == self.cols) {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.resize_gen += 1;
        let generation = self.resize_gen;
        let host = self.host.clone();
        let id = self.terminal_id.clone();
        let store = self.core.store.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(60))
                .await;
            let current = this
                .update(cx, |view, _| view.resize_gen == generation)
                .unwrap_or(false);
            if !current {
                return;
            }
            store.update(cx, |store, cx| {
                store.terminal_resize(&host, &id, rows, cols, cx)
            });
        })
        .detach();
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let metrics = self.metrics.get();
        let x = f32::from(position.x - metrics.origin.x);
        let y = f32::from(position.y - metrics.origin.y);
        if x < 0. || y < 0. || f32::from(metrics.cell_w) <= 0. || f32::from(metrics.cell_h) <= 0. {
            return None;
        }
        let col = (x / f32::from(metrics.cell_w)) as usize;
        let row = (y / f32::from(metrics.cell_h)) as usize;
        Some((row, col))
    }

    fn point_at(&self, position: Point<Pixels>, cx: &App) -> Option<BufferPoint> {
        let (row, col) = self.cell_at(position)?;
        let session = self.session.read(cx);
        let rows = session.emulator.rows();
        let cols = session.emulator.cols();
        if row >= rows {
            return None;
        }
        Some(
            session
                .emulator
                .buffer_point(row, col.min(cols.saturating_sub(1))),
        )
    }

    fn select_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(anchor) = self.anchor else {
            return;
        };
        let Some(focus) = self.point_at(position, cx) else {
            return;
        };
        self.session.update(cx, |session, cx| {
            session.emulator.select(anchor, focus);
            cx.notify();
        });
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.session.read(cx).emulator.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let bytes = self.session.read(cx).emulator.encode_paste(&text);
        let Ok(data) = String::from_utf8(bytes) else {
            return;
        };
        self.core
            .store
            .read(cx)
            .terminal_write(&self.host, &self.terminal_id, &data);
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        self.wake_cursor();
        let stroke = &event.keystroke;
        if stroke.key == "escape" && self.core.nav.read(cx).command_open {
            return;
        }
        if matches!(
            stroke.key.as_str(),
            "control" | "alt" | "shift" | "function" | "platform" | "meta"
        ) {
            return;
        }
        // Cmd shortcuts stay with the app. On Linux, Ctrl+Shift stays with the
        // app too (those bindings win before this handler).
        if stroke.modifiers.platform {
            return;
        }
        let name = if stroke.modifiers.control || stroke.modifiers.alt {
            stroke.key.to_string()
        } else if stroke.key.chars().count() == 1 {
            stroke
                .key_char
                .as_ref()
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| stroke.key.to_string())
        } else {
            stroke.key.to_string()
        };
        let key = term::Key {
            key: name,
            ctrl: stroke.modifiers.control,
            alt: stroke.modifiers.alt,
            shift: stroke.modifiers.shift,
        };
        let bytes = self.session.read(cx).emulator.encode(&key);
        let Ok(data) = String::from_utf8(bytes) else {
            return;
        };
        if data.is_empty() {
            return;
        }
        self.core
            .store
            .read(cx)
            .terminal_write(&self.host, &self.terminal_id, &data);
        cx.stop_propagation();
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let session = self.session.read(cx);
        let snapshot = session.emulator.snapshot();
        let selection = session.emulator.selection();
        let status = if let Some(error) = session.error.clone() {
            Some(error)
        } else if session.offline {
            Some("Reconnecting…".to_string())
        } else if session.attaching {
            Some("Attaching…".to_string())
        } else {
            None
        };
        let metrics = self.metrics.clone();
        let entity = cx.entity().clone();
        let focused = self.focus.is_focused(window);
        // A slow blink while focused; a steady block otherwise.
        if focused {
            self.start_blinking(window, cx);
        }
        let show_cursor = !focused || self.blink_on;
        let grid = canvas(
            move |bounds, window, app| {
                let (cell_w, cell_h) = cell_size(window);
                metrics.set(Metrics {
                    origin: bounds.origin,
                    cell_w,
                    cell_h,
                });
                let cols = (f32::from(bounds.size.width) / f32::from(cell_w)).floor() as u32;
                let rows = (f32::from(bounds.size.height) / f32::from(cell_h)).floor() as u32;
                let entity = entity.clone();
                app.defer(move |cx| {
                    entity.update(cx, |view, cx| view.note_size(rows, cols, cx));
                });
                shape_snapshot(&snapshot, selection, show_cursor, window)
            },
            move |bounds, lines, window, app| {
                window.paint_quad(fill(bounds, p.surface0));
                let (_, cell_h) = cell_size(window);
                for (index, line) in lines.into_iter().enumerate() {
                    let origin = point(bounds.origin.x, bounds.origin.y + cell_h * index as f32);
                    // Backgrounds first: programs' colors, the selection and
                    // the block cursor. `paint` draws only the glyphs.
                    let _ =
                        line.paint_background(origin, cell_h, TextAlign::Left, None, window, app);
                    let _ = line.paint(origin, cell_h, TextAlign::Left, None, window, app);
                }
            },
        )
        .size_full();

        div()
            .id(SharedString::from(format!("terminal-{}", self.terminal_id)))
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface0)
            .font_family(theme::mono_font())
            .when(focused, |d| d.border_1().border_color(p.accent))
            .on_action(cx.listener(|this, _: &Copy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| this.paste(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key(event, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus(window, cx);
                    this.selecting = true;
                    this.anchor = this.point_at(event.position, cx);
                    if let Some(anchor) = this.anchor {
                        this.session.update(cx, |session, cx| {
                            session.emulator.select(anchor, anchor);
                            cx.notify();
                        });
                    }
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if this.selecting {
                    this.select_to(event.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if this.selecting {
                        this.select_to(event.position, cx);
                    }
                    this.selecting = false;
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let metrics = this.metrics.get();
                let lines = match event.delta {
                    ScrollDelta::Lines(delta) => delta.y,
                    ScrollDelta::Pixels(delta) => f32::from(delta.y) / f32::from(metrics.cell_h),
                };
                let steps = lines.round() as i32;
                if steps != 0 {
                    this.session.update(cx, |session, cx| {
                        session.emulator.scroll(steps);
                        cx.notify();
                    });
                }
                cx.stop_propagation();
            }))
            .when_some(status, |d, status| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .px(px(8.))
                        .py(px(4.))
                        .text_size(size::sm())
                        .bg(p.surface2)
                        .text_color(p.status_warning)
                        .child(status),
                )
            })
            .child(div().flex_1().min_h(px(0.)).child(grid))
    }
}

fn cell_size(window: &Window) -> (Pixels, Pixels) {
    let font = mono_font(false, false);
    let font_id = window.text_system().resolve_font(&font);
    let cell_w = window
        .text_system()
        .advance(font_id, size::code(), 'm')
        .map(|size| size.width)
        .unwrap_or(px(7.2));
    let cell_h = window
        .text_system()
        .bounding_box(font_id, size::code())
        .size
        .height
        .max(size::code());
    (cell_w, cell_h)
}

fn mono_font(bold: bool, italic: bool) -> Font {
    Font {
        family: theme::mono_font().into(),
        features: FontFeatures::default(),
        fallbacks: None,
        weight: if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

/// The screen's lines, shaped to paint, with the cursor drawn when
/// `show_cursor`, as it is in the lit half of its blink.
fn shape_snapshot(
    snapshot: &Snapshot,
    selection: Option<(BufferPoint, BufferPoint)>,
    show_cursor: bool,
    window: &mut Window,
) -> Vec<gpui_kit::gpui::ShapedLine> {
    snapshot
        .lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let buffer_line = snapshot
                .scrollback
                .saturating_sub(snapshot.scroll_offset)
                .saturating_add(row);
            let selected = term::selection_cols(selection, buffer_line);
            let cursor_col = snapshot.cursor.and_then(|cursor| {
                (show_cursor && cursor.row == row && !cursor.hidden).then_some(cursor.col)
            });
            let runs = style_runs(&line.cells, selected.as_ref(), cursor_col);
            let text: String = runs.iter().map(|run| run.text.as_str()).collect();
            let text_runs: Vec<TextRun> = runs
                .iter()
                .map(|run| TextRun {
                    len: run.text.len(),
                    font: mono_font(run.bold, run.italic),
                    color: run.fg,
                    background_color: Some(run.bg),
                    underline: run.underline.then(|| UnderlineStyle {
                        thickness: px(1.),
                        color: Some(run.fg),
                        wavy: false,
                    }),
                    strikethrough: run.strike.then(|| gpui_kit::gpui::StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(run.fg),
                    }),
                })
                .collect();
            window.text_system().shape_line(
                SharedString::from(text),
                size::code(),
                &text_runs,
                None,
            )
        })
        .collect()
}

struct StyleRun {
    text: String,
    fg: Hsla,
    bg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

/// `cells` as runs of one style each. The cursor, at column `cursor`, is
/// always a block, its cell's colors swapped, whatever shape the program
/// asked for: a bar or an underline is hard to find at a glance.
fn style_runs(
    cells: &[SnapshotCell],
    selected: Option<&std::ops::RangeInclusive<usize>>,
    cursor: Option<usize>,
) -> Vec<StyleRun> {
    let mut runs = Vec::new();
    let mut col = 0usize;
    for cell in cells {
        let mut style = colors(&cell.style);
        let in_selection = selected
            .is_some_and(|range| (col..=col + cell.width as usize - 1).any(|c| range.contains(&c)));
        if in_selection {
            style.1 = rgba_hsla(rgb(0xffffff), 0.2);
        }
        let on_cursor = cursor
            .is_some_and(|cursor_col| cursor_col >= col && cursor_col < col + cell.width as usize);
        if on_cursor {
            std::mem::swap(&mut style.0, &mut style.1);
        }
        let same = runs.last().is_some_and(|run: &StyleRun| {
            run.fg == style.0
                && run.bg == style.1
                && run.bold == cell.style.bold
                && run.italic == cell.style.italic
                && run.underline == cell.style.underline
                && run.strike == cell.style.strikethrough
        });
        if same {
            runs.last_mut().unwrap().text.push_str(&cell.text);
        } else {
            runs.push(StyleRun {
                text: cell.text.clone(),
                fg: style.0,
                bg: style.1,
                bold: cell.style.bold,
                italic: cell.style.italic,
                underline: cell.style.underline,
                strike: cell.style.strikethrough,
            });
        }
        col += cell.width as usize;
    }
    if runs.is_empty() {
        runs.push(StyleRun {
            text: String::new(),
            fg: hsla(palette().foreground),
            bg: hsla(palette().surface0),
            bold: false,
            italic: false,
            underline: false,
            strike: false,
        });
    }
    runs
}

fn colors(style: &term::CellStyle) -> (Hsla, Hsla) {
    let mut fg = resolve(style.fg);
    let mut bg = resolve(style.bg);
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if style.dim {
        let mut dimmed: Hsla = fg;
        dimmed.l *= 0.72;
        fg = dimmed;
    }
    (fg, bg)
}

fn resolve(color: PaintColor) -> Hsla {
    let p = palette();
    match color {
        PaintColor::DefaultFg => hsla(p.foreground),
        PaintColor::DefaultBg => hsla(p.surface0),
        PaintColor::Indexed(index) => hsla(theme::ansi(index)),
        PaintColor::Rgb(color) => hsla(gpui_kit::gpui::rgb(
            u32::from(color.r) << 16 | u32::from(color.g) << 8 | u32::from(color.b),
        )),
    }
}

fn hsla(color: gpui_kit::gpui::Rgba) -> Hsla {
    color.into()
}

fn rgba_hsla(color: gpui_kit::gpui::Rgba, alpha: f32) -> Hsla {
    theme::with_alpha(color, alpha)
}
