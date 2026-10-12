//! Agent and workspace archive entry points, with confirmation and fresh-state checks.
use crate::app::nav::Selection;
use crate::app::{Closed, Core, Tab};
use crate::ui;
use fleet::workspace_archive::{ArchiveActivity, ArchivePlan, archive_target};
use fleet::{Bucket, requests};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::gpui::{App, Window};

pub fn available(core: &Core, host: &str, workspace: &str, source: Option<&str>, cx: &App) -> bool {
    let store = core.store.read(cx);
    !store.workspace_archive_pending(host, workspace)
        && archive_target(&store.fleet, host, workspace, source).is_ok()
}

pub fn agent_menu(menu: PopupMenu, core: &Core, host: &str, agent: &str, cx: &App) -> PopupMenu {
    let exists = can_close_agent(core, host, agent, cx);
    if is_subagent(core, host, agent, cx) {
        let detach = core
            .store
            .read(cx)
            .fleet
            .host(host)
            .is_some_and(|record| record.has("agentDetach"));
        let archive = {
            let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
            PopupMenuItem::new("Archive subagent…")
                .disabled(!exists)
                .on_click(move |_, window, cx| archive_subagent(&core, &host, &agent, window, cx))
        };
        let menu = menu.item(archive);
        if !detach {
            return menu;
        }
        let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
        return menu.item(
            PopupMenuItem::new("Detach subagent…")
                .disabled(!exists)
                .on_click(move |_, window, cx| detach_subagent(&core, &host, &agent, window, cx)),
        );
    }
    let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
    menu.item(
        PopupMenuItem::new("Close agent tab")
            .disabled(!exists)
            .on_click(move |_, window, cx| close_agent(&core, &host, &agent, window, cx)),
    )
}

/// Whether `agent` was started by another agent. Paseo keeps such an agent
/// with its parent: closing its tab hides it rather than archiving it.
pub fn is_subagent(core: &Core, host: &str, agent: &str, cx: &App) -> bool {
    live_agent(&core.store.read(cx).fleet, host, agent)
        .is_some_and(|record| record.parent_agent_id.is_some())
}

fn agent_title(core: &Core, host: &str, agent: &str, cx: &App) -> String {
    live_agent(&core.store.read(cx).fleet, host, agent)
        .map(|record| short_name(&record.display_title()))
        .unwrap_or_default()
}

/// Archives a subagent once the user confirms, as Paseo's track does.
pub fn archive_subagent(core: &Core, host: &str, agent: &str, window: &mut Window, cx: &mut App) {
    let Some(running) = live_agent(&core.store.read(cx).fleet, host, agent)
        .map(|record| matches!(record.bucket(), Bucket::Running | Bucket::NeedsInput))
    else {
        return;
    };
    let title = agent_title(core, host, agent, cx);
    let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
    ui::confirm(
        window,
        cx,
        if running {
            "Archive running subagent?"
        } else {
            "Archive subagent?"
        },
        &if running {
            format!("This stops \"{title}\" and archives it.")
        } else {
            format!("This archives \"{title}\".")
        },
        "Archive",
        move |cx| {
            core.store.update(cx, |store, cx| {
                store.act(&host, requests::archive_agent(&agent), cx)
            });
        },
    );
}

/// Makes a subagent a standalone agent once the user confirms.
pub fn detach_subagent(core: &Core, host: &str, agent: &str, window: &mut Window, cx: &mut App) {
    let title = agent_title(core, host, agent, cx);
    let name = if title.is_empty() {
        "This subagent".to_string()
    } else {
        format!("\"{title}\"")
    };
    let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
    ui::confirm(
        window,
        cx,
        "Detach subagent?",
        &format!("{name} will leave its parent and continue as a standalone agent."),
        "Detach",
        move |cx| {
            core.store.update(cx, |store, cx| {
                store.act(&host, requests::detach_agent(&agent), cx)
            });
        },
    );
}

fn live_agent<'a>(fleet: &'a fleet::Fleet, host: &str, agent: &str) -> Option<&'a fleet::Agent> {
    fleet
        .agent(host, agent)
        .filter(|record| record.archived_at.is_none())
}

pub fn can_close_agent(core: &Core, host: &str, agent: &str, cx: &App) -> bool {
    live_agent(&core.store.read(cx).fleet, host, agent).is_some()
}

