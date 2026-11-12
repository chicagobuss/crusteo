# Recording the fleet

Reef can record every agent on every host it reaches: each prompt, reply, thought, tool call and
finished turn, scrubbed of secrets, sent to any number of **sinks**. A sink is a file of JSON lines,
a program Reef starts, or a web service Reef posts to, so records can go to a log store, a search
index, a bucket, a queue or anything else a few lines of code can write to. Every record has a
`body`, its searchable text in one field, and an agent deleted on its host gets a `forget` record,
so an index can drop what it holds for it.

Burrow's own timeline is the buffer. Each sink has a cursor for each agent, which moves only when
the sink takes a batch. After the sink, the store behind it or Reef itself was down, recording
resumes from the daemon where that sink stopped. Nothing is spooled on disk but the cursors.

Recording reads only Burrow's and Paseo's timelines, so it sees agents those daemons run. It is off
until `record.json` turns it on.

## `record.json`

Reef reads `record.json` from its data dir (`reef-data/` beside `hosts.json`, or `--data-dir`),
and reads it again within a second of any change, with no restart. A file that doesn't parse is
reported in Reef's log and the settings it had stay.

```json
{
  "follow_all": true,
  "redact": {
    "builtin": true,
    "patterns": [
      { "name": "customer-id", "regex": "CUS-[0-9]{6}", "replace": "[CUSTOMER]" }
    ],
    "drop": ["tool_output"]
  },
  "sinks": [
    { "name": "log", "type": "jsonl", "dir": "record/log" },
    {
      "name": "search",
      "type": "exec",
      "mode": "plain",
      "command": ["vector", "--config", "/etc/reef/vector.yaml"],
      "kinds": ["prompt", "reply", "tool_call", "turn_finished"]
    },
    {
      "name": "notes",
      "type": "exec",
      "command": ["/usr/local/bin/my-sink"],
      "env": { "STORE_URL": "https://store.example" },
      "settings": { "anything": "the program wants" },
      "kinds": ["turn_finished"],
      "labels": ["billing"],
      "redact": { "drop": ["thought"] }
    },
    {
      "name": "index",
      "type": "http",
      "url": "https://search.example/feed",
      "token_env": "SEARCH_FEED_TOKEN",
      "kinds": ["prompt", "reply", "tool_call", "turn_finished"],
      "from": "start"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `follow_all` | Follow every agent, not only those a phone has open. Recording needs it. |
| `redact.builtin` | The built-in patterns (below). On unless turned off. |
| `redact.patterns` | Your own: a name, a regular expression, and its replacement (default `[REDACTED]`). |
| `redact.drop` | Parts left out of every record: `thought`, `tool_input`, `tool_output`. Default `["tool_output"]`. |
| `sinks[].name` | Letters, digits, `-` and `_`. The sink's cursors are kept in `record/<name>.json`. |
| `sinks[].type` | `jsonl`, `exec` or `http`. |
| `sinks[].kinds` | Only these kinds of record. Empty or missing: all. A `forget` record is sent whatever this says. |
| `sinks[].hosts` | Only agents on these hosts, by name. |
| `sinks[].labels` | Only agents whose workspace has one of these labels. |
| `sinks[].redact` | More `patterns` and `drop`s, after the shared ones, for this sink only. |
| `sinks[].batch` | The most records in one batch. Default 200. |
| `sinks[].from` | Where an agent the sink has no cursor for begins: `"now"` (default) or `"start"`. |

A sink whose settings change starts again with the cursors it saved; one removed is stopped.

### Where a sink begins: `from`

With `"now"`, a new sink starts at each agent's end: it gets what happens from then on, not the
history before.

With `"start"`, an agent the sink has no cursor for is read from the beginning of its timeline, as
far back as the daemon still holds it, and then on as it goes. A search index made today then holds
yesterday's work too. It is Burrow's timeline that is read, so there is nothing new to store; the
sink's cursor starts at the beginning and moves as the sink takes batches.

- **Paced.** Reef reads a few agents at a time (eight reads in flight across the whole fleet, 200
  entries each), so a fleet of thousands does not swamp the daemons, and it stops reading an agent's
  backlog, and beginning on more agents, while a sink has many records it has not taken, so it never
  reads faster than the sink can use it. An agent whose turn has just ended is read ahead of the
  backlog, so live recording goes on while it works, and a sink behind holds up no other.
- **Resumes.** A Reef or a sink that stops partway goes on from the sink's cursor. A cursor for an
  agent is made once; changing `from` on a sink that has them does not read those agents again. To
  read everything again, stop Reef and delete `record/<name>.json` (the sink's cursors), or use a
  new name.
- **Other sinks** keep their place. A sink that starts from now is not sent the history another
  sink is reading, though it may see the summary of the agent's last turn again: a program sees
  each record at least once.
- **Archived agents are left out.** Reef follows the agents a daemon lists as active, so an
  archived agent has no history read for it, and one archived while a sink was reading it stops
  there. Whether a backfill should reach them is open (docs/design/surface-area.md, §8).

### `jsonl`

Appends each record as one line to `<dir>/<YYYY-MM-DD>.jsonl` (UTC), a file a day, synced to disk
before the batch counts. `dir` is relative to the data dir unless absolute. Tools that tail files
(Vector, Fluent Bit, Promtail) can read it from there.

### `exec`

Reef starts `command` (with `env` added to its environment), keeps it running, and feeds it over
its stdin. What it writes to stderr goes to Reef's log under the sink's name. If it exits or stops
answering, Reef starts it again with a growing pause, up to a minute, and nothing is lost: the
batch it had is sent again.

- **`mode: "acks"`** (the default) speaks `reef-sink/1`, below. A batch counts once the program
  acknowledges it, so an outage of the program, or of whatever it writes to, resumes exactly.
- **`mode: "plain"`** writes each record as a bare JSON line and counts a batch once written. Any
  tool that reads JSON lines on stdin works with no code: [Vector](https://vector.dev) (whose
  sinks cover Elasticsearch, S3 and R2, Kafka, Loki, ClickHouse, Datadog and many more),
  `kcat -P` for Kafka, Fluent Bit. A crash can lose or repeat a few records.

`packaging/reef/record/vector-elasticsearch/` is a complete example: Elasticsearch and Kibana in
Docker, and Vector fed by Reef.

### `http`

For a service that is not on Reef's machine, or that you would rather run apart from Reef: Reef
POSTs each batch to `url`, `reef-sink/1` over HTTP or HTTPS.

| Field | Meaning |
|---|---|
| `url` | Where batches go: `http://` or `https://`. Records leave the machine only for a URL put here, scrubbed first, as for every sink. |
| `token_env` | The name of an environment variable (of Reef's process) that holds a token, sent as `Authorization: Bearer <token>` with each batch. Left out: no header. If it is named and not set, the sink sends nothing and logs why. |

The request is `POST <url>` with `Content-Type: application/json` and this body:

```json
{"protocol": "reef-sink/1", "sink": "index", "id": 7, "records": [{...}, {...}]}
```

`id` is the batch's number, which grows with each try, as for a program; `sink` is the sink's name.
The next batch is sent only when this one is answered. The answer is the status:

| Answer | Meaning |
|---|---|
| any `2xx` | An `ack`: the records are safe where the service puts them. |
| `429` or `503` with `Retry-After: <seconds>` | A `retry`: the same records again after that long (100 ms to 10 minutes). |
| anything else (another status, no `Retry-After`, nothing listening, no answer in two minutes) | Logged, and tried again after a pause that grows from a second to a minute. Nothing is dropped. |

Redirects are not followed, so the token never goes where the settings did not say. Reef's log
names the status and the first words of the answer, so a wrong token (`401`) or a body that is too
big (`413`: lower `batch`) shows there. As with a program, records are upserts by `id` and a
service sees each at least once.

## The protocol, `reef-sink/1`

JSON lines both ways: one JSON object per line, UTF-8, `\n` at the end. (An `http` sink sends the
batch as the body of a POST and answers with the status, as above; the records are the same.)

Reef writes, first:

```json
{"type":"hello","protocol":"reef-sink/1","sink":"notes","reef":"0.1.0","settings":{}}
```

then a batch at a time, the next only after this one is answered:

```json
{"type":"batch","id":7,"records":[{...},{...}]}
```

The program answers each batch by its `id`:

```json
{"type":"ack","id":7}
{"type":"retry","id":7,"after_ms":5000,"error":"the store is down"}
```

An `ack` means the records are safe where the program puts them. A `retry` sends the same batch
again after `after_ms` (100 ms to 10 minutes). Any other line is ignored, so logging to stdout by
mistake does no harm; log to stderr. A batch unanswered for two minutes counts as a retry, and the
program is started again.

A program sees each record at least once. Records are upserts by `id`: keep the latest.

## Records, `fleet-event/1`

```json
{
  "v": "fleet-event/1",
  "id": "box/agt_1/epoch-3/412",
  "key": "4c0f1e6b8a2d5c7e9f0a1b2c3d4e5f60",
  "source": "reef@0.1.0",
  "time": "2026-11-01T10:00:00.000Z",
  "host": "box",
  "agent": { "id": "agt_1", "title": "Fix the build", "provider": "claude", "model": "claude-opus-5-5" },
  "workspace": { "id": "wks_1", "title": "shop-checkout", "labels": ["billing"], "cwd": "/home/dev/shop", "branch": "main" },
  "turn": "turn-17",
  "kind": "tool_call",
  "partial": false,
  "tool": { "name": "Bash", "status": "completed", "input": { "command": "cargo build" } },
  "body": "Bash\ncargo build",
  "redaction": "builtin/1+5e2a9c01"
}
```

| Field | Meaning |
|---|---|
| `id` | `host/agent/epoch/seq`: where in the agent's timeline it is. Stable: a reply that streams, or a tool call that finishes, is sent again under the same `id` with the latest. |
| `key` | For pairing with records from elsewhere (a session hook): SHA-256, cut to 128 bits, of the agent id, the kind and the normalized text, hex. No time in it. |
| `time` | When it happened, from the daemon, not when it was sent. |
| `kind` | `prompt`, `reply`, `thought`, `tool_call`, `todo`, `error`, `notice`, `compaction`, `turn_finished`, `gap`, `forget`. |
| `partial` | Still growing: a reply streaming, a tool call running. A later record with the same `id` replaces it. |
| `text` | A prompt's, reply's, thought's, error's or notice's text. |
| `tool` | A tool call: `name`, `status`, `input` and `output` (strings cut to 4000 characters), `error`. |
| `todo` | A task list's items. |
| `body` | The record's searchable text in one field, whatever its kind (below). Always there, an empty string when there is nothing to search. |
| `redaction` | Which rules scrubbed it: `builtin/1`, plus a hash of any custom rules. |

A `turn_finished` record sums a turn up: `prompt`, `reply` (the last), `tools` (calls by name),
`started`, `time` (ended), and `whole`, false when the turn began before Reef read it. A `gap`
record says the daemon's timeline started again (a new `epoch`), so records may be missing
before it.

### `body`: one field to search

The words of a record are in `text` for a reply, in `tool.name` and `tool.input` for a tool call,
and in `prompt` and `reply` for a finished turn. An index that searches `body` reaches them all, and
keeps the structured fields for filters and display.

| Kind | `body` |
|---|---|
| `prompt`, `reply`, `thought`, `error`, `notice` | `text` |
| `tool_call` | the tool's name, then its input as text, then its output, a line each |
| `todo` | the items' text, one a line |
| `turn_finished` | the prompt, then the reply, a blank line between |
| `compaction`, `gap`, `forget` | empty |

A tool's input or output as text is a string as it is, and a structure as the strings in it, one a
line (its keys are structure, not words). `body` is made from what is left of the record after
`redact.drop`: with `tool_output` dropped, no output is in it, as none is in `tool`, and with
`tool_input` dropped, no input. It is then scrubbed with the same patterns as the fields, as one
text, so a secret that spans two of its parts is caught too.

### `forget`: an agent was deleted

When an agent is deleted on its host, each sink that has a cursor for it gets one more record, and
drops the cursor once it has taken it:

```json
{"v": "fleet-event/1", "id": "box/agt_1/forget", "kind": "forget", "time": "2026-11-01T10:00:00.000Z",
 "host": "box", "agent": {"id": "agt_1"}, "body": ""}
```

The sink deletes everything it holds for that agent. A `forget` is sent whatever the sink's `kinds`
say, to sinks that take that agent's host (`hosts`), and is a record like any other: at least once,
and in order after the agent's last record.

- **Archiving is not deleting, and sends nothing.** A daemon's `agent_update` with `kind: "remove"`
  means only that the agent left its active list: Burrow sends it for an archived agent as well as a
  deleted one, and Paseo does too. So Reef asks the daemon for the agent by id
  (`fetch_agent_request`, which answers for archived agents too). One it still has is archived and
  is left alone, with its cursors; one it has not is deleted. An answer that is an error says
  nothing, and Reef asks again.
- **A deletion Reef did not see is caught up.** When Reef connects to a host (or `record.json`
  changes), each agent a sink has a cursor for and the host no longer lists is asked about in the
  same way. A host that was down, or a Reef that was off, when an agent was deleted forgets it
  then. A daemon says nothing when an *archived* agent is deleted, so that is found the same way,
  the next time Reef connects to the host or starts. An agent found archived is not asked about
  again for an hour, so a host that drops and returns often is not asked again and again.
- **A host removed from Reef is not a deletion.** Its agents are not forgotten.
- **Archived agents keep their cursors**, so a sink's cursor file holds one for every agent it has
  seen until the agent is deleted.

### The dedupe key

To pair a record with one a hook made of the same prompt or reply, compute the key the same way:
normalize the text (remove `\r`, the `<USER_REQUEST>` and `</USER_REQUEST>` tags, one leading `>`
or `> ` per line, then surrounding whitespace), then take the SHA-256 of
`<agent id>\n<kind>\n<normalized text>` and keep the first 16 bytes as hex. Two records are the
same event when their keys match and their times are close (30 s).

## Built-in redaction

On by default, as the session hook scrubs: PEM blocks, JWTs, AWS access keys, `sk-` API keys,
GitHub, Slack and Google tokens, `Bearer` tokens, values of `*SECRET*`, `*TOKEN*`, `*PASSWORD*`,
`*APIKEY*` and `*PRIVATE_KEY*` settings, credentials in URLs, and email addresses. Patterns apply
to what agents and people wrote (`text`, `prompt`, `reply`, `body`, every string in `tool` and
`todo`), before any sink's own; Reef's own fields, such as `id` and `source`, are left as they are.
