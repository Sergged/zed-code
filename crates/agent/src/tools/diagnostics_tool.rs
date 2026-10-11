use crate::{AgentTool, ToolCallEventStream, ToolInput};
use action_log::ActionLog;
use agent_client_protocol::schema::v1 as acp;
use futures::{Future, FutureExt as _};
use gpui::{App, AsyncApp, Entity, Task};
use language::{Buffer, DiagnosticSeverity, OffsetRangeExt};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::{fmt::Write, sync::Arc};
use ui::SharedString;
use util::markdown::MarkdownInlineCode;

type Result<T, E = String> = core::result::Result<T, E>;

/// Get errors, warnings, type errors and linter findings for the project or specific files.
///
/// This tool can be invoked after a series of edits to determine if further edits are necessary, or if the user asks to fix errors or warnings in their codebase.
///
/// It reports what the project's language servers report, which includes the project's configured linter (for Rust, clippy when it is set up as the check command). Running a compiler or linter in the terminal to learn about errors is therefore usually unnecessary — prefer this tool, and use the terminal for builds, tests, code generation, and anything else without a tool.
///
/// The tool opens the files it is given and connects them to the language servers itself, so you do not need to open them in the editor first. This works for any file in the project, including files you have not touched before.
///
/// When one or more paths are provided, shows all diagnostics for those specific files — pass every file you changed in a single call. When no path is provided, shows a summary of error and warning counts across the project.
///
/// The project-wide summary only covers files the language servers have already reported diagnostics for; it is not a full check of the repository. To check your own edits, always pass the files explicitly.
///
/// This tool attempts to refresh diagnostics before returning.
/// If refreshing diagnostics fails (for example, if the language server does not support pull-based diagnostics), it will return any diagnostics already present.
/// Note that, in this case, the results may be out-of-date, and may or may not reflect the most recent edits.
/// If this happens, do not attempt to re-run this tool in the hope that refreshing will later succeed. Failures are typically persistent.
///
/// <example>
/// To get diagnostics for specific files:
/// {
///     "paths": ["src/main.rs", "src/lib.rs"]
/// }
///
/// To get a project-wide diagnostic summary:
/// {}
/// </example>
///
/// <guidelines>
/// - "No errors or warnings" only means the code is clean. Other outcomes are not that:
///   - "has not been checked" means no language server has reported for that file yet, so the file was never verified. This is expected right after you move or rename a file, or right after an edit the server has not analyzed yet. Retry once, and if it is still unchecked, verify that file in the terminal and tell the user which files remain unverified.
///   - "no language server attached" means this tool cannot check the file at all (an unsupported language, or the server is not running). Use the terminal for it, and say so.
///   - "may be stale" means the refresh failed and the output may predate your edits. Verify in the terminal before reporting success.
///   Never explain an unexpected result as "the diagnostics were wrong" — the accurate statement is "this file was not checked".
/// - If you think you can fix a diagnostic, make 1-2 attempts and then give up.
/// - Don't remove code you've generated just because you can't fix an error. The user can help you fix it.
/// </guidelines>
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsToolInput {
    /// The paths to get diagnostics for. Pass every file you changed in one call. If not
    /// provided, returns a project-wide summary.
    ///
    /// These paths should never be absolute, and the first component
    /// of each path should always be a root directory in a project.
    ///
    /// <example>
    /// If the project has the following root directories:
    ///
    /// - lorem
    /// - ipsum
    ///
    /// If you wanna access diagnostics for `dolor.txt` in `ipsum`, you should use the path `ipsum/dolor.txt`.
    /// </example>
    pub paths: Vec<String>,
}

pub struct DiagnosticsTool {
    project: Entity<Project>,
    action_log: Entity<ActionLog>,
}

impl DiagnosticsTool {
    pub fn new(project: Entity<Project>, action_log: Entity<ActionLog>) -> Self {
        Self {
            project,
            action_log,
        }
    }
}

async fn with_cancellation<T>(f: impl Future<Output = T>, s: &ToolCallEventStream) -> Result<T> {
    futures::select! {
        result = f.fuse() => Ok(result),
        _ = s.cancelled_by_user().fuse() => {
            Err("Diagnostics cancelled by user".to_string())
        }
    }
}

