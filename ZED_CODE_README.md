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

- `git_panel.message_editor_min_lines` (default `6`): minimum height, in lines, of the commit message editor in the git panel.
- `tabs.show_full_tab_titles` (default `false`): expand tabs to fit the full file name instead of truncating it.
- `scrollbar.track` (default `"track"`): `"track"` reserves space for a scrollbar track next to the content, `"thumb"` floats the scrollbar over it.
- `completion_menu_item_kind` (default `"off"`): gains an `"icon"` value that shows a syntax-colored symbol icon per completion entry.
- `completions.suggest_selection` (default `"first"`): which completion is preselected when the completions menu opens. `"first"` always selects the first entry; `"recently_used"` preselects the most recently accepted completion among the top-scoring entries; `"recently_used_by_prefix"` remembers the completion accepted for a prefix and preselects it again when that prefix is typed.
- `project_panel.title_tooltip_delay`: now also applies to the tooltips of editor tabs and git panel entries.
- `status_bar.icon_scale` (default `1.0`): multiplier for the panel-button icons in the vertical status strips flanking the workspace. At `1.0` they are the same size as the bottom status bar's icons; larger values grow the icons and the strips with them. The strips host only the panel toggles (plus the threads-sidebar toggle) and hide themselves entirely when their side has no buttons to show.
- `search.dock` (default `"left"`): where to dock the search panel, `"left"` or `"right"`; its toggle button follows the dock like every other panel's.
- `references_panel` (default `{ "button": true, "dock": "left", "default_width": 320 }`): the references panel's toggle button, dock side (`"left"` or `"right"`) and default width. The references panel is a branch-local crate.

## Agent changes in Zed Code

Behavior changes to the built-in agent (Zed AI) layered on top of upstream.

### File tools work across the whole filesystem

`read_file`, `write_file`, `edit_file`, `delete_path`, `move_path`, `copy_path`, `create_directory` and `list_directory` accept any absolute path (or one with a `~` prefix), not only project paths and `~/.agents/skills`. Access is gated by the user's `tool_permissions` rules, and destructive operations refuse protected roots (the filesystem root, the home directory, and ancestors of a project worktree).

**Path forms.** A project-relative path normally starts with a worktree root directory name (`zed/crates/foo.rs`); that prefix always resolves. A bare relative path (`crates/foo.rs`) also resolves when it is unambiguous, and a bare path into a gitignored tree (`node_modules/...`) is made to resolve by scanning that directory on demand. Anything outside the project is addressed by an absolute path (or one with a `~` prefix).

### Upstream vs. this branch

The file tools are a mix of upstream behavior and `zed-code` changes. Provenance, so a merge never mistakes one for the other:

**Taken from upstream, unchanged:**

