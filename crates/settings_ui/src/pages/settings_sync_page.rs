//! The Settings Sync page: setup card when nothing is linked, otherwise status,
//! conflict resolution, tracked files, per-machine keys, history and restore.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local};
use git_ui_core::file_diff_view::FileDiffView;
use gpui::{ClickEvent, Entity, PromptLevel, ScrollHandle, Subscription, Task, prelude::*};
use serde_json::Value;
use settings_sync::{
    AttentionReason, ConflictKind, ConflictStrategy, DiffLineKind, FileGroup, FileSyncDecision,
    HistoryEntry, PendingConflict, Resolution, RestorePreview, RestoreSource, SettingsSyncService,
    Side, SyncConfig, SyncEvent, SyncState, describe_time_ago, diff_lines,
};
use ui::{
    Banner, ButtonLink, Divider, IconButton, Indicator, ListItem, Switch, ToggleState, Tooltip,
    prelude::*,
};

use crate::{
    SettingsWindow,
    components::{
        EnumVariantDropdown, NumberField, NumberFieldMode, SettingsInputField,
        SettingsSectionHeader,
    },
};

const HISTORY_LIMIT: usize = 20;
const DIFF_PREVIEW_LINES: usize = 80;
const MIN_PULL_INTERVAL_MINUTES: usize = 1;
const MAX_PULL_INTERVAL_MINUTES: usize = 24 * 60;

pub(crate) fn render_settings_sync_page(
    settings_window: &SettingsWindow,
    _scroll_handle: &ScrollHandle,
    _window: &mut Window,
    _cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    match settings_window.settings_sync_page() {
        Some(page) => page.into_any_element(),
        None => gpui::Empty.into_any_element(),
    }
}

