//! An agent-facing `grep` that takes a search root.
//!
//! Behaves like the terminal search tools: the model points at a root with
//! `path` and the results come from there. Internally this is a wrapper over
//! upstream's [`GrepTool`], so `grep_tool.rs` stays byte-identical to upstream
//! and keeps merging cleanly.
//!
//! When `path` is absent the input is handed to upstream's tool unchanged: its
//! project-wide search, snippet rendering with symbol context, clickable
//! locations and pagination all stay in use. When `path` is given the root is
//! walked here, because upstream's search is scoped to worktrees and cannot
//! express an arbitrary root. A directly-walked root applies no gitignore rule —
//! what is in the root is what gets searched — which is what makes a named
//! `node_modules` reachable without a separate opt-in flag.

use super::grep_tool::{GrepTool, GrepToolInput};
use super::ignored_scan::prescan_ignored_ancestor;
use super::tool_permissions::{
    authorize_symlink_access, canonicalize_worktree_roots, external_path_excluded_by_settings,
    external_path_resolution_target, resolve_external_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use acp_thread::MentionUri;
use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use project::{Project, search::SearchQuery};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    cmp,
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::markdown::{MarkdownCodeBlock, MarkdownInlineCode};
use util::paths::PathMatcher;

/// Searches the contents of files with a regular expression
///
/// - Prefer this tool to path search when searching for symbols, because you
///   won't need to guess what path it's in.
/// - Supports full regex syntax (eg. "log.*Error", "function\\s+\\w+", etc.)
/// - Set `path` to search a specific root — a directory inside the project, or
///   any absolute or `~`-prefixed path outside it (another repository, a config
///   directory, `node_modules`, and so on). Without `path`, the whole project is
///   searched and gitignored directories are skipped unless `include_pattern`
///   names one from a project root.
/// - With `path` set, exactly that root is walked: gitignore rules do not apply,
///   so a named `node_modules` or build directory is searched normally.
/// - Never use this tool to search for paths. Only search file contents with it.
/// - Results are paginated with 20 matches per page. Use the optional 'offset'
///   parameter to request subsequent pages.
/// - DO NOT use HTML entities solely to escape characters in the tool parameters.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentGrepToolInput {
    /// A regex pattern to search for in file contents. Note that the regex will
    /// be parsed by the Rust `regex` crate.
    ///
    /// Do NOT specify a path here! This will only be matched against the code
    /// **content**.
    pub regex: String,
    /// A glob pattern for the paths of files to include in the search.
    /// Supports standard glob patterns like "**/*.rs" or "frontend/src/**/*.ts".
    /// If omitted, all files under the search root are searched.
    ///
    /// With `path` set, the pattern is matched against paths **relative to that
    /// root**. Without `path`, it is matched against full project paths that
    /// start with a project root directory name, as before.
    ///
    /// <example>
    /// If the project has the following root directories:
    ///
    /// - /a/b/backend
    /// - /c/d/frontend
    ///
    /// Use "backend/**/*.rs" to search only Rust files in the backend root
    /// directory. Use "frontend/src/**/*.ts" to search TypeScript files only in
    /// the frontend root directory (sub-directory "src"). Use "**/*.rs" to
    /// search Rust files across all root directories.
    /// </example>
    #[serde(default)]
    pub include_pattern: Option<String>,
    /// The root to search: a project-relative, absolute, or `~`-prefixed path to
    /// a directory. If omitted, every worktree of the project is searched.
    ///
    /// This is what makes the tool behave like the terminal search tools — point
    /// it at a directory and the results come from there. A root outside the
    /// project, and a root inside a gitignored directory, are searched directly:
    /// no gitignore rule is applied to a root you named.
    ///
    /// <example>
    /// - `~/.config/nvim` — search a configuration directory outside the project
    /// - `zed/crates/agent/src` — scope the search to one subtree
    /// - `zed/node_modules/pkg` — search inside a gitignored directory
    /// </example>
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Optional starting position for paginated results, counted in matches
    /// (0-based). When not provided, starts from the beginning.
    #[serde(default)]
    pub offset: u32,
    /// Whether the regex is case-sensitive. Defaults to false (case-insensitive).
    #[serde(default)]
    pub case_sensitive: bool,
}

