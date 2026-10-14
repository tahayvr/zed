//! The git implementation of the sync backend: a persistent clone of the sync
//! repository under the sync data directory. Fetch and push go through Zed's
//! git crate so credential prompts reuse the askpass machinery; everything
//! else is a plain git invocation inside the clone.

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use askpass::AskPassDelegate;
use fs::{Fs, RemoveOptions};
use futures::FutureExt as _;
use git::repository::{FetchOptions, GitRepository, PushOptions};
use gpui::{AsyncApp, BackgroundExecutor};
use util::command::new_command;

use crate::history::{HistoryEntry, log_format, parse_history};

const REMOTE_NAME: &str = "origin";
const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(15);
const FALLBACK_AUTHOR_NAME: &str = "Zed Settings Sync";
const FALLBACK_AUTHOR_EMAIL: &str = "settings-sync@zed.dev";

pub struct GitBackend {
    fs: Arc<dyn Fs>,
    repo_dir: PathBuf,
    url: String,
    branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The remote branch exists and the clone now matches it.
    Updated,
    /// The remote has no such branch yet; the next publish creates it.
    RemoteBranchMissing,
}

async fn run_git_in(dir: &PathBuf, args: &[&str]) -> Result<String> {
    let output = new_command("git")
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(anyhow!("git {} failed: {detail}", args.join(" ")))
    }
}

/// Whether the remote branch exists. Fails on any network or authentication
/// problem rather than mistaking it for an empty remote.
pub async fn remote_branch_exists(
    url: &str,
    branch: &str,
    executor: &BackgroundExecutor,
) -> Result<bool> {
    let url = url.to_string();
    let refspec = format!("refs/heads/{branch}");
    let mut command = new_command("git");
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["ls-remote", "--heads", &url, &refspec])
        .output();
    let timeout = executor.timer(LS_REMOTE_TIMEOUT);
    futures::select_biased! {
        output = output.fuse() => {
            let output = output.context("running git ls-remote")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(anyhow!("Could not reach {url}: {stderr}"));
            }
            Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
        }
        _ = timeout.fuse() => Err(anyhow!("Timed out reaching {url}")),
    }
}

impl GitBackend {
    pub fn new(fs: Arc<dyn Fs>, repo_dir: PathBuf, url: String, branch: String) -> Self {
        Self {
            fs,
            repo_dir,
            url,
            branch,
        }
    }

    pub fn repo_dir(&self) -> &PathBuf {
        &self.repo_dir
    }

    pub fn repo_file_path(&self, repo_path: &str) -> PathBuf {
        self.repo_dir.join(repo_path)
    }

    async fn run_git(&self, args: &[&str]) -> Result<String> {
        run_git_in(&self.repo_dir, args).await
    }

    async fn is_cloned(&self) -> bool {
        self.fs.is_dir(&self.repo_dir.join(".git")).await
    }

    fn open_repo(&self) -> Result<Arc<dyn GitRepository>> {
        let system_git = which::which("git").ok();
        let repo = self
            .fs
            .open_repo(&self.repo_dir.join(".git"), system_git.as_deref())?;
        // The clone is Zed's own scratch mirror, so its config is trusted.
        repo.set_trusted(true);
        Ok(repo)
    }

