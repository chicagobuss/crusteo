# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-11

The first release without a pre-release tag. Highlights:

- **Agents can message agents on other hosts**, through Reef, once you turn it on (off by
  default): prompts, follow-ups by id, and the fleet's hosts and agents in the agent tools.
- **Burrow serves workspace terminals and session import**, so Paseo's app has both against it.
- **A light theme** for the desktop app (#1).
- Extensions can offer settings and right-click menus, and a scrolling box inside a conversation
  scrolls on its own.

Before you rely on it:

- **Burrow has no passwords.** Keep it on loopback or a private network such as a VPN.
- **Messaging between hosts adds no reach beyond Reef's:** an agent reaches only the hosts Reef
  already connects to, and only through the agent tools Reef allows.
- **Terminals end when Burrow stops,** so upgrading or restarting Burrow closes open terminals.
- **Session import lists Claude Code and Codex sessions only.**
- **Paseo's app against Burrow** still lacks diffs, file editing, voice, connected hosts and relay
  pairing. See docs/burrow/compatibility.md.

### Added

- **A light theme for the desktop app** (#1). Settings → Window → Theme: Match system (the
  default), Light or Dark. Match system follows the system's setting, and switches when it does;
  a switch repaints every window at once, popped-out ones included. The light palette is
  Paseo's, with terminal colours, dialog shadows and scroll fades made to read on white.
- **Agents can message agents on other hosts, through Reef**, once you turn it on: a new
  setting, "Agents can message agents on other hosts", in Skiff's and crusteo's settings, off by
  default and for the whole fleet. With it on, an agent's `send_agent_prompt` for an agent its
  Burrow does not have goes to the Reef, which finds it on any host by id, a unique id prefix
  (seven characters or more) or its name, and delivers it there as a local prompt would be:
  steered into a running turn where the provider can, else queued behind it, never cut off unless
  asked. The receipt names the target's host. When the turn ends, errors or needs permission, the
  sender gets the same finish notice a local child sends, with a `Host:` line. Reef keeps each
  route under way in `routes.json` until the sender's Burrow has the notice, so a restart of
  either Burrow or of Reef loses none. A name several agents share is refused with the
  candidates listed. A pair of agents may route 20 prompts in ten minutes. With the setting off,
  or no Reef connected, nothing changes, and the sender is told which, and what would help.
  With it on, the other agent tools reach other hosts too. `list_hosts` lists the fleet's
  hosts. `list_agents` with `fleet: true`, and `burrow ls --fleet`, list every host's agents,
  each with its host. `get_agent_status`, `get_agent_activity`, `cancel_agent` and
  `respond_to_permission` work on an agent anywhere by its id or name, so the follow-ups a
  finish notice points to (a cut-off reply, a waiting permission) need no host. Nothing that
  creates, archives or reconfigures an agent goes across, and Reef's allowlist enforces that.
  **It is not a lock**: Burrow has no passwords, so anything that reaches a host can prompt its
  agents with the setting on or off; routing adds no reach beyond Reef's own. Reef also answers
  `GET`/`POST /api/settings` for a desktop in reef mode.

- **Extensions can offer settings:** a Rust extension's `Contributions::settings` show in
  Settings under Extensions, one line of text each (such as a service's address), with Change to
  edit or clear it. crusteo keeps them in `integrations.json`, leaving its other keys as they are,
  and tells the extension, so a new value can apply without a restart.

- **Toolbar items can have a right-click menu:** a `ToolbarItem`'s `menu()` entries come first
  when the item is right-clicked, above the bars' own (Customize toolbars…), in any bar,
  the title bar included. `crusteo::extensions::open_settings` opens Settings at its Extensions
  section, for an entry such as "… settings".

- **Terminals on Burrow hosts.** Burrow runs workspace terminals, as Paseo's daemon does, so
  crusteo's terminals (and Paseo's app's) work on hosts that moved to Burrow. Before, they
  worked only on hosts still running Paseo's daemon.
  - A terminal is a PTY on the host, running the user's shell (or a command) in the
    workspace's folder.
  - Clients list and watch a workspace's terminals, make them, attach to them, type into them,
    resize, rename, read them back and kill them.
  - Attaching brings back the screen as it is, history and modes included (from crusteo's own
    emulator, serialized), then the output as it comes. Burrow answers the program's own
    queries (cursor position, device attributes).
  - A socket that falls behind gets a fresh screen once it catches up, rather than everything
    it missed.
  - The client that claimed a terminal's size keeps it.
  - Archiving a workspace closes its terminals. Burrow advertises `workspaceTerminals`,
    `terminal-restore-modes` and `terminal-size-ownership`.
  - **Terminals do not survive a restart of Burrow**; an upgrade closes them.
  - crusteo now keeps a terminal's frames that arrive before the reply naming their slot,
    which a fast host can send.

- **Project suggestions for Paseo 0.15's app:** Burrow answers `project.suggestions.request` and
  advertises `projectSuggestions`. The app's first-run setup lists the folders agents already
  ran in on that host, from Claude Code's transcripts and Codex's threads of the last 30 days. A
  worktree or subfolder counts toward its repository's main checkout. Projects already added,
  the home folder and title-only sessions are left out.

- **Import a session, for Paseo's app:** Burrow answers `fetch_recent_provider_sessions_request`
  and `import_agent_request`, and advertises `importSessionWorkspaceTarget` and
  `importSessionSearch`. The app's Import session sheet lists the Claude Code and Codex sessions
  on that host, with their names and first and last prompts, for a folder or a search, and
  leaves out those an agent already has. Importing one makes an agent in the folder's workspace
  (made when there is none) with the session's conversation as its timeline; its first prompt
  resumes the session. An archived agent of the session comes back in place of a new one. Other
  providers' sessions are not listed.

- **Agents by workspace label:** `burrow ls --label billing` lists the agents whose workspace has
  that label (several `--label`s must all match; `--label key=value` matches an agent's own
  label), and the agent tool `list_agents` takes `workspaceLabels` the same way, across every
  directory, with each agent's `workspaceLabels` in the result. Before, only an agent's own
  `key=value` labels could be matched, and workspace labels are the ones the app shows.

- **Agents can run `burrow`:** Burrow puts its own folder on each agent's `PATH` when no `burrow`
  is on it, as it does for the Paseo app's `paseo`. A Burrow installed outside `PATH` (as in
  `~/burrow/burrow`) left its agents with no `burrow` command.

- **`burrow run`, `burrow wait` and `burrow logs`**, with `paseo run`'s flags (`--provider
  codex/gpt-5.4`, `--model`, `--thinking`, `--mode`, `--title`, `--cwd`, `--workspace`, `--env`,
  `--label`, `--no-wait`, `--wait-timeout 5m`), so an agent needs only Burrow's CLI to start a
  helper, wait for it and read what it did. Run from an agent, the new one is its child in its
  workspace. `run` and `wait` exit 2 when the agent waits on a permission and 3 when it is still
  working at the limit, as `send` does.

- **Restart a host from crusteo:** the Hosts list has Restart beside Reconnect. Burrow answers
  Paseo's `restart_server_request`: it stops as it does on SIGTERM, then starts its own program
  again in the same process, so systemd, launchd or an init script sees no change, and an upgraded
  program on disk is the one that starts. With agents mid-turn, crusteo says how many it would
  interrupt and asks first. A Burrow from before this says it does not know the request.

- On Linux, every coding-agent CLI Burrow starts gets the **desktop's session bus as it is at
  that moment**, so a CLI that keeps its login in the desktop keyring (Antigravity's `agy`)
  can read it under a Burrow started at boot by a service manager, and after a new desktop login.
  Burrow's own `DBUS_SESSION_BUS_ADDRESS` while its socket is there, else systemd's per-user bus,
  else the one the user's running keyring or desktop session uses. Before, such a Burrow had none
  (or a dead one), and `agy` failed until the service was given the address by hand. `burrow
  status` on the host says which bus the CLIs get, or that there is none until someone logs in to
  the desktop.

- **Hosts through SSH**, as Paseo's desktop app and CLI reach them. A host's address can be
  `ssh://[user@]host[:port]` (`?daemonPort=N` when its daemon is not on 6767). It works in the
  desktop's Hosts screen, Reef's `hosts.json` and a phone's Add host, and `burrow --host`. Each
  connection runs the system `ssh` with Paseo's arguments (`-W 127.0.0.1:<daemonPort>`,
  `BatchMode`, no forwardings), with the keys, agent and `~/.ssh/config` of whoever runs it. That
  covers aliases, `ProxyJump` and ports, and reaches a daemon that listens only on its host's
  loopback with no port opened or tunnel kept up. A host it cannot reach shows what ssh said
  ("Permission denied (publickey)", "stdio forwarding failed"). Each reconnect starts ssh again,
  and closing the connection ends it.

- History has **Grep** and **Sort**. Grep narrows the docked History as you type, to agents whose
  name, workspace, host, tool or model, state, labels (`key=value` or the key alone), question or
  error line, or latest reply holds every word typed; the header says "12 of 136", Esc empties the
  box, and Ctrl+G (Cmd+G) docks History and puts the cursor in it. Sort orders it by Recent (the
  latest change, as before), Needs you first, Last prompt, Newest agent, Oldest agent or Name A–Z,
  and is kept across restarts.
