# Paseo agent conversation: data model, timeline, rendering, composer and lifecycle

Scope: everything a native client needs to show and drive one agent conversation the way the Paseo
app does. That covers the agent directory model, the per-agent timeline (fetch, live stream, the
client reducer), tool-call rendering, composer actions (send, interrupt, queue, permissions,
mode/model/thinking, slash commands) and agent lifecycle (create, archive, rename).

Out of scope, and covered in [`transport.md`](./transport.md): the WebSocket URL, `hello`,
`server_info`, the `{"type":"session","message":…}` envelope, `requestId` correlation, `rpc_error`,
keepalive, owned-subscription mechanics, directory-sync cursors and reconnect. Cross-references
below look like "transport §4.6".

Sources: Paseo tag `v0.10.1` at `/tmp/paseo-v0101`. Paths are relative to `packages/`.

| Concern | File |
|---|---|
| Wire schemas (zod) | `protocol/src/messages.ts` (line refs below are into this file unless stated) |
| Agent lifecycle enum, state buckets | `protocol/src/agent-lifecycle.ts`, `protocol/src/agent-state-bucket.ts` |
| Tool-call display names and summaries | `protocol/src/tool-call-display.ts` |
| Client stream reducer (core) | `app/src/types/stream.ts` |
| Fetch-page reducer, seq gate, frame batching | `app/src/timeline/session-stream-reducers.ts` |
| Fetch/subscribe orchestration | `app/src/timeline/viewed-timeline-sync.ts`, `timeline-sync-plan.ts`, `contexts/session-context.tsx` |
| Turn liveness ("is running", elapsed) | `app/src/timeline/turn-liveness.ts`, `turn-time.ts` |
| Directory projection of agent snapshots | `app/src/runtime/directory-sync/internal/agent-store.ts`, `utils/agent-snapshots.ts` |
| Stream view, permission cards | `app/src/agent-stream/view.tsx`, `components/message.tsx`, `components/tool-call-details.tsx` |
| Composer, queue | `app/src/composer/{index.tsx,actions.ts,input/state.ts}` |
| Server timeline store and projection | `server/src/server/agent/agent-timeline-store.ts`, `timeline-projection.ts` |

### Verification

On 2026-10-02 I drove an isolated throwaway daemon (`0.10.1`, `PASEO_HOME=/tmp/crusteo-testd`,
`ws://127.0.0.1:6790/ws`). The driver was a Node 22 script in `/tmp/crusteo-capture/`
(`lib.mjs`, `scenario*.mjs`; raw frames in `scenario*.jsonl`). It created, prompted and then
archived four agents: three `claude` / `claude-haiku-4-5` and one `codex` / `gpt-5.5`.

The runs covered:
- tool calls
- permission deny then allow
- plan-mode approval rejected
- interrupt by `cancel_agent_request` mid-stream
- interrupt by sending with `activeTurnBehavior:"interrupt"`
- mode, model and thinking switches
- rename
- clear attention
- timeline paging in all three directions

Frames tagged **[live]** are real and trimmed only where marked `…`. Frames tagged **[schema]** are
built from the zod schemas. No reasoning stream was produced by any model I tried, so `reasoning`
is documented from code only.

### Legend

- **E** (essential): needed for the client to feel like Paseo.
- **N** (nice-to-have): can come later.

---

## 1. Agent data model

### 1.1 `AgentSnapshotPayload` (messages.ts:809) — E

Every agent-bearing message carries this full snapshot, for example:
- `agent_update` upsert
- `fetch_agents_response.entries[].agent`
- `fetch_agent_response`
- `agent.create.response`
- `cancel_agent_response`
- `clear_agent_attention_response`
- `fetch_agent_timeline_response.agent`

There is no partial/patch form: always replace the whole record.

| Field | Type | Notes |
|---|---|---|
| `id` | string | UUID. |
| `provider` | string | `"claude"`, `"codex"`, `"opencode"`, `"pi"`, `"cursor"`, ACP ids… (`AgentProviderSchema = z.string()`). |
| `cwd` | string | Absolute working dir. |
| `workspaceId` | string? | Workspace linkage (`wks_…`). |
| `model` | string \| null | Selected model id; `null` = provider default. |
| `features` | `AgentFeature[]`? | `{type:"toggle",id,label,value:boolean,…}` or `{type:"select",id,label,value:string\|null,options[]}`. |
| `thinkingOptionId` | string \| null? | Selected thinking level. |
| `effectiveThinkingOptionId` | string \| null? | What the provider actually uses. |
| `createdAt`, `updatedAt` | ISO string | `updatedAt` is the conflict-resolution key (§1.6). |
| `lastUserMessageAt` | ISO \| null | |
| `status` | enum | `"initializing" \| "idle" \| "running" \| "error" \| "closed"` (`agent-lifecycle.ts`). |
| `activeTurn` | `{turnId, startedAt: ISO\|null}` \| null? | Present while a foreground turn is open. Drives the "working" footer and elapsed timer. |
| `capabilities` | object | `supportsStreaming`, `supportsSessionPersistence`, `supportsDynamicModes`, `supportsMcpServers`, `supportsReasoningStream`, `supportsToolInvocations`, `supportsRewind{Conversation,Files,Both}`, `supportsSessionListing?`, plus any extra booleans. |
| `currentModeId` | string \| null | |
| `availableModes` | `{id,label,description?,icon?,colorTier?}[]` | Mode picker source. |
| `pendingPermissions` | `AgentPermissionRequest[]` | Authoritative list of open prompts (§4.4). |
| `persistence` | `{provider,sessionId,nativeHandle?,metadata?}` \| null | Provider session handle. |
| `runtimeInfo` | `{provider,sessionId\|null,model?,thinkingOptionId?,modeId?,extra?}`? | |
| `lastUsage` | `{inputTokens?,cachedInputTokens?,outputTokens?,totalCostUsd?,contextWindowMaxTokens?,contextWindowUsedTokens?}`? | From the last `turn_completed`. Drives the context-usage indicator. |
| `lastError` | string? | |
| `title` | string \| null | Display name. The server auto-derives it from the first prompt when not set. `update_agent_request.name` overwrites it. |
| `labels` | `Record<string,string>` | Defaults to `{}`. `paseo.parent-agent-id` marks a subagent (`protocol/src/agent-labels.ts`). |
| `requiresAttention` | boolean? | The "unread" flag (§1.4). |
| `attentionReason` | `"finished"\|"error"\|"permission"` \| null? | |
| `attentionTimestamp` | ISO \| null? | |
| `archivedAt` | ISO \| null? | Non-null = archived. |
| `providerUnavailable` | boolean? | The provider CLI is not installed or enabled. |

**[live]** `fetch_agent_response.payload` (trimmed):

```json
{"requestId":"req-…-16",
 "agent":{"id":"d570fc52-2dfe-41ba-b92a-a4a8ba3c06c9","provider":"claude","cwd":"/tmp/crusteo-scratch",
  "workspaceId":"wks_8d1c0d93f6947859","model":"claude-haiku-4-5","thinkingOptionId":null,"effectiveThinkingOptionId":null,
  "runtimeInfo":{"provider":"claude","sessionId":"e278bce7-…","model":"claude-haiku-4-5","modeId":"default","extra":{"runtimeModel":"claude-haiku-4-5"}},
  "createdAt":"2026-10-02T06:57:18.364Z","updatedAt":"2026-10-02T06:57:41.059Z","lastUserMessageAt":"2026-10-02T06:57:36.953Z",
  "status":"idle","activeTurn":null,
  "capabilities":{"supportsStreaming":true,"supportsSessionPersistence":true,"supportsSessionListing":true,"supportsDynamicModes":true,
   "supportsMcpServers":true,"supportsReasoningStream":true,"supportsToolInvocations":true,
   "supportsRewindConversation":true,"supportsRewindFiles":true,"supportsRewindBoth":true},
  "currentModeId":"default",
  "availableModes":[{"id":"plan","label":"Plan Mode","description":"Analyze the codebase without executing tools or edits"},
   {"id":"default","label":"Always Ask","description":"Prompts for permission the first time a tool is used"},
   {"id":"acceptEdits","label":"Accept File Edits","description":"…"},{"id":"auto","label":"Auto mode","description":"…"},
   {"id":"bypassPermissions","label":"Bypass","description":"Skip all permission prompts (use with caution)"}],
  "features":[],"pendingPermissions":[],
  "persistence":{"provider":"claude","sessionId":"e278bce7-…","nativeHandle":"e278bce7-…","metadata":{"provider":"claude","cwd":"/tmp/crusteo-scratch","modeId":"default","model":"claude-haiku-4-5","title":"crusteo capture"}},
  "title":"crusteo capture renamed","labels":{"purpose":"capture"},
  "lastUsage":{"inputTokens":10,"cachedInputTokens":24810,"outputTokens":53,"totalCostUsd":0.03581485,"contextWindowMaxTokens":200000,"contextWindowUsedTokens":25032},
  "requiresAttention":true,"attentionReason":"finished","attentionTimestamp":"2026-10-02T06:57:24.849Z","archivedAt":null},
 "project":{"projectKey":"prj_a5d9c22a39d0cdc0","projectName":"crusteo-scratch","workspaceName":"crusteo-scratch",
  "checkout":{"cwd":"/tmp/crusteo-scratch","isGit":false,"currentBranch":null,"remoteUrl":null,"worktreeRoot":null,"isPaseoOwnedWorktree":false,"mainRepoRoot":null}},
 "error":null}
```

Mode `icon`/`colorTier` are absent on the agent snapshot here. The provider snapshot does carry them,
e.g. `"icon":"ShieldCheck","colorTier":"moderate"`. The app looks them up by mode id in the provider
snapshot.

`AgentListItemPayload` (messages.ts:842) is a slimmer legacy row with `shortId`. The legacy
broadcasts `agent_list` and `agent_status` (messages.ts:4072, 4081) predate directory
subscriptions. A modern client can ignore all three.

### 1.2 Status, turns and the sidebar state bucket — E

`status` is coarse. The app treats **turn liveness** as the source of truth for "is it running"
(`timeline/turn-liveness.ts`):

```
TurnLiveness = {phase:"idle", cancellationRequestId}
             | {phase:"open", turnId, startedAt, cancellationRequestId}
transitions:
  snapshot(activeTurn|null)   from every agent snapshot (activeTurn != null → open, else idle)
  stream_open(turnId,startedAt) on agent_stream turn_started   (startedAt = envelope timestamp)
  stream_close(turnId)          on turn_completed | turn_failed | turn_canceled
                                (ignored if both ids are non-null and differ: a stale close for an older turn)
  cancellation_started/settled  local, around cancel_agent_request (shows a "cancelling" state)
openTurn keeps the earlier startedAt when the same turnId re-opens.
```

