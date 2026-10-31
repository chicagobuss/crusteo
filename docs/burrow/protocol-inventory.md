# Paseo protocol v1: message inventory

What a Paseo-compatible daemon must speak, as of Paseo **0.11.2** (`WS_PROTOCOL_VERSION = 1`).
Sources: `packages/protocol/src/messages.ts` (`SessionInboundMessageSchema`,
`SessionOutboundMessageSchema`, `WSInboundMessageSchema`, `WSOutboundMessageSchema`) and the split
files `chat/`, `schedule/`, `loop/`, `browser-automation/rpc-schemas.ts`; handlers in
`packages/server/src/server/session.ts` and `websocket-server.ts`; crusteo usage from
`crates/fleet/src/requests.rs` and `src/` in crusteo.

Counts from the schema unions: **206** client→daemon session message types and **224**
daemon→client session message types (181 named responses and 43 other messages), inside 4
inbound and 3 outbound top-level frame kinds, plus 8 binary frame opcodes.

## Legend

| Column | Meaning |
|---|---|
| **C** | Crusteo sends it (requests) or handles it (events). `yes`, or blank. |
| **Tier** | **P0**: needed for crusteo to connect and show a host. **P1**: needed to run and talk to agents, and everything else crusteo uses. **P2**: everything else the official apps use. **—**: dead or legacy; answer like an unknown request. |
| **Status** | `done` = implemented in the phase 0 skeleton. |

Replies follow one naming rule (`replies.ts`): `x_request` → `x_response`, `x.request` →
`x.response`, `a/b` → `a/b/response`, a bare name → `name_response`. Exceptions are noted in
the tables. Every reply carries `payload.requestId`. Requests are flat (`{type, requestId, ...args}`);
outbound messages are `{type, payload}`. Both travel inside `{"type":"session","message":...}`.

## 1. Frames and session

| Message | Dir | Purpose | C | Tier | Status |
|---|---|---|---|---|---|
| `hello` (bare) | C→D | First frame within 15 s: `clientId`, `clientType`, `protocolVersion`, `capabilities`, `auth?` | yes | P0 | done |
| `status{status:"server_info"}` | D→C | Welcome: `serverId`, `hostname`, `version`, `permissions`, `features` | yes | P0 | done |
| `hello.rejected` (bare) | D→C | Before an auth or protocol close, if the hello asked for it | yes | P0 | done (protocol only; no auth yet) |
| `ping` / `pong` (bare) | both | Liveness; the first ping claims the 45 s lease | yes | P0 | done |
| `recording_state` (bare) | C→D | Voice; ignored | | P2 | done (ignored) |
| `ping` (session) → `pong` | C→D | RPC ping with `clientSentAt`/`serverReceivedAt`/`serverSentAt` | | P0 | done |
| `rpc_error` | D→C | Correlated failure: `requestId`, `requestType?`, `error`, `code` | yes | P0 | done |
| `status{status:"error"}` | D→C | Uncorrelated failure (no `requestId` to answer) | | P0 | done |
| `client_heartbeat` | C→D | Focus and visibility; drives `shouldNotify`. No reply | | P1 | done |
| `activity_log` | D→C | Log lines (event subscription) | | P2 | |
| `artifact` | D→C | Markdown/diff/image/code artifact | | P2 | |
| `daemon.get_status.request` | C→D | Daemon status RPC | | P2 | |
| `daemon.get_pairing_offer.request` | C→D | Pairing offer / QR | | P2 | |
| `daemon.config.reload.request` | C→D | Reload `config.json` | | P2 | |
| `get_daemon_config_request` / `set_daemon_config_request` | C→D | Read / patch mutable config | | P2 | |
| `read_project_config_request` / `write_project_config_request` | C→D | Read / write a repo's `paseo.json` | | P2 | |
| `diagnostics.request` | C→D | Diagnostics dump | | P2 | |
| `shutdown_server_request` / `restart_server_request` | C→D | Reply `status{shutdown_requested\|restart_requested}` | | P2 | |
| `daemon.update.request` | C→D | Self-update; `daemon.update.progress`* then `.response` | | P2 | |
| `status{daemon_config_changed\|plugin_catalog_changed\|plugin_settings_changed}` | D→C | Config and plugin changes (event subscription) | | P2 | |

