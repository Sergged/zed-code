mod actions;
pub(crate) mod autoscroll;
pub(crate) mod scroll_amount;

use crate::editor_settings::ScrollBeyondLastLine;
use crate::{
    Anchor, DisplayPoint, DisplayRow, Editor, EditorEvent, EditorMode, EditorSettings,
    MultiBufferSnapshot, RowExt, SelectionEffects, SizingBehavior, ToPoint,
    display_map::{DisplaySnapshot, ToDisplayPoint},
    hover_popover::hide_hover,
    persistence::EditorDb,
};
pub use autoscroll::{Autoscroll, AutoscrollStrategy};
use core::fmt::Debug;
use gpui::{
    Along, App, AppContext as _, Axis, Context, Entity, EntityId, OngoingScroll, Pixels, Task,
    TouchPhase, Window, point,
};
use language::language_settings::{AllLanguageSettings, SoftWrap};
use language::{Bias, Point};
use scheduler;
pub use scroll_amount::ScrollAmount;
use settings::Settings;
use std::{cmp::Ordering, time::Duration};
use ui::scrollbars::ScrollbarAutoHide;
use util::ResultExt;
use workspace::{ItemId, WorkspaceId};

const SCROLLBAR_SHOW_INTERVAL: Duration = Duration::from_secs(1);

/// Total duration of a smooth scroll animation, mirroring VS Code's ~125ms.
const SMOOTH_SCROLL_DURATION: Duration = Duration::from_millis(128);

pub struct WasScrolled(pub(crate) bool);

pub type ScrollOffset = f64;
pub type ScrollPixelOffset = f64;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollAnchor {
    pub offset: gpui::Point<ScrollOffset>,
    pub anchor: Anchor,
}

impl ScrollAnchor {
    pub(super) fn new() -> Self {
        Self {
            offset: gpui::Point::default(),
            anchor: Anchor::Min,
        }
    }

    pub fn scroll_position(&self, snapshot: &DisplaySnapshot) -> gpui::Point<ScrollOffset> {
        self.offset.apply_along(Axis::Vertical, |offset| {
            if self.anchor == Anchor::Min {
                0.
            } else {
                let scroll_top = self.anchor.to_display_point(snapshot).row().as_f64();
                (offset + scroll_top).max(0.)
            }
        })
    }

    pub fn top_row(&self, buffer: &MultiBufferSnapshot) -> u32 {
        self.anchor.to_point(buffer).row
    }
}

/// In the split diff view, the two sides share a ScrollAnchor using this struct.
/// Either side can set a ScrollAnchor that points to its own multibuffer, and we store the ID of the display map
/// that the last-written anchor came from so that we know how to resolve it to a DisplayPoint.
///
/// For normal editors, this just acts as a wrapper around a ScrollAnchor.
#[derive(Clone, Copy, Debug)]
pub struct SharedScrollAnchor {
    pub scroll_anchor: ScrollAnchor,
    pub display_map_id: Option<EntityId>,
}

impl SharedScrollAnchor {
    pub fn scroll_position(&self, snapshot: &DisplaySnapshot) -> gpui::Point<ScrollOffset> {
        let snapshot = if let Some(display_map_id) = self.display_map_id
            && display_map_id != snapshot.display_map_id
        {
            let companion_snapshot = snapshot
                .companion_snapshot()
                .expect("shared scroll anchor references a non native display map, but snapshot has no companion");
            assert_eq!(
                companion_snapshot.display_map_id, display_map_id,
                "shared scroll anchor display map should match the snapshot's split companion"
            );
            companion_snapshot
        } else {
            snapshot
        };

        self.scroll_anchor.scroll_position(snapshot)
    }

    pub fn scroll_top_display_point(&self, snapshot: &DisplaySnapshot) -> DisplayPoint {
        let snapshot = if let Some(display_map_id) = self.display_map_id
            && display_map_id != snapshot.display_map_id
        {
            let companion_snapshot = snapshot
                .companion_snapshot()
                .expect("shared scroll anchor references a non native display map, but snapshot has no companion");
            assert_eq!(
                companion_snapshot.display_map_id, display_map_id,
                "shared scroll anchor display map should match the snapshot's split companion"
            );
            companion_snapshot
        } else {
            snapshot
        };

        self.scroll_anchor.anchor.to_display_point(snapshot)
    }
}

#[derive(Copy, Clone, Default, PartialEq, Eq)]
pub enum ScrollbarThumbState {
    #[default]
    Idle,
    Hovered,
    Dragging,
}

#[derive(PartialEq, Eq)]
pub struct ActiveScrollbarState {
    axis: Axis,
    thumb_state: ScrollbarThumbState,
}

impl ActiveScrollbarState {
    pub fn new(axis: Axis, thumb_state: ScrollbarThumbState) -> Self {
        ActiveScrollbarState { axis, thumb_state }
    }

    pub fn thumb_state_for_axis(&self, axis: Axis) -> Option<ScrollbarThumbState> {
        (self.axis == axis).then_some(self.thumb_state)
    }
}