/// The "Sync Now" button in the sub-page header, beside the breadcrumbs, the
/// way LLM Providers places "Add Provider".
pub(crate) fn render_settings_sync_header_action(
    settings_window: &SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) -> Option<AnyElement> {
    let page = settings_window.settings_sync_page()?;
    let service = page.read(cx).service.clone();
    let state = service.read(cx);
    if !state.is_configured() {
        return None;
    }
    let running = state.is_running();
    let paused = state.is_paused();
    Some(
        Button::new("settings-sync-now", "Sync Now")
            .tab_index(0_isize)
            .style(ButtonStyle::Outlined)
            .start_icon(
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .disabled(running || paused)
            .when(paused, |this| this.tooltip(Tooltip::text("Sync is paused")))
            .on_click(move |_, _, cx| {
                service.update(cx, |service, cx| service.sync_now(true, cx));
            })
            .into_any_element(),
    )
}

struct RestoreState {
    repo_path: String,
    source: RestoreSource,
    preview: Option<RestorePreview>,
    error: Option<String>,
    applying: bool,
}

pub struct SettingsSyncPage {
    service: Entity<SettingsSyncService>,
    scroll_handle: ScrollHandle,
    per_machine_key_error: Option<SharedString>,
    setup_url: String,
    setup_branch: String,
    setup_machine_name: String,
    setup_groups: HashSet<FileGroup>,
    setup_sync_extensions: bool,
    setup_error: Option<SharedString>,
    connecting: bool,
    action_error: Option<SharedString>,
    key_paths: Vec<String>,
    history: Option<Vec<HistoryEntry>>,
    history_loading: bool,
    history_error: Option<String>,
    expanded_history: HashSet<String>,
    backups: HashSet<String>,
    restore: Option<RestoreState>,
    key_choices: HashMap<(String, String), Side>,
    file_choices: HashMap<String, Side>,
    expanded_conflict_diffs: HashSet<String>,
    resolving: bool,
    _subscriptions: Vec<Subscription>,
}

impl SettingsSyncPage {
    pub fn new(
        service: Entity<SettingsSyncService>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let setup_machine_name = service
            .read(cx)
            .config()
            .map(|config| config.machine_name.clone())
            .unwrap_or_else(settings_sync::config::default_machine_name);

        let subscriptions = vec![
            cx.observe(&service, |_, _, cx| cx.notify()),
            cx.subscribe(&service, |this, _, event: &SyncEvent, cx| {
                if let SyncEvent::Completed(_) = event {
                    this.load_history(cx);
                    this.load_key_paths(cx);
                    this.load_backups(cx);
                }
                cx.notify();
            }),
        ];

        let mut this = Self {
            service,
            scroll_handle: ScrollHandle::new(),
            per_machine_key_error: None,
            setup_url: String::new(),
            setup_branch: "main".to_string(),
            setup_machine_name,
            setup_groups: FileGroup::ALL.into_iter().collect(),
            setup_sync_extensions: true,
            setup_error: None,
            connecting: false,
            action_error: None,
            key_paths: Vec::new(),
            history: None,
            history_loading: false,
            history_error: None,
            expanded_history: HashSet::default(),
            backups: HashSet::default(),
            restore: None,
            key_choices: HashMap::default(),
            file_choices: HashMap::default(),
            expanded_conflict_diffs: HashSet::default(),
            resolving: false,
            _subscriptions: subscriptions,
        };
        if this.service.read(cx).is_configured() {
            this.load_history(cx);
            this.load_key_paths(cx);
            this.load_backups(cx);
        }
        this
    }

    fn load_history(&mut self, cx: &mut Context<Self>) {
        if !self.service.read(cx).is_configured() {
            return;
        }
        self.history_loading = true;
        let task = self
            .service
            .update(cx, |service, cx| service.history(HISTORY_LIMIT, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.history_loading = false;
                match result {
                    Ok(entries) => {
                        this.history = Some(entries);
                        this.history_error = None;
                    }
                    Err(error) => this.history_error = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_key_paths(&mut self, cx: &mut Context<Self>) {
        let task = self
            .service
            .update(cx, |service, cx| service.settings_key_paths(cx));
        cx.spawn(async move |this, cx| {
            let key_paths = task.await;
            this.update(cx, |this, cx| {
                this.key_paths = key_paths;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_backups(&mut self, cx: &mut Context<Self>) {
        let repo_paths: Vec<String> = self
            .service
            .read(cx)
            .file_statuses()
            .iter()
            .map(|status| status.file.repo_path.clone())
            .collect();
        let tasks: Vec<(String, Task<bool>)> = repo_paths
            .into_iter()
            .map(|repo_path| {
                let task = self
                    .service
                    .update(cx, |service, cx| service.has_backup(&repo_path, cx));
                (repo_path, task)
            })
            .collect();
        cx.spawn(async move |this, cx| {
            let mut backups = HashSet::default();
            for (repo_path, task) in tasks {
                if task.await {
                    backups.insert(repo_path);
                }
            }
            this.update(cx, |this, cx| {
                this.backups = backups;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn update_config(&mut self, update: impl FnOnce(&mut SyncConfig), cx: &mut Context<Self>) {
        let task = self
            .service
            .update(cx, |service, cx| service.update_config(update, cx));
        cx.spawn(async move |this, cx| {
            if let Err(error) = task.await {
                this.update(cx, |this, cx| {
                    this.action_error = Some(format!("{error:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let url = self.setup_url.trim().to_string();
        let branch = self.setup_branch.trim().to_string();
        let branch = if branch.is_empty() {
            "main".to_string()
        } else {
            branch
        };
        if url.is_empty() {
            self.setup_error = Some("Enter the repository URL.".into());
            cx.notify();
            return;
        }
        let looks_like_git_url = url.starts_with("https://")
            || url.starts_with("http://")
            || url.starts_with("git@")
            || url.starts_with("ssh://")
            || url.starts_with("git://");
        if !looks_like_git_url {
            self.setup_error = Some("Enter an HTTPS or SSH git URL.".into());
            cx.notify();
            return;
        }

        let mut config = SyncConfig::new_git(url, branch);
        config.files = FileGroup::ALL
            .into_iter()
            .filter(|group| self.setup_groups.contains(group))
            .collect();
        config.sync_extensions = self.setup_sync_extensions;
        let machine_name = self.setup_machine_name.trim().to_string();
        if !machine_name.is_empty() {
            config.machine_name = machine_name;
        }

        self.connecting = true;
        self.setup_error = None;
        cx.notify();
        let task = self
            .service
            .update(cx, |service, cx| service.connect(config, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.connecting = false;
                match result {
                    Ok(()) => {
                        this.load_history(cx);
                        this.load_key_paths(cx);
                    }
                    Err(error) => this.setup_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.history = None;
        self.backups.clear();
        self.restore = None;
        self.key_choices.clear();
        self.file_choices.clear();
        let task = self
            .service
            .update(cx, |service, cx| service.disconnect(cx));
        task.detach_and_log_err(cx);
        cx.notify();
    }

    fn run_resolution(&mut self, resolutions: Vec<(String, Resolution)>, cx: &mut Context<Self>) {
        if resolutions.is_empty() {
            return;
        }
        self.resolving = true;
        self.action_error = None;
        cx.notify();
        let task = self
            .service
            .update(cx, |service, cx| service.resolve_conflicts(resolutions, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.resolving = false;
                this.key_choices.clear();
                this.file_choices.clear();
                this.expanded_conflict_diffs.clear();
                if let Err(error) = result {
                    this.action_error = Some(format!("{error:#}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_resolutions(&mut self, cx: &mut Context<Self>) {
        let conflicts = self.service.read(cx).conflicts().to_vec();
        let mut resolutions = Vec::new();
        for pending in conflicts {
            let repo_path = pending.file.repo_path.clone();
            if pending.conflict.is_key_level() {
                let choices = pending
                    .conflict
                    .key_conflicts
                    .iter()
                    .map(|conflict| {
                        let key = conflict.key();
                        let side = self
                            .key_choices
                            .get(&(repo_path.clone(), key.clone()))
                            .copied()
                            .unwrap_or(Side::Local);
                        (key, side)
                    })
                    .collect();
                resolutions.push((repo_path, Resolution::PerKey(choices)));
            } else if let Some(side) = self.file_choices.get(&repo_path) {
                resolutions.push((repo_path, Resolution::Side(*side)));
            }
        }
        self.run_resolution(resolutions, cx);
    }

    fn resolve_all(&mut self, side: Side, cx: &mut Context<Self>) {
        let resolutions = self
            .service
            .read(cx)
            .conflicts()
            .iter()
            .map(|pending| (pending.file.repo_path.clone(), Resolution::Side(side)))
            .collect();
        self.run_resolution(resolutions, cx);
    }

    fn open_conflict_in_editor(&self, pending: &PendingConflict, cx: &mut Context<Self>) {
        let remote_path = self
            .service
            .read(cx)
            .paths()
            .conflicts_dir()
            .join(&pending.file.repo_path);
        let local_path = pending.file.local_path.clone();
        workspace::with_active_or_new_workspace(cx, move |workspace, window, cx| {
            let workspace = workspace.weak_handle();
            FileDiffView::open(remote_path, local_path, None, workspace, window, cx)
                .detach_and_log_err(cx);
        });
    }

    fn start_restore(&mut self, repo_path: String, source: RestoreSource, cx: &mut Context<Self>) {
        self.restore = Some(RestoreState {
            repo_path: repo_path.clone(),
            source: source.clone(),
            preview: None,
            error: None,
            applying: false,
        });
        cx.notify();
        let task = self.service.update(cx, |service, cx| {
            service.restore_preview(repo_path.clone(), source.clone(), cx)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if let Some(restore) = this.restore.as_mut()
                    && restore.repo_path == repo_path
                    && restore.source == source
                {
                    match result {
                        Ok(preview) => restore.preview = Some(preview),
                        Err(error) => restore.error = Some(format!("{error:#}")),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_restore(&mut self, cx: &mut Context<Self>) {
        let Some(restore) = self.restore.as_mut() else {
            return;
        };
        restore.applying = true;
        let repo_path = restore.repo_path.clone();
        let source = restore.source.clone();
        cx.notify();
        let task = self
            .service
            .update(cx, |service, cx| service.restore(repo_path, source, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.restore = None;
                        this.load_backups(cx);
                    }
                    Err(error) => {
                        if let Some(restore) = this.restore.as_mut() {
                            restore.applying = false;
                            restore.error = Some(format!("{error:#}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // Rendering

    fn render_setup(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.service.read(cx).state().clone();
        let busy = self.connecting || state.is_syncing();

        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(Headline::new("Sync your settings").size(HeadlineSize::Small))
                    .child(
                        Label::new(
                            "Store your Zed configuration in a private git repository you control.",
                        )
                        .color(Color::Muted),
                    ),
            )
            .child(self.render_setup_field(
                "sync-setup-url",
                "Repository URL",
                "SSH or HTTPS URL of a private repository, empty or already in use by another machine.",
                "git@github.com:you/zed-config.git",
                self.setup_url.clone(),
                |this, value| this.setup_url = value,
                cx,
            ))
            .child(self.render_setup_field(
                "sync-setup-branch",
                "Branch",
                "Defaults to main.",
                "main",
                self.setup_branch.clone(),
                |this, value| this.setup_branch = value,
                cx,
            ))
            .child(self.render_setup_field(
                "sync-setup-machine-name",
                "This machine's name",
                "Shown in the sync history.",
                "This machine's name",
                self.setup_machine_name.clone(),
                |this, value| this.setup_machine_name = value,
                cx,
            ))
            .child(
                v_flex()
                    .gap_2()
                    .child(Label::new("What to sync"))
                    .child(h_flex().gap_4().flex_wrap().children(
                        FileGroup::ALL.into_iter().flat_map(|group| {
                            let checked = self.setup_groups.contains(&group);
                            let group_checkbox = Switch::new(
                                SharedString::from(format!("setup-group-{}", group.hint())),
                                ToggleState::from(checked),
                            )
                            .label(group.label())
                            .tab_index(0_isize)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.setup_groups.contains(&group) {
                                    this.setup_groups.remove(&group);
                                } else {
                                    this.setup_groups.insert(group);
                                }
                                cx.notify();
                            }));
                            // Extensions ride inside settings.json, so their
                            // checkbox follows Settings and depends on it.
                            let extensions_checkbox = (group == FileGroup::Settings).then(|| {
                                Switch::new(
                                    "setup-sync-extensions",
                                    ToggleState::from(checked && self.setup_sync_extensions),
                                )
                                .label("Extensions")
                                .tab_index(0_isize)
                                .disabled(!checked)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.setup_sync_extensions = !this.setup_sync_extensions;
                                    cx.notify();
                                }))
                            });
                            std::iter::once(group_checkbox).chain(extensions_checkbox)
                        }),
                    )),
            )
            .children(self.setup_error.clone().map(|error| {
                Banner::new()
                    .severity(Severity::Error)
                    .child(Label::new(error).size(LabelSize::Small))
            }))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .pt_2()
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Label::new("No repository yet?")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                ButtonLink::new(
                                    "Create a private one on GitHub",
                                    "https://github.com/new?name=zed-config&visibility=private",
                                )
                                .label_size(LabelSize::Small)
                                .label_color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when(busy, |this| {
                                this.child(
                                    Label::new(state.label())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            })
                            .child(
                                Button::new(
                                    "settings-sync-connect",
                                    if busy { "Connecting…" } else { "Connect" },
                                )
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Medium)
                                // Focusable, so clicking it blurs the fields above
                                // and commits their values before `connect` reads them.
                                .tab_index(0_isize)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_setup_field(
        &self,
        id: &'static str,
        title: &'static str,
        description: &'static str,
        placeholder: &'static str,
        initial: String,
        store: fn(&mut Self, String),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = cx.weak_entity();
        v_flex()
            .gap_1()
            .child(Label::new(title))
            .child(
                Label::new(description)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                SettingsInputField::new(id)
                    .with_initial_text(initial)
                    .with_placeholder(placeholder)
                    .tab_index(0)
                    .confirm_on_focus_out()
                    .aria_label(title)
                    .on_confirm(move |value, _, cx| {
                        let value = value.unwrap_or_default().trim().to_string();
                        page.update(cx, |this, cx| {
                            store(this, value);
                            cx.notify();
                        })
                        .ok();
                    }),
            )
            .into_any_element()
    }

    fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let service = self.service.read(cx);
        let Some(config) = service.config() else {
            return gpui::Empty.into_any_element();
        };
        let git = config.backend.git().clone();
        let state = service.state().clone();
        let last_sync = service.last_sync();
        let last_error = service.last_error().map(ToOwned::to_owned);

        let (badge_color, badge_text) = match &state {
            SyncState::Idle => (Color::Success, state.label()),
            SyncState::Syncing(_) => (Color::Accent, state.label()),
            SyncState::NeedsAttention(_) => (Color::Warning, state.label()),
            SyncState::Paused => (Color::Muted, state.label()),
            SyncState::Error(_) => (Color::Error, state.label()),
            SyncState::NotConfigured => (Color::Muted, state.label()),
        };

        let last_sync_text = match last_sync {
            Some(time) => format!("Last synced {}", describe_time_ago(time)),
            None => "Not synced yet".to_string(),
        };
        let detail = match &state {
            SyncState::Error(message) => Some(message.to_string()),
            SyncState::NeedsAttention(AttentionReason::Authentication) => Some(
                "Git could not authenticate with the repository. Sync now to enter credentials, or check your SSH keys and credential helper."
                    .to_string(),
            ),
            SyncState::NeedsAttention(AttentionReason::Offline) => {
                Some(last_error.unwrap_or_else(|| "The repository could not be reached.".to_string()))
            }
            _ => None,
        };

        v_flex()
            .gap_3()
            .child(
                h_flex().justify_between().items_start().gap_4().child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Headline::new("Settings Sync").size(HeadlineSize::Small))
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .child(Indicator::dot().color(badge_color))
                                        .child(
                                            Label::new(badge_text)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                ),
                        )
                        .child(
                            Label::new(format!("{} ({})", git.url, git.branch))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new(last_sync_text)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                ),
            )
            .children(detail.map(|detail| {
                Banner::new()
                    .severity(if matches!(state, SyncState::Error(_)) {
                        Severity::Error
                    } else {
                        Severity::Warning
                    })
                    .child(Label::new(detail).size(LabelSize::Small))
            }))
            .children(self.action_error.clone().map(|error| {
                Banner::new()
                    .severity(Severity::Error)
                    .child(Label::new(error).size(LabelSize::Small))
            }))
            .into_any_element()
    }

    fn render_missing_paths(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let missing = self.service.read(cx).missing_paths().to_vec();
        if missing.is_empty() {
            return None;
        }
        Some(
            Banner::new()
                .severity(Severity::Warning)
                .wrap_content(true)
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new("These settings came from another machine and point to paths that do not exist here:")
                                .size(LabelSize::Small),
                        )
                        .children(missing.into_iter().enumerate().map(|(index, setting)| {
                            let key = setting.key.clone();
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Label::new(format!("{}: {}", setting.key, setting.value))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Button::new(("keep-per-machine", index), "Keep per machine")
                                        .style(ButtonStyle::Outlined)
                                        .tab_index(0_isize)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let key = key.clone();
                                            this.update_config(
                                                move |config| {
                                                    if !config.per_machine_keys.contains(&key) {
                                                        config.per_machine_keys.push(key);
                                                    }
                                                },
                                                cx,
                                            );
                                        })),
                                )
                        }))
                        .child(
                            Button::new("dismiss-missing-paths", "Dismiss")
                                .style(ButtonStyle::Subtle)
                                .tab_index(0_isize)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.service.update(cx, |service, cx| service.dismiss_missing_paths(cx));
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_conflicts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let conflicts = self.service.read(cx).conflicts().to_vec();
        if conflicts.is_empty() {
            return None;
        }
        let resolving = self.resolving;

        let items = conflicts
            .iter()
            .enumerate()
            .map(|(index, pending)| self.render_conflict(index, pending, cx));

        Some(
            section(
                "Conflicts",
                v_flex()
                    .gap_3()
                    .child(
                        Label::new("Changed both locally and remotely since the last sync.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .children(items)
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(
                                    "apply-resolutions",
                                    if resolving {
                                        "Applying…"
                                    } else {
                                        "Apply choices"
                                    },
                                )
                                .style(ButtonStyle::Filled)
                                .tab_index(0_isize)
                                .disabled(resolving)
                                .on_click(cx.listener(|this, _, _, cx| this.apply_resolutions(cx))),
                            )
                            .child(
                                Button::new("resolve-all-local", "Keep All Local")
                                    .style(ButtonStyle::Outlined)
                                    .tab_index(0_isize)
                                    .disabled(resolving)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.resolve_all(Side::Local, cx)
                                    })),
                            )
                            .child(
                                Button::new("resolve-all-remote", "Use All Remote")
                                    .style(ButtonStyle::Outlined)
                                    .tab_index(0_isize)
                                    .disabled(resolving)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.resolve_all(Side::Remote, cx)
                                    })),
                            ),
                    )
                    .into_any_element(),
            )
            .into_any_element(),
        )
    }

    fn render_conflict(
        &self,
        index: usize,
        pending: &PendingConflict,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let repo_path = pending.file.repo_path.clone();
        let label = pending.file.label.clone();
        let description = match pending.conflict.kind {
            ConflictKind::BothChanged => "Changed on both sides",
            ConflictKind::LocalDeleted => "Deleted locally, changed remotely",
            ConflictKind::RemoteDeleted => "Deleted remotely, changed locally",
        };

        let body = if pending.conflict.is_key_level() {
            let rows =
                pending
                    .conflict
                    .key_conflicts
                    .iter()
                    .enumerate()
                    .map(|(key_index, conflict)| {
                        let key = conflict.key();
                        let choice = self
                            .key_choices
                            .get(&(repo_path.clone(), key.clone()))
                            .copied()
                            .unwrap_or(Side::Local);
                        let local_preview = preview_value(conflict.local.as_ref());
                        let remote_preview = preview_value(conflict.remote.as_ref());
                        h_flex()
                            .gap_3()
                            .items_center()
                            .py_1()
                            .child(
                                div()
                                    .w_48()
                                    .child(Label::new(key.clone()).size(LabelSize::Small)),
                            )
                            .child(self.render_side_choice(
                                ("conflict-key-local", index * 1000 + key_index),
                                format!("Local: {local_preview}"),
                                choice == Side::Local,
                                {
                                    let repo_path = repo_path.clone();
                                    let key = key.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.key_choices
                                            .insert((repo_path.clone(), key.clone()), Side::Local);
                                        cx.notify();
                                    })
                                },
                            ))
                            .child(self.render_side_choice(
                                ("conflict-key-remote", index * 1000 + key_index),
                                format!("Remote: {remote_preview}"),
                                choice == Side::Remote,
                                {
                                    let repo_path = repo_path.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.key_choices
                                            .insert((repo_path.clone(), key.clone()), Side::Remote);
                                        cx.notify();
                                    })
                                },
                            ))
                    });
            v_flex().children(rows).into_any_element()
        } else {
            let choice = self.file_choices.get(&repo_path).copied();
            let (local_label, remote_label) = match pending.conflict.kind {
                ConflictKind::BothChanged => ("Keep Local", "Use Remote"),
                ConflictKind::LocalDeleted => ("Delete Remote", "Restore From Remote"),
                ConflictKind::RemoteDeleted => ("Keep Local", "Delete Local"),
            };
            let diff_expanded = self.expanded_conflict_diffs.contains(&repo_path);
            let can_diff = pending.conflict.local.is_some() && pending.conflict.remote.is_some();

            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(self.render_side_choice(
                            ("conflict-file-local", index),
                            local_label.to_string(),
                            choice == Some(Side::Local),
                            {
                                let repo_path = repo_path.clone();
                                cx.listener(move |this, _, _, cx| {
                                    this.file_choices.insert(repo_path.clone(), Side::Local);
                                    cx.notify();
                                })
                            },
                        ))
                        .child(self.render_side_choice(
                            ("conflict-file-remote", index),
                            remote_label.to_string(),
                            choice == Some(Side::Remote),
                            {
                                let repo_path = repo_path.clone();
                                cx.listener(move |this, _, _, cx| {
                                    this.file_choices.insert(repo_path.clone(), Side::Remote);
                                    cx.notify();
                                })
                            },
                        ))
                        .when(can_diff, |this| {
                            this.child(
                                Button::new(
                                    ("conflict-toggle-diff", index),
                                    if diff_expanded {
                                        "Hide changes"
                                    } else {
                                        "Show changes"
                                    },
                                )
                                .style(ButtonStyle::Subtle)
                                .tab_index(0_isize)
                                .on_click({
                                    let repo_path = repo_path.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        if !this.expanded_conflict_diffs.remove(&repo_path) {
                                            this.expanded_conflict_diffs.insert(repo_path.clone());
                                        }
                                        cx.notify();
                                    })
                                }),
                            )
                            .child(
                                Button::new(("conflict-open-editor", index), "Open diff in editor")
                                    .style(ButtonStyle::Subtle)
                                    .tab_index(0_isize)
                                    .on_click({
                                        let pending = pending.clone();
                                        cx.listener(move |this, _, _, cx| {
                                            this.open_conflict_in_editor(&pending, cx);
                                        })
                                    }),
                            )
                        }),
                )
                .when(diff_expanded && can_diff, |this| {
                    this.child(render_diff(
                        pending.conflict.local.as_deref().unwrap_or_default(),
                        pending.conflict.remote.as_deref().unwrap_or_default(),
                        "local",
                        "remote",
                        cx,
                    ))
                })
                .into_any_element()
        };

        v_flex()
            .gap_1()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(label))
                    .child(
                        Label::new(description)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn render_side_choice(
        &self,
        id: impl Into<ElementId>,
        label: String,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        Button::new(id, label)
            .style(ButtonStyle::Outlined)
            .tab_index(0_isize)
            .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
            .toggle_state(selected)
            .on_click(on_click)
            .into_any_element()
    }

    fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let service = self.service.read(cx);
        let Some(config) = service.config() else {
            return gpui::Empty.into_any_element();
        };
        let enabled: HashSet<FileGroup> = config.files.iter().copied().collect();
        let statuses = service.file_statuses().to_vec();
        let group_count = FileGroup::ALL.len();
        let settings_enabled = enabled.contains(&FileGroup::Settings);
        let sync_extensions = config.sync_extensions;

        let extensions_row = setting_sub_row(
            "Extensions",
            "auto_install_extensions in settings.json.",
            Switch::new(
                "sync-extensions",
                ToggleState::from(settings_enabled && sync_extensions),
            )
            .tab_index(0_isize)
            .disabled(!settings_enabled)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.update_config(move |config| config.sync_extensions = !sync_extensions, cx);
            }))
            .into_any_element(),
            cx,
        )
        .into_any_element();
        let mut extensions_row = Some(extensions_row);

        let rows = FileGroup::ALL
            .into_iter()
            .enumerate()
            .map(|(index, group)| {
                let is_enabled = enabled.contains(&group);
                let group_statuses: Vec<_> = statuses
                    .iter()
                    .filter(|status| status.file.group == group)
                    .collect();
                let is_directory_group = matches!(group, FileGroup::Snippets | FileGroup::Themes);

                let status_text: Option<String> = if !is_enabled {
                    None
                } else if group_statuses.is_empty() {
                    Some("Not synced yet".to_string())
                } else if is_directory_group {
                    let conflicts = group_statuses
                        .iter()
                        .filter(|status| !status.settled)
                        .count();
                    let count = group_statuses
                        .iter()
                        .filter(|status| status.decision != FileSyncDecision::BothMissing)
                        .count();
                    Some(if conflicts > 0 {
                        format!("{count} files, {conflicts} in conflict")
                    } else if count == 0 {
                        "No files".to_string()
                    } else {
                        format!("{count} files in sync")
                    })
                } else {
                    let status = group_statuses[0];
                    Some(if status.settled {
                        match status.decision {
                            FileSyncDecision::BothMissing => {
                                "Not present on either side".to_string()
                            }
                            FileSyncDecision::InSync => "In sync".to_string(),
                            decision if decision.pushes() => "Pushed".to_string(),
                            decision if decision.pulls() => "Pulled".to_string(),
                            _ => "In sync".to_string(),
                        }
                    } else {
                        status.decision.description().to_string()
                    })
                };
                let status_color = match status_text.as_deref() {
                    Some(text) if text.contains("onflict") => Color::Warning,
                    _ => Color::Muted,
                };

                let backup_paths: Vec<String> = group_statuses
                    .iter()
                    .filter(|status| self.backups.contains(&status.file.repo_path))
                    .map(|status| status.file.repo_path.clone())
                    .collect();

                let control = h_flex()
                    .gap_2()
                    .items_center()
                    .children(backup_paths.into_iter().map(|repo_path| {
                        let id = SharedString::from(format!("restore-backup-{repo_path}"));
                        Button::new(id, "Restore backup")
                            .style(ButtonStyle::Subtle)
                            .tab_index(0_isize)
                            .tooltip(Tooltip::text(
                                "Bring back the local file from before a sync last replaced it",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.start_restore(repo_path.clone(), RestoreSource::Backup, cx);
                            }))
                    }))
                    .children(
                        status_text.map(|text| {
                            Label::new(text).size(LabelSize::Small).color(status_color)
                        }),
                    )
                    .child(
                        Switch::new(
                            SharedString::from(format!("sync-group-{}", group.hint())),
                            ToggleState::from(is_enabled),
                        )
                        .tab_index(0_isize)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_config(
                                move |config| {
                                    if config.files.contains(&group) {
                                        config.files.retain(|existing| *existing != group);
                                    } else {
                                        config.files.push(group);
                                        config.files.sort();
                                    }
                                },
                                cx,
                            );
                        })),
                    )
                    .into_any_element();

                let is_settings = group == FileGroup::Settings;
                // The Settings row carries the Extensions sub-row, which
                // draws its own borders.
                let row = setting_row(
                    group.label(),
                    group.hint(),
                    control,
                    !is_settings && index + 1 < group_count,
                );
                match (is_settings, extensions_row.take()) {
                    (true, Some(extensions_row)) => v_flex()
                        .w_full()
                        .child(row)
                        .child(extensions_row)
                        .into_any_element(),
                    _ => row.into_any_element(),
                }
            });

        section("Files", v_flex().children(rows).into_any_element()).into_any_element()
    }

    fn render_per_machine_keys(&self, cx: &mut Context<Self>) -> AnyElement {
        let service = self.service.read(cx);
        let Some(config) = service.config() else {
            return gpui::Empty.into_any_element();
        };
        let keys = config.per_machine_keys.clone();
        let border_color = cx.theme().colors().border.opacity(0.6);

        let rows: Vec<AnyElement> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| self.render_per_machine_key_row(index, key.clone(), cx))
            .collect();

        let page = cx.weak_entity();
        let add_input = SettingsInputField::new("per-machine-key-new")
            .with_placeholder("Add key (e.g. terminal.font_size)…")
            .tab_index(0)
            .with_buffer_font()
            .display_clear_button()
            .display_confirm_button()
            .clear_on_confirm()
            .on_confirm(move |key, _window, cx| {
                let Some(key) = key else {
                    return;
                };
                let key = key.trim().to_string();
                if key.is_empty() {
                    return;
                }
                page.update(cx, |this, cx| this.add_per_machine_key(None, key, cx))
                    .ok();
            })
            .into_any_element();

        section(
            "Per-Machine Settings",
            v_flex()
                .gap_0p5()
                .child(
                    Label::new(
                        "Keys that keep their local value and never sync. For OS-specific values, the macos, linux and windows blocks in settings.json keep the value in the repository instead.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .render_code_spans(),
                )
                .child(
                    v_flex()
                        .mt_2()
                        .w_full()
                        .gap_1p5()
                        .when(rows.is_empty(), |this| {
                            this.child(
                                h_flex()
                                    .p_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_dashed()
                                    .border_color(border_color)
                                    .child(
                                        Label::new("Nothing configured")
                                            .size(LabelSize::Small)
                                            .color(Color::Disabled),
                                    ),
                            )
                        })
                        .when(!rows.is_empty(), |this| this.child(v_flex().gap_1p5().children(rows)))
                        .child(add_input)
                        .children(self.per_machine_key_error.clone().map(|error| {
                            Label::new(error).size(LabelSize::Small).color(Color::Error)
                        })),
                )
                .into_any_element(),
        )
        .into_any_element()
    }

    fn render_per_machine_key_row(
        &self,
        index: usize,
        key: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = cx.weak_entity();
        let key_for_delete = key.clone();
        let key_for_update = key.clone();

        SettingsInputField::new(format!("per-machine-key-{index}"))
            .with_initial_text(key)
            .tab_index(0)
            .with_buffer_font()
            .color(Color::Default)
            .action_slot(
                IconButton::new(format!("per-machine-key-delete-{index}"), IconName::Trash)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tab_index(0_isize)
                    .tooltip(Tooltip::text("Remove Key"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let key = key_for_delete.clone();
                        this.per_machine_key_error = None;
                        this.update_config(
                            move |config| {
                                config.per_machine_keys.retain(|existing| *existing != key)
                            },
                            cx,
                        );
                    })),
            )
            .on_confirm(move |new_key, _window, cx| {
                let Some(new_key) = new_key else {
                    return;
                };
                let new_key = new_key.trim().to_string();
                if new_key.is_empty() || new_key == key_for_update {
                    return;
                }
                page.update(cx, |this, cx| {
                    this.add_per_machine_key(Some(key_for_update.clone()), new_key, cx)
                })
                .ok();
            })
            .into_any_element()
    }

    /// Add a per-machine key, or replace `previous` with it when a row was
    /// edited. Keys are checked against settings.json so a typo does not
    /// silently protect nothing.
    fn add_per_machine_key(
        &mut self,
        previous: Option<String>,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if key == "auto_install_extensions" {
            self.per_machine_key_error =
                Some("Use the Extensions toggle under Files instead.".into());
            cx.notify();
            return;
        }
        if !self.key_paths.contains(&key) {
            self.per_machine_key_error =
                Some(format!("settings.json has no key named {key}.").into());
            cx.notify();
            return;
        }
        self.per_machine_key_error = None;
        self.update_config(
            move |config| {
                if let Some(previous) = previous {
                    config
                        .per_machine_keys
                        .retain(|existing| *existing != previous);
                }
                if !config.per_machine_keys.contains(&key) {
                    config.per_machine_keys.push(key);
                }
            },
            cx,
        );
    }

    fn render_options(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let service = self.service.read(cx);
        let Some(config) = service.config() else {
            return gpui::Empty.into_any_element();
        };
        let strategy = config.conflict_strategy;
        let auto_sync = config.auto_sync;
        let interval_minutes = config.pull_interval_minutes as usize;
        let service_handle = self.service.clone();

        let strategy_dropdown = EnumVariantDropdown::new(
            "sync-conflict-strategy",
            strategy,
            <ConflictStrategy as strum::VariantArray>::VARIANTS,
            <ConflictStrategy as strum::VariantNames>::VARIANTS,
            {
                let service = service_handle.clone();
                move |value, _, cx| {
                    service.update(cx, |service, cx| {
                        service
                            .update_config(|config| config.conflict_strategy = value, cx)
                            .detach_and_log_err(cx);
                    });
                }
            },
        )
        .tab_index(0)
        .title_case(false);

        let interval_field = NumberField::new("sync-interval", interval_minutes, window, cx)
            .mode(NumberFieldMode::Edit, cx)
            .min(MIN_PULL_INTERVAL_MINUTES)
            .max(MAX_PULL_INTERVAL_MINUTES)
            .tab_index(0)
            .aria_label("Check For Changes Every")
            .on_change({
                let service = service_handle;
                move |minutes, _, cx| {
                    let minutes = (*minutes)
                        .clamp(MIN_PULL_INTERVAL_MINUTES, MAX_PULL_INTERVAL_MINUTES)
                        as u64;
                    service.update(cx, |service, cx| {
                        service
                            .update_config(move |config| config.pull_interval_minutes = minutes, cx)
                            .detach_and_log_err(cx);
                    });
                }
            });

        section(
            "Behavior",
            v_flex()
                .child(setting_row(
                    "Sync Automatically",
                    "Sync after config changes and check the repository on an interval.",
                    Switch::new("sync-auto", ToggleState::from(auto_sync))
                        .tab_index(0_isize)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_config(move |config| config.auto_sync = !auto_sync, cx);
                        }))
                        .into_any_element(),
                    true,
                ))
                .child(setting_row(
                    "Check For Changes Every",
                    "Minutes between checks for changes from other machines.",
                    interval_field.into_any_element(),
                    true,
                ))
                .child(setting_row(
                    "When Both Sides Change",
                    "How automatic syncs settle conflicts. Manual syncs always ask.",
                    strategy_dropdown.into_any_element(),
                    false,
                ))
                .into_any_element(),
        )
        .into_any_element()
    }

    fn render_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let content: AnyElement = if let Some(restore) = &self.restore {
            self.render_restore(restore, cx)
        } else if self.history_loading && self.history.is_none() {
            Label::new("Loading history…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        } else if let Some(error) = &self.history_error {
            Label::new(format!("Could not read history: {error}"))
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element()
        } else {
            let entries = self.history.clone().unwrap_or_default();
            if entries.is_empty() {
                Label::new("No syncs yet.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                v_flex()
                    .children(itertools::intersperse_with(
                        entries
                            .iter()
                            .enumerate()
                            .map(|(index, entry)| self.render_history_entry(index, entry, cx)),
                        || Divider::horizontal_dashed().into_any_element(),
                    ))
                    .into_any_element()
            }
        };

        section(
            "History",
            v_flex()
                .gap_2()
                .child(
                    Label::new(
                        "Every sync is a commit. Open one to restore an earlier version of a file.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(content)
                .into_any_element(),
        )
        .into_any_element()
    }

    fn render_history_entry(
        &self,
        index: usize,
        entry: &HistoryEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded_history.contains(&entry.hash);
        let when = format_date(&entry.date);
        let who = entry
            .host
            .clone()
            .map(|host| format!(" · {host}"))
            .unwrap_or_default();
        let toggle_listener = |hash: String| {
            cx.listener(move |this: &mut Self, _, _, cx| {
                if !this.expanded_history.remove(&hash) {
                    this.expanded_history.insert(hash.clone());
                }
                cx.notify();
            })
        };
        let on_toggle = toggle_listener(entry.hash.clone());
        let on_click = toggle_listener(entry.hash.clone());

        v_flex()
            .w_full()
            .child(
                ListItem::new(("history-entry", index))
                    .start_slot(
                        IconButton::new(
                            ("history-toggle", index),
                            if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            },
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tab_index(0_isize)
                        .on_click(on_toggle),
                    )
                    .on_click(on_click)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(Label::new(format!("{when}{who}")).size(LabelSize::Small))
                            .child(
                                Label::new(entry.short_hash().to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx),
                            ),
                    ),
            )
            .when(expanded, |this| {
                this.children(entry.files.iter().enumerate().map(|(file_index, file)| {
                    let repo_path = file.path.clone();
                    let hash = entry.hash.clone();
                    let deleted = file.status == 'D';
                    ListItem::new(("history-file", index * 1000 + file_index))
                        .indent_level(1)
                        .child(Label::new(file.path.clone()).size(LabelSize::Small).color(
                            if deleted {
                                Color::Muted
                            } else {
                                Color::Default
                            },
                        ))
                        .when(!deleted, |this| {
                            this.end_slot(
                                Button::new(
                                    ("history-restore", index * 1000 + file_index),
                                    "Restore",
                                )
                                .style(ButtonStyle::Subtle)
                                .tab_index(0_isize)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.start_restore(
                                            repo_path.clone(),
                                            RestoreSource::Commit(hash.clone()),
                                            cx,
                                        );
                                    },
                                )),
                            )
                        })
                }))
            })
            .into_any_element()
    }

    fn render_restore(&self, restore: &RestoreState, cx: &mut Context<Self>) -> AnyElement {
        let source_label = match &restore.source {
            RestoreSource::Commit(hash) => format!("version {}", &hash[..hash.len().min(7)]),
            RestoreSource::Backup => "the local backup".to_string(),
        };
        let title = format!("Restore {} from {source_label}", restore.repo_path);

        let body: AnyElement = if let Some(error) = &restore.error {
            Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element()
        } else if let Some(preview) = &restore.preview {
            if preview.current.as_deref() == Some(preview.restored.as_str()) {
                Label::new("The local file already matches that version.")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                v_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("Replaces {}", preview.local_path.display()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(render_diff(
                        preview.current.as_deref().unwrap_or_default(),
                        &preview.restored,
                        "now",
                        "restore",
                        cx,
                    ))
                    .into_any_element()
            }
        } else {
            Label::new("Loading…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };

        let can_apply =
            restore.preview.as_ref().is_some_and(|preview| {
                preview.current.as_deref() != Some(preview.restored.as_str())
            }) && restore.error.is_none()
                && !restore.applying;

        v_flex()
            .gap_2()
            .child(Label::new(title))
            .child(body)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("restore-confirm", if restore.applying { "Restoring…" } else { "Replace local file" })
                            .style(ButtonStyle::Filled)
                            .tab_index(0_isize)
                            .disabled(!can_apply)
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_restore(cx))),
                    )
                    .child(
                        Button::new("restore-cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .tab_index(0_isize)
                            .disabled(restore.applying)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.restore = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Label::new("The current file is backed up first. The next sync sends the restored version to your other machines.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }

    fn render_advanced(&self, cx: &mut Context<Self>) -> AnyElement {
        let root = self.service.read(cx).paths().root().clone();
        let paused = self.service.read(cx).is_paused();

        let current_machine_name = self
            .service
            .read(cx)
            .config()
            .map(|config| config.machine_name.clone())
            .unwrap_or_default();
        let machine_name_control = SettingsInputField::new("sync-machine-name")
            .with_initial_text(current_machine_name)
            .with_placeholder("This machine's name")
            .tab_index(0)
            .confirm_on_focus_out()
            .aria_label("Machine Name")
            .on_confirm({
                let page = cx.weak_entity();
                move |name, _, cx| {
                    let name = name
                        .as_deref()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(ToOwned::to_owned);
                    let Some(name) = name else {
                        return;
                    };
                    page.update(cx, |this, cx| {
                        let unchanged = this
                            .service
                            .read(cx)
                            .config()
                            .is_some_and(|config| config.machine_name == name);
                        if !unchanged {
                            this.update_config(move |config| config.machine_name = name, cx);
                        }
                    })
                    .ok();
                }
            })
            .into_any_element();

        let disconnect_control = Button::new("disconnect", "Disconnect")
            .style(ButtonStyle::Outlined)
            .tab_index(0_isize)
            .on_click(cx.listener(|_, _, window, cx| {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    "Disconnect settings sync?",
                    Some(
                        "Removes the local clone, snapshots and backups. Your repository and config files are left untouched.",
                    ),
                    &["Disconnect", "Cancel"],
                    cx,
                );
                let page = cx.weak_entity();
                cx.spawn(async move |_, cx| {
                    if answer.await != Ok(0) {
                        return;
                    }
                    page.update(cx, |this, cx| this.disconnect(cx)).ok();
                })
                .detach();
            }))
            .into_any_element();

        section(
            "Advanced",
            v_flex()
                .child(setting_row(
                    "Machine Name",
                    "Shown in the sync history.",
                    machine_name_control,
                    true,
                ))
                .child(setting_row(
                    "Sync Folder",
                    "The clone, base snapshots, backups and sync config live here.",
                    Button::new("reveal-sync-folder", "Reveal")
                        .style(ButtonStyle::Outlined)
                        .tab_index(0_isize)
                        .on_click(move |_, _, cx| cx.reveal_path(&root))
                        .into_any_element(),
                    true,
                ))
                .child(setting_row(
                    "Pause Sync",
                    "Stop syncing until resumed.",
                    Switch::new("sync-paused", ToggleState::from(paused))
                        .tab_index(0_isize)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.service.update(cx, |service, cx| {
                                if paused {
                                    service.resume(cx);
                                } else {
                                    service.pause(cx);
                                }
                            });
                        }))
                        .into_any_element(),
                    true,
                ))
                .child(setting_row(
                    "Disconnect",
                    "Forget the repository and local sync state.",
                    disconnect_control,
                    false,
                ))
                .into_any_element(),
        )
        .into_any_element()
    }
}

impl Render for SettingsSyncPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let configured = self.service.read(cx).is_configured();

        let mut page = v_flex()
            .id("settings-sync-page")
            .track_scroll(&self.scroll_handle)
            .size_full()
            .min_w_0()
            .pt_2p5()
            .px_8()
            .pb_16()
            .gap_6()
            .overflow_y_scroll();

        if !configured {
            page = page.child(self.render_setup(cx));
        } else {
            page = page
                .child(self.render_status(cx))
                .children(self.render_missing_paths(cx))
                .children(self.render_conflicts(cx))
                .child(self.render_files(cx))
                .child(self.render_per_machine_keys(cx))
                .child(self.render_options(window, cx))
                .child(self.render_history(cx))
                .child(self.render_advanced(cx));
        }
        page
    }
}

fn section(title: &'static str, content: AnyElement) -> impl IntoElement {
    v_flex()
        .gap_2()
        .child(SettingsSectionHeader::new(title).no_padding(true))
        .child(content)
}

/// A sub-item under a setting row, drawn the way the settings window draws
/// the fields that hang off a parent setting such as Icon Theme.
fn setting_sub_row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: AnyElement,
    cx: &App,
) -> impl IntoElement {
    let title = title.into();
    h_flex()
        .id(title.clone())
        .w_full()
        .min_w_0()
        .justify_between()
        .items_center()
        .gap_4()
        .p_4()
        .border_t_1()
        .border_b_1()
        .border_dashed()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_background.opacity(0.2))
        .child(
            v_flex()
                .w_full()
                .max_w_2_3()
                .min_w_0()
                .child(Label::new(title))
                .child(
                    Label::new(description.into())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .render_code_spans(),
                ),
        )
        .child(control)
}

/// One setting row in the layout the rest of the settings window uses: title
/// and description on the left, the control on the right.
fn setting_row(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: AnyElement,
    bottom_border: bool,
) -> impl IntoElement {
    let title = title.into();
    v_flex()
        .w_full()
        .child(
            h_flex()
                .id(title.clone())
                .w_full()
                .min_w_0()
                .justify_between()
                .items_center()
                .gap_4()
                .pt_4()
                .pb_4()
                .child(
                    v_flex()
                        .w_full()
                        .max_w_2_3()
                        .min_w_0()
                        .child(Label::new(title))
                        .child(
                            Label::new(description.into())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .render_code_spans(),
                        ),
                )
                .child(control),
        )
        .when(bottom_border, |this| this.child(Divider::horizontal()))
}

fn preview_value(value: Option<&Value>) -> String {
    match value {
        None => "(not set)".to_string(),
        Some(value) => {
            let text = serde_json::to_string(value).unwrap_or_default();
            if text.chars().count() > 60 {
                let truncated: String = text.chars().take(57).collect();
                format!("{truncated}…")
            } else {
                text
            }
        }
    }
}

fn format_date(rfc3339: &str) -> String {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%b %-d, %Y %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.to_string())
}

