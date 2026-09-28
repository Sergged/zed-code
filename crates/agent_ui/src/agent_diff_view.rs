//! A single-file review view for the agent's edits.
//!
//! This is the per-file counterpart to [`crate::AgentDiffPane`]: where the pane
//! shows *all* changed files in one multibuffer, this view shows exactly one
//! file, collapsed to its diff hunks, with the agent's per-hunk Keep/Reject
//! controls. It is inspired by the git panel's `SoloDiffView` but is
//! deliberately git-agnostic: the diff comes straight from the agent's
//! action log (`buffer` vs its pristine base), not from a repository.

use crate::agent_diff::{
    agent_diff_renderer, keep_edits_in_selection, reject_edits_in_ranges, reject_edits_in_selection,
};
use crate::{Keep, KeepAll, Reject, RejectAll};
use acp_thread::AcpThread;
use action_log::ActionLogTelemetry;
use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{
    Anchor, Direction, Editor, EditorEvent, EditorSettings, SplittableEditor, ToggleSplitDiff,
    actions::{GoToHunk, GoToPreviousHunk},
};
use gpui::{
    Action, AnyElement, App, AppContext as _, Context, Empty, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, SharedString, Subscription, Task, TaskExt, WeakEntity, Window,
    div,
};
use language::{Anchor as TextAnchor, Buffer, OffsetRangeExt as _, Point};
use multi_buffer::{MultiBuffer, excerpt_context_lines};
use project::{Project, ProjectPath};
use settings::{Settings, SettingsStore};
use std::any::{Any, TypeId};
use std::ops::Range;
use std::sync::Arc;
use ui::{
    Button, Color, Divider, Icon, IconButton, IconName, KeyBinding, Label, Tooltip, prelude::*,
};
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::{ItemEvent, SaveOptions, TabContentParams, TabTooltipContent},
    searchable::SearchableItemHandle,
};

/// A single changed file, reviewed in isolation.
pub struct AgentDiffView {
    thread: Entity<AcpThread>,
    buffer: Entity<Buffer>,
    editor: Entity<SplittableEditor>,
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    _settings_subscription: Subscription,
}

impl AgentDiffView {
    /// Opens (or focuses an existing) single-file review view for `buffer`.
    pub fn open_or_focus(
        thread: Entity<AcpThread>,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        workspace
            .update(cx, |workspace, cx| {
                let existing = workspace
                    .items_of_type::<Self>(cx)
                    .find(|item| item.read(cx).buffer.entity_id() == buffer.entity_id());
                if let Some(existing) = existing {
                    workspace.activate_item(&existing, true, true, window, cx);
                    return existing;
                }

                let view = cx.new(|cx| {
                    Self::new(
                        thread.clone(),
                        buffer.clone(),
                        diff.clone(),
                        workspace.weak_handle(),
                        window,
                        cx,
                    )
                });
                workspace.add_item_to_center(Box::new(view.clone()), window, cx);
                view
            })
            .ok()
    }

