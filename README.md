# BedWatch Updater

A cross-platform desktop app for checking available updates across system tools and package managers. Built with [Tauri](https://tauri.app) and [Svelte](https://svelte.dev).

**Report-only — it never installs or upgrades anything.** Each check tells you what's outdated and gives you the command to apply the fix yourself.

## What it checks

| Section | Tool required |
|---|---|
| macOS System Updates | `softwareupdate` (macOS only) |
| Homebrew Apps (Casks) | `brew` (macOS only) |
| Mac App Store | `mas` (macOS only) |
| Untracked Apps | macOS only |
| Homebrew Formulae | `brew` (macOS only) |
| npm Global Packages | `npm` |
| pip (Python Packages) | `pip3` / `pip` |
| Ruby via rvm | `rvm` |
| Ruby via rbenv | `rbenv` |

Sections are shown or hidden automatically based on your platform. Missing tools are skipped gracefully.

## Development

### Prerequisites

- [Rust](https://rustup.rs) (stable)
- [Node.js](https://nodejs.org) 18+
- On Linux: `libwebkit2gtk-4.1-dev`, `libappindicator3-dev`, `librsvg2-dev`, `patchelf`

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

## Releases

Releases are built automatically via GitHub Actions when a version tag is pushed:

```bash
git tag v1.0.0
git push origin v1.0.0
```

This triggers a build for macOS, Linux, and Windows and creates a draft GitHub Release with installers attached.
