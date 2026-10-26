# Burrow

`burrow` is Crusteo's own daemon. It runs coding agents on one host and speaks the Paseo daemon
WebSocket protocol, version 1. Because the protocol is unchanged, Crusteo, `reef` and the
Paseo apps connect to it the way they connect to Paseo's daemon.

It runs next to Paseo's daemon until it has proved as good or better. It uses its own port
(`127.0.0.1:6797`; Paseo uses 6767) and its own state folder (`$BURROW_HOME`, else
`~/.burrow`). It never writes `~/.paseo`, and reads it only when asked to import it
([below](#move-a-host-over-from-paseo)).

| Document | Covers |
|---|---|
| [install.md](install.md) | Download, run as a service, upgrade, roll back; replacing Paseo on a host |
| [design.md](design.md) | Architecture, reasons, provider adapters, testing, roadmap, risks, open questions |
| [compatibility.md](compatibility.md) | Which Paseo versions it serves, 0.12 changes, differential test results, open incompatibilities |
| [protocol-inventory.md](protocol-inventory.md) | Every protocol v1 message with Crusteo usage, priority tier and status; Paseo's on-disk state |

## Status: runs Claude, Codex, pi, Antigravity and Cursor agents

### Working, and tested with Crusteo's `paseo-client` and `fleet`

- **Connection:** handshake, close codes, the socket lease and the outbound cap.
- **Directories:** owned subscriptions; the agent, workspace, project and label directories with
  sync cursors. A workspace in a git checkout is reported as one.
- **Agents:**
  - create, send, steer, cancel, archive, refresh and delete;
  - permissions and questions, which never time out;
  - attention;
  - mode, model and thinking changes;
  - the providers snapshot.
- **Workspace labels:** the catalog with Paseo's colours; assigning (which makes a label the
  catalog lacks) and unassigning; renaming, recolouring and deleting; pushes to every label
  subscription ([compatibility.md](compatibility.md#workspace-labels)).
- **Projects and workspaces** ([compatibility.md](compatibility.md#projects-and-workspaces)):
  - adding a project, git or not, with Paseo's project keys; making a folder for one;
  - the folder picker's search, and a new worktree's starting branches;
  - workspaces for a folder or a new git worktree (checking out a branch, or branching off a
    starting ref), background or not, with an initial agent;
  - agents made with git options, in a new worktree or on a branch made in place;
  - `paseo.json` setup scripts, with their progress;
  - archiving a workspace, which removes its worktree;
  - removing a project, which archives its open workspaces and leaves the disk as it is.
- **Turns:** `foreground-turn-N` and `autonomous-turn-N` turn ids (UUIDs for ACP agents, as in
  Paseo), and Paseo's 60 ms coalescing of streamed text and tool updates.
- **Persistence:** projects, workspaces (archived ones too), the label catalog and agents are
  saved as JSON records; git worktrees Burrow makes are in `<home>/worktrees`. Each agent's timeline is an append-only file that keeps its epoch and seqs across
  restarts. After a restart, agents come back `closed` and resume their provider session when
  they are next used. A deleted agent's record and timeline are removed.
- **CLI upgrades:** each provider's CLI is checked (its `--version`) every 5 minutes. When it
  changed, or a CLI turned up, the provider is found again with its models, and clients get a
  `providers_snapshot_update`. Every hour, and on `refresh_providers_snapshot_request`, providers
  are probed again even with the CLI unchanged, since an account can be offered a new model
  without an upgrade (Codex lists what the account offers). New sessions use the new CLI; an idle
  agent still on the old one restarts it at its next prompt, on the same provider session; a
  running agent keeps its CLI.
- **Claude Code:** the user's own `claude`, driven the way Paseo's Agent SDK drives it.
  Permissions, questions, plans, steer, interrupt, resume, history from Claude's transcripts, and
  the context meter all work.
- **Codex:** the user's own `codex`, one `codex app-server` per agent, driven the way Paseo's
  Codex adapter drives it. Models come from Codex itself. Command, file-change and MCP approvals,
  questions, steer, interrupt, mode, model and reasoning-effort changes, resume of the Codex
  thread, and the context meter all work. What is not mapped yet is listed in
  [compatibility.md](compatibility.md#codex).
- **pi:** the user's own `pi` in its RPC mode, driven the way Paseo's pi provider drives it. The
  models pi is configured with, thinking levels, steer, interrupt, resume, extension dialogs as
  questions, history from pi's session files, and the context meter all work.
- **Antigravity:** the user's own `agy` in stream-json print mode, shown as `antigravity` as
  Paseo 0.12's plugin shows it. Models from `agy models`, model changes, interrupt and resume
  work. agy cannot ask for permission when another app drives it, so it runs every tool without
  asking, and each session opens with that warning.
- **Cursor:** the user's own `cursor-agent`, over the Agent Client Protocol the way Paseo's
  generic ACP client drives it. Its models and modes, tool calls, plans, permissions, interrupt and
  resume (`session/load`) work. Another ACP agent is one more configuration in
  `burrow-provider-acp`.

### Verified against a real model

The smoke test ran in WSL on Claude Code 2.1.303 with Haiku 4.5:
- a plain turn;
- a Bash command allowed through a permission prompt, which really wrote its file;
- a question answered by header;
- an interrupted turn;
- a daemon restart, then a resumed turn that remembered the conversation.

The same scenario through Paseo 0.12.0 and 0.11.4 gave the same conversation
([compatibility.md](compatibility.md)).

The Codex smoke test ran in WSL on codex-cli 0.162.1 with GPT-6-Luna at low effort: a plain
turn, a command approved through a permission prompt (it wrote its file), an interrupted turn,
then a daemon restart and a resumed turn that remembered the first answer.
The Antigravity smoke test ran agy 1.4.2 on Gemini 3.8 Flash (low): a plain turn, a shell
command, a refused and an accepted mode change, a model switch, an interrupted turn, then a
daemon restart and a resumed turn that remembered the first answer.
A pi smoke test ran pi 1.0.3 on Gemini 2.5 Flash-Lite: a plain turn, a bash command that
really wrote its file, an interrupted turn, then two daemon restarts and resumed turns that
remembered the conversation ([compatibility.md](compatibility.md#pi)).
The Cursor smoke test ran in WSL on `cursor-agent` 2026.10.01 with GPT-5 Mini: a plain turn, a
shell command allowed through a permission prompt, an interrupted turn, and a resumed turn after a
restart ([compatibility.md](compatibility.md#cursor-acp)).

### Not yet

Terminals, checking out pull requests and forge search, rewind and fork, provider subagents,
`list_commands` content, notification targeting (`shouldNotify`), password auth. All of these
get Paseo's answer to an unknown request, or a refusal that says so.

## Crates

| Crate | |
|---|---|
| `crates/burrow-protocol` | Wire rules and typed payloads for protocol v1. serde only, no I/O. |
| `crates/burrow-daemon` | The `burrow` binary: listener, connections, subscriptions, store, persistence, agent manager, handlers. |
| `crates/burrow-provider` | The provider interface, and an echo provider that runs no model. |
| `crates/burrow-provider-claude` | Claude Code in stream-json control mode. |
| `crates/burrow-provider-codex` | Codex through `codex app-server` (JSON-RPC over stdio). |
| `crates/burrow-provider-pi` | pi in its JSONL RPC mode. |
| `crates/burrow-provider-antigravity` | Antigravity's `agy` in stream-json print mode. |
| `crates/burrow-provider-acp` | Agent Client Protocol agents over stdio JSON-RPC; Cursor's configuration. |

## Run it next to Paseo

```sh
cargo build --release -p burrow-daemon
target/release/burrow            # ws://127.0.0.1:6797/ws, state in ~/.burrow
```

Then add the host in Crusteo (`127.0.0.1:6797`, or the host's address if bound wider) or in the
Reef.

| Flag | Default | |
|---|---|---|
| `--listen` | `127.0.0.1:6797` (`BURROW_LISTEN`) | There is no password support. Bind it wider only on a locked-down network. |
| `--home` | `$BURROW_HOME`, else `~/.burrow` | Records, timelines and `server-id`, the identity clients key the host by. |
| `--worktrees-root` | `<home>/worktrees` (`BURROW_WORKTREES_ROOT`) | Where new git worktrees go, as `<project hash>/<slug>`. |
| `--claude-bin`, `--no-claude` | `claude` on PATH | Claude is offered when found. |
| `--codex-bin`, `--no-codex` | `codex` on PATH | Codex is offered when found. At startup Burrow asks it for its models, which takes a second or two. |
| `--pi-bin`, `--no-pi` | `pi` on PATH | pi is offered when found, with the models it is configured with. |
| `--agy-bin`, `--no-agy` | `agy` on PATH | Antigravity is offered when `agy` 1.1.15 or later is found. |
| `--cursor-bin`, `--no-cursor` | `cursor-agent` on PATH | Cursor is offered when found, run as `<bin> acp`. |
| `--echo-provider` | off | A provider that answers from the prompt text, for trying clients without a model. |
| `--server-id`, `--hostname`, `--advertise-version` | taken from home, the OS, and `<version>-burrow` | What `server_info` reports. |
| `--seed-demo` | off | Adds a placeholder project and agent to an empty home. |
| `--allow-host`, `--allow-origin` | none | Extra `Host` and `Origin` values to accept at upgrade. |

Other settings:
- Each `BURROW_*` variable here was `CARAPACE_*` before 0.1.0-beta.1. For one release the old
  name still works when the new one is not set, and the log says it is deprecated. The default
  home moves from `~/.carapace` on first start ([install.md](install.md#upgrading-from-carapace)).
- `BURROW_LOG=debug` turns on verbose logs.
- `BURROW_CLAUDE_TRACE=<dir>` records every frame exchanged with each Claude Code process as
  JSONL. A recording contains account details, so scrub it before using it as a fixture.
- `BURROW_CODEX_TRACE=<dir>` does the same for each `codex app-server`. Its recordings hold the
  user's whole Codex config (`config/read`) and hook paths; scrub those too.
- `BURROW_PI_TRACE=<dir>` does the same for each pi process. A recording contains pi's system
  prompt, with the user's skills and context files, so scrub it too.
- `BURROW_ACP_TRACE=<dir>` does the same for each ACP agent session, Cursor's included.
- On SIGTERM or Ctrl-C the daemon stops its agents and flushes state.

## Move a host over from Paseo

`burrow import-paseo` copies a Paseo home's Claude agents into Burrow's state folder once,
so they keep showing in every client after the host switches. The whole switch, the server id and
the way back included, is in [install.md](install.md#replace-paseo-on-a-host).

```sh
burrow import-paseo --dry-run    # print what would happen; writes nothing
burrow import-paseo              # --from ~/.paseo (default), --home as for the daemon
```

**Stop Burrow on that state folder first.** It holds the project and workspace
registries in memory and rewrites them on its next change, which would drop what the import
added. There is no lock file to check this with, so the import cannot tell. Paseo's daemon can
keep running: its records are only read, and it writes them atomically.

What comes over, from Paseo 0.11's records (`agents/<cwd slug>/<id>.json`,
`projects/projects.json`, `projects/workspaces.json`, `projects/workspace-labels.json`):

| Paseo | Burrow |
|---|---|
| A Claude agent | The same id, title, labels, cwd, workspace, timestamps (`updatedAt` is the later of `updatedAt` and `lastActivityAt`), attention, `archivedAt`, mode (`lastModeId`, else `config.modeId`), model, thinking level, system prompt, `runtimeInfo` and `persistence`. It comes back `closed`, and its next prompt resumes the Claude session with `--resume`. |
| Its conversation | Read once from `$CLAUDE_CONFIG_DIR` (else `~/.claude`)`/projects/<encoded cwd>/<session>.jsonl` into Burrow's timeline, under a new epoch. Without a transcript the agent still comes over, with an empty timeline and a warning. |
| A project | The same id, root, kind, names and `projectKey`. A project whose root Burrow already has is not added; its workspaces join Burrow's project. |
| A workspace | The same id, project, cwd, kind, names, title, pin and label assignments. Git facts come from `git` if the checkout is there, else from Paseo's branch and worktree root. |
| A label | The same name and colour, in Paseo's order. A label Burrow already has by that name keeps Burrow's colour. |
| An active agent with no workspace in the registry | Placed by its cwd, as a new agent would be. |

What is skipped, with one line each:
- agents of other providers (`pi`, `codex`, ...) and internal agents;
- files and registry entries that do not parse, such as a label with a colour outside Paseo's
  ten;
- archived projects and workspaces, unless an imported active agent sits in one: Burrow
  cannot archive either, so they would come back unarchived;
- an older copy of a record that Paseo left behind under another cwd slug;
- feature toggles (`features`) and MCP or tool-policy settings, which Burrow does not
  implement.

Anything Burrow's state folder already has wins. An agent, project or workspace whose id is
there, or a label whose name is, is reported as already there and left alone, so a second run adds nothing and keeps every
change made since. The import refuses a `--home` inside the Paseo home.

## Tests

```sh
cargo test -p burrow-protocol -p burrow-daemon -p burrow-provider -p burrow-provider-claude \
  -p burrow-provider-codex -p burrow-provider-antigravity -p burrow-provider-pi \
  -p burrow-provider-acp
```

| Tests | Cover |
|---|---|
| `burrow-daemon/tests/wire.rs` | The protocol, with a bare WebSocket client |
| `burrow-daemon/tests/paseo_client.rs` | Crusteo's bootstrap sequence |
| `burrow-daemon/tests/agents.rs` | The agent manager end to end with the echo provider, through `paseo-client`, rendered with `fleet`; deleting a running, an archived and an unknown agent |
| `burrow-daemon/tests/restart.rs` | State and timelines across a restart |
| `burrow-daemon/tests/labels.rs` | Workspace labels through `paseo-client` in crusteo's request shapes: making, assigning, renaming, recolouring and deleting, refusals, pushes to every socket, a cursor catching up, and a restart; `fleet`'s directory follows the pushes |
| `burrow-daemon/tests/workspaces.rs` | Projects and workspaces through `paseo-client` on scratch git repositories: adding git and plain projects and their keys, folder and worktree workspaces (checkout, branch-off, taken names, a folder inside the checkout), where the worktrees land and their branches, an initial agent and the creation phases, agents with git options, setup scripts, the starting branches, the folder search, refusals, archiving, removing projects, and restarts. Echo provider; no network |
| `reef/tests/burrow.rs` | Reef's create, relabel and delete actions against a real Burrow in-process |
| `burrow-daemon/tests/import_paseo.rs` | `import-paseo` on a made-up Paseo home: what the clients then read (labels included), what is skipped, and a second run |
| `burrow-provider-claude/tests/replay.rs` | The Claude adapter against scripted CLI transcripts, and one real Haiku session, recorded and scrubbed; no process, no model |
| `burrow-provider-codex/tests/replay.rs` | The Codex adapter against scripted app-server transcripts, and one real session and its resume after a restart, recorded and scrubbed |
| `burrow-daemon/tests/codex.rs` | A Codex agent through the daemon against a scripted app-server: the providers snapshot, turn ids, runtime info, usage, an approval, the timeline |
| `burrow-provider-pi/tests/replay.rs` | The pi adapter against scripted RPC transcripts, and one real Gemini session, recorded and scrubbed; no process, no model |
| `burrow-provider-acp/tests/replay.rs` | The ACP session against scripted agent transcripts, the catalog probe, and one real Cursor session and its resume, recorded and scrubbed; no process, no model |
| `burrow-daemon/examples/smoke.rs` | Not a test: it drives one real agent (Claude by default; Codex with `first codex <model>`, Antigravity with `agy`, pi with `pi <model>`, Cursor with `cursor <model>`) through any daemon and logs every frame, for live and differential checks. Every turn is a real model call. |

## Next step: one shared protocol crate

`burrow-protocol` (the daemon's side) and `paseo-client` (the client's side) both describe
the wire. Merge them into one `paseo-protocol` crate that both the daemon and the clients depend
on (design.md, section 10).

| Concept | `burrow-protocol` | `paseo-client` |
|---|---|---|
| Protocol version | `PROTOCOL_VERSION` (`lib.rs`) | `PROTOCOL_VERSION` (`lib.rs`) |
| Hello capabilities | `hello::capability` names, `Hello` (parse) | `proto::capabilities()` (build) |
| Welcome | `server_info::ServerInfo` (serialize) | `proto::ServerInfo::from_status` (parse) |
| Envelope | `envelope::{session, rpc_error, request_id}` | `connection.rs` `unwrap_session`, `settle` (`rpc_error` decode) |
| Directory sync | `directory::{SyncCursor, SyncMeta, SyncRemoval, PageInfo}` | `proto::{Cursor, SyncMeta, Removal, PageInfo}` |
| Directory entities | `directory::{AgentSnapshot, WorkspaceDescriptor, ProjectDescriptor, ProjectPlacement}` | `proto::{Agent, Workspace, Project}` |
| State bucket | `directory::StateBucket`, `store::agent_bucket` | `proto::Bucket`, `Agent::bucket` |
| Terminal frames | none yet | `terminal.rs`: opcodes, `encode_frame`/`decode_frame`, resize, `TerminalState` snapshot |

How to merge each side:
- Where `burrow-protocol` serializes one of these and `paseo-client` parses it, a single type
  should derive both `Serialize` and `Deserialize`.
- Keep `paseo-client`'s leniency: unknown fields are kept, optional fields default.
- Keep `burrow-protocol`'s distinction between nullable fields (sent as `null`) and optional
  fields (omitted).
- Burrow's terminals should use `paseo-client`'s terminal codec rather than write a second
  one.
