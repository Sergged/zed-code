//! An agent-facing `find_path` that takes a search root.
//!
//! The counterpart to [`super::agent_grep`]: `path` names the root to list, and
//! the glob is matched against paths relative to it. Without `path` the input is
//! handed to upstream's [`FindPathTool`] unchanged, so `find_path_tool.rs` stays
//! as upstream and keeps merging cleanly.

use super::find_path_tool::{FindPathTool, FindPathToolInput, FindPathToolOutput};
use super::ignored_scan::prescan_ignored_ancestor;
use super::tool_permissions::{
    authorize_symlink_access, canonicalize_worktree_roots, external_path_excluded_by_settings,
    external_path_resolution_target, resolve_external_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use acp_thread::MentionUri;
use agent_client_protocol::schema::v1 as acp;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    cmp,
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::paths::PathMatcher;

/// Find file paths that match a given pattern
///
/// - Returns matching file paths sorted alphabetically
/// - Prefer the `grep` tool to this tool when searching for symbols unless you
///   have specific information about paths.
/// - Set `path` to list a specific root — a directory inside the project, or any
///   absolute or `~`-prefixed path outside it. Without `path` the whole project
///   is searched and gitignored directories are skipped unless the glob names one
///   from a project root.
/// - With `path` set, exactly that root is listed: gitignore rules do not apply,
///   so a named `node_modules` or build directory is listed normally.
/// - Results are paginated with 50 matches per page. Use the optional 'offset'
///   parameter to request subsequent pages.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentFindPathToolInput {
    /// The glob to match against every path under the search root.
    ///
    /// <example>
    /// If the project has the following root directories:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can get back the first two paths by providing a glob of "*thing*.txt"
    /// </example>
    pub glob: String,
    /// The root to list: a project-relative, absolute, or `~`-prefixed path to a
    /// directory. If omitted, every worktree of the project is searched.
    ///
    /// With a root set, the glob is matched against paths **relative to that
    /// root**, and no gitignore rule applies to the root you named.
    ///
    /// <example>
    /// - `~/.config/nvim` — list files in a configuration directory outside the project
    /// - `zed/crates/agent/src` — list files in one subtree
    /// - `zed/node_modules/pkg` — list files inside a gitignored directory
    /// </example>
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Optional starting position for paginated results, counted in matches
    /// (0-based). When not provided, starts from the beginning.
    #[serde(default)]
    pub offset: usize,
}

const RESULTS_PER_PAGE: usize = 50;
const MAX_DEPTH: usize = 32;
const MAX_ENTRIES_VISITED: usize = 20_000;

pub struct AgentFindPathTool {
    project: Entity<Project>,
}

impl AgentFindPathTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for AgentFindPathTool {
    type Input = AgentFindPathToolInput;
    type Output = FindPathToolOutput;

