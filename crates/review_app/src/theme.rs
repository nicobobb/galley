use std::sync::Arc;

use gpui::{App, HighlightStyle, Hsla, Pixels, Rgba, px, rgb, rgba};
use review_git::FileStatus;
use review_syntax::Token;
use ui::{ScrollAxes, ScrollableHandle, Scrollbars};

#[cfg(target_os = "windows")]
pub const UI_FONT: &str = "Segoe UI";
#[cfg(target_os = "windows")]
pub const CODE_FONT: &str = "Consolas";
#[cfg(target_os = "macos")]
pub const UI_FONT: &str = ".SystemUIFont";
#[cfg(target_os = "macos")]
pub const CODE_FONT: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const UI_FONT: &str = "Noto Sans";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const CODE_FONT: &str = "DejaVu Sans Mono";

pub const UI_TEXT_SIZE: Pixels = px(13.);
pub const CODE_TEXT_SIZE: Pixels = px(13.);
pub const ROW_HEIGHT: Pixels = px(20.);

/// Sets up the theme that Zed's `ui` components read their colors from, tuned to our palette.
pub fn init(cx: &mut App) {
    ::theme::init(::theme::LoadThemes::JustBase, cx);
    let mut theme = (**::theme::GlobalTheme::theme(cx)).clone();
    let colors = &mut theme.styles.colors;
    // The base theme hides the thumb until it's hovered; keep it visible.
    colors.scrollbar_thumb_background = rgb(0x4a4d55).into();
    colors.scrollbar_thumb_hover_background = rgb(0x5d616b).into();
    colors.scrollbar_thumb_active_background = rgb(0x6e727d).into();
    colors.surface_background = background().into();
    ::theme::GlobalTheme::update_theme(cx, Arc::new(theme));
}

/// A vertical scrollbar that stays visible, for a pane scrolled through `handle`.
pub fn vertical_scrollbar<H: ScrollableHandle>(handle: &H) -> Scrollbars<H> {
    Scrollbars::always_visible(ScrollAxes::Vertical).tracked_scroll_handle(handle)
}

pub fn background() -> Rgba {
    rgb(0x1e1f22)
}
pub fn panel() -> Rgba {
    rgb(0x25272b)
}
pub fn border() -> Rgba {
    rgb(0x35373c)
}
pub fn text() -> Rgba {
    rgb(0xd4d6db)
}
pub fn muted() -> Rgba {
    rgb(0x8b8f98)
}
pub fn selection() -> Rgba {
    rgb(0x2d3f5f)
}
pub fn hover() -> Rgba {
    rgb(0x2c2f35)
}
pub fn error() -> Rgba {
    rgb(0xf47174)
}
pub fn warning() -> Rgba {
    rgb(0xe5c07b)
}

pub fn added_text() -> Rgba {
    rgb(0x73c991)
}
pub fn removed_text() -> Rgba {
    rgb(0xf47174)
}
pub fn added_background() -> Rgba {
    rgba(0x2ea04326)
}
pub fn removed_background() -> Rgba {
    rgba(0xf8514926)
}
pub fn hunk_background() -> Rgba {
    rgba(0x388bfd1a)
}
pub fn hunk_text() -> Rgba {
    rgb(0x7aa2f7)
}

pub fn status_color(status: FileStatus) -> Rgba {
    match status {
        FileStatus::Added | FileStatus::Untracked => added_text(),
        FileStatus::Deleted => removed_text(),
        FileStatus::Renamed | FileStatus::TypeChanged => rgb(0x7aa2f7),
        FileStatus::Modified => rgb(0xe5c07b),
    }
}

pub fn token_style(token: Token) -> HighlightStyle {
    let color: u32 = match token {
        Token::Attribute => 0xd19a66,
        Token::Comment => 0x7f848e,
        Token::Constant => 0xd19a66,
        Token::Function => 0x61afef,
        Token::Keyword => 0xc678dd,
        Token::Number => 0xd19a66,
        Token::Operator => 0x56b6c2,
        Token::Property => 0xe06c75,
        Token::Punctuation => 0xa0a6b1,
        Token::String => 0x98c379,
        Token::Tag => 0xe06c75,
        Token::Type => 0xe5c07b,
        Token::Variable => 0xd7a26b,
    };
    HighlightStyle {
        color: Some(Hsla::from(rgb(color))),
        font_style: (token == Token::Comment).then_some(gpui::FontStyle::Italic),
        ..Default::default()
    }
}

pub fn accent() -> Rgba {
    rgb(0x7aa2f7)
}
pub fn accent_background() -> Rgba {
    rgba(0x388bfd40)
}
/// Filler for the empty side of a side-by-side row.
pub fn empty_cell() -> Rgba {
    rgb(0x1a1b1e)
}
pub fn backdrop() -> Rgba {
    rgba(0x0000_0066)
}
/// Highlight for diff lines selected to start a thread.
pub fn line_selection() -> Rgba {
    rgba(0x7aa2f733)
}