## 2. Subscriptions and directory sync

| Message | Dir | Purpose | C | Tier | Status |
|---|---|---|---|---|---|
| `session.events.set_subscription.request` | C→D | Opt into event streams (17 names, e.g. `project.update`, `agent_permission_request`) | yes | P0 | done |
| `subscription.release.request` | C→D | Release any owned subscription; acks unknown ids | yes | P0 | done |
| `fetch_agents_request` | C→D | Agent directory page; `subscribe:{}` opens a subscription, `sync` makes it sequenced | yes | P0 | done |
| `agent_update` | D→C | Full-snapshot `upsert` or `remove`, per subscription | yes | P0 | done |
| `fetch_workspaces_request` | C→D | Workspace directory, plus `emptyProjects` | yes | P0 | done |
| `workspace_update` | D→C | `upsert` or `remove` (with `emptyProject?`, `removedProjectId?`) | yes | P0 | done |
| `project.list.request` | C→D | Project list, optionally sequenced (feature `projectList`) | yes | P0 | done |
| `project.update` | D→C | Project `upsert` / `remove` (event subscription) | yes | P0 | done |
| `creation.subscribe.request` | C→D | Reattach to a creation in progress | | P2 | |

## 3. Agents and timeline

| Message | Dir | Purpose | C | Tier | Status |
|---|---|---|---|---|---|
| `fetch_agent_request` | C→D | One agent by id, unique prefix or exact title | | P0 | done |
| `fetch_agent_timeline_request` | C→D | Timeline page: `tail`/`before`/`after`, `cursor{epoch,seq}`, `limit` (app: 40) | yes | P0 | done (unprojected rows) |
| `agent.timeline.set_subscription.request` | C→D | The complete set of agents whose live stream this subscription carries | yes | P0 | done |
| `agent_stream` | D→C | Live event: `timeline` rows (with `seq`, `epoch`), `turn_*`, `thread_started`, `permission_*`, `attention_required` | yes | P0 | done (timeline rows) |
| `agent.timeline.replacement` | D→C | Timeline rebuilt (new epoch); refetch the tail | yes | P1 | |
| `agent.create.request` | C→D | Create (modern, with receipts); `agent.create.update`* when subscribed | yes | P1 | done |
| `create_agent_request` | C→D | Legacy create; reply `status{agent_created\|agent_create_failed}` | | P1 | done |
| `send_agent_message_request` | C→D | Prompt: `text`, `messageId`, `images?`, `attachments?`, `activeTurnBehavior?`, `sourceAgentId?` (Paseo 0.12, behind `agentMessageProvenance`: the sending agent; delivered in a `<paseo-system kind="agent-message">` envelope and recorded as an `agent_message` call), `sessionMode?` (Paseo 0.13: the mode set before the prompt; one the agent cannot take refuses the send) | yes | P1 | done |
| `cancel_agent_request` | C→D | Interrupt the turn | yes | P1 | done |
| `archive_agent_request` | C→D | Archive; reply `agent_archived` | yes | P1 | done |
| `refresh_agent_request` | C→D | Reload / unarchive; reply `status{agent_refreshed}` | yes | P1 | done |
| `update_agent_request` | C→D | Rename and merge labels (also client tab labels) | yes | P1 | done |
| `clear_agent_attention` | C→D | Clear the unread flag; reply `clear_agent_attention_response` | yes | P1 | done |
| `agent_attention_required` | D→C | Finished / error / permission, with `shouldNotify` and a notification | yes | P1 | done |
| `fetch_agent_history_request` | C→D | Archived / history search (not conversation history); `paseo delete --all/--cwd` reads it | yes | P1 | done: every agent, archived and background ones too, newest first, paged; no search |
| `agent.timeline.list_prompts.request` | C→D | Prompt index for jump-to-prompt | yes | P1 | |
| `list_commands_request` | C→D | Slash commands and skills for an agent or draft | yes | P1 | |
| `agent.detach.request` | C→D | Detach a child agent (`paseo detach`) | yes | P1 | done: drops `paseo.parent-agent-id` |
| `agent.provider_subagents.list.request` | C→D | Provider sub-agents of an agent (feature `providerSubagents`) | yes | P1 | |
| `agent.provider_subagents.timeline.get.request` | C→D | A sub-agent's timeline | yes | P1 | |
| `agent.provider_subagents.update` | D→C | Sub-agent upsert / timeline / remove | yes | P1 | |
| `agent_deleted`, `agent_archived` | D→C | Replies to delete / archive | yes | P1 | done |
| `delete_agent_request` | C→D | Hard delete; reply `agent_deleted` | yes | P1 | done |
| `close_items_request` | C→D | Archive agents and kill terminals together | | P2 | |
| `wait_for_finish_request` | C→D | Block until the turn ends: `paseo send` (unless `--no-wait`), `paseo run`, `paseo wait` | | P1 | done: `idle`/`error`/`permission`/`timeout`, as Paseo answers |
| `resume_agent_request` | C→D | Resume a provider handle; reply `status{agent_resumed}` | | P2 | |
| `import_agent_request` | C→D | Import a provider session | | P2 | |
| `fetch_recent_provider_sessions_request` | C→D | Importable provider sessions | | P2 | |
| `agent.timeline.search.request` | C→D | Find in chat | | P2 | |
| `agent.timeline.append.request` | C→D | Plugin appends a `plugin` item | | P2 | |
| `agent.fork_context.request` | C→D | Export context for "fork chat from here" | | P2 | |
| `agent.rewind.request` | C→D | Rewind conversation and/or files | | P2 | |
| `agent_status` | D→C | Legacy status broadcast | | — | |
| `agent_list`, `send_agent_message` | — | Defined but outside the wire unions | | — | |