`turnId` values are opaque strings: Claude sends `foreground-turn-1`, `foreground-turn-2`, …; Codex
sends `codex-turn-0` **[live]**.

The sidebar/tab status dot uses `deriveAgentStateBucket` (`protocol/src/agent-state-bucket.ts`).
Rules, applied in order:

| Bucket | Rule | Priority |
|---|---|---|
| `needs_input` | `pendingPermissions.length > 0` or `attentionReason == "permission"` | 0 |
| `failed` | `status == "error"` or `attentionReason == "error"` | 1 |
| `running` | `status == "running"` | 2 |
| `attention` | `requiresAttention` | 3 |
| `done` | anything else | 4 |

`getAgentStatusPriority` is the same, except `initializing` takes priority 3. The app sorts by
these priorities.

### 1.3 Workspace linkage — E

- An agent belongs to a workspace through `workspaceId`.
- Directory responses and `agent_update` upserts also carry `project`
  (`ProjectPlacementPayload`: `{projectKey, projectName, workspaceName?, checkout}`), which places
  the agent in the project → workspace tree.
- Creating an agent in a non-git cwd with no `workspaceId` makes the daemon mint a workspace for
  that directory **[live]**: `wks_8d1c…` for `/tmp/crusteo-scratch`.
- Workspaces themselves come from `fetch_workspaces_request` (see transport §4.3).

### 1.4 Attention ("unread") — E

- The server sets `requiresAttention=true` with `attentionReason` when a turn finishes, errors, or
  asks for permission. The flag is edge-triggered (`agent-manager.ts emitState → checkAndSetAttention`).
- It is announced three ways:
  1. an `agent_update` upsert with the flags set;
  2. `agent_attention_required` on the session event feed (subscribe to it via
     `session.events.set_subscription.request`, transport §4.4);
  3. (legacy) an `agent_stream` event of type `attention_required`.
- `shouldNotify` tells the client whether to raise an OS notification. The server decides this from
  `client_heartbeat.focusedAgentId` and the other connected clients' activity.

**[live]**

```json
{"type":"agent_attention_required","payload":{"agentId":"d570fc52-…","reason":"finished","timestamp":"2026-10-02T06:57:24.852Z",
 "shouldNotify":false,"notification":{"title":"Agent finished","body":"Working directory is empty, both commands executed successfully.",
 "data":{"serverId":"srv_example2","workspaceId":"wks_8d1c0d93f6947859","agentId":"d570fc52-…","reason":"finished"}},
 "subscriptionId":"7c1d46bb-…"}}
```

A permission prompt produced `{"reason":"permission","notification":{"title":"Agent needs permission","body":"<JSON of the tool input>",…}}`.

Clearing attention:
- Send `clear_agent_attention {agentId: string|string[], requestId?}`. The reply is
  `clear_agent_attention_response {requestId, agentId, agents: AgentSnapshot[]}`, followed by an
  `agent_update` with `requiresAttention:false` **[live]**.
- The app clears when the user focuses the agent's input, sends a prompt, or blurs the agent
  (`hooks/use-agent-attention-clear.ts`, `utils/agent-attention.ts`). It does this only while
  connected and `requiresAttention`, and **never when `attentionReason == "permission"`**: that
  clears itself when the permission is resolved.

### 1.5 Pending permissions on the snapshot — E

`pendingPermissions[]` is authoritative. On every upsert the app **replaces** that agent's entries
in its pending-permission map (`agent-store.ts replacePendingPermissions`). Live
`agent_permission_request` and `agent_permission_resolved` events add and remove entries between
snapshots, keyed by `${agentId}:${request.id}` (§4.4).

### 1.6 Getting the agent list and live updates — E

The app keeps one live subscription for the active directory
(`app/src/runtime/directory-sync/index.ts:624`):

**[schema]** Request, as the app sends it:

```json
{"type":"fetch_agents_request","requestId":"r1","scope":"active",
 "sort":[{"key":"updated_at","direction":"desc"}],"page":{"limit":200},
 "subscribe":{},"sync":{"generation":"…","afterSeq":42}}
```

- `scope:"active"` excludes archived agents.
- `filter` (`{labels?, projectKeys?, statuses?, includeArchived?, requiresAttention?, thinkingOptionId?}`)
  replaces `scope` when given.
- `sort.key` is one of `status_priority`, `created_at`, `updated_at`, `title`.
- `page.limit` is at most 200. Follow `pageInfo.nextCursor`, but only the first page subscribes.
- `sync` is sent only when `server_info.features.directorySync` (transport §4.5).

**[live]** Response:

```json
{"type":"fetch_agents_response","payload":{"requestId":"req-…-2","subscriptionId":"a09093f2-01e9-44fc-aba8-e69d4f681c4a",
 "entries":[],"pageInfo":{"nextCursor":null,"prevCursor":null,"hasMore":false}}}
```

Each entry is `{agent: AgentSnapshot, project: ProjectPlacement, syncSeq?}`.

**[live]** Live updates after that:

```json
{"type":"agent_update","payload":{"subscriptionId":"a09093f2-…","kind":"upsert","agent":{…AgentSnapshot…},"project":{…}}}
{"type":"agent_update","payload":{"subscriptionId":"a09093f2-…","kind":"remove","agentId":"d570fc52-…"}}
```

- `generation` and `seq` are only present when sync cursors are in use.
- Expect a lot of upserts. During one 7 s turn I saw 10: on `running`, on `thread_started`, on each
  tool update, and on idle. Some were byte-identical duplicates.
- Archiving an agent produces `remove` on an `active` subscription, delivered up to three times
  **[live]**.

Client rules (`agent-store.ts`, `utils/agent-directory-update-policy.ts`):
1. **Upsert ordering.** Accept the incoming snapshot only if `incoming.updatedAt >= current.updatedAt`.
   When it is older, keep the current record but still take `incoming.lastUsage` if it differs.
2. Merge turn liveness via `snapshot(activeTurn)`. Keep the earlier `startedAt` for the same `turnId`.
3. Replace the agent's pending permissions from the snapshot.
4. An upsert that moves the turn from open to idle triggers the composer queue drain (§4.3).
5. `remove` drops the agent from the directory and clears its queued messages and draft. The cached
   transcript is only destroyed on true deletion (`agent_deleted`).

Other list sources:
- **Archived/history list** (N): `fetch_agent_history_request {requestId, filter?, search?, sort?, page?}`
  → `fetch_agent_history_response {requestId, entries[], pageInfo, searchTruncated?}`. This is the
  "past chats" search, **not** the conversation history.
- **Single agent** (E): `fetch_agent_request {requestId, agentId}` (full id, unique prefix, or exact
  title) → `fetch_agent_response {requestId, agent|null, project?, error}`. Use it to open an
  agent that is not in the active list, e.g. an archived one.

---

## 2. The timeline

### 2.1 Concepts — E

- **Row and seq.** The daemon keeps one timeline per agent: an ordered list of `AgentTimelineItem`
  rows. Every live `timeline` event gets a monotonically increasing integer `seq`, starting at 1 per
  epoch. Turn and permission events have **no** seq.
- **Epoch.** A UUID naming the current timeline incarnation. If the cursor's epoch differs from the
  server's, the client's copy is invalid and must be replaced.
  - It changes when the daemon rebuilds the timeline, for example after a conversation rewind
    (`agent-manager.ts:3171`) or a reload from provider history.
- **Projection.** The server does not retain raw deltas. It folds them into **projected** rows as
  they arrive (`server/agent/timeline-projection.ts`), so `projection:"canonical"` and
  `"projected"` returned identical entries in testing **[live]**: 11 entries for 25 seqs. Folding
  rules:
  - `assistant_merge`: an `assistant_message` chunk is appended to the previous row when the seqs
    are adjacent, the `turnId` is the same, and `messageId` is equal or missing on the incoming chunk.
  - `reasoning_merge`: the same for `reasoning` chunks.
  - `tool_lifecycle`: all updates with the same `callId` (in the same `turnId`) merge into the row
    where the call **first** appeared. The merge keeps typed detail over `unknown`, merges
    `metadata`, and takes the latest status and error.
  - `identity`: plugin items with the same `pluginId/id` merge.
  - A projected entry spans `seqStart..seqEnd` and lists `sourceSeqRanges`, which can be
    non-contiguous for a tool call interleaved with others **[live]**: `[{2,3},{6,6}]`.
  - Entries are ordered by `seqStart`, i.e. where each item first appeared.
- **Live vs fetched.** Live `agent_stream` frames carry raw deltas: assistant text arrives as
  separate chunks, and each tool call arrives as several updates. A fetched page carries folded
  rows. The client reducer (§2.6) must produce the same picture from either source.

### 2.2 Subscribing to an agent's live stream — E

**[live]** Request:

```json
{"type":"agent.timeline.set_subscription.request","agentIds":["d570fc52-2dfe-41ba-b92a-a4a8ba3c06c9"],"requestId":"req-…-4"}
```

**[live]** Reply:

```json
{"type":"agent.timeline.set_subscription.response","payload":{"agentIds":["d570fc52-…"],"requestId":"req-…-4","subscriptionId":"36817ba9-281a-4c3f-b11b-fdd333ec6134"}}
```

- `agentIds` is the **complete desired set** for that subscription; re-send it to change the set.
  Release semantics are covered in transport §4.6.
- The app subscribes only to agents the user has opened. Agents stay subscribed while their tabs
  exist, even when hidden (`viewed-timeline-sync.ts`).
- Without the `selective_agent_timeline` hello capability, the daemon broadcasts every agent's
  stream.

**[live]** `agent_stream` envelope:

```json
{"type":"agent_stream","payload":{"agentId":"d570fc52-…","event":{"type":"timeline","provider":"claude",
 "item":{"type":"tool_call","callId":"toolu_01DQUxdEcQTMLwTiHskuGtff","name":"Bash","detail":{"type":"unknown","input":{},"output":null},"status":"running","error":null},
 "turnId":"foreground-turn-1"},
 "timestamp":"2026-10-02T06:57:23.186Z","seq":2,"epoch":"6298d5a9-b61d-453e-a209-a0858f159011","subscriptionId":"36817ba9-…"}}
```

- `seq` and `epoch` are present only for `event.type == "timeline"`.
- The app uses `timestamp` as the item time; it drives turn duration.

**[schema]** Timeline invalidation (E):

```json
{"type":"agent.timeline.replacement","payload":{"subscriptionId":"…","agentId":"…","epoch":"<new epoch>"}}
```

