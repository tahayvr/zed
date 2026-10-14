use gpui::Entity;
use ui::{ButtonLike, CommonAnimationExt as _, Divider, DividerColor, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView};

use crate::service::{
    AttentionReason, SettingsSyncService, SyncState, describe_time_ago, open_sync_page_in,
};

/// Status bar indicator: hidden until sync is set up, then an icon that spins
/// while syncing and turns into a warning when the user has something to resolve.
pub struct SettingsSyncButton {
    /// `None` when settings sync was never initialized, as in tests that
    /// build a workspace without it.
    service: Option<Entity<SettingsSyncService>>,
}

impl SettingsSyncButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let service = SettingsSyncService::try_global(cx);
        if let Some(service) = &service {
            cx.observe(service, |_, _, cx| cx.notify()).detach();
        }
        Self { service }
    }
}

impl Render for SettingsSyncButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(service) = self.service.as_ref().map(|service| service.read(cx)) else {
            return div().hidden().into_any_element();
        };
        if !service.is_configured() {
            return div().hidden().into_any_element();
        }
        let state = service.state().clone();
        let last_sync = service.last_sync();

        let (icon, color) = match &state {
            SyncState::NotConfigured | SyncState::Idle => (IconName::ArrowCircle, Color::Muted),
            SyncState::Syncing(_) => (IconName::ArrowCircle, Color::Accent),
            SyncState::NeedsAttention(AttentionReason::Conflicts(_)) => {
                (IconName::Warning, Color::Warning)
            }
            SyncState::NeedsAttention(_) => (IconName::Warning, Color::Warning),
            SyncState::Paused => (IconName::ArrowCircle, Color::Disabled),
            SyncState::Error(_) => (IconName::Warning, Color::Error),
        };

        let tooltip = match &state {
            SyncState::Idle => match last_sync {
                Some(time) => format!("Settings synced {}", describe_time_ago(time)),
                None => "Settings sync is set up".to_string(),
            },
            SyncState::Error(message) => format!("Settings sync failed: {message}"),
            other => format!("Settings sync: {}", other.label()),
        };

        let icon_element = Icon::new(icon).size(IconSize::Small).color(color);
        let icon_element = if state.is_syncing() {
            icon_element.with_rotate_animation(2).into_any_element()
        } else {
            icon_element.into_any_element()
        };

        h_flex()
            .gap_0p5()
            .child(Divider::vertical().color(DividerColor::Border))
            .child(
                ButtonLike::new("settings-sync-status")
                    .child(icon_element)
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(|_, window, cx| open_sync_page_in(window, cx)),
            )
            .into_any_element()
    }
}

impl StatusItemView for SettingsSyncButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// The button is already conditional on sync being set up, so it offers
    /// no separate hide setting.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}