const RESULTS_PER_PAGE: usize = 20;
/// How many lines of context surround a match in a directly-walked root.
const CONTEXT_LINES: usize = 2;
const MAX_DEPTH: usize = 32;
const MAX_FILES_VISITED: usize = 20_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct AgentGrepTool {
    project: Entity<Project>,
}

impl AgentGrepTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

/// One rendered match: a line range in a file, with the snippet that range
/// covers.
struct RootMatch {
    abs_path: PathBuf,
    start_row: u32,
    end_row: u32,
    snippet: String,
}

impl AgentTool for AgentGrepTool {
    type Input = AgentGrepToolInput;
    type Output = String;

    const NAME: &'static str = "grep";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => {
                let regex = MarkdownInlineCode(&input.regex);
                let case_info = if input.case_sensitive {
                    " (case-sensitive)"
                } else {
                    ""
                };
                match &input.path {
                    Some(path) => {
                        let root = MarkdownInlineCode(&path.display().to_string());
                        format!("Search {root} for regex {regex}{case_info}")
                    }
                    None if input.offset > 0 => {
                        format!("Get page of search results for regex {regex}{case_info}")
                    }
                    None => format!("Search files for regex {regex}{case_info}"),
                }
            }
            Err(_) => "Search with regex".into(),
        }
        .into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;

            let Some(raw_path) = input.path.clone() else {
                // No root named: upstream's project-wide search already is the
                // right behavior, so hand the input over unchanged.
                return delegate(&project, input, event_stream, cx).await;
            };

            let fs = project.read_with(cx, |project, _| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            // A root outside every worktree is walked directly through the
            // filesystem: the project's search runs over worktrees and cannot
            // reach past them.
            if let Some(external_root) =
                resolve_external_path(&raw_path, &canonical_roots, fs.as_ref()).await
            {
                if let Some(setting) =
                    cx.update(|cx| external_path_excluded_by_settings(&external_root, cx))
                {
                    return Err(format!(
                        "Cannot search {} because its path matches the user's global `{setting}` setting",
                        raw_path.display()
                    ));
                }

                let requested = raw_path.display().to_string();
                if let Some(canonical_target) =
                    external_path_resolution_target(&raw_path, &external_root)
                {
                    let authorize = cx.update(|cx| {
                        authorize_symlink_access(
                            Self::NAME,
                            &requested,
                            &canonical_target,
                            &event_stream,
                            cx,
                        )
                    });
                    authorize.await.map_err(|error| error.to_string())?;
                }

                return search_root(&external_root, &input, &project, fs.as_ref(), &event_stream, cx)
                    .await;
            }

            // A bare relative path into a gitignored directory the worktree never
            // scanned cannot resolve through the snapshot; scan it first so it
            // resolves like the rooted and absolute forms.
            prescan_ignored_ancestor(&project, &raw_path, cx).await;

            let absolute = cx.update(|cx| {
                let project = project.read(cx);
                project
                    .find_project_path(&raw_path, cx)
                    .and_then(|project_path| project.absolute_path(&project_path, cx))
            });
            let Some(absolute) = absolute else {
                return Err(format!(
                    "Could not find path {} in the project or on the filesystem",
                    raw_path.display()
                ));
            };

            search_root(&absolute, &input, &project, fs.as_ref(), &event_stream, cx).await
        })
    }
}

/// Hands the input to upstream's `grep`, keeping its project-wide search and
/// rendering in use.
async fn delegate(
    project: &Entity<Project>,
    input: AgentGrepToolInput,
    event_stream: ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    let value = serde_json::to_value(GrepToolInput {
        regex: input.regex,
        include_pattern: input.include_pattern,
        offset: input.offset,
        case_sensitive: input.case_sensitive,
    })
    .map_err(|error| error.to_string())?;

    let tool = Arc::new(GrepTool::new(project.clone()));
    let task = cx.update(|cx| tool.run(ToolInput::ready(value), event_stream, cx));
    task.await
}

