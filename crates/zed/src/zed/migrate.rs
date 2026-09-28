use anyhow::{Context as _, Result};
use editor::Editor;
use fs::Fs;
use gpui::WeakEntity;
use migrator::{SettingChange, migrate_keymap, migrate_settings, settings_diff};
use settings::{KeymapFile, Settings, SettingsStore};
use util::ResultExt;
use workspace::notifications::NotifyTaskExt;

use std::sync::Arc;

use gpui::{Entity, EventEmitter, Global, Task, TextStyle, TextStyleRefinement};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use theme_settings::ThemeSettings;
use ui::{Tooltip, prelude::*};
use workspace::item::ItemHandle;
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MigrationType {
    Keymap,
    Settings,
}

pub struct MigrationBanner {
    workspace: WeakEntity<Workspace>,
    migration_type: Option<MigrationType>,
    should_migrate_task: Option<Task<()>>,
    markdown: Option<Entity<Markdown>>,
    changes: Vec<SettingChange>,
}

pub enum MigrationEvent {
    ContentChanged {
        migration_type: MigrationType,
        migrating_in_memory: bool,
    },
}

pub struct MigrationNotification;

impl EventEmitter<MigrationEvent> for MigrationNotification {}

impl MigrationNotification {
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalMigrationNotification>()
            .map(|notifier| notifier.0.clone())
    }

    pub fn set_global(notifier: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalMigrationNotification(notifier));
    }
}

struct GlobalMigrationNotification(Entity<MigrationNotification>);

impl Global for GlobalMigrationNotification {}

impl MigrationBanner {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        if let Some(notifier) = MigrationNotification::try_global(cx) {
            cx.subscribe(
                &notifier,
                move |migrator_banner, _, event: &MigrationEvent, cx| {
                    migrator_banner.handle_notification(event, cx);
                },
            )
            .detach();
        }
        Self {
            workspace,
            migration_type: None,
            should_migrate_task: None,
            markdown: None,
            changes: Vec::new(),
        }
    }

    fn handle_notification(&mut self, event: &MigrationEvent, cx: &mut Context<Self>) {
        match event {
            MigrationEvent::ContentChanged {
                migration_type,
                migrating_in_memory,
            } => {
                if *migrating_in_memory {
                    self.migration_type = Some(*migration_type);
                    self.refresh_and_show(cx);
                } else {
                    cx.emit(ToolbarItemEvent::ChangeLocation(
                        ToolbarItemLocation::Hidden,
                    ));
                    self.reset(cx);
                };
            }
        }
    }

    /// Reads the settings or keymap file, computes which entries the migration
    /// changes, and shows the banner with that list as a tooltip.
    fn refresh_and_show(&mut self, cx: &mut Context<Self>) {
        let Some(migration_type) = self.migration_type else {
            return;
        };
        let fs = <dyn Fs>::global(cx);
        self.should_migrate_task = Some(cx.spawn(async move |this, cx| {
            if let Some(changes) = migration_changes(fs, migration_type).await {
                this.update(cx, |this, cx| this.show(changes, cx)).log_err();
            }
        }));
    }

    fn show(&mut self, changes: Vec<SettingChange>, cx: &mut Context<Self>) {
        let (file_type, backup_file_name) = match self.migration_type {
            Some(MigrationType::Keymap) => (
                "keymap",
                paths::keymap_backup_file()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            Some(MigrationType::Settings) => (
                "settings",
                paths::settings_backup_file()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            None => return,
        };

        let migration_text = format!(
            "Your {} file uses deprecated settings which can be \
            automatically updated. A backup will be saved to `{}`",
            file_type, backup_file_name
        );

        self.markdown = Some(cx.new(|cx| Markdown::new(migration_text.into(), None, None, cx)));
        self.changes = changes;

        cx.emit(ToolbarItemEvent::ChangeLocation(
            ToolbarItemLocation::Secondary,
        ));
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.should_migrate_task.take();
        self.migration_type.take();
        self.markdown.take();
        self.changes.clear();
        cx.notify();
    }
}

impl EventEmitter<ToolbarItemEvent> for MigrationBanner {}

impl ToolbarItemView for MigrationBanner {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.reset(cx);

        let Some(target) = active_pane_item
            .and_then(|item| item.act_as::<Editor>(cx))
            .and_then(|editor| editor.update(cx, |editor, cx| editor.target_file_abs_path(cx)))
        else {
            return ToolbarItemLocation::Hidden;
        };

        if &target == paths::keymap_file() {
            self.migration_type = Some(MigrationType::Keymap);
            self.refresh_and_show(cx);
        } else if &target == paths::settings_file() {
            self.migration_type = Some(MigrationType::Settings);
            self.refresh_and_show(cx);
        }

        ToolbarItemLocation::Hidden
    }
}

