# Paseo v0.10.1: hosts, workspaces, sidebar, tabs, shortcuts, design tokens

Scope: the **navigation structure** of the Paseo desktop app (Electron build of `packages/app`), written so a native GPUI client can behave and look the same. The wire transport and the agent timeline are covered in sibling docs; this doc only restates the envelope where it's needed to read the examples.

Source of truth: Paseo repo at tag `v0.10.1`, checked out at `/tmp/paseo-v0101`. Unless a path is absolute, it is relative to `/tmp/paseo-v0101/packages/`. Anything not confirmed by reading code is marked **(inferred)**. A full list of files read is at the end.

---

## 0. Conventions used below

**Wire envelope.** WebSocket frames are `{"type":"session","message":<msg>}` in both directions. Other top-level frame types are `hello`, `ping`, `pong`, `recording_state` and `hello.rejected` (`protocol/src/messages.ts` ~7440–7485).
- **Requests are flat:** `{"type":"<x>_request" | "<ns>.request", "requestId":"…", …fields}`.
- **Responses and pushes are wrapped:** `{"type":"…", "payload":{…}}`. Responses echo `payload.requestId`.
- **Two naming families:**
  - legacy snake_case: `fetch_workspaces_request` → `fetch_workspaces_response`
  - dotted: `workspace.pin.set.request` → `workspace.pin.set.response`. The client derives the response type with `type.replace(/\.request$/, ".response")`.
- **Generic failure** (used by fetch_workspaces, project.list and all `workspace.label.*` RPCs):
  `{"type":"rpc_error","payload":{"requestId","requestType","error","code"}}`
- **Owned subscriptions:**
  - The client sends `subscribe: {}`. The daemon assigns `subscriptionId` and returns it in the response; later pushes carry `payload.subscriptionId`.
  - Release with `{"type":"subscription.release.request","requestId","subscriptionId"}` → `subscription.release.response {requestId, subscriptionId}`.
- **Feature gates:** capabilities are booleans in `server_info.features` (from the `status` / `server_info` frame). `hostSupportsFeature(serverInfo, name)` is `features?.[name] === true` (`app/src/runtime/host-features.ts`).

