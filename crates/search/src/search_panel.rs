use std::collections::HashSet;
use std::ops::Range;
use std::pin::pin;

use editor::{Editor, EditorSettings};
use file_icons::FileIcons;
use futures::StreamExt;
use gpui::{
    Action, AnyElement, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter,
    FocusHandle, Focusable, KeyContext, ListHorizontalSizingBehavior, ListSizingBehavior, Pixels,
    Render, ScrollStrategy, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, actions, px, uniform_list,
};
use language::{Buffer, ToPoint};
use lsp_locations::{LocationMatch, location_match_for_range, render_matched_line};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use project::{
    Project, ProjectPath, SearchResults,
    search::{SearchQuery, SearchResult},
};
use settings::Settings;
use text::Anchor;
use ui::{
    CommonAnimationExt, IconButton, IconButtonShape, ListItem, ListItemSpacing, LoadingLabel,
    ScrollAxes, Scrollbars, Tab, Toggleable, Tooltip, WithScrollbar,
    prelude::*,
    scrollbars::{ScrollbarVisibility, ShowScrollbar},
};
use util::{ResultExt as _, paths::PathMatcher};
use workspace::{
    DeploySearch, HideStatusItem, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::{ItemSettings, PreviewTabsSettings},
};

use crate::{
    EXCLUDE_PLACEHOLDER, INCLUDE_PLACEHOLDER, SEARCH_ICON, SearchOption, SearchOptions,
    SearchSource, ToggleCaseSensitive, ToggleIncludeIgnored, ToggleRegex, ToggleWholeWord,
    project_search::{ToggleFilters, split_glob_patterns},
    search_bar::{input_base_styles, render_text_input},
};

actions!(
    search_panel,
    [
        /// Toggles the panel with the project search bar and its results.
        Toggle,
        /// Toggles focus on the search panel.
        ToggleFocus,
    ]
);

const SEARCH_PANEL_KEY: &str = "SearchPanel";

/// The search panel has no settings of its own, so its scrollbars follow the
/// editor scrollbar setting that the other panels inherit by default.
#[derive(Default)]
struct SearchPanelScrollbarProxy;

impl ScrollbarVisibility for SearchPanelScrollbarProxy {
    fn visibility(&self, cx: &App) -> ShowScrollbar {
        EditorSettings::get_global(cx).scrollbar.show
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SearchPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<SearchPanel>(window, cx) {
                workspace.close_panel::<SearchPanel>(window, cx);
            }
        });
    })
    .detach();
}

/// A VSCode-style search panel: the project search bar (query input, options
/// and path filters) on top, with the matches listed below as file headers and
/// matched lines, like the references panel. The header button opens a clean
/// project search multibuffer.
pub struct SearchPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    query_editor: Entity<Editor>,
    included_files_editor: Entity<Editor>,
    excluded_files_editor: Entity<Editor>,
    search_options: SearchOptions,
    filters_enabled: bool,
    included_opened_only: bool,
    query_error: Option<String>,
    include_error: Option<String>,
    exclude_error: Option<String>,
    searching: bool,
    results: Option<PanelResults>,
    collapsed_files: HashSet<ProjectPath>,
    selected_entry: Option<usize>,
    limit_reached: bool,
    scroll_handle: UniformListScrollHandle,
    pending_search: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
enum Entry {
    Header(ProjectPath),
    Match(usize),
}

struct PanelResults {
    /// Rows in display order: one file header followed by its matches.
    entries: Vec<Entry>,
    matches: Vec<LocationMatch>,
}

/// Builds the display rows from the (path-grouped) matches: one header per
/// file, then its matches, or only the headers for collapsed files.
fn build_entries(matches: &[LocationMatch], collapsed_files: &HashSet<ProjectPath>) -> Vec<Entry> {
    let mut entries = Vec::with_capacity(matches.len());
    let mut last_path: Option<&ProjectPath> = None;
    for (match_index, location_match) in matches.iter().enumerate() {
        if last_path != Some(&location_match.path) {
            entries.push(Entry::Header(location_match.path.clone()));
            last_path = Some(&location_match.path);
        }
        if !collapsed_files.contains(&location_match.path) {
            entries.push(Entry::Match(match_index));
        }
    }
    entries
}

