//! The resident sync service: owns the configuration, runs the engine against
//! the persistent clone, reacts to config file changes, timers and extension
//! installs, and exposes state for the settings page and status bar.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use askpass::AskPassDelegate;
use chrono::{DateTime, Local};
use extension_host::ExtensionStore;
use fs::{CopyOptions, Fs, RemoveOptions};
use futures::StreamExt as _;
use git_ui_core::askpass_modal::AskPassModal;
use gpui::{
    Action as _, App, AsyncApp, Context, Entity, EventEmitter, Global, SharedString, Subscription,
    Task, WeakEntity,
};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    MultiWorkspace,
    notifications::{
        NotificationId, show_app_notification, simple_message_notification::MessageNotification,
    },
};
use zed_actions::OpenSettingsAt;

use crate::{
    SETTINGS_SYNC_PATH,
    config::{
        ConflictStrategy, FileGroup, SyncConfig, SyncPaths, SyncStateFile, load_config, load_state,
        save_config, save_state,
    },
    engine::{
        FileConflict, FilePlan, FileSides, FileSyncDecision, PlannedAction, Resolution,
        SHARED_KEYMAP_REPO_FILE, Side, SyncContext, TrackedFile, all_tracked_files,
        local_path_for_repo_file, plan_file, reconcile_auto_install_extensions, resolve_conflict,
    },
    git_backend::{FetchOutcome, GitBackend, remote_branch_exists},
    history::HistoryEntry,
    merge::{FileTransform, PathSetting, find_absolute_paths, list_key_paths, parse_jsonc},
};

/// How long after the last config file change an automatic sync starts.
const FILE_CHANGE_DEBOUNCE: Duration = Duration::from_secs(3);
/// Every toggle in the settings window writes settings.json, so changes made
/// while one is open are batched over a longer quiet period.
const SETTINGS_WINDOW_DEBOUNCE: Duration = Duration::from_secs(10);
/// Events for a file within this window after the service wrote it are the
/// service's own write echoing back through the watcher.
const WRITE_ECHO_WINDOW: Duration = Duration::from_secs(5);
/// Give the settings store time to reload a pulled settings.json before asking
/// the extension store to act on it.
const EXTENSION_INSTALL_DELAY: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionReason {
    Conflicts(usize),
    Authentication,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    NotConfigured,
    Idle,
    Syncing(SharedString),
    NeedsAttention(AttentionReason),
    Paused,
    Error(SharedString),
}

impl SyncState {
    pub fn label(&self) -> SharedString {
        match self {
            SyncState::NotConfigured => "Not set up".into(),
            SyncState::Idle => "In sync".into(),
            SyncState::Syncing(step) => step.clone(),
            SyncState::NeedsAttention(AttentionReason::Conflicts(count)) => format!(
                "{count} conflict{} to resolve",
                if *count == 1 { "" } else { "s" }
            )
            .into(),
            SyncState::NeedsAttention(AttentionReason::Authentication) => {
                "Authentication needed".into()
            }
            SyncState::NeedsAttention(AttentionReason::Offline) => "Repository unreachable".into(),
            SyncState::Paused => "Paused".into(),
            SyncState::Error(_) => "Sync failed".into(),
        }
    }

