//! A plain-text input, single or multi-line (soft wrapped), with IME support.
//! Adapted from GPUI's `examples/input.rs` (Apache-2.0).

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding,
    KeyContext, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, SharedString, Style, TextRun, UTF16Selection, Window, WrappedLine, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::theme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Newline,
        Submit,
    ]
);

pub fn bind_keys(cx: &mut App) {
    const MULTILINE: Option<&str> = Some("TextInput && multiline");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("up", Up, MULTILINE),
        KeyBinding::new("down", Down, MULTILINE),
        KeyBinding::new("shift-up", SelectUp, MULTILINE),
        KeyBinding::new("shift-down", SelectDown, MULTILINE),
        KeyBinding::new("secondary-a", SelectAll, Some("TextInput")),
        KeyBinding::new("secondary-v", Paste, Some("TextInput")),
        KeyBinding::new("secondary-c", Copy, Some("TextInput")),
        KeyBinding::new("secondary-x", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput && submit_on_enter")),
        KeyBinding::new(
            "enter",
            Newline,
            Some("TextInput && multiline && !submit_on_enter"),
        ),
        KeyBinding::new("secondary-enter", Submit, Some("TextInput")),
        KeyBinding::new("shift-enter", Newline, MULTILINE),
    ]);
}

pub enum TextInputEvent {
    Changed,
    Submitted,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    multiline: bool,
    /// When false (e.g. commit messages), Enter inserts a newline and only
    /// Ctrl/Cmd+Enter submits.
    submit_on_enter: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    /// Layout from the last paint, used for hit testing and cursor movement.
    last_layout: Option<TextLayout>,
    is_selecting: bool,
}

/// The shaped lines of the last paint. Each entry is one hard (newline separated)
/// line, which may wrap into several visual rows.
struct TextLayout {
    lines: Vec<LaidOutLine>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
}

