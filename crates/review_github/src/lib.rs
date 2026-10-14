//! Reviews GitHub pull requests locally: checks a PR out into a worktree of its own,
//! pushes fixes back to its branch and submits reviews with line comments. GitHub is
//! reached through the `gh` CLI, so signing in is `gh auth login`'s business.

mod drafts;

pub use drafts::{DiffSide, DraftComment, load_drafts, save_drafts};

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use review_git::git;
use serde::Deserialize;
use serde_json::{Value, json};

/// A pull request on github.com.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    /// Parses a PR URL (`https://github.com/owner/repo/pull/123`, also with a trailing
    /// `/files` or `#discussion…`, or without the scheme) or `owner/repo#123`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Some((slug, number)) = text.split_once('#')
            && let Some((owner, repo)) = slug.split_once('/')
            && let Some(pr) = Self::new(owner, repo, number)
        {
            return Some(pr);
        }
        let rest = text
            .strip_prefix("https://")
            .or_else(|| text.strip_prefix("http://"))
            .unwrap_or(text);
        let rest = rest.strip_prefix("www.").unwrap_or(rest);
        let rest = rest.strip_prefix("github.com/")?;
        let mut parts = rest.split(['/', '#', '?']);
        let (owner, repo) = (parts.next()?, parts.next()?);
        if parts.next()? != "pull" {
            return None;
        }
        Self::new(owner, repo, parts.next()?)
    }

    fn new(owner: &str, repo: &str, number: &str) -> Option<Self> {
        let valid = |name: &str| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        (valid(owner) && valid(repo)).then_some(())?;
        Some(Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: number.parse().ok().filter(|&number| number > 0)?,
        })
    }

    /// `owner/repo`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// A file-system friendly name for this PR, e.g. `owner-repo-123`.
    fn file_stem(&self) -> String {
        format!("{}-{}-{}", self.owner, self.repo, self.number)
    }

    fn head_ref(&self) -> String {
        format!("refs/galley/pr/{}/head", self.number)
    }

    fn base_ref(&self) -> String {
        format!("refs/galley/pr/{}/base", self.number)
    }
}

/// What the user asked to review: a full reference, or a number in the current repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrTarget {
    Ref(PrRef),
    Number(u64),
}

impl PrTarget {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().trim_start_matches('#').parse() {
            Ok(number) if number > 0 => Some(Self::Number(number)),
            _ => PrRef::parse(text).map(Self::Ref),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub pr: PrRef,
    pub title: String,
    pub url: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
    pub author: String,
    pub head_branch: String,
    pub base_branch: String,
    /// `owner/repo` the head branch lives in (a fork for cross-repository PRs); `None`
    /// when that repository was deleted.
    pub head_repo: Option<String>,
    pub is_cross_repository: bool,
    pub maintainer_can_modify: bool,
}

impl PullRequest {
    /// Why fixes can't be pushed to the PR's branch, if they can't.
    pub fn push_blocker(&self) -> Option<&'static str> {
        if self.state != "OPEN" {
            Some("The pull request isn't open.")
        } else if self.is_cross_repository && self.head_repo.is_none() {
            Some("The fork this pull request comes from was deleted.")
        } else if self.is_cross_repository && !self.maintainer_can_modify {
            Some("The fork doesn't allow edits from maintainers.")
        } else {
            None
        }
    }
}

/// A pull request checked out in a worktree that Galley owns.
#[derive(Debug, Clone)]
pub struct PrCheckout {
    pub pull_request: PullRequest,
    pub worktree: PathBuf,
    /// Remote of the worktree's repository that points at the PR's repository.
    remote: String,
}

impl PrCheckout {
    /// Local ref holding the PR's head as last fetched or pushed.
    pub fn head_ref(&self) -> String {
        self.pull_request.pr.head_ref()
    }

    /// Local ref holding the PR's base branch, to diff against its merge base.
    pub fn base_ref(&self) -> String {
        self.pull_request.pr.base_ref()
    }
}

