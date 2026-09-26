use std::collections::HashSet;
use std::ops::Range;

use editor::actions::FindAllReferences;
use editor::{Editor, EditorSettings};
use file_icons::FileIcons;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter,
    ExternalDragPayload, FileDragPaths, FocusHandle, Focusable, Hsla, KeyContext,
    ListHorizontalSizingBehavior, ListSizingBehavior, MouseButton, MouseDownEvent, Pixels, Point,
    Render, ScrollStrategy, UniformListScrollHandle, WeakEntity, Window, actions, uniform_list,
};
use language::ToPoint;
use lsp_locations::{LocationMatch, build_location_matches, render_matched_line};
use menu::{Cancel, Confirm, SelectFirst, SelectLast, SelectNext, SelectPrevious};
use project::{Project, ProjectPath};
use project_panel::ProjectPanel;
use project_panel::{ContextMenuPlacement, project_panel_settings::ProjectPanelSettings};
use theme_settings::ThemeSettings;
use ui::scrollbars::{ScrollbarVisibility, ShowScrollbar};
use ui::{CommonAnimationExt, ScrollAxes, Scrollbars, Tab, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::item::{ItemSettings, PreviewTabsSettings, Settings};

actions!(
    references_panel,
    [
        /// Toggles the panel showing the results of the last "find all references" query.
        Toggle,
        /// Toggles focus on the references panel.
        ToggleFocus,
    ]
);

const REFERENCES_PANEL_KEY: &str = "ReferencesPanel";

/// The references panel has no settings of its own, so it follows the
/// global `editor.scrollbar.show` setting like other panels do when their
/// own `scrollbar` setting is unset.
#[derive(Default)]
struct ReferencesPanelScrollbarAccessor;

impl ScrollbarVisibility for ReferencesPanelScrollbarAccessor {
    fn visibility(&self, cx: &App) -> ShowScrollbar {
        EditorSettings::get_global(cx).scrollbar.show
    }
}

/// Selected and hovered row backgrounds, matching the thread panel rows
/// (`ThreadItem`). Both blend over the panel background at render time.
fn row_backgrounds(cx: &App) -> (Hsla, Hsla) {
    let colors = cx.theme().colors();
    (
        colors.element_active,
        colors
            .element_active
            .blend(colors.element_background.opacity(0.2)),
    )
}

/// Preview shown while dragging a file header out of the panel, mirroring the
/// drag previews of the git and project panels.
struct DraggedFileView {
    filename: String,
    click_offset: Point<Pixels>,
}

impl Render for DraggedFileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui_font = ThemeSettings::get_global(cx).ui_font.clone();
        h_flex()
            .font(ui_font)
            .pl(self.click_offset.x + px(12.))
            .pt(self.click_offset.y + px(12.))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .items_center()
                    .py_1()
                    .px_2()
                    .rounded_lg()
                    .bg(cx.theme().colors().background)
                    .child(Label::new(self.filename.clone())),
            )
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ReferencesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<ReferencesPanel>(window, cx) {
                workspace.close_panel::<ReferencesPanel>(window, cx);
            }
        });
    })
    .detach();

    // Handle `FindAllReferences` on full editors so the results open in this
    // panel (like VSCode's references view) instead of a multibuffer.
    cx.observe_new(
        |editor: &mut Editor, _: Option<&mut Window>, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }
            let handle = cx.entity().downgrade();
            editor
                .register_action(move |_action: &FindAllReferences, window, cx| {
                    handle_find_all_references(&handle, window, cx);
                })
                .detach();
        },
    )
    .detach();
}

fn handle_find_all_references(editor: &WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        return;
    };
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        find_all_references(&editor, workspace, window, cx)
    });
}

