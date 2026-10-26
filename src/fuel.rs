//! Title-bar fuel gauges and the composer tint that reads the same snapshot.
//! What each gauge shows is [`usage_source::view`]'s, the same rows the
//! hub sends phones.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement, PathBuilder, SharedString, Styled, canvas,
    div, point, px, rgb,
};
use gpui_kit::prelude::FluentBuilder;
use usage_source::view::{self, Row};
use usage_source::{Eligibility, FuelBand, Severity, UsageSnapshot, Window};

use crate::app::Core;
use crate::theme::{palette, size as sz, with_alpha};
use crate::ui;

pub fn gauges(core: &Core, cx: &App) -> AnyElement {
    let now = usage_source::unix_now();
    let (rows, eligibility) = {
        let usage = core.usage.read(cx);
        (usage.rows(now), usage.eligibility.clone())
    };
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .children(rows.iter().map(|row| gauge(core, row, &eligibility, now)))
        .into_any_element()
}

/// Composer footer color for an agent's provider. None when that provider has no pool.
pub fn label_tint(provider: &str, snapshot: Option<&UsageSnapshot>) -> Option<Hsla> {
    let snapshot = snapshot?;
    let now = usage_source::unix_now();
    let pool = usage_source::worst_pool(&snapshot.pools, provider)?;
    let window = usage_source::binding_window(pool)?;
    let stale = usage_source::is_stale(snapshot.served_at_unix, now);
    Some(band_color(
        usage_source::fuel_band(window.remaining_percent),
        stale,
    ))
}

/// One subscription's meter: a pace dial
/// for its most constraining window and that window's slack, `-16` when it
/// burns ahead of the clock, `+4` when quota is to spare.
fn gauge(core: &Core, row: &Row, eligibility: &[Eligibility], now: i64) -> impl IntoElement {
    let p = palette();
    let pool = &row.pool;
    let stale = row.stale;
    let color = with_alpha_hsla(severity_color(row.severity), if stale { 0.45 } else { 1. });
    let slack = row.slack.clone();
    let tip = row.tip.clone();
    let windows = pool.windows.clone();
    let reason = eligibility
        .iter()
        .find(|item| item.pool == pool.id)
        .and_then(|item| item.reasons.first().cloned());
    let id = SharedString::from(format!("fuel-{}", pool.id));
    let needle = row.needle;
    // A read that failed, as Paseo's card marks it: a red dot for an
    // error, a muted one for an account unavailable, its numbers kept.
    let failed = (pool.failure.as_ref())
        .filter(|_| row.retained)
        .map(|failure| match failure.status.as_str() {
            "error" => p.status_danger,
            _ => p.muted,
        });
    let failure = (row.retained).then(|| {
        (
            view::failure_text(pool),
            view::last_updated(row.served_at_unix, now),
        )
    });

    let button = Button::new(id)
        .ghost()
        .xsmall()
        .px(px(5.))
        .gap(px(4.))
        .text_color(p.muted)
        .when(stale, |button| button.opacity(0.45))
        .tooltip(tip)
        .child(ui::icon(provider_icon(&pool.provider), 12., p.muted))
        .when_some(row.account.clone(), |button, account| {
            button.child(div().text_size(sz::sm()).child(account))
        })
        .child(dial(needle, color))
        .child(
            div()
                .min_w(px(22.))
                .text_size(sz::sm())
                .text_color(color)
                .child(slack),
        )
        .when_some(failed, |button, color| {
            button.child(div().size(px(6.)).rounded_full().bg(color))
        })
        .dropdown_menu(move |menu, _, _| {
            window_menu(menu, &windows, now, stale, failure.clone(), reason.clone())
        });

    // A stale snapshot must not invite a launch against numbers we no longer trust.
    let burn = row.use_before.clone();
    let provider = pool.provider.clone();
    let core = core.clone();
    div()
        .flex()
        .items_center()
        .gap(px(2.))
        .child(button)
        .when_some(burn, |row, window| {
            row.child(use_chip(core, &pool.id, &provider, &window, now))
        })
}