/// A tab action requires an actual local close; a card action archives even
/// when that agent has no locally open tab.
#[derive(Clone, Debug)]
pub enum AgentCloseOrigin {
    Tab(String),
    Card,
}

impl AgentCloseOrigin {
    fn tab_key(&self, host: &str, workspace: Option<&str>) -> Option<String> {
        match self {
            Self::Tab(key) => Some(key.clone()),
            Self::Card => workspace.map(|workspace| {
                Selection {
                    host: host.to_string(),
                    workspace: workspace.to_string(),
                }
                .key()
            }),
        }
    }

    fn should_archive(&self, agent: &str, closed: Option<&Closed>) -> bool {
        match self {
            Self::Card => true,
            Self::Tab(_) => matches!(closed, Some(Closed::Agent(id)) if id == agent),
        }
    }
}

/// Facts that determine which agent and activity a close affects. Workspace
/// archive availability is deliberately excluded: it only controls an optional
/// offer, and that action goes through show's separate fresh-state check.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentCloseState {
    workspace: Option<String>,
    running: bool,
    last: bool,
}

impl AgentCloseState {
    fn read(fleet: &fleet::Fleet, host: &str, agent: &str) -> Option<Self> {
        let record = live_agent(fleet, host, agent)?;
        let workspace = record.workspace_id.clone();
        let last = workspace.as_deref().is_some_and(|workspace| {
            fleet.host(host).is_some_and(|host| {
                !host.directory.agents.values().any(|other| {
                    other.id != agent
                        && other.archived_at.is_none()
                        && other.workspace_id.as_deref() == Some(workspace)
                })
            })
        });
        Some(Self {
            workspace,
            running: matches!(record.bucket(), Bucket::Running | Bucket::NeedsInput),
            last,
        })
    }
}

/// Card actions archive the agent even when it has no locally open tab.
pub fn close_agent(core: &Core, host: &str, agent: &str, window: &mut Window, cx: &mut App) {
    close_agent_tab(core, host, agent, AgentCloseOrigin::Card, window, cx);
}

/// Closes an agent tab, preserving its caller's exact navigation key. Running
/// agents ask first; the last agent offers a workspace archive instead.
pub fn close_agent_tab(
    core: &Core,
    host: &str,
    agent: &str,
    origin: AgentCloseOrigin,
    window: &mut Window,
    cx: &mut App,
) {
    // A subagent's tab only hides it, as in Paseo; a card, which no tab
    // backs, archives it.
    if is_subagent(core, host, agent, cx) {
        match &origin {
            AgentCloseOrigin::Tab(key) => {
                core.nav.update(cx, |nav, cx| {
                    nav.close_tab(key, &Tab::Agent(agent.to_string()), cx)
                });
            }
            AgentCloseOrigin::Card => archive_subagent(core, host, agent, window, cx),
        }
        return;
    }
    let store = core.store.read(cx);
    let Some(state) = AgentCloseState::read(&store.fleet, host, agent) else {
        // A stale local tab can close without sending an archive for a missing
        // or already archived agent.
        if let AgentCloseOrigin::Tab(key) = &origin {
            core.nav.update(cx, |nav, cx| {
                nav.close_tab(key, &Tab::Agent(agent.to_string()), cx)
            });
        }
        return;
    };
    let workspace_name = state
        .workspace
        .as_deref()
        .and_then(|workspace| store.fleet.workspace(host, workspace))
        .map(|w| w.title.clone().unwrap_or_else(|| w.name.clone()))
        .unwrap_or_default();

    let close = {
        let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
        let (state, origin) = (state.clone(), origin.clone());
        let handle = window.window_handle();
        move |cx: &mut App| {
            let (core, host, agent, state, origin) = (
                core.clone(),
                host.clone(),
                agent.clone(),
                state.clone(),
                origin.clone(),
            );
            // The previous modal must close before a changed state can open
            // another confirmation. Even the immediate path rechecks here.
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    finish_agent_close(&core, &host, &agent, &state, &origin, window, cx)
                });
            });
        }
    };
    let stops = if state.running {
        " and stops its current turn"
    } else {
        ""
    };
    match state.workspace {
        Some(workspace) if state.last && available(core, host, &workspace, Some(agent), cx) => {
            let archive = {
                let (core, host, agent) = (core.clone(), host.to_string(), agent.to_string());
                move |window: &mut Window, cx: &mut App| {
                    let (core, host, agent, workspace) =
                        (core.clone(), host.clone(), agent.clone(), workspace.clone());
                    let handle = window.window_handle();
                    cx.defer(move |cx| {
                        let _ = handle.update(cx, |_, window, cx| {
                            show(&core, &host, &workspace, Some(&agent), window, cx)
                        });
                    });
                }
            };
            ui::choose(
                window,
                cx,
                &format!(
                    "Close the last agent in \"{}\"?",
                    short_name(&workspace_name)
                ),
                &format!(
                    "This is the only agent left in the workspace. Closing its tab \
                     archives the agent{stops}. You can archive the whole workspace instead."
                ),
                ("Close agent tab", move |_: &mut Window, cx: &mut App| {
                    close(cx)
                }),
                ("Archive workspace…", archive),
            );
        }
        _ if state.running => ui::confirm(
            window,
            cx,
            "Archive running agent?",
            "Closing this tab archives the agent and stops its current turn.",
            "Archive",
            close,
        ),
        _ => close(cx),
    }
}

