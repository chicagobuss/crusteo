//! How a card says where it is: its swatch, folder and place label, and
//! the paths they show.

use fleet::HostId;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::gpui::{
    AnyElement, App, ClickEvent, InteractiveElement, IntoElement, ParentElement, Pixels,
    SharedString, StatefulInteractiveElement, Styled, div, px, rgb,
};

use gpui_kit::prelude::FluentBuilder;

use crate::app::Core;
use crate::theme::{self, palette, size};

use super::*;

/// `path` as a menu shows it: a home folder as `~` (`/home/<user>`,
/// `/Users/<user>` or `C:\Users\<user>`, on whichever host it is), and the
/// middle of a long one cut to `…`.
pub(super) fn short_path(path: &str) -> String {
    let home = ["/home/", "/Users/"]
        .iter()
        .find_map(|prefix| {
            let rest = path.strip_prefix(prefix)?;
            Some(match rest.split_once('/') {
                Some((_, after)) if !after.is_empty() => format!("~/{after}"),
                _ => "~".to_string(),
            })
        })
        .or_else(|| {
            let lower = path.to_ascii_lowercase();
            let rest = (lower.get(1..9) == Some(":\\users\\")).then(|| &path[9..])?;
            Some(match rest.split_once('\\') {
                Some((_, after)) if !after.is_empty() => format!("~\\{after}"),
                _ => "~".to_string(),
            })
        })
        .unwrap_or_else(|| path.to_string());
    let chars: Vec<char> = home.chars().collect();
    if chars.len() <= MENU_PATH_CHARS {
        return home;
    }
    // The end names the folder, so it keeps the most.
    let head = MENU_PATH_CHARS / 3;
    let tail = MENU_PATH_CHARS - head - 1;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// `agent`'s colour, as an index into LANE_COLORS: the one pinned for it,
/// else (with coloured lanes on) its first label's lane colour, as Lanes
/// draws that lane. None for an agent with neither.
pub fn agent_swatch(core: &Core, host: &HostId, agent: &str, cx: &App) -> Option<usize> {
    let nav = core.nav.read(cx);
    if let Some(index) = nav.agent_color(host, agent) {
        return Some(index);
    }
    if !nav.colored_lanes {
        return None;
    }
    let fleet = &core.store.read(cx).fleet;
    let record = fleet.agent(host, agent)?;
    let workspace = (record.workspace_id.as_deref()).and_then(|id| fleet.workspace(host, id));
    let (key, named) = match &fleet.labels {
        Some(catalog) => {
            let id = fleet::labels::effective(record, workspace, catalog)
                .into_iter()
                .next()?;
            let label = catalog.get(&id)?;
            (id.to_string(), Some(label.color.clone()))
        }
        None => {
            let name = workspace?.labels.first()?;
            let key = fleet::directory::label_key(name);
            let named = (fleet.host(host)?.directory.labels.iter())
                .find(|def| fleet::directory::label_key(&def.name) == key)
                .map(|def| def.color.clone());
            (key, named)
        }
    };
    Some(colors::index(
        &key,
        named.as_deref(),
        &nav.lane_color_choices,
    ))
}

/// The folder `agent` works in (its own, else its workspace's) and its
/// workspace's branch, if any.
pub fn agent_folder(
    core: &Core,
    host: &HostId,
    agent: &str,
    cx: &App,
) -> Option<(String, Option<String>)> {
    let fleet = &core.store.read(cx).fleet;
    let record = fleet.agent(host, agent)?;
    let workspace = (record.workspace_id.as_deref()).and_then(|id| fleet.workspace(host, id));
    let folder = Some(record.cwd.clone())
        .filter(|cwd| !cwd.trim().is_empty())
        .or_else(|| workspace.map(|w| w.directory().to_string()))
        .filter(|dir| !dir.trim().is_empty())?;
    let branch = workspace.and_then(|w| w.branch()).map(str::to_string);
    Some((folder, branch))
}

/// A path with the home folder as `~`, not shortened otherwise.
pub(super) fn tilde(path: &str) -> String {
    let home = ["/home/", "/Users/", r"C:\Users\"]
        .iter()
        .find_map(|prefix| {
            let rest = path.strip_prefix(prefix)?;
            let cut = rest.find(['/', '\\']).unwrap_or(rest.len());
            Some(format!("~{}", &rest[cut..]))
        });
    home.unwrap_or_else(|| path.to_string())
}

/// What hovering a place label says, each part named: the workspace in full
/// (the label may cut it short), the agent's working directory and branch,
/// and that a click copies the directory. With neither, the host.
fn place_hint(
    workspace: Option<&str>,
    folder: Option<&(String, Option<String>)>,
    host_name: &str,
) -> String {
    let mut lines = Vec::new();
    if let Some(workspace) = workspace {
        lines.push(format!("Paseo workspace: {workspace}"));
    }
    if let Some((folder, branch)) = folder {
        lines.push(format!("Working directory: {}", tilde(folder)));
        if let Some(branch) = branch {
            lines.push(format!("Branch: {branch}"));
        }
        lines.push("Click to copy the working directory".into());
    }
    if lines.is_empty() {
        lines.push(host_name.to_string());
    }
    lines.join("\n")
}

/// Where an agent runs, "workspace · host", muted and at most `max` wide. A
/// long workspace name is cut short, never the host. Hovering names the
/// workspace in full and the agent's working directory and branch; a click
/// copies the directory.
pub fn place_label(
    core: &Core,
    host: &HostId,
    agent: &str,
    (workspace, host_name): (Option<String>, String),
    max: Pixels,
    cx: &App,
) -> AnyElement {
    let p = palette();
    let folder = agent_folder(core, host, agent, cx);
    let hint = place_hint(workspace.as_deref(), folder.as_ref(), &host_name);
    let store = core.store.clone();
    let host_part = match workspace {
        Some(_) => format!("\u{a0}· {host_name}"),
        None => host_name,
    };
    div()
        .id(SharedString::from(format!("place-{host}-{agent}")))
        .min_w(px(0.))
        .max_w(max)
        .flex()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(size::sm())
        .text_color(p.muted)
        .when(folder.is_some(), |label| {
            label
                .cursor_pointer()
                .hover(move |style| style.text_color(p.foreground))
        })
        .children(workspace.map(|workspace| div().min_w(px(0.)).truncate().child(workspace)))
        .child(div().flex_shrink_0().child(host_part))
        .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        .on_click(move |_: &ClickEvent, _, cx| {
            cx.stop_propagation();
            if let Some((folder, _)) = &folder {
                cx.write_to_clipboard(gpui_kit::gpui::ClipboardItem::new_string(folder.clone()));
                store.update(cx, |store, cx| store.toast(format!("Copied {folder}"), cx));
            }
        })
        .into_any_element()
}

/// A bar tinted with LANE_COLORS[`index`] over `surface`, as a lane header is.
pub fn swatch_tint(surface: gpui_kit::gpui::Rgba, index: usize) -> gpui_kit::gpui::Rgba {
    colors::tint(
        surface,
        rgb(theme::LANE_COLORS[index].1),
        theme::LANE_HEADER_TINT,
    )
}

#[cfg(test)]
mod path_tests {
    use super::short_path;

    #[test]
    fn a_folder_on_hover_shows_home_as_a_tilde_and_keeps_the_rest_whole() {
        let long =
            "/home/sam/src/a-very-long-project-name/worktrees/feature-branch-with-a-long-name";
        assert_eq!(
            super::tilde(long),
            "~/src/a-very-long-project-name/worktrees/feature-branch-with-a-long-name"
        );
        assert_eq!(super::tilde("/Users/sam"), "~");
        assert_eq!(super::tilde(r"C:\Users\sam\src\app"), r"~\src\app");
        assert_eq!(super::tilde("/srv/app"), "/srv/app");
    }

    #[test]
    fn home_folders_read_as_a_tilde_and_long_paths_lose_their_middle() {
        assert_eq!(short_path("/home/sam/src/crusteo"), "~/src/crusteo");
        assert_eq!(short_path("/Users/sam/code"), "~/code");
        assert_eq!(short_path("/Users/sam"), "~");
        assert_eq!(short_path(r"C:\Users\sam\src\app"), r"~\src\app");
        assert_eq!(short_path("/srv/app"), "/srv/app");
        assert_eq!(short_path("unknown path"), "unknown path");
        let long =
            "/home/sam/src/a-very-long-project-name/worktrees/feature-branch-with-a-long-name";
        let shown = short_path(long);
        assert_eq!(shown.chars().count(), super::MENU_PATH_CHARS);
        assert!(shown.starts_with("~/src/") && shown.contains('…'));
        assert!(shown.ends_with("feature-branch-with-a-long-name"));
    }

    #[test]
    fn hovering_a_place_names_the_workspace_and_the_working_directory() {
        let folder = ("/home/sam/code".to_string(), Some("fix-retry".to_string()));
        assert_eq!(
            super::place_hint(
                Some("shop-checkout-and-a-long-name"),
                Some(&folder),
                "buildbox"
            ),
            "Paseo workspace: shop-checkout-and-a-long-name\nWorking directory: ~/code\n\
             Branch: fix-retry\nClick to copy the working directory"
        );
        let no_branch = ("/srv/app".to_string(), None);
        assert_eq!(
            super::place_hint(None, Some(&no_branch), "buildbox"),
            "Working directory: /srv/app\nClick to copy the working directory"
        );
        assert_eq!(
            super::place_hint(Some("shop"), None, "buildbox"),
            "Paseo workspace: shop"
        );
        assert_eq!(super::place_hint(None, None, "buildbox"), "buildbox");
    }
}
