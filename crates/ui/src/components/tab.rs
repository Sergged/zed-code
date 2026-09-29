use std::cmp::Ordering;

use gpui::{AnyElement, IntoElement, Stateful};
use smallvec::SmallVec;

use crate::prelude::*;

const START_TAB_SLOT_SIZE: Pixels = px(12.);

/// The position of a [`Tab`] within a list of tabs.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabPosition {
    /// The tab is first in the list.
    First,

    /// The tab is in the middle of the list (i.e., it is not the first or last tab).
    ///
    /// The [`Ordering`] is where this tab is positioned with respect to the selected tab.
    Middle(Ordering),

    /// The tab is last in the list.
    Last,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TabCloseSide {
    Start,
    End,
}

#[derive(IntoElement, RegisterComponent)]
pub struct Tab {
    div: Stateful<Div>,
    selected: bool,
    position: TabPosition,
    close_side: TabCloseSide,
    start_slot: Option<AnyElement>,
    end_slot: Option<AnyElement>,
    children: SmallVec<[AnyElement; 2]>,
}

impl Tab {
    pub fn new(id: impl Into<ElementId>) -> Self {
        let id = id.into();
        Self {
            div: div()
                .id(id.clone())
                .debug_selector(|| format!("TAB-{}", id)),
            selected: false,
            position: TabPosition::First,
            close_side: TabCloseSide::End,
            start_slot: None,
            end_slot: None,
            children: SmallVec::new(),
        }
    }

    pub fn position(mut self, position: TabPosition) -> Self {
        self.position = position;
        self
    }

    pub fn close_side(mut self, close_side: TabCloseSide) -> Self {
        self.close_side = close_side;
        self
    }

    pub fn start_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.start_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    pub fn end_slot<E: IntoElement>(mut self, element: impl Into<Option<E>>) -> Self {
        self.end_slot = element.into().map(IntoElement::into_any_element);
        self
    }

    pub fn content_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx) - px(1.)
    }

    pub fn container_height(cx: &App) -> Pixels {
        DynamicSpacing::Base32.px(cx)
    }
}