/// Fetches `target` and checks it out in Galley's worktree for it, creating the
/// worktree (and a clone of the repository, if `local_repo` isn't one) the first time.
/// An existing worktree is moved to the PR's new head only when that loses nothing.
pub fn checkout(
    target: &PrTarget,
    local_repo: &Path,
    progress: &dyn Fn(String),
) -> Result<PrCheckout> {
    progress("Loading the pull request…".into());
    let pull_request = view(target, local_repo)?;
    let pr = &pull_request.pr;
    let worktree = data_dir()?.join("worktrees").join(pr.file_stem());

    // A worktree from an earlier run keeps whatever repository it was made from.
    let (repo, remote) = if is_work_tree(&worktree) {
        remote_for(&worktree, pr).with_context(|| {
            format!(
                "{} has no remote for {}; delete that folder and try again",
                worktree.display(),
                pr.slug()
            )
        })?
    } else if let Some(found) = remote_for(local_repo, pr) {
        found
    } else {
        cached_clone(pr, progress)?
    };

    progress(format!("Fetching #{}…", pr.number));
    fetch(&repo, &remote, &pull_request)?;
    progress("Preparing the worktree…".into());
    update_worktree(&repo, &worktree, pr)?;
    Ok(PrCheckout {
        pull_request,
        worktree,
        remote,
    })
}

const PR_FIELDS: &str = "number,title,url,state,author,headRefName,baseRefName,\
                         headRepository,isCrossRepository,maintainerCanModify";

fn view(target: &PrTarget, local_repo: &Path) -> Result<PullRequest> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Json {
        title: String,
        url: String,
        state: String,
        author: Option<Login>,
        head_ref_name: String,
        base_ref_name: String,
        head_repository: Option<Repository>,
        is_cross_repository: bool,
        maintainer_can_modify: bool,
    }
    #[derive(Deserialize)]
    struct Login {
        login: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Repository {
        name_with_owner: String,
    }

    let output = match target {
        PrTarget::Ref(pr) => gh(
            None,
            &[
                "pr",
                "view",
                &pr.number.to_string(),
                "--repo",
                &pr.slug(),
                "--json",
                PR_FIELDS,
            ],
            None,
        )?,
        // gh works out the repository from the remotes of the one Galley was opened in.
        PrTarget::Number(number) => gh(
            Some(local_repo),
            &["pr", "view", &number.to_string(), "--json", PR_FIELDS],
            None,
        )?,
    };
    let json: Json =
        serde_json::from_str(&output).context("unexpected output from `gh pr view`")?;
    Ok(PullRequest {
        pr: PrRef::parse(&json.url)
            .with_context(|| format!("{} isn't a github.com pull request", json.url))?,
        title: json.title,
        url: json.url,
        state: json.state,
        author: json.author.map(|author| author.login).unwrap_or_default(),
        head_branch: json.head_ref_name,
        base_branch: json.base_ref_name,
        head_repo: json.head_repository.map(|repo| repo.name_with_owner),
        is_cross_repository: json.is_cross_repository,
        maintainer_can_modify: json.maintainer_can_modify,
    })
}

fn is_work_tree(path: &Path) -> bool {
    path.join(".git").exists()
        && git(path, &["rev-parse", "--is-inside-work-tree"], None)
            .is_ok_and(|output| output.trim() == "true")
}

/// The repository at `path` and the name of its remote for `pr`'s repository.
fn remote_for(path: &Path, pr: &PrRef) -> Option<(PathBuf, String)> {
    let urls = git(
        path,
        &["config", "--get-regexp", r"^remote\..*\.url$"],
        None,
    )
    .ok()?;
    let slug = pr.slug();
    urls.lines().find_map(|line| {
        let (key, url) = line.split_once(' ')?;
        let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
        repo_slug(url)
            .is_some_and(|url_slug| url_slug.eq_ignore_ascii_case(&slug))
            .then(|| (path.to_path_buf(), name.to_string()))
    })
}

