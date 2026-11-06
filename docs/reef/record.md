# Recording the fleet

Reef can record every agent on every host it reaches: each prompt, reply, thought, tool call and
finished turn, scrubbed of secrets, sent to any number of **sinks**. A sink is a file of JSON lines
or a program Reef starts, so records can go to a log store, a search index, a bucket, a queue or
anything else a few lines of code can write to.

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
| `sinks[].type` | `jsonl` or `exec`. |
| `sinks[].kinds` | Only these kinds of record. Empty or missing: all. |
| `sinks[].hosts` | Only agents on these hosts, by name. |
| `sinks[].labels` | Only agents whose workspace has one of these labels. |
| `sinks[].redact` | More `patterns` and `drop`s, after the shared ones, for this sink only. |
| `sinks[].batch` | The most records in one batch. Default 200. |

A new sink starts at each agent's end: it gets what happens from then on, not the history before.
A sink whose settings change starts again with the cursors it saved; one removed is stopped.

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

## The protocol, `reef-sink/1`

JSON lines both ways: one JSON object per line, UTF-8, `\n` at the end.

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
  "redaction": "builtin/1+5e2a9c01"
}
```

| Field | Meaning |
|---|---|
| `id` | `host/agent/epoch/seq`: where in the agent's timeline it is. Stable: a reply that streams, or a tool call that finishes, is sent again under the same `id` with the latest. |
| `key` | For pairing with records from elsewhere (a session hook): SHA-256, cut to 128 bits, of the agent id, the kind and the normalized text, hex. No time in it. |
| `time` | When it happened, from the daemon, not when it was sent. |
| `kind` | `prompt`, `reply`, `thought`, `tool_call`, `todo`, `error`, `notice`, `compaction`, `turn_finished`, `gap`. |
| `partial` | Still growing: a reply streaming, a tool call running. A later record with the same `id` replaces it. |
| `text` | A prompt's, reply's, thought's, error's or notice's text. |
| `tool` | A tool call: `name`, `status`, `input` and `output` (strings cut to 4000 characters), `error`. |
| `todo` | A task list's items. |
| `redaction` | Which rules scrubbed it: `builtin/1`, plus a hash of any custom rules. |

A `turn_finished` record sums a turn up: `prompt`, `reply` (the last), `tools` (calls by name),
`started`, `time` (ended), and `whole`, false when the turn began before Reef read it. A `gap`
record says the daemon's timeline started again (a new `epoch`), so records may be missing
before it.

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
to what agents and people wrote (`text`, `prompt`, `reply`, every string in `tool` and `todo`),
before any sink's own; Reef's own fields, such as `id` and `source`, are left as they are.
