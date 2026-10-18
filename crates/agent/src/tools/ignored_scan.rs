//! On-demand scanning of gitignored directories for the file tools.
//!
//! Zed's scanner skips gitignored directories (`node_modules`, `dist`,
//! `target`), so their contents have no snapshot entry until something asks for
//! them. These helpers let a tool ask — but only when the model named into the
//! directory, so an ordinary project operation never pulls in a whole ignored
//! tree.

use futures::StreamExt as _;
use gpui::{App, AsyncApp, Entity};
use project::{Project, ProjectPath, WorktreeId};
use std::path::Path;
use std::sync::Arc;
use util::rel_path::{RelPath, RelPathBuf};

/// The gitignored directory that keeps a bare relative `path` from resolving:
/// the deepest snapshot entry at or above the path, when that entry is a
/// gitignored directory the scanner left unloaded.
///
/// `rel_path` itself counts as its own ancestor, so naming an ignored directory
/// directly is covered as well as naming something inside it.
///
/// Only bare relative paths qualify. A rooted or absolute path resolves without
/// needing an entry (see `Project::find_project_path`), and a path that already
/// resolves needs nothing — so this never scans speculatively.
fn ignored_unscanned_ancestor(
    project: &Project,
    path: &Path,
    cx: &App,
) -> Option<(WorktreeId, Arc<RelPath>)> {
    if super::tool_permissions::is_absolute_or_rooted(path)
        || path.to_string_lossy().starts_with('~')
    {
        return None;
    }
    if project.find_project_path(path, cx).is_some() {
        return None;
    }

    let relative_path = RelPath::new(path, project.path_style(cx)).ok()?;
    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let snapshot = worktree.snapshot();
        let Some(deepest_entry) = relative_path
            .ancestors()
            .find_map(|ancestor| snapshot.entry_for_path(ancestor))
        else {
            continue;
        };
        if deepest_entry.is_ignored && deepest_entry.kind.is_unloaded() {
            return Some((snapshot.id(), deepest_entry.path.clone()));
        }
    }

    None
}

/// Scans the gitignored directory that keeps a bare relative `path` from
/// resolving, and waits for the scan. Returns whether a scan ran.
///
/// Callers run this before resolving, so a bare relative path into a gitignored
/// directory resolves like the rooted/absolute form instead of bouncing the
/// model back with a hint. It is a no-op for paths that already resolve and for
/// paths whose parent is not an unscanned gitignored directory.
pub async fn prescan_ignored_ancestor(
    project: &Entity<Project>,
    path: &Path,
    cx: &mut AsyncApp,
) -> bool {
    let Some((worktree_id, prefix)) = project.read_with(cx, |project, cx| {
        ignored_unscanned_ancestor(project, path, cx)
    }) else {
        return false;
    };

    scan_ignored_prefix(project, worktree_id, prefix, cx).await
}

/// Scans the gitignored directory that keeps an already-resolved `project_path`
/// from having snapshot entries, and waits for the scan. Returns whether a scan
/// ran.
///
/// Callers that already hold a `ProjectPath` use this instead of
/// [`prescan_ignored_ancestor`]. The rooted and absolute forms reach a
/// gitignored directory through `Project::find_project_path`, which resolves
/// them *without* an entry — so by the time such a path is resolved there is
/// nothing left that tells the string-based helper it is the one that needs a
/// scan. Working from the resolved path removes that dependency, so a path of
/// any form below an unloaded gitignored directory scans the same way.
///
/// A no-op when the path already has an entry, or when the deepest entry above
/// it is an ordinary (scanned) directory.
pub async fn prescan_ignored_ancestor_for_project_path(
    project: &Entity<Project>,
    project_path: &ProjectPath,
    cx: &mut AsyncApp,
) -> bool {
    let Some((worktree_id, prefix)) = project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(project_path.worktree_id, cx)?;
        let snapshot = worktree.read(cx).snapshot();
        // `RelPath::ancestors` yields the path itself first, so this covers both
        // a requested ignored directory and a path below one.
        let entry = project_path
            .path
            .ancestors()
            .find_map(|ancestor| snapshot.entry_for_path(ancestor))?;
        (entry.is_ignored && entry.kind.is_unloaded()).then(|| (snapshot.id(), entry.path.clone()))
    }) else {
        return false;
    };

    scan_ignored_prefix(project, worktree_id, prefix, cx).await
}

