use super::ignored_scan::prescan_ignored_ancestor;
use super::tool_permissions::{
    authorize_symlink_access, canonicalize_worktree_roots, detect_symlink_escape,
    ensure_path_not_hidden_by_settings, explain_unresolved_path,
    external_path_excluded_by_settings, external_path_resolution_target,
    is_protected_external_path, permission_path_forms, resolve_creatable_external_path,
    resolve_creatable_global_skill_path, sensitive_settings_kind,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use futures::FutureExt as _;
use gpui::{App, AppContext as _, AsyncApp, Entity, SharedString, Task};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::Settings;
use std::sync::Arc;
use util::markdown::MarkdownInlineCode;
use util::rel_path::RelPath;

use crate::{
    AgentTool, ToolCallEventStream, ToolInput, ToolPermissionDecision,
    authorize_with_sensitive_settings, decide_permission_for_path_forms,
};
use std::path::{Path, PathBuf};

/// Creates a new directory at the specified path, and all necessary parent directories. Returns confirmation that the directory was created.
///
/// Use this whenever you need to create new directories. Paths inside the project are created directly.
///
#[cfg_attr(
    any(target_os = "linux", target_os = "macos"),
    doc = "This tool can also create a directory **outside** the project. When agent terminal \
    commands are sandboxed, doing so grants those commands write access to exactly that new \
    directory — so, rather than requesting write access to a broad existing parent (e.g. your \
    home directory) just to create something inside it, create the specific directory here \
    first and then write into it. Paths outside the project may be absolute; they are subject \
    to the user's agent tool permission rules."
)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    doc = "Paths outside the project may be absolute; they are subject to the user's agent tool \
    permission rules."
)]
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CreateDirectoryToolInput {
    /// The path of the new directory.
    ///
    /// <example>
    /// If the project has the following structure:
    ///
    /// - directory1/
    /// - directory2/
    ///
    /// You can create a new directory by providing a path of "directory1/new_directory"
    /// </example>
    ///
    /// <example>
    /// Outside the project, any absolute or `~`-prefixed path is accepted — for
    /// example `~/.agents/skills/my-skill` to create a global agent skill directory.
    /// </example>
    pub path: String,

    #[cfg_attr(
        any(target_os = "linux", target_os = "macos"),
        doc = "Justification for creating a directory **outside** the project, shown to the \
        user (attributed to you) in the approval prompt that grants sandboxed terminal \
        commands write access to it. Required only for out-of-project paths; ignored for \
        paths inside the project."
    )]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        doc = "Unused on this platform."
    )]
    #[serde(default)]
    pub reason: Option<String>,
}

pub struct CreateDirectoryTool {
    project: Entity<Project>,
}

impl CreateDirectoryTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for CreateDirectoryTool {
    type Input = CreateDirectoryToolInput;
    type Output = String;

