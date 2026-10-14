# Zed Code

Zed Code layers VS Code-like UX onto [Zed](https://github.com/zed-industries/zed) without changing its architecture, crate layout, or core behavior. It brings the missing pieces of a conventional editor — dock panels for search and references, a git panel you can rearrange by dragging, smooth scrolling, and panels that dock where you want them — and extends the agent so its file tools reach the whole filesystem while still stopping at your own permission rules.

Everything here is additive. Where a change extends something upstream already ships, the upstream part stays untouched and only the behavior described here is new. When upstream ships its own version of the same problem, that version wins and this branch's version is dropped.

Releases are listed newest first. Each one corresponds to a working session, so the dates are when the work landed rather than a published schedule.

## Zed Code 2026.10.12

Oct 12, 2026 · based on Zed v1.22.x

The recommended setup moves to DeepSeek v4.1, the agent panel gets the editor's scrollbar and tidier queued-message rows, a code action applied by the agent now reaches disk, and the editor's vertical scrollbar is twice as wide.

## Features

### Agent

- Moved the default model to `go/deepseek-v4.1-flash` (thinking on, effort `low`); the `Write` and `Ask` profiles no longer pin their own model, so both inherit the default, and summary/compaction fall back to it as well. Favorites are now `go/deepseek-v4.1-flash`, `go/muse-spark-1.3-contributor` and `go/mimo-v2.6-flash`.
- Added a loading state to the model selector: while a provider is still fetching its model list the selector shows **Loading Models…** and is disabled, instead of showing an empty list. `LanguageModelProvider` gained an `is_loading_models` hook, overridden by opencode while its fetch task is in flight.
- Gave the agent panel's conversation list the editor scrollbar style (a full-track thumb with square corners, no padding), so it reads as one surface with the editor.

## Zed Code 2026.10.11

Oct 11, 2026 · based on Zed v1.22.x

The agent's search tools can be pointed at any directory, the completion menu shows a completion's detail in its own column, the diagnostics tool stops calling a file it never checked clean, and a cancelled permission prompt no longer wedges the conversation.

## Features

### Agent tools

- Added a `path` search root to `grep` and `find_path`. Point it at a directory — inside the project, or any absolute or `~`-prefixed path outside it — and exactly that root is searched, gitignore included, so a named `node_modules` or build directory is reachable with no flag. Without `path` both delegate to the project-wide search unchanged.
- Improved `list_directory` and the symbol tools (`go_to_definition`, `find_references`, `rename`, code actions) to resolve a path below an unscanned gitignored directory the way the bare relative form already did.

### Agent

- Reworked `diagnostics` to tell "checked and clean" apart from "never checked", "no language server" and "may be stale", and to accept several files in one call so every edited file is checked together.

### Agent guide

- Improved the guide to send error, warning and lint checks to the `diagnostics` tool instead of the terminal, and to describe the `grep` / `find_path` search root.

### Editor

- Moved a completion's `detail` (or `label_details.description`) into the menu's own detail column, instead of appending it to the label. A server like TypeScript's only returns `detail` on resolve, so appending it used to rewrite the label on resolve; the label now stays the label.
- Improved the completion shortcut to toggle the documentation when the menu is already open, instead of reopening it.
- Added a `Loading…` placeholder to the completions menu, shown from the moment completions are requested until the results arrive.
- Persisted the `completions.suggest_selection` memory to the database, so the preselected completion survives a restart.

## Bug Fixes

- Fixed `list_directory` reporting `Path not found`, and the symbol tools reporting a file as missing, for a rooted or absolute path below an unscanned gitignored directory.
- Fixed `diagnostics` reporting a file that no language server had looked at as clean, and never registering a freshly opened file with the servers.
- Fixed a cancelled permission prompt wedging the conversation: every Allow / Deny button and its keyboard shortcut stopped working until restart.
- Fixed trackpad scrolling in the editor sometimes doing nothing until you clicked in the editor: the axis lock zeroed the vertical delta whenever the first event of a two-finger gesture was horizontal-dominant, and for an editor that cannot scroll horizontally the surviving horizontal delta was clamped away. The editor no longer applies the axis lock when it cannot scroll horizontally.
- Fixed the wheel being swallowed over a git-gutter diff-hunk marker and over the gap between stacked hover popovers, which were hitboxes in front of the editor that blocked scroll.

## Breaking Changes and Notices

- Changed the `diagnostics` tool input from `path` (one string) to `paths` (an array). All files the agent changed can now be checked in a single call.

## Zed Code 2026.10.04

Oct 4, 2026 · based on Zed v1.22.x

Scrollbars stop being a single global switch and become something each surface configures for itself, the Git Panel learns to open entries as preview tabs, and every diff view — Git and agent — now shares one Keep/Reject pill.

## Features

### Settings

- Removed the global `scrollbar.track` setting in favor of a per-surface one.
  - `show` still inherits `scrollbar.show` when it is unset.
  - `track` is no longer inherited and defaults to `"track"` on each surface: `search.scrollbar.track`, `references_panel.scrollbar.track`, `git_panel.scrollbar.track`, `project_panel.scrollbar.track`, and the new `context_menu.scrollbar.track`.
- Added the `context_menu` settings section, covering scrollbar behavior in context menus and dropdowns.
- Added the new scrollbar settings to the Settings window, along with sections for the references panel and context menus.
- Added `status_bar.icon_scale` and `tabs.show_full_tab_titles` to the Settings window.

### Git

- Added preview tab support for Git Panel entries. A single click opens the diff in a preview tab that the next diff replaces, while a double click opens it in a persistent tab.
- Improved the Git Panel to follow the file focused in the editor with the list's own selection.

### Diff views

- Added a single diff hunk action pill shared by every diff renderer, so stage/unstage/restore and keep/reject look and behave the same in the Git views and the agent's review view.
  - The pill's buttons are full height and its vertical padding is symmetric, so it sits as one line above the hunk instead of covering its first row.

### Agent

- Improved queued message controls to a two-row grid: steer and delete above, Send Now and edit below.

## Bug Fixes

- Fixed search and references results being clipped instead of scrolling horizontally when a match line was wider than the first row.
- Fixed a file written outside the project being treated as project content afterwards, which made every later operation on it fail as "not in the project".
- Fixed Keep and Reject being dropped when the action started on the left side of a split diff.
- Fixed the agent diff view showing hunks and diff stats from the moment it opened rather than the current state of the thread.

## Breaking Changes and Notices

- Removed the global `scrollbar.track` setting. Each surface now has its own `scrollbar.track`, defaulting to `"track"`; remove the old key from your `settings.json`.
- Changed `search.dock` and `references_panel.dock` to accept only `left` and `right`. Both panels previously allowed `bottom` through the schema but could not dock there.
- Changed the Git Panel's focused-file indication from a dedicated highlight to the list's own selection.

## Zed Code 2026.09.29

Sep 29, 2026 · based on Zed v1.22.x

A polish release across the surfaces: tooltips stop swallowing the scroll wheel, tabs and diagnostic popovers stop overlapping their own content, and the agent's diff view gains a file path and a diff stat.

## Features

### Editor

- Improved context menu scrollbars to follow `scrollbar.track` as well as `scrollbar.show`, so the scrollbar floats over the menu when a thumb is configured.
- Improved the recently-used completion memory to survive a language server resolving a completion, which can rewrite both its label and its insert text.
- Improved tabs so the close button's icon keeps an equal inset from the tab edge as the file icon does from the other side.
- Improved search and references result lists to be measured by their widest match row, so long lines scroll horizontally instead of being clipped.

### Git

- Improved commit and blame tooltips to pass the scroll wheel through to the editor, capturing it only while the message overflows.
- Improved the Git Panel so clicking a checkbox toggles it without also selecting the row or starting a file drag.

### Agent

- Added a style toolbar and breadcrumbs to the single-file agent diff view, so it shows the file path, the Show Full File and split/unified controls, and a `+`/`-` diff stat.

### Settings

- Updated the starter settings to use `agent.threads_sidebar.position` instead of the removed `agent.sidebar_side`.
- Changed the Settings window to open maximized on Windows, where the default size left the setting descriptions wrapping aggressively.

## Bug Fixes

- Fixed a physical mouse wheel scrolling horizontally a different distance than vertically, because line deltas were converted with the glyph width on one axis and the line height on the other.
- Fixed clicking a Git Panel checkbox also selecting the row and starting a file drag.
- Fixed the tab close button sitting closer to the tab edge than the file icon did to the other side.
- Fixed the preselected completion staying at the very bottom edge of the menu instead of being visible when the menu opened.
- Fixed diagnostic popover action buttons overlapping the message on long diagnostics.
- Fixed the recently-used completion being forgotten when the language server resolved it.
- Fixed a file written outside the project being treated as project content afterwards.

## Breaking Changes and Notices

- Changed the starter settings to `agent.threads_sidebar.position`, replacing `agent.sidebar_side`. Existing settings are migrated by Zed Code's settings migrations.
- Changed the Settings window to open maximized on Windows.

## Zed Code 2026.09.28

Sep 27–28, 2026 · based on Zed v1.22.x

The agent's file tools stop being confined to the project and stop being blinded by gitignore. Its guide becomes overridable at runtime, and the first-run configuration is written for you.

## Features

### Agent tools

- Added whole-filesystem support to the file tools. `read_file`, `write_file`, `edit_file`, `delete_path`, `move_path`, `copy_path`, `create_directory` and `list_directory` accept absolute and `~`-prefixed paths, not just project paths and `~/.agents/skills`.
  - Access is still gated by your `tool_permissions` rules, and destructive operations refuse the filesystem root, your home directory, and ancestors of an open worktree.
  - `file_scan_exclusions` and `private_files` became hard blocks in every file tool, checked before any approval prompt, so a path the agent may not read it also may not modify.
  - Deleting or moving a symlink acts on the link rather than its target, the way `rm` and `mv` do; `copy_path` dereferences, like `cp`.
- Added matching of `tool_permissions` rules against every textual form of a path: as written, `~`-expanded, worktree-relative, and absolute. A rule can now target any project (`^src/`) or exactly one (`^/Users/me/myproject/src/`), whichever form the model used.
- Stopped gitignore from hiding files from the tools. `grep`, `find_path` and `list_directory` scan ignored directories on demand instead of treating them as absent.
  - For the two search tools the scan is opt-in: an ignored directory is only scanned when the pattern's literal prefix names one from the project root, so an ordinary `**` glob never pulls in `node_modules`.

### Agent guide

- Added a runtime override for the agent guide. Drop a `system_prompt.hbs` into `~/.config/zed/prompt_overrides` and the next message picks it up, with no rebuild and no restart; a file that fails to render falls back to the built-in guide.
- Improved the guide to describe the new path forms, added a Web Search section explaining when to reach for it, and rewrote the multi-agent delegation guidance around concrete triggers and parallel spawns.

### Agent interface

- Added previous and next hunk buttons to the agent diff view toolbar.
- Simplified the agent diff view so it no longer duplicates the split/unified and fold controls that the search bar already renders.
- Added text previews for queued messages, so a queued entry reads as two lines with a full-text tooltip instead of a live editor.

### Search and panels

- Added the `search.dock` setting (default `"left"`) and made the references panel dockable, with `references_panel.dock`, `references_panel.button` and `references_panel.default_width` (default `320`).
- Improved the references panel to select and center the match the query was started from.
- Improved project search to seed its query from the current selection, the word under the cursor, or the buffer search bar, and run it.
- Improved the status strips to hide themselves when they have nothing to show, and moved the workspace sidebar toggle out of the sidebar into them.

### Editor and tabs

- Improved scroll routing so a mostly-vertical trackpad gesture scrolls horizontally overflowing elements such as the tab bar.
- Improved tabs to show the unsaved-changes indicator in the place of the close button, swapping to close (or unpin) on hover without shifting the layout.

### Git

- Added highlighting for the file focused in the editor in the Git Panel's changes list.

## Recommended starting setup

- Zed Code now creates `~/.config/zed/settings.json`, `keymap.json` and `AGENTS.md` from the bundled defaults when they are missing. Existing files are never overwritten.

## Building Zed Code

- Added support for a pre-fetched WebRTC archive. The LiveKit build script downloads a ~240 MB archive on a clean build and times out easily; `script/bundle-fast-mac` now uses `.webrtc/<tag>/` when its tag matches the one the build expects, and ignores a stale copy rather than silently reusing it.

## Bug Fixes

- Fixed the Git Panel waiting for the file watcher to notice an index write before showing a staged change.

## Breaking Changes and Notices

- Changed `tool_permissions` rules so they match every textual form of a path. A rule written as `^src/` now matches the same file however the model happens to spell it, which makes such a rule apply across projects. Write rules with the path form in mind.
- Zed Code writes its starter configuration into Zed's own configuration directory, so an official Zed installed on the same machine reads the same `settings.json`, `keymap.json` and `AGENTS.md`. Changing `APP_NAME` in `crates/paths/src/paths.rs` separates them, and moves an existing user's settings as well.

## Zed Code 2026.09.26

Sep 26, 2026 · based on Zed v1.20.x, and merging upstream Zed v1.22.x

Completion menus learn what you actually accept, project search gets a real panel with a context menu, and the agent gains a single-file review view. This release also merges upstream Zed v1.22.x.

## Features

### Editor and completions

- Added a syntax-colored symbol icon per completion entry with `completion_menu_item_kind: "icon"`.
- Added the `completions.suggest_selection` setting (default `"first"`). `"recently_used"` preselects the most recently accepted completion among the top-scoring entries; `"recently_used_by_prefix"` remembers the completion accepted for a prefix and preselects it again when that prefix is typed.
- Added the `scrollbar.track` setting (default `"track"`). `"track"` reserves space for a scrollbar track next to the content; `"thumb"` floats the scrollbar over it.
- Added a scrollbar to context menus whose entries overflow, following the `scrollbar.show` setting. This covers every context menu, including submenus and dropdowns.
- Improved smooth scrolling to step between buffer and project search results.
- Improved `project_panel.title_tooltip_delay` so it also applies to editor tabs and Git Panel entries.

### Search

- Added a search dock panel. Deploy Search opens the panel instead of a project search multibuffer.
- Improved search result rows to use the threads panel's styling and to carry the project panel's context menu.

### Git

- Added context menus for Git Panel directories, covering staging, unstaging, and discarding a folder.
- Added the ability to stage individual hunks from a solo diff.

### Agent

- Added selections and diagnostics as thread context. A diagnostic can be added to a thread, not just an editor or terminal selection, so a compiler or linter error reaches the agent without being copied by hand.
- Added a single-file review view. Clicking a changed file in a thread's edits block opens that file on its own, with per-file Keep and Reject.
- Added five built-in agent skills alongside `create-skill`: `strict-typescript`, `strict-angular`, `strict-react`, `strict-compose-plan`, and `strict-implement-plan`.
- Bundled a default `AGENTS.md` and created it on demand when opening global rules, mirroring how the settings files are seeded.

### Tasks

- Added per-OS task template overrides. `osx`, `linux`, and `windows` can each replace the command, args, cwd, and env of a task.
- Added the `$ZED_ACTIVE_WORKTREE_ROOT` variable, which follows the worktree selected in the title bar or the Git Panel's repository selector rather than the file being edited.
- Added a default "Open External Terminal" task that opens the system terminal in the selected folder (`open -a Terminal` on macOS, `start cmd /k` on Windows). It adds its tab to the terminal dock without focusing it and closes the tab when the command finishes.

### Web search

- Added TinyFish as a selectable web search provider. It is independent of the language model provider, so `search_web` works with any model.
- Improved the `search_web` tool with richer parameters: purpose, location, language, domain include and exclude filters, `web` or `news` as the domain type, date bounds, a recency window, and pagination.

## Bug Fixes

- Fixed the expand/collapse-all button's arrow icons pointing the wrong way.
- Fixed a duplicate `TerminalSettings` import in the Agent Panel after the v1.22.x merge.

## Zed Code 2026.09.21

Sep 19–21, 2026 · based on Zed v1.20.x

The first Zed Code release: the panels you toggle from the status bar get their own place in the layout, the Git Panel can be rearranged by dragging, and the editor scrolls smoothly.

## Features

### Panels and docks

- Added a dock panel for Find All References. Results open in a panel instead of a tab, so you keep your place in the file you are reading.
  - The panel selects and centers the reference the query was started from.
- Moved project search into a dock panel.
- Added a "Project" header above the project panel tree with an expand/collapse-all button.
- Added vertical status strips on the left and right edges of the workspace that host the panel toggles.
  - Added the `status_bar.icon_scale` setting (default `1.0`) to scale the strip icons; the strips grow with them.

### Editor and tabs

- Improved scrolling to animate along a composed easing curve, one frame at a time, instead of stepping.
- Added go-to-definition inside a diff to stay in that view, rather than opening a different editor when the target falls outside a solo diff's excerpts.
- Added the `tabs.show_full_tab_titles` setting (default `false`) to expand a tab to the full file name instead of truncating it.

### Git

- Added drag and drop for Git Panel tree directories and status entries; a marked selection moves together.
- Added a hover "View File" action on status rows.
- Added diffing of solo diffs against the base their section implies — staged content against `HEAD`, unstaged against the index — with a separate view per base.
- Added opening solo diffs in preview tabs that replace one another.
- Added the `git_panel.message_editor_min_lines` setting (default `6`) for the minimum height of the commit message editor.

## Recommended starting setup

- Added a recommended starting configuration, bundled with Zed Code and seeded on first run.
  - The keymap starts from `base_keymap: "VSCode"` and rebinds panels and editor navigation the way VS Code does — `secondary-e` for the workspace sidebar, `secondary-shift-b` for the right dock, `ctrl-left`/`ctrl-right` for line start and end, `cmd-left`/`cmd-right` for word movement — unbinding the Zed defaults it replaces.
  - The settings file starts from a dark Ayu Dark theme, 20/16 UI and buffer fonts, `"auto_update": false`, and a split diff style.

## Building Zed Code

- Added `script/bundle-fast-mac`, which builds `zed` and `cli` once, packages them without rebuilding, and ad-hoc signs the result. Upstream's `script/bundle-mac` also embeds a git binary and produces a DMG, but it builds three times.
- Enabled the `runtime_shaders` feature for `gpui_apple`, so Metal shaders compile at runtime instead of needing a full Xcode install. Rendering performance matches the official build; first startup is slightly slower. Revert `crates/gpui_macos/Cargo.toml` to go back to the canonical ahead-of-time build.
- Set the release channel to `dev`, so local builds use their own data directory and CLI socket instead of sharing them with an installed Zed, and disabled incremental compilation in the `dbg`, `release`, and `release-fast` profiles.
