GALLEY
======

Galley is for reviewing the changes in a git repository (for example, code
written by AI) and discussing them with Claude Code.

REQUIREMENTS
- 64-bit Windows 10 or 11.
- git: https://git-scm.com/download/win
- Optional, for agent threads: Claude Code, installed and signed in
  (https://claude.com/claude-code). Everything else works without it.

INSTALL (or update to a new version)
1. Extract the zip to any folder.
2. Double-click install.cmd.
3. Open a NEW terminal (terminals that were already open don't see the change).

If Windows shows "Windows protected your PC", click "More info" and then
"Run anyway": it appears because the executable isn't signed.

USAGE
  galley                    open the repository in the current directory
  galley C:\path\to\repo    open that repository
  galley --base main        compare the current branch against main (like a PR)
  galley --help             show help

Main shortcuts inside the app:
  Alt+Up/Down        previous / next file
  Ctrl+P             go to any file
  Ctrl+Shift+F       search text in the repository
  Alt+S              unified / side-by-side diff
  Click, Shift+click select lines
  Alt+T              open a Claude Code thread on the selection
  Alt+Enter          stage / unstage the file
  Ctrl+Enter         commit (in the commit message box)

UNINSTALL
Double-click uninstall.cmd.
