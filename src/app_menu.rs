//! The macOS menu bar. Windows and Linux windows have none; everything here
//! is also reachable from the window, so the menus mostly name what the
//! keyboard already does, with its shortcuts shown beside each item.

use gpui_kit::component::input;
use gpui_kit::gpui::{
    App, KeyBinding, Menu, MenuItem, OsAction, PromptLevel, SystemMenuType, Window,
};

use crate::app::Core;
use crate::shell::{
    CommandCenter, CycleLayout, NewAgentTab, NewTerminal, NewWorkspace, OpenSettings, ShowHelp,
    ToggleSidebar, ZoomIn, ZoomOut, ZoomReset,
};

gpui_kit::actions!(
    app_menu,
    [About, Quit, Hide, HideOthers, ShowAll, Minimize, ZoomWindow]
);

/// The version, and on a build from a checkout the commit it was built at,
/// since every build so far is 0.1.0.
pub fn version() -> String {
    match option_env!("CRUSTEO_COMMIT") {
        Some(commit) => format!("{} ({commit})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The version, the Paseo protocol spoken, and each host's daemon version
/// as it reported on connecting, since the fleet runs several.
pub fn about(core: &Core, window: &mut Window, cx: &mut App) {
    let mut detail = format!(
        "Version {}\nPaseo protocol {}",
        version(),
        paseo_client::PROTOCOL_VERSION
    );
    let fleet = &core.store.read(cx).fleet;
    if !fleet.hosts.is_empty() {
        detail.push('\n');
    }
    for host in &fleet.hosts {
        let daemon = match &host.server {
            Some(server) if !server.version.is_empty() => format!("Paseo {}", server.version),
            Some(_) => "Paseo, version not reported".to_string(),
            None => "not connected".to_string(),
        };
        detail.push_str(&format!("\n{}: {daemon}", host.name()));
    }
    // Nothing to do with the answer, but it is awaited all the same: on
    // Windows, dropping it closes the dialog as soon as it opens.
    let answer = window.prompt(PromptLevel::Info, "Crusteo", Some(&detail), &["OK"], cx);
    cx.foreground_executor()
        .spawn(async move {
            answer.await.ok();
        })
        .detach();
}

pub fn install(cx: &mut App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    // With a menu bar, these are the app's to answer rather than the
    // system's, so they need bindings of their own.
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    set_menus(&[], cx);
}

/// The menu bar, with extensions' commands in the menus they name. Called
/// again once the window has asked extensions what they add.
pub fn set_menus(commands: &[crate::extensions::Command], cx: &mut App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let mut menus = vec![
        Menu::new("Crusteo").items([
            MenuItem::action("About Crusteo", About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Crusteo", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Crusteo", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Workspace", NewWorkspace),
            MenuItem::action("New Agent Tab", NewAgentTab),
            MenuItem::action("New Terminal", NewTerminal),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find on Board", crate::layouts::lanes::FindOnBoard),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Center", CommandCenter),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Next Layout", CycleLayout),
            MenuItem::separator(),
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Actual Size", ZoomReset),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", ZoomWindow),
        ]),
        Menu::new("Help").items([MenuItem::action("Keyboard Shortcuts", ShowHelp)]),
    ];
    // Backwards, as each goes first in its menu: they end up in order.
    for command in commands.iter().rev() {
        let Some(name) = command.menu else {
            continue;
        };
        let item = MenuItem::action(
            command.name.clone(),
            crate::extensions::RunCommand {
                id: command.id.clone(),
            },
        );
        match menus.iter_mut().find(|menu| menu.name == name) {
            // First in its menu, as the app's own items are the standing ones.
            Some(menu) => menu.items.insert(0, item),
            // A menu of its own goes before Help.
            None => {
                let at = menus.len() - 1;
                menus.insert(at, Menu::new(name).items([item]));
            }
        }
    }
    cx.set_menus(menus);
}