fn finish_agent_close(
    core: &Core,
    host: &str,
    agent: &str,
    expected: &AgentCloseState,
    origin: &AgentCloseOrigin,
    window: &mut Window,
    cx: &mut App,
) {
    let latest = AgentCloseState::read(&core.store.read(cx).fleet, host, agent);
    if latest.as_ref() != Some(expected) {
        close_agent_tab(core, host, agent, origin.clone(), window, cx);
        return;
    }
    let key = origin.tab_key(host, expected.workspace.as_deref());
    let closed = key.as_deref().and_then(|key| {
        core.nav.update(cx, |nav, cx| {
            nav.close_tab(key, &Tab::Agent(agent.to_string()), cx)
        })
    });
    // A Paseo tab action only archives the tab it actually closed. Card
    // actions intentionally archive even if that agent has no local tab.
    if !origin.should_archive(agent, closed.as_ref()) {
        return;
    }
    core.store.update(cx, |store, cx| {
        store.act(host, requests::archive_agent(agent), cx)
    });
}

pub fn workspace_menu(
    menu: PopupMenu,
    core: &Core,
    host: &str,
    workspace: &str,
    cx: &App,
) -> PopupMenu {
    let enabled = available(core, host, workspace, None, cx);
    let (core, host, workspace) = (core.clone(), host.to_string(), workspace.to_string());
    menu.item(
        PopupMenuItem::new("Archive workspace…")
            .disabled(!enabled)
            .on_click(move |_, window, cx| show(&core, &host, &workspace, None, window, cx)),
    )
}

pub fn show(
    core: &Core,
    host: &str,
    workspace: &str,
    source: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    if core
        .store
        .read(cx)
        .workspace_archive_pending(host, workspace)
    {
        return;
    }
    let plan = ArchivePlan::build(&core.store.read(cx).fleet, host, workspace, source);
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            core.store.update(cx, |store, cx| store.toast(error, cx));
            return;
        }
    };
    let title = format!("Archive workspace \"{}\"?", short_name(&plan.name));
    let body = confirmation_body(&plan);
    let core = core.clone();
    let handle = window.window_handle();
    ui::confirm(window, cx, &title, &body, "Archive workspace", move |cx| {
        let latest = ArchivePlan::build(
            &core.store.read(cx).fleet,
            &plan.host,
            &plan.workspace,
            plan.source_agent.as_deref(),
        );
        match latest {
            Ok(latest) if !plan.needs_reconfirm(&latest) => {
                core.store.update(cx, |store, cx| {
                    store.archive_workspace(&plan.host, &plan.workspace, cx)
                });
            }
            Ok(_) => {
                // Show changes received since opening, after this window's
                // current update has ended and the old dialog has closed.
                let core = core.clone();
                let plan = plan.clone();
                cx.defer(move |cx| {
                    let _ = handle.update(cx, |_, window, cx| {
                        show(
                            &core,
                            &plan.host,
                            &plan.workspace,
                            plan.source_agent.as_deref(),
                            window,
                            cx,
                        )
                    });
                });
            }
            Err(error) => {
                core.store.update(cx, |store, cx| store.toast(error, cx));
            }
        }
    });
}

