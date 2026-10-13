//! The help panel: what the keys do, and where to make crusteo your own.
//! The "?" item in the title bar opens it, as do F1 and Ctrl+/ (Cmd+/).

use std::rc::Rc;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::gpui::{
    AnyElement, App, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px, rgba,
};

use crate::theme::{palette, size};

/// The modifier the app's shortcuts use: Cmd on macOS, Ctrl elsewhere.
pub fn primary() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    }
}

/// Keys pressed together, and what they do.
type Row = (&'static [&'static str], &'static str);

/// A titled group of [`Row`]s.
type Section = (&'static str, &'static [Row]);

/// What a button in the panel does.
pub type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

/// Keys and what they do, by where they work. `Mod` stands for
/// [`primary`].
const SECTIONS: &[Section] = &[
    (
        "Everywhere",
        &[
            (&["Mod", "K"], "Command center: find any agent or workspace"),
            (&["Mod", "Shift", "L"], "Next layout"),
            (&["Mod", "T"], "New agent tab"),
            (&["Mod", "Shift", "T"], "New terminal"),
            (&["Mod", "W"], "Close tab"),
            (&["Mod", "N"], "New workspace"),
            (&["Mod", "]"], "Next workspace"),
            (&["Mod", "["], "Previous workspace"),
            (&["Mod", "1…9"], "Go to a workspace in the sidebar"),
            (&["Mod", "L"], "Focus the composer"),
            (&["Mod", "B"], "Show or hide the sidebar"),
            (&["Mod", ","], "Settings"),
            (&["Mod", "="], "Zoom in"),
            (&["Mod", "-"], "Zoom out"),
            (&["Mod", "0"], "Actual size"),
            (&["F1"], "This help"),
        ],
    ),
    (
        "Lanes",
        &[
            (&["←", "→"], "Move between lanes"),
            (&["↑", "↓"], "Move between cards in a lane"),
            (&["Shift", "← →"], "Scroll by a lane"),
            (&["Enter"], "Open the card, terminal or fold"),
            (&["F"], "Focus mode: the card large, the rest dimmed"),
            (&["Esc"], "Leave focus mode, or collapse the card"),
            (&["Mod", "Wheel"], "Make lanes wider or narrower"),
            (&["Middle-drag"], "Pan the board"),
        ],
    ),
    (
        "Terminals",
        &[
            (&["Mod", "Shift", "C"], "Copy"),
            (&["Mod", "Shift", "V"], "Paste"),
        ],
    ),
    (
        "Toolbars",
        &[
            (&["Right-click"], "On any bar: customize the toolbars"),
            (&["Esc"], "Done customizing"),
        ],
    ),
];

fn key_chip(key: &str) -> AnyElement {
    let p = palette();
    let key = if key == "Mod" { primary() } else { key };
    div()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(p.border)
        .bg(p.surface2)
        .text_size(px(11.))
        .text_color(p.foreground)
        .child(key.to_string())
        .into_any_element()
}

/// A key in GPUI's form, such as `"secondary-shift-i"`, as the panel shows
/// it: `Mod`, `Shift`, `I`.
pub fn key_parts(key: &str) -> Vec<String> {
    let (mods, last) = match key.rsplit_once('-') {
        // "secondary--" is Mod and the minus key.
        Some((mods, "")) => (mods.trim_end_matches('-'), "-"),
        Some((mods, last)) => (mods, last),
        None => ("", key),
    };
    let mut parts: Vec<String> = mods
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| match part {
            "secondary" => "Mod".to_string(),
            "cmd" => "Cmd".to_string(),
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            other => other.to_string(),
        })
        .collect();
    parts.push(if last.chars().count() == 1 {
        last.to_uppercase()
    } else {
        let mut chars = last.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default()
    });
    parts
}

fn section(title: &str, rows: &[(Vec<String>, String)]) -> AnyElement {
    let p = palette();
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .text_size(size::sm())
                .text_color(p.muted)
                .child(title.to_string()),
        )
        .children(rows.iter().map(|(keys, what)| {
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(124.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(3.))
                        .children(keys.iter().map(|key| key_chip(key))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(size::sm())
                        .text_color(p.foreground)
                        .child(what.to_string()),
                )
        }))
        .into_any_element()
}