impl SearchPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| SearchPanel::new(workspace, window, cx))
        })
    }

    fn new(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 4, window, cx);
            editor.set_placeholder_text("Search all files…", window, cx);
            editor.set_use_autoclose(false);
            editor.set_use_selection_highlight(false);
            editor
        });
        let included_files_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(INCLUDE_PLACEHOLDER, window, cx);
            editor
        });
        let excluded_files_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(EXCLUDE_PLACEHOLDER, window, cx);
            editor
        });

        let subscriptions = Vec::new();

        Self {
            workspace: workspace.weak_handle(),
            project: workspace.project().clone(),
            focus_handle: cx.focus_handle(),
            query_editor,
            included_files_editor,
            excluded_files_editor,
            // Read the same option defaults as the project search bar so the
            // panel and the results multibuffer agree on the initial state.
            search_options: SearchOptions::from_settings(&EditorSettings::get_global(cx).search),
            filters_enabled: false,
            included_opened_only: false,
            query_error: None,
            include_error: None,
            exclude_error: None,
            searching: false,
            results: None,
            collapsed_files: HashSet::default(),
            selected_entry: None,
            limit_reached: false,
            scroll_handle: UniformListScrollHandle::new(),
            pending_search: None,
            _subscriptions: subscriptions,
        }
    }

    fn dispatch_context(&self) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("SearchPanel");
        dispatch_context.add("menu");
        dispatch_context
    }

    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut Context<Self>) {
        self.search_options.toggle(option);
        cx.notify();
    }

    /// Runs the search from the search bar. Bound to Enter anywhere in the
    /// search menu; `Confirm` while the results list is focused opens the
    /// selected entry instead, via [`SearchPanel::open_selected`].
    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.search(window, cx);
    }

    fn toggle_filters(&mut self, cx: &mut Context<Self>) {
        self.filters_enabled = !self.filters_enabled;
        self.include_error = None;
        self.exclude_error = None;
        cx.notify();
    }

    fn toggle_opened_only(&mut self, cx: &mut Context<Self>) {
        self.included_opened_only = !self.included_opened_only;
        cx.notify();
    }

    /// Opens a clean project search multibuffer in the active pane, exactly
    /// like the workspace's project search button used to.
    fn open_search_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(DeploySearch::default()), cx);
    }

    /// Runs a search with the panel's current query, options and filters,
    /// listing the matches in the panel.
    fn search(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(query) = self.build_search_query(cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        self.searching = true;
        self.results = None;
        self.selected_entry = None;
        self.limit_reached = false;
        cx.notify();

        let search = project.update(cx, |project, cx| project.search(query, cx));
        self.pending_search = Some(cx.spawn(async move |this, cx| {
            // Keep `SearchResults` (and thus its `task_handle`) alive while
            // consuming the stream: dropping the handle cancels the search
            // task before it has delivered any results.
            let SearchResults {
                rx,
                task_handle: _task_handle,
            } = search;
            let mut buffers_with_ranges = Vec::new();
            let mut limit_reached = false;
            let mut matches_stream = pin!(rx.ready_chunks(1024));
            while let Some(results) = matches_stream.next().await {
                for result in results {
                    match result {
                        SearchResult::Buffer { buffer, ranges } => {
                            buffers_with_ranges.push((buffer, ranges));
                        }
                        SearchResult::LimitReached => limit_reached = true,
                        SearchResult::Searching | SearchResult::WaitingForScan => {}
                    }
                }
            }
            this.update(cx, |this, cx| {
                this.set_results(buffers_with_ranges, limit_reached, cx);
            })
            .log_err();
        }));
    }

    fn set_results(
        &mut self,
        buffers_with_ranges: Vec<(Entity<Buffer>, Vec<Range<Anchor>>)>,
        limit_reached: bool,
        cx: &mut Context<Self>,
    ) {
        self.searching = false;
        self.limit_reached = limit_reached;
        self.collapsed_files.clear();
        let mut location_matches = Vec::new();
        for (buffer, ranges) in buffers_with_ranges {
            for range in ranges {
                if let Some(location_match) = location_match_for_range(&buffer, range, cx) {
                    location_matches.push(location_match);
                }
            }
        }
        // Group by file and order by position so the grouped display list is
        // stable, then drop exact-duplicate ranges.
        location_matches
            .sort_by(|a, b| a.path.cmp(&b.path).then(a.range.start.cmp(&b.range.start)));
        location_matches.dedup_by(|a, b| a.path == b.path && a.range == b.range);
        self.results = Some(PanelResults {
            entries: build_entries(&location_matches, &self.collapsed_files),
            matches: location_matches,
        });
        self.selected_entry = None;
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Collapses or expands all file groups at once; `toggle_group` collapses
    /// or expands the group of an individual file.
    fn toggle_group(&mut self, path: ProjectPath, cx: &mut Context<Self>) {
        if !self.collapsed_files.remove(&path) {
            self.collapsed_files.insert(path);
        }
        self.rebuild_entries(cx);
    }

    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let Some(results) = &self.results else {
            return;
        };
        self.collapsed_files = if collapsed {
            results.matches.iter().map(|m| m.path.clone()).collect()
        } else {
            HashSet::default()
        };
        self.rebuild_entries(cx);
    }

    fn all_collapsed(&self) -> bool {
        let Some(results) = &self.results else {
            return false;
        };
        let group_count = results
            .matches
            .iter()
            .map(|m| &m.path)
            .collect::<HashSet<_>>()
            .len();
        group_count > 0 && self.collapsed_files.len() == group_count
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        let Some(results) = &mut self.results else {
            return;
        };
        results.entries = build_entries(&results.matches, &self.collapsed_files);
        self.selected_entry = None;
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn select_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_entry = Some(index);
        self.scroll_handle
            .scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    /// Moves the selection by `direction` rows (only match rows are selectable).
    fn select_relative(&mut self, direction: i32, cx: &mut Context<Self>) {
        let Some(results) = self.results.as_ref() else {
            return;
        };
        let start = match self.selected_entry {
            Some(index) => index as i64,
            None if direction > 0 => -1,
            None => results.entries.len() as i64,
        };
        let mut index = start;
        loop {
            index += direction as i64;
            if index < 0 || index >= results.entries.len() as i64 {
                return;
            }
            if matches!(results.entries[index as usize], Entry::Match(_)) {
                self.select_entry(index as usize, cx);
                return;
            }
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_relative(1, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.select_relative(-1, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(results) = self.results.as_ref()
            && let Some(first) = results
                .entries
                .iter()
                .position(|entry| matches!(entry, Entry::Match(_)))
        {
            self.select_entry(first, cx);
        }
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(results) = self.results.as_ref()
            && let Some(last) = results
                .entries
                .iter()
                .rposition(|entry| matches!(entry, Entry::Match(_)))
        {
            self.select_entry(last, cx);
        }
    }

    fn open_selected(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.open_selected_entry(window, cx);
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(Box::new(ToggleFocus), cx);
    }

    fn open_selected_entry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry_index) = self.selected_entry else {
            return;
        };
        self.open_entry(entry_index, window, cx);
    }

    /// Opens the location of the match at `entry_index`. Files open in a
    /// preview tab that is reused for every match whose file has no permanent
    /// tab; files already open keep their existing tab. The panel stays
    /// focused so the list remains navigable.
    fn open_entry(&mut self, entry_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(results) = self.results.as_ref() else {
            return;
        };
        let Some(Entry::Match(match_index)) = results.entries.get(entry_index) else {
            return;
        };
        let Some(location_match) = results.matches.get(*match_index) else {
            return;
        };
        self.open_location(location_match, window, cx);
    }

    fn open_location(
        &self,
        location_match: &LocationMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let snapshot = location_match.buffer.read(cx).snapshot();
        let point_range = location_match.anchor_range.start.to_point(&snapshot)
            ..location_match.anchor_range.end.to_point(&snapshot);
        let buffer = location_match.buffer.clone();
        let focus_handle = self.focus_handle.clone();
        workspace.update(cx, |workspace, cx| {
            let preview_tabs_settings = PreviewTabsSettings::get_global(cx);
            let editor: Entity<Editor> = workspace.open_project_item(
                None,
                buffer,
                true,
                true,
                // Always keep the current preview tab as a preview: with the
                // default `enable_keep_preview_on_code_navigation = false` the
                // workspace would promote the active preview tab to a permanent
                // tab on every click, so switching between matches from
                // different files would open a new preview tab each time
                // instead of reusing one.
                true,
                preview_tabs_settings.enable_preview_file_from_code_navigation,
                window,
                cx,
            );
            editor.update(cx, |editor, cx| {
                editor.go_to_singleton_buffer_range(point_range, window, cx);
            });
            window.focus(&focus_handle, cx);
        });
    }

    fn build_search_query(&mut self, cx: &mut Context<Self>) -> Option<SearchQuery> {
        let Some(workspace) = self.workspace.upgrade() else {
            return None;
        };
        let project = workspace.read(cx).project().clone();
        let query_text = self.query_editor.read(cx).text(cx);

        let included_files = if self.filters_enabled {
            match parse_path_matches(self.included_files_editor.read(cx).text(cx), &project, cx) {
                Ok(matcher) => {
                    self.include_error = None;
                    matcher
                }
                Err(error) => {
                    self.include_error = Some(error.to_string());
                    PathMatcher::default()
                }
            }
        } else {
            PathMatcher::default()
        };
        let excluded_files = if self.filters_enabled {
            match parse_path_matches(self.excluded_files_editor.read(cx).text(cx), &project, cx) {
                Ok(matcher) => {
                    self.exclude_error = None;
                    matcher
                }
                Err(error) => {
                    self.exclude_error = Some(error.to_string());
                    PathMatcher::default()
                }
            }
        } else {
            PathMatcher::default()
        };

        // If the project contains multiple visible worktrees, we match the
        // include/exclude patterns against full paths to allow them to be
        // disambiguated. For single worktree projects we use worktree relative
        // paths for convenience.
        let match_full_paths = project.read(cx).visible_worktrees(cx).count() > 1;
        let open_buffers = if self.included_opened_only {
            Some(self.open_buffers(cx, workspace.read(cx)))
        } else {
            None
        };

        let query = match self.search_options.build_query(
            query_text,
            included_files,
            excluded_files,
            match_full_paths,
            open_buffers,
        ) {
            Ok(query) => {
                self.query_error = None;
                Some(query)
            }
            Err(error) => {
                self.query_error = Some(error.to_string());
                None
            }
        };
        cx.notify();
        if self.query_error.is_some()
            || self.include_error.is_some()
            || self.exclude_error.is_some()
        {
            return None;
        }
        if query.as_ref().is_some_and(|query| query.is_empty()) {
            return None;
        }
        query
    }

    fn open_buffers(&self, cx: &App, workspace: &Workspace) -> Vec<Entity<Buffer>> {
        let mut buffers = Vec::new();
        for editor in workspace.items_of_type::<Editor>(cx) {
            if let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() {
                buffers.push(buffer);
            }
        }
        buffers
    }

    fn render_header(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_collapsed = self.all_collapsed();
        let collapse_button = IconButton::new(
            "collapse-all",
            if is_collapsed {
                IconName::ListExpand
            } else {
                IconName::ListCollapse
            },
        )
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::text(if is_collapsed {
            "Expand All"
        } else {
            "Collapse All"
        }))
        .on_click(cx.listener(|this, _, _, cx| {
            this.set_all_collapsed(!this.all_collapsed(), cx);
        }));

        h_flex()
            .id("search-panel-toolbar")
            .h(Tab::container_height(cx))
            .flex_shrink_0()
            .max_w_full()
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .relative()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap(DynamicSpacing::Base04.rems(cx))
                    .pl(DynamicSpacing::Base04.rems(cx))
                    .child(Icon::new(SEARCH_ICON).color(Color::Muted))
                    .child(Label::new("Search").truncate()),
            )
            .child(
                h_flex()
                    .px_1()
                    .h_full()
                    .flex_none()
                    .gap_1()
                    .child(collapse_button)
                    .child(
                        IconButton::new("open-search-editor", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open Search Editor"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_search_editor(window, cx);
                            })),
                    ),
            )
    }

    fn render_contents(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let results_area = if self.searching {
            self.render_searching(cx)
        } else {
            match &self.results {
                None => {
                    self.render_empty_state(&["Run a search with Enter", "to see results here"])
                }
                Some(results) if results.matches.is_empty() => {
                    self.render_empty_state(&["No results found"])
                }
                Some(_) => self.render_results_list(window, cx),
            }
        };

        v_flex()
            .id("search-panel-contents")
            .size_full()
            .overflow_hidden()
            .child(self.render_search_bar(cx))
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(1.))
                    .w_full()
                    .bg(cx.theme().colors().border),
            )
            .child(results_area)
            .into_any_element()
    }

    fn render_search_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme_colors = cx.theme().colors();
        let border_color = |has_error: bool, cx: &App| {
            if has_error {
                Color::Error.color(cx)
            } else {
                theme_colors.border_variant
            }
        };
        let focus_handle = self.focus_handle.clone();

        let query_input = input_base_styles(border_color(self.query_error.is_some(), cx), |div| {
            div.flex_1()
        })
        .child(div().flex_1().py_1().child(render_text_input(
            &self.query_editor,
            None,
            cx,
        )));

        let options_row = h_flex()
            .gap_1()
            .child(SearchOption::CaseSensitive.as_button(
                self.search_options,
                SearchSource::Buffer,
                focus_handle.clone(),
            ))
            .child(SearchOption::WholeWord.as_button(
                self.search_options,
                SearchSource::Buffer,
                focus_handle.clone(),
            ))
            .child(SearchOption::Regex.as_button(
                self.search_options,
                SearchSource::Buffer,
                focus_handle.clone(),
            ))
            .child(div().flex_1());

        let filter_button = IconButton::new("search-panel-filter-button", IconName::Filter)
            .shape(IconButtonShape::Square)
            .toggle_state(self.filters_enabled)
            .tooltip(|_window, cx| Tooltip::for_action("Toggle Filters", &ToggleFilters, cx))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_filters(cx);
            }));

        let search_line = h_flex()
            .w_full()
            .gap_1()
            .child(query_input)
            .child(filter_button);

        let query_error_line = self.query_error.as_ref().map(|error| {
            Label::new(error.to_string())
                .size(LabelSize::Small)
                .color(Color::Error)
        });

        let filter_line = self.filters_enabled.then(|| {
            let include_input =
                input_base_styles(border_color(self.include_error.is_some(), cx), |div| {
                    div.flex_1()
                })
                .child(render_text_input(&self.included_files_editor, None, cx));
            let exclude_input =
                input_base_styles(border_color(self.exclude_error.is_some(), cx), |div| {
                    div.flex_1()
                })
                .child(render_text_input(&self.excluded_files_editor, None, cx));

            let mode_buttons = h_flex()
                .gap_1()
                .child(
                    IconButton::new("search-panel-opened-only", IconName::FolderSearch)
                        .shape(IconButtonShape::Square)
                        .toggle_state(self.included_opened_only)
                        .tooltip(Tooltip::text("Only Search Open Files"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_opened_only(cx);
                        })),
                )
                .child(SearchOption::IncludeIgnored.as_button(
                    self.search_options,
                    SearchSource::Buffer,
                    focus_handle,
                ))
                .child(div().flex_1());

            v_flex()
                .gap_1()
                .child(h_flex().gap_1().child(include_input).child(exclude_input))
                .child(mode_buttons)
        });

        let filter_error_line = self
            .include_error
            .as_ref()
            .or(self.exclude_error.as_ref())
            .map(|error| {
                Label::new(error.to_string())
                    .size(LabelSize::Small)
                    .color(Color::Error)
            });

        v_flex()
            .w_full()
            .flex_shrink_0()
            .p_2()
            .gap_2()
            // Match the project search toolbar's dark surface so the gaps
            // inside the input borders look the same as in the multibuffer.
            .bg(cx.theme().colors().toolbar_background)
            .on_action(cx.listener(Self::confirm))
            .child(search_line)
            .children(query_error_line)
            .child(options_row)
            .children(filter_line)
            .children(filter_error_line)
            .into_any_element()
    }

    fn render_searching(&self, _cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1p5()
            .child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(2),
            )
            .child(
                LoadingLabel::new("Searching")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_empty_state(&self, lines: &[&'static str]) -> AnyElement {
        v_flex()
            .size_full()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_0p5()
            .px_4()
            .children(
                lines
                    .iter()
                    .map(|line| Label::new(*line).color(Color::Muted).size(LabelSize::Small)),
            )
            .into_any_element()
    }

    fn render_results_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(results) = &self.results else {
            return self.render_empty_state(&["No results found"]);
        };
        let items_len = results.entries.len();
        let max_line_number = results
            .matches
            .iter()
            .map(|location_match| location_match.line_number)
            .max()
            .unwrap_or(0);
        // Measure the list by the first match row instead of the first entry:
        // the uniform list applies one measured height to every row, and file
        // headers are shorter than match rows, so measuring a header would
        // overlap the match rows at large font sizes.
        let first_match_index = results
            .entries
            .iter()
            .position(|entry| matches!(entry, Entry::Match(_)))
            .unwrap_or(0);
        let file_count = results
            .entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Header(_)))
            .count();
        let summary = format!(
            "{} {} in {} {}",
            results.matches.len(),
            if results.matches.len() == 1 {
                "result"
            } else {
                "results"
            },
            file_count,
            if file_count == 1 { "file" } else { "files" },
        );

        let list = uniform_list(
            "results",
            items_len,
            cx.processor(move |this, range: Range<usize>, window, cx| {
                let Some(results) = &this.results else {
                    return Vec::new();
                };
                results
                    .entries
                    .get(range.clone())
                    .map(|entries| entries.to_vec())
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .map(|(offset, entry)| {
                        this.render_entry(
                            results,
                            range.start + offset,
                            entry,
                            max_line_number,
                            window,
                            cx,
                        )
                    })
                    .collect()
            }),
        )
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .size_full()
        .with_width_from_item(Some(first_match_index))
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&self.scroll_handle);

        v_flex()
            .size_full()
            .flex_1()
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .px(DynamicSpacing::Base06.rems(cx))
                    .py_1()
                    .gap_1()
                    .child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(self.limit_reached, |this| {
                        this.child(
                            Label::new("search results are limited")
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    }),
            )
            .child(list)
            // Long match lines need a horizontal scrollbar; the list's
            // `Unconstrained` sizing already enables horizontal scrolling.
            .custom_scrollbars(
                Scrollbars::for_settings::<SearchPanelScrollbarProxy>()
                    .tracked_scroll_handle(&self.scroll_handle.clone())
                    .with_track_along(ScrollAxes::Both, cx.theme().colors().panel_background)
                    .tracked_entity(cx.entity_id()),
                window,
                cx,
            )
            .into_any_element()
    }

    fn render_entry(
        &self,
        results: &PanelResults,
        entry_index: usize,
        entry: Entry,
        max_line_number: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            Entry::Header(path) => self.render_file_header(&path, cx),
            Entry::Match(match_index) => self.render_match_entry(
                results,
                entry_index,
                match_index,
                max_line_number,
                window,
                cx,
            ),
        }
    }

    fn render_file_header(&self, path: &ProjectPath, cx: &mut Context<Self>) -> AnyElement {
        let path_style = self.project.read(cx).path_style(cx);
        let file_name = path
            .path
            .file_name()
            .map(|name| name.to_string())
            .unwrap_or_default();
        let directory = path
            .path
            .parent()
            .map(|parent| parent.display(path_style))
            .map(SharedString::new)
            .unwrap_or_default();
        let file_icon = ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(path.path.as_std_path(), cx))
            .flatten()
            .map(|icon| {
                Icon::from_path(icon)
                    .color(Color::Muted)
                    .size(IconSize::Small)
            });
        let path = path.clone();
        h_flex()
            .id(path.path.as_std_path().to_string_lossy().into_owned())
            .w_full()
            .min_w_0()
            .px(DynamicSpacing::Base06.rems(cx))
            .py_1()
            .gap_1p5()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_group(path.clone(), cx);
            }))
            .children(file_icon)
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(file_name).size(LabelSize::Small))
                    .when(!directory.is_empty(), |this| {
                        this.child(
                            Label::new(directory)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate_start(),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_match_entry(
        &self,
        results: &PanelResults,
        entry_index: usize,
        match_index: usize,
        max_line_number: u32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(location_match) = results.matches.get(match_index) else {
            return div().into_any_element();
        };
        let selected = self.selected_entry == Some(entry_index);
        ListItem::new(entry_index)
            .spacing(ListItemSpacing::Sparse)
            .inset(true)
            // Don't apply the hover style on top of the selected item: the
            // active row keeps its selected background while hovered.
            .selectable(!selected)
            .toggle_state(selected)
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selected_entry = Some(entry_index);
                this.open_selected_entry(window, cx);
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2p5()
                    .text_sm()
                    .child(
                        h_flex()
                            .w(rems((max_line_number.max(1).ilog10() + 1) as f32 * 0.6))
                            .justify_end()
                            .child(
                                Label::new(location_match.line_number.to_string()).color(
                                    Color::Custom(cx.theme().colors().text_muted.opacity(0.5)),
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(render_matched_line(location_match, cx)),
                    ),
            )
            .into_any_element()
    }
}

fn parse_path_matches(
    text: String,
    project: &Entity<Project>,
    cx: &App,
) -> anyhow::Result<PathMatcher> {
    let path_style = project.read(cx).path_style(cx);
    let queries = split_glob_patterns(&text)
        .into_iter()
        .map(str::trim)
        .filter(|maybe_glob_str| !maybe_glob_str.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Ok(PathMatcher::new(&queries, path_style)?)
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SearchPanel {
    fn persistent_name() -> &'static str {
        "Search Panel"
    }

    fn panel_key() -> &'static str {
        SEARCH_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        EditorSettings::get_global(cx)
            .search
            .button
            .then_some(SEARCH_ICON)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Search")
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_editor.focus_handle(cx)
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn hide_button_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.editor.search.get_or_insert_default().button = Some(false);
        }))
    }
}

