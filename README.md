# PartyMAN Update Manager

A macOS desktop app for checking and installing software updates across system tools, package managers, and apps. Built with [Tauri](https://tauri.app) and [Svelte](https://svelte.dev).

## What it covers

| Source | What it manages | Tool |
|---|---|---|
| macOS updates | Apple's system updates | `softwareupdate` |
| App Store | Mac App Store apps | `mas` (installed by PartyMAN's setup) |
| Homebrew apps | Apps installed with Homebrew | `brew` |
| Apps without auto-updates | Apps nothing keeps up to date; PartyMAN can hand them to Homebrew | `brew` |
| Developer tools → Homebrew formulae | Command-line tools from Homebrew | `brew` |
| Developer tools → npm | Global Node packages | `npm` |
| Developer tools → pip | Python packages | `pip3` / `pip` |
| Developer tools → rbenv, rvm | Ruby versions and gems | `rbenv` / `rvm` |
| Developer tools → asdf | Runtimes managed by asdf | `asdf` |

Developer tools are hidden until turned on in Settings, and only the ones installed on the Mac are listed. They never count towards the update total.

## First run

Two things a Mac needs before PartyMAN can update apps, and PartyMAN walks through both:

- **Homebrew.** If it is missing, PartyMAN offers to set it up: it runs Homebrew's own installer, asks for the administrator password through its own dialog, and installs the two small helpers it relies on (`jq`, and `mas` for the App Store). A non-administrator account is told that an administrator has to do this step.
- **App Management.** macOS protects installed apps from being changed by other apps, so replacing one — which is what an update is — needs PartyMAN allowed under **System Settings → Privacy & Security → App Management** (Full Disk Access also grants it). PartyMAN checks on launch and, if it is blocked, shows a notice with a button to the right pane. Without it, updates fail with "Operation not permitted".

## Features

- A sidebar of every source with its outdated count; one click to see the list
- **Check all** at any time (⌘R), with the result preselected so one more click updates everything, or pick and choose
- **Ignore** an item until a newer version appears
- **Enable auto-updates** for unmanaged apps by handing them to Homebrew
- **History** of every run with its outcome, duration and version changes, searchable, kept for 180 days; a diagnostic log alongside it (Settings → About → Logs)
- **Open apps are handled**: before an update, PartyMAN says which of the selected apps are running and offers to quit them and reopen them afterwards (Chromium and Electron apps crash if replaced while open); apps that have already updated themselves are left alone rather than reinstalled or downgraded
- **Stop** a running update from the Log view or the menu bar; apps already updated stay updated, the one in progress is put back by Homebrew, and the rest are recorded as cancelled rather than failed (macOS installs can't be stopped once started)
- **Menu bar** that works on its own: the count, each source with the outdated apps by name (click one to update just it), **Install all** and **Check now** — the window never has to open; an install started there finishes with a notification saying what happened
- Scheduled background checks with optional notifications; each run also checks Homebrew itself against its latest release, and a banner offers **Update Homebrew** when one is out
- Four themes: Slate, Light, Dark, Warm Dark

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