/// `owner/repo` for a github.com remote URL in any of its HTTPS or SSH spellings.
fn repo_slug(url: &str) -> Option<String> {
    let (_, path) = url.trim().split_once("github.com")?;
    let path = path.strip_prefix([':', '/'])?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| format!("{owner}/{repo}"))
}

/// A bare clone of `pr`'s repository kept by Galley, for PRs opened outside a clone.
fn cached_clone(pr: &PrRef, progress: &dyn Fn(String)) -> Result<(PathBuf, String)> {
    let dir = data_dir()?
        .join("repos")
        .join(&pr.owner)
        .join(format!("{}.git", pr.repo));
    if !dir.join("HEAD").exists() {
        progress(format!("Cloning {}…", pr.slug()));
        std::fs::create_dir_all(dir.parent().context("invalid data directory")?)?;
        let dir_arg = dir.to_string_lossy();
        gh(
            None,
            &["repo", "clone", &pr.slug(), &dir_arg, "--", "--bare"],
            None,
        )?;
    }
    Ok((dir, "origin".to_string()))
}

fn fetch(repo: &Path, remote: &str, pull_request: &PullRequest) -> Result<()> {
    let pr = &pull_request.pr;
    git(
        repo,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            remote,
            &format!("+refs/pull/{}/head:{}", pr.number, pr.head_ref()),
            &format!("+refs/heads/{}:{}", pull_request.base_branch, pr.base_ref()),
        ],
        None,
    )
    .with_context(|| format!("couldn't fetch #{} from {remote}", pr.number))?;
    Ok(())
}

fn update_worktree(repo: &Path, worktree: &Path, pr: &PrRef) -> Result<()> {
    let head_ref = pr.head_ref();
    if is_work_tree(worktree) {
        let clean = git(worktree, &["status", "--porcelain"], None)?
            .trim()
            .is_empty();
        let behind = git(
            worktree,
            &["merge-base", "--is-ancestor", "HEAD", &head_ref],
            None,
        )
        .is_ok();
        // Otherwise the worktree has local work: leave it for the user to push or drop.
        if clean && behind {
            git(
                worktree,
                &["checkout", "--quiet", "--detach", &head_ref],
                None,
            )?;
        }
        return Ok(());
    }
    if worktree.exists() {
        // A worktree whose repository was deleted or moved.
        git(repo, &["worktree", "prune"], None).ok();
        bail!(
            "{} exists but isn't a usable worktree; delete it and try again",
            worktree.display()
        );
    }
    std::fs::create_dir_all(worktree.parent().context("invalid data directory")?)?;
    let worktree_arg = worktree.to_string_lossy();
    git(
        repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &worktree_arg,
            &head_ref,
        ],
        None,
    )?;
    Ok(())
}

/// Commits in the worktree that the PR's branch doesn't have yet.
pub fn unpushed_commits(checkout: &PrCheckout) -> Result<usize> {
    let range = format!("{}..HEAD", checkout.head_ref());
    let count = git(&checkout.worktree, &["rev-list", "--count", &range], None)?;
    count
        .trim()
        .parse()
        .context("unexpected `git rev-list` output")
}

/// Pushes the worktree's commits to the PR's branch (never forcing). Returns a summary.
pub fn push(checkout: &PrCheckout) -> Result<String> {
    let pull_request = &checkout.pull_request;
    if let Some(blocker) = pull_request.push_blocker() {
        bail!(blocker);
    }
    let count = unpushed_commits(checkout)?;
    if count == 0 {
        bail!("there are no new commits to push");
    }
    let target = match (&pull_request.head_repo, pull_request.is_cross_repository) {
        (Some(fork), true) => fork_url(fork),
        _ => checkout.remote.clone(),
    };
    let refspec = format!("HEAD:refs/heads/{}", pull_request.head_branch);
    git(
        &checkout.worktree,
        &["push", "--quiet", &target, &refspec],
        None,
    )?;
    git(
        &checkout.worktree,
        &["update-ref", &checkout.head_ref(), "HEAD"],
        None,
    )?;
    Ok(format!(
        "Pushed {count} commit{} to {}",
        if count == 1 { "" } else { "s" },
        pull_request.head_branch
    ))
}