The app's response is to refetch the tail and **replace** the transcript:
`fetch_agent_timeline_request {direction:"tail",limit:40,projection:"projected"}`, with the result
treated as `reset:true` (`timeline/timeline-replacement.ts`, `consumeForcedTimelineTailReplacement`).

The client SDK also synthesizes two local, non-wire messages:
- `agent.timeline.subscription_restored`, after a reconnect re-establishes membership; the app
  then runs a catch-up;
- `agent.timeline.error`.

### 2.3 `AgentStreamEventPayload` — every variant (messages.ts:717) — E

| `type` | Fields | Has seq | Client effect |
|---|---|---|---|
| `thread_started` | `sessionId`, `provider` | no | Provider session bound. Finalize thoughts only. |
| `turn_started` | `provider`, `turnId?` | no | Turn liveness → open (`startedAt` = envelope timestamp). |
| `turn_completed` | `provider`, `turnId?`, `usage?` (AgentUsage) | no | Close turn; flush head; finalize thoughts. Usage lands on the next snapshot's `lastUsage`. |
| `turn_failed` | `provider`, `turnId?`, `error`, `code?`, `diagnostic?` | no | Close turn. The server **also appends** a timeline `assistant_message` whose text is `"[System Error] <error>\n\ncode: <code>\n\n<diagnostic>"` (`agent-manager.ts:4723`). That is what the user sees; it renders like a normal assistant message. Pending permissions are resolved with "Turn failed". |
| `turn_canceled` | `provider`, `turnId?`, `reason` | no | Close turn. Seen live: `reason:"Interrupted"`. Running tool calls get a `canceled` timeline update just before it. |
| `timeline` | `provider`, `item: AgentTimelineItem`, `turnId?` | **yes** | Reduce into the transcript (§2.6). |
| `permission_requested` | `provider`, `request: AgentPermissionRequest` | no | Duplicates `agent_permission_request` (§4.4). The reducer only finalizes thoughts. |
| `permission_resolved` | `provider`, `requestId`, `resolution: AgentPermissionResponse` | no | Duplicates `agent_permission_resolved`. |
| `attention_required` | `provider`, `reason`, `timestamp`, `shouldNotify`, `notification?` | no | Legacy path for §1.4. |

**[live]** Examples:

```json
{"type":"turn_started","provider":"claude","turnId":"foreground-turn-1"}
{"type":"thread_started","sessionId":"e278bce7-1552-4733-875c-7e722971647a","provider":"claude"}
{"type":"turn_completed","provider":"claude","turnId":"foreground-turn-1","usage":{"inputTokens":18,"cachedInputTokens":37576,"outputTokens":274,"totalCostUsd":0.02361435,"contextWindowMaxTokens":200000,"contextWindowUsedTokens":24128}}
{"type":"turn_canceled","provider":"claude","turnId":"foreground-turn-1","reason":"Interrupted"}
```

### 2.4 `AgentTimelineItem` — every variant (messages.ts:663) — E

| `item.type` | Fields | Identity (merge key) |
|---|---|---|
| `user_message` | `text`, `messageId?`, `clientMessageId?` | `clientMessageId`, then `messageId`, then (legacy) text |
| `assistant_message` | `text` (a **delta** when live, full text when fetched), `messageId?` (provider message id, e.g. `msg_011Cfcy…`) | Adjacent chunks with the same `messageId` |
| `reasoning` | `text` (delta when live) | Adjacent chunks |
| `tool_call` | `callId`, `name`, `detail: ToolCallDetail`, `status`, `error`, `metadata?` (§3.1) | `callId`, scoped by `turnId` |
| `todo` | `items: [{text, completed, id?, status?: "pending"\|"in_progress"\|"completed", activeForm?}]` | The latest todo list per provider |
| `error` | `message` | none |
| `notification` | `level: "info"\|"warning"\|"error"`, `message` | none |
| `compaction` | `status: "loading"\|"completed"`, `trigger?: "auto"\|"manual"`, `preTokens?` | `completed` updates the open `loading` row |
| `plugin` | `id`, `pluginId`, `kind`, `version`, `data` (JSON) | `${pluginId}/${id}` (N: plugin renderers) |

**[live]** A complete Claude turn, in delivery order (`agent_stream.event` only, with `seq`):

```
—    {"type":"turn_started","provider":"claude","turnId":"foreground-turn-1"}
1    {"type":"timeline","item":{"type":"user_message","text":"Use the Bash tool to run `ls -la`, then separately run `echo hi`. Then reply in one short line.","messageId":"8c81baa4-…","clientMessageId":"8c81baa4-…"},"turnId":"foreground-turn-1"}
—    {"type":"thread_started","sessionId":"e278bce7-…","provider":"claude"}
2    {"type":"timeline","item":{"type":"tool_call","callId":"toolu_01DQ…","name":"Bash","detail":{"type":"unknown","input":{},"output":null},"status":"running","error":null},…}
3    {"type":"timeline","item":{"type":"tool_call","callId":"toolu_01DQ…","name":"Bash","detail":{"type":"shell","command":"ls -la"},"status":"running","error":null},…}
4    {"type":"timeline","item":{"type":"tool_call","callId":"toolu_01XS…","name":"Bash","detail":{"type":"shell","command":"echo hi"},"status":"running","error":null},…}
5    {"type":"timeline","item":{"type":"tool_call","callId":"toolu_01XS…","name":"Bash","detail":{"type":"shell","command":"echo hi","output":"hi"},"status":"completed","error":null},…}
6    {"type":"timeline","item":{"type":"tool_call","callId":"toolu_01DQ…","name":"Bash","detail":{"type":"shell","command":"ls -la","output":"total 0\ndrwxrwxr-x  2 …"},"status":"completed","error":null},…}
7    {"type":"timeline","item":{"type":"assistant_message","text":"Working","messageId":"msg_011CfcyX73VjJtzu7anSvcQt"},…}
8    {"type":"timeline","item":{"type":"assistant_message","text":" directory is empty","messageId":"msg_011CfcyX73VjJtzu7anSvcQt"},…}
9    {"type":"timeline","item":{"type":"assistant_message","text":", both commands","messageId":"msg_011CfcyX73VjJtzu7anSvcQt"},…}
10   {"type":"timeline","item":{"type":"assistant_message","text":" executed successfully.","messageId":"msg_011CfcyX73VjJtzu7anSvcQt"},…}
—    {"type":"turn_completed","provider":"claude","turnId":"foreground-turn-1","usage":{…}}
```

What the trace shows:
- The submitted prompt is echoed as `user_message` seq 1, with `clientMessageId == messageId ==` the
  `messageId` the client sent.
- Every Claude tool call opens with `detail:{type:"unknown",input:{}}`, then upgrades to a typed
  detail. Never let `unknown` overwrite a typed detail.
- Two tool calls run concurrently and complete out of order.
- Codex differs: it emits `name:"shell"` with `cwd` and `exitCode`, starts typed, and sends no
  `unknown` phase **[live]**:

```json
{"type":"tool_call","callId":"call_fRItYpVU7ou1KrHtqqWCNSTN","name":"shell","detail":{"type":"shell","command":"echo hi","cwd":"/tmp/crusteo-scratch","output":"hi\n","exitCode":0},"status":"completed","error":null}
```

A denied permission produces a failed tool call with two updates **[live]**:

```json
5 {"type":"tool_call","callId":"toolu_01Qx…","name":"Write","detail":{"type":"write","filePath":"/tmp/crusteo-scratch/denied.txt","content":"hi"},"status":"failed","error":{"message":"Not that file name; use allowed.txt instead."}}
6 {"type":"tool_call","callId":"toolu_01Qx…","name":"Write","detail":{…},"status":"failed","error":{"type":"tool_result","content":"Not that file name; use allowed.txt instead.","is_error":true,"tool_use_id":"toolu_01Qx…"}}
```

An interrupt cancels the running tool call, then the turn **[live]**:

```json
22 {"type":"tool_call","callId":"toolu_011z…","name":"Bash","detail":{"type":"unknown","input":{},"output":null},"status":"canceled","error":null}
—  {"type":"turn_canceled","provider":"claude","turnId":"foreground-turn-3","reason":"Interrupted"}
```

### 2.5 Fetching history: `fetch_agent_timeline_request` — E

**[schema]** Request (messages.ts:1829):

```json
{"type":"fetch_agent_timeline_request","agentId":"…","requestId":"r9",
 "direction":"tail","cursor":{"epoch":"…","seq":12},"limit":40,"projection":"projected","mergeWindow":false}
```

| Field | Meaning |
|---|---|
| `direction` | `"tail"` (latest N; the default), `"before"` (rows with seq **< cursor.seq**), `"after"` (rows with seq **> cursor.seq**). |
| `cursor` | `{epoch, seq}`. Ignored for `tail`. |
| `limit` | Count of **projected entries**, not seqs. `0` = all. The app always uses `40` (`TIMELINE_FETCH_PAGE_SIZE`). |
| `projection` | Always `"projected"` in the app. |
| `mergeWindow` | `true` only for "jump to prompt" pages that land outside the loaded range (N). |

**[live]** Response (entries trimmed):

```json
{"type":"fetch_agent_timeline_response","payload":{"requestId":"req-…-11","agentId":"d570fc52-…","agent":{…AgentSnapshot…},
 "direction":"tail","projection":"projected","epoch":"6298d5a9-b61d-453e-a209-a0858f159011",
 "reset":false,"staleCursor":false,"gap":false,
 "window":{"minSeq":1,"maxSeq":25,"nextSeq":26},
 "startCursor":{"epoch":"6298d5a9-…","seq":1},"endCursor":{"epoch":"6298d5a9-…","seq":25},
 "hasOlder":false,"hasNewer":false,
 "entries":[
  {"provider":"claude","item":{"type":"user_message","text":"Use the Bash tool …","clientMessageId":"8c81baa4-…","messageId":"8c81baa4-…"},
   "timestamp":"2026-10-02T06:57:18.407Z","seqStart":1,"seqEnd":1,"sourceSeqRanges":[{"startSeq":1,"endSeq":1}],"turnId":"foreground-turn-1","collapsed":[]},
  {"provider":"claude","item":{"type":"tool_call","callId":"toolu_01DQ…","name":"Bash","detail":{"type":"shell","command":"ls -la","output":"total 0…"},"status":"completed","error":null},
   "timestamp":"2026-10-02T06:57:23.342Z","seqStart":2,"seqEnd":6,"sourceSeqRanges":[{"startSeq":2,"endSeq":3},{"startSeq":6,"endSeq":6}],"turnId":"foreground-turn-1","collapsed":["tool_lifecycle"]},
  {"provider":"claude","item":{"type":"tool_call","callId":"toolu_01XS…","name":"Bash","detail":{"type":"shell","command":"echo hi","output":"hi"},"status":"completed","error":null},
   "timestamp":"2026-10-02T06:57:23.337Z","seqStart":4,"seqEnd":5,"sourceSeqRanges":[{"startSeq":4,"endSeq":5}],"turnId":"foreground-turn-1","collapsed":["tool_lifecycle"]},
  {"provider":"claude","item":{"type":"assistant_message","text":"Working directory is empty, both commands executed successfully.","messageId":"msg_011CfcyX73VjJtzu7anSvcQt"},
   "timestamp":"2026-10-02T06:57:24.541Z","seqStart":7,"seqEnd":10,"sourceSeqRanges":[{"startSeq":7,"endSeq":10}],"turnId":"foreground-turn-1","collapsed":["assistant_merge"]},
  …],
 "error":null}}
```

