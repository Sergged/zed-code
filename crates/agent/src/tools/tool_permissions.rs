use crate::{
    Thread, ToolCallEventStream, ToolPermissionContext, ToolPermissionDecision,
    decide_permission_for_path_forms,
};
use agent_client_protocol::schema::v1 as acp;
use agent_skills::is_agents_skills_path;
use anyhow::{Result, anyhow};
use fs::Fs;
use gpui::{App, Entity, Task, WeakEntity};
use project::{Project, ProjectPath, WorktreeSettings};
use settings::Settings;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use util::rel_path::{RelPath, RelPathBuf};
use util::{normalize_path, paths::component_matches_ignore_ascii_case};

pub enum SensitiveSettingsKind {
    Local,
    Global,
    AgentSkills,
}

/// Result of resolving a path within the project with symlink safety checks.
///
/// See [`resolve_project_path`].
#[derive(Debug, Clone)]
pub enum ResolvedProjectPath {
    /// The path resolves to a location safely within the project boundaries.
    Safe(ProjectPath),
    /// The path resolves through a symlink to a location outside the project.
    /// Agent tools should prompt the user before proceeding with access.
    SymlinkEscape {
        /// The project-relative path (before symlink resolution).
        project_path: ProjectPath,
        /// The canonical (real) filesystem path the symlink points to.
        canonical_target: PathBuf,
    },
}

/// Asynchronously canonicalizes the absolute paths of all worktrees in a
/// project using the provided `Fs`. The returned paths can be passed to
/// [`resolve_project_path`] and related helpers so that they don't need to
/// perform blocking filesystem I/O themselves.
pub async fn canonicalize_worktree_roots<C: gpui::AppContext>(
    project: &Entity<Project>,
    fs: &Arc<dyn Fs>,
    cx: &C,
) -> Vec<PathBuf> {
    let abs_paths: Vec<Arc<Path>> = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path())
            .collect()
    });

    let mut canonical_roots = Vec::with_capacity(abs_paths.len());
    for abs_path in &abs_paths {
        match fs.canonicalize(abs_path).await {
            Ok(canonical) => canonical_roots.push(canonical),
            Err(_) => canonical_roots.push(abs_path.to_path_buf()),
        }
    }
    canonical_roots
}

/// Walks up ancestors of `path` to find the deepest one that exists on disk and
/// can be canonicalized, then reattaches the remaining suffix components.
///
/// This is needed for paths where the leaf (or intermediate directories) don't
/// exist yet but an ancestor may be a symlink. For example, when creating
/// `.zed/settings.json` where `.zed` is a symlink to an external directory.
///
/// Note: intermediate directories *can* be symlinks (not just leaf entries),
/// so we must walk the full ancestor chain. For example:
///   `ln -s /external/config /project/.zed`
/// makes `.zed` an intermediate symlink directory.
async fn canonicalize_with_ancestors(path: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(path);
    let mut suffix_components = Vec::new();
    loop {
        match current {
            Some(ancestor) => match fs.canonicalize(ancestor).await {
                Ok(canonical) => {
                    let mut result = canonical;
                    for component in suffix_components.into_iter().rev() {
                        result.push(component);
                    }
                    return Some(result);
                }
                Err(_) => {
                    if let Some(file_name) = ancestor.file_name() {
                        suffix_components.push(file_name.to_os_string());
                    }
                    current = ancestor.parent();
                }
            },
            None => return None,
        }
    }
}

/// Returns the canonicalized global agent skills directory
/// (`~/.agents/skills`).
///
/// Recomputed on every call rather than cached: the underlying
/// `canonicalize_with_ancestors` is a few `stat` syscalls (which the OS
/// page cache already handles), and a process-wide cache would either go
/// stale if the user moved `~/.agents/skills`, or pollute across tests
/// using different `FakeFs` instances.
async fn canonical_global_skills_dir(fs: &dyn Fs) -> Option<PathBuf> {
    canonicalize_with_ancestors(&agent_skills::global_skills_dir(), fs).await
}

fn is_within_any_worktree(canonical_path: &Path, canonical_worktree_roots: &[PathBuf]) -> bool {
    canonical_worktree_roots
        .iter()
        .any(|root| canonical_path.starts_with(root))
}

/// If `path` names `~/.agents/skills` or one of its descendants, return the
/// canonicalized absolute path. Returns `None` for any path that resolves
/// outside the global skills tree, for relative paths that don't start with
/// `~`, or if the skills directory itself can't be canonicalized (fail closed
/// — better to refuse access than to compare against a non-canonical path).
///
/// This is the gate that lets `read_file` / `list_directory` reach into the
/// global skills directory — which lives outside any worktree — without
/// also opening up arbitrary external paths.
pub async fn resolve_global_skill_path(path: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    let normalized_path = resolve_lexical_global_skill_path(path)?;

    // Canonicalize both sides so symlinks can't sneak the path out of the
    // skills tree (and so different but equivalent path representations
    // match). The lexical check above intentionally runs first, so a
    // symlinked `~/.agents/skills` root can't broaden the allowlist to every
    // path under the symlink target. A linked immediate skill directory is
    // allowed separately, but only for paths that stay under that skill target.
    let canonical_path = fs.canonicalize(&normalized_path).await.ok()?;
    let canonical_skills_dir = canonical_global_skills_dir(fs).await?;

    if canonical_path.starts_with(&canonical_skills_dir)
        || is_in_linked_global_skill_dir(
            &normalized_path,
            &canonical_path,
            &canonical_skills_dir,
            fs,
        )
        .await
    {
        Some(canonical_path)
    } else {
        None
    }
}

async fn is_in_linked_global_skill_dir(
    path: &Path,
    canonical_path: &Path,
    canonical_skills_dir: &Path,
    fs: &dyn Fs,
) -> bool {
    let skills_dir = normalize_path(&agent_skills::global_skills_dir());
    let Ok(relative_path) = path.strip_prefix(&skills_dir) else {
        return false;
    };
    let Some(Component::Normal(skill_dir_name)) = relative_path.components().next() else {
        return false;
    };

    let skill_dir = skills_dir.join(skill_dir_name);
    let Ok(canonical_skill_dir) = fs.canonicalize(&skill_dir).await else {
        return false;
    };

    !canonical_skill_dir.starts_with(canonical_skills_dir)
        && canonical_path.starts_with(&canonical_skill_dir)
        && fs
            .is_file(&skill_dir.join(agent_skills::SKILL_FILE_NAME))
            .await
}

