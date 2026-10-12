# Paseo daemon WebSocket: transport and session mechanics

Scope: the direct-TCP WebSocket between a client and a Paseo daemon (`paseo` server). It covers the
connection, the handshake, the envelope, request/response correlation, errors, keepalive,
subscriptions, binary frames, and reconnect. It does not cover the relay or E2EE transport, or the
per-message domain schemas beyond the directory and timeline messages needed to show the mechanics.

Source of truth: Paseo tag `v0.10.1` (`/tmp/paseo-v0101`), with a diff check against `v0.9.2` and
`v0.10.2`. Main files:

| Concern | File |
|---|---|
| WS server, upgrade, hello, lease, close codes | `packages/server/src/server/websocket-server.ts` |
| Per-socket delivery/ownership ("owned subscriptions") | `packages/server/src/server/session/owned-subscriptions/{index,replies}.ts` |
| Session dispatch, fetch/subscribe handlers, events | `packages/server/src/server/session.ts` |
| Lease / backpressure constants | `packages/server/src/server/websocket/physical-socket.ts` |
| Directory sync cursors | `packages/server/src/server/directory-sync/` |
| Envelope + hello + server_info schemas | `packages/protocol/src/messages.ts` (search `WebSocket Level Messages`) |
| Client capability names | `packages/protocol/src/client-capabilities.ts` |
| Binary frame codecs | `packages/protocol/src/binary-frames/{demux,terminal,file-transfer}.ts` |
| Reference client | `packages/client/src/daemon-client.ts`, `connection/{index,owned,legacy}.ts` |
| App reconnect / catch-up | `packages/app/src/runtime/directory-sync/index.ts`, `packages/app/src/timeline/{viewed-timeline-sync,timeline-sync-plan}.ts` |

Verified live on 2026-10-02 against `ws://127.0.0.1:6767/ws` (host "alpha", daemon `0.10.1`, no
password), using read-only probes in `/tmp/paseo-probe/*.mjs`. Frames below marked **[live]** were
captured from that daemon. Frames marked **[schema]** were built from the zod schemas.

---

## 1. Connecting

### 1.1 URL

```
ws://<host>:<port>/ws          # default port 6767; wss:// when TLS-fronted
```

- The path must be exactly `/ws`, because the server is `new WebSocketServer({ server, path: "/ws" })`.
  The reference builder `buildDaemonWebSocketUrl()` always emits `/ws`.
- A direct connection takes no query parameters. (`?serverId=…&role=client` is for relay URLs only.)
- The CLI can also use a unix socket: `ws+unix://<socketPath>:/ws`.

### 1.2 Upgrade-time checks (HTTP, before any WS frame)

| Check | Rule | Failure |
|---|---|---|
| Lifecycle | The daemon must be "accepting" | HTTP **503** `Server not ready` |
| `Host` header | Allowed if `localhost`, `*.localhost`, **any IP literal**, or a configured `hostnames` entry | HTTP **403** `Host not allowed` |
| `Origin` header | Allowed if **absent**, `*`, allowlisted, or same-origin as `Host` | HTTP **403** `Origin not allowed` |

A native client should **not send `Origin`**. `tokio-tungstenite` does not send it by default.
Connecting by IP address (LAN or VPN) passes the Host check. A DNS name passes only if the daemon's
`hostnames` config includes it.

### 1.3 Subprotocols and headers

With no password configured, nothing is required. The server echoes the first offered subprotocol,
if any, so offer none.

Password auth exists in three forms. The daemons used for these notes did not enable it.

| Mechanism | Versions | Form |
|---|---|---|
| WS subprotocol | 0.9.x and 0.10.x | `Sec-WebSocket-Protocol: paseo.bearer.<password>`. Only when the password is RFC 7230 token-safe. |
| HTTP header | 0.10.x only | `Authorization: Bearer <password>` |
| In-hello credential | 0.10.x only | `hello.auth = {"kind":"password","password":"…"}` or `{"kind":"localCredential","token":"…"}` |

A bad header credential closes the socket with **4401** `Incorrect password` right after the
upgrade. In 0.9.2 a missing credential also closes with 4401 `Password required`. In 0.10.x the
missing-credential check moves to hello time (see 2.3).

---

## 2. Handshake

### 2.1 Client hello (must be the first frame)

The first frame must be a text frame containing `hello`. The server waits **15 s**
(`HELLO_TIMEOUT_MS`) and otherwise closes with **4001** `Hello timeout`.

Exact frame from the reference client, with crusteo values substituted:

```json
{"type":"hello",
 "clientId":"crusteo-<stable-id>",
 "clientType":"cli",
 "protocolVersion":1,
 "appVersion":"crusteo/0.1.0",
 "capabilities":{
   "hello_rejection":true,
   "owned_subscriptions":true,
   "explicit_event_subscriptions":true,
   "selective_agent_timeline":true,
   "all_providers":true,
   "reasoning_merge_enum":true,
   "custom_mode_icons":true,
   "terminal_reflowable_snapshot":true,
   "provider_subagents":true,
   "projected_subagent_timeline":true,
   "project_updates":true,
   "compact_provider_snapshots":true,
   "provider_snapshot_references":true,
   "timeline_replacement_invalidation":true,
   "timeline_notifications":true,
   "plugin_timeline_items":true,
   "workspace_setup_blocked":true}}
```

Field rules (`WSHelloMessageSchema`):

