//! Agent timelines: history reads and live stream envelopes.
//!
//! A timeline is an ordered list of canonical rows per agent. Each row has a
//! `seq` that grows by one, starting at 1 within an `epoch`; the epoch is a
//! UUID that changes when the daemon rebuilds the timeline. History reads
//! return *projected* entries (tool-call lifecycles and text chunks folded
//! together), live `agent_stream` frames carry canonical rows.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::directory::AgentSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineCursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Tail,
    Before,
    After,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Projection {
    #[default]
    Projected,
    Canonical,
}

/// `FetchAgentTimelineRequestMessageSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTimelineRequest {
    pub request_id: String,
    pub agent_id: String,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub cursor: Option<TimelineCursor>,
    /// Projected entries, not seqs. 0 means all.
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub projection: Option<Projection>,
    #[serde(default)]
    pub merge_window: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeqRange {
    pub start_seq: u64,
    pub end_seq: u64,
}

/// `AgentTimelineEntryPayloadSchema`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEntry {
    pub provider: String,
    /// An `AgentTimelineItem`: `user_message`, `assistant_message`,
    /// `reasoning`, `tool_call`, `todo`, `error`, `notification`, `compaction`
    /// or `plugin`.
    pub item: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub seq_start: u64,
    pub seq_end: u64,
    pub source_seq_ranges: Vec<SeqRange>,
    /// Which folding rules merged rows into this entry:
    /// `assistant_merge`, `reasoning_merge`, `tool_lifecycle`, `identity`.
    pub collapsed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineWindow {
    pub min_seq: u64,
    pub max_seq: u64,
    pub next_seq: u64,
}

/// `FetchAgentTimelineResponseMessageSchema.payload`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchTimelineResponse {
    pub request_id: String,
    pub agent_id: String,
    pub agent: Option<AgentSnapshot>,
    pub direction: Direction,
    pub projection: Projection,
    pub epoch: String,
    pub reset: bool,
    pub stale_cursor: bool,
    pub gap: bool,
    pub window: TimelineWindow,
    pub start_cursor: Option<TimelineCursor>,
    pub end_cursor: Option<TimelineCursor>,
    pub has_older: bool,
    pub has_newer: bool,
    pub entries: Vec<TimelineEntry>,
    pub error: Option<String>,
}

/// `agent.timeline.set_subscription.request`: the complete desired set.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineSubscriptionRequest {
    pub request_id: String,
    pub agent_ids: Vec<String>,
}

/// The `agent_stream` payload around one stream event. `seq` and `epoch`
/// are present only for `event.type == "timeline"`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStream {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    pub agent_id: String,
    pub event: Value,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
}
