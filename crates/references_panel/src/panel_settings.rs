use editor::ui_scrollbar_settings_from_raw;
use gpui::Pixels;
use settings::{IntoGpui, RegisterSetting, Settings};
use ui::scrollbars::{ScrollbarTrack, ShowScrollbar};
use workspace::dock::DockPosition;

#[derive(Debug, RegisterSetting)]
pub struct ReferencesPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub scrollbar: ReferencesPanelScrollbarSettings,
}

/// Scrollbar settings for the references panel.
///
/// `show` is `None` when unset, which means "inherit the editor's
/// `scrollbar.show`". `track` is never inherited: there is no global scrollbar
/// track setting, so an unset value falls back to the type default.
#[derive(Copy, Clone, Debug, Default)]
pub struct ReferencesPanelScrollbarSettings {
    pub show: Option<ShowScrollbar>,
    pub track: ScrollbarTrack,
}

impl Settings for ReferencesPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.references_panel.as_ref().unwrap();

        Self {
            button: panel.button.unwrap(),
            dock: panel.dock.unwrap().into(),
            default_width: panel.default_width.unwrap().into_gpui(),
            scrollbar: ReferencesPanelScrollbarSettings {
                show: panel
                    .scrollbar
                    .as_ref()
                    .and_then(|scrollbar| scrollbar.show)
                    .map(ui_scrollbar_settings_from_raw),
                track: panel
                    .scrollbar
                    .as_ref()
                    .and_then(|scrollbar| scrollbar.track)
                    .map(editor::ui_scrollbar_track_from_raw)
                    .unwrap_or_default(),
            },
        }
    }
}
