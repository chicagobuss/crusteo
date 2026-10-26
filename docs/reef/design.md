# Reef: design, and the Skiff protocol v0

Reef is the fleet service (crusteo-hub before 0.1.0-beta.1). Skiff is the page it serves phones,
and the Skiff protocol, described here, is what they speak to it downstream.

Status: phase 1 (reads) and phase 2 (actions, creating, deleting and relabelling agents,
turn-finished notifications, Skiff's controls) built in `crates/reef`. Web Push and the native app are specified here and come later.

## 1. Why

The Paseo phone app tries to be the whole desktop app on a phone: one connection per daemon, the
full directory, every timeline it has opened. With several hosts and hundreds of agents the phone
runs out of memory.

Reef turns that around. It is an always-on, headless process on a box that reaches every daemon.
It keeps the whole fleet current, as crusteo desktop does, and gives a phone only what is on its
screen: a short needs-you list, a page of agents, or one conversation already grouped into turns.
The phone never talks to a daemon.

It is piece 2 of three: daemons (Paseo, or Burrow speaking the same protocol v1) upstream, Reef,
and thin clients downstream.

```
 host A: Paseo daemon ──┐                                   ┌── Skiff, in the phone's browser
 host B: Burrow ────────┼── Paseo protocol v1 ──► Reef ─────┼── native app (later)
 host C: Paseo daemon ──┘    one socket per host    │       └── crusteo desktop (optional, later)
                                                     └── push notifications (later)
```

## 2. Architecture

### 2.1 Upstream: reused, not rewritten

Reef is crusteo without GPUI. It depends on `paseo-client`, `fleet` and `usage-source` only; it
does not depend on `gpui` or on the `crusteo` crate.

- **Connections.** One `paseo_client::Client` per host from the hosts file (crusteo's `hosts.json`
  format). The client handles hello, the 10 s keepalive ping (so the 45 s lease never lapses),
  reconnect with backoff, and bounded event queues. Reef uses its own client id,
  `reef-client-id.json` in its data dir; it never reads crusteo's `client-id.json`.
- **Bootstrap,** in the same order as crusteo's store: session events, projects, workspaces, agents
  (every page), labels; pushes that arrive meanwhile are held and replayed after the replies, so a
  snapshot never erases a newer push.
- **Background workspaces.** A Paseo 0.12 daemon leaves its background workspaces, and every
  agent in them, out of directory reads, sync and pushes unless the client sends
  `includeBackground`. Reef sends `includeBackground: true` at the top level of its workspace
  and agent reads (`fleet::requests::DirectoryRead`) to every host that advertises
  `backgroundWorkspaces`, as crusteo does, so it holds them all; what a phone sees is decided per
  list (3.6). Older daemons do not hide anything and are not sent the flag.
- **The model** is `fleet::Fleet`: the same `Directory` reducers, `updatedAt` ordering and bucket
  rules the desktop app uses.
- **Timelines** stream only for conversations a phone has open (plus five minutes after the last
  phone closes one, so reopening is instant). They use `fleet::timeline::Timeline`: tail fetch,
  live seq gate, catch-up `after` a gap, replace on reset or epoch change, `before` pages for older
  history.
- **A fixed list of requests, enforced.** Every request goes through `upstream::guarded`, which
  refuses any type outside `READ_REQUESTS` (the subscription and fetch reads,
  `subscription.release.request`, `usage.list_reports.request` for the `usage` view, and
  `get_providers_snapshot_request` for the new-agent form) and `WRITE_REQUESTS`, which holds
  exactly what the phone's actions send: `agent_permission_response`, `cancel_agent_request`,
  `send_agent_message_request`, `archive_agent_request`, `agent.create.request`,
  `delete_agent_request` and `workspace.label.assignment.set.request`. Reef cannot open a
  terminal, rename or archive a workspace, rename or recolour a label, change an agent's
  key/value labels (`update_agent_request`), use the older `create_agent_request`, or send
  `clear_agent_attention`; opening a conversation on the phone never clears "ready to review" for
  the people at their desks. Reef logs every write it sends.
- **"Ready to review" is ignored.** Reef shows Paseo's state buckets except that one: a finished
  agent is simply done, and needs-you holds only questions, permissions and failures.

### 2.2 The core: one task

`reef.rs` runs one task that owns all state: the upstream model and every phone's subscriptions.
Daemon events, request replies (as closures applied to the state) and phone messages all arrive on
channels, so there are no locks.

Reducers only mark what changed: the fleet, or particular conversations. Every `flush` (150 ms) the
task works out each affected view again, diffs it against what that phone was last sent, and sends
one delta per changed view. Streaming text, which Paseo already coalesces in 60 ms windows, reaches
the phone at most about seven times a second.

### 2.3 Views are pure

`lists.rs` and `views.rs` hold the projections, all pure and unit-tested:

- `agent_row`: one agent as a list row.
- `needs_you`, `agent_list`: filtered, sorted and grouped rows, windowed per group.
- `conversation`: a window of the timeline grouped into turns, using `fleet::timeline::turn_starts`,
  `finished_turns` and `turn_footer`, the same rules the desktop conversation uses (a turn starts at
  a user message or where the turn id changes, so an orchestrator's own `autonomous-turn-N` turns
  get footers too).
- `diff_rows`, `diff_turns`: the deltas.

### 2.4 Downstream: one port

`server.rs` listens on `127.0.0.1:6798` by default. `GET /` serves Skiff, `/ws` is the
protocol's WebSocket, `GET /healthz` answers `ok`, and `GET /image/<key>?t=<token>` serves an image
from a conversation (3.7). A WebSocket from a page on another origin is refused (its `Origin` must
match `Host`), and every request whose `Host` is not an IP address, `localhost` or a name given with
`--host-name` gets a 403 (section 7).

### 2.5 What moved out of `src/app`

- `fleet::clock`: `parse_iso`, `iso_from_millis`, `now_millis`, `now_iso`, `time_ago`, `uuid_v4`.
  The store re-exports them, so crusteo's views are unchanged.
- `fleet::timeline::turn_footer`: a finished turn's end time and duration, which the desktop
  conversation computed inline. Both now use it.
- `fleet::display::hidden_in_stream`: which items a conversation leaves out (a plan waiting for
  approval shows as its permission card).

The store's connection, bootstrap and timeline glue is GPUI code (entities, `cx.spawn`), so it was
ported into `reef::upstream` rather than moved. Moving it into a shared crate later would mean
abstracting the executor; see open question 7.

## 3. Downstream protocol v0

### 3.1 Transport

- One WebSocket per phone: `ws://<reef>/ws`, or `wss://` behind TLS (section 6).
- Text frames only, one JSON object per frame. Every message has a `type`.
- Phone frames are at most 64 KiB, except a `send` or `steer` with images once the hello was
  accepted, which may be 12 MiB. Before the hello a socket may send 64 KiB in all, and at most 16
  sockets may be waiting to sign in at once.
- Unknown fields are ignored on both sides. Field names are `snake_case`.

### 3.2 Hello and auth

The first frame must be a hello, within 10 s:

```json
{"type": "hello", "v": 0, "token": "<device token>", "device": "pixel"}
```

A Reef started with `--open` asks for no token: any hello is welcomed, and the phone goes by the
`device` name it gave. The page always tries without a token first, and shows its sign-in form only
when a Reef refuses it.

Reef answers:

```json
{"type": "welcome", "v": 0, "hub": {"version": "0.1.0"}, "device": "phone",
 "hosts": [ <host>, ... ],
 "settings": {"notify_default": true, "show_background": true},
 "limits": {"max_subscriptions": 8, "list_max": 100, "items_max": 60, "text_max": 16384}}
```

or `{"type": "error", "code": "unauthorized", ...}` and a close with code 4401. A `v` newer than the
Reef's gets `code: "version"`.

- **Device tokens** are 32 random bytes (64 hex digits), one per device, kept in
  `reef-tokens.json` in the data dir as `[{"name": "phone", "token": "..."}]`. On first start Reef
  makes one and prints it once. Removing an entry revokes that device.
- The token goes in the hello, not the URL, so it never lands in a proxy's access log. The phone
  page reads it once from the address's `#token=` fragment (which browsers never send) and keeps it
  in local storage.
- `device` in the welcome is the token's name, as Reef knows it.

### 3.3 Views

A phone subscribes to views by an id it picks. Each view is either a list or a conversation.

```json
{"type": "subscribe", "id": "ny", "view": "needs_you", "limit": 20}
{"type": "subscribe", "id": "list", "view": "agents", "offset": 0, "limit": 30,
 "group": "label", "sort": "recent", "collapsed": ["label:docs"], "more": {"label:billing": 50},
 "filter": {"hosts": ["example-host"], "labels": ["billing"], "states": ["running"],
            "query": "flaky test", "subagents": false, "background": false}}
{"type": "subscribe", "id": "conv", "view": "conversation",
 "host": "example-host", "agent": "<agent id>", "turns": 3}
{"type": "unsubscribe", "id": "conv"}
{"type": "window", "id": "list", "offset": 30, "limit": 30}
{"type": "resync", "id": "list"}
```

- A phone has at most 8 views. Subscribing with an id in use replaces that view.
- `window` moves or grows a list's window and is answered with a fresh snapshot.
- `resync` asks for a fresh snapshot of any view.
- A view that cannot open is answered with `{"type": "error", "id": "conv", "code": "not_found"}`.

### 3.4 Snapshots, deltas and sequence numbers

Every view starts with a snapshot, then gets deltas:

```json
{"type": "snapshot", "id": "list", "seq": 0, ...the whole view...}
{"type": "delta", "id": "list", "seq": 1, ...only what changed...}
```

- A snapshot replaces everything the phone holds for the view and sets its `seq`.
- Each delta has `seq` one more than the last message for that view. A phone that sees any other
  number has lost track: it sends `resync` and drops deltas until the snapshot comes.
- Reef sends a snapshot instead of deltas when a view opens, on `window` and `resync`, after a
  phone fell behind (3.13), and when a daemon rebuilt a conversation's timeline.
- Lists and conversations are made of keyed parts (rows, turns, items). Deltas use the same three
  fields at every level:
  - `upsert` (lists) or `turns` (conversations): parts that are new or changed, whole;
  - `remove`: keys of parts gone;
  - `order`: every key in order, present only when the order or the membership changed.

  A phone applies `remove`, then the upserts (replace by key, or append), then `order` if present.
- Other fields of a delta replace the field of the same name. `null` means it is gone.

### 3.5 Hosts

The welcome carries the hosts, and `{"type": "hosts", "hosts": [...]}` follows whenever one changes:

```json
{"name": "example-host", "state": "online", "version": "0.11.2", "agents": 42,
 "labels": ["billing", "infra"]}
```

Beside `hosts`, the welcome and every `hosts` message carry `labels`: every label in Reef's
catalog by its id (3.9.2), with its `name` and a CSS `color`. A host's own `labels` are the Paseo
label names it has, for reference. Paseo's
colour names (`violet`, `sky`, `emerald`, ...) become their foreground on a dark background, as
crusteo's identity palette has them; a hex colour passes through; a label nobody defined, or with no
usable colour, gets one picked from its name with Paseo's identity hash, so it is the same on
every device. The page uses them for label chips on rows and cards, for a stripe and title on
board columns, a dot on list sections, and a tint and title on map blocks.

`state` is `connecting`, `online`, `offline` or `refused`; the last two carry a `reason`. `since`
is when the host last came up or went down (a host retrying between connecting and offline is one
outage), so a phone can tell a blip from an outage; the page names a host only once it has been
down for two minutes, and otherwise shows a status dot per host. `labels` are the workspace labels
on that host, for the agent-list filter.

### 3.6 Agent rows

Both list views send rows of this shape. Empty fields are left out.

```json
{"key": "example-host/1f2e...", "host": "example-host", "id": "1f2e...",
 "title": "Fix the flaky retry test", "state": "needs_input", "notify": true,
 "provider": "claude", "model": "claude-opus-5-5",
 "workspace": "shop-checkout", "project": "shop", "labels": ["billing"], "background": true,
 "at": "2026-10-17T09:50:00.000Z", "parent": "<parent agent id>",
 "reply": "Done: the tests pass and the change is on the branch.",
 "reply_at": "2026-10-17T09:49:30.000Z",
 "ask": {"id": "permission-...", "kind": "question", "title": "Question",
         "question": "Ship the fix alone, or with the refactor?", "more": 1}}
```

- `state` is Paseo's bucket, without "ready to review" (2.1): `needs_input`, `failed`, `running` or
  `done`.
- `notify` says whether a finished turn of this agent raises a notification (3.10).
- `background` is there, `true`, when the agent's workspace is one of Paseo 0.12's background
  workspaces (2.1). The page tags such a row "background", muted, on its existing line, except in
  the Background group, which says so already.
- `at` is the agent's last update.
- `reply` is the first line of the agent's latest reply, and `reply_at` its time, once Reef
  knows them (below).
- `ask` summarizes the first pending permission or question; `more` counts the others.

**`needs_you`** lists agents on online hosts that wait on a person: `needs_input` first, then
`failed`, newest first within each. Its snapshot has `total`, `offset` and `rows`.

**`agents`** lists every agent that passes the filter, sorted, and optionally grouped, all on the
Reef:

- Filter fields are all optional: `hosts`, `labels` (workspace labels, matched as crusteo matches
  them; `"\u0000background"`, which no label can be, is Background: background workspaces'
  agents, which match it and no label of their own, as they are grouped; ignored while
  background is hidden), `states`, `query` (every word must appear in the title, workspace or
  project), `subagents` (off by default) and `background`: whether background workspaces' agents are listed.
  Left out, `background` follows Reef's `show_background` setting (3.10), which is on by
  default; the page sends it only once this device chose in Filters ("Background shown" or
  "Background hidden"; "Background: … (Reef)" follows Reef), and counts hidden background in
  the Filters badge like any other filter. The `map` view filters the same way. `needs_you` is
  not filtered: an agent waiting on a person shows there wherever it lives.
- `sort` is `recent` (the default: the agent that last did something first, as Paseo's history
  view has it), `needs` (needs input, failed, running, done; newest first in each) or `name`.
- `group` is `none` (the default), `host`, `label`, `state` or `workspace`. Grouped, the snapshot
  has `groups`, every group in order with its `key`, `label` and `total`, and the rows carry their
  `group`. An agent with two labels is in both label groups, so a grouped row's `key` is
  `<group key>|<host>/<agent id>`. By label, a background workspace's agent is in the Background
  group (`background`) and only there, as in crusteo's Background lane, whatever its labels.
  Groups come in the hosts' order, the labels' order (no label, then Background, last), the
  states' order, or, for workspaces, newest activity first. A change in the groups comes as a
  `groups` field in a delta.
- The window: ungrouped, `offset` and `limit` (default 30, at most 100); grouped, `limit` rows per
  group, `more` raising it for some groups, and no rows for the groups in `collapsed`. `total`
  counts every match. Skiff shows groups as folding sections, or as board columns on a
  wide screen.

**`map`** is the zoomable map's view. It takes the same `group` and `filter` as `agents`, and
`focus`: the agents (`host/agent`, at most 80) whose cards the phone shows up close.

```json
{"type": "subscribe", "id": "map", "view": "map", "group": "label", "filter": {}, "focus": []}
{"type": "focus", "id": "map", "agents": ["example-host/1f2e...", "example-host/9a8b..."]}
```

- The snapshot has `lanes` (`key`, `label`, `total`), a tile for every agent that passes the
  filter, and `cards`, full agent rows for the focused agents only.
- A tile is small: `key` (`<lane>|<host>/<agent>`), `agent`, `lane`, `state`, `at` (its last
  change, to the minute, which is all a far view needs and keeps busy agents quiet) and `ask` when
  it waits on a question or permission. Hundreds of agents cost a few tens of kilobytes once.
- Lanes keep the grouping's own order, workspaces by name rather than activity; tiles in a lane
  come in the order agents were created. Nothing moves when an agent works, so a phone's map keeps
  its shape.
- `focus` replaces the focused set and is answered with a delta. Latest replies are read only for
  focused cards, through the same limits as the lists.
- A delta nests the row-style changes of its two lists: `{"tiles": {upsert, remove, order},
  "cards": {upsert, remove, order}}`, beside `lanes` when they changed.
- The conversation a map opens up close is an ordinary `conversation` view.

**Latest replies.** Reef keeps the first line of each agent's latest reply. It takes it from an
open conversation as it streams, and from the read a turn-finished notification makes. Otherwise
it reads the end of the agent's timeline (12 entries), but only for agents some phone's window
shows, at most four reads at a time across the fleet, once per agent, again only when one of its
turns ends, and not again for five minutes after a read that found nothing.

**`usage`** is each account's usage limits, the numbers crusteo's title-bar meters draw:

```json
{"type": "subscribe", "id": "usage", "view": "usage"}
→ {"type": "snapshot", "id": "usage", "seq": 0, "total": 2, "offset": 0, "rows": [
   {"key": "claude:work", "provider": "claude", "account": "work", "label": "sam@work.example",
    "plan": "team", "status": "ok", "served_at": 1799999940, "stale": false,
    "retained": false, "failure": null,
    "severity": "warn", "slack": "-12", "needle": 31, "binding": "weekly_all", "tightest": true,
    "use_before": {"window": "session", "remaining": 74.0, "resets_at": 1800005100},
    "windows": [{"key": "weekly_all", "name": "weekly Global", "used": 39.0, "remaining": 61.0,
                 "resets_at": 1800258900, "length": 604800, "binding": true}, ...]}, ...]}
```

- Where the numbers come from: every connected daemon with usage sources (the `usageSources`
  feature) is asked for its usage reports when it connects and then every two minutes, and, with
  `--usage <url>`, a usage feed's `/v1/usage` is read every minute. Both are crusteo's sources,
  asked as often as crusteo asks them. Reef asks; phones never do, so any number of phones cost
  the feed nothing. A failed read keeps the last good one, which goes stale.
- An account whose own read failed keeps its last good numbers, as Paseo 0.12 keeps them: the
  daemon's `lastSuccessful` report when it sends one, else the account's numbers from the source's
  previous reading. An account with nothing kept has no row. Accounts keep their places from one
  read to the next, though a daemon lists its reports in the order they finish. A report Paseo
  keys `default` (a source's only login: Cursor before 0.12, Copilot, Grok) is the host's login,
  not an account, so it goes by host and matches another host's only on its numbers.
- One model, `usage-source`, reads and merges them for crusteo and Reef alike: each account once
  (the same use and reset seen through two hosts, or by a daemon and the feed, is one account,
  named by the feed when it knows it), each as fresh as its freshest source. Windows a feed lists
  twice (a prefixed key with the same numbers) show once, and the account-wide window beside scoped
  ones is "Global".
- A row is one account's gauge: `account` tells it apart from the provider's other accounts (null
  when it has none), `binding` is the window that constrains it most, `severity` (`critical`,
  `warn`, `on_pace`, `surplus`, `unknown`) and `slack` (quota left minus time left in that window)
  its pace, `needle` the pace dial's angle, `stale` whether its numbers are over ten minutes old,
  `retained` whether they are kept from before a failed read, with `failure` saying how it failed
  (`status`, `error` or `unavailable`, and Paseo's `problem` text) and `served_at` when the kept
  numbers were read, `use_before` a window worth using before it resets (never when stale or
  retained), and `tightest` marks the one row that most needs watching, fresh rows first. Times
  are unix seconds; the phone counts down itself.
- Rows are worked out again when a reading lands and every ten seconds, since pace and staleness
  move with the clock; only changes go out, as an ordinary list delta.

A preview is plain text from the first line with real words: empty lines and lines that are only
layout (rules, code fences and the code in them, bare bullets, table rules) are skipped, and
heading and quote marks, bullets and numbers, task boxes, emphasis, code ticks and link syntax are
taken out. A reply that is only code previews its first code line. Notifications use the same.

### 3.7 Conversations

A conversation's snapshot:

```json
{"type": "snapshot", "id": "conv", "seq": 0,
 "agent": <agent row>, "loading": false, "error": null,
 "has_older": true,
 "working": {"since": "2026-10-17T10:00:00.000Z"},
 "asks": [ <ask>, ... ],
 "queued": [{"id": "<message id>", "text": "Add a section on tests.", "error": "..."}],
 "turns": [ <turn>, ... ]}
```

- `loading` is true until Reef has the first page from the daemon; `error` says why a page
  failed.
- `working` is set while a turn runs, `null` otherwise.
- `asks` are the pending permissions and questions, with what it takes to answer them (3.9):
  `id`, `kind` (`tool`, `plan`, `question`, `mode`, `other`), `title`, `name`, `summary` (the tool
  call in a line), `actions` (`id`, `label`, `behavior`: the provider's own, else Deny and Accept,
  or Implement for a plan), `plan` (its Markdown) for a plan, and `questions` (`header`,
  `question`, `options`, `multi`, `other`) and `dismiss` (the dismiss button's label) for a
  question.
- `queued` are messages waiting on Reef for the turn to end (3.9). One with `error` failed to
  send and waits for the phone to steer it in again or drop it.

**Turns.** A turn starts at a message from the user, or where the agent's turn id changes, so an
orchestrator's own turns stand apart:

```json
{"key": "m-1", "by": "user",
 "footer": {"at": "2026-10-17T09:01:12.000Z", "secs": 72},
 "clipped": 12,
 "items": [ <item>, ... ]}
```

- `key` is the id of the turn's first item.
- `by` is `user` or `agent`.
- `footer` is present once the turn has finished: when it ended and, if it took a second or more,
  how long. The running turn and an unanswered message have none.
- `clipped` counts the turn's first items the window leaves out (a turn too long to send whole).

**Items** are compact. A phone gets one line per tool call and asks for more when the user taps:

| `kind` | Fields |
|---|---|
| `user` | `text`, `images` (a count) and `image_keys` (one per image) |
| `assistant` | `text` (Markdown) |
| `thought` | `chars`, `loading`, `detail` |
| `tool` | `name` ("Shell", "Read", ...), `summary` (the command or path), `status`, `icon`, `detail`, and `agent` when the row names an agent: `{id, title, lead, known}` for the sender of a message from another agent ("From" planner), a notice about one (planner "finished"; `lead` puts the agent before `name`), or the agent a Paseo tool call acts on ("To" worker). `known` says whether the host knows the agent, so a tap can open it |
| `todo` | `label` ("Created 3 tasks", "Completed Write the tests"), `done`, `total` |
| `notice` | `level`, `text` |
| `compaction` | `label` |

Every item has `id` and `at`. A `text` longer than 16 KiB is cut on a character boundary and marked
`clipped: true`.

Images are never inside a frame. A phone fetches each one over plain HTTP, as
`GET /image/<key>?t=<device token>` (the token percent-encoded; no `t` with `--open`). The key is
the image's content hash, so the answer never changes and a phone can keep it; Reef serves only
images of conversations it holds.

**Deltas** follow 3.4 at two levels. A turn patch carries `key`, the turn fields that changed, the
items new or changed in `items`, and `order` (item ids) when the items' order or membership changed.
A new turn comes whole. Text that only grew comes as an append:

```json
{"type": "delta", "id": "conv", "seq": 7,
 "turns": [{"key": "r-5", "items": [{"id": "r-5", "append": " are done.", "at": "..."}]}]}
```

**The window.** A conversation opens with its last `turns` turns (default 3), but never more than
60 items; a single longer turn shows its last 60. New turns are added at the end as they come.
The window never holds more than four times 60 items: past that its oldest turns leave it as new
ones arrive, and a fresh snapshot (a `resync`, or a phone that fell behind) carries the last 60
items only, so a conversation left open for days stays small. To see earlier ones:

```json
{"type": "older", "req": "r1", "id": "conv", "turns": 5}
```

Reef widens the window by up to 5 turns (again at most 60 more items), first from what it holds
and, when that runs out, by fetching the previous page from the daemon. The new turns arrive as an
ordinary delta, then `{"type": "result", "req": "r1", "ok": true, "has_older": true}`.

**Detail.** The full text of a tool call or thought:

```json
{"type": "detail", "req": "r2", "id": "conv", "item": "<item id>"}
→ {"type": "result", "req": "r2", "ok": true,
   "detail": {"item": "<item id>", "sections": [{"title": "Output", "text": "...", "clipped": true}]}}
```

Each section is at most 64 KiB. Sections follow crusteo's own expanded tool rows: a shell's
`$ command` and output, a read's numbered lines, an edit's diff, an unknown tool's input and output.

### 3.8 Results and errors

A message with a `req` gets exactly one `result` with that `req`:

```json
{"type": "result", "req": "r3", "ok": false, "error": {"code": "not_found", "message": "no such item"}}
```

Codes: `unauthorized`, `version`, `bad_request`, `unknown_view`, `not_found`,
`too_many_subscriptions`, `upstream` (the daemon refused, failed, timed out, or its host is
offline), and `not_yet` (reserved for actions a Reef does not serve). Problems with no `req` come as `{"type": "error", "code": ..., "message": ...}`,
with the view's `id` when they concern one.

### 3.9 Actions

```json
{"type": "action", "req": "r4", "action": "send", "host": "example-host", "agent": "<id>",
 "text": "Ship the fix alone.", "message_id": "<id the phone made>"}
```

| `action` | Extra fields | What Reef does |
|---|---|---|
| `permission` | `ask` (its id), `decision` (`allow`/`deny`), `action_id`, `message` | `agent_permission_response` with `behavior` and `selectedActionId` (default `accept`/`reject`, as crusteo sends) and, for a deny, `message` (default "Denied by user") |
| `answer` | `ask`, and `answers` (one `{"selected": [option indexes], "other": "typed text"}` per question, in order) or `dismiss: true` | `agent_permission_response` built by `fleet::questions`, as crusteo builds it: `allow` with `updatedInput` (the question's input plus `answers` keyed by header, chosen labels joined by ", ", typed text replacing a single choice), or `deny` with "Dismissed by user". Every question needs an answer first. |
| `stop` | | `cancel_agent_request` |
| `send` | `text`, `message_id`, `images` (optional, at most 8, each `{"data": <base64>, "mime_type": ...}`) | Idle agent with nothing on its way or waiting: `send_agent_message_request` at once. Otherwise queued on Reef (the result says `queued: true`) and sent, as a plain send, when its turn ends, one message per idle edge, as the Paseo app's queue does. At most 20 wait per agent. |
| `steer` | `text` and `message_id` (and `images`, as for `send`), or only the `message_id` of a queued message | `send_agent_message_request` with `activeTurnBehavior: "steer"` now (the daemon interrupts instead when the provider cannot steer); without it for an idle agent |
| `unqueue` | `message_id` | Drops a queued message; nothing is sent |
| `archive` | | `archive_agent_request`, with a 60 s timeout; the page asks first |
| `delete` | | `delete_agent_request`: the agent is removed from the host with its history, which archiving keeps. The page asks first. The agent leaves the lists when the daemon answers. |
| `relabel` | `labels` (every label the agent should have, by name) | One `update_agent_request` carrying only the agent's `crusteo.labels` (3.9.2) |
| `create` | `host`, `provider`, `model`, `mode`, `thinking`, `workspace` or `cwd`, `labels` (optional), `text`, `message_id`; no `agent` | `agent.create.request`, carrying the new agent's labels (3.9.1) |
| `notify` | `on` (`true`, `false`, or `null` to follow the default) | Sets whether this agent's finished turns notify (3.10) |
| `settings` | `notify_default`, `show_background` (either or both) | Sets the default for agents with no choice of their own, and whether lists show background workspaces' agents on phones that did not choose; no `host` or `agent` |
| `host_add` | `name`, `endpoint` (a port of 6767 is assumed), `password` (optional) | Adds a daemon to the fleet and connects; saved to the hosts file first when Reef has one |
| `host_remove` | `name` | Drops a host, its connection and everything kept for its agents; saved the same way |

- Every action on an agent names `host` and `agent`; `create` names only `host`, and `settings`,
  `host_add` and `host_remove` name none (the host ones take `name`). Reef checks that the
  agent exists and, for `permission` and `answer`, that the ask is still pending, before anything
  is sent.
- `message_id` makes `send` and `steer` idempotent: the daemon deduplicates on it, a repeated send
  of a queued message is not queued twice, and the message shows in the conversation at once,
  matched with the daemon's echo by that id.
- The result comes once the daemon answered: `{"ok": true}`, plus `sent: true` or `queued: true`
  for a send. A permission's result follows the daemon's `agent_permission_resolved` (15 s at
  most); the ask also leaves `asks` then.
- A queued message whose send fails stays at the front of the queue with its `error`, and is not
  tried again by itself; the messages behind it still go.
- Queued messages live in Reef's memory: a Reef restart loses them.
- Not in v0, on purpose: terminals, renaming, mode and model switches on a running agent, new
  workspaces from a project (Paseo's `workspace.create.request`), and `clear_agent_attention`.

#### 3.9.1 Creating an agent

A new-agent form first asks for its choices on one host:

```json
{"type": "create_options", "req": "r5", "host": "example-host"}
→ {"type": "result", "req": "r5", "ok": true, "host": "example-host", "labels": true,
   "providers": [
     {"id": "claude", "label": "Claude", "ready": true, "mode": "default",
      "modes": [{"id": "default", "label": "Always ask"}, {"id": "plan", "label": "Plan"}],
      "models": [{"id": "claude-opus-5-5", "label": "Opus 5.5", "default": true,
                  "thinking": [{"id": "low", "label": "Low"}, ...], "thinking_default": "medium"}]},
     {"id": "opencode", "label": "OpenCode", "ready": false, "status": "unavailable"}],
   "workspaces": [{"id": "<workspace id>", "name": "shop-checkout", "project": "shop",
                   "directory": "/home/dev/code/shop", "labels": ["billing"]}]}
```

- `providers` comes from the host's `get_providers_snapshot_request`, in either of Paseo's
  encodings (`fleet::providers`). Reef reads it only when a phone asks, once however many ask
  at the same time, and keeps it for ten minutes (five seconds while a provider is still loading);
  a host that reconnects is read again. Providers that are not ready come by name and `status`
  only. A model has `thinking` only when it offers thinking options.
- `workspaces` are the host's workspaces from the directory Reef already holds, the most
  recently active first, at most 200. Each one's `labels` are those a new agent there starts
  with: the ones every current agent in it has, or, with none, the ones it inherits (3.9.2).
  The top-level `labels` says whether the host has workspace labels.
- The label set to pick from is Reef's catalog, which every phone has from `hosts` (3.5).

Then:

```json
{"type": "action", "req": "r6", "action": "create", "host": "example-host",
 "provider": "claude", "model": "claude-opus-5-5", "mode": "plan", "thinking": "high",
 "workspace": "<workspace id>", "labels": ["billing", "release"],
 "text": "Write the release notes.", "message_id": "<id the phone made>"}
→ {"type": "result", "req": "r6", "ok": true, "host": "example-host", "agent": "<agent id>",
   "key": "example-host/<agent id>"}
```

- `workspace` creates the agent in a workspace the host has, in its directory. Instead, `cwd`
  names any directory on the host: a workspace's own directory means that workspace, and any
  other is left to the daemon, which finds or makes the workspace for it (and refuses a directory
  that does not exist).
- `provider` must be one of the host's, and ready; `model`, `mode` and `thinking` must be its own.
  Left out, they are the provider's defaults, as crusteo's new-agent draft picks them: the default
  model, that model's default thinking, the provider's default mode. A wrong one is refused before
  anything is sent: `not_found` for a host, provider or workspace Reef does not know,
  `bad_request` for the rest.
- `text` is the first message (`initialPrompt`, with `message_id` as its `clientMessageId`; Reef
  makes one when the phone does not). The request is built by `fleet::requests::create_agent`,
  the one crusteo sends; its `labels` (Paseo's per-agent key/value map) carry the agent's own
  labels under `crusteo.labels`, as 3.9.2 has them.
- `labels` names them; a name the catalog lacks makes a label. Left out, the agent starts with
  its workspace's default, as the form showed it; for a directory with no workspace yet it has
  none of its own and inherits the workspace's. The workspace's own labels are left alone.
- The result comes once the daemon created the agent. The agent is in Reef's directory by then
  even if the daemon's push has not come yet, so the phone can open its conversation at once.
- `agent.create.request`, `delete_agent_request` and `workspace.label.assignment.set.request` are
  the same in Paseo 0.10, 0.11 and 0.12, so Reef sends one shape to every host. Reef does
  not use the older `create_agent_request`, whose result comes as a `status` message. Burrow
  serves all three as Paseo does (`reef/tests/burrow.rs` runs the actions against it).

#### 3.9.2 Labels

Labels belong to agents. Reef keeps the fleet's catalog (`reef-labels.json` in its data dir):
each label has an id that never changes (`lbl_…`), a name and a colour, and a deleted one
leaves a tombstone. An agent's own labels are ids under one key of Paseo's per-agent labels,
`crusteo.labels`, as `v1:<id>,<id>` (`fleet::labels`); `v1:` alone is "no labels". An agent
without that key inherits its workspace's Paseo labels, each standing for the catalog label it
was imported as or later adopted as. So two agents in one workspace can have different labels,
and Paseo's apps, which show workspace labels, are left as they were.

- The catalog is made once, when every online host has loaded (or 20 s after start): a label for
  each distinct Paseo label name across the hosts. Where hosts colour a name differently, the
  colour most hosts use wins; a name no host defines takes the identity colour its name picks.
  Paseo labels seen later are adopted the same way.
- Rows show an agent's labels by name, and `hosts` carries the catalog as `labels`, by id (3.5).
  A label filter or group names labels by id.
- `relabel` names every label the agent should have. A name the catalog lacks makes a label, in
  the colour its name picks. Reef sends one `update_agent_request` with only `agentId` and
  `labels: {"crusteo.labels": ...}`: Paseo merges labels, so the agent's other keys (such as
  `paseo.parent-agent-id`) stay. It is the only `update_agent_request` Reef's guard lets through;
  a name, or any other label key, is refused before it leaves. The same labels in another order
  or spelling send nothing.
- The new labels show on the agent as soon as the daemon confirms them; its `agent_update`
  agrees later.
- Names are trimmed, at most 64 characters, at most 20 per request.
- Desktops in reef mode read and change the catalog at `/api/labels` (3.15).

### 3.10 Notifications and settings

When an agent's turn ends, Reef tells every phone:

```json
{"type": "notification", "id": "n12", "kind": "turn_finished", "state": "done",
 "key": "example-host/1f2e...", "host": "example-host", "agent": "1f2e...",
 "title": "Fix the flaky retry test", "body": "All tests pass now.",
 "at": "2026-10-17T10:00:00.000Z"}
```

- A turn ends when the directory shows an agent go from running to idle (`state: "done"`) or to an
  error (`state: "failed"`), or when an open conversation streams `turn_completed` or
  `turn_failed`. The same end seen both ways is announced once. Turns that ended while Reef was
  disconnected from a host are announced when it reconnects.
- `body` is the first non-empty line of the agent's last reply (Markdown heading marks dropped, 200
  characters at most), from its open conversation or a short tail read of its timeline; for a
  failure with no reply, the agent's last error.
- A turn the phone stopped is not announced.
- Whether a turn is announced follows the settings: the agent's own choice (the `notify` action),
  else `notify_default`. Settings live on Reef in `reef-settings.json` in its data dir, so they
  hold on every device; the welcome carries `settings`, and `{"type": "settings", "settings":
  {"notify_default": false, "show_background": true}}` goes to every phone when they change.
  Agent rows carry each agent's `notify`.
- A background workspace's agent is announced like any other, by its `notify`, whether or not a
  phone's lists show it.
- `show_background` (on unless set off) is Reef-wide default for listing background
  workspaces' agents (3.6); the page's Settings sheet switches it.
- `kind` leaves room for more notifications later. Web Push (section 5) will deliver this same
  event to phones whose page is closed.

Skiff shows a notification as a banner (stacking, gone after a few seconds, a tap opens
the conversation), buzzes with `navigator.vibrate` where the phone can, and, when the page is in
the background and the browser allows it, raises a system notification. Browsers allow that only
on a secure origin (HTTPS, `localhost`, or Chrome's "insecure origins treated as secure" flag for
Reef's address); the page checks and never fails without it.

### 3.11 Keepalive

- The phone sends `{"type": "ping", "t": <anything>}` every 25 s while visible; Reef answers
  `{"type": "pong", "t": ...}`. Reef also sends WebSocket pings every 30 s.
- A socket silent for 90 s is closed.
- A phone that comes back from the background reconnects, says hello and subscribes again.
  Snapshots are small, so that is cheap; there is no session resume in v0.

### 3.12 Payload budgets

Nothing Reef sends unasked is bigger than about one screen:

| What | Budget |
|---|---|
| List rows per view | 30 by default, at most 100 |
| Conversation window | 3 turns by default, at most 60 items per snapshot or `older` |
| One item's text | 16 KiB, then cut and marked |
| Tool calls and thoughts | one line; the body only on `detail`, 64 KiB per section |
| Images | a count and keys; each image by its own `GET /image/<key>` |
| Views per phone | 8 |
| Deltas | at most one per view per 150 ms |

Measured on the `--demo` fleet: a conversation's three-turn snapshot is about 1.4 KB, and a needs-you
row about 330 bytes, so a full list of 20 is under 7 KB.

### 3.13 Backpressure

- Each phone has a queue of 256 frames.
- When a delta does not fit, the phone counts as behind: its queued frames still go out, but every
  view of it is marked for a fresh snapshot, sent once its queue is at least half empty. Deltas in
  between are not queued, so a slow phone costs Reef one view's worth of memory, not a backlog.
- A `result`, `pong` or `error` that does not fit closes the connection; the phone reconnects.
  That includes the result of an `older`, which goes out with the view's next messages.
- Upstream, Reef drains each daemon's events on their own task, as `paseo-client` requires, and
  never waits for a daemon reply in the event loop.

### 3.14 Versioning

- `v` in hello and welcome is the protocol version, 0 now.
- Adding message types, fields, views, actions or error codes does not change it; phones ignore what
  they do not know.
- A change an old phone would misread (a field changing meaning, a removed message) bumps it. The
  Reef may then serve old and new side by side for a while, choosing by the phone's `v`.

### 3.15 Desktops in reef mode

A crusteo desktop with `reef.json` in its data dir takes its hosts and labels from Reef over
plain HTTP, gated as images are (`--open`, or `?t=<device token>`), and still connects to every
daemon itself.

- `GET /api/hosts`: `{"reef": <version>, "hosts": [{"id", "name", "endpoint", "password":
  bool}]}`. A password is never sent, only whether one is needed.
- `GET /api/labels`: `{"ok": true, "catalog": {...}}`, the whole catalog (3.9.2) with its
  `revision`, bumped on every change.
- `POST /api/labels` (JSON, at most 64 KB; a browser's request only from Reef's own origin) makes
  one change: `{"op": "create", "name", "color"?}` (without a colour, the one its name picks),
  `{"op": "rename", "id", "name"}`, `{"op": "recolor", "id", "color"}` or `{"op": "delete",
  "id"}`. The answer has the new `revision`, `id` for a label made, and the catalog after it, or
  `{"ok": false, "error"}`.

## 4. The phone viewer

- **First, a page served by Reef** at `/`: one HTML file with its script, styles and icons
  inline, no build step, no framework. Nothing to install, the same origin as the WebSocket, and
  once served over HTTPS (section 6) it can be added to the home screen and receive push
  notifications.
- **Layout changes at breakpoints** rather than stretching:
  - under 900px, one column: the list, or the conversation with Back;
  - from 900px, two panes: the list (380px) and the open conversation;
  - from 1400px, with a grouping chosen on the Agents tab, the list becomes a board of 300px
    columns, one per group, and the conversation a side panel.

  Conversation text stays within 80ch; extra width goes to the list or the board.
- **The list:** Needs you and Agents tabs. Agents has search, grouping, sort and host, label and
  state filters (behind a Filters button on a phone). Each row shows the title, the time, the
  latest reply's first line (or the pending question), host, workspace and state, its labels as
  coloured chips, and a notify bell. Grouping, sorting, filters and folded groups are kept per
  device in local storage. The grouping defaults to label, as crusteo's Lanes does, for the list,
  the board and the map, until the device picks another (a stored marker keeps an explicit "No
  groups"). An agent with several labels shows in each label's group; "No label" comes last but
  for "Background", which holds background workspaces' agents and nothing else. The label filter
  offers Background after the labels while background is shown; hiding it drops that choice.
- **The conversation:** permission buttons, the question form, the notify bell, the agent's labels
  as chips under its title, and Labels, Archive and Delete. Labels opens an editor: the chips to
  take off, the fleet's other labels to put on, and a new one by name. Archive and Delete ask
  first, in the page's own confirmation sheet rather than the browser's dialog. The composer is one auto-growing line with the send arrow inside it: at a desk Enter
  sends and Shift+Enter is a new line; on a phone (a coarse pointer) Enter is a new line and the
  arrow sends. While the agent works the arrow queues and a small Steer sends now; queued
  messages show one line each with Steer and drop. Stop sits beside the working timer.
- **The Map tab**, a board of every agent you move around like a map, drawn differently at each
  distance:
  - *Far:* each lane of the current grouping is a block of small tiles (state colour, a pulse
    while working, a glow for the last few minutes' work, a mark when it waits on someone), the
    block named and carrying a count of those waiting. Blocks are packed into the map's own shape
    by an ordered squarified treemap, sized by agent count, so a portrait phone stacks them and a
    desk lays them out wide; tiles wrap inside each block. A new shape (rotation, resize) re-packs
    with an animation.
  - *Middle:* tapping a block, or zooming in past one, shows its agents as cards as wide as the
    screen: title, host, state, time and the latest reply. Only cards in view are drawn, and only
    they are the map's `focus`.
  - *Near:* a card opens into the live conversation where it is and grows to fill the map: the
    same pane, composer, permission buttons and question forms as elsewhere. Back, Esc or pinching
    out closes it into its card.

  One finger pans and two pinch (the map has `touch-action: none`, so the page never zooms);
  on a desk the wheel pans and a trackpad pinch (a wheel with Ctrl) zooms. Fit all returns to the
  whole map; a minimap shows where you are when zoomed in, and in a lane when the screen is wide
  enough; a notification's tap flies to its card and flashes it. Gestures pause while you type.
  The board is absolutely positioned and moved with a CSS transform.
- **Hosts:** a status dot per host in the top bar; a tap lists each host's state, since when and
  why. A host is named in the bar only after two minutes down.
- **Usage:** the top bar shows the tightest account's pace dial and slack, coloured by severity as
  crusteo's meters are, and hidden when nothing reports usage; a tap slides in a drawer with every
  account, its plan, the use-before-reset hint and each window's use and reset.
- **Going back:** Back, Esc, or a swipe to the right, which drags what it leaves (the conversation,
  the open card, a lane, a picture, the usage drawer) with the finger; past a third of the screen,
  or flicked, it goes, otherwise it springs back.
- **New agent:** the + in the top bar opens a form: the host, then its ready providers, model,
  mode and thinking (when the model has thinking options), a directory (the host's workspaces, or
  another typed in), labels, and the first message. It remembers the last choices on each host,
  in the device's preferences. Once created, the agent's conversation opens, on the map as its
  card opened up close.
- **Settings** (notify default, system notifications) and the hosts list open as sheets;
  notifications show as banners. The new-agent form, the label editor and confirmations are
  panels that rise from the bottom on a phone and sit in the middle on a desk; they redraw only
  themselves, so what is typed in them survives the fleet changing, and Back or Esc closes them.
- Every action shows while it runs and says why if it failed.
- The toolbar and composer are never redrawn under you, each pane redraws only itself, and nothing
  is redrawn while a finger is down, so taps land while a reply streams.
- At most one list view and one conversation are subscribed at a time.
- **Later, a native app** (Jetpack Compose, or a thin WebView/TWA shell around the same page) against
  the same protocol, if the page feels lacking: better notifications, share targets, a widget.
- crusteo desktop keeps talking to daemons directly. A "through Reef" mode is possible later.

## 5. Push notifications (later)

- **Now:** turn-finished notifications reach a phone whose page is open (3.10).
- **What push adds:** the same `notification` events for a phone whose page is closed. Later kinds
  might add an agent entering `needs_input`.
- **How:** Web Push with VAPID. Reef makes a VAPID key pair on first start. The page registers a
  service worker, subscribes with Reef's public key, and sends the subscription in a new message,
  `{"type": "push.register", "subscription": {...}}`, tied to its device token. Reef sends
  encrypted pushes straight to the browser's push service (an HTTPS request; no third-party server
  of ours).
- **Payload:** small and private by default: "Needs you: <agent title> on <host>", and the agent's
  key so a tap opens the conversation. No conversation text unless the user turns it on.
- **Rules:** one push per agent per transition; none for the device whose phone shows that
  conversation right now; a quiet period setting.
- **Needs HTTPS.** Browsers allow service workers and push only on secure origins. iOS delivers
  web push only to a page added to the home screen.
- Crates: Web Push needs ECDH P-256, HKDF and AES-GCM (RFC 8291) and an HTTPS client; none is in the
  lock yet. `rustls` is, so an HTTPS client is close; this is the one new-dependency decision of
  push.

## 6. Running it: Tailscale and HTTPS

```
reef --host-name <machine>.<tailnet>.ts.net /path/to/hosts.json   # listens on 127.0.0.1:6798
tailscale serve --bg --https=443 http://127.0.0.1:6798
```

`tailscale serve` passes the `*.ts.net` name on as `Host`, so Reef must be told it with
`--host-name`, or every request gets a 403 (section 7). Reaching Reef by IP address needs nothing.

- Reef binds loopback by default. `tailscale serve` terminates TLS with a certificate for the
  machine's `*.ts.net` name and forwards to it, so only the tailnet reaches Reef and the phone
  gets a real HTTPS origin (needed for push and home-screen install). Reef needs no TLS code.
- `--bind 0.0.0.0` serves a plain LAN or other private network over HTTP, which is fine where the
  network is locked down. Over plain HTTP everything works except system notifications, which
  need a secure origin (3.10).
- The box needs to reach every daemon (they listen on the tailnet or LAN). Daemons accept an IP
  address in `Host`, so use IPs or names they allow in `hosts.json`.
- First start prints a device token. Open `https://<machine>.<tailnet>.ts.net/#token=<token>` on the
  phone once; the page keeps it. On a network where reaching Reef at all is the lock, `--open`
  does without tokens.
- Run it as a service (systemd unit, launchd agent, or a Windows scheduled task) so it is always on.
- `--usage http://<feed>:<port>` adds a usage feed to the `usage` view (3.6); the daemons' own usage
  reports need nothing. Reef reads the feed; the feed need not be reachable from phones.
- `--demo` serves a made-up fleet from fake daemons inside the process, contacting nothing, for
  trying the page. Two of its daemons report usage for a few made-up accounts, one through both.

## 7. Security

Reef is meant for locked-down private networks; that is the decision for now.

- Anyone with a device token sees the fleet and can act on any agent: there are no scopes. Tokens
  are per device, random, compared in constant time, and revocable by editing the tokens file.
- A token holder can also add hosts (`host_add`), which makes Reef connect to any address it can
  reach, with any password, using Reef's own client id. Treat a token as access to Reef's
  network position, not only to the fleet.
- With `--open` there are no tokens: anyone who can reach Reef can act on any agent and add
  hosts.
- Plain HTTP on any bind address is allowed.
- Image URLs carry the device token in their query (`/image/<key>?t=`), so it can show up in a
  proxy's logs or the browser's history; Reef's answers say `Referrer-Policy: no-referrer`.
- Cross-site WebSockets are refused by the `Origin` check, and a page elsewhere cannot read the
  token, which lives in Skiff's own storage.
- DNS rebinding is refused by a `Host` allowlist on every request (page, `/ws`, `/image/`,
  `/healthz`): IP addresses, `localhost` and `*.localhost`, and names given with the repeatable
  `--host-name <name>` (for example the `*.ts.net` name `tailscale serve` forwards). Anything else
  gets a 403. A page that rebinds its own name to Reef's address sends that name, so `Origin ==
  Host` alone would let it in; this matters most with `--open`.
- Before its hello is accepted a socket may send 64 KiB in all and at most 16 may wait at once, so a
  stranger cannot make Reef buffer big frames.
- What Reef can send a daemon is a fixed list (2.1); anything else is refused before it leaves.

## 8. Tests

`cargo test -p reef` runs the unit tests and three end-to-end suites. Each starts fake daemons
(`reef::fake`, in process) and a Reef, and drives a phone over a real WebSocket; the phone
(`tests/support`) applies snapshots and deltas exactly as section 3.4 says and checks every sequence
number. The fake daemon serves the writes the way a daemon does (pushes first, then the reply) and
records each one whole.

- `end_to_end.rs`: token refusal; needs-you order and changes; list filters and windows; a
  conversation with an orchestrator's own turns grouped and footed; streamed text as appends; a
  turn's footer appearing when it ends; `detail`; `older` from loaded history and from a fetched
  page; a daemon dropping its connections and Reef catching up; and that watching sends only
  reads.
- `actions.rs`: each action sends exactly its one request, in crusteo's format: permission allow
  and deny; a question answered by header, refused while unanswered, and dismissed; stop (and no
  notification for it); send to an idle agent; send to a busy one queued and sent on the idle edge;
  steer, steering a queued message, and dropping one; archive; and asks or agents no longer there
  refused before anything is sent.
- `agents.rs`: the new-agent form's providers and workspaces, the providers read once and kept;
  creating an agent in a workspace or a typed directory, with the provider's defaults, its
  workspace labelled, and its conversation open at once; every create Reef cannot serve (an
  unknown host, provider, model, mode, thinking option or workspace, a provider not ready, bad
  labels) refused before anything is sent; deleting; relabelling with labels put on and taken
  off, and nothing sent when nothing changes; and, after each, that the daemon got nothing off
  Reef's list.
- `lists.rs`: grouped lists with counts that move in deltas and folded groups; rows' latest
  replies read once per shown agent, and again when a turn ends; hosts' `since`.
- `usage.rs`: two daemons' usage reports and a usage feed (`reef::fake::FakeFeed`) merged into one
  row per account, with deltas when either changes; phones subscribing and resyncing never make
  Reef read the feed; a Reef without usage sends an empty view and asks no daemon. Reef's rows
  for a fixed feed and set of reports must equal `crates/usage-source/tests/fixtures/agreement/
  rows.json`, which crusteo's gauges are checked against too.
- `map.rs`: tiles for every agent and nothing read for them; cards and their replies only for the
  focus; a tile's state change as a small nested delta; focus cleared.
- `notifications.rs`: a finished turn announced with the first line of the reply, once even when
  the stream and the directory both see it; a failed turn; per-agent and default settings, pushed
  to phones and saved.
- `background.rs`: a fake 0.12 daemon (`set_background_workspaces`) that hides background
  workspaces unless asked: Reef asks it (and only it) with `includeBackground`, rows carry
  `background`, a phone's filter hides them, Reef-wide setting hides them for phones that did
  not choose, and a hidden background agent's finished turn is still announced. `--demo` serves
  one such workspace on `demo-a`.

## 9. Open questions

1. **Queue after Stop.** As in the Paseo app, a message queued while a turn runs is sent when the
   turn ends, including when it ends because the phone pressed Stop. Should Stop also hold the
   queue?
2. **Queue across restarts.** Queued messages live in Reef's memory. Worth saving them?
3. **More notification kinds.** Should an agent starting to wait on a question or permission notify
   too, separately from a finished turn?
4. **Several users.** Tokens are per device, not per person, and settings are Reef-wide. Fine for one
   person's Reef; teammates would want their own settings.
5. **Sharing the store.** Reef ports crusteo's store glue rather than sharing it. Worth extracting
   an executor-agnostic `fleet-sync` crate that both use, now that there are two copies?
6. **History on the phone.** Should the agent list also offer archived agents (a separate, paged
   `fetch_agent_history_request` view)?

## 10. Next

1. **Push:** VAPID keys, `push.register`, delivering `notification` events to closed pages, and the
   new crypto and HTTPS client dependencies.
2. **Deployment:** a service unit, a `tokens` subcommand (add, list, revoke), and a short guide.
3. **Usage gauges** as another view, reusing `usage-source` and the daemons' usage reports.
4. Then the native app and, optionally, crusteo desktop through Reef.