struct LaidOutLine {
    line: WrappedLine,
    /// Byte offset of the line start in the content.
    start: usize,
    /// Vertical offset from the top of the input.
    top: Pixels,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub fn new(
        placeholder: impl Into<SharedString>,
        multiline: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            multiline,
            submit_on_enter: true,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            is_selecting: false,
        }
    }

    pub fn submit_on_enter(mut self, submit_on_enter: bool) -> Self {
        self.submit_on_enter = submit_on_enter;
        self
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical_offset(-1);
        self.move_to(offset, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical_offset(1);
        self.move_to(offset, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical_offset(-1);
        self.select_to(offset, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.vertical_offset(1);
        self.select_to(offset, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let line_start = self.content[..self.cursor_offset()]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.move_to(line_start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let line_end = self.content[cursor..]
            .find('\n')
            .map_or(self.content.len(), |index| cursor + index);
        self.move_to(line_end, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let previous = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == previous {
                return;
            }
            self.select_to(previous, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submitted);
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let offset = self.index_for_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n");
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.copy(&Copy, window, cx);
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    /// The offset one visual row above (`-1`) or below (`1`) the cursor.
    fn vertical_offset(&self, direction: i32) -> usize {
        let Some(layout) = &self.last_layout else {
            return self.cursor_offset();
        };
        let Some(position) = layout.position_for_offset(self.cursor_offset()) else {
            return self.cursor_offset();
        };
        let target = point(
            position.x,
            position.y + layout.line_height * direction as f32 + layout.line_height / 2.,
        );
        if target.y < layout.bounds.top() {
            0
        } else if target.y > layout.bounds.bottom() {
            self.content.len()
        } else {
            layout.offset_for_position(target)
        }
    }

    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        match &self.last_layout {
            Some(layout) if !self.content.is_empty() => layout.offset_for_position(position),
            _ => 0,
        }
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }
}

impl TextLayout {
    fn line_for_offset(&self, offset: usize) -> Option<&LaidOutLine> {
        self.lines.iter().rev().find(|line| line.start <= offset)
    }

    /// Window position of the top-left corner of the character at `offset`.
    fn position_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
        let line = self.line_for_offset(offset)?;
        let position = line
            .line
            .position_for_index(offset - line.start, self.line_height)?;
        Some(self.bounds.origin + point(position.x, position.y + line.top))
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let local_y = position.y - self.bounds.top();
        let line = self
            .lines
            .iter()
            .rev()
            .find(|line| line.top <= local_y)
            .or(self.lines.first());
        let Some(line) = line else {
            return 0;
        };
        let local = point(position.x - self.bounds.left(), local_y - line.top);
        let index = match line
            .line
            .closest_index_for_position(local, self.line_height)
        {
            Ok(index) | Err(index) => index,
        };
        line.start + index.min(line.line.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let new_text = if self.multiline {
            new_text.replace("\r\n", "\n")
        } else {
            new_text.replace(['\r', '\n'], " ")
        };

        self.content =
            (self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.marked_range =
            (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let start = layout.position_for_offset(range.start)?;
        let end = layout.position_for_offset(range.end)?;
        Some(Bounds::from_corners(
            start,
            point(end.x.max(start.x), end.y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.last_layout.as_ref()?;
        Some(self.offset_to_utf16(layout.offset_for_position(point)))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    lines: Vec<LaidOutLine>,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl TextElement {
    /// Shapes the text (or placeholder) wrapped to `width`, one entry per hard line.
    fn shape(input: &TextInput, width: Option<Pixels>, window: &mut Window) -> Vec<LaidOutLine> {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let (text, color) = if input.content.is_empty() {
            (input.placeholder.clone(), theme::muted().into())
        } else {
            (input.content.clone(), style.color)
        };
        let base_run = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match input
            .marked_range
            .as_ref()
            .filter(|_| !input.content.is_empty())
        {
            Some(marked) => vec![
                TextRun {
                    len: marked.start,
                    ..base_run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(gpui::UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..base_run.clone()
                },
                TextRun {
                    len: text.len() - marked.end,
                    ..base_run
                },
            ],
            None => vec![base_run],
        };
        let wrapped = window
            .text_system()
            .shape_text(text, font_size, &runs, width, None)
            .unwrap_or_default();

        let mut lines = Vec::with_capacity(wrapped.len());
        let (mut start, mut top) = (0, px(0.));
        for line in wrapped {
            let height = line.size(line_height).height;
            let len = line.len();
            lines.push(LaidOutLine { line, start, top });
            start += len + 1;
            top += height;
        }
        lines
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.clone();
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let line_height = window.line_height();
                let input = input.read(cx);
                let height = if input.multiline {
                    TextElement::shape(input, width, window)
                        .iter()
                        .map(|line| line.line.size(line_height).height)
                        .fold(px(0.), |total, height| total + height)
                        .max(line_height)
                } else {
                    line_height
                };
                size(width.unwrap_or(px(0.)), height)
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let wrap_width = input.multiline.then_some(bounds.size.width);
        let lines = TextElement::shape(input, wrap_width, window);
        let line_height = window.line_height();
        let position = |offset: usize| -> Option<Point<Pixels>> {
            let line = lines.iter().rev().find(|line| line.start <= offset)?;
            let position = line
                .line
                .position_for_index(offset - line.start, line_height)?;
            Some(bounds.origin + point(position.x, position.y + line.top))
        };

        let selected_range = input.selected_range.clone();
        let mut selections = Vec::new();
        let mut cursor = None;
        if input.content.is_empty() || selected_range.is_empty() {
            let offset = if input.content.is_empty() {
                0
            } else {
                input.cursor_offset()
            };
            if let Some(position) = position(offset) {
                cursor = Some(fill(
                    Bounds::new(position, size(px(1.5), line_height)),
                    theme::text(),
                ));
            }
        } else if let (Some(start), Some(end)) =
            (position(selected_range.start), position(selected_range.end))
        {
            let color = theme::selection();
            let mut row_top = start.y;
            while row_top <= end.y {
                let left = if row_top == start.y {
                    start.x
                } else {
                    bounds.left()
                };
                let right = if row_top == end.y {
                    end.x
                } else {
                    bounds.right()
                };
                if right > left {
                    selections.push(fill(
                        Bounds::from_corners(
                            point(left, row_top),
                            point(right, row_top + line_height),
                        ),
                        color,
                    ));
                }
                row_top += line_height;
            }
        }

        PrepaintState {
            lines,
            cursor,
            selections,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for selection in prepaint.selections.drain(..) {
            window.paint_quad(selection);
        }
        let line_height = window.line_height();
        for line in &prepaint.lines {
            line.line
                .paint(
                    bounds.origin + point(px(0.), line.top),
                    line_height,
                    gpui::TextAlign::Left,
                    Some(bounds),
                    window,
                    cx,
                )
                .ok();
        }
        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let lines = std::mem::take(&mut prepaint.lines);
        self.input.update(cx, |input, _| {
            // The placeholder isn't content: keep no layout so hit testing maps to 0.
            input.last_layout = (!input.content.is_empty()).then_some(TextLayout {
                lines,
                bounds,
                line_height,
            });
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("TextInput");
        if self.multiline {
            key_context.add("multiline");
        }
        if self.submit_on_enter {
            key_context.add("submit_on_enter");
        }
        div()
            .w_full()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
