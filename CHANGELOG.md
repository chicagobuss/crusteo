# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/chicagobuss/crusteo/compare/v0.1.0-beta.2...HEAD
[0.1.0-beta.2]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.2
[0.1.0-beta.1]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-beta.1
[0.1.0-alpha.6]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.6
[0.1.0-alpha.5]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.5
[0.1.0-alpha.4]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.4
[0.1.0-alpha.3]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.1
