# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/chicagobuss/crusteo/compare/v0.1.0-alpha.1...HEAD
[0.1.0-alpha.1]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.1