- Burrow sends each agent's latest reply with the agent (`lastReplyPreview`, the 220-character
  preview its notifications use), updated as each turn ends, so History shows and greps every
  agent's reply without reading each one's conversation. A Burrow started after this upgrade
  fills it in once for agents it already had, from their saved conversations, without counting
  that as a change to them. Agents on Paseo's own daemon, which sends none, still have their
  replies read as before.
- Skiff has **micro usage dials**, to try on a device with `?usage=micro` (`?usage=dial` goes
  back). Up to four accounts share the top bar's room in a 2×2 block, each dial showing two
  numbers. The needle is how fast the account is burning: up is on pace, left is room to spare,
  right is too fast, moving as far for ten times the room as for ten times too fast. The centre's
  colour, solid at the middle and fading out before the rim, is how much is left in that window:
  blue when full, then green, yellow, orange and red, to dark red when spent; indigo means
  nothing in it is used yet. The rim is in the account's colour. In the usage drawer, a button at
  the end of each account's heading, in its dial's colour while it shows, slides open that
  account's settings: whether it shows (four at most), its place in the block, and its colour.
  These choices belong to that device.
- Burrow asks Claude Code which models it offers, so a new Claude model needs no Burrow release
  and no restart. At discovery (start, a change of the `claude` file, the hourly probe) it starts
  the CLI in print mode with `--no-session-persistence` and the user's hooks turned off (a
  `SessionStart` hook would otherwise count every probe as a session), sends an `initialize` request and no
  message, and reads the `models` of the reply: labels, thinking levels and the default come from
  the CLI, and an alias such as `opus` or `fable` is offered under the model it resolves to
  (`claude-fable-5-1`), once. Models Burrow's table does not know get a 200k context window until
  it does. The built-in list stays as the fallback when the CLI does not answer.
- Providers snapshot entries for Claude and Codex carry the CLI's `version` and `installKind`
  (`native`, `package` or `unknown`), and each running agent carries `cli`: the version its
  process runs and whether a newer one is installed, which it switches to at its next prompt
  when idle. Paseo's apps ignore these fields.
- Reef tells Skiff when a coding-agent CLI upgrades or a model appears, once for the fleet: one
  notification per new version ("Claude Code 2.1.320, on 7 of 10 hosts · new: Fable 5.2") that
  is sent again under the same id as more hosts upgrade, two minutes after the first change. A
  model that appears with no version change is announced on its own. A setting, **Notify on CLI
  updates and new models**, turns it off. Skiff's Hosts sheet lists each host's CLIs, marking
  those behind the newest version in the fleet. The desktop app shows the same as a toast and
  on its Hosts screen.
- Skiff's map has bands when grouped by combination too, as crusteo's Lanes does: the raised
  labels an agent has, together, are its band ("billing", or "billing + metrics" when both are
  raised), holding one lane of its other labels' combination, or "just" the band. A combination's
  box offers each of its labels to raise ("Raise billing", "Raise metrics"), a band's lane lowers
  every label of its band, and the Bands menu shows by combination as by label. Before, bands
  existed only by label, and a combination's box offered nothing.
- Skiff's notification cards can be pinned, so one stays until you dismiss it, and dismissed with
  the x at their top right or a sideways swipe; a tap still opens the agent. A thin line along a
  card's top drains as its seven seconds run out, and stops while a finger is on it. Pinned cards
  survive a fifth arriving; Skiff's own banners are the same cards.
- Rename an agent from Skiff: the agent's details have a Rename button beside its name, which
  becomes the name to edit (Enter or Save renames it, Esc keeps it). Reef sends the daemon one
  `update_agent_request` with the new name alone, as crusteo's rename does, checked field by
  field like the labels one; a blank name or one over 200 characters is refused before anything
  is sent. The new name shows at once.
- `burrow status` asks the running Burrow what it is: version, pid, providers and agent counts,
  and on Linux where its log goes (a file, the journal, or a closed terminal). It connects and says hello as a client does, so a Burrow whose
  port answers but whose sessions fail shows as failing (exit 1). It finds Burrow through Paseo's
  daemon lock, `--listen` or the default; `--host` or `PASEO_HOST` names it.
- Burrow's own agent commands, for hosts without Paseo's CLI and for agents that look for one:
  `burrow ls` (`-a` adds archived ones), `inspect`, `send` (waits for the reply and prints the
  agent's last message; `--no-wait`, `--timeout`, `--prompt-file`), `rename`, `label key=value`,
  `stop` and `archive` (which stops a running agent first, as archiving one mid-turn does not
  stick). An agent is its id, a unique prefix of it, or its name; an ambiguous prefix lists the
  matches. They talk to this host's Burrow, or to another with `--host <host[:port]>` (port 6767
  when left out) or `PASEO_HOST`; `--json` prints snapshots. `burrow --help` lists them first,
  then what Paseo's CLI is still for, and why Burrow should run under a service manager.
- A prompt for an agent mid-turn whose provider cannot take a message mid-turn (Cursor, OpenCode,
  Gemini, Antigravity) can wait behind the turn instead of cutting it off: it starts when the turn
  ends, in order, up to 20 at a time. That is now the default for prompts from agents (the
  `send_agent_prompt` tool and `burrow send`), for finish notices and for schedules; agents whose
  provider can be steered are still steered. `activeTurnBehavior` gains `queue` (always wait) and
  `auto` (the new default); `steer` keeps Paseo's meaning, falling back to interrupting, and a
  prompt from a person (Paseo's apps, crusteo, Skiff) still replaces the turn unless it asks
  otherwise. `burrow send` has `--queue`, `--interrupt` and `--steer`. Stopping or archiving the
  agent drops what waits.
- `send_agent_prompt`'s receipt says what became of the prompt (`disposition`: started, steered,
  queued with its `queuePosition`, or interrupting) and who got it (`target`: id, title and host),
  and a queued prompt's finish notice comes when its own turn ends, with that turn's reply. The
  protocol's `send_agent_message_response` carries the disposition, message id and turn id, and
  `wait_for_finish_request` takes a `turnId` or a queued prompt's `messageId` to wait for that
  turn rather than whichever is running.
- Reef's recording puts every record's searchable text in one field, `body`: a reply's `text`, a
  tool call's name, input and output, a finished turn's prompt and reply, a to-do list's items.
  An index that searches `body` reaches them all, where one that searched `text` reached about a
  seventh of the records. It is scrubbed as the other fields are and leaves out what `redact.drop`
  drops (no tool output in it while `tool_output` is dropped). The Elasticsearch example through
  Vector maps it as text.
- A recording sink can start `from: "start"`: an agent it has no cursor for is read from the
  beginning of its timeline, as far back as the daemon holds it, so a search index made today holds
  yesterday's work. It is read a few agents at a time, and waits while the sink is far behind. A
  sink without `from` starts at each agent's end, as before. Archived agents are left out.
- Reef's recording tells each sink when an agent is deleted: a `forget` record, sent whatever the
  sink's `kinds` say, after which the sink's cursor for it goes. A daemon says `remove` for an
  archived agent as well as a deleted one, so Reef asks the daemon for the agent by id, and only
  one it no longer has is forgotten; archiving sends nothing. A deletion Reef missed, because the
  host was down or Reef was off, is caught up when Reef connects to the host.
- Recording has an `http` sink: Reef POSTs each batch of `reef-sink/1` to a URL, with the token
  from the environment variable `token_env` names as `Authorization: Bearer`, for a plugin that is
  not on Reef's machine. Any `2xx` is the ack; a `429` or `503` with `Retry-After` is a retry; any
  other answer is logged and tried again after a pause that grows to a minute, and nothing is
  dropped. See docs/reef/record.md.

### Changed

- **Hosts slides in as a pane** over any layout, as Settings does, sized for its list, instead of
  taking over the Paseo layout's whole screen with its buttons far from the hosts' names.

- Skiff's views go round: the map, the current conversation (the one last opened, from the map or
  the list), the list, and back to the map. A swipe left goes on round and a swipe right goes back
  round, so a swipe left on the map reopens the current conversation, one on the list goes to the
  map, and a swipe right on the list reopens the conversation. With no conversation opened yet,
  or its agent gone from the map, it is the map and the list. A trackpad's two-finger swipe does
  the same. In a lane a swipe right still goes back, and a conversation's back button still goes
  back to where it was opened from.
- Skiff puts the filters panel away while a conversation is open, however it was opened, and
  brings it back with the map or the list, which its filters are for.
- The docked History's Grep box and Sort sit in its header row, beside the count, rather than in
  a row of their own under it, so the list starts a row higher.
- Builds between releases call themselves `0.2.0-internal-<commit>` (`crusteo --version`, About, the
  settings screen, `burrow --version` and `reef --version`) rather than the last release's version
  with the commit beside it; a release still says `0.2.0 (<commit>)`. A Windows build from a
  checkout whose git directory Windows' git cannot open (a worktree made from WSL) now finds its
  commit from git's files rather than saying "unknown".
- Such a build of Burrow tells clients it is `0.2.0-internal-<commit>-burrow`, so `burrow status`,
  Reef's Hosts screen and Paseo's apps can tell two of them apart; they all said
  `0.2.0-internal-burrow`. A release still says `0.2.0-burrow`.

### Fixed