/// Registers `prefix` as a path prefix to scan on its worktree and waits for it.
async fn scan_ignored_prefix(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    prefix: Arc<RelPath>,
    cx: &mut AsyncApp,
) -> bool {
    let scan = project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(worktree_id, cx)?;
        let local = worktree.read(cx).as_local()?;
        Some(local.add_path_prefix_to_scan(prefix))
    });

    let Some(scan) = scan else {
        return false;
    };
    scan.into_future().await;
    true
}

/// The leading literal components of a glob (`backend/**/*.rs` → `backend`).
///
/// Empty when the glob starts with a wildcard or has an empty component. A
/// leading wildcard can match at any depth, so it names no directory in
/// particular.
pub fn glob_literal_prefix(glob: &str) -> RelPathBuf {
    let mut prefix = RelPathBuf::new();
    for component in glob
        .trim_start_matches(|c| c == '/' || c == '\\')
        .split(|c| c == '/' || c == '\\')
    {
        if component.is_empty() || component.contains(|c| matches!(c, '*' | '?' | '[' | '{')) {
            break;
        }
        if prefix.push_component(component).is_err() {
            break;
        }
    }
    prefix
}

/// Whether a glob's literal `prefix` names a path inside `dir_path` — i.e. the
/// glob explicitly reaches into the directory rather than merely not excluding
/// it.
///
/// `find_path` and `grep` use this to keep gitignored content out of ordinary
/// searches: a glob whose prefix does not enter an ignored directory leaves that
/// directory unloaded, so `node_modules` and friends are only ever scanned when
/// the model names into them.
pub fn glob_reaches_dir(dir_path: &RelPath, prefix: &RelPath) -> bool {
    !prefix.is_empty() && prefix.starts_with(dir_path)
}

/// Scans the gitignored directories that a glob's literal `prefix` reaches
/// into, and returns whether the prefix reaches any ignored directory at all.
///
/// Used by `grep`: unlike `find_path` it cannot scan directories itself with
/// `add_path_prefix_to_scan`, so a `grep` naming into gitignored content loads
/// that subtree before the search runs. An already-loaded ignored directory
/// still reports `true` (the search should look there) without scanning again.
pub async fn prescan_ignored_dirs_for_prefix(
    project: &Entity<Project>,
    prefix: &RelPath,
    cx: &mut AsyncApp,
) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let (reaches, dirs_to_scan) = project.read_with(cx, |project, cx| {
        let mut reaches = false;
        let mut dirs_to_scan = Vec::new();
        for worktree in project.worktrees(cx) {
            let (settings, snapshot) = {
                let worktree = worktree.read(cx);
                let Some(local) = worktree.as_local() else {
                    continue;
                };
                (local.settings(), worktree.snapshot())
            };
            for entry in snapshot.entries(true, 0) {
                if entry.is_ignored
                    && !settings.is_path_excluded(&entry.path)
                    && glob_reaches_dir(
                        snapshot.root_name().join(&entry.path).as_rel_path(),
                        prefix,
                    )
                {
                    reaches = true;
                    if entry.kind.is_unloaded() {
                        dirs_to_scan.push((worktree.clone(), entry.path.clone()));
                    }
                }
            }
        }
        (reaches, dirs_to_scan)
    });

    for (worktree, path) in dirs_to_scan {
        let scan = worktree.update(cx, |worktree, _| {
            worktree
                .as_local_mut()
                .map(|local| local.add_path_prefix_to_scan(path))
        });
        if let Some(scan) = scan {
            scan.into_future().await;
        }
    }
    reaches
}
