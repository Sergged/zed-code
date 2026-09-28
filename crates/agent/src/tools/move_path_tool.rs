use super::ignored_scan::prescan_ignored_ancestor;
use super::tool_permissions::{
    DirectFsPath, PathExistence, authorize_direct_fs_path, authorize_symlink_escapes,
    canonicalize_worktree_roots, collect_symlink_escapes, ensure_path_not_hidden_by_settings,
    explain_unresolved_path, explain_unscanned_ignored_path, is_protected_external_path,
    permission_path_forms, resolve_direct_fs_path, resolves_to_global_skills_dir,
    sensitive_settings_kind,
};
use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    authorize_with_sensitive_settings, decide_permission_for_path_groups,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use futures::FutureExt as _;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::{path::Path, sync::Arc};
use util::markdown::MarkdownInlineCode;

/// Moves or renames a file or directory, and returns confirmation that the move succeeded.
///
/// If the source and destination directories are the same, but the filename is different, this performs a rename. Otherwise, it performs a move.
///
/// This tool should be used when it's desirable to move or rename a file or directory without changing its contents at all.
/// Project-relative paths that start with a project root directory always resolve; bare project-relative paths also work when unambiguous. Absolute paths are accepted too, and every form is subject to the user's agent tool permission rules.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MovePathToolInput {
    /// The source path of the file or directory to move/rename.
    ///
    /// <example>
    /// If the project has the following files:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can move the first file by providing a source_path of "directory1/a/something.txt"
    /// </example>
    pub source_path: String,

    /// The destination path where the file or directory should be moved/renamed to.
    /// If the paths are the same except for the filename, then this will be a rename.
    ///
    /// <example>
    /// To move "directory1/a/something.txt" to "directory2/b/renamed.txt",
    /// provide a destination_path of "directory2/b/renamed.txt"
    /// </example>
    pub destination_path: String,
}

pub struct MovePathTool {
    project: Entity<Project>,
}

impl MovePathTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for MovePathTool {
    type Input = MovePathToolInput;
    type Output = String;

    const NAME: &'static str = "move_path";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Move
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            let src = MarkdownInlineCode(&input.source_path);
            let dest = MarkdownInlineCode(&input.destination_path);
            let src_path = Path::new(&input.source_path);
            let dest_path = Path::new(&input.destination_path);

