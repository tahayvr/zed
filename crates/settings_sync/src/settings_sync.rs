//! Settings sync keeps Zed's user configuration in step across machines
//! through a private git repository the user controls. The engine and merge
//! logic are pure and tested; the service wires them to the filesystem, git
//! and the UI.

pub mod config;
pub mod engine;
pub mod git_backend;
pub mod history;
pub mod merge;
pub mod service;
mod status_item;

use std::sync::Arc;

use fs::Fs;
use gpui::{App, AppContext as _, actions};

pub use config::{ConflictStrategy, FileGroup, SyncConfig, SyncPaths};
pub use engine::{
    ConflictKind, FileConflict, FileSyncDecision, Resolution, Side, TrackedFile,
    platform_keymap_repo_file,
};
pub use history::{HistoryEntry, HistoryFile};
pub use merge::{DiffLine, DiffLineKind, KeyConflict, PathSetting, diff_lines};
pub use service::{
    AttentionReason, FileStatus, PendingConflict, RestorePreview, RestoreSource,
    SettingsSyncService, SyncEvent, SyncState, SyncSummary, describe_time_ago, open_sync_page_in,
};
pub use status_item::SettingsSyncButton;

/// `zed::OpenSettingsAt` path of the Settings Sync page in the settings window.
pub const SETTINGS_SYNC_PATH: &str = "settings_sync";

actions!(
    settings_sync,
    [
        /// Opens the Settings Sync page, where a repository can be linked.
        SetUp,
        /// Opens the Settings Sync page.
        Open,
        /// Syncs settings with the repository now.
        SyncNow,
        /// Pauses automatic settings sync.
        Pause,
        /// Resumes automatic settings sync.
        Resume,
        /// Opens the Settings Sync page at its history section.
        ViewHistory,
    ]
);

pub fn init(fs: Arc<dyn Fs>, cx: &mut App) {
    let service =
        cx.new(|cx| SettingsSyncService::new(fs, SyncPaths::new(SyncPaths::default_root()), cx));
    SettingsSyncService::set_global(service.clone(), cx);

    cx.on_action(|_: &SetUp, cx| open_sync_page(cx));
    cx.on_action(|_: &Open, cx| open_sync_page(cx));
    cx.on_action(|_: &ViewHistory, cx| open_sync_page(cx));
    cx.on_action({
        let service = service.clone();
        move |_: &SyncNow, cx| {
            service.update(cx, |service, cx| service.sync_now(true, cx));
        }
    });
    cx.on_action({
        let service = service.clone();
        move |_: &Pause, cx| {
            service.update(cx, |service, cx| service.pause(cx));
        }
    });
    cx.on_action(move |_: &Resume, cx| {
        service.update(cx, |service, cx| service.resume(cx));
    });
}

fn open_sync_page(cx: &mut App) {
    workspace::with_active_or_new_workspace(cx, |_, window, cx| {
        open_sync_page_in(window, cx);
    });
}