    pub fn is_syncing(&self) -> bool {
        matches!(self, SyncState::Syncing(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub file: TrackedFile,
    /// What the last run found before acting on it.
    pub decision: FileSyncDecision,
    /// Whether the last run settled this file (false for conflicts).
    pub settled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingConflict {
    pub file: TrackedFile,
    pub conflict: FileConflict,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub pulled: Vec<String>,
    pub pushed: Vec<String>,
    pub merged: Vec<String>,
    pub conflicts: usize,
    pub source_host: Option<String>,
}

impl SyncSummary {
    pub fn changed_anything(&self) -> bool {
        !self.pulled.is_empty() || !self.pushed.is_empty() || !self.merged.is_empty()
    }
}

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Changed,
    Completed(SyncSummary),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreSource {
    Commit(String),
    Backup,
}

pub struct RestorePreview {
    pub current: Option<String>,
    pub restored: String,
    pub local_path: PathBuf,
}

struct GlobalSettingsSyncService(Entity<SettingsSyncService>);

impl Global for GlobalSettingsSyncService {}

pub struct SettingsSyncService {
    fs: Arc<dyn Fs>,
    paths: SyncPaths,
    config: Option<SyncConfig>,
    state: SyncState,
    file_statuses: Vec<FileStatus>,
    conflicts: Vec<PendingConflict>,
    missing_paths: Vec<PathSetting>,
    last_sync: Option<DateTime<Local>>,
    last_error: Option<String>,
    last_summary: Option<SyncSummary>,
    paused: bool,
    settings_window_open: bool,
    running: bool,
    /// Extensions installed here as of the last run; see `SyncStateFile`.
    known_installed_extensions: BTreeSet<String>,
    rerun_requested: Option<bool>,
    recently_written: HashMap<PathBuf, Instant>,
    _watch_task: Option<Task<()>>,
    _timer_task: Option<Task<()>>,
    debounce_task: Option<Task<()>>,
    _extension_subscription: Option<Subscription>,
}

impl EventEmitter<SyncEvent> for SettingsSyncService {}

impl SettingsSyncService {
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSettingsSyncService>()
            .map(|global| global.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalSettingsSyncService>().0.clone()
    }

    pub fn set_global(service: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalSettingsSyncService(service));
    }

    pub fn new(fs: Arc<dyn Fs>, paths: SyncPaths, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            fs,
            paths,
            config: None,
            state: SyncState::NotConfigured,
            file_statuses: Vec::new(),
            conflicts: Vec::new(),
            missing_paths: Vec::new(),
            last_sync: None,
            last_error: None,
            last_summary: None,
            paused: false,
            settings_window_open: false,
            running: false,
            known_installed_extensions: BTreeSet::new(),
            rerun_requested: None,
            recently_written: HashMap::default(),
            _watch_task: None,
            _timer_task: None,
            debounce_task: None,
            _extension_subscription: None,
        };
        this.load(cx);
        this
    }

    // Accessors used by the settings page and status bar.

    pub fn paths(&self) -> &SyncPaths {
        &self.paths
    }

    pub fn config(&self) -> Option<&SyncConfig> {
        self.config.as_ref()
    }

    pub fn is_configured(&self) -> bool {
        self.config.is_some()
    }

    pub fn state(&self) -> &SyncState {
        &self.state
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn file_statuses(&self) -> &[FileStatus] {
        &self.file_statuses
    }

    pub fn conflicts(&self) -> &[PendingConflict] {
        &self.conflicts
    }

    pub fn missing_paths(&self) -> &[PathSetting] {
        &self.missing_paths
    }

    pub fn last_sync(&self) -> Option<DateTime<Local>> {
        self.last_sync
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn last_summary(&self) -> Option<&SyncSummary> {
        self.last_summary.as_ref()
    }

    fn set_state(&mut self, state: SyncState, cx: &mut Context<Self>) {
        if self.state != state {
            self.state = state;
            cx.emit(SyncEvent::Changed);
            cx.notify();
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let paths = self.paths.clone();
        cx.spawn(async move |this, cx| {
            let config = load_config(&fs, &paths).await;
            let state = load_state(&fs, &paths).await;
            this.update(cx, |this, cx| {
                match config {
                    Ok(config) => this.config = config,
                    Err(error) => {
                        log::error!("settings sync: {error:#}");
                        this.last_error = Some(format!("{error:#}"));
                    }
                }
                this.last_sync = state
                    .last_sync
                    .as_deref()
                    .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                    .map(|time| time.with_timezone(&Local));
                this.last_error = this.last_error.take().or(state.last_error);
                this.known_installed_extensions = state.installed_extensions.into_iter().collect();
                if this.config.is_some() {
                    this.state = SyncState::Idle;
                    this.start_background_work(cx);
                    if this.should_auto_sync() {
                        this.sync_now(false, cx);
                    }
                }
                cx.emit(SyncEvent::Changed);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn should_auto_sync(&self) -> bool {
        self.config.as_ref().is_some_and(|config| config.auto_sync) && !self.paused
    }

    fn start_background_work(&mut self, cx: &mut Context<Self>) {
        self.start_watcher(cx);
        self.start_timer(cx);
        self.subscribe_to_extensions(cx);
    }

    fn stop_background_work(&mut self) {
        self._watch_task = None;
        self._timer_task = None;
        self.debounce_task = None;
        self._extension_subscription = None;
    }

    fn start_watcher(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let config_dir = paths::config_dir().clone();
        self._watch_task = Some(cx.spawn(async move |this, cx| {
            let (mut events, _watcher) = fs.watch(&config_dir, Duration::from_millis(100)).await;
            while let Some(batch) = events.next().await {
                let changed: Vec<PathBuf> = batch
                    .into_iter()
                    .map(|event| event.path)
                    .filter(|path| is_tracked_path(path))
                    .collect();
                if changed.is_empty() {
                    continue;
                }
                if this
                    .update(cx, |this, cx| this.on_tracked_files_changed(changed, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn start_timer(&mut self, cx: &mut Context<Self>) {
        let minutes = self
            .config
            .as_ref()
            .map(|config| config.pull_interval_minutes.max(1))
            .unwrap_or(15);
        self._timer_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(minutes * 60))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    if this.should_auto_sync() {
                        log::info!("settings sync: interval timer elapsed, syncing");
                        this.sync_now(false, cx);
                    }
                });
                if keep_going.is_err() {
                    break;
                }
            }
        }));
    }

    fn subscribe_to_extensions(&mut self, cx: &mut Context<Self>) {
        let Some(store) = ExtensionStore::try_global(cx) else {
            return;
        };
        self._extension_subscription =
            Some(cx.subscribe(&store, |this, _, event, cx| match event {
                extension_host::Event::ExtensionInstalled(id) => {
                    this.schedule_auto_sync(&format!("extension {id} installed"), cx);
                }
                extension_host::Event::ExtensionUninstalled(id) => {
                    this.schedule_auto_sync(&format!("extension {id} uninstalled"), cx);
                }
                _ => {}
            }));
    }

    fn on_tracked_files_changed(&mut self, changed: Vec<PathBuf>, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.recently_written
            .retain(|_, written| now.duration_since(*written) < WRITE_ECHO_WINDOW);
        let (external, echoed): (Vec<_>, Vec<_>) = changed
            .iter()
            .partition(|path| !self.recently_written.contains_key(*path));
        for path in &echoed {
            log::debug!("settings sync: ignoring own write of {}", path.display());
        }
        if let Some(path) = external.first() {
            self.schedule_auto_sync(&format!("{} changed", path.display()), cx);
        }
    }

    fn schedule_auto_sync(&mut self, reason: &str, cx: &mut Context<Self>) {
        if !self.should_auto_sync() {
            log::info!("settings sync: {reason}, automatic sync is off or paused");
            return;
        }
        let debounce = if self.settings_window_open {
            SETTINGS_WINDOW_DEBOUNCE
        } else {
            FILE_CHANGE_DEBOUNCE
        };
        log::info!(
            "settings sync: {reason}, syncing in {} seconds",
            debounce.as_secs()
        );
        self.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(debounce).await;
            this.update(cx, |this, cx| this.sync_now(false, cx)).ok();
        }));
    }

    /// While a settings window is open, automatic syncs wait a little longer
    /// after the last change so a series of toggles lands as one run.
    pub fn set_settings_window_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.settings_window_open = open;
        if !open && self.debounce_task.is_some() {
            self.schedule_auto_sync("settings window closed", cx);
        }
    }

    pub fn pause(&mut self, cx: &mut Context<Self>) {
        self.paused = true;
        self.debounce_task = None;
        if self.config.is_some() {
            self.set_state(SyncState::Paused, cx);
        }
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        if self.config.is_some() {
            self.set_state(SyncState::Idle, cx);
            self.sync_now(false, cx);
        }
    }

    pub fn sync_now(&mut self, interactive: bool, cx: &mut Context<Self>) {
        if self.config.is_none() {
            return;
        }
        if self.running {
            self.rerun_requested = Some(self.rerun_requested.unwrap_or(false) || interactive);
            return;
        }
        self.running = true;
        self.debounce_task = None;
        log::info!("settings sync: run started (interactive: {interactive})");
        cx.spawn(async move |this, cx| {
            let result = Self::run(this.clone(), interactive, cx).await;
            this.update(cx, |this, cx| this.finish_run(result, interactive, cx))
                .ok();
        })
        .detach();
    }

    fn finish_run(
        &mut self,
        result: Result<SyncSummary>,
        interactive: bool,
        cx: &mut Context<Self>,
    ) {
        self.running = false;
        match result {
            Ok(summary) => {
                self.last_error = None;
                self.last_sync = Some(Local::now());
                let state = if summary.conflicts > 0 {
                    SyncState::NeedsAttention(AttentionReason::Conflicts(summary.conflicts))
                } else {
                    SyncState::Idle
                };
                log::info!(
                    "settings sync: run finished (pushed: {:?}, pulled: {:?}, merged: {:?}, conflicts: {})",
                    summary.pushed,
                    summary.pulled,
                    summary.merged,
                    summary.conflicts
                );
                self.notify_completed(&summary, interactive, cx);
                self.last_summary = Some(summary.clone());
                self.set_state(state, cx);
                cx.emit(SyncEvent::Completed(summary));
                let fs = self.fs.clone();
                let paths = self.paths.clone();
                let state_file = SyncStateFile {
                    last_sync: self.last_sync.map(|time| time.to_rfc3339()),
                    last_error: None,
                    installed_extensions: self.known_installed_extensions.iter().cloned().collect(),
                };
                cx.background_spawn(async move {
                    save_state(&fs, &paths, &state_file).await.log_err();
                })
                .detach();
            }
            Err(error) => {
                let message = format!("{error:#}");
                log::error!("settings sync failed: {message}");
                self.last_error = Some(message.clone());
                let state = classify_error(&message);
                if let SyncState::NeedsAttention(AttentionReason::Authentication) = &state {
                    self.notify_authentication(cx);
                }
                self.set_state(state, cx);
                let fs = self.fs.clone();
                let paths = self.paths.clone();
                let state_file = SyncStateFile {
                    last_sync: self.last_sync.map(|time| time.to_rfc3339()),
                    last_error: Some(message),
                    installed_extensions: self.known_installed_extensions.iter().cloned().collect(),
                };
                cx.background_spawn(async move {
                    save_state(&fs, &paths, &state_file).await.log_err();
                })
                .detach();
            }
        }
        cx.notify();
        if let Some(interactive) = self.rerun_requested.take() {
            self.sync_now(interactive, cx);
        }
    }

    /// Successful syncs stay quiet; the status bar carries the last sync time.
    /// Only conflicts from an unattended run ask for attention.
    fn notify_completed(&self, summary: &SyncSummary, interactive: bool, cx: &mut App) {
        if summary.conflicts > 0 && !interactive {
            let count = summary.conflicts;
            let message = format!(
                "Settings sync needs your attention: {count} conflict{} between local and remote settings",
                if count == 1 { "" } else { "s" }
            );
            show_app_notification(
                NotificationId::unique::<ConflictNotification>(),
                cx,
                move |cx| {
                    cx.new(|cx| {
                        MessageNotification::new(message.clone(), cx)
                            .primary_message("Resolve")
                            .primary_on_click(|window, cx| {
                                open_sync_page_in(window, cx);
                            })
                    })
                },
            );
        }
    }

    fn notify_authentication(&self, cx: &mut App) {
        show_app_notification(
            NotificationId::unique::<AuthenticationNotification>(),
            cx,
            |cx| {
                cx.new(|cx| {
                    MessageNotification::new(
                        "Settings sync could not authenticate with your repository. Run a sync from the Settings Sync page to enter credentials.",
                        cx,
                    )
                    .primary_message("Open Settings Sync")
                    .primary_on_click(|window, cx| {
                        open_sync_page_in(window, cx);
                    })
                })
            },
        );
    }

    fn backend(fs: Arc<dyn Fs>, paths: &SyncPaths, config: &SyncConfig) -> GitBackend {
        let git = config.backend.git();
        GitBackend::new(fs, paths.repo_dir(), git.url.clone(), git.branch.clone())
    }

    fn read_run_context(
        this: &WeakEntity<Self>,
        cx: &mut AsyncApp,
    ) -> Result<(Arc<dyn Fs>, SyncPaths, SyncConfig)> {
        this.read_with(cx, |this, _| {
            let config = this.config.clone().context("settings sync is not set up")?;
            anyhow::Ok((this.fs.clone(), this.paths.clone(), config))
        })?
    }

    async fn run(
        this: WeakEntity<Self>,
        interactive: bool,
        cx: &mut AsyncApp,
    ) -> Result<SyncSummary> {
        let (fs, paths, config) = Self::read_run_context(&this, cx)?;
        let backend = Self::backend(fs.clone(), &paths, &config);
        let git = config.backend.git();

        this.update(cx, |this, cx| {
            this.set_state(SyncState::Syncing("Fetching…".into()), cx)
        })?;

        if !fs.is_dir(&paths.repo_dir().join(".git")).await {
            let exists =
                remote_branch_exists(&git.url, &git.branch, cx.background_executor()).await?;
            backend.ensure_clone(exists).await?;
        }
        let askpass = askpass_delegate(interactive, "Settings sync: fetch", cx);
        let remote_exists = backend.fetch(askpass, cx).await? == FetchOutcome::Updated;

        let (installed, previously_installed) = this.read_with(cx, |this, cx| {
            (
                installed_extension_ids(cx),
                this.known_installed_extensions.clone(),
            )
        })?;
        let settings_path = paths::settings_file().clone();
        if config.sync_extensions
            && let Some(text) = load_if_exists(&fs, &settings_path).await
            && let Some(reconciled) =
                reconcile_auto_install_extensions(&text, &installed, &previously_installed)
        {
            log::info!("settings sync: updated auto_install_extensions from installed extensions");
            write_local(&this, &fs, &settings_path, reconciled, cx).await?;
        }
        this.update(cx, |this, _| {
            this.known_installed_extensions = installed;
        })?;

        this.update(cx, |this, cx| {
            this.set_state(SyncState::Syncing("Comparing…".into()), cx)
        })?;

        let plans = Self::plan(&fs, &paths, &config, &backend, remote_exists).await?;
        for plan in &plans {
            log::info!(
                "settings sync: {} -> {:?}",
                plan.file.repo_path,
                plan.decision
            );
        }
        let statuses: Vec<FileStatus> = plans
            .iter()
            .map(|plan| FileStatus {
                file: plan.file.clone(),
                decision: plan.decision,
                settled: !matches!(plan.action, PlannedAction::Conflict(_)),
            })
            .collect();

        let outcome = Self::apply_plans(&this, &fs, &paths, &backend, plans, cx).await?;
        let mut summary = Self::finish_apply(
            &this,
            &fs,
            &paths,
            &config,
            &backend,
            outcome,
            interactive,
            cx,
        )
        .await?;

        if !summary.pulled.is_empty() || !summary.merged.is_empty() {
            summary.source_host = backend
                .history(None, 1)
                .await
                .ok()
                .and_then(|entries| entries.into_iter().next())
                .and_then(|entry| entry.host);
        }

        this.update(cx, |this, _| {
            this.file_statuses = statuses;
        })?;
        Ok(summary)
    }

    async fn plan(
        fs: &Arc<dyn Fs>,
        paths: &SyncPaths,
        config: &SyncConfig,
        backend: &GitBackend,
        remote_exists: bool,
    ) -> Result<Vec<FilePlan>> {
        let repo_dir = backend.repo_dir().clone();
        let base_dir = paths.base_dir();

        let snippet_names = union_json_names(
            fs,
            &[
                paths::snippets_dir().clone(),
                repo_dir.join("snippets"),
                base_dir.join("snippets"),
            ],
        )
        .await;
        let theme_names = union_json_names(
            fs,
            &[
                paths::themes_dir().clone(),
                repo_dir.join("themes"),
                base_dir.join("themes"),
            ],
        )
        .await;
        let all_files = all_tracked_files(config, &snippet_names, &theme_names);

        let mut local_looks_fresh = true;
        let mut remote_looks_fresh = true;
        for file in &all_files {
            if fs.is_file(&file.local_path).await {
                local_looks_fresh = false;
            }
            if remote_exists && fs.is_file(&backend.repo_file_path(&file.repo_path)).await {
                remote_looks_fresh = false;
            }
        }
        let context = SyncContext {
            local_looks_fresh,
            remote_looks_fresh,
        };

        // A base recorded against another repository or branch, or a remote
        // that has no branch yet, describes nothing here; start it over.
        let recorded_source = load_if_exists(fs, &paths.base_source_file()).await;
        let base_usable = remote_exists
            && recorded_source.as_deref() == Some(config.backend.source_id().as_str());
        if !base_usable {
            fs.remove_dir(
                &base_dir,
                RemoveOptions {
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            )
            .await?;
        }

        let auto_resolve = match config.conflict_strategy {
            ConflictStrategy::Ask => None,
            ConflictStrategy::KeepLocal => Some(Side::Local),
            ConflictStrategy::UseRemote => Some(Side::Remote),
        };
        let settings_transform = FileTransform::for_settings(&config.effective_per_machine_keys());

        let mut plans = Vec::new();
        for file in all_files {
            if !config.syncs(file.group) {
                continue;
            }
            let local = load_if_exists(fs, &file.local_path).await;
            let mut remote = if remote_exists {
                load_if_exists(fs, &backend.repo_file_path(&file.repo_path)).await
            } else {
                None
            };
            // A repository from before keymaps were split per platform only
            // has the shared file; use it to seed this platform's keymap.
            if remote.is_none()
                && remote_exists
                && file.group == FileGroup::Keymap
                && file.repo_path != SHARED_KEYMAP_REPO_FILE
            {
                remote = load_if_exists(fs, &backend.repo_file_path(SHARED_KEYMAP_REPO_FILE)).await;
                if remote.is_some() {
                    log::info!(
                        "settings sync: seeding {} from the shared {SHARED_KEYMAP_REPO_FILE}",
                        file.repo_path
                    );
                }
            }
            let base = if base_usable {
                load_if_exists(fs, &base_dir.join(&file.repo_path)).await
            } else {
                None
            };
            let transform = if file.is_settings() {
                &settings_transform
            } else {
                &FileTransform::Identity
            };
            let sides = FileSides {
                local: local.as_deref(),
                remote: remote.as_deref(),
                base: base.as_deref(),
            };
            plans.push(plan_file(file, sides, transform, context, auto_resolve));
        }
        Ok(plans)
    }

    async fn apply_plans(
        this: &WeakEntity<Self>,
        fs: &Arc<dyn Fs>,
        paths: &SyncPaths,
        backend: &GitBackend,
        plans: Vec<FilePlan>,
        cx: &mut AsyncApp,
    ) -> Result<ApplyOutcome> {
        let mut outcome = ApplyOutcome::default();

        for plan in plans {
            let FilePlan { file, action, .. } = plan;
            let repo_file = backend.repo_file_path(&file.repo_path);
            match action {
                PlannedAction::Nothing => outcome.settled.push(file),
                PlannedAction::WriteRemote(content) => {
                    write_file(fs, &repo_file, content).await?;
                    outcome.repo_paths.push(file.repo_path.clone());
                    outcome.pushed.push(file.label.clone());
                    outcome.settled.push(file);
                }
                PlannedAction::WriteLocal { content, backup } => {
                    if backup {
                        backup_local(fs, paths, &file).await?;
                    }
                    record_settings_write(fs, &file, &content, &mut outcome).await;
                    write_local(this, fs, &file.local_path, content, cx).await?;
                    outcome.pulled.push(file.label.clone());
                    outcome.settled.push(file);
                }
                PlannedAction::DeleteRemote => {
                    remove_file(fs, &repo_file).await?;
                    outcome.repo_paths.push(file.repo_path.clone());
                    outcome.pushed.push(file.label.clone());
                    outcome.settled.push(file);
                }
                PlannedAction::DeleteLocal => {
                    backup_local(fs, paths, &file).await?;
                    remove_file(fs, &file.local_path).await?;
                    outcome.pulled.push(file.label.clone());
                    outcome.settled.push(file);
                }
                PlannedAction::Merged { local, remote, .. } => {
                    let current_local = load_if_exists(fs, &file.local_path).await;
                    if current_local.as_deref() != Some(local.as_str()) {
                        if current_local.is_some() {
                            backup_local(fs, paths, &file).await?;
                        }
                        record_settings_write(fs, &file, &local, &mut outcome).await;
                        write_local(this, fs, &file.local_path, local, cx).await?;
                    }
                    let current_remote = load_if_exists(fs, &repo_file).await;
                    if current_remote.as_deref() != Some(remote.as_str()) {
                        write_file(fs, &repo_file, remote).await?;
                        outcome.repo_paths.push(file.repo_path.clone());
                    }
                    outcome.merged.push(file.label.clone());
                    outcome.settled.push(file);
                }
                PlannedAction::Conflict(conflict) => {
                    // Keep the repository version on disk so a diff editor can
                    // show it next to the local file.
                    let conflict_copy = paths.conflicts_dir().join(&file.repo_path);
                    match &conflict.remote {
                        Some(remote) => write_file(fs, &conflict_copy, remote.clone()).await?,
                        None => remove_file(fs, &conflict_copy).await?,
                    }
                    outcome.conflicts.push(PendingConflict { file, conflict });
                }
            }
        }
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_apply(
        this: &WeakEntity<Self>,
        fs: &Arc<dyn Fs>,
        paths: &SyncPaths,
        config: &SyncConfig,
        backend: &GitBackend,
        outcome: ApplyOutcome,
        interactive: bool,
        cx: &mut AsyncApp,
    ) -> Result<SyncSummary> {
        if !outcome.repo_paths.is_empty() {
            this.update(cx, |this, cx| {
                this.set_state(SyncState::Syncing("Pushing…".into()), cx)
            })?;
            let message = format!(
                "sync: {} from {}",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                config.machine_name
            );
            log::info!("settings sync: pushing {:?}", outcome.repo_paths);
            let askpass = askpass_delegate(interactive, "Settings sync: push", cx);
            backend
                .publish(&outcome.repo_paths, &message, askpass, cx)
                .await?;
        }

        // Record what both sides now agree on, for the next run's deletion
        // detection and settings merge.
        let base_dir = paths.base_dir();
        fs.create_dir(&base_dir).await?;
        fs.atomic_write(paths.base_source_file(), config.backend.source_id())
            .await?;
        for file in &outcome.settled {
            let repo_file = backend.repo_file_path(&file.repo_path);
            let base_file = base_dir.join(&file.repo_path);
            match load_if_exists(fs, &repo_file).await {
                Some(content) => write_file(fs, &base_file, content).await?,
                None => remove_file(fs, &base_file).await?,
            }
        }

        let missing_paths = match &outcome.settings_written {
            Some((before, after)) => {
                find_missing_paths(
                    fs,
                    before.as_deref(),
                    after,
                    &config.effective_per_machine_keys(),
                )
                .await
            }
            None => Vec::new(),
        };
        let settings_pulled = outcome.settings_written.is_some();

        this.update(cx, |this, cx| {
            this.conflicts = outcome.conflicts.clone();
            this.missing_paths = missing_paths;
            if settings_pulled {
                this.install_pulled_extensions(cx);
            }
        })?;

        Ok(SyncSummary {
            pulled: outcome.pulled,
            pushed: outcome.pushed,
            merged: outcome.merged,
            conflicts: outcome.conflicts.len(),
            source_host: None,
        })
    }

    /// A pulled settings.json may list extensions this machine lacks. The
    /// extension store only auto-installs at startup, so prompt it here once
    /// the settings store has had a chance to reload the file.
    fn install_pulled_extensions(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(EXTENSION_INSTALL_DELAY)
                .await;
            cx.update(|cx| {
                if let Some(store) = ExtensionStore::try_global(cx) {
                    store.update(cx, |store, cx| store.auto_install_extensions(cx));
                }
            });
        })
        .detach();
    }

    /// Link a repository and run the first sync. Resolves once the clone exists
    /// and the first sync has been started; progress continues through state.
    pub fn connect(&mut self, config: SyncConfig, cx: &mut Context<Self>) -> Task<Result<()>> {
        let fs = self.fs.clone();
        let paths = self.paths.clone();
        self.set_state(SyncState::Syncing("Checking repository…".into()), cx);
        cx.spawn(async move |this, cx| {
            let git = config.backend.git().clone();
            let result = async {
                let exists =
                    remote_branch_exists(&git.url, &git.branch, cx.background_executor()).await?;
                this.update(cx, |this, cx| {
                    this.set_state(SyncState::Syncing("Cloning…".into()), cx)
                })?;
                let backend = Self::backend(fs.clone(), &paths, &config);
                // A clone left behind by an earlier, different repository
                // would be fetched against the wrong remote.
                fs.remove_dir(
                    &paths.repo_dir(),
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await?;
                fs.remove_dir(
                    &paths.base_dir(),
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await?;
                backend.ensure_clone(exists).await?;
                save_config(&fs, &paths, &config).await?;
                anyhow::Ok(())
            }
            .await;

            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.config = Some(config);
                    this.paused = false;
                    this.last_error = None;
                    this.conflicts.clear();
                    this.file_statuses.clear();
                    this.set_state(SyncState::Idle, cx);
                    this.start_background_work(cx);
                    this.sync_now(true, cx);
                    Ok(())
                }
                Err(error) => {
                    this.set_state(SyncState::NotConfigured, cx);
                    Err(error)
                }
            })?
        })
    }

    /// Forget the repository and every local sync artifact. The repository and
    /// the config directory are left untouched.
    pub fn disconnect(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.stop_background_work();
        self.config = None;
        self.conflicts.clear();
        self.file_statuses.clear();
        self.missing_paths.clear();
        self.last_summary = None;
        self.last_sync = None;
        self.last_error = None;
        self.paused = false;
        self.set_state(SyncState::NotConfigured, cx);
        let fs = self.fs.clone();
        let root = self.paths.root().clone();
        cx.background_spawn(async move {
            fs.remove_dir(
                &root,
                RemoveOptions {
                    recursive: true,
                    ignore_if_not_exists: true,
                },
            )
            .await
        })
    }

    pub fn update_config(
        &mut self,
        update: impl FnOnce(&mut SyncConfig),
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(mut config) = self.config.clone() else {
            return Task::ready(Err(anyhow!("settings sync is not set up")));
        };
        let previous_interval = config.pull_interval_minutes;
        update(&mut config);
        self.config = Some(config.clone());
        if config.pull_interval_minutes != previous_interval {
            self.start_timer(cx);
        }
        cx.emit(SyncEvent::Changed);
        cx.notify();
        let fs = self.fs.clone();
        let paths = self.paths.clone();
        cx.background_spawn(async move { save_config(&fs, &paths, &config).await })
    }

    /// Settle pending conflicts. Files without a resolution stay pending.
    pub fn resolve_conflicts(
        &mut self,
        resolutions: Vec<(String, Resolution)>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.running {
            return Task::ready(Err(anyhow!("a sync is already running")));
        }
        self.running = true;
        self.set_state(SyncState::Syncing("Resolving…".into()), cx);
        cx.spawn(async move |this, cx| {
            let result = Self::run_resolution(this.clone(), resolutions, cx).await;
            let outcome = result
                .as_ref()
                .map(|_| ())
                .map_err(|error| anyhow!("{error:#}"));
            this.update(cx, |this, cx| this.finish_run(result, true, cx))?;
            outcome
        })
    }

    async fn run_resolution(
        this: WeakEntity<Self>,
        resolutions: Vec<(String, Resolution)>,
        cx: &mut AsyncApp,
    ) -> Result<SyncSummary> {
        let (fs, paths, config) = Self::read_run_context(&this, cx)?;
        let backend = Self::backend(fs.clone(), &paths, &config);
        let pending = this.read_with(cx, |this, _| this.conflicts.clone())?;
        let settings_transform = FileTransform::for_settings(&config.effective_per_machine_keys());

        let mut plans = Vec::new();
        let mut still_pending = Vec::new();
        for pending_conflict in pending {
            let resolution = resolutions
                .iter()
                .find(|(repo_path, _)| repo_path == &pending_conflict.file.repo_path)
                .map(|(_, resolution)| resolution.clone());
            match resolution {
                Some(resolution) => {
                    let transform = if pending_conflict.file.is_settings() {
                        &settings_transform
                    } else {
                        &FileTransform::Identity
                    };
                    let action =
                        resolve_conflict(&pending_conflict.conflict, &resolution, transform);
                    plans.push(FilePlan {
                        file: pending_conflict.file,
                        decision: FileSyncDecision::Conflict,
                        action,
                    });
                }
                None => still_pending.push(pending_conflict),
            }
        }

        let mut outcome = Self::apply_plans(&this, &fs, &paths, &backend, plans, cx).await?;
        outcome.conflicts.extend(still_pending);
        let settled: Vec<String> = outcome
            .settled
            .iter()
            .map(|file| file.repo_path.clone())
            .collect();
        let summary =
            Self::finish_apply(&this, &fs, &paths, &config, &backend, outcome, true, cx).await?;
        this.update(cx, |this, _| {
            for status in &mut this.file_statuses {
                if settled.contains(&status.file.repo_path) {
                    status.settled = true;
                    status.decision = FileSyncDecision::InSync;
                }
            }
        })?;
        Ok(summary)
    }

    pub fn history(&self, limit: usize, cx: &mut Context<Self>) -> Task<Result<Vec<HistoryEntry>>> {
        let Some(config) = self.config.clone() else {
            return Task::ready(Err(anyhow!("settings sync is not set up")));
        };
        let backend = Self::backend(self.fs.clone(), &self.paths, &config);
        cx.background_spawn(async move { backend.history(None, limit).await })
    }

    pub fn has_backup(&self, repo_path: &str, cx: &mut Context<Self>) -> Task<bool> {
        let fs = self.fs.clone();
        let backup = self.paths.backups_dir().join(repo_path);
        cx.background_spawn(async move { fs.is_file(&backup).await })
    }

    pub fn restore_preview(
        &self,
        repo_path: String,
        source: RestoreSource,
        cx: &mut Context<Self>,
    ) -> Task<Result<RestorePreview>> {
        let Some(config) = self.config.clone() else {
            return Task::ready(Err(anyhow!("settings sync is not set up")));
        };
        let fs = self.fs.clone();
        let paths = self.paths.clone();
        let backend = Self::backend(fs.clone(), &paths, &config);
        cx.background_spawn(async move {
            let local_path = local_path_for_repo_file(&repo_path)
                .with_context(|| format!("{repo_path} is not a file settings sync manages"))?;
            let restored = match &source {
                RestoreSource::Commit(hash) => backend
                    .file_at(hash, &repo_path)
                    .await?
                    .with_context(|| format!("{repo_path} does not exist in {hash}"))?,
                RestoreSource::Backup => fs.load(&paths.backups_dir().join(&repo_path)).await?,
            };
            let current = load_if_exists(&fs, &local_path).await;
            // Keys kept per machine stay as they are on this machine.
            let restored = if repo_path == "settings.json" {
                FileTransform::for_settings(&config.effective_per_machine_keys())
                    .to_local(&restored, current.as_deref())
            } else {
                restored
            };
            Ok(RestorePreview {
                current,
                restored,
                local_path,
            })
        })
    }

    /// Write an earlier version of a file to this machine, backing up the
    /// current one first. The next sync sends it to the repository.
    pub fn restore(
        &mut self,
        repo_path: String,
        source: RestoreSource,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let preview = self.restore_preview(repo_path.clone(), source, cx);
        let fs = self.fs.clone();
        let paths = self.paths.clone();
        cx.spawn(async move |this, cx| {
            let preview = preview.await?;
            if preview.current.as_deref() == Some(preview.restored.as_str()) {
                return Ok(());
            }
            if preview.current.is_some() {
                let backup = paths.backups_dir().join(&repo_path);
                copy_file(&fs, &preview.local_path, &backup).await?;
            }
            write_local(&this, &fs, &preview.local_path, preview.restored, cx).await?;
            this.update(cx, |this, cx| {
                if this.should_auto_sync() {
                    this.sync_now(true, cx);
                }
            })?;
            Ok(())
        })
    }

    /// Dotted key paths in the current settings.json, for the per-machine key picker.
    pub fn settings_key_paths(&self, cx: &mut Context<Self>) -> Task<Vec<String>> {
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            let Some(text) = load_if_exists(&fs, paths::settings_file()).await else {
                return Vec::new();
            };
            parse_jsonc(&text)
                .map(|value| list_key_paths(&value))
                .unwrap_or_default()
        })
    }

    pub fn dismiss_missing_paths(&mut self, cx: &mut Context<Self>) {
        self.missing_paths.clear();
        cx.notify();
    }
}

#[derive(Default)]
struct ApplyOutcome {
    repo_paths: Vec<String>,
    pulled: Vec<String>,
    pushed: Vec<String>,
    merged: Vec<String>,
    settled: Vec<TrackedFile>,
    conflicts: Vec<PendingConflict>,
    /// settings.json before and after this run wrote it locally.
    settings_written: Option<(Option<String>, String)>,
}

struct ConflictNotification;
struct AuthenticationNotification;

async fn record_settings_write(
    fs: &Arc<dyn Fs>,
    file: &TrackedFile,
    content: &str,
    outcome: &mut ApplyOutcome,
) {
    if !file.is_settings() {
        return;
    }
    let before = match &outcome.settings_written {
        Some((before, _)) => before.clone(),
        None => load_if_exists(fs, &file.local_path).await,
    };
    outcome.settings_written = Some((before, content.to_string()));
}

fn classify_error(message: &str) -> SyncState {
    let lower = message.to_lowercase();
    let authentication = [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied",
        "host key verification failed",
        "invalid credentials",
        "repository not found",
    ];
    let offline = [
        "could not resolve host",
        "unable to access",
        "connection refused",
        "connection timed out",
        "network is unreachable",
        "timed out",
        "could not reach",
        "no route to host",
    ];
    if authentication.iter().any(|needle| lower.contains(needle)) {
        SyncState::NeedsAttention(AttentionReason::Authentication)
    } else if offline.iter().any(|needle| lower.contains(needle)) {
        SyncState::NeedsAttention(AttentionReason::Offline)
    } else {
        SyncState::Error(message.to_string().into())
    }
}

fn is_tracked_path(path: &Path) -> bool {
    if path == paths::settings_file().as_path()
        || path == paths::keymap_file().as_path()
        || path == paths::tasks_file().as_path()
        || path == paths::debug_scenarios_file().as_path()
        || path == paths::agents_file().as_path()
    {
        return true;
    }
    let is_json = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
    is_json
        && path.parent().is_some_and(|parent| {
            parent == paths::snippets_dir().as_path() || parent == paths::themes_dir().as_path()
        })
}

fn installed_extension_ids(cx: &App) -> BTreeSet<String> {
    ExtensionStore::try_global(cx)
        .map(|store| {
            store
                .read(cx)
                .installed_extensions()
                .iter()
                .filter(|(_, entry)| !entry.dev)
                .map(|(id, _)| id.to_string())
                .collect()
        })
        .unwrap_or_default()
}

async fn load_if_exists(fs: &Arc<dyn Fs>, path: &Path) -> Option<String> {
    if !fs.is_file(path).await {
        return None;
    }
    fs.load(path).await.ok()
}

async fn list_json_names(fs: &Arc<dyn Fs>, dir: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if !fs.is_dir(dir).await {
        return names;
    }
    let Ok(mut entries) = fs.read_dir(dir).await else {
        return names;
    };
    while let Some(entry) = entries.next().await {
        let Ok(path) = entry else { continue };
        let is_json = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        if is_json
            && fs.is_file(&path).await
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            names.insert(name.to_string());
        }
    }
    names
}

async fn union_json_names(fs: &Arc<dyn Fs>, dirs: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in dirs {
        names.extend(list_json_names(fs, dir).await);
    }
    names
}

async fn write_file(fs: &Arc<dyn Fs>, path: &Path, content: String) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    fs.atomic_write(path.to_path_buf(), content).await
}

async fn remove_file(fs: &Arc<dyn Fs>, path: &Path) -> Result<()> {
    fs.remove_file(
        path,
        RemoveOptions {
            recursive: false,
            ignore_if_not_exists: true,
        },
    )
    .await
}

async fn copy_file(fs: &Arc<dyn Fs>, source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs.create_dir(parent).await?;
    }
    fs.copy_file(
        source,
        target,
        CopyOptions {
            overwrite: true,
            ignore_if_exists: false,
        },
    )
    .await
}

