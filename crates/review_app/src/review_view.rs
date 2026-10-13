use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    AnyElement, ClickEvent, Context, Entity, FocusHandle, FontWeight, HighlightStyle,
    ListHorizontalSizingBehavior, ScrollStrategy, SharedString, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use review_agent::permissions::CommandSettings;
use review_git::{FileDiff, LineKind, RepoWatcher, Review, StageState};
use review_syntax::HighlightedLine;
use review_update::Release;
use ui::WithScrollbar as _;

use crate::agent_panel::{AgentPanel, AgentPanelEvent, Side, ThreadAnchor};
use crate::file_tree::{FileTree, TreeEntry};
use crate::search_modal::{SearchModal, SearchModalEvent, SearchMode};
use crate::split_diff::{RowTag, SplitRow};
use crate::text_input::{TextInput, TextInputEvent};
use crate::{
    NewThread, NextFile, OpenFileFinder, OpenTextSearch, PreviousFile, Reload, ToggleAgentPanel,
    ToggleSplitDiff, ToggleStaged, theme,
};

/// Files bigger than this aren't opened in the file view.
const MAX_OPEN_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Galley tends to stay open for days, so it checks again now and then.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Progress of updating Galley to a newer release, shown in the header.
enum UpdateState {
    /// Up to date, or the check failed (offline, rate limited): nothing to show.
    None,
    Available(Release),
    Installing(SharedString),
    /// The new executable is in place; it runs from the next start.
    Installed(SharedString),
    Failed(Release, SharedString),
    /// Installed, but launching it failed; reopening Galley by hand picks it up.
    RestartFailed(SharedString),
}

enum LoadState {
    Loading,
    Failed(SharedString),
    Loaded(Review),
}

/// Syntax highlighting for both sides of one file, indexed by 0-based line number.
#[derive(Default)]
struct FileHighlights {
    old: Option<Vec<HighlightedLine>>,
    new: Option<Vec<HighlightedLine>>,
}

/// A repository file shown in full, outside the diff (opened from search).
struct OpenFile {
    path: String,
    /// `None` while loading; `Err` when it can't be shown.
    text: Option<Result<String, String>>,
}

/// One rendered line of the diff pane.
enum Row {
    Notice(SharedString),
    HunkHeader(SharedString),
    Line {
        kind: LineKind,
        old_lineno: Option<u32>,
        new_lineno: Option<u32>,
        text: SharedString,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    },
}

impl Row {
    fn tag(&self) -> RowTag {
        match self {
            Row::Line { kind, .. } => RowTag::Line(*kind),
            Row::Notice(_) | Row::HunkHeader(_) => RowTag::Banner,
        }
    }

    fn line_number(&self, side: Side) -> Option<u32> {
        match (self, side) {
            (Row::Line { old_lineno, .. }, Side::Old) => *old_lineno,
            (Row::Line { new_lineno, .. }, Side::New) => *new_lineno,
            _ => None,
        }
    }
}