/// Walks `root` and renders the matching lines, the way a terminal content
/// search does. No gitignore rule applies: the root was named explicitly.
async fn search_root(
    root: &Path,
    input: &AgentGrepToolInput,
    project: &Entity<Project>,
    fs: &dyn Fs,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    if !fs.is_dir(root).await {
        return Err(format!("Could not find path {}", root.display()));
    }

    let path_style = cx.update(|cx| project.read(cx).path_style(cx));

    let include_matcher = match input.include_pattern.as_deref() {
        Some(pattern) => Some(
            PathMatcher::new([pattern], path_style)
                .map_err(|error| format!("invalid include glob pattern: {error}"))?,
        ),
        None => None,
    };

    let query = SearchQuery::regex(
        &input.regex,
        false,
        input.case_sensitive,
        true,
        false,
        // Path filtering is done here, against paths relative to the walked
        // root; the query is used only for content matching.
        PathMatcher::default(),
        PathMatcher::default(),
        false,
        None,
    )
    .map_err(|error| error.to_string())?;

    let matches_wanted = input.offset as usize + RESULTS_PER_PAGE + 1;
    let mut matches: Vec<RootMatch> = Vec::new();
    let mut files_visited = 0usize;
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    visited_dirs.insert(root.to_path_buf());

    'walk: while let Some((dir, depth)) = stack.pop() {
        let mut entries = match fs.read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        while let Some(entry) = futures::select! {
            entry = entries.next().fuse() => entry,
            _ = event_stream.cancelled_by_user().fuse() => {
                return Err("Search cancelled by user".to_string());
            }
        } {
            let Ok(entry) = entry else {
                continue;
            };

            // `futures::select!` polls the directory stream first, and on a fast
            // filesystem it is always ready — so cancellation needs an explicit
            // check too, or a long walk would ignore it until the directory is
            // exhausted.
            if event_stream.was_cancelled_by_user() {
                return Err("Search cancelled by user".to_string());
            }

            if matches.len() >= matches_wanted || files_visited >= MAX_FILES_VISITED {
                break 'walk;
            }

            // `metadata` follows symlinks, so read the link flag from the entry
            // itself and never descend through one: a symlinked directory could
            // leave the root or form a cycle.
            let is_symlink = fs
                .metadata(&entry)
                .await
                .ok()
                .flatten()
                .is_none_or(|metadata| metadata.is_symlink);
            if is_symlink {
                continue;
            }

            if fs.is_dir(&entry).await {
                if depth + 1 > MAX_DEPTH {
                    continue;
                }
                let canonical = match fs.canonicalize(&entry).await {
                    Ok(canonical) => canonical,
                    Err(_) => continue,
                };
                if !canonical.starts_with(root) || !visited_dirs.insert(canonical) {
                    continue;
                }
                stack.push((entry, depth + 1));
                continue;
            }

            let relative = entry.strip_prefix(root).unwrap_or(&entry).to_path_buf();
            if let Some(matcher) = &include_matcher
                && !matcher.is_match_std_path(&relative)
            {
                continue;
            }
            if cx
                .update(|cx| external_path_excluded_by_settings(&entry, cx))
                .is_some()
            {
                continue;
            }

            let size = fs
                .metadata(&entry)
                .await
                .ok()
                .flatten()
                .map_or(0, |metadata| metadata.len);
            if size > MAX_FILE_BYTES {
                continue;
            }

            files_visited += 1;

            // `load` fails on non-UTF-8 content, which is how binary files drop
            // out of the walk.
            let Ok(content) = fs.load(&entry).await else {
                continue;
            };
            let lines = LineIndex::new(&content);
            let ranges: Vec<(usize, usize)> = query
                .search_str(&content)
                .into_iter()
                .map(|range| {
                    (
                        lines.row_for_byte(range.start),
                        lines.row_for_byte(range.end.saturating_sub(1).max(range.start)),
                    )
                })
                .collect();
            if ranges.is_empty() {
                continue;
            }

            let last_row = lines.line_count().saturating_sub(1);
            for (start_row, end_row) in merge_ranges(ranges, CONTEXT_LINES, last_row) {
                if matches.len() >= matches_wanted {
                    break 'walk;
                }
                matches.push(RootMatch {
                    abs_path: entry.clone(),
                    start_row: start_row as u32,
                    end_row: end_row as u32,
                    snippet: lines.snippet(start_row, end_row),
                });
            }
        }
    }

    render_root_matches(&matches, input.offset, event_stream)
}