impl Render for SearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("search-panel")
            .size_full()
            .overflow_hidden()
            .key_context(self.dispatch_context())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _, cx| {
                this.toggle_search_option(SearchOptions::CASE_SENSITIVE, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| {
                this.toggle_search_option(SearchOptions::WHOLE_WORD, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| {
                this.toggle_search_option(SearchOptions::REGEX, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleIncludeIgnored, _, cx| {
                this.toggle_search_option(SearchOptions::INCLUDE_IGNORED, cx);
            }))
            .child(self.render_header(window, cx))
            .child(self.render_contents(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectSearchView;
    use fs::FakeFs;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext, WindowHandle};
    use serde_json::json;
    use settings::SettingsStore;
    use std::time::Duration;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);

            theme_settings::init(theme::LoadThemes::JustBase, cx);

            editor::init(cx);
            crate::init(cx);

            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .editor
                        .search
                        .get_or_insert_default()
                        .search_on_type = Some(false);
                });
            });
        });
    }

    async fn build_workspace(
        cx: &mut TestAppContext,
    ) -> (WindowHandle<MultiWorkspace>, Entity<Workspace>) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "one.rs": "const ONE: usize = 1;\nconst ONEROUS: usize = 2;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();
        (window, workspace)
    }

    fn add_panel(
        window: &WindowHandle<MultiWorkspace>,
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> Entity<SearchPanel> {
        let cx = &mut VisualTestContext::from_window((*window).into(), cx);
        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| SearchPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        })
    }

    /// Runs a search from the panel and pumps until the search completes.
    fn run_search(panel: &Entity<SearchPanel>, query: &str, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            panel.query_editor.update(cx, |editor, cx| {
                editor.set_text(query, window, cx);
            });
            panel.confirm(&Confirm, window, cx);
        });
        for _ in 0..20 {
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.background_executor.run_until_parked();
            if !cx.read(|cx| panel.read(cx).searching) {
                break;
            }
        }
    }

    fn window_active_search_view(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> Option<Entity<ProjectSearchView>> {
        cx.update(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        })
    }

    #[gpui::test]
    async fn test_toggle_action_toggles_panel(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        // Like the project and git panels, the search panel contributes a
        // button to the panel buttons group in the left status strip.
        let has_icon = cx.update(|window, cx| panel.read(cx).icon(window, cx).is_some());
        assert!(
            has_icon,
            "the search panel should appear in the panel buttons group"
        );

        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(Toggle), cx);
            })
            .unwrap();

        let dock_open = cx.update(|_window, cx| workspace.read(cx).left_dock().read(cx).is_open());
        assert!(dock_open, "toggling should open the search panel");

        cx.update(|window, cx| {
            assert!(
                panel
                    .read(cx)
                    .query_editor
                    .focus_handle(cx)
                    .is_focused(window),
                "toggling should focus the query input"
            );
        });

        // Toggling again (with the panel focused) closes it.
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(Toggle), cx);
            })
            .unwrap();
        let dock_open = cx.update(|_window, cx| workspace.read(cx).left_dock().read(cx).is_open());
        assert!(!dock_open, "toggling again should close the search panel");
    }

    #[gpui::test]
    async fn test_confirm_runs_search_and_shows_results(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        run_search(&panel, "ONEROUS", cx);

        let (entries_len, matches_len, searching) = cx.update(|_window, cx| {
            let panel = panel.read(cx);
            let results = panel.results.as_ref().expect("results should be set");
            (
                results.entries.len(),
                results.matches.len(),
                panel.searching,
            )
        });
        assert!(!searching, "search should have completed");
        assert_eq!(matches_len, 1, "ONEROUS appears once");
        assert_eq!(entries_len, 2, "one file header plus the match row");
    }

    #[gpui::test]
    async fn test_collapsing_a_file_group(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        run_search(&panel, "ONE", cx);

        let (one_path, entries_len) = cx.update(|_window, cx| {
            let results = panel
                .read(cx)
                .results
                .as_ref()
                .expect("results should be set");
            let first_header = match &results.entries[0] {
                Entry::Header(path) => path.clone(),
                Entry::Match(_) => panic!("the first entry should be a file header"),
            };
            (first_header, results.entries.len())
        });
        assert_eq!(entries_len, 8, "two file headers plus six match rows");

        panel.update_in(cx, |panel, _window, cx| {
            panel.toggle_group(one_path, cx);
        });

        let (entries_len, all_collapsed) = cx.update(|_window, cx| {
            let panel = panel.read(cx);
            (
                panel.results.as_ref().unwrap().entries.len(),
                panel.all_collapsed(),
            )
        });
        assert_eq!(
            entries_len, 6,
            "collapsing one file keeps only its header and the other file's rows"
        );
        assert!(!all_collapsed, "one collapsed file out of two is not all");

        // Toggling the same file again expands its group back.
        panel.update_in(cx, |panel, _window, cx| {
            panel.set_all_collapsed(true, cx);
        });
        let (entries_len, all_collapsed) = cx.update(|_window, cx| {
            let panel = panel.read(cx);
            (
                panel.results.as_ref().unwrap().entries.len(),
                panel.all_collapsed(),
            )
        });
        assert_eq!(entries_len, 2, "collapse all keeps only the headers");
        assert!(all_collapsed);

        panel.update_in(cx, |panel, _window, cx| {
            panel.set_all_collapsed(false, cx);
        });
        let entries_len =
            cx.update(|_window, cx| panel.read(cx).results.as_ref().unwrap().entries.len());
        assert_eq!(entries_len, 8, "expanding all restores every match row");
    }

    #[gpui::test]
    async fn test_empty_query_does_not_start_search(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.search(window, cx);
        });

        cx.update(|_window, cx| {
            let panel = panel.read(cx);
            assert!(panel.results.is_none(), "no query should not run a search");
            assert!(!panel.searching);
        });
    }

    #[gpui::test]
    async fn test_invalid_regex_shows_error_without_searching(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.query_editor.update(cx, |editor, cx| {
                editor.set_text("[", window, cx);
            });
            panel.toggle_search_option(SearchOptions::REGEX, cx);
            panel.search(window, cx);
        });

        cx.update(|_window, cx| {
            let panel = panel.read(cx);
            assert!(
                panel.query_error.is_some(),
                "an invalid regex should show an error"
            );
            assert!(!panel.searching);
            assert!(panel.results.is_none());
        });
    }

    #[gpui::test]
    async fn test_opening_match_opens_buffer_in_editor(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        run_search(&panel, "ONEROUS", cx);
        panel.update_in(cx, |panel, window, cx| {
            panel.open_entry(1, window, cx);
        });

        let has_editor = cx.update(|_window, cx| {
            workspace
                .read(cx)
                .active_item(cx)
                .map(|item| item.downcast::<Editor>().is_some())
                .unwrap_or(false)
        });
        assert!(
            has_editor,
            "opening a match should open its buffer in the pane"
        );
    }

    #[gpui::test]
    async fn test_open_search_editor_opens_clean_multibuffer(cx: &mut TestAppContext) {
        let (window, workspace) = build_workspace(cx).await;
        let panel = add_panel(&window, &workspace, cx);
        let cx = &mut VisualTestContext::from_window(window.into(), cx);

        panel.update_in(cx, |panel, window, cx| {
            panel.open_search_editor(window, cx);
        });

        let search_view = window_active_search_view(&workspace, cx);
        assert!(
            search_view.is_some(),
            "the header button should open a project search multibuffer"
        );
    }
}
