//! The lists behind the title bar's counts: every agent in one state, the
//! state's latest change first, with what each one is about.
//!
//! A list holds exactly the agents its count counts
//! ([`crate::Fleet::agent_bucket_counts`]). Building it only reads the
//! fleet; nothing here marks an agent seen.

use paseo_client::proto::{Agent, Bucket};

use crate::Fleet;

/// One agent in a state's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    pub host: crate::HostId,
    pub agent_id: String,
    /// Where opening it goes; an agent with none cannot be opened.
    pub workspace_id: Option<String>,
    pub title: String,
    /// Its workspace, else its project, else the last part of its
    /// directory.
    pub place: String,
    pub provider: String,
    pub model: Option<String>,
    /// When the agent last changed, which for an agent in a state it has
    /// not left is when it entered it or after.
    pub changed_at: Option<String>,
    pub background: bool,
    /// What it waits on or failed with, in a line: the pending question or
    /// permission, or the error. None leaves the line to its last reply.
    pub detail: Option<String>,
}

impl StateRow {
    /// `provider/model`, or whichever of them is known.
    pub fn model_label(&self) -> String {
        match &self.model {
            Some(model) if !self.provider.is_empty() => format!("{}/{model}", self.provider),
            Some(model) => model.clone(),
            None => self.provider.clone(),
        }
    }
}

