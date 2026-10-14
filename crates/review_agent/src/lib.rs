//! Runs Claude Code headless (`claude -p --output-format stream-json`) and turns its
//! JSON-lines stream into typed events. Follow-up messages resume the same session,
//! so a review thread keeps its conversation context.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use serde_json::Value;

pub mod permissions;

/// What the agent is asked to do in one turn of a thread.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub cwd: PathBuf,
    pub prompt: String,
    /// Session to continue; `None` starts a new conversation.
    pub resume_session: Option<String>,
    /// Extra pre-approved permission rules (`--allowedTools`), e.g. `Bash(cargo test:*)`.
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    SessionStarted {
        session_id: String,
    },
    /// A new block of assistant prose begins.
    TextStart,
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        detail: String,
    },
    ToolResult {
        id: String,
        is_error: bool,
    },
    Finished {
        is_error: bool,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
        /// Shell commands the agent tried but wasn't allowed to run.
        denied_commands: Vec<String>,
    },
    /// The process couldn't run or exited without a result.
    Failed(String),
}

/// A running agent turn. Dropping it doesn't stop the process; call [`AgentRun::cancel`].
pub struct AgentRun {
    child: Arc<Mutex<Option<Child>>>,
}

impl AgentRun {
    /// Spawns Claude Code and streams its events to `on_event` from a background
    /// thread. The last event is always [`AgentEvent::Finished`] or [`AgentEvent::Failed`].
    pub fn start(
        request: AgentRequest,
        mut on_event: impl FnMut(AgentEvent) + Send + 'static,
    ) -> Result<Self> {
        let mut command = Command::new("claude");
        command
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            .arg("--include-partial-messages")
            // The reviewer sees every edit as a diff right away, so let the agent edit
            // files in the repo without asking. Other tools (shell, network) need a rule
            // in `allowed_tools` or the user's Claude Code settings, or they're denied.
            .args(["--permission-mode", "acceptEdits"])
            .current_dir(&request.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session) = &request.resume_session {
            command.args(["--resume", session]);
        }
        // Variadic flag: keep it last.
        if !request.allowed_tools.is_empty() {
            command.arg("--allowedTools").args(&request.allowed_tools);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .context("couldn't run `claude`; is Claude Code installed and on the PATH?")?;
        // The prompt goes through stdin to avoid command-line length and quoting limits.
        let mut stdin = child.stdin.take().context("stdin unavailable")?;
        stdin.write_all(request.prompt.as_bytes())?;
        drop(stdin);
        let stdout = child.stdout.take().context("stdout unavailable")?;
        let mut stderr = child.stderr.take().context("stderr unavailable")?;

        let child = Arc::new(Mutex::new(Some(child)));
        let reader_child = child.clone();
        std::thread::spawn(move || {
            let stderr_reader = std::thread::spawn(move || {
                let mut output = String::new();
                stderr.read_to_string(&mut output).ok();
                output
            });

            let mut finished = false;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                for event in parse_line(&line, &request.cwd) {
                    finished |= matches!(event, AgentEvent::Finished { .. });
                    on_event(event);
                }
            }

            let status = reader_child
                .lock()
                .ok()
                .and_then(|mut child| child.take())
                .and_then(|mut child| child.wait().ok());
            let stderr = stderr_reader.join().unwrap_or_default();
            if !finished {
                let message = match status {
                    None => "cancelled".to_string(),
                    Some(status) if !stderr.trim().is_empty() => {
                        format!("{status}: {}", stderr.trim())
                    }
                    Some(status) => format!("claude exited without a result ({status})"),
                };
                on_event(AgentEvent::Failed(message));
            }
        });

        Ok(Self { child })
    }

