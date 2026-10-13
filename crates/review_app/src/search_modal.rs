//! Modal for jumping anywhere in the repository: fuzzy file finder and text search.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{
    App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyBinding, ScrollStrategy, SharedString, Stateful, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use review_search::{TextSearchOptions, TextSearchResults};

use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;

actions!(
    search_modal,
    [
        SelectNextResult,
        SelectPreviousResult,
        DismissSearch,
        ToggleRegex
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNextResult, Some("SearchModal")),
        KeyBinding::new("up", SelectPreviousResult, Some("SearchModal")),
        KeyBinding::new("escape", DismissSearch, Some("SearchModal")),
        KeyBinding::new("alt-r", ToggleRegex, Some("SearchModal")),
    ]);
}

const MAX_FILE_RESULTS: usize = 500;
const MAX_TEXT_RESULTS: usize = 2000;
const RESULT_HEIGHT: f32 = 26.;
const VISIBLE_RESULTS: usize = 14;
/// Characters of context kept before the first match when it's far into the line.
const LINE_CONTEXT: usize = 40;
const MAX_LINE_CHARS: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Files,
    Text,
}

enum ResultItem {
    File {
        path: SharedString,
        ranges: Vec<Range<usize>>,
    },
    Line {
        path: SharedString,
        line_number: u32,
        text: SharedString,
        ranges: Vec<Range<usize>>,
    },
}

pub enum SearchModalEvent {
    Open { path: String, line: Option<u32> },
    Dismissed,
}

pub struct SearchModal {
    mode: SearchMode,
    repo_root: PathBuf,
    input: Entity<TextInput>,
    regex: bool,
    /// All repository files, listed once when the finder opens.
    files: Option<Arc<Vec<String>>>,
    results: Vec<ResultItem>,
    selected: usize,
    scroll: UniformListScrollHandle,
    status: SharedString,
    /// Set to stop the in-flight search when the query changes.
    cancel: Arc<AtomicBool>,
    search_task: Option<Task<()>>,
    list_task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<SearchModalEvent> for SearchModal {}

impl Focusable for SearchModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl SearchModal {
    pub fn new(
        mode: SearchMode,
        repo_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = match mode {
            SearchMode::Files => "Search files by name…",
            SearchMode::Text => "Search text in the repository…",
        };
        let input = cx.new(|cx| TextInput::new(placeholder, false, cx));
        input.focus_handle(cx).focus(window, cx);
        let subscription = cx.subscribe(&input, |this, _, event, cx| match event {
            TextInputEvent::Changed => this.update_results(cx),
            TextInputEvent::Submitted => this.confirm(cx),
        });
        let mut this = Self {
            mode,
            repo_root,
            input,
            regex: false,
            files: None,
            results: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            status: SharedString::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            search_task: None,
            list_task: None,
            _subscription: subscription,
        };
        if mode == SearchMode::Files {
            this.list_files(cx);
        }
        this
    }

