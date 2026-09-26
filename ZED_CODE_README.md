# zed-code

`zed-code` is a personal working branch of [Zed](https://github.com/zed-industries/zed), maintained **additively**.

## What this project is

- **Additive, not a refactor.** The point is to bring good UX solutions from VS Code (and other editors) onto Zed's base architecture. Zed's architecture, crate layout and core behavior stay as upstream ships them; we layer the missing UX on top.
- **Smallest possible diff per feature.** Prefer a focused change that adds the UX behavior over restructuring existing code. Avoid drive-by refactors, renames and reorganization: they make merges from upstream painful and hide the actual feature.
- **Upstream stays the source of truth.** When upstream ships its own take on the same problem, prefer upstream's version and drop ours.
- **Some commits are local-only** and must not be pushed. They are marked in the subject with `(do not PR)` — personal settings, keymap, and local macOS build tooling.

## Working with the branch

- Branch: `zed-code`, based on upstream `main` and periodically merged with release branches (`origin/v1.21.x`, `origin/v1.22.x`, …).
- After merging upstream, check `crates/zed/RELEASE_CHANNEL`: merges can reset it (e.g. to `preview`). Keep it at `dev` for local builds — the dev channel uses its own data directory (`db/0-dev`) and its own CLI socket, while other channels share data and the socket with the installed Zed app and can migrate its databases.
- Local build tooling added on this branch: `script/bundle-fast-mac` (see below).

## Building on macOS without full Xcode

This checkout has local tweaks for building on macOS without a full Xcode install.

```sh
cargo build -p zed -j 6               # debug binary
cargo build -p zed --release -j 6     # release binary

CARGO_BUILD_JOBS=4 script/bundle-fast-mac   # fast .app bundle (one build, see script/bundle-fast-mac)
# release channel comes from crates/zed/RELEASE_CHANNEL (dev => "Zed Dev.app");
# the script logs the channel and warns when it is not `dev`.

# artifacts:
./target/debug/zed                                        # debug binary
./target/release/zed                                      # release binary
target/aarch64-apple-darwin/release/bundle/osx/           # Zed Dev.app (bundle)
```

Notes:

- **`script/bundle-fast-mac`** does one build (`zed` + `cli`), packages it with `CARGO_BUNDLE_SKIP_BUILD` (so cargo-bundle does not rebuild everything), ad-hoc signs the result and prints the channel. Upstream's `script/bundle-mac` additionally builds `remote_server`, embeds a git binary, copies the provisioning profile and creates a DMG — but it builds three times and takes ~1.5 h locally.
- **Slow final step? It is thin LTO, not the linker.** Every crate is built with `linker-plugin-lto` (`lto = "thin"` in `[profile.release]`), so most of the wait is LTO codegen inside rustc. For faster local iteration use `--profile release-fast` (`lto = false`, `codegen-units = 16`), at a small runtime cost. A faster linker (lld) does not help with thin LTO.
- **`-j 6`** is for a 16 GiB machine; more parallel rustc jobs push it into swap.
- Merging upstream branches can reset `crates/zed/RELEASE_CHANNEL` (e.g. to `preview`/`stable`) — keep it at `dev` for local builds (see above).

Local tweaks in this checkout:

- **Metal shaders are compiled at runtime** — the `runtime_shaders` feature is enabled in `crates/gpui_macos/Cargo.toml` (`gpui_apple = { workspace = true, features = ["runtime_shaders"] }`). Normally shaders are AOT-compiled during the build by `xcrun metal`, which only exists with a full Xcode install; with this feature the shader source is embedded and compiled by macOS on first launch. Cost: a slightly slower first startup. Rendering performance is the same as the official build. To restore the canonical AOT build, install full Xcode and revert that file: `git checkout crates/gpui_macos/Cargo.toml`.
- **Incremental compilation is disabled** — `incremental = false` is set explicitly in every profile (`dev`, `dbg`, `release`, `release-fast`) in the root `Cargo.toml`, and the per-crate `[profile.dev.package.*] incremental = true` overrides for `zed`/`editor`/`workspace`/`project` are removed. Reason: incremental artifacts for this workspace grow to several GB. Trade-off: rebuilding `zed`/`editor`/`workspace`/`project` during the dev loop is a full recompile instead of incremental.

## Settings added in this branch

Custom settings introduced or extended on the `zed-code` branch (defaults live in `assets/settings/default.json`; the branch's recommended starting values are seeded from `assets/settings/initial_user_settings.json`):

- `git_panel.message_editor_min_lines` (default `6`): minimum height, in lines, of the commit message editor in the git panel; the maximum height is twice this value.
- `tabs.show_full_tab_titles` (default `false`): expand tabs to fit the full file name instead of truncating it.
- `scrollbar.track` (default `"track"`): `"track"` reserves space for a scrollbar track next to the content, `"thumb"` floats the scrollbar over it.
- `completion_menu_item_kind` (default `"off"`): gains an `"icon"` value that shows a syntax-colored symbol icon per completion entry.
- `project_panel.title_tooltip_delay`: now also applies to the tooltips of editor tabs and git panel entries.

## Recommended initial settings, keymap and agent rules

Unlike "Settings added in this branch" above (options this branch adds to Zed itself), the files below are the branch's **recommended starting setup**. They are seeded on first run:

- `assets/settings/initial_user_settings.json` — seeded as the user settings;
- `assets/keymaps/initial.json` — seeded as the initial user keymap;
- `assets/settings/initial_agents_md.md` — seeded as the global `AGENTS.md` (agent rules).

A fresh Zed Code install therefore starts with a VS Code-like setup (`"base_keymap": "VSCode"`) and a ready-made agent rules file, while keeping Zed's architecture and defaults underneath.

### Editor and buffer

- `when_closing_with_no_tabs: "keep_window_open"` — closing the last tab keeps the window open.
- `double_click_in_multibuffer: "select"` — double-click in a multibuffer (search results, references) selects the word instead of opening the file.
- `excerpt_context_lines: 10`, `expand_excerpt_lines: 100` — more surrounding context in search/reference results.
- `vertical_scroll_margin: 1.0` — one line of margin above/below while scrolling, so the cursor is never glued to the edge.
- `show_edit_predictions: false` — inline AI edit predictions off.
- `format_on_save: "modifications_if_available"` — format only the modified ranges when the formatter supports it.
- `auto_signature_help: false`, `show_signature_help_after_edits: true` — no signature popup while typing, but it appears after edits.
- `completions: { words: "fallback", suggest_selection: "recently_used" }` — word completions only when the language server has nothing; the list pre-selects the most recently used item.
- `completion_menu_item_kind: "icon"` — branch feature: a syntax-colored symbol icon per completion entry.
- `hover_popover_delay` / `hover_popover_hiding_delay: 400` — hover popovers appear and hide after 400 ms.
- `show_whitespaces: "boundary"` — render whitespace only at word boundaries.
- `lsp_results_location: "picker"` — LSP results open in a quick-open style picker instead of a multibuffer.

### Tabs, panels and chrome

- `tabs`: `show_full_tab_titles: true` (branch feature), `show_close_button: "always"`, `file_icons: true`, `git_status: true`, `show_diagnostics: "all"`.
- `preview_tabs: {}` — preview tabs with default behavior (a single click opens a preview tab).
- `tab_bar`: pinned tabs on a separate row; navigation-history buttons hidden.
- `title_bar`: menus hidden (the macOS menu bar is used), user menu shown, sign-in and branch-status icon hidden.
- `toolbar`: agent review, selections menu, quick actions and breadcrumbs off — minimal chrome.
- `gutter`: only the git gutter (`git_gutter_width.custom: 16`) and line numbers (`min_line_number_digits: 4` keeps the gutter width stable); folds, bookmarks and breakpoints hidden.
- Panels docked left: `project_panel` (width 320, `folder_indicator: "both"`, comfortable entry spacing, scrollbar `auto`, sticky scroll off), `git_panel` (tree view, grouped by staging, primary click opens the file diff, status shown as a colored label); `bottom_dock_layout: "contained"`; `outline_panel.button: false`; `diagnostics.button: true`; `status_bar.line_endings_button: true`; `terminal.show_count_badge: false`.
- `minimap`: always visible in the active editor, thumb on hover.
- `scrollbar`: always visible with a reserved track (`track: "track"` — the branch's `scrollbar.track` setting), git-diff and cursor markers on.

### Git and diff

- `diff_view_style: "split"`, `minimum_split_diff_width: 0` — always side-by-side diffs.
- `git.hunk_style: "unstaged_hollow"`, `show_stage_restore_buttons: false` — diff decorations without stage/restore buttons in the gutter.
- `git_panel.message_editor_min_lines: 2` — branch feature.

### Fonts, theme and misc

- `ui_font_size: 20`, `buffer_font_size: 16`, `agent_ui_font_size: 20`, `agent_buffer_font_size: 16`, `scroll_sensitivity: 1.0`.
- Themes: dark `Ayu Dark`, light `One Light`; icon theme `Material Icon Theme` in both variants.
- `auto_update: false` — locally built apps must not auto-update.
- `telemetry`: diagnostics on, metrics off, no Anthropic retention.
- `hide_mouse: "never"`.

### AI / agent panel

- Default model: `opencode` / `go/deepseek-v4-flash` with thinking on, effort `high`. Two profiles: `Write` (all tools) and `Ask` (read-only: no delete/create/copy), both with effort `max`.
- `favorite_models`: `go/deepseek-v4-flash`, `go/kimi-k3`, `go/deepseek-v4-pro`, `go/minimax-m3`, `go/glm-5.2` (thinking/effort set per entry).
- Tool permissions: `write_file` and `edit_file` always allowed; `terminal` auto-allowed for a read-only allowlist (`grep`, `head`, `echo`, `tail`, `ls`, `date`, `wc`, `cat`, `sort`, `uniq`, `sed`, `du`, `pgrep`, `stat`, `find`, `strings`); `fetch` allowed for `skills.sh`, `raw.githubusercontent.com`, `api.github.com`; `search_web` allowed.
- Sandbox: `allow_unsandboxed: true`, `network_hosts: ["*.npmjs.org"]`.
- Layout and behavior: `agent.dock` / `sidebar_side: "right"`, default width 420, `message_editor_min_lines: 2` (branch feature), `auto_compact.threshold: "90%"`, `thinking_display: "auto"`, edit/terminal cards collapsed, single-file review off.

### Custom models (`language_models.opencode`)

- `show_zen_models: false` — hide the built-in Zen model list.
- Two custom entries: `deepseek-v4-flash-vision-exp` ("DeepSeek V4 Flash Vision (Direct)") and `deepseek-v4-flash` ("DeepSeek V4 (Direct)"): 1M context / 384k max output, `openai_chat` protocol, interleaved reasoning, reasoning effort levels (`none`/`low`/`high`/`max` for the vision entry, `low`/`high`/`max` for the text one), `subscription: "go"`.

### Agent rules (`assets/settings/initial_agents_md.md`)

Seeded as the global `AGENTS.md` on first run — the always-on instructions the agent picks up in every project:

- **Stack skills (`strict-*`)** — identify the project stack and load the matching built-in skill before writing or reviewing code: `strict-typescript` (TS/JS/web), `strict-angular` (plus `strict-typescript`), `strict-react` (plus `strict-typescript`). Planning is skills too: `/strict-compose-plan`, `/strict-implement-plan`.
- **Behavior** — do not leave plan mode without an explicit confirmation; apply global rules only to the code the task touches (no drive-by refactors).
- **Quality** — read the source or type definitions of third-party APIs instead of relying on memory; no `disable`/`ignore` suppressions, refactor instead; run diagnostics after changes and fix every error and warning.
- **Tooling** — prefer file tools over the terminal, and the diagnostics tool over long compiler/linter runs; keep mermaid simple (it often fails to render).
- **Subagents and images** — `spawn_agent` accepts an optional `model`; images can be inspected inside a subagent (e.g. `go/deepseek-v4-flash-vision-exp`) so they stay out of the main thread context.
- **Documentation integrity** — never delete comments unrelated to the change.

### Keymap (`assets/keymaps/initial.json`, on top of `base_keymap: "VSCode"`)

- **Panels and docks**: `secondary-e` toggles the workspace sidebar (Zed's `alt-cmd-j` unbound), `secondary-shift-b` toggles the right dock (Zed's `secondary-r` unbound), `secondary-shift-e` / `secondary-shift-g` / `secondary-shift-l` toggle the project panel / git panel / agent panel.
- **Project panel**: `enter` opens the entry instead of renaming (`f2` also unbound, `space` no longer opens); rename moves to `secondary-r`.
- **Tabs**: `secondary-w` closes the active item but never closes pinned tabs (`close_pinned: false`).
- **Editor navigation**: `ctrl-left` / `ctrl-right` — beginning/end of line (soft-wrap aware, stops at indent); `cmd-left` / `cmd-right` — previous/next word (Zed's `alt-left` / `alt-right` unbound); `ctrl-shift-left` / `ctrl-shift-right` — select to line start/end; `cmd-shift-left` / `cmd-shift-right` — select by word.
- **Lines and cursors**: `cmd-up` / `cmd-down` — move the current line up/down (Zed's paragraph navigation is moved to `ctrl-up` / `ctrl-down`); `secondary-shift-up` / `secondary-shift-down` — add a cursor above/below (Zed's `cmd-alt-up` / `cmd-alt-down` unbound); `secondary-shift-d` — duplicate line down.
- **Editing**: `secondary-space` — show completions; `secondary-backspace` — delete forward (Zed's delete-to-line-start unbound); `secondary-r` in the editor — rename symbol; `secondary-shift-s` — save all (Save As unbound).
- **Diff hunks**: `secondary-"` toggles all diff hunks instead of only expanding them.
- **Agent**: `secondary-l` — add the current selection to the agent thread (editor, ACP thread and terminal; Zed's `cmd->` unbound); `secondary-shift-l` — toggle the agent panel.
- **Pane history and zoom**: `secondary--` / `secondary-=` — go back / forward (Zed's `ctrl--` / `ctrl-_` unbound); all buffer and UI font-zoom bindings (`cmd-=`, `cmd--`, `cmd-0`, `ctrl-=`, …) are unbound so the configured font sizes stay fixed.
- **Terminal**: `secondary-shift-c` outside the terminal spawns the `Open External Terminal` task (the collab panel's `cmd-shift-c` is unbound).
- **Keymap editor**: `secondary-e` is unbound there so the sidebar shortcut works while editing keymaps.
