# TODO

## Grant/handle macOS App Management permission for updating apps in /Applications

**Problem:** On macOS 13+ (Ventura and stricter in Sonoma/Sequoia), the OS protects
app bundles in `/Applications`. Modifying, replacing, or even `chown`-ing another
app's bundle is blocked with `Operation not permitted` (EPERM) — *even as root* —
unless the responsible app has been granted **App Management** or **Full Disk
Access**. This is why some Homebrew cask upgrades (e.g. Google Chrome) fail.

**What we've done so far:** `brew_upgrade_cask` in `src-tauri/src/lib.rs` now detects
the chown EPERM case and shows an actionable message + opens the App Management pane,
instead of dumping a wall of `chown: … Operation not permitted` errors. This makes the
failure *legible* but does not prevent it.

**The durable fix (this TODO):** make it so users don't hit the wall in the first place.
Options to evaluate:

- [ ] Ship the app so it can appear in / request **App Management** permission
      (proper code signing + hardened runtime; confirm the bundle is attributed as the
      "responsible process" for the brew-spawned file operations).
- [ ] Add a **first-run / onboarding check**: detect whether the app has App Management
      or Full Disk Access, and if not, prompt the user to grant it before their first
      cask upgrade (with a button that opens
      `x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles`).
- [ ] Document the requirement in the README / app Settings.

**References:**
- Error handling: `src-tauri/src/lib.rs` → `brew_cask_upgrade_fn()` (the
  `Permission denied @ apply2files` / `Operation not permitted` branch).
- Panes: App Management `?Privacy_AppBundles`, Full Disk Access `?Privacy_AllFiles`.

## Help users find a cask when the name doesn't match

**Problem:** Adoption only accepts a cask that genuinely corresponds to the app —
matched on bundle identifier, app path, declared app artifact, or an exact token
or name. That correctly refuses nonsense like `filefillet` for FileZilla, but it
leaves the user to work out the right cask themselves when a real one exists
under an unrelated name.

**What we've done so far:** an unmatched row offers the candidates Homebrew's
fuzzy search returned, plus a field that accepts either a bare cask token or a
`formulae.brew.sh` link — pasting
`https://formulae.brew.sh/cask/dbeaver-enterprise#default` resolves to
`dbeaver-enterprise`, and the token is checked against Homebrew before it is
used. Rows that match nothing offer **No cask available**, which hides them.

**The idea (this TODO):** a way to look the cask up without leaving the app.
A button opening `https://formulae.brew.sh/cask/?q=<app name>` was built and then
removed — it worked, but sending people to a browser to copy an address back is a
clumsy round trip. Worth revisiting as something better:

- [ ] Search Homebrew's catalogue from inside the app, showing name, description
      and homepage so the right cask is recognisable without a browser.
- [ ] Rank suggestions by something meaningful — publisher or homepage host
      against the app's own — rather than by name similarity, which is what
      produced the wrong matches in the first place.
- [ ] If the browser round trip returns, note that `open_release_url`'s allowlist
      must include `https://formulae.brew.sh` again; it was narrowed back to
      GitHub when the button was removed.

**References:**
- Matching: `src-tauri/src/lib.rs` → `verify_cask_apps()` / `mark_verified()`.
- Pasted input: `src-tauri/src/lib.rs` → `resolve_cask_input()`.
- The row UI: `src/App.svelte` → the untracked-apps row, `useTypedCask()`.

## Don't count the same app twice across sources

**Problem:** An app available from more than one source is counted once per
source. Microsoft Remote Desktop showed as 2 pending updates — one from
`app_store` (`1295203466`, 11.3.8 → 11.3.9) and one from `brew_casks`
(`microsoft-remote-desktop`) — for a single product.

v1.0.3 matches casks by bundle identity, but only *within* the Homebrew section.
Nothing reconciles an entry in one section against the same app in another, so
the menu-bar count overstates how much is actually outstanding.

- [ ] Reconcile sections against each other before totalling — bundle identifier
      is the natural key, since `mas` and the cask both resolve to one.
- [ ] Decide which source wins when both offer the app, and install only from
      that one. Upgrading via Homebrew an app the App Store manages leaves the
      two disagreeing about the installed version.

**References:**
- Counting: `src-tauri/src/schedule.rs` → `total_from_counts()`, `count_for()`,
  `recount()`.
- Identity matching that exists today: `src-tauri/src/lib.rs` → `verify_cask_apps()`.

## Disabled casks are counted as pending forever

**Problem:** `brew outdated --cask --greedy` reports casks Homebrew has since
disabled, but `brew upgrade` then refuses them, so they can never clear. They sit
in the menu-bar count permanently with no explanation.

Live example: `microsoft-remote-desktop` — Caskroom stub at 10.7.7 from July 2022,
cask at 10.9.10, and the app not on disk at all:

```
Warning: Not upgrading microsoft-remote-desktop, it is deprecated because it is
discontinued upstream! It was disabled on 2025-10-01.
Replacement: brew install --cask windows-app
```

The end-of-run failure summary added in v1.0.3 doesn't help here — the user never
reaches a failure, the number simply never goes down.

- [ ] Detect deprecated/disabled casks (`brew info --json=v2` carries
      `deprecated`, `disabled` and `replacement_cask`) and keep them out of the
      count.
- [ ] Surface them as their own thing — "no longer available, replaced by
      `windows-app`" — offering the replacement or removal rather than an upgrade
      that cannot succeed.
- [ ] Same treatment for a cask whose app is missing from disk, which fails as
      `It seems the App source '/Applications/X.app' is not there` (seen with
      `brave-browser`).

**References:**
- Cask check: `src-tauri/src/lib.rs` (the `brew outdated --cask --greedy` section).
- Counting: `src-tauri/src/schedule.rs` → `count_for()`.
