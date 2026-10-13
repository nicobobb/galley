//! Review threads: each one is anchored to a file (and optionally a line range) and
//! holds a Claude Code conversation about it.

use std::path::PathBuf;

use futures::StreamExt as _;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding,
    ScrollHandle, SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};
use review_agent::permissions::{self, CommandSettings};
use review_agent::{AgentEvent, AgentRequest, AgentRun, ReviewAnchor};
use ui::WithScrollbar as _;

use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

actions!(agent_panel, [StopAgent]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", StopAgent, Some("AgentPanel"))]);
}

/// Tools whose successful result means files in the repo changed.
const EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];

/// Which side of the diff a thread's line numbers refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

#[derive(Debug, Clone)]
pub struct ThreadAnchor {
    pub path: String,
    /// Inclusive line range on `side`; `None` when the thread is about the whole file.
    pub lines: Option<(Side, u32, u32)>,
    /// Diff excerpt with `+`, `-` and ` ` prefixes.
    pub excerpt: String,
}

impl ThreadAnchor {
    fn lines_label(&self) -> Option<String> {
        let (side, start, end) = self.lines?;
        let side = match side {
            Side::Old => " (old version)",
            Side::New => "",
        };
        Some(if start == end {
            format!("line {start}{side}")
        } else {
            format!("lines {start}–{end}{side}")
        })
    }

    pub fn label(&self) -> String {
        let name = self.path.rsplit('/').next().unwrap_or(&self.path);
        match self.lines_label() {
            Some(lines) => format!("{name} · {lines}"),
            None => format!("{name} · whole file"),
        }
    }
}

enum ToolState {
    Running,
    Done,
    Failed,
}

enum ThreadItem {
    User(String),
    Assistant(String),
    Tool {
        id: String,
        name: String,
        detail: String,
        state: ToolState,
    },
    Notice(String),
    Error(String),
    /// Shell commands the agent wasn't allowed to run in the last turn.
    Denied(Vec<String>),
}

pub struct Thread {
    pub id: usize,
    pub anchor: ThreadAnchor,
    session_id: Option<String>,
    items: Vec<ThreadItem>,
    run: Option<AgentRun>,
    _events: Option<Task<()>>,
}

impl Thread {
    pub fn is_running(&self) -> bool {
        self.run.is_some()
    }
}

pub enum AgentPanelEvent {
    /// The agent edited files; the diff should be reloaded.
    FilesChanged,
    /// The user clicked a thread's anchor.
    Reveal(ThreadAnchor),
}