    pub fn cancel(&self) {
        if let Ok(mut child) = self.child.lock()
            && let Some(mut child) = child.take()
        {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

/// Parses one line of `stream-json` output. Lines from subagents and event types we
/// don't display yield no events. File paths under `cwd` are made relative to it.
pub fn parse_line(line: &str, cwd: &Path) -> Vec<AgentEvent> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if !value["parent_tool_use_id"].is_null() {
        return Vec::new();
    }
    let str_of = |value: &Value| value.as_str().unwrap_or_default().to_string();

    match value["type"].as_str() {
        Some("system") if value["subtype"] == "init" => vec![AgentEvent::SessionStarted {
            session_id: str_of(&value["session_id"]),
        }],
        Some("stream_event") => {
            let event = &value["event"];
            match event["type"].as_str() {
                Some("content_block_start") if event["content_block"]["type"] == "text" => {
                    vec![AgentEvent::TextStart]
                }
                Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                    vec![AgentEvent::TextDelta(str_of(&event["delta"]["text"]))]
                }
                _ => Vec::new(),
            }
        }
        Some("assistant") => content_blocks(&value)
            .filter(|block| block["type"] == "tool_use")
            .map(|block| AgentEvent::ToolUse {
                id: str_of(&block["id"]),
                name: str_of(&block["name"]),
                detail: tool_detail(&block["input"], cwd),
            })
            .collect(),
        Some("user") => content_blocks(&value)
            .filter(|block| block["type"] == "tool_result")
            .map(|block| AgentEvent::ToolResult {
                id: str_of(&block["tool_use_id"]),
                is_error: block["is_error"].as_bool().unwrap_or(false),
            })
            .collect(),
        Some("result") => {
            let mut denied_commands = Vec::new();
            for denial in value["permission_denials"].as_array().into_iter().flatten() {
                if denial["tool_name"] == "Bash"
                    && let Some(command) = denial["tool_input"]["command"].as_str()
                    && !denied_commands.iter().any(|denied| denied == command)
                {
                    denied_commands.push(command.to_string());
                }
            }
            vec![AgentEvent::Finished {
                is_error: value["is_error"].as_bool().unwrap_or(false)
                    || value["subtype"] != "success",
                cost_usd: value["total_cost_usd"].as_f64(),
                duration_ms: value["duration_ms"].as_u64(),
                denied_commands,
            }]
        }
        _ => Vec::new(),
    }
}

fn content_blocks(value: &Value) -> impl Iterator<Item = &Value> {
    value["message"]["content"].as_array().into_iter().flatten()
}

/// A one-line summary of a tool call's input, e.g. the file it edits.
fn tool_detail(input: &Value, cwd: &Path) -> String {
    let detail = match ["file_path", "notebook_path"]
        .iter()
        .find_map(|key| input[key].as_str())
    {
        Some(path) => relative_path(path, cwd),
        None => ["command", "pattern", "url", "description"]
            .iter()
            .find_map(|key| input[key].as_str())
            .unwrap_or_default()
            .to_string(),
    };
    let first_line = detail.lines().next().unwrap_or_default();
    match first_line.char_indices().nth(120) {
        Some((index, _)) => format!("{}…", &first_line[..index]),
        None => first_line.to_string(),
    }
}

/// `path` relative to `base` with `/` separators, or unchanged when it's outside `base`.
fn relative_path(path: &str, base: &Path) -> String {
    let normalize = |path: &str| path.replace('\\', "/");
    let path = normalize(path);
    let base = normalize(&base.to_string_lossy());
    let base = base.trim_end_matches('/');
    // Windows paths are case-insensitive and tools disagree on drive letter case.
    let matches = path.len() > base.len()
        && path.is_char_boundary(base.len())
        && path[..base.len()].eq_ignore_ascii_case(base)
        && path[base.len()..].starts_with('/');
    if matches {
        path[base.len() + 1..].to_string()
    } else {
        path
    }
}

/// The code a review thread is about.
#[derive(Debug, Clone)]
pub struct ReviewAnchor {
    pub path: String,
    /// Human readable line range, e.g. "lines 7–13"; `None` for the whole file.
    pub lines: Option<String>,
    /// Diff excerpt with `+`, `-` and ` ` prefixes.
    pub excerpt: String,
}

/// Builds the first message of a thread: the reviewer's question plus the code it's about.
pub fn review_prompt(anchor: &ReviewAnchor, question: &str, repo_root: &Path) -> String {
    let location = match &anchor.lines {
        Some(lines) => format!("`{}` ({lines})", anchor.path),
        None => format!("`{}`", anchor.path),
    };
    let mut prompt = format!(
        "I'm reviewing uncommitted changes in the repository `{}`.\n\
         My comment is about {location}.\n",
        repo_root.display()
    );
    if !anchor.excerpt.is_empty() {
        prompt.push_str("\nDiff excerpt:\n```diff\n");
        prompt.push_str(&anchor.excerpt);
        if !anchor.excerpt.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("```\n");
    }
    prompt.push_str("\nReviewer's comment:\n");
    prompt.push_str(question.trim());
    prompt.push_str(
        "\n\nIf the code needs to change, edit the files directly; \
         the reviewer will see your changes as a diff. Reply concisely, in the same \
         language as the reviewer's comment.",
    );
    prompt
}

/// Staged diffs longer than this are cut; the `--stat` summary at the top still lists
/// every file.
const MAX_COMMIT_DIFF_CHARS: usize = 60_000;

/// Builds the prompt asking for a commit message for `staged_diff`, written in the
/// style of the repository's `recent_subjects`.
pub fn commit_message_prompt(staged_diff: &str, recent_subjects: &[String]) -> String {
    let mut prompt = String::from(
        "Write a git commit message for the staged changes below.\n\
         - First line: a summary of at most 72 characters, in the imperative mood.\n\
         - If the change needs explaining, add a blank line and a short body saying \
         what changed and why, wrapped at 72 characters.\n\
         - Reply with the message only: no quotes, no code fences, no commentary.\n",
    );
    if !recent_subjects.is_empty() {
        prompt.push_str(
            "\nMatch the language and style of the repository's recent commit subjects \
             below. Use a prefix such as `feat:` or `fix(scope):` only if they do.\n",
        );
        for subject in recent_subjects {
            prompt.push_str(&format!("- {subject}\n"));
        }
    }
    prompt.push_str("\nStaged changes:\n```diff\n");
    match staged_diff.char_indices().nth(MAX_COMMIT_DIFF_CHARS) {
        Some((index, _)) => {
            prompt.push_str(&staged_diff[..index]);
            prompt.push_str("\n[diff truncated]\n");
        }
        None => prompt.push_str(staged_diff),
    }
    if !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("```\n");
    prompt
}

/// Asks Claude Code for a commit message, with no tools: everything it needs is in
/// `prompt`. Blocks until it answers; uses a small, fast model.
pub fn generate_commit_message(cwd: &Path, prompt: &str) -> Result<String> {
    let mut command = Command::new("claude");
    command
        .args(["-p", "--output-format", "json", "--no-session-persistence"])
        .args(["--model", "haiku", "--tools", ""])
        .current_dir(cwd)
        .stdin(Stdio::piped())
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
        .context("couldn't run `claude`; is Claude Code installed and on the PATH?")?;
    let mut stdin = child.stdin.take().context("stdin unavailable")?;
    stdin.write_all(prompt.as_bytes())?;
    drop(stdin);
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result = parse_print_result(&stdout);
    if !output.status.success() && result.is_err() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude exited with {}: {}", output.status, stderr.trim());
    }
    result
}