    const NAME: &'static str = "find_path";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => match &input.path {
                Some(path) => format!(
                    "Find paths matching `{}` under {}",
                    input.glob,
                    path.display()
                )
                .into(),
                None => format!("Find paths matching `{}`", input.glob).into(),
            },
            Err(_) => "Find paths".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| FindPathToolOutput::Error {
                error: error.to_string(),
            })?;

            let Some(raw_path) = input.path.clone() else {
                return delegate(&project, input, event_stream, cx).await;
            };

            let fs = project.read_with(cx, |project, _| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            let root = if let Some(external_root) =
                resolve_external_path(&raw_path, &canonical_roots, fs.as_ref()).await
            {
                if let Some(setting) =
                    cx.update(|cx| external_path_excluded_by_settings(&external_root, cx))
                {
                    return Err(FindPathToolOutput::Error {
                        error: format!(
                            "Cannot search {} because its path matches the user's global `{setting}` setting",
                            raw_path.display()
                        ),
                    });
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
                    if let Err(error) = authorize.await {
                        return Err(FindPathToolOutput::Error {
                            error: error.to_string(),
                        });
                    }
                }
                external_root
            } else {
                prescan_ignored_ancestor(&project, &raw_path, cx).await;

                let absolute = cx.update(|cx| {
                    let project = project.read(cx);
                    project
                        .find_project_path(&raw_path, cx)
                        .and_then(|project_path| project.absolute_path(&project_path, cx))
                });
                match absolute {
                    Some(absolute) => absolute,
                    None => {
                        return Err(FindPathToolOutput::Error {
                            error: format!(
                                "Could not find path {} in the project or on the filesystem",
                                raw_path.display()
                            ),
                        });
                    }
                }
            };

            let mut matches =
                collect_matching_paths(&root, &input.glob, &project, fs.as_ref(), &event_stream, cx)
                    .await?;
            matches.sort();

            let start = cmp::min(input.offset, matches.len());
            let end = cmp::min(start + RESULTS_PER_PAGE, matches.len());
            let page = &matches[start..end];

            event_stream.update_fields(
                acp::ToolCallUpdateFields::new()
                    .title(if page.is_empty() {
                        "No matches".into()
                    } else if page.len() == 1 {
                        "1 match".into()
                    } else {
                        format!("{} matches", page.len())
                    })
                    .content(
                        page.iter()
                            .map(|path| {
                                let uri = MentionUri::File {
                                    abs_path: path.clone(),
                                };
                                acp::ToolCallContent::Content(acp::Content::new(
                                    acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                                        path.to_string_lossy(),
                                        uri.to_uri().to_string(),
                                    )),
                                ))
                            })
                            .collect::<Vec<_>>(),
                    ),
            );

            Ok(FindPathToolOutput::Success {
                offset: input.offset,
                current_matches_page: page.to_vec(),
                all_matches_len: matches.len(),
            })
        })
    }
}

