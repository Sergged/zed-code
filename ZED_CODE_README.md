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

- `tabs.show_full_tab_titles` (default `false`): expand tabs to fit the full file name instead of truncating it.
- `completion_menu_item_kind` (default `"off"`): gains an `"icon"` value that shows a syntax-colored symbol icon per completion entry.
- `completions.suggest_selection` (default `"first"`): which completion is preselected when the completions menu opens. `"first"` always selects the first entry; `"recently_used"` preselects the most recently accepted completion among the top-scoring entries; `"recently_used_by_prefix"` remembers the completion accepted for a prefix and preselects it again when that prefix is typed.
- `project_panel.title_tooltip_delay`: now also applies to the tooltips of editor tabs and git panel entries.

## Agent changes in Zed Code

Behavior changes to the built-in agent (Zed AI) layered on top of upstream.


**Path forms.** A project-relative path normally starts with a worktree root directory name (`zed/crates/foo.rs`); that prefix always resolves. A bare relative path (`crates/foo.rs`) also resolves when it is unambiguous, and a bare path into a gitignored tree (`node_modules/...`) is made to resolve by scanning that directory on demand. Anything outside the project is addressed by an absolute path (or one with a `~` prefix).

### Upstream vs. this branch

**Taken from upstream, unchanged:**

- `Project::find_project_path` and its three branches (absolute; relative with an existing entry; root-name strip without an entry). The bare / rooted / absolute behavior described above is upstream's, not ours.
- The `tool_permissions` model: `default`, `always_allow` / `always_deny` / `always_confirm`, and regex matching of tool input text.

**Added or overridden by this branch (`zed-code`):**

- The user's `file_scan_exclusions` / `private_files` are still enforced — global globs against the canonical target, plus the worktree's own globs when the target belongs to one. Out-of-project targets keep the global-only check, as before.
- A path that resolves **somewhere other than itself** — something along the way is a symlink — asks the user first, naming the real canonical target. The snapshot normally catches that (the scanner marks such entries and stores their canonical path), but it never looked inside an unscanned directory, so `resolve_direct_fs_path` canonicalizes and `authorize_direct_fs_path` raises the same prompt the scanned case gets, via the existing `authorize_symlink_access`. This applies to both path forms: an absolute path into an unscanned directory used to be canonicalized and written to silently, which is the hole this closed.
- **Deleting or moving a symlink acts on the link, not its target** — the way `rm`/`mv` and the in-project route do, inside and outside the project alike. `DirectFsPath` carries the named path (leaf not followed), and `delete_path` / `move_path` use it when the leaf is a symlink, so nothing follows the link. The escape prompt still fires, as it does for a scanned in-project symlink: the path does resolve outside the project. `copy_path` is the exception: it dereferences, like `cp` (and like the in-project copy), so copying a link copies what it points at.
- Everything else returns `None` and keeps using the worktree.

`delete_path` on the ignored _directory_ itself needed no fallback — the directory is an entry. `create_directory` resolves a not-yet-existing directory through its parent (`resolve_new_directory_parent`), the same trick `write_file` uses for a new file, so a bare path there works after the parent's on-demand scan.

#### When an ignored directory gets scanned

The global FIFO ordering across sessions is unchanged and still deliberate: `is_first_tool_call` uses it to decide which single card carries the buttons, so only one prompt is actionable at a time.

- A `## Web Search` section (rendered when `search_web` is available) explains when to reach for it, and — when `fetch` is also available — that search results carry URLs worth fetching for the full content.
- `tool_permissions.rs` — `test_real_fs_external_path_resolution_and_protection` exercises the real `RealFs` canonicalization macOS applies to `/tmp`, so the external/protected classification is tested against the actual filesystem rather than a fake. It is a plain `#[test]`, not `#[gpui::test]`, because `RealFs` spawns real threads that the deterministic test scheduler forbids.
- Symlink-escape authorization per tool, including the confirm-once and deny-policy paths.
- Settings-blocked calls assert both the error text and that **no** authorization event was emitted, which is what pins the "check before prompt" ordering.
The toolbar mirrors `SoloDiffView` left to right, which meant splitting it into two registered toolbar items rather than one:

- `AgentDiffViewStyleToolbar` (`PrimaryLeft`, registered **before** `Breadcrumbs`) — Show Full File and the split/unified controls, so the file path follows them.
- `AgentDiffViewToolbar` (`PrimaryRight`) — the `+`/`-` stat, hunk navigation, Reject / Keep, Reject All / Keep All, and View File.

No other toolbar duplicates those controls: `SoloDiffStyleToolbar` and `FileDiffStyleToolbar` only match their own item types, and the chain `act_as_type(SplittableEditor) == None` → `uses_hidden_splittable_editor == true` → `needs_expand_collapse_option == false` keeps the search bar from re-rendering the split/unified and fold buttons. The same `false` sends it to `Secondary`, which is the separate plate below the toolbar.

Like `SoloDiffView`, the view returns `None` from `Item::act_as_type` for `SplittableEditor`. That keeps the buffer search bar in its own toolbar row below the buttons — both toolbars stay visible at once instead of the search displacing the review actions — and stops the search bar from re-rendering the split/unified and fold controls the toolbar already owns. Returning `Some` there is what puts the search in `PrimaryLeft` and produces the duplicate controls.

A thread accepts more than editor and terminal selections: a diagnostic can be added to a thread too ("Add Diagnostic to Thread"), so a compiler or linter error travels to the agent as context without being copied by hand.