fn expand_home_prefix(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }

    let mut components = path.components();
    let first_component = components.next()?;
    if !matches!(first_component, Component::Normal(component) if component == "~") {
        return None;
    }

    let mut expanded = paths::home_dir().clone();
    for component in components {
        match component {
            Component::Normal(component) => expanded.push(component),
            Component::CurDir => {}
            Component::ParentDir => expanded.push(".."),
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(expanded)
}

fn expand_and_normalize_absolute_path(path: &Path) -> Option<PathBuf> {
    let expanded_path = expand_home_prefix(path)?;
    let normalized_path = normalize_path(&expanded_path);
    normalized_path.is_absolute().then_some(normalized_path)
}

fn resolve_lexical_global_skill_path(path: &Path) -> Option<PathBuf> {
    let normalized_path = expand_and_normalize_absolute_path(path)?;
    let normalized_skills_dir = normalize_path(&agent_skills::global_skills_dir());

    normalized_path
        .starts_with(&normalized_skills_dir)
        .then_some(normalized_path)
}

/// If `path` names `~/.agents/skills` or one of its descendants, return a
/// canonical absolute path for it. Unlike [`resolve_global_skill_path`], the
/// target path may or may not exist on disk yet — the caller decides whether
/// to read, write, or create it. Returns `None` for any other path, including
/// siblings of the global skills tree or paths that would escape it with `..`
/// or symlinks.
pub async fn resolve_creatable_global_skill_path(path: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    let normalized_path = resolve_lexical_global_skill_path(path)?;
    let canonical_path = canonicalize_with_ancestors(&normalized_path, fs).await?;
    let canonical_skills_dir = canonical_global_skills_dir(fs).await?;

    if canonical_path.starts_with(&canonical_skills_dir) {
        Some(canonical_path)
    } else {
        None
    }
}

/// Best available explanation for why a model-supplied path doesn't resolve to
/// a file the tool can act on, or `None` when there is nothing more specific to
/// say than the caller's own message.
///
/// Covers both shapes of "the model can fix this", so every file tool reports
/// them the same way:
///
/// - the path resolves to a [`ProjectPath`] but has no snapshot entry, because
///   its gitignored parent directory was never scanned;
/// - the path doesn't resolve at all, which is either a bare relative path into
///   such a directory or a path the user's `file_scan_exclusions` covers.
///
/// Callers should use it as `explain_unresolved_path(..).unwrap_or_else(||
/// their own message)`.
pub fn explain_unresolved_path(project: &Project, path: &Path, cx: &App) -> Option<String> {
    if let Some(project_path) = project.find_project_path(path, cx)
        && let Some(explanation) = explain_unscanned_ignored_path(project, &project_path, cx)
    {
        return Some(explanation);
    }

    explain_unresolved_relative_path(project, path, cx)
}

/// Explains why a project path that resolved to a [`ProjectPath`] still has no
/// snapshot entry, when the reason is a gitignored directory the worktree
/// hasn't scanned.
///
/// The read tools can reach such files (they pre-scan ignored directories on
/// demand), but the mutating tools resolve paths through the snapshot, so they
/// have nothing to act on. Naming the cause keeps the model from retrying the
/// same call — or worse, concluding the file doesn't exist.
pub fn explain_unscanned_ignored_path(
    project: &Project,
    project_path: &ProjectPath,
    cx: &App,
) -> Option<String> {
    let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
    let snapshot = worktree.read(cx).snapshot();
    if snapshot.entry_for_path(&project_path.path).is_some() {
        return None;
    }

    let deepest_entry = project_path
        .path
        .ancestors()
        .find_map(|ancestor| snapshot.entry_for_path(ancestor))?;
    if !(deepest_entry.is_ignored && deepest_entry.kind.is_unloaded()) {
        return None;
    }

    let path_style = project.path_style(cx);
    Some(format!(
        "`{}` is inside the gitignored directory `{}`, which this project hasn't scanned, so tools that modify files can't reach it. The read tools (`read_file`, `list_directory`, `grep`, `find_path`) can: use `read_file` to inspect it and tell the user which change to make.",
        project_path.path.display(path_style),
        deepest_entry.path.display(path_style),
    ))
}

/// Whether the target of a direct-filesystem operation has to exist already.
#[derive(Clone, Copy)]
pub enum PathExistence {
    /// The path must exist, and the whole chain above it is resolved through
    /// the filesystem.
    MustExist,
    /// The path may not exist yet; only the existing prefix is resolved.
    MayNotExist,
}

/// A path a file tool can act on through the filesystem directly, with no
/// worktree snapshot entry to go through.
pub struct DirectFsPath {
    /// The canonical absolute path to read from or write to.
    pub absolute_path: PathBuf,
    /// The path the model named, made absolute (`~` expanded, `.`/`..` folded)
    /// but with its final component *not* followed. Differs from
    /// `absolute_path` when the leaf is a symlink; tools that act on the named
    /// entry itself (delete, move) use this so a symlink is removed or renamed
    /// rather than its target.
    pub named_path: PathBuf,
    /// The project path this target came from, when it lives inside a worktree.
    ///
    /// Callers apply that worktree's `file_scan_exclusions` / `private_files`
    /// globs to it, which must not happen for genuinely out-of-project paths
    /// (their globs are worktree-scoped and don't apply there).
    pub project_path: Option<ProjectPath>,
    /// The real target, when the requested path is inside a worktree but
    /// resolves outside it — i.e. something along the way is a symlink.
    ///
    /// The snapshot normally catches this (the scanner marks such entries and
    /// stores their canonical path), but it never looked inside an unscanned
    /// directory, so the file tools have to canonicalize and notice themselves.
    /// Callers must show the user this target before touching it.
    pub symlink_escape: Option<PathBuf>,
}

/// Resolves a model-supplied path that a file tool should act on through the
/// filesystem instead of the worktree snapshot.
///
/// That's two kinds of path:
///
/// - **Outside the project.** The normal external-path route.
/// - **Inside the project, but the worktree never scanned its parent
///   directory** because the parent is gitignored (`node_modules`, `dist`,
///   `target`). The scanner skips those, so a file below one has no snapshot
///   entry, and the read tools already reach it by loading from disk.
///
/// Everything else returns `None` and keeps using the worktree, which is what
/// applies the scanner's `file_scan_exclusions` / `private_files` filtering and
/// its symlink verification.
pub async fn resolve_direct_fs_path(
    path: &Path,
    existence: PathExistence,
    project: &Entity<Project>,
    canonical_worktree_roots: &[PathBuf],
    fs: &dyn Fs,
    cx: &mut gpui::AsyncApp,
) -> Option<DirectFsPath> {
    let canonicalize = |path: PathBuf| async move {
        match existence {
            PathExistence::MustExist => fs.canonicalize(&path).await.ok(),
            PathExistence::MayNotExist => canonicalize_with_ancestors(&path, fs).await,
        }
    };

    // Outside the project: any absolute (or `~`-prefixed) path that resolves
    // outside every worktree. Relative paths are left to the worktree below.
    if let Some(normalized_path) = expand_and_normalize_absolute_path(path)
        && let Some(canonical_path) = canonicalize(normalized_path.clone()).await
        && !is_within_any_worktree(&canonical_path, canonical_worktree_roots)
    {
        // The permission rules match the path text the model wrote, while the OS
        // acts on the resolved path. Whenever the two differ — anything along
        // the way is a symlink — the rules decided about a different path than
        // the one actually touched, so the user must see the real target. The
        // one exception is a path the worktree scanner already flagged: the
        // tool's own authorization then prompts with the same target, and
        // prompting here as well would ask twice.
        let snapshot_would_prompt = project.read_with(cx, |project, cx| {
            matches!(
                resolve_project_path(project, path, canonical_worktree_roots, cx),
                Ok(ResolvedProjectPath::SymlinkEscape { .. })
            )
        });
        let symlink_escape = (normalized_path != canonical_path && !snapshot_would_prompt)
            .then(|| canonical_path.clone());
        return Some(DirectFsPath {
            symlink_escape,
            absolute_path: canonical_path,
            named_path: normalized_path,
            project_path: None,
        });
    }

    // Inside a worktree the scanner never scanned through: only when the file
    // has no entry, and the deepest entry that does exist is an ignored
    // directory that was left unloaded.
    let (project_path, named_path) = project.read_with(cx, |project, cx| {
        let project_path = project.find_project_path(path, cx)?;
        if project.entry_for_path(&project_path, cx).is_some() {
            return None;
        }

        let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
        let snapshot = worktree.read(cx);
        let deepest_entry = project_path
            .path
            .ancestors()
            .find_map(|ancestor| snapshot.entry_for_path(ancestor))?;
        if !(deepest_entry.is_ignored && deepest_entry.kind.is_unloaded()) {
            return None;
        }

        let named_path = snapshot.absolutize(&project_path.path);
        Some((project_path, named_path))
    })?;

    let canonical_path = canonicalize(named_path.clone()).await?;
    let within_worktree = is_within_any_worktree(&canonical_path, canonical_worktree_roots);
    Some(DirectFsPath {
        symlink_escape: (!within_worktree).then(|| canonical_path.clone()),
        absolute_path: canonical_path,
        named_path,
        project_path: Some(project_path),
    })
}

/// Applies the user's settings to a direct-filesystem target, then asks the user
/// when the requested path resolves somewhere other than itself.
///
/// The second half of [`resolve_direct_fs_path`]: the resolver decides which
/// route a path takes, this decides whether it may. Settings are hard blocks and
/// are checked first, so a blocked call never prompts.
///
/// Every file tool runs this for a direct-filesystem target, which is what makes
/// an unscanned directory behave like the outside of the project: the user sees
/// the same prompt, naming the same real target, and the same settings apply.
///
/// `prompt_for_symlink` controls whether this raises the "the path you named is
/// not the file that will be touched" prompt for a `symlink_escape`. Pass
/// `false` when the caller raises that prompt itself afterwards (the edit tools,
/// whose [`authorize_file_edit`] handles it, so one call doesn't prompt twice).
pub async fn authorize_direct_fs_path(
    tool_name: &str,
    requested_path: &str,
    subject: &str,
    direct_path: &DirectFsPath,
    prompt_for_symlink: bool,
    event_stream: &ToolCallEventStream,
    cx: &mut gpui::AsyncApp,
) -> Result<(), String> {
    // Global globs can describe any path; a target that came from a worktree
    // also has that worktree's own globs.
    if let Some(setting) =
        cx.update(|cx| external_path_excluded_by_settings(&direct_path.absolute_path, cx))
    {
        return Err(format!(
            "Cannot {subject} because its path matches the user's global `{setting}` setting: {requested_path}"
        ));
    }
    if let Some(project_path) = &direct_path.project_path {
        cx.update(|cx| ensure_path_not_hidden_by_settings(project_path, subject, cx))
            .map_err(|error| format!("{error:#}"))?;
    }

    if prompt_for_symlink && let Some(canonical_target) = &direct_path.symlink_escape {
        cx.update(|cx| {
            authorize_symlink_access(
                tool_name,
                requested_path,
                canonical_target,
                event_stream,
                cx,
            )
        })
        .await
        .map_err(|error| error.to_string())?;
    }

    Ok(())
}

/// Resolves a model-supplied path that lives outside every project worktree to a
/// canonical absolute path.
///
/// This is the general counterpart to [`resolve_global_skill_path`]: it accepts
/// *any* absolute (or `~`-prefixed) path, not only the global skills tree, so
/// file tools can operate across the whole filesystem. Relative paths are not
/// resolved here — they stay project-relative and go through the normal
/// project-path machinery. Paths that canonicalize back into a worktree also
/// return `None`, so in-project access can't bypass the worktree scanner (and
/// its `file_scan_exclusions` / `private_files` filtering).
///
/// The target must already exist; use [`resolve_creatable_external_path`] for
/// paths that may not exist yet (creates and overwrites).
pub async fn resolve_external_path(
    path: &Path,
    canonical_worktree_roots: &[PathBuf],
    fs: &dyn Fs,
) -> Option<PathBuf> {
    let normalized_path = expand_and_normalize_absolute_path(path)?;
    let canonical_path = fs.canonicalize(&normalized_path).await.ok()?;
    (!is_within_any_worktree(&canonical_path, canonical_worktree_roots)).then_some(canonical_path)
}

/// Like [`resolve_external_path`], but the target path (and any intermediate
/// directories) may not exist yet. Used for file/directory creation and
/// overwrites.
pub async fn resolve_creatable_external_path(
    path: &Path,
    canonical_worktree_roots: &[PathBuf],
    fs: &dyn Fs,
) -> Option<PathBuf> {
    let normalized_path = expand_and_normalize_absolute_path(path)?;
    let canonical_path = canonicalize_with_ancestors(&normalized_path, fs).await?;
    (!is_within_any_worktree(&canonical_path, canonical_worktree_roots)).then_some(canonical_path)
}

/// The canonical target when the absolute (or `~`-prefixed) `requested` path
/// resolves somewhere other than itself — i.e. a symlink along the way — or
/// `None` when it resolves to itself or isn't an absolute/`~` path.
///
/// Tools that resolve external paths directly through the filesystem (rather
/// than through [`resolve_direct_fs_path`]) use this to raise the same
/// "the path you named is not the file that will be touched" prompt: the
/// permission rules match the text the model wrote, while the OS acts on the
/// resolved path, so a differing resolution must be shown to the user first.
pub fn external_path_resolution_target(requested: &Path, canonical: &Path) -> Option<PathBuf> {
    let normalized = expand_and_normalize_absolute_path(requested)?;
    (normalized != canonical).then(|| canonical.to_path_buf())
}

/// Like [`external_path_resolution_target`], but canonicalizes `requested`
/// itself, for callers that don't already hold the resolved path. `None` when
/// the path resolves to itself, isn't an absolute/`~` path, or doesn't exist.
pub async fn external_symlink_target(requested: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    let normalized = expand_and_normalize_absolute_path(requested)?;
    let canonical = fs.canonicalize(&normalized).await.ok()?;
    (normalized != canonical).then_some(canonical)
}

/// Builds a project-relative path from an absolute path so the worktree-scoped
/// `file_scan_exclusions` / `private_files` globs can also be matched against
/// out-of-project paths. Root and `.` components are dropped; `..` (which
/// canonical external paths never contain) makes this fail closed.
fn rel_path_for_external(path: &Path) -> Option<RelPathBuf> {
    let mut rel_path = RelPathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => {
                rel_path.push_component(component.to_str()?).ok()?;
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => return None,
        }
    }
    (!rel_path.is_empty()).then_some(rel_path)
}

