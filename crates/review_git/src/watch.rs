//! Watches a repository for changes that affect the review: edits to files that
//! aren't ignored, plus index and ref updates (staging, commits, checkouts).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use git2::Repository;
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};

/// Bursts of events (an agent writing several files, a checkout) collapse into one call.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Stops watching when dropped.
pub struct RepoWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl RepoWatcher {
    /// Calls `on_change` from a background thread whenever relevant files change.
    pub fn new(repo_root: &Path, on_change: impl Fn() + Send + 'static) -> Result<Self> {
        let repo_root = repo_root.to_path_buf();
        let git_dir = Repository::open(&repo_root)?.path().to_path_buf();

        let filter = ChangeFilter {
            repo_root: repo_root.clone(),
            git_dir: git_dir.clone(),
        };
        let mut debouncer = new_debouncer(DEBOUNCE, move |result: DebounceEventResult| {
            let Ok(events) = result else { return };
            // Opened per batch: `Repository` isn't `Sync`, and batches are infrequent.
            let Ok(repo) = Repository::open(&filter.repo_root) else {
                return;
            };
            if events
                .iter()
                .any(|event| filter.is_relevant(&repo, &event.path))
            {
                on_change();
            }
        })?;
        debouncer
            .watcher()
            .watch(&repo_root, RecursiveMode::Recursive)?;
        // Linked worktrees and `--separate-git-dir` keep the git dir elsewhere.
        if !git_dir.starts_with(&repo_root) {
            debouncer
                .watcher()
                .watch(&git_dir, RecursiveMode::Recursive)?;
        }
        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

struct ChangeFilter {
    repo_root: PathBuf,
    git_dir: PathBuf,
}

impl ChangeFilter {
    fn is_relevant(&self, repo: &Repository, path: &Path) -> bool {
        if let Ok(relative) = path.strip_prefix(&self.git_dir) {
            return relative == Path::new("index")
                || relative == Path::new("HEAD")
                || relative.starts_with("refs");
        }
        match path.strip_prefix(&self.repo_root) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                !repo.is_path_ignored(relative).unwrap_or(false)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn reports_tracked_edits_but_not_ignored_files() {
        let dir = std::env::temp_dir().join(format!("review_watch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target")).unwrap();
        Repository::init(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        // Canonical path: the watcher reports paths under the real (long) temp path.
        let dir = dir.canonicalize().unwrap();

        let (sender, receiver) = mpsc::channel();
        let _watcher = RepoWatcher::new(&dir, move || sender.send(()).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        std::fs::write(dir.join("target").join("build.log"), "noise").unwrap();
        assert!(
            receiver.recv_timeout(Duration::from_millis(1200)).is_err(),
            "ignored files shouldn't trigger a reload"
        );

        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_ok());

        drop(_watcher);
        std::fs::remove_dir_all(&dir).ok();
    }
}
