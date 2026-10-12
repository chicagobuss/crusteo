//! Shared application state. Every layout reads the same fleet and the same
//! navigation; switching layout does not reconnect or lose the place.

pub mod config;
pub mod nav;
pub mod notifications;
pub mod store;
pub mod stress;
pub mod usage;

use gpui_kit::gpui::{AnyView, App, AppContext, Entity, Window};

pub use nav::{Closed, Nav, Screen, Tab};
pub use store::{AgentSession, FleetEvent, FleetStore, TerminalSession, TerminalWatch};

#[derive(Clone)]
pub struct Core {
    pub store: Entity<FleetStore>,
    pub nav: Entity<Nav>,
    pub usage: Entity<usage::Usage>,
    /// Every toolbar item, from the shell, layouts and extensions.
    pub toolbars: Entity<crate::toolbars::Registry>,
    /// What extensions added, once the window has opened.
    pub extensions: Entity<crate::extensions::Contributed>,
}

impl Core {
    pub fn new(cx: &mut App) -> Self {
        let hosts = config::load_hosts();
        let store = cx.new(|cx| FleetStore::new(hosts, cx));
        let nav = cx.new(|_| Nav::load());
        // Keep tabs reconciled with the directory, and stream exactly the
        // conversations some tab shows. Tabs follow which agents, workspaces
        // and terminals exist, so an agent's status changing in place (most
        // pushes) skips the work.
        let reconciled_at: std::rc::Rc<std::cell::Cell<Option<(u64, u64)>>> = Default::default();
        cx.observe(&store, {
            let nav = nav.clone();
            let store_handle = store.clone();
            move |store, cx| {
                let log = store.read(cx);
                let membership_changed = reconciled_at.get().is_none_or(|(at, terminals)| {
                    terminals != log.terminal_nav_revision()
                        || log.changes_since(at).is_none_or(|mut changes| {
                            changes.any(|change| {
                                !matches!(
                                    change.change,
                                    fleet::DirChange::AgentUpdated(_)
                                        | fleet::DirChange::WorkspaceDetail(_)
                                )
                            })
                        })
                });
                let directory_changed = reconciled_at.get().map(|(directory, _)| directory)
                    != Some(log.change_cursor());
                reconciled_at.set(Some((log.change_cursor(), log.terminal_nav_revision())));
                if directory_changed {
                    let tabs = nav.read(cx).open_agent_tabs();
                    store_handle.update(cx, |store, cx| store.mark_open_subagent_tabs(&tabs, cx));
                }
                if !membership_changed {
                    return;
                }
                let lists = store.read(cx).terminal_nav_lists();
                let inputs = nav::reconcile_inputs(&store.read(cx).fleet, &lists);
                let changed = nav.update(cx, |nav, cx| {
                    let selection_changed = nav.reconcile_selection(&store.read(cx).fleet);
                    nav.reconcile(inputs) || selection_changed
                });
                if changed {
                    nav.update(cx, |nav, cx| {
                        nav.save();
                        cx.notify();
                    });
                }
                let nav = nav.clone();
                let store_handle = store_handle.clone();
                cx.defer(move |cx| sync_terminals(&store_handle, &nav, cx));
            }
        })
        .detach();
        cx.observe(&nav, {
            let store = store.clone();
            move |nav, cx| {
                let keep = nav.read(cx).open_agents();
                let tabs = nav.read(cx).open_agent_tabs();
                let terminals = nav.read(cx).open_terminals();
                store.update(cx, |store, cx| {
                    store.mark_open_subagent_tabs(&tabs, cx);
                    store.retain_sessions(&keep, cx);
                    store.retain_terminals(&terminals, cx);
                });
                let nav = nav.clone();
                let store = store.clone();
                cx.defer(move |cx| sync_terminals(&store, &nav, cx));
            }
        })
        .detach();
        let usage = cx.new(|_| usage::Usage::default());
        let toolbars = cx.new(|_| crate::toolbars::Registry::default());
        let extensions = cx.new(|_| crate::extensions::Contributed::default());
        Self {
            store,
            nav,
            usage,
            toolbars,
            extensions,
        }
    }
}

fn sync_terminals(store: &Entity<FleetStore>, nav: &Entity<Nav>, cx: &mut App) {
    let mut wanted = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut consider = Vec::new();
    if let Some(selection) = nav.read(cx).selection.clone() {
        consider.push(selection);
    }
    let keys: Vec<String> = nav.read(cx).tabs.keys().cloned().collect();
    for key in keys {
        let terminal = nav
            .read(cx)
            .tabs_for(&key)
            .iter()
            .any(|tab| matches!(tab, Tab::Terminal(_)));
        if !terminal {
            continue;
        }
        if let Some((host, workspace)) = key.split_once(':') {
            consider.push(nav::Selection {
                host: host.to_string(),
                workspace: workspace.to_string(),
            });
        }
    }
    for selection in consider {
        let pair = (selection.host.clone(), selection.workspace.clone());
        if !seen.insert(pair) {
            continue;
        }
        let cwd = store
            .read(cx)
            .fleet
            .workspace(&selection.host, &selection.workspace)
            .map(|workspace| workspace.directory().to_string())
            .filter(|cwd| !cwd.is_empty());
        if let Some(cwd) = cwd {
            wanted.push(TerminalWatch {
                host: selection.host,
                workspace: selection.workspace,
                cwd,
            });
        }
    }
    let lanes_active = nav.read(cx).layout == "lanes";
    store.update(cx, |store, cx| {
        if !lanes_active {
            store.ensure_terminal_watches("lanes", Vec::new(), cx);
        }
        store.ensure_terminal_watches("paseo", wanted, cx);
    });
}

/// A way to look at the fleet. Layouts are registered by id and built
/// lazily; each gets the same [`Core`].
pub struct LayoutEntry {
    pub id: &'static str,
    pub title: &'static str,
    pub build: fn(&Core, &mut Window, &mut App) -> AnyView,
    /// Whether the shell draws the layout as a cached view. A cached view
    /// that re-renders redraws every cached view inside it too, so a layout
    /// that caches its own parts and changes often draws uncached, and its
    /// parts survive its re-renders.
    pub cached: bool,
}