/// The agents in `bucket`, as its count counts them: not archived, and with
/// `hide_background` none in background workspaces. Most recently changed
/// first, then by host and id, so the order never depends on how the
/// directory stores them.
pub fn rows(fleet: &Fleet, bucket: Bucket, hide_background: bool) -> Vec<StateRow> {
    let mut rows: Vec<StateRow> = fleet
        .shown_agents(hide_background)
        .filter(|(_, agent)| agent.archived_at.is_none() && agent.bucket() == bucket)
        .map(|(host, agent)| {
            let directory = &host.directory;
            let workspace =
                (agent.workspace_id.as_deref()).and_then(|id| directory.workspaces.get(id));
            let mut place = crate::lanes::workspace_name(workspace);
            if place.is_empty() {
                place = workspace
                    .and_then(|workspace| directory.projects.get(&workspace.project_id))
                    .map(|project| project.name())
                    .unwrap_or_else(|| {
                        let cwd = agent.cwd.trim_end_matches('/');
                        cwd.rsplit('/').next().unwrap_or(cwd).to_string()
                    });
            }
            StateRow {
                host: host.id().clone(),
                agent_id: agent.id.clone(),
                workspace_id: agent.workspace_id.clone(),
                title: agent.display_title(),
                place,
                provider: agent.provider_id().to_string(),
                model: agent.model_label().map(str::to_string),
                changed_at: agent.updated_at.clone(),
                background: directory.in_background(agent),
                detail: match bucket {
                    Bucket::NeedsInput => Some(ask(agent)),
                    Bucket::Failed => error(agent),
                    _ => None,
                },
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.changed_at
            .cmp(&a.changed_at)
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    rows
}

/// What an agent that needs input waits on, as the "Needs you" strip and
/// the cards' controls name it: the first question it asks, or the
/// permission and what it would run.
pub fn ask(agent: &Agent) -> String {
    let Some(request) = agent.pending_permissions.first() else {
        return "Needs permission".into();
    };
    let more = match agent.pending_permissions.len() {
        0 | 1 => String::new(),
        n => format!(" (+{} more)", n - 1),
    };
    if crate::questions::is_question(request) {
        let asked = crate::questions::parse(&request["input"])
            .and_then(|questions| questions.into_iter().next())
            .map(|question| question.question)
            .filter(|question| !question.trim().is_empty());
        return match asked {
            Some(question) => format!("Asks: {}{more}", one_line(&question)),
            None => format!("Has a question{more}"),
        };
    }
    // "Permission: Bash cargo test": the tool, and the first line of what
    // it would run, when the request says.
    let tool = request["title"]
        .as_str()
        .or(request["name"].as_str())
        .filter(|tool| !tool.trim().is_empty());
    let call = crate::display::permission_call(request);
    let what = crate::display::tool_display(&call, &agent.cwd)
        .summary
        .or_else(|| request["input"]["command"].as_str().map(str::to_string))
        .and_then(|summary| {
            let line = summary
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())?;
            Some(one_line(line)).filter(|line| Some(line.as_str()) != tool)
        });
    let asked = [tool.map(str::to_string), what]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if asked.is_empty() {
        format!("Permission{more}")
    } else {
        format!("Permission: {asked}{more}")
    }
}

/// The error a failed agent's last turn ended with, in a line, when its
/// daemon says.
pub fn error(agent: &Agent) -> Option<String> {
    agent.raw["lastError"]
        .as_str()
        .map(crate::preview::first_line)
        .filter(|line| !line.is_empty())
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostConfig;
    use crate::HostId;
    use serde_json::{Value, json};

    fn fleet(agents: &[Value]) -> Fleet {
        let mut fleet = Fleet::new(["alpha", "beta"].map(|name| HostConfig {
            name: name.into(),
            ..Default::default()
        }));
        for host in &mut fleet.hosts {
            for (id, background) in [("w1", false), ("w2", true)] {
                host.directory.apply_push(&json!({"type": "workspace_update", "payload": {
                    "kind": "upsert", "workspace": {"id": id, "projectId": "p", "name": format!("{id}-name"),
                    "status": "done", "background": background}}}));
            }
        }
        for agent in agents {
            let host = agent["host"].as_str().unwrap_or("alpha");
            let host = fleet.host_mut(&HostId::new(host)).unwrap();
            host.directory.apply_push(
                &json!({"type": "agent_update", "payload": {"kind": "upsert", "agent": agent}}),
            );
        }
        fleet
    }

    fn agent(id: &str, updated: &str, extra: Value) -> Value {
        let mut agent = json!({"id": id, "workspaceId": "w1", "title": id, "provider": "claude",
            "model": "opus", "status": "idle", "updatedAt": updated});
        for (key, value) in extra.as_object().unwrap() {
            agent[key] = value.clone();
        }
        agent
    }

    fn ready() -> Value {
        json!({"requiresAttention": true, "attentionReason": "finished"})
    }

    fn ids(rows: &[StateRow]) -> Vec<&str> {
        rows.iter().map(|row| row.agent_id.as_str()).collect()
    }

    #[test]
    fn each_list_holds_its_state_newest_change_first() {
        let fleet = fleet(&[
            agent("old", "2026-10-01T00:00:00Z", ready()),
            agent("new", "2026-10-03T00:00:00Z", ready()),
            agent("mid", "2026-10-02T00:00:00Z", ready()),
            agent("busy", "2026-10-04T00:00:00Z", json!({"status": "running"})),
            agent(
                "broke",
                "2026-10-02T00:00:00Z",
                json!({"status": "error", "lastError": "Provider quit\nat line 3"}),
            ),
            agent("done", "2026-10-05T00:00:00Z", json!({})),
            agent(
                "asks",
                "2026-10-01T00:00:00Z",
                json!({"pendingPermissions": [{"id": "p1", "name": "Bash",
                "title": "Bash", "detail": {"type": "shell", "command": "cargo test"}}]}),
            ),
        ]);
        let ready_rows = rows(&fleet, Bucket::Attention, false);
        assert_eq!(ids(&ready_rows), ["new", "mid", "old"]);
        assert!(ready_rows.iter().all(|row| row.detail.is_none()));
        assert_eq!(ids(&rows(&fleet, Bucket::Running, false)), ["busy"]);
        let failed = rows(&fleet, Bucket::Failed, false);
        assert_eq!(failed[0].detail.as_deref(), Some("Provider quit"));
        let needs = rows(&fleet, Bucket::NeedsInput, false);
        assert_eq!(ids(&needs), ["asks"]);
        assert_eq!(
            needs[0].detail.as_deref(),
            Some("Permission: Bash cargo test")
        );
        assert_eq!(needs[0].place, "w1-name");
        assert_eq!(needs[0].model_label(), "claude/opus");
    }

    #[test]
    fn ties_order_by_host_then_id_and_undated_agents_go_last() {
        let same = "2026-10-02T00:00:00Z";
        let mut on_beta = agent("a", same, ready());
        on_beta["host"] = json!("beta");
        let mut undated = agent("z0", same, ready());
        undated.as_object_mut().unwrap().remove("updatedAt");
        let fleet = fleet(&[
            agent("b", same, ready()),
            on_beta,
            agent("a", same, ready()),
            undated,
        ]);
        let rows = rows(&fleet, Bucket::Attention, false);
        let keys: Vec<(&str, &str)> = (rows.iter())
            .map(|row| (row.host.as_str(), row.agent_id.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("alpha", "a"),
                ("alpha", "b"),
                ("beta", "a"),
                ("alpha", "z0")
            ]
        );
    }

    #[test]
    fn a_list_holds_what_its_count_counts() {
        let mut archived = agent("gone", "2026-10-03T00:00:00Z", ready());
        archived["archivedAt"] = json!("2026-10-04T00:00:00Z");
        let mut hidden = agent("hidden", "2026-10-03T00:00:00Z", ready());
        hidden["workspaceId"] = json!("w2");
        let fleet = fleet(&[
            agent("seen", "2026-10-02T00:00:00Z", ready()),
            archived,
            hidden,
        ]);
        for hide in [false, true] {
            for (bucket, count) in fleet.agent_bucket_counts(hide) {
                assert_eq!(
                    rows(&fleet, bucket, hide).len(),
                    count,
                    "{bucket:?} hide={hide}"
                );
            }
        }
        let shown = rows(&fleet, Bucket::Attention, false);
        assert_eq!(ids(&shown), ["hidden", "seen"]);
        assert!(shown[0].background);
        assert_eq!(ids(&rows(&fleet, Bucket::Attention, true)), ["seen"]);
    }

    #[test]
    fn building_a_list_leaves_every_agent_as_it_was() {
        let fleet = fleet(&[
            agent("r1", "2026-10-02T00:00:00Z", ready()),
            agent("r2", "2026-10-03T00:00:00Z", ready()),
        ]);
        let before: Vec<Agent> = fleet.agents().map(|(_, agent)| agent.clone()).collect();
        for bucket in Bucket::GROUP_ORDER {
            rows(&fleet, bucket, false);
        }
        let after: Vec<Agent> = fleet.agents().map(|(_, agent)| agent.clone()).collect();
        assert_eq!(before, after);
        assert_eq!(rows(&fleet, Bucket::Attention, false).len(), 2);
        // The only thing a list asks a daemon for is a read of a timeline's
        // end: no subscription, and nothing that clears attention.
        let request = crate::requests::last_reply("r1");
        assert_eq!(request["type"], "fetch_agent_timeline_request");
        assert_eq!(request["direction"], "tail");
        assert!(request.get("subscribe").is_none());
        assert!(!request.to_string().contains("attention"));
    }

    #[test]
    fn questions_and_permissions_read_as_one_line() {
        let question = json!({"id": "q", "kind": "question", "name": "AskUserQuestion", "title": "Question",
            "input": {"questions": [{"id": "0", "header": "Scope", "question": "Resume the\nwhole run?",
                "options": [{"label": "Yes"}]}]}});
        let mut asking = Agent {
            pending_permissions: vec![question, json!({"id": "p2", "name": "Bash"})],
            ..Default::default()
        };
        assert_eq!(ask(&asking), "Asks: Resume the whole run? (+1 more)");
        asking.pending_permissions = vec![json!({"id": "p", "name": "Edit"})];
        assert_eq!(ask(&asking), "Permission: Edit");
        // A command without a typed detail, over several lines: its first.
        asking.pending_permissions = vec![json!({"id": "p", "name": "Bash",
            "input": {"command": "make deploy-preview\necho done"}})];
        assert_eq!(ask(&asking), "Permission: Bash make deploy-preview");
        // Nothing but an id: the word alone.
        asking.pending_permissions = vec![json!({"id": "p"})];
        assert_eq!(ask(&asking), "Permission");
        asking.pending_permissions.clear();
        assert_eq!(ask(&asking), "Needs permission");
        assert_eq!(error(&asking), None);
    }
}