**Storage.** "AsyncStorage" means `window.localStorage` in Electron/web **(inferred from the library's web backend)**. A native client can use any local store; the key names are listed so state could be migrated.

---

## 1. Hosts

### 1.1 Saved host registry

| Item | Value |
|---|---|
| Storage key | `@paseo:daemon-registry` (`REGISTRY_STORAGE_KEY`, `app/src/runtime/host-runtime.ts:1376`) |
| Format | `JSON.stringify(HostProfile[])`, rewritten whole on every change |
| Client id | `@paseo:client-id-v1` → `cid_<uuid without dashes>` (`app/src/utils/client-id.ts`) |
| Schema | `app/src/types/host-connection.ts` (all `z.strictObject`) |

```ts
StoredHostConnection = discriminatedUnion("type", [
  { id?, type: "directTcp",    endpoint: string, useTls?: boolean, password?: string /* legacy; migrated up to profile.password */ },
  { id?, type: "directSocket", path: string },
  { id?, type: "directPipe",   path: string },
  { id?, type: "remoteSsh",    host: string, sshPort?: number, daemonPort?: number },
  { id?, type: "relay",        relayEndpoint: string, useTls?: boolean, daemonPublicKeyB64: string },
])
StoredHostProfile = {
  serverId: string (min 1),
  password?: string,
  label?: string,
  appearance?: { color: "none"|"violet"|"sky"|"emerald"|"orange"|"pink"|"indigo"|"teal"|"red"|"amber"|"blue",
                 badgeDisplay: "name"|"icon"|"hidden"|null },
  lifecycle?: {},                       // reserved, always {}
  connections: StoredHostConnection[] (min 1),
  preferredConnectionId?: string|null,  // persisted but NOT read by connection selection (latency wins)
  createdAt?: ISO, updatedAt?: ISO,
}
```

**Connection ids are derived, not random.** They are recomputed on load:

| type | id |
|---|---|
| directTcp | `direct:<host:port>`; loopback addresses become `localhost:<port>` |
| directSocket | `socket:<path>` |
| directPipe | `pipe:<path>` |
| remoteSsh | `ssh:<encodeURIComponent(host)>:<sshPort or "">:<daemonPort or "">` (daemonPort 6767 is dropped) |
| relay | `relay:wss:<endpoint>` when `useTls === true`, else `relay:<endpoint>` |

Example stored value:
```json
[{"serverId":"srv_abc123","password":"hunter2","label":"alpha",
  "appearance":{"color":"teal","badgeDisplay":null},"lifecycle":{},
  "connections":[
    {"id":"direct:alpha.example:6767","type":"directTcp","endpoint":"alpha.example:6767","useTls":false},
    {"id":"relay:wss:relay.paseo.sh:443","type":"relay","relayEndpoint":"relay.paseo.sh:443","useTls":true,"daemonPublicKeyB64":"BASE64KEY"}],
  "preferredConnectionId":"direct:alpha.example:6767",
  "createdAt":"2026-09-01T12:00:00.000Z","updatedAt":"2026-09-02T08:00:00.000Z"}]
```

**Load rules** (`HostRuntimeStore.loadFromStorage`):
- Invalid JSON, or any profile failing normalisation, discards the whole key.
- Profiles whose `serverId` starts with `local:` (placeholders) are dropped.
- **Upsert merge:** profiles with the same `serverId`, or that already contain an equal connection, merge into one. Connections are upserted by id. The label is replaced only if it still equals the `serverId`.
- **Removal:** removing a host's last connection removes the host.

**How each connection type becomes a transport:**

| type | URL |
|---|---|
| directTcp | `ws(s)://host:port/ws` |
| relay | `ws(s)://<relay>/ws?serverId=<id>&role=client&v=2`, E2EE with `daemonPublicKeyB64`. Default relay is `relay.paseo.sh:443`, with TLS when the port is 443 |
| directSocket / directPipe / remoteSsh | Electron-only IPC transports (`paseo+desktop://socket?path=…`, `…pipe?path=…`, `…ssh?host=…&port=…&daemonPort=…`) |

**Pairing links** (`protocol/src/connection-offer.ts`). The offer is `{v:2, serverId, daemonPublicKeyB64, relay:{endpoint, useTls?}}`, carried in one of these forms:
- `https://app.paseo.sh/#offer=<base64url(JSON)>`
- `relay://<host:port>/<serverId>?key=<pubkey>&ssl=true&password=<pw>`
- any URL with `#connect=<urlencoded relay:// URI>`
- `tcp://host:port?ssl=true&password=…`

`components/pair-link-modal.tsx` shows a password field on the `password_required` and `incorrect_password` errors.

### 1.2 Multiple hosts at once

`HostRuntimeStore` is a process singleton. For **each** `serverId` it holds:
- one `HostRuntimeController` (connection state machine, at most one live `DaemonClient`)
- one `DirectorySync`
- one `TimelineReplica`
- one entry in `useSessionStore.sessions[serverId]`

One `ReplicaCache` is shared and keyed by serverId. `syncHosts()` diffs the registry against these: new hosts get a controller and `start()`, removed hosts are stopped, disposed and cleared. **All hosts connect concurrently and independently.**

**Selecting a connection within one host** (latency-driven, `host-runtime.ts`):
- **Probe cycle:** a tick every 2 s. Each probe opens a throwaway client, requires `server_info` with the expected serverId, and measures latency with a 5 s timeout.
- **Probe intervals:**
  - the active online connection: every 10 s (uses the live client's liveness RTT)
  - other connections while one is online: every 120 s
  - otherwise by age: every 2 s, then 5 s, then 10 s, then 30 s
- **Picking the active connection:**
  - The first success becomes active.
  - Switch away only if the active connection is unavailable, or another one beats it by at least 40 ms on 3 consecutive cycles.
  - Auth-rejected connections are skipped until the password changes.
- **Reconnect** (inside `client/src/daemon-client.ts`):
  - backoff is `min(1500·2^attempt, 30000)` ms; connect timeout 15 s
  - heartbeat every 10 s with a 15 s timeout; 2 misses force a reconnect
  - an auth failure stops reconnecting
- **Startup probe:** on boot the app probes `localhost:6767` once (2.5 s timeout). Electron instead starts or attaches to the built-in daemon over IPC (`start_desktop_daemon`) and upserts it from its listen string.

**Status enums (exact strings):**

| Name | Values |
|---|---|
| `HostRuntimeConnectionStatus` | `"idle" \| "connecting" \| "online" \| "offline" \| "error"` (idle is never emitted; unknown hosts read as `connecting`) |
| `DaemonClient` state | `{status:"idle"} \| {status:"connecting",attempt} \| {status:"connected"} \| {status:"disconnected",reason?} \| {status:"disposed"}` |
| Client → host status mapping | connected→online; connecting/idle→connecting; disposed or `disconnected` with no reason / `"client_closed"`→offline; any other reason→error |
| `authFailureReason` | `"password_required" \| "incorrect_password" \| null` |
| `agentDirectoryStatus` | `"idle" \| "initial_loading" \| "revalidating" \| "ready" \| "error_before_first_success" \| "error_after_ready"` |
| `ConnectionProbeState` | `{status:"pending"} \| {status:"unavailable"} \| {status:"available", latencyMs}` |

`connectionEpoch` increments on each transition into online, and `clientGeneration` on each new client. Together they form the `DirectorySourceToken` that invalidates stale fetches.

### 1.3 Per-host status in the UI

- **`HostStatusDot`** (`app/src/components/host-status-dot.tsx`): an 8×8 circle.
  - `online` → `statusSuccess`
  - `connecting` → `statusWarning`
  - anything else → `statusDanger`
- **Text labels** (`utils/daemons.ts`): `Online`, `Connecting`, `Offline`, `Error`, `Idle`.
- **Host picker** (`app/src/components/hosts/host-picker.tsx`, opened from the sidebar footer `Server` button):
  - Hosts are listed local-first. Each row shows status dot + label + active-connection description + a trailing `Settings` gear.
  - The description is `"Local"` for socket/pipe, otherwise the endpoint without `:443`/`:80`.
  - System rows: `__add_host__` "Add host" (also `__all_hosts__`, `__enable_built_in_daemon__`).
  - The list becomes searchable at 10+ hosts. Desktop placement is `top-start`, minWidth 240.
  - **Selecting a host opens its settings** (`/settings/hosts/<id>/host`). It is **not** an "active host" switch; the app has no single active host.
- **Host badge in workspace rows** (`app/src/hosts/appearance.ts`, `use-host-badges.ts`, `host-badge.tsx`):
  - Renders a lucide `Server` 12px, plus the label when `badgeDisplay === "name"`.
  - Colour is `identityForeground(color)`, or `foregroundMuted` for `"none"`.
  - When `badgeDisplay` is `null`, the effective value is `"hidden"` for the local daemon and `"name"` for remote hosts.
  - Badges only appear when the sidebar's projects span ≥ 2 hosts and the "Host" row item is on.
- **Host chooser modal** (`app/src/hosts/host-chooser.tsx`), used when an action needs a host:
  - With 0 hosts it goes to add-host; with 1 host it picks it immediately.
  - Otherwise: a 640px modal titled "Choose host" with a "Search hosts..." input. Rows are dot + label + serverId. ↑/↓ wrap, Enter chooses, Esc closes.
- **Host filter:** the host *filter* for the sidebar lives in the Display-preferences menu (§3.8).

### 1.4 Directory sync (`app/src/runtime/directory-sync/**`)

**What it keeps per host:**
- **Agents:** `AgentDirectoryReplica`, with data in `sessions[serverId].agents`.
- **Workspaces and projects:** `WorkspaceDirectoryReplica`, made of `Map<workspaceId, WorkspaceDescriptor>`, `Map<projectId, ProjectDescriptor>` and a project→workspace index.
- **Cursors:** `DirectoryCheckpoint = {projects?, workspaces?, agents?: {generation: string, afterSeq: number}}`.

**Demand-driven:**
- Nothing is fetched until a view acquires demand.
- **Full demand** (the sidebar) runs `refreshAll`, in this order:
  1. `project.list.request` (if feature `projectList`)
  2. paged `fetch_workspaces_request` (only page 1 subscribes)
  3. `fetch_agents_request`
  4. connect workspace labels
- It also subscribes to session events `["project.update","script_status_update"]` via `session.events.set_subscription.request`.

Requests (exact fields):
```ts
fetch_workspaces_request { type, requestId,
  filter?: { query?, projectId?, idPrefix? /*ignored*/ },
  sort?: { key: "status_priority"|"activity_at"|"name"|"project_id", direction: "asc"|"desc" }[],
  page?: { limit: 1..200, cursor? }, subscribe?: { subscriptionId? }, sync?: { generation?, afterSeq? } }
fetch_agents_request { type, requestId, scope?: "active",
  filter?: { labels?, projectKeys?, statuses?, includeArchived?, requiresAttention?, thinkingOptionId? },
  sort?: { key: "status_priority"|"created_at"|"updated_at"|"title", direction }[],
  page?, subscribe?, sync? }
"project.list.request" { type, requestId, sync?: { generation?, afterSeq? } }
"session.events.set_subscription.request" { type, requestId, events: string[], notifications?: boolean }
```

Every list response can carry:
```ts
DirectorySyncMetadata = { generation: string, headSeq: int, mode: "snapshot"|"changes",
  reason?: "no_cursor"|"generation_changed"|"cursor_expired"|"changes_too_large",
  removals: { id, seq }[] }
```

**Sync semantics:**
- `generation` is a random UUID per daemon process.
- A missing or different generation, or an expired `afterSeq` (tombstones are trimmed at 5000), produces `mode:"snapshot"`. Otherwise the response is `mode:"changes"`: only entries with `seq > afterSeq`, plus `removals`.
- With `sync` set, the daemon ignores paging and `filter` is rejected (`invalid_request`).
- Live pushes carry `generation` and `seq`, which advance the cursor monotonically.
- Deltas that arrive while a fetch is in flight are buffered in a per-family transaction (`transaction.ts`) and replayed after the snapshot commits. A source-token change discards the transaction as stale.

Example:
```json
{"type":"session","message":{"type":"fetch_workspaces_request","requestId":"6f1c…","sort":[{"key":"activity_at","direction":"desc"}],"page":{"limit":200},"subscribe":{},"sync":{"generation":"b6e0…","afterSeq":17}}}
```

### 1.5 Replica cache (`app/src/runtime/replica-cache/**`)

Persists the directory and a timeline tail per host, so the sidebar renders instantly on startup.

**Electron/web store:** IndexedDB `paseo-replica-row-store`, v1.
- Object store `rows` with keyPath `["serverId","kind","id"]`. Rows are `{serverId, kind, id, payload}`.
- Object store `meta` holds `schema_version` = 1. A mismatch clears `rows`.

**Native store:** SQLite `paseo-replica-row-store.db`.
```sql
CREATE TABLE rows (server_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('agent','workspace','project','timeline','checkpoint')),
  id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY (server_id, kind, id));
CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
```

**Payloads per kind:**

| kind | payload |
|---|---|
| `agent` | `{snapshot (trimmed AgentSnapshotPayload), turn?: {phase:"idle"} \| {phase:"open",turnId,startedAt}, projectPlacement, lastActivityAt}` |
| `workspace` | full descriptor (`activityAt` forced null) |
| `project` | project descriptor |
| `checkpoint` (id `"singleton"`) | `{version:1, cursors:{projects?,workspaces?,agents?}}` |
| `timeline` (id = agentId) | `{agentId, items (≤50), range:{epoch,startSeq,endSeq}\|null, hasOlder}` |

**Behaviour:**
- **Writes:** batched and flushed after 1 s, when the app is hidden, and before reads.
- **Budget:** 32 MiB. When over, whole hosts are evicted least-recently-written first. An evicted host ignores mutations until its next full baseline.
- **Host removal:** hosts that leave the registry get their rows deleted.
- **Legacy cleanup:** the old `paseo-replica-cache` DB / `@paseo:replica-cache` key is deleted once.

**Activity** (`runtime/activity/*`): an in-memory `Map<agentId, Date>` of last activity, coalesced per animation frame. It is not persisted separately; `lastActivityAt` lives in the agent row.

---

## 2. Projects and workspaces

### 2.1 Identity and enums

| Thing | Value |
|---|---|
| workspaceId | `wks_` + 16 hex. `workspace.create.request.workspaceId` must match `^wks_[a-f0-9]{16}$` |
| projectId | `prj_` + 16 hex (legacy-migrated ids may be a path or a `remote:` key) |
| projectKey | cross-host equivalence key: `remote:<host>/<path>[#subdir:<rel>]`, or a normalised local path |
| status (`WorkspaceStateBucket`) | `"needs_input" \| "failed" \| "running" \| "attention" \| "done"` |
| projectKind | `"git" \| "non_git" \| "directory"` (directory is legacy) |
| workspaceKind | `"directory" \| "local_checkout" \| "checkout" \| "worktree"` (checkout is legacy) |
| Agent lifecycle | `"initializing" \| "idle" \| "running" \| "error" \| "closed"` |
| attentionReason | `"finished" \| "error" \| "permission" \| null` |
| Label colours | `violet, sky, emerald, orange, pink, indigo, teal, red, amber, blue` (new-label default `violet`) |

**"Isolation" is an app-only concept** (new-workspace screen: `"local" | "worktree"`):
- `local` sends `source.kind:"directory"`, a new workspace record over the existing directory.
- `worktree` sends `source.kind:"worktree"`: the daemon cuts a Paseo-owned git worktree. It is only offered when `projectKind === "git"`.

**What `workspaceKind` means:** `local_checkout` is a non-linked git checkout, `worktree` is a linked git worktree, and `directory` is non-git.

### 2.2 `WorkspaceDescriptorPayload` (`messages.ts` ~3968)

```ts
{ id, projectId,
  projectDisplayName,                 // customName ?? derived
  projectCustomName?: string|null, projectCustomIconRevision?: string|null,
  projectRootPath, workspaceDirectory? /* defaults to projectRootPath */, worktreeSlug?,
  projectKind, workspaceKind,
  name,                               // title ?? live branch (≠HEAD) ?? dir basename
  title?: string|null,                // raw user title; null = derived
  pinnedAt?: string|null,             // ISO; null/absent = not pinned
  labels?: string[],                  // label NAMES (omitted when empty)
  archivingAt: string|null (default null),   // transient "archive in progress"
  status: WorkspaceStateBucket, statusEnteredAt: string|null,
  activityAt: string|null,            // always null from the v0.10.1 daemon
  diffStat?: {additions, deletions}|null,
  scripts: WorkspaceScriptPayload[] (default []),
  gitRuntime?: { currentBranch?, remoteUrl?, isPaseoOwnedWorktree?, isDirty?, aheadBehind?: {ahead,behind}|null,
                 aheadOfOrigin?, behindOfOrigin? } | null,
  githubRuntime?: { featuresEnabled?, pullRequest?: { number?, url, title, state, baseRefName, headRefName, isMerged,
                    isDraft?, mergeable?, checks?: [{name,status,url,workflow?,duration?,traits?}],
                    checksStatus?: "none"|"pending"|"success"|"failure",
                    reviewDecision?: "approved"|"changes_requested"|"pending"|null, repoOwner?, repoName? } | null,
                    error?: {message}|null, refreshedAt? } | null,
  forge?: string /* absent ⇒ "github" */, project?: ProjectPlacementPayload, syncSeq?: int }
```

Notes on fields the descriptor does **not** have:
- **No archived flag.** Archived workspaces never appear on the wire; they arrive as a `remove`.
- **No `createdAt` and no branch field.** The branch is `gitRuntime.currentBranch`.

`WorkspaceProjectDescriptorPayload` = `{projectId, projectKey?, projectDisplayName, projectCustomName?, projectCustomIconRevision?, projectIconRevision?, projectRootPath, projectKind, syncSeq?}`.

`WorkspaceScriptPayload` = `{scriptName, type:"script"|"service", hostname, port|null, localProxyUrl?, publicProxyUrl?, proxyUrl|null, lifecycle:"running"|"stopped", health:"healthy"|"unhealthy"|null, exitCode|null, terminalId|null}`.

**Labels:** `WorkspaceLabelDefinition = {name, color}`.
- **There is no label id.** Identity is the lower-cased, whitespace-collapsed name.
- The catalog is daemon-global. Workspaces store names, and the colour comes from that workspace's host catalog.

### 2.3 Fetch / subscribe and update events

| Purpose | Request → response | Push event |
|---|---|---|
| Workspaces | `fetch_workspaces_request` → `fetch_workspaces_response {requestId, subscriptionId?, entries: WorkspaceDescriptor[], emptyProjects: ProjectDescriptor[], pageInfo:{nextCursor,prevCursor,hasMore}, sync?}` | `workspace_update` |
| Projects | `project.list.request` → `project.list.response {requestId, projects[], sync?}` (feature `projectList`; without it, projects are synthesised from workspaces) | `project.update` (via the session-events subscription) |
| Labels | `workspace.label.list.request {requestId, subscribe?, sync?:{generation,afterSeq}}` → `…response {requestId, subscriptionId?, labels[], sync:{mode,generation,headSeq,removals:[{name,seq}]}}` | `workspace.label.update` |
| Scripts | — | `script_status_update {subscriptionId?, workspaceId, scripts[]}` |

**Event payloads:**
```ts
workspace_update.payload =
  | { subscriptionId?, kind:"upsert", workspace: WorkspaceDescriptor, generation?, seq? }
  | { subscriptionId?, kind:"remove", id, emptyProject?: ProjectDescriptor /* keep showing empty project */,
      removedProjectId?: string, generation?, seq? }
"project.update".payload =
  | { subscriptionId?, kind:"upsert", project, generation?, seq? }
  | { subscriptionId?, kind:"remove", projectId, generation?, seq? }
"workspace.label.update".payload =
  | { subscriptionId?, kind:"upsert", label:{name,color}, previousName? /* rename */, generation, seq }
  | { subscriptionId?, kind:"remove", name, generation, seq }
```

**How the client applies them:**
- **Labels:** an update is applied only if `generation` matches and `seq === afterSeq + 1`; otherwise the client refetches.
- **Project upsert:** copies `projectDisplayName`, `projectCustomName`, `projectCustomIconRevision`, `projectRootPath` and `projectKind` into all of that project's workspaces.
- **Project remove:** deletes the project and all of its workspaces.

Example response and remove:
```json
{"type":"session","message":{"type":"fetch_workspaces_response","payload":{"requestId":"6f1c…","subscriptionId":"sub_1",
 "entries":[{"id":"wks_3fa9c0d1e2b4a5f6","projectId":"prj_0011aabbccddeeff","projectDisplayName":"paseo","projectCustomName":null,
   "projectCustomIconRevision":null,"projectRootPath":"/home/u/code/paseo","workspaceDirectory":"/home/u/.paseo/worktrees/paseo/brave-otter",
   "worktreeSlug":"brave-otter","projectKind":"git","workspaceKind":"worktree","name":"fix-login","title":null,
   "pinnedAt":"2026-09-30T12:00:00.000Z","labels":["Blocked"],"archivingAt":null,"status":"running",
   "statusEnteredAt":"2026-10-01T08:15:00.000Z","activityAt":null,"diffStat":{"additions":12,"deletions":3},"scripts":[],
   "gitRuntime":{"currentBranch":"fix-login","isPaseoOwnedWorktree":true,"isDirty":true,"aheadOfOrigin":2,"behindOfOrigin":0},
   "githubRuntime":{"featuresEnabled":true,"pullRequest":null,"error":null},"forge":"github","syncSeq":17}],
 "emptyProjects":[],"pageInfo":{"nextCursor":null,"prevCursor":null,"hasMore":false},
 "sync":{"generation":"b6e0…","headSeq":17,"mode":"snapshot","reason":"no_cursor","removals":[]}}}}

{"type":"session","message":{"type":"workspace_update","payload":{"subscriptionId":"sub_1","kind":"remove","id":"wks_3fa9c0d1e2b4a5f6",
 "emptyProject":{"projectId":"prj_0011aabbccddeeff","projectDisplayName":"paseo","projectRootPath":"/home/u/code/paseo","projectKind":"git"},
 "generation":"b6e0…","seq":18}}}
```

### 2.4 Mutations

All mutations are request/response. Success is followed by `workspace_update` and/or `project.update` pushes, which are what actually update the UI.

| Mutation | Request | Response payload | Gate / notes |
|---|---|---|---|
| Create workspace | `workspace.create.request {requestId, workspaceId?, agent?, subscribe?, idempotencyKey?, title?, firstAgentContext?:{prompt?,attachments?}, source}` (see below) | `{requestId, workspace\|null, agent?, creation?, setupTerminalId\|null, setupSkippedReason?, error\|null, errorCode?}` | Progress arrives as `workspace.create.update` (CreationSnapshot). Reconnect recovery: `creation.subscribe.request {kind,idempotencyKey}`. Feature `creationLifecycle` → `subscribe:true` |
| Open project (find-or-create workspace by dir) | `open_project_request {cwd}` | `open_project_response {workspace\|null, error, errorCode?}` | legacy, still used |
| Add project | `project.add.request {cwd}` | `{project\|null, error, errorCode?}` | `projectAdd` |
| Pin / unpin | `workspace.pin.set.request {workspaceId, pinned}` | `{requestId, workspaceId, accepted, pinnedAt\|null, error}` | `workspacePinning`. The client sends `pinned = pinnedAt == null` |
| Rename workspace | `workspace.title.set.request {workspaceId, title: string\|null}` | `{requestId, workspaceId, accepted, title, error}` | null or blank clears it back to the derived name |
| Rename project | `project.rename.request {projectId, customName\|null}` | `{requestId, projectId, accepted, customName, error}` | |
| Assign / unassign label (also the **only** way to create one) | `workspace.label.assignment.set.request {workspaceId, label:{name,color}, assigned}` | `{requestId, label, workspaceLabels: string[]}` | `workspaceLabels`. If the label already exists, its existing colour wins |
| Edit label | `workspace.label.update.request {name, newName?, color?}` | `{requestId, label, affectedWorkspaceCount}` | errors `label_not_found`, `label_name_taken` |
| Delete label | `workspace.label.delete.request {name}` (preflight `workspace.label.delete.inspect.request`) | `{requestId, affectedWorkspaceCount}` | |
| Archive workspace | `archive_workspace_request {workspaceId}` | `{requestId, workspaceId, archivedAt\|null, error}` | Daemon emits an upsert with `archivingAt` set, tears down agents and terminals, removes Paseo-owned worktree dirs, then emits `remove` (+`emptyProject`) |
| Unarchive ("recovery") | `workspace.recovery.inspect.request {workspaceId}` → `{state:{kind:"recoverable",workspaceName,action,branch}\|{kind:"unavailable",reason,message}}`; `workspace.recovery.restore.request {workspaceId}` | `{requestId, workspaceId, accepted, error}` | `workspaceRecovery`; 150 s client timeout |
| Mark read | `workspace.clear_attention.request {workspaceId: string\|string[]}` | `{requestId, workspaceId, clearedAgentIds, results[], success, error}` | menu "Mark as read" when status is `attention` or `failed` |
| Mark unread | `workspace.mark_unread.request {workspaceId}` | `{requestId, workspaceId, markedAgentId\|null, success, error}` | `workspaceMarkUnread`; offered when status is `done` |
| Remove project | `project.remove.request {projectId}` | `{requestId, projectId, accepted, removedWorkspaceIds[], error}` | `projectRemove`. Archives all of the project's workspaces; files on disk are untouched |
| Project icon | `project.icon.set.request {projectId, source:{type:"automatic"}\|{type:"upload",data}}`, `project.icon.get.request {projectId}` → `{icon:{data,mimeType}\|null}` | | `projectCustomIcon`. Cache key `[serverId, projectId, projectIconRevision]` |
| Reorder | **none.** Order is client-local (§3.9) | | |

**Create-workspace `source`:**
```ts
| { kind:"directory", path, projectId? }
| { kind:"worktree", cwd?, projectId?, action?: "branch-off"|"checkout", refName?, baseBranch?, branchName?,
    checkoutSource?: { kind:"change_request", forge?, number, projectPath? }, githubPrNumber?, worktreeSlug? }
```

Example (what the new-workspace screen sends for a worktree with a first prompt):
```json
{"type":"workspace.create.request","requestId":"r1","idempotencyKey":"draft_abc","subscribe":true,
 "source":{"kind":"worktree","cwd":"/home/u/code/paseo","projectId":"prj_0011aabbccddeeff","worktreeSlug":"brave-otter","action":"branch-off","refName":"refs/remotes/origin/main"},
 "firstAgentContext":{"prompt":"Fix the login bug"},
 "agent":{"config":{"provider":"claude","cwd":"/home/u/code/paseo"},"initialPrompt":"Fix the login bug","clientMessageId":"draft_abc:initial-message","labels":{}}}
```
`CreationSnapshot.phase` ∈ `accepted | workspace_ready | agent_ready | prompt_started | completed | failed`.

### 2.5 Workspace status derivation

**Daemon (authoritative `status`).** Each source maps to a bucket; the highest-priority bucket across all sources wins. Priority: `needs_input > failed > running > attention > done`.
- **Per agent** (`protocol/src/agent-state-bucket.ts`), first match wins:
  1. pendingPermissions > 0 or attentionReason `permission` → `needs_input`
  2. status `error` or attentionReason `error` → `failed`
  3. status `running` → `running`
  4. `requiresAttention` → `attention`
  5. else `done`
- **Which agents count:**
  - Only non-archived agents of the workspace.
  - Root agents contribute their full bucket; same-workspace descendants only contribute `running`.
- **Terminals:** activity `needs_input` → `needs_input`, `finished` → `attention`, `working` → `running`.
- **Optimistic create:** a create with a first agent emits `running` straight away.

**App ("effective" status, `hooks/sidebar-workspaces-view-model.ts`).** Use the descriptor status if it is not `done`. Otherwise:
- a pending create with an initial agent → `running`
- otherwise the newest root agent's own bucket, where `turn.phase === "open"` counts as running

**Vocabulary mapping:**

| UI term | Bucket |
|---|---|
| running | `running` |
| needs attention | `needs_input` ("Needs input") and `failed` ("Failed") |
| unread / finished-not-seen | `attention` ("Ready to review") |
| idle | `done` ("Done") |

Status-group labels are hard-coded English in `hooks/sidebar-status-view-model.ts`.

**Collapsed-project aggregate order:** `needs_input, failed, running, attention, done`. Status-group display order: `needs_input, failed, attention, running, done`.

---

## 3. The left sidebar (desktop)

Main files:
- `app/src/components/left-sidebar.tsx`
- `app/src/components/sidebar-workspace-list.tsx`
- `app/src/components/sidebar/**`
- `app/src/sidebar-nav/model.ts`

### 3.1 Container

| Property | Value |
|---|---|
| Width | default **320**, min **200**, max **600**, clamped to `max(200, min(600, viewport − 400))` (`stores/panel-store/state.ts`, `components/desktop-sidebar-layout.ts`) |
| Persistence | `sidebarWidth` and `desktop.agentListOpen` (default open) in zustand key `panel-state` (v16) |
| Style | background `surfaceSidebar`, 1px right border `border` |
| Collapsed | no icon rail: `display:none` (stays mounted). Toggle is instant (Cmd/Ctrl+B) |
| Resize handle | 10px strip straddling the right edge (`right:-5`), `col-resize`. After 100 ms hover it shows a 1px line in `foreground` @0.25. Drag activates after ±6px x and fails after ±12px y |
| Where the sidebar is shown | `/new`, `/open-project`, `/sessions`, `/schedules` and `/h/<knownServerId>/…`. Not on `/settings/*` (own list/detail layout), `/welcome`, `/pair-scan` or `/` |
| macOS | traffic lights at (16,10), reserved area 78×45. When the sidebar owns the top-left corner, it renders a 36px chrome row (drag region). The window-level sidebar toggle (`PanelLeft` 16, stroke 1.5) sits at `left = 78+12` |
| Windows/Linux | window controls are top-right. The toggle lives in the content header instead |

### 3.2 Render order, top to bottom

```
[chrome row, 36px, drag region — only when sidebar owns mac top-left corner, or dev build label]
NAV ROWS group          (paddingTop 8 / 0 below chrome, gap 2, paddingBottom 6, 1px bottom border)
  + New workspace       Plus           → /new[?serverId&dir&projectId]  (hover shows Cmd/Ctrl+N)
  ⟲ History             History        → /sessions   (active when path includes /sessions)
  🔍 Search             Search         → opens command center (hover shows Cmd/Ctrl+K)
  🗓 Schedules          CalendarClock  → /schedules  (active when path includes /schedules)
  [plugin sidebar items: key plugin:<pluginId>:<contributionId> → /h/<s>/plugin/<pid>/sidebar/<cid>]
SCROLL LIST (paddingH 8, top 2, bottom 16)
  [Pinned section]      only if any pinned workspaces
  "Workspaces" header   + Display-preferences button (Settings2)
  Project blocks → workspace rows   (or empty state / filter-empty state)
[Callout slot]          update / Rosetta / "Set up worktree scripts" — at most one
FOOTER (row, gap 8, paddingH 8, paddingV 12, 1px top border)
  [FolderPlus "Add project"  (flex 1)]  [Server: hosts]  [Import: import session]  [CircleHelp: help]  [Settings]
```

There is **no** logo or host switcher at the top. In the Electron UI, hosts surface in three places only: the footer `Server` button (host picker → host settings), the Display menu's Host filter, and the per-row host badge.

**Nav rows** (`components/sidebar/sidebar-nav-rows.tsx`, `sidebar-header-row.tsx`, variant `"compact"`):
- **Row look:**
  - row, gap 8, minHeight 28, paddingV 4, paddingH 8, radius 8
  - background `surfaceSidebarHover` when hovered **or** active
  - icon 14 and label base 14 / normal, both `foregroundMuted` → `foreground` when highlighted
  - shortcut chord right-aligned, **only while hovered**
- **Labels** (i18n keys): `sidebar.actions.newWorkspace` "New workspace", `sidebar.sections.sessions` "History", `sidebar.sections.search` "Search", `sidebar.sections.schedules` "Schedules".
- **New workspace target:** `/new?serverId=<active>&dir=<projectRoot>&projectId=…`, using the active workspace's host/project when that host supports `workspaceMultiplicity` or the project can make worktrees. Otherwise `/new?serverId=` or bare `/new`.
- **Configurable:**
  - setting `sidebarNavItems: {key, visible}[]` in `@paseo:app-settings`; array order is display order
  - empty means the default order, all visible
  - edited in Settings → Sidebar (up/down arrows + visibility switch)
  - if all items are hidden, the whole group (including its border) is omitted

### 3.3 Grouping modes and data

`groupMode` is `"project" | "status"` (store `sidebar-view`, v6, default `"project"`).

**Project mode:**
1. **Pinned section** (if any workspace has `pinnedAt` and the host has `workspacePinning`):
   - header text "Pinned": sm 12, `foregroundMuted`, minHeight 36, paddingH 8. A 12px chevron shows on hover; click collapses.
   - rows are flush (not indented) and use the project icon as the leading visual
   - order: newest pin first, then the stored `pinnedWorkspaceOrder`
2. **"Workspaces" header:**
   - literal (not i18n), sm 12, `foregroundMuted`, paddingLeft 8 / right 4 / vertical 4
   - right side: the `Settings2` 14 display-preferences trigger (28×28, radius 6), tooltip "Display preferences"
   - shown if there are unpinned projects or any filter is active
3. **Project blocks**, each followed by its workspaces.

**Projects:**
- Merged across hosts by `projectKey`. The view key is the projectKey, or `JSON.stringify([serverId, projectId])`.
- Name is `projectCustomName ?? projectDisplayName ?? last path segment`.
- Default sort: numeric, case-insensitive name. The user's drag order is applied on top. New projects are **appended**; new workspaces are **prepended**.
- A project with no unpinned workspaces still shows, with a ghost "New workspace" row.

**Status mode:**
- Pinned section, then the "Workspaces" header, then groups in the order needs_input, failed, attention, running, done (empty groups skipped).
- Group header: icon slot 16 with `CircleAlert` / `CircleX` / `CircleCheck` / `CircleDot` in their dot colours (done: `CircleCheck` muted), then label "Needs input" / "Failed" / "Ready to review" / "Working" / "Done".
- Rows are indented 16, sorted by `statusEnteredAt` (newest first), and not draggable.

**Filters** (in `sidebar-view`):
- host allowlist
- project allowlist
- label any-of (`""` = "Unlabelled")

**Row limit:** 20 per group, then a "Show more" / "Show less" row (`ChevronDown`/`ChevronUp` 14).

**Archived workspaces are never listed.** While `archivingAt != null`, the row is disabled and shows the running ring.

**Agents never appear as sidebar rows.** Agents are reached through workspace tabs, History, or the command center.

**Loading skeleton:** shown before the first hydration. Three blocks at opacity 1 / 0.7 / 0.4, `surface2` bars pulsing 0.4↔0.8 at 1 s each way.

**Empty state card:**
- style: `surface0`, radius 8, margin 8, padding 24/16
- no projects: "No projects yet" / "Add a project to get started", with ghost buttons "Add project" and "Import session"
- filters left nothing: "No workspaces match" / "Clear filters"

### 3.4 Project row

| Property | Value |
|---|---|
| Geometry | minHeight 36, padding 8/8, radius 8, marginBottom 4, gap 8; block paddingBottom 12 when it has children |
| Background | hover `surfaceSidebarHover`; pressed/drag `surface2`. Dragging adds a 1px border, scale 1.02 and `shadow.md`. Never "selected" |
| Leading slot (16×20) | project icon 16×16: the uploaded icon, or a fallback square with radius 4, identity-colour fill and a white uppercase initial at 9px. On hover it is replaced by a chevron (`ChevronDown`/`ChevronRight` 14, hard-coded `#9ca3af`) |
| Collapsed badge | 12px corner badge at (−4,−4) with the aggregate bucket: needs_input = `CircleAlert` 12; failed/attention = 6px dot; running = `StatusRing`; done = none |
| Title | name; base 14, weight 400, `foregroundMuted`, one line. **No counts** |
| Hover actions (desktop: hidden until hover) | `+` (24×24, radius 6, `Plus` 15, tooltip "New workspace") → `/new?serverId&dir&name&projectId`; kebab `MoreVertical` 14 → "Project actions" menu (width 220) |
| Click | toggles collapse (`sidebar-collapsed-sections`: `collapsedProjectKeys`, `collapsedWorkspaceGroupKeys`, `collapsedPinned`) |

**Project menu** (kebab or right-click):
1. `Settings` "Open project settings" → `/settings/hosts/<s>/projects/<p>`
2. `ExternalLink` "Open in new window" (Electron, local daemon)
3. `FolderOpen` "Open in file manager" (Electron)
4. `Trash2` "Remove project", which confirms with `Remove "{{projectName}}" from the sidebar?\n\nFiles on disk will not be changed.`

### 3.5 Workspace row (`components/sidebar/sidebar-workspace-row-content.tsx`)

**Geometry:**
- minHeight 36, marginBottom 2, paddingV 8, paddingLeft 8, paddingRight 12, radius 8, column, gap 4
- project-mode rows are **not** indented, so the status slot aligns under the project icon
- status-mode rows: paddingLeft 16

**Background:** pressed/drag `surface2` > selected `surfaceSidebarSelected` > hover `surfaceSidebarHover`. In project mode, the selected state only applies while the route is a workspace route.

**Line 1:** `[status slot 16×20] gap 8 [title (flex 1)] [trailing slot]`.
- **Title:**
  - text: `workspace.name`, or `currentBranch ?? name` when the setting `workspaceTitleSource` = `"branch"` (default `"title"`)
  - style: base 14, weight 400, lineHeight 20, `foreground` at **opacity 0.76** (1.0 on hover)
- **Trailing slot** (setting `sidebarWorkspaceTrailing`, default `"diff"`):
  - `diff`: `+N` in `statusSuccess` and `−M` in `statusDanger`, sm 12, gap 4
  - `timestamp`: time-ago since `statusEnteredAt` in `foregroundExtraMuted`
  - `none`
- **Kebab:** on hover, `MoreVertical` 14 overlays the trailing slot over a 48px gradient scrim in the row's hover colour.

**Line 2, the meta row:**
- layout: gap 6, items separated by "·" (sm, `foregroundExtraMuted`, lineHeight 16), 12px glyphs
- omitted entirely when empty
- items are toggled by setting `sidebarRowItems`; defaults: branch off, project off, host on, changeRequest on, services on, labels on

Meta items, in order:
1. `GitBranch` + branch (muted)
2. `Folder` + project name (pinned and status rows only)
3. Host badge (§1.3)
4. PR: `GitPullRequest` (`statusSuccess`) / `GitMerge` (`statusMerged`, " Merged") / `GitPullRequestClosed` (`statusDanger`, " Closed") + `#number`. Clicking opens the URL
5. CI checks: a circle-check, cross or pie glyph plus "passed" / "failed" / "running" in `statusSuccess` / `statusDanger` / `statusWarning` (setting `sidebarChecksDisplay` = `iconAndText` | `icon` | `none`)
6. Service: `Globe` + the first running service script name (`statusSuccess`, or `statusDanger` when unhealthy)
7. Label chips: paddingH 6, radius full, background = identity fill + `1a` alpha, text sm in identity foreground

**Status slot (exact precedence)** for rows without a leading project icon:

| Condition | Renders |
|---|---|
| loading (archiving / creating) | `StatusRing` |
| `running` | `StatusRing`: 6px centre dot in `statusDotRunning`; ring track @0.3 plus a 90° head arc @0.9; diameter 12; one turn per **900 ms**, linear, phase-locked to wall-clock so all rings spin in sync |
| `needs_input` | `CircleAlert` 12, fill `statusDotWarning`, stroke `surface0` |
| `attention` (unread) | solid 6px dot, `statusDotSuccess` |
| `done` | 6px dot, `foregroundExtraMuted` @ opacity 0.3 (placeholder) |
| `failed` | workspace-kind icon 14 muted (`Monitor` local_checkout, `FolderGit2` worktree, `Folder` other) + an 8px `statusDotDanger` dot with a 1px `surface0` ring at the bottom-right |

Pinned and status-mode rows use the project icon as the leading visual, with a corner badge for the row's own bucket.

**Shortcut badges:** hold Cmd (mac) or Ctrl (other) for 150 ms and the first 9 visible rows (collapsed groups skipped) show a badge: 18×18, radius 2, 1px `surface2` border, `surface0` fill, sm/500 number, at top 1 / right 0.

**Hover card** (desktop only): 260 wide, `surface1`, 1px `borderAccent`, radius 8, placed 4px to the right of the row.
- Contents: name, PR, diff stat, host, branch (click copies), path (click copies), checks.

**Workspace menu** (kebab: width 260, "Workspace actions"; right-click: same items):
1. `Copy` "Copy path"
2. `Copy` "Copy branch name" (git only)
3. `Pencil` "Rename workspace" (modal → `workspace.title.set.request`)
4. `CircleCheck` "Mark as read" (attention/failed)
5. `Circle` "Mark as unread" (done + `workspaceMarkUnread`)
6. `Pin` "Pin to top" / `PinOff` "Unpin" (⌘⇧P)
7. `Tag` "Labels" (sub-page: toggle labels, then "Create label")
8. `FolderOpen` "Open in file manager" (Electron)
9. `Archive` "Archive"
   - ⌘⇧⌫ is shown when the row is selected.
   - Worktrees with uncommitted or unpushed work get a confirm: `Archive "{{workspaceName}}"?`.
   - Archiving the active workspace redirects to `/new?…` for its project.

### 3.6 Footer

| Element | Spec |
|---|---|
| Add project | flex 1, minHeight 32, padding 6/8, gap 8, radius 8, hover `surfaceSidebarHover`. `FolderPlus` 14 + "Add project" (base, muted→foreground). Tooltip shows Cmd/Ctrl+O. Opens the add-project flow |
| Icon buttons | 28×28, muted→foreground on hover, tooltip above |
| Hosts | `Server` 14 → host picker (§1.3). "Add host" → `/settings/general?addHost=<timestamp>` |
| Import session | `Import` 16 → import-session sheet |
| Help | `CircleHelp` 16 → menu: "Keyboard shortcuts", "What's new", "Run diagnostics", "Discord", "Create GitHub issue", then the version line and one line per host version |
| Settings | `Settings` 16 → `/settings` (desktop redirects to `/settings/general`); tooltip Cmd/Ctrl+, |

### 3.7 Callout slot
Sits between the list and the footer and shows the single highest-priority callout.
- Card: paddingV 12, paddingH 16, 1px top border.
- Content: title (base) + description (sm muted), with up to 2 buttons and an optional `X`.
- Dismissals persist in `@paseo:sidebar-callout-dismissals`.

### 3.8 Display-preferences menu (`components/sidebar/display-preferences/menu.tsx`)
Width 232, titled "Display". Contents:
- **Grouping:** Project | Status
- **Title:** Title | Branch name
- **Show:**
  - toggles: Branch, Project, Host, Pull request, Services, Labels
  - Checks sub-page: Icon and text | Icon only | Hidden
  - single-choice: Diff stats | Last activity
- **Host filter** (only with more than 1 host): "All hosts", then hosts with status dots
- **Project filter** (only with more than 1 project)
- **Labels filter:** label dots, "Unlabelled", "Clear filter", "Manage labels…"

### 3.9 Drag-to-reorder (client-local)
- **Gesture:** dnd-kit, vertical only, activates after 6px of movement. The whole row is the handle.
- **Store:** `sidebar-project-workspace-order` (v1) with `projectOrder`, `workspaceOrderByProject[viewKey]` and `pinnedWorkspaceOrder`. Keys that are currently hidden are preserved on merge.
- **No daemon RPC.**

---

## 4. Routes and selection

Expo-router file routes live in `app/src/app/**`. Builders are in `app/src/utils/host-routes.ts`.

| Route | Screen / behaviour |
|---|---|
| `/` | Splash, then redirect to the last workspace's host, else the first online host `/h/<id>`, else the first saved host, else `/welcome` |
| `/welcome` | Welcome (no sidebar) |
| `/new?serverId&dir&name&projectId&draftId` | New-workspace screen |
| `/open-project` | Open project (home) |
| `/sessions` | History (agent sessions) |
| `/schedules` | Schedules |
| `/pair-scan` | QR pairing |
| `/settings` → `/settings/:section` | Sections: general, appearance, sidebar, chat, terminal, browser, editor, shortcuts, integrations, notifications, permissions, diagnostics, about. `?addHost=` opens add-host |
| `/settings/hosts/:serverId/:hostSection` | Sections: projects, connections (default), pair-device, agents, metadata, workspaces, providers, usage, terminals, plugins, host |
| `/settings/hosts/:serverId/projects/:projectId` | Project settings |
| `/h/:serverId` | Host stack. Index redirects to the last workspace on this host, else `/open-project` |
| `/h/:serverId/workspace/:workspaceId[?open=<intent>]` | **Workspace screen.** A non-URL-safe id is encoded `b64_<base64url>`. `open` intents: `agent:<id>`, `terminal:<id>`, `file:<b64>`, `draft:<id>`, `setup:<id>` |
| `/h/:serverId/agent/:agentId` | Resolves the agent's workspace, then goes to `…/workspace/<w>?open=agent:<id>` |
| `/h/:serverId/plugin/:pluginId/:kind/:contributionId` | Plugin surface (sidebar items use `kind=sidebar`) |

**Selection:**
- The active workspace is parsed from `/h/:serverId/workspace/:workspaceId` (`stores/navigation-active-workspace-store`).
- It is persisted to `paseo:last-workspace-route-selection`.

**Clicking a workspace row** calls `navigateToWorkspace({serverId, workspaceId})`:
- It reveals the tab of the agent most needing attention (permission > error > finished, oldest first).
- It navigates to the workspace route. Workspace screens are kept mounted in a retained deck, so switching back is instant.

**Workspace navigation shortcuts:** Cmd/Ctrl+1…9 and Cmd/Ctrl+[ / ] use the sidebar's published visual order (`sidebarShortcutWorkspaceTargets`, the first 9 rows, skipping collapsed groups).

---

## 5. Workspace tabs, panes and the Explorer

Main files:
- `app/src/workspace-tabs/**`
- `app/src/panels/**`
- `app/src/screens/workspace/**`
- `app/src/stores/workspace-layout-*.ts`
- `app/src/components/split-container*.tsx`

**Key fact:** tab and layout state is **client-local only**. `messages.ts` has no layout messages, and the daemon never sees tabs. Closing tabs does have daemon side effects (§5.3).

### 5.1 Tab targets

```ts
interface WorkspaceTab { tabId: string; target: WorkspaceTabTarget; createdAt: number; state?: JsonValue }
type WorkspaceTabTarget =
  | { kind:"new_tab" }                                     // launcher placeholder
  | { kind:"draft"; draftId; setup?: {provider,cwd,modeId,model,thinkingOptionId,featureValues} }
  | { kind:"agent"; agentId }
  | { kind:"provider_subagent"; parentAgentId; subagentId }
  | { kind:"terminal"; terminalId }
  | { kind:"browser"; browserId }                         // Electron only, main pane only
  | { kind:"changes_tree" } | { kind:"files" }            // Explorer only
  | { kind:"pull_request" }
  | { kind:"file"; path; lineStart?; lineEnd? }
  | { kind:"working_diff"; focusPath?; focusRequestId? }  // "Diff"
  | { kind:"plugin"; pluginId; panelId; context:"workspace" }
  | { kind:"plugin"; pluginId; panelId; context:"agent"; agentId }
  | { kind:"setup"; workspaceId }                         // main pane only
  | { kind:"commit_diff"; sha }                           // ephemeral, never persisted
```

**Tab ids** (`workspace-tabs/identity.ts`):

| Target | tabId |
|---|---|
| agent | `agent_<agentId>` |
| terminal | `terminal_<id>` |
| browser | `browser_<id>` |
| file | `file_<path>` (line numbers are not part of the id) |
| draft | `<draftId>` |
| setup | `setup_<workspaceId>` |
| commit_diff | `commit_diff_<sha>` |
| working_diff | `working_diff` |
| changes_tree / files / pull_request | the kind string itself |
| provider_subagent | `provider_subagent_<len>_<parent>_<len>_<sub>` |
| plugin | `plugin_workspace_…` or `plugin_agent_…` (length-prefixed parts) |
| new_tab, and any open with intent `"new"` | `tab_<uuid>` |

**De-duplication key:** `${kind}:${resourceKey}`. Reveal and background opens retarget an existing tab in place instead of opening a duplicate.

**Tab labels and icons** (from each panel's `useDescriptor`):

| Kind | Label | Icon / status |
|---|---|---|
| agent | agent title (skeleton while loading); subtitle "`<Provider>` agent" | provider icon; status bucket from the agent |
| draft | "New Agent" | `SquarePen` |
| terminal | `title ?? name ?? "Terminal"` | `Terminal`; status from terminal activity |
| file | basename | material file icon; dirty dot when modified |
| changes_tree / working_diff / files | "Changes" / "Diff" / "Files" | |
| pull_request | "Pull request", or the PR number once known | |
| commit_diff | short sha | |
| browser | page title / hostname | favicon |
| setup | | check when completed; alert when failed or blocked |

### 5.2 Opening

One API: `openTab({workspaceKey, target, intent: "new"|"reveal"|"background", pin?, placement?, insertionPosition?, parentTabId?, state?})`.

**Intents:**
- `"new"` always creates a new instance and focuses it.
- `"reveal"` finds an existing tab by resource key and focuses it, or creates one.
- `"background"` does the same lookup but never steals focus.

**Placement:**
- `{mode:"pane", paneId}`: open there; an existing tab is moved there.
- `{mode:"prefer", paneId}`: new tabs open there; an existing tab stays put.
- `{mode:"focused"}`: the focused pane.
- `{mode:"ambient"}`: the default.

**Insertion:**
- A new tab is appended, or inserted after `afterTabId`.
- A pane holding only a `new_tab` gets that tab replaced.

**Auto-reconciliation** (runs on every relevant change):
- Every non-archived **root agent** of the workspace and every standalone terminal gets a background tab automatically.
- Tabs whose agent was archived, or whose terminal no longer exists, are closed. Archiving on one client therefore closes the tab on every client.

**Other open sources:**
- **"+" / Cmd+T menu.** Primary order:
  1. Agent (draft)
  2. Terminal
  3. Changes (git only)
  4. Diff (git only)
  5. Files
  6. Browser (Electron only)
  7. Pull request (when a PR exists)

  Items the pane can't host are dropped. Plugin panels follow, then terminal profiles with "Edit profiles".
- **Draft becomes agent:** when a draft is submitted, its tab is retargeted to `{kind:"agent"}` and keeps the same tabId.
- **File links from chat:** reveal in the focused pane, with `parentTabId` set.
- **Open beside:** finds or creates the side pane. The first time it wraps the root in a horizontal group `[root, newPane]` with sizes `[0.7, 0.3]`.
- **Preview behaviour:** when opening from the Explorer tree, an unmodified active `file` (or `working_diff`) tab in the destination pane is replaced instead of a new tab being opened.

### 5.3 Closing (and daemon side effects)

Triggers: the tab's X, middle-click, context-menu "Close", or Cmd/Ctrl+W.

| Tab kind | Effect |
|---|---|
| modified file | "Unsaved changes" confirm first |
| terminal | confirm "Close terminal?" → `kill_terminal_request {terminalId}` → `kill_terminal_response {terminalId, success}` |
| root agent | if running, confirm "Archive running agent?" → `archive_agent_request {agentId}`. **Closing a root agent tab archives the agent** |
| subagent (has a parent) | `update_agent_request {agentId, labels:{"paseo.open-agent-tab.<clientId>":"false"}}`. Hide only, no archive |
| browser | local and IPC only |
| everything else | layout-only |

**Bulk closes:**
- Actions: Close to the left, Close to the right, Close other tabs, Close pane.
- One confirm is shown for the batch. Agents to archive and terminals to kill go in `close_items_request {agentIds[], terminalIds[]}` → `close_items_response {agents:[{agentId,archivedAt}], terminals:[{terminalId,success}]}`.

**When a pane empties:**
- The `main` pane, the Explorer, and the last visible ordinary pane get a fresh `new_tab`.
- Any other pane is removed, and a group left with one child collapses.

**Next focused tab:** the recorded parent tab, else the tab to the right, else the tab to the left.

### 5.4 Reorder, move, rename
- **Reorder:** drag within a tab row (dnd-kit, 8px activation) → `reorderTabsInPane(paneId, tabIds)`.
- **Move:**
  - by dropping on another pane's tab row or its centre zone
  - with Cmd+Alt+Shift+Arrow (mac)
  - with Explorer "Move to main panel"
- **Rename** (agent and terminal tabs only; modal, 200-char max):
  - terminal: `terminal.rename.request {terminalId, title}`
  - agent: `update_agent_request {agentId, name}`
- **No tab pinning** in the UI.
- **Other context-menu items:**
  - Copy resume command
  - Copy agent id / Copy terminal id
  - Copy file path
  - Reload agent (`refresh_agent_request`)

### 5.5 Split panes
```ts
type SplitNode = { kind:"pane"; pane: SplitPane } | { kind:"group"; group: SplitGroup };
interface SplitPane  { id; tabIds: string[]; focusedTabId: string|null; hidden?: true; tabs: WorkspaceTab[] }
interface SplitGroup { id; direction: "horizontal"|"vertical"; children: SplitNode[]; sizes: number[] /* fractions */ }
interface WorkspaceLayout { root: SplitNode; focusedPaneId: string|null; parentTabIdByTabId?: Record<string,string> }
```

**Default tree:**
- group `workspace-root` (horizontal, `[0.78,0.22]`) containing:
  - pane `main` with one `new_tab`
  - pane `explorer`, `hidden:true`, with tabs `files` and `changes_tree`
- New ids are `pane_<uuid>` and `group_<uuid>`.

**Splitting:**
- left/right produce a horizontal group; top/bottom produce a vertical group.
- If the parent group already runs in that direction, the new pane is inserted beside the target and the target's size is halved. Otherwise the target is wrapped in a new group with sizes `[0.5,0.5]`.
- `MAX_TREE_DEPTH = 5`. Each child keeps at least `MIN_SPLIT_SIZE = 0.1`.
- User-dragged sizes are stored separately in `splitSizesByWorkspace[wk][groupId]` and override the tree's sizes.

**Drop zones** (when dragging a tab over a pane):
- the centre 40% moves the tab into that pane
- the outer 15% edges split the pane
- otherwise the nearest edge wins
- over a tab row, a 4px accent bar marks the insertion point

**Other pane features:**
- **Maximize:** a transient per-pane toggle.
- **Focus mode** (Cmd/Ctrl+Shift+F): renders only the focused pane.
- **Mounting:** each pane keeps at most 3 tab panels mounted (LRU), always retaining modified ones.
- **Pane toolbar:** "+", Maximize/Restore, and a "…" menu with "Split pane right" / "Split pane down".

### 5.6 Persistence (client-local)

| Item | Value |
|---|---|
| Key | `workspace-layout-state` (zustand persist, `version: 2`) |
| Per-workspace key | `workspaceKey = "<serverId>:<workspaceId>"` |
| Persisted | `layoutByWorkspace` (ephemeral `new_tab`/`commit_diff` tabs stripped), `pinnedAgentIdsByWorkspace`, `splitSizesByWorkspace`, `explorerSidebarWidthByWorkspace` (px), `explorerPaneIdByWorkspace`, `sidePaneIdByWorkspace`, `pullRequestTabAutoOpenedByWorkspace` |
| Not persisted | hidden agents, maximize, focus restoration |
| Purge | the workspace's layout is purged when it is archived |

Example:
```json
{"version":2,"state":{"layoutByWorkspace":{"srvA:wks_3fa9c0d1e2b4a5f6":{
  "root":{"kind":"group","group":{"id":"workspace-root","direction":"horizontal","sizes":[0.78,0.22],"children":[
    {"kind":"pane","pane":{"id":"main","tabIds":["agent_a1","terminal_t1"],"focusedTabId":"agent_a1",
      "tabs":[{"tabId":"agent_a1","target":{"kind":"agent","agentId":"a1"},"createdAt":1759400000000},
              {"tabId":"terminal_t1","target":{"kind":"terminal","terminalId":"t1"},"createdAt":1759400001000}]}},
    {"kind":"pane","pane":{"id":"explorer","hidden":true,"tabIds":["files","changes_tree"],"focusedTabId":"changes_tree",
      "tabs":[{"tabId":"files","target":{"kind":"files"},"createdAt":1759400000000},
              {"tabId":"changes_tree","target":{"kind":"changes_tree"},"createdAt":1759400000000}]}}]}},
  "focusedPaneId":"main"}},
 "splitSizesByWorkspace":{},"explorerSidebarWidthByWorkspace":{"srvA:wks_3fa9c0d1e2b4a5f6":360}}}
```

### 5.7 Tab row and Explorer visuals

| Item | Value |
|---|---|
| Tab row | height 36 (`WORKSPACE_SECONDARY_HEADER_HEIGHT`), `surface0`, 1px bottom border, row padding 4 |
| Tab chip | height 28, radius 6, paddingH 8, gap between chips 4, icon 14, content gap 4 |
| Tab width | natural label width clamped to [96, 160]. On overflow, tabs shrink toward 96; below that the row scrolls horizontally |
| Tab states | hover `surface1`; active in focused pane `surface2`; active in unfocused pane `surface1`. Label muted → foreground when active |
| Tab status overlay on icon | running → `StatusRing`; failed/attention → dot; needs_input → alert circle |
| Close button | 18×18 X over a 48px trailing scrim, shown on hover. Modified-dot 8px. Tooltip after 400 ms |
| Header (desktop, right side) | plugin buttons, scripts button, open-in-editor, workspace actions, Explorer toggle (`PanelRight`, Cmd/Ctrl+E) |
| Explorer dock | right side, outside the split tree. Width default 320, min 240, workspace body keeps ≥ 400 |
| Explorer tab rail | 36 tall, `surfaceSidebar`; tabs 26 tall, max 180, radius 6, no close button; hover/active `interactionHighlight` |
| Explorer default tabs | `files`, `changes_tree` (and `pull_request` when a PR is detected; setting `pullRequestOpenLocation` default `"explorer"`) |

### 5.8 Terminal protocol (summary; all requests carry `requestId`)

| Purpose | Request | Response / event |
|---|---|---|
| List | `list_terminals_request {cwd?, workspaceId?}` | `list_terminals_response {terminals:[{id,name,cwd?,workspaceId?,title?,activity?}]}` |
| Live list | `subscribe_terminals_request {cwd, workspaceId?}` | `terminals_changed {subscriptionId?, workspaceId?, cwd, terminals[]}` |
| Create | `create_terminal_request {cwd, workspaceId?, name?, agentId?, command?, args?, size?}` | `create_terminal_response {terminal\|null, error}` |
| Attach | `subscribe_terminal_request {terminalId, restore?:{mode:"live"\|"visible-snapshot"\|"full-snapshot", scrollbackLines?, size?}}` | `subscribe_terminal_response {terminalId, slot, subscriptionId?, error}` |
| Input / resize | `terminal_input {terminalId, message:{type:"input",data}\|{type:"resize",rows,cols,intent?:"claim"\|"update"}\|{type:"mouse",…}}` | — |
| Detach | `subscription.release.request` | |
| Kill | `kill_terminal_request` | |
| Rename | `terminal.rename.request` | |

---

## 6. Keyboard shortcuts and command center

Main files:
- `app/src/keyboard/keyboard-shortcuts.ts` (the single binding table)
- `app/src/command-center/**`
- `desktop/src/features/menu.ts`

### 6.1 Default bindings

**Guard abbreviations:**
- "CC✗": inactive while the command center is open.
- "Term✗": inactive when focus is in a terminal.
- "Edit✗": inactive in text inputs.

**Platform and modifiers:**
- Mac uses Cmd; Windows/Linux use Ctrl. Non-mac bindings are almost all Term✗, so Ctrl-keys reach the shell.
- A native desktop client should behave like Electron (`isDesktop = true`).
- The "web" column applies to the browser build only.

| Action (help id) | Label | Mac | Win/Linux | Notes |
|---|---|---|---|---|
| command-center.toggle (`toggle-command-center`) | Toggle command center | ⌘K | Ctrl+K | works while open (closes it) |
| command-center.files (`search-files`) | Search files | ⌘P | Ctrl+P | on a workspace route opens the command center in "files" scope; otherwise `workspace.project.pick` |
| shortcuts.dialog.toggle (`show-shortcuts`) | Show keyboard shortcuts | ? (Shift+/) | ? | only when focus is not in a text field or terminal |
| settings.toggle (`toggle-settings`) | Toggle settings | ⌘, | Ctrl+, | on /settings returns to the last workspace |
| theme.cycle (`cycle-theme`) | Cycle theme | ⌘⌥T | Ctrl+Alt+T | |
| agent.new (`new-agent`) | Open project | ⌘O | Ctrl+O | add-project flow |
| workspace.new (`new-workspace`) | New workspace | ⌘N | Ctrl+N | |
| workspace.navigate.index (`workspace-jump-index`) | Jump to workspace | ⌘1–9 | Ctrl+1–9 | web: Alt+1–9. Index into sidebar order |
| workspace.navigate.relative (`workspace-prev`/`-next`) | Previous/Next workspace | ⌘[ / ⌘] | Ctrl+[ / Ctrl+] | web: Alt+[ / ]. Wraps |
| workspace.pin (`pin-workspace`) | "Pin chat" (raw label) | ⌘⇧P | Ctrl+Shift+P | |
| workspace.archive (`archive-workspace`) | Archive workspace | ⌘⇧⌫ | Ctrl+Shift+Backspace | |
| workspace.tab.menu.open (`workspace-tab-new`) | New tab | ⌘T | Ctrl+T | |
| workspace.tab.target.agent | New agent | ⌘⇧A | Ctrl+Shift+A | |
| workspace.terminal.new | New terminal | ⌘⇧T | Ctrl+Shift+T | |
| workspace.tab.target.browser | New browser | ⌘⇧B | Ctrl+Shift+B | |
| workspace.tab.target.changes | Changes | ⌘⇧G | Ctrl+Shift+G | git only |
| workspace.tab.target.files | Files | ⌘⇧E | Ctrl+Shift+E | |
| workspace.tab.close.current | Close current tab | ⌘W | Ctrl+W | web: Alt+Shift+W |
| workspace.tab.navigate.index (`workspace-tab-jump-index`) | Jump to tab | ⌘⌥1–9 | Alt+1–9 | focused pane; non-mac has **no** Term✗ guard |
| workspace.tab.navigate.relative | Previous/Next tab | ⌥⇧[ / ⌥⇧] | Alt+Shift+[ / ] | wraps within the focused pane |
| workspace.pane.split.right / .down | Split pane right/down | ⌘\ / ⌘⇧\ | — | mac only |
| workspace.pane.focus.{left,right,up,down} | Focus pane … | ⌘⇧←→↑↓ | — | mac only; Edit✗ |
| workspace.pane.move-tab.{…} | Move tab … | ⌘⌥⇧←→↑↓ | — | mac only |
| workspace.pane.close | Close pane | ⌘⇧W | — | mac only |
| sidebar.toggle.left (`toggle-left-sidebar`) | Toggle left sidebar | ⌘B | Ctrl+B | mac: works with the command center open |
| sidebar.toggle.right (`toggle-right-sidebar`) | Toggle Explorer sidebar | ⌘E | Ctrl+E | also hidden Ctrl+` on all platforms (not rebindable) |
| sidebar.toggle.both | Toggle both sidebars | ⌘. | Ctrl+. | |
| view.toggle.focus (`toggle-focus`) | Toggle focus mode | ⌘⇧F | Ctrl+Shift+F | |
| message-input focus | Focus message input | ⌘L | Ctrl+L | |
| message-input mode-cycle | Cycle agent mode | ⇧Tab | Shift+Tab | composer only |
| voice-toggle | Toggle voice mode | ⌘⇧D | Ctrl+Shift+D | |
| dictation-toggle | Start/stop dictation | ⌘D | Ctrl+D | |
| agent.interrupt | Interrupt agent | Esc | Esc | composer or `other` scope; does not preventDefault unless handled |
| voice-mute-toggle | Mute/unmute voice | Space | Space | `other` scope |

**Outside the registry:**
- Find: Cmd/Ctrl+F.
- Composer: Enter sends, Shift+Enter inserts a newline, Cmd/Ctrl+Enter queues while the agent runs.
- Terminal (non-mac): Ctrl+C copies when there is a selection; Ctrl+V pastes.

**Electron menu accelerators:**
- `CmdOrCtrl+Shift+N` New Window
- `CmdOrCtrl+=` / `-` / `0` zoom
- `CmdOrCtrl+R` / `Shift+R` reload (the embedded browser if focused, else the app)

**No default chords exist.** Chords (space-separated combos, 1500 ms timeout) are possible only through user overrides.

### 6.2 Syntax, overrides, dispatch

**Combo string:**
- `+`-joined tokens with exactly one key.
- Modifiers: `Cmd`, `Ctrl`, `Alt`, `Shift`, `Mod`.
- `Digit` is a wildcard for 1–9, including the numpad, and produces the `index` payload.
- Matching uses `event.key` (layout-aware), plus `event.code` when Alt is held (macOS Option rewrites the key).
- Bare modifier presses are ignored.

**Overrides** (`@paseo:keyboard-shortcut-overrides`): `Record<bindingId, string | null>`.
- `null` means deliberately unassigned; an absent key means the default.
- Example: `{"command-center-toggle-cmd-k-mac":"Cmd+J","workspace-pin-cmd-shift-p-mac":null}`.
- Keys are **binding ids** (for example `sidebar-toggle-left-mac-cmd-b`), not action ids.

**Display:**
- Mac shows ⌃⌥⇧⌘ + key with no separators.
- Others show `Ctrl+Shift+X`.

**Dispatch pipeline** (`hooks/use-keyboard-shortcuts.ts`, capture phase):
1. A top modal overlay's key handler runs first; the command center uses this.
2. Skip while IME is composing or while a shortcut is being captured.
3. Resolve the focus scope: `terminal`, `command-center`, `message-input`, `editable`, `other` (or `browser` for keys from a webview).
4. The **first** matching binding in table order wins.
5. Route it to an action handler. Handlers are sorted by priority (higher first: new-tab panel 250, tabs row / focused composer 200, workspace screen 100, global 0), then by most recent registration. `preventDefault` is applied only if a handler returns true.

### 6.3 Command center (`command-center/command-center.tsx`)

**Opening and closing:**
- Opened with ⌘K / Ctrl+K or the sidebar Search row. ⌘P / Ctrl+P opens it in **Files scope**.
- **There are no `>`, `@` or `#` prefixes.**
- Closed with Esc, a backdrop click, choosing a result, or ⌘K again.

**Panel:**
- size 640×560 (max 92% × 80%), top-centre with a 48px top inset
- backdrop `rgba(0,0,0,0.5)`; panel `surface0`, 1px `border`, radius 8, `shadow.lg`
- header: paddingH 16, paddingV 12, bottom border; input 14px with placeholder "Search commands, files, workspaces, and agents..."
- Files scope: a chip before the input (Folder 14 + "Files" + X 12; `surface2`, 1px `borderAccent`, radius 6) and placeholder "Search files...". Backspace on an empty query removes the chip

**Rows:**
- Heights: 36 for actions without a subtitle, choices and files; 56 for workspaces, agents and actions with a subtitle.
- Padding 16/8; highlight `surface1`.
- Shortcut badge on the right: `surface3` @50%, radius 6.
- Section headers: 32 for the first section, 49 when preceded by a divider.

**Keys inside:** ↑/↓ (wrap), Enter runs the active row, Esc closes. Mouse hover highlights a row but does not move the active row.

**Sections** (empty query; Workspaces and Agents limited to 5 rows each):
1. **Workspace actions** (when a workspace is active):
   - New agent, primary git action
   - when there is a query, also: New terminal, New browser, Open Changes/Files/Pull request, Pin/Unpin, Rename workspace, Copy path, Copy branch name, Show setup, Toggle Explorer, Prev/Next/Close tab, Rename tab, Copy id, Reload agent, Copy resume command, Copy file path, Close left/right/others, Labels ▸ name (toggle), Split/Focus/Move/Close pane, Toggle focus mode, secondary git actions (Commit, Pull, Push, Create PR, Merge…, Archive workspace)
2. **Actions:**
   - always: New workspace, Import session, History, Schedules, Settings, Keyboard shortcuts
   - only with a query: Add project, Home, Toggle left sidebar, Group by status/project
3. **Model ▸ provider ▸ model**, **Thinking ▸**, **Mode ▸**, **Plan mode ▸ On/Off**, **Fast ▸ On/Off**: choice rows from the active composer, shown only with a query
4. Plugin commands
5. **Workspaces**: all hosts; title `title ?? name`; subtitle `[host] · project · branch`. A query like `#123` / `pr 123` matches the PR number
6. **Agents**: title, subtitle `[host] · workspace · time ago`, sorted permission > attention > running > recent
7. **Files**: Files scope, or any non-empty query

**Scoring:** whitespace tokens, all of which must match. Tiers: exact, whole word, prefix, word-start, substring, subsequence (`protocol/src/search/text-match.ts`).

**File search request** (100 ms debounce):
```json
{"type":"directory_suggestions_request","requestId":"…","query":"app.ts","cwd":"/home/u/code/paseo",
 "includeFiles":true,"includeDirectories":false,"limit":100}
```
Response: `directory_suggestions_response {payload:{requestId, directories: string[], entries: [{path, kind:"file"|"directory"}], error|null}}`. Choosing a file reveals a `file` tab in the focused pane.

### 6.4 Shortcuts help dialog
"Shortcuts" sheet with a search box, sections in this order: General, Projects & Workspaces, Tabs & Panes, Layout, Agent Input. Rebinding happens in Settings → Shortcuts (capture mode; Backspace deletes the last step).

---

## 7. Visual design tokens

Sources: `app/src/styles/theme.ts`, `styles/identity-colors.ts`, `constants/layout.ts`, `components/ui/control-geometry.ts`, `desktop/src/window/*`.

### 7.1 Themes
- The default setting is `auto`: `light` on a light OS, `dark` ("Paseo") on a dark OS.
- Selectable variants: zinc, midnight, claude, ghostty, pureBlack, plus plugin themes.
- The accent is fixed per theme; there is no accent picker.
- **No fonts are bundled.**
- Electron window: default 1200×800, minimum 400×300; background `#181B1A` (dark) / `#ffffff` (light).

### 7.2 Dark ("Paseo", default) semantic palette

| Token | Value | Token | Value |
|---|---|---|---|
| surface0 (app bg, headers, tab bar) | `#181B1A` | foreground | `#fafafa` |
| surface1 | `#1E2120` | foregroundMuted | `#A1A5A4` |
| surface2 | `#272A29` | foregroundExtraMuted | `#717574` |
| surface3 | `#434645` | border | `#252B2A` |
| surface4 | `#595B5B` | borderAccent | `#2F3534` |
| surfaceSidebar | `#141716` | accent | `#20744A` |
| surfaceSidebarHover (= surface1) | `#1E2120` | accentBright (links) | `#7ccba0` |
| surfaceSidebarSelected (= surface2) | `#272A29` | accentForeground | `#ffffff` |
| surfaceWorkspace (= surface1) | `#1E2120` | destructive | `#c64f43` |
| surfaceDiffEmpty | `#252827` | popover (= surface2) | `#272A29` |
| interactionHighlight | `rgba(255,255,255,0.08)` | ring | `#d4d4d8` |
| statusSuccess | `#6cb17b` | statusDotSuccess (unread/attention) | `#35c264` |
| statusDanger | `#d8847b` | statusDotDanger (failed) | `#f7796d` |
| statusWarning | `#c09664` | statusDotWarning (needs input) | `#db932e` |
| statusMerged | `#a890d5` | statusDotRunning | `#5caaf6` |
| status*Tint | status colour + `29` (≈16%) | diff added / removed bg | `rgba(108,177,123,0.15)` / `rgba(216,132,123,0.1)` |

**Dark shadows:**

| Name | Colour | Offset | Radius |
|---|---|---|---|
| sm | `rgba(0,0,0,.25)` | 0/2 | 4 |
| md | `rgba(0,0,0,.20)` | 0/4 | 8 |
| lg | `rgba(0,0,0,.40)` | 0/12 | 24 |

**Terminal ANSI (dark):**
- background surface0, foreground and cursor `#fafafa`, selection `rgba(255,255,255,0.2)`
- normal: black `#141716`, red `#e07070`, green `#5dba80`, yellow `#d4a44a`, blue `#6a9de0`, magenta `#b07ad0`, cyan `#4aabb8`, white `#d4d4d8`
- bright: brightBlack `#434645`, brightRed `#e89090`, brightGreen `#7ecf9a`, brightYellow `#e0be6e`, brightBlue `#8ab4e8`, brightMagenta `#c49ae0`, brightCyan `#6ec2cc`, brightWhite `#f0f0f2`

**Dark variants.** All variants share the status, status-dot, diff, ANSI and shadow values above.

| Token | zinc | midnight | claude | ghostty | pureBlack |
|---|---|---|---|---|---|
| surface0 | `#18181b` | `#161820` | `#1f1f1e` | `#282c34` | `#000000` |
| surface1 | `#1f1f22` | `#1c1e27` | `#262523` | `#2f333d` | `#0a0a0a` |
| surface2 | `#27272a` | `#252731` | `#2f2d2b` | `#383c48` | `#111111` |
| surface3 | `#3f3f46` | `#3c3e4c` | `#4a4745` | `#4a4f5e` | `#202020` |
| surfaceSidebar | `#131316` | `#121420` | `#1a1918` | `#21252d` | `#000000` |
| foregroundMuted | `#a1a1aa` | `#9a9db0` | `#ada9a5` | `#c8ccd8` | `#a1a1aa` |
| border | `#27272a` | `#242636` | `#2c2a27` | `#353a47` | `#1c1c1c` |
| accent | `#e4e4e7` | `#3b6fcf` | `#d97757` | `#89b4fa` | `#20744A` |

**Light theme** (brief):
- surfaces: surface0 `#ffffff`, surface1 `#fafafa`, surface2 `#f4f4f5`, surface3 `#e4e4e7`; surfaceSidebar `#f4f4f5`
- text: foreground `#1a1a1e`, muted `#71717a`, extraMuted `#a1a1aa`
- border `#e4e4e7`
- status: `#3e704a` / `#9d433b` / `#7b5d39`
- dots: success `#299f51`, danger `#f12e2f`, warning `#b37824`, running `#268ae0`
- derivation differences from dark: sidebarSelected = **surface3**, popover = surface0

### 7.3 Identity colours (project icon fallback, host colour, workspace labels)

The project icon colour is picked by `hash = hash*31 + charCode` over the view key, modulo 10.

| Name | Fill (icon bg, white letter) | Foreground dark | Foreground light |
|---|---|---|---|
| violet | `#7a6aa8` | `#a392d5` | `#6d49b5` |
| sky | `#3d7ea6` | `#6aa6ce` | `#3d6985` |
| emerald | `#388068` | `#6cae96` | `#3e6e5d` |
| orange | `#a4673a` | `#cc8f64` | `#845838` |
| pink | `#b05c80` | `#d87da3` | `#974168` |
| indigo | `#6a70b8` | `#9299d5` | `#5251c2` |
| teal | `#368080` | `#6cacab` | `#3e6d6c` |
| red | `#b06260` | `#d88381` | `#9c4243` |
| amber | `#8f7838` | `#b29d64` | `#716239` |
| blue | `#5179b0` | `#7ba1d5` | `#39649e` |

**Label chip:** background = fill + `1a` alpha; text in the foreground colour. Label dot 10px.

### 7.4 Scales

| Group | Values |
|---|---|
| Spacing | 0.5:2, 1:4, 1.5:6, 2:8, 3:12, 4:16, 6:24, 8:32, 12:48, 16:64 |
| Font size | sm 12, **base 14**, lg 16, xl 18, 2xl 20, 3xl 22, 4xl 26; content (chat) 15; code 12 |
| Font weight | normal 400, medium 500, semibold 600, bold 700 |
| Radius | sm 2, base 4, md 6, lg 8, xl 12, 2xl 16, full 9999 |
| Icon size | xs 12, sm 14, md 16, lg 20 (lucide icons) |
| Border width | 1 (2 for focus rings) |
| Breakpoints | sm 576, md 720, lg 992, xl 1200. "Compact" layout is under 720 |
| Control heights | tight 28, compact 32, field 44; header icon button 26 |
| Line heights | sidebar title 20, meta row 16, menu row 18, chat `round(15×1.4)` = 21, diff `round(code×1.5)` = 18 |

**Font stacks:**

| Use | Stack |
|---|---|
| UI | `system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif` |
| Mono | `SFMono-Regular, Menlo, Monaco, Consolas, 'Liberation Mono', 'Courier New', monospace` |
| Terminal | `JetBrains Mono, JetBrainsMono Nerd Font, …, Menlo, monospace`, at size = code font size (12) |

**User appearance settings:**
- `uiBaseFontSize` (10–21, default 14) scales sm through 4xl by `size/14`.
- `contentFontSize` (default 15) and `codeFontSize` (default 12) are set separately.
- Spacing, radius and icon sizes do **not** scale.
- Default syntax theme: "One" (One Dark: base `#abb2bf`, keyword `#c678dd`, comment `#5c6370`, string `#98c379`, number `#d19a66`, function `#61afef`, type `#e5c07b`, tag `#e06c75`, operator `#56b6c2`).

### 7.5 Layout dimensions (quick reference)

| Element | Value |
|---|---|
| Left sidebar | 320 default (200–600), `surfaceSidebar`, 1px right border |
| Header rows / tab bar | 36 high (`HEADER_INNER_HEIGHT`, `WORKSPACE_SECONDARY_HEADER_HEIGHT`) |
| Sidebar nav row | minHeight 28, radius 8, paddingH 8 inside an 8px container |
| Project row / workspace row | minHeight 36 / 36; workspace rows are effectively 36 + meta line |
| Status slot | **16 wide × 20 high** (`theme.iconSize.md`); dot 6, overlay dot 8, alert 12, ring 12 in a 14 frame |
| Sidebar footer | paddingV 12; add-project button 32 high; icon buttons 28 |
| Workspace tab | 28 high, 96–160 wide, radius 6 |
| Explorer dock | 320 default, min 240; rail 36 high, tabs 26 |
| Menus | `surface1`, 1px `borderAccent`, radius 8, `shadow.md`; rows 28 high, padding 4/8, radius 6, hover `surface2` |
| Tooltip | popover bg (`surface2`), 1px `borderAccent`, radius 12, padding 4/8 |
| Command center | 640×560, radius 8 |
| macOS traffic lights | at (16,10); reserved area 78×45 |
| Win/Linux window controls | top-right: Windows 46 wide, Linux 36 wide; close hover `rgba(232,17,35,0.9)` |
| Scrollbar | 8px, thumb `foregroundExtraMuted` @40% (hover `foregroundMuted`) |

**Timings:**

| Item | Value |
|---|---|
| Status ring rotation | 900 ms |
| Skeleton pulse | 1 s each way |
| Shortcut badge reveal | 150 ms |
| Resize highlight delay | 100 ms |
| Tab tooltip | 400 ms |
| Sidebar tooltips | 300 ms |
| Hover card fade | 80 ms |
| Submenu hover-open | 90 ms |

---

## 8. Implementation notes and oddities

1. **Tab and layout state is client-local only.** Do not look for a sync API. Closing a root-agent tab **archives** the agent, and closing a terminal tab **kills** it.
2. **No daemon order for the sidebar.** Order comes from client sort (name) plus the user's drag order; `activityAt` is always null in v0.10.1.
3. **Archive and unarchive are asymmetric.** Archived workspaces vanish from the wire, and unarchive is `workspace.recovery.*`.
4. **Labels:** there is no create RPC (assignment creates the label) and no id (the name is the identity).
5. **Hosts are not "switched".** All hosts are live at once; the sidebar merges projects across hosts by `projectKey`.
6. **Dead or inert code in v0.10.1, safe to skip:**
   - `components/sidebar/sidebar-workspace-row.tsx` is unused.
   - The "Creating..." row state never triggers.
   - The `new-worktree` shortcut has no binding.
   - The project chevron uses hard-coded `#9ca3af`.
   - "Mark as read/unread" and the "Workspaces" header are hard-coded English.
7. **Ctrl+B vs Ctrl+.:** on non-mac the left-sidebar binding is Ctrl+B, though its id says `ctrl-period` and one tooltip advertises `mod+.`.
8. **Possible Ctrl+W overlap (inferred):** the Electron Window menu's `close` role may default to Ctrl+W on Windows/Linux, overlapping close-tab. Not confirmed.

---

## Files read

All paths are under `/tmp/paseo-v0101/packages/`.

**Protocol:**
- `protocol/src/messages.ts`: envelope ~7440–7485; directory-sync requests ~1212–1310; workspace/project schemas ~3820–4560; terminals; labels; recovery; creation; directory suggestions
- `protocol/src/agent-state-bucket.ts`, `agent-lifecycle.ts`, `workspace-labels.ts`, `connection-offer.ts`, `host-connection-schema.ts`, `daemon-endpoints.ts`, `terminal-activity.ts`, `client-capabilities.ts`, `search/text-match.ts`, `agent-labels.ts`, `agent-deep-link.ts`

**Client:** `client/src/daemon-client.ts` (connection state, reconnect, hello, owned subscriptions, workspace/label/project/terminal RPCs), `client/src/connection/{owned,index,legacy}.ts`, `client/src/creation/index.ts`

**Server (semantics only):** `server/src/server/session.ts` (dispatch and handlers), `workspace-directory.ts`, `workspace-registry.ts`, `workspace-registry-model.ts`, `workspace-archive-service.ts`, `workspace-labels/internal/service.ts`, `directory-sync/index.ts`, `directory-sync/internal/versioned-collection.ts`, `project-key.ts`, `agent/agent-manager.ts` (excerpts)

**App, hosts and runtime:**
- `hosts/{appearance.ts,use-host-badges.ts,host-badge.tsx,host-chooser.tsx}`, `types/host-connection.ts`
- `runtime/host-runtime.ts`, `runtime/host-features.ts`, `runtime/daemon-start-service.ts`, `runtime/websocket-factory*.ts`
- `runtime/directory-sync/{index,transaction,agent-replica,workspace-replica,internal/agent-store}.ts`
- `runtime/replica-cache/*`, `runtime/activity/*`
- `navigation/{host-runtime-bootstrap.ts,host-route-context.tsx,workspace-route-navigation.ts,settings-navigation.ts}`
- `components/{host-status-dot.tsx,hosts/host-picker.tsx,pair-link-modal.tsx,pair-link-credentials.ts}`, `app/pair-scan.tsx`
- `utils/{client-id,daemons,connection-selection,test-daemon-connection,agent-directory-sync,agent-snapshots}.ts`

**App, projects and workspaces:**
- `projects/*`
- `workspace/{workspace-archive,project-workspace-archive,use-workspace-archive,mark-unread,legacy-daemon-workspaces}.ts`
- `workspace-labels/**`
- `stores/session-store.ts`
- `screens/new-workspace-screen.tsx` (parts), `screens/new-workspace/*` (parts)
- `components/{workspace-rename-modal,workspace-pin-shortcut-handler}.tsx`
- `hooks/{sidebar-workspaces-view-model,sidebar-status-view-model,use-sidebar-pins,use-sidebar-workspace-pin,use-global-workspace-pin-action,use-workspace-read-state,use-sidebar-workspaces-list}.ts`
- `utils/{sidebar-agent-state,status-dot-color,status-indicator-geometry,status-loader,workspace-agent-activity,project-status-badge-content}.ts`

**App, sidebar:**
- `sidebar-nav/{model,use-sidebar-nav-items}.ts`
- `components/{left-sidebar,sidebar-workspace-list,sidebar-resize-handle,sidebar-callout,sidebar-callout-slot.web,sidebar-agent-list-skeleton,workspace-hover-card,project-icon-view,diff-stat,draggable-list.web}.tsx`, `components/{sidebar-resize-handle-layout,desktop-sidebar-layout}.ts`
- `components/sidebar/**`: nav rows, header row, help menu, row content, workspace menu, project leading visual, status list, pinned header, empty states, display-preferences/*, workspace-meta-row/*, workspace-trailing
- `components/status-ring/*`, `components/desktop/titlebar-drag-region.tsx`, `components/headers/menu-header.tsx`
- `stores/{sidebar-view-store,sidebar-order-store,sidebar-collapsed-sections-store/*,navigation-active-workspace-store/*,last-workspace-selection,panel-store/*}.ts`
- `screens/settings/sidebar/sidebar-nav-section.tsx`
- `app/**` routes: `_layout`, index, welcome, sessions, schedules, open-project, new, settings/**, h/[serverId]/**
- `utils/{host-routes,sidebar-shortcuts,desktop-window}.ts(x)`

**App, tabs and panes:**
- `workspace-tabs/**`, `panels/**`
- `screens/workspace/**`: tab model, layout, desktop tabs row, presentation, menu, new-tab menu, pane state, bulk close, rename, explorer sidebar and tab rail, header, terminals, plus `workspace-screen.tsx` (large parts)
- `stores/workspace-layout-{store,storage,ids,constants,actions}.ts`, `stores/explorer-tab-memory.ts`
- `components/split-container*.ts(x)`, `split-drop-zone.tsx`, `explorer-sidebar-layout.ts`, `resize-handle-*.ts`, `sortable-inline-list*.tsx`
- `subagents/*` (close-tab policy, open-tab labels), `workspace/file-open/index.ts`, `utils/{split-navigation,prepare-workspace-tab}.ts`

**App, keyboard and command center:**
- `keyboard/**` (including `shift/**`), `stores/keyboard-shortcuts-store.ts`, `screens/settings/keyboard-shortcuts-section.tsx`
- `hooks/{use-keyboard-shortcuts,use-keyboard-shortcut-overrides,use-keyboard-action-handler,use-global-new-workspace-action}.ts`
- `command-center/**`, `plugins/command-center/*`, `components/keyboard-shortcuts-dialog.tsx`, `components/ui/shortcut.tsx`
- `utils/{format-shortcut,shortcut-platform,command-center-focus-restore}.ts`, `lib/overlay-root.ts`, `git/policy.ts`
- `desktop/src/features/menu.ts`, `desktop/src/features/browser-keyboard/*`

**App, theme:**
- `styles/*`, `appearance/*`, `screens/settings/appearance/*`, `hooks/use-settings/storage.ts`, `constants/layout.ts`, `plugins/themes/index.ts`
- `components/ui/{control-geometry,icon-button-chrome,button,tooltip,status-badge,trailing-action-scrim}.ts(x)`, `components/ui/menu/*`
- `git/diff-document/palette.ts`, `terminal/runtime/terminal-font.ts`, `highlight/src/{colors,themes}.ts`
- `desktop/src/window/{window-manager,chrome}.ts`, `desktop/src/main.ts`, `app/public/index.html`
- `i18n/resources/en.ts` (sidebar, tabs, shortcuts, command center, workspace labels strings)
