# Security policy

Galley runs Claude Code with permission to edit files in the repository you open, and to run the
shell commands you allow. Bugs that let the agent go beyond that (running commands that aren't on
the allowlist, writing outside the repository, or reading settings from the repository itself)
are treated as security issues.

## Reporting a vulnerability

Please don't open a public issue. Report it privately through
[GitHub's private vulnerability reporting](https://github.com/nicobobb/galley/security/advisories/new)
instead, with steps to reproduce if you can. You'll get a reply as soon as possible, and a fix
will be released before the details are made public.

## Supported versions

Only the latest release gets security fixes.
