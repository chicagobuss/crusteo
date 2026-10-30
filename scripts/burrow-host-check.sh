#!/usr/bin/env bash
# What people and agents on a host get from the Burrow running there, used the
# way they use it: Paseo's CLI with no --host and no PASEO_* in the environment.
# paseo-cli-compat.sh checks the commands against a throwaway Burrow and passes
# --host itself; this checks the host's own Burrow, as it is.
#
#   scripts/burrow-host-check.sh <paseo CLI> [--agents]
#
# Run it on the host, as the user Burrow runs as. Without --agents it only reads
# (and runs `paseo daemon start`, which must start nothing). With --agents it
# also makes real agents (Claude, the cheapest model it offers, and Codex), so it
# spends a little quota, then archives them and restarts Burrow to check that
# its agents' CLIs end with it. Run that only on a host with no agent mid-turn.
set -u
cli=${1:?usage: burrow-host-check.sh <paseo CLI> [--agents]}
agents=${2:-}
for v in $(env | grep -o '^PASEO_[A-Z_]*'); do unset "$v"; done
pass=0
fail=0
ok() { pass=$((pass + 1)); echo "PASS $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }
check() {
  local name=$1
  shift
  if out=$(timeout 60 "$@" 2>&1); then ok "$name"; else bad "$name"; fi
  printf '%s\n' "$out" | head -n 3 | cut -c1-160 | sed 's/^/     /'
}
burrow_pid() { pgrep -u "$(id -u)" -f 'burrow --listen' | head -1; }
echo "### $(hostname): $(timeout 10 "$cli" --version 2>/dev/null | tail -1) against Burrow pid $(burrow_pid)"

lock=${PASEO_HOME:-$HOME/.paseo}/paseo.pid
if [ "$(grep -o '"pid": *[0-9]*' "$lock" 2>/dev/null | grep -o '[0-9]*$')" = "$(burrow_pid)" ]; then
  ok "Burrow holds Paseo's daemon lock"
else
  bad "Burrow holds Paseo's daemon lock ($lock)"
fi
check "ls" "$cli" ls -g
check "ls from a bare environment" env -i HOME="$HOME" PATH="$(dirname "$(command -v node)"):/usr/bin:/bin" "$cli" ls -g
check "status" "$cli" status
check "provider ls" "$cli" provider ls
check "provider models claude" "$cli" provider models claude
check "provider diagnostic claude" "$cli" provider diagnostic claude
if "$cli" daemon pair 2>&1 | grep -q 'Relay pairing is disabled'; then ok "daemon pair: relay off"; else bad "daemon pair"; fi
before=$(burrow_pid)
if "$cli" daemon start 2>&1 | grep -q 'Already running'; then ok "daemon start finds Burrow running"; else bad "daemon start"; fi
[ "$(burrow_pid)" = "$before" ] && ok "and Burrow is the same process" || bad "Burrow changed"

if [ "$agents" = --agents ]; then
  probe=$(mktemp)
  cat > "$probe" <<'EOF'
# Run inside an agent: what its own paseo CLI does.
cli=$1; busy=$2; model=$3
env | grep '^PASEO_' | sort
"$cli" inspect "$PASEO_AGENT_ID" 2>&1 | grep -c "$PASEO_AGENT_ID" | sed 's/^/inspect-self: /'
"$cli" send --no-wait "$busy" "a message from the probe agent" 2>&1 | grep -c sent | sed 's/^/sent: /'
"$cli" run --no-wait --provider claude --model "$model" --title host-check-child "Reply with exactly: CHILD OK" 2>&1 | tail -1
EOF
  uuid() { grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -1; }
  model=$("$cli" provider models claude --json 2>/dev/null | python3 -c '
import json, sys
models = json.load(sys.stdin)
models = models.get("models", models) if isinstance(models, dict) else models
ids = [m.get("id") for m in models if m.get("id")]
print(next((i for i in ids if "haiku" in i), ids[0]))')
  work=$(mktemp -d)
  busy=$("$cli" run --provider codex --title host-check-busy --cwd "$work" --no-wait \
    "Run this shell command in the foreground and wait for it: sleep 45. Then reply with exactly: BUSY DONE" 2>&1 | uuid)
  for _ in $(seq 1 30); do "$cli" inspect "$busy" 2>/dev/null | grep -q '^Status *running' && break; sleep 2; done
  probe_agent=$("$cli" run --provider claude --model "$model" --title host-check-probe --cwd "$work" --no-wait \
    "Use your shell tool to run exactly this, then reply with its output verbatim: bash $probe $cli $busy $model" 2>&1 | uuid)
  "$cli" wait "$probe_agent" --timeout 300 >/dev/null 2>&1
  said=$("$cli" logs "$probe_agent" --filter text --tail 1 2>&1)
  printf '%s\n' "$said" | grep -q "PASEO_AGENT_ID=$probe_agent" && ok "an agent knows its own id" || bad "PASEO_AGENT_ID"
  printf '%s\n' "$said" | grep -q '^PASEO_HOST=' && ok "and where its daemon is" || bad "PASEO_HOST"
  printf '%s\n' "$said" | grep -q 'inspect-self: [1-9]' && ok "its bare paseo inspect works" || bad "inspect from inside"
  printf '%s\n' "$said" | grep -q 'sent: 1' && ok "its bare paseo send works" || bad "send from inside"
  timeline=$HOME/.burrow/timelines/$busy.jsonl
  if python3 - "$timeline" "$probe_agent" <<'EOF'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if '"seq"' in line]
first = rows[0].get("turnId")
hits = [r for r in rows if (r["item"].get("agentMessage") or {}).get("sender", {}).get("id") == sys.argv[2]]
sys.exit(0 if hits and hits[0].get("turnId") == first else 1)
EOF
  then ok "the message is from the agent, and steered the busy turn"; else bad "provenance or steer"; fi
  child=$("$cli" ls -g --json 2>/dev/null | python3 -c '
import json, sys
print(next((a["id"] for a in json.load(sys.stdin) if (a.get("title") or a.get("name")) == "host-check-child"), ""))')
  parent=$(python3 -c 'import json, sys; print((json.load(open(sys.argv[1])).get("labels") or {}).get("paseo.parent-agent-id", ""))' \
    "$HOME/.burrow/agents/$child.json" 2>/dev/null)
  [ "$parent" = "$probe_agent" ] && ok "its child is its own" || bad "child's parent: $parent"
  "$cli" send "$probe_agent" "Use your shell tool to start the command sleep 4321 as a background task (run_in_background), do not wait for it, and reply with exactly: STARTED" >/dev/null 2>&1
  "$cli" wait "$probe_agent" --timeout 120 >/dev/null 2>&1
  [ -n "$(pgrep -f 'sleep 4321')" ] && ok "the agent's background task runs" || bad "no background task to check"
  "$cli" archive "$probe_agent" >/dev/null 2>&1
  sleep 5
  [ -z "$(pgrep -f 'sleep 4321')" ] && ok "and ends when the agent is archived" || { bad "the background task outlived its agent"; pkill -f 'sleep 4321'; }
  clis=$(pgrep -P "$(burrow_pid)" | tr '\n' ' ')
  systemctl --user restart burrow 2>/dev/null || sudo -n /etc/init.d/burrow restart >/dev/null
  sleep 8
  left=""
  for p in $clis; do [ -d "/proc/$p" ] && left="$left $p"; done
  [ -z "$left" ] && ok "agents' CLIs ($clis) ended with Burrow" || bad "CLIs outlived Burrow:$left"
  for id in "$busy" "$child"; do [ -n "$id" ] && "$cli" archive "$id" >/dev/null 2>&1; done
  rm -f "$probe"
  rm -rf "$work"
fi
echo "### $(hostname): $pass passed, $fail failed"
[ "$fail" = 0 ]
