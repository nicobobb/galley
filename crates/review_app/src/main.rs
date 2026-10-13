// Release builds are GUI apps on Windows: launching from a terminal returns the
// prompt right away instead of tying it to the window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_panel;
mod file_tree;
mod review_view;
mod search_modal;
mod split_diff;
mod text_input;
mod theme;

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, prelude::*, px,
    size,
};
use gpui_platform::application;

use crate::review_view::ReviewView;

actions!(
    galley,
    [
        NextFile,
        PreviousFile,
        Reload,
        NewThread,
        ToggleAgentPanel,
        OpenFileFinder,
        OpenTextSearch,
        ToggleSplitDiff,
        ToggleStaged,
        Quit
    ]
);

struct Args {
    path: PathBuf,
    base: Option<String>,
}

const USAGE: &str = "usage: galley [REPO_PATH] [--base REF]";
const HELP: &str = "Galley: review code changes with Claude Code.

usage: galley [REPO_PATH] [--base REF]

  REPO_PATH     repository to review (defaults to the current directory)
  --base REF    compare against the merge base with REF, e.g. main
  --version     print the version
  --help        print this help";

fn parse_args() -> Result<Args> {
    let mut path = None;
    let mut base = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--base" | "-b" => base = Some(args.next().context("--base requires a ref")?),
            "--help" | "-h" => {
                attach_parent_console();
                println!("{HELP}");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                attach_parent_console();
                println!("galley {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => bail!("unknown option: {arg}\n{USAGE}"),
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            _ => bail!("too many arguments\n{USAGE}"),
        }
    }
    let path = match path {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    Ok(Args { path, base })
}

fn main() {
    let Args { path, base } = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            attach_parent_console();
            eprintln!("Error: {error:#}");
            std::process::exit(1);
        }
    };

    application().run(move |cx: &mut App| {
        theme::init(cx);
        cx.bind_keys([
            KeyBinding::new("alt-down", NextFile, None),
            KeyBinding::new("alt-up", PreviousFile, None),
            KeyBinding::new("f5", Reload, None),
            KeyBinding::new("ctrl-r", Reload, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("alt-t", NewThread, None),
            KeyBinding::new("alt-a", ToggleAgentPanel, None),
            KeyBinding::new("secondary-p", OpenFileFinder, None),
            KeyBinding::new("secondary-shift-f", OpenTextSearch, None),
            KeyBinding::new("alt-s", ToggleSplitDiff, None),
            KeyBinding::new("alt-enter", ToggleStaged, None),
        ]);
        search_modal::bind_keys(cx);
        text_input::bind_keys(cx);
        agent_panel::bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // Fit the work area (it excludes the taskbar) on small or scaled displays.
        let visible = cx
            .primary_display()
            .map(|display| display.visible_bounds().size)
            .unwrap_or(size(px(1280.), px(820.)));
        let window_size = size(
            px(1280.).min(visible.width * 0.92),
            px(820.).min(visible.height * 0.92),
        );
        let bounds = Bounds::centered(None, window_size, cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(concat!("Galley ", env!("CARGO_PKG_VERSION")).into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| ReviewView::new(path, base, window, cx)),
        )
        .expect("couldn't open the window");
        cx.activate(true);
    });
}

/// GUI-subsystem builds have no console; borrow the launching terminal's only to print
/// `--help`, `--version` or an argument error. A normal launch never attaches, because
/// closing a console terminates every process attached to it, and Galley must outlive
/// the terminal it was started from.
fn attach_parent_console() {
    #[cfg(all(windows, not(debug_assertions)))]
    // SAFETY: plain Win32 call without pointers; failure just means no console.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}
