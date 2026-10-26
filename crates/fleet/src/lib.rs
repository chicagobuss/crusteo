//! Fleet state: what every connected daemon has told us, merged.
//!
//! Nothing here does I/O or knows about GPUI. A driver feeds daemon replies
//! and pushes into [`Directory`]; views read [`Fleet`] and its projections.
//! That keeps the reducers testable against captured frames.

pub mod agent_messages;
pub mod clock;
pub mod creation;
pub mod directory;
pub mod display;
pub mod images;
pub mod labels;
pub mod lanes;
pub mod preview;
pub mod project_remove;
pub mod prompt;
pub mod prompt_index;
pub mod providers;
pub mod questions;
pub mod requests;
pub mod sidebar;
pub mod state_list;
pub mod subagents;
pub mod timeline;
pub mod workspace_archive;

pub use directory::{DirChange, Directory, LabelDef};
pub use lanes::{Lane, LaneCard, LaneGrouping, LaneState, LaneView, LaneWorkspace};
pub use paseo_client::proto::{Agent, Bucket, Project, Workspace};
pub use sidebar::{SidebarProject, SidebarRow, SidebarView};

/// Which saved host something belongs to: a local id, stable while the host
/// is renamed, never a display name. Hosts saved before ids existed got their
/// name of the time as their id (see [`assign_ids`]), so state saved under
/// that name still finds them; hosts added later get a generated `hst_…`.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct HostId(String);