pub struct AgentPanel {
    focus_handle: FocusHandle,
    repo_root: Option<PathBuf>,
    threads: Vec<Thread>,
    active: Option<usize>,
    next_id: usize,
    composer: Entity<TextInput>,
    scroll: ScrollHandle,
    /// Per-repository command allowlists and where they're saved.
    settings: CommandSettings,
    settings_path: Option<PathBuf>,
    settings_error: Option<String>,
    show_permissions: bool,
    permission_input: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AgentPanelEvent> for AgentPanel {}

impl Focusable for AgentPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl AgentPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| TextInput::new("Write a comment or request…", true, cx));
        let permission_input =
            cx.new(|cx| TextInput::new("Add a command, e.g. cargo test (Enter)", false, cx));
        let subscriptions = vec![
            cx.subscribe(&composer, |this, _, event, cx| {
                if let TextInputEvent::Submitted = event {
                    this.send(cx);
                }
            }),
            cx.subscribe(&permission_input, |this, input, event, cx| {
                if let TextInputEvent::Submitted = event {
                    let entry = input.read(cx).text().to_string();
                    if this.allow_command(&entry, cx) {
                        input.update(cx, |input, cx| input.set_text("", cx));
                    }
                }
            }),
        ];
        let settings_path = CommandSettings::default_path();
        let existing_path = CommandSettings::existing_path();
        let (settings, settings_error) = match existing_path.as_deref().map(CommandSettings::load) {
            Some(Ok(settings)) => (settings, None),
            Some(Err(error)) => (CommandSettings::default(), Some(format!("{error:#}"))),
            None => (
                CommandSettings::default(),
                Some("couldn't find the config directory".to_string()),
            ),
        };
        Self {
            focus_handle: cx.focus_handle(),
            repo_root: None,
            threads: Vec::new(),
            active: None,
            next_id: 1,
            composer,
            scroll: ScrollHandle::new(),
            settings,
            settings_path,
            settings_error,
            show_permissions: false,
            permission_input,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_repo_root(&mut self, repo_root: PathBuf) {
        self.repo_root = Some(repo_root);
    }

    fn allowed_commands(&self) -> &[String] {
        match &self.repo_root {
            Some(root) => self.settings.allowed(root),
            None => &[],
        }
    }

    /// Adds `entry` to this repository's allowlist and saves it. Returns whether it
    /// was added.
    fn allow_command(&mut self, entry: &str, cx: &mut Context<Self>) -> bool {
        let Some(root) = self.repo_root.clone() else {
            return false;
        };
        let added = self.settings.allow(&root, entry);
        if added {
            self.save_settings();
        }
        cx.notify();
        added
    }

    fn disallow_command(&mut self, entry: &str, cx: &mut Context<Self>) {
        if let Some(root) = self.repo_root.clone() {
            self.settings.disallow(&root, entry);
            self.save_settings();
            cx.notify();
        }
    }

    fn save_settings(&mut self) {
        self.settings_error = match &self.settings_path {
            Some(path) => self
                .settings
                .save(path)
                .err()
                .map(|error| format!("{error:#}")),
            None => Some("couldn't find the config directory".to_string()),
        };
    }

    /// Allows a denied command's prefix and asks the agent to carry on.
    fn allow_and_continue(&mut self, thread_index: usize, prefix: String, cx: &mut Context<Self>) {
        self.allow_command(&prefix, cx);
        self.active = Some(thread_index);
        self.send_message(
            format!(
                "You can now run commands starting with `{prefix}`. \
                 Carry on with what you were doing."
            ),
            cx,
        );
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    /// Opens a new, still empty thread about `anchor` and focuses the composer.
    pub fn new_thread(
        &mut self,
        anchor: ThreadAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Reuse an untouched draft instead of piling up empty threads.
        self.threads
            .retain(|thread| !thread.items.is_empty() || thread.is_running());
        self.threads.push(Thread {
            id: self.next_id,
            anchor,
            session_id: None,
            items: Vec::new(),
            run: None,
            _events: None,
        });
        self.next_id += 1;
        self.active = Some(self.threads.len() - 1);
        self.focus_composer(window, cx);
        cx.notify();
    }

    pub fn focus_composer(&self, window: &mut Window, cx: &mut App) {
        self.composer.focus_handle(cx).focus(window, cx);
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        self.active = Some(index);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text().trim().to_string();
        if self.send_message(text, cx) {
            self.composer
                .update(cx, |composer, cx| composer.set_text("", cx));
        }
    }

    /// Sends `text` to the active thread. Returns whether it was sent.
    fn send_message(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.active else {
            return false;
        };
        let Some(repo_root) = self.repo_root.clone() else {
            return false;
        };
        if text.is_empty() || self.threads[index].is_running() {
            return false;
        }
        let allowed_tools = self
            .allowed_commands()
            .iter()
            .map(|entry| permissions::rule_for(entry))
            .collect();

        let thread = &mut self.threads[index];
        // The first message carries the code context; follow-ups resume the session.
        let prompt = match &thread.session_id {
            Some(_) => text.clone(),
            None => {
                let anchor = &thread.anchor;
                review_agent::review_prompt(
                    &ReviewAnchor {
                        path: anchor.path.clone(),
                        lines: anchor.lines_label(),
                        excerpt: anchor.excerpt.clone(),
                    },
                    &text,
                    &repo_root,
                )
            }
        };
        thread.items.push(ThreadItem::User(text));
        let request = AgentRequest {
            cwd: repo_root,
            prompt,
            resume_session: thread.session_id.clone(),
            allowed_tools,
        };

        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        match AgentRun::start(request, move |event| {
            sender.unbounded_send(event).ok();
        }) {
            Ok(run) => {
                let thread_id = thread.id;
                thread.run = Some(run);
                thread._events = Some(cx.spawn(async move |this, cx| {
                    while let Some(event) = receiver.next().await {
                        let applied =
                            this.update(cx, |this, cx| this.apply_event(thread_id, event, cx));
                        if applied.is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(error) => thread.items.push(ThreadItem::Error(format!("{error:#}"))),
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
        true
    }

    fn stop(&mut self, _: &StopAgent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.active.and_then(|index| self.threads.get_mut(index))
            && let Some(run) = &thread.run
        {
            run.cancel();
            cx.notify();
        }
    }

    fn apply_event(&mut self, thread_id: usize, event: AgentEvent, cx: &mut Context<Self>) {
        let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        else {
            return;
        };
        match event {
            AgentEvent::SessionStarted { session_id } => thread.session_id = Some(session_id),
            AgentEvent::TextStart => thread.items.push(ThreadItem::Assistant(String::new())),
            AgentEvent::TextDelta(delta) => match thread.items.last_mut() {
                Some(ThreadItem::Assistant(text)) => text.push_str(&delta),
                _ => thread.items.push(ThreadItem::Assistant(delta)),
            },
            AgentEvent::ToolUse { id, name, detail } => {
                thread.items.push(ThreadItem::Tool {
                    id,
                    name,
                    detail,
                    state: ToolState::Running,
                });
            }
            AgentEvent::ToolResult { id, is_error } => {
                let tool = thread.items.iter_mut().rev().find_map(|item| match item {
                    ThreadItem::Tool {
                        id: tool_id,
                        name,
                        state,
                        ..
                    } if *tool_id == id => Some((name, state)),
                    _ => None,
                });
                if let Some((name, state)) = tool {
                    *state = if is_error {
                        ToolState::Failed
                    } else {
                        ToolState::Done
                    };
                    if !is_error && EDIT_TOOLS.contains(&name.as_str()) {
                        cx.emit(AgentPanelEvent::FilesChanged);
                    }
                }
            }
            AgentEvent::Finished {
                is_error,
                cost_usd,
                duration_ms,
                denied_commands,
            } => {
                if !denied_commands.is_empty() {
                    thread.items.push(ThreadItem::Denied(denied_commands));
                }
                thread.run = None;
                let mut notice = if is_error {
                    "Finished with an error".to_string()
                } else {
                    "Done".to_string()
                };
                if let Some(duration) = duration_ms {
                    notice.push_str(&format!(" · {:.1} s", duration as f64 / 1000.));
                }
                if let Some(cost) = cost_usd {
                    notice.push_str(&format!(" · US$ {cost:.3}"));
                }
                thread.items.push(ThreadItem::Notice(notice));
                cx.emit(AgentPanelEvent::FilesChanged);
            }
            AgentEvent::Failed(message) => {
                thread.run = None;
                thread.items.push(ThreadItem::Error(message));
            }
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn render_thread_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_wrap()
            .gap_1()
            .children(self.threads.iter().enumerate().map(|(index, thread)| {
                let active = self.active == Some(index);
                let dot = if thread.is_running() {
                    theme::hunk_text()
                } else {
                    theme::muted()
                };
                div()
                    .id(("thread-tab", thread.id))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if active {
                        theme::accent()
                    } else {
                        theme::border()
                    })
                    .when(active, |tab| tab.bg(theme::selection()))
                    .on_click(cx.listener(move |this, _, _, cx| this.activate(index, cx)))
                    .child(div().size(px(6.)).rounded_full().bg(dot))
                    .child(format!("#{} {}", thread.id, thread.anchor.label()))
            }))
    }

    fn render_anchor(&self, thread: &Thread, cx: &mut Context<Self>) -> impl IntoElement {
        const MAX_EXCERPT_LINES: usize = 12;
        let anchor = thread.anchor.clone();
        let mut excerpt = anchor
            .excerpt
            .lines()
            .take(MAX_EXCERPT_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        if anchor.excerpt.lines().count() > MAX_EXCERPT_LINES {
            excerpt.push_str("\n…");
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_md()
            .bg(theme::background())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .id("anchor-link")
                    .cursor_pointer()
                    .text_color(theme::accent())
                    .hover(|style| style.underline())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(AgentPanelEvent::Reveal(anchor.clone()));
                    }))
                    .child(format!(
                        "{} · {}",
                        thread.anchor.path,
                        thread
                            .anchor
                            .lines_label()
                            .unwrap_or_else(|| "whole file".into())
                    )),
            )
            .when(!excerpt.is_empty(), |card| {
                card.child(
                    div()
                        .font_family(theme::CODE_FONT)
                        .text_size(px(12.))
                        .text_color(theme::muted())
                        .child(excerpt),
                )
            })
    }

    fn render_permissions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let allowed = self.allowed_commands().to_vec();
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .bg(theme::background())
            .border_1()
            .border_color(theme::border())
            .text_size(px(12.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Allowed commands in this repo"),
            )
            .child(div().text_color(theme::muted()).child(
                "Matched by prefix: `cargo test` allows `cargo test -p x`. Anything else is \
                 denied (the agent can still edit files). Permissions from your Claude Code \
                 settings apply too.",
            ))
            .when(allowed.is_empty(), |list| {
                list.child(div().text_color(theme::muted()).child("None yet."))
            })
            .children(allowed.into_iter().enumerate().map(|(index, entry)| {
                let removed = entry.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .font_family(theme::CODE_FONT)
                            .child(entry),
                    )
                    .child(
                        div()
                            .id(("disallow", index))
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(theme::muted())
                            .hover(|style| {
                                style.bg(theme::hover()).text_color(theme::removed_text())
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.disallow_command(&removed, cx)
                            }))
                            .child("✕"),
                    )
            }))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .font_family(theme::CODE_FONT)
                    .child(self.permission_input.clone()),
            )
            .when_some(self.settings_error.clone(), |section, error| {
                section.child(div().text_color(theme::error()).child(error))
            })
    }

    fn render_items(
        &self,
        thread_index: usize,
        thread: &Thread,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let running = thread.is_running();
        thread
            .items
            .iter()
            .enumerate()
            .map(|(item_index, item)| match item {
                ThreadItem::User(text) => div()
                    .p_2()
                    .rounded_md()
                    .bg(theme::selection())
                    .child(text.clone())
                    .into_any_element(),
                ThreadItem::Assistant(text) => render_markdownish(text),
                ThreadItem::Tool {
                    name,
                    detail,
                    state,
                    ..
                } => {
                    let (marker, color) = match state {
                        ToolState::Running => ("…", theme::muted()),
                        ToolState::Done => ("✓", theme::added_text()),
                        ToolState::Failed => ("✗", theme::removed_text()),
                    };
                    div()
                        .flex()
                        .gap_2()
                        .text_size(px(12.))
                        .child(div().flex_none().w(px(12.)).text_color(color).child(marker))
                        .child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .font_family(theme::CODE_FONT)
                                .text_color(theme::muted())
                                .child(detail.clone()),
                        )
                        .into_any_element()
                }
                ThreadItem::Notice(text) => div()
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child(text.clone())
                    .into_any_element(),
                ThreadItem::Error(text) => div()
                    .text_size(px(12.))
                    .text_color(theme::error())
                    .child(text.clone())
                    .into_any_element(),
                ThreadItem::Denied(commands) => {
                    let allowed = self.allowed_commands();
                    div()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme::removed_text())
                        .text_size(px(12.))
                        .child("The agent tried to run commands that aren't allowed:")
                        .children(commands.iter().enumerate().map(|(command_index, command)| {
                            let prefix = permissions::suggest_prefix(command);
                            let already = allowed.contains(&prefix);
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .font_family(theme::CODE_FONT)
                                        .text_color(theme::muted())
                                        .child(format!("$ {command}")),
                                )
                                .when(!already, |row| {
                                    row.child(
                                        div()
                                            .id(("allow", item_index * 100 + command_index))
                                            .px_2()
                                            .py_0p5()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .bg(theme::accent_background())
                                            .hover(|style| style.bg(theme::selection()))
                                            .when(running, |button| button.opacity(0.5))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.allow_and_continue(
                                                    thread_index,
                                                    prefix.clone(),
                                                    cx,
                                                )
                                            }))
                                            .child(format!(
                                                "Allow “{}” and continue",
                                                permissions::suggest_prefix(command)
                                            )),
                                    )
                                })
                                .when(already, |row| {
                                    row.child(
                                        div().text_color(theme::added_text()).child("Allowed"),
                                    )
                                })
                        }))
                        .into_any_element()
                }
            })
            .collect()
    }

    fn render_composer(&self, running: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (button_label, hint): (SharedString, &str) = if running {
            ("Stop".into(), "Esc stops the agent")
        } else {
            ("Send".into(), "Enter sends · Shift+Enter for a new line")
        };
        div()
            .flex_none()
            .p_2()
            .border_t_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .p_2()
                    .min_h(px(64.))
                    .max_h(px(240.))
                    .rounded_md()
                    .bg(theme::background())
                    .border_1()
                    .border_color(theme::border())
                    .child(self.composer.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(theme::muted())
                            .child(hint),
                    )
                    .child(
                        div()
                            .id("composer-button")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if running {
                                theme::removed_background()
                            } else {
                                theme::accent_background()
                            })
                            .hover(|style| style.bg(theme::hover()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if running {
                                    this.stop(&StopAgent, window, cx);
                                } else {
                                    this.send(cx);
                                }
                            }))
                            .child(button_label),
                    ),
            )
    }
}

