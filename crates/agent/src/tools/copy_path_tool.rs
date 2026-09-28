use super::ignored_scan::prescan_ignored_ancestor;
use super::tool_permissions::{
    PathExistence, authorize_direct_fs_path, authorize_symlink_escapes,
    canonicalize_worktree_roots, collect_symlink_escapes, ensure_path_not_hidden_by_settings,
    explain_unresolved_path, is_protected_external_path, permission_path_forms,
    resolve_direct_fs_path, sensitive_settings_kind,
};
use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    authorize_with_sensitive_settings, decide_permission_for_path_groups,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use futures::FutureExt as _;
use gpui::{App, Entity, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::path::Path;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;

/// Copies a file or directory, and returns confirmation that the copy succeeded.
/// Directory contents will be copied recursively.
///
/// This tool should be used when it's desirable to create a copy of a file or directory without modifying the original.
/// It's much more efficient than doing this by separately reading and then writing the file or directory's contents, so this tool should be preferred over that approach whenever copying is the goal.
/// Project-relative paths that start with a project root directory always resolve; bare project-relative paths also work when unambiguous. Absolute paths are accepted too, and every form is subject to the user's agent tool permission rules.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CopyPathToolInput {
    /// The source path of the file or directory to copy.
    /// If a directory is specified, its contents will be copied recursively.
    ///
    /// <example>
    /// If the project has the following files:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can copy the first file by providing a source_path of "directory1/a/something.txt"
    /// </example>
    pub source_path: String,
    /// The destination path where the file or directory should be copied to.
    ///
    /// <example>
    /// To copy "directory1/a/something.txt" to "directory2/b/copy.txt", provide a destination_path of "directory2/b/copy.txt"
    /// </example>
    pub destination_path: String,
}

pub struct CopyPathTool {
    project: Entity<Project>,
}

impl CopyPathTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for CopyPathTool {
    type Input = CopyPathToolInput;
    type Output = String;