fn render_root_matches(
    matches: &[RootMatch],
    offset: u32,
    event_stream: &ToolCallEventStream,
) -> Result<String, String> {
    if matches.is_empty() {
        return Ok("No matches found".to_string());
    }

    let start = cmp::min(offset as usize, matches.len());
    let end = cmp::min(start + RESULTS_PER_PAGE, matches.len());
    let page = &matches[start..end];
    let has_more = matches.len() > end;

    let mut output = String::new();
    let mut content = Vec::new();
    let mut locations = Vec::new();
    let mut last_path: Option<&Path> = None;

    for matched in page {
        if last_path != Some(matched.abs_path.as_path()) {
            writeln!(output, "\n## Matches in {}", matched.abs_path.display()).ok();
            last_path = Some(matched.abs_path.as_path());
        }

        let label = if matched.start_row == matched.end_row {
            format!("L{}", matched.start_row + 1)
        } else {
            format!("L{}-{}", matched.start_row + 1, matched.end_row + 1)
        };
        writeln!(output, "\n### {label}\n").ok();
        output.push_str("```\n");
        output.push_str(&matched.snippet);
        if !matched.snippet.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("```\n");

        let uri = MentionUri::Selection {
            abs_path: Some(matched.abs_path.clone()),
            line_range: matched.start_row..=matched.end_row,
            column: None,
        };
        content.push(acp::ToolCallContent::Content(acp::Content::new(
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                format!("{}#{}", matched.abs_path.display(), label),
                uri.to_uri().to_string(),
            )),
        )));
        locations.push(
            acp::ToolCallLocation::new(matched.abs_path.clone()).line(Some(matched.start_row)),
        );
        content.push(acp::ToolCallContent::Content(acp::Content::new(
            acp::ContentBlock::Text(acp::TextContent::new(
                MarkdownCodeBlock {
                    tag: "",
                    text: &matched.snippet,
                }
                .to_string(),
            )),
        )));
    }

    event_stream.update_fields(
        acp::ToolCallUpdateFields::new()
            .content(content)
            .locations(locations),
    );

    if has_more {
        Ok(format!(
            "Showing matches {}-{} (there were more matches found; use offset: {} to see next page):\n{output}",
            offset + 1,
            offset + page.len() as u32,
            offset + RESULTS_PER_PAGE as u32,
        ))
    } else {
        Ok(format!("Found {} matches:\n{output}", page.len()))
    }
}

/// Byte offsets of each line start, for turning a match's byte range into row
/// numbers without rescanning the content per match.
struct LineIndex<'a> {
    content: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    fn new(content: &'a str) -> Self {
        let mut starts = vec![0];
        for (index, byte) in content.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(index + 1);
            }
        }
        Self { content, starts }
    }

    fn line_count(&self) -> usize {
        self.starts.len()
    }

    fn row_for_byte(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1)
    }

    fn snippet(&self, start_row: usize, end_row: usize) -> String {
        let last_row = self.line_count().saturating_sub(1);
        let start_row = start_row.min(last_row);
        let end_row = end_row.min(last_row);
        if start_row > end_row {
            return String::new();
        }
        let start = self.starts[start_row];
        let end = self
            .starts
            .get(end_row + 1)
            .copied()
            .unwrap_or(self.content.len());
        self.content[start..end].to_string()
    }
}