- **A scrolling box inside a conversation scrolls alone.** The wheel over a tool's details, or
  any box with its own scroll inside a conversation or a lane, moved the box and the
  conversation (or the lane) together. Now the box scrolls until it reaches its end, and only
  then does the conversation or lane behind it, as in a browser.

- **Burrow no longer signs Claude out.** Its model check (at start, and hourly) killed `claude`
  the moment it had listed its models. A CLI that starts after its sign-in has expired renews it
  first, and one killed mid-renewal left the old refresh token spent and the new one unsaved:
  every Claude agent on the host then failed with "OAuth session expired and could not be
  refreshed" until someone signed in again. It struck hosts where no agent had run for longer
  than a sign-in lasts, where the check was the first to renew it. The check now closes the CLI's
  input and lets it exit by itself, which takes a third of a second; it is killed only if it is
  still running a minute later.
- **Burrow listens as soon as it starts**, and finds its providers meanwhile. It used to listen
  only once every provider had said what it offers, which takes over 30 seconds on a host where
  one is slow (Antigravity timing out, pi still loading its models), so an upgrade that waited 30
  seconds for the port put the old binary back though the new one was fine. Each provider now
  joins the providers snapshot as it is found, pushed to `providers_snapshot_update` subscribers.
  Creating an agent, sending to one, or changing its mode, model or thinking while its provider
  is still being found waits for it rather than failing. `burrow status` and
  `daemon.get_status` say "not looked for yet" for a provider not reached yet.
- The macOS upgrade script (`upgrade-macos.sh`) gives a started Burrow 90 seconds to listen
  before it puts `burrow.prev` back (`BURROW_LISTEN_WAIT_SECONDS` to change it), and its log says
  how long it took.
- A recording sink's cursor never goes back when another sink, further behind, makes Reef read an
  agent again from its own place; it could be put back and sent records it had had. A turn's
  summary is also no longer sent again to a sink that is past the turn.
- A finish notice, `burrow send` and a schedule run report the end of the turn they started or
  joined, with that turn's reply. Before, they waited for the agent to go idle, so when another
  prompt started a turn straight after, they could report that turn and its reply instead.

- Burrow keeps serving when its standard output and error go away, as when it was started from a
  terminal that has since closed. Each log line it could not write made the logging library
  report the failure on stderr, which panicked once stderr was gone too: every new connection
  died on its first log line (Paseo's CLI saw "Transport closed (code 1006)") while the daemon
  and its agents kept running. A line that cannot be written is now dropped. Its log also has no
  colour codes when it goes to a file.
- An Antigravity agent's finished reply no longer ends in a "[System Error]" its run did not
  have. agy can end a turn whose reply has already come with an ERROR result carrying an old
  error (a 5 minute stream timeout reported after an 80 second turn its own transcript marks
  DONE). Burrow now keeps such a turn as completed, with agy's error as a separate note; an
  ERROR before the reply still fails the turn. A failed turn's "[System Error]" is a message of
  its own, so it no longer runs on from the reply's last line.

### Security

- Burrow's agent tools (its MCP server) no longer serve a client with no token. A request without
  one was a "top-level client" with the full tools, so any program on the host that found the
  loopback port could create and prompt agents. Such a client now sends the token Burrow writes to
  `~/.burrow/mcp-token` (readable by its user alone, kept across restarts) as `Authorization:
  Bearer`; `burrow mcp-bridge`, given an address and no token, reads it. Agents are unaffected:
  every provider hands each its own token, and none of Paseo's apps or CLI uses this server.
  `--mcp-tokenless` (`BURROW_MCP_TOKENLESS=1`) accepts a client with no token again, for a script
  not yet changed.
- Reef's `/api/hosts` and `/api/labels` take the device token in an `Authorization: Bearer` header,
  and the desktop in reef mode sends it there, so it no longer goes in a URL that a proxy's or
  Reef's own log keeps. `?t=` is still read on them for a desktop from before this, and a desktop
  asks again with it of a Reef that answers its header with a 401. `/image/` keeps `?t=`, as an
  `<img>` cannot send a header.

## [0.1.0-beta.6] - 2026-11-06

### Added

- Skiff shows which agents finished a turn you haven't looked at: a blue dot on their rows and
  boxes and a blue ring on their map tiles. "Unseen" is the host's "ready to review", so it is
  the same everywhere: opening an agent in Skiff marks it seen for crusteo too, and focusing its
  prompt in crusteo marks it seen for Skiff. A turn that ends while you have it open in Skiff is
  seen too.
- Skiff's list: every agent in one flat list over the map, looked at as Unseen (blocked agents
  first, then the unseen), Most recent or Blocked, through the same filters as the map. It comes
  in with the Map/List switch, a swipe left on the map, the handle on the map's right edge, a
  sideways two-finger trackpad swipe or L, and goes with the same the other way, Back or Esc. An
  agent opened from it comes back to it.
- Skiff shows subagents (agents another agent made through its agent tools), which it used to
  leave out: on the map and in the list, each with "↳ <parent>" under its name, a tap on which
  opens the parent.
- Skiff's agent details: a tap on the left of a conversation's header (its name and the lines
  under it) opens what the agent runs on (provider, model, effort, mode, how full its context
  is), where (host, workspace, project, its directory to copy), whose it is and the subagents it
  made (each opens with a tap), when it was made, last active, how long it has been working or
  its last turn took, its last error, its labels and bell, and its id to copy. The header shows
  the agent's state as a dot before its name instead of a word, and ↳ before its host when it
  is a subagent.
- A tap on Skiff's boat opens About: a line on what Skiff is, the Reef it talks to (its version and
  commit, its address), how many hosts are online and agents on the map, and the keys and
  gestures.

### Changed

- crusteo's History docks at the window's right instead of sliding in over the layout: the
  History button (or Ctrl+Shift+H, ⇧⌘H on a Mac) shows it beside whatever layout is up, which
  keeps scrolling on its own, until the button is pressed again. Its left edge drags its width.
  Both are kept across restarts. The counts' lists ("56 ready to review") still slide in.
  A click on an agent in the docked History shows it in the layout already up, which it does
  not leave: Fleet marks its card and scrolls to its host and to it, Board marks its
  workspace's card and scrolls to it, Lanes reveals its card and Paseo opens its tab. The row
  picked stays marked. Its rows name hosts as the rest of crusteo does, not by their ids.
- Skiff has one bar on top: its new boat icon (also the browser tab's), the host dots, the
  Map/List switch with the count of agents waiting on you, usage, the view's options, + and
  Settings. The Needs you and Agents tabs and the History button are gone: the list does what
  they did. The map's filters, grouping, bands and key are under the options button, and the
  filters apply to the list too.

### Fixed

- Paseo's desktop app connects to Burrow again. Burrow refused its WebSocket because of the
  origin it sends, `paseo://app`, which Paseo's own daemon always accepts; the app showed
  "Transport closed (code 1006)" or kept reconnecting. Burrow now accepts the origins Paseo's
  daemon does without being told: `paseo://app`, `https://app.paseo.sh`, and
  `http://localhost:<port>` and `http://127.0.0.1:<port>` on its own port.
- In Lanes, a band's name and its buttons (move, collapse, stop banding) stay in sight as you
  scroll along a band wider than the window: the name at the left of the part of the band in
  view and the buttons at its right, back at the band's own ends once those are in view. They
  used to sit on the band's first lane, and scrolled away with it.
- Text in a Skiff conversation can be selected and copied again (a long press on a phone): it
  opens over the map now, which turns selection off so a drag pans. Thoughts and tool calls can
  be selected too, their rows and what they unfold to; selecting is not a tap that folds them.
  While text is selected the conversation waits to redraw (a working agent's updates come once
  you let go), so the selection stays, and a swipe does not start.
- On Skiff's map, spreading two fingers (or a trackpad pinch) over an agent's box in a lane opens
  it again, and a closing conversation shrinks back into its box rather than into a corner.
- Archiving a workspace is saved before Burrow answers and before it removes the worktree, so
  a restart no longer brings the workspace back. A Burrow that has shut down saves nothing
  more, so a worktree setup still finishing cannot overwrite what the next start saved. On
  Windows a save waits out a file that a virus scanner or indexer holds open. A save that fails
  is tried again, and shutting down reports what could not be saved instead of only logging it.

## [0.1.0-beta.5] - 2026-11-06

### Added

- Text selected in a conversation takes the keyboard, and one blinking caret shows where keys
  go. Shift with the arrows, Home and End moves the selection's end (Ctrl, or Option on a Mac,
  a word at a time), within its turn; Ctrl/Cmd+A selects the turn, and Esc clears it and goes
  back to the message box. Typing or pasting goes back to the message box too. The selection
  that doesn't have the keyboard, in the conversation or the message box, stays in grey.
- Reef records the fleet: with `record.json` in its data dir it follows every agent and sends
  each prompt, reply, thought, tool call and finished turn, as `fleet-event/1` records scrubbed
  of keys, tokens and email addresses, to any number of sinks. A sink is a file a day of JSON
  lines, or a program Reef starts and feeds over stdin: one that acknowledges each batch
  (`reef-sink/1`), or any tool that reads JSON lines, such as Vector, which reaches
  Elasticsearch, S3 and R2, Kafka, Loki and more. Each sink has a cursor per agent, so after the
  sink, the store behind it or Reef itself was down, recording resumes from the daemon's
  timeline where it stopped. The file is read again on every change; `reef --demo --data-dir`
  records the demo fleet. See `docs/reef/record.md`; `packaging/reef/record/` has Vector into
  Elasticsearch, in Docker.