Field semantics (`agent-timeline-store.ts fetch`):
- `window` covers the whole timeline: `minSeq` = first retained seq, `maxSeq = nextSeq-1`.
- `startCursor`/`endCursor` cover this page (`null` when the page is empty).
- `staleCursor` = the cursor's epoch differs from the server's.
- `gap` = an `after` cursor older than `minSeq-1`.
- `reset = staleCursor || gap`. The server then serves a **tail** page instead, and the client must
  **replace**, not merge.
- `hasOlder`/`hasNewer` are relative to this page.
- **[live]** `before` with cursor seq 12 and limit 3 returned `startCursor.seq 4 … endCursor.seq 11`
  with `hasOlder:true`. `after` with limit 3 returned `hasNewer:true`.
- `error` is non-null on failure; the client SDK throws.

### 2.6 How the app keeps a transcript in sync — E

Fetch plans (`timeline-sync-plan.ts`):

| Plan | Request |
|---|---|
| `planTimelineTailFetch()` | `{direction:"tail",limit:40,projection:"projected"}` |
| `planTimelineCatchUpAfter(c)` | `{direction:"after",cursor:{epoch,seq:c.endSeq},limit:40,projection:"projected"}` |
| `planTimelineOlderFetch(c)` | `{direction:"before",cursor:{epoch,seq:c.startSeq},limit:40,projection:"projected"}` |
| `planTimelinePromptJump(t)` (N) | `{direction:"before",cursor:{epoch,seq:t.seq+21},limit:40,projection:"projected",mergeWindow:true}` |

**Open an agent** (`viewed-timeline-sync.ts`, `contexts/session-context.tsx:389-505`):
1. Add the agent to the desired set and send `agent.timeline.set_subscription.request` with the
   full set.
2. **After** the membership ack, run a catch-up:
   - If there is a known cursor `{epoch,endSeq}` (a persisted cache, or a previous session),
     request `after` it. If that first page says `hasNewer`, the client fell too far behind:
     abandon it and fetch the **tail** with forced `reset:true`.
   - With no cursor, request the **tail**.
3. Apply the page through `processTimelineResponse` (below). While the catch-up runs, live
   `agent_stream` frames are already arriving and are reduced as a provisional overlay (below).
4. If the page has `hasNewer` and an `endCursor`, keep paging `after` until `hasNewer:false`.
   Retry with exponential backoff (1 s doubling to 30 s).
5. On disconnect, drop membership and mark catch-ups owed. On reconnect, re-send membership, then
   catch up `after` the stored cursor.
6. On `agent.timeline.replacement`, fetch the tail and replace.
7. When scrolled to the top and the oldest loaded range has `hasOlder`, fetch `before`
   `cursor.startSeq` (`hooks/use-load-older-agent-history.ts`) and prepend.
8. When the transcript reaches the authoritative state and the agent is idle, drain the composer
   queue (§4.3).

**Client cursor.** Per agent the app stores `TimelineCursor = {epoch, startSeq, endSeq,
retainedRanges?}`, the contiguous loaded seq range. `retainedRanges` only matters for
prompt-jump windows (N).

**Live seq gate** (`processTimelineSequencingGate`). This applies only to `timeline` events that
have `seq` and `epoch`, and only once a baseline page has been applied. With
`cur = {epoch, endSeq}`:

| Case | Decision |
|---|---|
| No cursor | `init`: apply, `cursor = {epoch, seq, seq}` |
| `epoch != cur.epoch` and `seq == 1` | Reset: clear tail and head, apply, `cursor = {epoch,1,1}` |
| `epoch != cur.epoch` otherwise | Drop |
| `seq <= cur.endSeq` | Drop (already have it) |
| `seq == cur.endSeq + 1` | Accept: apply, `cur.endSeq = seq` |
| `seq > cur.endSeq + 1` | Gap: **do not apply**; catch up `after {epoch, cur.endSeq}` |

Non-timeline events (turn and permission events) always apply.

- **Detached view.** If the loaded window is not at the tail (`newer == "available"`, e.g. after a
  prompt jump), live events are ignored entirely; the next catch-up brings the view current.
- **Before baseline.** Until the first authoritative page lands, live events are applied only to
  the **head** lane as an overlay. Unmatched user messages go to the head too. The tail page then
  replaces the tail, and the head is reconciled against it (`replaceWithCanonicalStream`).

**Batching** (`createSessionAgentStreamReducerQueue`). Live events are queued per agent and reduced
in one batch per animation frame. A 48 ms timer is the fallback. This keeps token streaming smooth;
any frame-aligned batching works.

**`processTimelineResponse`, condensed.** The response is turned into "units", each a synthetic
`timeline` event with a `seqEnd` cursor. Then:
1. If `error` is set, keep the current state and surface the error.
2. **Replace path.** Taken when `reset`, or on the initial tail during init, or on a resume tail
   whose epoch differs or whose `window.maxSeq < cur.endSeq`, or whose first entry starts after
   `cur.endSeq+1`. Hydrate the units from empty (`hydrateStreamState`), then
   `replaceWithCanonicalStream`:
   - keep local optimistic user rows whose `clientMessageId` is still being sent;
   - match local rows to canonical rows by `clientMessageId`;
   - keep head items that are newer than the canonical coverage;
   - continue a still-streaming assistant head if its text extends the canonical tail assistant.
3. **Discard path.** A resume tail with `window.maxSeq == cur.endSeq` (nothing new). Only
   acknowledge user messages.
4. **Incremental `after`.** Accept if `startSeq <= cur.endSeq+1` and `endSeq > cur.endSeq`.
   Units that overlap the live head are reconciled (`reconcileOverlappingProjected*`): a projected
   assistant or reasoning row whose `sourceSeqRanges` straddles `cur.endSeq` **replaces** the
   partially streamed head text instead of appending a duplicate. Remaining units go through the
   live reducer with source `canonical`.
5. **Incremental `before`.** Accept only if `endSeq == cur.startSeq - 1`. Hydrate the page and
   prepend it. At the seam, merge a tool call with the same identity, and concatenate an assistant
   message split across the page boundary.

**Simplified port** (recommended; it gives the same visible result):
- Keep `tail: Vec<Item>` and `head: Vec<Item>` per agent, plus `cursor`.
- Buffer live frames until the first tail page is applied.
- Apply the page as a **replace**, then drain the buffer through the seq gate (drop `seq <= endSeq`).
- Any gap or epoch mismatch → refetch `after`, or on `reset`, replace from the tail.
- For `before` pages, hydrate and prepend.

Since the server's projected rows are exactly the fold of the same deltas, hydrating a page with the
same reducer reproduces the live result. Two exceptions need the reconciliation in step 4:
- a page whose last assistant row is still streaming: replace the head text with the projected text
  when it `startsWith` the head text;
- tool calls: merge by identity.

### 2.7 The client reducer: `StreamItem` model and algorithm — E

Rendered item kinds (`app/src/types/stream.ts:80`):

```
StreamItem =
  UserMessageItem  {kind:"user_message", id, clientMessageId?, messageId?, turnId?, timelineCursor?, text, timestamp, images?, attachments?}
  AssistantMessageItem {kind:"assistant_message", id, messageId?, turnId?, timelineCursor?, text, timestamp}
  ThoughtItem      {kind:"thought", id, turnId?, timelineCursor?, text, timestamp, status:"loading"|"ready"}
  ToolCallItem     {kind:"tool_call", id, turnId?, timelineCursor?, timestamp, payload:{source:"agent", data:{provider,callId,name,status,error,detail,metadata?}}}
  TodoListItem     {kind:"todo_list", id, turnId?, timelineCursor?, timestamp, provider, items: TodoEntry[], activity: TaskActivity}
  NotificationItem {kind:"notification", sourceType:"error"|"notification", id, level:"info"|"warning"|"error", message, timestamp, …}
  CompactionItem   {kind:"compaction", id, status:"loading"|"completed", trigger?, preTokens?, timestamp, …}
  PluginTimelineStreamItem {kind:"plugin", id:"<pluginId>/<itemId>", pluginId, pluginItemId, itemKind, version, data, timestamp, …}
timelineCursor = {epoch, seq}   // seq = the event seq (live) or entry.seqEnd (fetched)
TaskActivity = {type:"created",count} | {type:"added"|"started"|"completed", task:string}
```

**Two lanes.**
- `tail` is committed history.
- `head` holds actively streaming items: only `assistant_message` and `thought` are "streamable".
- Render `tail ++ head`.

`applyStreamEvent(tail, head, event, timestamp, source, cursor)`:

```
1. If event is timeline user_message → applyCanonicalUserMessageEvent (below); return.
2. If event ∈ {turn_completed, turn_failed, turn_canceled}:
       flush head into tail (finalizing thoughts: status "ready"; skip ids already in tail);
       finalize thoughts in tail; return.
3. kind = itemKind(event)
       user_message → user_message; assistant_message → assistant_message;
       reasoning → thought; tool_call → tool_call; todo → todo_list;
       error | notification → notification; plugin → plugin;
       compaction and any non-timeline event → null.
4. Should the head flush before applying? Yes when:
       head is non-empty and kind != null, and any of:
         - kind is not streamable;
         - the last streamable head item has a different kind (assistant ↔ thought);
         - kind is assistant_message, the incoming messageId is set, and it differs from the
           head assistant's messageId.
5. Resume: if kind == assistant_message, head is empty, and the last tail item is an
   assistant_message with the same messageId (or the incoming one has none),
   move that tail item back into head.
6. If kind is streamable → reduce into head; otherwise → reduce into tail.
```

`reduceStreamUpdate(state, event)` per `item.type`. Every branch except `reasoning` ends with
`finalizeActiveThoughts`, which marks every `thought` as `ready`.

