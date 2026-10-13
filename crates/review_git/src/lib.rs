//! Loads the changes to review from a git repository: the working tree (including
//! staged and untracked files) compared against `HEAD`, or against the merge-base
//! with another ref when reviewing a whole branch.

mod git_cli;
mod watch;

pub use git_cli::{commit, stage, unstage};
pub use watch::RepoWatcher;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use git2::{Delta, DiffFindOptions, DiffOptions, Oid, Patch, Repository, Status, StatusOptions};

/// Files bigger than this are listed but their contents aren't loaded.
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Review {
    pub repo_root: PathBuf,
    /// The checked out branch, or `detached at <id>` when `HEAD` points at a commit.
    pub head_label: Option<String>,
    /// Human readable description of what the working tree is compared against.
    pub base_label: String,
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    TypeChanged,
}

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
            FileStatus::Untracked => 'U',
            FileStatus::TypeChanged => 'T',
        }
    }
}

/// Whether a file's changes are in the index, relative to `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    /// Changes only in the working tree.
    Unstaged,
    /// All changes are staged.
    Staged,
    /// Some changes staged, more in the working tree.
    Partial,
    /// No uncommitted changes: the file differs from the base only through commits.
    Committed,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Path relative to the repository root, using `/` separators.
    pub path: String,
    /// Previous path, only for renames.
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub stage: StageState,
    pub binary: bool,
    pub additions: usize,
    pub deletions: usize,
    pub hunks: Vec<Hunk>,
    /// Full contents of the old side, used to syntax highlight removed lines.
    pub old_text: Option<String>,
    /// Full contents of the new side, used to syntax highlight added and context lines.
    pub new_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number on the old side.
    pub old_lineno: Option<u32>,
    /// 1-based line number on the new side.
    pub new_lineno: Option<u32>,
    /// Line contents without the trailing newline.
    pub text: String,
}

