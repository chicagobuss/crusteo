//! Fleet state: what every connected daemon has told us, merged.
//!
//! Nothing here does I/O or knows about GPUI. A driver feeds daemon replies
//! and pushes into [`Directory`]; views read [`Fleet`] and its projections.
//! That keeps the reducers testable against captured frames.

pub mod directory;
pub mod display;
pub mod lanes;
pub mod prompt;
pub mod prompt_index;
pub mod providers;
pub mod requests;
pub mod sidebar;
pub mod subagents;
pub mod timeline;
pub mod workspace_archive;

pub use directory::{DirChange, Directory, LabelDef};
pub use lanes::{Lane, LaneCard, LaneGrouping, LaneState, LaneView, LaneWorkspace};
pub use paseo_client::proto::{Agent, Bucket, Project, Workspace};
pub use sidebar::{SidebarProject, SidebarRow, SidebarView};

/// A daemon the user has saved. Crusteo owns this list; daemons do not know
/// about each other.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct HostConfig {
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
}

impl Fleet {
    pub fn new(configs: impl IntoIterator<Item = HostConfig>) -> Self {
        Self {
            hosts: configs.into_iter().map(Host::new).collect(),
        }
    }

    pub fn host(&self, name: &str) -> Option<&Host> {
        self.hosts.iter().find(|host| host.name() == name)
    }

    pub fn host_mut(&mut self, name: &str) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|host| host.name() == name)
    }

    pub fn workspace(&self, host: &str, workspace_id: &str) -> Option<&Workspace> {
        self.host(host)?.directory.workspaces.get(workspace_id)
    }

    pub fn agent(&self, host: &str, agent_id: &str) -> Option<&Agent> {
        self.host(host)?.directory.agents.get(agent_id)
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

    /// Counts per bucket across every online host, for fleet summaries.
    /// Agents, not archived, by state: what Lanes shows as cards.
    pub fn agent_bucket_counts(&self) -> [(Bucket, usize); 5] {
        let mut counts = Bucket::GROUP_ORDER.map(|bucket| (bucket, 0usize));
        for (_, agent) in self.agents() {
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