    const NAME: &'static str = "copy_path";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Move
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> ui::SharedString {
        if let Ok(input) = input {
            let src = MarkdownInlineCode(&input.source_path);
            let dest = MarkdownInlineCode(&input.destination_path);
            format!("Copy {src} to {dest}").into()
        } else {
            "Copy path".into()
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
            let input = input.recv().await.map_err(|e| e.to_string())?;
            let decision = cx.update(|cx| {
                let forms = [
                    permission_path_forms(project.read(cx), Path::new(&input.source_path), cx),
                    permission_path_forms(project.read(cx), Path::new(&input.destination_path), cx),
                ];
                decide_permission_for_path_groups(Self::NAME, &forms, AgentSettings::get_global(cx))
            });
            if let ToolPermissionDecision::Deny(reason) = decision {
                return Err(reason);
            }

            let fs = project.read_with(cx, |project, _cx| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            // A bare relative path into a gitignored directory the worktree
            // hasn't scanned can't resolve through the snapshot; scan that
            // directory first so it resolves like the rooted/absolute form.
            prescan_ignored_ancestor(&project, Path::new(&input.source_path), cx).await;
            prescan_ignored_ancestor(&project, Path::new(&input.destination_path), cx).await;

            let direct_source = resolve_direct_fs_path(
                Path::new(&input.source_path),
                PathExistence::MustExist,
                &project,
                &canonical_roots,
                fs.as_ref(),
                cx,
            )
            .await;
            let direct_destination = resolve_direct_fs_path(
                Path::new(&input.destination_path),
                PathExistence::MayNotExist,
                &project,
                &canonical_roots,
                fs.as_ref(),
                cx,
            )
            .await;
            let external_source_path = direct_source
                .as_ref()
                .map(|direct_path| direct_path.absolute_path.clone());
            let external_destination_path = direct_destination
                .as_ref()
                .map(|direct_path| direct_path.absolute_path.clone());

            if external_source_path
                .as_ref()
                .is_some_and(|path| is_protected_external_path(path, &canonical_roots))
                || external_destination_path
                    .as_ref()
                    .is_some_and(|path| is_protected_external_path(path, &canonical_roots))
            {
                return Err(format!(
                    "Refusing to copy to or from a protected path outside the project: {} -> {}",
                    input.source_path, input.destination_path
                ));
            }

            // `file_scan_exclusions` / `private_files` are hard blocks, so check
            // this operation before prompting: there is no point asking the user to
            // approve something that can't run. A target that resolves somewhere
            // other than the requested path (a symlink) asks the user too.
            for (direct_path, requested, subject) in [
                (direct_source.as_ref(), &input.source_path, "copy from"),
                (
                    direct_destination.as_ref(),
                    &input.destination_path,
                    "copy to",
                ),
            ] {
                let Some(direct_path) = direct_path else {
                    continue;
                };
                authorize_direct_fs_path(
                    Self::NAME,
                    requested,
                    subject,
                    direct_path,
                    true,
                    &event_stream,
                    cx,
                )
                .await?;
            }
            if let Some(source) = project.read_with(cx, |project, cx| {
                project
                    .find_project_path(&input.source_path, cx)
                    .filter(|_| external_source_path.is_none())
            }) {
                cx.update(|cx| ensure_path_not_hidden_by_settings(&source, "copy from", cx))
                    .map_err(|error| format!("{error:#}"))?;
            }
            if let Some(destination) = project.read_with(cx, |project, cx| {
                project
                    .find_project_path(&input.destination_path, cx)
                    .filter(|_| external_destination_path.is_none())
            }) {
                cx.update(|cx| ensure_path_not_hidden_by_settings(&destination, "copy to", cx))
                    .map_err(|error| format!("{error:#}"))?;
            }

            let symlink_escapes: Vec<(&str, std::path::PathBuf)> =
                project.read_with(cx, |project, cx| {
                    collect_symlink_escapes(
                        project,
                        &input.source_path,
                        &input.destination_path,
                        &canonical_roots,
                        cx,
                    )
                });

            let sensitive_kind = sensitive_settings_kind(
                Path::new(&input.source_path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await
            .or(sensitive_settings_kind(
                Path::new(&input.destination_path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await);

            let needs_confirmation = matches!(decision, ToolPermissionDecision::Confirm)
                || (matches!(decision, ToolPermissionDecision::Allow) && sensitive_kind.is_some());

            let authorize = if !symlink_escapes.is_empty() {
                // Symlink escape authorization replaces (rather than supplements)
                // the normal tool-permission prompt. The symlink prompt already
                // requires explicit user approval with the canonical target shown,
                // which is strictly more security-relevant than a generic confirm.
                Some(cx.update(|cx| {
                    authorize_symlink_escapes(Self::NAME, &symlink_escapes, &event_stream, cx)
                }))
            } else if needs_confirmation {
                Some(cx.update(|cx| {
                    let src = MarkdownInlineCode(&input.source_path);
                    let dest = MarkdownInlineCode(&input.destination_path);
                    let context = crate::ToolPermissionContext::new(
                        Self::NAME,
                        vec![input.source_path.clone(), input.destination_path.clone()],
                    );
                    let title = format!("Copy {src} to {dest}");
                    authorize_with_sensitive_settings(
                        sensitive_kind,
                        context,
                        &title,
                        &event_stream,
                        cx,
                    )
                }))
            } else {
                None
            };

            if let Some(authorize) = authorize {
                authorize.await.map_err(|e| e.to_string())?;
            }

            if external_source_path.is_some() || external_destination_path.is_some() {
                let source_path = if let Some(external_source_path) = external_source_path {
                    external_source_path
                } else {
                    project.read_with(cx, |project, cx| {
                        let project_path = project
                            .find_project_path(&input.source_path, cx)
                            .ok_or_else(|| {
    explain_unresolved_path(project, Path::new(&input.source_path), cx).unwrap_or_else(|| {
        format!(
            "Source path {} was not found in the project.",
            input.source_path
        )
    })
})?;
                        project.entry_for_path(&project_path, cx).ok_or_else(|| {
                            format!("Source path {} was not found in the project.", input.source_path)
                        })?;
                        project.absolute_path(&project_path, cx).ok_or_else(|| {
                            format!("Source path {} could not be resolved.", input.source_path)
                        })
                    })?
                };

                let destination_path = if let Some(external_destination_path) =
                    external_destination_path
                {
                    external_destination_path
                } else {
                    project.read_with(cx, |project, cx| {
                        let project_path = project.find_project_path(&input.destination_path, cx).ok_or_else(|| {
                            format!(
                                "Destination path {} was outside the project.",
                                input.destination_path
                            )
                        })?;
                        project.absolute_path(&project_path, cx).ok_or_else(|| {
                            format!(
                                "Destination path {} could not be resolved.",
                                input.destination_path
                            )
                        })
                    })?
                };

                futures::select! {
                    result = fs::copy_recursive(
                        fs.as_ref(),
                        &source_path,
                        &destination_path,
                        fs::CopyOptions::default(),
                    ).fuse() => {
                        result.map_err(|e| format!("Copying {} to {}: {e}", input.source_path, input.destination_path))?;
                    }
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Copy cancelled by user".to_string());
                    }
                }

                return Ok(format!(
                    "Copied {} to {}",
                    input.source_path, input.destination_path
                ));
            }

            let copy_task = project.update(cx, |project, cx| {
                match project
                    .find_project_path(&input.source_path, cx)
                    .and_then(|project_path| project.entry_for_path(&project_path, cx))
                {
                    Some(entity) => match project.find_project_path(&input.destination_path, cx) {
                        Some(project_path) => Ok(project.copy_entry(entity.id, project_path, cx)),
                        None => Err(format!(
                            "Destination path {} was outside the project.",
                            input.destination_path
                        )),
                    },
                    None => Err(format!(
                        "Source path {} was not found in the project.",
                        input.source_path
                    )),
                }
            })?;

            let result = futures::select! {
                result = copy_task.fuse() => result,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Copy cancelled by user".to_string());
                }
            };
            result.map_err(|e| {
                format!(
                    "Copying {} to {}: {e}",
                    input.source_path, input.destination_path
                )
            })?;
            Ok(format!(
                "Copied {} to {}",
                input.source_path, input.destination_path
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::{TestAppContext, UpdateGlobal as _};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::{SettingsStore, SplicingVec};
    use std::path::PathBuf;
    use util::path;

    /// Installs user-level `worktree` settings, which both the in-project and the
    /// out-of-project settings checks read via `WorktreeSettings::get_global`.
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

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Allow;
            AgentSettings::override_global(settings, cx);
        });
    }

    #[gpui::test]
    async fn test_copy_path_global_skill_directory_to_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skill_dir = agent_skills::global_skills_dir().join("my-skill");
        fs.insert_tree(&skill_dir, json!({ "SKILL.md": "content" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: input_path,
                    destination_path: path!("/root/project/my-skill").to_string(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("agent skills"),
            "Authorization title should mention agent skills, got: {title}",
        );
        assert!(
            auth.options
                .first_option_of_kind(acp::PermissionOptionKind::AllowAlways)
                .is_none(),
            "agent skills prompt must not offer an \"Always allow\" option: {:?}",
            auth.options,
        );
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should copy after approval: {result:?}");
        assert!(fs.is_dir(&skill_dir).await);
        assert_eq!(
            fs.load(path!("/root/project/my-skill/SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    #[gpui::test]
    async fn test_copy_path_project_directory_to_global_skill_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root/project"),
            json!({ "exported-skill": { "SKILL.md": "content" } }),
        )
        .await;
        let skills_dir = agent_skills::global_skills_dir();
        fs.create_dir(&skills_dir).await.unwrap();
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let destination_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("exported-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: path!("/root/project/exported-skill").to_string(),
                    destination_path,
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("agent skills"),
            "Authorization title should mention agent skills, got: {title}",
        );
        assert!(
            auth.options
                .first_option_of_kind(acp::PermissionOptionKind::AllowAlways)
                .is_none(),
            "agent skills prompt must not offer an \"Always allow\" option: {:?}",
            auth.options,
        );
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should copy after approval: {result:?}");
        assert!(
            fs.is_dir(path!("/root/project/exported-skill").as_ref())
                .await
        );
        assert_eq!(
            fs.load(skills_dir.join("exported-skill").join("SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    /// Copying out of (and into) a directory the worktree never scanned goes
    /// through the same direct-filesystem route as a path outside the project.
    #[gpui::test]
    async fn test_copy_path_gitignored_file_in_unscanned_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": { "main.rs": "fn main() {}" },
                "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
            }),
        )
        .await;
        fs.create_dir(path!("/outside").as_ref()).await.unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let result = cx
            .update(|cx| {
                tool.clone().run(
                    ToolInput::resolved(CopyPathToolInput {
                        source_path: "node_modules/pkg/index.js".to_string(),
                        destination_path: path!("/outside/copied.js").to_string(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(
            result.is_ok(),
            "copying out of a gitignored directory should succeed: {result:?}"
        );
        assert_eq!(
            fs.load(path!("/outside/copied.js").as_ref()).await.unwrap(),
            "module.exports = 1;"
        );
        assert!(
            fs.is_file(path!("/root/node_modules/pkg/index.js").as_ref())
                .await,
            "the source must be left in place"
        );
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_source_requests_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("points outside the project")
                || title.contains("symlinks outside project"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;
        assert!(result.is_ok(), "should succeed after approval: {result:?}");
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_denied(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        drop(auth);

        let result = task.await;
        assert!(result.is_err(), "should fail when denied");
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_confirm_requires_single_approval(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.default = settings::ToolPermissionMode::Confirm;
            AgentSettings::override_global(settings, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("points outside the project")
                || title.contains("symlinks outside project"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Expected a single authorization prompt",
        );

        let result = task.await;
        assert!(
            result.is_ok(),
            "Tool should succeed after one authorization: {result:?}"
        );
    }

    #[gpui::test]
    async fn test_copy_path_symlink_escape_honors_deny_policy(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(
                "copy_path".into(),
                agent_settings::ToolRules {
                    default: Some(settings::ToolPermissionMode::Deny),
                    ..Default::default()
                },
            );
            AgentSettings::override_global(settings, cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "file.txt": "content" }
                },
                "external": {
                    "secret.txt": "SECRET"
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/project/link_to_external").as_ref(),
            PathBuf::from("../external"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));

        let input = CopyPathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_copy".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
            .await;

        assert!(result.is_err(), "Tool should fail when policy denies");
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Deny policy should not emit symlink authorization prompt",
        );
    }

    #[gpui::test]
    async fn test_copy_path_external_source_to_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({ "data.json": "outside" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: path!("/outside/data.json").to_string(),
                    destination_path: "project/copied.json".to_string(),
                }),
                ToolCallEventStream::test().0,
                cx,
            )
        });
        let result = task.await;
        assert!(
            result.is_ok(),
            "should copy from an absolute out-of-project path: {result:?}"
        );
        assert_eq!(
            fs.load(&PathBuf::from(path!("/root/project/copied.json")))
                .await
                .unwrap(),
            "outside"
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/outside/data.json")))
                .await,
            "copying out of the project must not consume the source"
        );
    }

    #[gpui::test]
    async fn test_copy_path_respects_private_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "secret.txt": "secret", "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.private_files =
                Some(vec!["**/secret.txt".to_string()].into());
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: "project/secret.txt".to_string(),
                    destination_path: "project/copy.txt".to_string(),
                }),
                event_stream,
                cx,
            )
        });
        let result = task.await;

        let error = result.unwrap_err();
        assert!(
            error.contains("private_files"),
            "error should name the blocking setting, got: {error}"
        );
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "a settings-blocked copy must not prompt for authorization",
        );
        assert!(
            fs.metadata(&PathBuf::from(path!("/root/project/copy.txt")))
                .await
                .unwrap()
                .is_none(),
            "nothing should be copied when the source is private",
        );
    }

    #[gpui::test]
    async fn test_copy_path_respects_file_scan_exclusions_on_destination(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.file_scan_exclusions =
                Some(SplicingVec::from(vec!["**/generated".to_string()]));
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: "project/src/main.rs".to_string(),
                    destination_path: "project/generated/main.rs".to_string(),
                }),
                event_stream,
                cx,
            )
        });
        let result = task.await;

        let error = result.unwrap_err();
        assert!(
            error.contains("file_scan_exclusions"),
            "error should name the blocking setting, got: {error}"
        );
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "a settings-blocked copy must not prompt for authorization",
        );
        assert!(
            !fs.is_dir(&PathBuf::from(path!("/root/project/generated")))
                .await,
            "nothing should be created when the destination is excluded",
        );
    }

    #[gpui::test]
    async fn test_copy_path_respects_private_files_outside_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({ "secret.txt": "outside secret" }))
            .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.private_files =
                Some(vec!["**/secret.txt".to_string()].into());
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CopyPathTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CopyPathToolInput {
                    source_path: "project/src/main.rs".to_string(),
                    destination_path: path!("/outside/secret.txt").to_string(),
                }),
                event_stream,
                cx,
            )
        });
        let result = task.await;

        let error = result.unwrap_err();
        assert!(
            error.contains("private_files"),
            "error should name the blocking setting, got: {error}"
        );
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "a settings-blocked copy must not prompt for authorization",
        );
        assert_eq!(
            fs.load(&PathBuf::from(path!("/outside/secret.txt")))
                .await
                .unwrap(),
            "outside secret",
            "a blocked out-of-project destination must be left untouched",
        );
    }
}