/// Checks the user's global `file_scan_exclusions` and `private_files` settings
/// against an out-of-project path. Returns the name of the setting that matched,
/// or `None` when the path is allowed.
///
/// Only the global settings apply here: the per-worktree overrides are scoped to
/// a worktree, which an external path by definition is not part of.
pub fn external_path_excluded_by_settings(path: &Path, cx: &App) -> Option<&'static str> {
    let rel_path = rel_path_for_external(path)?;
    let settings = WorktreeSettings::get_global(cx);
    if settings.is_path_excluded(&rel_path) {
        Some("file_scan_exclusions")
    } else if settings.is_path_private(&rel_path) {
        Some("private_files")
    } else {
        None
    }
}

/// Whether `path` is too broad to be the target of a destructive out-of-project
/// operation (delete/move/overwrite): the filesystem root, the user's home
/// directory, or an ancestor of a project worktree. Guarding these prevents a
/// single tool call from wiping out the user's home or the project itself.
pub fn is_protected_external_path(path: &Path, canonical_worktree_roots: &[PathBuf]) -> bool {
    path.parent().is_none()
        || path == util::paths::home_dir()
        || canonical_worktree_roots
            .iter()
            .any(|root| root.as_path() != path && root.starts_with(path))
}

fn is_strict_descendant(path: &Path, ancestor: &Path) -> bool {
    path != ancestor && path.starts_with(ancestor)
}

/// Returns whether `path` resolves to the global agent skills directory itself.
///
/// This is used by destructive tools to reject operations targeting the root
/// `~/.agents/skills` directory while still allowing operations on individual
/// skills or resources beneath it.
pub async fn resolves_to_global_skills_dir(path: &Path, fs: &dyn Fs) -> bool {
    let Some(normalized_path) = resolve_lexical_global_skill_path(path) else {
        return false;
    };
    let Some(canonical_path) = canonicalize_with_ancestors(&normalized_path, fs).await else {
        return false;
    };
    let Some(canonical_skills_dir) = canonical_global_skills_dir(fs).await else {
        return false;
    };

    canonical_path == canonical_skills_dir
}

/// Filters a previously-resolved global skills path so that callers which
/// must never act on `~/.agents/skills` itself (move, delete) only see paths
/// that point strictly below the skills root.
async fn restrict_to_skill_descendant(
    canonical_path: Option<PathBuf>,
    fs: &dyn Fs,
) -> Option<PathBuf> {
    let canonical_path = canonical_path?;
    let canonical_skills_dir = canonical_global_skills_dir(fs).await?;
    is_strict_descendant(&canonical_path, &canonical_skills_dir).then_some(canonical_path)
}

/// Like [`resolve_global_skill_path`], but only succeeds for paths strictly
/// below `~/.agents/skills`, not the skills directory itself.
pub async fn resolve_global_skill_descendant_path(path: &Path, fs: &dyn Fs) -> Option<PathBuf> {
    restrict_to_skill_descendant(resolve_global_skill_path(path, fs).await, fs).await
}

/// Like [`resolve_creatable_global_skill_path`], but only succeeds for paths
/// strictly below `~/.agents/skills`, not the skills directory itself.
pub async fn resolve_creatable_global_skill_descendant_path(
    path: &Path,
    fs: &dyn Fs,
) -> Option<PathBuf> {
    restrict_to_skill_descendant(resolve_creatable_global_skill_path(path, fs).await, fs).await
}