/// Hands the input to upstream's `find_path`, keeping its project-wide listing
/// in use.
async fn delegate(
    project: &Entity<Project>,
    input: AgentFindPathToolInput,
    event_stream: ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<FindPathToolOutput, FindPathToolOutput> {
    let value = serde_json::to_value(FindPathToolInput {
        glob: input.glob,
        offset: input.offset,
    })
    .map_err(|error| FindPathToolOutput::Error {
        error: error.to_string(),
    })?;

    let tool = Arc::new(FindPathTool::new(project.clone()));
    let task = cx.update(|cx| tool.run(ToolInput::ready(value), event_stream, cx));
    task.await
}

/// Lists every entry under `root` whose path, relative to the root, matches
/// `glob`. No gitignore rule applies: the root was named explicitly.
///
/// The traversal mirrors the one in `agent_grep`, deliberately rather than
/// through a shared helper: a content search stops as soon as it has enough
/// matches, while a path listing has to see the whole tree before it can report
/// a total.
async fn collect_matching_paths(
    root: &Path,
    glob: &str,
    project: &Entity<Project>,
    fs: &dyn Fs,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<Vec<PathBuf>, FindPathToolOutput> {
    if !fs.is_dir(root).await {
        return Err(FindPathToolOutput::Error {
            error: format!("Could not find path {}", root.display()),
        });
    }

    let path_style = cx.update(|cx| project.read(cx).path_style(cx));
    let matcher = PathMatcher::new([if glob.is_empty() { "*" } else { glob }], path_style)
        .map_err(|error| FindPathToolOutput::Error {
            error: format!("Invalid glob: {error}"),
        })?;

    let mut matches = Vec::new();
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
                return Err(FindPathToolOutput::Error {
                    error: "Path search cancelled by user".to_string(),
                });
            }
        } {
            let Ok(entry) = entry else {
                continue;
            };

            // `futures::select!` polls the directory stream first, and on a fast
            // filesystem it is always ready — so cancellation needs an explicit
            // check too, or a long listing would ignore it until the directory is
            // exhausted.
            if event_stream.was_cancelled_by_user() {
                return Err(FindPathToolOutput::Error {
                    error: "Path search cancelled by user".to_string(),
                });
            }

            if matches.len() >= MAX_ENTRIES_VISITED {
                break 'walk;
            }

            // `metadata` follows symlinks, so read the link flag from the entry
            // and never descend through one.
            let is_symlink = fs
                .metadata(&entry)
                .await
                .ok()
                .flatten()
                .is_none_or(|metadata| metadata.is_symlink);
            if is_symlink {
                continue;
            }

            if cx
                .update(|cx| external_path_excluded_by_settings(&entry, cx))
                .is_some()
            {
                continue;
            }

            let relative = entry.strip_prefix(root).unwrap_or(&entry);
            if matcher.is_match_std_path(relative) {
                matches.push(entry.clone());
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
            }
        }
    }

    Ok(matches)
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

    async fn run_find_path_tool(
        input: AgentFindPathToolInput,
        project: Entity<Project>,
        cx: &mut TestAppContext,
    ) -> FindPathToolOutput {
        let tool = Arc::new(AgentFindPathTool::new(project));
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

    fn page_paths(output: &FindPathToolOutput) -> Vec<String> {
        match output {
            FindPathToolOutput::Success {
                current_matches_page,
                ..
            } => current_matches_page
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            FindPathToolOutput::Error { error } => panic!("unexpected error: {error}"),
        }
    }

    #[gpui::test]
    async fn test_root_outside_project_is_listed(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "keep.rs": "fn main() {}", "skip.txt": "text" }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "**/*.rs".to_string(),
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = page_paths(&output);
        assert!(
            paths.iter().any(|path| path.contains("keep.rs")),
            "a root outside the project should be listed, got: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("skip.txt")),
            "the glob should filter results, got: {paths:?}"
        );
    }

    #[gpui::test]
    async fn test_tilde_root_outside_project_is_listed(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/Users/zed"), json!({ "notes.md": "text" }))
            .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "*.md".to_string(),
                path: Some(PathBuf::from("~/")),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = page_paths(&output);
        assert!(
            paths.iter().any(|path| path.contains("notes.md")),
            "a `~` root should resolve and be listed, got: {paths:?}"
        );
    }

    #[gpui::test]
    async fn test_named_gitignored_root_is_listed(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                ".gitignore": "node_modules/\n",
                "node_modules": { "pkg": { "index.js": "text" } },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "**/*.js".to_string(),
                path: Some(PathBuf::from("root/node_modules")),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = page_paths(&output);
        assert!(
            paths.iter().any(|path| path.contains("index.js")),
            "naming a gitignored root should list it, got: {paths:?}"
        );
    }

    #[gpui::test]
    async fn test_missing_root_is_reported(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "*".to_string(),
                path: Some(PathBuf::from("root/does-not-exist")),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        match output {
            FindPathToolOutput::Error { error } => assert!(
                error.contains("Could not find path"),
                "a missing root should be reported clearly, got: {error}"
            ),
            FindPathToolOutput::Success { .. } => {
                panic!("a missing root should be an error, got a success")
            }
        }
    }

    #[gpui::test]
    async fn test_results_are_sorted(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "c.txt": "text", "a.txt": "text", "b.txt": "text" }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "*.txt".to_string(),
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = page_paths(&output);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "results should be sorted alphabetically");
    }

    #[gpui::test]
    async fn test_no_path_delegates_to_upstream(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ ".git": {}, "src": { "main.rs": "fn main() {}" } }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "**/*.rs".to_string(),
                path: None,
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = page_paths(&output);
        assert!(
            paths.iter().any(|path| path.contains("main.rs")),
            "without a path the upstream project-wide search should run, got: {paths:?}"
        );
    }

    #[gpui::test]
    async fn test_private_files_are_refused(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "secret.txt": "text", "notes.md": "text" }),
        )
        .await;

        set_worktree_settings(cx, |settings| {
            settings.project.worktree.private_files =
                Some(vec!["**/secret.txt".to_string()].into());
        });

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let output = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "*.*".to_string(),
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;

        let paths = page_paths(&output);
        assert!(
            paths.iter().any(|path| path.contains("notes.md")),
            "a regular file should be listed, got: {paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.contains("secret.txt")),
            "a private file must not be listed, got: {paths:?}"
        );
    }

    /// The symlink gate is the same call sequence as `grep`'s, but it lives in
    /// this tool's own copy, so it gets its own regression test.
    #[gpui::test]
    async fn test_external_symlink_root_requests_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({ "real": { "notes.md": "text" } }))
            .await;
        fs.create_symlink(
            path!("/outside/link").as_ref(),
            PathBuf::from(path!("/outside/real")),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(AgentFindPathTool::new(project.clone()));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(AgentFindPathToolInput {
                    glob: "*.md".to_string(),
                    path: Some(PathBuf::from(path!("/outside/link"))),
                    offset: 0,
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

        let output = match task.await {
            Ok(output) | Err(output) => output,
        };
        let paths = page_paths(&output);
        assert!(
            paths.iter().any(|path| path.contains("notes.md")),
            "an approved symlink root should be listed, got: {paths:?}"
        );
    }

    #[gpui::test]
    async fn test_pagination_covers_all_matches(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({})).await;
        for index in 0..60 {
            fs.insert_file(
                PathBuf::from(path!("/outside")).join(format!("file-{index:02}.txt")),
                b"text".to_vec(),
            )
            .await;
        }

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let first = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "*".to_string(),
                path: Some(PathBuf::from(path!("/outside"))),
                offset: 0,
            },
            project.clone(),
            cx,
        )
        .await;
        match &first {
            FindPathToolOutput::Success {
                all_matches_len,
                current_matches_page,
                ..
            } => {
                assert_eq!(*all_matches_len, 60);
                assert_eq!(current_matches_page.len(), RESULTS_PER_PAGE);
            }
            FindPathToolOutput::Error { error } => panic!("unexpected error: {error}"),
        }

        let second = run_find_path_tool(
            AgentFindPathToolInput {
                glob: "*".to_string(),
                path: Some(PathBuf::from(path!("/outside"))),
                offset: RESULTS_PER_PAGE,
            },
            project.clone(),
            cx,
        )
        .await;
        match &second {
            FindPathToolOutput::Success {
                all_matches_len,
                current_matches_page,
                offset,
            } => {
                assert_eq!(*all_matches_len, 60);
                assert_eq!(*offset, RESULTS_PER_PAGE);
                assert_eq!(current_matches_page.len(), 10);
            }
            FindPathToolOutput::Error { error } => panic!("unexpected error: {error}"),
        }
    }

    /// The listing must observe cancellation even though
    /// `futures::select!` prefers the (always-ready) directory stream.
    #[gpui::test]
    async fn test_listing_is_cancelled(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ ".git": {} })).await;
        fs.insert_tree(path!("/outside"), json!({ "notes.md": "text" }))
            .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(AgentFindPathTool::new(project.clone()));
        let (event_stream, _events, mut cancel) = ToolCallEventStream::test_with_cancellation();
        cancel.send(true).unwrap();

        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(AgentFindPathToolInput {
                    glob: "*.md".to_string(),
                    path: Some(PathBuf::from(path!("/outside"))),
                    offset: 0,
                }),
                event_stream,
                cx,
            )
        });

        match task.await {
            Ok(FindPathToolOutput::Error { error }) | Err(FindPathToolOutput::Error { error }) => {
                assert!(
                    error.contains("cancelled by user"),
                    "a cancelled listing should say so, got: {error}"
                )
            }
            other => panic!("expected a cancellation error, got: {other:?}"),
        }
    }
}