/// Outcome of refreshing one file's diagnostics, as far as the tool can tell.
///
/// The distinction matters because "checked and clean" and "never checked" produce the same
/// empty diagnostic list, and only the first one means the code is clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshOutcome {
    /// The language server answered with a fresh report.
    Refreshed,
    /// The pull failed; whatever we print may predate the user's edits.
    Stale,
}

impl RefreshOutcome {
    fn message(self) -> &'static str {
        match self {
            Self::Refreshed => "Diagnostics successfully refreshed.",
            Self::Stale => "Failed to refresh diagnostics. Diagnostics may be stale.",
        }
    }
}

/// Why a file could not be verified, when it could not be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UncheckedReason {
    /// No language server is attached to the buffer, so this tool cannot check it.
    NoLanguageServer,
    /// Servers are attached, but none has reported for the buffer yet.
    NoReportYet,
}

impl UncheckedReason {
    fn message(self) -> &'static str {
        match self {
            Self::NoLanguageServer => {
                "No language server is attached to this file, so it was not checked. \
                 This tool cannot verify it — use the terminal instead, and tell the user."
            }
            Self::NoReportYet => {
                "No language server has reported diagnostics for this file yet — it has not \
                 been checked. This is expected right after the file was moved, renamed, or \
                 edited. Retry once; if it is still unchecked, verify it in the terminal and \
                 say which files remain unverified."
            }
        }
    }
}

/// Renders the per-file diagnostic lines for one entry per diagnostic.
fn format_diagnostics(entries: &[(DiagnosticSeverity, u32, String)], path: &str) -> String {
    let mut output = String::new();
    for (severity, line, message) in entries {
        let severity = match severity {
            &DiagnosticSeverity::ERROR => "error",
            &DiagnosticSeverity::WARNING => "warning",
            _ => continue,
        };
        writeln!(output, "{path}: {severity} at line {line}: {message}").ok();
    }
    output
}

/// Renders the result for a single file: either its diagnostics, or why it was not checked.
fn format_file_result(
    path: &str,
    entries: &[(DiagnosticSeverity, u32, String)],
    unchecked: Option<UncheckedReason>,
    refresh: RefreshOutcome,
) -> String {
    let mut output = format!("{}\n\n{}\n", path, refresh.message());

    match unchecked {
        Some(reason) => {
            let _ = writeln!(output, "{}", reason.message());
        }
        None if entries.is_empty() => {
            let _ = writeln!(output, "No errors or warnings.");
        }
        None => {
            let _ = write!(output, "{}", format_diagnostics(entries, path));
        }
    }

    output
}

/// Reads the diagnostics recorded on a buffer as `(severity, line, message)`.
///
/// Returns `None` when no language server has ever reported for this buffer, which is
/// materially different from a server reporting an empty set: the first means the file
/// was never checked, the second means it is clean.
fn read_buffer_diagnostics(buffer: &Buffer) -> Option<Vec<(DiagnosticSeverity, u32, String)>> {
    if !buffer.has_diagnostic_reports() {
        return None;
    }

    let snapshot = buffer.snapshot();
    let mut entries = Vec::new();
    for (_, group) in snapshot.diagnostic_groups(None) {
        let entry = &group.entries[group.primary_ix];
        let range = entry.range.to_point(&snapshot);
        if !matches!(
            entry.diagnostic.severity,
            DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING
        ) {
            continue;
        }
        entries.push((
            entry.diagnostic.severity,
            range.start.row + 1,
            entry.diagnostic.message.to_string(),
        ));
    }
    Some(entries)
}

/// Whether any language server is attached to the buffer.
fn has_language_server(project: &Project, buffer: &Buffer, cx: &App) -> bool {
    project
        .lsp_store()
        .read(cx)
        .language_server_ids_for_opened_buffer(buffer.remote_id())
        .is_some_and(|server_ids| !server_ids.is_empty())
}