/// Returns the kind of sensitive settings or agent skills location this path targets, if any:
/// either inside a `.zed/` local-settings directory, inside `.agents/skills/`, or inside
/// the global config dir.
///
/// `canonical_worktree_roots` should be the result of
/// [`canonicalize_worktree_roots`]; it's used to re-check the local
/// `.zed/` and `.agents/skills/` protections against the canonical form
/// of `path`, which catches two classes of bypass that the raw-component
/// scan misses:
///
///   1. `..` traversal, e.g. `.agents/foo/../skills/SKILL.md`. The raw
///      components are `[.agents, foo, .., skills, SKILL.md]`, so the
///      consecutive-pair match in [`is_agents_skills_path`] fails.
///   2. Intra-project symlinks, e.g. a symlink `safe -> .zed` followed
///      by `safe/settings.json`. `resolve_project_path` correctly classes
///      this as *not* a symlink escape (it stays inside the project), so
///      the raw-path check is our only line of defense and it doesn't see
///      `.zed` either.
///
/// After canonicalizing we strip the matching worktree root before
/// re-scanning components, so that a worktree literally rooted at a path
/// like `~/projects/.zed/foo` doesn't classify every file inside it as
/// `.zed/` local-settings — only files that have `.zed` (or
/// `.agents/skills`) inside the worktree are flagged.
pub async fn sensitive_settings_kind(
    path: &Path,
    canonical_worktree_roots: &[PathBuf],
    fs: &dyn Fs,
) -> Option<SensitiveSettingsKind> {
    let local_settings_folder = paths::local_settings_folder_name();

    // Fast path: scan the raw path components before any I/O. Covers the
    // common case where the agent passes a path that literally contains
    // `.zed/` or `.agents/skills/`.
    if path.components().any(|component| {
        component_matches_ignore_ascii_case(component.as_os_str(), local_settings_folder)
    }) {
        return Some(SensitiveSettingsKind::Local);
    }

    if is_agents_skills_path(path) {
        return Some(SensitiveSettingsKind::AgentSkills);
    }

    if let Some(canonical_path) = canonicalize_with_ancestors(path, fs).await {
        // Re-check the local protections against the canonical path,
        // restricted to within the project's worktrees, to catch `..`
        // and intra-project-symlink bypasses (see doc comment above).
        for root in canonical_worktree_roots {
            let Ok(relative) = canonical_path.strip_prefix(root) else {
                continue;
            };

            if relative.components().any(|component| {
                component_matches_ignore_ascii_case(component.as_os_str(), local_settings_folder)
            }) {
                return Some(SensitiveSettingsKind::Local);
            }
            if is_agents_skills_path(relative) {
                return Some(SensitiveSettingsKind::AgentSkills);
            }

            // The canonical path can only live inside one worktree, so
            // stop after the first match.
            break;
        }

        if let Some(canonical_skills_dir) = canonical_global_skills_dir(fs).await {
            if canonical_path.starts_with(&canonical_skills_dir) {
                return Some(SensitiveSettingsKind::AgentSkills);
            }
        }

        if let Some(canonical_config_dir) =
            canonicalize_with_ancestors(paths::config_dir(), fs).await
        {
            if canonical_path.starts_with(&canonical_config_dir) {
                return Some(SensitiveSettingsKind::Global);
            }
        }
    }

    None
}

/// Rejects a resolved project path that the user's `file_scan_exclusions` or
/// `private_files` settings hide from the agent (global or worktree-scoped).
///
/// Every file tool runs this so read-only and mutating tools enforce the same
/// rules: if the agent may not read a path, it may not modify it either.
/// `subject` completes the error message, e.g. `"write to"`, `"delete"`.
pub fn ensure_path_not_hidden_by_settings(
    project_path: &ProjectPath,
    subject: &str,
    cx: &App,
) -> Result<()> {
    let display_path = project_path.path.to_string();

    let global_settings = WorktreeSettings::get_global(cx);
    if global_settings.is_path_excluded(&project_path.path) {
        return Err(anyhow!(
            "Cannot {subject} because its path matches the global `file_scan_exclusions` setting: {display_path}"
        ));
    }
    if global_settings.is_path_private(&project_path.path) {
        return Err(anyhow!(
            "Cannot {subject} because its path matches the global `private_files` setting: {display_path}"
        ));
    }

    let worktree_settings = WorktreeSettings::get(Some(project_path.into()), cx);
    if worktree_settings.is_path_excluded(&project_path.path) {
        return Err(anyhow!(
            "Cannot {subject} because its path matches the worktree `file_scan_exclusions` setting: {display_path}"
        ));
    }
    if worktree_settings.is_path_private(&project_path.path) {
        return Err(anyhow!(
            "Cannot {subject} because its path matches the worktree `private_files` setting: {display_path}"
        ));
    }

    Ok(())
}

/// Explains why a model-supplied project-relative path failed to resolve, when
/// the reason is something the model can act on.
///
/// Bare relative paths (without the worktree root name) are disambiguated
/// against worktree snapshot entries, so one that points into a gitignored
/// directory the worktree hasn't scanned (e.g. `node_modules/...`) can't be
/// resolved — while the same file is reachable as a worktree-relative path
/// (prefixed with the root name) or as an absolute path. Every worktree and
/// interpretation that resolves to such a directory is listed, rather than a
/// single guessed one, so a multi-root workspace doesn't steer the model to the
/// wrong root. Paths matching the user's `file_scan_exclusions` setting are
/// never accessible, in any form.
///
/// Returns `None` for absolute and `~`-prefixed paths (handled by the
/// external-path machinery) and for paths with nothing specific to say, so
/// callers keep their generic error.
pub fn explain_unresolved_relative_path(
    project: &Project,
    path: &Path,
    cx: &App,
) -> Option<String> {
    if path.is_absolute() || path.to_string_lossy().starts_with('~') {
        return None;
    }

    let path_style = project.path_style(cx);
    let mut suggestions: Vec<String> = Vec::new();
    let mut excluded = false;

    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let snapshot = worktree.snapshot();

        // Mirror `Project::find_project_path`'s two interpretations of a
        // relative path: with the worktree root name as a prefix, and as a
        // literal worktree-relative path.
        let mut candidates = Vec::with_capacity(2);
        if let Ok(stripped) = path.strip_prefix(worktree.root_name().as_std_path())
            && let Ok(relative_path) = RelPath::new(stripped, path_style)
        {
            candidates.push(relative_path);
        }
        if let Ok(relative_path) = RelPath::new(path, path_style) {
            candidates.push(relative_path);
        }

        for relative_path in candidates {
            let project_path: ProjectPath = (snapshot.id(), relative_path.into_arc()).into();
            let settings = WorktreeSettings::get(Some((&project_path).into()), cx);
            if settings.is_path_excluded(&relative_path) {
                excluded = true;
                continue;
            }

            // The deepest existing entry decides: if the path sits inside an
            // ignored directory that hasn't been scanned yet, hand the model the
            // working forms to retry with. (Once such a directory is scanned its
            // entries exist and resolution succeeds, so a miss there is a genuine
            // "not found".)
            let Some(deepest_entry) = relative_path
                .ancestors()
                .find_map(|ancestor| snapshot.entry_for_path(ancestor))
            else {
                continue;
            };
            if !(deepest_entry.is_ignored && deepest_entry.kind.is_unloaded()) {
                continue;
            }

            for form in [
                worktree
                    .root_name()
                    .join(&relative_path)
                    .display(snapshot.path_style())
                    .to_string(),
                snapshot.absolutize(&relative_path).display().to_string(),
            ] {
                if !suggestions.contains(&form) {
                    suggestions.push(form);
                }
            }
        }
    }

    if excluded {
        return Some(format!(
            "`{}` matches the user's `file_scan_exclusions` setting and can't be accessed.",
            path.display()
        ));
    }

    if suggestions.is_empty() {
        return None;
    }

    let forms = suggestions
        .iter()
        .map(|suggestion| format!("`{suggestion}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "`{}` is inside a gitignored directory the project hasn't scanned, so it can't be resolved as a bare project-relative path. Use one of: {forms}.",
        path.display()
    ))
}

