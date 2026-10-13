# Galley

A native desktop app for reviewing AI-written code before it ships. Galley shows your
uncommitted changes (or a branch against its base) as a fast, syntax-highlighted diff, and lets
you open review threads with [Claude Code](https://claude.com/claude-code) anchored to the exact
lines you're looking at. Everything runs locally.

Built in Rust on [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), the UI
framework behind the Zed editor.

> The name comes from the *galley proof*: the copy a printer hands over to be checked and
> corrected before anything goes to print.

## Features

- **Diff review**: unified or side-by-side diffs with tree-sitter highlighting, a file tree, and
  automatic reload when files, the index or commits change on disk.
- **Review threads with Claude Code**: select lines, press `Alt+T`, and ask. The agent gets the
  diff excerpt, can edit files directly, and its edits show up in the diff right away. Follow-ups
  continue the same session.
- **Per-repository command allowlist**: the agent can't run shell commands unless you allow them,
  and denied commands can be approved from the thread with one click.
- **Staging and committing**: per-file stage/unstage and commits through your system `git`, so
  hooks, commit signing and repository config behave exactly as in a terminal.
- **Search**: fuzzy file finder (`Ctrl+P`) and repository-wide text search (`Ctrl+Shift+F`).

## Requirements

- Windows 10 or 11, 64-bit. Galley is currently developed and released for Windows only.
- [git](https://git-scm.com/download/win) on the `PATH`.
- Optional: [Claude Code](https://claude.com/claude-code), installed and signed in, for review
  threads. Everything else works without it.

## Installation

### From a release

Download `galley-<version>-windows-x64.zip` from the [Releases](../../releases) page, extract it,
double-click `install.cmd`, and open a new terminal. The installer copies `galley.exe` to
`%LOCALAPPDATA%\Programs\Galley` and adds it to your user `PATH`; no admin rights needed.
`uninstall.cmd` reverses it.

The executable isn't code-signed, so Windows SmartScreen may ask for confirmation
("More info" → "Run anyway").

### From source

With [rustup](https://rustup.rs) installed (the pinned toolchain is picked up automatically):

```sh
cargo install --path crates/review_app --locked
```

This builds a release binary and puts `galley.exe` in `~/.cargo/bin`. Run the same command again
(with Galley closed) to update.

## Usage

```sh
galley [REPO_PATH] [--base REF]
```

Without a path, Galley opens the repository in the current directory. The release build is a GUI
app, so the terminal is free as soon as the window opens.

- Without `--base`: shows working tree changes (staged, unstaged and untracked) against `HEAD`.
- With `--base main`: compares against the merge base with `main`, the way a pull request from the
  current branch would look.

### Keyboard shortcuts

| Shortcut | Action |
|---|---|
| `Alt+↓` / `Alt+↑` | Next / previous file (in tree order) |
| `Alt+S` | Toggle unified / side-by-side diff |
| Tree checkbox / `Alt+Enter` | Stage / unstage the file |
| `Ctrl+Enter` in the message box | Commit what's staged |
| `Ctrl+P` | Go to any file in the repository (fuzzy search) |
| `Ctrl+Shift+F` | Search text in the repository (`Alt+R` toggles regex) |
| Click / `Shift`+click in the diff | Select lines |
| `Alt+T` | Open a Claude Code thread on the selected lines (or the whole file) |
| `Alt+A` | Show or hide the agent panel |
| `Enter` / `Shift+Enter` | In the agent panel: send / new line |
| `Esc` | In the agent panel: stop the agent |
| `F5` / `Ctrl+R` | Reload manually (disk, staging and commit changes are picked up automatically) |
| `Ctrl+Q` | Quit |

## Review threads

Each thread is anchored to a file and a range of lines. The first message sends the agent the diff
excerpt; later messages resume the same session (`claude --resume`). The agent runs with
`--permission-mode acceptEdits`, so it can edit files in the repository without asking: every edit
is visible in the diff immediately. Requires `claude` on the `PATH`.

### Allowed commands

By default the agent can't run commands. Under **Permissions**, at the top of the agent panel, you
keep a list per repository:

- Each entry is a prefix: `cargo test` allows `cargo test -p foo`. It's passed to Claude Code as
  `--allowedTools "Bash(cargo test:*)"`.
- Raw Claude Code permission rules also work, e.g. `WebFetch(domain:docs.rs)`.
- When the agent tries a command that isn't allowed, the thread shows it with an
  **Allow and continue** button, which adds a suggested prefix and asks the agent to carry on.
- The list is stored in `%APPDATA%\galley\settings.json` (`~/.config/galley` on Linux,
  `~/Library/Application Support/galley` on macOS), never inside the repository.
- Permissions from your own Claude Code settings (`~/.claude/settings.json`) apply too.

## Staging and committing

Each file in the tree has a checkbox: empty (unstaged), checked (staged) or a dash (partially
staged, with further changes in the working tree). The panel at the bottom of the sidebar stages or
unstages everything and commits. All of it goes through your system `git`.

## Development

The workspace is split into UI-free library crates and the app:

| Crate | Responsibility |
|---|---|
| `review_agent` | Runs Claude Code in `stream-json` mode and turns its output into typed events. |
| `review_git` | Loads diffs with `git2` (files, hunks, lines, full contents of both sides), watches the repository, and stages/commits through the `git` CLI. |
| `review_search` | Lists files respecting `.gitignore`, fuzzy matching (`nucleo`) and text search (`grep-*`). |
| `review_syntax` | tree-sitter highlighting, split per line. |
| `review_app` | The `galley` binary: the GPUI interface. |

Before sending changes, run the same checks as CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

GPUI is pre-1.0, so it's pinned to a Zed commit in `Cargo.toml`. When bumping it, also check
`rust-toolchain.toml` and the `[patch.crates-io]` entries against Zed's root `Cargo.toml`.

The C runtime is linked statically (`.cargo/config.toml`), so `galley.exe` doesn't need the Visual
C++ Redistributable.

### Releases

`scripts/package.ps1` builds `dist\galley-<version>-windows-x64.zip` with the executable, the
install scripts, a short `README.txt` and the license. To publish a release, bump the version in `Cargo.toml`
and push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`); CI builds the zip and
creates the GitHub release.

## License

Galley is free software, licensed under the [GNU General Public License v3.0 or later](LICENSE).
It builds on Zed's `ui` and `theme` crates, which are GPL-3.0-or-later themselves.
