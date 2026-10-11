use std::collections::VecDeque;
use std::fmt;
use std::path::Path;

use super::ignored_scan::{prescan_ignored_ancestor, prescan_ignored_ancestor_for_project_path};
use gpui::{App, AsyncApp, Entity};
use language::{Buffer, Location};
use project::{CodeAction, Project};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use text::ToPoint as _;
use text::{Anchor, Point};

/// Identifies a specific symbol (declaration or usage) in the source code.
///
/// Use the file path, line number, and symbol name from file outlines, grep results, or other tool outputs to populate these fields.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SymbolLocator {
    /// The relative path of the file containing the symbol (e.g. "crates/editor/src/editor.rs").
    pub file_path: String,

    /// The 1-based line number where the symbol appears. Use the line numbers from file outlines or grep results.
    pub line: u32,

    /// The name of the symbol (function name, type name, variable name, etc.)
    pub symbol_name: String,
}

pub struct PendingCodeActions {
    pub actions: Vec<CodeAction>,
    pub buffer: Entity<Buffer>,
}

pub type CodeActionStore = Entity<Option<PendingCodeActions>>;

pub struct ResolvedSymbol {
    pub buffer: Entity<Buffer>,
    pub position: Anchor,
    pub line_text: String,
    pub truncated: bool,
}

pub const MAX_LINE_DISPLAY_LEN: usize = 200;

pub struct LocationDisplay {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub snippet: String,
    pub truncated: bool,
}

impl LocationDisplay {
    pub fn from_location(location: &Location, cx: &App) -> Self {
        let snapshot = location.buffer.read(cx).snapshot();
        let range =
            location.range.start.to_point(&snapshot)..location.range.end.to_point(&snapshot);
        let path = location
            .buffer
            .read(cx)
            .file()
            .map(|f| f.full_path(cx).display().to_string())
            .unwrap_or_else(|| "<untitled>".to_string());

        let start_line = range.start.row + 1;
        let end_line = range.end.row + 1;

        let line_len = snapshot.line_len(range.start.row);
        let truncated = line_len as usize > MAX_LINE_DISPLAY_LEN;
        let snippet: String = snapshot
            .text_for_range(Point::new(range.start.row, 0)..Point::new(range.start.row, line_len))
            .flat_map(|chunk| chunk.chars())
            .skip_while(|c| c.is_whitespace())
            .take(MAX_LINE_DISPLAY_LEN)
            .collect::<String>();
        let snippet = snippet.trim_end().to_string();

        Self {
            path,
            start_line,
            end_line,
            snippet,
            truncated,
        }
    }
}

impl fmt::Display for LocationDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let truncated_label = if self.truncated { " (truncated)" } else { "" };
        if self.start_line == self.end_line {
            writeln!(f, "{}#L{}{truncated_label}", self.path, self.start_line)?;
        } else {
            writeln!(
                f,
                "{}#L{}-{}{truncated_label}",
                self.path, self.start_line, self.end_line
            )?;
        }
        writeln!(f, "```")?;
        writeln!(f, "{}", self.snippet)?;
        write!(f, "```")
    }
}

/// Searches for `needle` in a char iterator, returning the byte offset of the
/// first occurrence without collecting the full iterator into a string.
///
/// Equivalent to [`str::find`]
fn find_in_char_iter(chars: impl Iterator<Item = char>, needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return Some(0);
    }

    let mut window: VecDeque<char> = VecDeque::with_capacity(needle_chars.len());
    let mut byte_offsets: VecDeque<usize> = VecDeque::with_capacity(needle_chars.len());
    let mut byte_offset = 0usize;

    for ch in chars {
        window.push_back(ch);
        byte_offsets.push_back(byte_offset);
        byte_offset += ch.len_utf8();

        if window.len() > needle_chars.len() {
            window.pop_front();
            byte_offsets.pop_front();
        }

        if window.len() == needle_chars.len()
            && window.iter().zip(needle_chars.iter()).all(|(a, b)| a == b)
        {
            return byte_offsets.front().copied();
        }
    }

    None
}