## 4. Permissions and questions

Questions and plan approvals are permission requests with `kind: "question"` or `"plan"`
(`kind` is one of `tool`, `plan`, `question`, `mode`, `other`). Pending requests also appear on
the agent snapshot (`pendingPermissions`, authoritative) and on `agent_stream`.

| Message | Dir | Purpose | C | Tier | Status |
|---|---|---|---|---|---|
| `agent_permission_request` | D→C | A prompt: `{agentId, request{id, kind, name, input, detail?, actions?, ...}}` | yes | P1 | done |
| `agent_permission_response` | C→D | Fire-and-forget answer; `requestId` is the *permission* id; `allow{updatedInput?}` / `deny{message?, interrupt?}` | yes | P1 | done |
| `agent_permission_resolved` | D→C | Confirmation (delivered more than once in practice) | yes | P1 | done |

## 5. Workspaces, projects, worktrees, scripts

| Message | Purpose | C | Tier |
|---|---|---|---|
| `workspace.create.request` (+ `workspace.create.update`*) | Create a directory or worktree workspace, with an initial agent. Served: both sources, background, `callerAgentId`, receipts in memory; not change-request checkouts | yes | P1 |
| `archive_workspace_request` | Archive a workspace (and its Paseo worktree). Served | yes | P1 |
| `workspace.pin.set.request` | Pin / unpin | yes | P1 |
| `workspace.title.set.request` | Rename | yes | P1 |
| `workspace.clear_attention.request` / `workspace.mark_unread.request` | Read / unread | yes | P1 |
| `script_status_update` (D→C) | Workspace script states (event subscription) | yes | P1 |
| `workspace_setup_progress` (D→C) | Setup script progress. Sent | | P1 |
| `project.add.request` | Register a folder as a project. Served | yes | P1 |
| `project.remove.request` | Remove a project, archiving its open workspaces (feature `projectRemove`). Served | yes | P1 |
| `project.rename.request` | Rename a project | | P2 |
| `project.icon.set.request`, `project.icon.get.request`, `project_icon_request` | Project icons | | P2 |
| `project.create_directory.request` | New project from a new folder. Served | | P2 |
| `project.github.clone.request` | New project from a clone | | P2 |
| `open_project_request` | Open a cwd as a project. Served | | P2 |
| `workspace.recovery.inspect.request` / `.restore.request` | Recreate a deleted worktree | | P2 |
| `workspace.github.search_repositories.request` | Repository search | | P2 |
| `workspace_setup_status_request` | The last setup snapshot. Served | | P2 |
| `workspace.setup.subscribe.request` | C→D | Paseo 0.13 setup panel: snapshot, then that workspace's `workspace_setup_progress` (behind `workspaceSetupObservation`); workspaces carry `setupStatus`, and `createdAt` / `lastUserMessageAt` (behind `workspaceSortTimes`) | yes | P1 | done |
| `workspace.setup.run.request` | Rerun setup scripts | | P2 |
| `paseo_worktree_list_request` | Paseo-owned worktrees of a repository. Served | | P2 |
| `paseo_worktree_archive_request`, `create_paseo_worktree_request` | Archive by path; the older create | | P2 |
| `workspace.script.list/start/stop.request`, `start_workspace_script_request` (legacy) | Workspace scripts and services | | P2 |
| `list_available_editors_request`, `open_in_editor_request` (legacy) | Open in editor | | P2 |
| `directory_suggestions_request` | Path autocomplete; the Add Project folder search. Served | yes | P1 |

