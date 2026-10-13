//! Staging and committing through the `git` executable rather than libgit2, so the
//! user's hooks, commit signing, filters and config behave exactly as in a terminal.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

/// Stages the current state of `paths` (additions, modifications and deletions).
pub fn stage(repo_root: &Path, paths: &[&str]) -> Result<()> {
    let mut args = vec!["add", "--all", "--"];
    args.extend_from_slice(paths);
    git(repo_root, &args, None).map(drop)
}

/// Removes `paths` from the index, keeping the working tree as is.
pub fn unstage(repo_root: &Path, paths: &[&str]) -> Result<()> {
    let has_head = git(
        repo_root,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        None,
    )
    .is_ok();
    let mut args = if has_head {
        vec!["restore", "--staged", "--"]
    } else {
        // Before the first commit there's no HEAD to restore from.
        vec!["rm", "--cached", "-r", "--quiet", "--ignore-unmatch", "--"]
    };
    args.extend_from_slice(paths);
    git(repo_root, &args, None).map(drop)
}

/// Commits the index with `message`, running the repository's hooks. Returns the
/// short hash and first line of the new commit.
pub fn commit(repo_root: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        bail!("the commit message is empty");
    }
    git(repo_root, &["commit", "--file=-"], Some(message))?;
    let hash = git(repo_root, &["log", "-1", "--format=%h"], None)?;
    // Hooks (commit-msg) may rewrite the message, so read it back.
    let message = git(repo_root, &["log", "-1", "--format=%B"], None)?;
    let subject = message.lines().next().unwrap_or_default();
    Ok(format!("{} {subject}", hash.trim()))
}

fn git(repo_root: &Path, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo_root)
        // Paths are file names, never globs or pathspec magic.
        .arg("--literal-pathspecs")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .context("couldn't run `git`; is it installed and on the PATH?")?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().context("stdin unavailable")?;
        pipe.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // `git commit` with nothing staged explains why on stdout.
        let message = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("git exited with an error")
            .to_string();
        bail!(message);
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StageState, load_review};

    fn temp_repo(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("review_git_cli_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "test"],
            &["config", "user.email", "test@example.com"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(&dir, args, None).unwrap();
        }
        dir
    }

    fn stage_of(dir: &Path, path: &str) -> StageState {
        let review = load_review(dir, None).unwrap();
        review
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .stage
    }

    #[test]
    fn stages_unstages_and_commits() {
        let dir = temp_repo("flow");
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();

        // Before the first commit.
        stage(&dir, &["a.txt"]).unwrap();
        assert_eq!(stage_of(&dir, "a.txt"), StageState::Staged);
        assert_eq!(stage_of(&dir, "b.txt"), StageState::Unstaged);
        unstage(&dir, &["a.txt"]).unwrap();
        assert_eq!(stage_of(&dir, "a.txt"), StageState::Unstaged);

        stage(&dir, &["a.txt"]).unwrap();
        let summary = commit(&dir, "First commit\n\nBody").unwrap();
        assert!(summary.ends_with(" First commit"), "{summary}");
        let review = load_review(&dir, None).unwrap();
        assert_eq!(
            review
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["b.txt"]
        );

        // Partially staged: staged edit plus a further unstaged edit.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        stage(&dir, &["a.txt"]).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(stage_of(&dir, "a.txt"), StageState::Partial);

        // Deletions stage too.
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        stage(&dir, &["a.txt"]).unwrap();
        assert_eq!(stage_of(&dir, "a.txt"), StageState::Staged);
        unstage(&dir, &["a.txt"]).unwrap();
        assert_eq!(stage_of(&dir, "a.txt"), StageState::Unstaged);

        let error = commit(&dir, "nothing").unwrap_err().to_string();
        assert!(!error.is_empty());
        assert!(commit(&dir, "  ").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