/// Resolves a path within the project, checking for symlink escapes.
///
/// This is the primary entry point for agent tools that need to resolve a
/// user-provided path string into a validated `ProjectPath`. It combines
/// path lookup (`find_project_path`) with symlink safety verification.
///
/// `canonical_worktree_roots` should be obtained from
/// [`canonicalize_worktree_roots`] before calling this function so that no
/// blocking I/O is needed here.
///
/// # Returns
///
/// - `Ok(ResolvedProjectPath::Safe(project_path))` — the path resolves to a
///   location within the project boundaries.
/// - `Ok(ResolvedProjectPath::SymlinkEscape { .. })` — the path resolves
///   through a symlink to a location outside the project. Agent tools should
///   prompt the user before proceeding.
/// - `Err(..)` — the path could not be found in the project or could not be
///   verified. The error message is suitable for returning to the model.
pub fn resolve_project_path(
    project: &Project,
    path: impl AsRef<Path>,
    canonical_worktree_roots: &[PathBuf],
    cx: &App,
) -> Result<ResolvedProjectPath> {
    let path = path.as_ref();
    let project_path = project
        .find_project_path(path, cx)
        .ok_or_else(|| anyhow!("Path {} is not in the project", path.display()))?;

    let worktree = project
        .worktree_for_id(project_path.worktree_id, cx)
        .ok_or_else(|| anyhow!("Could not resolve path {}", path.display()))?;
    let snapshot = worktree.read(cx);

    // Fast path: if the entry exists in the snapshot and is not marked
    // external, we know it's safe (the background scanner already verified).
    if let Some(entry) = snapshot.entry_for_path(&project_path.path) {
        if !entry.is_external {
            return Ok(ResolvedProjectPath::Safe(project_path));
        }

        // Entry is external (set by the worktree scanner when a symlink's
        // canonical target is outside the worktree root). Return the
        // canonical path if the entry has one, otherwise fall through to
        // filesystem-level canonicalization.
        if let Some(canonical) = &entry.canonical_path {
            if is_within_any_worktree(canonical.as_ref(), canonical_worktree_roots) {
                return Ok(ResolvedProjectPath::Safe(project_path));
            }

            return Ok(ResolvedProjectPath::SymlinkEscape {
                project_path,
                canonical_target: canonical.to_path_buf(),
            });
        }
    }

    // For missing/create-mode paths (or external descendants without their own
    // canonical_path), resolve symlink safety through snapshot metadata rather
    // than std::fs canonicalization. This keeps behavior correct for non-local
    // worktrees and in-memory fs backends.
    for ancestor in project_path.path.ancestors() {
        let Some(ancestor_entry) = snapshot.entry_for_path(ancestor) else {
            continue;
        };

        if !ancestor_entry.is_external {
            return Ok(ResolvedProjectPath::Safe(project_path));
        }

        let Some(canonical_ancestor) = ancestor_entry.canonical_path.as_ref() else {
            continue;
        };

        let suffix = project_path.path.strip_prefix(ancestor).map_err(|_| {
            anyhow!(
                "Path {} could not be resolved in the project",
                path.display()
            )
        })?;

        let canonical_target = if suffix.is_empty() {
            canonical_ancestor.to_path_buf()
        } else {
            canonical_ancestor.join(suffix.as_std_path())
        };

        if is_within_any_worktree(&canonical_target, canonical_worktree_roots) {
            return Ok(ResolvedProjectPath::Safe(project_path));
        }

        return Ok(ResolvedProjectPath::SymlinkEscape {
            project_path,
            canonical_target,
        });
    }

    Ok(ResolvedProjectPath::Safe(project_path))
}

/// The textual forms of a model-supplied path that permission rules should be
/// matched against: the path as written, plus the worktree-relative and
/// absolute forms when it resolves into a worktree.
///
/// Rules match tool input text, so matching all of these lets a rule target any
/// project (a bare-relative pattern such as `^src/`) or one specific project
/// (an absolute pattern such as `^/Users/me/proj/src/`) regardless of how the
/// model spelled the path.
pub fn permission_path_forms(project: &Project, path: &Path, cx: &App) -> Vec<String> {
    let mut forms = vec![path.to_string_lossy().into_owned()];

    // Expand a `~`-prefixed path so `~/.agents/skills/...` and
    // `/Users/me/.agents/skills/...` can share one rule.
    if let Some(expanded) = expand_home_prefix(path) {
        let expanded = normalize_path(&expanded).to_string_lossy().into_owned();
        if !forms.contains(&expanded) {
            forms.push(expanded);
        }
    }

    if let Some(project_path) = project.find_project_path(path, cx) {
        let relative = project_path
            .path
            .display(project.path_style(cx))
            .to_string();
        if !forms.contains(&relative) {
            forms.push(relative);
        }

        if let Some(absolute) = project.absolute_path(&project_path, cx) {
            let absolute = absolute.to_string_lossy().into_owned();
            if !forms.contains(&absolute) {
                forms.push(absolute);
            }
        }
    }

    forms
}

/// Prompts the user for permission when a path resolves through a symlink to a
/// location outside the project. This check is an additional gate after
/// settings-based deny decisions: even if a tool is configured as "always allow,"
/// a symlink escape still requires explicit user approval.
pub fn authorize_symlink_access(
    tool_name: &str,
    display_path: &str,
    canonical_target: &Path,
    event_stream: &ToolCallEventStream,
    cx: &mut App,
) -> Task<Result<()>> {
    let title = format!(
        "`{}` points outside the project (symlink to `{}`)",
        display_path,
        canonical_target.display(),
    );

    let context = ToolPermissionContext::symlink_target(
        tool_name,
        vec![canonical_target.display().to_string()],
    );

    event_stream.authorize_always_prompt(title, context, cx)
}

pub fn authorize_with_sensitive_settings(
    kind: Option<SensitiveSettingsKind>,
    context: ToolPermissionContext,
    title: &str,
    event_stream: &ToolCallEventStream,
    cx: &mut App,
) -> Task<Result<()>> {
    match kind {
        Some(SensitiveSettingsKind::Local) => {
            event_stream.authorize_always_prompt(format!("{title} (local settings)"), context, cx)
        }
        Some(SensitiveSettingsKind::Global) => {
            event_stream.authorize_always_prompt(format!("{title} (settings)"), context, cx)
        }
        Some(SensitiveSettingsKind::AgentSkills) => event_stream.authorize_always_prompt(
            format!("{title} (agent skills)"),
            context.for_agent_skills(),
            cx,
        ),
        None => event_stream.authorize(title, context, cx),
    }
}

/// Creates a single authorization prompt for multiple symlink escapes.
/// Each escape is a `(display_path, canonical_target)` pair.
///
/// Accepts `&[(&str, PathBuf)]` to match the natural return type of
/// [`detect_symlink_escape`], avoiding intermediate owned-to-borrowed
/// conversions at call sites.
pub fn authorize_symlink_escapes(
    tool_name: &str,
    escapes: &[(&str, PathBuf)],
    event_stream: &ToolCallEventStream,
    cx: &mut App,
) -> Task<Result<()>> {
    debug_assert!(!escapes.is_empty());

    if escapes.len() == 1 {
        return authorize_symlink_access(tool_name, escapes[0].0, &escapes[0].1, event_stream, cx);
    }

    let targets = escapes
        .iter()
        .map(|(path, target)| format!("`{}` → `{}`", path, target.display()))
        .collect::<Vec<_>>()
        .join(" and ");
    let title = format!("{} (symlinks outside project)", targets);

    let context = ToolPermissionContext::symlink_target(
        tool_name,
        escapes
            .iter()
            .map(|(_, target)| target.display().to_string())
            .collect(),
    );

    event_stream.authorize_always_prompt(title, context, cx)
}

/// Checks whether a path escapes the project via symlink, without creating
/// an authorization task. Useful for pre-filtering paths before settings checks.
pub fn path_has_symlink_escape(
    project: &Project,
    path: impl AsRef<Path>,
    canonical_worktree_roots: &[PathBuf],
    cx: &App,
) -> bool {
    matches!(
        resolve_project_path(project, path, canonical_worktree_roots, cx),
        Ok(ResolvedProjectPath::SymlinkEscape { .. })
    )
}

/// Collects symlink escape info for a path without creating an authorization task.
/// Returns `Some((display_path, canonical_target))` if the path escapes via symlink.
pub fn detect_symlink_escape<'a>(
    project: &Project,
    display_path: &'a str,
    canonical_worktree_roots: &[PathBuf],
    cx: &App,
) -> Option<(&'a str, PathBuf)> {
    match resolve_project_path(project, display_path, canonical_worktree_roots, cx).ok()? {
        ResolvedProjectPath::Safe(_) => None,
        ResolvedProjectPath::SymlinkEscape {
            canonical_target, ..
        } => Some((display_path, canonical_target)),
    }
}

/// Collects symlink escape info for two paths (source and destination) and
/// returns any escapes found. This deduplicates the common pattern used by
/// tools that operate on two paths (copy, move).
///
/// Returns a `Vec` of `(display_path, canonical_target)` pairs for paths
/// that escape the project via symlink. The returned vec borrows the display
/// paths from the input strings.
pub fn collect_symlink_escapes<'a>(
    project: &Project,
    source_path: &'a str,
    destination_path: &'a str,
    canonical_worktree_roots: &[PathBuf],
    cx: &App,
) -> Vec<(&'a str, PathBuf)> {
    let mut escapes = Vec::new();
    if let Some(escape) = detect_symlink_escape(project, source_path, canonical_worktree_roots, cx)
    {
        escapes.push(escape);
    }
    if let Some(escape) =
        detect_symlink_escape(project, destination_path, canonical_worktree_roots, cx)
    {
        escapes.push(escape);
    }
    escapes
}