/// The fork's URL in the protocol the user set up gh with.
fn fork_url(slug: &str) -> String {
    let protocol = gh(
        None,
        &["config", "get", "git_protocol", "-h", "github.com"],
        None,
    )
    .unwrap_or_default();
    if protocol.trim() == "ssh" {
        format!("git@github.com:{slug}.git")
    } else {
        format!("https://github.com/{slug}.git")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    fn api_name(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// Submits a review on the PR's head as last fetched or pushed. Returns its URL.
pub fn submit_review(
    checkout: &PrCheckout,
    event: ReviewEvent,
    body: &str,
    comments: &[DraftComment],
) -> Result<String> {
    if event != ReviewEvent::Approve && body.trim().is_empty() && comments.is_empty() {
        bail!("write a summary or add line comments first");
    }
    let commit_id = git(
        &checkout.worktree,
        &["rev-parse", &checkout.head_ref()],
        None,
    )?;
    let payload = review_payload(commit_id.trim(), event, body, comments);
    let pr = &checkout.pull_request.pr;
    let endpoint = format!("repos/{}/pulls/{}/reviews", pr.slug(), pr.number);
    let output = gh(
        None,
        &["api", "--method", "POST", &endpoint, "--input", "-"],
        Some(&payload.to_string()),
    )?;
    let response: Value = serde_json::from_str(&output).unwrap_or_default();
    Ok(response["html_url"]
        .as_str()
        .unwrap_or(&checkout.pull_request.url)
        .to_string())
}

fn review_payload(
    commit_id: &str,
    event: ReviewEvent,
    body: &str,
    comments: &[DraftComment],
) -> Value {
    let comments = comments
        .iter()
        .map(|comment| {
            let mut value = json!({
                "path": comment.path,
                "body": comment.body,
                "line": comment.line,
                "side": comment.side,
            });
            if comment.start_line < comment.line {
                value["start_line"] = json!(comment.start_line);
                value["start_side"] = json!(comment.side);
            }
            value
        })
        .collect::<Vec<_>>();
    json!({
        "commit_id": commit_id,
        "event": event.api_name(),
        "body": body.trim(),
        "comments": comments,
    })
}

/// Where Galley keeps clones, worktrees and drafts: `%LOCALAPPDATA%\galley` on Windows.
fn data_dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    };
    Ok(base
        .context("couldn't find the local data directory")?
        .join("galley"))
}

fn gh(cwd: Option<&Path>, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn().context(
        "couldn't run `gh`; install the GitHub CLI (https://cli.github.com) and run `gh auth login`",
    )?;
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().context("stdin unavailable")?;
        pipe.write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut message = stderr.trim().to_string();
        if let Some(details) = api_error_details(&stdout) {
            message = format!("{message}: {details}");
        }
        if message.is_empty() {
            message = "gh exited with an error".to_string();
        }
        bail!(message);
    }
    Ok(stdout)
}

