//! The pure part of settings sync: which files are tracked, what to do with
//! each one given its local, remote and base content, and how a conflict is
//! settled. The service applies the resulting plan; nothing here does I/O.

use std::{collections::BTreeSet, path::PathBuf};

use serde_json::Value;

use crate::{
    config::{FileGroup, SyncConfig},
    merge::{
        FileTransform, KeyConflict, apply_merge_edits, merge_json, parse_jsonc, set_value_in_jsonc,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedFile {
    pub group: FileGroup,
    /// Path inside the repository, always with `/` separators.
    pub repo_path: String,
    pub local_path: PathBuf,
    pub label: String,
}

impl TrackedFile {
    pub fn is_settings(&self) -> bool {
        self.group == FileGroup::Settings
    }
}

/// The repository holds one keymap per platform, so each OS keeps its own key
/// bindings without any configuration. Unknown platforms share `keymap.json`.
pub fn platform_keymap_repo_file() -> &'static str {
    if cfg!(target_os = "macos") {
        "keymap.macos.json"
    } else if cfg!(target_os = "linux") {
        "keymap.linux.json"
    } else if cfg!(target_os = "windows") {
        "keymap.windows.json"
    } else {
        SHARED_KEYMAP_REPO_FILE
    }
}

/// Repositories created before keymaps were split per platform hold a single
/// shared keymap. It seeds a platform's keymap the first time that platform
/// syncs, and is never written to again.
pub const SHARED_KEYMAP_REPO_FILE: &str = "keymap.json";

/// Every file a sync could touch, regardless of which groups are enabled.
/// Snippet and theme names are discovered by the caller from both sides and
/// the base, since those directories hold arbitrarily named files.
pub fn all_tracked_files(
    config: &SyncConfig,
    snippet_names: &BTreeSet<String>,
    theme_names: &BTreeSet<String>,
) -> Vec<TrackedFile> {
    let fixed = |group: FileGroup, name: &str, local_path: PathBuf, label: &str| TrackedFile {
        group,
        repo_path: name.to_string(),
        local_path,
        label: label.to_string(),
    };
    let _ = config;

    let mut files = vec![
        fixed(
            FileGroup::Settings,
            "settings.json",
            paths::settings_file().clone(),
            "Settings",
        ),
        fixed(
            FileGroup::Keymap,
            platform_keymap_repo_file(),
            paths::keymap_file().clone(),
            "Key Bindings",
        ),
        fixed(
            FileGroup::Tasks,
            "tasks.json",
            paths::tasks_file().clone(),
            "Tasks",
        ),
        fixed(
            FileGroup::Debug,
            "debug.json",
            paths::debug_scenarios_file().clone(),
            "Debug Scenarios",
        ),
        fixed(
            FileGroup::Agents,
            "AGENTS.md",
            paths::agents_file().clone(),
            "Agent Rules",
        ),
    ];
    files.extend(snippet_names.iter().map(|name| TrackedFile {
        group: FileGroup::Snippets,
        repo_path: format!("snippets/{name}"),
        local_path: paths::snippets_dir().join(name),
        label: format!("Snippet: {name}"),
    }));
    files.extend(theme_names.iter().map(|name| TrackedFile {
        group: FileGroup::Themes,
        repo_path: format!("themes/{name}"),
        local_path: paths::themes_dir().join(name),
        label: format!("Theme: {name}"),
    }));
    files
}

/// Where a file from the repository lives in Zed's config directory, or `None`
/// when the repository path is not one sync manages. Every keymap variant maps
/// to the one `keymap.json` Zed reads.
pub fn local_path_for_repo_file(repo_path: &str) -> Option<PathBuf> {
    match repo_path {
        "settings.json" => return Some(paths::settings_file().clone()),
        "tasks.json" => return Some(paths::tasks_file().clone()),
        "debug.json" => return Some(paths::debug_scenarios_file().clone()),
        "AGENTS.md" => return Some(paths::agents_file().clone()),
        "keymap.json" | "keymap.macos.json" | "keymap.linux.json" | "keymap.windows.json" => {
            return Some(paths::keymap_file().clone());
        }
        _ => {}
    }
    let (dir, name) = repo_path.split_once('/')?;
    if name.is_empty() || name.contains('/') || !name.ends_with(".json") {
        return None;
    }
    match dir {
        "snippets" => Some(paths::snippets_dir().join(name)),
        "themes" => Some(paths::themes_dir().join(name)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSyncDecision {
    BothMissing,
    PushNew,
    PullNew,
    InSync,
    PushLocal,
    PullRemote,
    DeleteRemote,
    DeleteLocal,
    Conflict,
    ConflictLocalDeleted,
    ConflictRemoteDeleted,
}

impl FileSyncDecision {
    pub fn is_conflict(self) -> bool {
        matches!(
            self,
            FileSyncDecision::Conflict
                | FileSyncDecision::ConflictLocalDeleted
                | FileSyncDecision::ConflictRemoteDeleted
        )
    }

    pub fn pushes(self) -> bool {
        matches!(
            self,
            FileSyncDecision::PushNew
                | FileSyncDecision::PushLocal
                | FileSyncDecision::DeleteRemote
        )
    }

    pub fn pulls(self) -> bool {
        matches!(
            self,
            FileSyncDecision::PullNew
                | FileSyncDecision::PullRemote
                | FileSyncDecision::DeleteLocal
        )
    }

    pub fn description(self) -> &'static str {
        match self {
            FileSyncDecision::BothMissing => "Not present on either side",
            FileSyncDecision::PushNew => "Local only, not pushed yet",
            FileSyncDecision::PullNew => "Repository only, not pulled yet",
            FileSyncDecision::InSync => "In sync",
            FileSyncDecision::PushLocal => "Local changes",
            FileSyncDecision::PullRemote => "Repository changes",
            FileSyncDecision::DeleteRemote => "Deleted locally, will be removed remotely",
            FileSyncDecision::DeleteLocal => "Deleted remotely, local copy will be removed",
            FileSyncDecision::Conflict => "Conflict: changed on both sides",
            FileSyncDecision::ConflictLocalDeleted => "Conflict: deleted locally, changed remotely",
            FileSyncDecision::ConflictRemoteDeleted => {
                "Conflict: deleted remotely, changed locally"
            }
        }
    }
}

/// The three versions of one tracked file. `base` is the repository copy as of
/// the last successful sync.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileSides<'a> {
    pub local: Option<&'a str>,
    pub remote: Option<&'a str>,
    pub base: Option<&'a str>,
}

/// Context shared by every file in one run.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncContext {
    /// No tracked file exists locally: a new or reset machine, where a missing
    /// file means "not there yet" rather than "deleted".
    pub local_looks_fresh: bool,
    /// No tracked file exists in the repository: an empty or wiped repository.
    pub remote_looks_fresh: bool,
}

/// Change detection is by content against the base snapshot rather than by
/// modification time, so it survives restores, copies and clock skew. On the
/// first sync there is no base and both sides count as changed.
pub fn decide(
    sides: FileSides<'_>,
    transform: &FileTransform,
    context: SyncContext,
) -> FileSyncDecision {
    let FileSides {
        local,
        remote,
        base,
    } = sides;

    let changed = |side: Option<&str>| match (side, base) {
        (Some(side), Some(base)) => !transform.equal(side, base),
        (None, None) => false,
        _ => true,
    };
    let previously_synced = base.is_some()
        && !(local.is_none() && context.local_looks_fresh)
        && !(remote.is_none() && context.remote_looks_fresh);

    match (local, remote) {
        (None, None) => FileSyncDecision::BothMissing,
        (Some(local), None) => {
            if !previously_synced {
                FileSyncDecision::PushNew
            } else if changed(Some(local)) {
                FileSyncDecision::ConflictRemoteDeleted
            } else {
                FileSyncDecision::DeleteLocal
            }
        }
        (None, Some(remote)) => {
            if !previously_synced {
                FileSyncDecision::PullNew
            } else if changed(Some(remote)) {
                FileSyncDecision::ConflictLocalDeleted
            } else {
                FileSyncDecision::DeleteRemote
            }
        }
        (Some(local), Some(remote)) => {
            if transform.equal(local, remote) {
                return FileSyncDecision::InSync;
            }
            match (changed(Some(local)), changed(Some(remote))) {
                (true, false) => FileSyncDecision::PushLocal,
                (false, true) => FileSyncDecision::PullRemote,
                _ => FileSyncDecision::Conflict,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    BothChanged,
    LocalDeleted,
    RemoteDeleted,
}

/// A conflict the engine could not settle on its own. For settings.json the
/// non-conflicting changes have already been merged into `merged_local`, and
/// only `key_conflicts` need a decision.
#[derive(Debug, Clone, PartialEq)]
pub struct FileConflict {
    pub kind: ConflictKind,
    pub local: Option<String>,
    pub remote: Option<String>,
    pub key_conflicts: Vec<KeyConflict>,
    pub merged_local: Option<String>,
}

impl FileConflict {
    pub fn is_key_level(&self) -> bool {
        !self.key_conflicts.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlannedAction {
    Nothing,
    WriteRemote(String),
    WriteLocal {
        content: String,
        backup: bool,
    },
    DeleteRemote,
    DeleteLocal,
    /// Key-level merge of settings.json that needed no decision.
    Merged {
        local: String,
        remote: String,
        summary: String,
    },
    Conflict(FileConflict),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FilePlan {
    pub file: TrackedFile,
    pub decision: FileSyncDecision,
    pub action: PlannedAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// Decide what to do with one file. `auto_resolve` settles any conflict in
/// favour of that side; `None` leaves conflicts for the user.
pub fn plan_file(
    file: TrackedFile,
    sides: FileSides<'_>,
    transform: &FileTransform,
    context: SyncContext,
    auto_resolve: Option<Side>,
) -> FilePlan {
    let decision = decide(sides, transform, context);
    let local = sides.local.unwrap_or_default();
    let remote = sides.remote.unwrap_or_default();

    let action = match decision {
        FileSyncDecision::BothMissing | FileSyncDecision::InSync => PlannedAction::Nothing,
        FileSyncDecision::PushNew => PlannedAction::WriteRemote(transform.to_remote(local, None)),
        FileSyncDecision::PullNew => PlannedAction::WriteLocal {
            content: transform.to_local(remote, None),
            backup: false,
        },
        FileSyncDecision::PushLocal => {
            PlannedAction::WriteRemote(transform.to_remote(local, Some(remote)))
        }
        FileSyncDecision::PullRemote => PlannedAction::WriteLocal {
            content: transform.to_local(remote, Some(local)),
            backup: true,
        },
        FileSyncDecision::DeleteRemote => PlannedAction::DeleteRemote,
        FileSyncDecision::DeleteLocal => PlannedAction::DeleteLocal,
        FileSyncDecision::ConflictLocalDeleted => {
            let conflict = FileConflict {
                kind: ConflictKind::LocalDeleted,
                local: None,
                remote: sides.remote.map(ToOwned::to_owned),
                key_conflicts: Vec::new(),
                merged_local: None,
            };
            match auto_resolve {
                Some(side) => resolve_conflict(&conflict, &Resolution::Side(side), transform),
                None => PlannedAction::Conflict(conflict),
            }
        }
        FileSyncDecision::ConflictRemoteDeleted => {
            let conflict = FileConflict {
                kind: ConflictKind::RemoteDeleted,
                local: sides.local.map(ToOwned::to_owned),
                remote: None,
                key_conflicts: Vec::new(),
                merged_local: None,
            };
            match auto_resolve {
                Some(side) => resolve_conflict(&conflict, &Resolution::Side(side), transform),
                None => PlannedAction::Conflict(conflict),
            }
        }
        FileSyncDecision::Conflict => {
            let mut conflict = FileConflict {
                kind: ConflictKind::BothChanged,
                local: Some(local.to_string()),
                remote: Some(remote.to_string()),
                key_conflicts: Vec::new(),
                merged_local: None,
            };
            if file.is_settings()
                && let Some(merged) = merge_settings(local, remote, sides.base, transform)
            {
                if merged.conflicts.is_empty() {
                    let merged_remote = transform.to_remote(&merged.text, Some(remote));
                    return FilePlan {
                        file,
                        decision,
                        action: PlannedAction::Merged {
                            local: merged.text,
                            remote: merged_remote,
                            summary: merged.summary,
                        },
                    };
                }
                conflict.key_conflicts = merged.conflicts;
                conflict.merged_local = Some(merged.text);
            }
            match auto_resolve {
                Some(side) => resolve_conflict(&conflict, &Resolution::Side(side), transform),
                None => PlannedAction::Conflict(conflict),
            }
        }
    };

    FilePlan {
        file,
        decision,
        action,
    }
}

struct SettingsMerge {
    text: String,
    conflicts: Vec<KeyConflict>,
    summary: String,
}

/// Key-level merge of settings.json: changes made on one side only are
/// combined, and only keys both sides changed need a decision. Returns `None`
/// when the file cannot be merged that way (unparseable, not an object, or
/// the two sides only differ in comments or formatting).
fn merge_settings(
    local: &str,
    remote: &str,
    base: Option<&str>,
    transform: &FileTransform,
) -> Option<SettingsMerge> {
    let local_value = parse_jsonc(local)?;
    let remote_value = parse_jsonc(remote)?;
    if !local_value.is_object() || !remote_value.is_object() {
        return None;
    }
    if transform.equal(local, remote) {
        return None;
    }
    let base_value = base.and_then(parse_jsonc);
    let result = merge_json(
        base_value.as_ref(),
        &local_value,
        &remote_value,
        transform.ignored_keys(),
    );
    let text = apply_merge_edits(local, &result.edits);
    let edit_count = result.edits.len();
    let summary = format!(
        "{edit_count} remote change{}",
        if edit_count == 1 { "" } else { "s" }
    );
    Some(SettingsMerge {
        text,
        conflicts: result.conflicts,
        summary,
    })
}

/// How the user (or the configured strategy) settled one conflict.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Side(Side),
    /// For settings.json: which side wins for each conflicting key, by key path.
    PerKey(Vec<(String, Side)>),
}

/// Turn a resolved conflict into the writes that settle it on both sides.
pub fn resolve_conflict(
    conflict: &FileConflict,
    resolution: &Resolution,
    transform: &FileTransform,
) -> PlannedAction {
    let local = conflict.local.as_deref();
    let remote = conflict.remote.as_deref();

    match conflict.kind {
        ConflictKind::LocalDeleted => match resolution {
            Resolution::Side(Side::Local) => PlannedAction::DeleteRemote,
            _ => PlannedAction::WriteLocal {
                content: transform.to_local(remote.unwrap_or_default(), None),
                backup: false,
            },
        },
        ConflictKind::RemoteDeleted => match resolution {
            Resolution::Side(Side::Local) => {
                PlannedAction::WriteRemote(transform.to_remote(local.unwrap_or_default(), None))
            }
            _ => PlannedAction::DeleteLocal,
        },
        ConflictKind::BothChanged => {
            let local = local.unwrap_or_default();
            let remote = remote.unwrap_or_default();
            match resolution {
                Resolution::Side(Side::Local) => {
                    let base_text = conflict.merged_local.as_deref().unwrap_or(local);
                    let merged_remote = transform.to_remote(base_text, Some(remote));
                    if base_text == local {
                        PlannedAction::WriteRemote(merged_remote)
                    } else {
                        PlannedAction::Merged {
                            local: base_text.to_string(),
                            remote: merged_remote,
                            summary: "kept local values".to_string(),
                        }
                    }
                }
                Resolution::Side(Side::Remote) => {
                    let text = match &conflict.merged_local {
                        Some(merged) if !conflict.key_conflicts.is_empty() => {
                            apply_key_choices(merged, &conflict.key_conflicts, |_| Side::Remote)
                        }
                        _ => transform.to_local(remote, Some(local)),
                    };
                    PlannedAction::Merged {
                        local: text.clone(),
                        remote: transform.to_remote(&text, Some(remote)),
                        summary: "used remote values".to_string(),
                    }
                }
                Resolution::PerKey(choices) => {
                    let merged = conflict.merged_local.as_deref().unwrap_or(local);
                    let text = apply_key_choices(merged, &conflict.key_conflicts, |key| {
                        choices
                            .iter()
                            .find(|(choice_key, _)| choice_key == key)
                            .map(|(_, side)| *side)
                            .unwrap_or(Side::Local)
                    });
                    let resolved = conflict.key_conflicts.len();
                    PlannedAction::Merged {
                        local: text.clone(),
                        remote: transform.to_remote(&text, Some(remote)),
                        summary: format!(
                            "{resolved} conflict{} resolved",
                            if resolved == 1 { "" } else { "s" }
                        ),
                    }
                }
            }
        }
    }
}

fn apply_key_choices(
    merged_local: &str,
    key_conflicts: &[KeyConflict],
    choose: impl Fn(&str) -> Side,
) -> String {
    key_conflicts
        .iter()
        .fold(merged_local.to_string(), |text, conflict| {
            match choose(&conflict.key()) {
                Side::Local => text,
                Side::Remote => {
                    set_value_in_jsonc(&text, &conflict.key_path, conflict.remote.as_ref())
                }
            }
        })
}

/// Reconcile `auto_install_extensions` in a settings.json text against the
/// extensions installed on this machine: installed ids become `true`, a `true`
/// entry is dropped only for an extension this machine had and has since
/// uninstalled (it was in `previously_installed`), and explicit `false`
/// entries are always preserved as user intent. Entries for extensions that
/// were never installed here come from other machines and are left alone so
/// they can be auto-installed. Returns the new text when it changed.
pub fn reconcile_auto_install_extensions(
    settings_text: &str,
    installed_ids: &BTreeSet<String>,
    previously_installed: &BTreeSet<String>,
) -> Option<String> {
    let parsed = parse_jsonc(settings_text).unwrap_or_else(|| Value::Object(Map::new()));
    if !parsed.is_object() {
        return None;
    }
    let existing = parsed
        .get("auto_install_extensions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let uninstalled_here =
        |id: &String| !installed_ids.contains(id) && previously_installed.contains(id);

    let mut reconciled = Map::new();
    for (id, value) in &existing {
        if value == &Value::Bool(false) || !uninstalled_here(id) {
            reconciled.insert(id.clone(), value.clone());
        }
    }
    for id in installed_ids {
        reconciled.insert(id.clone(), Value::Bool(true));
    }

    let added = installed_ids
        .iter()
        .any(|id| existing.get(id) != Some(&Value::Bool(true)));
    let removed = existing
        .iter()
        .any(|(id, value)| value == &Value::Bool(true) && uninstalled_here(id));
    if !added && !removed {
        return None;
    }

    Some(set_value_in_jsonc(
        settings_text,
        &["auto_install_extensions".to_string()],
        Some(&Value::Object(reconciled)),
    ))
}

use serde_json::Map;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file() -> TrackedFile {
        TrackedFile {
            group: FileGroup::Tasks,
            repo_path: "tasks.json".into(),
            local_path: PathBuf::from("/config/tasks.json"),
            label: "Tasks".into(),
        }
    }

    fn settings_file() -> TrackedFile {
        TrackedFile {
            group: FileGroup::Settings,
            repo_path: "settings.json".into(),
            local_path: PathBuf::from("/config/settings.json"),
            label: "Settings".into(),
        }
    }

    fn sides<'a>(
        local: Option<&'a str>,
        remote: Option<&'a str>,
        base: Option<&'a str>,
    ) -> FileSides<'a> {
        FileSides {
            local,
            remote,
            base,
        }
    }

    #[test]
    fn decisions_without_a_base() {
        let identity = FileTransform::Identity;
        let context = SyncContext::default();
        assert_eq!(
            decide(sides(None, None, None), &identity, context),
            FileSyncDecision::BothMissing
        );
        assert_eq!(
            decide(sides(Some("a"), None, None), &identity, context),
            FileSyncDecision::PushNew
        );
        assert_eq!(
            decide(sides(None, Some("a"), None), &identity, context),
            FileSyncDecision::PullNew
        );
        assert_eq!(
            decide(sides(Some("a"), Some("a"), None), &identity, context),
            FileSyncDecision::InSync
        );
        assert_eq!(
            decide(sides(Some("a"), Some("b"), None), &identity, context),
            FileSyncDecision::Conflict
        );
    }

    #[test]
    fn decisions_with_a_base() {
        let identity = FileTransform::Identity;
        let context = SyncContext::default();
        assert_eq!(
            decide(
                sides(Some("new"), Some("old"), Some("old")),
                &identity,
                context
            ),
            FileSyncDecision::PushLocal
        );
        assert_eq!(
            decide(
                sides(Some("old"), Some("new"), Some("old")),
                &identity,
                context
            ),
            FileSyncDecision::PullRemote
        );
        assert_eq!(
            decide(sides(Some("x"), Some("y"), Some("old")), &identity, context),
            FileSyncDecision::Conflict
        );
        assert_eq!(
            decide(sides(None, Some("old"), Some("old")), &identity, context),
            FileSyncDecision::DeleteRemote
        );
        assert_eq!(
            decide(sides(Some("old"), None, Some("old")), &identity, context),
            FileSyncDecision::DeleteLocal
        );
        assert_eq!(
            decide(sides(None, Some("new"), Some("old")), &identity, context),
            FileSyncDecision::ConflictLocalDeleted
        );
        assert_eq!(
            decide(sides(Some("new"), None, Some("old")), &identity, context),
            FileSyncDecision::ConflictRemoteDeleted
        );
    }

    #[test]
    fn a_fresh_machine_only_pulls() {
        let identity = FileTransform::Identity;
        let context = SyncContext {
            local_looks_fresh: true,
            remote_looks_fresh: false,
        };
        assert_eq!(
            decide(sides(None, Some("old"), Some("old")), &identity, context),
            FileSyncDecision::PullNew
        );
    }

    #[test]
    fn settings_conflicts_merge_by_key() {
        let base = r#"{"a": 1, "b": 1}"#;
        let local = "{\n  \"a\": 2,\n  \"b\": 1\n}\n";
        let remote = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let plan = plan_file(
            settings_file(),
            sides(Some(local), Some(remote), Some(base)),
            &FileTransform::Identity,
            SyncContext::default(),
            None,
        );
        match plan.action {
            PlannedAction::Merged { local, remote, .. } => {
                assert_eq!(parse_jsonc(&local), Some(json!({"a": 2, "b": 2})));
                assert_eq!(parse_jsonc(&remote), Some(json!({"a": 2, "b": 2})));
            }
            other => panic!("expected a merge, got {other:?}"),
        }
    }

    #[test]
    fn settings_key_conflicts_are_reported_with_the_merged_text() {
        let base = r#"{"a": 1, "b": 1, "c": 1}"#;
        let local = "{\n  \"a\": 2,\n  \"b\": 1,\n  \"c\": 5\n}\n";
        let remote = "{\n  \"a\": 3,\n  \"b\": 2,\n  \"c\": 1\n}\n";
        let plan = plan_file(
            settings_file(),
            sides(Some(local), Some(remote), Some(base)),
            &FileTransform::Identity,
            SyncContext::default(),
            None,
        );
        let PlannedAction::Conflict(conflict) = plan.action else {
            panic!("expected a conflict");
        };
        assert_eq!(conflict.key_conflicts.len(), 1);
        assert_eq!(conflict.key_conflicts[0].key(), "a");
        let merged = conflict.merged_local.as_deref().unwrap();
        assert_eq!(parse_jsonc(merged), Some(json!({"a": 2, "b": 2, "c": 5})));

        let resolved = resolve_conflict(
            &conflict,
            &Resolution::PerKey(vec![("a".into(), Side::Remote)]),
            &FileTransform::Identity,
        );
        let PlannedAction::Merged { local, .. } = resolved else {
            panic!("expected a merge");
        };
        assert_eq!(parse_jsonc(&local), Some(json!({"a": 3, "b": 2, "c": 5})));
    }

    #[test]
    fn whole_file_conflicts_follow_the_chosen_side() {
        let plan = plan_file(
            file(),
            sides(Some("mine"), Some("theirs"), Some("base")),
            &FileTransform::Identity,
            SyncContext::default(),
            Some(Side::Remote),
        );
        assert_eq!(
            plan.action,
            PlannedAction::Merged {
                local: "theirs".into(),
                remote: "theirs".into(),
                summary: "used remote values".into()
            }
        );

        let plan = plan_file(
            file(),
            sides(Some("mine"), Some("theirs"), Some("base")),
            &FileTransform::Identity,
            SyncContext::default(),
            Some(Side::Local),
        );
        assert_eq!(plan.action, PlannedAction::WriteRemote("mine".into()));
    }

    #[test]
    fn deletion_conflicts_resolve() {
        let plan = plan_file(
            file(),
            sides(None, Some("new"), Some("old")),
            &FileTransform::Identity,
            SyncContext::default(),
            Some(Side::Local),
        );
        assert_eq!(plan.action, PlannedAction::DeleteRemote);

        let plan = plan_file(
            file(),
            sides(Some("new"), None, Some("old")),
            &FileTransform::Identity,
            SyncContext::default(),
            Some(Side::Remote),
        );
        assert_eq!(plan.action, PlannedAction::DeleteLocal);
    }

    #[test]
    fn reconciles_auto_install_extensions() {
        let settings = "{\n  \"auto_install_extensions\": {\n    \"old\": true,\n    \"from_other_machine\": true,\n    \"never\": false\n  }\n}\n";
        let installed: BTreeSet<String> = ["html".to_string()].into_iter().collect();
        let previously: BTreeSet<String> = ["old".to_string()].into_iter().collect();
        let next = reconcile_auto_install_extensions(settings, &installed, &previously).unwrap();
        assert_eq!(
            parse_jsonc(&next),
            Some(json!({
                "auto_install_extensions": {
                    "html": true,
                    "from_other_machine": true,
                    "never": false
                }
            }))
        );
        assert!(reconcile_auto_install_extensions(&next, &installed, &installed).is_none());
    }

    #[test]
    fn maps_repo_paths_to_local_paths() {
        assert_eq!(
            local_path_for_repo_file("keymap.linux.json"),
            Some(paths::keymap_file().clone())
        );
        assert_eq!(
            local_path_for_repo_file("snippets/rust.json"),
            Some(paths::snippets_dir().join("rust.json"))
        );
        assert_eq!(local_path_for_repo_file("README.md"), None);
        assert_eq!(local_path_for_repo_file("snippets/a/b.json"), None);
    }
}