impl InteractiveElement for Tab {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl StatefulInteractiveElement for Tab {}

impl Toggleable for Tab {
    fn toggle_state(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

impl ParentElement for Tab {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for Tab {
    #[allow(refining_impl_trait)]
    fn render(self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        let (text_color, tab_bg, _tab_hover_bg, _tab_active_bg) = match self.selected {
            false => (
                cx.theme().colors().text_muted,
                cx.theme().colors().tab_inactive_background,
                cx.theme().colors().ghost_element_hover,
                cx.theme().colors().ghost_element_active,
            ),
            true => (
                cx.theme().colors().text,
                cx.theme().colors().tab_active_background,
                cx.theme().colors().element_hover,
                cx.theme().colors().element_active,
            ),
        };

        let (start_slot, end_slot) = match self.close_side {
            TabCloseSide::End => (self.start_slot, self.end_slot),
            TabCloseSide::Start => (self.end_slot, self.start_slot),
        };

        // The trailing slot is sized to the close button, whose box includes
        // padding around its icon. Subtract that padding from the tab's outer
        // padding so the icon keeps the intended distance to the tab's edge.
        let (end_slot_icon_size, end_slot_padding) = IconSize::Small.square_components(window, cx);
        let end_slot_size = end_slot_icon_size + end_slot_padding * 2.;
        // The tab's outer element reserves 1px on each side (position padding
        // or border), and the close button's box includes padding around its
        // icon. Compensate for both so the three visible insets — left edge to
        // file icon, label to close icon, close icon to right edge — are equal.
        let inset = DynamicSpacing::Base12.px(cx);
        let (pl, gap, pr) = match self.close_side {
            TabCloseSide::End => (
                inset - px(1.),
                inset - end_slot_padding,
                inset - end_slot_padding - px(1.),
            ),
            TabCloseSide::Start => (
                inset + end_slot_padding - px(1.),
                inset - end_slot_padding,
                inset - px(1.),
            ),
        };

        self.div
            .h(Tab::container_height(cx))
            .bg(tab_bg)
            .border_color(cx.theme().colors().border)
            .map(|this| match self.position {
                TabPosition::First => {
                    if self.selected {
                        this.pl_px().border_r_1().pb_px()
                    } else {
                        this.pl_px().pr_px().border_b_1()
                    }
                }
                TabPosition::Last => {
                    if self.selected {
                        this.border_l_1().border_r_1().pb_px()
                    } else {
                        this.pl_px().border_b_1().border_r_1()
                    }
                }
                TabPosition::Middle(Ordering::Equal) => this.border_l_1().border_r_1().pb_px(),
                TabPosition::Middle(Ordering::Less) => this.border_l_1().pr_px().border_b_1(),
                TabPosition::Middle(Ordering::Greater) => this.border_r_1().pl_px().border_b_1(),
            })
            .cursor_pointer()
            .child(
                h_flex()
                    .group("")
                    .relative()
                    .h(Tab::content_height(cx))
                    .pl(pl)
                    .pr(pr)
                    .gap(gap)
                    .text_color(text_color)
                    .when_some(start_slot, |this, content| {
                        // Only reserve space in the leading slot when it
                        // actually has content, keeping the leading edge tight
                        // like other editors' tabs.
                        this.child(
                            h_flex()
                                .size(START_TAB_SLOT_SIZE)
                                .justify_center()
                                .child(content),
                        )
                    })
                    .children(self.children)
                    .child(
                        // The trailing slot is sized to the close button so the
                        // button and the unsaved-changes indicator share one
                        // identical width and swapping between them doesn't
                        // shift the layout.
                        h_flex()
                            .size(end_slot_size)
                            .justify_center()
                            .children(end_slot),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IconButtonShape;
    use gpui::{Render, TestAppContext};
    use std::cell::Cell;
    use std::rc::Rc;

    struct GeometryProbe {
        icon_padding: Rc<Cell<Pixels>>,
    }

    impl Render for GeometryProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.icon_padding
                .set(IconSize::Small.square_components(window, cx).1);
            h_flex().child(
                Tab::new("probe")
                    .position(TabPosition::Middle(Ordering::Less))
                    .child(
                        h_flex()
                            .debug_selector(|| "probe-content".into())
                            .gap_1()
                            .child(
                                div()
                                    .debug_selector(|| "probe-icon".into())
                                    .child(Icon::new(IconName::FileRust).size(IconSize::Small)),
                            )
                            .child(
                                div()
                                    .debug_selector(|| "probe-label".into())
                                    .child(Label::new("main.rs").single_line()),
                            ),
                    )
                    .end_slot(
                        div().debug_selector(|| "probe-close".into()).child(
                            IconButton::new("close", IconName::Close)
                                .shape(IconButtonShape::Square)
                                .size(ButtonSize::None)
                                .icon_size(IconSize::Small),
                        ),
                    ),
            )
        }
    }

    #[gpui::test]
    fn test_tab_insets_are_equal(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let icon_padding = Rc::new(Cell::new(px(0.)));
        let (_probe, cx) = cx.add_window_view({
            let icon_padding = icon_padding.clone();
            |_, _| GeometryProbe { icon_padding }
        });
        cx.run_until_parked();

        let tab = cx.debug_bounds("TAB-probe").unwrap();
        let icon = cx.debug_bounds("probe-icon").unwrap();
        let label = cx.debug_bounds("probe-label").unwrap();
        let close = cx.debug_bounds("probe-close").unwrap();
        // The close button's box is `icon + padding` on each side, so the icon
        // glyph sits `padding` inside the measured button box.
        let padding = icon_padding.get();

        let left_to_icon = icon.left() - tab.left();
        let label_to_close_icon = (close.left() + padding) - label.right();
        let close_icon_to_right = tab.right() - (close.right() - padding);

        let tolerance = px(0.5);
        let within = |a: Pixels, b: Pixels| a - b < tolerance && b - a < tolerance;
        assert!(
            within(left_to_icon, label_to_close_icon) && within(left_to_icon, close_icon_to_right),
            "tab insets should be equal, got left edge to icon: {left_to_icon:?}, \
             label to close icon: {label_to_close_icon:?}, close icon to right edge: {close_icon_to_right:?}"
        );
    }
}

impl Component for Tab {
    fn scope() -> ComponentScope {
        ComponentScope::Navigation
    }

    fn description() -> &'static str {
        "A tab component that can be used in a tabbed interface, \
        supporting different positions and states."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![example_group_with_title(
                "Variations",
                vec![
                    single_example(
                        "Default",
                        Tab::new("default").child("Default Tab").into_any_element(),
                    ),
                    single_example(
                        "Selected",
                        Tab::new("selected")
                            .toggle_state(true)
                            .child("Selected Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "First",
                        Tab::new("first")
                            .position(TabPosition::First)
                            .child("First Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "Middle",
                        Tab::new("middle")
                            .position(TabPosition::Middle(Ordering::Equal))
                            .child("Middle Tab")
                            .into_any_element(),
                    ),
                    single_example(
                        "Last",
                        Tab::new("last")
                            .position(TabPosition::Last)
                            .child("Last Tab")
                            .into_any_element(),
                    ),
                ],
            )])
            .into_any_element()
    }
}
