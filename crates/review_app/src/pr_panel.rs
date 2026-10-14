//! The pull request being reviewed: its details, pushing local fixes to its branch,
//! draft line comments and submitting them as a GitHub review.

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    ScrollHandle, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use review_github::{DiffSide, DraftComment, PrCheckout, ReviewEvent};
use ui::WithScrollbar as _;

use crate::agent_panel::{Side, ThreadAnchor};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

/// Excerpt lines shown on a draft card.
const MAX_EXCERPT_LINES: usize = 6;

pub enum PrPanelEvent {
    /// The user clicked a draft's location.
    Reveal(ThreadAnchor),
    /// Drafts were added, removed or submitted; the diff's markers need repainting.
    DraftsChanged,
    /// Local commits were pushed to the PR's branch.
    Pushed,
}

pub struct PrPanel {
    focus_handle: FocusHandle,
    checkout: PrCheckout,
    drafts: Vec<DraftComment>,
    /// Where the comment being written goes; `None` when not writing one.
    pending: Option<ThreadAnchor>,
    draft_input: Entity<TextInput>,
    body_input: Entity<TextInput>,
    /// Local commits the PR's branch doesn't have yet.
    unpushed: usize,
    /// Files with changes that aren't committed.
    uncommitted: usize,
    /// Result of the last action: (is error, message).
    status: Option<(bool, SharedString)>,
    /// Link to the review just submitted.
    review_url: Option<String>,
    /// What's running in `task`, e.g. "Pushing…".
    busy: Option<&'static str>,
    task: Option<Task<()>>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PrPanelEvent> for PrPanel {}

impl Focusable for PrPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PrPanel {
    pub fn new(checkout: PrCheckout, cx: &mut Context<Self>) -> Self {
        let draft_input = cx.new(|cx| TextInput::new("Write a comment…", true, cx));
        let body_input = cx.new(|cx| {
            TextInput::new("Review summary (optional for approvals)", true, cx)
                .submit_on_enter(false)
        });
        let subscriptions = vec![cx.subscribe(&draft_input, |this, _, event, cx| {
            if let TextInputEvent::Submitted = event {
                this.add_draft(cx);
            }
        })];
        let (drafts, status) = match review_github::load_drafts(&checkout.pull_request.pr) {
            Ok(drafts) => (drafts, None),
            Err(error) => (Vec::new(), Some((true, format!("{error:#}").into()))),
        };
        Self {
            focus_handle: cx.focus_handle(),
            checkout,
            drafts,
            pending: None,
            draft_input,
            body_input,
            unpushed: 0,
            uncommitted: 0,
            status,
            review_url: None,
            busy: None,
            task: None,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    pub fn checkout(&self) -> &PrCheckout {
        &self.checkout
    }

    pub fn drafts(&self) -> &[DraftComment] {
        &self.drafts
    }

    pub fn set_local_state(&mut self, unpushed: usize, uncommitted: usize, cx: &mut Context<Self>) {
        self.unpushed = unpushed;
        self.uncommitted = uncommitted;
        cx.notify();
    }

    pub fn show_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.status = Some((true, message.into()));
        cx.notify();
    }

    /// Starts writing a comment on `anchor`, which must have a line range.
    pub fn start_draft(
        &mut self,
        anchor: ThreadAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending = Some(anchor);
        self.status = None;
        self.draft_input.focus_handle(cx).focus(window, cx);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.draft_input
            .update(cx, |input, cx| input.set_text("", cx));
        cx.notify();
    }

    fn add_draft(&mut self, cx: &mut Context<Self>) {
        let body = self.draft_input.read(cx).text().trim().to_string();
        let Some(anchor) = &self.pending else {
            return;
        };
        let Some((side, start_line, line)) = anchor.lines else {
            return;
        };
        if body.is_empty() {
            return;
        }
        self.drafts.push(DraftComment {
            path: anchor.path.clone(),
            side: match side {
                Side::Old => DiffSide::Left,
                Side::New => DiffSide::Right,
            },
            start_line,
            line,
            body,
            excerpt: anchor.excerpt.clone(),
        });
        self.save_drafts();
        self.cancel_draft(cx);
        cx.emit(PrPanelEvent::DraftsChanged);
    }

    fn remove_draft(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.drafts.len() {
            self.drafts.remove(index);
            self.save_drafts();
            cx.emit(PrPanelEvent::DraftsChanged);
            cx.notify();
        }
    }

    fn save_drafts(&mut self) {
        if let Err(error) = review_github::save_drafts(&self.checkout.pull_request.pr, &self.drafts)
        {
            self.status = Some((true, format!("{error:#}").into()));
        }
    }

    fn push(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let checkout = self.checkout.clone();
        let push = cx.background_spawn(async move { review_github::push(&checkout) });
        self.busy = Some("Pushing…");
        self.status = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = push.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                this.status = Some(match result {
                    Ok(summary) => (false, summary.into()),
                    Err(error) => (true, format!("{error:#}").into()),
                });
                cx.emit(PrPanelEvent::Pushed);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit(&mut self, event: ReviewEvent, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let checkout = self.checkout.clone();
        let body = self.body_input.read(cx).text().to_string();
        let drafts = self.drafts.clone();
        let submit = cx.background_spawn(async move {
            review_github::submit_review(&checkout, event, &body, &drafts)
        });
        self.busy = Some("Submitting the review…");
        self.status = None;
        self.review_url = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = submit.await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(url) => {
                        this.drafts.clear();
                        this.save_drafts();
                        this.body_input
                            .update(cx, |input, cx| input.set_text("", cx));
                        this.status = Some((false, "Review submitted.".into()));
                        this.review_url = Some(url);
                        cx.emit(PrPanelEvent::DraftsChanged);
                    }
                    Err(error) => this.status = Some((true, format!("{error:#}").into())),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_details(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pr = &self.checkout.pull_request;
        let url = pr.url.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("#{} {}", pr.pr.number, pr.title)),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child(format!(
                        "{} · {} → {} · {}",
                        pr.author,
                        pr.head_branch,
                        pr.base_branch,
                        pr.state.to_lowercase()
                    )),
            )
            .child(
                div()
                    .id("open-pr")
                    .text_size(px(12.))
                    .cursor_pointer()
                    .text_color(theme::accent())
                    .hover(|style| style.underline())
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                    .child("Open on GitHub"),
            )
    }

    fn render_local_changes(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.unpushed == 0 && self.uncommitted == 0 {
            return None;
        }
        let pr = &self.checkout.pull_request;
        let commits = match self.unpushed {
            1 => "1 local commit".to_string(),
            count => format!("{count} local commits"),
        };
        Some(
            card()
                .gap_1p5()
                .text_size(px(12.))
                .when(self.unpushed > 0, |card| match pr.push_blocker() {
                    Some(reason) => card.child(
                        div()
                            .text_color(theme::warning())
                            .child(format!("{commits} can't be pushed: {reason}")),
                    ),
                    None => card.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_1().child(format!("{commits} not pushed yet")))
                            .child(
                                button("push", format!("Push to {}", pr.head_branch))
                                    .when(self.busy.is_some(), |button| button.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| this.push(cx))),
                            ),
                    ),
                })
                .when(self.uncommitted > 0, |card| {
                    card.child(div().text_color(theme::muted()).child(match self.uncommitted {
                        1 => "1 file with uncommitted changes: commit it to push it.".to_string(),
                        count => format!(
                            "{count} files with uncommitted changes: commit them to push them."
                        ),
                    }))
                })
                .when(!self.drafts.is_empty(), |card| {
                    card.child(div().text_color(theme::warning()).child(
                        "Comments point at lines of your local version: push first so they \
                         land on the right lines.",
                    ))
                }),
        )
    }

    fn render_pending(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let anchor = self.pending.as_ref()?;
        Some(
            card()
                .gap_2()
                .border_color(theme::warning())
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::muted())
                        .child(format!("New comment · {}", anchor.label())),
                )
                .child(
                    div()
                        .p_2()
                        .min_h(px(56.))
                        .max_h(px(200.))
                        .rounded_md()
                        .bg(theme::panel())
                        .border_1()
                        .border_color(theme::border())
                        .child(self.draft_input.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.))
                                .text_color(theme::muted())
                                .child("Enter adds · Shift+Enter for a new line"),
                        )
                        .child(
                            link("cancel-draft", "Cancel")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_draft(cx))),
                        )
                        .child(
                            button("add-draft", "Add comment")
                                .on_click(cx.listener(|this, _, _, cx| this.add_draft(cx))),
                        ),
                ),
        )
    }

    fn render_drafts(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.drafts
            .iter()
            .enumerate()
            .map(|(index, draft)| {
                let anchor = ThreadAnchor {
                    path: draft.path.clone(),
                    lines: Some((
                        match draft.side {
                            DiffSide::Left => Side::Old,
                            DiffSide::Right => Side::New,
                        },
                        draft.start_line,
                        draft.line,
                    )),
                    excerpt: draft.excerpt.clone(),
                };
                let mut excerpt = draft
                    .excerpt
                    .lines()
                    .take(MAX_EXCERPT_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                if draft.excerpt.lines().count() > MAX_EXCERPT_LINES {
                    excerpt.push_str("\n…");
                }
                let label = format!(
                    "{} · {}",
                    draft.path,
                    anchor.lines_label().unwrap_or_default()
                );
                card()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id(("draft-anchor", index))
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .text_color(theme::accent())
                                    .hover(|style| style.underline())
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.emit(PrPanelEvent::Reveal(anchor.clone()))
                                    }))
                                    .child(label),
                            )
                            .child(
                                div()
                                    .id(("remove-draft", index))
                                    .px_1()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(12.))
                                    .text_color(theme::muted())
                                    .hover(|style| {
                                        style.bg(theme::hover()).text_color(theme::removed_text())
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_draft(index, cx)
                                    }))
                                    .child("✕"),
                            ),
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
                    .child(div().child(draft.body.clone()))
                    .into_any_element()
            })
            .collect()
    }

    fn render_submit(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let drafts = match self.drafts.len() {
            0 => "No line comments".to_string(),
            1 => "1 line comment".to_string(),
            count => format!("{count} line comments"),
        };
        let busy = self.busy.is_some();
        let review_button = |id: &'static str, label: &'static str, event: ReviewEvent| {
            button(id, label)
                .when(busy, |button| button.opacity(0.5))
                .on_click(cx.listener(move |this, _, _, cx| this.submit(event, cx)))
        };
        div()
            .flex_none()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child(format!("{drafts} · Alt+R comments on the selected lines")),
            )
            .child(
                div()
                    .p_2()
                    .min_h(px(56.))
                    .max_h(px(160.))
                    .rounded_md()
                    .bg(theme::background())
                    .border_1()
                    .border_color(theme::border())
                    .child(self.body_input.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(review_button(
                        "submit-comment",
                        "Comment",
                        ReviewEvent::Comment,
                    ))
                    .child(review_button(
                        "submit-approve",
                        "Approve",
                        ReviewEvent::Approve,
                    ))
                    .child(review_button(
                        "submit-request-changes",
                        "Request changes",
                        ReviewEvent::RequestChanges,
                    )),
            )
            .when_some(self.busy, |section, busy| {
                section.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::muted())
                        .child(busy),
                )
            })
            .when_some(self.status.clone(), |section, (is_error, message)| {
                section.child(
                    div()
                        .text_size(px(12.))
                        .text_color(if is_error {
                            theme::error()
                        } else {
                            theme::added_text()
                        })
                        .child(message),
                )
            })
            .when_some(self.review_url.clone(), |section, url| {
                section.child(
                    div()
                        .id("review-url")
                        .text_size(px(12.))
                        .cursor_pointer()
                        .text_color(theme::accent())
                        .hover(|style| style.underline())
                        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                        .child("See it on GitHub"),
                )
            })
    }
}

fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .p_2()
        .rounded_md()
        .bg(theme::background())
        .border_1()
        .border_color(theme::border())
}

fn button(id: &'static str, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .bg(theme::accent_background())
        .hover(|style| style.bg(theme::selection()))
        .child(label.into())
}

fn link(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_1()
        .rounded_sm()
        .cursor_pointer()
        .text_color(theme::muted())
        .hover(|style| style.bg(theme::hover()).text_color(theme::text()))
        .child(label)
}

impl Render for PrPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.drafts.is_empty() && self.pending.is_none();
        div()
            .key_context("PrPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("pr-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(self.render_details(cx))
                            .children(self.render_local_changes(cx))
                            .children(self.render_drafts(cx))
                            .children(self.render_pending(cx))
                            .when(empty, |list| {
                                list.child(div().text_color(theme::muted()).child(
                                    "Select lines in the diff (click, Shift+click to extend) and \
                                     press Alt+R to leave a comment. Comments stay here as \
                                     drafts until you submit the review.",
                                ))
                            }),
                    )
                    .custom_scrollbars(theme::vertical_scrollbar(&self.scroll), window, cx),
            )
            .child(self.render_submit(cx))
    }
}
