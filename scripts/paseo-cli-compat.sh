#!/usr/bin/env bash
# Runs Paseo's own CLI against a Burrow and prints one line per command:
# whether it worked, as docs/burrow/compatibility.md records it.
#
#   scripts/paseo-cli-compat.sh <paseo-cli> <host:port> <git repo dir>
#
# Use a throwaway Burrow with only the echo provider, which runs no model and
# answers each prompt with "echo: <prompt>":
#
#   burrow --listen 127.0.0.1:6801 --home /tmp/compat-home --echo-provider \
#     --no-claude --no-codex --no-agy --no-pi --no-cursor
#
# and a Paseo CLI installed on its own, e.g.
#
#   npm install --prefix /tmp/paseo-cli-0.13.2 @getpaseo/cli@0.13.2
#   scripts/paseo-cli-compat.sh /tmp/paseo-cli-0.13.2/node_modules/.bin/paseo \
#     127.0.0.1:6801 /tmp/compat-repo
#
# The repo needs a commit; a paseo.json with worktree.setup commands makes the
# worktree step run them. Agents it makes are deleted at the end.
set -u
cli=$1 host=$2 repo=$3
paseo() { "$cli" --host "$host" "$@"; }
version=$("$cli" --version 2>/dev/null | tail -1)
pass=0 fail=0
# check <name> <expected: ok|fails> <command...>: runs it, shows the outcome.
check() {
  local name=$1 want=$2; shift 2
  local out code
  out=$("$@" 2>&1); code=$?
  local got=ok; [ $code -ne 0 ] && got=fails
  local mark=PASS; [ "$got" != "$want" ] && mark=FAIL
  [ $mark = PASS ] && pass=$((pass + 1)) || fail=$((fail + 1))
  printf '%-5s %-8s %-34s %s\n' "$mark" "$version" "$name" "$(echo "$out" | tr '\n' ' ' | cut -c1-110)"
  LAST=$out
}
# The agent id in a command's JSON output (stdout only: notes go to stderr).
id_of() { python3 -c 'import json,sys; d=json.loads(sys.stdin.read()); d=d[0] if isinstance(d,list) else d; print(d.get("agentId") or d.get("id") or d.get("agent",{}).get("id",""))'; }
# run <title> <args...>: `paseo run --json`, checked, leaving the id in RUN_ID.
run() {
  local title=$1; shift
  local json
  json=$(paseo run --json --provider echo --cwd "$repo" --title "$title" "$@" 2>/dev/null)
  RUN_ID=$(echo "$json" | id_of 2>/dev/null)
  check "run $title: $*" ok test -n "$RUN_ID"
}

check "ls -g" ok paseo ls -g
run compat-a "hello"
a=$RUN_ID
check "send (waits for the reply)" ok paseo send "$a" "again"
check "logs show the reply" ok bash -c "'$cli' --host '$host' logs '$a' | grep -q 'echo: again'"
check "inspect" ok paseo inspect "$a"
check "send --no-wait, then wait" ok bash -c "'$cli' --host '$host' send --no-wait '$a' 'slow x' && '$cli' --host '$host' wait '$a'"
run compat-b --no-wait "start"
b=$RUN_ID
check "send from inside an agent" ok env PASEO_AGENT_ID="$a" "$cli" --host "$host" send "$b" "from agent a"
check "  shown as from that agent" ok bash -c "'$cli' --host '$host' logs '$b' | grep -qiE 'compat-a|from agent a'"
PASEO_AGENT_ID=$a run compat-child "child task"
child=$RUN_ID
check "agent detach" ok paseo agent detach "$child"
run compat-wt --new-workspace worktree "in a worktree"
check "stop" ok paseo stop "$a"
check "archive" ok paseo archive "$b"
check "ls -a shows the archived one" ok bash -c "'$cli' --host '$host' ls -a -g | grep -q compat-b"
check "delete one" ok paseo delete "$child"
check "delete --all" ok paseo delete --all
check "ls is empty after" ok bash -c "! '$cli' --host '$host' ls -a -g | grep -q compat-"
# Not in Burrow yet; recorded so a change shows.
check "host ls (connected hosts)" fails paseo host ls
echo "$version: $pass as expected, $fail not"
