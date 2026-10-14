//! Pure JSON helpers for the sync engine: three-way key merge of settings.json,
//! per-machine key handling, key path listing, absolute-path detection and a
//! line diff for previews. Nothing here touches the filesystem.

use serde_json::{Map, Value};
use settings_json::replace_value_in_json_text;

pub type KeyPath = Vec<String>;

/// Above this many line pairs the LCS table is not worth building for a preview.
const MAX_DIFF_CELLS: usize = 4_000_000;

/// `terminal.font_size` -> `["terminal", "font_size"]`
pub fn parse_key_path(key: &str) -> KeyPath {
    key.split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn key_path_string(path: &[String]) -> String {
    path.join(".")
}

fn key_paths(keys: &[String]) -> Vec<KeyPath> {
    keys.iter()
        .map(|key| parse_key_path(key))
        .filter(|path| !path.is_empty())
        .collect()
}

/// Parse JSON with comments, returning `None` when the source is empty or has
/// syntax errors so callers can fall back to treating the file as opaque text.
pub fn parse_jsonc(src: &str) -> Option<Value> {
    if src.trim().is_empty() {
        return None;
    }
    settings_json::parse_json_with_comments::<Value>(src).ok()
}

/// Indentation used by a JSONC source, falling back to Zed's default of two
/// spaces, so edits match the surrounding file.
pub fn detect_tab_size(src: &str) -> usize {
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed == line {
            continue;
        }
        let indent = &line[..line.len() - trimmed.len()];
        if indent.starts_with('\t') {
            return 1;
        }
        return indent.len();
    }
    2
}

pub fn get_at_path<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut node = root;
    for segment in path {
        node = node.as_object()?.get(segment)?;
    }
    Some(node)
}

fn remove_at_path(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut node = root;
    for segment in parents {
        match node
            .as_object_mut()
            .and_then(|object| object.get_mut(segment))
        {
            Some(child) => node = child,
            None => return,
        }
    }
    if let Some(object) = node.as_object_mut() {
        object.remove(last);
    }
}

fn without_paths(root: &Value, paths: &[KeyPath]) -> Value {
    let mut copy = root.clone();
    for path in paths {
        remove_at_path(&mut copy, path);
    }
    copy
}

/// Deep equality of two parsed values, disregarding the given keys.
pub fn equal_ignoring(a: &Value, b: &Value, ignored_keys: &[String]) -> bool {
    let paths = key_paths(ignored_keys);
    without_paths(a, &paths) == without_paths(b, &paths)
}

