#!/bin/bash
# Upgrades Burrow on a Mac that runs it as the launchd agent in
# dev.crusteo.burrow.plist, safely from anywhere: an SSH session, or an agent
# that Burrow itself runs (which the restart would otherwise end mid-step).
#
#   upgrade-macos.sh <new burrow binary>
#
# It hands the work to a one-off launchd job in your login session, then
# returns. The job waits until no agent is mid-turn (up to 30 minutes), stops
# Burrow, keeps the old binary as burrow.prev, installs the new one, signs it,
# starts Burrow again and checks it listens. Its log is upgrade.log beside the
# binary.
#
# Signing is what keeps macOS's Local Network permission. macOS gives it to
# a program by its signature, and an ad-hoc signature (what cargo and the
# release archive give) is a new program with every build: each upgrade then
# loses the permission, and every agent Burrow runs, with their tools, gets
# "No route to host" for the LAN and the VPN until it is allowed again. Signed
# with the same certificate and identifier every time, it is allowed once.
# Any code signing certificate in your login keychain does, a self-signed one
# included (Keychain Access > Certificate Assistant > Create a Certificate,
# type Code Signing). Name it, or its SHA-1 hash, in BURROW_SIGN_IDENTITY or
# on the first line of ~/.config/burrow/sign-identity. The job signs from the
# login session, where the keychain is open; codesign over SSH usually cannot.
#
# Environment: BURROW_DIR (the binary's folder, default ~/burrow),
# BURROW_HOME (the state folder, default ~/.burrow), BURROW_SIGN_IDENTITY.
set -u

LABEL=dev.crusteo.burrow
JOB=dev.crusteo.burrow-upgrade
DIR=${BURROW_DIR:-$HOME/burrow}
HOME_DIR=${BURROW_HOME:-$HOME/.burrow}
PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
LOG=$DIR/upgrade.log
IDLE_WAIT_SECONDS=1800

if [ "${1:-}" != "--job" ]; then
  new=${1:?usage: upgrade-macos.sh <new burrow binary>}
  "$new" --version >/dev/null || { echo "$new does not run" >&2; exit 1; }
  [ -f "$PLIST" ] || { echo "no $PLIST: install the launchd agent first" >&2; exit 1; }
  identity=${BURROW_SIGN_IDENTITY:-$(head -n 1 "$HOME/.config/burrow/sign-identity" 2>/dev/null)}
  [ -n "$identity" ] || echo "warning: no signing identity; macOS may ask again for Local Network access" >&2
  install -m 0755 "$new" "$DIR/burrow.new"
  install -m 0755 "$0" "$DIR/upgrade-job.sh"
  launchctl remove "$JOB" 2>/dev/null
  launchctl submit -l "$JOB" -- /usr/bin/env BURROW_DIR="$DIR" BURROW_HOME="$HOME_DIR" \
    BURROW_SIGN_IDENTITY="$identity" /bin/bash "$DIR/upgrade-job.sh" --job
  echo "queued $("$DIR/burrow.new" --version); it installs once no agent is mid-turn. Log: $LOG"
  exit 0
fi

exec >>"$LOG" 2>&1
say() { echo "$(date '+%F %T') $*"; }
done_() {
  launchctl remove "$JOB"
  exit "$1"
}
# Agents mid-turn, from Burrow's records (top-level "status" of each).
busy() { grep -l -E '^  "status": "(running|initializing)"' "$HOME_DIR"/agents/*.json 2>/dev/null | wc -l | tr -d ' '; }

say "upgrade to $("$DIR/burrow.new" --version) queued"
# Lets an agent that asked for this finish its turn first.
sleep 30
waited=0
while [ "$(busy)" != 0 ]; do
  if [ "$waited" -ge "$IDLE_WAIT_SECONDS" ]; then
    say "$(busy) agents still mid-turn after $((IDLE_WAIT_SECONDS / 60)) minutes; not upgrading"
    done_ 1
  fi
  sleep 10
  waited=$((waited + 10))
done

say "stopping burrow"
launchctl bootout "gui/$(id -u)/$LABEL"
for _ in $(seq 1 30); do
  pgrep -f "$DIR/burrow --listen" >/dev/null || break
  sleep 1
done
cp -p "$DIR/burrow" "$DIR/burrow.prev"
install -m 0755 "$DIR/burrow.new" "$DIR/burrow"
if [ -n "${BURROW_SIGN_IDENTITY:-}" ]; then
  if codesign --force --sign "$BURROW_SIGN_IDENTITY" --identifier "$LABEL" "$DIR/burrow"; then
    say "signed as $LABEL"
  else
    say "signing failed; it runs ad-hoc signed, and macOS may ask for Local Network access again"
  fi
else
  say "no signing identity; it runs ad-hoc signed"
fi
say "starting $("$DIR/burrow" --version)"
launchctl bootstrap "gui/$(id -u)" "$PLIST"
for _ in $(seq 1 30); do
  lsof -nP -a -c burrow -iTCP -sTCP:LISTEN >/dev/null 2>&1 && break
  sleep 1
done
if lsof -nP -a -c burrow -iTCP -sTCP:LISTEN; then
  say "finished"
  done_ 0
fi
say "burrow is not listening; putting burrow.prev back"
launchctl bootout "gui/$(id -u)/$LABEL"
cp -p "$DIR/burrow.prev" "$DIR/burrow"
launchctl bootstrap "gui/$(id -u)" "$PLIST"
done_ 1