- **assistant_message** (`appendAssistantMessage`):
  - Strip `\r`; ignore empty chunks.
  - If the last item is an assistant message and (incoming `messageId` is undefined or equals
    `last.messageId`), append the text. Update `timestamp` and `timelineCursor`.
  - Else, if `source == live`, the last item is a `user_message`, and the second-to-last is a
    matching assistant message, append to that one. This covers a steer/interrupt user row
    inserted mid-stream.
  - Else, if the chunk has non-whitespace, push a new item. `id = messageId` if unused, else
    `${messageId}:segment:<ts36>`, else `assistant_<ts>_<hash>_<len36>`.
  - Whitespace-only chunks never start a new item.
- **reasoning** (`appendThought`): append to the last item if it is a `thought`, setting status
  `loading`. Otherwise push a new thought (non-whitespace only).
- **tool_call** (`reduceTimelineToolCall`):
  - Normalize the name: trim, `[.\s-]+` → `_`, lowercase.
  - **Claude `TodoWrite`/`todo_write`** become a todo list instead of a tool row. Parse the
    `detail.input` of an `unknown` detail as `{todos:[{content,status,activeForm?}]}`.
    `text = activeForm || content`, `completed = status=="completed"`.
  - Claude `TaskCreate`/`TaskUpdate`/`TaskList` are **dropped**.
  - Any provider's `update_plan` (`{plan:[{step,status}]}`) also becomes a todo list.
  - Otherwise upsert by identity. Identity is `callId`, or `turn:<enc(turnId)>/<enc(callId)>` when
    the event has a `turnId`. The item id is `agent_tool_<identity>`. The row stays where it first
    appeared.
  - Merge rules (`mergeAgentToolCallItem`):
    - **status**: `failed` wins over everything. An existing `canceled` stays canceled. An
      incoming `canceled` does not override `completed`. `completed` beats `running`.
    - **error**: kept only when the merged status is `failed`, as
      `incoming ?? existing ?? {message:"Tool call failed"}`. Otherwise `null`.
    - **detail**:
      - `unknown` never replaces a typed detail;
      - a typed detail replaces `unknown`;
      - the same type shallow-merges `{...existing, ...incoming}`;
      - two `unknown`s merge `input` and `output` separately (incoming `null`, or an empty object
        over a non-empty one, keeps the existing value);
      - different typed kinds: incoming wins.
    - **metadata**: shallow merge.
    - Other fields: incoming overwrites.
- **todo** (`appendTodoList`): normalize the items. Diff them against the **last todo_list of the
  same provider** (`deriveTaskActivities`, keyed by `id ?? "${index}:${text}"`):

  | Change | Activity |
  |---|---|
  | No previous list | `created{count}` |
  | New key | `added{task}` |
  | Status changes to completed | `completed{task}` |
  | Status changes to in_progress | `started{task}` |

  - No activities → update the previous list's items in place.
  - A single `added` right after a `created` row, with all tasks pending → fold into the
    `created` row and bump its count.
  - Otherwise push **one `todo_list` row per activity**, each carrying the full items.
  - The latest `todo.items` also feeds the composer's task-list panel (`taskSnapshot`).
- **error** → `notification{sourceType:"error", level:"error", message}`.
- **notification** → `notification{level, message}`.
- **compaction**: a `completed` item updates the first `loading` compaction row in place
  (status, trigger, preTokens). A `completed` with no loading row present is pushed as new.
  `loading` always pushes.
- **plugin**: upsert by `${pluginId}/${id}`.
- Non-timeline events only run `finalizeActiveThoughts`.

**Turn membership** (`applyTimelineTurnId`):
- After reducing a timeline event with `turnId`, stamp that `turnId` on the **last** item.
- For a user message carrying `clientMessageId`, find the row with that `clientMessageId` and set
  (or clear) its `turnId`. The canonical row is authoritative.

**User messages and optimistic send** (`applyCanonicalUserMessageEvent`, `produceUserMessage`):
- When the user sends, the composer immediately appends a local row
  `{kind:"user_message", id: clientMessageId, clientMessageId, text, timestamp, images, attachments}`.
  It goes to the head if the head is non-empty, otherwise to the tail. During a steer it also gets
  the current `turnId`.
- When the canonical `user_message` arrives:
  1. Flush the head to the tail.
  2. Find a matching row. Both have `clientMessageId` → compare those. Else both have `messageId`
     → compare those. Else (legacy) the local row has `clientMessageId` and no `messageId`, and the
     canonical `messageId == local.clientMessageId` or the texts are equal.
  3. If matched, merge the ids and cursor into the **existing** row (keeping its local
     images/attachments for display), then **move it to the end** of the tail.
  4. If not matched and it has content, append it.
  5. The matched `clientMessageId` is "acknowledged", which clears the pending/greyed state.
- The local row stays "pending" (rendered with `aria-busy` / dimmed) while its `clientMessageId` is
  in the submission set. If the send RPC fails or returns `accepted:false`, remove the unreconciled
  local row (`removeSubmittedUserMessage`).

**Grouping for display** (`agent-stream/turn-membership.ts`, `timeline/turn-time.ts`):
- `continuesTurn(prev, next)`: if both have a `turnId`, compare them; otherwise
  `next.kind != user_message`.
- A turn's **completed footer** (shown after the last assistant message of a finished turn) has
  "Copy turn", an optional fork menu (N), the completion time and the duration. Duration is the
  last item timestamp of the turn minus the user message timestamp.
- While a turn is open, the footer is a **working indicator**: a 14 px synced loader plus a live
  elapsed timer from `activeTurn.startedAt` or the `turn_started` timestamp.
- Pending permission cards render **below** the footer, at the bottom of the stream.

### 2.8 Rendering the non-tool items — E unless marked

| Kind | Rendering (`components/message.tsx`, `agent-stream/view.tsx:860`) |
|---|---|
| `user_message` | Right-aligned bubble (`surface3` background, `alignItems:flex-end`). Image thumbnails (lightbox on tap) and attachment pills above the text. Hover row: timestamp, copy, rewind (N). Consecutive user rows are grouped visually. |
| `assistant_message` | Full-width markdown with code blocks (copy button), file-path links that open in the workspace, mermaid diagrams (N). Head text is revealed with a smoothing animation (`agent-stream/text-reveal.ts`, N). |
| `thought` | Rendered **as a tool-call row** named `"thinking"`: label `Thinking`, brain icon. Shimmers while `status=="loading"`, collapsed by default. Expanding shows the text. The setting "Always expand reasoning" (`autoExpandReasoning`, default false) expands it inline. |
| `tool_call` | §3. A `name=="speak"` call with a string `unknown.input` renders as a "Spoke" speech bubble instead (N). |
| `todo_list` | A collapsible badge. CheckSquare icon `Created N tasks`; Plus/CircleDot/Check icons `Added`/`Started`/`Completed` followed by the task text. Expands to the task rows. |
| `notification` | A row with an info/warning/error icon (16 px) and the message. |
| `compaction` | A centered divider label: `Compacting...` (loading), `Context automatically compacted` (auto), `Context manually compacted` (manual), `Context compacted ({{K}}K tokens)` (preTokens/1000), else `Context compacted`. |
| `plugin` | Plugin-provided renderer (N). |

Tool calls, thoughts and todo rows form a "tool sequence". The last item in a sequence gets a 16 px
bottom margin; the others get 4 px (`agent-stream/layout.ts`).

---

## 3. Tool-call rendering

### 3.1 Wire shape (messages.ts:526-659, `protocol/src/agent-types.ts:212-344`) — E

```
tool_call = { type:"tool_call", callId, name /* raw provider name: "Bash", "Write", "shell", "mcp__x__y" */,
              detail: ToolCallDetail, metadata?: object,
              status: "running"|"completed"|"failed"|"canceled",
              error: null  /* running, completed, canceled */ | <any non-null>  /* required when failed */ }
```

`ToolCallDetail` (discriminated on `type`):

| `type` | Fields |
|---|---|
| `shell` | `command`, `cwd?`, `output?`, `exitCode?: number\|null` |
| `read` | `filePath`, `content?`, `offset?`, `limit?` |
| `edit` | `filePath`, `oldString?`, `newString?`, `unifiedDiff?` |
| `write` | `filePath`, `content?` |
| `search` | `query`, `toolName?: "search"\|"grep"\|"glob"\|"web_search"`, `content?`, `filePaths?`, `webResults?: [{title,url}]`, `annotations?`, `numFiles?`, `numMatches?`, `durationMs?`, `durationSeconds?`, `truncated?`, `mode?: "content"\|"files_with_matches"\|"count"` |
| `fetch` | `url`, `prompt?`, `result?`, `code?`, `codeText?`, `bytes?`, `durationMs?` |
| `worktree_setup` | `worktreePath`, `branchName`, `log`, `commands: [{index, command, cwd, log, status: "running"\|"completed"\|"failed", exitCode, durationMs?}]`, `truncated?` |
| `sub_agent` | `subAgentType?`, `description?`, `childSessionId?`, `log` (lines `[ToolName] summary`), `actions?` (compat; always `[]`) |
| `plain_text` | `label?`, `text?`, `icon?` (one of `wrench`, `square_terminal`, `eye`, `pencil`, `search`, `bot`, `sparkles`, `brain`, `mic_vocal`) |
| `plan` | `text` (markdown) |
| `unknown` | `input`, `output` (any JSON or null). MCP tools and anything unmapped. |

**[schema]** Examples:

```json
{"type":"read","filePath":"/repo/src/a.ts","content":"import x…","offset":10,"limit":50}
{"type":"edit","filePath":"/repo/a.ts","oldString":"foo","newString":"bar"}
{"type":"search","query":"TODO","toolName":"grep","filePaths":["a.ts"],"numFiles":1,"mode":"files_with_matches"}
{"type":"fetch","url":"https://x.dev","prompt":"summarize","result":"…","code":200,"codeText":"OK"}
{"type":"sub_agent","subAgentType":"Explore","description":"find X","log":"[Read] src/a.ts\n[Grep] foo","actions":[]}
{"type":"plain_text","label":"my-skill","icon":"sparkles","text":"…"}
{"type":"unknown","input":{"foo":1},"output":null}
```

How the server picks the detail for Claude (`server/…/claude/tool-call-mapper.ts`,
`tool-call-detail-parser.ts`):

| Detail | Tool names |
|---|---|
| `shell` | `Bash` |
| `read` | `Read` |
| `write` | `Write` |
| `edit` | `Edit`, `MultiEdit`, `apply_patch`, … |
| `search` (`web_search`) | `WebSearch` |
| `search` (`grep`) | `Grep` |
| `search` (`glob`) | `Glob` |
| `fetch` | `WebFetch` |
| `plain_text` | `Skill` |
| `sub_agent` | `Task`/`Agent` sidechains, as a synthetic `name:"Task"` card |
| `plan` | `ExitPlanMode` |
| `unknown` | everything else |