All C→D; replies by the naming rule.

## 6. Labels

| Message | Purpose | C | Tier | Status |
|---|---|---|---|---|
| `workspace.label.list.request` (+ `subscribe`, `sync`) | Label catalog; `sync` meta is always present and removals are keyed by `name` | yes | P0 | done |
| `workspace.label.update` (D→C) | Catalog `upsert` (with `previousName?`) / `remove` | yes | P1 | done |
| `workspace.label.assignment.set.request` | Assign / unassign a label on a workspace; assigning makes a label the catalog lacks | yes | P1 | done |
| `workspace.label.update.request`, `.delete.request`, `.delete.inspect.request` | Edit the catalog | | P2 | done |

## 7. Terminals

Terminal I/O after `subscribe_terminal_request` travels in binary frames tagged with the
returned `slot`: `0x01` output, `0x02` input, `0x03` resize (JSON), `0x04` snapshot (JSON
state), `0x05` restore. Slots are per socket.

| Message | Purpose | C | Tier |
|---|---|---|---|
| `subscribe_terminals_request` → `terminals_changed` (repeats) | Watch a cwd/workspace's terminal list | yes | P1 |
| `unsubscribe_terminals_request` | Stop watching (no reply) | | P1 |
| `create_terminal_request` | Spawn a PTY (`cwd`, `command?`, `args?`, `size`) | yes | P1 |
| `subscribe_terminal_request` | Attach; reply carries `slot`; `restore` mode `live`/`visible-snapshot`/`full-snapshot` | yes | P1 |
| `unsubscribe_terminal_request` | Detach (no reply) | | P1 |
| `terminal_input` | JSON input / resize / mouse (fire-and-forget) | yes | P1 |
| `kill_terminal_request` | Kill | yes | P1 |
| `terminal_stream_exit` (D→C) | The attached terminal exited | yes | P1 |
| `list_terminals_request`, `terminal.rename.request`, `capture_terminal_request` | List, rename, capture text | | P2 |
| `terminal_attention_required` (D→C) | Bell / attention | | P2 |

## 8. Providers, models, modes, agent configuration

| Message | Purpose | C | Tier |
|---|---|---|---|
| `get_providers_snapshot_request` | Providers with modes, models, thinking options; `compactSnapshot` when the client has `compact_provider_snapshots` | yes | P1 |
| `providers_snapshot_update` (D→C) | Snapshot changed (event subscription) | | P1 |
| `refresh_providers_snapshot_request` | Force a re-probe | | P1 |
| `set_agent_mode_request`, `set_agent_model_request`, `set_agent_thinking_request` | Switch mode / model / thinking level | yes | P1 |
| `set_agent_feature_request`, `agent.config.apply.request` | Feature toggles; bundled config change | | P2 |
| `list_provider_models_request`, `list_provider_modes_request`, `list_provider_features_request`, `list_available_providers_request` | Per-provider lists (pre-snapshot API) | | P2 |
| `provider_diagnostic_request` | Diagnose a provider install | | P2 |

## 9. Usage

| Message | Purpose | C | Tier |
|---|---|---|---|
| `usage.list_reports.request` → `usage.list_reports.update`* then `.response` | Per-account usage reports (feature `usageSources`) | yes | P1 |
| `provider.usage.list.request` | Older flattened form (`providerUsageList`, `COMPAT` until 2027-03) | yes | P1 |