/// Checks authorization for file edits, handling symlink escapes and
/// sensitive settings paths.
///
/// # Authorization precedence
///
/// When a symlink escape is detected, the symlink authorization prompt
/// *replaces* (rather than supplements) the normal tool-permission prompt.
/// This is intentional: the symlink prompt already requires explicit user
/// approval and displays the canonical target, which provides strictly more
/// security-relevant information than the generic tool confirmation. Requiring
/// two sequential prompts for the same operation would degrade UX without
/// meaningfully improving security, since the user must already approve the
/// more specific symlink-escape prompt.
pub fn authorize_file_edit(
    tool_name: &str,
    path: &Path,
    thread: &WeakEntity<Thread>,
    event_stream: &ToolCallEventStream,
    cx: &mut App,
) -> Task<Result<()>> {
    let path_str = path.to_string_lossy();

    let settings = agent_settings::AgentSettings::get_global(cx);
    let forms = thread
        .read_with(cx, |thread, cx| {
            permission_path_forms(thread.project().read(cx), path, cx)
        })
        .unwrap_or_else(|_| vec![path_str.to_string()]);
    let decision = decide_permission_for_path_forms(tool_name, &forms, settings);

    if let ToolPermissionDecision::Deny(reason) = decision {
        return Task::ready(Err(anyhow!("{}", reason)));
    }

    let path_owned = path.to_path_buf();
    let title = format!("Edit {}", util::markdown::MarkdownInlineCode(&path_str));
    let tool_name = tool_name.to_string();
    let thread = thread.clone();
    let event_stream = event_stream.clone();

    // The raw-path sensitivity checks are synchronous (pure path inspection).
    // We still have to spawn anyway to resolve symlink escapes against the
    // worktree, but we can short-circuit straight to the appropriate
    // SensitiveSettingsKind on these fast paths and skip the async
    // `sensitive_settings_kind` canonicalization step below.
    let local_settings_folder = paths::local_settings_folder_name();
    let is_local_settings = path.components().any(|component| {
        component_matches_ignore_ascii_case(component.as_os_str(), local_settings_folder)
    });
    let is_agents_skills = is_agents_skills_path(path);

    cx.spawn(async move |cx| {
        // Resolve the path and check for symlink escapes.
        let (project_entity, fs) = thread.read_with(cx, |thread, cx| {
            let project = thread.project().clone();
            let fs = project.read(cx).fs().clone();
            (project, fs)
        })?;

        let canonical_roots = canonicalize_worktree_roots(&project_entity, &fs, cx).await;

        let resolved = project_entity.read_with(cx, |project, cx| {
            resolve_project_path(project, &path_owned, &canonical_roots, cx)
        });

        if let Ok(ResolvedProjectPath::SymlinkEscape {
            canonical_target, ..
        }) = &resolved
        {
            let authorize = cx.update(|cx| {
                authorize_symlink_access(
                    &tool_name,
                    &path_owned.to_string_lossy(),
                    canonical_target,
                    &event_stream,
                    cx,
                )
            });
            return authorize.await;
        }

        // Create-mode paths may not resolve yet, so also inspect the parent path
        // for symlink escapes before applying settings-based allow decisions.
        if resolved.is_err() {
            // An out-of-project path never goes through the worktree, so the
            // snapshot can't flag a symlink along the way: resolve it here and
            // show the real target. This replaces (does not supplement) the
            // generic tool prompt — the symlink case is strictly more specific.
            if let Some(canonical_target) = external_symlink_target(&path_owned, fs.as_ref()).await
            {
                let authorize = cx.update(|cx| {
                    authorize_symlink_access(
                        &tool_name,
                        &path_owned.to_string_lossy(),
                        &canonical_target,
                        &event_stream,
                        cx,
                    )
                });
                return authorize.await;
            }

            if let Some(parent_path) = path_owned.parent() {
                let parent_resolved = project_entity.read_with(cx, |project, cx| {
                    resolve_project_path(project, parent_path, &canonical_roots, cx)
                });

                if let Ok(ResolvedProjectPath::SymlinkEscape {
                    canonical_target, ..
                }) = &parent_resolved
                {
                    let authorize = cx.update(|cx| {
                        authorize_symlink_access(
                            &tool_name,
                            &path_owned.to_string_lossy(),
                            canonical_target,
                            &event_stream,
                            cx,
                        )
                    });
                    return authorize.await;
                }
            }
        }

        let explicitly_allowed = matches!(decision, ToolPermissionDecision::Allow);

        // Check sensitive settings asynchronously. Short-circuit on the
        // raw-path fast paths to skip the canonicalization in
        // `sensitive_settings_kind`; the slow path still runs for paths
        // that don't trivially look sensitive, so `..` traversal and
        // intra-project-symlink bypasses are still caught there.
        let settings_kind = if is_local_settings {
            Some(SensitiveSettingsKind::Local)
        } else if is_agents_skills {
            Some(SensitiveSettingsKind::AgentSkills)
        } else {
            sensitive_settings_kind(&path_owned, &canonical_roots, fs.as_ref()).await
        };

        let is_sensitive = settings_kind.is_some();
        if explicitly_allowed && !is_sensitive {
            return Ok(());
        }

        match settings_kind {
            Some(SensitiveSettingsKind::Local) => {
                let authorize = cx.update(|cx| {
                    let context = ToolPermissionContext::new(
                        &tool_name,
                        vec![path_owned.to_string_lossy().to_string()],
                    );
                    event_stream.authorize_always_prompt(
                        format!("{title} (local settings)"),
                        context,
                        cx,
                    )
                });
                return authorize.await;
            }
            Some(SensitiveSettingsKind::Global) => {
                let authorize = cx.update(|cx| {
                    let context = ToolPermissionContext::new(
                        &tool_name,
                        vec![path_owned.to_string_lossy().to_string()],
                    );
                    event_stream.authorize_always_prompt(format!("{title} (settings)"), context, cx)
                });
                return authorize.await;
            }
            Some(SensitiveSettingsKind::AgentSkills) => {
                let authorize = cx.update(|cx| {
                    let context = ToolPermissionContext::new(
                        &tool_name,
                        vec![path_owned.to_string_lossy().to_string()],
                    )
                    .for_agent_skills();
                    event_stream.authorize_always_prompt(
                        format!("{title} (agent skills)"),
                        context,
                        cx,
                    )
                });
                return authorize.await;
            }
            None => {}
        }

        match resolved {
            Ok(_) => Ok(()),
            Err(_) => {
                let authorize = cx.update(|cx| {
                    let context = ToolPermissionContext::new(
                        &tool_name,
                        vec![path_owned.to_string_lossy().to_string()],
                    );
                    event_stream.authorize(&title, context, cx)
                });
                authorize.await
            }
        }
    })
}

/// The user's choice when prompted about how to handle unsaved changes
/// in a buffer that the agent wants to edit or overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyBufferDecision {
    /// Save the buffer's pending edits to disk, then proceed.
    /// (Edit-mode prompt only.)
    Save,
    /// Discard the buffer's pending edits (reload from disk), then proceed.
    Discard,
    /// Keep the buffer's pending edits and cancel the agent's operation.
    /// (Overwrite-mode prompt only.)
    Keep,
}

/// Which prompt to show when the agent encounters a dirty buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyBufferPromptKind {
    /// The agent wants to apply targeted edits on top of the current
    /// content. Offers Save (persist edits, then edit on top) vs Discard
    /// (revert to disk, then edit).
    Edit,
    /// The agent wants to overwrite the file's entire contents. Offers
    /// Keep (cancel the overwrite to preserve the user's work) vs
    /// Discard (reload from disk and let the agent overwrite).
    Overwrite,
}