- Skiff's map can raise a label to a band, as crusteo's Lanes does: its agents sit together in
  one framed cell under the label's name, split into lanes by their other labels ("just <label>"
  for those with none), instead of in those labels' own lanes. Filters has a Bands choice that
  raises any label or lowers a raised one; a lane's heading has "Raise to band", and a band's
  lanes "Lower band". Bands are kept on the device, like its other map choices. Each band is a
  section of the map of its own, above the other lanes (beside them on a wide screen), and no
  lane is packed thinner than its name and a tile where the map has room.
- Skiff: History, every agent with the latest activity first, from a History button at the
  right of the tabs (after Filters) or a swipe left from a conversation. Its order holds still for five seconds at a time, so a row stays where it was while you
  reach for it: a line under the heading fills while a re-sort is coming, stops while you touch
  the list, and sorts now when tapped; rows slide to their new places.
- Skiff's map tiles show how fresh each agent is as well as its state: each tile is split on
  the diagonal, its state colour above and, below, white for just now darkening to black at two
  weeks on a log scale (a week is nearly black). A key in a strip above the map explains it; ×
  hides it and the new help button (?) beside Settings brings it back. Idle tiles are a muted
  blue rather than grey, to stand apart from the grey of age, and in the light theme the map's
  lanes are a stone grey, so the white of a fresh tile shows.

### Changed

- Skiff's map: a group opened from the map is a grid of boxes, two across on a phone, the most
  recently active agent first (top left), strictly by activity. Each box has the agent's name,
  how it is and since when ("Done · 3m"), and its host, in its tile's state colour with its age
  along the bottom; one waiting on you has an amber outline. The boxes hold their order as
  Recent's rows do: they move once the line under the group's heading has filled, never under a
  finger, and tapping the line sorts now (one shared implementation for both).
- The map's group names sit on a strip of the label's colour, dark text in light mode and light
  in dark, with the count beside them; a name too long for its group wraps to two lines instead
  of being cut short.
- Skiff: a swipe right from the map's open card goes all the way out to the whole map, rather
  than to its lane, and the agent's tile pulses to show where it was.
- Lanes: a card's last line keeps how long ago its agent was active at its right edge when it
  shows a status ("Finished", "Needs permission") or the agent's last reply, rather than losing
  it.

### Fixed

- Burrow: a prompt to a Claude agent no longer ends its turn at once, with a "finished"
  notification in crusteo and Skiff as it starts and the work then shown as a turn of its own.
  When a background task's notification woke Claude Code just as the prompt arrived, the
  notification's query ran first and its result was taken for the prompt's. Burrow now starts
  Claude with `--replay-user-messages` and follows the prompt through Claude's queue
  (`command_lifecycle`), so only the prompt's own result ends its turn.
- The Reef and Burrow archives for macOS and Linux build again: the packaging script copied
  `packaging/<component>/` with `install`, which stopped at `packaging/reef/record/`, the
  Vector example. Subfolders are copied now, and the Windows zip has the example's files too
  rather than an empty folder.
- Skiff no longer shows another agent's thinking. A thought's text, once opened, was kept by
  its id alone, and every agent's timeline numbers its thoughts the same way (`thought_1`,
  `thought_2`, ...), so opening a second agent showed the first one's thought under the same
  number. Opened details now belong to the open agent and are dropped when another opens, and a
  thought opened while it was still streaming is read again once it has grown. Reef's demo fleet
  gives each agent a thought of its own.
- Burrow no longer offers a provider that cannot run. An ACP agent whose probe has never
  succeeded since Burrow started, such as Gemini's CLI signed in with a personal Google account
  ("This client is no longer supported for Gemini Code Assist for individuals"), is listed as
  `error` with that reason: crusteo's New agent dialog leaves it out, Skiff names it under "Not
  ready here" with the reason, and creating an agent on it is refused at once instead of failing
  at its first prompt. A provider that has probed well once stays offered through later
  failures. A CLI that cannot even start no longer leaves its provider "loading" for good.
- Burrow: `/clear` on a Claude agent empties its context meter, rather than leaving the cleared
  conversation's size until the next reply.
