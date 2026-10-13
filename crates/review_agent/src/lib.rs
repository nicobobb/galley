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
}