    const NAME: &'static str = "create_directory";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            format!("Create directory {}", MarkdownInlineCode(&input.path)).into()
        } else {
            "Create directory".into()
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

            let fs = project.read_with(cx, |project, _cx| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            // A bare relative path to a not-yet-existing directory resolves via
            // its parent, which may sit in a gitignored directory the worktree
            // hasn't scanned: scan that directory first so the parent resolves.
            prescan_ignored_ancestor(&project, Path::new(&input.path), cx).await;

            // Resolve where this directory lives. The global agent-skills dir is
            // always allowed outside the project; any other absolute path that
            // resolves outside every worktree is treated the same way (created
            // directly through the filesystem, gated by the tool-permission rules).
            let global_skill_directory =
                resolve_creatable_global_skill_path(Path::new(&input.path), fs.as_ref()).await;
            let external_directory = resolve_creatable_external_path(
                Path::new(&input.path),
                &canonical_roots,
                fs.as_ref(),
            )
            .await;
            if let Some(external_directory) = &external_directory
                && is_protected_external_path(external_directory, &canonical_roots)
            {
                return Err(format!(
                    "Refusing to create a directory at a protected path outside the project: {}",
                    input.path
                ));
            }

            // The user's global `file_scan_exclusions` / `private_files` settings
            // apply to out-of-project paths too, as they do in every other file
            // tool, and are checked before prompting.
            if let Some(external_directory) = &external_directory
                && let Some(setting) =
                    cx.update(|cx| external_path_excluded_by_settings(external_directory, cx))
            {
                return Err(format!(
                    "Cannot create a directory because its path matches the user's global `{setting}` setting: {}",
                    input.path
                ));
            }
            let in_project_path = project
                .read_with(cx, |project, cx| -> Result<Option<ProjectPath>, String> {
                    // `file_scan_exclusions` / `private_files` are hard blocks, so
                    // reject before prompting: there is no point asking the user to
                    // approve a directory that can't be created.
                    //
                    // A rooted or absolute path resolves as-is (`find_project_path`
                    // needs no entry for those); a bare relative path needs one, so
                    // for a directory that doesn't exist yet resolve its parent and
                    // append the new name — the same trick `write_file` uses.
                    let project_path = match project.find_project_path(&input.path, cx) {
                        Some(project_path) => Some(project_path),
                        None => resolve_new_directory_parent(project, Path::new(&input.path), cx)?,
                    };
                    if let Some(project_path) = &project_path {
                        ensure_path_not_hidden_by_settings(project_path, "create a directory", cx)
                            .map_err(|error| format!("{error:#}"))?;
                    }
                    Ok(project_path)
                })
                .map_err(|error| format!("{error:#}"))?;
            let in_project = in_project_path.is_some();

            let out_of_project = !in_project && global_skill_directory.is_none();
            let sandboxing = project.read_with(cx, |project, cx| {
                crate::sandboxing::sandboxing_enabled_for_project(project, cx)
            });
            let platform_supported = cfg!(any(target_os = "linux", target_os = "macos"));

            // When agent terminal commands are sandboxed, creating a directory
            // outside the project also grants those commands write access to
            // exactly it, so route through the sandbox-grant flow (which shows the
            // real, canonicalized target and fully replaces the normal prompts).
            // With sandboxing off, the directory is created directly through the
            // filesystem like any other file tool.
            if out_of_project && sandboxing && platform_supported && external_directory.is_some() {
                return create_out_of_project_directory(&project, &input, &event_stream, cx).await;
            }
            if out_of_project && external_directory.is_none() {
                return Err(cx
                    .update(|cx| {
                        explain_unresolved_path(
                            project.read(cx),
                            Path::new(&input.path),
                            cx,
                        )
                    })
                    .unwrap_or_else(|| "Path to create was outside the project".to_string()));
            }

            let decision = cx.update(|cx| {
                let forms = permission_path_forms(project.read(cx), Path::new(&input.path), cx);
                decide_permission_for_path_forms(
                    Self::NAME,
                    &forms,
                    AgentSettings::get_global(cx),
                )
            });

            if let ToolPermissionDecision::Deny(reason) = decision {
                return Err(reason);
            }

            let destination_path: Arc<str> = input.path.as_str().into();

            let external_symlink_target = external_directory.as_ref().and_then(|directory| {
                external_path_resolution_target(Path::new(&input.path), directory)
            });
            let symlink_escape_target = project
                .read_with(cx, |project, cx| {
                    detect_symlink_escape(project, &input.path, &canonical_roots, cx)
                        .map(|(_, target)| target)
                })
                .or(external_symlink_target);

            let sensitive_kind =
                sensitive_settings_kind(Path::new(&input.path), &canonical_roots, fs.as_ref())
                    .await;

            let decision =
                if matches!(decision, ToolPermissionDecision::Allow) && sensitive_kind.is_some() {
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
                        &input.path,
                        &canonical_target,
                        &event_stream,
                        cx,
                    )
                }))
            } else {
                match decision {
                    ToolPermissionDecision::Allow => None,
                    ToolPermissionDecision::Confirm => Some(cx.update(|cx| {
                        let title = format!("Create directory {}", MarkdownInlineCode(&input.path));
                        let context =
                            crate::ToolPermissionContext::new(Self::NAME, vec![input.path.clone()]);
                        authorize_with_sensitive_settings(
                            sensitive_kind,
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

            if let Some(external_directory) = external_directory {
                futures::select! {
                    result = fs.create_dir(&external_directory).fuse() => {
                        result.map_err(|e| format!("Creating directory {destination_path}: {e}"))?;
                    }
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Create directory cancelled by user".to_string());
                    }
                }

                return Ok(format!("Created directory {destination_path}"));
            }

            let create_entry = match in_project_path {
                Some(project_path) => {
                    project.update(cx, |project, cx| project.create_entry(project_path, true, cx))
                }
                None => return Err("Path to create was outside the project".to_string()),
            };

            futures::select! {
                result = create_entry.fuse() => {
                    result.map_err(|e| format!("Creating directory {destination_path}: {e}"))?;
                }
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Create directory cancelled by user".to_string());
                }
            }

            Ok(format!("Created directory {destination_path}"))
        })
    }
}