/// A half-dial, green on the left through blue at the top to red on the
/// right, with the needle at the window's pace. No needle when the window
/// has no rate to show.
fn dial(needle: Option<f64>, color: Hsla) -> impl IntoElement {
    const W: f32 = 26.;
    const H: f32 = 14.;
    let p = palette();
    let bands = [
        (0., 74., p.status_success),
        (74., 106., rgb(0x79c0ff)),
        (106., 140., p.status_warning),
        (140., 180., p.status_danger),
    ];
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = point(bounds.origin.x + px(W / 2.), bounds.origin.y + px(H));
            // From the left, clockwise over the top: 0 is due left, 180 due right.
            let at = |degrees: f32, radius: f32| {
                let theta = (180. - degrees).to_radians();
                point(
                    center.x + px(radius * theta.cos()),
                    center.y - px(radius * theta.sin()),
                )
            };
            for (from, to, band) in bands {
                let mut path = PathBuilder::stroke(px(3.));
                let steps = 8;
                path.move_to(at(from, H - 2.));
                for step in 1..=steps {
                    let degrees = from + (to - from) * step as f32 / steps as f32;
                    path.line_to(at(degrees, H - 2.));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, with_alpha(band, 0.55));
                }
            }
            if let Some(needle) = needle {
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(center);
                path.line_to(at(90. + needle as f32, H - 1.));
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .w(px(W))
    .h(px(H))
    .flex_shrink_0()
}

fn severity_color(severity: Severity) -> Hsla {
    let p = palette();
    match severity {
        Severity::Critical => p.status_danger.into(),
        Severity::Warn => p.status_warning.into(),
        Severity::OnPace => rgb(0x79c0ff).into(),
        Severity::Surplus => p.status_success.into(),
        Severity::Unknown => p.muted.into(),
    }
}

fn with_alpha_hsla(mut color: Hsla, alpha: f32) -> Hsla {
    color.a *= alpha;
    color
}

/// The gauge's windows, then why its read failed and when it last worked
/// (when its numbers are retained), or that they are stale.
fn window_menu(
    mut menu: gpui_kit::component::menu::PopupMenu,
    windows: &[Window],
    now: i64,
    stale: bool,
    failure: Option<(Option<String>, String)>,
    reason: Option<String>,
) -> gpui_kit::component::menu::PopupMenu {
    let retained = failure.is_some();
    for window in windows {
        menu = menu.item(PopupMenuItem::label(view::describe_window(
            window, now, retained,
        )));
    }
    if let Some((failure, updated)) = failure {
        menu = menu.separator();
        for line in failure.iter().flat_map(|text| text.lines()) {
            menu = menu.item(PopupMenuItem::label(line.to_string()));
        }
        menu = menu.item(PopupMenuItem::label(updated));
    } else if stale {
        menu = menu.separator().item(PopupMenuItem::label(view::STALE));
    }
    if let Some(reason) = reason {
        menu = menu.separator().item(PopupMenuItem::label(reason));
    }
    menu
}

fn use_chip(
    core: Core,
    pool_id: &str,
    provider: &str,
    window: &Window,
    now: i64,
) -> impl IntoElement {
    let p = palette();
    let explain = view::use_before_text(window, now);
    let provider = provider.to_string();
    let id = SharedString::from(format!("fuel-use-{pool_id}"));
    Button::new(id)
        .ghost()
        .xsmall()
        .label("use")
        .text_color(p.accent_bright)
        .bg(with_alpha(p.accent, 0.28))
        .rounded(px(8.))
        .px(px(6.))
        .tooltip("Use it before reset")
        .dropdown_menu(move |menu, _, _| {
            let launch = PopupMenuItem::new(format!("New agent on {provider}")).on_click({
                let (core, provider) = (core.clone(), provider.clone());
                move |_, window, cx| {
                    crate::layouts::paseo::open_new_agent(
                        &core,
                        Some(provider.clone()),
                        window,
                        cx,
                    );
                }
            });
            menu.item(PopupMenuItem::label(explain.clone()))
                .item(PopupMenuItem::label(
                    "Opens New agent. Nothing starts until you create it.",
                ))
                .separator()
                .item(launch)
        })
}

fn provider_icon(provider: &str) -> &'static str {
    match provider {
        "claude" => "bot",
        // Each provider's own mark, as near as the icon set comes.
        "codex" => "aperture",
        "gemini" => "sparkle",
        "cursor" => "mouse-pointer",
        _ => "activity",
    }
}

fn band_color(band: FuelBand, stale: bool) -> Hsla {
    let p = palette();
    let color = match band {
        FuelBand::Green => p.status_success,
        FuelBand::Amber => p.status_warning,
        FuelBand::Red => p.status_danger,
    };
    with_alpha(color, if stale { 0.45 } else { 1. })
}
