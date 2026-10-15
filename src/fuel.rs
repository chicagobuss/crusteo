//! Title-bar fuel gauges and the composer tint that reads the same snapshot.

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement, PathBuilder, SharedString, Styled, canvas,
    div, point, px, rgb,
};
use gpui_kit::prelude::FluentBuilder;
use usage_source::{Eligibility, FuelBand, Pool, Severity, UsageSnapshot, Window};

use crate::app::{Core, Screen};
use crate::theme::{palette, size as sz, with_alpha};
use crate::ui;

pub fn gauges(core: &Core, cx: &App) -> AnyElement {
    let (snapshot, eligibility, served) = {
        let hub = core.usage.read(cx);
        (
            hub.snapshot.clone(),
            hub.eligibility.clone(),
            hub.pool_served.clone(),
        )
    };
    let Some(snapshot) = snapshot else {
        return div().into_any_element();
    };
    let now = usage_source::unix_now();
    let pools = snapshot.pools;
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .children(
            pools
                .iter()
                .filter(|pool| !pool.windows.is_empty())
                .map(|pool| {
                    // Each gauge dims on its own numbers going stale.
                    let served = served
                        .get(&pool.id)
                        .copied()
                        .unwrap_or(snapshot.served_at_unix);
                    let stale = usage_source::is_stale(served, now);
                    gauge(core, pool, &pools, &eligibility, now, stale)
                }),
        )
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
fn gauge(
    core: &Core,
    pool: &Pool,
    pools: &[Pool],
    eligibility: &[Eligibility],
    now: i64,
    stale: bool,
) -> impl IntoElement {
    let p = palette();
    let binding = usage_source::constraining_window(pool, now);
    let pace = binding.and_then(|window| usage_source::pace(window, now));
    let severity = binding
        .map(|window| usage_source::severity(window, now))
        .unwrap_or(Severity::Unknown);
    let color = with_alpha_hsla(severity_color(severity), if stale { 0.45 } else { 1. });
    let slack = pace
        .map(|pace| {
            let rounded = pace.slack.round() as i64;
            if rounded > 0 {
                format!("+{rounded}")
            } else {
                rounded.to_string()
            }
        })
        .unwrap_or_else(|| "—".into());
    let tip = window_tip(pool, binding, now, stale);
    let windows = pool.windows.clone();
    let reason = eligibility
        .iter()
        .find(|item| item.pool == pool.id)
        .and_then(|item| item.reasons.first().cloned());
    let id = SharedString::from(format!("fuel-{}", pool.id));
    let needle = pace.as_ref().and_then(usage_source::needle_degrees);

    let button = Button::new(id)
        .ghost()
        .xsmall()
        .px(px(5.))
        .gap(px(4.))
        .text_color(p.muted)
        .when(stale, |button| button.opacity(0.45))
        .tooltip(tip)
        .child(ui::icon(provider_icon(&pool.provider), 12., p.muted))
        .when_some(account_short(pool, pools), |button, account| {
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
        .dropdown_menu(move |menu, _, _| window_menu(menu, &windows, now, stale, reason.clone()));

    // A stale snapshot must not invite a launch against numbers we no longer trust.
    let burn = if stale {
        None
    } else {
        usage_source::use_before_reset(pool, now).cloned()
    };
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

/// Which account a meter is, when its provider has more than one: the
/// first word of its label, e.g. "work" or "personal". When another
/// account's label starts the same, as `sam.lee@gmail.com` and
/// `sam@northwindtraders.com` do, the first word that tells it apart,
/// trying an email's domain first: "gmail".
fn account_short(pool: &Pool, pools: &[Pool]) -> Option<String> {
    let others: Vec<Vec<String>> = (pools.iter())
        .filter(|other| other.provider == pool.provider && other.id != pool.id)
        .map(|other| {
            let (local, domain) = label_words(&other.account_label);
            [local, domain].concat()
        })
        .collect();
    if others.is_empty() {
        return None;
    }
    let (local, domain) = label_words(&pool.account_label);
    let first = (local.first().or(domain.first()))
        .cloned()
        .unwrap_or_else(|| pool.account_label.to_lowercase());
    let word = if others.iter().any(|other| other.first() == Some(&first)) {
        (domain.iter().chain(&local))
            .find(|word| others.iter().all(|other| !other.contains(word)))
            .cloned()
            .unwrap_or(first)
    } else {
        first
    };
    Some(word.chars().take(10).collect())
}

/// A label's words, lowercased: an email's local part, and its domain
/// without the top level ("gmail" of gmail.com). Other labels are all
/// local part.
fn label_words(label: &str) -> (Vec<String>, Vec<String>) {
    let split = |part: &str| -> Vec<String> {
        part.split(['-', '.', ' ', '_'])
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    match label.split_once('@') {
        Some((local, domain)) => {
            let mut domain = split(domain);
            domain.pop();
            (split(local), domain)
        }
        None => (split(label), Vec::new()),
    }
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

fn window_menu(
    mut menu: gpui_kit::component::menu::PopupMenu,
    windows: &[Window],
    now: i64,
    stale: bool,
    reason: Option<String>,
) -> gpui_kit::component::menu::PopupMenu {
    for window in windows {
        menu = menu.item(PopupMenuItem::label(describe_window(window, now)));
    }
    if stale {
        menu = menu
            .separator()
            .item(PopupMenuItem::label("Last update is over 10 minutes old"));
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
    let explain = format!(
        "{:.0}% of {} left, resets in {}.",
        window.remaining_percent,
        window.label,
        usage_source::format_reset(window.resets_at, now)
    );
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
        .dropdown_menu(move |menu, _, cx| {
            let selection = core.nav.read(cx).selection.clone();
            let mut launch = PopupMenuItem::new(format!("New agent on {provider}"));
            if let Some(selection) = selection {
                let core = core.clone();
                let provider = provider.clone();
                launch = launch.on_click(move |_, _, cx| {
                    core.nav.update(cx, |nav, cx| {
                        nav.pending_provider = Some(provider.clone());
                        nav.layout = "paseo".into();
                        nav.screen = Screen::Workspace;
                        nav.new_draft(&selection.key(), cx);
                    });
                });
            } else {
                launch = launch.disabled(true);
            }
            menu.item(PopupMenuItem::label(explain.clone()))
                .item(PopupMenuItem::label("Opens a draft. Nothing is launched."))
                .separator()
                .item(launch)
        })
}

/// Which subscription, and its binding window with its pace.
fn window_tip(pool: &Pool, binding: Option<&Window>, now: i64, stale: bool) -> String {
    let mut lines = vec![format!("{} {}", pool.provider, pool.account_label)];
    if let Some(binding) = binding {
        lines.push(describe_window(binding, now));
    }
    if stale {
        lines.push("Last update is over 10 minutes old".into());
    }
    lines.join("\n")
}

/// "weekly 52% left, resets in 4.7d, 1.9x too fast".
fn describe_window(window: &Window, now: i64) -> String {
    let pace = usage_source::pace(window, now)
        .and_then(|pace| usage_source::ratio_text(&pace))
        .map(|text| format!(", {text}"))
        .unwrap_or_default();
    format!(
        "{} {:.0}% left, resets in {}{pace}",
        window.label,
        window.remaining_percent,
        usage_source::format_reset(window.resets_at, now)
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(id: &str, provider: &str, label: &str) -> Pool {
        Pool {
            id: id.into(),
            provider: provider.into(),
            account_label: label.into(),
            status: "ok".into(),
            windows: Vec::new(),
        }
    }

    #[test]
    fn accounts_are_named_by_what_sets_them_apart() {
        let pools = [
            pool("a", "claude", "sam.lee@gmail.com"),
            pool("b", "claude", "sam@northwindtraders.com"),
            pool("c", "codex", "sam@example.com"),
            pool("d", "codex", "work-account"),
        ];
        let short = |i: usize| account_short(&pools[i], &pools);
        assert_eq!(short(0).as_deref(), Some("gmail"));
        assert_eq!(short(1).as_deref(), Some("northwindt"), "cut to ten");
        assert_eq!(short(2).as_deref(), Some("sam"), "no clash, first word");
        assert_eq!(short(3).as_deref(), Some("work"));
        let alone = [pool("e", "gemini", "me@example.com")];
        assert_eq!(account_short(&alone[0], &alone), None);
    }
}