/// Expands each row range by `context` lines and merges the ones that touch, so
/// one snippet covers a cluster of nearby matches the way upstream's
/// buffer-backed path does.
fn merge_ranges(
    ranges: Vec<(usize, usize)>,
    context: usize,
    last_row: usize,
) -> Vec<(usize, usize)> {
    let mut expanded: Vec<(usize, usize)> = ranges
        .into_iter()
        .map(|(start, end)| {
            (
                start.saturating_sub(context),
                end.saturating_add(context).min(last_row),
            )
        })
        .collect();
    expanded.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in expanded {
        match merged.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentTool;
    use agent_client_protocol::schema::v1 as acp;
    use gpui::{TestAppContext, UpdateGlobal};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// Installs user-level `worktree` settings, which the out-of-project
    /// settings check reads via `WorktreeSettings::get_global`.
    fn set_worktree_settings(
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut settings::SettingsContent),
    ) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, update);
            });
        });
    }

    fn extract_paths_from_results(result: &str) -> Vec<String> {
        result
            .lines()
            .filter_map(|line| line.strip_prefix("## Matches in "))
            .map(|path| path.to_string())
            .collect()
    }

    async fn run_grep_tool(
        input: AgentGrepToolInput,
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> String {
        let tool = Arc::new(AgentGrepTool::new(project));
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(input),
                ToolCallEventStream::test().0,
                cx,
            )
        });

        match task.await {
            Ok(output) | Err(output) => output,
        }
    }

    #[gpui::test]
    async fn test_path_outside_project_is_searched(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ ".git": {}, "src": { "main.rs": "fn main() {}" } }),
        )
        .await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "notes.md": "const OUTSIDE_MARKER = 1;\n" }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "OUTSIDE_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = extract_paths_from_results(&result);
        assert!(
            paths.iter().any(|path| path.contains("notes.md")),
            "a root outside the project should be searched, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_tilde_root_outside_project_is_searched(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/Users/zed"),
            json!({ "notes.md": "const HOME_MARKER = 1;\n" }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "HOME_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from("~/")),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = extract_paths_from_results(&result);
        assert!(
            paths.iter().any(|path| path.contains("notes.md")),
            "a `~` root should resolve and be searched, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_named_gitignored_root_is_searched(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                ".gitignore": "node_modules/\n",
                "node_modules": { "pkg": { "index.js": "const DEPRECATED_API = 'old';\n" } },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "DEPRECATED_API".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from("root/node_modules")),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = extract_paths_from_results(&result);
        assert!(
            paths.iter().any(|path| path.contains("index.js")),
            "naming a gitignored root should search it, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_include_pattern_is_relative_to_the_root(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({
                "keep.rs": "const MARKER = 1;\n",
                "skip.txt": "const MARKER = 2;\n",
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "MARKER".to_string(),
                include_pattern: Some("**/*.rs".to_string()),
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = extract_paths_from_results(&result);
        assert!(
            paths.iter().any(|path| path.contains("keep.rs")),
            "include_pattern should match relative to the root, got: {result}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("skip.txt")),
            "include_pattern should exclude non-matching files, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_missing_root_is_reported(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "anything".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from("root/does-not-exist")),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        assert!(
            result.contains("Could not find path"),
            "a missing root should be reported clearly, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_no_path_delegates_to_upstream(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ ".git": {}, "src": { "main.rs": "fn main() {}\n// PROJECT_MARKER\n" } }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "PROJECT_MARKER".to_string(),
                include_pattern: None,
                path: None,
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        assert!(
            result.contains("main.rs"),
            "without a path the upstream project-wide search should run, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_private_files_are_refused(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "secret.txt": "const SECRET_MARKER = 1;\n" }),
        )
        .await;

        set_worktree_settings(cx, |settings| {
            settings.project.worktree.private_files =
                Some(vec!["**/secret.txt".to_string()].into());
        });

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "SECRET_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        assert!(
            !result.contains("secret.txt"),
            "a private file must not be searched, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_walk_does_not_create_worktrees(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "notes.md": "const OUTSIDE_MARKER = 1;\n" }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let before = cx.update(|cx| project.read(cx).worktrees(cx).count());

        let _ = run_grep_tool(
            AgentGrepToolInput {
                regex: "OUTSIDE_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let after = cx.update(|cx| project.read(cx).worktrees(cx).count());
        assert_eq!(
            before, after,
            "walking a root must not create a worktree (the single-file worktree trap)"
        );
    }

    #[test]
    fn test_merge_ranges_expands_and_joins() {
        assert_eq!(
            merge_ranges(vec![(10, 10), (12, 12)], 2, 100),
            vec![(8, 14)],
            "ranges within the context distance merge into one snippet"
        );
        assert_eq!(
            merge_ranges(vec![(1, 1), (40, 40)], 2, 100),
            vec![(0, 3), (38, 42)],
            "distant ranges stay separate, and the start clamps at zero"
        );
        assert_eq!(
            merge_ranges(vec![(90, 95)], 2, 100),
            vec![(88, 97)],
            "the end is expanded by the context lines"
        );
    }

    #[test]
    fn test_line_index_rows_and_snippet() {
        let content = "first\nsecond\nthird\n";
        let index = LineIndex::new(content);

        assert_eq!(index.line_count(), 4);
        assert_eq!(index.row_for_byte(0), 0);
        assert_eq!(index.row_for_byte(6), 1);
        assert_eq!(index.row_for_byte(13), 2);
        assert_eq!(index.snippet(0, 1), "first\nsecond\n");
        assert_eq!(index.snippet(2, 2), "third\n");
    }

    #[test]
    fn test_kind_is_search() {
        assert_eq!(AgentGrepTool::kind(), acp::ToolKind::Search);
    }

    /// An external root that is itself a symlink resolves to a different
    /// directory than the path text names, so the user sees the real target
    /// before the walk starts.
    #[gpui::test]
    async fn test_external_symlink_root_requests_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "real": { "notes.md": "const OUTSIDE_MARKER = 1;\n" } }),
        )
        .await;
        fs.create_symlink(
            path!("/outside/link").as_ref(),
            PathBuf::from(path!("/outside/real")),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(AgentGrepTool::new(project.clone()));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(AgentGrepToolInput {
                    regex: "OUTSIDE_MARKER".to_string(),
                    include_pattern: None,
                    path: Some(PathBuf::from(path!("/outside/link"))),
                    offset: 0,
                    case_sensitive: false,
                }),
                event_stream,
                cx,
            )
        });

        let authorization = event_rx.expect_authorization().await;
        let title = authorization
            .tool_call
            .fields
            .title
            .as_deref()
            .unwrap_or("");
        assert!(title.contains("symlink to"), "title: {title}");
        assert!(
            title.contains("real"),
            "title should name the real target: {title}"
        );
        authorization
            .response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;
        match result {
            Ok(output) | Err(output) => assert!(
                output.contains("notes.md"),
                "an approved symlink root should be searched, got: {output}"
            ),
        }
    }

    #[gpui::test]
    async fn test_files_larger_than_the_cap_are_skipped(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({})).await;

        let oversized = format!(
            "const SIZE_MARKER = 1;\n{}",
            "x".repeat(MAX_FILE_BYTES as usize + 1)
        );
        fs.insert_file(path!("/outside/big.txt"), oversized.into_bytes())
            .await;
        fs.insert_file(
            path!("/outside/small.txt"),
            b"const SIZE_MARKER = 2;\n".to_vec(),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "SIZE_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = extract_paths_from_results(&result);
        assert!(
            paths.iter().any(|path| path.contains("small.txt")),
            "a file under the cap should be searched, got: {result}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("big.txt")),
            "a file over the cap should be skipped, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_binary_files_are_skipped(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({})).await;
        fs.insert_file(
            path!("/outside/bin.dat"),
            b"\xff\xfe const BIN_MARKER = 1;\x00".to_vec(),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "BIN_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        assert!(
            !result.contains("bin.dat"),
            "non-UTF-8 content should drop out of the walk, got: {result}"
        );
    }

    /// `offset` counts matches, not pages, and the last page must stop
    /// advertising more. Matches are spaced so the ±2 context lines do not merge
    /// them into one snippet.
    #[gpui::test]
    async fn test_pagination_covers_all_matches(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({})).await;

        let mut content = String::new();
        for index in 0..25 {
            writeln!(content, "const ROW_MARKER = {index};").ok();
            for _ in 0..5 {
                content.push_str("filler\n");
            }
        }
        fs.insert_file(path!("/outside/many.txt"), content.into_bytes())
            .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let first = run_grep_tool(
            AgentGrepToolInput {
                regex: "ROW_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;
        assert_eq!(
            first.matches("### ").count(),
            RESULTS_PER_PAGE,
            "the first page should hold a full page: {first}"
        );
        assert!(
            first.contains("use offset: 20"),
            "the first page should point at the next one: {first}"
        );

        let second = run_grep_tool(
            AgentGrepToolInput {
                regex: "ROW_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 20,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;
        assert_eq!(
            second.matches("### ").count(),
            5,
            "the last page should hold the remainder: {second}"
        );
        assert!(
            !second.contains("use offset"),
            "the last page should not advertise more: {second}"
        );
    }

    /// The walk must observe cancellation even though `futures::select!` prefers
    /// the (always-ready) directory stream.
    #[gpui::test]
    async fn test_walk_is_cancelled(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "notes.md": "const OUTSIDE_MARKER = 1;\n" }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(AgentGrepTool::new(project.clone()));
        let (event_stream, _events, mut cancel) = ToolCallEventStream::test_with_cancellation();
        cancel.send(true).unwrap();

        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(AgentGrepToolInput {
                    regex: "OUTSIDE_MARKER".to_string(),
                    include_pattern: None,
                    path: Some(PathBuf::from(path!("/outside"))),
                    offset: 0,
                    case_sensitive: false,
                }),
                event_stream,
                cx,
            )
        });

        match task.await {
            Ok(output) | Err(output) => assert!(
                output.contains("cancelled by user"),
                "a cancelled walk should say so, got: {output}"
            ),
        }
    }

    #[gpui::test]
    async fn test_depth_limit_is_enforced(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "shallow.txt": "const DEPTH_MARKER = 1;\n" }),
        )
        .await;

        let mut deepest = PathBuf::from(path!("/outside"));
        for _ in 0..(MAX_DEPTH + 5) {
            deepest.push("d");
        }
        fs.create_dir(&deepest).await.unwrap();
        fs.insert_file(
            deepest.join("deep.txt"),
            b"const DEPTH_MARKER = 2;\n".to_vec(),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let result = run_grep_tool(
            AgentGrepToolInput {
                regex: "DEPTH_MARKER".to_string(),
                include_pattern: None,
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
                case_sensitive: false,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = extract_paths_from_results(&result);
        assert!(
            paths.iter().any(|path| path.contains("shallow.txt")),
            "a shallow file should be found, got: {result}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("deep.txt")),
            "a path deeper than MAX_DEPTH should not be walked, got: {result}"
        );
    }

    #[gpui::test]
    async fn test_external_results_carry_clickable_links(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({ "notes.txt": "the needle" }))
            .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(AgentGrepTool::new(project.clone()));
        let (event_stream, mut events) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(AgentGrepToolInput {
                    regex: "needle".to_string(),
                    include_pattern: None,
                    path: Some(PathBuf::from(path!("/outside"))),
                    offset: 0,
                    case_sensitive: false,
                }),
                event_stream,
                cx,
            )
        });
        let output = task.await.expect("grep should succeed");
        assert!(output.contains("notes.txt"), "got: {output}");

        let update = events.expect_update_fields().await;
        let content = update.content.expect("expected content blocks");
        let links = content
            .iter()
            .filter_map(|block| match block {
                acp::ToolCallContent::Content(inner) => match &inner.content {
                    acp::ContentBlock::ResourceLink(link) => Some(link),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(links.len(), 1, "expected one resource link per match");

        let expected_uri = MentionUri::Selection {
            abs_path: Some(PathBuf::from(path!("/outside/notes.txt"))),
            line_range: 0..=0,
            column: None,
        }
        .to_uri()
        .to_string();
        assert!(
            links.iter().any(|link| link.uri == expected_uri),
            "missing the selection link for the walked root, got: {links:?}"
        );

        let locations = update.locations.expect("expected locations");
        assert!(
            locations.iter().any(|location| {
                location.path.to_string_lossy().replace('\\', "/")
                    == path!("/outside/notes.txt").replace('\\', "/")
                    && location.line == Some(0)
            }),
            "missing the location for the walked root, got: {locations:?}"
        );
    }
}