- Burrow: a message that interrupts a busy Claude agent (as another agent's send does) no longer
  shows a `[System Error] … error_during_execution` and marks the new turn failed while the agent
  works on. That error was the interrupted turn's own aborted result, arriving after the new
  prompt; it is now dropped.
- Burrow no longer fills the temporary directory with OpenCode's native library. Burrow ran every
  provider's CLI for its `--version` each 5 minutes, and OpenCode, built with Bun, unpacks a 4 MB
  library into `/tmp` at each launch and never removes it: about 2 GB a day, in memory where
  `/tmp` is a RAM disk. A CLI is now run for its version only when its file changed, and
  OpenCode's probes and sessions unpack into Burrow's home (`tmp/bun`), where each probe clears
  copies over an hour old.
- Ctrl/Cmd+C after selecting text in a conversation copies it, rather than the message box's
  selection, or nothing, over the clipboard.

## [0.1.0-beta.4] - 2026-10-31

### Added

- Burrow gives Claude, Codex, pi, Cursor, OpenCode, Gemini and Antigravity agents Paseo's MCP
  tools (`mcp__paseo__create_agent`, `send_agent_prompt`, `create_heartbeat`, `list_agents`, …), 32
  of Paseo's 40, with Paseo 0.14's own catalogue of names, descriptions and schemas. The server
  listens only on loopback and knows each agent by a token of its own. An agent that makes or
  prompts another is told when that one finishes, errors or needs permission, as under Paseo.
  ACP agents get the tools too, which Paseo does not give them: over HTTP when the agent takes
  that, else through `burrow mcp-bridge` on its stdio. pi gets them from a small extension for
  that session only, leaving its `mcp.json` alone. Antigravity gets them from a `burrow` plugin
  Burrow keeps in `~/.gemini/config/plugins`, which has no tools when `agy` runs outside Burrow.
  Their MCP calls show as the tool they call ("List hosts"), not as "Other" or "call_mcp_tool".
- Burrow runs OpenCode (`opencode acp`) and Gemini CLI (`gemini --acp`) agents over the Agent
  Client Protocol, through the client that runs Cursor's: models and modes from the agent, tool
  calls and permissions. `--opencode-bin`/`--no-opencode` and `--gemini-bin`/`--no-gemini` choose
  them.
- A History button in the title bar (Ctrl/Cmd+Shift+H): every agent in one list, the latest
  change first, in the rows the state counts' lists use, each with its state and what it is
  about. It slides in from the edge nearest the button, or the right for the key.
- Lanes' one-line rows, under an open card and wherever cards show as one line, end with how long
  ago their agent last did something ("5h ago"), as a conversation's turns do.
- Burrow sends Paseo's notifications: when an agent finishes or needs permission, the one Paseo
  app that is in use is told to show a notification (`shouldNotify`, from its
  `client_heartbeat`s), and with no app in use, phones that registered get a push through Expo,
  as from Paseo's daemon; an app looking at that agent gets neither. Push tokens are leased for 48
  hours, dropped when Expo or the phone says so (`push.unregister.request`), kept in
  `push-tokens.json` in Paseo's format, and brought over by `import-paseo`. `--push-url` and
  `--no-push` change where they go.
- Burrow runs Paseo's schedules: `paseo schedule create/ls/inspect/logs/pause/resume/update/
  run-once/delete` and an agent's own `paseo heartbeat` work against it. A schedule sends its
  prompt every N minutes or on a five-field cron in any time zone, to an agent (as a system
  notice, shown as a heartbeat in its timeline) or to a new agent in a new workspace each time,
  archived after. Each slot runs once, a busy agent skips it, run limits and expiry end it, and
  so does archiving its agent. Schedules are kept in Paseo's format, and `import-paseo` brings
  Paseo's over.

### Changed

- Settings slides in as a pane over whatever layout is showing, instead of switching to the Paseo
  layout's Settings screen; closing it (Escape, ×, or a click outside) leaves the layout as it
  was. Opened from the Paseo sidebar it comes in from the left, from the menu or Ctrl/Cmd+, from
  the right. Its changes still apply as they are made, visible behind it.
- Usage bars keep what is left at the right, so use drains them from the left.

## [0.1.0-beta.3] - 2026-10-29

### Added

- Lanes can show any label as a band: from a lane header's right-click menu, the label's agents'
  lanes gather under a strip in its colour, split by their other labels, with "just *label*"
  for agents that have only that one. Each band moves as a unit, collapses into one tab, and can
  be put back to plain lanes from its strip. The label picker lists banded labels first.
- Skiff groups by combination, as crusteo's Combinations lanes do: each agent is in one group
  under all its labels ("Billing + infra"), so the map and the board show it once.
- A conversation shows when each step began (an agent's message and the tool calls under it)
  at the right of the step's last line, so it costs no height. A turn's last step leaves it to
  the turn's footer.
- Skiff shows how full an open agent's context is at the end of the line under its title
  ("Claude · Opus 5.5 · High · 42% context"), amber from 80% and red from 90%. crusteo's
  composer colours its "% context" the same way.
- Burrow serves whole-turn timeline pages (`wholeTurnTimeline`), which Paseo 0.14's app asks for
  to fold finished turns into summaries: a page of history never starts or ends partway into a
  turn.
- Burrow stops the CLI of an agent that has been idle for an hour, as Paseo 0.14 does, and the
  agent's next prompt resumes its session in a new one. An agent keeps its CLI while a turn,
  a question, a busy subagent or an ACP agent's terminal is still running.
- `upgrade-macos.sh` in Burrow's examples upgrades Burrow on a Mac from anywhere, an agent Burrow
  runs included: a one-off launchd job waits until no agent is mid-turn, swaps the binary (keeping
  `burrow.prev`, and putting it back if the new one does not listen) and signs it with your
  certificate. Signing with the same certificate and identifier keeps macOS's Local Network
  access across upgrades; an ad-hoc build is a new program to macOS, and its agents lose the LAN
  and VPN until access is allowed again. The install guide explains it.

### Changed

- Lane headers are a quarter shorter: a lane's state counts sit at the right end of its hosts'
  row instead of a row of their own, and the header's padding above and below is tighter. The
  host chips give way to the counts, from two chips to one to "N hosts", each naming its hosts
  on hover.
- Skiff's conversation header splits in two: the title and the two lines under it on the left,
  and on the right the agent's buttons, larger, with its labels beneath them. When the labels
  don't all fit, a chevron opens a full-width row with every one.
- Burrow starts Claude Code as `CLAUDE_CODE_ENTRYPOINT=Burrow` rather than `sdk-ts`, and clears
  the variables of a Claude session it was started from, as Paseo 0.14 does for itself. Paseo
  made the change because `sdk-ts` sessions used up subscription limits faster.
- Paseo 0.14.0-beta.2's CLI is checked against Burrow: every agent command works, as with 0.12
  and 0.13.
- Burrow folds timeline history as Paseo does: a tool call's updates are one entry, and so are
  an assistant message's or a reasoning block's chunks. A page of 40 now holds 40 of those
  rather than 40 raw rows, and a whole timeline (what `paseo logs` and `paseo wait` read) comes
  as a quarter to a third as many entries and about a third fewer bytes. Pages match Paseo
  0.14's, checked against its own code on real timelines. `projection: "canonical"` still
  gets one entry per row.

- A usage gauge's menu in crusteo, and each account in Skiff's usage panel, is a table: a
  heading per group of limits ("Gemini Models", "Fable", "All models"), a line per period
  ("weekly", "5h", "session") with what is left as a bar and a number, when it resets and its
  pace ("1.8× room", "1.6× too fast"), and a dot on the window the dial shows. The models a
  group covers, which usage feeds list with each limit, show on hover in crusteo and are left
  out of Skiff.
- The pace dials stretch their room side up to 99× on a log scale, so 10× and 30× to spare
  read apart; the too-fast side still stops at 10×.

### Fixed

- Sending a prompt in Skiff scrolls the conversation to the end, even if you'd scrolled up to
  read. The keyboard opening, the composer growing or an image loading no longer leave the end
  of the conversation out of sight.
- Skiff no longer shows the previous agent's conversation, thoughts included, under the title of
  the one just opened while that one loads. From a slow host it could read as the new agent's
  for seconds.
- Burrow's agents get what Paseo gives its agents: `PASEO_AGENT_ID`, `PASEO_AGENT_CWD`, and
  `PASEO_HOST` at Burrow's own address. Without them a `paseo` command run inside an agent said
  "Daemon is not running", a message one agent sent another arrived from nobody and cancelled the
  other's turn if it was busy, and `paseo run` made unparented agents in new workspaces.
  `paseo run --env` is applied too.
- A Burrow that has replaced Paseo on its port holds Paseo's daemon lock (`~/.paseo/paseo.pid`),
  so Paseo's CLI without `--host` (in a shell, a cron job, an agent's own script) finds it, and
  `paseo daemon start` no longer offers to start a second daemon. Never over a running Paseo's
  lock; `--no-paseo-lock` turns it off.
- Ending an agent's CLI ends its process tree and group, `SIGTERM` then `SIGKILL`, as Paseo does:
  its MCP servers, running tools and background tasks used to outlive it. Burrow ends its agents'
  CLIs before it exits.
- `paseo status`, `paseo daemon status`, `paseo provider models`, `paseo provider diagnostic`,
  `paseo daemon pair` (relay pairing off) and `paseo agent update --thinking` work against Burrow.
  Models in the providers snapshot carry their provider, as Paseo's schema requires.
- Providers no longer say they support MCP servers that Burrow does not pass on.
- `burrow import-paseo` gives a Burrow that has no server id yet Paseo's, so clients see the same
  host after the switch. It used to leave that to a manual copy; a Burrow first started without
  one made its own.
- A Codex agent's first message after Codex updated itself no longer fails with "Failed to resume
  Codex thread …: already has an active writer". Burrow restarts an idle agent on the new CLI at
  its next message; it now waits for the old CLI to let go of the thread first, and tries a
  resume that still finds the thread busy again a few times over about four seconds. A thread
  that stays open elsewhere (the Codex app, another daemon) fails the message with that reason.
- A Codex or Gemini account that a usage feed names one way ("personal") and a daemon another
  (its email) no longer shows two gauges when the two read it either side of a reset. The feed's
  `account_fingerprint` is matched against the one the daemon's email gives; until now only the
  numbers could match them, and those differ until both have read the reset.

## [0.1.0-beta.2] - 2026-10-27

### Added

- Burrow's `--listen` can be given more than once (or `BURROW_LISTEN` comma-separated), so it can
  serve loopback and a VPN's address and nothing else. An address that cannot be bound yet, such
  as a VPN's just after boot, is tried again every few seconds until it can.
- Images in a reply open full size on a click in crusteo, as a prompt's do, read from the host
  when it keeps the file.
- Skiff shows the images in an agent's replies, and a prompt's images sent from another client
  or loaded from history: Reef reads the files a Burrow keeps a prompt's images in, and the files
  a reply's images name, from the host. A tap opens one full size. Only files a conversation
  names are read, never a path a phone gives.

### Changed

- An image pasted into crusteo's composer is sent at most 2000 px on its longest side, as Skiff
  sends them, and as a JPEG when a PNG would be over 8 MB, so other clients can read it back
  from the daemon's copy.
- `burrow import-paseo` brings over the agents of every provider Burrow runs, not just Claude's:
  Codex, Cursor, pi and Antigravity agents keep their sessions, which their next prompt resumes,
  and Codex and pi agents their conversations too (Codex's read through `codex app-server`).
  Only OpenCode and custom ACP agents are left in `~/.paseo`. Run it again on a host that has
  switched to bring the rest over; what is already in Burrow stays as it is.

### Fixed

- An agent can message another agent on a Burrow host with Paseo 0.12's CLI (`paseo send` from
  inside an agent), which refused with "Update the Paseo host to send messages with agent
  provenance". Burrow takes the sender (`sourceAgentId`) as Paseo does: the agent gets the
  message in Paseo's envelope naming the sender, and the conversation shows it as from that
  agent. A child agent's first prompt from `paseo run` is shown the same way.
- Paseo's CLI commands that wait work against Burrow: `paseo send` (unless `--no-wait`),
  `paseo run` and `paseo wait` asked for `wait_for_finish_request`, which Burrow did not answer,
  so they failed right after sending. Burrow answers as Paseo does: when the turn ends, the agent
  asks for a permission, or the wait times out. `paseo delete --all` / `--cwd` (the agent
  history), `paseo detach`, and a send's `sessionMode` (Paseo 0.13) work too.
- Paseo's 0.13 app against a Burrow host: its sidebar's created and last-prompt sorts work (the
  app turned them off for every host when one lacked them), and its workspace setup panel shows
  a setup's progress instead of "Update the host to view workspace setup details". Workspaces
  carry when they were made (an older one takes its first agent's time), their agents' latest
  prompt, and their setup's status.
- Paseo's app could not start an agent on a Burrow host: its tool and model pickers stayed on
  "Loading..." because Burrow did not advertise `providersSnapshot`, which it already served.
- `paseo ls -a` (and any read asking for archived agents) now lists a Burrow's archived agents.
- `docs/burrow/compatibility.md` sets out how Burrow and Paseo work together in each direction,
  which versions were checked and how; `scripts/paseo-cli-compat.sh` runs Paseo's own CLI against
  a Burrow and reports each command.
- The open card's bar in Lanes, and a popped-out window's, always show the host: a long
  workspace name is cut short instead of pushing the host out of sight.
- Skiff: a thought's details wrap as prose instead of running off to the right.
- A tall image in a prompt, such as a phone screenshot, no longer draws down over the messages
  below it; it fits its thumbnail, and clicking anywhere on it opens it full size. The same goes
  for the composer's thumbnails.

## [0.1.0-beta.1] - 2026-10-26

### Added

- **New agent is one centered dialog**, wherever it starts: a lane's or a workspace's +, the
  workspace menus, the Paseo layout's tab row, Ctrl/Cmd+T and the usage chip. In Skiff's order:
  where it works (a workspace on a host, a new workspace, or a **new project**, found and added
  right there), the tool, model, mode and thinking, the agent's own labels in reef mode, and the
  first message (images can be pasted). Enter creates; Esc closes and keeps what was typed for
  next time. What an agent was made with on a host starts the next one there. It replaces the
  draft card in Lanes and the New Agent tab in the Paseo layout.
- **Burrow picks up CLI upgrades and new models without a restart.** It checks each coding
  agent's CLI version every 5 minutes, probes again for new models every hour and when a client
  asks (`refresh_providers_snapshot_request`), and tells clients what changed. New agents use the
  new CLI, and an idle agent switches to it at its next prompt.
- **Pop out** an open Lanes card into a window of its own (the button left of Focus on its bar):
  the conversation and its composer, in a window you can move, resize, snap beside another
  (Win+Left / Win+Right) or put on another screen. It has crusteo's own title bar: the agent's
  state and title (click the title to rename it), its workspace and host, its labels, a button
  that puts the card back in its lane, and its colour. The bar takes the colour of the agent's
  first label's lane, or one you pick from the colour dot (the open card's bar in Lanes then
  matches); the button beside it goes back to the label's. Each agent's window reopens where it
  was last. While it's out, its card in the lane is marked, and clicking it brings the window
  forward.
- **Labels belong to agents** with a Reef. Reef keeps the fleet's labels: each has an id that
  never changes, a name and a colour, made once from the labels your hosts already have (where
  hosts colour one differently, most hosts win), and labels made later on any host join it. An
  agent's own labels travel with it, on its host, so two agents in one workspace can be labelled
  differently; an agent without its own still shows its workspace's labels, as before. On the
  phone, an agent's Labels panel changes that agent alone, and a new agent starts with the labels
  the agents in its workspace share. A desktop on a Reef places agents in Lanes by their own
  labels: a card's label picker changes that agent, a workspace box's changes the agents in that
  box, and a draft started in a label's lane makes an agent with that label. The label filter
  and find work by agent too. Paseo's apps keep showing workspace labels, untouched.
- **Add project** and a real **New workspace**, in the Lanes draft composer and on the Paseo
  layout's New workspace screen. Add project (the Project picker's "Add project…", the sidebar's
  footer and empty state, or the command palette) picks a host, then searches that host's folders
  as you type: a name searches its home, a path such as `~/code/` lists what is inside; arrows pick,
  Tab looks inside, Enter adds. A folder that is not a git repository is added too, as Paseo adds
  it, and its workspaces open in the folder until `git init` is run there. New workspace chooses
  the project (on its own host, never simply the first one), Folder or Worktree, the branch (one
  you type, an existing one to check out, or, left blank, one named after the first prompt), and
  what the worktree starts from: the default branch, a branch, or an open pull or merge request on
  hosts that list them. A line under it says what will be made, such as "New worktree on new branch
  feat/x from origin/main on devbox". The workspace and its first agent are made together; a
  blank prompt makes the workspace alone. Each project remembers how its last workspace was made.
  A branch you name stays as named, where Paseo would rename it after the first agent's work.
  Problems show in the draft, which keeps what was typed; when the workspace is made but its agent
  fails, the draft stays, in the new workspace, and Enter tries the agent again. Hosts without
  Paseo's project and worktree requests get the folder workspace and a typed path. Drafts say
  "Host offline" rather than "Loading providers…" for a host that is gone.
- **Remove project**, from the sidebar project row's menu (its "⋮" or a right-click), the
  right-click menu of a Lanes lane grouped by project, or the command palette ("Remove project app
  (devbox)"), on hosts that can remove projects. It removes one host's project: an entry merged
  across hosts offers "Remove from devbox…" per host. A dialog says what will happen: the project
  leaves that host's list, its open workspaces are archived with their agents, and files,
  worktrees and branches stay on disk. While an agent in it is working or waiting for input, the
  dialog names them and asks to stop or archive them first, where Paseo would stop them. It also
  waits while the host is offline or still loading its workspaces and agents, after a reconnect
  too, so an agent that started meanwhile is not missed. A host's refusal shows in the dialog; the
  project leaves every view when the host says it is gone.
- `burrow` removes projects as Paseo does (`project.remove.request`, `projectRemove`): the
  project's open workspaces are archived with their agents, nothing on disk is touched, and the
  project is forgotten for good, so projects left over from testing no longer need deleting from
  `projects.json` by hand.
- `burrow` adds projects and makes workspaces as Paseo 0.12 does: projects git or not, with
  Paseo's project keys; workspaces for a folder or a new git worktree, checking out a branch or
  branching off a starting ref, with an initial agent; agents made with git options; `paseo.json`
  setup scripts; the folder search and the starting branches a new-project or new-workspace form
  reads; and archiving a workspace, which removes its worktree. Worktrees go in
  `<home>/worktrees/<project hash>/<slug>`. Pull requests cannot be checked out yet.
- The title bar's counts ("56 ready to review", "1 working", and the others) open a list of those
  agents, sliding in from the right under the title bar, the latest change first. Each row shows
  the agent, its host, workspace and model, how long ago it changed, and what it is about: the
  question or permission it waits on, the error it failed with, or the first line of its last
  reply. A click on a row opens the agent as a notification's click does. The list itself only
  reads, so it marks nothing as reviewed. Click the count again, press Escape or click outside to
  close it; **Show on board** singles the agents out on the Lanes board, as a click on the count
  used to.
- Conversations show images a daemon keeps as files: the image an agent's tool returned, which
  Paseo saves and names in its reply, and the images another client attached to a prompt, from a
  daemon that keeps them. Paseo keeps only a prompt's text, so those cannot be shown from a Paseo
  host, in the Paseo apps either. Each image is read from its host when its message comes into
  view, kept in memory, and shown as a placeholder while it loads or if the host cannot send it.
  A click on a prompt's image, including one sent from Crusteo, opens it full size.
- `burrow` keeps the images attached to a prompt, in its state folder, and serves them back, so
  Crusteo shows them whichever client sent them. Paseo's apps still show only the sender's.
- Conversations, on the desktop and in `reef`, show who sent a message when one agent
  messages another (Paseo 0.12): "From planner" and the message's first line, on the one-line row
  that was "Agent message"; a notice about another agent reads as a status line, such as "worker
  finished" and the first line of its reply. A message to another agent reads "To worker" and the
  prompt's first line. Clicking or tapping the agent's name opens it. Paseo's own tools are labelled
  as Paseo labels them ("Check agent", "Create terminal"), and prompts that still carry Paseo's
  agent-message tags, as replayed histories do, show the same way.
- Crusteo and `reef` show Paseo 0.12's background workspaces and their agents, which a 0.12
  daemon otherwise leaves out, tagged "background". Grouped by label, they have their own
  Background lane in Lanes, and Background group in Reef's list, board and map, after
  Unlabelled and only there, not also under their labels. The label filter has Background (and,
  in Lanes, Unlabelled) after the labels. "Show background" (on by default) hides them in every
  view, Background lane and choice included: in Crusteo it is in the sidebar's display menu, at
  the foot of the Lanes label filter, and in Settings; in Reef it is a Reef-wide setting, and
  each phone can choose otherwise in Filters.
- `burrow` has background workspaces as Paseo 0.12 does: `workspace.create` and `agent.create`
  can make one, it is saved with the workspace, and it shows only to clients that ask for it.
- `reef` can create, delete and relabel agents from the phone. The + in the top bar opens a
  new-agent form: pick a host, one of its ready providers, a model, mode and thinking level, one
  of its workspaces or any directory, labels, and the first message; the page remembers your last
  choices on each host and opens the new agent's conversation. A conversation shows the agent's
  labels and can add or remove them (they are its workspace's labels, as in Crusteo), and can
  delete the agent for good as well as archive it. Archive and delete now ask in the page's own
  confirmation sheet instead of the browser's dialog.
- `burrow`, an early version of Crusteo's own daemon. It runs next to Paseo's daemon, on its own
  port (6797) with its own state folder, and Crusteo, `reef` and the Paseo apps can connect
  to it. It runs Claude Code agents with your own `claude` install: start agents, send and steer
  messages, stop a turn, answer permission prompts and questions, and archive agents.
  Conversations survive a restart of the daemon without clients having to reload them.
- `burrow` also runs Codex agents with your own `codex` install, when it is on PATH: start them
  on any model Codex lists, approve or deny commands and file edits, answer its questions, steer
  or stop a turn, change mode, model and reasoning effort, and pick a conversation up again after
  the daemon restarts. `--codex-bin` names another binary and `--no-codex` turns it off.
- `burrow` also runs Antigravity agents with your own `agy` install, shown as `antigravity`
  with the models `agy models` lists, as Paseo 0.12 shows them. Agents start, answer, use tools,
  stop mid-turn, switch models and carry on after a restart. Antigravity cannot ask for
  permission when another app drives it, so every tool runs without asking, and each session
  says so. `--agy-bin` picks the binary and `--no-agy` turns it off.
- `burrow` also runs Cursor agents with your own `cursor-agent` install, over the Agent Client
  Protocol the way Paseo does: Cursor's models and modes, tool calls and plans, permission
  prompts, stopping a turn, and picking up the same Cursor session after a restart. `--cursor-bin`
  names another binary and `--no-cursor` turns it off. Other ACP agents can follow with little
  more than their command.
- `burrow` deletes agents and keeps workspace labels the way Paseo does, so deleting and
  relabelling agents from `reef`, and Crusteo's coloured label chips and label picker, work
  on Burrow hosts. Labels keep their colours, survive a restart, and can be renamed,
  recoloured and deleted from the Paseo apps.
- `burrow import-paseo` moves a host over from Paseo: it copies the Claude agents in `~/.paseo`
  into Burrow once, with their titles, labels and label colours, projects, workspaces and
  conversations, and their next message picks up the same Claude session. `--dry-run` shows what it would do, a second run
  adds nothing, and Paseo's own files are only read. Stop Burrow before running it.
- `burrow` also runs pi agents, with your own `pi` install and the models it is configured with:
  start agents, send and steer messages, change the model and thinking level, stop a turn, answer
  your pi extensions' questions, and pick the conversation back up after a restart. `--pi-bin`
  picks the binary and `--no-pi` turns pi off.
- Fleet and Board scroll and size their columns the way Lanes does: a column width pill with a
  slider and Fit columns, Cmd/Ctrl+wheel to resize, a plain wheel over the headers or Shift+wheel
  to scroll across, a middle-button pan or autoscroll, edge fades with a scrollbar on hover, and
  fades at the top and bottom of columns with more to scroll to. Each layout keeps its own width.
- Conversations fade at the top and bottom where there is more to scroll to, in every layout, and
  show an arrow back to the latest message while scrolled up from it.
- Your own messages can be selected and copied like the agent's replies, and have a copy button
  after the time they were sent. Copy buttons brighten under the pointer.
- `reef`, for running the fleet from a phone: an always-on process that keeps every host
  connected and serves a small mobile page with what needs you, the agent list, and any
  conversation. From the phone you can allow or deny permissions, answer questions, send messages
  (queued until a busy agent's turn ends, or steered in now), stop a turn and archive an agent, and
  get a notification when an agent's turn finishes, switchable per agent. The agent list groups by
  host, label, state or workspace, sorts with the most recently active first, and shows each
  agent's latest reply; on a wide screen the page shows the list and the conversation side by side,
  or a board of columns. Agents are grouped by label unless you choose otherwise, and labels show
  as chips in their colours, which also mark the groups, columns and map lanes. A Map tab shows
  every agent as a tile on a map shaped to your screen: pinch or tap into a lane to see its cards,
  and into a card to talk to that agent. Reply previews are plain text from the first line with
  words. The phone talks only to Reef, so it never holds the whole fleet. Phones sign in with a
  device token, or with none on a Reef started with `--open`, for networks where reaching it is lock
  enough. See `docs/reef/design.md`; `reef --demo` shows it with a made-up fleet.
- Reef: send images from the phone (the + in the composer, or paste), see images in a
  conversation's history (each fetched once, then kept by the browser), and add or remove hosts
  from Settings without a restart. Tables, lists, headings, italics and links in replies render. A
  big swipe right goes back a level. A lane's cards show agents waiting on you, then working ones,
  then the rest, newest first.
- Reef: account usage on the phone. The top bar shows the tightest account's pace dial, as
  crusteo's title bar does, and a tap opens every account with its plan, how much of each limit is
  used and when it resets, and a hint to use one before it resets. Reef asks the daemons for
  their usage reports itself, and `--usage <url>` adds a usage feed; phones never reach the feed.
  crusteo and Reef now read and merge usage with the same code, so they show the same
  accounts the same way.
- Reef: swiping back follows your finger: the conversation, card, lane or picture slides away to
  show what is under it, and goes if you let go past a third of the screen or flick, or springs
  back if not. The map stays on the lane you were in when the screen turns, and an open card hides
  the tabs.
- Releases ship `reef` and `burrow` beside the desktop app, each in an archive of its own
  for every platform: `reef-<version>-<target>` and `burrow-<version>-<target>`, static
  (musl) for Linux x86_64 and arm64, universal for macOS, and for Windows. Each archive has its
  install guide and example service files: a systemd unit for Reef with `hosts.json` and
  `reef-settings.json` examples, systemd user units for Burrow (through the login shell, or with
  the environment written out), and launchd agents for macOS. `SHA256SUMS.txt` covers them all.
- Install guides, `docs/reef/install.md` and `docs/burrow/install.md`: download, run as a service,
  upgrade and roll back; for Burrow also running next to Paseo, and replacing Paseo on a host
  with its server id and `burrow import-paseo`, and going back.
- Burrow runs on macOS: it has replaced Paseo on an Apple Silicon Mac through the launchd agent.
  The install guide now covers quitting Paseo's desktop app (which restarts its daemon) and its
  Login Item, the launchd steps for taking over 6767, and going back.
- `crusteo`, `reef` and `burrow` all have the release version, and print it with the
  commit they were built from in `--version` and in their first line of output. Burrow tells
  clients it is `<version>-burrow`.

### Changed

- Lanes: an open card goes to the top of its lane, outside its workspace's box, with its workspace
  and host in its bar. The lane's other cards line up under it, one line each and without
  workspace headers: those needing you first (a permission, a question, an error), then running
  ones, then the rest, newest first. Each row has its workspace in smaller text after its name;
  hover it for the host, model and folder too. The list shows
  a few rows and scrolls, with fades and a thin scrollbar; drag the open card's bottom edge to give
  it more room, double-click to put it back. Cards of the open card's workspace are outlined.
- Lanes and pop-out windows: hover an open card's workspace and host for the folder its agent
  works in and its branch, and click to copy the folder. A one-line row's hover shows the folder too.
- Lanes that hold more than they show have a thin scrollbar, always there while they do.
- A Lanes card's message box no longer keeps the transcript's prompt-index gutter on its left.
- Every saved host has an `id` in `hosts.json`, and what Crusteo remembers about a host (tabs, pins,
  card order, filters, how its last workspace was made) follows that id rather than its name. A host
  saved before takes its name as its id the first time, so nothing saved is lost; a host added
  later gets a generated one, so a new host with an old one's name starts fresh.
- New names: the daemon carapace is now **Burrow** (binary `burrow`, crates `burrow-*`), the hub
  crusteo-hub is now **Reef** (binary `reef`, crate `reef`), and the phone page it serves is
  **Skiff**; release archives, service files and install guides follow. What they left behind moves
  on first start: Burrow renames `~/.carapace` to `~/.burrow` (rewriting the paths in its records
  and reconnecting its worktrees), Reef renames its `hub` data folder to `reef-data` (not `reef`,
  the name of the binary beside it) and its `hub-*.json` files to `reef-*.json`, and Skiff moves a
  phone's saved token and choices, so hosts keep their identity and phones their tokens. For one release `CARAPACE_*` variables still work when the
  `BURROW_*` ones are not set, and `CRUSTEO_HUB_PAGE` when `REEF_PAGE` is not, each with a
  deprecation warning. The Paseo protocol and Skiff's protocol are unchanged. See "Upgrading from
  carapace" and "Upgrading from crusteo-hub" in the install guides.

### Fixed

- Reef reads a host's tools and models again as soon as the host says they changed (a CLI
  upgraded under its Burrow), so Skiff's new-agent form no longer shows them up to ten minutes
  old.
- Right-clicking a link in an agent's reply opens a menu to open it or copy its address.
- An agent a Paseo daemon had dropped could stay on the desktop (and in Reef) for good, and
  archiving or deleting it failed. The daemon numbers its changes for every client at once and
  doesn't push the ones another client's fetch finds, so following the numbers on pushes skipped
  them. Reconnecting now catches up from the last fetch, and such an agent goes then.
- A link to another agent in a conversation (who sent a message, or the agent a tool call acts
  on) could open, or take its title from, an agent with the same id on a different host, such as
  agents imported from Paseo into Burrow. It now looks only on the conversation's own host.
- A blank label, or the same label twice ("UX" and "ux"), from a daemon no longer shows as its own
  group; a blank one fell into "No label" in Reef.
- Reef: a hosts file with a host name that is empty, has a `/`, is over 64 characters or is used
  twice now stops Reef at start, naming the entry, instead of mixing up those hosts' agents.
- Reef: a turn stopped while its conversation was open did not count as ended until the agent
  list caught up, so messages queued behind it waited longer.
- Reef: a phone waking up could hold two connections to Reef and show each notification twice
  or more. Reconnecting no longer drops the open conversation or your place in it, and the
  composer's placeholder is no longer clipped.
- Reef: messages queued for a busy agent could stay stuck: if its turn ended while a message was
  still on its way, or a queued message was refused, the rest never went. They go now, a refused
  one stays at the front marked so you can retry or drop it, and a new message no longer jumps
  ahead of ones already waiting.
- Reef: images in a conversation did not load for a device token with a `+`, `/` or `=` in it.
- Reef: a web page elsewhere could reach a Reef through a DNS name it controlled. Reef now answers
  only to IP addresses, `localhost` and names given with `--host-name`; if you reach it by a DNS
  name (for example behind `tailscale serve`), pass that name.
- Replies from an agent that resumes on its own, as an orchestrator does, went without a time
  footer until the next message from you, which could be hours of replies later. Every turn now
  ends in its own footer; a turn of a single message shows when it was, without a duration.
- In a short conversation, such as an open Lanes card dragged short, the prompt index's ticks ran
  up over the card's title and down behind the message box. They stay inside the conversation
  now, and scroll when there are more than fit.
- Text selected itself: when a drag's mouse release went missing, the selection kept following the
  pointer with no button held, across every card it passed. A drag now ends once the button is up.
- A selection could run on across turns, conversations and cards. It now stays within the turn
  it began in.
- Usage gauges, and Reef's usage panel, no longer vanish when an account's usage fails to
  refresh. As in Paseo 0.12 they keep the last numbers that came through, marked with a red dot
  (an error) or a grey one (the account is unavailable), and say why and when they were last
  updated. Daemons before 0.12 and usage feeds get the same, from what crusteo read last. Gauges
  also keep their places when a daemon lists its accounts in a different order, and two hosts'
  different Cursor, Copilot or Grok logins no longer merge into one gauge.

## [0.1.0-alpha.6] - 2026-10-15

### Fixed

- One usage source falling behind, such as a host gone offline or a hub that stopped answering,
  dimmed every usage gauge and hid their "use" hints. Each gauge now dims only when its own numbers
  are over ten minutes old, an account being as fresh as the freshest source reporting it.

## [0.1.0-alpha.5] - 2026-10-14

### Added

- Lanes: a card opened from a notification blinks a bright outline for a few seconds, so the eye
  finds it. Settings → Notifications → Flash the card sets how long: off, 2, 4 or 8 seconds.
- Settings → Notifications: how long an agent's notification stays up, 0 to 15 seconds or until
  closed (hovering still pauses it); how several sit together, as a compact stack, a list that
  never runs past the window, or only the newest; and how many to keep at once. "Dismiss all
  notifications" is in the title bar's menu and the command center.

### Fixed

- In the message box, Home and End went to the ends of the whole paragraph rather than the line it
  wraps onto, and Up and Down did nothing on the first or last line. Home and End now go to the
  wrapped line's ends (the paragraph's on a second press), and Up and Down go to the start or end of
  the text from the first or last line. A patched copy of `gpui-base` (`vendor/gpui-base`).

## [0.1.0-alpha.4] - 2026-10-14

### Added

- Lanes: a workspace's right-click menu, and an agent card's, start with the workspace's folder on
  its host, dimmed, with a home folder as `~` and the full path on hover; "unknown path" when the
  host doesn't say.

### Fixed

- A question an agent asks, such as Claude Code's AskUserQuestion, showed as a permission with Deny
  and Accept, and Accept sent no answers. It is a form now, as in Paseo: each question's options,
  single or multiple choice, a box for another answer where one is allowed, and Submit. Cards and
  the Needs-you strip say the agent has a question, to be answered in its conversation.

## [0.1.0-alpha.3] - 2026-10-13

### Added

- Right-clicking selected text in a conversation offers Copy, as shown, and Copy as Markdown, the
  source the agent wrote, with its bold, code, links and lists.
- A window border, off by default: Settings → Window sets a solid line around the window's edge,
  1 to 8 px wide, in any color, to tell crusteo apart from the windows beside it.

### Changed

- The title bar's "?" is now a menu: Settings, Keyboard shortcuts, Customize toolbars and About.
- Less space between the app icon and the title bar's first item on Windows and Linux; the icon
  drags the window as the empty bar does.
- Terminals show a block cursor that blinks slowly while the terminal has focus, whatever shape the
  program asks for, and stays lit while you type or output arrives.

### Fixed

- Terminals drew no cell backgrounds: programs' background colors, the selection highlight and the
  cursor were all missing.
- A lane with a terminal open kept the ordinary lane width and could not be resized; it is as wide
  as a lane with an open card, and its edge drags.

## [0.1.0-alpha.2] - 2026-10-13

Windows fixes, checked against Paseo 0.10.1 through 0.11.2.

### Added

- About on Windows and Linux: the help panel (F1) shows the version and has an About button, and
  the command center (Ctrl/Cmd+K) offers "About Crusteo" on every platform.
- Lanes: an open card's or terminal's bottom edge can be dragged to make it shorter, leaving more of
  the lane to the cards below it. Dragged back to the bottom, or double-clicked, it fills the lane
  again.
- Lanes: fades at the top and bottom of a lane with more cards to scroll to, and stronger fades at
  the board's sides, with a shadow so they show against dark cards.

### Fixed

- About closed as soon as it opened on Windows.
- `hosts.json`, `integrations.json` and `ui.json` saved with a UTF-8 byte-order mark, as PowerShell
  and older Notepad save them, were ignored and the defaults used instead.
- Terminals restored from a newer Paseo daemon shifted text after wide characters, such as CJK, one
  column right.
- The help panel's buttons fell below the fold when extensions added shortcuts.
- A trackpad swipe over the Lanes header row moved the board by the swipe's drift up or down rather
  than its sideways motion. Trackpads now scroll the board sideways only; a mouse wheel there still
  turns into sideways scrolling.
- Usage gauges fed by the daemons showed no pace for Claude, whose reset times carry an offset
  (`+00:00`) rather than `Z`, and so merged different Claude accounts into one gauge.
- A Codex gauge from a Paseo 0.10 daemon, which calls the weekly window "session", showed no pace;
  it now takes the window's length from a newer daemon reporting the same account.
- Two accounts of one provider whose labels start alike, such as `sam.lee@gmail.com` and
  `sam@northwindtraders.com`, are told apart on their gauges ("gmail", "northwindt").

## [0.1.0-alpha.1] - 2026-10-11

First alpha, for a few early testers. Early software: expect rough edges.

### Added

- Native Rust/GPUI desktop client for Paseo daemons (protocol 1; tested against Paseo 0.10.1
  through 0.11.2), for Linux,
  macOS and Windows.
- Concurrent connections to any number of daemons, with handshake, heartbeat, reconnect with
  backoff, directory-sync cursors and resume after reconnect. Optional per-host password.
- Paseo layout: sidebar merged across hosts (projects, workspaces, pinning, status grouping,
  filters), workspace header, client-local agent tabs, and New workspace, History, Hosts and
  Settings screens.
- Live conversations: streamed assistant text, tool-call rows with Paseo's names, summaries, icons
  and expandable details, TodoWrite task rows, plan cards, and sequence-checked catch-up after gaps.
- Permission prompts answered inline and from Fleet and Lanes cards.
- Composer with send, steer (Enter), queue (Ctrl/Cmd+Enter) and interrupt (Escape); mode, model
  and thinking pickers; slash-command completion.
- Clipboard images in the composer (up to eight images, 20 MB total), converted to PNG, in new
  agents, sends, steers and queued messages; failed sends can be restored with their images.
- Prompt scroll index along the conversation edge with hover previews and jump-to-prompt, loading
  older history on demand.
- Built-in terminals backed by the daemon's terminal sessions.
- Lanes layout: workspace cards in lanes by label (labels, label combinations, or both), project,
  host or workspace; in-place conversations, relabeling, colored lanes and recency-tinted headers.
  Lanes can be dragged by a grip, collapsed into tabs, narrowed and widened, and filtered by host,
  tool, state and label. Workspaces can be pinned and their cards reordered. New agents start from a
  lane's header, in a chosen or new workspace. Hovering a lane's name shows its labels in full and
  which hosts define them.
- Find on the Lanes board (Ctrl/Cmd+F) across card titles, labels, hosts, workspaces and loaded
  conversations, including collapsed lanes.
- Subagent tracks in conversations, with archiving of finished subagents.
- Fleet layout: one column per host, agents ordered by urgency, per-host state counts.
- Board layout: workspaces in columns by state (Needs input, Failed, Ready to review, Working,
  Done).
- Workspace archiving with a confirmation that lists affected agents and warns about worktrees
  with uncommitted changes or unpushed commits.
- Command palette (Ctrl/Cmd+K) across all hosts, and keyboard shortcuts for navigation, tabs,
  layouts, zoom and the composer. Customizable toolbars and a help panel (F1).
- App icon, shown in the title bar, the macOS Dock and the Windows taskbar.
- macOS menu bar, with About showing the version, the Paseo protocol and each host's daemon version.
- Usage gauges in the title bar from the daemons' own provider usage, one per account.
- Extensions: Lua toolbar items, and Rust extensions run with `crusteo::run_with`, which can add
  toolbar items, commands, a panel, Lanes find results, workspace actions, Needs-you items and
  usage gauge sources. Settings come from `integrations.json` or the environment.
- `CRUSTEO_STRESS` synthetic fleet for performance work, and `CRUSTEO_PROFILE` sampling profiler
  on Windows.
- Patched GPUI (`vendor/gpui-pre`) that skips redraws on window moves and lets cached views be
  reused after they move.

[Unreleased]: https://github.com/chicagobuss/crusteo/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/chicagobuss/crusteo/releases/tag/v0.2.0
[0.1.0-beta.6]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.6
[0.1.0-beta.5]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.5
[0.1.0-beta.4]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.4
[0.1.0-beta.3]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.3
[0.1.0-beta.2]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.2
[0.1.0-beta.1]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.1
[0.1.0-alpha.6]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.6
[0.1.0-alpha.5]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.5
[0.1.0-alpha.4]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.4
[0.1.0-alpha.3]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.1