impl SymbolLocator {
    /// Resolves this locator into a concrete buffer and position.
    ///
    /// Opens the file at `file_path`, then searches for `symbol_name` on the
    /// specified `line`. Returns an error if the file can't be found, the line
    /// is out of range, or the symbol name doesn't appear on that line.
    /// If the symbol name appears multiple times on the line, uses the first
    /// occurrence.
    pub async fn resolve(
        &self,
        project: &Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<ResolvedSymbol, String> {
        let Self {
            file_path,
            line,
            symbol_name,
        } = self;

        // A bare relative path into a gitignored directory the worktree never scanned
        // can't resolve through the snapshot at all, so scan that directory
        // before asking for a project path. Without this the symbol tools
        // report a file that exists as missing, and the model falls back to the
        // terminal even though `read_file` opens the very same path.
        prescan_ignored_ancestor(&project, Path::new(file_path), cx).await;

        let project_path =
            project.read_with(cx, |project, cx| project.find_project_path(file_path, cx));
        let Some(project_path) = project_path else {
            return Err(format!("Could not find path '{file_path}' in project",));
        };

        // A rooted or absolute path resolves without a snapshot entry, so the
        // scan above is a no-op for it and the ignored ancestor is still
        // unloaded; scan it now that the path is resolved.
        prescan_ignored_ancestor_for_project_path(&project, &project_path, cx).await;

        let open_buffer_task =
            project.update(cx, |project, cx| project.open_buffer(project_path, cx));

        let buffer = open_buffer_task
            .await
            .map_err(|e| format!("Failed to open '{}': {e}", self.file_path))?;

        let (position, line_text, truncated) = buffer.read_with(cx, |buffer, _cx| {
            let snapshot = buffer.snapshot();
            let row = line.saturating_sub(1);

            if row > snapshot.max_point().row {
                let line_count = snapshot.max_point().row + 1;
                return Err(format!(
                    "Line {line} is beyond the end of '{file_path}' (file has {line_count} lines)",
                ));
            }

            let line_len = snapshot.line_len(row);
            let truncated = line_len as usize > MAX_LINE_DISPLAY_LEN;
            let line_start = Point::new(row, 0);
            let line_end = Point::new(row, line_len);
            let line_chars = || {
                snapshot
                    .text_for_range(line_start..line_end)
                    .flat_map(|chunk| chunk.chars())
            };

            let byte_offset = find_in_char_iter(line_chars(), symbol_name).ok_or_else(|| {
                let preview: String = line_chars()
                    .skip_while(|c| c.is_whitespace())
                    .take(MAX_LINE_DISPLAY_LEN)
                    .collect();
                format!(
                    "Symbol '{symbol_name}' not found on line {line} of '{file_path}'. \
                     Line content: '{}'",
                    preview.trim_end()
                )
            })?;

            let position = snapshot.anchor_before(Point::new(row, byte_offset as u32));
            let display_text: String = line_chars()
                .skip_while(|c| c.is_whitespace())
                .take(MAX_LINE_DISPLAY_LEN)
                .collect::<String>();
            let display_text = display_text.trim_end().to_string();

            Ok((position, display_text, truncated))
        })?;

        Ok(ResolvedSymbol {
            buffer,
            position,
            line_text,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui::proptest::prelude::*;
    use project::{FakeFs, Project};
    use serde_json::json;
    use util::path;

    #[gpui::property_test]
    fn find_in_char_iter_test(
        // limited character sets to increase odds of finding matches
        #[strategy = "[abcd]{100,1000}"] haystack: String,
        #[strategy = "[abcd]{1,5}"] needle: String,
    ) -> Result<(), TestCaseError> {
        let expected = haystack.find(&needle);
        let actual = find_in_char_iter(haystack.chars(), &needle);
        prop_assert_eq!(actual, expected);
        Ok::<_, TestCaseError>(())
    }

    /// A bare relative path into a gitignored directory the worktree never
    /// scanned used to fail with `Could not find path ... in project`, which
    /// reads as "this file doesn't exist" — the model then reaches for the
    /// terminal even though `read_file` opens the very same path.
    #[gpui::test]
    async fn test_resolve_symbol_in_gitignored_directory(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                // A git repository is required for `.gitignore` to apply.
                ".git": {},
                ".gitignore": "node_modules/\n",
                "node_modules": {
                    "pkg": { "index.js": "module.exports = 1;\n" },
                },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let locator = SymbolLocator {
            file_path: "node_modules/pkg/index.js".into(),
            line: 1,
            symbol_name: "module.exports".into(),
        };

        let resolved = cx
            .update(|cx| {
                let project = project.clone();
                cx.spawn(async move |cx| locator.resolve(&project, cx).await)
            })
            .await
            .unwrap_or_else(|error| panic!("resolving in a gitignored dir should work: {error}"));
        assert_eq!(resolved.line_text, "module.exports = 1;");
        assert!(!resolved.truncated);
    }

    /// The prescan must not invent a hit for a symbol that isn't there: a
    /// genuinely missing file still reports the file as missing.
    #[gpui::test]
    async fn test_resolve_symbol_missing_file_in_gitignored_directory(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": {},
                ".gitignore": "node_modules/\n",
                "node_modules": { "pkg": { "index.js": "module.exports = 1;\n" } },
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        cx.executor().run_until_parked();

        let locator = SymbolLocator {
            file_path: "node_modules/pkg/missing.js".into(),
            line: 1,
            symbol_name: "module.exports".into(),
        };

        let result = cx
            .update(|cx| {
                let project = project.clone();
                cx.spawn(async move |cx| locator.resolve(&project, cx).await)
            })
            .await;
        let Err(error) = result else {
            panic!("a missing file must not resolve");
        };
        assert!(
            error.contains("node_modules/pkg/missing.js"),
            "expected an error naming the missing file, got: {error}"
        );
    }
}