/// Changed lines only, as a compact preview; the full file is one click away in
/// the diff editor.
fn render_diff(
    local: &str,
    remote: &str,
    local_label: &'static str,
    remote_label: &'static str,
    cx: &App,
) -> AnyElement {
    let Some(lines) = diff_lines(local, remote) else {
        return Label::new("Too many differences to preview.")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element();
    };
    let changed: Vec<_> = lines
        .into_iter()
        .filter(|line| line.kind != DiffLineKind::Same)
        .collect();
    let total = changed.len();
    let status = cx.theme().status();
    let width = local_label.len().max(remote_label.len());

    v_flex()
        .font_buffer(cx)
        .text_xs()
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border)
        .overflow_hidden()
        .children(changed.into_iter().take(DIFF_PREVIEW_LINES).map(|line| {
            let (label, background) = match line.kind {
                DiffLineKind::Local => (local_label, status.deleted_background),
                DiffLineKind::Remote => (remote_label, status.created_background),
                DiffLineKind::Same => ("", status.modified_background),
            };
            div()
                .px_2()
                .bg(background)
                .child(format!("{label:width$} │ {}", line.text))
        }))
        .when(total > DIFF_PREVIEW_LINES, |this| {
            this.child(
                div().px_2().child(
                    Label::new(format!(
                        "… {} more changed lines",
                        total - DIFF_PREVIEW_LINES
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
            )
        })
        .into_any_element()
}