/// Create a directory that lives **outside** the project by granting sandboxed
/// terminal commands write access to exactly it.
///
/// The directory is created (Linux: eagerly, pinning the inode; macOS: after
/// approval) and the user is shown the real, canonicalized target in the sandbox
/// approval prompt — which is what defends against a concurrent symlink swap: the
/// grant is always against the inode/path the user actually saw. On denial, only
/// the directories we created are removed.
async fn create_out_of_project_directory(
    project: &Entity<Project>,
    input: &CreateDirectoryToolInput,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<String, String> {
    // Narrowing a grant to a brand-new directory only makes sense when the
    // project's terminal commands are sandboxed, and only on platforms that can
    // grant a not-yet-existing directory. Otherwise keep the historical
    // "outside the project" rejection.
    let sandboxing = project.read_with(cx, |project, cx| {
        crate::sandboxing::sandboxing_enabled_for_project(project, cx)
    });
    let platform_supported = cfg!(any(target_os = "linux", target_os = "macos"));
    if !sandboxing || !platform_supported {
        return Err("Path to create was outside the project".to_string());
    }

    let Some(reason) = input
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    else {
        return Err(
            "Creating a directory outside the project grants sandboxed terminal commands write \
             access to it, so a `reason` is required: briefly justify why the directory is needed, \
             then try again."
                .to_string(),
        );
    };
    let reason = reason.to_string();

    let absolute = resolve_absolute_path(project, &input.path, cx)
        .ok_or_else(|| format!("Couldn't resolve `{}` to an absolute path.", input.path))?;

    let prepared = cx
        .background_spawn(async move { sandbox::GrantableWriteDir::prepare(&absolute) })
        .await
        .map_err(|error| format!("Creating directory {}: {error}", input.path))?;

    let canonical = prepared.canonical_path().to_path_buf();
    // The directory was just created and its inode pinned, so persist the
    // resolved canonical alongside the raw request: enforcement rebuilds the
    // grant from the vetted canonical via a verifying reopen.
    let request = crate::sandboxing::SandboxRequest {
        write_paths: vec![settings::GrantedWritePath::resolved(
            prepared.untrusted_raw_path().to_path_buf(),
            canonical.clone(),
        )],
        ..Default::default()
    };

    let approve = cx.update(|cx| event_stream.authorize_sandbox(request, reason, cx));
    match approve.await {
        Ok(()) => {
            let display = canonical.display().to_string();
            cx.background_spawn(async move { prepared.finalize() })
                .await
                .map_err(|error| format!("Creating directory {display}: {error}"))?;
            Ok(format!("Created directory {display}"))
        }
        Err(error) => {
            // Roll back exactly what we created; leave the user no litter.
            cx.background_spawn(async move { prepared.discard() }).await;
            Err(format!("Create directory cancelled: {error}"))
        }
    }
}

/// Resolves the target of `create_directory` when the directory doesn't exist
/// yet, by resolving its **parent** and appending the new name.
///
/// A rooted or absolute path resolves without an entry, but a bare relative
/// path (`new_dir`) needs one to disambiguate — so without this it could never
/// create a new directory. `write_file` does the same for a new file. Returns
/// `Ok(None)` when the path isn't inside a worktree.
fn resolve_new_directory_parent(
    project: &Project,
    path: &Path,
    cx: &App,
) -> Result<Option<ProjectPath>, String> {
    let Some(parent_path) = path.parent() else {
        return Ok(None);
    };
    let Some(parent_project_path) = project.find_project_path(parent_path, cx) else {
        return Ok(None);
    };
    let Some(parent_entry) = project.entry_for_path(&parent_project_path, cx) else {
        return Ok(None);
    };
    if !parent_entry.is_dir() {
        return Ok(None);
    }

    // The parent is where the user's `file_scan_exclusions` / `private_files`
    // apply to a not-yet-existing child.
    ensure_path_not_hidden_by_settings(&parent_project_path, "create a directory", cx)
        .map_err(|error| format!("{error:#}"))?;

    let Some(file_name) = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .and_then(|file_name| RelPath::from_unix_str(file_name).ok())
    else {
        return Ok(None);
    };

    Ok(Some(ProjectPath {
        path: parent_project_path.path.join(file_name).into(),
        ..parent_project_path
    }))
}

/// Resolve a model-provided path to an absolute, lexically-normalized path.
/// Relative paths are joined onto the first worktree root.
fn resolve_absolute_path(
    project: &Entity<Project>,
    raw: &str,
    cx: &mut AsyncApp,
) -> Option<PathBuf> {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let base = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        })?;
        base.join(path)
    };
    util::paths::normalize_lexically(&absolute).ok()
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

    /// Installs user-level `worktree` settings, which the in-project settings
    /// check reads via `WorktreeSettings::get_global`.
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
    async fn test_create_directory_allows_global_skill_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .to_string_lossy()
            .into_owned();
        let created_path = agent_skills::global_skills_dir().join("my-skill");

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CreateDirectoryToolInput {
                    path: input_path,
                    reason: None,
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
        assert!(
            result.is_ok(),
            "Tool should create global skill directory: {result:?}"
        );
        assert!(fs.is_dir(&created_path).await);
    }

    #[gpui::test]
    async fn test_create_directory_rejects_other_global_paths(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root/project"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));
        let outside_path = agent_skills::global_skills_dir()
            .parent()
            .expect("global skills directory should have a parent")
            .join("not-skills");

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: outside_path.to_string_lossy().into_owned(),
                        reason: None,
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

        assert!(
            result.is_err(),
            "Tool should reject paths outside the project and global skills directory"
        );
        assert!(!fs.is_dir(&outside_path).await);
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "Non-skill global path should not emit an agent-skills authorization prompt",
        );
    }

    #[gpui::test]
    async fn test_create_directory_symlink_escape_requests_authorization(cx: &mut TestAppContext) {
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

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CreateDirectoryToolInput {
                    path: "project/link_to_external".into(),
                    reason: None,
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
        assert!(
            result.is_ok(),
            "Tool should succeed after authorization: {result:?}"
        );
    }

    #[gpui::test]
    async fn test_create_directory_symlink_escape_denied(cx: &mut TestAppContext) {
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

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CreateDirectoryToolInput {
                    path: "project/link_to_external".into(),
                    reason: None,
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
    async fn test_create_directory_symlink_escape_confirm_requires_single_approval(
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

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CreateDirectoryToolInput {
                    path: "project/link_to_external".into(),
                    reason: None,
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
        assert!(
            result.is_ok(),
            "Tool should succeed after one authorization: {result:?}"
        );
    }

    #[gpui::test]
    async fn test_create_directory_symlink_escape_honors_deny_policy(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.tool_permissions.tools.insert(
                "create_directory".into(),
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

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: "project/link_to_external".into(),
                        reason: None,
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

    /// Out-of-project creation goes through the sandbox write-grant prompt and,
    /// on approval, creates the *specific* new directory (not its broad parent).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[gpui::test]
    async fn test_create_directory_out_of_project_creates_and_grants(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "project": { "src": {} } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        // The sandbox create path operates on the *real* filesystem, so use a
        // real directory outside the (fake) project.
        let scratch = tempfile::tempdir().unwrap();
        let target = scratch.path().join("new_grant_dir");
        assert!(!target.exists());

        let tool = Arc::new(CreateDirectoryTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let path_input = target.to_string_lossy().into_owned();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CreateDirectoryToolInput {
                    path: path_input,
                    reason: Some("scratch space for the build".into()),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        let details = acp_thread::sandbox_authorization_details_from_meta(&auth.tool_call.meta)
            .expect("out-of-project create should request a sandbox write grant");
        // The grant is for exactly the new directory, not its parent, and
        // carries the resolved canonical established when it was created.
        let expected_canonical = scratch.path().canonicalize().unwrap().join("new_grant_dir");
        assert_eq!(details.write_paths.len(), 1);
        assert_eq!(
            details.write_paths[0].canonical_or_requested(),
            expected_canonical
        );

        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::AllowThread.as_id()),
                acp::PermissionOptionKind::AllowAlways,
            ))
            .unwrap();

        let result = task.await;
        assert!(result.is_ok(), "expected success, got {result:?}");
        assert!(
            target.is_dir(),
            "the new directory should have been created"
        );
    }

    /// Denying the grant removes the directory we eagerly created, leaving no
    /// trace on the filesystem.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[gpui::test]
    async fn test_create_directory_out_of_project_denied_cleans_up(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({ "project": { "src": {} } }))
            .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let scratch = tempfile::tempdir().unwrap();
        let target = scratch.path().join("denied_dir");

        let tool = Arc::new(CreateDirectoryTool::new(project));
        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let path_input = target.to_string_lossy().into_owned();
        let task = cx.update(|cx| {
            tool.run(
                ToolInput::resolved(CreateDirectoryToolInput {
                    path: path_input,
                    reason: Some("scratch space".into()),
                }),
                event_stream,
                cx,
            )
        });

        let auth = event_rx.expect_authorization().await;
        auth.response
            .send(acp_thread::SelectedPermissionOutcome::new(
                acp::PermissionOptionId::new(acp_thread::SandboxPermission::Deny.as_id()),
                acp::PermissionOptionKind::RejectOnce,
            ))
            .unwrap();

        let result = task.await;
        assert!(result.is_err(), "denied create should fail");
        assert!(
            !target.exists(),
            "denied create should leave no directory behind"
        );
    }

    /// `file_scan_exclusions` is a hard block, checked before any prompt: the
    /// create fails and the directory is never made.
    #[gpui::test]
    async fn test_create_directory_respects_file_scan_exclusions(cx: &mut TestAppContext) {
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

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: "project/generated/sub".to_string(),
                        reason: None,
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
            "a settings-blocked create must not prompt for authorization",
        );
        assert!(
            fs.metadata(&PathBuf::from(path!("/root/project/generated")))
                .await
                .unwrap()
                .is_none(),
            "nothing should be created under an excluded path",
        );
    }

    /// Same hard block for `private_files`.
    #[gpui::test]
    async fn test_create_directory_respects_private_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.private_files = Some(vec!["**/secrets".to_string()].into());
        });
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: "project/secrets".to_string(),
                        reason: None,
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
            "a settings-blocked create must not prompt for authorization",
        );
        assert!(
            fs.metadata(&PathBuf::from(path!("/root/project/secrets")))
                .await
                .unwrap()
                .is_none(),
            "nothing should be created at a private path",
        );
    }

    /// The settings checks must not block ordinary in-project directories.
    /// Creating a directory needs no existing entry, so it works inside a
    /// gitignored directory that the project never scanned. This is the
    /// counterpart to the mutating tools' limitation, which needs an entry for
    /// the target itself.
    #[gpui::test]
    async fn test_create_directory_inside_gitignored_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "project": {
                    "src": { "main.rs": "fn main() {}" },
                    "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
                },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: "project/node_modules/pkg/generated".to_string(),
                        reason: None,
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(result.is_ok(), "expected success, got {result:?}");
        assert!(
            fs.is_dir(&PathBuf::from(path!(
                "/root/project/node_modules/pkg/generated"
            )))
            .await
        );
    }

    /// The global settings apply to an out-of-project directory too, as they do
    /// in every other file tool.
    #[gpui::test]
    async fn test_create_directory_outside_project_respects_settings(cx: &mut TestAppContext) {
        init_test(cx);
        set_worktree_settings(cx, |settings| {
            settings.project.worktree.file_scan_exclusions =
                Some(SplicingVec::from(vec!["**/generated".to_string()]));
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        fs.create_dir(path!("/outside").as_ref()).await.unwrap();

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: path!("/outside/generated").to_string(),
                        reason: None,
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        let error = result.unwrap_err();
        assert!(
            error.contains("file_scan_exclusions"),
            "expected the error to name the blocking setting, got: {error}"
        );
        assert!(
            !fs.is_dir(&PathBuf::from(path!("/outside/generated"))).await,
            "the excluded directory must not be created"
        );
    }

    /// A bare relative path to a not-yet-existing directory resolves through its
    /// parent, so it creates the directory like the rooted/absolute form — even
    /// inside a gitignored directory, which is scanned on demand first.
    #[gpui::test]
    async fn test_create_directory_bare_relative_gitignored_path_scans_and_creates(
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
                "project": {
                    "src": { "main.rs": "fn main() {}" },
                    "node_modules": { "pkg": { "index.js": "module.exports = 1;" } },
                },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: "node_modules/pkg/newdir".to_string(),
                        reason: None,
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await;

        assert!(
            result.is_ok(),
            "the bare form should create the directory: {result:?}"
        );
        assert!(
            fs.is_dir(&PathBuf::from(path!(
                "/root/project/node_modules/pkg/newdir"
            )))
            .await,
            "the directory should have been created"
        );
    }

    #[gpui::test]
    async fn test_create_directory_in_project_succeeds(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({ "project": { "src": { "main.rs": "fn main() {}" } } }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let tool = Arc::new(CreateDirectoryTool::new(project));

        let (event_stream, mut event_rx) = ToolCallEventStream::test();
        let result = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(CreateDirectoryToolInput {
                        path: "project/new_dir".to_string(),
                        reason: None,
                    }),
                    event_stream,
                    cx,
                )
            })
            .await;

        assert!(result.is_ok(), "expected success, got {result:?}");
        assert!(
            !matches!(
                event_rx.try_recv(),
                Ok(Ok(crate::ThreadEvent::ToolCallAuthorization(_)))
            ),
            "an allowed in-project create must not prompt for authorization",
        );
        assert!(
            fs.is_dir(&PathBuf::from(path!("/root/project/new_dir")))
                .await
        );
    }
}
