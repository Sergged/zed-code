use super::ignored_scan::prescan_ignored_ancestor;
use super::tool_permissions::{
    DirectFsPath, PathExistence, authorize_direct_fs_path, authorize_symlink_access,
    canonicalize_worktree_roots, detect_symlink_escape, ensure_path_not_hidden_by_settings,
    explain_unresolved_path, explain_unscanned_ignored_path, is_protected_external_path,
    permission_path_forms, resolve_direct_fs_path, resolves_to_global_skills_dir,
    sensitive_settings_kind,
};
use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    authorize_with_sensitive_settings, decide_permission_for_path_forms,
};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use futures::{FutureExt as _, SinkExt, StreamExt, channel::mpsc};
use gpui::{App, AppContext, Entity, SharedString, Task};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::path::Path;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;

/// Deletes the file or directory (and the directory's contents, recursively) at the specified path, and returns confirmation of the deletion.
///
/// A project-relative path that starts with a project root directory always resolves; a bare project-relative path also works when it is unambiguous. Absolute paths are accepted too, and every form is subject to the user's agent tool permission rules.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DeletePathToolInput {
    /// The path of the file or directory to delete.
    ///
    /// <example>
    /// If the project has the following files:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can delete the first file by providing a path of "directory1/a/something.txt"
    /// </example>
    pub path: String,
}

pub struct DeletePathTool {
    project: Entity<Project>,
    action_log: Entity<ActionLog>,
}

impl DeletePathTool {
    pub fn new(project: Entity<Project>, action_log: Entity<ActionLog>) -> Self {
        Self {
            project,
            action_log,
        }
    }
}

impl AgentTool for DeletePathTool {
    type Input = DeletePathToolInput;
    type Output = String;