    /// Make sure the persistent clone exists. When the remote branch does not
    /// exist yet the clone is initialised empty and pointed at the remote, so
    /// the first publish creates the branch.
    pub async fn ensure_clone(&self, remote_branch_exists: bool) -> Result<()> {
        if self.is_cloned().await {
            self.ensure_remote_url().await?;
            return Ok(());
        }
        if self.fs.is_dir(&self.repo_dir).await {
            self.fs
                .remove_dir(
                    &self.repo_dir,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await?;
        }
        let parent = self
            .repo_dir
            .parent()
            .context("sync repository directory has no parent")?
            .to_path_buf();
        self.fs.create_dir(&parent).await?;

        let target = self.repo_dir.to_string_lossy().into_owned();
        if remote_branch_exists {
            run_git_in(
                &parent,
                &["clone", "--branch", &self.branch, "--", &self.url, &target],
            )
            .await?;
        } else {
            self.fs.create_dir(&self.repo_dir).await?;
            self.run_git(&["init", "-b", &self.branch]).await?;
            self.run_git(&["remote", "add", REMOTE_NAME, &self.url])
                .await?;
        }
        self.ensure_identity().await?;
        Ok(())
    }

    async fn ensure_remote_url(&self) -> Result<()> {
        let current = self
            .run_git(&["remote", "get-url", REMOTE_NAME])
            .await
            .unwrap_or_default();
        if current.trim() != self.url {
            self.run_git(&["remote", "set-url", REMOTE_NAME, &self.url])
                .await?;
        }
        Ok(())
    }

    /// Commits need an identity; fall back to a sync-specific one in the clone's
    /// local config so first-time users are not blocked on `git config`.
    async fn ensure_identity(&self) -> Result<()> {
        let name = self
            .run_git(&["config", "user.name"])
            .await
            .unwrap_or_default();
        if name.trim().is_empty() {
            self.run_git(&["config", "user.name", FALLBACK_AUTHOR_NAME])
                .await?;
        }
        let email = self
            .run_git(&["config", "user.email"])
            .await
            .unwrap_or_default();
        if email.trim().is_empty() {
            self.run_git(&["config", "user.email", FALLBACK_AUTHOR_EMAIL])
                .await?;
        }
        Ok(())
    }

    /// Fetch the remote branch and make the clone's working tree match it. The
    /// clone is a mirror, never a place where reconciliation happens, so a hard
    /// reset is the right operation.
    pub async fn fetch(&self, askpass: AskPassDelegate, cx: &mut AsyncApp) -> Result<FetchOutcome> {
        let repo = self.open_repo()?;
        repo.fetch(
            FetchOptions::All,
            askpass,
            Arc::new(HashMap::default()),
            cx.clone(),
        )
        .await
        .context("fetching the settings sync repository")?;

        let remote_ref = format!("{REMOTE_NAME}/{}", self.branch);
        let exists = self
            .run_git(&["rev-parse", "--verify", "--quiet", &remote_ref])
            .await
            .is_ok();
        if !exists {
            return Ok(FetchOutcome::RemoteBranchMissing);
        }
        self.run_git(&["checkout", "-B", &self.branch, &remote_ref])
            .await?;
        self.run_git(&["reset", "--hard", &remote_ref]).await?;
        self.run_git(&["clean", "-fd"]).await?;
        Ok(FetchOutcome::Updated)
    }

    /// Stage the given paths (writes and deletions alike), commit and push.
    pub async fn publish(
        &self,
        paths: &[String],
        message: &str,
        askpass: AskPassDelegate,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add", "-A", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run_git(&args).await?;

        let staged = self.run_git(&["diff", "--cached", "--name-only"]).await?;
        if staged.trim().is_empty() {
            return Ok(());
        }
        self.run_git(&["commit", "--quiet", "--no-verify", "-m", message])
            .await?;

        let repo = self.open_repo()?;
        repo.push(
            self.branch.clone(),
            self.branch.clone(),
            REMOTE_NAME.to_string(),
            Some(PushOptions::SetUpstream),
            askpass,
            Arc::new(HashMap::default()),
            cx.clone(),
        )
        .await
        .context("pushing to the settings sync repository")?;
        Ok(())
    }

    pub async fn history(&self, path: Option<&str>, limit: usize) -> Result<Vec<HistoryEntry>> {
        let format = format!("--format={}", log_format());
        let max_count = format!("--max-count={}", limit.max(1) * 4);
        let mut args = vec![
            "log",
            format.as_str(),
            max_count.as_str(),
            "--name-status",
            "--no-renames",
        ];
        if let Some(path) = path {
            args.push("--");
            args.push(path);
        }
        let output = match self.run_git(&args).await {
            Ok(output) => output,
            // A repository with no commits yet has no history, not an error.
            Err(error) if error.to_string().contains("does not have any commits") => String::new(),
            Err(error) => return Err(error),
        };
        let mut entries = parse_history(&output);
        entries.truncate(limit);
        Ok(entries)
    }

    pub async fn file_at(&self, revision: &str, repo_path: &str) -> Result<Option<String>> {
        let spec = format!("{revision}:{repo_path}");
        match self.run_git(&["show", &spec]).await {
            Ok(text) => Ok(Some(text)),
            Err(error) => {
                let message = error.to_string();
                if message.contains("does not exist")
                    || message.contains("exists on disk, but not in")
                {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }
}
