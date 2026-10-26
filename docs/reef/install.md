# Installing Reef

Reef (`reef`) is crusteo without the window. It runs on one always-on machine that can reach
every daemon (Paseo's or Burrow), keeps one connection to each, and serves phones Skiff, a
single-file page, and a compact protocol on port 6798. Phones only ever talk to Reef. How it
works, and its security model, are in [design.md](design.md). Reef was called crusteo-hub before
0.1.0-beta.1; see [Upgrading from crusteo-hub](#upgrading-from-crusteo-hub).

This guide uses `user` for the account, `192.0.2.10` for a daemon's address and `reef.example`
for Reef's name; use your own.

## Download

Each [GitHub release](https://github.com/chicagobuss/crusteo/releases) has an archive per
platform, and `SHA256SUMS.txt` for all of them:

| Platform | Archive |
|---|---|
| Linux x86_64 (static, any distribution) | `reef-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 (static, any distribution) | `reef-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS (Apple Silicon and Intel) | `reef-<version>-macos-universal.tar.gz` |
| Windows x86_64 | `reef-<version>-x86_64-pc-windows-msvc.zip` |

Each holds the `reef` binary, this guide as `INSTALL.md`, the license, the changelog, and
`examples/`: a systemd unit and drop-in, a launchd agent, and example `hosts.json` and
`reef-settings.json` (in the source tree they are in `packaging/reef/`).

```sh
version=0.1.0-beta.1
target=x86_64-unknown-linux-musl
base=https://github.com/chicagobuss/crusteo/releases/download/v$version
curl -LO "$base/reef-$version-$target.tar.gz"
curl -LO "$base/SHA256SUMS.txt"
sha256sum --check --ignore-missing SHA256SUMS.txt
tar -xzf "reef-$version-$target.tar.gz"
```

On macOS use `shasum -a 256 --check --ignore-missing SHA256SUMS.txt`. The binary is not signed or
notarized; one downloaded with a browser needs `xattr -d com.apple.quarantine reef` before
it runs.

## Install

The examples keep everything in one folder, `~/reef`:

```sh
mkdir -p ~/reef
install -m 0755 "reef-$version-$target/reef" ~/reef/
cp "reef-$version-$target/examples/hosts.json" ~/reef/
~/reef/reef --version      # reef 0.1.0-beta.1 (<commit>)
```

### hosts.json

The daemons to connect to, in crusteo's own `hosts.json` format, so the desktop app's file works
as it is:

```json
[
  {"name": "buildbox", "endpoint": "192.0.2.10:6767"},
  {"name": "workstation", "endpoint": "192.0.2.11:6797"}
]
```

- `endpoint` is `host:port`: 6767 for Paseo's daemon, 6797 for a Burrow next to it.
- `password` is optional, for daemons that require one.
- Daemons check the `Host` header, so use IP addresses or names the daemon allows.
- Hosts added or removed from a phone are saved back to this file.
- Names must be unique, without a `/`, and at most 64 characters; Reef stops at start, naming the
  entry, if one is not. Reef knows hosts by name, so it writes each host's `id` as its name; an `id`
  the desktop app gave a host is replaced.

### The data folder

Reef keeps its own files in a `reef-data` folder beside `hosts.json` (`--data-dir` puts them
elsewhere). It makes them on first start:

| File | Holds |
|---|---|
| `reef-client-id.json` | The client id Reef gives every daemon. Never shared with the desktop app. |
| `reef-tokens.json` | Device tokens, `[{"name": "phone", "token": "<hex>"}]`. Add a device by adding an entry with a long random token; revoke one by removing it, then restart Reef. |
| `reef-settings.json` | Reef-wide settings, which phones change from the page: turn-finished notifications on by default (`notify_default`), each agent's own choice by `host/agent` (`notify`), and whether lists show Paseo 0.12's background workspaces (`show_background`). `examples/reef-settings.json` shows the format; there is no need to write it by hand. |

## Try it

```sh
cd ~/reef
./reef hosts.json
```

On first start it prints a device token, then `reef <version> (<commit>) listening on
http://127.0.0.1:6798`. Open `http://127.0.0.1:6798/#token=<token>` once; the page keeps the
token. `./reef --demo` serves a made-up fleet instead, contacting no daemon.

| Option | Default | |
|---|---|---|
| `--bind <addr>` | `127.0.0.1` | `0.0.0.0` serves every network the machine is on, over plain HTTP. |
| `--port <n>` | `6798` | |
| `--data-dir <dir>` | `reef-data` beside the hosts file | |
| `--token <token>` | | Accept this one token instead of the tokens file. |
| `--open` | off | No device tokens: anyone who can reach Reef can act on every agent. Only where reaching Reef at all is the lock. |
| `--host-name <name>` | | A DNS name Reef is reached by (repeatable). Requests with any other `Host` than an IP address, `localhost` or these get a 403. |
| `--usage <url>` | | Also show account usage from a usage feed (an `http://` base URL serving `/v1/usage`). |
| `--demo` | | A made-up fleet from fake daemons in the process. |
| `-V`, `--version` | | Print the version and commit. |

### Reaching it from a phone

- **On a tailnet,** keep Reef on loopback and let `tailscale serve` give it HTTPS, which
  system notifications need:

  ```sh
  ./reef hosts.json --host-name reef.example
  tailscale serve --bg --https=443 http://127.0.0.1:6798
  ```

  `--host-name` must be the name `tailscale serve` forwards (`<machine>.<tailnet>.ts.net`).
- **On a private network,** `--bind 0.0.0.0` and open `http://192.0.2.20:6798/#token=<token>`
  (Reef's address). Everything but system notifications works over plain HTTP.

`curl http://127.0.0.1:6798/healthz` answers `ok` while it runs.

## Run it as a service

### Linux (systemd)

`examples/reef.service` is a system unit that runs Reef as `user` from
`/home/user/reef`. Edit `User=` and the paths, then:

```sh
sudo cp examples/reef.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now reef
journalctl -u reef -f      # the first device token is printed here
```

It binds `0.0.0.0` and asks phones for a token; its comments say how to keep it on loopback or
use `--open`. To add options without editing the unit, use a drop-in that sets `REEF_ARGS`, as
`examples/usage.conf` does for a usage feed:

```sh
sudo mkdir -p /etc/systemd/system/reef.service.d
sudo cp examples/usage.conf /etc/systemd/system/reef.service.d/
sudo systemctl daemon-reload && sudo systemctl restart reef
```

### macOS (launchd)

`examples/dev.crusteo.reef.plist` is a launchd agent that runs Reef from
`/Users/user/reef` at login and logs to `reef.log` there. Edit the paths, then:

```sh
cp examples/dev.crusteo.reef.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.crusteo.reef.plist
launchctl bootout gui/$(id -u)/dev.crusteo.reef      # to stop it
```

The agent is untested so far. If macOS asks whether Reef may find devices on the local
network, allow it, or it cannot reach daemons on private addresses.

### Windows

Run `reef.exe hosts.json` in a terminal. To keep it running, create a Task Scheduler task
that starts it at log on with the folder as its start directory. This is untested so far.

## Upgrade

Stop it, keep the old binary as `reef.prev`, put the new one in place, and start it. The
data folder and `hosts.json` stay as they are.

```sh
tar -xzf "reef-$version-$target.tar.gz"
sudo systemctl stop reef
mv ~/reef/reef ~/reef/reef.prev
install -m 0755 "reef-$version-$target/reef" ~/reef/
sudo systemctl start reef
~/reef/reef --version
```

A phone with the page open reconnects by itself; reload the page there to get the new one.

## Roll back

```sh
sudo systemctl stop reef
mv ~/reef/reef.prev ~/reef/reef
sudo systemctl start reef
```

## Upgrading from crusteo-hub

Reef was called crusteo-hub before 0.1.0-beta.1, and Skiff was its page. The binary, the service
and the data folder have new names; the data moves by itself.

- **Data.** With no `--data-dir`, Reef's data folder is `reef-data` beside `hosts.json`. When that
  does not exist and a `hub` folder does, Reef renames `hub` to `reef-data` at start. In the data folder,
  `hub-client-id.json`, `hub-tokens.json` and `hub-settings.json` become `reef-client-id.json`,
  `reef-tokens.json` and `reef-settings.json`, unless the new file is already there. Daemons see
  the same client id, and phones keep their device tokens. A folder given with `--data-dir` stays
  where it is; only its files are renamed. Each move is printed (`reef: moved …`,
  `reef: renamed …`), and one that fails stops Reef rather than let it make new tokens.
- **Skiff.** The first time a phone loads the new page it moves its saved token and choices from
  the old page's storage, so it stays signed in. Reload the page on each phone after the upgrade.
- **Environment.** `REEF_PAGE` replaces `CRUSTEO_HUB_PAGE`, which still works for one release
  and prints that it is deprecated. In the systemd unit and its drop-ins, `HUB_ARGS` is now
  `REEF_ARGS`.
- **Service.** The unit is `reef.service`; on macOS the launchd label is `dev.crusteo.reef`.

```sh
tar -xzf "reef-$version-$target.tar.gz"
sudo systemctl disable --now crusteo-hub
mv ~/crusteo-hub ~/reef                     # hosts.json and the hub folder come along
install -m 0755 "reef-$version-$target/reef" ~/reef/
# The new unit; carry over User=, --bind and any other changes from crusteo-hub.service.
sudo cp "reef-$version-$target/examples/reef.service" /etc/systemd/system/
# A drop-in such as usage.conf moves to reef.service.d, with HUB_ARGS renamed:
sudo mkdir -p /etc/systemd/system/reef.service.d
sed 's/HUB_ARGS/REEF_ARGS/' /etc/systemd/system/crusteo-hub.service.d/usage.conf \
  | sudo tee /etc/systemd/system/reef.service.d/usage.conf
sudo rm -r /etc/systemd/system/crusteo-hub.service /etc/systemd/system/crusteo-hub.service.d
sudo systemctl daemon-reload
sudo systemctl enable --now reef
journalctl -u reef | grep -E 'reef: (moved|renamed)'
```

To go back to crusteo-hub, stop Reef and give the old binary its old names back, then start
`crusteo-hub` again with its own unit:

```sh
sudo systemctl disable --now reef
cd ~/reef && mv reef hub
for f in hub/reef-*.json; do mv "$f" "hub/hub-${f#hub/reef-}"; done
```

The old page reads its own storage, which Skiff emptied, so each phone signs in once more with
its token from `hub-tokens.json`; the tokens themselves are unchanged.

## Build from source

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked -p reef --target x86_64-unknown-linux-musl
# target/x86_64-unknown-linux-musl/release/reef
```

The static build needs no C toolchain beyond the system `cc`; `musl-tools` (Debian/Ubuntu)
provides `musl-gcc` should a dependency ever need it. A plain `cargo build --release -p reef`
works on Linux, macOS and Windows too. Builds from a checkout report its commit; set
`CRUSTEO_COMMIT` when building from a source archive.