    fn list_files(&mut self, cx: &mut Context<Self>) {
        self.status = "Indexing files…".into();
        let root = self.repo_root.clone();
        let cancel = self.cancel.clone();
        let listing = cx.background_spawn(async move { review_search::list_files(&root, &cancel) });
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let files = listing.await;
            this.update(cx, |this, cx| {
                this.files = Some(Arc::new(files));
                this.update_results(cx);
            })
            .ok();
        }));
    }

    fn query(&self, cx: &App) -> String {
        self.input.read(cx).text().to_string()
    }

    fn update_results(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let query = self.query(cx);
        match self.mode {
            SearchMode::Files => self.match_files(query, cx),
            SearchMode::Text => self.search_text(query, cx),
        }
    }

    fn match_files(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(files) = self.files.clone() else {
            return;
        };
        let matching = cx.background_spawn(async move {
            let matches = review_search::fuzzy_match_paths(&query, &files, MAX_FILE_RESULTS);
            let total = files.len();
            let results = matches
                .into_iter()
                .map(|found| ResultItem::File {
                    path: files[found.index].clone().into(),
                    ranges: found.ranges,
                })
                .collect::<Vec<_>>();
            (results, total)
        });
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let (results, total) = matching.await;
            this.update(cx, |this, cx| {
                this.status = format!("{} of {total} files", results.len()).into();
                this.set_results(results, cx);
            })
            .ok();
        }));
    }

    fn search_text(&mut self, query: String, cx: &mut Context<Self>) {
        if query.is_empty() {
            self.status = SharedString::default();
            self.set_results(Vec::new(), cx);
            return;
        }
        let root = self.repo_root.clone();
        let cancel = self.cancel.clone();
        let options = TextSearchOptions {
            regex: self.regex,
            limit: MAX_TEXT_RESULTS,
        };
        self.search_task = Some(cx.spawn(async move |this, cx| {
            // Wait for a pause in typing before walking the repository.
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            this.update(cx, |this, cx| {
                this.status = "Searching…".into();
                cx.notify();
            })
            .ok();
            let search = cx.background_spawn(async move {
                review_search::search_text(&root, &query, options, &cancel)
            });
            let result = search.await;
            this.update(cx, |this, cx| match result {
                Ok(TextSearchResults { matches, truncated }) => {
                    let count = matches.len();
                    let files = matches
                        .iter()
                        .map(|found| &found.path)
                        .collect::<std::collections::HashSet<_>>()
                        .len();
                    this.status = if truncated {
                        format!("First {count} results in {files} files (limit reached)")
                    } else {
                        format!("{count} results in {files} files")
                    }
                    .into();
                    let results = matches
                        .into_iter()
                        .map(|found| {
                            let (text, ranges) = excerpt_line(&found.line, &found.ranges);
                            ResultItem::Line {
                                path: found.path.into(),
                                line_number: found.line_number,
                                text: text.into(),
                                ranges,
                            }
                        })
                        .collect();
                    this.set_results(results, cx);
                }
                Err(error) => {
                    this.status = format!("Invalid query: {error}").into();
                    this.set_results(Vec::new(), cx);
                }
            })
            .ok();
        }));
    }

    fn set_results(&mut self, results: Vec<ResultItem>, cx: &mut Context<Self>) {
        self.results = results;
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNextResult, _: &mut Window, cx: &mut Context<Self>) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
            self.scroll
                .scroll_to_item(self.selected, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &SelectPreviousResult,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.results.is_empty() {
            self.selected = (self.selected + self.results.len() - 1) % self.results.len();
            self.scroll
                .scroll_to_item(self.selected, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn toggle_regex(&mut self, _: &ToggleRegex, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == SearchMode::Text {
            self.regex = !self.regex;
            self.update_results(cx);
            cx.notify();
        }
    }

    fn dismiss(&mut self, _: &DismissSearch, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        cx.emit(SearchModalEvent::Dismissed);
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(result) = self.results.get(self.selected) else {
            return;
        };
        self.cancel.store(true, Ordering::Relaxed);
        cx.emit(match result {
            ResultItem::File { path, .. } => SearchModalEvent::Open {
                path: path.to_string(),
                line: None,
            },
            ResultItem::Line {
                path, line_number, ..
            } => SearchModalEvent::Open {
                path: path.to_string(),
                line: Some(*line_number),
            },
        });
    }

    fn render_result(&self, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = index == self.selected;
        let row = div()
            .id(index)
            .h(px(RESULT_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .whitespace_nowrap()
            .overflow_hidden()
            .when(selected, |row| row.bg(theme::selection()))
            .when(!selected, |row| row.hover(|style| style.bg(theme::hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = index;
                this.confirm(cx);
            }));
        let matched = HighlightStyle {
            color: Some(theme::accent().into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        match &self.results[index] {
            ResultItem::File { path, ranges } => row.child(
                StyledText::new(path.clone())
                    .with_highlights(ranges.iter().map(|range| (range.clone(), matched))),
            ),
            ResultItem::Line {
                path,
                line_number,
                text,
                ranges,
            } => {
                let found = HighlightStyle {
                    background_color: Some(theme::accent_background().into()),
                    ..Default::default()
                };
                row.child(
                    div()
                        .flex_none()
                        .max_w(px(260.))
                        .truncate()
                        .text_color(theme::muted())
                        .child(format!("{path}:{line_number}")),
                )
                .child(
                    div()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .font_family(theme::CODE_FONT)
                        .child(
                            StyledText::new(text.clone())
                                .with_highlights(ranges.iter().map(|range| (range.clone(), found))),
                        ),
                )
            }
        }
    }
}

/// Trims leading indentation and, for long lines, keeps a window around the first
/// match. Returns the text to show and the match ranges rebased onto it.
fn excerpt_line(line: &str, ranges: &[Range<usize>]) -> (String, Vec<Range<usize>>) {
    let normalized = review_syntax::normalize_line(line);
    // Tabs expand to spaces, so map ranges by recomputing them on the normalized text.
    let ranges = if normalized.len() == line.len() {
        ranges.to_vec()
    } else {
        let shift = |offset: usize| review_syntax::normalize_line(&line[..offset]).len();
        ranges
            .iter()
            .map(|range| shift(range.start)..shift(range.end))
            .collect()
    };
    let indent = normalized.len() - normalized.trim_start().len();
    let first_match = ranges.first().map_or(indent, |range| range.start);
    // Keep the first match visible: skip ahead when it's far from the line start.
    let mut start = indent;
    if normalized[indent..first_match.max(indent)].chars().count() > LINE_CONTEXT * 2 {
        start = first_match.saturating_sub(LINE_CONTEXT).max(indent);
        while !normalized.is_char_boundary(start) {
            start -= 1;
        }
    }
    let mut end = normalized.len();
    if let Some((index, _)) = normalized[start..].char_indices().nth(MAX_LINE_CHARS) {
        end = start + index;
    }
    let prefix = if start > indent { "…" } else { "" };
    let text = format!("{prefix}{}", &normalized[start..end]);
    let ranges = ranges
        .iter()
        .filter(|range| range.start >= start && range.end <= end)
        .map(|range| range.start - start + prefix.len()..range.end - start + prefix.len())
        .collect();
    (text, ranges)
}

impl Render for SearchModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.mode {
            SearchMode::Files => "Go to file",
            SearchMode::Text => "Search the repository",
        };
        let list_height = px(RESULT_HEIGHT * self.results.len().clamp(1, VISIBLE_RESULTS) as f32);
        div()
            .key_context("SearchModal")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::toggle_regex))
            .occlude()
            .w(px(720.))
            .flex()
            .flex_col()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().flex_none().text_color(theme::muted()).child(title))
                    .child(
                        div()
                            .flex_1()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(theme::background())
                            .child(self.input.clone()),
                    )
                    .when(self.mode == SearchMode::Text, |header| {
                        header.child(
                            div()
                                .id("regex-toggle")
                                .flex_none()
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .cursor_pointer()
                                .font_family(theme::CODE_FONT)
                                .border_1()
                                .border_color(if self.regex {
                                    theme::accent()
                                } else {
                                    theme::border()
                                })
                                .text_color(if self.regex {
                                    theme::accent()
                                } else {
                                    theme::muted()
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_regex(&ToggleRegex, window, cx)
                                }))
                                .child(".*"),
                        )
                    }),
            )
            .child(
                uniform_list(
                    "search-results",
                    self.results.len(),
                    cx.processor(|this, range: Range<usize>, _window, cx| {
                        range
                            .map(|index| this.render_result(index, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .h(list_height),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_size(px(12.))
                    .text_color(theme::muted())
                    .child(div().flex_1().child(self.status.clone()))
                    .child(match self.mode {
                        SearchMode::Files => "↑↓ select · Enter open · Esc close",
                        SearchMode::Text => "↑↓ select · Enter open · Alt+R regex · Esc close",
                    }),
            )
    }
}

#[cfg(test)]
// Single-range match lists are the expected values, not a mistaken range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_trims_indent_and_rebases_ranges() {
        let (text, ranges) = excerpt_line("\t    let hello = 1;", &[9..14]);
        assert_eq!(text, "let hello = 1;");
        assert_eq!(&text[ranges[0].clone()], "hello");

        let long = format!("{}needle{}", "x".repeat(300), "y".repeat(300));
        let (text, ranges) = excerpt_line(&long, &[300..306]);
        assert!(text.starts_with('…'));
        assert_eq!(&text[ranges[0].clone()], "needle");
        assert!(text.chars().count() <= MAX_LINE_CHARS + 1);

        let comment = format!("/// {} needle", "word ".repeat(30));
        let (text, ranges) = excerpt_line(&comment, &[comment.len() - 6..comment.len()]);
        assert!(text.starts_with('…') && text.ends_with("needle"));
        assert_eq!(&text[ranges[0].clone()], "needle");
    }
}