/// Prompts the user about how to handle a dirty buffer that the agent
/// wants to edit or overwrite. Returns the chosen action; the caller is
/// responsible for actually performing the corresponding side effect
/// (save / reload / cancel) before continuing.
pub fn authorize_dirty_buffer(
    kind: DirtyBufferPromptKind,
    event_stream: &ToolCallEventStream,
    cx: &mut App,
) -> Task<Result<DirtyBufferDecision>> {
    let (message, options) = match kind {
        DirtyBufferPromptKind::Edit => (
            "This file has unsaved changes. Do you want to save or discard them \
             before the agent continues editing?"
                .to_string(),
            vec![
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("save"),
                    "Save",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("discard"),
                    "Discard",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ],
        ),
        DirtyBufferPromptKind::Overwrite => (
            "This file has unsaved changes and the agent wants to overwrite it.".to_string(),
            vec![
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("discard"),
                    "Overwrite",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    acp::PermissionOptionId::new("keep"),
                    "Cancel",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ],
        ),
    };

    let prompt = event_stream.prompt_for_decision(None, Some(message), options, cx);
    cx.spawn(async move |_cx| {
        let option_id = prompt.await?;
        match option_id.0.as_ref() {
            "save" => Ok(DirtyBufferDecision::Save),
            "discard" => Ok(DirtyBufferDecision::Discard),
            "keep" => Ok(DirtyBufferDecision::Keep),
            other => Err(anyhow!(
                "Unexpected dirty-buffer decision option_id: {other}"
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs;
    use gpui::TestAppContext;
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

    async fn worktree_roots(
        project: &Entity<Project>,
        fs: &Arc<dyn Fs>,
        cx: &TestAppContext,
    ) -> Vec<PathBuf> {
        let abs_paths: Vec<Arc<Path>> = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .map(|wt| wt.read(cx).abs_path())
                .collect()
        });

        let mut roots = Vec::with_capacity(abs_paths.len());
        for p in &abs_paths {
            match fs.canonicalize(p).await {
                Ok(c) => roots.push(c),
                Err(_) => roots.push(p.to_path_buf()),
            }
        }
        roots
    }

    #[test]
    fn test_real_fs_external_path_resolution_and_protection() {
        // A plain (non-gpui) test: `RealFs` runs real OS threads, which the
        // deterministic gpui test scheduler forbids. This exercises the exact
        // canonicalization real users hit (e.g. macOS rewriting `/tmp` to
        // `/private/tmp`), verifying that a path outside the worktree resolves
        // as external, is not treated as protected, and survives the same
        // delete steps `delete_path` performs.
        let worktree_dir = tempfile::tempdir().expect("create worktree temp dir");
        let outside_dir = tempfile::tempdir().expect("create external temp dir");
        let doomed_file = outside_dir.path().join("doomed.txt");
        std::fs::write(&doomed_file, "bye").expect("write external file");

        let dispatcher = Arc::new(gpui::ThreadedDispatcher::new());
        let executor = gpui::BackgroundExecutor::new(dispatcher.clone());
        let fs = fs::RealFs::new(None, executor.clone());

        let canonical_roots = vec![
            futures::executor::block_on(fs.canonicalize(worktree_dir.path()))
                .expect("canonicalize worktree"),
        ];

        let resolved = futures::executor::block_on(resolve_external_path(
            &doomed_file,
            &canonical_roots,
            fs.as_ref(),
        ))
        .expect("external path should resolve");
        assert!(
            !is_within_any_worktree(&resolved, &canonical_roots),
            "resolved path must be outside every worktree"
        );
        assert!(
            !is_protected_external_path(&resolved, &canonical_roots),
            "external file must not be treated as protected: {}",
            resolved.display()
        );
        assert!(
            !futures::executor::block_on(resolves_to_global_skills_dir(&doomed_file, fs.as_ref())),
            "external file must not be mistaken for the global skills dir"
        );

        // The same removal `delete_path` performs on the resolved path.
        futures::executor::block_on(fs.remove_file(&resolved, fs::RemoveOptions::default()))
            .expect("delete external file");
        assert!(
            !doomed_file.exists(),
            "external file should have been deleted"
        );

        // `RealFs` keeps long-lived watcher dispatch tasks on the background
        // executor; dropping them blocks the test thread, so leak them (the
        // test process exits anyway once the main harness thread returns).
        std::mem::forget(fs);
        std::mem::forget(executor);
        std::mem::forget(dispatcher);
    }

    #[gpui::test]
    async fn test_resolve_creatable_global_skill_path_allows_tilde_path(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill");
        let expected_path = agent_skills::global_skills_dir().join("my-skill");

        let resolved = resolve_creatable_global_skill_path(&input_path, fs.as_ref())
            .await
            .expect("global skill path should resolve");

        assert_eq!(resolved, expected_path);
    }

    #[gpui::test]
    async fn test_resolve_global_skill_path_allows_tilde_path(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let skill_file = agent_skills::global_skills_dir()
            .join("my-skill")
            .join("SKILL.md");
        fs.insert_tree(
            skill_file
                .parent()
                .expect("skill file should have a parent"),
            json!({ "SKILL.md": "---\nname: my-skill\ndescription: test\n---" }),
        )
        .await;

        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .join("SKILL.md");
        let resolved = resolve_global_skill_path(&input_path, fs.as_ref())
            .await
            .expect("global skill file should resolve");

        assert_eq!(resolved, skill_file);
    }

    #[gpui::test]
    async fn test_resolve_global_skill_path_allows_symlinked_skill_dir(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let skills_dir = agent_skills::global_skills_dir();
        fs.insert_tree(
            path!("/external/my-skill"),
            json!({
                "SKILL.md": "---\nname: my-skill\ndescription: test\n---",
                "references": { "guide.md": "details" }
            }),
        )
        .await;
        fs.create_dir(&skills_dir)
            .await
            .expect("global skills directory should be created");
        fs.create_symlink(
            &skills_dir.join("my-skill"),
            PathBuf::from(path!("/external/my-skill")),
        )
        .await
        .expect("skill directory should be symlinked");

        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .join("references")
            .join("guide.md");
        let resolved = resolve_global_skill_path(&input_path, fs.as_ref())
            .await
            .expect("symlinked global skill resource should resolve");

        assert_eq!(
            resolved,
            PathBuf::from(path!("/external/my-skill/references/guide.md"))
        );
    }

    #[gpui::test]
    async fn test_resolve_global_skill_path_rejects_escape_from_symlinked_skill_dir(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let skills_dir = agent_skills::global_skills_dir();
        fs.insert_tree(
            path!("/external/my-skill"),
            json!({
                "SKILL.md": "---\nname: my-skill\ndescription: test\n---",
            }),
        )
        .await;
        fs.insert_tree(path!("/private"), json!({ "secret.txt": "secret" }))
            .await;
        fs.create_symlink(
            &PathBuf::from(path!("/external/my-skill/secret")),
            PathBuf::from(path!("/private")),
        )
        .await
        .expect("nested symlink should be created");
        fs.create_dir(&skills_dir)
            .await
            .expect("global skills directory should be created");
        fs.create_symlink(
            &skills_dir.join("my-skill"),
            PathBuf::from(path!("/external/my-skill")),
        )
        .await
        .expect("skill directory should be symlinked");

        let input_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("my-skill")
            .join("secret")
            .join("secret.txt");

        assert!(
            resolve_global_skill_path(&input_path, fs.as_ref())
                .await
                .is_none(),
            "nested symlinks inside a symlinked skill must not broaden global skill access",
        );
    }

    #[gpui::test]
    async fn test_resolve_creatable_global_skill_path_rejects_other_home_paths(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let sibling_path = PathBuf::from("~").join(".agents").join("not-skills");
        let escaped_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("..")
            .join("not-skills");

        assert!(
            resolve_creatable_global_skill_path(&sibling_path, fs.as_ref())
                .await
                .is_none()
        );
        assert!(
            resolve_creatable_global_skill_path(&escaped_path, fs.as_ref())
                .await
                .is_none()
        );
    }

    #[gpui::test]
    async fn test_resolve_creatable_global_skill_path_rejects_symlink_escape(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let skills_dir = agent_skills::global_skills_dir();
        fs.create_dir(&skills_dir)
            .await
            .expect("global skills directory should be created");
        fs.create_dir(path!("/external").as_ref())
            .await
            .expect("external directory should be created");
        fs.create_symlink(&skills_dir.join("link"), PathBuf::from(path!("/external")))
            .await
            .expect("symlink should be created");

        let escaped_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("link")
            .join("new-dir");

        assert!(
            resolve_creatable_global_skill_path(&escaped_path, fs.as_ref())
                .await
                .is_none()
        );
    }

    #[gpui::test]
    async fn test_global_skill_path_resolvers_reject_absolute_paths_when_skills_dir_is_symlink_to_root(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(paths::home_dir(), json!({ ".agents": {} }))
            .await;
        fs.insert_tree(path!("/tmp"), json!({ "outside.txt": "outside" }))
            .await;

        let skills_dir = agent_skills::global_skills_dir();
        fs.create_symlink(&skills_dir, PathBuf::from(path!("/")))
            .await
            .expect("global skills directory should be symlinked to root");

        let outside_path = PathBuf::from(path!("/tmp/outside.txt"));
        assert!(
            resolve_global_skill_path(&outside_path, fs.as_ref())
                .await
                .is_none(),
            "existing absolute paths outside the lexical global skills tree should not resolve",
        );
        assert!(
            resolve_creatable_global_skill_path(&outside_path, fs.as_ref())
                .await
                .is_none(),
            "creatable absolute paths outside the lexical global skills tree should not resolve",
        );

        let traversed_path = PathBuf::from("~")
            .join(".agents")
            .join("skills")
            .join("..")
            .join("outside");
        assert!(
            resolve_creatable_global_skill_path(&traversed_path, fs.as_ref())
                .await
                .is_none(),
            "paths that normalize outside the lexical global skills tree should not resolve",
        );
    }

    #[gpui::test]
    async fn test_global_skill_path_resolvers_reject_absolute_paths_when_skills_dir_is_symlink_to_home(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            paths::home_dir(),
            json!({
                ".agents": {},
                "outside.txt": "outside",
            }),
        )
        .await;

        let skills_dir = agent_skills::global_skills_dir();
        fs.create_symlink(&skills_dir, paths::home_dir().clone())
            .await
            .expect("global skills directory should be symlinked to home");

        let outside_path = paths::home_dir().join("outside.txt");
        assert!(
            resolve_global_skill_path(&outside_path, fs.as_ref())
                .await
                .is_none(),
            "existing absolute paths outside the lexical global skills tree should not resolve",
        );
        assert!(
            resolve_creatable_global_skill_path(&outside_path, fs.as_ref())
                .await
                .is_none(),
            "creatable absolute paths outside the lexical global skills tree should not resolve",
        );
    }

    #[gpui::test]
    async fn test_resolve_project_path_safe_for_normal_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root/project"),
            json!({
                "src": {
                    "main.rs": "fn main() {}",
                    "lib.rs": "pub fn hello() {}"
                },
                "README.md": "# Project"
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.run_until_parked();
        let fs_arc: Arc<dyn Fs> = fs;
        let roots = worktree_roots(&project, &fs_arc, cx).await;

        cx.read(|cx| {
            let project = project.read(cx);

            let resolved = resolve_project_path(project, "project/src/main.rs", &roots, cx)
                .expect("should resolve normal file");
            assert!(
                matches!(resolved, ResolvedProjectPath::Safe(_)),
                "normal file should be Safe, got: {:?}",
                resolved
            );

            let resolved = resolve_project_path(project, "project/README.md", &roots, cx)
                .expect("should resolve readme");
            assert!(
                matches!(resolved, ResolvedProjectPath::Safe(_)),
                "readme should be Safe, got: {:?}",
                resolved
            );
        });
    }

    #[gpui::test]
    async fn test_resolve_project_path_detects_symlink_escape(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {
                    "src": {
                        "main.rs": "fn main() {}"
                    }
                },
                "external": {
                    "secret.txt": "top secret"
                }
            }),
        )
        .await;

        fs.create_symlink(path!("/root/project/link").as_ref(), "../external".into())
            .await
            .expect("should create symlink");

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.run_until_parked();
        let fs_arc: Arc<dyn Fs> = fs;
        let roots = worktree_roots(&project, &fs_arc, cx).await;

        cx.read(|cx| {
            let project = project.read(cx);

            let resolved = resolve_project_path(project, "project/link", &roots, cx)
                .expect("should resolve symlink path");
            match &resolved {
                ResolvedProjectPath::SymlinkEscape {
                    canonical_target, ..
                } => {
                    assert_eq!(
                        canonical_target,
                        Path::new(path!("/root/external")),
                        "canonical target should point to external directory"
                    );
                }
                ResolvedProjectPath::Safe(_) => {
                    panic!("symlink escaping project should be detected as SymlinkEscape");
                }
            }
        });
    }

    #[gpui::test]
    async fn test_resolve_project_path_allows_intra_project_symlinks(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root/project"),
            json!({
                "real_dir": {
                    "file.txt": "hello"
                }
            }),
        )
        .await;

        fs.create_symlink(path!("/root/project/link_dir").as_ref(), "real_dir".into())
            .await
            .expect("should create symlink");

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.run_until_parked();
        let fs_arc: Arc<dyn Fs> = fs;
        let roots = worktree_roots(&project, &fs_arc, cx).await;

        cx.read(|cx| {
            let project = project.read(cx);

            let resolved = resolve_project_path(project, "project/link_dir", &roots, cx)
                .expect("should resolve intra-project symlink");
            assert!(
                matches!(resolved, ResolvedProjectPath::Safe(_)),
                "intra-project symlink should be Safe, got: {:?}",
                resolved
            );
        });
    }

    #[gpui::test]
    async fn test_resolve_project_path_missing_child_under_external_symlink(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "project": {},
                "external": {
                    "existing.txt": "hello"
                }
            }),
        )
        .await;

        fs.create_symlink(path!("/root/project/link").as_ref(), "../external".into())
            .await
            .expect("should create symlink");

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.run_until_parked();
        let fs_arc: Arc<dyn Fs> = fs;
        let roots = worktree_roots(&project, &fs_arc, cx).await;

        cx.read(|cx| {
            let project = project.read(cx);

            let resolved = resolve_project_path(project, "project/link/new_dir", &roots, cx)
                .expect("should resolve missing child path under symlink");
            match resolved {
                ResolvedProjectPath::SymlinkEscape {
                    canonical_target, ..
                } => {
                    assert_eq!(
                        canonical_target,
                        Path::new(path!("/root/external/new_dir")),
                        "missing child path should resolve to escaped canonical target",
                    );
                }
                ResolvedProjectPath::Safe(_) => {
                    panic!("missing child under external symlink should be SymlinkEscape");
                }
            }
        });
    }

    #[gpui::test]
    async fn test_resolve_project_path_allows_cross_worktree_symlinks(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "worktree_one": {},
                "worktree_two": {
                    "shared_dir": {
                        "file.txt": "hello"
                    }
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/worktree_one/link_to_worktree_two").as_ref(),
            PathBuf::from("../worktree_two/shared_dir"),
        )
        .await
        .expect("should create symlink");

        let project = Project::test(
            fs.clone(),
            [
                path!("/root/worktree_one").as_ref(),
                path!("/root/worktree_two").as_ref(),
            ],
            cx,
        )
        .await;
        cx.run_until_parked();
        let fs_arc: Arc<dyn Fs> = fs;
        let roots = worktree_roots(&project, &fs_arc, cx).await;

        cx.read(|cx| {
            let project = project.read(cx);

            let resolved =
                resolve_project_path(project, "worktree_one/link_to_worktree_two", &roots, cx)
                    .expect("should resolve cross-worktree symlink");
            assert!(
                matches!(resolved, ResolvedProjectPath::Safe(_)),
                "cross-worktree symlink should be Safe, got: {:?}",
                resolved
            );
        });
    }

    #[gpui::test]
    async fn test_resolve_project_path_missing_child_under_cross_worktree_symlink(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "worktree_one": {},
                "worktree_two": {
                    "shared_dir": {}
                }
            }),
        )
        .await;

        fs.create_symlink(
            path!("/root/worktree_one/link_to_worktree_two").as_ref(),
            PathBuf::from("../worktree_two/shared_dir"),
        )
        .await
        .expect("should create symlink");

        let project = Project::test(
            fs.clone(),
            [
                path!("/root/worktree_one").as_ref(),
                path!("/root/worktree_two").as_ref(),
            ],
            cx,
        )
        .await;
        cx.run_until_parked();
        let fs_arc: Arc<dyn Fs> = fs;
        let roots = worktree_roots(&project, &fs_arc, cx).await;

        cx.read(|cx| {
            let project = project.read(cx);

            let resolved = resolve_project_path(
                project,
                "worktree_one/link_to_worktree_two/new_dir",
                &roots,
                cx,
            )
            .expect("should resolve missing child under cross-worktree symlink");
            assert!(
                matches!(resolved, ResolvedProjectPath::Safe(_)),
                "missing child under cross-worktree symlink should be Safe, got: {:?}",
                resolved
            );
        });
    }
}