The search `query` comes from input `query`, `q` or `pattern`.

`ExitPlanMode` **[live]**:
- It is `name:"ExitPlanMode"` with `unknown` detail while it waits for approval.
- After the decision it becomes `name:"plan_approval"`, `detail:{type:"plan",text}`.
- `metadata.approved` is `true`/`false`, and `failed` is mapped to `completed`.

### 3.2 Collapsed row ("badge") — E

Every tool call is one row (`ExpandableBadge`, `components/message.tsx:2681`):

```
[icon 12px in a 22px slot] [displayName, muted] [summary, muted, single line, ellipsized]   [open-file button on hover]
```

- **Running**: there is **no spinner**. The label and summary get an animated shimmer sweep, and
  the label is at 0.72 opacity. Shimmer duration is
  `clamp(1.25 + chars*0.008 − (chars≤12 ? 0.25 : 0), 1, 2.3)` s.
- **Failed**: the icon is replaced by `TriangleAlert` in the destructive colour.
- **Completed and canceled** look the same; canceled has no special marker.
- **No** exit code, duration, diff counts or match counts are shown, even though the wire carries
  them.
- Hover or expand swaps the icon for a chevron (rotated 90° when open), but only if the row has
  details.

Display name, summary and icon (`protocol/src/tool-call-display.ts:83-168`,
`app/src/utils/tool-call-icon-name.ts`; icons are lucide):

| `detail.type` | displayName | summary | icon |
|---|---|---|---|
| shell | `Shell` | `command` | square_terminal |
| read | `Read` | `filePath` with the cwd prefix stripped (`.` if it equals cwd) | eye |
| edit | `Edit` | stripped `filePath` | pencil |
| write | `Write` | stripped `filePath` | pencil |
| search | `Search` | `query` | search |
| fetch | `Fetch` | `url` | search |
| worktree_setup | `Worktree setup` | `branchName` | square_terminal |
| sub_agent | `subAgentType` or `Task` | `description` | bot |
| plain_text | humanized `name` | `label` | `detail.icon` or wrench |
| plan | — (renders a PlanCard, §3.4) | — | brain |
| unknown | humanized `name` | — | wrench |

Overrides by name:

| Name | With | Result |
|---|---|---|
| `task` (any case) | `unknown` detail | `Task`, summary `metadata.subAgentActivity`, bot icon |
| `thinking` | — | `Thinking`, brain icon. This is how `thought` items render. |
| `terminal` | — | `Terminal` |
| `speak` | — | mic_vocal icon |
| Paseo MCP tools (`mcp__paseo__*`, `paseo.*`) | — | Paseo logo, leaf name |

`humanizeToolName` (for other names):
- Names containing `:`, `.`, `/` or `__` are shown verbatim, so `mcp__github__create_issue` stays as is.
- Otherwise, replace `[._-]+` with a space, lowercase, then capitalize the first letter
  (`web_fetch` → `Web fetch`).

Clickability (`tool-calls/presentation.ts:69`):
- `hasDetails = error != null || meaningfulDetail`.
  - shell always counts as meaningful;
  - read and write need a path or content;
  - search needs a non-empty query, content, paths, results or annotations;
  - unknown needs a non-empty leaf.
- `isLoadingDetails = running && error == null && !meaningful`.
- If neither holds, the row is not pressable.

"Open file" (N) is offered for:
- read, edit and write `filePath`;
- simple shell commands `cat|bat|less|more|head|tail|wc|nl|tac|od|xxd|file|stat|column|md5|md5sum|sha1sum|sha256sum|shasum <file>` with none of `|><&;\`$()`.

### 3.3 Expanded detail — E

Desktop expands inline under the row (max height 400; 200 inside a group). Compact/mobile uses a
bottom sheet. Content comes from `ToolCallDetailsContent` (`components/tool-call-details.tsx:650`).

| Kind | Expanded content |
|---|---|
| shell | One monospace block: muted `$ ` + `command`, a blank line, then `output`. Scrolls both ways. Full-bleed. `cwd`/`exitCode` are not shown. |
| edit | Diff view. Parse `unifiedDiff` if present (drop `diff --git`/`index`/`---`/`+++`; `@@` lines become headers). Otherwise an LCS line diff of `oldString` → `newString` with word-level highlighting on paired remove/add lines (N). Add `rgba(46,160,67,.15)`, remove `rgba(248,81,73,.1)`. Syntax highlighting by extension (N). No line numbers. |
| write | `content`, syntax-highlighted, no gutter. |
| read | `content` with a line-number gutter starting at `offset ?? 1`. |
| search | Stacked sections, each only if present: `content` (monospace); `filePaths` (newline-joined); `webResults` as `title\nurl`; `annotations`. |
| fetch | Monospace `url`, a blank line, then `result`. |
| worktree_setup | Monospace `log`, or `Preparing worktree <branch> at <path>` when it is empty. |
| sub_agent | Optional muted `session <childSessionId>`. Each `[Tool] summary` log line becomes a row (title-cased tool name + summary). Other text below. Fallback: `<type>: <description>` or `Sub-agent activity`. |
| plain_text | `text` in the UI font. |
| unknown | `Input` header + pretty JSON (strings verbatim), then `Output` header + JSON. Empty sections are skipped. A string input with null output shows as plain text. Paseo MCP tools get structured `Prompt`/`Details`/`Result` sections (N). |
| any failed | An extra `Error` section (destructive title and border, monospace). Its text is `error` if it is a string, else `error.content` if that is a string, else `JSON.stringify(error, null, 2)`. |
| nothing to show | A 3-line skeleton while loading, else italic `No additional details available`. |

### 3.4 Special cases — E

- **Plan** (`detail.type=="plan"`): never a badge. A `PlanCard` shows a chevron, a title and the
  plan as markdown.
  - Titles: `Plan`; `Approved plan` (`metadata.approved===true`); `Rejected plan` (`false`);
    `Canceled plan`.
  - Rejected and canceled cards start collapsed.
  - `ExitPlanMode` and running `plan_approval` items are **hidden** from the stream, because the
    permission card shows them (`tool-calls/detail-level/projection.ts:26`).
- **Todo tool calls** (`TodoWrite`, `update_plan`) never render as tool rows (§2.7).
- **Detail level** (N): setting `toolCallDetailLevel: "detailed" | "overview"`, labelled
  "Tool call display" with options "Full detail"/"Summary". The default is **`detailed`**: one row
  per call. In `overview`, each run of consecutive tool calls (plan and speak excluded) collapses
  into one group row, wrench icon, shimmering while any member runs. Its label is built from:
  - `edited N file(s)` (distinct edit/write paths)
  - `ran N command(s)`
  - `read N file(s)`
  - `searched N time(s)`
  - `called Paseo N time(s)`
  - `used N other tool(s)`

  The parts are joined "a, b, and c" and the first letter is capitalized.
- **Subagents** (N): inline they are just a `sub_agent`/`Task` badge. Full child timelines arrive
  via `agent.provider_subagents.*` messages and show in a separate subagent track.

---

## 4. Composer actions

### 4.1 Send a message — E

**[live]** Request (messages.ts:1349):

```json
{"type":"send_agent_message_request","requestId":"req-…-6","agentId":"d570fc52-…",
 "text":"Use the Bash tool to run `ls -la`, then separately run `echo hi`. Then reply in one short line.",
 "messageId":"8c81baa4-28ba-48dc-9b9d-84a5bf4aa31a"}
```

**[live]** Reply. It arrives **after** `turn_started` and the echoed `user_message`:

```json
{"type":"send_agent_message_response","payload":{"requestId":"req-…-6","agentId":"d570fc52-…","accepted":true,"error":null}}
```

- `agentId` accepts a full id, a unique prefix, or an exact title.
- `messageId` is the client message id. The server deduplicates on it and echoes it as **both**
  `messageId` and `clientMessageId` on the timeline `user_message` **[live]**.
- Do not use `send_agent_message` (no `_request` suffix). It is still in the schema, but the
  daemon does not handle it.

Optional fields:
- `activeTurnBehavior`: `"interrupt" | "steer"` (§4.3). The server default is `"interrupt"`.
- `images`: `[{data: <base64>, mimeType}]`.
- `attachments`: `AgentAttachment[]` (messages.ts:1157). Invalid entries are silently dropped. Kinds:
  - `{type:"text", mimeType:"text/plain", text, title?, contextKind?, externalResource?}`
  - `{type:"uploaded_file", id, fileName, mimeType, size, path}` (upload first with
    `file.upload.request`, N)
  - `review` (`application/paseo-review`, `{cwd, mode, baseRef?, comments[]}`), N
  - `forge_change_request`, `forge_issue`, `github_pr`, `github_issue`, N

Server-side effects:
- Sending to an archived agent **unarchives** it.
- Pending permissions are cleared.
- The response is sent once the run has started.

The optimistic local row is described in §2.7. Generate `messageId = uuid v4`, insert the local
row, then send. On `accepted:false` or an error, remove the row and restore the draft.

### 4.2 Interrupt / abort — E

**[live]** Request (messages.ts:1802):

```json
{"type":"cancel_agent_request","agentId":"ed153bf6-…","requestId":"req-…-4"}
```

**[live]** Reply (messages.ts:4790). It is sent only when `requestId` is present:

```json
{"type":"cancel_agent_response","payload":{"requestId":"req-…-4","agentId":"ed153bf6-…","agent":{…AgentSnapshot…},"error":null}}
```

**[live]** Mid-stream sequence: assistant delta seq 11 → `turn_canceled
{reason:"Interrupted"}` → `cancel_agent_response`, all within the same millisecond. The text
already streamed is kept.

- Cancelling an idle agent is a no-op that still replies.
- UI: while a turn is active, a stop button replaces Send, and **Escape** is bound to
  `agent.interrupt` (`keyboard/keyboard-shortcuts.ts:1140`).
- Between the click and the close event the app shows a "cancelling" state
  (`cancellation_started`/`cancellation_settled`).

### 4.3 Sending while a turn is running ("active turn behavior") — E

**Server** (`agent-manager.ts:2725`, `agent-prompt.ts:306`):
- `interrupt`: cancel the running turn, then start a new turn with the message.
- `steer`: inject the text into the active turn if the provider supports it, else fall back to
  replace-the-turn.

**[live]** `activeTurnBehavior:"interrupt"` while a Bash call was running produced:
1. `tool_call … "status":"canceled"`
2. `turn_canceled {turnId:"foreground-turn-3", reason:"Interrupted"}`
3. `turn_started {turnId:"foreground-turn-4"}`
4. the new `user_message`, then `send_agent_message_response`.

