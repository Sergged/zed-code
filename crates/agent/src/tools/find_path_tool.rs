use crate::{AgentTool, ToolCallEventStream, ToolInput, glob_literal_prefix, glob_reaches_dir};
use acp_thread::MentionUri;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Result, anyhow};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{App, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use project::{Project, WorktreeSettings};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::fmt::Write;
use std::{cmp, path::PathBuf, sync::Arc};
use util::paths::PathMatcher;
use util::rel_path::RelPath;

/// Find file paths that match a given pattern.
///
/// - Returns matching file paths sorted alphabetically
/// - Prefer the `grep` tool to this tool when searching for symbols unless you have specific information about paths.
/// - Use this tool when you need to find files by name patterns
/// - Gitignored files are searched only when the glob names into a gitignored directory (e.g. `myproject/node_modules/**`). Ordinary project globs neither scan nor match them; only the user's explicit `file_scan_exclusions` / `private_files` settings can block paths outright.
/// - Results are paginated with 50 matches per page. Use the optional 'offset' parameter to request subsequent pages.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindPathToolInput {
    /// The glob to match against every path in the project.
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
    /// Optional starting position for paginated results (0-based).
    /// When not provided, starts from the beginning.
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FindPathToolOutput {
    Success {
        offset: usize,
        current_matches_page: Vec<PathBuf>,
        all_matches_len: usize,
    },
    Error {
        error: String,
    },
}

impl From<FindPathToolOutput> for LanguageModelToolResultContent {
    fn from(output: FindPathToolOutput) -> Self {
        match output {
            FindPathToolOutput::Success {
                offset,
                current_matches_page,
                all_matches_len,
            } => {
                if current_matches_page.is_empty() {
                    "No matches found".into()
                } else {
                    let mut llm_output = format!("Found {} total matches.", all_matches_len);
                    if all_matches_len > RESULTS_PER_PAGE {
                        write!(
                            &mut llm_output,
                            "\nShowing results {}-{} (provide 'offset' parameter for more results):",
                            offset + 1,
                            offset + current_matches_page.len()
                        )
                        .ok();
                    }

                    for mat in current_matches_page {
                        write!(&mut llm_output, "\n{}", mat.display()).ok();
                    }

                    llm_output.into()
                }
            }
            FindPathToolOutput::Error { error } => error.into(),
        }
    }
}

const RESULTS_PER_PAGE: usize = 50;

pub struct FindPathTool {
    project: Entity<Project>,
}

impl FindPathTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for FindPathTool {
    type Input = FindPathToolInput;
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
        let mut title = "Find paths".to_string();
        if let Ok(input) = input {
            title.push_str(&format!(" matching “`{}`”", input.glob));
        }
        title.into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| FindPathToolOutput::Error {
                error: e.to_string(),
            })?;

            let search_paths_task = cx.update(|cx| search_paths(&input.glob, project, cx));

            let matches = futures::select! {
                result = search_paths_task.fuse() => result.map_err(|e| FindPathToolOutput::Error { error: e.to_string() })?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err(FindPathToolOutput::Error { error: "Path search cancelled by user".to_string() });
                }
            };
            let paginated_matches: &[PathBuf] = &matches[cmp::min(input.offset, matches.len())
                ..cmp::min(input.offset + RESULTS_PER_PAGE, matches.len())];

            event_stream.update_fields(
                acp::ToolCallUpdateFields::new()
                    .title(if paginated_matches.is_empty() {
                        "No matches".into()
                    } else if paginated_matches.len() == 1 {
                        "1 match".into()
                    } else {
                        format!("{} matches", paginated_matches.len())
                    })
                    .content(
                        paginated_matches
                            .iter()
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
                current_matches_page: paginated_matches.to_vec(),
                all_matches_len: matches.len(),
            })
        })
    }
}

