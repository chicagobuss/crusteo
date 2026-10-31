# Compatibility between Burrow and Paseo

Burrow and Paseo's daemon both speak Paseo's WebSocket protocol, version 1, so each side's
clients can talk to the other side's daemon. This page says how far that goes in each direction,
for which versions, and how each claim was checked. "Checked" here means one of three things:
run against the real thing, covered by automated tests, or read from Paseo's source. The tables
say which. The sections after them go into detail, area by area.

Every wire change from Paseo 0.10.3 to 0.14.0-beta.2 is additive, so one Burrow serves clients of
all those versions. It advertises only the feature flags it implements (`server_info.features`), and
clients hide or refuse the rest.

## At a glance

| Direction | Versions | State | How it was checked |
|---|---|---|---|
| Paseo's CLI → Burrow | 0.11.4, 0.12.0, 0.13.2, 0.14.0-beta.2 | Every agent command works, with or without `--host`, from a shell or from inside an agent. `paseo host …` (connected hosts), schedules and terminals do not ([below](#what-agents-and-tools-on-the-host-rely-on)) | [`scripts/paseo-cli-compat.sh`](../../scripts/paseo-cli-compat.sh) against a Burrow with the echo provider: 19 of 19 as expected on each version (0.12.0 and 0.13.2 on 2026-10-27, 0.14.0-beta.2 on 2026-10-28). Real Claude and Codex agents running the CLI inside their own sessions, on two hosts (2026-10-28) |
| Paseo's app → Burrow | 0.13.2 (its web build) | The core flows work ([below](#paseos-app-against-burrow)). Terminals, diffs, file editing, schedules, voice, connected hosts and relay pairing do not | Walked through by hand against a throwaway Burrow (2026-10-27). 0.14's app was not walked through; what it adds that Burrow serves (whole-turn pages) is covered by `burrow-daemon` tests |
| crusteo and Reef → Paseo's daemon | 0.11.0, 0.12.0, 0.13.2 in daily use | Everything crusteo and Reef use | Daily use on a mixed fleet; `fleet` and `reef` tests against fake daemons in Paseo's shapes |
| crusteo and Reef → Burrow | Burrow | Everything, and what Burrow adds (prompt images kept for every client, the label catalog in reef mode) | `burrow-daemon` tests drive Burrow with crusteo's own `paseo-client` and `fleet`; `reef` tests run a real Burrow; daily use |
| Paseo's state (`~/.paseo`) → Burrow | 0.11.4, 0.13.0, and a desktop-app install | `burrow import-paseo` brings Claude, Codex, Cursor, pi and Antigravity agents (sessions resume), projects, workspaces and labels | Hosts moved over, one with 83 agents from 0.13.0; `burrow-daemon/tests/import_paseo.rs` |
| Burrow → Paseo (going back) | Any | Paseo starts as it was, since its state was never written. What was done in Burrow since stays in `~/.burrow` | [install.md](install.md#going-back-to-paseo); not needed on any host so far |
| Daemon ↔ daemon (Paseo's connected hosts) | 0.13 | Not supported. A Paseo daemon refuses Burrow as a connected host | Read from Paseo's source |

## Paseo's CLI against Burrow

`scripts/paseo-cli-compat.sh <paseo> <host:port> <repo>` runs each command below with Paseo's own
CLI and says whether it did what it should. Its header shows how to install a CLI version on its
own and start a Burrow that runs no model.

| Command | 0.12.0 | 0.13.2 | 0.14.0-beta.2 |
|---|---|---|---|
| `ls -g`, `ls -a -g` (archived agents too) | works | works | works |
| `run` (waits for the reply), `run --no-wait` | works | works | works |
| `run --new-workspace worktree` (a worktree, its `paseo.json` setup) | works | works | works |
| `send` (waits for the reply), `send --no-wait` then `wait` | works | works | works |
| `send` from inside an agent (`PASEO_AGENT_ID`: shown as from that agent) | works | works | works |
| `run` from inside an agent (a child), `agent detach` | works | works | works |
| `logs`, `inspect`, `stop`, `archive` | works | works | works |
| `delete <id>`, `delete --all` | works | works | works |
| `status`, `daemon status`, `provider models`, `provider diagnostic`, `agent update --thinking` | n/c | n/c | works |
| `host ls` | no such command | refused: "Update the managing daemon to use connected hosts" | refused, the same |

n/c: not checked. The script passes `--host` to every command and sets `PASEO_AGENT_ID` itself for
the two "inside an agent" rows, so it says nothing about what an agent's own environment gives it.
That is checked separately ([below](#what-agents-and-tools-on-the-host-rely-on)).

Before 2026-10-27 the waits, sends from an agent, `delete --all`, `detach` and `ls -a` failed:
Burrow did not answer `wait_for_finish_request`, `fetch_agent_history_request` or
`agent.detach.request`, did not advertise `agentMessageProvenance`, and left archived agents out
of `includeArchived` reads.

## What agents and tools on the host rely on

Agents, people's shells, cron jobs and hooks use the daemon through more than the protocol: the
environment it starts agents in, the files in `~/.paseo` that Paseo's CLI reads, and how it ends
an agent's processes. Before 2026-10-28 Burrow did none of this, and agents on hosts that had moved
to Burrow lost messages to each other.

| What | Paseo | Burrow |
|---|---|---|
| An agent's environment | `PASEO_AGENT_ID`, `PASEO_AGENT_CWD`, and the daemon's `PASEO_HOME` | The two ids, and `PASEO_HOST` at Burrow's own loopback address in place of `PASEO_HOME` (both set makes the CLI refuse to choose). `PASEO_HOME` and what a Paseo agent, terminal or the desktop app sets are taken out, as Paseo takes them out. On macOS, the Paseo app's CLI folder is put on `PATH` when no `paseo` is on it |
| What that changes for a `paseo` command an agent runs | It reaches its daemon with no `--host`; `send` is from that agent and steers a busy target; `run` makes a child of it in its workspace; `heartbeat` works | The same. Without the ids a send arrived from nobody and *interrupted* a busy target's turn, and `run` made an unparented agent in a new workspace |
| `paseo run --env K=V` | Set in the agent's environment | The same, kept with the agent for its next start |
| `$PASEO_HOME/paseo.pid` | Written by the supervisor; every CLI command without `--host` finds the daemon through it (0.13 on) | Written while Burrow runs, in Paseo's format, and removed when it stops ([install.md](install.md#paseos-daemon-lock)). Never over a live Paseo daemon's lock. Without it every bare command said "Daemon is not running … Start with: `paseo daemon start`" |
| `paseo daemon start` | Starts the daemon | Finds Burrow running and starts nothing |
| `paseo daemon stop` | `SIGTERM` to the lock's pid | The same: it stops Burrow |
| Ending an agent's CLI | `SIGTERM` to its process tree, `SIGKILL` two seconds later | The same, to its tree and its own process group, while the CLI still runs. Its MCP servers, running tools and background tasks used to outlive it. A tool already handed to init (`nohup … &` from a shell that has exited) is out of reach under both |
| Agents' CLIs when the daemon stops | Ended with it | Ended before Burrow exits, which a service manager no longer does for them: each leads its own process group |

Checked with Paseo's 0.14.0-beta.2 CLI (and a host's own 0.11.4) on two hosts, with no `--host` and
no `PASEO_*` in the environment, from a shell and from `env -i`: `ls`, `status`, `daemon status`,
`provider ls/models/diagnostic`, `daemon pair` ("Relay pairing is disabled") and `daemon start`.
Then real agents: a Claude agent ran the CLI inside its own session (`ls`, `inspect` of itself,
`send` to a Codex agent in the middle of a turn, `run` of a child), and its message joined the
running turn as a message from it, the child was its own in its workspace, a background task it
started ended when it was archived, and the agents' CLIs ended with a Burrow restart.

Still missing, by how often they come up:
- **Live git state** of workspaces: branch, diff counts and PR state are read when a workspace is
  made and not again; a worktree removed by hand stays listed.
- **Requests not answered:** `workspace rename`, `project rename`, `worktree create` and `worktree
  archive` (`run --new-workspace worktree` and `workspace archive` do the same), terminals, `import`,
  `usage`, plugins, Hub, `reload` (the daemon's config), and `daemon stop` and `restart` given
  `--host`.
- **`run --output-schema`** is not enforced by the provider; only the CLI's own retry applies.
- **Paseo's daemon settings** in `~/.paseo/config.json` are not read: a daemon-wide system
  prompt, per-provider `env` and `command` overrides, and `daemon.auth.password`. A password
  set for Paseo does not protect Burrow, which accepts every client.
- **MCP servers** a client asks for are not passed to the agent (Burrow's own `paseo` server is;
  [below](#agent-tools-mcp)); Burrow does not say it supports them.
- **Paseo's orchestration skills** in `~/.claude`, `~/.codex` and `~/.agents` are not kept up to
  date.
- **Logs:** Burrow logs to stderr. Under launchd and SysV init its log file is not rotated, and
  `daemon status` names `~/.paseo/daemon.log`, which Burrow does not write.

## Agent tools (MCP)

Paseo gives its agents an MCP server, `paseo`, with tools to make and steer other agents, make
workspaces, keep heartbeats and schedules, and answer permissions (`mcp__paseo__create_agent`
and so on). Burrow gives Claude and Codex agents the same server, with Paseo 0.14.0's own
catalogue: its `tools/list` output (names, titles, descriptions, input schemas), captured from a
real daemon, is what Burrow serves, the agent-scoped catalogue to agents and the top-level one to
other clients. A test checks every tool Burrow serves against it. Checked with real agents
through Paseo's CLI 0.14.0: Claude, Codex, Cursor, OpenCode, Gemini, pi and Antigravity agents
each called `list_hosts` and `list_agents` and gave back the host's `serverId`, and a Claude agent
made a child with `create_agent` and was told mid-session when it finished.

- **Handed to the CLI** as Paseo does: Claude by `--mcp-config` (beside the user's own servers),
  Codex by `config.mcp_servers` on `thread/start` and `thread/resume`, pi by an extension that
  registers the server for that session (`pi.registerMcpServer`, as Paseo's own pi extension
  does; the user's `mcp.json` is left alone).
- **ACP agents** (Cursor, OpenCode, Gemini) get it in `mcpServers` on `session/new` and
  `session/load`, where Paseo's ACP client sends none: over HTTP when the agent's
  `mcpCapabilities.http` says it takes that, else as a stdio server, `burrow mcp-bridge`, which
  relays each JSON-RPC line to the HTTP server with the agent's token (passed in its environment,
  not its arguments). Gemini CLI connects MCP servers only in folders it trusts
  (`~/.gemini/trustedFolders.json`) and asks permission before each tool call, as it does for its
  own servers; Cursor asks too.
- **Antigravity** reads MCP servers only from its global `mcp_config.json` and from installed
  plugins, with no per-session way to add one, so Paseo gives it none. Burrow keeps a plugin,
  `~/.gemini/config/plugins/burrow/`, written when an agy agent first starts and its
  `mcp_config.json` rewritten whenever Burrow's path changes. It runs `burrow mcp-bridge`, which
  takes the server and token from the environment Burrow starts `agy` with. Started any other way
  (`agy` by hand) the bridge has no tools, so agy runs as before. The file holds no token; the
  agent's shell does see `BURROW_MCP_URL` and `BURROW_MCP_TOKEN` (its own token), since agy hands
  its whole environment to its servers. `agy plugin disable burrow` turns it off, and Burrow
  leaves a disabled plugin disabled.
- **Tool rows:** an MCP call by Cursor, OpenCode, Gemini or Antigravity is named
  `mcp__<server>__<tool>`, with the tool's arguments as its input and `{output}` as Claude's
  detail has them, so clients show "List hosts" with Paseo's context rather than "Other" or
  "call_mcp_tool". ACP has no field for it: the name is read from
  Cursor's `rawInput`, Gemini's title `<tool> (<server> MCP Server)` and OpenCode's `<server>_<tool>`
  (for a server the session was given), and from agy's `call_mcp_tool` parameters, where its plugin
  prefix (`burrow_`) is dropped.
- **pi's extension** is `<home>/pi/burrow-mcp-servers.mjs`, written by Burrow; it holds no token.
  The servers come in `BURROW_PI_MCP_SERVERS`, which it removes, so the commands pi runs do not
  inherit it. pi reaches the tools through its `codemode` tool, as for any server it is given
  without an `exposure`.
- **Loopback only:** the server listens on `127.0.0.1` on a port of its own, whatever `--listen`
  says, and rmcp checks `Host`. Each agent's entry carries a bearer token of its own, and the
  token is what names the caller, so one agent cannot act as another; Paseo serves it on its main
  port and takes the caller from a `?callerAgentId=` query anyone can write. A request without a
  token is a top-level client; one with a token Burrow did not issue is refused.
- **Served (32 of Paseo's 40):** `list_hosts` (this host only), `create_workspace` (local, or a
  worktree branching off or checking out a branch), `list_workspaces`, `archive_workspace`,
  `rename_workspace`, `create_agent`, `send_agent_prompt`, `get_agent_status`,
  `get_agent_activity`, `list_agents`, `cancel_agent`, `archive_agent`, `kill_agent`,
  `update_agent`, `set_agent_mode`, `list_pending_permissions`, `respond_to_permission`, the
  schedule and heartbeat tools, `list_providers`, `list_models`, `list_profiles` (always empty)
  and `inspect_provider`. Results carry `structuredContent` and Paseo's model-visible text (each
  list's `_count` and `_ids`, then the JSON); errors are their message, as a thrown error is.
- **Scoped to the calling agent,** as Paseo's are: what it makes is its child in its workspace,
  paths resolve against its directory, `list_agents` defaults to its directory, heartbeats are its
  own, and `send_agent_prompt` is a message from it (steering a running turn), answered with a
  receipt.
- **Finish notices:** `create_agent` and `send_agent_prompt` with `notifyOnFinish` (the agent
  default) tell the caller when the other agent finishes, errors, needs permission (once per
  request) or closes, in Paseo's `agent-notification` envelope and wording; kept in
  `finish-subscriptions.json` across restarts.
- **Not served:** the terminal tools, the workspace script tools and `speak` (Burrow has no
  terminals, scripts or voice), checking out a pull request in `create_workspace`, provider
  features in `create_agent` and `update_agent` (refused with a message), and `serverId` for
  another host (Burrow has no connected hosts). Paseo's two-second batching of an agent's messages
  is not done: a message is delivered at once.

## Schedules

`schedule/create`, `list`, `inspect`, `logs`, `pause`, `resume`, `delete`, `run-once` and `update`,
as Paseo's `schedule/` service has them (`paseo schedule …`; `paseo heartbeat …` is the same
requests with the calling agent as the target). Checked with Paseo's CLI 0.11.2 against a
throwaway Burrow: create with `--every` and `--run-now`, `ls`, `inspect`, `logs`, `run-once`,
`pause`, `resume`, `delete`, and `heartbeat create` from inside an agent.

- **Cadence:** `every` N ms, or a five-field cron (`*`, lists, ranges, steps; no names) matched
  minute by minute in its IANA time zone, UTC without one. `every` runs at once unless
  `runOnCreate` is false; a cron waits for its first slot unless it is true. A cron updated
  without a time zone keeps the one it had.
- **Runs:** every second, an active schedule that is due claims its slot and moves `nextRunAt` to
  the first slot after now, so each slot runs once and a long run queues none behind it; one run
  at a time per schedule. A slot missed while Burrow was down is skipped. A run under way when
  Burrow stops is failed at the next start ("Daemon restarted before the scheduled run
  completed").
- **An agent target** gets the prompt in a bare `<paseo-system>` envelope headed `Schedule "name"
  fired (id=…, run=…).`; its timeline shows a `heartbeat` call (`callId` `paseo-system:<sha256>`,
  sparkles icon), not a user prompt, and it does not title the agent. A busy agent fails the run;
  a deleted or archived one completes the schedule, as archiving it later does. The run waits for
  the turn: a permission request or an error fails it; its output is the last reply.
- **A new-agent target** gets a new workspace for its `cwd` (a new worktree with `isolation:
  "worktree"`), an agent labelled `paseo.schedule-id` and `paseo.schedule-run` with the prompt as
  its first message, no attention notification, and its workspace archived after the run unless
  `archiveOnFinish` is false. A `cwd` that is gone completes the schedule.
- `maxRuns` and `expiresAt` complete a schedule; `run-once` runs outside the cadence, leaves it
  alone, and answers when the run is over. Errors are `rpc_error` with code
  `schedule_request_failed` and Paseo's messages.
- **Kept** in `<home>/schedules/<id>.json` in Paseo's format; `import-paseo` copies Paseo's
  `~/.paseo/schedules/` as they are, so a heartbeat carries on with its agent, paused or not.
- Paseo's schedule and heartbeat MCP tools are served ([above](#agent-tools-mcp)), with the
  `createOrReplace` only they use.

## Paseo's app against Burrow

Paseo 0.13.2's web build (in its server package, `dist/server/web-ui`), served from a local
folder and connected to a throwaway Burrow by **Direct connection**. Burrow was started with
`--allow-origin` naming the page's origin, since a page's WebSocket sends one.

| What | Works | Note |
|---|---|---|
| Direct connection: host and port, no password | yes | |
| Sidebar: projects and workspaces | yes | |
| Sort by **Created** and **Last prompt** | yes | Needs `workspaceSortTimes`, which every host must have, or the app turns these sorts off everywhere |
| History, Search | yes | |
| An agent's conversation, live replies | yes | |
| Send a message | yes | |
| **New workspace** with a first message; the tool, model, thinking and mode pickers | yes | Needs `providersSnapshot`. Without it the pickers stayed on "Loading..." |
| A permission prompt, **Accept** | yes | |
| A workspace's **Setup** panel | yes | Needs `workspaceSetupObservation` |
| Terminals | no | Burrow has no terminals; the app logs the refused subscription |
| Changes (diffs) and review, file editing | no | Not served |
| Schedules | served, not walked through | Burrow answers every `schedule/*` request ([below](#schedules)); the app's page was not tried against it |
| Voice | no | |
| Hosts management, connected hosts | no | |
| Pairing link, relay | no | Direct connection only: Burrow has neither a relay nor passwords |
| Import a session | not tried | |

This walkthrough found and fixed `providersSnapshot`. Burrow answered the request, but did not
advertise the flag the app waits for.

## What changed in Paseo 0.12.0 → 0.13.2 → 0.14.0-beta.2

Read from `git diff v0.12.0 v0.13.2` and `git diff v0.13.2 v0.14.0-beta.2` of Paseo's
`packages/protocol`, `client`, `cli`, `app` and `server`, and from 0.14's changelog.
`protocolVersion` is still 1.

| Change | Who needs it | Burrow |
|---|---|---|
| `send_agent_message_request.sessionMode` | CLI, app | Done: the mode is set before the prompt |
| `workspaceSortTimes`: `createdAt`, `lastUserMessageAt` on workspaces | App sidebar | Done |
| `workspaceSetupObservation`: `workspace.setup.subscribe.request`, `setupStatus` on workspaces | App setup panel | Done |
| `connectedHosts`, `hostMultiplexing`: `hosts.*` requests, host routing | CLI `host`, app, daemon-to-daemon | Not done |
| `agentMessageBatching`, `sourceAgentServerId` | Daemon-to-daemon only | Not done; the CLI never sets them |
| `deliveryStatus: "queued"` on a send's reply | CLI prints "queued" | Not sent; the CLI prints "sent" |
| `checkoutDiffAll` ("All changes" in review) | App | Not done; Burrow serves no diffs |
| `fileByteWrites` | App file editor | Not done; Burrow edits no files |
| Hello `sessionPermissions`, `routed`, `producer` | Connected hosts | Ignored |
| 0.14: `wholeTurnTimeline`, `wholeTurns` on `fetch_agent_timeline_request` | App: folding finished turns (its client asks only a host that advertises it) | Done: a tail or older page starts at a turn's first row, and an older page runs to the end of its last turn. A turn is rows with one turn id, or without ids, a prompt, an agent message or a heartbeat and what follows |
| 0.14: idle runtimes released after an hour (#7782) | Hosts with many agents | Done: an idle agent with a session to resume, no subagent busy and no ACP terminal running has its CLI stopped after an hour; its next prompt resumes the session. `idleRuntimeCleanupSetting` (the on/off switch in the app) is not advertised: Burrow has no daemon config requests |
| 0.14: Claude launched as `CLAUDE_CODE_ENTRYPOINT=Paseo`, not `sdk-ts` (#7976) | Subscription limits | Done as `Burrow`, after clearing a parent Claude session's variables, as Paseo does |
| 0.14: `speechSettings`, `speech.local_models.*` | App voice settings | Not done; Burrow has no voice |
| 0.14: `terminalsSurviveRestart`, `killTerminals`/`keepTerminals`, `terminalCleanupUnconfirmed` | App terminals | Not done; Burrow has no terminals |
| 0.14: `cachedClientBundles`, plugin tools and MCP servers | Plugins | Not done; Burrow has no plugins |
| 0.14: `envOverrides`, `features` in daemon config | App settings | Not done |
| After 0.14.0-beta.2 (Paseo's `main`): `resumeOnStartupSetting`, `resumePrompt` | Agents running when the daemon stopped | Not done yet; waits for a release |

No timeline item types or agent snapshot fields changed in that range.

## crusteo and Reef against Paseo's daemon

crusteo's desktop and Reef read Paseo daemons of 0.11.0, 0.12.0 and 0.13.2 every day, alongside
Burrows. They use the feature flags each host advertises (`directorySync`,
`backgroundWorkspaces`, `providersSnapshot`, `workspaceLabels`, ...) and send nothing a host
does not advertise. Nothing in 0.13 breaks them. Two small gaps:
- a prompt carrying several agent messages batched in one envelope shows as raw text, where it
  reaches crusteo unprojected;
- an agent message's `sender.serverId` (a sender on another host) is ignored, so the sender's
  title is looked up on the wrong host.

## Checking a new Paseo release

1. Read the diff of `packages/protocol/src/messages.ts` and of the `features` that
   `websocket-server.ts` advertises, and search `packages/client` and `packages/app` for new
   `features?.` gates and "Update the host" messages.
2. Run `scripts/paseo-cli-compat.sh` with the new CLI.
3. Serve the new web build against a throwaway Burrow and walk through the table above.
4. Update this page: the tables above, and the sections below where behaviour changed.

## Which version's behaviour it follows

Where versions differ, Burrow follows the newest, unless the change would break an older
client.

| Behaviour | 0.11 | 0.12 | Burrow |
|---|---|---|---|
| A create whose first prompt fails | The create fails | The agent is created; the failure shows on its timeline | 0.12 |
| An `allow` answer to a question with no usable answers | Accepted | Refused (`question_answer_required`); the question stays pending | 0.12 |
| `internal: true` on create | Accepted | Refused | 0.12 (no client sends it) |
| `background` on create together with `workspaceId` | Accepted | Refused | 0.12 |
| Claude's `--version` probe | Any output | Only `x.y.z (Claude Code)` | 0.12 |
| Background workspaces (`features.backgroundWorkspaces`) | Absent | Present | 0.12 ([below](#background-workspaces)) |
| Agent-message provenance (`features.agentMessageProvenance`) | Absent | Present | 0.12: the prompt reaches the provider in Paseo's `<paseo-system kind="agent-message">` envelope and is recorded as an `agent_message` call from the sender |

## What changed from 0.11.2 to 0.12.0

These are read from `git diff v0.11.2 v0.12.0` of Paseo's `packages/protocol` and
`packages/server`.

- **Protocol.** `WS_PROTOCOL_VERSION` is still 1, and no message type was added or removed. The
  additions:
  - `tool_call.agentMessage`, for agent-to-agent messages, shown as a synthetic `agent_message`
    tool call;
  - `snapshot.internal`;
  - `includeBackground` on directory reads (a sequenced read with it uses the generation
    `<generation>:background`);
  - `sourceAgentId` on sends;
  - `background` on creates and workspace descriptors;
  - usage-report `lastSuccessful`;
  - the feature flags `backgroundWorkspaces` and `agentMessageProvenance` (and
    `hopliteProvider`, from 0.11.4).
- **Agent path.** The two rows marked 0.12 in the table above. Timeline storage, turn-id rules
  and coalescing did not change. On the Claude side, settings changes no longer restart an
  active turn, and the context meter reads `modelUsage[activeModel]`.
- **0.11.4 over 0.11.2.** Only the Hoplite provider, opt-in through configuration, and an
  ACP catalog change. The Agent SDK stays at 0.3.246, and the agent path does not change.

## Background workspaces

Paseo 0.12 can make a workspace a background one (`background` on `workspace.create.request`, or
on `agent.create.request` when it makes a new workspace). A 0.12 daemon then leaves that
workspace, and every agent in it, out of `fetch_workspaces_request`, `fetch_agents_request`,
directory sync, `emptyProjects` and the `workspace_update` and `agent_update` pushes, unless the
client sends `includeBackground: true`. The flag goes at the top level of the request or in
`filter`; a sequenced read refuses any `filter`, so it must be at the top level there. A read or
subscription that includes background workspaces is sequenced in the generation
`<generation>:background`, so a cursor from a read without them restarts as a snapshot.
Descriptors carry `background`, and `server_info.features.backgroundWorkspaces` says the daemon
does all this.

What Crusteo and `reef` send:
- **To a daemon that advertises `backgroundWorkspaces`** (Paseo 0.12, and Burrow): every
  workspace and agent read, first pages, later pages and sequenced reads alike, carries a top-level
  `includeBackground: true`, which also reaches the subscription it opens. They therefore hold
  every background workspace and agent, and filter them on the client by the descriptor's
  `background`, so hiding or showing them needs no new request.
- **To Paseo 0.10 and 0.11:** nothing new. Their directory request schemas are plain zod objects,
  which drop unknown keys (the one `.strict()` object in `messages.ts`, a skills selection, is
  not in these reads), so the flag would be harmless, but those daemons have no background
  workspaces to hide and are not sent it.

Burrow follows 0.12, read from its `session.ts`, `agent/directory.ts`,
`directory-sync/index.ts`, `workspace-directory.ts` and `workspace-provisioning-service.ts`:
- **Advertised:** `backgroundWorkspaces`.
- **Making one:** `workspace.create.request` with `background: true` makes a new background
  workspace, for a folder or a new worktree ([below](#projects-and-workspaces)). Without
  `background`, a workspace made with `callerAgentId` is background when the caller's workspace
  is. `agent.create.request` with `background: true` and no `workspaceId` always makes a new
  background workspace for the agent's `cwd`. A plain create's lookup by directory skips
  background workspaces, as Paseo's does.
- **Refused, as in 0.12:** `background` together with `workspaceId` on `agent.create.request`, or
  together with `callerAgentId` when no worktree is made; `internal: true` on
  `workspace.create.request`; `background` on its initial `agent`.
- **Saved:** `background` is kept on the workspace record in `workspaces.json` (left out when
  false) and comes back after a restart. `burrow import-paseo` carries it over from Paseo's registry.
- **Reads and pushes:** without `includeBackground` (top level or `filter`), workspace and agent
  reads leave background workspaces and their agents out, and a project with only background
  workspaces is in `emptyProjects`. With it, sequenced reads and the subscription's pushes use
  `<generation>:background`. Each subscription remembers which it asked for: an agent in a
  background workspace never reaches one that did not ask, and a background workspace's
  `workspace_update` neither.
- **Descriptors** always carry `background`, `false` or `true`, as Paseo's do.

Differences:
- `workspace_update` removals go to every workspace subscription, background or not; a client
  ignores the id of a workspace it never saw.

## Projects and workspaces

Adding projects and making workspaces, git worktrees included, follows Paseo 0.12.0's
`session.ts` handlers and the services behind them: `workspace-provisioning-service.ts`,
`worktree-session.ts`, `paseo-worktree-service.ts`, `worktree-core.ts`,
`resolve-worktree-creation-intent.ts`, `utils/worktree.ts`, `utils/worktree-metadata.ts`,
`utils/directory-suggestions.ts`, `project-key.ts` and `workspace-archive-service.ts`. It was
read from those sources and tested against real scratch repositories
(`burrow-daemon/tests/workspaces.rs`); no Paseo daemon was run for comparison.

Advertised: `projectAdd`, `stableProjectIdentity` (Paseo's app adds projects only with both),
`projectCreateDirectory`, `projectRemove` and `workspaceMultiplicity`.

| Request | Reply | What it does |
|---|---|---|
| `project.add.request {cwd}` | `project.add.response {project, error, errorCode?}` | Registers a folder, git or not, and makes no workspace |
| `project.create_directory.request {parentPath, name}` | `.response {directoryPath, project, error, errorCode}` | Makes a folder and registers it |
| `open_project_request {cwd}` | `open_project_response {workspace, error, errorCode?}` | The folder's own workspace, made with its project if missing |
| `directory_suggestions_request` | `directory_suggestions_response {directories, entries, error}` | The folder picker's search |
| `workspace.create.request` | `workspace.create.response {workspace, agent?, creation, error, errorCode?, setupTerminalId}` | A new workspace for a folder or a new worktree, with an initial agent if asked |
| `agent.create.request`, `create_agent_request` with `git`, `worktree` or `worktreeName` | as without them | An agent in a new worktree, or on a branch made or switched to in place |
| `branch_suggestions_request {cwd, query?, limit?}` | `.response {branches, branchDetails, error}` | The starting branches |
| `validate_branch_request {cwd, branchName}` | `.response {exists, resolvedRef, isRemote, error}` | Whether a branch exists here or on `origin` |
| `paseo_worktree_list_request {cwd? \| repoRoot?}` | `.response {worktrees, error}` | Burrow's worktrees of a repository |
| `workspace_setup_status_request {workspaceId}` | `.response {workspaceId, snapshot}` | The last setup snapshot |
| `archive_workspace_request {workspaceId}` | `.response {workspaceId, archivedAt, error}` | Archives a workspace and removes its worktree |
| `project.remove.request {projectId}` | `.response {projectId, accepted, removedWorkspaceIds, error}` | Archives the project's open workspaces, then forgets the project |

**Projects.**
- A project's root is the folder as given, `~` expanded and resolved; it is not moved to the git
  top level. Its kind is `git` or `non_git`, its name the folder's.
- A folder that is already a project answers with it, its kind and key brought up to date. The
  push is `project.update` to event subscribers. Neither Paseo nor Burrow runs `git init`.
- A missing folder answers `Directory not found: <path>` with `errorCode: "directory_not_found"`.
  `project.create_directory` refuses with Paseo's codes and messages: `invalid_name`,
  `parent_directory_not_found`, `directory_exists`, `permission_denied`, `filesystem_error`. A
  folder made but not registered is removed again.
- **Keys** are Paseo's `deriveProjectKey`: `remote:<host>[:<port>]/<path>` for a checkout whose
  `origin` parses (the path lowercased on GitHub), with `#subdir:<path>` for a project inside a
  checkout; else `host:<serverId>:<real path>`. Clients merge projects by this key across hosts.
  New projects get it, and a project Burrow made before gets it the next time it is used to
  place a workspace or is added again. Agents placed by their directory use the same rule.

**`workspace.create.request`** never reuses a workspace.
- **`directory` source** (`path`, `projectId?`): a missing folder fails with `directory_not_found`.
  With `projectId` the workspace joins that project (`Unknown project: <id>` otherwise), else
  the project rooted at the folder, made if missing. A git checkout's workspace is
  `local_checkout`, or `worktree` when it is a linked worktree, and is named after its branch.
- **`worktree` source** (`cwd` or `projectId`, `action`, `refName`, `baseBranch`, `branchName`,
  `worktreeSlug`): neither `cwd` nor `projectId` fails with `source_required`. The worktree is
  cut from the checkout `cwd` is in (from its main checkout when `cwd` is in one of Burrow's
  own worktrees):
  - `action: "checkout"` checks out `refName`, fetching it from `origin` when only `origin` has
    it (`Unknown branch: <name>` otherwise). A branch already checked out elsewhere gets a new
    branch off it, `<name>-1`. Without `refName`: `action "checkout" requires refName or
    checkoutSource`.
  - Otherwise (`branch-off`, the default) a new branch is made off `refName`, else `baseBranch`,
    else the default branch (`origin/HEAD`, else `main`, else `master`). The base resolves as
    `refs/heads/<name>`, then `refs/remotes/origin/<name>`, then as given; a remote-tracking
    base is fetched first, for at most 15 s. The branch is `branchName`, else the slug. A slug
    not given is `slugify(branchName)`, else a random two-word name (`brisk-otter`; Paseo's
    words differ). A branch name that is taken gives a new branch named after the slug, off the
    taken one, as in Paseo. Names that clash with another branch's path get `-N`.
  - The workspace's directory is the same folder inside the new worktree as `cwd` is inside its
    checkout. Its project is `projectId`'s, else that of a workspace at `cwd` or at the
    repository, else the repository's own.
  - The workspace is `worktree`, named after its branch, with `worktreeSlug` (the worktree's folder
    name), `gitRuntime.isPaseoOwnedWorktree: true`, and `baseBranch` (the exact base ref, such as
    `refs/heads/main`) saved on its record.
  - A worktree whose workspace could not be made is removed again.
- **`title`** names the workspace; without it, the first line of the initial agent's prompt (or of
  `firstAgentContext.prompt`), cut at 60 characters.
- **`agent`** (`WorkspaceInitialAgentSchema`) is made in the new workspace once it is ready. Its
  `config.cwd` must be inside the source (`Agent directory must be inside the workspace source`
  otherwise); it then works in the workspace's directory, as Paseo 0.12 places agents made for a
  workspace. Its `agentId` is kept when given. A failed agent leaves the workspace.
- **`workspaceId`** is used as the new workspace's id when given; one that exists fails with
  `workspace_id_conflict`.
- **`creation`** goes through Paseo's phases, each bumping `revision`: `accepted` (0),
  `workspace_ready`, then with an agent `agent_ready` and `prompt_started` (when its first prompt
  went out), then `completed`. A failure is `phase: "failed"` with `failedStage` (`workspace` or
  `agent`) and `outcomeUnknown` (false only for `directory_not_found` and `source_required`, as in
  Paseo), and the reply's `error`, `errorCode` and, after `workspace_ready`, `workspace`. With
  `subscribe: true` each phase is also pushed to the requesting socket as `workspace.create.update`.
- **Pushes:** `workspace_update` and `project.update` as the records change, and
  `workspace_setup_progress` (event subscription) for a worktree's setup.
- **`idempotencyKey`:** a retry with the same request gets the same reply, with its own
  `requestId`; a different request under the same key gets `workspace_request_key_conflict`.

**Where worktrees go**, as in Paseo with Burrow's home in place of `~/.paseo`:
`<home>/worktrees/<project hash>/<slug>`, or `--worktrees-root` (`BURROW_WORKTREES_ROOT`) in
place of `<home>/worktrees`. The hash is Paseo's: eight base-36 digits of a SHA-256 of the main
checkout's real path. A taken folder gets `-1`, `-2`, ... Each worktree gets Paseo's metadata
file, `<its git dir>/paseo/worktree.json` (`baseRefName`, `baseRef`,
`changeRequestLookupTarget`), and the source's `paseo.json` when git did not bring one.

**Setup scripts.** `paseo.json`'s `worktree.setup` (a command or a list) runs in the new
workspace's directory once it is made, through `bash -c` (PowerShell on Windows), with Paseo's
variables `PASEO_SOURCE_CHECKOUT_PATH`, `PASEO_ROOT_PATH`, `PASEO_WORKTREE_PATH`,
`PASEO_BRANCH_NAME` and `PASEO_WORKTREE_PORT` (a free port, kept in the metadata file). The first
command that fails stops the rest.
- For `workspace.create` and `agent.create`'s `worktree` target, progress goes to
  `workspace_setup_progress` subscribers as `{workspaceId, status, detail, error}`: `running`, then
  `completed` or `failed`, with Paseo's `worktree_setup` detail and log
  (`==> [1/2] Running: ...`, `<== [1/2] Exit 0 in 1.20s`). The last snapshot answers
  `workspace_setup_status_request`.
- For `agent.create`'s `git` options and `worktreeName`, as in Paseo, the outcome lands on the
  agent's timeline as one `paseo_worktree_setup` tool call, also when there are no commands.

**`agent.create.request` with git options** (`normalizeAgentCreationGitOptions`):
- `git: {createWorktree, createNewBranch, newBranchName, baseBranch, worktreeSlug, refName,
  action}`, the legacy `worktreeName` (a new branch and slug of that name), or
  `worktree: {mode: "branch-off", newBranch, base?} | {mode: "checkout-branch", branch}`.
  `worktree` and `git` together are refused. Names are slugified and validated with Paseo's
  messages (`Invalid branch name: ...`, `Invalid worktree name: ...`).
- With `createWorktree`, the agent is made in a new worktree workspace, as above, titled after its
  prompt; if the agent then cannot start, the workspace is archived and its worktree removed.
- Without it, `createNewBranch` makes the branch off `baseBranch` (else the default branch) in the
  checkout itself, and `baseBranch` alone switches to it; both refuse a checkout with uncommitted
  changes (`Working directory has uncommitted changes. Commit or stash before switching
  branches.`).
- `callerAgentId` names the asking agent (`Caller agent <id> not found` otherwise): without a
  workspace the new agent joins the caller's workspace and directory, labelled
  `paseo.parent-agent-id`.

**The folder search** (`directory_suggestions_request`) is Paseo's ported, ranking included. Without
`cwd` it searches the user's home and answers absolute paths; a query names a place when it
starts with `~`, `./` or is absolute inside the home, and an empty query answers nothing. With
`cwd` it searches that folder and answers relative paths, skipping gitignored entries and
walking a few dot folders (`.github`, `.claude`, ...); an empty query lists the top level. Both
skip `node_modules`, `target`, `build` and Paseo's other output folders. 0.12's two fixes are kept:
- **No hang in `~/Library`.** On macOS a home search never walks into `~/Library`, whose
  protected folders block a read forever for a daemon that cannot ask for consent; typing a path
  inside it still lists it.
- **No memory blow-up when searches overlap.** The walk goes round-robin a level at a time and
  lists a folder only when its turn comes, so it holds at most what the 20,000-entry scan budget
  reaches (a home search also stops after 5,000 entries once it has a good match). Every listing
  and `realpath` goes through two shared slots, and searches that need the same folder share one
  pending read, so a stuck mount holds one thread, not one per keystroke.

**Archiving** (`archive_workspace_request`): the workspace's agents are archived, then the
workspace; it leaves the directory with a `workspace_update` removal (with `emptyProject` when its
project has no workspace left) and is kept, archived, in `workspaces.json`. A worktree Burrow
made is then removed, once no other workspace uses it: `paseo.json`'s `worktree.teardown` runs
first, then `git worktree remove --force`, the folder (retried), and `git worktree prune`. The
branch stays. A teardown or removal that fails leaves the folder and logs why; the workspace stays
archived. Archiving again answers the first `archivedAt`; an unknown id answers `Workspace not
found: <id>`.

**Removing a project** (`project.remove.request`) follows `handleProjectRemoveRequest`, which is
the same in Paseo 0.10.0, 0.11.0 and 0.12.0 (the request dates from 0.1.97, behind `projectRemove`):
- **Open workspaces are archived first, not refused.** Each of the project's workspaces still in the
  directory has its agents archived (a running one is stopped), then is archived itself and kept in
  `workspaces.json` with its `projectId`. Workspaces archived before stay as they are.
- **Nothing on disk changes.** Unlike `archive_workspace_request`, no worktree is removed and no
  `worktree.teardown` runs: worktrees, branches and files stay, as Paseo's app promises ("Files on
  disk will not be changed").
- **The project is forgotten** and `projects.json` saved without it. Adding the folder again makes
  a new project, with a new id.
- **Reply:** `{requestId, projectId, accepted: true, removedWorkspaceIds, error: null}`, the ids of
  the workspaces it archived. An unknown id is accepted too, with nothing removed, as in Paseo; a
  request without `projectId` does not validate (`unknown_schema`).
- **Pushes,** before the reply: `workspace_update {kind: "remove", id, removedProjectId}` for each
  workspace archived (sequenced on synced subscriptions, with no `emptyProject`); with none, one
  unsequenced removal naming the project's first archived workspace, else the project's own id, so
  clients drop an empty project; then `project.update {kind: "remove", projectId}` to event
  subscribers, when the project existed. Agent removals and workspace updates come first, as the
  agents are archived.

Differences:
- **Removing a project** does not first push the workspaces with `archivingAt`, and Paseo's failure
  path (`accepted: false` with an `activity_log` error) cannot happen, since Burrow's writes do
  not fail the request. "First archived workspace" is the lowest id, where Paseo takes its
  registry's first.
- **Change requests are not checked out.** `checkoutSource`, `githubPrNumber` and
  `worktree: {mode: "checkout-pr"}` are refused with "Checking out a change request needs a forge,
  which this daemon does not offer", and `forge.search.request` and `github_search_request` are
  not served. Paseo resolves a pull or merge request through its forge services: the remote's
  forge, then `gh`, `glab` or `tea` with GraphQL queries, cross-repository push remotes, auth
  states and caches, some 7,000 lines. That is more than shelling out, so it waits.
- **No first-agent naming.** Paseo asks a model for a title and branch name after the first prompt
  and renames the placeholder branch. Burrow makes no model calls: the branch keeps its
  name, and the title is the prompt's first line.
- **Setup reports less often:** when each command starts and ends, not per output chunk. On an
  agent's timeline only the final tool call is written (Paseo also streams a live running one).
  `worktree.terminals` are not started (Burrow has no terminals), and service scripts do not
  apply.
- The first push of a workspace made with an initial agent is not marked `running` ahead of the
  agent, as Paseo's optimistic status does; the agent's own push follows at once.
- **Idempotency receipts live in memory**, so a retry after a restart makes a new workspace.
  `workspaceRequestReceipts` is therefore not advertised, and `creation.subscribe.request` is not
  served.
- **Archiving** does not first push the workspace with `archivingAt`. `paseo_worktree_archive_request`
  (archive by path or by worktree) and workspace recovery (`workspace.recovery.*`) are not served.
- The folder search keeps no listing cache between keystrokes, and compares paths exactly, where
  Paseo folds case on Windows.
- An `agent.create.request` with a `workspaceId` keeps its own `cwd`; Paseo 0.12 moves the agent
  to the workspace's directory. Without a `workspaceId` Burrow still reuses the directory's
  workspace, where Paseo 0.12 makes a new one.
- A failed `agent.create.request` answers without a `creation` snapshot, as before.

## Differential testing

The same scenario ran through Burrow and through throwaway Paseo 0.12.0 and 0.11.4 daemons,
each with its own scratch `PASEO_HOME` on a spare loopback port. The scenario: create a Claude
agent on Haiku 4.5, a plain turn, a Bash call that needs permission, a question, an interrupted
turn, then a daemon restart and a resumed turn. The driver is `crates/burrow-daemon/examples/smoke.rs`,
which uses Crusteo's `paseo-client` and `fleet`. It logs every frame; the logs were compared
message type by message type, as JSON key paths, and as push order per turn.

What came out:
- **Paseo 0.12.0 and 0.11.4 sent identical shapes** for this scenario.
- **The conversation was the same on all three**: the same replies, the permission and the
  question answered the same way, the same interrupt.
- **After a restart, Paseo rebuilt the timeline under a new epoch**, with renumbered seqs
  (16 rows became 14), which invalidates every client's cursor. Burrow kept the epoch and
  carried on from seq 17. Both resumed the Claude session, and the model remembered the earlier
  turns.
- **Push order matched.** Differences that remain:
  - Paseo sends more `agent_update` upserts during a turn; Burrow sends fewer.
  - Burrow's `send_agent_message_response` arrives right after the user-message row; in
    Paseo, one more `agent_update` usually comes first.
- **Shapes Burrow lacked, now fixed:**
  - git placement for a workspace in a checkout (`isGit`, branch, worktree root, `gitRuntime`);
  - `thinkingOptionId` and `effectiveThinkingOptionId` sent as `null`;
  - `runtimeInfo` from creation, with `extra.runtimeModel`;
  - `persistence.metadata`;
  - `fetchedAt` as a map per provider;
  - an unrecognised tool's result wrapped as `{"output": ...}`;
  - a permission reply that follows its resolution events.
- **Left as they are, on purpose:**
  - `capabilities.supportsRewind*` and `supportsSessionListing` are not sent, because Burrow
    has neither feature.
  - `lastUsage` is present, where Paseo omitted it in this run.

## Codex

The Codex adapter (`crates/burrow-provider-codex`) follows Paseo 0.12's
`codex-app-server-agent.ts` and its `codex/` mappers, which are unchanged at Paseo's newer HEAD. It
was verified against codex-cli 0.162.1 (see [README.md](README.md#verified-against-a-real-model))
and against Paseo's sources, not diffed against a running Paseo daemon.

What clients get, as Paseo sends it:
- **Provider entry:** id `codex`, Paseo's label, description and mode list with icons and colour
  tiers (`auto`, `auto-review`, `full-access`; `auto-review` and the default only from Codex
  0.115). Models are Codex's `model/list`, with the configured default model and effort applied,
  labels such as `GPT-6.1-Sol`, thinking options with their descriptions, `metadata`, and no
  `isSelectable`.
- **Turns:** `codex-turn-N` turn ids; `turn_canceled` with reason `interrupted`; a failed turn's
  error is Codex's message.
- **Timeline items:** streamed assistant text keyed by Codex's item id, only the missing suffix on
  completion, and Paseo's `---` rule before a turn's second message; reasoning summaries; `shell`
  calls with the shell wrapper removed; `apply_patch` calls as `edit` details with paths relative
  to the workspace; MCP calls named `server.tool`; `web_search`; plans; `todo` items from plan
  updates; compaction rows; `request_user_input` question rows; a failed row for a denied command.
- **Permissions:** `CodexBash`, `CodexFileChange` and `CodexMcpElicitation` requests of kind
  `tool`, and `request_user_input` of kind `question`, in Paseo's shapes. Answers become Codex's
  `accept`, `decline` or (with `interrupt`) `cancel`; question answers are keyed by header.
- **Agent state:** `runtimeInfo` is Codex's (resolved model, `thinkingOptionId`, `modeId`,
  `extra.collaborationMode`), which also sets `effectiveThinkingOptionId`; `lastUsage` follows
  `thread/tokenUsage/updated` during a turn, so the context meter moves while Codex works.

For this the provider interface gained, as default methods and new event variants that other
providers need not use: `Provider::foreground_turn_id`, `Provider::model_fields`,
`ProviderEvent::UsageUpdated` and `ProviderEvent::RuntimeInfo`.

Differences that remain:
- **Turn numbering.** Codex turns are numbered from 1 and continue across restarts with the
  Burrow's counter. Paseo's start at `codex-turn-0` in each process.
- **Thread start.** Paseo starts the Codex thread before `agent.create` answers; Burrow starts
  it in the background, so `thread_started` can arrive during the first turn.
- **Not offered yet:** the agent features (Speed and the Plan toggle, so no plan mode and no plan
  approvals; turns always ask for the default service tier), slash commands (`/compact`, `/goal`,
  skills, custom prompts), the "applies next turn" notices for mode and thinking changes, and
  archiving the Codex thread when an agent is archived.
- **Not mapped yet:** Codex sub-agent threads (`provider_subagent` events; their notifications are
  dropped), image items and image results of MCP calls, asynchronous questions, terminal
  interaction rows.
- **Persistence.** `persistence.metadata` has Burrow's common fields, not Paseo's Codex extras
  (`threadId`, `thinkingOptionId`, `systemPrompt`, ...).
- **Permission ids** are Burrow's `permission-<uuid>`, as for Claude; Paseo uses
  `permission-<itemId>`. The failed row of a denied command uses Paseo's form.
- **Resume** asks Codex for the thread without its turns (`excludeTurns`), since Burrow keeps
  its own timeline. Paseo reads the full thread back, which Codex now marks as deprecated.
- **Withdrawn approvals.** Codex's `serverRequest/resolved` withdraws any pending request; Paseo
  handles it only for MCP elicitations.

## Antigravity

The `antigravity` provider (`crates/burrow-provider-antigravity`) follows two pieces of Paseo
0.12.0:
- the bundled `antigravity-provider` plugin (`plugins/antigravity-provider/server/`);
- the bridge that turns plugin events into agent events
  (`packages/server/src/server/agent/plugin-provider.ts`).

The daemon offers it when `agy --version` prints `x.y.z`, 1.1.15 or later (`--agy-bin`,
`--no-agy`).

| | Paseo 0.12 | Burrow |
|---|---|---|
| Provider | `antigravity`, "Antigravity", "Antigravity CLI on your host" | Same |
| Launch | `agy --input-format stream-json --output-format stream-json --add-dir <cwd> --disable-slash-commands --dangerously-skip-permissions [--model <id>] [--conversation <id>]`, in its own process group | Same |
| Models | `agy models` (`id<TAB>label`), none default, no thinking options: effort is part of the id | Same. Read at startup, and again when a session starts while there are none (not signed in yet) |
| Modes | One, `full-access` (`ShieldOff`, `dangerous`, `isUnattended`) | Same, without `isUnattended`, which Burrow's mode type lacks. Any other mode is refused |
| Permissions and questions | None. The CLI cannot ask when another app drives it, so every tool runs | Same |
| Opening notice | A warning `notification` each time a session opens | Same, recorded before the first prompt (the trait's `opening_items`) |
| Text | `assistant_message` deltas, `messageId` `<turn key>:<step index>` | Same |
| Tools | `tool_call`, `callId` `<turn key>:<step index>`. Details `shell`, `read`, `write`, `edit`, `sub_agent`, `plain_text`, `unknown`. Tools still running end with the turn | Same, except that an MCP call is named `mcp__<server>__<tool>` with its arguments as input (Paseo: `call_mcp_tool`) |
| Agent tools (MCP) | None | Burrow's `paseo` server, through its plugin ([Agent tools](#agent-tools-mcp)); checked with agy 1.4.11 |
| Denials by agy's own policy | One notification per new denial set: `Shell command denied`, `Action denied` or `N actions denied` | Same |
| Interrupt | `SIGINT` to the group, `SIGTERM` after 1 s, `SIGKILL` after 2 s. `turn_canceled` with "Canceled by provider". The next prompt restarts the CLI on the conversation | Same |
| Model change | Applied at the next prompt, by restarting the CLI | Same |
| Persistence | `sessionId` `plugin:{"version":1,"data":{"conversationId":"…"}}` | The same id. `persistence.metadata` is Burrow's (provider, cwd, mode, model), not `pluginProviderPersistence` |
| Usage | `usage_updated` after each finished generation, summed per turn; `turn_completed` has no usage | The per-turn sum comes on `turn_completed.usage` and `lastUsage`. No `usage_updated` |
| Turn ids | A UUID per turn | `foreground-turn-N`, as for Claude. Clients treat turn ids as opaque |
| After a daemon restart | The timeline becomes the provider's history. agy replays nothing, so that is only the new notice | The timeline is kept, and the new notice is appended |
| Images | Saved as private files, named on `Attached image: <path>` lines | Same |
| `agy` not signed in | Opening the session fails, so the create fails | The agent is created and its first prompt fails with Paseo's sign-in message (0.12's rule for a failed first prompt) |
| A frame it cannot parse | Fails the session | Ignored |

The notice and the mode description keep Paseo's wording, "Paseo starts it with
--dangerously-skip-permissions", so they read the same in every client.

**Smoke test.** This ran in WSL on agy 1.4.2 with `gemini-3.8-flash-low`, on a scratch home and a
loopback port, with `smoke.rs agy` and then `resume`. It used eight model turns over two runs.
What it covered:
- a plain answer;
- a shell command that ran without a prompt and wrote its file, shown as a `shell` tool call;
- a refused `plan` mode, an accepted `full-access`, and a model change to
  `gemini-3.8-flash-medium`, which restarted the CLI on the same conversation;
- an interrupted turn, ending in `turn_canceled`;
- a daemon restart with the same epoch, then a turn that remembered the first answer.

No `agy` process was left running afterwards.

The two recorded transcripts are replay fixtures. No Paseo daemon was run for comparison; the
table above comes from reading Paseo's sources.

## Cursor (ACP)

Paseo has no built-in Cursor provider. Its app adds one from its ACP catalog as a custom provider:
id `cursor`, label `Cursor`, description `Cursor's coding agent`, command `cursor-agent acp`,
run through its generic ACP client (`acp-agent.ts`, `cursor-acp-agent.ts`). Burrow offers
that provider when `cursor-agent` is on PATH, with the same shapes:

- **Catalog.** At startup a throwaway session is opened (`initialize`, `session/new` in the home
  directory) and Cursor's `cursor/list_available_models` extension lists the models, each with its
  own thinking options. Modes are the session's (`agent`, `plan`, `ask` in 2026.10.01), without
  icons; there is no default mode. Until the probe ends, the providers snapshot says `loading`;
  then subscribers to `providers_snapshot_update` get the new snapshot, as Paseo pushes it when a
  probe lands (in each client's own encoding, without `requestId`).
- **Agents.** Turn ids are UUIDs and permission request ids are bare UUIDs, as in Paseo. The
  snapshot's modes, current mode and `runtimeInfo` (`model`, `thinkingOptionId`, `modeId`,
  `extra.title`, `extra.updatedAt`) come from the session. Requests name `provider: "acp"` inside,
  because Paseo renames only the outer events of its generic client.
- **Timeline.** Message and thought chunks, tool calls merged from `tool_call` and
  `tool_call_update` with Paseo's detail per ACP `kind`, plans as `todo` items, and calls still
  running when a turn is canceled shown as canceled. Cursor gives chunks no message id, so each
  message gets a UUID.
- **Settings.** The configured mode, model and thinking option are applied with
  `session/set_mode`, `session/set_model` and `session/set_config_option` before the first prompt
  goes out. Selecting a model in Cursor also changes the Cursor CLI's own default model.
- **Resume.** After a restart the agent's next use starts `cursor-agent acp` again and calls
  `session/load`. Cursor replays the conversation, which Burrow does not show again: its own
  timeline already has it.

Its MCP calls (and those of OpenCode and Gemini) are named by server and tool, where Paseo names
them `other` ([above](#agent-tools-mcp)); checked with `cursor-agent` 2026.10.22.

Verified against `cursor-agent` 2026.10.01 on GPT-5 Mini through
`crates/burrow-daemon/examples/smoke.rs`: a plain turn, a shell command allowed through a
permission prompt that really wrote its file, an interrupted turn, then a daemon restart and a
resumed turn that remembered the first answer. That session is recorded, scrubbed, in
`burrow-provider-acp/tests/fixtures/real_cursor_*.jsonl`. No Paseo daemon was run for
comparison; the shapes come from Paseo's sources.

For this the provider interface gained `ProviderEvent::ModesAvailable` and
`Provider::loading`, and uses the `foreground_turn_id`, `RuntimeInfo` and `UsageUpdated` that Codex
added. A permission request may keep the id its adapter chose.

Where Burrow differs from Paseo on purpose:
- A session Cursor can no longer load is replaced by a new one, where Paseo fails to load the
  agent. A mode, model or thinking option the session refuses is logged and skipped, where Paseo
  fails the create.
- `runtimeInfo.provider` is always `cursor`. Paseo sends `acp` in the ones its `model_changed`
  events carry, and `cursor` after its next refresh.
- Tool calls replayed by `session/load` are not kept, so a later cancel never marks them canceled.

Not yet: the composer features Paseo lists for ACP agents (`auto_accept`, and Cursor's `fast`),
and slash commands (`available_commands_update`). Sessions get Burrow's `paseo` MCP server
([above](#agent-tools-mcp)), where Paseo's get none.

## OpenCode and Gemini (ACP)

Two more ACP agents, through the same client as Cursor and offered when their CLI is on PATH
(`--opencode-bin`, `--gemini-bin`; `--no-opencode`, `--no-gemini`):
- **OpenCode**, `opencode acp`: id `opencode`, label `OpenCode`, Paseo's provider id. Paseo drives
  OpenCode through `opencode serve` and its HTTP API instead, so the timeline, models and
  resume are not Paseo's; an OpenCode agent made under Paseo does not resume under Burrow.
- **Gemini**, `gemini --acp`: id `gemini`, label `Gemini`. Paseo has no Gemini provider; there it
  is a custom ACP provider with the same command.
- Models and modes come from the probe session's answer (`models`, else its `model` option),
  with no default mode. Checked with OpenCode 1.18.33 (261 models, 2 modes) and Gemini CLI
  0.64.0 (4 modes), each answering a turn that called Burrow's MCP tools.

## Deleting agents

`delete_agent_request` follows Paseo 0.12's `handleDeleteAgentRequest`, read from its sources:
- **What goes:** the agent, active or archived, by its exact id: its record and its timeline file.
  A running session is closed, as Paseo's `closeAgent` closes it, not interrupted, so no
  `turn_canceled` or attention is sent for it. The provider's own session files (Claude's
  transcript, Codex's thread) are left, as in Paseo. The workspace stays and is described again
  without the agent, even when it was the last one.
- **The reply** is `agent_deleted {agentId, requestId}`, to the requesting socket only. An id the
  Burrow does not know gets the same reply, as in Paseo; there is no error.
- **Pushes:** `agent_update {kind: "remove", agentId}` (with `generation` and `seq` on synced
  subscriptions, and a tombstone for sync reads), then `workspace_update` for its workspace.

Differences:
- Burrow pushes the removal to every agent subscription that showed the agent, on every
  socket. Paseo sends it only to the deleting session's subscriptions; other sessions get an
  upsert with the closed snapshot and see the deletion at their next directory resync.
- Paseo also pushes a removal, without `seq`, for an id it does not know; Burrow pushes
  nothing then.

## Workspace labels

The label requests follow Paseo 0.12's `workspace-labels/` service and its protocol schemas,
unchanged since 0.11.2. `server_info.features.workspaceLabels` is advertised.

| Request | Reply | What it does |
|---|---|---|
| `workspace.label.list.request` (`subscribe`, `sync`) | `.list.response {labels, sync, subscriptionId?}` | The catalog, or with a cursor only the changes since it |
| `workspace.label.assignment.set.request {workspaceId, label: {name, color}, assigned}` | `.assignment.set.response {label, workspaceLabels}` | Puts a label on a workspace or takes it off |
| `workspace.label.update.request {name, newName?, color?}` | `.update.response {label, affectedWorkspaceCount}` | Renames or recolours a label |
| `workspace.label.delete.request {name}` | `.delete.response {affectedWorkspaceCount}` | Deletes a label and takes it off every workspace |
| `workspace.label.delete.inspect.request {name}` | `.delete.inspect.response {affectedWorkspaceCount}` | How many workspaces a delete would change |

The same as Paseo:
- **Labels have no ids.** Names are matched with whitespace collapsed and without case, and are
  stored trimmed. A colour is one of Paseo's ten names (`violet`, `sky`, `emerald`, `orange`,
  `pink`, `indigo`, `teal`, `red`, `amber`, `blue`); any other does not validate
  (`unknown_schema`).
- **There is no create request.** Assigning a label the catalog lacks makes it, with the request's
  colour, at the end of the catalog. For a label the catalog has, its own name and colour win.
  Unassigning makes nothing.
- **Renames** rewrite every workspace's assignment in place; a recolour changes only the
  catalog; a delete takes the label off every workspace. Renaming onto another label's name is
  refused.
- **Refusals** are `rpc_error` with Paseo's codes and messages: `workspace_not_found`,
  `label_name_empty`, `label_not_found`, `label_name_taken`.
- **Pushes** go to every socket's label subscription, whoever made the change:
  `workspace.label.update {kind: "upsert", label, previousName?}` (`previousName` only on a
  rename) or `{kind: "remove", name}`, each with `generation` and `seq`. A workspace whose labels
  changed gets a `workspace_update`. A change that changes nothing pushes nothing.
- **Catching up:** a cursor is answered from a journal of the last 256 changes, folded as Paseo
  folds them (the last upsert per name; a rename removes the old name; a label made and removed
  within the window leaves nothing). Removals are keyed by `name`, and there is no `reason`. An
  older or foreign cursor gets the whole catalog.
- **Workspace descriptors** carry `labels` (names, in the order assigned) and leave the field out
  when there are none.
- **Persistence:** the catalog is `workspace-labels.json`, a bare `[{name, color}]` array as in
  Paseo; assignments are on the workspace records in `workspaces.json`.

Differences:
- Paseo writes the catalog and the workspace registry as one transaction, with a journal file to
  roll back. Burrow writes them as two files, the catalog first when a label is made. A crash
  between the two can leave a new label in the catalog without its assignment, or a deleted
  label still on a workspace, which shows it in the client's default colour.
- The label journal's `generation` is the directory generation the other collections share. Both
  are one UUID per process.
- A `sync` cursor without `generation` or `afterSeq` reads as no cursor. Paseo refuses it as a
  malformed request.
- Burrow cannot archive workspaces, so counts and rewrites cover only the workspaces it has.

## Prompt images

Paseo 0.10 to 0.12 sends a prompt's images to the provider and records only its text: the
`user_message` row has no images, so no client but the sender ever shows them. Burrow keeps
them, on purpose:
- Each image is written once to `<home>/attachments/<sha256>.<ext>` (a temporary directory without
  a home), and the `user_message` lists them as `images: [{path, mimeType}]`. Paseo's clients drop
  the unknown field; crusteo shows them.
- `file_explorer_request` (mode `file`) serves those files and nothing else, base64 in the JSON
  reply, `maxBytes` honoured, as Paseo serves the image files its agents save. Any other path is refused with Paseo's "Access outside of workspace is not allowed", and a
  listing with an error.
- Images that cannot be decoded, or are not of an image type, are left out of the list; the
  prompt still goes through.

## Open incompatibilities

- **Interrupts.** Burrow interrupts with `cancel_queued: true`, which also withdraws
  messages still queued in the CLI. It does not withdraw single messages
  (`cancel_async_message`), which Paseo does before the CLI has started a turn.
- **Provider subagents.** Claude's Task subagent frames are not mapped
  (`agent.provider_subagents.*` is unanswered).
- **Notifications** are Paseo's (`agent-attention-policy.ts`, `push/`). A client that asked for
  them (`session.events.set_subscription.request` with `notifications: true`) and whose last
  `client_heartbeat` is under three minutes old is present. When an agent needs attention, a
  present client that is visible and focused on it stops everything; otherwise the most recently
  active present client gets `shouldNotify: true`; with nobody present, every phone that
  registered a token (`register_push_token`, leased for 48 hours and renewed by that phone's
  heartbeats) gets a push through Expo's push service, as from Paseo's daemon, except for an
  error. Expo's `DeviceNotRegistered` and `InvalidCredentials` drop a token, and so does
  `push.unregister.request` (`pushTokenRevocation`). Tokens are kept in `push-tokens.json` in
  Paseo's format, and `import-paseo` brings Paseo's over. `--push-url` sends elsewhere and
  `--no-push` sends none. Terminals' attention, which Paseo also pushes, does not arise: Burrow
  has no terminals.
- **Not offered yet:** ACP agents other than Cursor, OpenCode and Gemini, terminals, checking out
  change requests and
  forge search ([above](#projects-and-workspaces)), `list_commands` content, search in the agent
  history list (it lists every agent, unfiltered), rewind and fork, diffs and file
  editing, voice, connected hosts, relay pairing and passwords.
- **Antigravity:** the differences in [the table above](#antigravity): no `usage_updated`,
  Burrow turn ids, and no `isUnattended` on its mode. Antigravity's child conversations show only
  as `sub_agent` tool calls, as in Paseo.
- **Mode name.** Claude Code 2.1.303 no longer lists `default` as a `--permission-mode` choice.
  Burrow leaves the flag out for `default`. At runtime it falls back to `manual` if the CLI
  refuses `default`; Paseo 0.12 still sends `default`, and the CLI accepted it in testing.

## pi

Burrow's pi provider follows Paseo 0.12.0's (`providers/pi/`), checked against pi 1.0.3. Paseo's
pi files did not change after 0.12.0.

**The same as Paseo:**
- The providers entry: id `pi`, label, description, no modes, `defaultModeId` null, and
  `supportsMcpServers` false.
- Models: read from a pi process (`get_available_models`) at startup. Ids are `<provider>/<id>`,
  labels and descriptions are made the same way, and the default is the model a fresh pi starts
  on. Thinking options are pi's levels the model supports, defaulting to `medium` or the next one
  up.
- The launch: `pi --mode rpc` with `--model` and `--thinking`. A resumed session starts with
  `--session` on its own model, and is then switched to the agent's model if pi still has it.
- Timeline items: text and reasoning deltas, tool calls with Paseo's details for `bash`, `read`,
  `edit`, `write`, `find`, `grep` and `ls`, compaction, provider retries as `error` items,
  extension notifications, and extension messages as synthetic tool rows.
- Questions: an extension's `select`, `confirm`, `input` or `editor` dialog becomes a
  `kind: "question"` request with one `Response` question, and the answer goes back as pi's
  `extension_ui_response`.
- Interrupts: `clear_queue`, then `abort`; the turn ends `turn_canceled` with reason
  `interrupted`. A failed run's error reads like Paseo's (`<error> (stopReason=..., model=...)`).
- `thread_started` at the start of every run.

**Different, on purpose or for now:**
- Turn ids are Burrow's `foreground-turn-N`; Paseo's pi provider uses random UUIDs.
- Usage is read with `get_session_stats` when a run ends and sent on `turn_completed`, which also
  sets `lastUsage`. Paseo sends `turn_completed` without usage, sets `lastUsage` from a separate
  update, and also polls every 3 s during a turn.
- `user_message` rows are Burrow's, with the client's message id. Paseo takes them from its
  own pi extension, with pi's entry id as `messageId`.
- `persistence.nativeHandle` is pi's session id; Paseo's is the session file. pi 1.0 resumes from
  either.
- Paseo loads an extension of its own into pi (system prompt, MCP servers, user entry capture,
  rewind). Burrow's only registers the session's MCP servers ([above](#agent-tools-mcp)): the
  system prompt goes in with `--append-system-prompt`, and it has no rewind to offer.
- Paseo's adapters for third-party pi extensions are not ported: `pi-ask-user`'s combined
  question with a comment, the subagent extensions (`provider_subagent` events), the todo and
  goal extensions, and `pi-mcp-adapter`'s tool names. Their dialogs still come through as plain
  questions.
- Model entries have no `metadata` (`provider`, `modelId`), and thinking options no
  `description`: the provider interface has no field for them.
- `/compact` and `/autocompact` go to pi as typed instead of through Paseo's out-of-band handling,
  and `list_commands` content, pi session listing and import are not offered.
- The model list is read once, when the daemon starts.

**Smoke test.** pi 1.0.3 on `google/gemini-2.5-flash-lite` with thinking off, through Burrow
and `examples/smoke.rs` (`pi` phase, then `resume` twice):
- a plain answer;
- a bash call that wrote its file, shown as a completed `shell` tool call;
- a turn interrupted after its first text, ended `turn_canceled`;
- after each of two daemon restarts the agent came back `closed`, its next turn resumed the pi
  session (same epoch, seqs carried on), and the model named the file it had written before the
  first restart.

The first run's pi frames, scrubbed, are the `real_gemini_session.jsonl` replay fixture. No
Paseo daemon ran for this one, so there is no differential log yet.
