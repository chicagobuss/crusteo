//! Shared application state. Every layout reads the same fleet and the same
//! navigation; switching layout does not reconnect or lose the place.

pub mod config;
pub mod images;
pub mod nav;
pub mod notifications;
pub mod reef;
pub mod store;
pub mod stress;
pub mod usage;

use gpui_kit::gpui::{AnyView, App, AppContext, Entity, Window};

pub use nav::{Closed, Nav, Screen, Tab};
pub use store::{AgentSession, FleetEvent, FleetStore, TerminalSession, TerminalWatch};

#[derive(Default)]
pub(crate) struct DraftHandoffs {
    pending: std::collections::BTreeMap<(fleet::HostId, String), fleet::prompt::Prompt>,
}
impl DraftHandoffs {
    fn stash(
        &mut self,
        key: (fleet::HostId, String),
        draft: Option<fleet::prompt::Prompt>,
    ) -> bool {
        let draft = draft.filter(|draft| !draft.text.is_empty() || !draft.images.is_empty());
        if self.pending.get(&key) == draft.as_ref() {
            return false;
        }
        match draft {
            Some(draft) => {
                self.pending.insert(key, draft);
            }
            None => {
                self.pending.remove(&key);
            }
        }
        true
    }
}

#[derive(Clone)]
pub struct Core {
    pub store: Entity<FleetStore>,
    pub nav: Entity<Nav>,
    pub usage: Entity<usage::Usage>,
    /// Every toolbar item, from the shell, layouts and extensions.
    pub toolbars: Entity<crate::toolbars::Registry>,
    /// What extensions added, once the window has opened.
    pub extensions: Entity<crate::extensions::Contributed>,
    /// Drafts handed back by detached windows, consumed by the next Lanes view.
    detached_drafts: Entity<DraftHandoffs>,
    /// The shell window belonging to this Core, never inferred from other windows.
    main_window: std::rc::Rc<std::cell::Cell<Option<gpui_kit::gpui::AnyWindowHandle>>>,
}

impl Core {
    pub(crate) fn draft_handoffs(&self) -> &Entity<DraftHandoffs> {
        &self.detached_drafts
    }

    /// Consume a returned draft once, whichever layout next shows its composer.
    pub fn take_draft(
        &self,
        host: &fleet::HostId,
        agent: &str,
        cx: &mut App,
    ) -> Option<fleet::prompt::Prompt> {
        self.detached_drafts.update(cx, |drafts, _| {
            drafts.pending.remove(&(host.clone(), agent.to_string()))
        })
    }

    pub fn stash_draft(
        &self,
        host: &fleet::HostId,
        agent: &str,
        draft: Option<fleet::prompt::Prompt>,
        cx: &mut App,
    ) {
        self.detached_drafts.update(cx, |drafts, cx| {
            if drafts.stash((host.clone(), agent.to_string()), draft) {
                cx.notify();
            }
        });
    }

    pub(crate) fn set_main_window(&self, window: gpui_kit::gpui::AnyWindowHandle) {
        self.main_window.set(Some(window));
    }
    pub(crate) fn main_window(&self) -> Option<gpui_kit::gpui::AnyWindowHandle> {
        self.main_window.get()
    }

    /// Shows agent `agent` on `host`: its card in Lanes, else its tab. False
    /// when that host does not know it, or it has no workspace to open it in.
    pub fn open_agent(&self, host: &fleet::HostId, agent: &str, cx: &mut App) -> bool {
        let workspace = (self.store.read(cx).fleet.agent(host, agent))
            .and_then(|record| record.workspace_id.clone());
        let Some(workspace) = workspace else {
            return false;
        };
        self.nav
            .update(cx, |nav, cx| nav.open_agent(host, &workspace, agent, cx));
        true
    }

    /// The title of agent `agent` on `host`, when that host knows it.
    pub fn agent_title(&self, host: &fleet::HostId, agent: &str, cx: &App) -> Option<String> {
        let record = self.store.read(cx).fleet.agent(host, agent)?;
        Some(record.display_title())
    }

    /// The one host that knows agent `agent`, for a source that names no host
    /// (see [`fleet::Fleet::sole_host_of`]).
    pub fn agent_host(&self, agent: &str, cx: &App) -> Option<fleet::HostId> {
        let fleet = &self.store.read(cx).fleet;
        fleet.sole_host_of(agent).map(|host| host.id().clone())
    }

    pub fn new(cx: &mut App) -> Self {
        let hosts = config::load_hosts();
        let store = cx.new(|cx| FleetStore::new(hosts, cx));
        let nav = cx.new(|_| Nav::load());
        cx.on_app_quit({
            let nav = nav.clone();
            move |cx| {
                nav.update(cx, |nav, _| nav.save());
                async {}
            }
        })
        .detach();
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
            detached_drafts: cx.new(|_| DraftHandoffs::default()),
            main_window: Default::default(),
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
        if let Some((host, workspace)) = nav::parse_workspace_key(&key) {
            consider.push(nav::Selection {
                host,
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

#[cfg(test)]
mod draft_tests {
    use super::*;
    #[test]
    fn draft_handoff_keeps_images_is_host_qualified_and_consumed_once() {
        let mut drafts = DraftHandoffs::default();
        let key = (fleet::HostId::new("one"), "same-agent".into());
        let prompt = fleet::prompt::Prompt {
            text: "unsent".into(),
            images: vec![fleet::prompt::ImageAttachment::new("aGVsbG8=", "image/png")],
        };
        assert!(drafts.stash(key.clone(), Some(prompt.clone())));
        assert!(!drafts.stash(key.clone(), Some(prompt.clone())));
        assert!(
            drafts
                .pending
                .remove(&(fleet::HostId::new("two"), "same-agent".into()))
                .is_none()
        );
        assert_eq!(drafts.pending.remove(&key), Some(prompt));
        assert!(drafts.pending.remove(&key).is_none());
        assert!(!drafts.stash(key, Some(fleet::prompt::Prompt::default())));
    }
}