/// Opens the buffer for `path` and connects it to the language servers.
///
/// `open_buffer` on its own does not register the buffer with any server: `didOpen` is only
/// sent from `register_buffer_with_language_servers`, which the editor calls for visible tabs
/// and the action log calls for files the agent reads or edits. Without it a pull returns
/// nothing for a file the agent has never touched, and the tool would report a clean file it
/// never checked.
async fn open_and_register_buffer(
    project: &Entity<Project>,
    action_log: &Entity<ActionLog>,
    path: &Path,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<Entity<Buffer>> {
    let open_buffer_task = project.update(cx, |project, cx| {
        let Some(project_path) = project.find_project_path(path, cx) else {
            return Err(format!("Could not find path {} in project", path.display()));
        };
        Ok(project.open_buffer(project_path, cx))
    })?;

    let buffer = with_cancellation(open_buffer_task, event_stream)
        .await?
        .map_err(|e| e.to_string())?;

    action_log.update(cx, |log, cx| {
        log.buffer_read(buffer.clone(), cx);
    });

    Ok(buffer)
}

/// Pulls fresh diagnostics for one file's buffer.
async fn pull_diagnostics_for_buffer(
    project: &Entity<Project>,
    buffer: Entity<Buffer>,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<RefreshOutcome> {
    let lsp_store = project.read_with(cx, |project, _cx| project.lsp_store());
    let pull_task = lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.pull_diagnostics_for_buffer(buffer, cx)
    });
    let pull_result = with_cancellation(pull_task, event_stream).await?;
    if let Err(error) = &pull_result {
        log::warn!("Failed to pull diagnostics, using cached: {error:#}");
    }
    Ok(match pull_result {
        Ok(()) => RefreshOutcome::Refreshed,
        Err(_) => RefreshOutcome::Stale,
    })
}

/// Triggers a workspace diagnostics pull on all running servers.
async fn pull_workspace_diagnostics(
    project: &Entity<Project>,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<RefreshOutcome> {
    let lsp_store = project.read_with(cx, |project, _cx| project.lsp_store());
    let pull_task = lsp_store.update(cx, |lsp_store, cx| {
        lsp_store.pull_workspace_diagnostics_once(cx)
    });
    let succeeded = with_cancellation(pull_task, event_stream).await?;
    if !succeeded {
        log::warn!("Failed to pull workspace diagnostics, using cached");
    }
    Ok(if succeeded {
        RefreshOutcome::Refreshed
    } else {
        RefreshOutcome::Stale
    })
}

impl AgentTool for DiagnosticsTool {
    type Input = DiagnosticsToolInput;
    type Output = String;

    const NAME: &'static str = "diagnostics";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        let paths = input
            .map(|input| input.paths)
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>();

        match paths.len() {
            0 => "Check project diagnostics".into(),
            1 => format!("Check diagnostics for {}", MarkdownInlineCode(&paths[0])).into(),
            _ => format!("Check diagnostics for {} files", paths.len()).into(),
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
            let paths = input
                .paths
                .into_iter()
                .filter(|path| !path.is_empty())
                .collect::<Vec<_>>();

            if paths.is_empty() {
                return project_summary(&project, &event_stream, cx).await;
            }

            let mut sections = Vec::with_capacity(paths.len());
            for path in &paths {
                let buffer = open_and_register_buffer(
                    &project,
                    &action_log,
                    Path::new(path),
                    &event_stream,
                    cx,
                )
                .await?;

                let refresh =
                    pull_diagnostics_for_buffer(&project, buffer.clone(), &event_stream, cx)
                        .await?;

                let section = project.read_with(cx, |project, cx| {
                    let unchecked = if has_language_server(project, buffer.read(cx), cx) {
                        read_buffer_diagnostics(buffer.read(cx))
                            .is_none()
                            .then_some(UncheckedReason::NoReportYet)
                    } else {
                        Some(UncheckedReason::NoLanguageServer)
                    };
                    let entries = read_buffer_diagnostics(buffer.read(cx)).unwrap_or_default();
                    format_file_result(path, &entries, unchecked, refresh)
                });

                sections.push(section);
            }

            Ok(sections.join("\n"))
        })
    }
}

/// Renders the project-wide summary.
///
/// This is a report of what the language servers have already checked, not a check of the
/// repository: files with no recorded diagnostics are absent from it by construction. Saying
/// "no errors in the project" here would be the same false-clean the per-file path avoids.
async fn project_summary(
    project: &Entity<Project>,
    event_stream: &ToolCallEventStream,
    cx: &mut AsyncApp,
) -> Result<String> {
    let refresh = pull_workspace_diagnostics(project, event_stream, cx).await?;

    let (lines, checked_any) = project.read_with(cx, |project, cx| {
        let mut lines = Vec::new();
        let mut checked_any = false;

        for (project_path, _, summary) in project.diagnostic_summaries(true, cx) {
            checked_any = true;
            if summary.error_count > 0 || summary.warning_count > 0 {
                let Some(worktree) = project.worktree_for_id(project_path.worktree_id, cx) else {
                    continue;
                };
                lines.push(format!(
                    "{}: {} error(s), {} warning(s)",
                    worktree.read(cx).absolutize(&project_path.path).display(),
                    summary.error_count,
                    summary.warning_count
                ));
            }
        }

        (lines, checked_any)
    });

    Ok(render_project_summary(&lines, checked_any, refresh))
}