    const NAME: &'static str = "delete_path";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Delete
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Delete “`{}`”", input.path).into()
        } else {
            "Delete path".into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        let action_log = self.action_log.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;
            let path = input.path;

            let decision = cx.update(|cx| {
                let forms = permission_path_forms(project.read(cx), Path::new(&path), cx);
                decide_permission_for_path_forms(
                    Self::NAME,
                    &forms,
                    AgentSettings::get_global(cx),
                )
            });

            if let ToolPermissionDecision::Deny(reason) = decision {
                return Err(reason);
            }

            let fs = project.read_with(cx, |project, _cx| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            // A bare relative path into a gitignored directory the worktree
            // hasn't scanned can't resolve through the snapshot; scan that
            // directory first so it resolves like the rooted/absolute form.
            prescan_ignored_ancestor(&project, Path::new(&path), cx).await;

            if resolves_to_global_skills_dir(Path::new(&path), fs.as_ref()).await {
                return Err(
                    "Cannot delete the global agent skills directory itself. Delete a skill directory or file beneath it instead."
                        .to_string(),
                );
            }

            let mut direct_path = resolve_direct_fs_path(
                Path::new(&path),
                PathExistence::MustExist,
                &project,
                &canonical_roots,
                fs.as_ref(),
                cx,
            )
            .await;

            // A symlink names the link, not its target: delete the link itself,
            // the way `rm` does and the way an in-project path already behaves.
            // Only the leaf matters — an intermediate symlink is still followed.
            // The escape prompt still fires (as it does for a scanned in-project
            // symlink): the path does resolve outside the project.
            let unlink_leaf_symlink = match &direct_path {
                Some(direct) => fs
                    .metadata(&direct.named_path)
                    .await
                    .map_err(|e| format!("Deleting {path}: {e}"))?
                    .is_some_and(|metadata| metadata.is_symlink),
                None => false,
            };
            if unlink_leaf_symlink && let Some(direct) = &direct_path {
                direct_path = Some(DirectFsPath {
                    absolute_path: direct.named_path.clone(),
                    named_path: direct.named_path.clone(),
                    project_path: direct.project_path.clone(),
                    symlink_escape: direct.symlink_escape.clone(),
                });
            }

            let external_path = direct_path
                .as_ref()
                .map(|direct_path| direct_path.absolute_path.clone());
            if let Some(external_path) = &external_path
                && is_protected_external_path(external_path, &canonical_roots)
            {
                return Err(format!(
                    "Refusing to delete a protected path outside the project: {path}"
                ));
            }

            // The user's `file_scan_exclusions` / `private_files` settings apply on
            // every route, and are checked before prompting so a blocked delete
            // never asks for approval. A target that resolves somewhere other than
            // the requested path (a symlink) also asks the user first.
            if let Some(direct_path) = &direct_path {
                authorize_direct_fs_path(
                    Self::NAME,
                    &path,
                    "delete",
                    direct_path,
                    true,
                    &event_stream,
                    cx,
                )
                .await?;
            }

            // `file_scan_exclusions` / `private_files` are hard blocks, so reject
            // before prompting: there is no point asking the user to approve a
            // deletion that can't run.
            let in_project_path = if external_path.is_some() {
                None
            } else {
                let project_path = project.read_with(cx, |project, cx| {
                    project.find_project_path(&path, cx).ok_or_else(|| {
                        explain_unresolved_path(project, Path::new(&path), cx).unwrap_or_else(
                            || {
                                format!(
                                    "Couldn't delete {path} because that path isn't in this project."
                                )
                            },
                        )
                    })
                })?;
                cx.update(|cx| ensure_path_not_hidden_by_settings(&project_path, "delete", cx))
                    .map_err(|error| format!("{error:#}"))?;
                Some(project_path)
            };

            let symlink_escape_target = project.read_with(cx, |project, cx| {
                detect_symlink_escape(project, &path, &canonical_roots, cx)
                    .map(|(_, target)| target)
            });

            let settings_kind =
                sensitive_settings_kind(Path::new(&path), &canonical_roots, fs.as_ref()).await;

            let decision =
                if matches!(decision, ToolPermissionDecision::Allow) && settings_kind.is_some() {
                    ToolPermissionDecision::Confirm
                } else {
                    decision
                };

            let authorize = if let Some(canonical_target) = symlink_escape_target {
                // Symlink escape authorization replaces (rather than supplements)
                // the normal tool-permission prompt. The symlink prompt already
                // requires explicit user approval with the canonical target shown,
                // which is strictly more security-relevant than a generic confirm.
                Some(cx.update(|cx| {
                    authorize_symlink_access(
                        Self::NAME,
                        &path,
                        &canonical_target,
                        &event_stream,
                        cx,
                    )
                }))
            } else {
                match decision {
                    ToolPermissionDecision::Allow => None,
                    ToolPermissionDecision::Confirm => Some(cx.update(|cx| {
                        let context =
                            crate::ToolPermissionContext::new(Self::NAME, vec![path.clone()]);
                        let title = format!("Delete {}", MarkdownInlineCode(&path));
                        authorize_with_sensitive_settings(
                            settings_kind,
                            context,
                            &title,
                            &event_stream,
                            cx,
                        )
                    })),
                    ToolPermissionDecision::Deny(_) => None,
                }
            };

            if let Some(authorize) = authorize {
                authorize.await.map_err(|e| e.to_string())?;
            }

            if let Some(external_path) = external_path {
                // A leaf symlink is unlinked as-is, so it needs no metadata; the
                // target's metadata decides only for a real file or directory.
                let metadata = if unlink_leaf_symlink {
                    None
                } else {
                    Some(
                        fs.metadata(&external_path)
                            .await
                            .map_err(|e| format!("Deleting {path}: {e}"))?
                            .ok_or_else(|| format!("Deleting {path}: path not found"))?,
                    )
                };

                futures::select! {
                    result = async {
                        match metadata {
                            // A symlink names the link, not its target.
                            None => {
                                fs.remove_file(&external_path, fs::RemoveOptions::default())
                                    .await
                            }
                            Some(metadata) if metadata.is_dir => {
                                fs.remove_dir(
                                    &external_path,
                                    fs::RemoveOptions {
                                        recursive: true,
                                        ..fs::RemoveOptions::default()
                                    },
                                )
                                .await
                            }
                            Some(_) => {
                                fs.remove_file(&external_path, fs::RemoveOptions::default())
                                    .await
                            }
                        }
                    }.fuse() => {
                        result.map_err(|e| format!("Deleting {path}: {e}"))?;
                    }
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Delete cancelled by user".to_string());
                    }
                }

                return Ok(format!("Deleted {path}"));
            }

            let (project_path, worktree_snapshot) = project.read_with(cx, |project, cx| {
                let project_path = in_project_path.ok_or_else(|| {
                    format!("Couldn't delete {path} because that path isn't in this project.")
                })?;
                let worktree = project
                    .worktree_for_id(project_path.worktree_id, cx)
                    .ok_or_else(|| {
                        format!("Couldn't delete {path} because that path isn't in this project.")
                    })?;
                let worktree_snapshot = worktree.read(cx).snapshot();
                if project.entry_for_path(&project_path, cx).is_none() {
                    return Err(explain_unscanned_ignored_path(project, &project_path, cx)
                        .unwrap_or_else(|| {
                            format!("Couldn't delete {path} because that path isn't in this project.")
                        }));
                }
                Result::<_, String>::Ok((project_path, worktree_snapshot))
            })?;

            let (mut paths_tx, mut paths_rx) = mpsc::channel(256);
            cx.background_spawn({
                let project_path = project_path.clone();
                async move {
                    for entry in
                        worktree_snapshot.traverse_from_path(true, false, false, &project_path.path)
                    {
                        if !entry.path.starts_with(&project_path.path) {
                            break;
                        }
                        paths_tx
                            .send(ProjectPath {
                                worktree_id: project_path.worktree_id,
                                path: entry.path.clone(),
                            })
                            .await?;
                    }
                    anyhow::Ok(())
                }
            })
            .detach();

            loop {
                let path_result = futures::select! {
                    path = paths_rx.next().fuse() => path,
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Delete cancelled by user".to_string());
                    }
                };
                let Some(path) = path_result else {
                    break;
                };
                if let Ok(buffer) = project
                    .update(cx, |project, cx| project.open_buffer(path, cx))
                    .await
                {
                    action_log.update(cx, |action_log, cx| {
                        action_log.will_delete_buffer(buffer.clone(), cx)
                    });
                }
            }

            let deletion_task = project
                .update(cx, |project, cx| project.delete_file(project_path, cx))
                .ok_or_else(|| {
                    format!("Couldn't delete {path} because that path isn't in this project.")
                })?;

            futures::select! {
                result = deletion_task.fuse() => {
                    result.map_err(|e| format!("Deleting {path}: {e}"))?;
                }
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Delete cancelled by user".to_string());
                }
            }
            Ok(format!("Deleted {path}"))
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

    use crate::ToolCallEventStream;

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
    async fn test_delete_path_global_skill_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skills_dir = agent_skills::global_skills_dir();
        let skill_dir = skills_dir.join("my-skill");
        fs.insert_tree(&skill_dir, json!({ "SKILL.md": "content" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(DeletePathToolInput { path: input_path }),
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
        assert!(result.is_ok(), "should delete after approval: {result:?}");
        assert!(fs.is_dir(&skills_dir).await);
        assert!(!fs.is_dir(&skill_dir).await);
    }

    #[gpui::test]
    async fn test_delete_path_global_skill_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skill_file = agent_skills::global_skills_dir()
            .join("my-skill")
            .join("references")
            .join("notes.md");
        fs.create_dir(skill_file.parent().unwrap()).await.unwrap();
        fs.insert_file(&skill_file, b"notes".to_vec()).await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .join("references")
            .join("notes.md")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(DeletePathToolInput { path: input_path }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .expect("authorization response should send");

        let result = task.await;
        assert!(result.is_ok(), "should delete after approval: {result:?}");
        assert!(!fs.is_file(&skill_file).await);
    }

    #[gpui::test]
    async fn test_delete_path_rejects_global_skills_root(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skills_dir = agent_skills::global_skills_dir();
        fs.create_dir(&skills_dir).await.unwrap();
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput { path: input_path }),
                    event_stream,
                    cx,
                )
            })
            .await;

        assert!(result.is_err(), "should reject deleting skills root");
        assert!(fs.is_dir(&skills_dir).await);
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Deleting the skills root should fail before requesting authorization",
        );
    }

    #[gpui::test]
    async fn test_delete_path_symlink_escape_requests_authorization(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "main.rs": "fn main() {}" }
                },
                "external": {
                    "data": { "file.txt": "content" }
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

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(DeletePathToolInput {
                    path: "project/link_to_external".into(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("points outside the project") || title.contains("symlink"),
            "Authorization title should mention symlink escape, got: {title}",
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;
        // FakeFs cannot delete symlink entries (they are neither Dir nor File
        // internally), so the deletion itself may fail. The important thing is
        // that the authorization was requested and accepted — any error must
        // come from the fs layer, not from a permission denial.
        if let Err(err) = &result {
            let msg = format!("{err:#}");
            assert!(
                !msg.contains("denied") && !msg.contains("authorization"),
                "Error should not be a permission denial, got: {msg}",
            );
        }
    }

    #[gpui::test]
    async fn test_delete_path_symlink_escape_denied(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": { "main.rs": "fn main() {}" }
                },
                "external": {
                    "data": { "file.txt": "content" }
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

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(DeletePathToolInput {
                    path: "project/link_to_external".into(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;

        drop(auth);

        let result = task.await;
        assert!(
            result.is_err(),
            "Tool should fail when authorization is denied"
        );
    }

    #[gpui::test]
    async fn test_delete_path_symlink_escape_confirm_requires_single_approval(
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
                    "src": { "main.rs": "fn main() {}" }
                },
                "external": {
                    "data": { "file.txt": "content" }
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

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(DeletePathToolInput {
                    path: "project/link_to_external".into(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let title = auth.tool_call.fields.title.as_deref().unwrap_or("");
        assert!(
            title.contains("points outside the project") || title.contains("symlink"),
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
        if let Err(err) = &result {
            let message = format!("{err:#}");
            assert!(
                !message.contains("denied") && !message.contains("authorization"),
                "Error should not be a permission denial, got: {message}",
            );
        }
    }

    #[gpui::test]
    async fn test_delete_path_symlink_escape_honors_deny_policy(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(
                "delete_path".into(),
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
                    "src": { "main.rs": "fn main() {}" }
                },
                "external": {
                    "data": { "file.txt": "content" }
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

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: "project/link_to_external".into(),
                    }),
                    event_stream,
                    cx,
                )
            })
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
    async fn test_delete_path_outside_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        fs.create_dir(path!("/outside").as_ref()).await.unwrap();
        fs.insert_file(path!("/outside/doomed.txt"), b"bye".to_vec())
            .await;

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: path!("/outside/doomed.txt").to_string(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(result.is_ok(), "expected success, got {result:?}");
        assert!(
            !fs.is_file(path!("/outside/doomed.txt").as_ref()).await,
            "external file should have been deleted"
        );
    }

    /// A `private_files` match is a hard block: the delete fails, the file
    /// survives, and no authorization prompt is ever emitted.
    #[gpui::test]
    async fn test_delete_path_respects_private_files(cx: &mut TestAppContext) {
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

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: "project/secret.txt".to_string(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

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
            "a settings-blocked delete must not prompt for authorization",
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/root/project/secret.txt")))
                .await,
            "a private file must not be deleted",
        );
    }

    /// Same hard block for `file_scan_exclusions`, on a directory this time.
    #[gpui::test]
    async fn test_delete_path_respects_file_scan_exclusions(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "build": { "output.txt": "artifact" },
                    "src": { "main.rs": "fn main() {}" }
                }
            }),
        )
        .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.file_scan_exclusions =
                Some(SplicingVec::from(vec!["**/build".to_string()]));
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: "project/build".to_string(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

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
            "a settings-blocked delete must not prompt for authorization",
        );
        assert!(
            fs.is_dir(&PathBuf::from(path!("/root/project/build")))
                .await,
            "an excluded directory must not be deleted",
        );
    }

    /// The user's global settings also gate out-of-project deletes, and the check
    /// runs before the prompt so a blocked external delete asks for nothing.
    #[gpui::test]
    async fn test_delete_path_respects_private_files_outside_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root/project"),
            json!({ "src": { "main.rs": "fn main() {}" } }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({ "doomed.txt": "bye" }))
            .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.private_files =
                Some(vec!["**/doomed.txt".to_string()].into());
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: path!("/outside/doomed.txt").to_string(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

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
            "a settings-blocked delete must not prompt for authorization",
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/outside/doomed.txt")))
                .await,
            "a private out-of-project file must not be deleted",
        );
    }

    /// Deleting an ancestor of a worktree would take the project with it, so it
    /// is refused outright — again before any prompt.
    /// A gitignored directory is itself a snapshot entry, so deleting one works.
    #[gpui::test]
    async fn test_delete_path_removes_gitignored_directory(cx: &mut TestAppContext) {
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

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: "root/node_modules".into(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(result.is_ok(), "expected success, got {result:?}");
        assert!(
            !fs.is_dir(&PathBuf::from(path!("/root/node_modules"))).await,
            "the gitignored directory should have been removed"
        );
    }

    /// A file inside a directory the worktree never scanned (gitignored) has no
    /// snapshot entry, so the delete goes through the same direct-filesystem
    /// route as a file outside the project.
    #[gpui::test]
    async fn test_delete_path_removes_gitignored_file(cx: &mut TestAppContext) {
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

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        for path in [
            "root/node_modules/pkg/index.js",
            path!("/root/node_modules/pkg/other.js"),
        ] {
            fs.insert_file(
                path!("/root/node_modules/pkg/other.js"),
                b"module.exports = 2;".to_vec(),
            )
            .await;
            let result = cx
                .update(|cx| {
                    tool.clone().run(
                        ToolInput::resolved(DeletePathToolInput { path: path.into() }),
                        ToolCallEventStream::test().0,
                        cx,
                    )
                })
                .await;

            assert!(result.is_ok(), "deleting {path} should succeed: {result:?}");
        }

        for path in [
            path!("/root/node_modules/pkg/index.js"),
            path!("/root/node_modules/pkg/other.js"),
        ] {
            assert!(
                !fs.is_file(&PathBuf::from(path)).await,
                "{path} should have been removed"
            );
        }
    }

    /// A bare relative path into an unscanned gitignored directory is scanned on
    /// demand, so it deletes the file like the rooted/absolute form.
    #[gpui::test]
    async fn test_delete_path_bare_relative_gitignored_file_scans_and_deletes(
        cx: &mut TestAppContext,
    ) {
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

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));
        let result = cx
            .update(|cx| {
                tool.clone().run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: "node_modules/pkg/index.js".into(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(
            result.is_ok(),
            "the bare form should delete the file: {result:?}"
        );
        assert!(
            !fs.is_file(&PathBuf::from(path!("/root/node_modules/pkg/index.js")))
                .await,
            "the file should have been removed"
        );
    }

    /// The fallback route keeps the user's settings: a gitignored file is still
    /// protected when the worktree settings match it.
    #[gpui::test]
    async fn test_delete_path_gitignored_file_respects_file_scan_exclusions(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.file_scan_exclusions =
                Some(SplicingVec::from(vec!["**/node_modules".to_string()]));
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": { "main.rs": "fn main() {}" },
                "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: "root/node_modules/pkg/index.js".into(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("file_scan_exclusions"),
            "expected the error to name the blocking setting, got: {error}"
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/root/node_modules/pkg/index.js")))
                .await,
            "an excluded file must not be deleted"
        );
    }

    /// Deleting a symlink deletes the link itself, not its target — even when
    /// the link sits in a gitignored directory the scanner skipped, so the path
    /// is resolved directly through the filesystem. The prompt therefore must
    /// not name the outside target: nothing outside the project is touched.
    #[gpui::test]
    async fn test_delete_path_gitignored_symlink_deletes_link_not_target(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                ".gitignore": "node_modules/\n",
                "src": { "main.rs": "fn main() {}" },
                "node_modules": {},
            }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({ "secret.txt": "outside line" }))
            .await;
        fs.create_symlink(
            path!("/root/node_modules/evil.txt").as_ref(),
            PathBuf::from("../../outside/secret.txt"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        // The link resolves outside the project, so the escape prompt fires —
        // but approving it deletes the link, not its target.
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.clone().run(
                ToolInput::resolved(DeletePathToolInput {
                    path: "root/node_modules/evil.txt".into(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;

        assert!(
            result.is_ok(),
            "deleting a symlink should succeed, got: {result:?}"
        );
        assert!(
            !fs.is_file(&PathBuf::from(path!("/root/node_modules/evil.txt")))
                .await,
            "the symlink itself must be removed"
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/outside/secret.txt")))
                .await,
            "the symlink target must be left untouched"
        );
    }

    /// A symlink outside the project is also deleted as a link, not as its
    /// target — the path the model named is the path that disappears.
    #[gpui::test]
    async fn test_delete_path_external_symlink_deletes_link_not_target(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "main.rs": "fn main() {}" } }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({ "target.txt": "outside line" }))
            .await;
        fs.create_symlink(
            path!("/outside/link.txt").as_ref(),
            PathBuf::from("target.txt"),
        )
        .await
        .unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.clone().run(
                ToolInput::resolved(DeletePathToolInput {
                    path: "/outside/link.txt".into(),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new("allow"),
                acp::PermissionOptionKind::AllowOnce,
            ))
            .unwrap();

        let result = task.await;

        assert!(
            result.is_ok(),
            "deleting an external symlink should succeed: {result:?}"
        );
        assert!(
            !fs.is_file(&PathBuf::from(path!("/outside/link.txt"))).await,
            "the symlink itself must be removed"
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/outside/target.txt")))
                .await,
            "the symlink target must be left untouched"
        );
    }

    #[gpui::test]
    async fn test_delete_path_refuses_worktree_ancestor(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": { "src": { "main.rs": "fn main() {}" } },
                "sibling.txt": "sibling"
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let tool = Arc::new(DeletePathTool::new(project, action_log));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(DeletePathToolInput {
                        path: path!("/root").to_string(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

        let error = result.unwrap_err();
        assert!(
            error.contains("protected path"),
            "error should explain the path is protected, got: {error}"
        );
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "a protected path must not prompt for authorization",
        );
        assert!(
            fs.is_dir(&PathBuf::from(path!("/root/project"))).await,
            "the worktree must survive",
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/root/sibling.txt"))).await,
            "a protected ancestor's other contents must survive",
        );
    }
}