/// Runs the "find all references" query for `editor` and shows the results in
/// the [`ReferencesPanel`], opening it if it was closed.
fn find_all_references(
    editor: &Entity<Editor>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let editor = editor.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        // Load the panel up-front (it is normally added by the workspace
        // startup task, but may be missing when the query runs before that
        // task finishes) so the searching state can be shown right away.
        let panel = match workspace
            .read_with(cx, |workspace, cx| workspace.panel::<ReferencesPanel>(cx))
            .ok()
            .flatten()
        {
            Some(panel) => Some(panel),
            None => ReferencesPanel::load(workspace.clone(), cx.clone())
                .await
                .log_err(),
        };
        let Some(panel) = panel else {
            return;
        };

        // Show the searching state immediately so the panel does not look
        // stuck while the language server responds.
        workspace
            .update_in(cx, |workspace, window, cx| {
                if workspace.panel::<ReferencesPanel>(cx).is_none() {
                    workspace.add_panel(panel.clone(), window, cx);
                }
                panel.update(cx, |panel, cx| {
                    panel.set_searching(true, cx);
                });
                workspace.open_panel::<ReferencesPanel>(window, cx);
                cx.notify();
            })
            .log_err();

        let locations = {
            let Some(task) = editor
                .update(cx, |editor, cx| {
                    editor.find_all_references_locations(&project, cx)
                })
                .ok()
                .flatten()
            else {
                log::info!("no cursor position to find references for");
                return;
            };
            match task.await {
                Ok(locations) => locations,
                Err(error) => {
                    log::error!("find all references query failed: {error:#}");
                    workspace
                        .update(cx, |workspace, cx| workspace.show_error(error, cx))
                        .log_err();
                    return;
                }
            }
        };

        let Some(matches) = editor
            .update(cx, |_, cx| build_location_matches(&locations, cx))
            .ok()
        else {
            return;
        };

        workspace
            .update_in(cx, |workspace, window, cx| {
                let Some(panel) = workspace.panel::<ReferencesPanel>(cx) else {
                    return;
                };
                panel.update(cx, |panel, cx| {
                    panel.set_results(matches, cx);
                });
                workspace.open_panel::<ReferencesPanel>(window, cx);
                cx.notify();
            })
            .log_err();
    })
    .detach();
}

#[derive(Clone)]
enum Entry {
    Header(ProjectPath),
    Match(usize),
}

struct ReferenceResults {
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

pub struct ReferencesPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    /// Whether a "find all references" query is currently running.
    searching: bool,
    /// Files whose groups are collapsed to their headers.
    collapsed_files: HashSet<ProjectPath>,
    /// The results of the last "find all references" query. `None` means no
    /// query has been run yet, so the panel shows a hint instead of results.
    results: Option<ReferenceResults>,
    selected_entry: Option<usize>,
}