/// The answer in `claude -p --output-format json` output, cleaned up as a message.
fn parse_print_result(stdout: &str) -> Result<String> {
    let value: Value =
        serde_json::from_str(stdout.trim()).context("unexpected output from claude")?;
    let result = value["result"].as_str().unwrap_or_default();
    if value["is_error"].as_bool().unwrap_or(false) {
        anyhow::bail!(
            "{}",
            if result.is_empty() {
                "claude finished with an error"
            } else {
                result
            }
        );
    }
    let message = result.trim();
    // Models sometimes fence the message despite being asked not to.
    let message = match message.strip_prefix("```") {
        Some(fenced) => fenced
            .split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or_default()
            .trim_end()
            .trim_end_matches("```")
            .trim(),
        None => message,
    };
    if message.is_empty() {
        anyhow::bail!("claude returned an empty message");
    }
    Ok(wrap_body(message))
}

/// Rewraps the body of a commit message at 72 columns, which models don't reliably
/// do. The subject line is left alone; paragraphs and list items are reflowed, items
/// with a hanging indent.
fn wrap_body(message: &str) -> String {
    let mut lines = message.lines();
    let mut wrapped = lines.next().unwrap_or_default().to_string();
    // Paragraphs and list items, each as (is a list item, words); `None` for a blank line.
    let mut blocks: Vec<Option<(bool, String)>> = Vec::new();
    for line in lines {
        let line = line.trim();
        let item = line.starts_with("- ") || line.starts_with("* ");
        match blocks.last_mut() {
            _ if line.is_empty() => blocks.push(None),
            Some(Some((_, text))) if !item => {
                text.push(' ');
                text.push_str(line);
            }
            _ => blocks.push(Some((item, line.to_string()))),
        }
    }
    for block in blocks {
        wrapped.push('\n');
        if let Some((item, text)) = block {
            wrapped.push_str(&fill(&text, if item { "  " } else { "" }));
        }
    }
    wrapped
}