## 10. Files, git, diff, forge

| Message | Purpose | C | Tier |
|---|---|---|---|
| `checkout_status_request`, `checkout_status_update` (D→C) | Git status | | P2 |
| `checkout.diff.get.request`, `subscribe_checkout_diff_request` → `checkout_diff_update`, `unsubscribe_checkout_diff_request` | Diffs | | P2 |
| `checkout.refresh.request`, `checkout.discard_changes.request` | Refresh, discard | | P2 |
| `checkout_commit_request`, `checkout_merge_request`, `checkout_merge_from_base_request`, `checkout_pull_request`, `checkout_push_request` | Commit and sync | | P2 |
| `checkout_pr_create_request`, `checkout_pr_merge_request`, `checkout_pr_status_request`, `pull_request_timeline_request` | Pull requests | | P2 |
| `checkout.forge.set_auto_merge.request`, `checkout.github.set_auto_merge.request` (legacy) | Auto-merge | | P2 |
| `checkout.forge.get_check_details.request`, `checkout.github.get_check_details.request` (legacy) | CI check details | | P2 |
| `checkout.commits.list.request`, `checkout.commits.file_diff.request` | Commit list and per-file diff | | P2 |
| `branch_suggestions_request`, `validate_branch_request` | A new worktree's starting branches. Served | yes | P1 |
| `checkout_switch_branch_request`, `checkout.rename_branch.request` | Branches | | P2 |
| `stash_save_request`, `stash_pop_request`, `stash_list_request` | Stash | | P2 |
| `forge.search.request`, `github_search_request` (legacy) | Issue / PR search, for a PR's worktree. Not served: needs the forge services | | P2 |
| `file_explorer_request` (may answer with `0x10`/`0x11`/`0x12` binary file frames) | Browse and read files. Crusteo reads images with it (mode `file`, no binary frames); Burrow serves only the prompt images it keeps | yes | P1 |
| `fs.file.subscribe.request` → `fs.file.update`, `fs.file.unsubscribe.request` | Watch a file | | P2 |
| `fs.file.write.request` | Write with optimistic concurrency | | P2 |
| `fs.entry.create/rename/duplicate/delete.request` | File tree edits | | P2 |
| `file_download_token_request` | Token for an HTTP download | | P2 |
| `file.upload.request` (+ binary frames) | Upload an attachment | | P2 |

## 11. Automations and schedules