/// What the panel's buttons do, and the rows extensions add; the shell
/// supplies them.
pub struct Actions {
    pub close: Handler,
    pub customize: Handler,
    /// Extensions' commands that have keys: the keys, and what they do.
    pub extra: Vec<(Vec<String>, String)>,
}

/// The panel over everything, closed by clicking outside it or Escape.
pub fn panel(actions: Actions) -> AnyElement {
    let p = palette();
    let Actions {
        close,
        customize,
        extra,
    } = actions;
    let owned = |rows: &[Row]| -> Vec<(Vec<String>, String)> {
        rows.iter()
            .map(|(keys, what)| {
                (
                    keys.iter().map(|key| key.to_string()).collect(),
                    what.to_string(),
                )
            })
            .collect()
    };
    let column = |sections: &[Section], extra: Option<&[(Vec<String>, String)]>| {
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap(px(18.))
            .children(
                sections
                    .iter()
                    .map(|(title, rows)| section(title, &owned(rows))),
            )
            .children(
                extra
                    .filter(|rows| !rows.is_empty())
                    .map(|rows| section("Extensions", rows)),
            )
    };
    let backdrop_close = close.clone();
    let done_close = close.clone();
    let about_close = close.clone();
    div()
        .absolute()
        .inset_0()
        .bg(rgba(0x00000080))
        .flex()
        .justify_center()
        .items_start()
        .pt(px(56.))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            backdrop_close(window, cx)
        })
        .child(
            div()
                .id("help-panel")
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .w(px(760.))
                .max_w(gpui_kit::gpui::relative(0.92))
                .max_h(gpui_kit::gpui::relative(0.85))
                .overflow_y_scroll()
                .p(px(20.))
                .rounded(px(10.))
                .border_1()
                .border_color(p.border)
                .bg(p.surface0)
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(18.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .text_size(px(16.))
                                        .text_color(p.foreground)
                                        .child("Getting around crusteo"),
                                )
                                .child(
                                    div()
                                        .text_size(size::sm())
                                        .text_color(p.muted)
                                        .child(crate::app_menu::version()),
                                ),
                        )
                        .child(
                            Button::new("help-close")
                                .label("Close")
                                .ghost()
                                .small()
                                .on_click(move |_, window, cx| done_close(window, cx)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(28.))
                        // Extensions' rows go under the shorter column, so
                        // the panel's buttons stay in view.
                        .child(column(&SECTIONS[..1], None))
                        .child(column(&SECTIONS[1..], Some(&extra))),
                )
                .child(
                    div()
                        .pt(px(14.))
                        .border_t_1()
                        .border_color(p.border)
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(240.))
                                .text_size(size::sm())
                                .text_color(p.muted)
                                .child(
                                    "Every bar is yours to arrange: move, remove and add \
                                     items, or script your own in Lua.",
                                ),
                        )
                        .child(
                            Button::new("help-customize")
                                .label("Customize toolbars…")
                                .small()
                                .on_click(move |_, window, cx| customize(window, cx)),
                        )
                        .child(
                            Button::new("help-scripts")
                                .label("Open scripts folder")
                                .ghost()
                                .small()
                                .on_click(|_, _, cx| open_scripts_folder(cx)),
                        )
                        // Windows and Linux have no menu bar to hold About.
                        .child(
                            Button::new("help-about")
                                .label("About Crusteo…")
                                .ghost()
                                .small()
                                .on_click(move |_, window, cx| {
                                    about_close(window, cx);
                                    window.dispatch_action(Box::new(crate::app_menu::About), cx);
                                }),
                        ),
                ),
        )
        .into_any_element()
}

/// Opens the folder toolbar scripts load from, making it first if needed.
fn open_scripts_folder(cx: &mut App) {
    let dir = crate::extensions::lua::scripts_dir();
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("crusteo: making {}: {error}", dir.display());
    }
    cx.open_with_system(&dir);
}

#[cfg(test)]
mod tests {
    use super::key_parts;

    #[test]
    fn keys_read_as_the_panel_shows_them() {
        assert_eq!(key_parts("secondary-i"), ["Mod", "I"]);
        assert_eq!(key_parts("secondary-shift-k"), ["Mod", "Shift", "K"]);
        assert_eq!(key_parts("secondary--"), ["Mod", "-"]);
        assert_eq!(key_parts("f1"), ["F1"]);
        assert_eq!(key_parts("alt-enter"), ["Alt", "Enter"]);
    }
}