impl HostId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// A fresh id, `hst_` and 16 hex digits.
    pub fn generate() -> Self {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        hasher.write_u128(nanos);
        Self(format!("hst_{:016x}", hasher.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Gives every host without an id one, and a fresh one to any host whose id
/// another host already has. A host saved before ids existed takes its name,
/// so what was saved under that name still applies to it. Returns whether
/// anything changed, so the caller saves the list back.
pub fn assign_ids(configs: &mut [HostConfig]) -> bool {
    let mut taken = std::collections::HashSet::new();
    let mut changed = false;
    for index in 0..configs.len() {
        let id = &configs[index].id;
        if !id.is_empty() && taken.insert(id.clone()) {
            continue;
        }
        let name = HostId::new(configs[index].name.clone());
        let named_elsewhere =
            (configs.iter().enumerate()).any(|(other, config)| other != index && config.id == name);
        let id = if configs[index].id.is_empty()
            && !name.is_empty()
            && !named_elsewhere
            && !taken.contains(&name)
        {
            name
        } else {
            std::iter::repeat_with(HostId::generate)
                .find(|id| !taken.contains(id) && configs.iter().all(|config| &config.id != id))
                .expect("a fresh id")
        };
        taken.insert(id.clone());
        configs[index].id = id;
        changed = true;
    }
    changed
}

/// A daemon the user has saved. Crusteo owns this list; daemons do not know
/// about each other.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct HostConfig {
    /// Missing in files written before ids existed; [`assign_ids`] fills it.
    #[serde(default, skip_serializing_if = "HostId::is_empty")]
    pub id: HostId,
    /// What the host is called on screen; the user may change it.
    pub name: String,
    pub endpoint: Option<String>,
    /// For daemons that require one; most fleets on a private network do not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connection {
    #[default]
    Idle,
    Connecting,
    Online,
    Offline(String),
    /// The daemon refused our hello (password).
    Refused(String),
}

impl Connection {
    pub fn label(&self) -> &str {
        match self {
            Connection::Idle => "Idle",
            Connection::Connecting => "Connecting",
            Connection::Online => "Online",
            Connection::Offline(_) => "Offline",
            Connection::Refused(_) => "Error",
        }
    }

    pub fn is_online(&self) -> bool {
        matches!(self, Connection::Online)
    }
}

/// One saved host and everything it has reported.
#[derive(Debug, Clone, Default)]
pub struct Host {
    pub config: HostConfig,
    pub connection: Connection,
    pub server: Option<paseo_client::ServerInfo>,
    pub directory: Directory,
}

impl Host {
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    pub fn id(&self) -> &HostId {
        &self.config.id
    }

    /// For display only; identify a host by [`Host::id`].
    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn has(&self, feature: &str) -> bool {
        self.server
            .as_ref()
            .is_some_and(|server| server.has(feature))
    }
}

/// Every saved host, in the user's order.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    pub hosts: Vec<Host>,
    /// In reef mode, the fleet's label catalog: labels then belong to
    /// agents (see [`labels`]). None in Paseo-compatible mode, where they
    /// belong to workspaces.
    pub labels: Option<labels::Catalog>,
}

impl Fleet {
    /// The saved hosts; any without an id get one (see [`assign_ids`]).
    pub fn new(configs: impl IntoIterator<Item = HostConfig>) -> Self {
        let mut configs: Vec<HostConfig> = configs.into_iter().collect();
        assign_ids(&mut configs);
        Self {
            hosts: configs.into_iter().map(Host::new).collect(),
            labels: None,
        }
    }

    pub fn host(&self, id: &HostId) -> Option<&Host> {
        self.hosts.iter().find(|host| host.id() == id)
    }

    pub fn host_mut(&mut self, id: &HostId) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|host| host.id() == id)
    }

    /// The host shown as `name`, for input that only has a name: a host
    /// typed on a command line, or Skiff's protocol v0. Everything else goes
    /// by id.
    pub fn host_named(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.name() == name)
    }

    /// The display name of host `id`, or the id itself when it is gone.
    pub fn host_name<'a>(&'a self, id: &'a HostId) -> &'a str {
        self.host(id).map_or(id.as_str(), Host::name)
    }

    pub fn workspace(&self, host: &HostId, workspace_id: &str) -> Option<&Workspace> {
        self.host(host)?.directory.workspaces.get(workspace_id)
    }

    pub fn agent(&self, host: &HostId, agent_id: &str) -> Option<&Agent> {
        self.host(host)?.directory.agents.get(agent_id)
    }

    /// The one host that knows agent `agent_id`, for a source that names no
    /// host. None when no host or more than one does: the same id on two
    /// hosts (a daemon saved twice, agents imported from Paseo) is not a
    /// guess to make.
    pub fn sole_host_of(&self, agent_id: &str) -> Option<&Host> {
        let mut hosts =
            (self.hosts.iter()).filter(|host| host.directory.agents.contains_key(agent_id));
        let host = hosts.next()?;
        hosts.next().is_none().then_some(host)
    }

    /// Every agent across the fleet with the host it lives on.
    pub fn agents(&self) -> impl Iterator<Item = (&Host, &Agent)> {
        self.hosts.iter().flat_map(|host| {
            host.directory
                .agents
                .values()
                .map(move |agent| (host, agent))
        })
    }

    /// Every agent the views list: all of them, or with `hide_background`,
    /// all but those in background workspaces.
    pub fn shown_agents(&self, hide_background: bool) -> impl Iterator<Item = (&Host, &Agent)> {
        self.agents()
            .filter(move |(host, agent)| !(hide_background && host.directory.in_background(agent)))
    }

    /// Counts per bucket across every online host, for fleet summaries.
    /// Agents, not archived, by state: what Lanes shows as cards, so with
    /// `hide_background` those in background workspaces are not counted.
    pub fn agent_bucket_counts(&self, hide_background: bool) -> [(Bucket, usize); 5] {
        let mut counts = Bucket::GROUP_ORDER.map(|bucket| (bucket, 0usize));
        for (_, agent) in self.shown_agents(hide_background) {
            if agent.archived_at.is_some() {
                continue;
            }
            let bucket = agent.bucket();
            if let Some(slot) = counts.iter_mut().find(|(b, _)| *b == bucket) {
                slot.1 += 1;
            }
        }
        counts
    }

    /// Workspaces by the state Paseo reports for them, else their newest
    /// agent's.
    pub fn bucket_counts(&self) -> [(Bucket, usize); 5] {
        let mut counts = Bucket::GROUP_ORDER.map(|bucket| (bucket, 0usize));
        for host in &self.hosts {
            let newest_roots = host.directory.newest_roots();
            for workspace in host.directory.workspaces.values() {
                let bucket = host
                    .directory
                    .effective_bucket_with(workspace, &newest_roots);
                if let Some(slot) = counts.iter_mut().find(|(b, _)| *b == bucket) {
                    slot.1 += 1;
                }
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostId;
    use serde_json::json;

    #[test]
    fn hiding_background_drops_its_agents_from_counts() {
        let mut fleet = Fleet::new([HostConfig {
            name: "a".into(),
            ..Default::default()
        }]);
        let dir = &mut fleet.hosts[0].directory;
        for (id, background) in [("w1", false), ("w2", true)] {
            dir.apply_push(&json!({"type": "workspace_update", "payload": {"kind": "upsert",
                "workspace": {"id": id, "projectId": "p", "status": "running", "background": background}}}));
        }
        for (id, workspace) in [("a1", Some("w1")), ("a2", Some("w2")), ("a3", None)] {
            dir.apply_push(
                &json!({"type": "agent_update", "payload": {"kind": "upsert",
                "agent": {"id": id, "workspaceId": workspace, "status": "running"}}}),
            );
        }
        let running = |hide| fleet.agent_bucket_counts(hide)[3];
        assert_eq!(running(false), (Bucket::Running, 3));
        assert_eq!(running(true), (Bucket::Running, 2));
        let shown: Vec<&str> = fleet
            .shown_agents(true)
            .map(|(_, agent)| agent.id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(shown, ["a1", "a3"]);
        // Lookups by id still find a hidden agent.
        assert!(fleet.agent(&HostId::new("a"), "a2").is_some());
    }

    #[test]
    fn hosts_saved_before_ids_take_their_names_and_new_ones_get_fresh_ids() {
        let named = |name: &str| HostConfig {
            name: name.into(),
            ..Default::default()
        };
        // As an older hosts.json reads: names only.
        let mut configs = vec![named("devbox"), named("laptop")];
        assert!(assign_ids(&mut configs));
        assert_eq!(configs[0].id, HostId::new("devbox"));
        assert_eq!(configs[1].id, HostId::new("laptop"));
        assert!(!assign_ids(&mut configs), "nothing more to do");

        // Renamed, it keeps its id.
        configs[0].name = "buildbox".into();
        assert!(!assign_ids(&mut configs));
        assert_eq!(configs[0].id, HostId::new("devbox"));

        // A host added later, even with an old host's name, gets a fresh
        // id, so state saved for the old one does not attach to it.
        configs.push(named("devbox"));
        assert!(assign_ids(&mut configs));
        assert!(
            configs[2].id.as_str().starts_with("hst_"),
            "{}",
            configs[2].id
        );

        // Ids written in the file stay; a copied one is replaced.
        let mut copied = configs.clone();
        copied.push(HostConfig {
            id: HostId::new("laptop"),
            ..named("copy")
        });
        assert!(assign_ids(&mut copied));
        assert_eq!(&copied[..3], &configs[..]);
        assert!(copied[3].id.as_str().starts_with("hst_"));

        // Saved with the id, read back the same.
        let json = serde_json::to_value(&configs[1]).unwrap();
        assert_eq!(
            json,
            json!({"id": "laptop", "name": "laptop", "endpoint": null})
        );
        let back: HostConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, configs[1]);
    }

    #[test]
    fn an_agent_id_on_two_hosts_has_no_sole_host() {
        let mut fleet = Fleet::new(["a", "b"].map(|name| HostConfig {
            name: name.into(),
            ..Default::default()
        }));
        let add = |fleet: &mut Fleet, host: usize, id: &str| {
            fleet.hosts[host]
                .directory
                .apply_push(&json!({"type": "agent_update",
                "payload": {"kind": "upsert", "agent": {"id": id, "status": "idle"}}}));
        };
        add(&mut fleet, 0, "only-on-a");
        add(&mut fleet, 0, "imported");
        add(&mut fleet, 1, "imported");
        assert_eq!(fleet.sole_host_of("only-on-a").map(Host::name), Some("a"));
        assert!(fleet.sole_host_of("imported").is_none());
        assert!(fleet.sole_host_of("unknown").is_none());
        // Asked with its host, each copy is found where it is.
        assert!(fleet.agent(&HostId::new("b"), "imported").is_some());
        assert!(fleet.agent(&HostId::new("b"), "only-on-a").is_none());
    }
}
