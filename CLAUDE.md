# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

A single Bash script (`check-updates.sh`) that reports available updates across macOS system tools and package managers. It is **report-only** — it never installs or upgrades anything, only prints what is outdated and the command to apply each fix.

## Running

```bash
./check-updates.sh        # already chmod +x
bash check-updates.sh     # or invoke explicitly
```

No build, no dependencies, no tests. Lint with `shellcheck check-updates.sh`.

## Architecture

The script runs eight independent, sequential sections, each checking one update source:

1. macOS `softwareupdate`
2. Homebrew casks (`brew outdated --cask --greedy`)
3. Mac App Store (`mas outdated`)
4. Homebrew formulae (`brew outdated`)
5. npm globals (`npm outdated -g`)
6. pip (`pip3`/`pip list --outdated`)
7. rvm (Ruby versions + `gem outdated`)
8. rbenv (fallback Ruby version manager)

Conventions to preserve when editing:

- **Every section is gated by `command -v <tool>`** and calls `skip "<tool>"` when the tool is absent, so the script runs cleanly on machines missing any given tool. New sections must follow this pattern.
- **Output goes through the helper functions** defined near the top — `head`, `sep`, `ok`, `warn`, `info`, `skip` — which carry the ANSI colour scheme. Don't `echo` status lines directly; use the helpers so formatting stays consistent.
- When a section reports outdated items, it also prints the upgrade command via `info` (e.g. `brew upgrade --cask --greedy`) rather than running it — keep this report-only contract intact.