fn short_name(name: &str) -> String {
    let text = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = text.chars();
    let short: String = chars.by_ref().take(64).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

fn confirmation_body(plan: &ArchivePlan) -> String {
    let count = plan.agents.len();
    let mut lines = vec![
        format!("Host: {}", plan.host),
        match count {
            0 => "This archives the workspace.".into(),
            1 => "This archives the workspace and its agent.".into(),
            _ => format!("This archives the workspace and all {count} of its agents."),
        },
    ];
    if let Some(source) = plan
        .agents
        .iter()
        .find(|agent| Some(agent.id.as_str()) == plan.source_agent.as_deref())
    {
        lines.push(format!("Including: {}", short_name(&source.title)));
    }
    if plan.agents.iter().any(|agent| agent.activity.is_some()) {
        lines.push("Agents that are running or waiting for input will be stopped.".into());
    }
    let others: Vec<_> = plan
        .agents
        .iter()
        .filter(|agent| plan.source_agent.as_deref() != Some(&agent.id))
        .collect();
    if !others.is_empty() {
        lines.push(String::new());
        lines.push(if plan.source_agent.is_some() {
            format!("Other agents in this workspace ({}):", others.len())
        } else {
            "Agents in this workspace:".into()
        });
        for agent in others.iter().take(6) {
            let activity = agent
                .activity
                .map(|state| {
                    format!(
                        " ({})",
                        match state {
                            ArchiveActivity::Running => "running",
                            ArchiveActivity::NeedsInput => "waiting for input",
                        }
                    )
                })
                .unwrap_or_default();
            lines.push(format!("• {}{activity}", short_name(&agent.title)));
        }
        if others.len() > 6 {
            lines.push(format!("…and {} more", others.len() - 6));
        }
    }
    if plan.deletes_worktree {
        lines.push(String::new());
        lines.push("This also deletes the workspace's worktree.".into());
        let risks = &plan.worktree_risks;
        if risks.dirty {
            lines.push(match risks.diff {
                Some((additions, deletions)) => format!(
                    "Uncommitted changes ({additions} added lines, {deletions} deleted lines)"
                ),
                None => "Uncommitted changes".into(),
            });
        }
        if risks.unpushed > 0 {
            lines.push(format!("{} unpushed commits", risks.unpushed));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet::workspace_archive::{ArchiveAgent, WorktreeRisks};

    #[test]
    fn tab_origin_requires_closing_the_selected_agent_but_cards_do_not() {
        let tab = AgentCloseOrigin::Tab("original:key".into());
        assert!(!tab.should_archive("a", None));
        assert!(!tab.should_archive("a", Some(&Closed::Terminal("a".into()))));
        assert!(!tab.should_archive("a", Some(&Closed::Agent("other".into()))));
        assert!(tab.should_archive("a", Some(&Closed::Agent("a".into()))));
        assert!(AgentCloseOrigin::Card.should_archive("a", None));
    }

    #[test]
    fn tab_origin_preserves_the_exact_key_and_card_origin_uses_selection_keys() {
        let tab = AgentCloseOrigin::Tab("original:key".into());
        assert_eq!(tab.tab_key("mac", None).as_deref(), Some("original:key"));
        assert_eq!(
            tab.tab_key("mac", Some("changed")).as_deref(),
            Some("original:key")
        );
        assert_eq!(
            AgentCloseOrigin::Card.tab_key("mac", Some("w")),
            Some(
                Selection {
                    host: "mac".into(),
                    workspace: "w".into(),
                }
                .key()
            )
        );
        assert_eq!(AgentCloseOrigin::Card.tab_key("mac", None), None);
    }

    fn close_fixture() -> fleet::Fleet {
        let mut fleet = fleet::Fleet::new([fleet::HostConfig {
            name: "mac".into(),
            ..Default::default()
        }]);
        fleet
            .host_mut("mac")
            .unwrap()
            .directory
            .agents
            .insert("a".into(), close_record("a", "w", false));
        fleet
    }

    fn close_record(id: &str, workspace: &str, running: bool) -> fleet::Agent {
        fleet::Agent::from_value(serde_json::json!({
            "id": id, "workspaceId": workspace, "status": if running { "running" } else { "done" },
            "activeTurn": if running { serde_json::json!({"phase":"open"}) } else { serde_json::json!(null) }
        })).unwrap()
    }

    #[test]
    fn close_confirmation_detects_new_activity_and_workspace_changes() {
        let mut fleet = close_fixture();
        let shown = AgentCloseState::read(&fleet, "mac", "a").unwrap();
        assert!(!shown.running);
        let directory = &mut fleet.host_mut("mac").unwrap().directory;
        directory
            .agents
            .insert("a".into(), close_record("a", "w", true));
        let working = AgentCloseState::read(&fleet, "mac", "a").unwrap();
        assert!(working.running);
        assert_ne!(shown, working);
        fleet
            .host_mut("mac")
            .unwrap()
            .directory
            .agents
            .insert("a".into(), close_record("a", "other", false));
        assert_ne!(shown, AgentCloseState::read(&fleet, "mac", "a").unwrap());
    }

    #[test]
    fn close_confirmation_counts_live_siblings_including_child_agents() {
        let mut fleet = close_fixture();
        let shown = AgentCloseState::read(&fleet, "mac", "a").unwrap();
        assert!(shown.last);
        let mut child = close_record("child", "w", false);
        child.parent_agent_id = Some("a".into());
        fleet
            .host_mut("mac")
            .unwrap()
            .directory
            .agents
            .insert("child".into(), child);
        let changed = AgentCloseState::read(&fleet, "mac", "a").unwrap();
        assert!(!changed.last);
        assert_ne!(shown, changed);
        fleet
            .host_mut("mac")
            .unwrap()
            .directory
            .agents
            .get_mut("child")
            .unwrap()
            .archived_at = Some("archived".into());
        assert_eq!(shown, AgentCloseState::read(&fleet, "mac", "a").unwrap());
    }

    #[test]
    fn already_archived_and_missing_agents_have_no_remote_close_target() {
        let mut fleet = close_fixture();
        fleet
            .host_mut("mac")
            .unwrap()
            .directory
            .agents
            .get_mut("a")
            .unwrap()
            .archived_at = Some("archived".into());
        assert!(AgentCloseState::read(&fleet, "mac", "a").is_none());
        fleet.host_mut("mac").unwrap().directory.agents.remove("a");
        assert!(AgentCloseState::read(&fleet, "mac", "a").is_none());
        assert!(AgentCloseState::read(&fleet, "missing-host", "a").is_none());
    }

    #[test]
    fn warning_counts_all_agents_names_others_and_keeps_worktree_risks() {
        let mut agents = vec![ArchiveAgent {
            id: "source".into(),
            title: "Originating agent".into(),
            activity: None,
        }];
        agents.extend((0..8).map(|index| ArchiveAgent {
            id: format!("other-{index}"),
            title: format!("Other {index}"),
            activity: Some(ArchiveActivity::Running),
        }));
        let plan = ArchivePlan {
            host: "mac".into(),
            workspace: "w".into(),
            name: "Workspace".into(),
            source_agent: Some("source".into()),
            agents,
            deletes_worktree: true,
            worktree_risks: WorktreeRisks {
                dirty: true,
                diff: None,
                unpushed: 2,
            },
        };
        let body = confirmation_body(&plan);
        assert!(body.contains("all 9"));
        assert!(body.contains("Other agents in this workspace (8)"));
        assert!(body.contains("Including: Originating agent"));
        assert!(body.contains("Other 0 (running)"));
        assert!(!body.contains("Other 6"));
        assert!(body.contains("and 2 more"));
        assert!(body.contains("deletes the workspace's worktree"));
        assert!(body.contains("Uncommitted changes"));
        assert!(body.contains("2 unpushed commits"));
        let direct = ArchivePlan {
            source_agent: None,
            ..plan
        };
        let body = confirmation_body(&direct);
        assert!(body.contains("Originating agent"));
        assert!(body.contains("and 3 more"));
    }

    #[test]
    fn empty_single_agent_and_long_multiline_titles_remain_clear() {
        let mut plan = ArchivePlan {
            host: "mac".into(),
            workspace: "w".into(),
            name: "Workspace".into(),
            source_agent: None,
            agents: vec![],
            deletes_worktree: false,
            worktree_risks: WorktreeRisks::default(),
        };
        let empty = confirmation_body(&plan);
        assert!(!empty.contains("0 of"));
        assert!(!empty.contains("worktree"));
        plan.agents.push(ArchiveAgent {
            id: "a".into(),
            title: "Single agent".into(),
            activity: Some(ArchiveActivity::NeedsInput),
        });
        let single = confirmation_body(&plan);
        assert!(single.contains("and its agent"));
        assert!(single.contains("Single agent (waiting for input)"));
        assert!(single.contains("will be stopped"));
        assert_eq!(short_name("  first\n\tsecond  "), "first second");
        let bounded = short_name(&"界".repeat(100));
        assert_eq!(bounded.chars().count(), 65);
        assert!(bounded.ends_with('…'));
    }
}
