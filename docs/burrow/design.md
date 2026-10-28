# Burrow design

Burrow is Crusteo's own per-host daemon, one piece of a three-piece system:

1. **Burrow** (`crates/burrow-*`): runs coding agents and terminals on one host and speaks
   Paseo daemon protocol v1 ([protocol-inventory.md](protocol-inventory.md)).
2. **Reef** (`crates/reef`, `reef`): connects to many daemons through Crusteo's
   `paseo-client` and `fleet` crates, and streams a compact protocol to phones.
3. **Clients**: Crusteo desktop and Skiff, the page Reef serves to phones. The official Paseo apps also keep working
   against Burrow for every feature it advertises.

Burrow is only the daemon. Because it speaks unmodified protocol v1, Reef and the clients
treat Burrow and Paseo daemons the same, and a fleet can switch one host at a time.

This document gives the architecture, the reasons behind it, and the plan. Facts about Paseo
come from its 0.11.2 sources (`packages/server/src`, `packages/protocol/src`) unless marked
otherwise.

## 1. Goals

- **Wire compatibility.** A client that works against Paseo works against Burrow for every
  feature listed in `server_info.features`. Requests for features Burrow does not list get
  the reply Paseo gives an unknown request (`rpc_error`, `unknown_schema`), which clients
  already handle.
- **More efficient than Paseo.** See section 3.
- **Small and owned.**
  - Paseo's server is about 160,000 lines of non-test TypeScript in roughly 475 files, with
    166 `COMPAT(` shims.
  - Its largest files are `session.ts` (8,236 lines), the Codex provider (7,177), the Claude
    provider (6,199), the OpenCode provider (5,113) and `agent-manager.ts` (5,063).
  - Burrow implements only the subset its clients use. Each concern gets its own crate and is
    tested at the wire.
- **Proven side by side.** Burrow runs next to Paseo's daemon on the same hosts, with its own
  port (6797) and state folder (`~/.burrow`), never writing `~/.paseo`, until it is as good
  or better.