/// Write into the config directory, remembering the path so the watcher does
/// not mistake the write's own event for an external change.
async fn write_local(
    this: &WeakEntity<SettingsSyncService>,
    fs: &Arc<dyn Fs>,
    path: &Path,
    content: String,
    cx: &mut AsyncApp,
) -> Result<()> {
    let path = path.to_path_buf();
    this.update(cx, |this, _| {
        this.recently_written.insert(path.clone(), Instant::now());
    })?;
    write_file(fs, &path, content).await
}

async fn backup_local(fs: &Arc<dyn Fs>, paths: &SyncPaths, file: &TrackedFile) -> Result<()> {
    if !fs.is_file(&file.local_path).await {
        return Ok(());
    }
    let backup = paths.backups_dir().join(&file.repo_path);
    copy_file(fs, &file.local_path, &backup).await
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        util::paths::home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// After a sync brings settings.json in, point out absolute paths that came
/// with it but do not exist here. Paths the local file already had, and
/// per-machine keys, are left alone.
async fn find_missing_paths(
    fs: &Arc<dyn Fs>,
    before: Option<&str>,
    after: &str,
    per_machine_keys: &[String],
) -> Vec<PathSetting> {
    let known: BTreeSet<(String, String)> = before
        .and_then(parse_jsonc)
        .map(|value| {
            find_absolute_paths(&value)
                .into_iter()
                .map(|setting| (setting.key, setting.value))
                .collect()
        })
        .unwrap_or_default();
    let is_per_machine = |key: &str| {
        per_machine_keys
            .iter()
            .any(|ignored| key == ignored || key.starts_with(&format!("{ignored}.")))
    };

    let Some(after) = parse_jsonc(after) else {
        return Vec::new();
    };
    let mut missing = Vec::new();
    for setting in find_absolute_paths(&after) {
        if known.contains(&(setting.key.clone(), setting.value.clone()))
            || is_per_machine(&setting.key)
        {
            continue;
        }
        let path = expand_home(&setting.value);
        if !fs.is_file(&path).await && !fs.is_dir(&path).await {
            missing.push(setting);
        }
    }
    missing
}

/// Interactive runs prompt for git credentials through the askpass modal in a
/// workspace window. Unattended runs fail fast instead, and the resulting
/// authentication state tells the user to run a sync by hand.
fn askpass_delegate(
    interactive: bool,
    operation: &'static str,
    cx: &mut AsyncApp,
) -> AskPassDelegate {
    AskPassDelegate::new_with_cancellation(cx, move |prompt, tx, cancellation, cx| {
        if !interactive {
            drop(tx);
            return;
        }
        let window = cx.update(|cx| {
            cx.active_window()
                .and_then(|window| window.downcast::<MultiWorkspace>())
                .or_else(|| {
                    cx.windows()
                        .into_iter()
                        .find_map(|window| window.downcast::<MultiWorkspace>())
                })
        });
        let Some(window) = window else {
            drop(tx);
            return;
        };
        window
            .update(cx, |multi_workspace, window, cx| {
                window.activate_window();
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        AskPassModal::new(
                            operation.into(),
                            prompt.into(),
                            tx,
                            cancellation,
                            window,
                            cx,
                        )
                    });
                });
            })
            .ok();
    })
}

/// Open the Settings Sync page from a workspace window.
pub fn open_sync_page_in(window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        OpenSettingsAt {
            path: SETTINGS_SYNC_PATH.to_string(),
            target: None,
        }
        .boxed_clone(),
        cx,
    );
}

pub fn describe_time_ago(time: DateTime<Local>) -> String {
    let elapsed = Local::now().signed_duration_since(time);
    let seconds = elapsed.num_seconds().max(0);
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        let minutes = seconds / 60;
        format!(
            "{minutes} minute{} ago",
            if minutes == 1 { "" } else { "s" }
        )
    } else if seconds < 86_400 {
        let hours = seconds / 3600;
        format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" })
    } else {
        time.format("%b %-d, %Y at %H:%M").to_string()
    }
}
