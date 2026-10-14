# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/chicagobuss/crusteo/compare/v0.1.0-alpha.4...HEAD
[0.1.0-alpha.4]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.4
[0.1.0-alpha.3]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.3
[0.1.0-alpha.2]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.2
[0.1.0-alpha.1]: https://github.com/chicagobuss/crusteo/releases/tag/v0.1.0-alpha.1