| Message | Purpose | C | Tier |
|---|---|---|---|
| `schedule/create`, `schedule/list`, `schedule/inspect`, `schedule/logs`, `schedule/pause`, `schedule/resume`, `schedule/delete`, `schedule/run-once`, `schedule/update` | Cron-scheduled prompts to an agent or a new agent; replies `schedule/<op>/response`. Done ([compatibility.md](compatibility.md#schedules)) | | P2 |
| `loop/run`, `loop/list`, `loop/inspect`, `loop/logs`, `loop/stop` | `COMPAT(agentLoops)`: parse, but Paseo has no handler and never replies | | — |
| `chat/create`, `chat/list`, `chat/inspect`, `chat/delete`, `chat/post`, `chat/read`, `chat/wait` | `COMPAT(chatRooms)`: same, no handler | | — |

## 12. Everything else

| Area | Messages | Tier |
|---|---|---|
| Hub (7) | `hub.management.daemon.connect/get_status/disconnect/permissions.update.request`, `hub.execution.agent.create/validate.request`, `hub.execution.control.request`; events `hub.execution.agent.update`, `hub.execution.agent.stream` | P2 |
| Voice (8 in, 9 out) | `voice_audio_chunk`, `abort_request`, `audio_played`, `set_voice_mode`, `dictation_stream_start/chunk/finish/cancel`; out `assistant_chunk`, `audio_output`, `transcription_result`, `voice_input_state`, `dictation_stream_ack/finish_accepted/partial/final/error` | P2 |
| Push (2) | `register_push_token` (no reply), `push.unregister.request`: done, pushes through Expo | P2 |
| Plugins (15) | `plugin.catalog.get`, `plugin.list`, `plugin.logs.get`, `plugin.directory.install/inspect`, `plugin.source.install/status/update/update.preview/update.apply`, `plugin.reload/enable/disable/remove`, `plugin.rpc.invoke` | P2 |
| Skills (5) | `agent.skills.get_status/reconcile/uninstall/save_selection/import_legacy_selection` | P2 |
| Browser host (2 + 1) | `browser.host.register.request`, `browser.automation.execute.response` (client's reply to the daemon-initiated `browser.automation.execute.request`) | P2 |

## 13. Feature flags

`server_info.features` has 83 flags in the schema; Paseo 0.11.2 sends 80. Clients gate on them
and tell the user to update the host when a flag is missing, so **a compatible daemon advertises
only what it implements**. Burrow sends `ownedSubscriptions`, `directorySync`,
`selectiveAgentTimeline`, `explicitEventSubscriptions`, `projectList`, `workspaceLabels`,
Paseo 0.12's `backgroundWorkspaces`, and for adding projects and workspaces `projectAdd`,
`stableProjectIdentity`, `projectCreateDirectory` and `workspaceMultiplicity`. It does not send
`workspaceRequestReceipts` (its receipts do not survive a restart) or `forgeSearch`. The flags
crusteo reads: `directorySync`, `projectList`, `workspaceLabels`, `providerSubagents`,
`usageSources`, `providerUsageList`, `backgroundWorkspaces`.

## 14. State Paseo keeps on disk

Everything lives under `$PASEO_HOME` (default `~/.paseo`), as JSON validated with Zod, mostly
written atomically (temp file + rename). There is no migration framework; fields are added as
optional. From `docs/data-model.md`:

| Path | Holds |
|---|---|
| `config.json` | Daemon config: listen address, allowed hostnames, password hash, relay, MCP, provider overrides and custom providers, terminal and agent profiles, worktree root, speech, logging |
| `server-id` | Daemon identity, `srv_<base64url>` (`PASEO_SERVER_ID` overrides) |
| `daemon-keypair.json` | Relay E2EE keypair (mode 0600) |
| `paseo.pid` | PID lock: one daemon per home |
| `local-credential` | Per-run local client credential (0600), deleted on shutdown |
| `daemon.log` | Pino log |
| `agents/{sanitized-cwd}/{agentId}.json` | One record per agent: provider, cwd, `workspaceId`, title, labels, last status and mode, config, runtime info, provider persistence handle (resume token), attention, `archivedAt` |
| `projects/projects.json` | Project registry: `prj_<16 hex>` ids, root, kind, names, `projectKey`, `archivedAt` |
| `projects/workspaces.json` | Workspace registry: `wks_<hex>` ids, `cwd`, kind, names, branch, worktree root, base branch, ownership, pin, labels |
| `projects/workspace-labels.json` (+ `.transaction.json`) | Label catalog and a two-phase commit marker for renames |
| `projects/icons/` | Custom project icons |
| `schedules/{id}.json` | Schedules with cadence, target and run history |
| `runtime/managed-processes/{id}.json` | Helper processes to reconcile on boot |
| `plugins/sources.json`, `plugins/{id}/{uuid}/` | Managed plugins |
| `push-tokens.json` | Expo push tokens |
| `agent-requests/{sha256}.json` | Idempotency receipts for sends; never pruned |
| `creations/` | Idempotency records for agent and workspace creation; never pruned |
| `hub-relationship.json` | Hub link state |
| `uploads/{id}/` | Uploaded attachments |
| `runtime/opencode/`, `opencode-home/` | OpenCode bridge plugin and isolated home |
| `worktrees/{projectHash}/{slug}` (root: `worktrees.root`) | Paseo-owned git worktrees; collisions get `-1`, `-2`, ... Burrow uses the same layout under its own home |

Timelines are not in this list. Production uses `InMemoryAgentTimelineStore`, and bootstrap
deletes any old `agent-timelines/` directory on start. Agents load lazily after a restart. When
one loads, its timeline is rebuilt from the provider's own history through
`session.streamHistory()`: Claude's `~/.claude/projects/<dir>/<session>.jsonl`, Codex
`thread/read`. The rebuild gets a **new epoch**, so every client cursor goes stale. Terminals
are runtime-only and do not survive a restart. Log rotation (10 MB × 3) happens in the
supervisor process.