Not planned for now: the relay and E2EE transport, Hub (Paseo's own), voice and dictation,
plugins, forge integrations (PRs, checks), schedules. None of them is advertised.

## 2. Crates and layering

```
burrow (bin) ── burrow-daemon ──┬── burrow-protocol      wire types and rules; serde only
                                    ├── burrow-provider      session actor trait, echo provider
                                    │     ├── burrow-provider-claude   (done)
                                    │     ├── burrow-provider-codex    (done)
                                    │     └── burrow-provider-acp      (Cursor done)
                                    ├── burrow-terminal      PTY + headless VT state (P1)
                                    └── burrow-git           git CLI wrapper, worktrees (P1)
```

Modules inside `burrow-daemon`:

| Module | Owns |
|---|---|
| `server` | TCP accept and HTTP upgrade checks (`/ws` path, `Host`, `Origin`) |
| `connection` | One socket: hello, close codes, lease, outbound byte accounting, dispatch |
| `subscriptions` | The socket's owned subscriptions; turns store events into frames |
| `handlers` | One function per request type, from request to frames |
| `store` | The directory (projects, workspaces, agents, labels) and timelines, with seqs |
| `daemon` | Config, the store lock, the change feed |
| `agents`, `agent_requests`, `coalesce` | The agent manager: provider sessions, turns, permissions, attention, 60 ms coalescing |
| `labels` | The workspace label catalog, assignments and the label change journal |
| `persist` | Records and append-only timelines in the home directory, written off the store lock |
| `paseo_import` | `burrow import-paseo`: Paseo's agents (every provider both run), projects and workspaces into the store, once |
| `git` | What a workspace's checkout is; the `git` CLI with timeouts; path rules |
| `workspace_requests` | Adding projects, making and archiving workspaces, the forms' reads |
| `worktrees` | Burrow-owned git worktrees in Paseo's layout; branch reads and in-place branch changes |
| `project_key` | Paseo's `projectKey` |
| `setup` | `paseo.json` setup and teardown scripts |
| `suggest` | The folder picker's search |

Why this layering:
- The protocol crate is the piece that merges with Crusteo's wire types (section 10), so it
  depends only on serde.
- Providers are the riskiest code. They sit behind a channel interface, so a broken adapter
  cannot corrupt daemon state.
- Each provider crate depends only on `burrow-provider`, so it can be tested offline against
  recorded transcripts.

## 3. Runtime and efficiency

**Runtime.**
- **tokio**, multi-threaded.
- **tokio-tungstenite**, used directly rather than through axum. What clients observe is at the
  edges of the WebSocket layer, and a thin layer keeps those edges explicit:
  - upgrade refusals with HTTP 400 and 403;
  - close codes 4001, 4002, 4003 and 4401 with Paseo's reason texts;
  - termination without a close frame, which the client sees as 1006;
  - queued outbound bytes counted against 64 MiB.
- When HTTP routes are needed (`/api/health`, `/api/status`, `/api/files/download`), put axum
  in front and hand the upgraded sockets to the same connection code. axum's WebSocket also
  runs on tungstenite.

**Sockets.**
- Each socket has a reader task, which owns its subscriptions, and a writer task, which drains
  a channel.
- Bytes count from enqueue until written. Exceeding 64 MiB terminates the socket, as in Paseo.

**Store consistency.**
- The store sits behind one `Mutex<Store>`. Each mutation publishes its events on a `broadcast`
  channel while still holding the lock.
- A connection subscribes to that channel before it reads anything, and each subscription
  records the collection head it was opened at. So for any snapshot, an event either came before
  it (its seq is at or below that head, and it is dropped) or comes after it.
- Paseo needs per-id buffering and `updatedAt` comparisons for the same guarantee.
- A connection that falls 4,096 events behind is terminated. It reconnects and catches up from
  its cursors.

**Handlers.**
- Phase 0 handlers are synchronous reads and run inline.
- P1 handlers that wait (provider start, git, PTY spawn) run as tasks that write to the
  socket's outbox. Paseo's replies are not FIFO either; clients correlate by `requestId`.

**Efficiency compared with Paseo**, and the measures that serve it:

| Paseo | Burrow |
|---|---|
| One Node event loop for every socket, provider stream and git call. Only terminals run in a worker process. | Native code on tokio's worker threads. Provider adapters and PTYs each run in their own tasks. |
| Timelines are memory-only. After a restart, each agent's history is re-parsed from provider transcripts when the agent loads, under a new epoch, so every client re-downloads every open transcript. | A durable row store keeps epochs and seqs, so a reconnecting client resumes with an `after` read of only what it missed (section 5). |
| Idempotency receipts (`agent-requests/`, `creations/`) and schedule `runs[]` grow without bound. | Bounded retention. |
| About 90 feature flags and broadcast-era delivery for legacy sockets. | Only owned subscriptions. Only implemented features are advertised. |
| A frame is serialized per socket. | P1: serialize a push once per event and splice in the `subscriptionId`, because many sockets watch the same agent. |

Coalescing stays the same as Paseo's, because clients are tuned to it: streamed assistant text
and tool updates in a 60 ms window, terminal output in a 5 ms window.

## 4. Sessions, subscriptions and directory sync

**Hello and admission** (done, except auth).
- Checks run in Paseo's order:
  1. protocol version: below 1 gets `hello.rejected` if the client asked for it, then close 4003;
  2. auth;
  3. empty `clientId`: close 4002 `Invalid hello`;
  4. reserved `plugin:` clientId: close 4002 `Invalid plugin clientId`.
- Then exactly one `server_info`. A second hello gets 4002 `Unexpected hello`.

**Auth** (P0 remainder).
- With no password configured, every client is the owner.
- With a password, `hello.auth` carries `{kind:"password"}` (checked against a bcrypt hash) or
  `{kind:"localCredential"}` (a per-run token file).
- Legacy forms: an `Authorization: Bearer` header, or a `paseo.bearer.<password>` subprotocol.

**Lease** (done, tested with shortened timers).
- The first bare `{"type":"ping"}` claims a 45 s lease. Every later data frame renews it.
- Leases are checked every 10 s. An expired one terminates the socket.
- WebSocket control pings do not count. A socket that never pings is never leased.

**Session identity** (P1, together with `client_heartbeat`).
- A session is keyed by `(principal, clientId)`, can hold several sockets, and survives 90 s
  after its last socket closes.
- Resume matters for per-session state such as heartbeat focus for `shouldNotify`. It never
  restores subscriptions.

**Owned subscriptions** (done).
- Each subscription belongs to one socket and has a host-assigned UUID.
- Every subscribe request opens a new, independent subscription. A client-chosen
  `subscriptionId` gets `handler_error`.
- Releasing an unknown id is acknowledged.
- A socket without the `owned_subscriptions` capability gets no implicit delivery. The official
  apps, Crusteo and Reef all advertise it.

**Directory sync** (done).
- There is one `generation`, a UUID per daemon process, shared by the agent, workspace, project
  and label collections. Each collection has its own seq.
- A `sync` read returns either:
  - `snapshot`, with reason `no_cursor`, `generation_changed` or `cursor_expired`; or
  - `changes`: items with a later seq, plus removals from the last 5,000 tombstones.
- A `sync` read ignores `page`. A sequenced agent read requires `scope:"active"` and no filter.
- The generation stays per process even once state is persisted. A restart then costs each
  client one directory snapshot, which is small and already handled. Timelines, which are
  large, are what the durable store protects.

## 5. Timelines

**What Paseo does.**
- Production uses `InMemoryAgentTimelineStore`. Bootstrap deletes any old `agent-timelines/`
  directory.
- Agents load lazily after a restart. On load, `hydrateTimelineFromProvider` replays the
  provider's `streamHistory()`:
  - Claude: `~/.claude/projects/<dir>/<session>.jsonl` and its subagent files;
  - Codex: `thread/read`;
  - ACP: `loadSession`.
- The replay gets a new epoch and fresh seqs.
- Reads are projected (`timeline-projection.ts`). These folds apply:
  - adjacent assistant chunks (`assistant_merge`) and reasoning chunks (`reasoning_merge`);
  - a tool call's updates, by `callId` (`tool_lifecycle`);
  - plugin items (`identity`).
- Entries carry `seqStart`, `seqEnd` and `sourceSeqRanges`. Page limits count entries.

**Burrow.**
- **Canonical rows** (done). One row per provider event; `seq` starts at 1 per epoch. Live
  `agent_stream` frames carry these rows.
- **Durable rows** (done). Each row is appended to `timelines/<agentId>.jsonl`, whose first line
  holds the epoch.
  - Epochs and seqs survive restarts, so client caches and Reef's caches stay valid.
  - Only the recent tail of each agent is held in memory; older pages are read from the file.
  - Provider replay is needed only to import a session started elsewhere.
- **Projection at read time** (P1). Port the folding rules and `selectProjectedTimelinePage`:
  limits count entries, and a `tail` page pulls in earlier anchors whose updates overlap it.
  Phase 0 serves one entry per row. That is a valid unfolded projection, and clients render it
  correctly because they already merge live rows the same way.
- **Turn ids are opaque, and the provider's.** `foreground-turn-N` and `autonomous-turn-N` are
  Claude's; Codex sends ids like `codex-turn-0`. When a provider gives none, Paseo's agent
  manager falls back to the active foreground turn id, or else `autonomous-<uuid>`. Burrow
  does the same. Every live row carries a turn id; replayed history may not.
- **Failed turns.** `turn_failed` also appends an `assistant_message` that reads
  `"[System Error] <error>…"`. Clients show that row, so Burrow keeps it.

## 6. Agents, permissions and persistence

- **States:** `initializing | idle | running | error | closed`. `closed` means resumable but
  with no live runtime. Archive is a separate `archivedAt`.
- **No daemon-side queue.** Sending while a turn runs either interrupts it or steers it
  (`activeTurnBehavior`). The message queue lives in the clients.
- **Lazy load.** Agent records are read at startup; provider processes start only when an
  agent is used.
- **Permission requests never time out.** They stay pending, on the agent snapshot as
  `pendingPermissions`, until they are answered, the turn fails or is canceled, or the agent is
  archived. They are not tied to any socket.
- **Questions** are permission requests with `kind:"question"`, and plan approvals use
  `kind:"plan"`. Where they come from:
  - Claude: `AskUserQuestion` and `ExitPlanMode` through `can_use_tool`;
  - Codex: `item/tool/requestUserInput`.

**Persistence.** State lives in `$BURROW_HOME` (default `~/.burrow`). Phase 0 writes only
`server-id`.

| Path | Format | Phase |
|---|---|---|
| `server-id` | text, `srv_<hex>` | done |
| `config.json` | listen address, hostnames, password hash, providers, worktree root | P1 |
| `agents/<agentId>.json` | agent record with Paseo's fields, including the provider resume handle | done |
| `projects.json`, `workspaces.json` | registries; workspaces carry their label assignments | done |
| `workspace-labels.json` | the label catalog, `[{name, color}]` as in Paseo | done |
| `timelines/<agentId>.jsonl` | an epoch header, then one canonical row per line | done |
| `receipts/` | idempotency receipts, pruned by age | P1 |
| `worktrees/<project hash>/<slug>` | Burrow-owned git worktrees, in Paseo's layout | done |

- JSON files are written to a temp file and renamed into place, as Paseo does.
- Store modules expose operations rather than raw reads and writes, so SQLite can replace a
  store later without touching its callers.
- JSON comes first because it is easy to inspect, and close enough to Paseo's layout that the
  read-only importer from `~/.paseo` is small ([README.md](README.md#move-a-host-over-from-paseo)).
  It goes through the store like any other change, so imported records are Burrow's own.

## 7. Providers

**The interface** (`burrow-provider`, done).
- A provider describes itself (modes, models) and starts sessions.
- A session is an actor. The daemon sends `Command`s: prompt, interrupt, set mode, set model,
  answer a permission, shut down.
- It receives `ProviderEvent`s: session started, turn started / completed / failed / canceled,
  timeline item, permission requested, exited. A session that knows its own state (ACP) also
  reports its modes, its `runtimeInfo` and its usage.
- A provider can name its own foreground turn ids and say its catalog is still loading; both
  default to Claude's behaviour.
- The adapter task owns the child process.
- The agent manager owns the timeline, attention and permission bookkeeping, and coalescing, so
  none of that is written once per provider.

### Claude Code

Paseo goes through `@anthropic-ai/claude-agent-sdk` 0.3.246, which spawns the user's `claude`
from PATH (`resolveClaudeBinary`) rather than the binary the SDK bundles.

Verified in that SDK's `sdk.mjs`, the spawn line is:

```
claude --output-format stream-json --verbose --input-format stream-json
       [--permission-prompt-tool stdio] [--resume=<session>] [--model ..] [--permission-mode ..] ...
```

- **stdin** carries user messages and the host's `control_request`s. Subtypes include
  `initialize`, `interrupt`, `set_permission_mode`, `set_model`, `set_max_thinking_tokens`,
  `rewind_files` and `mcp_*`.
- **stdout** carries the stream (`system`, `user`, `assistant`, `stream_event` partials,
  `result`) and the CLI's `control_request`s. The one that matters is `can_use_tool`, answered
  with a `control_response` allow or deny, which becomes a permission request.
- Paseo's options:
  - `includePartialMessages`;
  - `allowDangerouslySkipPermissions`, so the agent can switch to bypass mode later;
  - `enableFileCheckpointing`, which rewind needs;
  - a `claude_code` preset system prompt with an appended section;
  - forwarded subagent text, and an interrupt that stops only the main turn.
- Burrow drives the CLI directly in Rust, without Node.
- **Risk:** this is the SDK's protocol, not a documented public API. Pin a tested range with
  `claude --version`, and keep recorded transcripts as fixtures (section 9). Here, WSL has
  Claude Code 2.1.296; the SDK bundles 2.1.246.

### Codex

`codex app-server`: JSON-RPC 2.0 over stdio; `CODEX_HOME` defaults to `~/.codex`.

| Direction | Methods |
|---|---|
| Requests | `initialize`, then the `initialized` notification; `thread/start`, `thread/resume`, `thread/read`, `thread/list`, `thread/fork`, `thread/rollback`, `thread/archive`, `thread/unarchive`, `thread/compact/start`; `turn/start`, `turn/steer`, `turn/interrupt`; `model/list`, `collaborationMode/list`, `skills/list`, `config/read` |
| Notifications | `thread/started`; `turn/started`, `turn/completed`, `turn/plan/updated`, `turn/diff/updated`; `item/started`, `item/completed`, `item/agentMessage/delta`, `item/reasoning/summaryTextDelta`, `item/fileChange/outputDelta`; `thread/tokenUsage/updated` |
| Server requests | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/tool/requestUserInput`, `mcpServer/elicitation/request` |

`codex app-server generate-json-schema` exists (codex-cli 0.162.1 here). The adapter's types
can therefore be generated with `typify` for the installed version, instead of reconstructed
from Paseo's 7,177-line adapter.

### ACP agents

- Paseo uses `@agentclientprotocol/sdk` ^0.17 over stdio.
- Agent methods: `initialize`, `newSession`, `loadSession`, `listSessions`, `prompt`, `cancel`,
  `setSessionMode`, `setSessionConfigOption`, `unstable_setSessionModel`,
  `unstable_closeSession`.
- Client callbacks the daemon must serve: `requestPermission`, `sessionUpdate`, file read and
  write, and terminal create, output, wait, release and kill.
- `burrow-provider-acp` is a small JSON-RPC client of its own; no ACP crate is in the lock file.
  An agent is a configuration: provider id, command, the client capability `_meta` it wants, and
  how it lists models (the session's answer, or Cursor's `cursor/list_available_models`).
- Cursor is the first (`cursor-agent acp`, provider `cursor`, as Paseo's ACP catalog adds it).
  Its shapes and differences are in [compatibility.md](compatibility.md#cursor-acp).
- OpenCode (an HTTP server plus a bridge plugin) and omp are P2.
- pi is done: `burrow-provider-pi` drives `pi --mode rpc` (JSONL commands, responses and
  events), as Paseo does; see [compatibility.md](compatibility.md#pi).

## 8. Terminals and worktrees

**Terminals** (P1).
- Paseo forks a worker process that runs node-pty. Each terminal has an `@xterm/headless`
  instance with 1,000 lines of scrollback.
- Wire frames are `[opcode][slot][payload]`:

  | Opcode | Payload |
  |---|---|
  | `0x01` | output |
  | `0x02` | input |
  | `0x03` | resize (JSON) |
  | `0x04` | snapshot (JSON `TerminalState`: cell grid, scrollback, wrap flags) |
  | `0x05` | restore |

- Slots are assigned per socket by `subscribe_terminal_request`. Restore modes are `live`,
  `visible-snapshot` and `full-snapshot`.
- Output coalesces in 5 ms. Terminals do not survive a daemon restart.
- Burrow uses `portable-pty` (ConPTY on Windows) and a headless VT emulator. Candidates are
  `alacritty_terminal`, `vt100`, or Crusteo's `term` crate, which would share fixes with the
  client.
- A PTY producing output faster than a socket drains it is throttled for that subscriber; the
  queue does not grow.
- Surviving a daemon restart would need a separate PTY-holder process. That is P2 at the
  earliest.

**Worktrees** (done; [compatibility.md](compatibility.md#projects-and-workspaces)).
- Shell out to `git`, as Paseo does:
  - `worktree add <path> [-b <branch> --no-track <base>]`;
  - `worktree list --porcelain`;
  - `worktree remove --force`, then remove the directory with retries, then `worktree prune`.
- Location: `$BURROW_HOME/worktrees/<project hash>/<slug>` (`--worktrees-root`); a collision
  adds `-1`, `-2`, ...
- Every git call runs on a blocking thread with a timeout. Paseo's process limits (64 git starts
  per second, 8 concurrent) are not kept yet.
- Not libgit2: its worktree support is partial, and matching what users run in their shells
  matters more.

## 9. Compatibility testing

1. **Wire tests (done).** `tests/wire.rs` drives the daemon with a bare WebSocket client. It
   checks frames, close codes, lease expiry, sync modes and per-socket delivery.
2. **Client tests (done).** `tests/paseo_client.rs` runs Crusteo's unmodified `paseo-client`
   through Crusteo's bootstrap sequence and a live push.
3. **Schema oracle (P1).**
   - The test suite exports every frame Burrow emits as JSONL, using Crusteo's
     `{"dir","frame"}` fixture format.
   - A Node script validates each frame with Paseo's own
     `WSOutboundMessageSchema.safeParse` from `@getpaseo/protocol`.
   - Paseo's schemas then serve as the conformance suite.
4. **Record and replay against real Paseo (done for the agent path; see compatibility.md).**
   - Run a throwaway Paseo daemon with its own `PASEO_HOME` on a spare loopback port. Never use
     the team's daemons.
   - Run the same scripted session against it and against Burrow, using `paseo-client` and a
     model-free provider.
   - Normalize ids and timestamps, then diff the frames structurally.
   - Read-only `fetch_*` probes of real daemons are acceptable only to answer a specific
     question.
5. **Provider fixtures (done for Claude: scripted transcripts and one recorded session).**
   - Record one real transcript per scenario: a text turn, a tool call, allow and deny, an
     interrupt, a question, and a resume. This costs model time once.
   - Replay the transcripts offline in CI.
6. **Manual end-to-end runs.** Crusteo, Reef and the official desktop app against a local
   Burrow, before a phase is called done.

## 10. One shared protocol crate

Today the wire format is described twice (the type-by-type list is in
[README.md](README.md#next-step-one-shared-protocol-crate)):
- Burrow's `burrow-protocol` holds the server's side: typed payloads it produces, inbound
  request structs, reply naming, codes.
- Crusteo's `paseo-client` holds the client's side: `proto.rs` (lenient typed views,
  capabilities), the envelope handling in `connection.rs`, and the terminal frame codec in
  `terminal.rs`.

Reef adds a third consumer. Merge them into one `crates/paseo-protocol`:

1. **Contents.**
   - Envelopes, hello, `server_info` and capability names.
   - Reply naming, close and error codes, timing constants.
   - Directory, timeline, permission and terminal payloads.
   - Binary frame codecs.
   - No I/O and no async: serde and serde_json only.
2. **One type, both directions.** Every payload derives both `Serialize` and `Deserialize`.
   Unknown fields are tolerated, and optional fields default, as Crusteo's `proto.rs` does
   today. `Option` fields keep Paseo's distinction between `nullable` (sent as `null`) and
   `optional` (left out), so the daemon emits what Paseo emits.
3. **Order of work.**
   1. Rename `burrow-protocol` to `paseo-protocol`, and move in `paseo-client`'s terminal
      codec.
   2. Change `paseo-client` to re-export its types, and delete the duplicates in `proto.rs`.
   3. Do it after the P1 payloads (agents, permissions, terminals) settle, so the API moves once.
4. **Generated JSON Schema as an oracle, not a source.** Paseo's `codegen` step only compiles a
   fast outbound validator with zod-aot; it does not generate types.
   - `z.toJSONSchema` on the two wire unions, run through `typify`, could produce Rust types.
   - About 20 constructs are lossy: 14 `.transform()`, 2 `z.preprocess`, 13 `.catch()`, a
     recursive `z.lazy`, and the non-discriminated timeline item union.
   - Generated types would also make every Paseo quirk part of our API.
   - So use the schema as the CI oracle (section 9) instead.

Burrow crates depend only on each other and on `paseo-client`, and only as a
dev-dependency of `burrow-daemon`. They do not depend on the desktop app, `fleet` or Reef.
Keep it that way, so the daemon builds and ships without GPUI.

## 11. Roadmap

**P0: connect and show a host (done, apart from the items below).**
- Done:
  - the handshake, `server_info`, close codes, the lease and the outbound cap;
  - owned subscriptions;
  - the agent, workspace, project and label directories, with sync cursors;
  - timeline pages and live rows;
  - event subscriptions;
  - `unknown_schema` for everything else;
  - a stable `server-id`;
  - a demo seed;
  - the echo provider.
- Remaining:
  - password auth and the matching `hello.rejected` frames;
  - the 90 s session resume;
  - `/api/health` and `/api/status`.

**P1: run and talk to agents, and everything else Crusteo and Reef use.**
1. **Done:** persistence with durable timelines, and the agent manager. It is exercised end to
   end with the echo provider through `paseo-client` and `fleet`: create, send, steer, cancel,
   archive, refresh, delete, attention, permissions, questions and restarts.
   - Still to do: the schema oracle.
2. **Done:** the Claude adapter, with permissions, questions, plans, steer, interrupt and resume.
   It was verified against Claude Code 2.1.303 and diffed against Paseo 0.12.0 and 0.11.4
   ([compatibility.md](compatibility.md)).
3. **Done:** the providers snapshot and mode, model and thinking changes.
   - Still to do: `list_commands_request` content (the slash commands Claude reports in its
     `init` frame).
4. Timeline projection; `agent.timeline.list_prompts.request`;
   `fetch_agent_history_request`.
5. Workspaces:
   - pin, title, read/unread state; checking out pull requests (needs the forge services).
   - **Done:** workspace labels, the whole request family
     ([compatibility.md](compatibility.md#workspace-labels)).
   - **Done:** adding projects, `workspace.create.request` for folders and git worktrees with an
     initial agent, git options on agent creation, setup scripts, archive, and the folder and
     branch reads ([compatibility.md](compatibility.md#projects-and-workspaces)).
6. Terminals.
7. **Done:** the Codex adapter, with approvals, questions, steer, interrupt and resume, verified
   against codex-cli 0.162.1 ([compatibility.md](compatibility.md#codex)). It maps JSON by hand,
   as Paseo does, rather than through generated types.
   - Still to do: plan mode and the Speed feature, sub-agents, slash commands.
8. Usage reports, provider sub-agents, `agent.detach.request`.
9. Extract the shared `paseo-protocol` crate (section 10).

**P2: what the official apps use beyond that.**
- Git status and diff, file browsing and editing.
- ACP agents beyond Cursor (done: the ACP client and Cursor), and OpenCode.
- Project management, scripts and setup, import and resume, rewind and fork.
- Schedules, push tokens.
- Relay and plugins only if they are needed.

## 12. Risks

1. **Provider protocols change underneath us.** Claude's stream-json control protocol and Codex's
   app-server are versioned with their CLIs. They are not stable public APIs. Paseo's five main
   adapters add up to about 25,000 lines because of this. Mitigation: version gates, generated
   Codex types, recorded fixtures.
2. **Projection and paging are subtle.** Tool calls interleave, page anchors move, and limits
   count entries. Clients reconcile live rows against pages, so a mismatch shows up as
   duplicated or missing transcript rows. Port Paseo's logic together with its test cases.
3. **Paseo moves fast.** Changes on the wire are additive, but the official apps gate new UI on
   new feature flags, so Burrow will trail the official app's newest features. Crusteo and the
   Reef are the clients we control.
4. **Terminal snapshots** must match the xterm-shaped JSON cell grid closely enough for the
   clients' renderers.
5. **Windows hosts.** ConPTY, path forms in project keys, and killing a process tree on
   interrupt all behave differently there than on Linux, where Paseo daemons usually run.

## 13. Questions

Answered:
- **Providers:** Claude, then Codex.
- **Auth:** no password is needed on the locked-down networks Burrow runs on.
- **Migration:** Burrow runs next to Paseo with its own state. A host that switches over
  imports its agents once with `burrow import-paseo`, which only reads `~/.paseo`.
- **Platforms:** Linux and Mac hosts for now.

Still open:
1. **Clients.** The official desktop and mobile apps as well as Crusteo and Reef? The
   official apps need more of P2 before they feel complete.
2. **Version string.** The apps display `server_info.version` and warn when it differs from
   their own. Report `0.1.0-burrow` (the current default), or the Paseo version we are
   compatible with?
3. **Side-by-side trials.** What counts as "as good or better" before a host switches over?
   For example: the same transcripts, the same latency, and memory use over a week.