### Built-in agent skills

Five skills ship inside the binary next to upstream's `create-skill`: `strict-typescript`, `strict-angular`, `strict-react`, `strict-compose-plan` and `strict-implement-plan`. The seeded `AGENTS.md` tells the agent to load them (see "Agent rules" below).

### Web search

- **TinyFish** is added as a web-search provider; it is selected independently of the language-model provider, so `search_web` works with any model.
- The `search_web` tool takes richer parameters — `purpose`, `location`, `language`, `include_domains` / `exclude_domains`, `domain_type` (`web` / `news`), `after_date` / `before_date`, `recency_minutes`, `page` — and each provider uses the fields it supports.

Everything in this section is a `zed-code` change — upstream ships none of it. Where a change extends an upstream primitive (the panel/dock framework, the status bar, the tab bar, the diff-hunk layout, the settings schema), the primitive is upstream and only the behavior described here is ours.

### Project panel

- A **Project** header above the tree with an expand/collapse-all toggle.

### Git panel and diffs

- Tree directories and status entries can be dragged out of the panel (a marked selection moves together).
- Diff hunks in a solo diff can be staged from the diff itself.
- A solo diff is taken against the base its section implies — staged content against `HEAD`, unstaged against the index — and each base gets its own view, named after the base it shows.
- Commit and blame tooltips capture the scroll wheel only while their message overflows; otherwise the editor behind scrolls.
- Staging updates the panel immediately — the index write refreshes the changed paths instead of waiting for the file watcher.
- The commit message editor's height is configurable through `git_panel.message_editor_min_lines`.
- Clicking a row's checkbox toggles it without selecting the row or starting a file drag, and leaves the row hovered so its hover-only controls stay visible.
### Search panel

- Deploy Search (`cmd-shift-f`) opens the search **dock panel** rather than a project-search multibuffer.
- Opening the panel seeds the query from the editor selection (or the word under the cursor, or the buffer search bar) and runs it.
### References panel

- Smooth scrolling is animated per frame along a composed easing curve, and stepping through buffer/project search results smooth-scrolls to each match.
- Go-to-definition inside a diff (including a solo diff) scrolls and moves the cursor in that view instead of opening a different editor.
### Keymap editor

- The context menu gains **Restore Default** (drops the user's unbind that suppressed a default binding) and **Add Alternative**.

- Task templates support per-OS overrides (`osx` / `linux` / `windows`: `command`, `args`, `cwd`, `env`) and a `$ZED_ACTIVE_WORKTREE_ROOT` variable that follows the title-bar worktree switcher / git-panel repository selection.
- The default tasks include **Open External Terminal**, which opens the OS terminal in the selected folder (`open -a Terminal` on macOS, `start cmd /k` on Windows).

## Recommended initial settings, keymap and agent rules

Unlike "Settings added in this branch" above (options this branch adds to Zed itself), the files below are the branch's **recommended starting setup**. They are seeded on first run:

- `assets/settings/initial_user_settings.json` — seeded as the user settings;
- `assets/keymaps/initial.json` — seeded as the initial user keymap;
- `assets/settings/initial_agents_md.md` — seeded as the global `AGENTS.md` (agent rules).

A fresh Zed Code install therefore starts with a VS Code-like setup (`"base_keymap": "VSCode"`) and a ready-made agent rules file, while keeping Zed's architecture and defaults underneath.

### The config directory is shared with the official Zed build

Seeding writes into Zed's own config directory: this branch keeps `paths::APP_NAME` at its upstream value (`"Zed"`, `crates/paths/src/paths.rs`), and on macOS that resolves to `~/.config/zed`. The `APP_NAME_LOWERCASE`/`CARGO_BIN_NAME` assertion in `crates/zed/src/main.rs` pins the binary name to `zed`, the bundling scripts' `APP_NAME` only sets `.app` metadata, and the release channel names (`Zed Dev`, and so on) do not affect these paths. So a build of this branch reads and writes the **same** `settings.json`, `keymap.json` and `AGENTS.md` as an official Zed installed on the same machine.

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

### Custom models (`language_models.opencode`)

- `show_zen_models: false` — hide the built-in Zen model list.
- Two custom entries: `deepseek-v4-flash-vision-exp` ("DeepSeek V4 Flash Vision (Direct)") and `deepseek-v4-flash` ("DeepSeek V4 (Direct)"): 1M context / 384k max output, `openai_chat` protocol, interleaved reasoning, reasoning effort levels (`none`/`low`/`high`/`max` for the vision entry, `low`/`high`/`max` for the text one), `subscription: "go"`.

### Agent rules (`assets/settings/initial_agents_md.md`)

Seeded as the global `AGENTS.md` on first run — the always-on instructions the agent picks up in every project:

- **Stack skills (`strict-*`)** — identify the project stack and load the matching built-in skill before writing or reviewing code: `strict-typescript` (TS/JS/web), `strict-angular` (plus `strict-typescript`), `strict-react` (plus `strict-typescript`). Planning is skills too: `/strict-compose-plan`, `/strict-implement-plan`.
- **Behavior** — do not leave plan mode without an explicit confirmation; apply global rules only to the code the task touches (no drive-by refactors).
- **Quality** — read the source or type definitions of third-party APIs instead of relying on memory; no `disable`/`ignore` suppressions, refactor instead; run diagnostics after changes and fix every error and warning.
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
