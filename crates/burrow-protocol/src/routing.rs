//! Burrow's own messages between a Burrow and the Reef that routes for it,
//! so an agent can message an agent on another host. Paseo has none of
//! these; a Burrow says it takes them with the `burrowFleetRouting` feature
//! ([`crate::server_info::feature::FLEET_ROUTING`]), and a Reef sends them to
//! no other daemon.
//!
//! Routing is off until the Reef's owner turns it on, fleet-wide. The Reef
//! tells each Burrow which, on connect and whenever it changes:
//!
//! * Reef → Burrow, [`ROUTER`] (no reply):
//!   `{type, reefId, routing: "on" | "off"}`.
//! * Burrow → Reef, [`REQUEST`] (an outbound session message):
//!   `{type, payload: {requestId, op, ...}}`. `op` is one of [`op`]'s:
//!   [`op::ROUTE_PROMPT`] (`targetAgentId`, `prompt`, `sourceAgentId`,
//!   `sourceServerId`, `activeTurnBehavior`, `notifyOnFinish`);
//!   [`op::LIST_HOSTS`] (no arguments; the result is Paseo's host directory,
//!   `{hosts: [...]}`, without the asking host); [`op::LIST_AGENTS`] (`args`,
//!   `list_agents`'s arguments; the result is `{agents, hosts}`, every
//!   connected Burrow's agents with their `host` and `serverId`, and each
//!   host asked with whether it answered); [`op::TOOL`] (`tool`, `args`, and
//!   `targetAgentId` or `serverId`: one of [`TOOLS`] run on the agent's or
//!   the server's host; the result is the tool's own, with `host` and
//!   `serverId`).
//! * Reef → Burrow, [`TOOL_REQUEST`] (a request): `{type, requestId, tool,
//!   args}`, one of [`TOOLS`] for this host to run as if no agent called it.
//!   Burrow answers [`TOOL_RESPONSE`] `{requestId, ok, result | error}`.
//! * Reef → Burrow, [`RESPONSE`] (no reply): `{type, requestId, ok,
//!   result | code, error}`, with `code` one of [`code`]'s.
//! * Reef → Burrow, [`FINISHED`] (a request): a routed prompt's turn ended
//!   or needs permission; `{type, requestId, noticeId, callerAgentId,
//!   agentId, title, serverId, host, reason, lastMessage, permission}`.
//!   Burrow answers [`FINISHED_RESPONSE`] `{requestId, delivered, gone,
//!   error}` once the notice is with the caller, or the caller is gone; the
//!   Reef keeps it and tries again until then.

/// The feature a Burrow advertises when it takes these.
pub use crate::server_info::feature::FLEET_ROUTING as FEATURE;

pub const ROUTER: &str = "burrow.fleet.router";
pub const REQUEST: &str = "burrow.fleet.request";
pub const RESPONSE: &str = "burrow.fleet.response";
pub const FINISHED: &str = "burrow.fleet.route_finished.request";
pub const FINISHED_RESPONSE: &str = "burrow.fleet.route_finished.response";
pub const TOOL_REQUEST: &str = "burrow.fleet.tool.request";
pub const TOOL_RESPONSE: &str = "burrow.fleet.tool.response";
/// A client of this Burrow (`burrow ls --fleet`) asks for every host's
/// agents: `{type, requestId, args}` with `list_agents`'s arguments. The
/// response's payload is `{requestId, ok, result | error}`, the result as
/// [`op::LIST_AGENTS`]'s.
pub const FLEET_AGENTS_REQUEST: &str = "burrow.fleet.agents.request";
pub const FLEET_AGENTS_RESPONSE: &str = "burrow.fleet.agents.response";

/// The agent tools one host runs for an agent on another, through the Reef:
/// the reads a finish notice points to, and stopping a turn or answering a
/// permission. Nothing that makes, archives or changes an agent.
pub const TOOLS: &[&str] = &[
    "list_agents",
    "get_agent_status",
    "get_agent_activity",
    "cancel_agent",
    "respond_to_permission",
];

/// What a Burrow asks its Reef.
pub mod op {
    /// Deliver a prompt to an agent on another host.
    pub const ROUTE_PROMPT: &str = "route_prompt";
    /// The fleet's other hosts.
    pub const LIST_HOSTS: &str = "list_hosts";
    /// Every host's agents.
    pub const LIST_AGENTS: &str = "list_agents";
    /// Run one of [`super::TOOLS`] on another host.
    pub const TOOL: &str = "tool";
}

/// Why a Reef refused.
pub mod code {
    /// The Reef's owner has not turned routing on.
    pub const OFF: &str = "routing_off";
    /// No agent in the fleet goes by that id, prefix or name.
    pub const NOT_FOUND: &str = "not_found";
    /// More than one agent goes by that name; `candidates` lists them.
    pub const AMBIGUOUS: &str = "ambiguous";
    /// The target's host is not connected to the Reef now.
    pub const HOST_OFFLINE: &str = "host_offline";
    /// The target's daemon refused or failed the prompt.
    pub const REFUSED: &str = "refused";
    /// Too many routed prompts from one agent to another just now.
    pub const RATE_LIMITED: &str = "rate_limited";
}