| Field | Type | Notes |
|---|---|---|
| `type` | `"hello"` | |
| `clientId` | non-empty string | Trimmed. Must not use the reserved plugin client-id form. **This is the session identity** (see 2.4). |
| `clientType` | `"mobile" \| "browser" \| "cli" \| "mcp" \| "hub"` | The CLI and SDK use `"cli"`. |
| `protocolVersion` | int | Must be `>= 1`. Anything lower gets `incompatible_protocol` / close **4003**. Current value is `1`. |
| `appVersion` | string? | Used only for logs and compatibility gating. |
| `auth` | object? | 0.10.x. See 1.3. |
| `capabilities` | object? (passthrough) | All booleans except `browser_host`. Unknown keys are kept. |

Capabilities that matter for a read-mostly native client:

- `owned_subscriptions`: **important.** It makes the socket a "modern" source. A modern source
  receives **only** (a) the first `server_info`, (b) replies to its own requests, and (c) frames
  for subscriptions it opened. Without this capability the socket is "legacy" and gets
  broadcast-era implicit delivery. Recommendation: always send it.
- `explicit_event_subscriptions`, `selective_agent_timeline`: with these, attention, permission,
  and timeline traffic is only delivered when you subscribe to it explicitly.
- `hello_rejection`: asks for a `hello.rejected` frame before an auth or protocol close. Sending
  `auth` implies it.
- The rest gate payload shapes, for example new timeline item types and compact provider
  snapshots. Advertise only what the Rust decoder can tolerate. The safest approach is to
  advertise them and decode unknown variants leniently.

### 2.2 Server welcome: `server_info`

On success the server sends exactly one frame: a `status` session message whose payload is
`status: "server_info"`. The reference client treats this frame as "connected", and only then
flushes queued requests and starts heartbeats.

**[live]** (features list truncated):

```json
{"type":"session","message":{"type":"status","payload":{
  "status":"server_info",
  "protocolVersion":1,
  "serverId":"srv_example",
  "hostname":"alpha",
  "version":"0.10.1",
  "permissions":["daemon.read","daemon.manage","tunnel.manage","access.manage","workspace.read",
                 "workspace.write","workspace.manage","automation.manage","hub.execute"],
  "desktopManaged":false,
  "capabilities":{"voice":{"dictation":{"enabled":true,"reason":""},"voice":{"enabled":true,"reason":""}}},
  "features":{"ownedSubscriptions":true,"agentRequestReceipts":true,"workspaceRequestReceipts":true,
    "creationLifecycle":true,"hubAgentRpc":true,"directorySync":true,"workspaceLabels":true,
    "providersSnapshot":true, "...": "~85 boolean flags total",
    "selectiveAgentTimeline":true,"explicitEventSubscriptions":true,"agentProfiles":true,"agentConfigApply":true}}}}
```

| Field | Notes |
|---|---|
| `serverId` | Stable daemon identity, `srv_…`. Use it as the host key. |
| `hostname` | OS hostname. Nullable. |
| `version` | Daemon semver string. Nullable. |
| `protocolVersion` | `1`. **Absent on 0.9.2.** |
| `permissions` | The session's grants. An owner (no password, or correct password) gets all 9. |
| `features` | Feature gates. Check `ownedSubscriptions`, `explicitEventSubscriptions`, `selectiveAgentTimeline`, `directorySync`, and `workspaceMultiplicity` before relying on the corresponding mechanics. All are `true` on 0.9.2 through 0.10.2. |
| `capabilities` | Server voice/dictation readiness. |

Later `server_info` pushes only arrive if you subscribe to the `status.server_info` event (see 4.4).

### 2.3 Rejection

`hello.rejected` is a top-level frame, **not** wrapped in `session`. 0.10.x sends it only if the
hello carried `auth` or `capabilities.hello_rejection: true`, and then closes the socket.

**[live]** (hello with `protocolVersion: 0`):

```
<< {"type":"hello.rejected","reason":"incompatible_protocol","accepts":["password"]}
-- close 4003 "Incompatible protocol version"
```

`reason` is one of `"password_required"`, `"incorrect_password"`, or `"incompatible_protocol"`.
Auth failures close with **4401** and the reason text `Password required` or `Incorrect password`.

### 2.4 Session identity and resume

- The server keys sessions by `(principalId, clientId)`. principalId is `"owner"` for every
  admitted client today.
- If a hello reuses a clientId whose session is still alive, the server **resumes** it. The
  session can be alive because another socket is still attached, or because it is inside a
  **90 s** grace period after the last socket closed. On resume the server re-sends
  `server_info`, which looks identical to a fresh one. **[live]**: verified by reconnecting with
  the same clientId.
- One session can have **several sockets at once**. Delivery is per socket: replies and
  subscriptions belong to the socket that made the request.
- **Subscriptions do not survive a socket close.** `delivery.detach(socket)` releases every
  subscription the socket owned. The session object surviving does not help a reconnecting
  client: it must re-subscribe (see section 6).
- A second `hello` on an already-admitted socket closes it with **4002** `Unexpected hello`
  **[live]**.
- Use one stable clientId per crusteo instance and host, for example a persisted random id. The
  CLI persists one per home directory with `getOrCreateCliClientId`.

### 2.5 Close codes