impl ReferencesPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            cx.new(|cx| ReferencesPanel::new(workspace, cx))
        })
    }

    fn new(workspace: &mut Workspace, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: workspace.weak_handle(),
            project: workspace.project().clone(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            searching: false,
            collapsed_files: HashSet::default(),
            results: None,
            selected_entry: None,
        }
    }

    fn dispatch_context(&self) -> KeyContext {
        let mut dispatch_context = KeyContext::new_with_defaults();
        dispatch_context.add("ReferencesPanel");
        dispatch_context.add("menu");
        dispatch_context
    }

    fn set_searching(&mut self, searching: bool, cx: &mut Context<Self>) {
        self.searching = searching;
        cx.notify();
    }

    fn set_results(&mut self, matches: Vec<LocationMatch>, cx: &mut Context<Self>) {
        self.searching = false;
        self.collapsed_files.clear();
        self.results = Some(ReferenceResults {
            entries: build_entries(&matches, &self.collapsed_files),
            matches,
        });
        self.selected_entry = None;
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Collapses or expands the file group for `path`; collapse keeps only the
    /// file header visible.
    fn toggle_group(&mut self, path: ProjectPath, cx: &mut Context<Self>) {
        if !self.collapsed_files.remove(&path) {
            self.collapsed_files.insert(path);
        }
        self.rebuild_entries(cx);
    }

    /// Collapses or expands all file groups at once.
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
        // Keep the current scroll position: collapsing or expanding a group
        // must not jump back to the top of the list.
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

    /// Opens the project panel's context menu for the file behind `path`.
    /// The menu is built by `ProjectPanel`, so any changes to the project
    /// panel's context menu apply here as well.
    fn deploy_context_menu_for_path(
        &self,
        path: &ProjectPath,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry_id = {
            let Some(entry) = self.project.read(cx).entry_for_path(path, cx) else {
                return;
            };
            entry.id
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            if let Some(project_panel) = workspace.panel::<ProjectPanel>(cx) {
                project_panel.update(cx, |panel, cx| {
                    panel.deploy_context_menu(
                        ContextMenuPlacement::AtMouse(position),
                        entry_id,
                        window,
                        cx,
                    );
                });
            }
        });
    }

    /// Opens the location of the match at `entry_index`. Files open in a
    /// preview tab that is reused for every reference whose file has no
    /// permanent tab; files already open (e.g. promoted out of preview by the
    /// user) keep their existing tab. The panel stays focused so the list
    /// remains navigable.
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
                // tab on every click, so switching between references from
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
            .id("references-panel-toolbar")
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
                    .child(Icon::new(IconName::Quote).color(Color::Muted))
                    .child(Label::new("References").truncate()),
            )
            .child(
                h_flex()
                    .px_1()
                    .h_full()
                    .flex_none()
                    .gap_1()
                    .child(collapse_button),
            )
    }

    fn render_contents(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.searching {
            return self.render_searching(window, cx);
        }
        match &self.results {
            None => self
                .render_empty_state(&["Run Find All References", "with Shift-F12 to see results"]),
            Some(results) if results.matches.is_empty() => {
                self.render_empty_state(&["No references found"])
            }
            Some(_) => self.render_results_list(window, cx),
        }
    }

    fn render_searching(&self, _window: &mut Window, _cx: &mut Context<Self>) -> AnyElement {
        // A plain rotating spinner like the agent panel's, using the muted
        // color the other agent spinners use (`loading_contents_spinner` in
        // `thread_view` uses the accent color).
        v_flex()
            .size_full()
            .flex_1()
            .items_center()
            .justify_center()
            .child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted)
                    .with_rotate_animation(3),
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
            return self.render_empty_state(&["No references found"]);
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
            .child(list)
            // Long reference lines need a horizontal scrollbar; the list's
            // `Unconstrained` sizing already enables horizontal scrolling. Both
            // axes follow the global `editor.scrollbar.show` setting.
            .custom_scrollbars(
                Scrollbars::for_settings::<ReferencesPanelScrollbarAccessor>()
                    .tracked_scroll_handle(&self.scroll_handle.clone())
                    .with_track_along_for(
                        ScrollAxes::Vertical,
                        EditorSettings::get_global(cx).scrollbar.track,
                        cx.theme().colors().panel_background,
                    )
                    .with_track_along(ScrollAxes::Horizontal, cx.theme().colors().panel_background)
                    .tracked_entity(cx.entity_id()),
                window,
                cx,
            )
            .into_any_element()
    }

    fn render_entry(
        &self,
        results: &ReferenceResults,
        entry_index: usize,
        entry: Entry,
        max_line_number: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match entry {
            Entry::Header(path) => self.render_file_header(&path, window, cx),
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

    fn render_file_header(
        &self,
        path: &ProjectPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        let path_for_context_menu = path.clone();
        let (_, hover_background) = row_backgrounds(cx);
        h_flex()
            .id(path.path.as_std_path().to_string_lossy().into_owned())
            .w_full()
            .min_w_0()
            .px(DynamicSpacing::Base06.rems(cx))
            // Fixed row height, same as the match rows: `uniform_list` applies
            // one measured row to every row, so header and match heights must
            // match or collapsing/expanding files would reflow the whole list.
            .h_7()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(hover_background))
            .on_drag(
                path.clone(),
                move |path: &ProjectPath, click_offset, _window, cx| {
                    cx.new(|_| DraggedFileView {
                        filename: path
                            .path
                            .file_name()
                            .map(|name| name.to_string())
                            .unwrap_or_default(),
                        click_offset,
                    })
                },
            )
            .external_drag_payload({
                let project = self.project.clone();
                move |path: &ProjectPath, _window, cx| {
                    let project = project.read(cx);
                    let worktree = project.worktree_for_id(path.worktree_id, cx)?;
                    let worktree = worktree.read(cx);
                    if !worktree.is_local() {
                        return None;
                    }
                    Some(ExternalDragPayload::Files(FileDragPaths::new([(
                        worktree.absolutize(&path.path),
                        false,
                    )])))
                }
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_group(path.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.deploy_context_menu_for_path(
                        &path_for_context_menu,
                        event.position,
                        window,
                        cx,
                    );
                }),
            )
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
        results: &ReferenceResults,
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
        let (selected_background, hover_background) = row_backgrounds(cx);
        h_flex()
            .id(entry_index)
            .w_full()
            .min_w_0()
            .px_1p5()
            .h_7()
            .gap_2p5()
            .text_sm()
            .cursor_pointer()
            // The active row keeps its selected background while hovered, like
            // the thread panel rows.
            .when(selected, |this| this.bg(selected_background))
            .when(!selected, |this| {
                this.hover(|style| style.bg(hover_background))
            })
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

impl EventEmitter<PanelEvent> for ReferencesPanel {}

impl Focusable for ReferencesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for ReferencesPanel {
    fn persistent_name() -> &'static str {
        "References Panel"
    }

    fn panel_key() -> &'static str {
        REFERENCES_PANEL_KEY
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

    fn default_size(&self, _window: &Window, cx: &App) -> Pixels {
        ProjectPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(IconName::Quote)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Find All References")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}

impl Render for ReferencesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("references-panel")
            .size_full()
            .overflow_hidden()
            .key_context(self.dispatch_context())
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .child(self.render_header(window, cx))
            .child(self.render_contents(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_lsp_test_context::EditorLspTestContext;
    use gpui::TestAppContext;
    use indoc::indoc;
    use language::Point;
    use workspace::Item as _;

    async fn rust_cx(
        capabilities: lsp::ServerCapabilities,
        cx: &mut TestAppContext,
    ) -> EditorLspTestContext {
        cx.update(crate::init);
        EditorLspTestContext::new_rust(capabilities, cx).await
    }

    fn references(uri: lsp::Uri, ranges: &[(u32, u32, u32)]) -> Vec<lsp::Location> {
        ranges
            .iter()
            .map(|&(row, start, end)| lsp::Location {
                uri: uri.clone(),
                range: lsp::Range::new(
                    lsp::Position::new(row, start),
                    lsp::Position::new(row, end),
                ),
            })
            .collect()
    }

    const SOURCE: &str = indoc! {r#"
        fn main() {
            let aˇbc = 123;
            let xyz = abc;
        }
    "#};

    fn add_panel(cx: &mut EditorLspTestContext) -> Entity<ReferencesPanel> {
        let workspace = cx.workspace.clone();
        workspace.update_in(&mut cx.cx.cx, |workspace, window, cx| {
            let panel = cx.new(|cx| ReferencesPanel::new(workspace, cx));
            workspace.add_panel(panel, window, cx);
            workspace
                .panel::<ReferencesPanel>(cx)
                .expect("panel should be registered in the left dock")
        })
    }

    #[gpui::test]
    async fn test_panel_shows_hint_before_any_query(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        let panel = add_panel(&mut cx);
        cx.update(|_window, cx| {
            assert!(
                panel.read(cx).results.is_none(),
                "no query has run yet, so the panel should show the hint state"
            );
        });
    }

    #[gpui::test]
    async fn test_find_all_references_opens_panel_with_results(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });
        let _ = add_panel(&mut cx);

        cx.dispatch_action(FindAllReferences::default());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        let (results_len, match_count, dock_open) = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            let results = panel
                .read(cx)
                .results
                .as_ref()
                .expect("results should be set");
            (
                results.entries.len(),
                results.matches.len(),
                workspace.read(cx).left_dock().read(cx).is_open(),
            )
        });
        assert_eq!(results_len, 3, "one file header plus two matches");
        assert_eq!(match_count, 2);
        assert!(dock_open, "the left dock should be open after a query");
    }

    #[gpui::test]
    async fn test_find_all_references_without_results(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |_params, _| {
                Ok(Some(Vec::new()))
            });
        let _ = add_panel(&mut cx);

        cx.dispatch_action(FindAllReferences::default());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            let results = panel
                .read(cx)
                .results
                .as_ref()
                .expect("results should be set");
            assert!(results.matches.is_empty());
        });
    }

    #[gpui::test]
    async fn test_cmd_click_fallback_shows_references_in_panel(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(async move |_params, _| {
                Ok(None)
            });
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });
        let _ = add_panel(&mut cx);

        let screen_coord = cx
            .editor(|editor, _, cx| editor.pixel_position_of_cursor(cx))
            .unwrap();
        cx.simulate_click(screen_coord, gpui::Modifiers::secondary_key());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        let match_count = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel
                .read(cx)
                .results
                .as_ref()
                .expect("cmd-click fallback should populate the references panel")
                .matches
                .len()
        });
        assert_eq!(
            match_count, 2,
            "the cmd-click go-to-definition fallback should show references in the panel"
        );
    }

    #[gpui::test]
    async fn test_opening_reference_reopens_file_when_editor_gone(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });
        let _ = add_panel(&mut cx);

        cx.dispatch_action(FindAllReferences::default());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        let entry_index = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel
                .read(cx)
                .results
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .position(|entry| matches!(entry, Entry::Match(1)))
                .expect("second reference should be present")
        });

        // Close the only file so there is no tab to navigate in; dropping the
        // editor happens in the app when the last tab is closed.
        cx.update_workspace(|workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            let item_id = pane.read(cx).active_item().unwrap().item_id();
            pane.update(cx, |pane, cx| {
                pane.remove_item(item_id, false, false, window, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.update(cx, |panel, cx| {
                panel.selected_entry = Some(entry_index);
                panel.open_selected_entry(window, cx);
            });
        });
        cx.run_until_parked();

        let (editor_count, selection) = cx.update_workspace(|workspace, _, cx| {
            let editors = workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>();
            let editor_count = editors.len();
            let selection = editors.into_iter().next().map(|editor| {
                editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    editor.selections.newest::<Point>(&display_snapshot).range()
                })
            });
            (editor_count, selection)
        });
        assert_eq!(
            editor_count, 1,
            "the reference's file should be reopened in the workspace"
        );
        assert_eq!(selection, Some(Point::new(2, 14)..Point::new(2, 17)));
    }

    #[gpui::test]
    async fn test_collapse_and_expand_groups(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });
        let _ = add_panel(&mut cx);

        cx.dispatch_action(FindAllReferences::default());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        let entries_len = |cx: &mut EditorLspTestContext| -> usize {
            let panel = workspace.read_with(&cx.cx.cx, |workspace, cx| {
                workspace.panel::<ReferencesPanel>(cx).unwrap()
            });
            panel.read_with(&cx.cx.cx, |panel, _| {
                panel.results.as_ref().unwrap().entries.len()
            })
        };
        assert_eq!(entries_len(&mut cx), 3, "one header plus two matches");

        cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.update(cx, |panel, cx| panel.set_all_collapsed(true, cx));
        });
        assert_eq!(entries_len(&mut cx), 1, "only the file header remains");

        cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.update(cx, |panel, cx| panel.set_all_collapsed(false, cx));
        });
        assert_eq!(entries_len(&mut cx), 3, "matches are restored");
    }

    #[gpui::test]
    async fn test_toggle_group_toggles_only_that_file(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });
        let _ = add_panel(&mut cx);

        cx.dispatch_action(FindAllReferences::default());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        let path = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            match &panel.read(cx).results.as_ref().unwrap().entries[0] {
                Entry::Header(path) => path.clone(),
                Entry::Match(_) => panic!("first entry should be a file header"),
            }
        });

        cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.update(cx, |panel, cx| panel.toggle_group(path.clone(), cx));
        });
        let entries_len = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.read(cx).results.as_ref().unwrap().entries.len()
        });
        assert_eq!(entries_len, 1, "toggling the group hides its matches");

        cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.update(cx, |panel, cx| panel.toggle_group(path, cx));
        });
        let entries_len = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.read(cx).results.as_ref().unwrap().entries.len()
        });
        assert_eq!(entries_len, 3, "toggling again restores the matches");
    }

    #[gpui::test]
    async fn test_selecting_reference_navigates_editor(cx: &mut TestAppContext) {
        let mut cx = rust_cx(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..Default::default()
            },
            cx,
        )
        .await;
        cx.set_state(SOURCE);
        cx.lsp
            .set_request_handler::<lsp::request::References, _, _>(async move |params, _| {
                let uri = params.text_document_position.text_document.uri;
                Ok(Some(references(uri, &[(1, 8, 11), (2, 14, 17)])))
            });
        let _ = add_panel(&mut cx);

        cx.dispatch_action(FindAllReferences::default());
        cx.run_until_parked();

        let workspace = cx.workspace.clone();
        let entry_index = cx.update(|_window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel
                .read(cx)
                .results
                .as_ref()
                .unwrap()
                .entries
                .iter()
                .position(|entry| matches!(entry, Entry::Match(1)))
                .expect("second reference should be present")
        });
        cx.update(|window, cx| {
            let panel = workspace
                .read(cx)
                .panel::<ReferencesPanel>(cx)
                .expect("references panel should exist");
            panel.update(cx, |panel, cx| {
                panel.selected_entry = Some(entry_index);
                panel.open_selected_entry(window, cx);
            });
        });
        cx.run_until_parked();

        cx.assert_editor_state(indoc! {r#"
            fn main() {
                let abc = 123;
                let xyz = «abcˇ»;
            }
        "#});
    }
}
