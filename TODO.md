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