pub struct ScrollManager {
    pub(crate) vertical_scroll_margin: ScrollOffset,
    anchor: Entity<SharedScrollAnchor>,
    /// Value to be used for clamping the x component of the SharedScrollAnchor's offset.
    ///
    /// We store this outside the SharedScrollAnchor so that the two sides of a split diff can share
    /// a horizontal scroll offset that may be out of range for one of the editors (when one side is wider than the other).
    /// Each side separately clamps the x component using its own scroll_max_x when reading from the SharedScrollAnchor.
    scroll_max_x: Option<f64>,
    ongoing: OngoingScroll,
    smooth_scroll_animation: Option<SmoothScrollAnimation>,
    /// The second element indicates whether the autoscroll request is local
    /// (true) or remote (false). Local requests are initiated by user actions,
    /// while remote requests come from external sources.
    autoscroll_request: Option<(Autoscroll, bool)>,
    last_autoscroll: Option<(
        gpui::Point<ScrollOffset>,
        ScrollOffset,
        ScrollOffset,
        AutoscrollStrategy,
    )>,
    show_scrollbars: bool,
    hide_scrollbar_task: Option<Task<()>>,
    active_scrollbar: Option<ActiveScrollbarState>,
    visible_line_count: Option<f64>,
    visible_column_count: Option<f64>,
    forbid_vertical_scroll: bool,
    minimap_thumb_state: Option<ScrollbarThumbState>,
    _save_scroll_position_task: Task<()>,
}

impl ScrollManager {
    pub fn new(cx: &mut Context<Editor>) -> Self {
        let anchor = cx.new(|_| SharedScrollAnchor {
            scroll_anchor: ScrollAnchor::new(),
            display_map_id: None,
        });
        ScrollManager {
            vertical_scroll_margin: EditorSettings::get_global(cx).vertical_scroll_margin,
            anchor,
            scroll_max_x: None,
            ongoing: OngoingScroll::default(),
            smooth_scroll_animation: None,
            autoscroll_request: None,
            show_scrollbars: true,
            hide_scrollbar_task: None,
            active_scrollbar: None,
            last_autoscroll: None,
            visible_line_count: None,
            visible_column_count: None,
            forbid_vertical_scroll: false,
            minimap_thumb_state: None,
            _save_scroll_position_task: Task::ready(()),
        }
    }

    pub fn set_native_display_map_id(
        &mut self,
        display_map_id: EntityId,
        cx: &mut Context<Editor>,
    ) {
        self.anchor.update(cx, |shared, _| {
            if shared.display_map_id.is_none() {
                shared.display_map_id = Some(display_map_id);
            }
        });
    }

    pub fn clone_state(
        &mut self,
        other: &Self,
        other_snapshot: &DisplaySnapshot,
        my_snapshot: &DisplaySnapshot,
        cx: &mut Context<Editor>,
    ) {
        let native_anchor = other.native_anchor(other_snapshot, cx);
        self.anchor.update(cx, |this, _| {
            this.scroll_anchor = native_anchor;
            this.display_map_id = Some(my_snapshot.display_map_id);
        });
        self.ongoing = other.ongoing;
    }

    pub fn offset(&self, cx: &App) -> gpui::Point<f64> {
        let mut offset = self.anchor.read(cx).scroll_anchor.offset;
        if let Some(max_x) = self.scroll_max_x {
            offset.x = offset.x.min(max_x);
        }
        offset
    }

    /// Get a ScrollAnchor whose `anchor` field is guaranteed to point into the multibuffer for the provided snapshot.
    ///
    /// For normal editors, this just retrieves the internal ScrollAnchor and is lossless. When the editor is part of a split diff,
    /// we may need to translate the anchor to point to the "native" multibuffer first. That translation is lossy,
    /// so this method should be used sparingly---if you just need a scroll position or display point, call the appropriate helper method instead,
    /// since they can losslessly handle the case where the ScrollAnchor was last set from the other side.
    pub fn native_anchor(&self, snapshot: &DisplaySnapshot, cx: &App) -> ScrollAnchor {
        let shared = self.anchor.read(cx);

        let mut result = if let Some(display_map_id) = shared.display_map_id
            && display_map_id != snapshot.display_map_id
        {
            let companion_snapshot = snapshot
                .companion_snapshot()
                .expect("shared scroll anchor references a non native display map, but the snapshot has no companion");
            assert_eq!(
                companion_snapshot.display_map_id, display_map_id,
                "shared scroll anchor display map should match the companion used for native anchor conversion"
            );

            let mut display_point = shared
                .scroll_anchor
                .anchor
                .to_display_point(companion_snapshot);
            *display_point.column_mut() = 0;
            let buffer_point = snapshot.display_point_to_point(display_point, Bias::Left);
            let anchor = snapshot.buffer_snapshot().anchor_before(buffer_point);
            ScrollAnchor {
                anchor,
                offset: shared.scroll_anchor.offset,
            }
        } else {
            shared.scroll_anchor
        };

        if let Some(max_x) = self.scroll_max_x {
            result.offset.x = result.offset.x.min(max_x);
        }
        result
    }

    pub fn shared_scroll_anchor(&self, cx: &App) -> SharedScrollAnchor {
        let mut shared = *self.anchor.read(cx);
        if let Some(max_x) = self.scroll_max_x {
            shared.scroll_anchor.offset.x = shared.scroll_anchor.offset.x.min(max_x);
        }
        shared
    }

    pub fn scroll_top_display_point(&self, snapshot: &DisplaySnapshot, cx: &App) -> DisplayPoint {
        self.anchor.read(cx).scroll_top_display_point(snapshot)
    }