impl Render for MigrationBanner {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let migration_type = self.migration_type;
        let changed_settings = format_changed_settings(migration_type, &self.changes);
        let settings = ThemeSettings::get_global(cx);
        let ui_font_family = settings.ui_font.family.clone();
        let line_height = settings.ui_font_size(cx) * 1.3;
        h_flex()
            .id("migration-banner")
            .py_1()
            .pl_2()
            .pr_1()
            .justify_between()
            .bg(cx.theme().status().info_background.opacity(0.6))
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .when(!changed_settings.is_empty(), |this| {
                this.tooltip(Tooltip::text(changed_settings))
            })
            .child(
                h_flex()
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_size(TextSize::Default.rems(cx))
                            .max_h(2 * line_height)
                            .when_some(self.markdown.as_ref(), |this, markdown| {
                                this.child(
                                    MarkdownElement::new(
                                        markdown.clone(),
                                        MarkdownStyle {
                                            base_text_style: TextStyle {
                                                color: cx.theme().colors().text,
                                                font_family: ui_font_family,
                                                ..Default::default()
                                            },
                                            inline_code: TextStyleRefinement {
                                                background_color: Some(
                                                    cx.theme().colors().background,
                                                ),
                                                ..Default::default()
                                            },
                                            ..Default::default()
                                        },
                                    )
                                    .into_any_element(),
                                )
                            }),
                    ),
            )
            .child(
                Button::new("backup-and-migrate", "Backup and Update").on_click({
                    let workspace = self.workspace.clone();
                    move |_, window, cx| {
                        let fs = <dyn Fs>::global(cx);
                        let task = match migration_type {
                            Some(MigrationType::Keymap) => {
                                cx.background_spawn(write_keymap_migration(fs.clone()))
                            }
                            Some(MigrationType::Settings) => {
                                cx.background_spawn(write_settings_migration(fs.clone()))
                            }
                            None => unreachable!(),
                        };
                        task.detach_and_notify_err(workspace.clone(), window, cx);
                    }
                }),
            )
            .into_any_element()
    }
}

async fn migration_changes(
    fs: Arc<dyn Fs>,
    migration_type: MigrationType,
) -> Option<Vec<SettingChange>> {
    let (old_text, new_text) = match migration_type {
        MigrationType::Keymap => {
            let old_text = KeymapFile::load_keymap_file(&fs).await.ok()?;
            let new_text = migrate_keymap(&old_text).ok().flatten()?;
            (old_text, new_text)
        }
        MigrationType::Settings => {
            let old_text = SettingsStore::load_settings(&fs).await.ok()?;
            let new_text = migrate_settings(&old_text).ok().flatten()?;
            (old_text, new_text)
        }
    };
    Some(settings_diff(&old_text, &new_text))
}

/// Cap on how many changed settings the tooltip lists, so a sweeping migration
/// does not produce an unusable wall of text.
const MAX_TOOLTIP_CHANGES: usize = 20;
const MAX_TOOLTIP_VALUE_CHARS: usize = 80;

/// Renders the changed settings as tooltip lines, one per setting.
fn format_changed_settings(
    migration_type: Option<MigrationType>,
    changes: &[SettingChange],
) -> String {
    if changes.is_empty() {
        return String::new();
    }

    let header = match migration_type {
        Some(MigrationType::Keymap) => "The migration updates these key bindings:",
        _ => "The migration updates these settings:",
    };
    let mut lines = Vec::with_capacity(changes.len() + 1);
    lines.push(header.to_string());

    for change in changes.iter().take(MAX_TOOLTIP_CHANGES) {
        let line = match (&change.before, &change.after) {
            (Some(before), Some(after)) => format!(
                "{}: {} -> {}",
                change.path,
                value_text(before),
                value_text(after)
            ),
            (Some(before), None) => format!("{}: {} (removed)", change.path, value_text(before)),
            (None, Some(after)) => format!("{}: {} (added)", change.path, value_text(after)),
            (None, None) => continue,
        };
        lines.push(line);
    }

    if changes.len() > MAX_TOOLTIP_CHANGES {
        lines.push(format!("…and {} more", changes.len() - MAX_TOOLTIP_CHANGES));
    }

    lines.join("\n")
}

fn value_text(value: &serde_json::Value) -> String {
    let mut text = value.to_string();
    if text.chars().count() > MAX_TOOLTIP_VALUE_CHARS {
        text = text.chars().take(MAX_TOOLTIP_VALUE_CHARS).collect();
        text.push('…');
    }
    text
}

async fn write_keymap_migration(fs: Arc<dyn Fs>) -> Result<()> {
    let old_text = KeymapFile::load_keymap_file(&fs).await?;
    let Ok(Some(new_text)) = migrate_keymap(&old_text) else {
        return Ok(());
    };
    let keymap_path = paths::keymap_file().as_path();
    if fs.is_file(keymap_path).await {
        fs.atomic_write(paths::keymap_backup_file().to_path_buf(), old_text)
            .await
            .with_context(|| "Failed to create settings backup in home directory".to_string())?;
        let resolved_path = fs
            .canonicalize(keymap_path)
            .await
            .with_context(|| format!("Failed to canonicalize keymap path {:?}", keymap_path))?;
        fs.atomic_write(resolved_path.clone(), new_text)
            .await
            .with_context(|| format!("Failed to write keymap to file {:?}", resolved_path))?;
    } else {
        fs.atomic_write(keymap_path.to_path_buf(), new_text)
            .await
            .with_context(|| format!("Failed to write keymap to file {:?}", keymap_path))?;
    }
    Ok(())
}

async fn write_settings_migration(fs: Arc<dyn Fs>) -> Result<()> {
    let old_text = SettingsStore::load_settings(&fs).await?;
    let Ok(Some(new_text)) = migrate_settings(&old_text) else {
        return Ok(());
    };
    let settings_path = paths::settings_file().as_path();
    if fs.is_file(settings_path).await {
        fs.atomic_write(paths::settings_backup_file().to_path_buf(), old_text)
            .await
            .with_context(|| "Failed to create settings backup in home directory".to_string())?;
        let resolved_path = fs
            .canonicalize(settings_path)
            .await
            .with_context(|| format!("Failed to canonicalize settings path {:?}", settings_path))?;
        fs.atomic_write(resolved_path.clone(), new_text)
            .await
            .with_context(|| format!("Failed to write settings to file {:?}", resolved_path))?;
    } else {
        fs.atomic_write(settings_path.to_path_buf(), new_text)
            .await
            .with_context(|| format!("Failed to write settings to file {:?}", settings_path))?;
    }
    Ok(())
}
