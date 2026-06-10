# PartyMAN Update Manager

A macOS desktop app for checking and installing software updates across system tools, package managers, and apps. Built with [Tauri](https://tauri.app) and [Svelte](https://svelte.dev).

## What it covers

| Section | What it manages | Tool required |
|---|---|---|
| OS System Updates | macOS software updates | `softwareupdate` |
| App Store | Mac App Store apps | `mas` |
| Homebrew Apps | GUI apps managed by Homebrew | `brew` |
| Apps Without Auto-Updates | Apps not yet connected to an update manager | `brew` |
| Dev → brew | Homebrew CLI formulae | `brew` |
| Dev → npm | Global Node packages | `npm` |
| Dev → pip | Python packages | `pip3` / `pip` |
| Dev → rbenv | Ruby versions and gems via rbenv | `rbenv` |
| Dev → rvm | Ruby versions and gems via rvm | `rvm` |

Sections missing their required tool are skipped automatically.

## Features

- **Check** any section individually or all at once
- **Select individual items** to update (OS updates, App Store, Homebrew Apps)
- **Enable auto-updates** for unmanaged apps by connecting them to Homebrew
- **Update history** — persistent log of every update run, searchable by app name or type, retained for 180 days
- **Dev tools** grouped under a single tab with sub-sections

## Development

### Prerequisites

- [Rust](https://rustup.rs) (stable)
- [Node.js](https://nodejs.org) 18+

### Run locally

```bash
npm install
npm run tauri dev
```

### Build for release

```bash
npm run tauri build
```

Outputs a platform-native installer in `src-tauri/target/release/bundle/`.
