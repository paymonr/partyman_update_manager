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
- [x] Add a **first-run / onboarding check**: detect whether the app has App Management
      or Full Disk Access, and if not, prompt the user to grant it before their first
      cask upgrade (with a button that opens
      `x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles`).
      *(`app_management_status` tries to create and remove an empty file inside
      an app the user owns and has opened; a strip on launch and a Settings →
      General row with "Open System Settings" / "Check again". Probed at most once
      a day while blocked. Verified on a fresh macOS 26 VM on 2026-10-01: a
      same-mode chmod never reaches the policy and is allowed everywhere, so the
      probe must really write; and macOS only protects a quarantined app after
      Gatekeeper approved its first launch, so unopened apps say nothing.)*
- [x] Document the requirement in the README / app Settings. *(README "First run";
      Settings → General shows the live status.)*

**References:**
- Error handling: `src-tauri/src/lib.rs` → `brew_cask_upgrade_fn()` (the
  `Permission denied @ apply2files` / `Operation not permitted` branch).
- Panes: App Management `?Privacy_AppBundles`, Full Disk Access `?Privacy_AllFiles`.

## Set up Homebrew for people who don't have it

**Done 2026-10-01:** `tooling_status` looks for `brew`, `jq` and `mas` at launch.
Without Homebrew the app shows a strip and "Set up" on the sources that need
it; `setup_homebrew` runs Homebrew's own installer (`NONINTERACTIVE=1`, the
password through our askpass dialog, Command Line Tools installed by the script
itself) and then `brew install jq mas`, streaming into a setup view and recorded
in history. With Homebrew but a helper missing, "Install helpers" runs the same
script, which skips the install.

- [x] Tried on a fresh macOS 26.6.2 VM (`tart` VM `partyman-test`, from
      Cirrus Labs' vanilla Tahoe image) on 2026-10-01: the strip, Set up
      Homebrew, PartyMAN's own password dialog (after removing the image's
      passwordless-sudo rule), Homebrew + jq + mas installed, the first checks
      ran by themselves; and the App Management *blocked* state appeared,
      was allowed in System Settings, and flipped to allowed. Not exercised:
      the Command Line Tools download (the image already had them) and anything
      App Store (Apple Accounts cannot sign in inside VMs).
      Notes for next time: the image's `admin` has passwordless sudo
      (`/etc/sudoers.d/admin-nopasswd`); apps launched with `open` from an SSH
      session inherit sshd's Full Disk Access, so launch test apps from Finder.
- [x] The UI side of the flow can be walked on any Mac: a debug build run with
      `PM_PRETEND_NO_BREW=1 npm run tauri dev` believes Homebrew is missing until
      one pretend setup has run (nothing is installed). Walked 2026-10-01: strip,
      "Set up" rows, setup view, history entry, follow-up Check all.
- [x] A non-administrator cannot install Homebrew: `tooling_status` reports
      `admin` (`dseditgroup -o checkmember`), and the strip, the source pages and
      Settings say an administrator has to do this step instead of offering it.

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

- [x] Detect deprecated/disabled casks (`brew info --json=v2` carries
      `deprecated`, `disabled` and `replacement_cask`) and keep them out of the
      count. *(v1.1.0)*
- [x] Surface them as their own thing — listed after the outdated apps with the
      date and any replacement, not offered for selection. *(v1.1.0; offering
      the replacement or removal is still open)*
- [ ] Same treatment for a cask whose app is missing from disk, which fails as
      `It seems the App source '/Applications/X.app' is not there` (seen with
      `brave-browser`).

**References:**
- Cask check: `src-tauri/src/lib.rs` (the `brew outdated --cask --greedy` section).
- Counting: `src-tauri/src/schedule.rs` → `count_for()`.

## Fleet management: report to, or be queried by, a server

**The idea:** a server that can see what each Mac has outstanding, and can ask a
Mac to install things. Either direction needs the same payload: per source, the
items with name, installed version, available version and whether it is really
updatable (not a disabled cask, not an app that updates itself).

**What exists already:** a scheduled run writes exactly that shape to
`last_check.json` and the counts to `schedule.json`, so reporting is close to
free once it is structured (see the next item).

**The constraint to keep:** installing needs the admin password entered at the
machine (the askpass flow), so a server cannot install silently. A server
*requests*; the app shows the request as pending until someone at the Mac clicks.
That matches the app's stance today — it only ever checks; a person starts installs.

- [x] One structured inventory in Rust — `{source, name, installed, available,
      updatable}` — read by the UI, the menu-bar count and the server alike.
      *(The check scripts emit one `__PM_ITEM__` line per item; `schedule.rs` →
      `parse_items()` is the only parser, and the window receives items rather
      than output.)*
- [ ] Decide the direction (app posts on each check, or server polls the app),
      identity (which Mac is this), and auth.
- [ ] A "requested by your admin" state in the UI: a highlighted group with an
      Install button, not something that looks already in progress.

## Dashboard layout (parked 2026-09-30)

The `design/simpler-layout` branch builds the sidebar (every source and its count
in one column). A dashboard alternative — one card per source with its count and
one action, drill in for the list — was mocked up and prototyped, and is parked
for now. The mockups and the clickable prototype are in `docs/dashboard-idea/`
(see its README); the live versions are on the "PartyMAN layout options" canvas.

If it comes back, the natural shape is an "All updates" row at the top of the
sidebar that shows the dashboard cards, rather than a layout switch.

- [ ] Build the dashboard as the overview page behind an "All updates" row.
- [x] Show real app names and versions in lists ("Docker Desktop · 4.86 → 4.93").