pub struct ReviewView {
    focus_handle: FocusHandle,
    repo_path: PathBuf,
    base: Option<String>,
    state: LoadState,
    selected: usize,
    /// Rows of the selected file; shared with the list's render callback.
    rows: Rc<Vec<Row>>,
    widest_row: Option<usize>,
    /// Show the diff side by side instead of unified.
    split: bool,
    /// Side-by-side layout of `rows`, and for each row the split row showing it.
    split_rows: Rc<Vec<SplitRow>>,
    split_index: Vec<usize>,
    file_tree: Option<FileTree>,
    /// Changed files in sidebar (tree) order, for next/previous navigation.
    file_order: Vec<usize>,
    tree_entries: Rc<Vec<TreeEntry>>,
    collapsed_dirs: HashSet<String>,
    /// Rows selected to anchor a thread, as (anchor, head) indices into `rows`.
    line_selection: Option<(usize, usize)>,
    /// When set, the main pane shows this file instead of the selected diff.
    open_file: Option<OpenFile>,
    /// Lines to select and scroll to once the rows that contain them are built.
    pending_lines: Option<(Side, u32, u32)>,
    search_modal: Option<(Entity<SearchModal>, Subscription)>,
    open_file_task: Option<Task<()>>,
    /// Highlights by file path, cleared on every reload.
    highlights: HashMap<String, Arc<FileHighlights>>,
    file_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    agent_panel: Entity<AgentPanel>,
    show_agent_panel: bool,
    commit_input: Entity<TextInput>,
    /// Result of the last stage/unstage/commit: (is error, message).
    git_status: Option<(bool, SharedString)>,
    git_task: Option<Task<()>>,
    load_task: Option<Task<()>>,
    highlight_task: Option<Task<()>>,
    /// Reloads the review when files change on disk; `None` until the first load.
    watcher: Option<(PathBuf, RepoWatcher, Task<()>)>,
    /// Why the repository can't be watched, shown so the user knows to reload by hand.
    watcher_error: Option<String>,
    /// The executable updates replace; `None` unless installed by `install.ps1`.
    installed_exe: Option<PathBuf>,
    update: UpdateState,
    update_check_task: Option<Task<()>>,
    update_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ReviewView {
    pub fn new(
        repo_path: PathBuf,
        base: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let agent_panel = cx.new(AgentPanel::new);
        let commit_input = cx.new(|cx| {
            TextInput::new("Commit message (Ctrl+Enter to commit)", true, cx).submit_on_enter(false)
        });
        let subscriptions = vec![
            cx.subscribe_in(&agent_panel, window, Self::on_agent_panel_event),
            cx.subscribe(&commit_input, |this, _, event, cx| {
                if let TextInputEvent::Submitted = event {
                    this.commit(cx);
                }
            }),
        ];
        let mut this = Self {
            focus_handle,
            repo_path,
            base,
            state: LoadState::Loading,
            selected: 0,
            rows: Rc::default(),
            widest_row: None,
            split: false,
            split_rows: Rc::default(),
            split_index: Vec::new(),
            file_tree: None,
            file_order: Vec::new(),
            tree_entries: Rc::default(),
            collapsed_dirs: HashSet::new(),
            line_selection: None,
            open_file: None,
            pending_lines: None,
            search_modal: None,
            open_file_task: None,
            highlights: HashMap::new(),
            file_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            agent_panel,
            show_agent_panel: false,
            commit_input,
            git_status: None,
            git_task: None,
            load_task: None,
            highlight_task: None,
            watcher: None,
            watcher_error: None,
            installed_exe: review_update::installed_exe(),
            update: UpdateState::None,
            update_check_task: None,
            update_task: None,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this.check_for_updates(cx);
        this
    }

    fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Some(exe) = &self.installed_exe else {
            return;
        };
        review_update::remove_old_exe(exe);
        if !CommandSettings::existing_path()
            .as_deref()
            .is_none_or(review_update::enabled_in)
        {
            return;
        }
        self.update_check_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let result = cx.background_spawn(async { review_update::check() }).await;
                let updated = this.update(cx, |this, cx| {
                    // A newer release replaces one found earlier, but never interrupts
                    // an install or hides its result.
                    if let (Ok(Some(release)), UpdateState::None | UpdateState::Available(_)) =
                        (result, &this.update)
                    {
                        this.update = UpdateState::Available(release);
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
                cx.background_executor().timer(UPDATE_CHECK_INTERVAL).await;
            }
        }));
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        let (UpdateState::Available(release) | UpdateState::Failed(release, _)) = &self.update
        else {
            return;
        };
        let Some(exe) = self.installed_exe.clone() else {
            return;
        };
        let release = release.clone();
        let version: SharedString = release.version.to_string().into();
        self.update = UpdateState::Installing(version.clone());
        cx.notify();
        let install = cx.background_spawn({
            let release = release.clone();
            async move { review_update::install(&release, &exe) }
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let result = install.await;
            this.update(cx, |this, cx| {
                this.update = match result {
                    Ok(()) => UpdateState::Installed(version),
                    Err(error) => UpdateState::Failed(release, format!("{error:#}").into()),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    /// Starts the new version with the same arguments and closes this one.
    fn restart(&mut self, cx: &mut Context<Self>) {
        let Some(exe) = &self.installed_exe else {
            return;
        };
        match std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .spawn()
        {
            Ok(_) => cx.quit(),
            Err(error) => {
                self.update = UpdateState::RestartFailed(format!("{error}").into());
                cx.notify();
            }
        }
    }

    fn review(&self) -> Option<&Review> {
        match &self.state {
            LoadState::Loaded(review) => Some(review),
            _ => None,
        }
    }

    fn selected_file(&self) -> Option<&FileDiff> {
        self.review()?.files.get(self.selected)
    }

    /// Path of the file in the main pane: an opened file or the selected diff.
    fn current_path(&self) -> Option<&str> {
        match &self.open_file {
            Some(open_file) => Some(&open_file.path),
            None => self.selected_file().map(|file| file.path.as_str()),
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let path = self.repo_path.clone();
        let base = self.base.clone();
        let load =
            cx.background_spawn(async move { review_git::load_review(&path, base.as_deref()) });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| this.set_review(result, cx)).ok();
        }));
    }

    fn set_review(&mut self, result: anyhow::Result<Review>, cx: &mut Context<Self>) {
        // Keep the same file selected across reloads, e.g. after an agent edited the repo.
        let previous_path = self.selected_file().map(|file| file.path.clone());
        self.highlights.clear();
        self.state = match result {
            Ok(review) => {
                let root = review.repo_root.clone();
                self.agent_panel
                    .update(cx, |panel, _| panel.set_repo_root(root.clone()));
                self.watch(root, cx);
                LoadState::Loaded(review)
            }
            Err(error) => LoadState::Failed(format!("{error:#}").into()),
        };
        self.rebuild_tree();
        let same_file = previous_path.as_ref().and_then(|path| {
            self.review()?
                .files
                .iter()
                .position(|file| &file.path == path)
        });
        if let Some(path) = self
            .open_file
            .as_ref()
            .map(|open_file| open_file.path.clone())
        {
            self.load_open_file(path, cx);
            return;
        }
        match same_file {
            // Same file: refresh in place without jumping to the top.
            Some(index) => {
                self.selected = index;
                self.line_selection = None;
                self.rebuild_rows();
                self.highlight_selected(cx);
                cx.notify();
            }
            None => self.select(self.file_order.first().copied().unwrap_or(0), cx),
        }
    }

    fn rebuild_tree(&mut self) {
        let tree = self
            .review()
            .map(|review| FileTree::new(review.files.iter().map(|file| file.path.as_str())));
        self.file_order = tree.as_ref().map(FileTree::file_order).unwrap_or_default();
        self.file_tree = tree;
        self.rebuild_tree_entries();
    }

    fn rebuild_tree_entries(&mut self) {
        let entries = self
            .file_tree
            .as_ref()
            .map(|tree| tree.entries(&self.collapsed_dirs))
            .unwrap_or_default();
        self.tree_entries = Rc::new(entries);
    }

    fn toggle_dir(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(path) {
            self.collapsed_dirs.insert(path.to_string());
        }
        self.rebuild_tree_entries();
        cx.notify();
    }

    /// Expands collapsed directories containing the file and scrolls the tree to it.
    fn reveal_in_tree(&mut self, index: usize) {
        let Some(path) = self
            .review()
            .and_then(|review| review.files.get(index))
            .map(|file| file.path.clone())
        else {
            return;
        };
        let before = self.collapsed_dirs.len();
        self.collapsed_dirs
            .retain(|dir| !path.starts_with(&format!("{dir}/")));
        if self.collapsed_dirs.len() != before {
            self.rebuild_tree_entries();
        }
        let entry = self.tree_entries.iter().position(|entry| {
            matches!(entry, TreeEntry::File { index: file_index, .. } if *file_index == index)
        });
        if let Some(entry) = entry {
            self.file_scroll
                .scroll_to_item(entry, ScrollStrategy::Nearest);
        }
    }

    /// Runs a git operation in the background, then reports and reloads.
    fn run_git(
        &mut self,
        operation: impl FnOnce(&std::path::Path) -> anyhow::Result<String> + Send + 'static,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.review().map(|review| review.repo_root.clone()) else {
            return;
        };
        let task = cx.background_spawn(async move { operation(&root) });
        self.git_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.git_status = match result {
                    Ok(message) => {
                        on_success(this, cx);
                        (!message.is_empty()).then(|| (false, message.into()))
                    }
                    Err(error) => Some((true, format!("{error:#}").into())),
                };
                this.reload(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle_stage(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(file) = self.review().and_then(|review| review.files.get(index)) else {
            return;
        };
        let paths = [Some(file.path.clone()), file.old_path.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        match file.stage {
            StageState::Committed => {}
            StageState::Staged => self.run_git(
                move |root| {
                    let paths = paths.iter().map(String::as_str).collect::<Vec<_>>();
                    review_git::unstage(root, &paths).map(|()| String::new())
                },
                |_, _| {},
                cx,
            ),
            StageState::Unstaged | StageState::Partial => self.run_git(
                move |root| {
                    let paths = paths.iter().map(String::as_str).collect::<Vec<_>>();
                    review_git::stage(root, &paths).map(|()| String::new())
                },
                |_, _| {},
                cx,
            ),
        }
    }

    fn toggle_selected_stage(&mut self, _: &ToggleStaged, _: &mut Window, cx: &mut Context<Self>) {
        if self.open_file.is_none() {
            self.toggle_stage(self.selected, cx);
        }
    }

    fn stage_all(&mut self, stage: bool, cx: &mut Context<Self>) {
        self.run_git(
            move |root| {
                let result = if stage {
                    review_git::stage(root, &["."])
                } else {
                    review_git::unstage(root, &["."])
                };
                result.map(|()| String::new())
            },
            |_, _| {},
            cx,
        );
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).text().trim().to_string();
        if message.is_empty() {
            self.git_status = Some((true, "Write a commit message.".into()));
            cx.notify();
            return;
        }
        self.run_git(
            move |root| {
                review_git::commit(root, &message).map(|summary| format!("Commit {summary}"))
            },
            |this, cx| {
                this.commit_input
                    .update(cx, |input, cx| input.set_text("", cx));
            },
            cx,
        );
    }

    fn toggle_split(&mut self, _: &ToggleSplitDiff, _: &mut Window, cx: &mut Context<Self>) {
        self.split = !self.split;
        self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn showing_split(&self) -> bool {
        self.split && self.open_file.is_none()
    }

    /// Scrolls the diff to a unified row, wherever it is in the current layout.
    fn scroll_to_row(&self, index: usize, strategy: ScrollStrategy) {
        let index = if self.showing_split() {
            self.split_index.get(index).copied().unwrap_or(0)
        } else {
            index
        };
        self.diff_scroll.scroll_to_item(index, strategy);
    }

    fn watch(&mut self, repo_root: PathBuf, cx: &mut Context<Self>) {
        if self
            .watcher
            .as_ref()
            .is_some_and(|(root, ..)| *root == repo_root)
        {
            return;
        }
        let (sender, mut receiver) = futures::channel::mpsc::unbounded();
        match RepoWatcher::new(&repo_root, move || {
            sender.unbounded_send(()).ok();
        }) {
            Ok(watcher) => {
                let task = cx.spawn(async move |this, cx| {
                    while receiver.next().await.is_some() {
                        if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                            break;
                        }
                    }
                });
                self.watcher = Some((repo_root, watcher, task));
                self.watcher_error = None;
            }
            // Without a watcher the review still works; F5 reloads manually.
            Err(error) => self.watcher_error = Some(format!("{error:#}")),
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = index;
        self.open_file = None;
        self.line_selection = None;
        self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.rebuild_rows();
        self.reveal_in_tree(index);
        self.highlight_selected(cx);
        cx.notify();
    }

    fn select_relative(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(count) = self.review().map(|review| review.files.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        // Leaving an opened file goes back to the diff where it was.
        let index = if self.open_file.is_some() {
            self.selected.min(count - 1)
        } else {
            let order = &self.file_order;
            let position = order
                .iter()
                .position(|&index| index == self.selected)
                .unwrap_or(0);
            order[(position as isize + delta).rem_euclid(order.len() as isize) as usize]
        };
        self.select(index, cx);
    }

    /// Shows `path` at `lines`: as a diff if it changed, otherwise the whole file.
    fn open_path(&mut self, path: &str, lines: Option<(Side, u32, u32)>, cx: &mut Context<Self>) {
        let diff_index = self
            .review()
            .and_then(|review| review.files.iter().position(|file| file.path == path));
        self.pending_lines = lines;
        match diff_index {
            Some(index) => self.select(index, cx),
            None => {
                self.line_selection = None;
                self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
                self.load_open_file(path.to_string(), cx);
            }
        }
    }

    fn load_open_file(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(root) = self.review().map(|review| review.repo_root.clone()) else {
            return;
        };
        if self
            .open_file
            .as_ref()
            .is_none_or(|open_file| open_file.path != path)
        {
            self.open_file = Some(OpenFile {
                path: path.clone(),
                text: None,
            });
            self.rebuild_rows();
        }
        let load = cx.background_spawn({
            let path = path.clone();
            async move {
                let bytes = std::fs::read(root.join(&path)).map_err(|error| error.to_string())?;
                if bytes.len() > MAX_OPEN_FILE_BYTES {
                    return Err("File too large to display.".to_string());
                }
                if bytes.contains(&0) {
                    return Err("Binary file: contents not shown.".to_string());
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let highlights = FileHighlights {
                    old: None,
                    new: review_syntax::highlight_file(&path, &text),
                };
                Ok((text, highlights))
            }
        });
        self.open_file_task = Some(cx.spawn(async move |this, cx| {
            let loaded = load.await;
            this.update(cx, |this, cx| {
                if this
                    .open_file
                    .as_ref()
                    .is_none_or(|open_file| open_file.path != path)
                {
                    return;
                }
                let text = loaded.map(|(text, highlights)| {
                    this.highlights.insert(path.clone(), Arc::new(highlights));
                    text
                });
                if let Some(open_file) = this.open_file.as_mut() {
                    open_file.text = Some(text);
                }
                this.rebuild_rows();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_search(&mut self, mode: SearchMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.review().map(|review| review.repo_root.clone()) else {
            return;
        };
        let modal = cx.new(|cx| SearchModal::new(mode, root, window, cx));
        let subscription = cx.subscribe_in(&modal, window, |this, _, event, window, cx| {
            if let SearchModalEvent::Open { path, line } = event {
                let lines = line.map(|line| (Side::New, line, line));
                this.open_path(path, lines, cx);
            }
            this.close_search(window, cx);
        });
        self.search_modal = Some((modal, subscription));
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_modal = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn click_row(&mut self, index: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        if !matches!(self.rows.get(index), Some(Row::Line { .. })) {
            return;
        }
        self.line_selection = match self.line_selection {
            Some((anchor, _)) if event.modifiers().shift => Some((anchor, index)),
            Some((anchor, head)) if anchor == index && head == index => None,
            _ => Some((index, index)),
        };
        cx.notify();
    }

    fn selected_rows(&self) -> Option<Range<usize>> {
        let (anchor, head) = self.line_selection?;
        Some(anchor.min(head)..anchor.max(head) + 1)
    }

    /// The anchor for a new thread: the selected lines, or the whole file.
    fn thread_anchor(&self) -> Option<ThreadAnchor> {
        let path = self.current_path()?.to_string();
        let Some(range) = self.selected_rows() else {
            return Some(ThreadAnchor {
                path,
                lines: None,
                excerpt: String::new(),
            });
        };
        let rows = &self.rows[range];
        let mut excerpt = String::new();
        for row in rows {
            if let Row::Line { kind, text, .. } = row {
                let prefix = match kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                excerpt.push(prefix);
                excerpt.push_str(text);
                excerpt.push('\n');
            }
        }
        // Refer to new-side line numbers unless only removed lines are selected.
        let side = if rows.iter().any(|row| row.line_number(Side::New).is_some()) {
            Side::New
        } else {
            Side::Old
        };
        let numbers = rows.iter().filter_map(|row| row.line_number(side));
        let (start, end) = numbers.fold((u32::MAX, 0), |(min, max), n| (min.min(n), max.max(n)));
        Some(ThreadAnchor {
            path,
            lines: (start <= end).then_some((side, start, end)),
            excerpt,
        })
    }

    fn new_thread(&mut self, _: &NewThread, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.thread_anchor() else {
            return;
        };
        self.show_agent_panel = true;
        self.agent_panel
            .update(cx, |panel, cx| panel.new_thread(anchor, window, cx));
        cx.notify();
    }

    fn toggle_agent_panel(
        &mut self,
        _: &ToggleAgentPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_agent_panel = !self.show_agent_panel;
        if self.show_agent_panel {
            self.agent_panel
                .update(cx, |panel, cx| panel.focus_composer(window, cx));
        } else {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn on_agent_panel_event(
        &mut self,
        _: &Entity<AgentPanel>,
        event: &AgentPanelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            AgentPanelEvent::FilesChanged => self.reload(cx),
            AgentPanelEvent::Reveal(anchor) => self.reveal(anchor, cx),
        }
    }

    /// Shows the anchor's file and scrolls to (and selects) its lines.
    fn reveal(&mut self, anchor: &ThreadAnchor, cx: &mut Context<Self>) {
        self.open_path(&anchor.path, anchor.lines, cx);
    }

    /// Selects and scrolls to `pending_lines` if the current rows contain them.
    fn apply_pending_lines(&mut self) {
        let Some((side, start, end)) = self.pending_lines else {
            return;
        };
        let in_range = |row: &Row| {
            row.line_number(side)
                .is_some_and(|n| (start..=end).contains(&n))
        };
        let first = self.rows.iter().position(in_range);
        let last = self.rows.iter().rposition(in_range);
        if let (Some(first), Some(last)) = (first, last) {
            self.pending_lines = None;
            self.line_selection = Some((first, last));
            self.scroll_to_row(first, ScrollStrategy::Center);
        }
    }

    /// Line ranges of the selected file that have a thread.
    fn thread_ranges(&self, cx: &Context<Self>) -> Vec<(Side, u32, u32)> {
        let Some(path) = self.current_path() else {
            return Vec::new();
        };
        self.agent_panel
            .read(cx)
            .threads()
            .iter()
            .filter(|thread| thread.anchor.path == path)
            .filter_map(|thread| thread.anchor.lines)
            .collect()
    }

    /// Highlights the selected file in the background, if it isn't cached yet.
    fn highlight_selected(&mut self, cx: &mut Context<Self>) {
        if self.open_file.is_some() {
            return;
        }
        let Some(file) = self.selected_file() else {
            return;
        };
        if self.highlights.contains_key(&file.path) {
            return;
        }
        let path = file.path.clone();
        let old_path = file.old_path.clone().unwrap_or_else(|| path.clone());
        let old_text = file.old_text.clone();
        let new_text = file.new_text.clone();
        let highlight = cx.background_spawn(async move {
            FileHighlights {
                old: old_text.and_then(|text| review_syntax::highlight_file(&old_path, &text)),
                new: new_text.and_then(|text| review_syntax::highlight_file(&path, &text)),
            }
        });
        let path = file.path.clone();
        self.highlight_task = Some(cx.spawn(async move |this, cx| {
            let highlights = highlight.await;
            this.update(cx, |this, cx| {
                this.highlights.insert(path.clone(), Arc::new(highlights));
                let showing = this.open_file.is_none()
                    && this.selected_file().is_some_and(|file| file.path == path);
                if showing {
                    this.rebuild_rows();
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(open_file) = &self.open_file {
            match &open_file.text {
                None => rows.push(Row::Notice("Loading…".into())),
                Some(Err(error)) => rows.push(Row::Notice(error.clone().into())),
                Some(Ok(text)) => {
                    let highlights = self
                        .highlights
                        .get(&open_file.path)
                        .cloned()
                        .unwrap_or_default();
                    for (index, line) in text.lines().enumerate() {
                        let text = review_syntax::normalize_line(line);
                        let highlights = highlights
                            .new
                            .as_ref()
                            .and_then(|lines| lines.get(index))
                            .filter(|highlighted| highlighted.text == text)
                            .map(|highlighted| {
                                highlighted
                                    .spans
                                    .iter()
                                    .map(|(range, token)| {
                                        (range.clone(), theme::token_style(*token))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        rows.push(Row::Line {
                            kind: LineKind::Context,
                            old_lineno: None,
                            new_lineno: Some(index as u32 + 1),
                            text: text.into(),
                            highlights,
                        });
                    }
                }
            }
        } else if let Some(file) = self.selected_file() {
            let highlights = self.highlights.get(&file.path).cloned().unwrap_or_default();
            if file.binary {
                rows.push(Row::Notice("Binary file: contents not shown.".into()));
            } else if file.hunks.is_empty() {
                rows.push(Row::Notice("No content changes.".into()));
            }
            for hunk in &file.hunks {
                rows.push(Row::HunkHeader(hunk.header.clone().into()));
                for line in &hunk.lines {
                    let (side, lineno) = match line.kind {
                        LineKind::Removed => (&highlights.old, line.old_lineno),
                        LineKind::Added | LineKind::Context => (&highlights.new, line.new_lineno),
                    };
                    let text = review_syntax::normalize_line(&line.text);
                    let highlighted = side
                        .as_ref()
                        .zip(lineno)
                        .and_then(|(lines, lineno)| lines.get(lineno as usize - 1))
                        // Guard against the file changing between diffing and reading it.
                        .filter(|highlighted| highlighted.text == text);
                    let highlights = highlighted
                        .map(|highlighted| {
                            highlighted
                                .spans
                                .iter()
                                .map(|(range, token)| (range.clone(), theme::token_style(*token)))
                                .collect()
                        })
                        .unwrap_or_default();
                    rows.push(Row::Line {
                        kind: line.kind,
                        old_lineno: line.old_lineno,
                        new_lineno: line.new_lineno,
                        text: text.into(),
                        highlights,
                    });
                }
            }
        }
        self.widest_row = rows
            .iter()
            .enumerate()
            .max_by_key(|(_, row)| match row {
                Row::Line { text, .. } => text.len(),
                Row::HunkHeader(text) | Row::Notice(text) => text.len(),
            })
            .map(|(index, _)| index);
        if self
            .line_selection
            .is_some_and(|(anchor, head)| anchor.max(head) >= rows.len())
        {
            self.line_selection = None;
        }
        let tags = rows.iter().map(Row::tag).collect::<Vec<_>>();
        let split_rows = crate::split_diff::split_rows(&tags);
        self.split_index = crate::split_diff::unified_to_split(&split_rows, rows.len());
        self.split_rows = Rc::new(split_rows);
        self.rows = Rc::new(rows);
        self.apply_pending_lines();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut header = div()
            .h(px(36.))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border());
        let repo_name = match self.review() {
            Some(review) => review.repo_root.clone(),
            None => self.repo_path.clone(),
        };
        let repo_name = repo_name
            .components()
            .next_back()
            .map(|name| name.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        header = header.child(div().font_weight(FontWeight::SEMIBOLD).child(repo_name));
        if let Some(review) = self.review() {
            if let Some(head_label) = &review.head_label {
                header = header.child(div().text_color(theme::accent()).child(head_label.clone()));
            }
            let additions: usize = review.files.iter().map(|file| file.additions).sum();
            let deletions: usize = review.files.iter().map(|file| file.deletions).sum();
            header = header
                .child(
                    div()
                        .text_color(theme::muted())
                        .child(format!("compared with {}", review.base_label)),
                )
                .child(div().flex_1())
                .child(match review.files.len() {
                    1 => "1 file".to_string(),
                    count => format!("{count} files"),
                })
                .child(
                    div()
                        .text_color(theme::added_text())
                        .child(format!("+{additions}")),
                )
                .child(
                    div()
                        .text_color(theme::removed_text())
                        .child(format!("−{deletions}")),
                );
            // Auto-reload is the expected default; only its absence is worth a mention.
            if self.watcher_error.is_some() {
                header = header.child(
                    div()
                        .text_color(theme::warning())
                        .child("Auto-refresh unavailable · F5 to reload"),
                );
            }
        } else {
            header = header.child(div().flex_1());
        }
        header.children(self.render_update(cx)).child(
            div()
                .text_color(theme::muted())
                .child("Ctrl+P file · Ctrl+Shift+F search · Alt+T thread · Alt+A agent"),
        )
    }

    fn render_update(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_2()
                .rounded_md()
                .cursor_pointer()
                .bg(theme::accent_background())
                .hover(|style| style.bg(theme::selection()))
                .child(label)
        };
        let (message, color, action) = match &self.update {
            UpdateState::None => return None,
            UpdateState::Available(release) => (
                format!("Galley {} available", release.version),
                theme::accent(),
                Some(
                    button("install-update", "Update")
                        .on_click(cx.listener(|this, _, _, cx| this.install_update(cx))),
                ),
            ),
            UpdateState::Installing(version) => {
                (format!("Updating to {version}…"), theme::muted(), None)
            }
            UpdateState::Installed(version) => (
                format!("Galley {version} installed"),
                theme::accent(),
                Some(
                    button("restart", "Restart")
                        .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
                ),
            ),
            UpdateState::Failed(_, error) => (
                format!("Update failed: {error}"),
                theme::warning(),
                Some(
                    button("retry-update", "Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.install_update(cx))),
                ),
            ),
            UpdateState::RestartFailed(error) => (
                format!("Updated, but couldn't restart ({error}); reopen Galley"),
                theme::warning(),
                None,
            ),
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .max_w(px(420.))
                        .truncate()
                        .text_color(color)
                        .child(message),
                )
                .children(action)
                .into_any_element(),
        )
    }

    fn render_file_list(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(320.))
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        uniform_list(
                            "files",
                            self.tree_entries.len(),
                            cx.processor(|this, range: Range<usize>, _window, cx| {
                                let entries = this.tree_entries.clone();
                                range
                                    .filter_map(|entry_index| {
                                        this.render_tree_entry(
                                            &entries[entry_index],
                                            entry_index,
                                            cx,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.file_scroll)
                        .size_full(),
                    )
                    .custom_scrollbars(theme::vertical_scrollbar(&self.file_scroll), window, cx),
            )
            .child(self.render_commit_panel(cx))
    }

    fn render_commit_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self
            .review()
            .map(|review| review.files.as_slice())
            .unwrap_or_default();
        let staged = files
            .iter()
            .filter(|file| matches!(file.stage, StageState::Staged | StageState::Partial))
            .count();
        let link = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .text_color(theme::muted())
                .hover(|style| style.bg(theme::hover()).text_color(theme::text()))
                .child(label)
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
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme::muted())
                            .child(match staged {
                                1 => "1 file staged".to_string(),
                                count => format!("{count} files staged"),
                            }),
                    )
                    .child(
                        link("stage-all", "Stage all")
                            .on_click(cx.listener(|this, _, _, cx| this.stage_all(true, cx))),
                    )
                    .child(
                        link("unstage-all", "Unstage all")
                            .on_click(cx.listener(|this, _, _, cx| this.stage_all(false, cx))),
                    ),
            )
            .child(
                div()
                    .p_2()
                    .min_h(px(56.))
                    .max_h(px(160.))
                    .overflow_hidden()
                    .rounded_md()
                    .bg(theme::background())
                    .border_1()
                    .border_color(theme::border())
                    .child(self.commit_input.clone()),
            )
            .child(
                div()
                    .id("commit")
                    .py_1()
                    .rounded_md()
                    .flex()
                    .justify_center()
                    .when(staged > 0, |button| {
                        button
                            .cursor_pointer()
                            .bg(theme::accent_background())
                            .hover(|style| style.bg(theme::selection()))
                            .on_click(cx.listener(|this, _, _, cx| this.commit(cx)))
                    })
                    .when(staged == 0, |button| {
                        button.bg(theme::hover()).text_color(theme::muted())
                    })
                    .child("Commit"),
            )
            .when_some(self.git_status.clone(), |panel, (is_error, message)| {
                panel.child(
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
    }

    fn render_tree_entry(
        &self,
        entry: &TreeEntry,
        entry_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let indent = |depth: usize| px(12. + 14. * depth as f32);
        let row = div()
            .id(("tree-entry", entry_index))
            .w_full()
            .h(px(26.))
            .pr_3()
            .flex()
            .items_center()
            .gap_1p5()
            .cursor_pointer()
            .whitespace_nowrap();
        match entry {
            TreeEntry::Dir {
                path,
                name,
                depth,
                collapsed,
            } => {
                let path = path.clone();
                Some(
                    row.pl(indent(*depth))
                        .text_color(theme::muted())
                        .hover(|style| style.bg(theme::hover()))
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_dir(&path, cx)))
                        .child(div().w(px(12.)).flex_none().child(if *collapsed {
                            "▸"
                        } else {
                            "▾"
                        }))
                        .child(div().min_w(px(0.)).truncate().child(name.clone()))
                        .into_any_element(),
                )
            }
            TreeEntry::File { index, name, depth } => {
                let file = self.review()?.files.get(*index)?;
                let index = *index;
                let is_selected = self.open_file.is_none() && index == self.selected;
                Some(
                    row.pl(indent(*depth))
                        .when(is_selected, |row| row.bg(theme::selection()))
                        .when(!is_selected, |row| {
                            row.hover(|style| style.bg(theme::hover()))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
                        .child(stage_checkbox(file.stage, index, cx))
                        .child(
                            div()
                                .w(px(12.))
                                .flex_none()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme::status_color(file.status))
                                .child(file.status.letter().to_string()),
                        )
                        .child(div().flex_1().min_w(px(0.)).truncate().child(name.clone()))
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme::added_text())
                                .child(format!("+{}", file.additions)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme::removed_text())
                                .child(format!("−{}", file.deletions)),
                        )
                        .into_any_element(),
                )
            }
        }
    }

    fn render_diff(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match (&self.open_file, self.selected_file()) {
            (Some(open_file), _) => format!("{} · no changes", open_file.path),
            (None, Some(file)) => match &file.old_path {
                Some(old_path) => format!("{old_path} → {}", file.path),
                None => file.path.clone(),
            },
            (None, None) => "No changes to review.".to_string(),
        };
        let thread_hint = match self.selected_rows() {
            Some(rows) if rows.len() == 1 => "Alt+T: thread on 1 line".to_string(),
            Some(rows) => format!("Alt+T: thread on {} lines", rows.len()),
            None => "Alt+T: thread on the file".to_string(),
        };
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(30.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().flex_1().font_family(theme::CODE_FONT).child(title))
                    .when(
                        self.open_file.is_none() && self.selected_file().is_some(),
                        |header| {
                            header.child(
                                div()
                                    .id("toggle-split")
                                    .px_2()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_color(theme::muted())
                                    .hover(|style| {
                                        style.bg(theme::hover()).text_color(theme::text())
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_split(&ToggleSplitDiff, window, cx)
                                    }))
                                    .child(if self.split {
                                        "Alt+S: unificado"
                                    } else {
                                        "Alt+S: side by side"
                                    }),
                            )
                        },
                    )
                    .when(self.current_path().is_some(), |header| {
                        header.child(
                            div()
                                .id("new-thread")
                                .px_2()
                                .rounded_md()
                                .cursor_pointer()
                                .text_color(theme::muted())
                                .hover(|style| style.bg(theme::hover()).text_color(theme::text()))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.new_thread(&NewThread, window, cx)
                                }))
                                .child(thread_hint),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(if self.showing_split() {
                        self.render_split_list(cx).into_any_element()
                    } else {
                        self.render_unified_list(cx).into_any_element()
                    })
                    .custom_scrollbars(theme::vertical_scrollbar(&self.diff_scroll), window, cx),
            )
    }

    fn render_unified_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "diff",
            self.rows.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let rows = this.rows.clone();
                let selection = this.selected_rows().unwrap_or(0..0);
                let thread_ranges = this.thread_ranges(cx);
                range
                    .map(|index| {
                        let row = &rows[index];
                        let has_thread = thread_ranges.iter().any(|(side, start, end)| {
                            row.line_number(*side)
                                .is_some_and(|n| (*start..=*end).contains(&n))
                        });
                        render_row(row, selection.contains(&index), has_thread)
                            .id(("diff-row", index))
                            .on_click(cx.listener(move |this, event, _, cx| {
                                this.click_row(index, event, cx)
                            }))
                            .into_any_element()
                    })
                    .collect::<Vec<AnyElement>>()
            }),
        )
        .track_scroll(&self.diff_scroll)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(self.widest_row)
        .size_full()
        .font_family(theme::CODE_FONT)
        .text_size(theme::CODE_TEXT_SIZE)
    }

    fn render_split_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        uniform_list(
            "split-diff",
            self.split_rows.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                let rows = this.rows.clone();
                let split_rows = this.split_rows.clone();
                let selection = this.selected_rows().unwrap_or(0..0);
                let thread_ranges = this.thread_ranges(cx);
                let has_thread = |row: &Row| {
                    thread_ranges.iter().any(|(side, start, end)| {
                        row.line_number(*side)
                            .is_some_and(|n| (*start..=*end).contains(&n))
                    })
                };
                range
                    .map(|split_index| match split_rows[split_index] {
                        SplitRow::Banner(index) => render_row(&rows[index], false, false)
                            .id(("split-row", split_index))
                            .into_any_element(),
                        SplitRow::Pair { left, right } => {
                            let cell = |index: Option<usize>, side: Side| {
                                let Some(index) = index else {
                                    return div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .h_full()
                                        .bg(theme::empty_cell())
                                        .into_any_element();
                                };
                                let row = &rows[index];
                                render_split_cell(
                                    row,
                                    side,
                                    selection.contains(&index),
                                    has_thread(row),
                                )
                                .id(("split-cell", index * 2 + side as usize))
                                .on_click(cx.listener(move |this, event, _, cx| {
                                    this.click_row(index, event, cx)
                                }))
                                .into_any_element()
                            };
                            div()
                                .id(("split-row", split_index))
                                .h(theme::ROW_HEIGHT)
                                .w_full()
                                .flex()
                                .child(cell(left, Side::Old))
                                .child(div().w(px(1.)).h_full().flex_none().bg(theme::border()))
                                .child(cell(right, Side::New))
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<AnyElement>>()
            }),
        )
        .track_scroll(&self.diff_scroll)
        .size_full()
        .font_family(theme::CODE_FONT)
        .text_size(theme::CODE_TEXT_SIZE)
    }
}

/// Checkbox showing whether a file is staged; clicking stages or unstages it.
fn stage_checkbox(stage: StageState, index: usize, cx: &mut Context<ReviewView>) -> AnyElement {
    if stage == StageState::Committed {
        return div().w(px(14.)).flex_none().into_any_element();
    }
    let (mark, filled) = match stage {
        StageState::Staged => ("✓", true),
        StageState::Partial => ("–", false),
        _ => ("", false),
    };
    div()
        .id(("stage", index))
        .size(px(14.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .text_size(px(10.))
        .border_color(if stage == StageState::Unstaged {
            theme::muted()
        } else {
            theme::accent()
        })
        .when(filled, |checkbox| {
            checkbox.bg(theme::accent()).text_color(theme::background())
        })
        .when(!filled, |checkbox| checkbox.text_color(theme::accent()))
        .hover(|style| style.border_color(theme::text()))
        .on_click(cx.listener(move |this, _, _, cx| {
            // Don't also select the row.
            cx.stop_propagation();
            this.toggle_stage(index, cx);
        }))
        .child(mark)
        .into_any_element()
}

/// One half of a side-by-side row: the old side (left) or the new side (right).
fn render_split_cell(row: &Row, side: Side, selected: bool, has_thread: bool) -> gpui::Div {
    let Row::Line {
        kind,
        old_lineno,
        new_lineno,
        text,
        highlights,
    } = row
    else {
        return div();
    };
    let (background, marker, marker_color) = match kind {
        LineKind::Added => (Some(theme::added_background()), "+", theme::added_text()),
        LineKind::Removed => (
            Some(theme::removed_background()),
            "-",
            theme::removed_text(),
        ),
        LineKind::Context => (None, " ", theme::muted()),
    };
    let lineno = match side {
        Side::Old => *old_lineno,
        Side::New => *new_lineno,
    };
    div()
        .relative()
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .cursor_pointer()
        .when_some(background, |cell, background| cell.bg(background))
        .child(
            div()
                .w(px(3.))
                .h_full()
                .flex_none()
                .when(has_thread, |strip| strip.bg(theme::accent())),
        )
        .child(gutter(lineno))
        .child(
            div()
                .w(px(18.))
                .flex_none()
                .pl_1()
                .text_color(marker_color)
                .child(marker),
        )
        .child(
            div()
                .flex_none()
                .child(StyledText::new(text.clone()).with_highlights(highlights.clone())),
        )
        .when(selected, |cell| {
            cell.child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(theme::line_selection())
                    .border_l_2()
                    .border_color(theme::accent()),
            )
        })
}

fn render_row(row: &Row, selected: bool, has_thread: bool) -> gpui::Div {
    let base = div()
        .h(theme::ROW_HEIGHT)
        .min_w_full()
        .flex()
        .items_center()
        .whitespace_nowrap()
        .font_family(theme::CODE_FONT);
    match row {
        Row::Notice(text) => base.px_3().text_color(theme::muted()).child(text.clone()),
        Row::HunkHeader(header) => base
            .px_3()
            .bg(theme::hunk_background())
            .text_color(theme::hunk_text())
            .child(header.clone()),
        Row::Line {
            kind,
            old_lineno,
            new_lineno,
            text,
            highlights,
        } => {
            let (background, marker, marker_color) = match kind {
                LineKind::Added => (Some(theme::added_background()), "+", theme::added_text()),
                LineKind::Removed => (
                    Some(theme::removed_background()),
                    "-",
                    theme::removed_text(),
                ),
                LineKind::Context => (None, " ", theme::muted()),
            };
            base.cursor_pointer()
                .when_some(background, |row, background| row.bg(background))
                .child(
                    // Thread marker strip.
                    div()
                        .w(px(3.))
                        .h_full()
                        .flex_none()
                        .when(has_thread, |strip| strip.bg(theme::accent())),
                )
                .child(gutter(*old_lineno))
                .child(gutter(*new_lineno))
                .child(
                    div()
                        .w(px(18.))
                        .flex_none()
                        .pl_1()
                        .text_color(marker_color)
                        .child(marker),
                )
                .child(
                    div()
                        .flex_none()
                        .pr_4()
                        .child(StyledText::new(text.clone()).with_highlights(highlights.clone())),
                )
                // Painted last so the selection tint covers the add/remove background.
                .when(selected, |row| {
                    row.child(
                        div()
                            .absolute()
                            .inset_0()
                            .bg(theme::line_selection())
                            .border_l_2()
                            .border_color(theme::accent()),
                    )
                })
                .relative()
        }
    }
}

fn gutter(lineno: Option<u32>) -> impl IntoElement {
    div()
        .w(px(48.))
        .flex_none()
        .pr_2()
        .flex()
        .justify_end()
        .text_color(theme::muted())
        .child(lineno.map(|lineno| lineno.to_string()).unwrap_or_default())
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            LoadState::Loading => {
                centered(div().text_color(theme::muted()).child("Loading changes…"))
            }
            LoadState::Failed(error) => {
                centered(div().text_color(theme::error()).child(error.clone()))
            }
            LoadState::Loaded(_) => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .child(self.render_file_list(window, cx))
                .child(self.render_diff(window, cx))
                .when(self.show_agent_panel, |body| {
                    body.child(self.agent_panel.clone())
                })
                .into_any_element(),
        };
        div()
            .key_context("ReviewView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &NextFile, _, cx| this.select_relative(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousFile, _, cx| this.select_relative(-1, cx)))
            .on_action(cx.listener(|this, _: &Reload, _, cx| this.reload(cx)))
            .on_action(cx.listener(Self::new_thread))
            .on_action(cx.listener(Self::toggle_agent_panel))
            .on_action(cx.listener(Self::toggle_split))
            .on_action(cx.listener(Self::toggle_selected_stage))
            .on_action(cx.listener(|this, _: &OpenFileFinder, window, cx| {
                this.open_search(SearchMode::Files, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenTextSearch, window, cx| {
                this.open_search(SearchMode::Text, window, cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .font_family(theme::UI_FONT)
            .text_size(theme::UI_TEXT_SIZE)
            .child(self.render_header(cx))
            .child(body)
            .when_some(self.search_modal.as_ref(), |root, (modal, _)| {
                root.child(
                    div()
                        .id("search-backdrop")
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .items_start()
                        .pt(px(72.))
                        .bg(theme::backdrop())
                        .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx)))
                        .child(modal.clone()),
                )
            })
    }
}

fn centered(content: impl IntoElement) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .p_8()
        .child(content)
        .into_any_element()
}