fn search_paths(glob: &str, project: Entity<Project>, cx: &mut App) -> Task<Result<Vec<PathBuf>>> {
    let path_style = project.read(cx).path_style(cx);
    let path_matcher = match PathMatcher::new(
        [
            // Sometimes models try to search for "". In this case, return all paths in the project.
            if glob.is_empty() { "*" } else { glob },
        ],
        path_style,
    ) {
        Ok(matcher) => matcher,
        Err(err) => return Task::ready(Err(anyhow!("Invalid glob: {err}"))),
    };
    let global_settings = WorktreeSettings::get_global(cx).clone();
    let worktrees: Vec<_> = project.read(cx).worktrees(cx).collect();

    // Skipping ignored directories that cannot hold a match keeps the scan
    // proportional to the glob instead of always pulling in every gitignored
    // tree. An empty prefix (a leading wildcard) means nothing can be skipped.
    let literal_prefix = glob_literal_prefix(glob);

    cx.spawn(async move |cx| {
        let mut results = Vec::new();
        for worktree in worktrees {
            // Wait for the worktree's initial scan so the snapshot is current
            // before we start pre-scanning ignored directories.
            let scan_complete = worktree.read_with(cx, |worktree, _| {
                worktree.as_local().map(|local| local.scan_complete())
            });
            if let Some(scan_complete) = scan_complete {
                scan_complete.await;
            }

            let mut snapshot = worktree.read_with(cx, |worktree, _| worktree.snapshot());
            let worktree_settings = worktree.read_with(cx, |worktree, _| {
                worktree.as_local().map(|local| local.settings())
            });

            // The gitignored directories the glob's literal prefix reaches into.
            // Only these may be scanned, and only these may be *matched*: an
            // ordinary project glob must not surface ignored content even after
            // some other tool loaded it, so the result never depends on scan
            // history. A leading wildcard names no directory, so it reaches none.
            let reached_ignored_dirs: Vec<Arc<RelPath>> = snapshot
                .entries(true, 0)
                .filter(|entry| {
                    entry.is_ignored
                        && glob_reaches_dir(
                            snapshot.root_name().join(&entry.path).as_rel_path(),
                            literal_prefix.as_rel_path(),
                        )
                })
                .map(|entry| entry.path.clone())
                .collect();

            // Gitignored directories aren't scanned by default; scan the reached
            // ones so ignored files (node_modules, build output, .env) are
            // findable. Only the user's explicit `file_scan_exclusions` setting
            // can block them — gitignore alone must not hide files from the tools.
            let entries_to_refresh: Vec<_> = reached_ignored_dirs
                .iter()
                .filter(|path| {
                    snapshot
                        .entry_for_path(path.as_ref())
                        .is_some_and(|entry| entry.kind.is_unloaded())
                        && worktree_settings
                            .as_ref()
                            .is_none_or(|settings| !settings.is_path_excluded(path))
                })
                .cloned()
                .collect();
            if !entries_to_refresh.is_empty() {
                let barriers = worktree.update(cx, |worktree, _| {
                    let local = worktree.as_local_mut()?;
                    Some(
                        entries_to_refresh
                            .into_iter()
                            .map(|path| local.add_path_prefix_to_scan(path).into_future())
                            .collect::<Vec<_>>(),
                    )
                });
                if let Some(barriers) = barriers {
                    futures::future::join_all(barriers).await;
                }
                snapshot = worktree.read_with(cx, |worktree, _| worktree.snapshot());
            }

            for entry in snapshot.entries(true, 0) {
                // Ignored entries are visible only inside a reached directory
                // (see above), so an ordinary glob never surfaces ignored content.
                if entry.is_ignored
                    && !entry.is_always_included
                    && !reached_ignored_dirs
                        .iter()
                        .any(|dir| entry.path.starts_with(dir))
                {
                    continue;
                }
                // Respect the user's explicit settings here too: a path search
                // must not surface files that read/write tools refuse.
                if global_settings.is_path_excluded(&entry.path)
                    || global_settings.is_path_private(&entry.path)
                    || worktree_settings.as_ref().is_some_and(|settings| {
                        settings.is_path_excluded(&entry.path)
                            || settings.is_path_private(&entry.path)
                    })
                {
                    continue;
                }
                if path_matcher.is_match(&snapshot.root_name().join(&entry.path)) {
                    results.push(snapshot.absolutize(&entry.path));
                }
            }
        }

        Ok(results)
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use gpui::TestAppContext;
    use gpui::UpdateGlobal as _;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use util::path;

    #[gpui::test]
    async fn test_find_path_tool(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                "apple": {
                    "banana": {
                        "carrot": "1",
                    },
                    "bandana": {
                        "carbonara": "2",
                    },
                    "endive": "3"
                }
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;

        let matches = cx
            .update(|cx| search_paths("root/**/car*", project.clone(), cx))
            .await
            .unwrap();
        assert_eq!(
            matches,
            &[
                PathBuf::from(path!("/root/apple/banana/carrot")),
                PathBuf::from(path!("/root/apple/bandana/carbonara"))
            ]
        );

        let matches = cx
            .update(|cx| search_paths("**/car*", project.clone(), cx))
            .await
            .unwrap();
        assert_eq!(
            matches,
            &[
                PathBuf::from(path!("/root/apple/banana/carrot")),
                PathBuf::from(path!("/root/apple/bandana/carbonara"))
            ]
        );
    }

    #[gpui::test]
    async fn test_find_path_includes_gitignored_directories(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": {
                    "main.rs": "fn main() {}",
                },
                "node_modules": {
                    "pkg": {
                        "index.js": "module.exports = 1;",
                    },
                },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let matches = cx
            .update(|cx| search_paths("root/node_modules/**/index.js", project.clone(), cx))
            .await
            .unwrap();
        assert!(
            matches
                .iter()
                .any(|path| path.to_string_lossy().ends_with("index.js")),
            "expected gitignored node_modules file to be found, got: {matches:?}"
        );
    }

    /// A glob with a literal prefix only scans the ignored directories that can
    /// hold a match; an unrelated ignored tree is left unloaded.
    #[gpui::test]
    async fn test_find_path_scans_only_relevant_ignored_dirs(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": { "main.rs": "fn main() {}" },
                "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let node_modules_is_unloaded = |project: &Entity<Project>, cx: &mut TestAppContext| {
            project.read_with(cx, |project, cx| {
                let Some(worktree) = project.worktrees(cx).next() else {
                    return false;
                };
                let snapshot = worktree.read(cx).snapshot();
                snapshot
                    .entry_for_path(RelPath::from_unix_str("node_modules").unwrap())
                    .is_some_and(|entry| entry.kind.is_unloaded())
            })
        };

        // `root/src/**` cannot match anything under `node_modules`, so it must
        // not scan it.
        let matches = cx
            .update(|cx| search_paths("root/src/**/*.rs", project.clone(), cx))
            .await
            .unwrap();
        assert!(
            matches
                .iter()
                .any(|path| path.to_string_lossy().ends_with("main.rs")),
            "expected the literal-prefix match, got: {matches:?}"
        );
        assert!(
            node_modules_is_unloaded(&project, cx),
            "node_modules must stay unloaded for a glob that cannot match it"
        );

        // A prefix inside the ignored directory does scan it, and finds it.
        let matches = cx
            .update(|cx| search_paths("root/node_modules/**/*.js", project.clone(), cx))
            .await
            .unwrap();
        assert!(
            matches
                .iter()
                .any(|path| path.to_string_lossy().ends_with("index.js")),
            "expected the ignored match, got: {matches:?}"
        );
    }

    /// A leading wildcard names no directory, so it must not reach into ignored
    /// content either — even though it could match a path there.
    #[gpui::test]
    async fn test_find_path_wildcard_does_not_reach_ignored(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": { "main.rs": "fn main() {}" },
                "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let matches = cx
            .update(|cx| search_paths("**/index.js", project.clone(), cx))
            .await
            .unwrap();
        assert!(
            !matches
                .iter()
                .any(|path| path.to_string_lossy().ends_with("index.js")),
            "a wildcard glob must not pull in ignored content, got: {matches:?}"
        );

        let node_modules_is_unloaded = project.read_with(cx, |project, cx| {
            let Some(worktree) = project.worktrees(cx).next() else {
                return false;
            };
            let snapshot = worktree.read(cx).snapshot();
            snapshot
                .entry_for_path(RelPath::from_unix_str("node_modules").unwrap())
                .is_some_and(|entry| entry.kind.is_unloaded())
        });
        assert!(
            node_modules_is_unloaded,
            "node_modules must stay unloaded for a wildcard glob"
        );
    }

    /// Matching is keyed off the glob's literal prefix, not off what happens to
    /// be loaded in the snapshot: once another tool scans an ignored directory,
    /// an ordinary glob must still not surface its files.
    #[gpui::test]
    async fn test_find_path_loaded_ignored_content_stays_hidden(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": { "main.rs": "fn main() {}" },
                "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        // Naming the ignored directory loads it into the snapshot.
        let matches = cx
            .update(|cx| search_paths("root/node_modules/**/*.js", project.clone(), cx))
            .await
            .unwrap();
        assert!(
            matches
                .iter()
                .any(|path| path.to_string_lossy().ends_with("index.js")),
            "expected the named ignored match, got: {matches:?}"
        );

        // Ordinary globs must not match it now that it is loaded.
        for glob in ["**/index.js", "root/**/*.js"] {
            let matches = cx
                .update(|cx| search_paths(glob, project.clone(), cx))
                .await
                .unwrap();
            assert!(
                !matches
                    .iter()
                    .any(|path| path.to_string_lossy().ends_with("index.js")),
                "glob `{glob}` must not surface loaded ignored content, got: {matches:?}"
            );
        }
    }

    /// Overrides the user's worktree file-access settings, which the file tools
    /// treat as hard blocks: a path search must not surface files that the
    /// read/write tools refuse.
    fn set_worktree_settings(
        cx: &mut TestAppContext,
        update: impl FnOnce(&mut settings::SettingsContent),
    ) {
        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, update);
            });
        });
    }

    #[gpui::test]
    async fn test_find_path_filters_private_and_excluded_paths(cx: &mut TestAppContext) {
        init_test(cx);
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.file_scan_exclusions =
                Some(settings::SplicingVec::from(vec!["**/build".to_string()]));
            settings.project.worktree.private_files = Some(vec!["**/.env".to_string()].into());
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            serde_json::json!({
                "src": { "main.rs": "fn main() {}" },
                "build": { "output.txt": "artifact" },
                ".env": "SECRET=1",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let matches = cx
            .update(|cx| search_paths("**/*", project.clone(), cx))
            .await
            .unwrap();
        let matches: Vec<String> = matches
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();

        assert!(
            matches
                .iter()
                .any(|path| path.ends_with("root/src/main.rs")),
            "expected the regular file to be found, got: {matches:?}"
        );
        assert!(
            !matches.iter().any(|path| path.contains("output.txt")),
            "`file_scan_exclusions` paths must not be surfaced, got: {matches:?}"
        );
        assert!(
            !matches.iter().any(|path| path.contains(".env")),
            "`private_files` paths must not be surfaced, got: {matches:?}"
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }
}