    fn new(
        thread: Entity<AcpThread>,
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = thread.read(cx).project().clone();
        let focus_handle = cx.focus_handle();
        let showing_full_file = EditorSettings::get_global(cx).file_diff.show_full_file;

        let multibuffer = cx
            .new(|cx| Self::build_multibuffer(buffer.clone(), diff.clone(), showing_full_file, cx));
        let editor = cx.new(|cx| {
            let workspace_entity = workspace.upgrade().expect("workspace must exist");
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project,
                workspace_entity,
                window,
                cx,
            );
            editor
                .set_diff_hunk_renderer(Some(agent_diff_renderer(&thread, workspace.clone())), cx);
            // Open on the first hunk rather than the top of the file, matching
            // the git panel's `SoloDiffView`.
            editor.rhs_editor().update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                editor.go_to_hunk_before_or_after_position(
                    &snapshot,
                    Point::new(0, 0),
                    Direction::Next,
                    true,
                    window,
                    cx,
                );
            });
            editor
        });

        let mut previous_diff_view_style = EditorSettings::get_global(cx).diff_view_style;
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                let diff_view_style = EditorSettings::get_global(cx).diff_view_style;
                if diff_view_style != previous_diff_view_style {
                    this.editor.update(cx, |editor, cx| {
                        if editor.diff_view_style() != diff_view_style {
                            editor.toggle_split(&ToggleSplitDiff, window, cx);
                        }
                    });
                    previous_diff_view_style = diff_view_style;
                    cx.notify();
                }
            });

        Self {
            thread,
            buffer,
            editor,
            focus_handle,
            workspace,
            _settings_subscription: settings_subscription,
        }
    }

    fn build_multibuffer(
        buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        showing_full_file: bool,
        cx: &mut Context<MultiBuffer>,
    ) -> MultiBuffer {
        let (ranges, context_line_count) =
            Self::excerpt_ranges(&buffer, &diff, showing_full_file, cx);

        let mut multibuffer = MultiBuffer::without_headers(buffer.read(cx).capability());
        multibuffer.set_excerpts_for_buffer(buffer, ranges, context_line_count, cx);
        multibuffer.add_diff(diff, cx);
        multibuffer
    }

    fn excerpt_ranges(
        buffer: &Entity<Buffer>,
        diff: &Entity<BufferDiff>,
        showing_full_file: bool,
        cx: &App,
    ) -> (Vec<Range<Point>>, u32) {
        if showing_full_file {
            (vec![Point::zero()..buffer.read(cx).max_point()], 0)
        } else {
            (
                Self::hunk_ranges(buffer, diff, cx),
                excerpt_context_lines(cx),
            )
        }
    }

    fn hunk_ranges(
        buffer: &Entity<Buffer>,
        diff: &Entity<BufferDiff>,
        cx: &App,
    ) -> Vec<Range<Point>> {
        let buffer = buffer.read(cx);
        diff.read(cx)
            .snapshot(cx)
            .hunks_intersecting_range(
                TextAnchor::min_for_buffer(buffer.remote_id())
                    ..TextAnchor::max_for_buffer(buffer.remote_id()),
                buffer,
            )
            .map(|diff_hunk| diff_hunk.buffer_range.to_point(buffer))
            .collect()
    }

    fn keep(&mut self, _: &Keep, window: &mut Window, cx: &mut Context<Self>) {
        let rhs_editor = self.editor.read(cx).rhs_editor().clone();
        rhs_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            keep_edits_in_selection(editor, &snapshot, &self.thread, window, cx);
        });
    }

    fn reject(&mut self, _: &Reject, window: &mut Window, cx: &mut Context<Self>) {
        let rhs_editor = self.editor.read(cx).rhs_editor().clone();
        rhs_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            reject_edits_in_selection(
                editor,
                &snapshot,
                &self.thread,
                self.workspace.clone(),
                window,
                cx,
            );
        });
    }

    fn reject_all(&mut self, _: &RejectAll, window: &mut Window, cx: &mut Context<Self>) {
        let rhs_editor = self.editor.read(cx).rhs_editor().clone();
        rhs_editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            reject_edits_in_ranges(
                editor,
                &snapshot,
                &self.thread,
                vec![Anchor::Min..Anchor::Max],
                self.workspace.clone(),
                window,
                cx,
            );
        });
    }

    fn keep_all(&mut self, _: &KeepAll, _window: &mut Window, cx: &mut Context<Self>) {
        let telemetry = ActionLogTelemetry::from(self.thread.read(cx));
        let action_log = self.thread.read(cx).action_log().clone();
        action_log.update(cx, |action_log, cx| {
            action_log.keep_all_edits(Some(telemetry), cx)
        });
    }

    /// Opens the working-tree file this diff reviews in the editor.
    fn view_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(project_path) = self.buffer.read(cx).file().map(|file| ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        }) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace
                .open_path_preview(project_path, None, false, false, true, window, cx)
                .detach_and_log_err(cx);
        });
    }
}

impl EventEmitter<EditorEvent> for AgentDiffView {}

