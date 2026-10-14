//! Parsing of `git log --name-status` output from the sync repository.

use crate::engine::local_path_for_repo_file;

/// Separators for `git log` output that cannot appear in commit subjects or paths.
pub const RECORD_SEPARATOR: char = '\x1e';
pub const FIELD_SEPARATOR: char = '\x1f';

pub fn log_format() -> String {
    format!("{RECORD_SEPARATOR}%H{FIELD_SEPARATOR}%cI{FIELD_SEPARATOR}%s")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryFile {
    /// `A` (added), `M` (modified), `D` (deleted), …
    pub status: char,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub hash: String,
    /// RFC 3339 commit time as git printed it.
    pub date: String,
    /// The machine that made the sync commit, when the subject names one.
    pub host: Option<String>,
    pub subject: String,
    pub files: Vec<HistoryFile>,
}

impl HistoryEntry {
    pub fn short_hash(&self) -> &str {
        &self.hash[..self.hash.len().min(7)]
    }
}

/// Parse `git log --name-status` output written with [`log_format`], keeping
/// only files that sync manages.
pub fn parse_history(output: &str) -> Vec<HistoryEntry> {
    output
        .split(RECORD_SEPARATOR)
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let mut lines = record.lines();
            let header = lines.next()?;
            let mut fields = header.split(FIELD_SEPARATOR);
            let hash = fields.next()?.trim().to_string();
            let date = fields.next()?.trim().to_string();
            let subject = fields.next().unwrap_or_default().trim().to_string();
            let host = subject
                .rsplit_once(" from ")
                .map(|(_, host)| host.trim().to_string())
                .filter(|host| !host.is_empty());

            let files = lines
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| {
                    // Renames and copies list the old path first; keep the new one.
                    let mut parts = line.split('\t');
                    let status = parts.next()?.chars().next()?;
                    let path = parts.next_back()?.to_string();
                    Some(HistoryFile { status, path })
                })
                .filter(|file| local_path_for_repo_file(&file.path).is_some())
                .collect();

            Some(HistoryEntry {
                hash,
                date,
                host,
                subject,
                files,
            })
        })
        .filter(|entry| !entry.files.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_output() {
        let output = format!(
            "{RECORD_SEPARATOR}abc123def{FIELD_SEPARATOR}2026-10-01T10:00:00+00:00{FIELD_SEPARATOR}sync: 2026-10-01T10:00:00Z from laptop\nM\tsettings.json\nA\tsnippets/rust.json\nD\tREADME.md\n{RECORD_SEPARATOR}fff{FIELD_SEPARATOR}2026-09-01T10:00:00+00:00{FIELD_SEPARATOR}init\nA\tREADME.md\n"
        );
        let entries = parse_history(&output);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.short_hash(), "abc123d");
        assert_eq!(entry.host.as_deref(), Some("laptop"));
        assert_eq!(
            entry.files,
            vec![
                HistoryFile {
                    status: 'M',
                    path: "settings.json".into()
                },
                HistoryFile {
                    status: 'A',
                    path: "snippets/rust.json".into()
                },
            ]
        );
    }
}