/// Loads the review for the repository containing `path`.
///
/// With `base == None` the working tree is compared against `HEAD`. With a ref, it's
/// compared against the merge-base of `HEAD` and that ref, which is what a pull
/// request from the current branch would show (plus any uncommitted work).
pub fn load_review(path: &Path, base: Option<&str>) -> Result<Review> {
    let repo = Repository::discover(path)
        .with_context(|| format!("no git repository found at {}", path.display()))?;
    let repo_root = repo
        .workdir()
        .context("bare repositories aren't supported")?
        .to_path_buf();

    let (base_tree, base_label) = match base {
        Some(base_ref) => {
            let head = repo.head()?.peel_to_commit()?;
            let other = repo
                .revparse_single(base_ref)
                .with_context(|| format!("couldn't find ref '{base_ref}'"))?
                .peel_to_commit()?;
            let merge_base = repo.merge_base(head.id(), other.id())?;
            let tree = repo.find_commit(merge_base)?.tree()?;
            (
                Some(tree),
                format!("{base_ref} (merge-base {})", short_id(merge_base)),
            )
        }
        // An unborn HEAD (fresh repo without commits) diffs against the empty tree.
        None => match repo.head() {
            Ok(head) => {
                let commit = head.peel_to_commit()?;
                (
                    Some(commit.tree()?),
                    format!("HEAD ({})", short_id(commit.id())),
                )
            }
            Err(_) => (None, "repository without commits".to_string()),
        },
    };

    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .context_lines(3);
    let mut diff = repo.diff_tree_to_workdir_with_index(base_tree.as_ref(), Some(&mut options))?;
    diff.find_similar(Some(
        DiffFindOptions::new().renames(true).for_untracked(true),
    ))?;

    let mut files = Vec::new();
    for index in 0..diff.deltas().len() {
        let Some(patch) = Patch::from_diff(&diff, index)? else {
            continue;
        };
        files.push(file_diff(&repo, &repo_root, &patch)?);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let stage_flags = stage_flags(&repo)?;
    for file in &mut files {
        let (staged, unstaged) = [Some(&file.path), file.old_path.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|path| stage_flags.get(path))
            .fold((false, false), |(staged, unstaged), (s, u)| {
                (staged || *s, unstaged || *u)
            });
        file.stage = match (staged, unstaged) {
            (true, true) => StageState::Partial,
            (true, false) => StageState::Staged,
            (false, true) => StageState::Unstaged,
            (false, false) => StageState::Committed,
        };
    }

    Ok(Review {
        head_label: head_label(&repo),
        repo_root,
        base_label,
        files,
    })
}

/// Reads `HEAD` directly instead of through `repo.head()`, which fails on an unborn
/// branch (a fresh repo still has one checked out).
fn head_label(repo: &Repository) -> Option<String> {
    let head = repo.find_reference("HEAD").ok()?;
    // Errors only for a branch name that isn't valid UTF-8.
    match head.symbolic_target().ok()? {
        Some(target) => Some(
            target
                .strip_prefix("refs/heads/")
                .unwrap_or(target)
                .to_string(),
        ),
        None => head
            .target()
            .map(|oid| format!("detached at {}", short_id(oid))),
    }
}

/// For each path with uncommitted changes: (has staged changes, has unstaged changes).
fn stage_flags(repo: &Repository) -> Result<HashMap<String, (bool, bool)>> {
    let staged = Status::INDEX_NEW
        | Status::INDEX_MODIFIED
        | Status::INDEX_DELETED
        | Status::INDEX_RENAMED
        | Status::INDEX_TYPECHANGE;
    let unstaged = Status::WT_NEW
        | Status::WT_MODIFIED
        | Status::WT_DELETED
        | Status::WT_RENAMED
        | Status::WT_TYPECHANGE;
    let mut options = StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo.statuses(Some(&mut options))?;
    Ok(statuses
        .iter()
        .filter_map(|entry| {
            let status = entry.status();
            let path = entry.path().ok()?.to_string();
            Some((
                path,
                (status.intersects(staged), status.intersects(unstaged)),
            ))
        })
        .collect())
}

fn file_diff(repo: &Repository, repo_root: &Path, patch: &Patch) -> Result<FileDiff> {
    let delta = patch.delta();
    let status = match delta.status() {
        Delta::Added => FileStatus::Added,
        Delta::Deleted => FileStatus::Deleted,
        Delta::Renamed | Delta::Copied => FileStatus::Renamed,
        Delta::Untracked => FileStatus::Untracked,
        Delta::Typechange => FileStatus::TypeChanged,
        _ => FileStatus::Modified,
    };
    let new_path = delta.new_file().path().map(path_to_string);
    let old_path = delta.old_file().path().map(path_to_string);
    let path = new_path.clone().or(old_path.clone()).unwrap_or_default();

    let old_text = if delta.old_file().id().is_zero() {
        None
    } else {
        repo.find_blob(delta.old_file().id())
            .ok()
            .and_then(|blob| decode_text(blob.content()))
    };
    let new_text = match (&new_path, status) {
        (_, FileStatus::Deleted) | (None, _) => None,
        (Some(new_path), _) => std::fs::read(repo_root.join(new_path))
            .ok()
            .and_then(|bytes| decode_text(&bytes)),
    };
    let binary = delta.flags().is_binary()
        || (old_text.is_none() && new_text.is_none() && status != FileStatus::Deleted);

    let mut hunks = Vec::with_capacity(patch.num_hunks());
    let (mut additions, mut deletions) = (0, 0);
    for hunk_index in 0..patch.num_hunks() {
        let (hunk, line_count) = patch.hunk(hunk_index)?;
        let mut lines = Vec::with_capacity(line_count);
        for line_index in 0..line_count {
            let line = patch.line_in_hunk(hunk_index, line_index)?;
            let kind = match line.origin() {
                ' ' => LineKind::Context,
                '+' => LineKind::Added,
                '-' => LineKind::Removed,
                // "\ No newline at end of file" markers and similar.
                _ => continue,
            };
            match kind {
                LineKind::Added => additions += 1,
                LineKind::Removed => deletions += 1,
                LineKind::Context => {}
            }
            let text = String::from_utf8_lossy(line.content());
            lines.push(DiffLine {
                kind,
                old_lineno: line.old_lineno(),
                new_lineno: line.new_lineno(),
                text: text.trim_end_matches(['\n', '\r']).to_string(),
            });
        }
        hunks.push(Hunk {
            header: String::from_utf8_lossy(hunk.header())
                .trim_end()
                .to_string(),
            lines,
        });
    }

    Ok(FileDiff {
        old_path: (status == FileStatus::Renamed)
            .then_some(old_path)
            .flatten(),
        path,
        status,
        // Filled in by `load_review` from the repository status.
        stage: StageState::Committed,
        binary,
        additions,
        deletions,
        hunks,
        old_text,
        new_text,
    })
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_TEXT_BYTES || bytes.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn short_id(oid: Oid) -> String {
    oid.to_string()[..7].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;

    fn commit_all(repo: &Repository, message: &str) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("test", "test@example.com").unwrap();
        let parents = repo
            .head()
            .ok()
            .map(|head| head.peel_to_commit().unwrap())
            .into_iter()
            .collect::<Vec<_>>();
        let parents = parents.iter().collect::<Vec<_>>();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )
        .unwrap();
    }

    #[test]
    fn reports_modified_added_and_untracked_files() {
        let dir = std::env::temp_dir().join(format!("review_git_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();

        std::fs::write(dir.join("main.rs"), "fn main() {\n    a();\n}\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        commit_all(&repo, "initial");

        std::fs::write(dir.join("main.rs"), "fn main() {\n    b();\n}\n").unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::write(dir.join("new.txt"), "hello\n").unwrap();

        let review = load_review(&dir, None).unwrap();
        let summary = review
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.additions, f.deletions))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("gone.txt", FileStatus::Deleted, 0, 1),
                ("main.rs", FileStatus::Modified, 1, 1),
                ("new.txt", FileStatus::Untracked, 1, 0),
            ]
        );

        let main = &review.files[1];
        let changed = main.hunks[0]
            .lines
            .iter()
            .filter(|line| line.kind != LineKind::Context)
            .map(|line| {
                (
                    line.kind,
                    line.old_lineno,
                    line.new_lineno,
                    line.text.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            changed,
            [
                (LineKind::Removed, Some(2), None, "    a();"),
                (LineKind::Added, None, Some(2), "    b();"),
            ]
        );
        assert!(main.old_text.as_deref().unwrap().contains("a();"));
        assert!(main.new_text.as_deref().unwrap().contains("b();"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn labels_branch_unborn_branch_and_detached_head() {
        let dir = std::env::temp_dir().join(format!("review_git_head_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        repo.set_head("refs/heads/feature/x").unwrap();

        let label = || load_review(&dir, None).unwrap().head_label;
        assert_eq!(label().as_deref(), Some("feature/x"));

        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "initial");
        assert_eq!(label().as_deref(), Some("feature/x"));

        let id = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(id).unwrap();
        assert_eq!(label(), Some(format!("detached at {}", short_id(id))));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