/// Greedy fill of `text` at 72 columns, indenting continuation lines by `hanging`.
fn fill(text: &str, hanging: &str) -> String {
    const WIDTH: usize = 72;
    let mut filled = String::new();
    let mut width = 0;
    for word in text.split_whitespace() {
        let word_width = word.chars().count();
        if width == 0 {
        } else if width + 1 + word_width > WIDTH {
            filled.push('\n');
            filled.push_str(hanging);
            width = hanging.len();
        } else {
            filled.push(' ');
            width += 1;
        }
        filled.push_str(word);
        width += word_width;
    }
    filled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_json_events() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"abc","tools":[]}"#,
            r#"{"type":"system","subtype":"hook_started","session_id":"abc"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ho"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hola"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"c:\\repo\\src\\a.rs","old_string":"x"}}]},"parent_tool_use_id":null}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t9","name":"Read","input":{}}]},"parent_tool_use_id":"task1"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hola","total_cost_usd":0.08,"duration_ms":2201,"permission_denials":[{"tool_name":"Bash","tool_use_id":"t2","tool_input":{"command":"cargo test -p x"}},{"tool_name":"WebFetch","tool_use_id":"t3","tool_input":{"url":"https://x"}}]}"#,
        ];
        let cwd = Path::new("C:\\repo");
        let events = lines
            .iter()
            .flat_map(|line| parse_line(line, cwd))
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            [
                AgentEvent::SessionStarted {
                    session_id: "abc".into()
                },
                AgentEvent::TextStart,
                AgentEvent::TextDelta("ho".into()),
                AgentEvent::ToolUse {
                    id: "t1".into(),
                    name: "Edit".into(),
                    detail: "src/a.rs".into()
                },
                AgentEvent::ToolResult {
                    id: "t1".into(),
                    is_error: false
                },
                AgentEvent::Finished {
                    is_error: false,
                    cost_usd: Some(0.08),
                    duration_ms: Some(2201),
                    denied_commands: vec!["cargo test -p x".into()],
                },
            ]
        );
    }

    #[test]
    fn relativizes_paths_inside_the_repo_only() {
        let base = Path::new("C:/Users/dev/repo/");
        assert_eq!(
            relative_path("C:\\Users\\dev\\repo\\web\\api.ts", base),
            "web/api.ts"
        );
        assert_eq!(relative_path("c:/users/dev/repo/a.rs", base), "a.rs");
        assert_eq!(
            relative_path("C:/Users/dev/repo2/a.rs", base),
            "C:/Users/dev/repo2/a.rs"
        );
        assert_eq!(relative_path("/tmp/x", base), "/tmp/x");
    }

    #[test]
    fn prompt_includes_location_excerpt_and_question() {
        let anchor = ReviewAnchor {
            path: "web/api.ts".into(),
            lines: Some("lines 7–8".into()),
            excerpt: "+const a = 1;\n-const a = 2;".into(),
        };
        let prompt = review_prompt(&anchor, "  Why did this change?  ", Path::new("C:/repo"));
        assert!(prompt.contains("`web/api.ts` (lines 7–8)"));
        assert!(prompt.contains("```diff\n+const a = 1;\n-const a = 2;\n```"));
        assert!(prompt.contains("Reviewer's comment:\nWhy did this change?\n"));
    }

    #[test]
    fn commit_prompt_includes_style_and_truncates_long_diffs() {
        let subjects = ["Add a thing".to_string(), "Fix another".to_string()];
        let prompt = commit_message_prompt(" a.rs | 1 +\n\n+x", &subjects);
        assert!(prompt.contains("- Add a thing\n- Fix another\n"));
        assert!(prompt.ends_with("```diff\n a.rs | 1 +\n\n+x\n```\n"));
        assert!(!commit_message_prompt("+x", &[]).contains("recent commit subjects"));

        let long = "+".repeat(MAX_COMMIT_DIFF_CHARS + 10);
        let prompt = commit_message_prompt(&long, &[]);
        assert!(prompt.contains("[diff truncated]"));
        assert!(prompt.len() < long.len() + 1000);
    }

    #[test]
    fn parses_print_results() {
        let ok = |result: &str| {
            parse_print_result(
                &serde_json::json!({"is_error": false, "result": result}).to_string(),
            )
        };
        assert_eq!(ok("  Add X\n\nBody\n").unwrap(), "Add X\n\nBody");
        assert_eq!(ok("```text\nAdd X\n```").unwrap(), "Add X");
        assert!(ok("  ").is_err());
        let error = parse_print_result(r#"{"is_error":true,"result":"Not logged in"}"#);
        assert_eq!(error.unwrap_err().to_string(), "Not logged in");
        assert!(parse_print_result("garbage").is_err());
    }

    #[test]
    fn wraps_commit_bodies_but_not_subjects() {
        let subject = "A subject line that is longer than seventy-two characters stays as it is";
        let body = "Add commit_message_prompt, which builds a prompt from the staged diff, \
                    and generate_commit_message, which runs it.";
        let item = "- A list item that is long enough to need wrapping onto a second line \
                    here.";
        let message = format!("{subject}\n\n{body}\n{item}\n- short\n\nlast");
        assert_eq!(
            wrap_body(&message),
            format!(
                "{subject}\n\n\
                 Add commit_message_prompt, which builds a prompt from the staged diff,\n\
                 and generate_commit_message, which runs it.\n\
                 - A list item that is long enough to need wrapping onto a second line\n  \
                 here.\n- short\n\nlast"
            )
        );
        // Lines the model already broke, slightly too long, are reflowed as one paragraph.
        assert_eq!(
            wrap_body(
                "Subject\n\nThe body is re-wrapped at seventy-two columns, almost but not quite\n\
                 since models don't reliably do that."
            ),
            "Subject\n\nThe body is re-wrapped at seventy-two columns, almost but not quite\n\
             since models don't reliably do that."
        );
        assert_eq!(
            wrap_body(
                "Subject\n\nOutput is unfenced and its whole body is re-wrapped at seventy-two \
                 columns,\nsince models don't."
            ),
            "Subject\n\nOutput is unfenced and its whole body is re-wrapped at seventy-two\n\
             columns, since models don't."
        );
        assert_eq!(wrap_body("Only a subject"), "Only a subject");
    }
}
