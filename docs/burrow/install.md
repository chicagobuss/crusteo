# Installing Burrow

`burrow` is crusteo's own daemon. It runs coding agents on one host (Claude Code, Codex,
Antigravity's `agy`, pi and Cursor's agent, with your own installs of each) and speaks Paseo's
daemon protocol, so crusteo, `reef` and the Paseo apps connect to it as they connect to
Paseo's daemon. What works and what does not yet is in [README.md](README.md).

It can run [next to Paseo](#run-it-next-to-paseo), on its own port with its own state, or
[replace Paseo on a host](#replace-paseo-on-a-host), keeping the host's identity and its Claude
agents.

> **Burrow has no password support yet.** Anyone who can reach its port can run agents as you.
> Keep it on `127.0.0.1`, or bind it wider only on a locked-down network or tailnet.

This guide uses `user` for the account and `192.0.2.10` for the host's address; use your own.

## Download

Each [GitHub release](https://github.com/chicagobuss/crusteo/releases) has an archive per
platform, and `SHA256SUMS.txt` for all of them:

| Platform | Archive |
|---|---|
| Linux x86_64 (static, any distribution) | `burrow-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 (static, any distribution) | `burrow-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS (Apple Silicon and Intel) | `burrow-<version>-macos-universal.tar.gz` |
| Windows x86_64 | `burrow-<version>-x86_64-pc-windows-msvc.zip` |

Linux is where Burrow is developed and tested. On macOS it has replaced Paseo on an Apple
Silicon Mac since 0.1.0-beta.1; on Windows only its tests run.

Each archive holds the `burrow` binary, this guide as `INSTALL.md`, the license, the changelog,
and `examples/`: two systemd user units and a launchd agent (in the source tree they are in
`packaging/burrow/`).

```sh
version=0.1.0-beta.1
target=x86_64-unknown-linux-musl
base=https://github.com/chicagobuss/crusteo/releases/download/v$version
curl -LO "$base/burrow-$version-$target.tar.gz"
curl -LO "$base/SHA256SUMS.txt"
sha256sum --check --ignore-missing SHA256SUMS.txt
tar -xzf "burrow-$version-$target.tar.gz"
```

On macOS use `shasum -a 256 --check --ignore-missing SHA256SUMS.txt`. The binary is not signed or
notarized; one downloaded with a browser needs `xattr -d com.apple.quarantine burrow` before it
runs.

## Install

The examples keep the binary in `~/burrow`; its state goes to `~/.burrow`.

```sh
mkdir -p ~/burrow
install -m 0755 "burrow-$version-$target/burrow" ~/burrow/
~/burrow/burrow --version      # burrow 0.1.0-beta.1 (<commit>)
```

Burrow offers each agent it finds: `claude`, `codex`, `pi`, `agy` and `cursor-agent` on PATH,
or the binaries `--claude-bin`, `--codex-bin`, `--pi-bin`, `--agy-bin` and `--cursor-bin` name.
`--no-claude` and the like turn one off. Its startup log says which it offers.

## Try it

```sh
~/burrow/burrow                      # ws://127.0.0.1:6797/ws, state in ~/.burrow
~/burrow/burrow --listen 0.0.0.0:6797
~/burrow/burrow --listen 127.0.0.1:6797 --listen 10.0.0.5:6797   # loopback and a VPN only
```

`--listen` can be given more than once. The first address must be free when Burrow starts; each
other one is tried again every few seconds until it binds. So a VPN's address, which appears only
once the VPN is up (after a reboot, say), is picked up on its own. On a laptop that joins other
networks, list loopback and the VPN rather than `0.0.0.0`: Burrow has no password support yet.

Then add the host in crusteo (`127.0.0.1:6797`, or `192.0.2.10:6797` from elsewhere) or in
Reef's `hosts.json`. It reports its version to clients as `<version>-burrow`. Every flag is in
[README.md](README.md#run-it-next-to-paseo); `BURROW_LOG=debug` turns on verbose logs.

## Run it as a service

### Linux (systemd user unit)

Burrow runs as you, with your agents' logins, so it is a **user** service. There are two
variants of the unit; install either as `~/.config/systemd/user/burrow.service`.

- **`examples/burrow.service`** starts Burrow through your login shell
  (`/bin/bash -lic 'exec …'`), so agents get the PATH and API keys your `~/.profile` and
  `~/.bashrc` set. Without it they miss keys: pi, for one, then lists only the few models it needs
  no key for.
- **`examples/burrow-environment.service`** writes the environment out instead: `PATH`, an
  `ANTHROPIC_BASE_URL` for a local proxy, and API keys from `~/.config/burrow/env`
  (`KEY=value` lines, `chmod 600`). Use it where the shell setup should not run in a service.

Both listen on `0.0.0.0:6797`; change that to `127.0.0.1` or to Paseo's port as needed.

```sh
mkdir -p ~/.config/systemd/user
cp examples/burrow.service ~/.config/systemd/user/burrow.service
systemctl --user daemon-reload
systemctl --user enable --now burrow
sudo loginctl enable-linger user       # keep it running while you are logged out
journalctl --user -u burrow -f
```

On SIGTERM, which `systemctl stop` sends, Burrow stops its agents and flushes its state.

### macOS (launchd)

`examples/dev.crusteo.burrow.plist` is a launchd agent that starts Burrow through `zsh -lic`
at login, on `127.0.0.1:6797`, logging to `burrow.log` beside the binary. Edit the paths, then:

```sh
cp examples/dev.crusteo.burrow.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.crusteo.burrow.plist
launchctl bootout gui/$(id -u)/dev.crusteo.burrow      # to stop it
```

This is how Burrow runs on the Mac it replaced Paseo on (with `--listen 127.0.0.1:6767` and a
second `--listen` for its VPN address, so other machines reach it without a proxy).

**Local Network access.** macOS asks once whether Burrow may reach devices on the local
network, and the agents Burrow runs, with every tool they start, share its answer: without it,
they get "No route to host" for LAN and VPN addresses (Apple's own tools such as `curl` and
`ssh` are exempt, which hides it). macOS ties the answer to the program's signature, and an
ad-hoc signature, which the release archive and `cargo build` give, is a new program with every
build. Sign Burrow with a code signing certificate, the same one and the same identifier every
time, and the answer carries over upgrades. A self-signed certificate does: Keychain Access >
Certificate Assistant > Create a Certificate, type Code Signing. Then:

```sh
codesign --force --sign "<certificate name or SHA-1>" --identifier dev.crusteo.burrow ~/burrow/burrow
```

or upgrade with `examples/upgrade-macos.sh`, which signs for you (below).

## Run it next to Paseo

Burrow's defaults are made for this: port 6797 (Paseo has 6767) and its own state folder,
`~/.burrow` (or `$BURROW_HOME`, or `--home`). Off Paseo's port it never writes `~/.paseo`. The
host then shows twice in clients, once per daemon; agents started on one are not on the other.
Paseo's CLI without `--host` reaches Paseo.

## Paseo's daemon lock

Paseo's CLI (0.13 on) finds the daemon on its host through `$PASEO_HOME/paseo.pid` (default
`~/.paseo/paseo.pid`): any command without `--host` reads its address there, and with no lock says
"Daemon is not running". A Burrow that has replaced Paseo, listening on Paseo's port (its
`config.json`'s `daemon.listen`, else 6767), holds that lock while it runs, in Paseo's format, so
`paseo ls` in a shell, a cron job's `paseo inspect` and `paseo daemon start` find Burrow. It is
removed when Burrow stops. Burrow never takes a lock a running Paseo daemon holds, and says so in
its log. `--no-paseo-lock` (or `BURROW_NO_PASEO_LOCK=1`) turns it off.

With the lock, `paseo daemon stop` stops Burrow, as it would stop Paseo.

Burrow's agents get `PASEO_HOST` with its address, so their `paseo` commands reach it either way.

## Replace Paseo on a host

Burrow takes over Paseo's port and identity, so every client sees the same host as before, and
`import-paseo` brings over its agents (Claude, Codex, Cursor, pi and Antigravity), projects,
workspaces and labels; each agent's next prompt resumes its session. OpenCode and custom ACP
agents are not imported (see [README.md](README.md#move-a-host-over-from-paseo) for exactly
what comes over).

1. **Stop Burrow** if it already runs on this host (`systemctl --user stop burrow`): the
   import needs its state folder to itself.
2. **Keep the host's identity.** Clients know a host by its server id. A Burrow that has never
   run here has none, and the import gives it Paseo's (it says `server id …, Paseo's`). If
   Burrow ran next to Paseo before, it has its own: copy Paseo's over it first,

   ```sh
   cp ~/.paseo/server-id ~/.burrow/server-id
   ```

   and remove the entry clients made for it on 6797, which they now see as a new host.
3. **Stop Paseo's daemon:** `paseo daemon stop`. If something else starts it again, such as
   Paseo's desktop app or a service of its own, stop that too, or it takes 6767 back. On macOS
   the desktop app runs the daemon and restarts it: quit the app
   (`osascript -e 'tell application "Paseo" to quit'`) and check that nothing holds the port
   (`lsof -iTCP:6767 -sTCP:LISTEN`). If Paseo is in System Settings → General → Login Items,
   remove it there, or it takes 6767 back at the next login.
4. **Import, once.** See what would come over, then do it:

   ```sh
   ~/burrow/burrow import-paseo --dry-run
   ~/burrow/burrow import-paseo
   ```

   It only reads `~/.paseo`. A second run adds nothing. Run it from a login shell (over SSH,
   `bash -lc '~/burrow/burrow import-paseo'`): it reads Codex threads through `codex`, and
   without the profile's PATH every Codex agent's history waits for its first prompt.
5. **Start Burrow on 6767.** In the unit, change `--listen 0.0.0.0:6797` to
   `--listen 0.0.0.0:6767`, then:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now burrow
   ```

   On macOS, change `127.0.0.1:6797` to `127.0.0.1:6767` in the launchd agent and load it as in
   [macOS (launchd)](#macos-launchd).

   On a host booted without systemd (SysV `init`), use an `/etc/init.d/burrow` script that
   starts it with `start-stop-daemon --start --background --make-pidfile --chuid <user>`
   through `bash -lc 'exec ~/burrow/burrow --listen … >>~/.burrow/burrow.log 2>&1'`, enable it
   with `update-rc.d burrow defaults`, and turn off Paseo's own script, if it has one, with
   `update-rc.d paseo disable`.

Clients reconnect to the same address and identity and find the imported agents, which resume
their Claude sessions on their next prompt.

### Going back to Paseo

```sh
systemctl --user disable --now burrow
paseo daemon start
```

On macOS:

```sh
launchctl bootout gui/$(id -u)/dev.crusteo.burrow
rm ~/Library/LaunchAgents/dev.crusteo.burrow.plist
open -a Paseo
```

Paseo's home was never written, so its agents are as they were before the switch. What was done
in Burrow since stays in `~/.burrow`.

## Upgrade

Stop it, keep the old binary as `burrow.prev`, put the new one in place, and start it. The
state folder carries over. To be able to roll back past a change in what Burrow saves, copy
the state folder too while it is stopped.

```sh
tar -xzf "burrow-$version-$target.tar.gz"
systemctl --user stop burrow
cp -a ~/.burrow ~/.burrow.prev               # optional
mv ~/burrow/burrow ~/burrow/burrow.prev
install -m 0755 "burrow-$version-$target/burrow" ~/burrow/
systemctl --user start burrow
~/burrow/burrow --version
```

### Upgrade on macOS

`examples/upgrade-macos.sh` does the upgrade from anywhere, an SSH session or an agent that
Burrow itself runs included, whose turn a direct restart would end halfway:

```sh
mkdir -p ~/.config/burrow
echo "<certificate name or SHA-1>" > ~/.config/burrow/sign-identity    # once
bash examples/upgrade-macos.sh "burrow-$version-macos-universal/burrow"
tail -f ~/burrow/upgrade.log
```

It hands the work to a one-off launchd job in your login session and returns. The job waits until
no agent is mid-turn (up to 30 minutes), stops Burrow, keeps the old binary as `burrow.prev`,
installs and signs the new one, starts Burrow and checks it listens, or puts `burrow.prev` back if
it does not. It signs from the login session because `codesign` over SSH usually cannot open the
keychain. `BURROW_SIGN_IDENTITY` overrides the file, and `BURROW_DIR` and `BURROW_HOME` the
binary's and the state folder.

## Roll back

```sh
systemctl --user stop burrow
mv ~/burrow/burrow.prev ~/burrow/burrow
# only if the state from before the upgrade should come back too:
# mv ~/.burrow ~/.burrow.new && mv ~/.burrow.prev ~/.burrow
systemctl --user start burrow
```

## Upgrading from carapace

Burrow was called carapace before 0.1.0-beta.1. The binary, the service and the state folder have
new names; the state moves by itself.

- **State.** Started with no `--home` and no `BURROW_HOME`, Burrow uses `~/.burrow`. When that does
  not exist and `~/.carapace` does, it first renames `~/.carapace` to `~/.burrow` and logs
  `moved the state directory from its carapace name`. Everything inside moves with it: `server-id`,
  so clients see the same host, the records and timelines, prompt images, and `worktrees/`. Paths
  into the old folder in the records and timelines are rewritten, and each worktree is reconnected
  to its repository (`git worktree repair`). If both folders exist, Burrow uses `~/.burrow`, leaves
  `~/.carapace` alone, and warns. A home given with `--home` or `BURROW_HOME` is used as it is, and
  nothing moves; so drop carapace's `--home ~/.carapace` from your unit (the examples pass none).
- **Environment.** `BURROW_HOME`, `BURROW_LISTEN`, `BURROW_LOG`, `BURROW_WORKTREES_ROOT` and the
  `BURROW_*_TRACE` variables replace their `CARAPACE_*` names. For one release, a `BURROW_*`
  variable that is not set falls back to its `CARAPACE_*` name, and the log says it is deprecated.
- **Service.** The unit is `burrow.service`, and `burrow-environment.service` reads
  `~/.config/burrow/env` instead of `~/.config/carapace/env`.

```sh
tar -xzf "burrow-$version-$target.tar.gz"
systemctl --user disable --now carapace
cp -a ~/.carapace ~/.carapace.prev           # optional: a copy carapace can go back to
mkdir -p ~/burrow
install -m 0755 "burrow-$version-$target/burrow" ~/burrow/
# The new unit; carry over your --listen and any other changes from carapace.service.
cp "burrow-$version-$target/examples/burrow.service" ~/.config/systemd/user/burrow.service
# Only for the environment variant: mv ~/.config/carapace ~/.config/burrow
rm ~/.config/systemd/user/carapace.service
systemctl --user daemon-reload
systemctl --user enable --now burrow
journalctl --user -u burrow | grep 'state directory'
```

To go back to carapace, run its binary on the moved folder, whose paths now name `~/.burrow`:
put back `carapace.service` with `--home %h/.burrow` in place of `--home %h/.carapace`, then
`systemctl --user disable --now burrow`, `systemctl --user daemon-reload` and
`systemctl --user enable --now carapace`. To return to the state from before the upgrade
instead, stop both and `mv ~/.carapace.prev ~/.carapace` (with `--home %h/.carapace`); what was
done in Burrow since is then not there.

## Build from source

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked -p burrow-daemon --target x86_64-unknown-linux-musl
# target/x86_64-unknown-linux-musl/release/burrow
```

The static build needs no C toolchain beyond the system `cc`; `musl-tools` (Debian/Ubuntu)
provides `musl-gcc` should a dependency ever need it. A plain
`cargo build --release -p burrow-daemon` works on Linux, macOS and Windows too. Builds from a
checkout report its commit; set `CRUSTEO_COMMIT` when building from a source archive.