| Code | Reason text | When |
|---|---|---|
| 1000 | (client's) | Normal close |
| 1001 | `Server shutting down` | Daemon is stopping or starting |
| 1006 | (none) | Server `terminate()`. Lease expiry (3.6) or the 64 MiB outbound high-water mark. **[live]** after lease expiry. |
| 4001 | `Hello timeout` | No hello within 15 s |
| 4002 | `Session message before hello` | Any non-hello frame before hello, including `{"type":"ping"}` and binary frames **[live]** |
| 4002 | `Invalid hello` | Unparseable or invalid first frame, or empty clientId **[live: non-JSON first frame]** |
| 4002 | `Invalid plugin clientId` | Reserved plugin clientId |
| 4002 | `Unexpected hello` | Second hello on an admitted socket **[live]** |
| 4003 | `Incompatible protocol version` | `protocolVersion < 1` **[live]** |
| 4401 | `Password required` / `Incorrect password` | Auth failure |

---

## 3. Envelope, correlation, errors, keepalive

### 3.1 Frame-level unions

All JSON travels as **text frames**, one JSON object per frame. Binary frames are covered in
section 5.

Client to server (`WSInboundMessageSchema`, discriminated on `type`):

```jsonc
{"type":"hello", ...}                                   // section 2.1
{"type":"ping"}                                         // WS-level liveness ping, no fields
{"type":"recording_state","isRecording":false}          // voice only; ignored otherwise
{"type":"session","message":<SessionInboundMessage>}    // every RPC and subscription
```

Server to client (`WSOutboundMessageSchema`):

```jsonc
{"type":"pong"}                                         // reply to WS-level ping, no payload
{"type":"session","message":<SessionOutboundMessage>}   // everything else
{"type":"hello.rejected","reason":"...","accepts":["password"]}   // 0.10.x
```

### 3.2 Session messages are wrapped, and asymmetric

Session messages always travel inside `{"type":"session","message":…}`. The two directions use
different shapes:

- **Inbound** (client to server) messages are **flat**. `type`, `requestId`, and the arguments are
  siblings:
  `{"type":"session","message":{"type":"fetch_agents_request","requestId":"r1","scope":"active","page":{"limit":2},"subscribe":{}}}`
- **Outbound** (server to client) messages are always `{type, payload}`. `requestId` and
  `subscriptionId` sit **inside `payload`**:
  `{"type":"session","message":{"type":"fetch_agents_response","payload":{"requestId":"r1","subscriptionId":"…","entries":[…],"pageInfo":{…}}}}`

### 3.3 Request/response correlation

- `requestId` is any client-chosen string. The reference client uses `crypto.randomUUID()`, and
  pings use `ping-<ms>-<rand>`. The server echoes it verbatim in `payload.requestId`.
- Each request gets **exactly one** terminal reply carrying that `requestId`: the matching
  response or an `rpc_error`. Some requests are followed by subscription traffic (section 4).
- How the reply type is named (`replies.ts`, `isReply`):
  - `foo_request` → `foo_response` (e.g. `fetch_agents_request` → `fetch_agents_response`)
  - `foo.bar.request` → `foo.bar.response` (e.g. `subscription.release.request` → `subscription.release.response`)
  - `a/b` → `a/b/response`
  - bare `foo` → `foo_response` (e.g. `clear_agent_attention` → `clear_agent_attention_response`)
  - Declared exceptions: `ping`→`pong`; `archive_agent_request`→`agent_archived`;
    `delete_agent_request`→`agent_deleted`; `agent_permission_response`→`agent_permission_resolved`;
    `subscribe_terminals_request`→`terminals_changed`; `daemon.update.request`→`daemon.update.progress`.
    Status replies: `create_agent_request`→`status{status:"agent_created"|"agent_create_failed"}`,
    `resume_agent_request`→`status{agent_resumed}`, `refresh_agent_request`→`status{agent_refreshed}`,
    `restart_server_request`→`status{restart_requested}`, `shutdown_server_request`→`status{shutdown_requested}`.
- Recommended client rule: route any outbound frame with a string `payload.requestId` to the
  pending waiter, then check the type. The reference client also treats `rpc_error`,
  `*_response`, `*.response`, and `*/response` as correlated.
- Responses can arrive **out of order**, because handlers are async and run concurrently
  **[live]**. In the first probe, `fetch_workspaces_response` arrived after the `pong` and
  `rpc_error` frames for requests sent later.

### 3.4 Errors

**`rpc_error`** (correlated) **[live]**:

```json
{"type":"session","message":{"type":"rpc_error","payload":{
  "requestId":"req-bogus-1","requestType":"definitely.not.a.request",
  "error":"Unknown request, try upgrading the daemon (currently v0.10.1)","code":"unknown_schema"}}}
```

| `code` | Cause |
|---|---|
| `unknown_schema` | The `session.message` failed schema validation **and** carried a string `requestId`. This covers both an unknown `type` and a known type with bad or missing fields (see the note below this table). |
| `invalid_message` | Other parse failures where a `requestId` could still be extracted |
| `handler_error` | The handler threw. `error` is `"Request failed: <msg>"`. |
| `access_denied` | The session lacks the permission for that request type |
| domain codes | e.g. `invalid_request`, `fetch_agents_failed`, `fetch_workspaces_failed` |

The `unknown_schema` code is misleading. **[live]**: `{"type":"fetch_agent_request","requestId":"req-bad-1"}`
is a known type with `agentId` missing, and it still returned `code:"unknown_schema"` with the
text "Unknown request, try upgrading the daemon". Do not read `unknown_schema` as "the daemon is
too old" without checking the request yourself.

**Uncorrelated errors**: if no `requestId` can be extracted, the server sends
`status{status:"error"}`. **[live]**:

```json
{"type":"session","message":{"type":"status","payload":{"status":"error","message":"Invalid message: Unexpected token 'h', \"this is not json\" is not valid JSON"}}}
```

These errors **do not close** the socket once hello has completed.

**In-payload errors**: some responses report failure inside the success type:
`fetch_agent_response.payload.error`, `fetch_agent_timeline_response.payload.error`,
`workspace.*.response.payload.{success,error}`. The SDK's `observe()` treats any string
`payload.error` as a failure.

**Timeouts**: the server has none per RPC. The reference client's defaults:

| Timeout | Value |
|---|---|
| WS connect plus `server_info` | 15 s, then reconnect |
| RPC wait | 60 s → `DAEMON_REQUEST_TIMEOUT` |
| Session ping | 5 s |
| Liveness ping | 15 s (3.5) |

On disconnect, every pending waiter is rejected immediately. Responses never cross sockets.

### 3.5 Keepalive and pings

There are two separate pings.

1. **WS-level liveness ping.** This is not a session message. **[live]**:
   ```
   >> {"type":"ping"}
   << {"type":"pong"}
   ```
   The reference client sends one **10 s** after the previous probe settles, with a 15 s timeout.
   **2 consecutive failures** make it dispose the socket and reconnect. Receiving any valid frame
   resets the failure counter.
2. **Session ping** (a correlated RPC with server timestamps). **[live]**:
   ```
   >> {"type":"session","message":{"type":"ping","requestId":"req-ping-1","clientSentAt":1790924201098}}
   << {"type":"session","message":{"type":"pong","payload":{"requestId":"req-ping-1","clientSentAt":1790924201098,"serverReceivedAt":1790924201099,"serverSentAt":1790924201099}}}
   ```
   The client uses this for "verify the connection" checks with a 3 s timeout and for RTT.

The server relies on RFC 6455 control-frame pings only through the ws library's automatic pong.
They neither count toward nor renew the lease.

### 3.6 Application socket lease (affects keepalive design)

- The **first WS-level `{"type":"ping"}`** after hello *claims* a **45 s** lease on the socket.
  After that, **every inbound frame renews it** (any text or binary). The server checks every
  10 s and calls `terminate()` on expired sockets. The client sees **1006 with an empty reason**.
- A socket that **never** sends `{"type":"ping"}` never holds a lease, so it is never reaped for
  silence.
- **[live]**: hello, then one `{"type":"ping"}`, then silence: the socket was terminated at
  49 to 52 s with `close 1006 ""`. The same experiment without the ping: the socket was still
  open after 75 s.
- Consequence for crusteo: either send `{"type":"ping"}` every 10 s, as the reference client
  does, for the life of the socket, or never send it. Never send it once and then stop.

### 3.7 Outbound backpressure

If the socket's `bufferedAmount` plus the next frame would exceed **64 MiB**, the server
terminates the socket (1006). Read promptly. Don't let the Rust read loop stall behind UI work.

---

## 4. Subscriptions

### 4.1 Model

A subscription has three steps:

1. **Open.** Send a request with a `requestId` that asks for a subscription, either through
   `subscribe: {}` on a directory fetch or through a dedicated `*.set_subscription.request` /
   `*.subscribe.request`.
2. **Ack and snapshot.** The correlated response's `payload.subscriptionId` holds a
   **server-assigned UUID**. For directory fetches the response also *is* the initial snapshot.
3. **Updates.** Each pushed message carries `payload.subscriptionId` with that same UUID. Route
   pushes by that id, not by type.
4. **Release.**
   ```
   >> {"type":"session","message":{"type":"subscription.release.request","requestId":"r9","subscriptionId":"<uuid>"}}
   << {"type":"session","message":{"type":"subscription.release.response","payload":{"requestId":"r9","subscriptionId":"<uuid>"}}}
   ```
   **[live]**: the server acks even an **unknown** id (`"no-such-sub"`), so the ack is not proof
   the id existed.

Rules (`SessionDelivery.begin`):
- A modern client **must not** supply `subscribe.subscriptionId`. If it does, the handler throws
  "Subscription IDs are assigned by the host" and the client gets an `rpc_error` with
  `handler_error`. Send `"subscribe":{}`.
- A modern subscription request **must** carry a `requestId`.
- For modern clients, every subscription request creates a **new, independent** subscription. A
  second `agent.timeline.set_subscription.request` does **not** replace the first one. To change
  membership, open the new subscription, then release the old one. Legacy sockets instead use
  one slot per family with last-request-wins.
- Socket close releases everything. `subscriptionId`s change on every reconnect.

### 4.2 Families

| Family | Open with | Ack/snapshot | Pushes |
|---|---|---|---|
| agents | `fetch_agents_request` + `subscribe:{}` | `fetch_agents_response` (snapshot page) | `agent_update` |
| workspaces | `fetch_workspaces_request` + `subscribe:{}` | `fetch_workspaces_response` (snapshot page) | `workspace_update` |
| events | `session.events.set_subscription.request {events:[…], notifications?}` | `session.events.set_subscription.response` (no snapshot) | The listed event types (4.4) |
| timelines | `agent.timeline.set_subscription.request {agentIds:[…]}` | `agent.timeline.set_subscription.response {agentIds (sorted, deduped)}` | `agent_stream`, `agent.timeline.replacement` |
| labels | `workspace.label.list.request` + `subscribe` | `.response` | `workspace.label.update` |
| files | `fs.file.subscribe.request` | | `fs.file.update` |
| diff | `subscribe_checkout_diff_request` | | `checkout_diff_update` |
| terminals | `subscribe_terminals_request` / `subscribe_terminal_request` | | `terminals_changed`, binary terminal frames |
| creation | `creation.subscribe.request` | | `agent.create.update`, `workspace.create.update` |
| browser host | `browser.host.register.request` | | `browser.automation.execute.request` |

### 4.3 Agents and workspaces directory

Request fields (`fetch_agents_request`):

| Field | Values |
|---|---|
| `scope` | `"active"` |
| `filter` | `{labels?, projectKeys?, statuses?, includeArchived?, requiresAttention?, thinkingOptionId?}` |
| `sort` | `[{key: "status_priority"\|"created_at"\|"updated_at"\|"title", direction: "asc"\|"desc"}]` |
| `page` | `{limit: 1..200, cursor?}` |
| `subscribe` | `{}` |
| `sync` | `{generation?, afterSeq?}` |

Request fields (`fetch_workspaces_request`):

| Field | Values |
|---|---|
| `filter` | `{query?, projectId?}` |
| `sort` | keys `"status_priority"\|"activity_at"\|"name"\|"project_id"` |
| `page`, `subscribe`, `sync` | Same as for agents |

Exchange **[live]** (abridged):

```
>> {"type":"session","message":{"type":"fetch_agents_request","requestId":"req-agents-1","scope":"active",
     "sort":[{"key":"updated_at","direction":"desc"}],"page":{"limit":2},"subscribe":{}}}
<< {"type":"session","message":{"type":"fetch_agents_response","payload":{
     "requestId":"req-agents-1",
     "subscriptionId":"a3eb692c-d0a9-40cc-8733-b2ab9a7b8586",
     "entries":[{"agent":{"id":"54939c15-…","provider":"claude","cwd":"/home/user/code",
                          "workspaceId":"wks_3333cccc4444dddd","model":"claude-opus-5-5","status":"running",
                          "activeTurn":{"turnId":"autonomous-turn-7","startedAt":"…"},"title":"do I have paseo checked out…",
                          "labels":{},"pendingPermissions":[],"requiresAttention":true,"attentionReason":"finished",
                          "createdAt":"…","updatedAt":"2026-10-02T06:56:40.986Z","archivedAt":null, "…":"…"},
                 "project":{"projectKey":"prj_1a2b3c4d5e6f7a8b","projectName":"alpha-code","workspaceName":"alpha-crusteo",
                            "checkout":{"cwd":"/home/user/code","isGit":false,"currentBranch":null,"remoteUrl":null,
                                        "worktreeRoot":null,"isPaseoOwnedWorktree":false,"mainRepoRoot":null}}},
                {"agent":{"id":"4ca4d5c7-…","provider":"antigravity","status":"idle", "…":"…"},"project":{…}}],
     "pageInfo":{"nextCursor":"eyJzb3J0Ijpbey…","prevCursor":null,"hasMore":true}}}}

>> {"type":"session","message":{"type":"fetch_workspaces_request","requestId":"req-ws-1",
     "sort":[{"key":"activity_at","direction":"desc"}],"page":{"limit":2},"subscribe":{}}}
<< {"type":"session","message":{"type":"fetch_workspaces_response","payload":{
     "requestId":"req-ws-1","subscriptionId":"40a79f04-ccb9-4330-b4db-0fcb8ccb4d33",
     "entries":[{"id":"wks_1111aaaa2222bbbb","projectId":"prj_1a2b3c4d5e6f7a8b","projectDisplayName":"alpha-code",
                 "projectCustomName":"alpha-code","projectCustomIconRevision":null,"projectRootPath":"/home/user/code",
                 "workspaceDirectory":"/home/user/code","projectKind":"non_git","workspaceKind":"directory",
                 "name":"alpha-code-scratch","title":"alpha-code-scratch","pinnedAt":null,
                 "labels":["personal"],"archivingAt":null,"status":"done","statusEnteredAt":"2026-09-29T18:45:42.316Z",
                 "activityAt":null,"diffStat":null,"scripts":[],"project":{…same shape as agent entry project…}}, …],
     "emptyProjects":[],
     "pageInfo":{"nextCursor":"eyJzb3J0Ijpbey…","prevCursor":null,"hasMore":true}}}}
```

Incremental update, **[live]** `workspace_update` (from a subscription opened with `sync:{}`):

```json
{"type":"session","message":{"type":"workspace_update","payload":{
  "kind":"upsert",
  "workspace":{"id":"wks_3333cccc4444dddd","status":"running","statusEnteredAt":"2026-10-02T06:57:07.308Z", "...":"full descriptor"},
  "generation":"3a52feb7-6b81-4545-886e-8cf08bbebff6","seq":162,
  "subscriptionId":"055dcb8e-7d22-428b-bd1f-4cc159ffc3ec"}}}
```

`agent_update` **[schema]** (not observed during the probe window):

```jsonc
{"type":"session","message":{"type":"agent_update","payload":
  {"kind":"upsert","agent":<AgentSnapshot>,"project":<ProjectPlacement>|null,"generation"?:"…","seq"?:N,"subscriptionId":"…"}}}
{"type":"session","message":{"type":"agent_update","payload":
  {"kind":"remove","agentId":"…","generation"?:"…","seq"?:N,"subscriptionId":"…"}}}
// workspace_update remove: {"kind":"remove","id":"wks_…","emptyProject"?:<ProjectDescriptor>,"generation"?,"seq"?,"subscriptionId"}
```

Semantics:
- **Updates are full-object upserts or removes keyed by id.** They are not patches.
- **Bootstrap ordering is handled by the server.** The subscription is registered *before* the
  snapshot is read. Updates that arrive during the read are buffered per id. After the response is
  sent they are flushed, and upserts older than the snapshot's `updatedAt` are dropped. A client
  can apply the snapshot and then the pushes in arrival order.
- **Paging versus subscription:** the subscription covers the **whole filter**, not the page.
  Open it on the first page only. The app fetches later pages with plain fetches (no
  `subscribe`), following `pageInfo.nextCursor` until `hasMore:false`.
- **Filter exit:** when an agent stops matching the subscription filter, it arrives as
  `kind:"remove"` even though the agent still exists.
- `generation`/`seq` only appear on pushes when the subscription was opened with `sync` present
  (see 4.5).

### 4.4 Session event subscriptions

```
>> {"type":"session","message":{"type":"session.events.set_subscription.request","requestId":"req-events-1",
     "events":["agent_attention_required","agent_permission_request","agent_permission_resolved","status.server_info","project.update"]}}
<< {"type":"session","message":{"type":"session.events.set_subscription.response","payload":{"requestId":"req-events-1","subscriptionId":"b8cf3a84-…"}}}
```
**[live]**

The `events` enum (`SessionEventSubscriptionSchema`): `project.update`,
`providers_snapshot_update`, `agent_attention_required`, `agent_permission_request`,
`agent_permission_resolved`, `checkout_status_update`, `script_status_update`,
`workspace_setup_progress`, `agent.provider_subagents.update`, `terminal_attention_required`,
`status.server_info`, `status.daemon_config_changed`, `status.plugin_catalog_changed`,
`status.plugin_settings_changed`, `activity_log`, `hub.execution.agent.update`,
`hub.execution.agent.stream`.

`status.*` entries match `{"type":"status","payload":{"status":"<suffix>",…}}`. The optional
`notifications:true` lets this socket win the `shouldNotify` flag on attention events.
There is no snapshot, so read the current state with an RPC.

### 4.5 Directory sync cursors (`features.directorySync`)

Adding `sync` to a directory fetch makes the read sequenced:

- Agents require `scope:"active"` and no `filter`. Otherwise the reply is
  `rpc_error invalid_request` "Sequenced agent directory reads require scope=active and no filter."
  **[live]**.
- Workspaces require no `filter`.
- **`page` is ignored.** With `sync` the server returns the whole directory with
  `pageInfo.hasMore:false` **[live]**, even with `page.limit:1`.
- Each entry gets `syncSeq`, and the response gets
  `sync: {generation, headSeq, mode, reason?, removals:[{id,seq}]}`.
- `generation` is a random UUID per daemon process, shared by the agent, workspace, and project
  collections. **Each collection has its own seq counter.**

| `sync` sent | Result **[live, workspaces, headSeq=310]** |
|---|---|
| `{}` | `mode:"snapshot", reason:"no_cursor"`, all 7 entries |
| `{generation, afterSeq: headSeq}` | `mode:"changes"`, 0 entries, `removals:[]` |
| `{generation, afterSeq: headSeq-5}` | `mode:"changes"`, the 1 entry with `syncSeq > afterSeq` |
| `{generation:"bogus", afterSeq:1}` | `mode:"snapshot", reason:"generation_changed"` (daemon restarted) |
| `{generation, afterSeq: headSeq+1000}` | `mode:"snapshot", reason:"cursor_expired"` (also returned when `afterSeq` is below the tombstone horizon; 5000 tombstones are kept) |

In `changes` mode, apply the upserts and `removals` on top of the cached state. In `snapshot`
mode, replace the cache. Later pushes carry `generation`/`seq`, so advance the cursor to
`max(seq)`.

### 4.6 Per-agent timeline stream

Open a subscription, then read history with a separate RPC. **[live]**:

```
>> {"type":"session","message":{"type":"agent.timeline.set_subscription.request","requestId":"req-tl-sub-1","agentIds":["54939c15-…"]}}
<< {"type":"session","message":{"type":"agent.timeline.set_subscription.response","payload":{"agentIds":["54939c15-…"],"requestId":"req-tl-sub-1","subscriptionId":"f7af47f1-…"}}}
>> {"type":"session","message":{"type":"fetch_agent_timeline_request","requestId":"req-tl-1","agentId":"54939c15-…","direction":"tail","limit":2,"projection":"projected"}}
<< {"type":"session","message":{"type":"fetch_agent_timeline_response","payload":{
     "requestId":"req-tl-1","agentId":"54939c15-…","agent":<AgentSnapshot>,
     "direction":"tail","projection":"projected","epoch":"3eb6d0ac-dcaa-4063-9839-8fb2b48dd643",
     "reset":false,"staleCursor":false,"gap":false,
     "window":{"minSeq":1,"maxSeq":500,"nextSeq":501},
     "startCursor":{"epoch":"3eb6d0ac-…","seq":490},"endCursor":{"epoch":"3eb6d0ac-…","seq":500},
     "hasOlder":true,"hasNewer":false,
     "entries":[{"provider":"claude","item":{"type":"tool_call","callId":"toolu_01LA…","name":"Task","status":"running",…},
                 "timestamp":"…","seqStart":490,"seqEnd":500,
                 "sourceSeqRanges":[{"startSeq":490,"endSeq":491},{"startSeq":493,"endSeq":496},{"startSeq":500,"endSeq":500}],
                 "turnId":"autonomous-turn-7","collapsed":["tool_lifecycle"]}, …],
     "error":null}}}
<< {"type":"session","message":{"type":"agent_stream","payload":{
     "agentId":"54939c15-…",
     "event":{"type":"timeline","provider":"claude","item":{"type":"tool_call","callId":"toolu_01Sj…","name":"Task","status":"running",…},"turnId":"autonomous-turn-7"},
     "timestamp":"2026-10-02T06:56:44.438Z","seq":501,"epoch":"3eb6d0ac-…",
     "subscriptionId":"f7af47f1-…"}}}
```

- A timeline row has a cursor `{epoch, seq}`. `seq` is per agent and increases by exactly 1 per
  canonical row. `epoch` changes when the timeline is rebuilt.
- The live `agent_stream` timeline events carry `seq` and `epoch`. Above, `endCursor.seq` was
  500 and the next push had `seq` 501.
- Live events are **canonical** rows. With `projection:"projected"`, history merges a tool call's
  lifecycle into one entry (`collapsed`, `sourceSeqRanges`). Merge live tool-call rows by
  `callId`.
- `fetch_agent_timeline_request`:
  - `direction`: `"tail"`, `"before"`, or `"after"`
  - `cursor`: `{epoch, seq}`
  - `limit`: 0 means all
  - `projection`: `"projected"` or `"canonical"`
  - `mergeWindow`: optional
- Response flags:
  - `staleCursor`: the cursor's epoch is not the current one.
  - `gap`: an `after` cursor fell below `minSeq-1`, because history was trimmed.
  - `reset`: `staleCursor || gap`. When it is true the server answers as a **tail** read.
    Replace the local timeline.
- `agent.timeline.replacement {agentId, epoch, subscriptionId}` means the server rebuilt the
  timeline. Drop local state for that agent and refetch the tail.
- `agent_stream.event.type` is something other than `timeline` for turn, status, and attention
  events. Those carry no `seq`.

---

## 5. Binary frames

Binary WS frames are separate from JSON. The server tries `decodeBinaryFrame` on **every** inbound
frame, text frames included (it decodes text as UTF-8 bytes). Valid JSON starts with `{`
(0x7B), so it never collides. Unknown opcodes fall through to JSON parsing, which fails with a
`status{error}` frame **[live: 0x7F frame]**.

Byte 0 is the opcode:

| Opcode | Kind | Layout |
|---|---|---|
| `0x01` Output (S→C) | terminal | `[op][slot u8][bytes…]` raw PTY output |
| `0x02` Input (C→S) | terminal | `[op][slot][bytes…]` keystrokes |
| `0x03` Resize (C→S) | terminal | `[op][slot][JSON {rows,cols,intent?:"claim"\|"update"}]` |
| `0x04` Snapshot (S→C) | terminal | `[op][slot][JSON TerminalState]` |
| `0x05` Restore (S→C) | terminal | `[op][slot][bytes…]` replay output |
| `0x10` FileBegin (S→C) | file | `[op][idLen u8][requestId utf8][metaLen u16 BE][JSON {mime,size,encoding:"utf-8"\|"binary",modifiedAt,revision?,fileName?}]` |
| `0x11` FileChunk | file | `[op][idLen][requestId][bytes…]` |
| `0x12` FileEnd | file | `[op][idLen][requestId]` (no body) |

- **Terminals.** `subscribe_terminal_request` (JSON) returns a response that assigns a `slot`
  (u8). After that, terminal I/O for that terminal goes over binary frames tagged with the slot.
  The slot mapping is per connection, and the client clears all slots on disconnect.
- **File reads.** A `file_explorer_request` for a file may be answered with
  `FileBegin/FileChunk*/FileEnd`, correlated by the same `requestId` as the JSON request. The SDK
  then synthesizes a `file_explorer_response` with `file:null`. Large files can be refused with
  `error:"File is too large to display"`.
- Binary frames sent before hello close the socket with 4002.

---

## 6. Reconnect semantics

### 6.1 What the reference `DaemonClient` does

1. On any close, error, connect timeout, or two liveness failures, it does all of the following:
   - Marks every subscription disconnected and forgets its `subscriptionId`.
   - Rejects all pending RPC waiters and queued sends with `DaemonConnectionError`.
   - Clears the terminal slots.
   - Drops `lastServerInfo`.
2. It reconnects with backoff `min(1500 ms · 2^attempt, 30 s)`. It resets the attempt counter
   when a `server_info` arrives. Auth failures disable reconnect.
3. It opens a new socket and sends the same `hello` with the **same clientId**. If within 90 s,
   this is a server-side session resume, but that doesn't preserve subscriptions.
4. On `server_info` it moves to `connected`, starts the liveness timer, and then:
   - **Re-issues every live subscription's original request** with a fresh `requestId`. Each one
     gets a new `subscriptionId` and a **fresh snapshot**. Observers receive the new snapshot
     through `snapshot()`.
   - Then flushes RPCs that were queued while connecting.
5. **No cursor is replayed automatically.** The transport layer only restores live delivery.
   Gap recovery is the consumer's job. The SDK's own comment: "Restores live delivery; the
   consumer owns history reads and gap recovery".

### 6.2 What the app layers on top

- **Directory (agents and workspaces).** The app persists `{generation, afterSeq}` per
  collection. On reconnect it re-opens `fetch_agents_request{scope:"active", sort:updated_at desc,
  page:{limit:200}, subscribe:{}, sync:<cursor>}` and the matching workspaces request. Then it
  applies the result:
  - `mode:"changes"` (upserts plus `removals`) is applied on top of the cache.
  - `mode:"snapshot"` replaces the cache.

  Live pushes received during the refresh are recorded in a transaction and replayed on top.
  It also opens `session.events` for `["project.update","script_status_update"]`.
- **Timelines.** For the set of agents being viewed, the app:
  - Opens `agent.timeline.set_subscription.request{agentIds}`.
  - **After the ack**, fetches `fetch_agent_timeline_request{direction:"after",
    cursor:{epoch, seq:<last endSeq>}, limit:<page>, projection:"projected"}`.
  - Repeats from `endCursor` while `hasNewer`. If the first resume page already has `hasNewer`,
    it gives up and fetches the latest `tail` instead.
  - With no cached cursor, it fetches `direction:"tail"`.
  - Handles `reset`/`staleCursor` and `agent.timeline.replacement` by replacing local state.
  - Treats a non-contiguous live `seq` (not `last+1`) as a gap and recovers it with an `after`
    fetch.

### 6.3 Recommended crusteo flow

```
connect ws://host:6767/ws (no Origin, no subprotocol)
send hello{clientId: stable, clientType:"cli", protocolVersion:1, capabilities:{owned_subscriptions, explicit_event_subscriptions, selective_agent_timeline, hello_rejection, …}}
await status.server_info (15 s timeout) -> record serverId/version/features
open: fetch_agents_request{scope:"active", subscribe:{}, sync:<cursor or {}>}
open: fetch_workspaces_request{subscribe:{}, sync:<cursor or {}>}
optional: session.events.set_subscription.request{events:[agent_attention_required, agent_permission_request, agent_permission_resolved, status.server_info]}
per viewed agent: agent.timeline.set_subscription.request{agentIds}, then fetch_agent_timeline_request tail/after
every 10 s: {"type":"ping"} expecting {"type":"pong"} within 15 s (2 misses -> reconnect)
on close: drop all subscriptionIds and waiters; backoff 1.5 s·2^n ≤30 s; redo everything above using saved cursors
```

---

## 7. Version notes (0.9.2 → 0.10.2)

`git diff v0.9.2 v0.10.2` over the protocol, WS server, and client shows only these transport
changes:

- **0.10.x adds:**
  - `hello.auth` (password or localCredential)
  - the `hello_rejection` capability and the top-level `hello.rejected` frame
  - `server_info.protocolVersion`
  - `Authorization: Bearer` header auth
  - admission (password check) moved from upgrade time to hello time
- **0.9.2:**
  - Password only through the `paseo.bearer.<pw>` subprotocol.
  - Auth failures are a bare close 4401 with no `hello.rejected`.
  - `server_info` has no `protocolVersion`.
  - The hello schema is not strict, so an extra `auth` or `hello_rejection` is silently stripped.
    The same hello frame works on every version in the fleet.
- **0.10.1 → 0.10.2:** no protocol or WS changes. Only the package version changed.
- Unchanged across the range: the `/ws` path, close codes, the 15 s hello timeout, the 45 s lease,
  the 90 s resume grace, the envelope, owned subscriptions, and directory sync.

### 0.10.x → 0.11.2

Checked on 2026-10-11 by diffing `packages/protocol` from `v0.10.1` to `v0.11.2`, and live against
0.10.1, 0.10.2, 0.10.3, 0.11.0 and 0.11.1 daemons and an isolated throwaway 0.11.2 daemon.

- The WS protocol version is still **1**; the server rejects only `protocolVersion < 1`.
- Every message change is additive: optional fields (usage window `shortLabel` and `summary`,
  plugin source `registry`, provider model `desktopTrigger`), a looser `ProviderOptions`, and a new
  usage API. Nothing crusteo uses was removed or renamed.
- **Usage reports** (`usageSources` feature, 0.11.0+): `usage.list_reports.request` is answered by
  one `usage.list_reports.update` per account (`{requestId, report}`, the report naming the account
  in `account.label` and the provider in `sourceId`) and then `usage.list_reports.response`
  (`{requestId, error}`). Every message carries the request id, so the client collects updates
  until the response (`Client::request_until`).
- `provider.usage.list.request` still works on 0.11.2 but is marked
  `COMPAT(providerUsageList) … remove after 2027-03-26`; it is the reports flattened to one entry
  per account with only the provider as id. crusteo uses it only for daemons without
  `usageSources`.
- 0.11.2 has wire-compatibility tests (`messages.wire-compat.test.ts`): older shapes keep parsing
  and new behavior is opt-in.

## 8. Surprises and gotchas (code versus live)

1. **`unknown_schema` also covers malformed known requests.** A missing `agentId` on
   `fetch_agent_request` returned "Unknown request, try upgrading the daemon" **[live]**. The
   cause is `isUnknownSchema = requestId != null && type === "session"`, regardless of the actual
   zod error.
2. **`subscription.release.request` always acks,** even for ids that never existed **[live]**.
3. **`sync` overrides `page`.** A sequenced directory read returns everything with `hasMore:false`
   even when `page.limit` is 1 **[live]**.
4. **Sending one WS ping commits you to keepalive.** One `{"type":"ping"}` followed by silence was
   terminated at about 50 s with code 1006 and an empty reason **[live]**. A socket that never
   pings is not leased.
5. **Inbound and outbound shapes differ.** Requests are flat and responses use `{type, payload}`.
   `requestId` and `subscriptionId` live in `payload` on the way out.
6. **Responses are not FIFO** relative to requests **[live]**.
7. **A modern client gets nothing unsolicited.** After `server_info` it receives no traffic
   unless it subscribes. A legacy (no-capabilities) socket observed for 8 s also received only
   `server_info`, because the daemon was quiet. In code, legacy sockets get broadcast-era
   delivery, including all `agent_stream` frames without `selective_agent_timeline`.
8. **Per-collection `seq` counters share one `generation`.** Agents `headSeq` was 158 and
   workspaces `headSeq` was 160/310 under the same generation **[live]**. Never compare seqs
   across collections.
9. **The session survives disconnects but subscriptions don't.** The 90 s resume grace keeps the
   server-side `Session` (and its clientId identity), but every subscription is released when its
   socket closes.

## Appendix: probe scripts

These are throwaway scripts and are not in the repo. Run them with Node 22 or later, which has a
global `WebSocket`.

| Script | Purpose |
|---|---|
| `/tmp/paseo-probe/probe.mjs` | Handshake, fetch agents/workspaces with subscribe, events, both pings, bogus requests, timeline subscribe and tail, release. Writes `frames.jsonl`. |
| `/tmp/paseo-probe/listen.mjs` | Long listen on directory subscriptions with `sync:{}` and a 10 s ping. `LEGACY=1 NOREQ=1` gives the no-capabilities variant. |
| `/tmp/paseo-probe/edge.mjs`, `edge2.mjs` | Pre-hello frames, protocolVersion 0, garbage, same-clientId resume, invalid JSON, unknown binary, duplicate hello. |
| `/tmp/paseo-probe/sync.mjs` | Directory sync cursor modes |
| `/tmp/paseo-probe/lease.mjs`, `nolease.mjs` | Lease expiry with and without an initial WS ping |
