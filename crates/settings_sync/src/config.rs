use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use fs::Fs;
use serde::{Deserialize, Serialize};

/// One group of files that can be synced. Snippets and themes are directories
/// whose contents are discovered on each run; the rest are single files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileGroup {
    Settings,
    Keymap,
    Tasks,
    Debug,
    Agents,
    Snippets,
    Themes,
}

impl FileGroup {
    pub const ALL: [FileGroup; 7] = [
        FileGroup::Settings,
        FileGroup::Keymap,
        FileGroup::Tasks,
        FileGroup::Debug,
        FileGroup::Agents,
        FileGroup::Snippets,
        FileGroup::Themes,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileGroup::Settings => "Settings",
            FileGroup::Keymap => "Key Bindings",
            FileGroup::Tasks => "Tasks",
            FileGroup::Debug => "Debug Scenarios",
            FileGroup::Agents => "Agent Rules",
            FileGroup::Snippets => "Snippets",
            FileGroup::Themes => "Local Themes",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            FileGroup::Settings => "settings.json",
            FileGroup::Keymap => "keymap.json, one per platform",
            FileGroup::Tasks => "tasks.json",
            FileGroup::Debug => "debug.json",
            FileGroup::Agents => "AGENTS.md",
            FileGroup::Snippets => "snippets/*.json",
            FileGroup::Themes => "themes/*.json",
        }
    }
}

/// How a conflict is settled when both sides changed the same file (or, for
/// settings.json, the same key) since the last sync.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    Serialize,
    Deserialize,
    strum::VariantArray,
    strum::VariantNames,
)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStrategy {
    /// Keep the non-conflicting changes and let the user decide the rest.
    #[default]
    #[strum(serialize = "Ask")]
    Ask,
    #[strum(serialize = "Local wins")]
    KeepLocal,
    #[strum(serialize = "Remote wins")]
    UseRemote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitBackendConfig {
    pub url: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfig {
    Git(GitBackendConfig),
}

impl BackendConfig {
    pub fn git(&self) -> &GitBackendConfig {
        match self {
            BackendConfig::Git(config) => config,
        }
    }

    /// Identifies the remote a base snapshot was taken against, so a base from
    /// another repository or branch is never used for deletion detection.
    pub fn source_id(&self) -> String {
        let git = self.git();
        format!("{}#{}", git.url, git.branch)
    }
}

/// Machine-local sync configuration. Lives outside the config directory on
/// purpose: it describes this machine's relationship to the repository and is
/// never itself synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    pub version: u32,
    pub backend: BackendConfig,
    pub files: Vec<FileGroup>,
    #[serde(default)]
    pub conflict_strategy: ConflictStrategy,
    #[serde(default)]
    pub per_machine_keys: Vec<String>,
    /// Whether `auto_install_extensions` in settings.json follows the
    /// repository. Off, it stays per machine and is not reconciled with the
    /// extensions installed here.
    #[serde(default = "default_true")]
    pub sync_extensions: bool,
    #[serde(default = "default_true")]
    pub auto_sync: bool,
    #[serde(default = "default_pull_interval")]
    pub pull_interval_minutes: u64,
    pub machine_name: String,
}

fn default_true() -> bool {
    true
}

fn default_pull_interval() -> u64 {
    15
}

impl SyncConfig {
    pub fn new_git(url: String, branch: String) -> Self {
        Self {
            version: 1,
            backend: BackendConfig::Git(GitBackendConfig { url, branch }),
            files: FileGroup::ALL.to_vec(),
            conflict_strategy: ConflictStrategy::default(),
            per_machine_keys: Vec::new(),
            sync_extensions: true,
            auto_sync: true,
            pull_interval_minutes: default_pull_interval(),
            machine_name: default_machine_name(),
        }
    }

    pub fn syncs(&self, group: FileGroup) -> bool {
        self.files.contains(&group)
    }

    /// Keys kept per machine, including `auto_install_extensions` when
    /// extensions are not synced.
    pub fn effective_per_machine_keys(&self) -> Vec<String> {
        let mut keys = self.per_machine_keys.clone();
        if !self.sync_extensions && !keys.iter().any(|key| key == AUTO_INSTALL_EXTENSIONS_KEY) {
            keys.push(AUTO_INSTALL_EXTENSIONS_KEY.to_string());
        }
        keys
    }
}

pub const AUTO_INSTALL_EXTENSIONS_KEY: &str = "auto_install_extensions";

pub fn default_machine_name() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "unknown-machine".to_string())
}

/// Persisted between runs so the UI can render without a fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStateFile {
    /// RFC 3339 timestamp of the last successful sync.
    pub last_sync: Option<String>,
    pub last_error: Option<String>,
    /// Extensions installed on this machine as of the last run. An entry in
    /// `auto_install_extensions` is only dropped for an extension that was in
    /// this list and is gone now, so a list pulled from another machine is
    /// not stripped before those extensions finish installing here.
    #[serde(default)]
    pub installed_extensions: Vec<String>,
}

/// Layout of the directory under `data_dir()` that holds all sync state.
#[derive(Debug, Clone)]
pub struct SyncPaths {
    root: PathBuf,
}

impl SyncPaths {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn default_root() -> PathBuf {
        paths::data_dir().join("settings_sync")
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn repo_dir(&self) -> PathBuf {
        self.root.join("repo")
    }

    pub fn base_dir(&self) -> PathBuf {
        self.root.join("base")
    }

    pub fn base_source_file(&self) -> PathBuf {
        self.base_dir().join(".source")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    /// Remote versions of conflicting files are written here so a diff editor
    /// can show them next to the local file.
    pub fn conflicts_dir(&self) -> PathBuf {
        self.root.join("conflicts")
    }
}

pub async fn load_config(fs: &Arc<dyn Fs>, paths: &SyncPaths) -> Result<Option<SyncConfig>> {
    let path = paths.config_file();
    if !fs.is_file(&path).await {
        return Ok(None);
    }
    let text = fs.load(&path).await?;
    let config = serde_json::from_str(&text)
        .with_context(|| format!("parsing settings sync config at {}", path.display()))?;
    Ok(Some(config))
}

pub async fn save_config(fs: &Arc<dyn Fs>, paths: &SyncPaths, config: &SyncConfig) -> Result<()> {
    fs.create_dir(paths.root()).await?;
    let text = serde_json::to_string_pretty(config)?;
    fs.atomic_write(paths.config_file(), text).await
}

pub async fn load_state(fs: &Arc<dyn Fs>, paths: &SyncPaths) -> SyncStateFile {
    let path = paths.state_file();
    if !fs.is_file(&path).await {
        return SyncStateFile::default();
    }
    match fs.load(&path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => SyncStateFile::default(),
    }
}

pub async fn save_state(fs: &Arc<dyn Fs>, paths: &SyncPaths, state: &SyncStateFile) -> Result<()> {
    fs.create_dir(paths.root()).await?;
    let text = serde_json::to_string_pretty(state)?;
    fs.atomic_write(paths.state_file(), text).await
}