impl Focusable for AgentDiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for AgentDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let label_content = self.tab_content_text(params.detail.unwrap_or_default(), cx);

        Label::new(label_content)
            .when(!params.selected, |this| this.color(Color::Muted))
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .read(cx)
            .file()
            .and_then(|file| {
                Some(
                    file.full_path(cx)
                        .file_name()?
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .unwrap_or_else(|| "Agent Diff".to_string())
            .into()
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.buffer
                .read(cx)
                .file()
                .map(|file| file.full_path(cx).to_string_lossy().into_owned())
                .unwrap_or_else(|| "Agent Diff".to_string())
                .into(),
        )
    }

    fn tab_tooltip_content(&self, _cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Text(
            "Agent Diff — single file review".into(),
        ))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Agent Diff View Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor
            .read(cx)
            .rhs_editor()
            .for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .has_conflict(cx)
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(options, project, window, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for AgentDiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .key_context("AgentDiffView")
            .on_action(cx.listener(Self::keep))
            .on_action(cx.listener(Self::reject))
            .on_action(cx.listener(Self::reject_all))
            .on_action(cx.listener(Self::keep_all))
            .flex()
            .flex_col()
            .size_full()
            .child(self.editor.clone())
    }
}

/// Toolbar for [`AgentDiffView`]: hunk navigation plus per-file Keep/Reject and
/// View File.
pub struct AgentDiffViewToolbar {
    active_view: Option<WeakEntity<AgentDiffView>>,
}

impl AgentDiffViewToolbar {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self { active_view: None }
    }

    fn active_view(&self) -> Option<Entity<AgentDiffView>> {
        self.active_view.as_ref()?.upgrade()
    }

    fn dispatch(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view() else {
            return;
        };
        let focus_handle = view.read(cx).focus_handle.clone();
        focus_handle.focus(window, cx);
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        });
    }

    /// Dispatches an editor action (e.g. hunk navigation) to the diff editor,
    /// which is what actually handles it.
    fn dispatch_to_editor(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.active_view() else {
            return;
        };
        let focus_handle = view.read(cx).editor.read(cx).focus_handle(cx);
        focus_handle.focus(window, cx);
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        });
    }
}

impl EventEmitter<ToolbarItemEvent> for AgentDiffViewToolbar {}

impl ToolbarItemView for AgentDiffViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.active_view = active_pane_item
            .and_then(|item| item.act_as::<AgentDiffView>(cx))
            .map(|entity| entity.downgrade());

        if self.active_view.is_some() {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for AgentDiffViewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.active_view() else {
            return Empty.into_any_element();
        };

        let focus_handle = view.read(cx).focus_handle.clone();
        let editor_focus_handle = view.read(cx).editor.read(cx).focus_handle(cx);

        // Hunk navigation stays at the left; the review actions are pushed to
        // the toolbar's right edge. The platform's buffer-search bar renders the
        // fold/expand and split/unified controls for multibuffer items, so we
        // don't repeat them here (same as `AgentDiffPane`).
        h_flex()
            .flex_1()
            .pl_0p5()
            .gap_1()
            .child(
                IconButton::new("agent-diff-prev-hunk", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Previous Hunk",
                        &GoToPreviousHunk,
                        &editor_focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_to_editor(&GoToPreviousHunk, window, cx)
                    })),
            )
            .child(
                IconButton::new("agent-diff-next-hunk", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::for_action_title_in(
                        "Next Hunk",
                        &GoToHunk,
                        &editor_focus_handle,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.dispatch_to_editor(&GoToHunk, window, cx)
                    })),
            )
            .child(
                h_flex()
                    .ml_auto()
                    .gap_1()
                    .child(
                        Button::new("agent-diff-reject", "Reject")
                            .key_binding(
                                KeyBinding::for_action_in(&Reject, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch(&Reject, window, cx)
                            })),
                    )
                    .child(
                        Button::new("agent-diff-keep", "Keep")
                            .key_binding(
                                KeyBinding::for_action_in(&Keep, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.dispatch(&Keep, window, cx)),
                            ),
                    )
                    .child(Divider::vertical())
                    .child(
                        Button::new("agent-diff-reject-all", "Reject All")
                            .key_binding(
                                KeyBinding::for_action_in(&RejectAll, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch(&RejectAll, window, cx)
                            })),
                    )
                    .child(
                        Button::new("agent-diff-keep-all", "Keep All")
                            .key_binding(
                                KeyBinding::for_action_in(&KeepAll, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dispatch(&KeepAll, window, cx)
                            })),
                    )
                    .child(Divider::vertical())
                    .child(
                        Button::new("agent-diff-view-file", "View File")
                            .tooltip(Tooltip::text("View File"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(view) = this.active_view() {
                                    view.update(cx, |view, cx| view.view_file(window, cx));
                                }
                            })),
                    ),
            )
            .into_any_element()
    }
}