**App** (`hooks/use-settings/storage.ts:30`):
- The setting is `sendBehavior: "interrupt" | "steer" | "queue"`, **default `"steer"`**. A stored
  `"interrupt"` is migrated to `"steer"`.
- With the turn running:
  - Plain **Enter**: `queue` → enqueue locally; `steer`/`interrupt` → send now with
    `activeTurnBehavior = sendBehavior`.
  - **Cmd/Ctrl+Enter** is the alternate action: if the setting is `queue` it sends now, otherwise
    it enqueues.
- If a permission request is pending, `queue` is treated as `interrupt`, so the message is not
  stranded behind the prompt.
- The queue is **client-only** state: `queuedMessages: Map<agentId, {id, text, attachments}[]>`.
  It renders above the input, one row per item (first two lines of text):
  - a pencil button pops the item back into the composer;
  - a "send now" arrow sends it with the user's `sendBehavior`; on failure it goes back to the
    front of the queue.
- **Auto-drain.** When an `agent_update` moves the turn from open to idle, or a timeline catch-up
  completes with the agent idle, send the first queued item with **no** `activeTurnBehavior`.
  Only one item is sent per idle edge.

### 4.4 Permission requests — E

**Arrival.** The same request arrives on three paths, so dedupe by `(agentId, request.id)`:
1. `agent_permission_request {agentId, request, subscriptionId?}` on the session event feed. This
   needs `session.events.set_subscription.request` with `"agent_permission_request"` and
   `"agent_permission_resolved"`.
2. `agent_stream` `permission_requested`.
3. The agent snapshot's `pendingPermissions[]`, which is authoritative.

`agent_permission_resolved` was delivered **twice** in testing, once with and once without
`subscriptionId` **[live]**.

**[live]** Tool permission (Claude `default` mode, `Write`):

```json
{"type":"agent_permission_request","payload":{"agentId":"cd21a9bc-…","request":{
 "id":"permission-6590cfe8-dece-402b-b996-8a41fbb9c4ac","provider":"claude","name":"Write","kind":"tool",
 "input":{"file_path":"/tmp/crusteo-scratch/denied.txt","content":"hi"},
 "detail":{"type":"write","filePath":"/tmp/crusteo-scratch/denied.txt","content":"hi"},
 "suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],
 "metadata":{"toolUseId":"toolu_01Qx2TMPmDQEkrk8RxJ3T3YJ"}},
 "subscriptionId":"3fea82d7-…"}}
```

**[live]** Plan approval (Claude `plan` mode):

```json
{"id":"permission-2ffe91f3-…","provider":"claude","name":"ExitPlanMode","kind":"plan",
 "input":{"plan":"# Create hello.txt with \"hi\"\n\n## Context\n…","planFilePath":"/home/user/.claude/plans/make-a-one-line-plan-adaptive-storm.md"},
 "actions":[{"id":"reject","label":"Reject","behavior":"deny","variant":"danger","intent":"dismiss"},
            {"id":"implement","label":"Implement","behavior":"allow","variant":"primary","intent":"implement"}],
 "metadata":{"toolUseId":"toolu_01FJ…","planText":"# Create hello.txt …"}}
```

Request schema (messages.ts:475):
- `id`, `provider`, `name`
- `kind: "tool"|"plan"|"question"|"mode"|"other"`
- `title?`, `description?`, `input?`
- `detail?: ToolCallDetail`
- `suggestions?: object[]` (opaque)
- `actions?: [{id, label, behavior: "allow"|"deny", variant?: "primary"|"secondary"|"danger", intent?: "implement"|"implement_resume"|"dismiss"}]`
- `metadata?`

**Response.** This is a fire-and-forget message; `requestId` is the **permission id**, not an RPC id.

**[live]** Deny with a message:

```json
{"type":"agent_permission_response","agentId":"cd21a9bc-…","requestId":"permission-6590cfe8-…",
 "response":{"behavior":"deny","message":"Not that file name; use allowed.txt instead."}}
```

**[live]** Allow:

```json
{"type":"agent_permission_response","agentId":"cd21a9bc-…","requestId":"permission-95a7f541-…","response":{"behavior":"allow"}}
```

**[live]** Plan reject:

```json
{"type":"agent_permission_response","agentId":"cd21a9bc-…","requestId":"permission-2ffe91f3-…",
 "response":{"behavior":"deny","selectedActionId":"reject","message":"Plan rejected (test)."}}
```

`response` is one of:
- `{behavior:"allow", selectedActionId?, updatedInput?: object, updatedPermissions?: object[]}`
- `{behavior:"deny", selectedActionId?, message?, interrupt?: boolean}`

**[live]** Confirmation:

```json
{"type":"agent_permission_resolved","payload":{"agentId":"cd21a9bc-…","requestId":"permission-6590cfe8-…","resolution":{"behavior":"deny","message":"Not that file name; use allowed.txt instead."}}}
```

The tool call then turns `failed` with the deny message as its error (§2.4). The model sees the
message and can adapt; in testing it retried with the suggested name. Failures come back as
`rpc_error` with `requestId` = the permission id. The app waits up to 15 s.

**UI** (`PermissionRequestCard`, `agent-stream/view.tsx:1401-1612`): cards stack at the bottom of
the stream, below the turn footer. Each card shows:
- the title (`request.title ?? request.name ?? "Permission Required"`) and description;
- `ToolCallDetailsContent(request.detail ?? {type:"unknown", input:request.input})`, max height 200;
- `How would you like to proceed?` and the buttons.

Buttons:
- If `actions` are present, one button per action. Allow sends
  `{behavior:"allow", selectedActionId}`; deny sends
  `{behavior:"deny", selectedActionId, message:"Denied by user"}`.
- If not, two defaults: **Deny** (`reject`) and **Accept** (`accept`). For `kind:"plan"`, Accept
  is labelled **Implement**.
- "Always allow" exists only as provider-supplied actions (`allow_always`, `always` on
  OpenCode/ACP). The app never sends `updatedPermissions` and ignores `suggestions`.

Special kinds:
- `kind:"plan"` renders a PlanCard from `metadata.planText ?? input.plan`, with the buttons in its
  footer. Claude offers `implement_resume` only when the mode to resume into is
  `bypassPermissions`.
- `kind:"question"` (N) renders a question form from
  `input.questions[] = {question, header, options:[{label, description?}], multiSelect?, allowOther?, allowEmpty?, placeholder?, dismissLabel?}`.
  - Submit: `{behavior:"allow", updatedInput:{...input, answers:{[header]: "labelA, labelB"}}}`
  - Dismiss: `{behavior:"deny", message:"Dismissed by user"}`

### 4.5 Mode, model, thinking and feature switching — E (feature: N)

All four reply with `{requestId, agentId, accepted, error, notice?: {type:"info"|"warning"|"error", message} | null}`.
The app toasts `notice`.

**[live]** Calls and replies:

```json
{"type":"set_agent_mode_request","agentId":"cd21a9bc-…","modeId":"plan","requestId":"req-…-8"}
→ {"type":"set_agent_mode_response","payload":{"requestId":"req-…-8","agentId":"cd21a9bc-…","accepted":true,"error":null,"notice":null}}
{"type":"set_agent_model_request","agentId":"cd21a9bc-…","modelId":"claude-sonnet-4-6","requestId":"req-…-11"}
→ {"type":"set_agent_model_response","payload":{"requestId":"req-…-11","agentId":"cd21a9bc-…","accepted":true,"error":null}}
{"type":"set_agent_thinking_request","agentId":"cd21a9bc-…","thinkingOptionId":"low","requestId":"req-…-12"}
→ {"type":"set_agent_thinking_response","payload":{"requestId":"req-…-12","agentId":"cd21a9bc-…","accepted":true,"error":null,"notice":null}}
```

- `modelId` and `thinkingOptionId` may be `null`, meaning provider default.
- `set_agent_feature_request {agentId, featureId, value, requestId}` → `set_agent_feature_response`
  (N).
- Bundled form (N): `agent.config.apply.request {agentId, config:{modelId?, modeId?, thinkingOptionId?, featureValues?}, requestId}`.
  Needs `features.agentConfigApply`.
- Each change is reflected in the next `agent_update` (`model`, `currentModeId`, `thinkingOptionId`).

Where the options come from:
- **Modes**: the agent's `availableModes` + `currentModeId`. Icons and colours come from the
  provider snapshot's `modes[]`, e.g. Claude:

  | Mode | Label | Icon | Tier |
  |---|---|---|---|
  | `plan` | "Plan Mode" | ShieldEllipsis | planning |
  | `default` | "Always Ask" | Shield | safe |
  | `acceptEdits` | — | ShieldPlus | moderate |
  | `auto` | — | ShieldCheck | moderate |
  | `bypassPermissions` | — | ShieldOff | dangerous |

- **Models and thinking levels**: the **provider snapshot**, not the agent.
  - Request: `get_providers_snapshot_request {cwd?, ifNoneMatch?, requestId}`.
  - Reply: `get_providers_snapshot_response {entries[], compactSnapshot?, snapshotHash, generatedAt, fetchedAt, requestId}`.
  - Pushes arrive as `providers_snapshot_update`.
  - With the `compact_provider_snapshots` capability, `entries` is empty **[live]** and the data is
    in `compactSnapshot`: `{entries:[{provider, status, enabled, label, description, defaultModeId, modes[], models:[{id, label, description?, isDefault?, isSelectable?, aliases?, contextWindowMaxTokens?, thinkingSet?: index}]}], thinkingSets:[{options:[{id,label,isDefault?}], defaultOptionId}]}`.
  - A Rust client can omit that capability to get plain `entries` with inline `thinkingOptions`.
  - **[live]** `claude-haiku-4-5` has no `thinkingSet`, so no thinking control is shown.
  - Only `status:"ready"` providers are selectable. Hide `isSelectable:false` models.
  - Force a refresh with `refresh_providers_snapshot_request {cwd?, providers?, requestId}`.
- **Context meter** (N): `lastUsage.contextWindowUsedTokens / contextWindowMaxTokens`.

### 4.6 Slash commands — E (list + insert), N (client-only commands)

**[live]** Request and reply:

```json
{"type":"list_commands_request","agentId":"cd21a9bc-…","requestId":"req-…-4"}
→ {"type":"list_commands_response","payload":{"agentId":"cd21a9bc-…","commands":[
   {"name":"advisor","description":"Let Claude consult a stronger model at key moments","argumentHint":"[fable|opus|sonnet|off]","kind":"skill"},…],
   "error":null,"requestId":"req-…-4"}}
```