/// Every key path in a parsed settings object, depth-first and dot-joined.
/// Arrays count as values. Keys containing a dot are skipped since a dotted
/// path cannot address them.
pub fn list_key_paths(settings: &Value) -> Vec<String> {
    fn walk(value: &Value, prefix: &str, out: &mut Vec<String>) {
        let Some(object) = value.as_object() else {
            return;
        };
        for (key, child) in object {
            if key.contains('.') {
                continue;
            }
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            out.push(path.clone());
            walk(child, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(settings, "", &mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSetting {
    pub key: String,
    pub value: String,
}

fn looks_like_absolute_path(value: &str) -> bool {
    if value.starts_with('/') || value.starts_with("~/") || value.starts_with("\\\\") {
        return true;
    }
    let mut chars = value.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
    )
}

/// `ssh_connections` holds paths on remote hosts and `env` values are
/// environment variables, not single local paths.
fn holds_non_local_paths(prefix: &str, name: &str) -> bool {
    name == "env" || (prefix.is_empty() && name == "ssh_connections")
}

/// String values in a parsed settings object (arrays included) that look like
/// absolute local file paths, with the dotted key they sit under.
pub fn find_absolute_paths(settings: &Value) -> Vec<PathSetting> {
    fn walk(value: &Value, key: &str, out: &mut Vec<PathSetting>) {
        match value {
            Value::String(text) => {
                if !key.is_empty() && looks_like_absolute_path(text) {
                    out.push(PathSetting {
                        key: key.to_string(),
                        value: text.clone(),
                    });
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, key, out);
                }
            }
            Value::Object(object) => {
                for (name, child) in object {
                    if holds_non_local_paths(key, name) {
                        continue;
                    }
                    let child_key = if key.is_empty() {
                        name.clone()
                    } else {
                        format!("{key}.{name}")
                    };
                    walk(child, &child_key, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(settings, "", &mut out);
    out
}

/// Set (or, with `None`, remove) one key path in a JSONC source, leaving the
/// rest of the text, comments and formatting included, as it was.
pub fn set_value_in_jsonc(src: &str, key_path: &[String], value: Option<&Value>) -> String {
    if key_path.is_empty() {
        return src.to_string();
    }
    let parsed = parse_jsonc(src);
    if value.is_none() {
        // The underlying helper writes `null` for a missing key; a removal of
        // something that is not there must be a no-op instead.
        let present = parsed
            .as_ref()
            .and_then(|root| get_at_path(root, key_path))
            .is_some();
        if !present {
            return src.to_string();
        }
    }
    if parsed.as_ref().is_some_and(|root| !root.is_object()) {
        return src.to_string();
    }

    let mut text = if src.trim().is_empty() {
        "{}\n".to_string()
    } else {
        src.to_string()
    };
    let tab_size = detect_tab_size(&text);
    let (range, replacement) = replace_value_in_json_text(&text, key_path, tab_size, value, None);
    if range.start > text.len() || range.end > text.len() || range.start > range.end {
        return src.to_string();
    }
    text.replace_range(range, &replacement);
    text
}

/// How a tracked file's content maps between the local copy and the repository
/// copy. Settings keys kept per machine are neither pushed nor pulled, and do
/// not count when comparing the two sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransform {
    Identity,
    PerMachineKeys(Vec<String>),
}

impl FileTransform {
    pub fn for_settings(per_machine_keys: &[String]) -> Self {
        if key_paths(per_machine_keys).is_empty() {
            FileTransform::Identity
        } else {
            FileTransform::PerMachineKeys(per_machine_keys.to_vec())
        }
    }

    /// What belongs in the repository for this local content, given what the
    /// repository holds now.
    pub fn to_remote(&self, local: &str, remote: Option<&str>) -> String {
        match self {
            FileTransform::Identity => local.to_string(),
            FileTransform::PerMachineKeys(keys) => carry_over(local, remote, keys),
        }
    }

    /// What belongs on disk for this repository content, given what is there now.
    pub fn to_local(&self, remote: &str, local: Option<&str>) -> String {
        match self {
            FileTransform::Identity => remote.to_string(),
            FileTransform::PerMachineKeys(keys) => carry_over(remote, local, keys),
        }
    }

    pub fn equal(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        match self {
            FileTransform::Identity => false,
            FileTransform::PerMachineKeys(keys) => match (parse_jsonc(a), parse_jsonc(b)) {
                (Some(a), Some(b)) => equal_ignoring(&a, &b, keys),
                _ => false,
            },
        }
    }

    pub fn ignored_keys(&self) -> &[String] {
        match self {
            FileTransform::Identity => &[],
            FileTransform::PerMachineKeys(keys) => keys,
        }
    }
}

/// Take `source` across to the other side, but leave every per-machine key the
/// way `target` already has it (or absent, when target does not set it).
fn carry_over(source: &str, target: Option<&str>, keys: &[String]) -> String {
    let Some(source_value) = parse_jsonc(source) else {
        return source.to_string();
    };
    if !source_value.is_object() {
        return source.to_string();
    }
    let target_value = target.and_then(parse_jsonc);
    let mut out = source.to_string();
    for path in key_paths(keys) {
        let target_at = target_value
            .as_ref()
            .and_then(|target| get_at_path(target, &path));
        out = set_value_in_jsonc(&out, &path, target_at);
    }
    out
}

/// A change local needs to take on from remote. `value: None` removes the key.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeEdit {
    pub key_path: KeyPath,
    pub value: Option<Value>,
}

/// A key both sides changed, to different values. `None` means deleted.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyConflict {
    pub key_path: KeyPath,
    pub local: Option<Value>,
    pub remote: Option<Value>,
}

impl KeyConflict {
    pub fn key(&self) -> String {
        key_path_string(&self.key_path)
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct MergeResult {
    pub edits: Vec<MergeEdit>,
    pub conflicts: Vec<KeyConflict>,
}

fn merge_at(
    base: Option<&Value>,
    local: Option<&Value>,
    remote: Option<&Value>,
    key_path: &mut KeyPath,
    out: &mut MergeResult,
) {
    if local == remote {
        return;
    }

    // Descend first so a change deep inside an object becomes one small edit
    // rather than a replacement of the whole object.
    if let (Some(Value::Object(local_object)), Some(Value::Object(remote_object))) = (local, remote)
    {
        let empty = Map::new();
        let base_object = match base {
            Some(Value::Object(object)) => object,
            _ => &empty,
        };
        let mut keys: Vec<&String> = base_object
            .keys()
            .chain(local_object.keys())
            .chain(remote_object.keys())
            .collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            key_path.push(key.clone());
            merge_at(
                base_object.get(key),
                local_object.get(key),
                remote_object.get(key),
                key_path,
                out,
            );
            key_path.pop();
        }
        return;
    }

    if local == base {
        out.edits.push(MergeEdit {
            key_path: key_path.clone(),
            value: remote.cloned(),
        });
        return;
    }
    if remote == base {
        return;
    }
    out.conflicts.push(KeyConflict {
        key_path: key_path.clone(),
        local: local.cloned(),
        remote: remote.cloned(),
    });
}

/// Three-way merge of two JSON objects against their common ancestor (`base`,
/// or `None` when there is none, in which case every differing key is a
/// conflict and keys present on one side only are kept). Arrays are treated as
/// single values. The result is expressed as what local has to change.
pub fn merge_json(
    base: Option<&Value>,
    local: &Value,
    remote: &Value,
    ignored_keys: &[String],
) -> MergeResult {
    let paths = key_paths(ignored_keys);
    let base = base.map(|base| without_paths(base, &paths));
    let local = without_paths(local, &paths);
    let remote = without_paths(remote, &paths);
    let mut out = MergeResult::default();
    merge_at(
        base.as_ref(),
        Some(&local),
        Some(&remote),
        &mut Vec::new(),
        &mut out,
    );
    out
}

pub fn apply_merge_edits(src: &str, edits: &[MergeEdit]) -> String {
    edits.iter().fold(src.to_string(), |text, edit| {
        set_value_in_jsonc(&text, &edit.key_path, edit.value.as_ref())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Same,
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// Line diff between the local and remote version of a file. Returns `None`
/// when the changed region is too large to compare cheaply.
pub fn diff_lines(local: &str, remote: &str) -> Option<Vec<DiffLine>> {
    let a: Vec<&str> = local.split('\n').collect();
    let b: Vec<&str> = remote.split('\n').collect();

    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end_a = a.len();
    let mut end_b = b.len();
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }

    let mid_a = &a[start..end_a];
    let mid_b = &b[start..end_b];
    if mid_a.len().saturating_mul(mid_b.len()) > MAX_DIFF_CELLS {
        return None;
    }

    // Longest-common-subsequence table, filled from the end so the walk below
    // can read it front to back.
    let width = mid_b.len() + 1;
    let mut table = vec![0u32; (mid_a.len() + 1) * width];
    for i in (0..mid_a.len()).rev() {
        for j in (0..mid_b.len()).rev() {
            table[i * width + j] = if mid_a[i] == mid_b[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }

    let same = |text: &str| DiffLine {
        kind: DiffLineKind::Same,
        text: text.to_string(),
    };
    let mut lines: Vec<DiffLine> = a[..start].iter().map(|line| same(line)).collect();

    let (mut i, mut j) = (0, 0);
    while i < mid_a.len() && j < mid_b.len() {
        if mid_a[i] == mid_b[j] {
            lines.push(same(mid_a[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            lines.push(DiffLine {
                kind: DiffLineKind::Local,
                text: mid_a[i].to_string(),
            });
            i += 1;
        } else {
            lines.push(DiffLine {
                kind: DiffLineKind::Remote,
                text: mid_b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < mid_a.len() {
        lines.push(DiffLine {
            kind: DiffLineKind::Local,
            text: mid_a[i].to_string(),
        });
        i += 1;
    }
    while j < mid_b.len() {
        lines.push(DiffLine {
            kind: DiffLineKind::Remote,
            text: mid_b[j].to_string(),
        });
        j += 1;
    }

    lines.extend(a[end_a..].iter().map(|line| same(line)));
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    #[test]
    fn parses_key_paths() {
        assert_eq!(
            parse_key_path("terminal.font_size"),
            vec!["terminal", "font_size"]
        );
        assert_eq!(parse_key_path(" a . b "), vec!["a", "b"]);
        assert!(parse_key_path("").is_empty());
    }

    #[test]
    fn merge_takes_one_sided_changes_and_flags_both_sided_ones() {
        let base = json!({"a": 1, "b": 2, "c": {"x": 1, "y": 1}});
        let local = json!({"a": 1, "b": 3, "c": {"x": 2, "y": 1}});
        let remote = json!({"a": 5, "b": 4, "c": {"x": 1, "y": 9}});
        let result = merge_json(Some(&base), &local, &remote, &[]);
        assert_eq!(
            result.edits,
            vec![
                MergeEdit {
                    key_path: vec!["a".into()],
                    value: Some(json!(5))
                },
                MergeEdit {
                    key_path: vec!["c".into(), "y".into()],
                    value: Some(json!(9))
                },
            ]
        );
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].key(), "b");
    }

    #[test]
    fn merge_without_base_keeps_one_sided_keys_and_conflicts_on_shared_ones() {
        let local = json!({"a": 1, "only_local": true});
        let remote = json!({"a": 2, "only_remote": true});
        let result = merge_json(None, &local, &remote, &[]);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].key(), "a");
        assert_eq!(
            result.edits,
            vec![MergeEdit {
                key_path: vec!["only_remote".into()],
                value: Some(json!(true))
            }]
        );
    }

    #[test]
    fn merge_ignores_per_machine_keys() {
        let base = json!({"a": 1, "machine": {"path": "/base"}});
        let local = json!({"a": 1, "machine": {"path": "/local"}});
        let remote = json!({"a": 1, "machine": {"path": "/remote"}});
        let result = merge_json(Some(&base), &local, &remote, &paths(&["machine.path"]));
        assert!(result.edits.is_empty());
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn edits_preserve_comments() {
        let src = "{\n  // keep me\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let edited = set_value_in_jsonc(src, &paths(&["b"]), Some(&json!(3)));
        assert!(edited.contains("// keep me"));
        assert_eq!(parse_jsonc(&edited), Some(json!({"a": 1, "b": 3})));
    }

    #[test]
    fn removing_a_missing_key_is_a_no_op() {
        let src = "{\n  \"a\": 1\n}\n";
        assert_eq!(set_value_in_jsonc(src, &paths(&["b"]), None), src);
        assert_eq!(set_value_in_jsonc(src, &paths(&["a", "b"]), None), src);
    }

    #[test]
    fn removing_a_present_key_works() {
        let src = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
        let edited = set_value_in_jsonc(src, &paths(&["b"]), None);
        assert_eq!(parse_jsonc(&edited), Some(json!({"a": 1})));
    }

    #[test]
    fn transform_carries_per_machine_keys_across() {
        let transform = FileTransform::for_settings(&paths(&["font_size"]));
        let local = "{\n  \"theme\": \"One\",\n  \"font_size\": 14\n}\n";
        let remote = "{\n  \"theme\": \"Two\",\n  \"font_size\": 20\n}\n";

        let to_remote = transform.to_remote(local, Some(remote));
        assert_eq!(
            parse_jsonc(&to_remote),
            Some(json!({"theme": "One", "font_size": 20}))
        );

        let to_local = transform.to_local(remote, Some(local));
        assert_eq!(
            parse_jsonc(&to_local),
            Some(json!({"theme": "Two", "font_size": 14}))
        );

        let to_remote_fresh = transform.to_remote(local, None);
        assert_eq!(parse_jsonc(&to_remote_fresh), Some(json!({"theme": "One"})));

        assert!(transform.equal(
            "{\"theme\": \"One\", \"font_size\": 1}",
            "{\"theme\": \"One\", \"font_size\": 2}"
        ));
        assert!(!transform.equal(
            "{\"theme\": \"One\", \"font_size\": 1}",
            "{\"theme\": \"Two\", \"font_size\": 1}"
        ));
    }

    #[test]
    fn lists_key_paths_depth_first() {
        let settings = json!({"a": {"b": 1, "c.d": 2}, "e": [1, 2]});
        assert_eq!(list_key_paths(&settings), vec!["a", "a.b", "e"]);
    }

    #[test]
    fn finds_absolute_paths() {
        let settings = json!({
            "lsp": {"rust-analyzer": {"binary": {"path": "/opt/ra"}}},
            "paths": ["~/one", "relative"],
            "terminal": {"env": {"PATH": "/usr/bin"}},
            "ssh_connections": [{"host": "/remote"}],
            "win": "C:\\Users\\me"
        });
        let found = find_absolute_paths(&settings);
        let keys: Vec<_> = found.iter().map(|setting| setting.key.as_str()).collect();
        assert_eq!(keys, vec!["lsp.rust-analyzer.binary.path", "paths", "win"]);
    }

    #[test]
    fn diffs_lines() {
        let diff = diff_lines("a\nb\nc", "a\nx\nc").unwrap();
        let kinds: Vec<_> = diff.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::Same,
                DiffLineKind::Local,
                DiffLineKind::Remote,
                DiffLineKind::Same
            ]
        );
    }

    #[test]
    fn detects_indentation() {
        assert_eq!(detect_tab_size("{\n    \"a\": 1\n}"), 4);
        assert_eq!(detect_tab_size("{\n\t\"a\": 1\n}"), 1);
        assert_eq!(detect_tab_size("{}"), 2);
    }
}