/// Renders assistant prose, showing fenced code blocks in the code font.
fn render_markdownish(text: &str) -> AnyElement {
    let mut blocks = Vec::new();
    let mut in_code = false;
    let mut current = String::new();
    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            if !current.trim().is_empty() {
                blocks.push((in_code, std::mem::take(&mut current)));
            }
            current.clear();
            in_code = !in_code;
            continue;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        blocks.push((in_code, current));
    }

    div()
        .flex()
        .flex_col()
        .gap_2()
        .children(blocks.into_iter().map(|(code, text)| {
            if code {
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme::background())
                    .font_family(theme::CODE_FONT)
                    .text_size(px(12.))
                    .child(text)
            } else {
                div().child(text.trim().to_string())
            }
        }))
        .into_any_element()
}

impl Render for AgentPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active.and_then(|index| self.threads.get(index));
        let panel = div()
            .key_context("AgentPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::stop))
            .w(px(460.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .border_l_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_none()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Agent · Claude Code"),
                            )
                            .child(
                                div()
                                    .id("toggle-permissions")
                                    .px_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .text_color(if self.show_permissions {
                                        theme::accent()
                                    } else {
                                        theme::muted()
                                    })
                                    .hover(|style| style.bg(theme::hover()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_permissions = !this.show_permissions;
                                        cx.notify();
                                    }))
                                    .child(format!(
                                        "Permissions ({})",
                                        self.allowed_commands().len()
                                    )),
                            ),
                    )
                    .when(self.show_permissions, |header| {
                        header.child(self.render_permissions(cx))
                    })
                    .when(!self.threads.is_empty(), |header| {
                        header.child(self.render_thread_tabs(cx))
                    }),
            );

        let Some(thread) = active else {
            return panel.child(div().p_3().text_color(theme::muted()).child(
                "Select lines in the diff (click, Shift+click to extend) and press \
                         Alt+T to open a thread with Claude Code. Without a selection, the \
                         thread is about the whole file.",
            ));
        };
        let running = thread.is_running();
        panel
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("thread-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(self.render_anchor(thread, cx))
                            .children(self.render_items(
                                self.active.unwrap_or_default(),
                                thread,
                                cx,
                            ))
                            .when(running, |items| {
                                items.child(div().text_color(theme::muted()).child("Working…"))
                            }),
                    )
                    .custom_scrollbars(theme::vertical_scrollbar(&self.scroll), window, cx),
            )
            .child(self.render_composer(running, cx))
    }
}