- `draftConfig {provider, cwd, modeId?, model?, thinkingOptionId?, featureValues?}` lists commands
  before the agent exists (new-agent composer).
- The app caches the list for 60 s.
- Typing `/` opens autocomplete. A pick inserts `/name `, and the message is sent as **plain text**
  via `send_agent_message_request`.
- Client-only commands fire only when the whole message is the command and there are no
  attachments:
  - `/exit` (aliases `/quit`, `/q`) archives the agent;
  - `/clear` (alias `/new`) archives the agent and opens a fresh draft with the same provider,
    mode, model, thinking and features.

---

## 5. Agent lifecycle from the UI

### 5.1 Create — E

The modern form is used when `server_info.features.creationLifecycle`. The legacy
`create_agent_request` has the same fields and replies with a `status` message
(`agent_created`/`agent_create_failed`).

**[live]** Minimal request (`agent.create.request`, messages.ts:1706):

```json
{"type":"agent.create.request","requestId":"req-…-3",
 "config":{"provider":"claude","cwd":"/tmp/crusteo-scratch","model":"claude-haiku-4-5","modeId":"default","title":"crusteo capture"},
 "labels":{}}
```

**[live]** Reply 40 ms later:

```json
{"type":"agent.create.response","payload":{"requestId":"req-…-3","agent":{…AgentSnapshot, "status":"idle", "runtimeInfo":{"provider":"claude","sessionId":null,"model":null,"modeId":"default"}…},"error":null}}
```

It is preceded by `agent_update` upserts (`initializing` → `idle`).

**[schema]** What the app sends (`composer/draft/workspace-tab.tsx:158`):

```json
{"type":"agent.create.request","requestId":"r4","idempotencyKey":"<draftId>","workspaceId":"wks_…",
 "config":{"provider":"claude","cwd":"/repo","modeId":"default","model":"claude-opus-5-5","thinkingOptionId":"medium"},
 "initialPrompt":"fix the flaky test","clientMessageId":"<uuid>","images":[…],"attachments":[…],"labels":{}}
```

`config` (messages.ts:436):
- `provider`, `cwd`
- `modeId?`, `model?`, `thinkingOptionId?`, `featureValues?`
- `title?` (trimmed, non-empty). The app does not set it; the server derives one.
- `providerOptions?`, `toolPolicy?`, `systemPrompt?`, `mcpServers?`

Other fields:
- `env?`, `worktreeName?`, `git?`
- `worktree?: {mode:"branch-off", newBranch, base?} | {mode:"checkout-branch", branch} | {mode:"checkout-pr", prNumber}`
- `autoArchive?`, `outputSchema?`, `callerAgentId?`
- Modern form only: `agentId?` (uuid) and `subscribe?`. With `subscribe:true`, progress arrives as
  `agent.create.update` with phases `accepted → workspace_ready → agent_ready → prompt_started →
  completed|failed` (N).

Initial prompt and optimistic row:
- `initialPrompt` + `clientMessageId` start the first turn.
- The app hands its optimistic user row over to the new agent's stream
  (`handoffCreatedAgentUserMessageToStream`, match policy "handoff") so the bubble does not flicker.
- Alternatively, create without a prompt, subscribe, and then `send_agent_message_request`. That is
  what the capture did.

Default picks (`provider-selection/resolve-agent-form.ts`):

| Choice | Order of preference |
|---|---|
| Provider | Initial value, then saved preference (ready providers only) |
| Mode | Initial value, then saved, then the provider's `defaultModeId`, then the first mode |
| Model | Initial value, then saved, then `isDefault`, then the first model |
| Thinking | Saved per model, then `defaultThinkingOptionId`, then the first option |

Preferences are persisted locally per provider.

### 5.2 Archive / close / delete / unarchive — E (delete, close: N)

**[live]** Archive request:

```json
{"type":"archive_agent_request","agentId":"d570fc52-…","requestId":"req-…-18"}
```

**[live]** Reply (2.6 s later; it waits for the provider to shut down):

```json
{"type":"agent_archived","payload":{"agentId":"d570fc52-…","archivedAt":"2026-10-02T06:57:42.063Z","requestId":"req-…-18"}}
```

- Archive cancels a running turn and clears attention.
- An `active`-scope agent subscription receives `agent_update {kind:"remove"}`; one with
  `includeArchived` receives an upsert with `archivedAt` set.
- Archived agents keep their transcript and can be opened via `fetch_agent_request` and the
  timeline fetch.
- **Unarchive**: there is no dedicated message. Either send a message (auto-unarchives) or send
  `refresh_agent_request {agentId, requestId}`. The latter replies with
  `status {status:"agent_refreshed", agentId, requestId, timelineSize?}`; it reloads the agent and
  re-sends its timeline, and the app then refetches the tail. The "Unarchive" button and "Reload
  agent" both use refresh.
- `close_items_request {agentIds[], terminalIds[], requestId}` → `close_items_response
  {agents:[{agentId, archivedAt}], terminals:[…]}`: archives several agents at once (tab close) (N).
- `delete_agent_request {agentId, requestId}` → `agent_deleted {agentId, requestId}` + `remove`:
  hard delete. The app barely uses it (N).

### 5.3 Rename and labels — N

**[live]** Request:

```json
{"type":"update_agent_request","agentId":"d570fc52-…","name":"crusteo capture renamed","labels":{"purpose":"capture"},"requestId":"req-…-15"}
```

**[live]** Reply:

```json
{"type":"update_agent_response","payload":{"requestId":"req-…-15","agentId":"d570fc52-…","accepted":true,"error":null}}
```

- An `agent_update` with the new `title` arrives first **[live]**.
- `name` is trimmed and sets `title`.
- `labels` are **merged** into the existing ones.
- Sending neither field returns `accepted:false`.

### 5.4 Other lifecycle RPCs — N

| RPC | Reply | Use |
|---|---|---|
| `resume_agent_request {handle:{provider, sessionId, nativeHandle?, metadata?}, overrides?, requestId}` | `status agent_resumed` | Resume a provider session |
| `import_agent_request` | `status agent_resumed` / `agent_create_failed` | Import a session |
| `agent.rewind.request {agentId, messageId, mode:"conversation"\|"files"\|"both", requestId}` | `agent.rewind.response {ok, error}` | Rewind; a conversation rewind triggers `agent.timeline.replacement` |
| `agent.detach.request` | — | Subagents |
| `agent.fork_context.request` | — | "Fork chat from here" |
| `agent.timeline.search.request` | — | Find in chat |
| `agent.timeline.list_prompts.request` | — | Prompt outline / jump |

---

## 6. Port checklist

**Essential (feels like Paseo):**
1. The active directory subscription (`fetch_agents_request` + `subscribe`) and `agent_update`
   upsert/remove, with `updatedAt` ordering.
2. Status buckets, turn liveness, and the attention flag with `clear_agent_attention`.
3. Timeline membership (`agent.timeline.set_subscription.request`), tail fetch, and the seq gate.
   Recover by gap catch-up (`after`), replace on `reset` or epoch change, handle
   `agent.timeline.replacement`, and page older history with `before`.
4. The reducer (§2.7):
   - head/tail lanes;
   - assistant delta concatenation by `messageId`;
   - thought finalization;
   - tool-call identity merge with the unknown/typed and status-precedence rules;
   - TodoWrite → todo rows;
   - optimistic user rows reconciled by `clientMessageId`;
   - `turnId` stamping;
   - frame-batched commits.
5. Rendering:
   - user bubble;
   - markdown assistant text;
   - Thinking rows;
   - tool badges with the exact name/summary table, a running shimmer, the failed alert, and
     expand-to-detail (shell `$ cmd`+output, edit diff, read with line numbers, JSON fallback);
   - PlanCard;
   - todo, notification and compaction rows;
   - the working footer with elapsed time, and the completed footer with duration.
6. Composer:
   - `send_agent_message_request` with `messageId`, images and text attachments;
   - stop button and Escape → `cancel_agent_request`;
   - `sendBehavior` (default `steer`) with the local queue and auto-drain;
   - permission cards driven by `actions`, with default Deny/Accept and `agent_permission_response`;
   - mode, model and thinking pickers from `availableModes` and the providers snapshot;
   - `/` autocomplete via `list_commands_request`.
7. Lifecycle: `agent.create.request` with an initial prompt, archive (`/exit`), unarchive via refresh.

**Nice-to-have:**
- overview grouping of tool calls;
- syntax and word-level diff highlighting;
- the open-file button;
- Paseo MCP structured detail;
- sub-agent tracks;
- question forms;
- rename/labels;
- `close_items_request`, delete, rewind, fork;
- prompt-jump `mergeWindow`;
- text-reveal animation;
- feature toggles and `agent.config.apply`;
- creation progress updates;
- forge/review/uploaded-file attachments;
- the `/clear` draft carry-over;
- the history search list;
- a persisted timeline cache (the app persists transcripts locally so they show instantly on launch).

---

## Appendix A: Observed quirks (live, 0.10.1)

- **Claude Bash tool calls start as `unknown`.** Their first update has
  `detail:{"type":"unknown","input":{},"output":null}`. Codex sends typed `shell` from the start,
  with `cwd` and `exitCode`.
- **Two updates for one failure.** A tool call failed by a permission denial gets two `failed`
  updates with different `error` shapes: `{message}`, then
  `{type:"tool_result", content, is_error, tool_use_id}`. Display uses `error.content` when it is a
  string.
- **Background shells.** Claude may background long shell commands
  (`output:"Command running in background with ID: …"`), so the turn completes while the command
  is still running. Its Bash tool also refuses `sleep N; …` in the foreground (a `failed`
  tool_result with a `<tool_use_error>`). Don't rely on a long sleep to test interrupts; interrupt
  a long text generation instead.
- **Noisy `agent_update`.** Expect several per second during a turn, including exact duplicates.
  Debounce re-renders.
- **Duplicate events.** `agent_permission_resolved` and `agent_update {kind:"remove"}` were each
  delivered more than once. Make handlers idempotent.
- **Response ordering.** `send_agent_message_response` arrives after `turn_started` and the echoed
  `user_message`, so the optimistic row is usually reconciled before the RPC resolves.
- **No seq on non-timeline events.** Turn and permission `agent_stream` events have no `seq` or
  `epoch`; apply them immediately, outside the seq gate.
- **Plan files outside the cwd.** Claude plan mode writes the plan file under
  `~/.claude/plans/` even when cwd is elsewhere (`planFilePath`).
- **Empty `entries` with compact snapshots.** `get_providers_snapshot_response.entries` is `[]` when
  the client advertises `compact_provider_snapshots`; the data is in `compactSnapshot`.