/// The specific reasons in a GitHub API error response, e.g. why a comment was
/// rejected, which `gh api` prints to stdout.
fn api_error_details(stdout: &str) -> Option<String> {
    let response: Value = serde_json::from_str(stdout).ok()?;
    let details = response["errors"]
        .as_array()?
        .iter()
        .filter_map(|error| match error {
            Value::String(text) => Some(text.clone()),
            _ => error["message"].as_str().map(str::to_string),
        })
        .collect::<Vec<_>>();
    (!details.is_empty()).then(|| details.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(owner: &str, repo: &str, number: u64) -> PrRef {
        PrRef {
            owner: owner.into(),
            repo: repo.into(),
            number,
        }
    }

    #[test]
    fn parses_pull_request_references() {
        let expected = Some(pr("zed-industries", "zed", 123));
        for text in [
            "https://github.com/zed-industries/zed/pull/123",
            "https://github.com/zed-industries/zed/pull/123/files",
            "https://www.github.com/zed-industries/zed/pull/123#discussion_r1",
            "github.com/zed-industries/zed/pull/123?w=1",
            " zed-industries/zed#123 ",
        ] {
            assert_eq!(PrRef::parse(text), expected, "{text}");
        }
        for text in [
            "https://github.com/zed-industries/zed",
            "https://github.com/zed-industries/zed/issues/123",
            "https://gitlab.com/a/b/pull/1",
            "https://github.com/a/b/pull/x",
            "https://github.com/a/b/pull/0",
            "a b/c#1",
            "C:\\repos\\app",
        ] {
            assert_eq!(PrRef::parse(text), None, "{text}");
        }
        assert_eq!(PrTarget::parse("42"), Some(PrTarget::Number(42)));
        assert_eq!(PrTarget::parse("#42"), Some(PrTarget::Number(42)));
        assert_eq!(
            PrTarget::parse("a/b#42"),
            Some(PrTarget::Ref(pr("a", "b", 42)))
        );
        assert_eq!(PrTarget::parse("main"), None);
    }

    #[test]
    fn reads_slugs_from_remote_urls() {
        for url in [
            "https://github.com/Owner/Repo.git",
            "https://github.com/Owner/Repo",
            "https://user@github.com/Owner/Repo/",
            "git@github.com:Owner/Repo.git",
            "ssh://git@github.com/Owner/Repo.git",
        ] {
            assert_eq!(repo_slug(url).as_deref(), Some("Owner/Repo"), "{url}");
        }
        assert_eq!(repo_slug("https://gitlab.com/Owner/Repo.git"), None);
        assert_eq!(repo_slug("https://github.com/Owner"), None);
    }

    #[test]
    fn builds_review_payloads() {
        let comments = [
            DraftComment {
                path: "src/a.rs".into(),
                side: DiffSide::Right,
                start_line: 7,
                line: 9,
                body: "Rename this".into(),
                excerpt: String::new(),
            },
            DraftComment {
                path: "src/b.rs".into(),
                side: DiffSide::Left,
                start_line: 3,
                line: 3,
                body: "Why remove it?".into(),
                excerpt: String::new(),
            },
        ];
        let payload = review_payload("abc", ReviewEvent::RequestChanges, " Summary ", &comments);
        assert_eq!(
            payload,
            json!({
                "commit_id": "abc",
                "event": "REQUEST_CHANGES",
                "body": "Summary",
                "comments": [
                    {"path": "src/a.rs", "body": "Rename this", "line": 9, "side": "RIGHT",
                     "start_line": 7, "start_side": "RIGHT"},
                    {"path": "src/b.rs", "body": "Why remove it?", "line": 3, "side": "LEFT"},
                ],
            })
        );
    }

    #[test]
    fn explains_api_errors() {
        let stdout = r#"{"message":"Unprocessable Entity","errors":["Line could not be resolved",{"message":"Pull request review thread line must be part of the diff"}],"status":"422"}"#;
        assert_eq!(
            api_error_details(stdout).as_deref(),
            Some(
                "Line could not be resolved; Pull request review thread line must be part of the diff"
            )
        );
        assert_eq!(api_error_details("not json"), None);
    }

    /// The fetch, worktree and push steps against a local repository standing in for
    /// GitHub, with the PR's head at `refs/pull/7/head` as GitHub publishes it.
    #[test]
    fn checks_out_updates_and_pushes_a_pull_request() {
        let root = std::env::temp_dir().join(format!("review_github_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (origin, clone, worktree) = (root.join("origin"), root.join("clone"), root.join("wt"));
        std::fs::create_dir_all(&origin).unwrap();
        let run = |dir: &Path, args: &[&str]| git(dir, args, None).unwrap();
        let commit = |dir: &Path, file: &str, text: &str| {
            std::fs::write(dir.join(file), text).unwrap();
            run(dir, &["add", "--all"]);
            run(dir, &["commit", "-q", "-m", file]);
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "test"],
            &["config", "user.email", "test@example.com"],
            &["config", "commit.gpgsign", "false"],
            &["config", "receive.denyCurrentBranch", "ignore"],
        ] {
            run(&origin, args);
        }
        commit(&origin, "a.txt", "a\n");
        run(&origin, &["checkout", "-q", "-b", "feature"]);
        commit(&origin, "b.txt", "b\n");
        run(&origin, &["update-ref", "refs/pull/7/head", "feature"]);
        run(&origin, &["checkout", "-q", "main"]);

        let origin_arg = origin.to_string_lossy().to_string();
        let clone_arg = clone.to_string_lossy().to_string();
        run(&root, &["clone", "-q", &origin_arg, &clone_arg]);
        for args in [
            &["config", "user.name", "test"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "commit.gpgsign", "false"],
        ] {
            run(&clone, args);
        }

        let pull_request = PullRequest {
            pr: pr("me", "app", 7),
            title: "Add b".into(),
            url: "https://github.com/me/app/pull/7".into(),
            state: "OPEN".into(),
            author: "me".into(),
            head_branch: "feature".into(),
            base_branch: "main".into(),
            head_repo: Some("me/app".into()),
            is_cross_repository: false,
            maintainer_can_modify: false,
        };
        fetch(&clone, "origin", &pull_request).unwrap();
        update_worktree(&clone, &worktree, &pull_request.pr).unwrap();
        assert!(worktree.join("b.txt").exists());
        let checkout = PrCheckout {
            pull_request,
            worktree: worktree.clone(),
            remote: "origin".into(),
        };
        assert_eq!(unpushed_commits(&checkout).unwrap(), 0);
        assert!(push(&checkout).is_err());

        // A fix made in the worktree goes to the PR's branch.
        commit(&worktree, "c.txt", "c\n");
        assert_eq!(unpushed_commits(&checkout).unwrap(), 1);
        assert_eq!(push(&checkout).unwrap(), "Pushed 1 commit to feature");
        assert_eq!(unpushed_commits(&checkout).unwrap(), 0);
        let rev = |dir: &Path, rev: &str| run(dir, &["rev-parse", rev]);
        assert_eq!(rev(&origin, "feature"), rev(&worktree, "HEAD"));

        // New commits on the PR move a clean worktree along...
        run(&origin, &["checkout", "-q", "feature"]);
        run(&origin, &["reset", "-q", "--hard"]);
        commit(&origin, "d.txt", "d\n");
        run(&origin, &["update-ref", "refs/pull/7/head", "feature"]);
        fetch(&clone, "origin", &checkout.pull_request).unwrap();
        update_worktree(&clone, &worktree, &checkout.pull_request.pr).unwrap();
        assert_eq!(rev(&origin, "feature"), rev(&worktree, "HEAD"));

        // ...but never discard local changes.
        std::fs::write(worktree.join("d.txt"), "edited\n").unwrap();
        commit(&origin, "e.txt", "e\n");
        run(&origin, &["update-ref", "refs/pull/7/head", "feature"]);
        fetch(&clone, "origin", &checkout.pull_request).unwrap();
        update_worktree(&clone, &worktree, &checkout.pull_request.pr).unwrap();
        assert_ne!(rev(&origin, "feature"), rev(&worktree, "HEAD"));
        assert_eq!(
            std::fs::read_to_string(worktree.join("d.txt")).unwrap(),
            "edited\n"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