            match dest_path
                .file_name()
                .and_then(|os_str| os_str.to_os_string().into_string().ok())
            {
                Some(filename) if src_path.parent() == dest_path.parent() => {
                    let filename = MarkdownInlineCode(&filename);
                    format!("Rename {src} to {filename}").into()
                }
                _ => format!("Move {src} to {dest}").into(),
            }
        } else {
            "Move path".into()
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
            let input = input
                .recv()
                .await
                .map_err(|e| e.to_string())?;
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

            if resolves_to_global_skills_dir(Path::new(&input.source_path), fs.as_ref()).await
                || resolves_to_global_skills_dir(
                    Path::new(&input.destination_path),
                    fs.as_ref(),
                )
                .await
            {
                return Err(
                    "Cannot move the global agent skills directory itself. Move a skill directory or file beneath it instead."
                        .to_string(),
                );
            }

            let mut direct_source = resolve_direct_fs_path(
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

            // A symlink names the link, not its target: move the link itself,
            // the way `mv` does and the way an in-project source already behaves.
            // Only the leaf matters — an intermediate symlink is still followed.
            // The escape prompt still fires (as it does for a scanned in-project
            // symlink): the path does resolve outside the project.
            let move_leaf_symlink = match &direct_source {
                Some(direct) => fs
                    .metadata(&direct.named_path)
                    .await
                    .map_err(|e| format!("Moving {}: {e}", input.source_path))?
                    .is_some_and(|metadata| metadata.is_symlink),
                None => false,
            };
            if move_leaf_symlink && let Some(direct) = &direct_source {
                direct_source = Some(DirectFsPath {
                    absolute_path: direct.named_path.clone(),
                    named_path: direct.named_path.clone(),
                    project_path: direct.project_path.clone(),
                    symlink_escape: direct.symlink_escape.clone(),
                });
            }

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
                    "Refusing to move to or from a protected path outside the project: {} -> {}",
                    input.source_path, input.destination_path
                ));
            }

            // `file_scan_exclusions` / `private_files` are hard blocks, so check
            // this operation before prompting: there is no point asking the user to
            // approve something that can't run. A target that resolves somewhere
            // other than the requested path (a symlink) asks the user too.
            for (direct_path, requested, subject) in [
                (direct_source.as_ref(), &input.source_path, "move from"),
                (
                    direct_destination.as_ref(),
                    &input.destination_path,
                    "move to",
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

            // `file_scan_exclusions` / `private_files` are hard blocks, so check
            // the in-project side of this operation before prompting: there is no
            // point asking the user to approve something that can't run.
            if let Some(source) = project.read_with(cx, |project, cx| {
                project
                    .find_project_path(&input.source_path, cx)
                    .filter(|_| external_source_path.is_none())
            }) {
                cx.update(|cx| ensure_path_not_hidden_by_settings(&source, "move from", cx))
                    .map_err(|error| format!("{error:#}"))?;
            }
            if let Some(destination) = project.read_with(cx, |project, cx| {
                project
                    .find_project_path(&input.destination_path, cx)
                    .filter(|_| external_destination_path.is_none())
            }) {
                cx.update(|cx| ensure_path_not_hidden_by_settings(&destination, "move to", cx))
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
                    let title = format!("Move {src} to {dest}");
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
                            explain_unscanned_ignored_path(project, &project_path, cx)
                                .unwrap_or_else(|| {
                                    format!(
                                        "Source path {} was not found in the project.",
                                        input.source_path
                                    )
                                })
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
                    result = fs.rename(
                        &source_path,
                        &destination_path,
                        fs::RenameOptions {
                            create_parents: true,
                            ..fs::RenameOptions::default()
                        },
                    ).fuse() => {
                        result.map_err(|e| format!("Moving {} to {}: {e}", input.source_path, input.destination_path))?;
                    }
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Move cancelled by user".to_string());
                    }
                }

                return Ok(format!(
                    "Moved {} to {}",
                    input.source_path, input.destination_path
                ));
            }

            let rename_task = project.update(cx, |project, cx| {
                match project
                    .find_project_path(&input.source_path, cx)
                    .and_then(|project_path| project.entry_for_path(&project_path, cx))
                {
                    Some(entity) => match project.find_project_path(&input.destination_path, cx) {
                        Some(project_path) => Ok(project.rename_entry(entity.id, project_path, cx)),
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

            futures::select! {
                result = rename_task.fuse() => result.map_err(|e| format!("Moving {} to {}: {e}", input.source_path, input.destination_path))?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Move cancelled by user".to_string());
                }
            };
            Ok(format!(
                "Moved {} to {}",
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
    async fn test_move_path_global_skill_directory_to_project(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let skill_dir = agent_skills::global_skills_dir().join("my-skill");
        fs.insert_tree(&skill_dir, json!({ "SKILL.md": "content" }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .to_string_lossy()
            .into_owned();
        let destination_path = path!("/root/project/my-skill").to_string();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(MovePathToolInput {
                    source_path: input_path,
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
        assert!(result.is_ok(), "should move after approval: {result:?}");
        assert!(!fs.is_dir(&skill_dir).await);
        assert_eq!(
            fs.load(path!("/root/project/my-skill/SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    #[gpui::test]
    async fn test_move_path_project_directory_to_global_skill_directory(cx: &mut TestAppContext) {
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

        let tool = Arc::new(MovePathTool::new(project));
        let destination_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("exported-skill")
            .to_string_lossy()
            .into_owned();

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(MovePathToolInput {
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
        assert!(result.is_ok(), "should move after approval: {result:?}");
        assert!(
            !fs.is_dir(path!("/root/project/exported-skill").as_ref())
                .await
        );
        assert_eq!(
            fs.load(skills_dir.join("exported-skill").join("SKILL.md").as_ref())
                .await
                .unwrap(),
            "content"
        );
    }

    /// Moving out of a directory the worktree never scanned goes through the
    /// same direct-filesystem route as a path outside the project.
    #[gpui::test]
    async fn test_move_path_gitignored_file_in_unscanned_directory(cx: &mut TestAppContext) {
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

        let tool = Arc::new(MovePathTool::new(project));
        let result = cx
            .update(|cx| {
                tool.clone().run(
                    ToolInput::resolved(MovePathToolInput {
                        source_path: "node_modules/pkg/index.js".to_string(),
                        destination_path: path!("/outside/moved.js").to_string(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(
            result.is_ok(),
            "moving out of a gitignored directory should succeed: {result:?}"
        );
        assert!(
            !fs.is_file(path!("/root/node_modules/pkg/index.js").as_ref())
                .await,
            "the source should be gone"
        );
        assert_eq!(
            fs.load(path!("/outside/moved.js").as_ref()).await.unwrap(),
            "module.exports = 1;"
        );
    }

    #[gpui::test]
    async fn test_move_path_symlink_escape_source_requests_authorization(cx: &mut TestAppContext) {
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

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
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
    async fn test_move_path_symlink_escape_denied(cx: &mut TestAppContext) {
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

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
        };

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx));

        let auth = event_rx.expect_authorization().await;
        drop(auth);

        let result = task.await;
        assert!(result.is_err(), "should fail when denied");
    }

    #[gpui::test]
    async fn test_move_path_symlink_escape_confirm_requires_single_approval(
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

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
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
    async fn test_move_path_symlink_escape_honors_deny_policy(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(
                "move_path".into(),
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

        let tool = Arc::new(MovePathTool::new(project));

        let input = MovePathToolInput {
            source_path: "project/link_to_external".into(),
            destination_path: "project/external_moved".into(),
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
    async fn test_move_path_external_source_to_project(cx: &mut TestAppContext) {
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

        let tool = Arc::new(MovePathTool::new(project));
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(MovePathToolInput {
                    source_path: path!("/outside/data.json").to_string(),
                    destination_path: "project/data.json".to_string(),
                }),
                ToolCallEventStream::test().0,
                cx,
            )
        });
        let result = task.await;
        assert!(
            result.is_ok(),
            "should move from an absolute out-of-project path: {result:?}"
        );
        assert_eq!(
            fs.load(&PathBuf::from(path!("/root/project/data.json")))
                .await
                .unwrap(),
            "outside"
        );
        assert!(
            fs.metadata(&PathBuf::from(path!("/outside/data.json")))
                .await
                .unwrap()
                .is_none(),
            "a successful move consumes the source"
        );
    }

    #[gpui::test]
    async fn test_move_path_respects_file_scan_exclusions(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "generated": { "x.txt": "x" } } }),
        )
        .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.file_scan_exclusions =
                Some(SplicingVec::from(vec!["**/generated".to_string()]));
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(MovePathTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(MovePathToolInput {
                    source_path: "project/generated/x.txt".to_string(),
                    destination_path: "project/x.txt".to_string(),
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
            "a settings-blocked move must not prompt for authorization",
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/root/project/generated/x.txt")))
                .await,
            "a blocked move must leave the source in place",
        );
    }

    /// A symlink is moved as a link, not as its target: the named path is what
    /// is renamed, and whatever it pointed at stays put.
    #[gpui::test]
    async fn test_move_path_external_symlink_moves_link_not_target(cx: &mut TestAppContext) {
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

        let tool = Arc::new(MovePathTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.clone().run(
                ToolInput::resolved(MovePathToolInput {
                    source_path: "/outside/link.txt".into(),
                    destination_path: "/outside/moved.txt".into(),
                }),
                event_stream,
                cx,
            )
        });

        // The source link resolves outside the project, so the escape prompt
        // fires; approving it moves the link, not its target.
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
            "moving an external symlink should succeed: {result:?}"
        );
        assert!(
            !fs.is_file(&PathBuf::from(path!("/outside/link.txt"))).await,
            "the original link must be gone"
        );
        assert!(
            fs.read_link(&PathBuf::from(path!("/outside/moved.txt")))
                .await
                .is_ok(),
            "the destination must be the moved symlink"
        );
        assert!(
            fs.is_file(&PathBuf::from(path!("/outside/target.txt")))
                .await,
            "the symlink target must be left untouched"
        );
    }
}