- `Project::find_project_path` and its three branches (absolute; relative with an existing entry; root-name strip without an entry). The bare / rooted / absolute behavior described above is upstream's, not ours.
- The `tool_permissions` model: `default`, `always_allow` / `always_deny` / `always_confirm`, and regex matching of tool input text.
- The symlink-escape permission prompt (`authorize_symlink_access`, upstream #49255) and the sensitive-settings classification (`.zed/`, the global config directory, `~/.agents/skills`; upstream #48641, #56456).
- Global agent skills (`resolve_global_skill_path`, upstream #57678) and the worktree primitives the branch builds on: `add_path_prefix_to_scan`, `refresh_entry*`, `should_scan_directory`, and the search-side gitignored pre-fetch in `project_search.rs` (upstream #42968).

**Added or overridden by this branch (`zed-code`):**

- File tools reach the whole filesystem: `resolve_external_path`, `resolve_creatable_external_path`, `resolve_direct_fs_path`, and the direct-filesystem route (arbitrary absolute / `~` paths, not just project paths and `~/.agents/skills`).
- The user's settings become hard blocks in _every_ tool (`ensure_path_not_hidden_by_settings`, `external_path_excluded_by_settings`), checked before any prompt.
- Protected roots (`is_protected_external_path`): the hard refusal for the filesystem root, the home directory, and worktree ancestors.
- Gitignore stops hiding files: the ignored-directory fallback, the path-triggered scan for a bare relative path (`prescan_ignored_ancestor`), the directory listing scan (`list_directory`), and — for the search tools — the opt-in pre-scan driven by the glob's literal prefix (`find_path`, and `grep` via `prescan_ignored_dirs_for_prefix`). The scanning helpers live together in `crates/agent/src/tools/ignored_scan.rs`; `tool_permissions.rs` keeps only the permission and path-resolution logic.
- `create_directory` resolves a bare relative path to a _new_ directory through its parent (`resolve_new_directory_parent`), the same trick `write_file` already used for a new file. Upstream advertised only the rooted/absolute forms, so a bare new directory used to fail.
- Permission rules match every form of a path (`permission_path_forms`, `decide_permission_for_path_forms`) — upstream matches the raw text only.
- The symlink prompt is extended to external paths and to every path form (`external_path_resolution_target`, `external_symlink_target`).
- Actionable errors (`explain_unresolved_relative_path`, `explain_unscanned_ignored_path`, `explain_unresolved_path`).
- This README and the protected-roots note in `assets/settings/default.json`.

Where a file collides (`crates/agent/src/tools/tool_permissions.rs` was created upstream in #49255), the branch keeps upstream's public shape and layers the above on top.

### The user's file-access settings are enforced by every file tool

`file_scan_exclusions` and `private_files` (global and worktree-scoped) are hard blocks, not soft warnings. `ensure_path_not_hidden_by_settings` in `crates/agent/src/tools/tool_permissions.rs` is called by every file tool — read and mutating alike, in-project and out-of-project — so a path the agent may not read it also may not modify. The check runs **before** the authorization prompt, so a blocked call never asks the user to approve something that cannot happen. `find_path` filters matching paths out of its results for the same reason: a path search must not surface what the read and write tools refuse.

### Permission rules match every form of a path

`tool_permissions` patterns are matched against every textual form of the tool's path, not only the string the model wrote: as written, `~`-expanded, worktree-relative, and absolute (`permission_path_forms` in `crates/agent/src/tools/tool_permissions.rs`, applied by `decide_permission_for_path_forms` in `crates/agent/src/tool_permissions.rs`). A rule can therefore target any project (`^src/`) or one specific project (`^/Users/me/mycoolproject/src/`), whichever form the model happened to use. `copy_path` / `move_path` decide each path over its own forms and still require both paths to be allowed. The raw text stays among the forms, so a rule written against the literal input keeps matching.

This is a branch-local change — upstream matches the raw text only — and it is a genuine semantic change: an `always_allow` now auto-approves the same file however it is spelled, so a bare-relative rule is effectively a cross-project rule. Write rules with the form in mind.

The forms are derived from `Project::find_project_path`, which is exactly what the tool itself uses to resolve the path, so the file a rule is tested against is the file the tool will act on. For a bare relative path in a multi-root workspace that resolution is a guess (the first worktree with an entry), but it is the _same_ guess the tool makes — the rule and the action never disagree.

### Gitignore no longer hides files from the tools

A file tool now has exactly two routes, and gitignore picks neither:

- **Through the worktree** — the file has a snapshot entry. The scanner has verified it, and the user's settings filter it.
- **Directly through the filesystem** — everything else: any path outside the project, and any file inside a directory the worktree never scanned.

The second case is the one that used to be a dead end. Zed's scanner deliberately skips gitignored directories (`node_modules`, `dist`, `target`) so it doesn't index tens of thousands of files, which left every file below one with no snapshot entry. The path-named tools now load that directory on demand before resolving (`prescan_ignored_ancestor`): `read_file` and `list_directory` then go through the worktree like any other file, while `delete_path`, `copy_path`, `move_path` and the edit session route the target through `resolve_direct_fs_path` (`crates/agent/src/tools/tool_permissions.rs`), which is what applies the settings and raises the symlink prompt when the ignored file resolves outside the worktree. So `read_file`, `write_file`, `edit_file`, `delete_path`, `copy_path`, `move_path` and `create_directory` all reach ignored trees now, in every worktree, not just the first.

The fallback keeps the worktree's guarantees rather than dropping them:

- The user's `file_scan_exclusions` / `private_files` are still enforced — global globs against the canonical target, plus the worktree's own globs when the target belongs to one. Out-of-project targets keep the global-only check, as before.
- A path that resolves **somewhere other than itself** — something along the way is a symlink — asks the user first, naming the real canonical target. The snapshot normally catches that (the scanner marks such entries and stores their canonical path), but it never looked inside an unscanned directory, so `resolve_direct_fs_path` canonicalizes and `authorize_direct_fs_path` raises the same prompt the scanned case gets, via the existing `authorize_symlink_access`. This applies to both path forms: an absolute path into an unscanned directory used to be canonicalized and written to silently, which is the hole this closed.
- **Deleting or moving a symlink acts on the link, not its target** — the way `rm`/`mv` and the in-project route do, inside and outside the project alike. `DirectFsPath` carries the named path (leaf not followed), and `delete_path` / `move_path` use it when the leaf is a symlink, so nothing follows the link. The escape prompt still fires, as it does for a scanned in-project symlink: the path does resolve outside the project. `copy_path` is the exception: it dereferences, like `cp` (and like the in-project copy), so copying a link copies what it points at.
- Everything else returns `None` and keeps using the worktree.

`delete_path` on the ignored _directory_ itself needed no fallback — the directory is an entry. `create_directory` resolves a not-yet-existing directory through its parent (`resolve_new_directory_parent`), the same trick `write_file` uses for a new file, so a bare path there works after the parent's on-demand scan.

#### When an ignored directory gets scanned

Gitignored directories stay unscanned until something asks for them. A bare relative path into one would have no snapshot entry to resolve against, so the file tools scan the directory on demand before resolving (see the first trigger below). Two worktree primitives load directories, with different reach:

- `add_path_prefix_to_scan(P)` registers P as a path prefix; `should_scan_directory` then treats every descendant of P as scannable, overriding both gitignore and `file_scan_depth` for that subtree. Reach: **P's whole subtree, recursively.**
- `refresh_entry(path)` / `refresh_entries_for_paths(...)` re-read the named paths. Reach: **those paths only** — nested ignored directories inside them stay unloaded.

Who calls them:

| Trigger                                                      | Primitive                                                             | Reach                            |
| ------------------------------------------------------------ | --------------------------------------------------------------------- | -------------------------------- |
| agent file tools: a bare relative path that doesn't resolve  | `add_path_prefix_to_scan` of the ignored ancestor, then resolve again | that subtree                     |
| agent `find_path` — glob's literal prefix enters one         | `add_path_prefix_to_scan` for that ignored, unloaded directory        | that subtree                     |
| agent `grep` — `include_pattern`'s literal prefix enters one | `add_path_prefix_to_scan` for that ignored, unloaded directory        | that subtree                     |
| agent `list_directory` on an ignored directory               | `add_path_prefix_to_scan` of that directory                           | that subtree                     |
| agent `read_file` (via the rooted or absolute form)          | `open_buffer`                                                         | the one file — no directory scan |
| project panel: expanding an ignored directory                | `add_path_prefix_to_scan`                                             | that subtree                     |
| opening/loading a file in the editor                         | buffer load / `refresh_entries_for_paths`                             | that path                        |
| a language server registering a file watcher                 | `add_path_prefix_to_scan` of the watched prefix                       | that subtree                     |
| rename / copy / drag in the panel, edit prediction           | `refresh_entry` / `refresh_entries_for_paths`                         | the named path                   |
| filesystem watcher events                                    | rescans under the affected subtree                                    | —                                |

Which agent tools scan, and how much — **all of this is branch-local** (upstream never pre-scans for these tools):

- The two search tools share one rule, implemented once in `glob_literal_prefix` / `glob_reaches_dir` (`crates/agent/src/tools/ignored_scan.rs`): an ignored directory is scanned and matched **only when the glob's literal prefix names into it**. A leading `**` names no directory, so it reaches nothing ignored — that is what keeps an ordinary search from exploding into `node_modules`.
- `find_path` scans the ignored, unloaded directory the glob enters — `root/node_modules/**` scans `node_modules`, while `root/src/**` and `**/index.js` leave it alone — and matches ignored entries only inside those same directories, so an ordinary glob never surfaces ignored content even after another tool loaded it (the result does not depend on scan history).
- `grep` applies the same rule to `include_pattern` (through `prescan_ignored_dirs_for_prefix`, because upstream's `PathInclusionMatcher` compares prefixes against worktree-relative paths and would not understand a root-prefixed pattern). With no `include_pattern`, or with one that stays in ordinary paths or starts with a wildcard, ignored directories are neither scanned nor matched.
- `list_directory` scans only the directory it was asked to list.
- `read_file` does not scan a whole directory: it opens the one file. It runs the shared on-demand scan only when a **bare relative path** would otherwise fail to resolve, so the bare form resolves like the rooted/absolute one.
- `edit_file`, `write_file`, `delete_path`, `copy_path`, `move_path`, `create_directory` run the same on-demand scan before resolving a bare relative path (for `create_directory` that path is the new directory's parent).

### Errors that name the actual cause

`explain_unresolved_relative_path` is the fallback for a bare relative path that still doesn't resolve inside a gitignored directory the worktree hasn't scanned; it lists every worktree-relative and absolute form the model can retry with, rather than guessing one root. `explain_unscanned_ignored_path` covers a resolved path that has no entry. The on-demand scan usually resolves such paths before either fires, so the two name the cause only when a scan can't help — a genuinely missing target, or a bare relative path whose parent also doesn't resolve — which is what keeps a model from reading "not found" as "this file doesn't exist" and giving up.

### Every path form and every gate, tabulated

**Path forms.** **R** = worktree-root-prefixed relative (`zed/src/a.rs`), **r** = bare relative (`src/a.rs`), **A** = absolute (`/Users/me/zed/src/a.rs`). _Ignored_ = a path inside a gitignored directory the scanner skipped; _Outside_ = any path outside every worktree.

Reachability — which tool accepts which form:

| Tool               | Project R               | Project r | Project A | Ignored R / A                                      | Ignored r                                   | Outside A / `~` |
| ------------------ | ----------------------- | --------- | --------- | -------------------------------------------------- | ------------------------------------------- | --------------- |
| `read_file`        | yes                     | yes       | yes       | yes                                                | yes, scans the directory on demand          | yes             |
| `list_directory`   | yes                     | yes       | yes       | yes                                                | same                                        | yes             |
| `edit_file`        | yes                     | yes       | yes       | yes                                                | same                                        | yes             |
| `write_file`       | yes                     | yes       | yes       | yes                                                | same                                        | yes             |
| `delete_path`      | yes                     | yes       | yes       | yes                                                | same                                        | yes             |
| `copy_path`        | yes                     | yes       | yes       | yes                                                | same                                        | yes             |
| `move_path`        | yes                     | yes       | yes       | yes                                                | same                                        | yes             |
| `create_directory` | yes                     | yes       | yes       | yes (no entry required)                            | yes, scans the parent's directory on demand | yes             |
| `grep`             | glob over project paths | —         | —         | opt-in: only when `include_pattern` names into one | —                                           | no              |
| `find_path`        | glob over project paths | —         | —         | opt-in: only when the glob names into one          | —                                           | no              |

Gates — what each tool enforces:

| Tool               | Settings                  | Protected roots                   | Sensitive paths | Symlink escape                 |
| ------------------ | ------------------------- | --------------------------------- | --------------- | ------------------------------ |
| `read_file`        | refuse                    | not checked                       | not checked     | prompt, naming the real target |
| `list_directory`   | refuse + entries filtered | not checked                       | not checked     | prompt                         |
| `edit_file`        | refuse                    | not checked (edit, not overwrite) | prompt (always) | prompt                         |
| `write_file`       | refuse                    | refuse                            | prompt (always) | prompt                         |
| `delete_path`      | refuse                    | refuse                            | prompt (always) | prompt                         |
| `copy_path`        | refuse                    | refuse (both paths)               | prompt (always) | prompt (both paths)            |
| `move_path`        | refuse                    | refuse (both paths)               | prompt (always) | prompt (both paths)            |
| `create_directory` | refuse                    | refuse                            | prompt (always) | prompt                         |
| `grep`             | results filtered          | not checked                       | not checked     | not checked                    |
| `find_path`        | results filtered          | not checked                       | not checked     | not checked                    |

Notes:

- **Settings** are `file_scan_exclusions` and `private_files` (global, plus the worktree's own for in-project paths). They are hard blocks, checked before any prompt, on read and mutating tools alike — including `create_directory` on the global skills directory, which applies them like every other tool.
- **Protected roots** are the filesystem root, the user's home directory, and any ancestor of an open worktree. Only destructive operations on paths outside the project guard against them; reads and in-project edits are not affected. See the `tool_permissions` comment in `assets/settings/default.json`.
- **Sensitive paths** are `.zed/`, the global config directory, and `~/.agents/skills`. Only the mutating tools gate them (an always-shown prompt, never remembered); the read tools are not restricted. This classification is upstream.
- **Symlink escape** means the resolved path differs from the path as written. The gate fires for both path forms: the snapshot marks such entries when a directory was scanned, and `resolve_direct_fs_path` / `external_symlink_target` canonicalize and notice otherwise. It fires for a leaf symlink too, even for `delete_path` / `move_path`, which act on the link itself rather than its target. The prompt is never remembered, because the permission rules match path text while the OS acts on the resolved file.
- **`grep` and `find_path`** take a glob, not a path, matched against project-relative paths (`zed/src/a.rs`); an absolute glob such as `/root/zed/**` will not match. Gitignored content is **opt-in**: it is scanned and searched only when the glob's (`find_path`) or `include_pattern`'s (`grep`) literal prefix starts inside an ignored directory, so a leading `**` reaches nothing ignored. They stay project-scoped, so they cannot reach a path outside every worktree.

### The agent guide is overridable at runtime

The system prompt template (`crates/agent/src/templates/system_prompt.hbs`) can be replaced at runtime — no rebuild, no restart:

```
~/.config/zed/prompt_overrides/system_prompt.hbs    # macOS
<data_dir>/prompt_overrides/system_prompt.hbs       # Linux / Windows
```

A `*.hbs` file dropped into the prompt-overrides directory (`paths::prompt_overrides_dir`, the same directory Zed already uses to override other prompts) replaces the built-in template of the same name at render time, so the next message picks it up. A file that fails to read or render falls back to the built-in template instead of panicking. Dev builds additionally read the checkout's `crates/agent/src/templates/*.hbs` at runtime via `fs_embed!` (edits there still require rebuilding the `agent` crate).

### Guide content updates

The agent guide (`system_prompt.hbs`, mirrored in `experimental_system_prompt.hbs`) was corrected and extended:

- File tools are described as operating across the whole filesystem, stated as facts rather than preferences: a project-relative path that starts with a worktree root name, and an absolute path, always resolve — including into gitignored content such as `node_modules` — while a bare project-relative path works only when it is unambiguous (gitignored directories are scanned on demand). The wording is deliberately not a "prefer relative" style rule — that is an unverifiable preference, whereas the path forms are a property of the harness.
- Search tools are described as staying out of gitignored directories unless the pattern names one from the project root (for example `root/node_modules/**`), and that a leading `**` does not reach them — matching the opt-in scan rule above.
- A `## Web Search` section (rendered when `search_web` is available) explains when to reach for it, and — when `fetch` is also available — that search results carry URLs worth fetching for the full content.
- The `## Multi-agent delegation` section was rewritten with concrete delegation triggers, cost framing (a sub-agent costs one tool call), parallel spawning in the same turn, self-contained briefs with disjoint write scopes, and optional sub-agent model selection via `list_agents_and_models` — pass the exact `models[].id` from the native entry (`is_native: true`); omitting `model` falls back to the configured subagent model, or the parent model when none is set. For long-output commands it asks the sub-agent to report the failing lines or diagnostics.

### Test coverage for the file tools

The filesystem behavior above is hard to check by hand, so it is covered by tests in the `agent` crate (`cargo test -p agent --lib`, ~800 tests). Each file tool is exercised along four axes: in-project paths, out-of-project absolute paths, gitignored content (including a second worktree with its own repository and `.gitignore`), and the user's settings (`private_files`, `file_scan_exclusions`, `tool_permissions`, protected roots). Notable cases:

- `tool_permissions.rs` — `test_real_fs_external_path_resolution_and_protection` exercises the real `RealFs` canonicalization macOS applies to `/tmp`, so the external/protected classification is tested against the actual filesystem rather than a fake. It is a plain `#[test]`, not `#[gpui::test]`, because `RealFs` spawns real threads that the deterministic test scheduler forbids.
- Symlink-escape authorization per tool, including the confirm-once and deny-policy paths.
- Settings-blocked calls assert both the error text and that **no** authorization event was emitted, which is what pins the "check before prompt" ordering.
- Gitignored targets are covered from both sides: each mutating tool has a test that it now edits/deletes/copies/moves a file inside an unscanned ignored directory, and `test_edit_file_gitignored_symlink_escape_asks_before_following` / `test_delete_path_gitignored_symlink_escape_asks_before_following` pin that a symlink out of one prompts with the real target and that denying it leaves the target untouched. `test_edit_file_gitignored_symlink_escape_allowed` pins the other half. `test_delete_path_gitignored_file_respects_file_scan_exclusions` and its `edit_file` counterpart pin that the fallback doesn't become a way around the user's settings.

### Single-file agent diff view

Clicking a changed file in a thread's edits block opens a single-file review view (`AgentDiffView`, `crates/agent_ui/src/agent_diff_view.rs`) instead of the multi-file `AgentDiffPane`; the per-row **Review** button is gone, since it duplicated the row click. The view offers hunk navigation and right-aligned Keep / Reject / Keep All / Reject All plus a View File action, opens scrolled to the first hunk, and does not duplicate the split/unified and fold controls that the buffer search bar already renders.

The Keep/Reject pill for a diff hunk now renders **above** the hunk (one line up, clamped to the sticky header) instead of covering its first row. This lives in the shared diff-hunk layout, so it applies to every diff hunk renderer, including the git views.

### Selections and diagnostics as thread context

A thread accepts more than editor and terminal selections: a diagnostic can be added to a thread too ("Add Diagnostic to Thread"), so a compiler or linter error travels to the agent as context without being copied by hand.

### Queued messages render as text previews

Messages queued while the agent is running render as a two-line clamped text preview with a full-text tooltip instead of a live editor; the row actions (steer, send now, delete) stay right-aligned.

### Built-in agent skills

Five skills ship inside the binary next to upstream's `create-skill`: `strict-typescript`, `strict-angular`, `strict-react`, `strict-compose-plan` and `strict-implement-plan`. The seeded `AGENTS.md` tells the agent to load them (see "Agent rules" below).

### Web search

- **TinyFish** is added as a web-search provider; it is selected independently of the language-model provider, so `search_web` works with any model.
- The `search_web` tool takes richer parameters — `purpose`, `location`, `language`, `include_domains` / `exclude_domains`, `domain_type` (`web` / `news`), `after_date` / `before_date`, `recency_minutes`, `page` — and each provider uses the fields it supports.

## Editor UI changes in Zed Code

Everything in this section is a `zed-code` change — upstream ships none of it. Where a change extends an upstream primitive (the panel/dock framework, the status bar, the tab bar, the diff-hunk layout, the settings schema), the primitive is upstream and only the behavior described here is ours.

### Project panel

- A **Project** header above the tree with an expand/collapse-all toggle.
- **Read-only affordances**: for a read-only project (no write access, remote/WSL file system) the write actions — rename, create, paste, delete, drag — are hidden or disabled instead of failing when clicked.

### Git panel and diffs

- Tree directories and status entries can be dragged out of the panel (a marked selection moves together).
- Status rows gain a hover **View File** action; directories get a context menu (stage / unstage / discard a folder, reveal in the OS file manager).
- Diff hunks in a solo diff can be staged from the diff itself.
- A solo diff is taken against the base its section implies — staged content against `HEAD`, unstaged against the index — and each base gets its own view.
- Solo diffs open in **preview tabs** that replace one another, and report the active project path.
- The row of the file focused in the editor is highlighted in the panel.
- Commit and blame tooltips capture the scroll wheel only while their message overflows; otherwise the editor behind scrolls.
- Staging updates the panel immediately — the index write refreshes the changed paths instead of waiting for the file watcher.

### Search panel

- Deploy Search (`cmd-shift-f`) opens the search **dock panel** rather than a project-search multibuffer.
- Opening the panel seeds the query from the editor selection (or the word under the cursor, or the buffer search bar) and runs it.
- Result rows match the threads-panel styling and get the project panel's context menu; the references panel shares the project panel's default width.
- `search.dock` chooses the side.

### References panel

- Find All References opens a **dock panel** (the branch-local `references_panel` crate) instead of a picker.
- The panel is dockable left/right, its status-bar button is hideable, and its width comes from `references_panel.default_width`.
- Opening selects and centers the match the query was run from.

### Editor

- Smooth scrolling is animated per frame along a composed easing curve, and stepping through buffer/project search results smooth-scrolls to each match.
- Go-to-definition inside a diff (including a solo diff) scrolls and moves the cursor in that view instead of opening a different editor.
- The completion menu can show a syntax-colored symbol icon per entry (`completion_menu_item_kind: "icon"`) and preselect an entry based on recent use (`completions.suggest_selection`).

### Tabs and workspace chrome

- The tab's trailing slot shows the unsaved-changes indicator in the place of the close button; hovering swaps in close (or unpin), VS Code-style, with no layout shift.
- Vertical status strips on the left and right edges host the panel toggles, and the workspace sidebar toggle moved into them (out of the sidebar). Their icons scale with `status_bar.icon_scale`, and a strip with nothing to show hides itself.
- Scroll input is routed by its dominant axis, so a mostly-vertical gesture scrolls horizontally-overflowing elements such as the tab bar.

### Keymap editor

- The context menu gains **Restore Default** (drops the user's unbind that suppressed a default binding) and **Add Alternative**.

### Settings window

- An **Others** page lists every `settings.json` key that has no dedicated page, generated from the settings schema.

### Tasks

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

Two consequences:

- Running a build of this branch now **creates** `~/.config/zed/settings.json` and `keymap.json` on first launch when they do not exist yet. Those files carry this branch's recommended values — `"base_keymap": "VSCode"`, the 20/16 font sizes, the Ayu Dark theme, `"auto_update": false`, the `telemetry` block — and an official Zed on the same machine will apply them too. Before the branch started seeding, a dev build left those files absent, so nothing leaked across.
- `AGENTS.md` is new for this branch, but it is seeded into the same shared directory for the same reason.

`crates/paths/src/paths.rs` already states the intended remedy ("Forks should change this to avoid colliding with Zed's user data"): giving the fork its own `APP_NAME` moves the config, data, cache and state directories away from an official install. That is a separate decision, because it also relocates an existing user's settings.

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
- Layout and behavior: `agent.dock` / `sidebar_side: "right"`, default width 420, `message_editor_min_lines: 2`, `auto_compact.threshold: "90%"`, `thinking_display: "auto"`, edit/terminal cards collapsed, single-file review off.

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