/// Renders the project-wide summary from the collected per-file counts.
///
/// `checked_any` is false when no language server has reported for any file, which is a
/// different statement from "the project has no problems" and must not be rendered as one.
fn render_project_summary(lines: &[String], checked_any: bool, refresh: RefreshOutcome) -> String {
    let mut output = format!("{}\n\n", refresh.message());
    if lines.is_empty() {
        if checked_any {
            output.push_str("No errors or warnings in the files the language servers reported on.");
        } else {
            output.push_str(
                "No language server has reported diagnostics for this project, so nothing was \
                 checked. This is not a clean bill of health — pass the files you changed as \
                 `paths` to check them, or use the terminal.",
            );
        }
    } else {
        for line in lines {
            let _ = writeln!(output, "{line}");
        }
        output.push_str(
            "\nThis summary covers only the files the language servers have reported on; pass \
             `paths` to check specific files.",
        );
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        severity: DiagnosticSeverity,
        line: u32,
        message: &str,
    ) -> (DiagnosticSeverity, u32, String) {
        (severity, line, message.to_string())
    }

    #[test]
    fn checked_clean_file_says_no_errors() {
        let output = format_file_result("root/src/main.rs", &[], None, RefreshOutcome::Refreshed);

        assert!(output.contains("root/src/main.rs"));
        assert!(output.contains("Diagnostics successfully refreshed."));
        assert!(output.contains("No errors or warnings."));
    }

    /// The regression this tool exists for: a file no server has reported on must not be
    /// presented as clean, or the model reports success for a file nobody checked.
    #[test]
    fn unchecked_file_is_not_reported_as_clean() {
        let output = format_file_result(
            "root/src/moved.rs",
            &[],
            Some(UncheckedReason::NoReportYet),
            RefreshOutcome::Refreshed,
        );

        assert!(output.contains("has not been checked"));
        assert!(!output.contains("No errors or warnings."));
    }

    #[test]
    fn file_without_language_server_is_not_reported_as_clean() {
        let output = format_file_result(
            "root/notes.md",
            &[],
            Some(UncheckedReason::NoLanguageServer),
            RefreshOutcome::Refreshed,
        );

        assert!(output.contains("No language server is attached"));
        assert!(!output.contains("No errors or warnings."));
    }

    #[test]
    fn stale_refresh_is_marked_on_every_file() {
        let first = format_file_result("root/a.rs", &[], None, RefreshOutcome::Stale);
        let second = format_file_result("root/b.rs", &[], None, RefreshOutcome::Stale);

        // A multi-file call reports each file separately, so the stale warning cannot be a
        // single line that only covers the whole response.
        assert!(first.contains("may be stale"));
        assert!(second.contains("may be stale"));
    }

    #[test]
    fn diagnostics_are_attributed_to_their_file() {
        let output = format_file_result(
            "root/src/lib.rs",
            &[
                entry(DiagnosticSeverity::ERROR, 12, "cannot find value"),
                entry(DiagnosticSeverity::WARNING, 40, "unused import"),
            ],
            None,
            RefreshOutcome::Refreshed,
        );

        assert!(output.contains("root/src/lib.rs: error at line 12: cannot find value"));
        assert!(output.contains("root/src/lib.rs: warning at line 40: unused import"));
    }

    #[test]
    fn project_summary_without_reports_is_not_a_clean_bill_of_health() {
        let output = render_project_summary(&[], false, RefreshOutcome::Refreshed);

        assert!(output.contains("nothing was checked"));
        assert!(!output.contains("No errors or warnings in the files"));
    }

    #[test]
    fn project_summary_with_reports_says_which_files_it_covers() {
        let output = render_project_summary(
            &["root/a.rs: 1 error(s), 0 warning(s)".to_string()],
            true,
            RefreshOutcome::Refreshed,
        );

        assert!(output.contains("root/a.rs: 1 error(s), 0 warning(s)"));
        assert!(output.contains("covers only the files the language servers have reported on"));
    }
}