    pub fn scroll_anchor_entity(&self) -> Entity<SharedScrollAnchor> {
        self.anchor.clone()
    }

    pub fn set_shared_scroll_anchor(&mut self, entity: Entity<SharedScrollAnchor>) {
        self.anchor = entity;
    }

    pub fn unshare_scroll_anchor(&mut self, snapshot: &DisplaySnapshot, cx: &mut Context<Editor>) {
        let scroll_anchor = self.native_anchor(snapshot, cx);
        self.anchor = cx.new(|_| SharedScrollAnchor {
            scroll_anchor,
            display_map_id: Some(snapshot.display_map_id),
        });
    }

    pub fn filter_scroll_delta(
        &mut self,
        delta: &mut gpui::Point<Pixels>,
        touch_phase: TouchPhase,
    ) {
        self.ongoing.filter(delta, touch_phase);
    }

    pub fn scroll_position(
        &self,
        snapshot: &DisplaySnapshot,
        cx: &App,
    ) -> gpui::Point<ScrollOffset> {
        let mut pos = self.anchor.read(cx).scroll_position(snapshot);
        if let Some(max_x) = self.scroll_max_x {
            pos.x = pos.x.min(max_x);
        }
        pos
    }

    fn set_scroll_position(
        &mut self,
        scroll_position: gpui::Point<ScrollOffset>,
        map: &DisplaySnapshot,
        scroll_beyond_last_line: ScrollBeyondLastLine,
        local: bool,
        autoscroll: bool,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> WasScrolled {
        let scroll_top = scroll_position.y.max(0.);
        let scroll_top = match scroll_beyond_last_line {
            ScrollBeyondLastLine::OnePage => scroll_top,
            ScrollBeyondLastLine::Off => {
                if let Some(height_in_lines) = self.visible_line_count {
                    let max_row = map.max_point().row().as_f64();
                    scroll_top.min(max_row - height_in_lines + 1.).max(0.)
                } else {
                    scroll_top
                }
            }
            ScrollBeyondLastLine::VerticalScrollMargin => {
                if let Some(height_in_lines) = self.visible_line_count {
                    let max_row = map.max_point().row().as_f64();
                    scroll_top
                        .min(max_row - height_in_lines + 1. + self.vertical_scroll_margin)
                        .max(0.)
                } else {
                    scroll_top
                }
            }
        };
        let scroll_top_row = DisplayRow(scroll_top as u32);
        let scroll_top_buffer_point = map
            .clip_point(
                DisplayPoint::new(scroll_top_row, scroll_position.x as u32),
                Bias::Left,
            )
            .to_point(map);
        let top_anchor = map.buffer_snapshot().anchor_before(scroll_top_buffer_point);

        self.set_anchor(
            ScrollAnchor {
                anchor: top_anchor,
                offset: point(
                    scroll_position.x.max(0.),
                    scroll_top - top_anchor.to_display_point(map).row().as_f64(),
                ),
            },
            map,
            scroll_top_buffer_point.row,
            local,
            autoscroll,
            workspace_id,
            window,
            cx,
        )
    }

    fn set_anchor(
        &mut self,
        anchor: ScrollAnchor,
        display_map: &DisplaySnapshot,
        top_row: u32,
        local: bool,
        autoscroll: bool,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> WasScrolled {
        let adjusted_anchor = if self.forbid_vertical_scroll {
            let current = self.anchor.read(cx);
            ScrollAnchor {
                offset: gpui::Point::new(anchor.offset.x, current.scroll_anchor.offset.y),
                anchor: current.scroll_anchor.anchor,
            }
        } else {
            anchor
        };

        self.scroll_max_x.take();
        self.autoscroll_request.take();

        let current = self.anchor.read(cx);
        if current.scroll_anchor == adjusted_anchor {
            return WasScrolled(false);
        }

        self.anchor.update(cx, |shared, _| {
            shared.scroll_anchor = adjusted_anchor;
            shared.display_map_id = Some(display_map.display_map_id);
        });
        cx.emit(EditorEvent::ScrollPositionChanged { local, autoscroll });
        self.show_scrollbars(window, cx);
        if let Some(workspace_id) = workspace_id {
            let item_id = cx.entity().entity_id().as_u64() as ItemId;
            let executor = cx.background_executor().clone();

            let db = EditorDb::global(cx);
            self._save_scroll_position_task = cx.background_executor().spawn(async move {
                executor.timer(Duration::from_millis(10)).await;
                log::debug!(
                    "Saving scroll position for item {item_id:?} in workspace {workspace_id:?}"
                );
                db.save_scroll_position(
                    item_id,
                    workspace_id,
                    top_row,
                    anchor.offset.x,
                    anchor.offset.y,
                )
                .await
                .log_err();
            });
        }
        cx.notify();

        WasScrolled(true)
    }

    pub fn show_scrollbars(&mut self, window: &mut Window, cx: &mut Context<Editor>) {
        if !self.show_scrollbars {
            self.show_scrollbars = true;
            cx.notify();
        }

        if cx.default_global::<ScrollbarAutoHide>().should_hide() {
            self.hide_scrollbar_task = Some(cx.spawn_in(window, async move |editor, cx| {
                cx.background_executor()
                    .timer(SCROLLBAR_SHOW_INTERVAL)
                    .await;
                editor
                    .update(cx, |editor, cx| {
                        editor.scroll_manager.show_scrollbars = false;
                        cx.notify();
                    })
                    .log_err();
            }));
        } else {
            self.hide_scrollbar_task = None;
        }
    }

    pub fn scrollbars_visible(&self) -> bool {
        self.show_scrollbars
    }

    pub fn has_autoscroll_request(&self) -> bool {
        self.autoscroll_request.is_some()
    }

    pub fn take_autoscroll_request(&mut self) -> Option<(Autoscroll, bool)> {
        self.autoscroll_request.take()
    }

    pub fn active_scrollbar_state(&self) -> Option<&ActiveScrollbarState> {
        self.active_scrollbar.as_ref()
    }

    pub fn dragging_scrollbar_axis(&self) -> Option<Axis> {
        self.active_scrollbar
            .as_ref()
            .filter(|scrollbar| scrollbar.thumb_state == ScrollbarThumbState::Dragging)
            .map(|scrollbar| scrollbar.axis)
    }

    pub fn any_scrollbar_dragged(&self) -> bool {
        self.active_scrollbar
            .as_ref()
            .is_some_and(|scrollbar| scrollbar.thumb_state == ScrollbarThumbState::Dragging)
    }

    pub fn set_hovered_scroll_thumb_axis(&mut self, axis: Axis, cx: &mut Context<Editor>) {
        self.update_active_scrollbar_state(
            Some(ActiveScrollbarState::new(
                axis,
                ScrollbarThumbState::Hovered,
            )),
            cx,
        );
    }

    pub fn set_dragged_scroll_thumb_axis(&mut self, axis: Axis, cx: &mut Context<Editor>) {
        self.update_active_scrollbar_state(
            Some(ActiveScrollbarState::new(
                axis,
                ScrollbarThumbState::Dragging,
            )),
            cx,
        );
    }

    pub fn reset_scrollbar_state(&mut self, cx: &mut Context<Editor>) {
        self.update_active_scrollbar_state(None, cx);
    }

    fn update_active_scrollbar_state(
        &mut self,
        new_state: Option<ActiveScrollbarState>,
        cx: &mut Context<Editor>,
    ) {
        if self.active_scrollbar != new_state {
            self.active_scrollbar = new_state;
            cx.notify();
        }
    }

    pub fn set_is_hovering_minimap_thumb(&mut self, hovered: bool, cx: &mut Context<Editor>) {
        self.update_minimap_thumb_state(
            Some(if hovered {
                ScrollbarThumbState::Hovered
            } else {
                ScrollbarThumbState::Idle
            }),
            cx,
        );
    }

    pub fn set_is_dragging_minimap(&mut self, cx: &mut Context<Editor>) {
        self.update_minimap_thumb_state(Some(ScrollbarThumbState::Dragging), cx);
    }

    pub fn hide_minimap_thumb(&mut self, cx: &mut Context<Editor>) {
        self.update_minimap_thumb_state(None, cx);
    }

    pub fn is_dragging_minimap(&self) -> bool {
        self.minimap_thumb_state
            .is_some_and(|state| state == ScrollbarThumbState::Dragging)
    }

    fn update_minimap_thumb_state(
        &mut self,
        thumb_state: Option<ScrollbarThumbState>,
        cx: &mut Context<Editor>,
    ) {
        if self.minimap_thumb_state != thumb_state {
            self.minimap_thumb_state = thumb_state;
            cx.notify();
        }
    }

    pub fn minimap_thumb_state(&self) -> Option<ScrollbarThumbState> {
        self.minimap_thumb_state
    }

    pub fn clamp_scroll_left(&mut self, max: f64, cx: &App) -> bool {
        let current_x = self.anchor.read(cx).scroll_anchor.offset.x;
        self.scroll_max_x = Some(max);
        current_x > max
    }

    pub fn set_forbid_vertical_scroll(&mut self, forbid: bool) {
        self.forbid_vertical_scroll = forbid;
    }

    pub fn forbid_vertical_scroll(&self) -> bool {
        self.forbid_vertical_scroll
    }
}

/// Interpolates a vertical scroll animation, inspired by VS Code's smooth
/// scrolling curve. Short jumps use `easeOutCubic`. Long jumps (more than two
/// and a half viewports) are split into three continuous phases: an eased
/// ramp over the first three quarters of a viewport, a fast linear sweep
/// through the middle, and an eased approach over the last three quarters of
/// a viewport. (VS Code's own composed curve jumps discontinuously at the
/// phase boundary; this one stays continuous.)
enum SmoothScrollCurve {
    Simple {
        start: f64,
        end: f64,
    },
    Composed {
        start: f64,
        sweep_start: f64,
        sweep_end: f64,
        end: f64,
    },
}

/// An in-flight smooth scroll animation.
///
/// The animation records when it started and re-interpolates the scroll
/// position from the elapsed time on every rendered frame (see
/// [`Editor::advance_smooth_scroll`]), so it runs at the display's refresh
/// rate instead of on a fixed timer.
struct SmoothScrollAnimation {
    /// The scroll position to finish at.
    target: gpui::Point<ScrollOffset>,
    /// The last scroll y applied by the animation, used to detect manual
    /// scrolling during the animation.
    last_applied_y: f64,
    /// When the animation started, in the virtualized test clock.
    started_at: scheduler::Instant,
    curve: SmoothScrollCurve,
    duration: Duration,
}

impl SmoothScrollCurve {
    fn interpolate(&self, t: f32) -> f64 {
        fn ease_out_cubic(t: f64) -> f64 {
            1. - (1. - t).powi(3)
        }

        let t = t as f64;
        match self {
            SmoothScrollCurve::Simple { start, end } => {
                *start + (*end - *start) * ease_out_cubic(t)
            }
            SmoothScrollCurve::Composed {
                start,
                sweep_start,
                sweep_end,
                end,
            } => {
                const RAMP: f64 = 0.33;
                if t < RAMP {
                    *start + (*sweep_start - *start) * ease_out_cubic(t / RAMP)
                } else if t < 1. - RAMP {
                    let u = (t - RAMP) / (1. - 2. * RAMP);
                    *sweep_start + (*sweep_end - *sweep_start) * u
                } else {
                    let u = (t - (1. - RAMP)) / RAMP;
                    *sweep_end + (*end - *sweep_end) * ease_out_cubic(u)
                }
            }
        }
    }
}

impl Editor {
    pub fn has_autoscroll_request(&self) -> bool {
        self.scroll_manager.has_autoscroll_request()
    }

    /// Smoothly scrolls the viewport to where the given autoscroll would take it.
    ///
    /// The target position is computed by applying the autoscroll once, then the
    /// viewport is rewound to the start and animated there via
    /// `smooth_scroll_to_position`. The animation is frame-driven, so it is
    /// deterministic in tests, where the scheduler clock is virtualized and
    /// frames are simulated explicitly.
    pub fn smooth_scroll_to(
        &mut self,
        autoscroll: Autoscroll,
        start_scroll: gpui::Point<ScrollOffset>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |editor, cx| -> anyhow::Result<()> {
            // Apply the autoscroll once to compute the target viewport position,
            // then rewind to the start and animate between the two positions.
            let target_scroll = editor
                .update_in(cx, |editor, window, cx| {
                    let display_map = editor.display_map.update(cx, |map, cx| map.snapshot(cx));
                    let line_height = editor
                        .style(cx)
                        .text
                        .line_height_in_pixels(window.rem_size());
                    let Some(visible_lines) = editor.visible_line_count() else {
                        return None;
                    };
                    let bounds = gpui::Bounds {
                        origin: gpui::Point::default(),
                        size: gpui::Size::new(
                            gpui::px(0.),
                            gpui::px((visible_lines * f64::from(line_height)).max(1.0) as f32),
                        ),
                    };
                    let height_in_lines = f64::from(bounds.size.height / line_height);
                    let max_row = display_map.max_point().row().as_f64();
                    let max_scroll_top = match editor.scroll_beyond_last_line(cx) {
                        ScrollBeyondLastLine::OnePage => max_row,
                        ScrollBeyondLastLine::Off => (max_row - height_in_lines + 1.).max(0.),
                        ScrollBeyondLastLine::VerticalScrollMargin => {
                            let margin = EditorSettings::get_global(cx).vertical_scroll_margin;
                            (max_row - height_in_lines + 1. + margin).max(0.)
                        }
                    };
                    editor.autoscroll_vertically(
                        bounds,
                        line_height,
                        max_scroll_top,
                        Some((autoscroll, true)),
                        window,
                        cx,
                    );
                    Some(editor.scroll_position(cx))
                })
                .ok()
                .flatten();

            let Some(target_scroll) = target_scroll else {
                // The editor has never been laid out, so the target is unknown.
                // Fall back to the plain autoscroll behavior.
                editor.update_in(cx, |editor, _, cx| {
                    editor.request_autoscroll(autoscroll, cx);
                })?;
                return Ok(());
            };

            editor.update_in(cx, |editor, window, cx| {
                editor.set_scroll_position(start_scroll, window, cx);
            })?;

            editor.update_in(cx, |editor, _, cx| {
                editor.smooth_scroll_to_position(start_scroll, target_scroll, cx);
            })?;
            Ok(())
        })
        .detach();
    }

    /// Smoothly animates the viewport between two scroll positions.
    ///
    /// The animation is frame-driven: it records the target and the time it
    /// started, and is advanced once per rendered frame by
    /// `advance_smooth_scroll`, so it runs at the display's refresh rate rather
    /// than on a fixed timer. It aborts as soon as the user scrolls manually.
    ///
    /// Jumps spanning more than two and a half viewports are split into a
    /// fast sweep through the middle with an eased approach at each end,
    /// mirroring VS Code's smooth scrolling, so long distances don't crawl.
    pub(crate) fn smooth_scroll_to_position(
        &mut self,
        start_scroll: gpui::Point<ScrollOffset>,
        target_scroll: gpui::Point<ScrollOffset>,
        cx: &mut Context<Self>,
    ) {
        let start_y = start_scroll.y;
        let end_y = target_scroll.y;
        let curve = if let Some(viewport_rows) = self.visible_line_count()
            && (end_y - start_y).abs() > 2.5 * viewport_rows
        {
            let (sweep_start, sweep_end) = if end_y > start_y {
                (start_y + 0.75 * viewport_rows, end_y - 0.75 * viewport_rows)
            } else {
                (start_y - 0.75 * viewport_rows, end_y + 0.75 * viewport_rows)
            };
            SmoothScrollCurve::Composed {
                start: start_y,
                sweep_start,
                sweep_end,
                end: end_y,
            }
        } else {
            SmoothScrollCurve::Simple {
                start: start_y,
                end: end_y,
            }
        };

        self.scroll_manager.smooth_scroll_animation = Some(SmoothScrollAnimation {
            target: target_scroll,
            // Read the position rather than trusting `start_scroll`, so the
            // abort check below tolerates clamping applied by the rewind.
            last_applied_y: self.scroll_position(cx).y,
            started_at: cx.background_executor().now(),
            curve,
            duration: SMOOTH_SCROLL_DURATION,
        });
        cx.notify();
    }

    /// Advances an in-flight smooth scroll animation by the time that has
    /// elapsed since it started, and requests another frame if it is still
    /// running. Called from the editor element's prepaint, i.e. once per
    /// rendered frame, so the animation follows the display's refresh rate
    /// rather than a fixed timer. Returns whether the scroll position changed.
    pub(crate) fn advance_smooth_scroll(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(animation) = self.scroll_manager.smooth_scroll_animation.take() else {
            return false;
        };

        let current_y = self.scroll_position(cx).y;
        if (current_y - animation.last_applied_y).abs() > 0.25 {
            // The user started scrolling manually while the animation was in
            // flight; drop the animation and keep their scroll position.
            return false;
        }

        let elapsed = cx
            .background_executor()
            .now()
            .saturating_duration_since(animation.started_at);
        let progress = elapsed.as_secs_f32() / animation.duration.as_secs_f32();
        if progress >= 1.0 {
            self.set_scroll_position(animation.target, window, cx);
            return true;
        }

        let y = animation.curve.interpolate(progress);
        self.set_scroll_position(gpui::Point::new(animation.target.x, y), window, cx);
        self.scroll_manager.smooth_scroll_animation = Some(SmoothScrollAnimation {
            last_applied_y: self.scroll_position(cx).y,
            ..animation
        });
        window.request_animation_frame();
        true
    }

    pub fn set_forbid_vertical_scroll(&mut self, forbid: bool) {
        self.scroll_manager.set_forbid_vertical_scroll(forbid);
    }

    pub fn scroll_top_display_point(&self, snapshot: &DisplaySnapshot, cx: &App) -> DisplayPoint {
        self.scroll_manager.scroll_top_display_point(snapshot, cx)
    }

    pub fn vertical_scroll_margin(&self) -> usize {
        self.scroll_manager.vertical_scroll_margin as usize
    }

    pub(crate) fn scroll_beyond_last_line(&self, cx: &App) -> ScrollBeyondLastLine {
        match self.mode {
            EditorMode::Minimap { .. }
            | EditorMode::Full {
                sizing_behavior: SizingBehavior::Default,
                ..
            } => EditorSettings::get_global(cx).scroll_beyond_last_line,

            EditorMode::Full { .. } | EditorMode::SingleLine | EditorMode::AutoHeight { .. } => {
                ScrollBeyondLastLine::Off
            }
        }
    }

    pub fn set_vertical_scroll_margin(&mut self, margin_rows: usize, cx: &mut Context<Self>) {
        self.scroll_manager.vertical_scroll_margin = margin_rows as f64;
        cx.notify();
    }

    pub fn visible_line_count(&self) -> Option<f64> {
        self.scroll_manager.visible_line_count
    }

    pub fn visible_row_count(&self) -> Option<u32> {
        self.visible_line_count()
            .map(|line_count| line_count as u32 - 1)
    }

    pub fn visible_column_count(&self) -> Option<f64> {
        self.scroll_manager.visible_column_count
    }

    pub(crate) fn set_visible_line_count(
        &mut self,
        lines: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opened_first_time = self.scroll_manager.visible_line_count.is_none();
        self.scroll_manager.visible_line_count = Some(lines);
        if opened_first_time {
            self.update_data_on_scroll(false, window, cx);
        }
    }

    pub(crate) fn set_visible_column_count(&mut self, columns: f64) {
        self.scroll_manager.visible_column_count = Some(columns);
    }

    pub fn apply_scroll_delta(
        &mut self,
        scroll_delta: gpui::Point<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut delta = scroll_delta;
        if self.scroll_manager.forbid_vertical_scroll {
            delta.y = 0.0;
        }
        let display_map = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let position = self.scroll_manager.scroll_position(&display_map, cx) + delta.map(f64::from);
        self.set_scroll_position_taking_display_map(position, true, false, display_map, window, cx);
    }

    pub fn set_scroll_position(
        &mut self,
        scroll_position: gpui::Point<ScrollOffset>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WasScrolled {
        let mut position = scroll_position;
        if self.scroll_manager.forbid_vertical_scroll {
            let current_position = self.scroll_position(cx);
            position.y = current_position.y;
        }
        self.set_scroll_position_internal(position, true, false, window, cx)
    }

    /// Scrolls so that `row` is at the top of the editor view.
    pub fn set_scroll_top_row(
        &mut self,
        row: DisplayRow,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let snapshot = self.snapshot(window, cx).display_snapshot;
        let new_screen_top = DisplayPoint::new(row, 0);
        let new_screen_top = new_screen_top.to_offset(&snapshot, Bias::Left);
        let new_anchor = snapshot.buffer_snapshot().anchor_before(new_screen_top);

        self.set_scroll_anchor(
            ScrollAnchor {
                anchor: new_anchor,
                offset: Default::default(),
            },
            window,
            cx,
        );
    }

    pub(crate) fn set_scroll_position_internal(
        &mut self,
        scroll_position: gpui::Point<ScrollOffset>,
        local: bool,
        autoscroll: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WasScrolled {
        let map = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let was_scrolled = self.set_scroll_position_taking_display_map(
            scroll_position,
            local,
            autoscroll,
            map,
            window,
            cx,
        );

        was_scrolled
    }

    fn set_scroll_position_taking_display_map(
        &mut self,
        scroll_position: gpui::Point<ScrollOffset>,
        local: bool,
        autoscroll: bool,
        display_map: DisplaySnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> WasScrolled {
        hide_hover(self, cx);
        let workspace_id = self.workspace.as_ref().and_then(|workspace| workspace.1);

        self.edit_prediction_preview
            .set_previous_scroll_position(None);

        let adjusted_position = if self.scroll_manager.forbid_vertical_scroll {
            let current_position = self.scroll_manager.scroll_position(&display_map, cx);
            gpui::Point::new(scroll_position.x, current_position.y)
        } else {
            scroll_position
        };
        let scroll_beyond_last_line = self.scroll_beyond_last_line(cx);
        self.scroll_manager.set_scroll_position(
            adjusted_position,
            &display_map,
            scroll_beyond_last_line,
            local,
            autoscroll,
            workspace_id,
            window,
            cx,
        )
    }

    pub fn scroll_position(&self, cx: &mut Context<Self>) -> gpui::Point<ScrollOffset> {
        let display_map = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        self.scroll_manager.scroll_position(&display_map, cx)
    }

    pub fn set_scroll_anchor(
        &mut self,
        scroll_anchor: ScrollAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        hide_hover(self, cx);
        let workspace_id = self.workspace.as_ref().and_then(|workspace| workspace.1);
        let display_map = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let top_row = scroll_anchor
            .anchor
            .to_point(&self.buffer().read(cx).snapshot(cx))
            .row;
        self.scroll_manager.set_anchor(
            scroll_anchor,
            &display_map,
            top_row,
            true,
            false,
            workspace_id,
            window,
            cx,
        );
    }

    pub(crate) fn set_scroll_anchor_remote(
        &mut self,
        scroll_anchor: ScrollAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        hide_hover(self, cx);
        let workspace_id = self.workspace.as_ref().and_then(|workspace| workspace.1);
        let buffer_snapshot = self.buffer().read(cx).snapshot(cx);
        if !scroll_anchor.anchor.is_valid(&buffer_snapshot) {
            log::warn!("Invalid scroll anchor: {:?}", scroll_anchor);
            return;
        }
        let display_map = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let top_row = scroll_anchor.anchor.to_point(&buffer_snapshot).row;
        self.scroll_manager.set_anchor(
            scroll_anchor,
            &display_map,
            top_row,
            false,
            false,
            workspace_id,
            window,
            cx,
        );
    }

    pub fn scroll_screen(
        &mut self,
        amount: &ScrollAmount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.mode, EditorMode::SingleLine) {
            cx.propagate();
            return;
        }

        if self.take_rename(true, window, cx).is_some() {
            return;
        }

        let mut current_position = self.scroll_position(cx);
        let Some(visible_line_count) = self.visible_line_count() else {
            return;
        };
        let Some(mut visible_column_count) = self.visible_column_count() else {
            return;
        };

        // If the user has a preferred line length, and has the editor
        // configured to wrap at the preferred line length, or bounded to it,
        // use that value over the visible column count. This was mostly done so
        // that tests could actually be written for vim's `z l`, `z h`, `z
        // shift-l` and `z shift-h` commands, as there wasn't a good way to
        // configure the editor to only display a certain number of columns. If
        // that ever happens, this could probably be removed.
        let settings = AllLanguageSettings::get_global(cx);
        if matches!(settings.defaults.soft_wrap, SoftWrap::Bounded)
            && (settings.defaults.preferred_line_length as f64) < visible_column_count
        {
            visible_column_count = settings.defaults.preferred_line_length as f64;
        }

        // If the scroll position is currently at the left edge of the document
        // (x == 0.0) and the intent is to scroll right, the gutter's margin
        // should first be added to the current position, otherwise the cursor
        // will end at the column position minus the margin, which looks off.
        if current_position.x == 0.0
            && amount.columns(visible_column_count) > 0.
            && let Some(last_position_map) = &self.last_position_map
        {
            current_position.x +=
                f64::from(self.gutter_dimensions.margin / last_position_map.em_advance);
        }
        let new_position = current_position
            + point(
                amount.columns(visible_column_count),
                amount.lines(visible_line_count),
            );
        self.set_scroll_position(new_position, window, cx);
    }

    pub fn scroll_screen_with_cursor_margin(
        &mut self,
        amount: &ScrollAmount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scroll_screen(amount, window, cx);

        let Some(visible_line_count) = self.visible_line_count() else {
            return;
        };
        let display_snapshot = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let top = self
            .scroll_manager
            .scroll_top_display_point(&display_snapshot, cx);
        let vertical_scroll_margin =
            (self.vertical_scroll_margin() as u32).min(visible_line_count as u32 / 2);

        let max_point = display_snapshot.max_point();
        let min_row = if top.row().0 == 0 {
            DisplayRow(0)
        } else {
            DisplayRow(top.row().0 + vertical_scroll_margin)
        };
        let max_row = if top.row().0 + visible_line_count as u32 >= max_point.row().0 {
            max_point.row()
        } else {
            DisplayRow(
                (top.row().0 + visible_line_count as u32)
                    .saturating_sub(1 + vertical_scroll_margin),
            )
        };

        self.change_selections(
            SelectionEffects::no_scroll().nav_history(false),
            window,
            cx,
            |s| {
                s.move_with(&mut |map, selection| {
                    let head = selection.head();
                    let new_row = if head.row() < min_row {
                        min_row
                    } else if head.row() > max_row {
                        max_row
                    } else {
                        head.row()
                    };
                    if new_row != head.row() {
                        let new_head =
                            map.clip_point(DisplayPoint::new(new_row, head.column()), Bias::Left);
                        selection.collapse_to(new_head, selection.goal);
                    }
                })
            },
        );
    }

    /// Returns an ordering. The newest selection is:
    ///     Ordering::Equal => on screen
    ///     Ordering::Less => above or to the left of the screen
    ///     Ordering::Greater => below or to the right of the screen
    pub fn newest_selection_on_screen(&self, window: &mut Window, cx: &mut App) -> Ordering {
        let snapshot = self.display_map.update(cx, |map, cx| map.snapshot(cx));
        let newest_head = self
            .selections
            .newest_anchor()
            .head()
            .to_display_point(&snapshot);
        let screen_top = self.scroll_manager.scroll_top_display_point(&snapshot, cx);

        if screen_top > newest_head {
            return Ordering::Less;
        }

        if let (Some(visible_lines), Some(visible_columns)) =
            (self.visible_line_count(), self.visible_column_count())
            && newest_head.row() <= DisplayRow(screen_top.row().0 + visible_lines as u32)
        {
            let text_layout_details = self.text_layout_details(window, cx);
            let font_id = text_layout_details
                .text_system
                .resolve_font(&text_layout_details.editor_style.text.font());
            let font_size = text_layout_details
                .editor_style
                .text
                .font_size
                .to_pixels(text_layout_details.rem_size);
            let on_screen = match text_layout_details
                .text_system
                .em_advance(font_id, font_size)
                .log_err()
            {
                Some(em_advance) => {
                    let head_x = snapshot.x_for_display_point(newest_head, &text_layout_details);
                    let screen_left_x =
                        snapshot.x_for_display_point(screen_top, &text_layout_details);
                    head_x <= screen_left_x + em_advance * visible_columns as f32
                }
                None => newest_head.column() <= screen_top.column() + visible_columns as u32,
            };
            if on_screen {
                return Ordering::Equal;
            }
        }

        Ordering::Greater
    }

    pub fn read_scroll_position_from_db(
        &mut self,
        item_id: u64,
        workspace_id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let scroll_position = EditorDb::global(cx).get_scroll_position(item_id, workspace_id);
        if let Ok(Some((top_row, x, y))) = scroll_position {
            let top_anchor = self
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_before(Point::new(top_row, 0));
            let scroll_anchor = ScrollAnchor {
                offset: gpui::Point::new(x, y),
                anchor: top_anchor,
            };
            self.set_scroll_anchor(scroll_anchor, window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SmoothScrollCurve;

    #[test]
    fn test_curve_endpoints() {
        let simple = SmoothScrollCurve::Simple {
            start: 5.,
            end: 20.,
        };
        assert_eq!(simple.interpolate(0.), 5.);
        assert_eq!(simple.interpolate(1.), 20.);

        let composed = SmoothScrollCurve::Composed {
            start: 0.,
            sweep_start: 7.5,
            sweep_end: 92.5,
            end: 100.,
        };
        assert_eq!(composed.interpolate(0.), 0.);
        assert_eq!(composed.interpolate(1.), 100.);
    }

    #[test]
    fn test_curves_are_monotonic() {
        let simple = SmoothScrollCurve::Simple {
            start: 100.,
            end: 10.,
        };
        let composed = SmoothScrollCurve::Composed {
            start: 100.,
            sweep_start: 92.5,
            sweep_end: 7.5,
            end: 0.,
        };
        for curve in [simple, composed] {
            let mut previous = curve.interpolate(0.);
            for i in 1..=100 {
                let value = curve.interpolate(i as f32 / 100.);
                assert!(
                    value < previous,
                    "curve must move monotonically towards the target"
                );
                previous = value;
            }
        }
    }

    #[test]
    fn test_composed_curve_is_continuous() {
        // Unlike VS Code's composed curve, which jumps at the phase boundary,
        // ours must not teleport at any time during the animation.
        let composed = SmoothScrollCurve::Composed {
            start: 0.,
            sweep_start: 7.5,
            sweep_end: 92.5,
            end: 100.,
        };
        let mut previous = composed.interpolate(0.);
        for i in 1..=10_000 {
            let value = composed.interpolate(i as f32 / 10_000.);
            let step = value - previous;
            assert!(
                step > 0. && step < 0.05,
                "curve must move continuously without teleporting at t = {}",
                i as f32 / 10_000.
            );
            previous = value;
        }
    }
}
