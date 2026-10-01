mod schedule;

use std::fs;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use image::GenericImageView;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

#[derive(Clone, serde::Serialize)]
struct OutputPayload {
    section: String,
    line: String,
}

#[derive(Clone, serde::Serialize)]
struct StatusPayload {
    section: String,
    status: String,
}

// A finished check: its status and, for the sections that list items, what it
// found — parsed once here rather than again from the output by the window.
#[derive(Clone, serde::Serialize)]
struct CheckDonePayload {
    section: String,
    status: String,
    items: Vec<schedule::CheckItem>,
}

#[derive(Clone, serde::Serialize)]
struct CaskCandidate {
    token: String,
    name: String,
    /// True only when the cask declares an app artifact matching the app on disk.
    /// Bulk adoption uses these alone: `brew search` matches on name similarity,
    /// and for an app with no cask it happily returns something unrelated —
    /// FileZilla yields "filefillet", Google Docs yields "google-trends" — which
    /// adopting blind would install over the top of nothing the user asked for.
    exact: bool,
}

/// The app's own identity, read from its bundle. Names change and resemble each
/// other; a bundle identifier does neither.
async fn app_bundle_id(app_name: &str) -> Option<String> {
    for base in ["/Applications", "$HOME/Applications"] {
        let script = format!(
            r#"/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "{base}/{app_name}.app/Contents/Info.plist" 2>/dev/null"#
        );
        if let Ok(out) = Command::new("bash").arg("-c").arg(&script).output().await {
            let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Every string anywhere in a cask's artifacts — where the uninstall stanzas name
/// the bundle identifiers a cask is responsible for.
fn artifact_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => a.iter().for_each(|v| artifact_strings(v, out)),
        serde_json::Value::Object(o) => o.values().for_each(|v| artifact_strings(v, out)),
        _ => {}
    }
}

fn cask_norm(s: &str) -> String {
    s.to_lowercase()
        .trim_end_matches(".app")
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Decides which candidates genuinely correspond to this app.
///
/// Three signals, any one of which is convincing:
///   * the cask declares an app artifact with this app's name — the strongest,
///     but pkg-based casks (Parsec, Zoom, the Microsoft apps) declare none;
///   * the cask token is exactly this app's name normalised;
///   * the cask's own display name is exactly this app's name normalised.
///
/// Anything else is left unconfirmed. Under-matching costs the user a manual
/// choice; over-matching installs software they never asked for.
async fn verify_cask_apps(app_name: &str, tokens: &[String]) -> std::collections::HashSet<String> {
    let mut verified = std::collections::HashSet::new();
    if tokens.is_empty() {
        return verified;
    }
    let safe: Vec<String> = tokens
        .iter()
        .filter(|t| t.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.'))
        .cloned()
        .collect();
    if safe.is_empty() {
        return verified;
    }

    let script = format!(
        r#"export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"; brew info --cask --json=v2 {} 2>/dev/null"#,
        safe.iter().map(|t| format!("'{t}'")).collect::<Vec<_>>().join(" ")
    );
    let out = match Command::new("bash").arg("-c").arg(&script).output().await {
        Ok(o) if o.status.success() => o,
        _ => return verified,
    };
    let json: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(j) => j,
        Err(_) => return verified,
    };

    let wanted = cask_norm(app_name);
    let bundle_id = app_bundle_id(app_name).await;
    let app_file = format!("{app_name}.app");

    for cask in json["casks"].as_array().unwrap_or(&vec![]) {
        let token = match cask["token"].as_str() {
            Some(t) => t.to_string(),
            None => continue,
        };

        let artifact_match = cask["artifacts"].as_array().map_or(false, |arts| {
            arts.iter().any(|a| {
                a["app"].as_array().map_or(false, |apps| {
                    apps.iter()
                        .filter_map(|v| v.as_str())
                        .any(|name| cask_norm(name) == wanted)
                })
            })
        });

        let name_match = cask["name"].as_array().map_or(false, |names| {
            names.iter()
                .filter_map(|v| v.as_str())
                .any(|n| cask_norm(n) == wanted)
        });

        // The strongest signal by far: the cask's uninstall stanzas name the very
        // bundle identifier this app carries, or point at its exact path. This is
        // what connects zoom.us.app to the "zoom" cask, which no amount of name
        // comparison can — they share no spelling at all.
        let mut strings = Vec::new();
        artifact_strings(&cask["artifacts"], &mut strings);
        let identity_match = bundle_id.as_ref().is_some_and(|id| strings.iter().any(|v| v == id))
            || strings
                .iter()
                .any(|v| v == &app_file || v.ends_with(&format!("/{app_file}")));

        if identity_match || artifact_match || cask_norm(&token) == wanted || name_match {
            verified.insert(token);
        }
    }
    verified
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct VersionChange {
    item: String,
    from: String,
    to: String,
}

// `default` at the container level so entries written before a field existed
// still load; get_upgrade_history fills in what can be worked out for them.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct HistoryEntry {
    ts: u64,
    section: String,
    label: String,
    items: Vec<String>,
    item_names: Vec<String>,
    lines: Vec<String>,
    /// Shared by every entry one run wrote, so a batch shows as one thing.
    run_id: String,
    /// "update", or "adopt" for an app handed to Homebrew.
    kind: String,
    /// "ok", "partial" or "failed". See outcome_from_lines.
    outcome: String,
    duration_secs: u64,
    exit_code: Option<i32>,
    versions: Vec<VersionChange>,
}

/// What a run's output says happened. The scripts mark each step with "→  Done"
/// on success and "✖" on failure, and Homebrew prints "🍺 … was successfully
/// upgraded!", so those are the signals — not "Error:" lines, which Homebrew
/// also prints on a first attempt that a retry then rescues.
fn outcome_from_lines(lines: &[String], exit_code: Option<i32>) -> &'static str {
    let done = lines.iter().filter(|l| {
        let t = l.trim_start();
        // Two spaces: our own marker. Homebrew's fetch lines start "✔︎ " (with a
        // variation selector and one space) and say nothing about the outcome.
        t.starts_with("→  Done") || t.starts_with("✔  ") || (t.starts_with('🍺') && t.contains("successfully"))
    }).count();
    let failed = lines.iter().filter(|l| l.trim_start().starts_with('✖')).count();
    // Our own marker for a dismissed password prompt, or AppleScript's for the
    // macOS updates' administrator dialog.
    let cancelled = lines.iter().filter(|l| {
        let t = l.trim_start();
        t.starts_with("↩  Cancelled") || t.contains("User canceled.")
    }).count();
    if failed > 0 && done > 0 {
        "partial"
    } else if failed > 0 {
        "failed"
    } else if cancelled > 0 && done == 0 {
        "cancelled"
    } else if matches!(exit_code, Some(c) if c != 0) && done == 0 {
        "failed"
    } else {
        "ok"
    }
}

/// Version changes Homebrew reports, in either of its two shapes:
///   ==> Upgrading docker-desktop
///     4.86.0,236216 -> 4.93.0,240920
/// and the closing summary line "docker-desktop 4.86.0,236216 -> 4.93.0,240920".
fn versions_from_lines(lines: &[String], only_item: Option<&str>) -> Vec<VersionChange> {
    let mut out: Vec<VersionChange> = Vec::new();
    let mut current: Option<String> = only_item.map(|s| s.to_string());
    for line in lines {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("==> Upgrading ") {
            current = rest.split_whitespace().next().map(|s| s.to_string());
            continue;
        }
        let parts: Vec<&str> = t.split_whitespace().collect();
        let (item, from, to) = match parts.as_slice() {
            [from, "->", to] => (current.clone(), *from, *to),
            [name, from, "->", to] => (Some(name.to_string()), *from, *to),
            _ => continue,
        };
        let Some(item) = item else { continue };
        if !from.chars().next().is_some_and(|c| c.is_ascii_digit()) { continue; }
        if out.iter().any(|v| v.item == item) { continue; }
        out.push(VersionChange { item, from: from.to_string(), to: to.to_string() });
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn new_run_id(section: &str) -> String {
    let n = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{}-{}", now_secs(), std::process::id(), n) + "-" + section
}

// The diagnostic log: what the app itself did and when, as opposed to the
// history, which is what the user did. One line per event, local time, kept to
// about 2 MB with one older file behind it. This is what to read when a run
// misbehaves; the history holds the run's own output.
fn diag_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("partyman.log"))
}

pub(crate) fn diag(app: &AppHandle, msg: &str) {
    use std::io::Write;
    let Some(path) = diag_path(app) else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    const ROTATE_AT: u64 = 2 * 1024 * 1024;
    if fs::metadata(&path).map(|m| m.len() > ROTATE_AT).unwrap_or(false) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{stamp}] {msg}");
    }
}

fn secs_since(start: std::time::Instant) -> u64 {
    start.elapsed().as_secs()
}

// Returns a bash function definition for upgrading a single cask.
// Handles three cases in order:
//   1. App lives in /Applications (normal)
//   2. App lives in ~/Applications ("App source not there" → retry with --appdir)
//   3. App is system-owned ("Permission denied @ apply2files" → chown via osascript, retry)
// Shared bash helper: recover from "Permission denied @ apply2files" (a root-owned
// app bundle) so brew can replace it. Used by both the cask-upgrade and the
// adopt-untracked-app flows. Reads the brew output file passed as $1.
// Returns 0 if ownership was fixed and the caller should retry the brew command;
// returns 1 if macOS blocked it (App Management / SIP) or the user cancelled — in
// which case actionable guidance has already been printed and the caller must not
// retry.
fn protected_bundle_fn() -> &'static str {
    r#"pm_fix_protected_bundle() {
  local tmpout="$1"
  local current_user app_path app_basename chownout rc
  current_user=$(whoami)
  app_path=$(grep "Permission denied @ apply2files" "$tmpout" | head -1 \
    | sed 's/.*@ apply2files - //' | sed 's|/Contents/.*||')
  if [ -z "$app_path" ]; then
    echo "✖  Could not determine app path."
    return 1
  fi
  app_basename=$(basename "$app_path")
  echo "→  $app_basename is protected by macOS — requesting administrator access…"
  chownout=$(mktemp)
  # `sudo -A` rides on the credential sudo already cached for this run (see
  # ASKPASS_PREAMBLE) rather than opening a second, unrelated auth dialog.
  sudo -A chown -R "$current_user" "$app_path" > "$chownout" 2>&1
  rc=$?
  if [ "$rc" -ne 0 ] && ! grep -q "Operation not permitted" "$chownout"; then
    if grep -qiE "no password was provided|a password is required|incorrect password" "$chownout"; then
      rm -f "$chownout"
      echo "✖  Cancelled — administrator access is needed for $app_basename."
      return 1
    fi
    # sudo itself could not run (e.g. the account is not an admin). Fall back to
    # Authorization Services, which can authenticate as a different admin user.
    # '2>&1; true' keeps chown errors in the result and stops `do shell script`
    # from raising, so we can inspect what actually happened.
    osascript -e "do shell script \"chown -R $current_user '$app_path' 2>&1; true\" with administrator privileges" > "$chownout" 2>&1
  fi
  if grep -q "Operation not permitted" "$chownout"; then
    # EPERM as root = macOS App Management / SIP bundle protection, not ownership.
    rm -f "$chownout"
    echo "✖  macOS blocked this: it protects $app_basename and won't let another app modify it — even with your password."
    echo "→  To let PartyMAN manage apps in /Applications, grant it permission once:"
    echo "     System Settings ▸ Privacy & Security ▸ App Management (or Full Disk Access)"
    echo "     → turn on \"PartyMAN Update Manager\", then quit and reopen this app and try again."
    echo "→  Opening Privacy & Security settings…"
    open "x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles" 2>/dev/null
    echo "→  (Google Chrome, Google Drive and some apps also update themselves automatically.)"
    return 1
  elif grep -qiE "User canceled|-128" "$chownout"; then
    rm -f "$chownout"
    echo "✖  Cancelled — administrator access is needed for $app_basename."
    return 1
  fi
  cat "$chownout"
  rm -f "$chownout"
  return 0
}
"#
}

// Live download progress.
//
// Homebrew only draws a progress bar when it is talking to a terminal, and we
// deliberately hand it a pipe so its output stays free of cursor codes and
// carriage returns. That leaves a multi-hundred-megabyte download looking like a
// frozen app. Instead of giving brew a terminal back, watch what it is actually
// doing: it writes to a `.incomplete` file in its download cache, so the size of
// that file is the real progress.
fn download_progress_fn() -> &'static str {
    r#"PM_CACHE_DIR="$(brew --cache 2>/dev/null)/downloads"

pm_progress_start() {
  (
    _pm_last=0
    while :; do
      _pm_f=$(ls -t "$PM_CACHE_DIR"/*.incomplete 2>/dev/null | head -1)
      if [ -n "$_pm_f" ]; then
        _pm_sz=$(stat -f %z "$_pm_f" 2>/dev/null || echo 0)
        # Only speak up every few megabytes, or a big download becomes a wall of
        # near-identical lines.
        if [ "$_pm_sz" -gt 0 ] && [ $(( _pm_sz - _pm_last )) -gt 4194304 ]; then
          printf '⬇  Downloading… %d MB\n' $(( _pm_sz / 1048576 ))
          _pm_last=$_pm_sz
        fi
      else
        _pm_last=0
      fi
      sleep 2
    done
  ) &
  PM_PROGRESS_PID=$!
  disown "$PM_PROGRESS_PID" 2>/dev/null
}

pm_progress_stop() {
  [ -n "$PM_PROGRESS_PID" ] && kill "$PM_PROGRESS_PID" 2>/dev/null
  PM_PROGRESS_PID=""
}
"#
}

// Runs `brew "$@"` with its output shown live and also saved to $1, returning
// brew's own exit status.
//
// This replaces `brew … 2>&1 | tee "$out"`. A pipeline lasts until tee sees
// end-of-file, and tee sees it only once every process holding the pipe has let
// go — brew *and anything brew started*. A cask that leaves a process running
// after it installs therefore stalled the batch on that cask indefinitely, long
// after brew itself had finished. Here only brew is waited on. tee gets a few
// seconds to copy what is left in the pipe; if it is still going after that, it
// is left to drain in the background, so whatever holds the pipe is never hit
// with SIGPIPE.
fn brew_logged_fn() -> &'static str {
    r#"pm_brew_logged() {
  local out="$1"; shift
  local fifo tee_pid rc i=0
  fifo=$(mktemp -u "${TMPDIR:-/tmp}/pm-brew.XXXXXX")
  if ! mkfifo -m 600 "$fifo" 2>/dev/null; then
    brew "$@" 2>&1 | tee "$out"
    return "${PIPESTATUS[0]}"
  fi
  tee "$out" < "$fifo" &
  tee_pid=$!
  brew "$@" > "$fifo" 2>&1
  rc=$?
  while kill -0 "$tee_pid" 2>/dev/null && [ "$i" -lt 30 ]; do
    sleep 0.1
    i=$((i + 1))
  done
  rm -f "$fifo"
  return "$rc"
}
"#
}

// The cask helper bundle: the protected-bundle recovery followed by the
// single-cask upgrade function that depends on it.
fn cask_fns() -> String {
    format!(
        "{}\n{}\n{}\n{}",
        brew_logged_fn(),
        protected_bundle_fn(),
        download_progress_fn(),
        brew_cask_upgrade_fn()
    )
}

fn brew_cask_upgrade_fn() -> &'static str {
    r#"brew_upgrade_cask() {
  local token="$1"
  local CURRENT_USER
  CURRENT_USER=$(whoami)
  local TMPOUT
  TMPOUT=$(mktemp)
  local APPDIR_FLAG=""

  pm_brew_logged "$TMPOUT" upgrade --cask "$token"
  local BREW_EXIT=$?

  if grep -q "It seems there is already an App at" "$TMPOUT"; then
    echo "→  Backup conflict (app may have self-updated) — retrying with --force…"
    rm -f "$TMPOUT"; TMPOUT=$(mktemp)
    pm_brew_logged "$TMPOUT" upgrade --cask --force $APPDIR_FLAG "$token"
    BREW_EXIT=$?
  fi

  if grep -q "App source.*is not there" "$TMPOUT"; then
    EXPECTED_PATH=$(grep "App source.*is not there" "$TMPOUT" | head -1 \
      | sed "s/.*App source '//;s/' is not there.*//")
    APP_NAME=$(basename "$EXPECTED_PATH")
    rm -f "$TMPOUT"
    TMPOUT=$(mktemp)
    if [ -n "$APP_NAME" ] && [ -d "$HOME/Applications/$APP_NAME" ]; then
      APPDIR_FLAG="--appdir $HOME/Applications"
      echo "→  App is in ~/Applications — reinstalling there…"
    else
      APPDIR_FLAG=""
      echo "→  App not found — reinstalling to /Applications…"
    fi
    pm_brew_logged "$TMPOUT" install --cask --force $APPDIR_FLAG "$token"
    BREW_EXIT=$?
  fi

  # sudo says this when the password dialog was dismissed or gave up unanswered;
  # the password server leaves a marker when it was dismissed.
  local NO_PW=0 CANCELLED=0
  grep -q "no password was provided" "$TMPOUT" 2>/dev/null && NO_PW=1
  [ "$NO_PW" -eq 1 ] && [ -n "$_pm_dir" ] && [ -f "$_pm_dir/cancelled" ] && CANCELLED=1

  if grep -q "Permission denied @ apply2files" "$TMPOUT"; then
    if pm_fix_protected_bundle "$TMPOUT"; then
      rm -f "$TMPOUT"
      brew upgrade --cask $APPDIR_FLAG "$token" 2>&1
      BREW_EXIT=$?
    else
      rm -f "$TMPOUT"
      BREW_EXIT=1
    fi
  else
    rm -f "$TMPOUT"
  fi

  echo "__PM_CASK_EXIT__:$token:$BREW_EXIT"
  if [ "$BREW_EXIT" -eq 0 ]; then
    echo "→  Done."
  elif [ "$CANCELLED" -eq 1 ]; then
    echo "↩  Cancelled: $token was left as it was."
  else
    echo "✖  Update failed for $token."
    [ "$NO_PW" -eq 1 ] && echo "   No password was entered when asked (the prompt waits five minutes), so the app was left as it was."
  fi
}
"#
}

// Bash function that adopts one app into Homebrew management. $1 is "user" to
// install into ~/Applications (otherwise /Applications), $2 is the cask token.
// Depends on pm_fix_protected_bundle (protected_bundle_fn) being defined too.
/// Printed after every adoption run. Successes scroll past; what the user needs
/// at the end is which apps did not make it, which cask was tried for each, and
/// why — none of which is recoverable from a wall of Homebrew output.
fn adopt_summary() -> &'static str {
    r#"echo ""
echo "────────────────────────────"
if [ -s "$PM_FAILURES" ]; then
  _pm_n=$(grep -c '' "$PM_FAILURES")
  echo "✖  $_pm_n app(s) could not be set up:"
  while IFS=$'	' read -r _app _tok _why; do
    echo "     • $_app — tried cask "$_tok""
    echo "         $_why"
  done < "$PM_FAILURES"
  echo ""
  echo "→  Everything else was set up. Run a check to move them to Homebrew Apps."
else
  echo "✔  All set! Run a check to move these apps to Homebrew Apps."
fi
rm -f "$PM_FAILURES""#
}

fn adopt_cask_fn() -> &'static str {
    r#"# $3 is the app's own name, so a failure can say which app it was and which
# cask was tried — the token alone ("filefillet") tells the user nothing.
pm_note_failure() {
  [ -n "$PM_FAILURES" ] && printf '%s	%s	%s
' "$1" "$2" "$3" >> "$PM_FAILURES"
}

adopt_cask() {
  local use_userdir="$1" token="$2" appname="${3:-$2}"
  local flag=""
  [ "$use_userdir" = "user" ] && flag="--appdir $HOME/Applications"
  local TMPOUT; TMPOUT=$(mktemp)
  pm_brew_logged "$TMPOUT" install --cask --force $flag "$token"
  local BREW_EXIT=$?

  # Homebrew's own words for the common, specific failures. A generic "setup
  # failed" hides the one thing that tells the user whether they can act.
  local reason=""
  if grep -q "different checksum" "$TMPOUT"; then
    reason="the download no longer matches Homebrew's checksum — the cask needs updating upstream"
  elif grep -q "has been disabled" "$TMPOUT"; then
    reason="that cask has been discontinued by Homebrew"
  elif grep -qE "Running installer for|/usr/sbin/installer|installer -pkg" "$TMPOUT"; then
    reason="its installer failed — the app may still be running, or is managed by your organisation"
  elif grep -q "No available Cask" "$TMPOUT"; then
    reason="no such cask"
  fi

  if grep -q "Permission denied @ apply2files" "$TMPOUT"; then
    if pm_fix_protected_bundle "$TMPOUT"; then
      rm -f "$TMPOUT"
      if brew install --cask --force $flag "$token" 2>&1; then
        echo "→  Done! $appname is now managed by Homebrew."
      else
        echo "✖  Setup failed for $appname."
        pm_note_failure "$appname" "$token" "the app bundle could not be replaced"
      fi
    else
      rm -f "$TMPOUT"
      pm_note_failure "$appname" "$token" "macOS blocked changing the app bundle"
    fi
  elif [ "$BREW_EXIT" -eq 0 ]; then
    rm -f "$TMPOUT"
    echo "→  Done! $appname is now managed by Homebrew."
  else
    rm -f "$TMPOUT"
    echo "✖  $appname couldn't be set up${reason:+ — $reason}."
    pm_note_failure "$appname" "$token" "${reason:-it could not be installed}"
  fi
}
"#
}

fn section_label(section: &str) -> &'static str {
    match section {
        "macos_updates"  => "OS System Updates",
        "app_store"      => "App Store",
        "brew_casks"     => "Homebrew Apps",
        "untracked_apps" => "Untracked Apps",
        "brew_formulae"  => "brew",
        "npm_globals"    => "npm",
        "pip_packages"   => "pip",
        "ruby_rbenv"     => "rbenv",
        "ruby_rvm"       => "rvm",
        "asdf"           => "asdf",
        "setup"          => "Setup",
        _                => "Unknown",
    }
}

fn log_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("updates.log"))
}

fn append_upgrade_log(app: &AppHandle, entry: HistoryEntry) {
    let path = match log_path(app) {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let entry_str = match serde_json::to_string(&entry) {
        Ok(s) => s,
        Err(_) => return,
    };

    let existing = fs::read_to_string(&path).unwrap_or_default();
    let cutoff = entry.ts.saturating_sub(180 * 24 * 3600);

    let mut kept: Vec<String> = existing
        .lines()
        .filter(|l| !l.is_empty())
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v["ts"].as_u64())
                .map(|t| t >= cutoff)
                .unwrap_or(false)
        })
        .map(|l| l.to_string())
        .collect();

    kept.push(entry_str);

    // Hard cap at 50 MB — drop oldest first
    const MAX_BYTES: usize = 50 * 1024 * 1024;
    let mut total: usize = kept.iter().map(|l| l.len() + 1).sum();
    while total > MAX_BYTES && kept.len() > 1 {
        let removed = kept.remove(0);
        total -= removed.len() + 1;
    }

    let _ = fs::write(&path, kept.join("\n") + "\n");
}

#[tauri::command]
async fn search_cask(app_name: String) -> Vec<CaskCandidate> {
    let normalized = app_name.to_lowercase()
        .replace(' ', "-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '+')
        .collect::<String>();

    let safe_name = app_name.replace('\'', "");
    let safe_norm = normalized.replace('\'', "");

    // Try exact token match first (no network needed)
    let exact_script = format!(
        r#"export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"; brew info --cask --json=v2 '{safe_norm}' 2>/dev/null"#
    );
    if let Ok(out) = Command::new("bash").arg("-c").arg(&exact_script).output().await {
        if out.status.success() && !out.stdout.is_empty() {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                let candidates: Vec<CaskCandidate> = json["casks"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|c| {
                        let token = c["token"].as_str()?.to_string();
                        let name = c["name"].as_array()
                            .and_then(|a| a.first())
                            .and_then(|v| v.as_str())
                            .unwrap_or(&token)
                            .to_string();
                        Some(CaskCandidate { token, name, exact: false })
                    })
                    .collect();
                if !candidates.is_empty() {
                    return mark_verified(&app_name, candidates).await;
                }
            }
        }
    }

    // Fall back to brew search (requires network)
    let search_script = format!(
        r#"export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"; brew search --casks '{safe_name}' 2>/dev/null"#
    );
    if let Ok(out) = Command::new("bash").arg("-c").arg(&search_script).output().await {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let fuzzy: Vec<CaskCandidate> = text.lines()
                .filter(|l| !l.is_empty() && !l.starts_with("==>") && !l.contains("No formulae or casks"))
                // Ten, not five: when nothing is confirmed the user picks from these
                // by hand, and a short list often omits the right one entirely.
                .take(10)
                .map(|t| CaskCandidate { token: t.trim().to_string(), name: t.trim().to_string(), exact: false })
                .collect();
            return mark_verified(&app_name, fuzzy).await;
        }
    }

    vec![]
}

/// Flags the candidates that genuinely install this app, confirmed ones first, so
/// a real match is distinguishable from a merely similar-looking name.
async fn mark_verified(app_name: &str, mut candidates: Vec<CaskCandidate>) -> Vec<CaskCandidate> {
    let tokens: Vec<String> = candidates.iter().map(|c| c.token.clone()).collect();
    let verified = verify_cask_apps(app_name, &tokens).await;
    for c in candidates.iter_mut() {
        c.exact = verified.contains(&c.token);
    }
    candidates.sort_by_key(|c| !c.exact);
    candidates
}

#[tauri::command]
async fn track_app(app: AppHandle, cask_token: String, appdir: Option<String>) {
    if !cask_token.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.') {
        emit_upgrade_line(&app, "untracked_apps", "Invalid cask token.").await;
        emit_upgrade_status(&app, "untracked_apps", "error").await;
        return;
    }
    // Only allow the known user-Applications path; reject anything else.
    let use_userdir = if matches!(appdir.as_deref(), Some("~/Applications")) { "user" } else { "" };
    let section = "untracked_apps";
    let script = format!(
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\n{logged}\n{helper}\n{adopt}\nadopt_cask '{ud}' '{token}'\necho '→  Run a check to see this app move to Homebrew Apps.'\nelse\n  echo '✖  brew not found'\nfi",
        logged = brew_logged_fn(),
        helper = protected_bundle_fn(),
        adopt = adopt_cask_fn(),
        ud = use_userdir,
        token = cask_token,
    );
    let ts = now_secs();
    let run_id = new_run_id(section);
    diag(&app, &format!("adopt {cask_token} ({run_id})"));
    let started = std::time::Instant::now();
    let result = run_upgrade_shell(&app, section, &script).await;
    let outcome = outcome_from_lines(&result.lines, result.exit_code).to_string();
    diag(&app, &format!("adopt {cask_token}: {outcome} ({run_id})"));
    append_upgrade_log(&app, HistoryEntry {
        ts,
        label: section_label(section).to_string(),
        section: section.to_string(),
        items: vec![cask_token.clone()],
        item_names: vec![cask_token],
        versions: Vec::new(),
        run_id,
        kind: "adopt".to_string(),
        outcome,
        duration_secs: secs_since(started),
        exit_code: result.exit_code,
        lines: result.lines,
    });
}

#[derive(serde::Deserialize)]
struct TrackItem {
    token: String,
    name: String,
    appdir: Option<String>,
}

// Adopt several apps into Homebrew in one shell, so a single sudo session covers
// the whole batch (one password prompt) instead of one prompt per app.
#[tauri::command]
async fn track_apps(app: AppHandle, items: Vec<TrackItem>) {
    let section = "untracked_apps";
    let mut calls = String::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for it in &items {
        if !it.token.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.') {
            continue;
        }
        let ud = if matches!(it.appdir.as_deref(), Some("~/Applications")) { "user" } else { "" };
        // The display name goes through too, so a failure can name the app
        // rather than only the cask that was tried.
        let name_esc = it.name.replace(['\\', '"'], "").replace('\'', "'\\''");
        calls.push_str(&format!(
            "echo '{CASK_START}{token}'\nadopt_cask '{ud}' '{token}' '{name}'\necho '{CASK_END}{token}'\n",
            ud = ud, token = it.token, name = name_esc
        ));
        tokens.push(it.token.clone());
        names.push(it.name.clone());
    }
    if tokens.is_empty() {
        emit_upgrade_line(&app, section, "No apps to enable.").await;
        emit_upgrade_status(&app, section, "done").await;
        return;
    }
    let script = format!(
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\nexport PM_ASKPASS_APP='{scope}'\nPM_FAILURES=$(mktemp)\n{logged}\n{helper}\n{progress}\n{adopt}\npm_progress_start\n{calls}pm_progress_stop\n{summary}\nelse\n  echo '✖  brew not found'\nfi",
        scope = askpass_scope(&names),
        logged = brew_logged_fn(),
        helper = protected_bundle_fn(),
        progress = download_progress_fn(),
        adopt = adopt_cask_fn(),
        calls = calls,
        summary = adopt_summary(),
    );
    let ts = now_secs();
    let run_id = new_run_id(section);
    diag(&app, &format!("adopt {} apps ({run_id}): {}", tokens.len(), tokens.join(", ")));
    let started = std::time::Instant::now();
    let result = run_upgrade_shell(&app, section, &script).await;
    let duration_secs = secs_since(started);
    let mut summary: Vec<String> = Vec::new();
    for (token, body_lines, _) in split_by_item(&result.lines) {
        let name = tokens.iter().position(|t| *t == token)
            .and_then(|i| names.get(i).cloned())
            .unwrap_or_else(|| token.clone());
        let outcome = outcome_from_lines(&body_lines, None).to_string();
        summary.push(format!("{token} {outcome}"));
        append_upgrade_log(&app, HistoryEntry {
            ts,
            label: section_label(section).to_string(),
            section: section.to_string(),
            items: vec![token],
            item_names: vec![name],
            versions: Vec::new(),
            run_id: run_id.clone(),
            kind: "adopt".to_string(),
            outcome,
            duration_secs,
            exit_code: result.exit_code,
            lines: body_lines,
        });
    }
    diag(&app, &format!("adopt: {} ({run_id})", summary.join(", ")));
}

// Sets an item aside until a newer version is available, then re-derives the
// count from what the last check found; nothing is re-checked.
#[tauri::command]
fn ignore_item(app: AppHandle, section: String, id: String, name: String, available: Option<String>) -> schedule::ScheduleConfig {
    let mut cfg = schedule::load(&app);
    cfg.ignored.retain(|g| !(g.section == section && g.id == id));
    cfg.ignored.push(schedule::IgnoredItem {
        section: section.clone(),
        id: id.clone(),
        name,
        available: available.unwrap_or_default(),
        since: now_secs(),
    });
    let _ = schedule::save(&app, &cfg);
    diag(&app, &format!("ignore {section}/{id} at {}", cfg.ignored.last().map(|g| g.available.as_str()).unwrap_or("")));
    let cfg = schedule::recount_stored(&app, &section);
    set_tray_count(&app, cfg.last_total);
    let _ = app.emit("schedule-updated", cfg.clone());
    cfg
}

#[tauri::command]
fn unignore_item(app: AppHandle, section: String, id: String) -> schedule::ScheduleConfig {
    let mut cfg = schedule::load(&app);
    cfg.ignored.retain(|g| !(g.section == section && g.id == id));
    let _ = schedule::save(&app, &cfg);
    diag(&app, &format!("stop ignoring {section}/{id}"));
    let cfg = schedule::recount_stored(&app, &section);
    set_tray_count(&app, cfg.last_total);
    let _ = app.emit("schedule-updated", cfg.clone());
    cfg
}

/// Batch adoptions recorded before runs were split wrote one entry for the whole
/// batch. Each app's part of the output ends with the line adopt_cask prints
/// for it, in the order the apps were listed, so the entry can be split the
/// way a new one would be — as long as every app has such a line.
fn split_legacy_adoption(e: HistoryEntry) -> Vec<HistoryEntry> {
    if e.kind != "adopt" || e.items.len() < 2 || e.run_id != format!("{}-{}", e.ts, e.section) {
        return vec![e];
    }
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    for line in &e.lines {
        cur.push(line.clone());
        let t = line.trim_start();
        let closes = t.starts_with("→  Done!")
            || t.starts_with("✖  Setup failed")
            || (t.starts_with('✖') && t.contains("couldn't be set up"));
        if closes {
            groups.push(std::mem::take(&mut cur));
        }
    }
    if groups.len() != e.items.len() {
        return vec![e];
    }
    groups
        .into_iter()
        .enumerate()
        .map(|(i, lines)| HistoryEntry {
            outcome: outcome_from_lines(&lines, None).to_string(),
            items: vec![e.items[i].clone()],
            item_names: vec![e.item_names.get(i).cloned().unwrap_or_else(|| e.items[i].clone())],
            lines,
            versions: Vec::new(),
            ..e.clone()
        })
        .collect()
}

/// Whether macOS lets this app change other apps' bundles in /Applications —
/// the App Management permission (Full Disk Access grants it too). Without it
/// every cask upgrade that replaces an app fails, as root or not, with
/// "Operation not permitted", which is how Firefox, Obsidian and Postman
/// failed in this user's history.
///
/// There is no API to ask, so this tries to modify another app the way
/// Homebrew would: it creates an empty file inside the bundle and removes it.
/// Refused means blocked, and nothing has changed; allowed means the file
/// existed for a moment. Tested on a fresh macOS 26 VM: a chmod to the mode a
/// bundle already has never reaches the policy and is allowed everywhere, so
/// it tells nothing — this must be a real write.
///
/// The target is an app the user owns, so ordinary permissions cannot be what
/// refuses, and one the user has opened at least once: macOS only protects a
/// quarantined app after Gatekeeper has approved it (the 0x40 bit in the
/// quarantine flags), so an unopened app would say "allowed" for every
/// process. Being refused also puts PartyMAN on the App Management list,
/// where the user can then allow it.
#[tauri::command]
fn app_management_status(app: AppHandle) -> String {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::getuid() };
        let Ok(entries) = fs::read_dir("/Applications") else { return "unknown".to_string() };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        // Apps the user has opened first: their answer is definitive either way.
        paths.sort_by_key(|p| !quarantine_approved(p));
        for path in paths {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".app") || name.starts_with("PartyMAN") {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else { continue };
            if meta.uid() != uid || !meta.is_dir() || !path.join("Contents").is_dir() {
                continue;
            }
            let probe = path.join("Contents/.partyman-probe");
            match fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
                Ok(_) => {
                    let _ = fs::remove_file(&probe);
                    if quarantine_approved(&path) {
                        diag(&app, &format!("app management: allowed (probed {name})"));
                        return "allowed".to_string();
                    }
                    // Allowed on an app macOS may not be protecting yet; keep looking.
                    continue;
                }
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                    diag(&app, &format!("app management: blocked (probed {name})"));
                    return "blocked".to_string();
                }
                Err(_) => continue,
            }
        }
        diag(&app, "app management: unknown (no opened app of the user's own to probe)");
        "unknown".to_string()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        "unknown".to_string()
    }
}

/// True when the bundle carries a quarantine attribute whose flags say the
/// user has approved opening it — the point from which macOS protects it.
#[cfg(target_os = "macos")]
fn quarantine_approved(path: &std::path::Path) -> bool {
    use std::ffi::CString;
    let Ok(cpath) = CString::new(path.as_os_str().as_encoded_bytes()) else { return false };
    let name = c"com.apple.quarantine";
    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::getxattr(cpath.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len(), 0, 0)
    };
    if n <= 0 {
        return false;
    }
    let value = String::from_utf8_lossy(&buf[..n as usize]);
    let flags = value.split(';').next().unwrap_or("");
    u32::from_str_radix(flags, 16).map(|f| f & 0x40 != 0).unwrap_or(false)
}

/// Which of the tools PartyMAN builds on are present. `brew` is the one that
/// matters: without it nothing outside the App Store can be checked or updated.
/// `jq` reads Homebrew's data for names and versions, `mas` is the App Store.
#[derive(Clone, serde::Serialize)]
struct ToolingStatus {
    brew: bool,
    jq: bool,
    mas: bool,
    npm: bool,
    pip: bool,
    rbenv: bool,
    rvm: bool,
    asdf: bool,
    /// Whether this account can install Homebrew at all: its installer needs an
    /// administrator, and it is kinder to say so than to fail after a download.
    admin: bool,
}

// Debug builds only: PM_PRETEND_NO_BREW=1 makes the app believe Homebrew is
// missing until one pretend setup has run, so the first-run flow can be walked
// through on a machine that has Homebrew. Never compiled into a release.
static PRETEND_SETUP_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn pretending_no_brew() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("PM_PRETEND_NO_BREW").is_some()
        && !PRETEND_SETUP_DONE.load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
async fn tooling_status() -> ToolingStatus {
    async fn sh(script: String) -> bool {
        Command::new("bash")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|st| st.success())
            .unwrap_or(false)
    }
    async fn has(tool: &str) -> bool {
        sh(format!("{CHECK_PREAMBLE}command -v {tool} >/dev/null 2>&1")).await
    }
    let pretend = pretending_no_brew();
    ToolingStatus {
        brew: !pretend && has("brew").await,
        jq: !pretend && has("jq").await,
        mas: !pretend && has("mas").await,
        npm: has("npm").await,
        pip: has("pip3").await || has("pip").await,
        rbenv: has("rbenv").await,
        rvm: sh(format!("{CHECK_PREAMBLE}command -v rvm >/dev/null 2>&1 || [ -s \"$HOME/.rvm/scripts/rvm\" ]")).await,
        asdf: has("asdf").await,
        admin: sh("dseditgroup -o checkmember -m \"$USER\" admin >/dev/null 2>&1".to_string()).await,
    }
}

// Installs Homebrew the way Homebrew documents — its own install script — and
// then the two helpers. The script asks for the administrator password through
// our askpass dialog (it honours SUDO_ASKPASS), installs Apple's Command Line
// Tools itself when they are missing, and refuses cleanly for a user who is not
// an administrator. Output streams to the window as a run so a long Command
// Line Tools download does not look like a hang.
#[tauri::command]
async fn setup_homebrew(app: AppHandle) {
    let section = "setup";
    let pretend = pretending_no_brew();
    let script = if pretend {
        PRETEND_SETUP_DONE.store(true, std::sync::atomic::Ordering::Relaxed);
        r#"
echo "→  Installing Homebrew. This can take several minutes; if your Mac needs Apple's command line tools first, those are installed too."
echo "==> Checking for \`sudo\` access..."; sleep 1
echo "==> Downloading and installing Homebrew..."; sleep 2
echo "==> Installation successful!"
echo "→  Installing the helpers PartyMAN uses: jq, to read Homebrew's data, and mas, for the App Store…"; sleep 2
echo "🍺  jq was successfully installed!"
echo "🍺  mas was successfully installed!"
echo "✔  Homebrew is set up. (pretend run — nothing was installed)"
"#
    } else {
        r#"
export NONINTERACTIVE=1
export PM_ASKPASS_APP='Homebrew'
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
if command -v brew &>/dev/null; then
  echo "→  Homebrew is already installed."
else
  echo "→  Installing Homebrew. This can take several minutes; if your Mac needs Apple's command line tools first, those are installed too."
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" 2>&1
fi
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
if command -v brew &>/dev/null; then
  echo "→  Installing the helpers PartyMAN uses: jq, to read Homebrew's data, and mas, for the App Store…"
  if brew install jq mas 2>&1; then
    echo "✔  Homebrew is set up."
  else
    echo "⚠  Homebrew is installed, but not every helper could be. App Store checking needs mas."
  fi
else
  echo "✖  Homebrew could not be installed. If you are not an administrator on this Mac, an administrator needs to install it."
fi
"#
    };
    let ts = now_secs();
    let run_id = new_run_id(section);
    diag(&app, &format!("setup: homebrew ({run_id})"));
    let started = std::time::Instant::now();
    let result = run_upgrade_shell(&app, section, script).await;
    let outcome = outcome_from_lines(&result.lines, result.exit_code).to_string();
    diag(&app, &format!("setup: {outcome} ({run_id})"));
    append_upgrade_log(&app, HistoryEntry {
        ts,
        label: section_label(section).to_string(),
        section: section.to_string(),
        items: vec!["homebrew".to_string()],
        item_names: vec!["Homebrew".to_string()],
        versions: Vec::new(),
        run_id,
        kind: "setup".to_string(),
        outcome,
        duration_secs: secs_since(started),
        exit_code: result.exit_code,
        lines: result.lines,
    });
}

#[tauri::command]
fn open_app_management_settings(app: AppHandle) {
    diag(&app, "opening System Settings → App Management");
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AppBundles")
        .spawn();
}

// Reveals the folder holding updates.log and partyman.log in the Finder.
#[tauri::command]
fn open_logs_folder(app: AppHandle) {
    if let Ok(dir) = app.path().app_data_dir() {
        let _ = std::process::Command::new("open").arg(dir).spawn();
    }
}

#[tauri::command]
fn get_upgrade_history(app: AppHandle) -> Vec<HistoryEntry> {
    let path = match log_path(&app) {
        Some(p) => p,
        None => return vec![],
    };
    if !path.exists() {
        return vec![];
    }
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut entries: Vec<HistoryEntry> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .map(|mut e| {
            // Entries from before these fields existed: the outcome and the
            // versions can still be read off the output, and a batch written
            // in one second by one section was one run.
            if e.outcome.is_empty() {
                e.outcome = outcome_from_lines(&e.lines, e.exit_code).to_string();
            }
            if e.kind.is_empty() {
                e.kind = if e.section == "untracked_apps" { "adopt" } else { "update" }.to_string();
            }
            if e.run_id.is_empty() {
                e.run_id = format!("{}-{}", e.ts, e.section);
            }
            if e.versions.is_empty() && e.kind == "update" {
                e.versions = versions_from_lines(&e.lines, e.items.first().map(|s| s.as_str()));
            }
            e
        })
        .flat_map(split_legacy_adoption)
        .collect();
    entries.reverse(); // newest first
    entries
}

async fn emit_line(app: &AppHandle, section: &str, line: &str) {
    let _ = app.emit(
        "check-output",
        OutputPayload { section: section.to_string(), line: line.to_string() },
    );
}

async fn emit_upgrade_line(app: &AppHandle, section: &str, line: &str) {
    let _ = app.emit(
        "upgrade-output",
        OutputPayload { section: section.to_string(), line: line.to_string() },
    );
}

async fn emit_upgrade_status(app: &AppHandle, section: &str, status: &str) {
    let _ = app.emit(
        "upgrade-status",
        StatusPayload { section: section.to_string(), status: status.to_string() },
    );
}

async fn emit_status(app: &AppHandle, section: &str, status: &str) {
    let _ = app.emit(
        "check-status",
        StatusPayload { section: section.to_string(), status: status.to_string() },
    );
}

// Puts the version managers on PATH so checks see the same tools the user's own
// shell would. Checks never need root, so this carries no askpass plumbing.
const CHECK_PREAMBLE: &str = r#"
export PATH="$HOME/.rvm/bin:$HOME/.rbenv/bin:$HOME/.nvm/versions/node/$(ls $HOME/.nvm/versions/node 2>/dev/null | tail -1)/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
command -v rbenv &>/dev/null && eval "$(rbenv init -)"
"#;

// Runs a section's check and returns its output instead of streaming it to a
// window. Used by the scheduled run, which has no webview to emit events to.
pub(crate) async fn run_check_collect(section: &str) -> Vec<String> {
    let script = match check_script(section) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let preamble = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        CHECK_PREAMBLE
    } else { "" };
    let shell = if cfg!(target_os = "windows") { "powershell" } else { "bash" };
    let flag  = if cfg!(target_os = "windows") { "-Command" } else { "-c" };

    let out = Command::new(shell)
        .arg(flag)
        .arg(format!("{preamble}{script}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .chain(String::from_utf8_lossy(&o.stderr).lines().collect::<Vec<_>>())
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

// How long output may keep arriving after the shell has exited, for the readers
// to catch up with what is still sitting in the pipe.
const DRAIN_GRACE: Duration = Duration::from_secs(3);

type LineSink = Arc<Mutex<Option<Vec<String>>>>;

/// Streams a child's stdout and stderr to `on_line` as they arrive, and returns
/// every line once the child has exited.
///
/// The run is over when the shell exits, not when its pipes close. Anything the
/// script starts inherits those pipes, and a process left running afterwards
/// keeps them open indefinitely. Waiting for end-of-file waited on that process
/// instead: the batch finished but never reported done, so the section stayed on
/// "Updating…", Run Check stayed disabled, and nothing reached the history until
/// the app was force-quit.
///
/// Readers still going after the grace period are left to drain in the
/// background rather than cancelled, so whatever holds the pipe never has its
/// writes fail with SIGPIPE. What they read from then on is discarded.
struct RunResult {
    lines: Vec<String>,
    exit_code: Option<i32>,
    /// False when something still held the output pipe after the shell exited
    /// and the readers were left behind: a sign of a process the run started
    /// and did not wait for.
    drained: bool,
}

async fn stream_child<F>(child: &mut Child, on_line: F) -> RunResult
where
    F: Fn(&str) + Send + Sync + 'static,
{
    let sink: LineSink = Arc::new(Mutex::new(Some(Vec::new())));
    let on_line = Arc::new(on_line);
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_line_reader(out, Arc::clone(&sink), Arc::clone(&on_line)));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_line_reader(err, Arc::clone(&sink), Arc::clone(&on_line)));
    }

    let exit_code = child.wait().await.ok().and_then(|st| st.code());
    let drained = tokio::time::timeout(DRAIN_GRACE, async {
        for r in &mut readers {
            let _ = r.await;
        }
    })
    .await
    .is_ok();

    // Dropping the handles detaches any reader still running; it does not stop it.
    let lines = sink.lock().ok().and_then(|mut s| s.take()).unwrap_or_default();
    RunResult { lines, exit_code, drained }
}

fn spawn_line_reader<R, F>(pipe: R, sink: LineSink, on_line: Arc<F>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    F: Fn(&str) + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Ok(mut guard) = sink.lock() {
                if let Some(lines) = guard.as_mut() {
                    on_line(&line);
                    lines.push(line);
                }
            }
        }
    })
}

async fn run_shell(app: &AppHandle, section: &str, script: &str) -> RunResult {
    let shell = if cfg!(target_os = "windows") { "powershell" } else { "bash" };
    let flag  = if cfg!(target_os = "windows") { "-Command" } else { "-c" };

    let preamble = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        CHECK_PREAMBLE
    } else { "" };

    let full_script = format!("{}{}", preamble, script);

    let mut child = match Command::new(shell)
        .arg(flag)
        .arg(&full_script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            diag(app, &format!("check {section}: failed to start the shell: {e}"));
            emit_line(app, section, &format!("Failed to spawn: {e}")).await;
            emit_status(app, section, "error").await;
            return RunResult { lines: vec![], exit_code: None, drained: true };
        }
    };

    let started = std::time::Instant::now();
    let app1 = app.clone();
    let sec1 = section.to_string();
    let result = stream_child(&mut child, move |line| {
        if line.starts_with("__PM_") {
            return;
        }
        let _ = app1.emit(
            "check-output",
            OutputPayload { section: sec1.clone(), line: line.to_string() },
        );
    })
    .await;
    diag(app, &format!(
        "check {section}: exit {} after {}s, {} lines{}",
        result.exit_code.map_or("none".to_string(), |c| c.to_string()),
        secs_since(started),
        result.lines.len(),
        if result.drained { "" } else { " — output still open after exit; readers detached" },
    ));
    result
}

// Sentinel lines emitted around each cask so the single-shell batch output can be
// split back into per-cask history entries. Filtered out of the live UI stream.
const CASK_MARKER_PREFIX: &str = "__PM_CASK_";
const CASK_START: &str = "__PM_CASK_START__:";
const CASK_END: &str = "__PM_CASK_END__:";
const CASK_EXIT: &str = "__PM_CASK_EXIT__:";

/// Splits one shell's output back into the per-item groups the sentinels mark,
/// each with the exit status its brew call reported (when the script emits one).
/// Lines outside any group — the batch summary, for instance — are dropped.
fn split_by_item(lines: &[String]) -> Vec<(String, Vec<String>, Option<i32>)> {
    let mut groups: Vec<(String, Vec<String>, Option<i32>)> = Vec::new();
    let mut cur: Option<(String, Vec<String>, Option<i32>)> = None;
    for line in lines {
        if let Some(token) = line.strip_prefix(CASK_START) {
            if let Some(g) = cur.take() { groups.push(g); }
            cur = Some((token.to_string(), Vec::new(), None));
        } else if line.strip_prefix(CASK_END).is_some() {
            if let Some(g) = cur.take() { groups.push(g); }
        } else if let Some(rest) = line.strip_prefix(CASK_EXIT) {
            if let Some((_, _, exit)) = cur.as_mut() {
                *exit = rest.rsplit(':').next().and_then(|c| c.parse().ok());
            }
        } else if let Some((_, body_lines, _)) = cur.as_mut() {
            body_lines.push(line.clone());
        }
    }
    if let Some(g) = cur.take() { groups.push(g); }
    groups
}

// Askpass preamble: sets SUDO_ASKPASS to a helper that pops a native password
// dialog whenever a child process (e.g. Homebrew) shells out to `sudo -A`.
//   - Names the specific app (or the batch) via $PM_ASKPASS_APP when set.
//   - Uses a plain `osascript` dialog (no "System Events") to avoid the TCC
//     automation permission prompt.
//   - Exits non-zero on Cancel or timeout so sudo aborts cleanly instead of
//     receiving an error string as the password.
//   - Asks ONCE per run, without ever storing the password: sudo's own
//     credential cache covers every later `sudo` call in the batch. That cache
//     is keyed to the invoking terminal, so run_upgrade_shell gives the shell a
//     controlling terminal shared by all its descendants — otherwise sudo falls
//     back to keying by parent process and each `brew` invocation asks again.
//     The keep-alive below stops that credential expiring mid-batch.
//   - Warns on a rejected password: sudo re-invokes the askpass helper for each
//     of its retries, so a second call from the same sudo process (same $PPID)
//     within a few seconds means what we handed over was wrong. $PM_ASK_STAMP
//     holds only "<ppid> <epoch>" — no password material is written to disk.
const ASKPASS_PREAMBLE: &str = r#"
export PATH="$HOME/.rvm/bin:$HOME/.rbenv/bin:$HOME/.nvm/versions/node/$(ls $HOME/.nvm/versions/node 2>/dev/null | tail -1)/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
command -v rbenv &>/dev/null && eval "$(rbenv init -)"
_pm_dir=$(mktemp -d /tmp/partyman-askpass-XXXXXX)
chmod 700 "$_pm_dir"
_pm_askpass="$_pm_dir/askpass"
_pm_fifo="$_pm_dir/pw.fifo"
mkfifo "$_pm_fifo" && chmod 600 "$_pm_fifo"

# The paths are written into the helper rather than passed through the
# environment. Homebrew invokes sudo with a scrubbed environment — SUDO_ASKPASS
# survives, nothing else does — so a helper that reads its configuration from
# variables finds none, tries to create its pipe at the filesystem root, and
# exits empty-handed. sudo reports that as "no password was provided".
{
  printf '#!/bin/bash\n'
  printf 'PM_PW_FIFO=%q\n'   "$_pm_fifo"
  printf 'PM_PW_DIR=%q\n'    "$_pm_dir"
  printf 'PM_PW_RETRY=%q\n'  "$_pm_dir/retry"
  printf 'PM_ASK_STAMP=%q\n' "$_pm_dir/asked"
  cat <<'PARTYMAN_ASKPASS'
# sudo calls this whenever it wants a password. It never prompts itself: it asks
# the server below, which prompts once and then answers from memory.
_pm_stamp="$PM_ASK_STAMP"
if [ -n "$_pm_stamp" ] && [ -s "$_pm_stamp" ]; then
  _pm_last=$(cat "$_pm_stamp" 2>/dev/null)
  # The same sudo process asking again within seconds means what it was given
  # was rejected, so tell the server to discard it and ask afresh.
  if [ "${_pm_last%% *}" = "$PPID" ] && [ $(( $(date +%s) - ${_pm_last##* } )) -lt 20 ]; then
    touch "$PM_PW_RETRY" 2>/dev/null
  fi
fi
[ -n "$_pm_stamp" ] && printf '%s %s' "$PPID" "$(date +%s)" > "$_pm_stamp"

# Each request carries its own reply pipe. A single shared pipe lets two
# exchanges run together and the answers arrive spliced into one another.
_pm_reply="$PM_PW_DIR/reply.$$"
mkfifo "$_pm_reply" 2>/dev/null || exit 1
chmod 600 "$_pm_reply"
if ! printf '%s\n' "$_pm_reply" > "$PM_PW_FIFO" 2>/dev/null; then
  rm -f "$_pm_reply"
  exit 1
fi
cat "$_pm_reply"
rm -f "$_pm_reply"
PARTYMAN_ASKPASS
} > "$_pm_askpass"
chmod 700 "$_pm_askpass"

export SUDO_ASKPASS="$_pm_askpass"
# Used by the server below, which runs in this shell and keeps its environment.
PM_PW_RETRY="$_pm_dir/retry"

# The password server. It is asked once and answers for the rest of the run, so
# the number of prompts does not depend on sudo's credential cache surviving —
# which it does not across a long batch, since each cask can outlast the
# five-minute timeout while downloading.
#
# The password lives only in this subshell's memory. It is never written to disk
# and never exported, so it is not visible in any child's environment. Opening
# the pipe for writing blocks until something actually asks, so nothing is
# requested unless a password is genuinely needed.
(
  _pw=""
  _pm_again=""
  # Held open read-write for the life of the run. Reopening per request made the
  # pipe readerless between requests, so callers arriving together got EPIPE, and
  # a failed read ended the loop outright — killing the server and leaving every
  # later request with no answer at all. Keeping our own write end open means the
  # read blocks for the next request instead of hitting EOF.
  exec 9<>"$_pm_fifo" || exit 1
  while :; do
    IFS= read -r _pm_reply <&9 || continue
    [ -z "$_pm_reply" ] && continue
    if [ -f "$PM_PW_RETRY" ]; then
      _pw=""
      rm -f "$PM_PW_RETRY"
      _pm_again="That password didn't work. "
    fi
    if [ -z "$_pw" ]; then
      _pm_app="${PM_ASKPASS_APP:-}"
      if [ -n "$_pm_app" ]; then
        _pm_msg="${_pm_again}PartyMAN Update Manager needs your administrator password to update ${_pm_app}."
      else
        _pm_msg="${_pm_again}PartyMAN Update Manager needs your administrator password to complete this update."
      fi
      # Shown by System Events and brought to the front first. From the menu bar
      # there is no PartyMAN window to anchor it, and a dialog that opens behind
      # whatever the user is reading goes unanswered until it gives up — which
      # is what "no password was provided" from sudo means. Five minutes, since
      # the download before it can take two.
      _pw=$(osascript -e 'tell application "System Events"' -e 'activate' -e "display dialog \"${_pm_msg}\" default answer \"\" with hidden answer with title \"PartyMAN Update Manager\" with icon caution giving up after 300" -e 'text returned of result' -e 'end tell' 2>"$_pm_dir/dialog.err")
      _pm_again=""
      # Cancel is AppleScript error -128; giving up after the timeout returns an
      # empty answer with no error. The scripts tell the two apart in their output.
      if [ -z "$_pw" ] && grep -q -- "-128" "$_pm_dir/dialog.err" 2>/dev/null; then
        touch "$_pm_dir/cancelled"
      else
        rm -f "$_pm_dir/cancelled"
      fi
    fi
    # A caller that gave up leaves nothing reading; that must not kill the server.
    printf '%s' "$_pw" > "$_pm_reply" 2>/dev/null || true
  done
) >/dev/null 2>&1 &
_pm_server=$!
disown "$_pm_server" 2>/dev/null

# run_upgrade_shell attaches a controlling terminal, which sudo prefers over
# SUDO_ASKPASS — and a prompt written there is invisible and never answered, so it
# would block forever. Our own calls and Homebrew's always pass -A, so this shim
# only matters for third-party scripts that shell out to a bare `sudo`. Left alone
# if the caller already chose how to read the password.
mkdir -p "$_pm_dir/bin"
cat > "$_pm_dir/bin/sudo" <<'PARTYMAN_SUDO'
#!/bin/bash
# Only sudo's own leading options are inspected; scanning further could mistake a
# flag belonging to the command being run (sudo tar -S …) for one of sudo's.
for _a in "$@"; do
  case "$_a" in
    --) break ;;
    -A|--askpass|-S|--stdin|-*[AS]*) exec /usr/bin/sudo "$@" ;;
    -*) ;;
    *) break ;;
  esac
done
exec /usr/bin/sudo -A "$@"
PARTYMAN_SUDO
chmod 700 "$_pm_dir/bin/sudo"
export PATH="$_pm_dir/bin:$PATH"

trap '[ -n "$_pm_server" ] && kill "$_pm_server" 2>/dev/null; [ -n "$_pm_dir" ] && rm -rf "$_pm_dir"' EXIT
"#;

// Label for the one password dialog that covers a whole batch: the app's own name
// when it is the only one, otherwise a count. Sanitized for the AppleScript
// string and for the single-quoted shell export it is interpolated into.
fn askpass_scope(names: &[String]) -> String {
    let raw = match names {
        [one] => one.clone(),
        _ => format!("these {} apps", names.len()),
    };
    raw.replace(['\\', '"'], "").replace('\'', "'\\''")
}

// Holds both ends of the pty open for the child's lifetime; closing the master
// would hang up the terminal we just attached. The parent's copy of the slave fd
// cannot be closed before spawn (the child needs to inherit it), so it is closed
// here too rather than leaked.
#[cfg(unix)]
struct PtyMaster {
    master: libc::c_int,
    slave: libc::c_int,
}

#[cfg(unix)]
impl Drop for PtyMaster {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.master);
            libc::close(self.slave);
        }
    }
}

// Give `cmd` a pty as its controlling terminal, so that one sudo authentication
// covers every `sudo` call the batch makes. sudo keys its cached credential to
// the invoking terminal; with no terminal at all it keys by parent process
// instead, which is why each `brew` invocation used to pop its own dialog.
//
// Only the *controlling* terminal is a pty — stdout/stderr stay pipes, so brew
// still sees a non-tty and its output keeps the exact form we already parse (no
// spinners, colour codes or CR line endings).
//
// The master end is held open but never read: nothing writes to this terminal in
// normal operation, because every sudo call in a batch routes its prompt to
// SUDO_ASKPASS — ours pass `-A` explicitly, Homebrew adds it whenever
// SUDO_ASKPASS is set (system_command.rb), and ASKPASS_PREAMBLE shims `sudo` on
// PATH so third-party scripts get it too. A `sudo` that still managed to reach
// this terminal would prompt into it and block, since sudo discards queued input
// (TCSAFLUSH) and macOS sets no passwd_timeout; that is why the shim exists.
//
// Best effort: on any failure the child still runs, just without a controlling
// terminal — i.e. the previous behaviour of asking per `brew` invocation.
#[cfg(unix)]
fn attach_controlling_pty(cmd: &mut Command) -> Option<PtyMaster> {
    use std::os::unix::process::CommandExt;

    let (master, slave) = unsafe {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let rc = libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return None;
        }
        (master, slave)
    };

    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            // Signal-safe calls only, and deliberately non-fatal: a child without
            // a controlling terminal still updates correctly.
            if libc::setsid() != -1 {
                libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            }
            libc::close(slave);
            Ok(())
        });
    }

    Some(PtyMaster { master, slave })
}

// Returns collected output lines for logging (including any cask sentinels).
async fn run_upgrade_shell(app: &AppHandle, section: &str, script: &str) -> RunResult {
    let shell = if cfg!(target_os = "windows") { "powershell" } else { "bash" };
    let flag  = if cfg!(target_os = "windows") { "-Command" } else { "-c" };

    let preamble = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        ASKPASS_PREAMBLE
    } else { "" };

    let full_script = format!("{}{}", preamble, script);

    let mut cmd = Command::new(shell);
    cmd.arg(flag)
        .arg(&full_script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Kept alive until the child exits; see attach_controlling_pty.
    #[cfg(unix)]
    let _pty = attach_controlling_pty(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            diag(app, &format!("update {section}: failed to start the shell: {e}"));
            emit_upgrade_line(app, section, &format!("Failed to spawn: {e}")).await;
            emit_upgrade_status(app, section, "error").await;
            return RunResult { lines: vec![], exit_code: None, drained: true };
        }
    };

    let started = std::time::Instant::now();
    let app1 = app.clone();
    let sec1 = section.to_string();
    let result = stream_child(&mut child, move |line| {
        if !line.starts_with(CASK_MARKER_PREFIX) {
            let _ = app1.emit(
                "upgrade-output",
                OutputPayload { section: sec1.clone(), line: line.to_string() },
            );
        }
    })
    .await;
    diag(app, &format!(
        "update {section}: exit {} after {}s, {} lines{}",
        result.exit_code.map_or("none".to_string(), |c| c.to_string()),
        secs_since(started),
        result.lines.len(),
        if result.drained { "" } else { " — output still open after exit; readers detached" },
    ));
    emit_upgrade_status(app, section, "done").await;
    result
}

fn check_script(section: &str) -> Option<&'static str> {
    match section {
        "macos_updates" => Some(r#"
if command -v softwareupdate &>/dev/null; then
  updates=$(softwareupdate -l 2>&1)
  if echo "$updates" | grep -q "No new software available"; then
    echo "✔  macOS is up to date."
  else
    # Each update is a "* Label: …" line followed by a "Title: …, Version: …,
    # … Action: restart," line; the item line carries what the app needs.
    echo "$updates" | awk '
      /^[ \t]*\*[ \t]*Label:/ { label = $0; sub(/^[ \t]*\*[ \t]*Label:[ \t]*/, "", label); print; next }
      /Title:/ && label != "" {
        title = $0; sub(/.*Title:[ \t]*/, "", title); sub(/,.*/, "", title)
        ver = ""; if (match($0, /Version:[ \t]*[^,]+/)) { ver = substr($0, RSTART + 8, RLENGTH - 8); gsub(/^[ \t]+/, "", ver) }
        note = ""; if ($0 ~ /Action:[ \t]*restart/) note = "restart"
        printf "__PM_ITEM__\t%s\t%s\t\t%s\t%s\n", label, title, ver, note
        print; label = ""; next }
      /^Software Update Tool/ || /^$/ || /^Finding available software/ { next }
      { print }'
  fi
else
  echo "✖  softwareupdate not found — not running on macOS"
fi
"#),
        "brew_casks" => Some(r#"
if command -v brew &>/dev/null; then
  echo "→  Refreshing Homebrew…"
  brew update --quiet 2>/dev/null
  outdated=$(brew outdated --cask --greedy 2>/dev/null | awk 'NF { print $1 }')
  # Casks Homebrew has disabled still show as outdated, but `brew upgrade`
  # refuses them, so they could never clear. They are listed after the "→" line,
  # where the item parsers stop, so they are neither counted nor offered.
  disabled=""
  info=""
  if [ -n "$outdated" ] && command -v jq &>/dev/null; then
    info=$(brew info --cask --json=v2 $outdated 2>/dev/null)
    disabled=$(printf '%s' "$info" \
      | jq -r '.casks[] | select(.disabled == true)
          | [.token, (.disable_date // ""), (.disable_replacement_cask // "")] | join("|")' 2>/dev/null)
  fi
  updatable=""
  while IFS= read -r tok; do
    [ -z "$tok" ] && continue
    printf '%s\n' "$disabled" | cut -d'|' -f1 | grep -qxF "$tok" && continue
    updatable="$updatable$tok
"
  done <<< "$outdated"
  if [ -z "$updatable" ]; then
    echo "✔  All Homebrew cask apps are up to date."
  else
    echo "⚠  Outdated apps:"
    if [ -n "$info" ]; then
      # One item line per updatable cask — token to act on, name to show,
      # installed and available versions — and a readable line beside it.
      printf '%s' "$info" \
        | jq -r '.casks[] | select(.disabled != true)
            | [.token, (.name[0] // .token), (.installed // ""), (.version // "")] | @tsv' 2>/dev/null \
        | while IFS=$'\t' read -r tok name inst ver; do
          [ -z "$tok" ] && continue
          printf '__PM_ITEM__\t%s\t%s\t%s\t%s\t\n' "$tok" "$name" "$inst" "$ver"
          if [ -n "$inst" ]; then
            printf '   %s  %s → %s  (%s)\n' "$name" "${inst%%,*}" "${ver%%,*}" "$tok"
          else
            printf '   %s  → %s  (%s)\n' "$name" "${ver%%,*}" "$tok"
          fi
        done
    else
      printf '%s' "$updatable" | while read -r line; do echo "   $line"; done
    fi
  fi
  if [ -n "$disabled" ]; then
    echo "→  Homebrew has disabled these, so it can no longer update them. They are not counted:"
    printf '%s\n' "$disabled" | while IFS='|' read -r tok date repl; do
      msg="   $tok"
      [ -n "$date" ] && msg="$msg (disabled $date)"
      [ -n "$repl" ] && msg="$msg: replaced by $repl, install it with: brew install --cask $repl"
      echo "$msg"
    done
  fi
  # Apps installed twice: once from the App Store and again by Homebrew. Each
  # source checks its own copy, and both counts stand when both copies are
  # behind, but the user should know there are two. A cask is taken to own an
  # App Store app when its artifacts name that app's bundle identifier or path,
  # or its token, display name or app artifact is the app's name. The name test
  # is needed for pkg casks: microsoft-word never names com.microsoft.Word
  # outright, only inside its cleanup paths.
  twice=""
  if command -v jq &>/dev/null; then
    mas_apps=$(for app in /Applications/*.app "$HOME/Applications"/*.app; do
      [ -e "$app/Contents/_MASReceipt/receipt" ] || continue
      printf '%s|%s\n' \
        "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist" 2>/dev/null)" \
        "$(basename "$app" .app)"
    done)
    installed=$(brew list --cask 2>/dev/null)
    if [ -n "$mas_apps" ] && [ -n "$installed" ]; then
      cask_refs=$(brew info --cask --json=v2 $installed 2>/dev/null \
        | jq -r '.casks[] | .token as $t
            | ((.artifacts | .. | strings | "\($t)|s|\(.)"),
               ((.name[]?, $t, (.artifacts[]?.app[]? | strings)) | "\($t)|n|\(.)"))' 2>/dev/null)
      twice=$(printf '%s\n' "$mas_apps" | while IFS='|' read -r id name; do
        [ -z "$name" ] && continue
        tok=$(printf '%s\n' "$cask_refs" | awk -F'|' -v id="$id" -v app="$name.app" '
          function norm(x) { x = tolower(x); sub(/\.app$/, "", x); gsub(/[^a-z0-9]/, "", x); return x }
          BEGIN { want = norm(app) }
          ($2 == "s" && ((id != "" && $3 == id) || substr($3, length($3) - length(app)) == "/" app)) \
            || ($2 == "n" && norm($3) == want) { print $1; exit }')
        [ -n "$tok" ] && printf '%s|%s\n' "$name" "$tok"
      done)
    fi
  fi
  if [ -n "$twice" ]; then
    echo "→  Installed twice, from the App Store and by Homebrew. Each copy is checked and counted on its own:"
    printf '%s\n' "$twice" | while IFS='|' read -r name tok; do
      echo "   $name: App Store, and Homebrew cask $tok"
    done
  fi
else
  echo "✖  brew not found — install from https://brew.sh"
fi
"#),
        "app_store" => Some(r#"
if command -v mas &>/dev/null; then
  outdated=$(mas outdated 2>/dev/null)
  if [ -z "$outdated" ]; then
    echo "✔  All App Store apps are up to date."
  else
    echo "⚠  Outdated App Store apps:"
    echo "$outdated" | while IFS= read -r line; do
      [ -z "$line" ] && continue
      id=${line%% *}; rest=${line#* }
      case "$rest" in
        *" ("*" -> "*")")
          name=${rest% (*}; vers=${rest##*(}; vers=${vers%)}
          inst=${vers%% -> *}; avail=${vers##* -> } ;;
        *) name=$rest; inst=""; avail="" ;;
      esac
      printf '__PM_ITEM__\t%s\t%s\t%s\t%s\t\n' "$id" "$name" "$inst" "$avail"
      echo "   $line"
    done
  fi
else
  echo "✖  mas not installed."
  echo "→  Install with: brew install mas"
fi
"#),
        "untracked_apps" => Some(r#"
cask_tokens=""
cask_apps=""
cask_strings=""
if command -v brew &>/dev/null; then
  cask_tokens=$(brew list --cask 2>/dev/null)
  if [ -n "$cask_tokens" ] && command -v jq &>/dev/null; then
    cask_json=$(brew info --cask --json=v2 $cask_tokens 2>/dev/null)
    cask_apps=$(printf '%s' "$cask_json" \
      | jq -r '.casks[].artifacts[]?.app[]? | select(type=="string")' 2>/dev/null)
    # Every string anywhere in an installed cask's artifacts. The uninstall
    # stanzas name the bundle identifiers a cask is responsible for, and for a
    # cask that installs from a .pkg that is the only thing tying it to its app:
    # tailscale-app declares no app artifact and shares no spelling with
    # Tailscale.app, and zoom does not look like zoom.us.app. It is the same
    # evidence the cask search uses to offer that cask in the first place.
    cask_strings=$(printf '%s' "$cask_json" \
      | jq -r '.casks[].artifacts | .. | strings' 2>/dev/null)
  fi
fi

is_tracked() {
  local app="$1"
  local base
  base=$(basename "$app")
  # Uninstallers, helper stubs and web shortcuts are not applications anyone
  # updates: an uninstaller ships alongside its app, a Drive shortcut is a
  # bookmark. Listing them is noise, and worse, it invites adopting an unrelated
  # cask that merely sounds similar.
  case "$base" in
    *[Uu]ninstall*|*"Helper.app"|*"URL Handler.app") return 0 ;;
  esac
  local bundle_id
  bundle_id=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist" 2>/dev/null)
  case "$bundle_id" in
    com.google.drivefs.shortcuts.*) return 0 ;;
  esac
  # PartyMAN updates itself, so it has no business appearing in its own list of
  # apps that lack auto-updates. Matched on bundle identifier so renaming the
  # app or installing it elsewhere does not bring it back.
  [ "$bundle_id" = "com.partyman.updater" ] && return 0
  # Apple ships its own apps under two different leaf certificates: older ones
  # say "Software Signing", current ones "macOS Software Signing". Matching only
  # the former let Apple's own apps — Safari among them — fall through as
  # untracked, since "Authority=Software Signing" is not a substring of
  # "Authority=macOS Software Signing".
  codesign -dvv "$app" 2>&1 | grep -qE "^Authority=(macOS )?Software Signing" && return 0
  [ -e "$app/Contents/_MASReceipt/receipt" ] && return 0
  [ -n "$cask_apps" ] && echo "$cask_apps" | grep -qxF "$base" && return 0
  if [ -n "$cask_strings" ]; then
    [ -n "$bundle_id" ] && echo "$cask_strings" | grep -qxF "$bundle_id" && return 0
    echo "$cask_strings" | grep -qxF "$app" && return 0
  fi
  local norm
  norm=$(echo "${base%.app}" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9')
  local tok
  while IFS= read -r tok; do
    [ -z "$tok" ] && continue
    [ "$(echo "$tok" | tr -cd 'a-z0-9')" = "$norm" ] && return 0
  done <<< "$cask_tokens"
  return 1
}

untracked=0
SEEN=$(mktemp)

for app in "/Applications"/*.app; do
  [ -e "$app" ] || continue
  name="$(basename "$app" .app)"
  is_tracked "$app" && continue
  echo "⚠  $name"
  echo "$name" >> "$SEEN"
  untracked=1
done

if [ -d "$HOME/Applications" ]; then
  for app in "$HOME/Applications"/*.app; do
    [ -e "$app" ] || continue
    name="$(basename "$app" .app)"
    grep -qxF "$name" "$SEEN" 2>/dev/null && continue
    is_tracked "$app" && continue
    echo "⚠  $name [~/Applications]"
    untracked=1
  done
fi

rm -f "$SEEN"
[ "$untracked" -eq 0 ] && echo "✔  No untracked apps found."
"#),
        "brew_formulae" => Some(r#"
if command -v brew &>/dev/null; then
  # --formula: without it brew lists outdated casks here too, and those are
  # Homebrew apps' business — they were being counted on both pages.
  outdated=$(brew outdated --formula --verbose 2>/dev/null)
  if [ -z "$outdated" ]; then
    echo "✔  All Homebrew formulae are up to date."
  else
    count=$(echo "$outdated" | grep -c '')
    echo "⚠  $count outdated formula(e):"
    echo "$outdated" | while read -r line; do echo "   $line"; done
    echo "→  To upgrade all: brew upgrade"
  fi
else
  echo "✖  brew not found"
fi
"#),
        "npm_globals" => Some(r#"
if command -v npm &>/dev/null; then
  outdated=$(npm outdated -g --parseable 2>/dev/null)
  if [ -z "$outdated" ]; then
    echo "✔  All global npm packages are up to date."
  else
    count=$(echo "$outdated" | grep -c '')
    echo "⚠  $count outdated global package(s):"
    npm outdated -g 2>/dev/null | while read -r line; do echo "   $line"; done
    echo "→  To upgrade all: npm update -g"
  fi
else
  echo "✖  npm not found"
fi
"#),
        "pip_packages" => Some(r#"
PIP_CMD=""
command -v pip3 &>/dev/null && PIP_CMD="pip3"
command -v pip  &>/dev/null && [ -z "$PIP_CMD" ] && PIP_CMD="pip"
if [ -n "$PIP_CMD" ]; then
  outdated=$($PIP_CMD list --outdated --format=columns 2>/dev/null | tail -n +3)
  if [ -z "$outdated" ]; then
    echo "✔  All pip packages are up to date."
  else
    count=$(echo "$outdated" | grep -c '')
    echo "⚠  $count outdated package(s):"
    echo "$outdated" | while read -r line; do echo "   $line"; done
    echo "→  Run '$PIP_CMD list --outdated' for full details"
  fi
else
  echo "✖  pip / pip3 not found"
fi
"#),
        "asdf" => Some(r#"
if command -v asdf &>/dev/null; then
  outdated=""
  n=0
  for p in $(asdf plugin list 2>/dev/null); do
    # `asdf list <plugin>` prints one version per line, the current one starred;
    # the newest installed is the last line. `asdf latest` asks the plugin.
    installed=$(asdf list "$p" 2>/dev/null | tr -d ' *' | grep -v '^$' | tail -1)
    latest=$(asdf latest "$p" 2>/dev/null)
    [ -z "$installed" ] && continue
    if [ -n "$latest" ] && [ "$installed" != "$latest" ]; then
      outdated="$outdated   $p  $installed → $latest
"
      n=$((n + 1))
    fi
  done
  if [ "$n" -eq 0 ]; then
    echo "✔  Every asdf runtime is at its latest version."
  else
    echo "⚠  $n runtime(s) with a newer version available:"
    printf '%s' "$outdated"
    echo "→  Installs alongside what you have; switch a project with: asdf set <plugin> <version>"
  fi
else
  echo "✖  asdf not found"
fi
"#),
        "ruby_rvm" => Some(r#"
if command -v rvm &>/dev/null || [ -s "$HOME/.rvm/scripts/rvm" ]; then
  [ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
  if command -v rvm &>/dev/null; then
    echo "→  Installed Ruby versions:"
    rvm list 2>/dev/null | grep -v "^$" | while read -r line; do echo "   $line"; done
    latest_known=$(rvm list known 2>/dev/null | grep -E "^\[ruby-\]" | tail -1 | tr -d '[]')
    current=$(rvm current 2>/dev/null)
    if [ -n "$latest_known" ] && [ -n "$current" ]; then
      if [[ "$current" == *"$latest_known"* ]]; then
        echo "✔  Current ($current) matches latest ($latest_known)."
      else
        echo "⚠  Current: $current  |  Latest: $latest_known"
        echo "→  To upgrade: rvm install $latest_known"
      fi
    fi
    outdated_gems=$(gem outdated 2>/dev/null)
    if [ -z "$outdated_gems" ]; then
      echo "✔  All gems up to date."
    else
      count=$(echo "$outdated_gems" | grep -c '')
      echo "⚠  $count outdated gem(s). Run 'gem outdated' for full list."
    fi
  else
    echo "✖  rvm could not be sourced"
  fi
else
  echo "✖  rvm not found"
fi
"#),
        "ruby_rbenv" => Some(r#"
if command -v rbenv &>/dev/null; then
  echo "→  Installed Ruby versions:"
  rbenv versions 2>/dev/null | while read -r line; do echo "   $line"; done
  current=$(rbenv version 2>/dev/null | awk '{print $1}')
  echo "→  Active: $current"
  outdated_gems=$(gem outdated 2>/dev/null)
  if [ -z "$outdated_gems" ]; then
    echo "✔  All gems up to date."
  else
    count=$(echo "$outdated_gems" | grep -c '')
    echo "⚠  $count outdated gem(s). Run 'gem outdated' for full list."
  fi
else
  echo "✖  rbenv not found"
fi
"#),
        _ => None,
    }
}

#[tauri::command]
async fn run_check(app: AppHandle, section: String) {
    match check_script(&section) {
        Some(script) => {
            tray_checking(&app, true);
            let lines = run_shell(&app, &section, script).await.lines;
            tray_checking(&app, false);
            let items = schedule::items_for(&app, &section, &lines);
            let _ = app.emit("check-status", CheckDonePayload {
                section: section.clone(),
                status: "done".to_string(),
                items,
            });
            // A check the user runs updates the count too, not only a scheduled one.
            if schedule::CHECKED_SECTIONS.contains(&section.as_str()) {
                let cfg = schedule::record_section(&app, &section, lines);
                set_tray_count(&app, cfg.last_total);
                let _ = app.emit("schedule-updated", cfg);
            }
        }
        None => {
            emit_line(&app, &section, &format!("Unknown section: {section}")).await;
            emit_status(&app, &section, "error").await;
        }
    }
}

// Printed after authorization so the user isn't left staring at a spinner: the
// native `do shell script … with administrator privileges` mechanism buffers all
// output and returns nothing until softwareupdate finishes, so there is no live
// progress during the (often multi-minute) install.
const MACOS_INSTALLING_NOTE: &str = "echo '→  Installing… macOS runs this with no live progress, so this window may look idle for several minutes. Please wait for the completion message.'";

// Branded heads-up shown before the native softwareupdate auth prompt. The system
// password dialog itself is drawn by macOS and shows "osascript"; this dialog makes
// clear PartyMAN triggered it and previews that name so it isn't a surprise. Returns
// a bash line that aborts the whole script cleanly if the user clicks Cancel.
fn macos_heads_up(what: &str) -> String {
    let template = r#"osascript -e 'display dialog "PartyMAN Update Manager is about to install __WHAT__.\n\nmacOS will now ask for your administrator password. Its prompt is shown by the system and may appear as \"osascript\"." with title "PartyMAN Update Manager" with icon note buttons {"Cancel", "Continue"} default button "Continue"' >/dev/null 2>&1 || { echo "✖  Update cancelled."; exit 0; }"#;
    template.replace("__WHAT__", what)
}

fn upgrade_script(section: &str) -> Option<String> {
    match section {
        "macos_updates" => {
            let heads_up = macos_heads_up("your available macOS system updates");
            Some(format!(
                "{heads_up}\n{MACOS_INSTALLING_NOTE}\nosascript -e 'do shell script \"softwareupdate -ia --verbose\" with administrator privileges' 2>&1\necho '→  macOS update complete.'"
            ))
        }
        "brew_casks" => Some(format!(r#"
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
if command -v brew &>/dev/null; then
  {fn_def}
  TMPOUT=$(mktemp)
  pm_brew_logged "$TMPOUT" upgrade --cask --greedy
  grep "It seems the App source" "$TMPOUT" 2>/dev/null \
    | sed 's/Error: //;s/:.*//' | tr -d ' ' | while IFS= read -r tok; do
    [ -z "$tok" ] && continue
    echo "→  Retrying $tok in ~/Applications…"
    brew upgrade --cask --appdir "$HOME/Applications" "$tok" 2>&1
  done
  rm -f "$TMPOUT"
  echo "→  Homebrew cask upgrade complete."
else
  echo "✖  brew not found"
fi
"#, fn_def = cask_fns())),
        "asdf" => Some(r#"
if command -v asdf &>/dev/null; then
  echo "→  Updating asdf plugins…"
  asdf plugin update --all 2>&1
  for p in $(asdf plugin list 2>/dev/null); do
    echo "→  Installing the latest $p… (a runtime can take a while to build)"
    asdf install "$p" latest 2>&1
  done
  echo "→  Done. Existing versions stay; switch a project or your default with: asdf set <plugin> <version>"
else
  echo "✖  asdf not found"
fi
"#.to_string()),
        "app_store" => Some(r#"
echo "→  Opening App Store Updates…"
open "macappstores://showUpdatesPage"
echo "✔  App Store opened — please click Update next to each app."
"#.to_string()),
        "brew_formulae" => Some(r#"
if command -v brew &>/dev/null; then
  brew upgrade 2>&1
  echo "→  Homebrew formulae upgrade complete."
else
  echo "✖  brew not found"
fi
"#.to_string()),
        "npm_globals" => Some(r#"
if command -v npm &>/dev/null; then
  npm update -g 2>&1
  echo "→  npm global packages updated."
else
  echo "✖  npm not found"
fi
"#.to_string()),
        "pip_packages" => Some(r#"
PIP_CMD=""
command -v pip3 &>/dev/null && PIP_CMD="pip3"
command -v pip  &>/dev/null && [ -z "$PIP_CMD" ] && PIP_CMD="pip"
if [ -n "$PIP_CMD" ]; then
  pkgs=$($PIP_CMD list --outdated --format=freeze 2>/dev/null | cut -d= -f1 | tr '\n' ' ')
  if [ -n "$pkgs" ]; then
    $PIP_CMD install --upgrade $pkgs 2>&1
    echo "→  pip packages updated."
  else
    echo "✔  Nothing to upgrade."
  fi
else
  echo "✖  pip / pip3 not found"
fi
"#.to_string()),
        "ruby_rvm" => Some(r#"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
if command -v gem &>/dev/null; then
  gem update 2>&1
  echo "→  gems updated."
else
  echo "✖  gem not found"
fi
"#.to_string()),
        "ruby_rbenv" => Some(r#"
if command -v gem &>/dev/null; then
  gem update 2>&1
  echo "→  gems updated."
else
  echo "✖  gem not found"
fi
"#.to_string()),
        _ => None,
    }
}

#[tauri::command]
async fn run_upgrade(app: AppHandle, section: String) {
    upgrade_section(&app, &section).await;
    settle_after_upgrade(&app, &section).await;
}

#[tauri::command]
async fn run_upgrade_items(app: AppHandle, section: String, items: Vec<String>, item_names: Vec<String>) {
    upgrade_items(&app, &section, &items, &item_names).await;
    settle_after_upgrade(&app, &section).await;
}

// Re-checks what is left so the menu-bar count matches what is now installed
// instead of what was outstanding before the upgrade ran. Not for the App Store:
// "updating" there only opens the App Store, so a recount now would record the
// updates as still outstanding before the user has installed anything. Its count
// moves on its next check.
async fn settle_after_upgrade(app: &AppHandle, section: &str) {
    if section == "app_store" || !schedule::CHECKED_SECTIONS.contains(&section) {
        return;
    }
    let lines = run_check_collect(section).await;
    let items = schedule::items_for(app, section, &lines);
    let cfg = schedule::record_section(app, section, lines.clone());
    set_tray_count(app, cfg.last_total);
    // The window's list is what its sidebar counts from, so it gets the fresh
    // result too — otherwise the app just updated would sit there as outdated
    // until the next check.
    let _ = app.emit("section-recounted", RecountPayload {
        section: section.to_string(),
        items,
        lines: lines.into_iter().filter(|l| !l.starts_with("__PM_")).collect(),
    });
    let _ = app.emit("schedule-updated", cfg);
}

/// What the re-check after an upgrade found, for the window to show without a
/// check of its own.
#[derive(Clone, serde::Serialize)]
struct RecountPayload {
    section: String,
    items: Vec<schedule::CheckItem>,
    lines: Vec<String>,
}

/// Runs a section's whole upgrade command. Returns the outcome, or None when the
/// section has no upgrade command.
async fn upgrade_section(app: &AppHandle, section: &str) -> Option<String> {
    let Some(script) = upgrade_script(section) else {
        emit_upgrade_line(app, section, &format!("No upgrade command for: {section}")).await;
        emit_upgrade_status(app, section, "error").await;
        return None;
    };
    emit_upgrade_status(app, section, "running").await;
    let ts = now_secs();
    let run_id = new_run_id(section);
    diag(app, &format!("update {section}: everything ({run_id})"));
    let started = std::time::Instant::now();
    let result = run_upgrade_shell(app, section, &script).await;
    let outcome = outcome_from_lines(&result.lines, result.exit_code).to_string();
    diag(app, &format!("update {section}: {outcome} ({run_id})"));
    append_upgrade_log(app, HistoryEntry {
        ts,
        label: section_label(section).to_string(),
        section: section.to_string(),
        items: vec![],
        item_names: vec![],
        versions: versions_from_lines(&result.lines, None),
        run_id,
        kind: "update".to_string(),
        outcome: outcome.clone(),
        duration_secs: secs_since(started),
        exit_code: result.exit_code,
        lines: result.lines,
    });
    Some(outcome)
}

/// Upgrades the chosen items of a section. Returns (name, outcome) per item: one
/// each for Homebrew, which reports per cask; the batch's outcome against every
/// name for the rest.
async fn upgrade_items(app: &AppHandle, section: &str, items: &[String], item_names: &[String]) -> Vec<(String, String)> {
    if items.is_empty() {
        emit_upgrade_line(app, section, "No items selected.").await;
        emit_upgrade_status(app, section, "done").await;
        return vec![];
    }

    // Brew casks run in a single shell so one sudo session (one password prompt)
    // covers the whole batch. Each cask is wrapped in sentinel markers so the
    // combined output can be split back into a per-cask history entry.
    if section == "brew_casks" {
        // Pair token with its display name before filtering so names stay aligned.
        let pairs: Vec<(String, String)> = items.iter().enumerate()
            .map(|(i, token)| (token.clone(), item_names.get(i).cloned().unwrap_or_else(|| token.clone())))
            .filter(|(token, _)| token.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.'))
            .collect();
        if pairs.is_empty() {
            emit_upgrade_line(app, section, "No items selected.").await;
            emit_upgrade_status(app, section, "done").await;
            return vec![];
        }

        let mut body = String::new();
        for (token, _) in &pairs {
            body.push_str(&format!("echo '{CASK_START}{token}'\n"));
            body.push_str(&format!("brew_upgrade_cask '{token}'\n"));
            body.push_str(&format!("echo '{CASK_END}{token}'\n"));
        }
        // One password covers the whole batch, so the dialog is labelled once for
        // the batch rather than per cask.
        let scope = askpass_scope(&pairs.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>());
        let script = format!(
            "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\nexport PM_ASKPASS_APP='{scope}'\n{fn_def}\npm_progress_start\n{body}pm_progress_stop\nelse\n  echo '✖  brew not found'\nfi",
            scope = scope,
            fn_def = cask_fns(),
            body = body,
        );
        emit_upgrade_status(app, section, "running").await;
        let ts = now_secs();
        let run_id = new_run_id(section);
        diag(app, &format!("update {section}: {} items ({run_id}): {}", pairs.len(),
            pairs.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(", ")));
        let started = std::time::Instant::now();
        let result = run_upgrade_shell(app, section, &script).await;
        let duration_secs = secs_since(started);

        let mut summary: Vec<String> = Vec::new();
        let mut outcomes: Vec<(String, String)> = Vec::new();
        for (token, body_lines, exit_code) in split_by_item(&result.lines) {
            let display_name = pairs.iter()
                .find(|(t, _)| *t == token)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| token.clone());
            let outcome = outcome_from_lines(&body_lines, exit_code).to_string();
            summary.push(format!("{token} {outcome}"));
            outcomes.push((display_name.clone(), outcome.clone()));
            append_upgrade_log(app, HistoryEntry {
                ts,
                label: section_label(section).to_string(),
                section: section.to_string(),
                items: vec![token.clone()],
                item_names: vec![display_name],
                versions: versions_from_lines(&body_lines, Some(&token)),
                run_id: run_id.clone(),
                kind: "update".to_string(),
                outcome,
                duration_secs,
                exit_code,
                lines: body_lines,
            });
        }
        // A cask the shell never reached (brew missing, the run cut short) was
        // not updated either.
        for (_, name) in &pairs {
            if !outcomes.iter().any(|(n, _)| n == name) {
                outcomes.push((name.clone(), "failed".to_string()));
            }
        }
        diag(app, &format!("update {section}: {} ({run_id})", summary.join(", ")));
        return outcomes;
    }

    let script: String = match section {
        "app_store" => {
            let list = if item_names.is_empty() { items.join(", ") } else { item_names.join(", ") };
            format!(
                "echo '→  Opening App Store Updates for: {list}'\nopen 'macappstores://showUpdatesPage'\necho '✔  App Store opened — please click Update next to each app.'"
            )
        }
        "macos_updates" => {
            let labels = items.iter()
                .map(|l| format!("\\\"{}\\\"", l.replace('"', "\\\"")))
                .collect::<Vec<_>>().join(" ");
            let names_src: Vec<String> = if item_names.is_empty() { items.to_vec() } else { item_names.to_vec() };
            let list = names_src.iter()
                .map(|n| n.replace(['\\', '"'], ""))
                .collect::<Vec<_>>().join(", ");
            let heads_up = macos_heads_up(&list);
            format!(
                "{heads_up}\n{MACOS_INSTALLING_NOTE}\nosascript -e 'do shell script \"softwareupdate -i {labels}\" with administrator privileges' 2>&1\necho '→  macOS update complete.'"
            )
        }
        _ => {
            emit_upgrade_line(app, section, "Individual upgrades not supported for this section.").await;
            emit_upgrade_status(app, section, "error").await;
            return vec![];
        }
    };

    emit_upgrade_status(app, section, "running").await;
    let ts = now_secs();
    let run_id = new_run_id(section);
    diag(app, &format!("update {section}: {} items ({run_id})", items.len()));
    let started = std::time::Instant::now();
    let result = run_upgrade_shell(app, section, &script).await;
    let display_names = if item_names.is_empty() { items.to_vec() } else { item_names.to_vec() };
    let outcome = outcome_from_lines(&result.lines, result.exit_code).to_string();
    diag(app, &format!("update {section}: {outcome} ({run_id})"));
    let outcomes = display_names.iter().map(|n| (n.clone(), outcome.clone())).collect();
    append_upgrade_log(app, HistoryEntry {
        ts,
        label: section_label(section).to_string(),
        section: section.to_string(),
        items: items.to_vec(),
        item_names: display_names,
        versions: versions_from_lines(&result.lines, None),
        run_id,
        kind: "update".to_string(),
        outcome,
        duration_secs: secs_since(started),
        exit_code: result.exit_code,
        lines: result.lines,
    });
    outcomes
}

#[derive(Clone, serde::Serialize)]
struct AppUpdateInfo {
    available: bool,
    version: String,
    url: String,
    notes: String,
}

fn version_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let mut it = s.split('.');
        let a = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let b = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let c = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (a, b, c)
    };
    parse(candidate) > parse(current)
}

#[tauri::command]
async fn check_app_update(current_version: String) -> AppUpdateInfo {
    let blank = AppUpdateInfo { available: false, version: String::new(), url: String::new(), notes: String::new() };
    let out = Command::new("curl")
        .args([
            "-sf", "--max-time", "8",
            "-H", "Accept: application/vnd.github+json",
            "-H", "User-Agent: PartyMAN-Update-Manager",
            "https://api.github.com/repos/paymonr/partyman_update_manager/releases/latest",
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(json) => {
                    let tag = json["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
                    let url = json["html_url"].as_str().unwrap_or("").to_string();
                    let notes = json["body"].as_str().unwrap_or("").to_string();
                    if !tag.is_empty() && version_newer(&tag, &current_version) {
                        AppUpdateInfo { available: true, version: tag, url, notes }
                    } else {
                        blank
                    }
                }
                Err(_) => blank,
            }
        }
        _ => blank,
    }
}

// Release notes for one specific version, used to show what changed after an
// update has already been applied — the pending update's own notes are gone by
// then, and a fresh download never had them.
/// Turns whatever the user pasted into a cask token, then checks it really
/// exists. Accepts a bare token or a formulae.brew.sh link, since looking the
/// cask up in a browser and copying the address is the natural way to find one.
#[tauri::command]
async fn resolve_cask_input(input: String) -> Option<CaskCandidate> {
    let trimmed = input.trim();
    let token = match trimmed.split("formulae.brew.sh/cask/").nth(1) {
        // .../cask/dbeaver-enterprise#default -> dbeaver-enterprise
        Some(rest) => rest
            .split(['#', '?', '/'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        None => trimmed.trim_start_matches('/').to_string(),
    };

    if token.is_empty()
        || token.len() > 64
        || !token.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.' || c == '+')
    {
        return None;
    }

    let script = format!(
        r#"export PATH="/usr/local/bin:/opt/homebrew/bin:$PATH"; brew info --cask --json=v2 '{token}' 2>/dev/null"#
    );
    let out = Command::new("bash").arg("-c").arg(&script).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let cask = json["casks"].as_array()?.first()?;
    let token = cask["token"].as_str()?.to_string();
    let name = cask["name"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .unwrap_or(&token)
        .to_string();
    // Chosen deliberately by the user, so it counts as confirmed.
    Some(CaskCandidate { token, name, exact: true })
}

#[tauri::command]
async fn get_release_notes(version: String) -> String {
    // Straight into a URL, so keep it to what a version can actually contain.
    if version.is_empty()
        || version.len() > 32
        || !version.chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return String::new();
    }

    let out = Command::new("curl")
        .args([
            "-sf", "--max-time", "8",
            "-H", "Accept: application/vnd.github+json",
            "-H", "User-Agent: PartyMAN-Update-Manager",
            &format!(
                "https://api.github.com/repos/paymonr/partyman_update_manager/releases/tags/v{version}"
            ),
        ])
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => serde_json::from_slice::<serde_json::Value>(&o.stdout)
            .ok()
            .and_then(|json| json["body"].as_str().map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[tauri::command]
fn open_release_url(url: String) {
    // Allowlisted so the webview cannot hand this arbitrary URLs to `open`.
    const ALLOWED: &[&str] = &[
        "https://github.com/paymonr/partyman_update_manager",
        "https://github.com/paymonr/kawaii-meadow",
        "https://github.com/paymonr",
    ];
    // Exact match, or the entry followed by a path separator. A bare prefix test
    // would also accept a look-alike account such as .../paymonr-evil.
    if ALLOWED
        .iter()
        .any(|base| url == *base || url.starts_with(&format!("{base}/")))
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
}

#[tauri::command]
fn get_platform() -> String {
    if cfg!(target_os = "macos") {
        "mac".to_string()
    } else if cfg!(target_os = "linux") {
        "linux".to_string()
    } else if cfg!(target_os = "windows") {
        "windows".to_string()
    } else {
        "unknown".to_string()
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
// The tray icon, kept so the update count on it can be refreshed later.
struct Tray(tauri::tray::TrayIcon);

// Held for the lifetime of the open app so a launchd run knows to stand down.
struct RunLock(#[allow(dead_code)] std::fs::File);

fn app_icon() -> Result<tauri::image::Image<'static>, Box<dyn std::error::Error>> {
    let png = include_bytes!("../icons/128x128@2x.png");
    let img = image::load_from_memory(png)?;
    let (w, h) = img.dimensions();
    Ok(tauri::image::Image::new_owned(img.to_rgba8().into_raw(), w, h))
}

// Removes the logo's dark backing plate, leaving the orange ring and white arrow
// on transparency so the menu bar shows through.
//
// A plain colour-key would leave a dark halo, because the pixels along each edge
// are a blend of the plate and the mark. Instead each pixel is un-composited: how
// far it has travelled from the plate colour towards the nearer of the two mark
// colours becomes its alpha, and it takes that mark colour outright. Edges stay
// smooth with no fringe.
fn drop_icon_plate(img: &mut image::RgbaImage) {
    const PLATE: [f32; 3] = [30.0, 39.0, 51.0]; // #1e2733
    const MARKS: [[f32; 3]; 2] = [
        [245.0, 128.0, 38.0], // #f58026, the ring
        [255.0, 255.0, 255.0], // the arrow
    ];

    let dist = |a: [f32; 3], b: [f32; 3]| {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    };

    for px in img.pixels_mut() {
        if px.0[3] == 0 {
            continue;
        }
        let c = [px.0[0] as f32, px.0[1] as f32, px.0[2] as f32];
        let mark = MARKS
            .iter()
            .min_by(|a, b| dist(c, **a).total_cmp(&dist(c, **b)))
            .copied()
            .unwrap_or(MARKS[1]);

        let span = dist(mark, PLATE);
        let coverage = if span > 0.0 {
            (dist(c, PLATE) / span).clamp(0.0, 1.0)
        } else {
            1.0
        };

        px.0[0] = mark[0] as u8;
        px.0[1] = mark[1] as u8;
        px.0[2] = mark[2] as u8;
        px.0[3] = (px.0[3] as f32 * coverage) as u8;
    }
}

// The menu-bar icon: the app's own logo, with the outstanding count in a badge
// over its top-right corner.
//
// It is deliberately *not* a template image. macOS treats a template as a mask —
// colour discarded, shape tinted to match the menu bar — which would flatten the
// logo and take the badge with it. Drawn in colour, both survive.
//
// Everything is composed at 4x and scaled down at the end, so the badge circle
// and the digits come out smooth rather than stepped.
fn tray_icon(count: usize) -> tauri::image::Image<'static> {
    const SIZE: u32 = 44; // 22pt at 2x, the standard menu-bar height
    const SS: u32 = 4;
    let big = SIZE * SS;

    let logo = image::load_from_memory(include_bytes!("../icons/128x128@2x.png"))
        .map(|img| {
            let mut rgba = img.to_rgba8();
            drop_icon_plate(&mut rgba);
            image::imageops::resize(&rgba, big, big, image::imageops::FilterType::Lanczos3)
        })
        .unwrap_or_else(|_| image::RgbaImage::new(big, big));
    let mut canvas = logo;

    if count > 0 {
        // A plain dot rather than a number. A count large enough to read at 22pt
        // covered most of the logo, and the exact figure is on the tooltip and in
        // the app; here it only needs to say "there is something waiting".
        let r = 7.0_f32;
        let (cx, cy) = (44.0 - r - 1.5, r + 1.5);
        let (bx, by, br) = (cx * SS as f32, cy * SS as f32, r * SS as f32);

        for y in 0..big {
            for x in 0..big {
                let dx = x as f32 + 0.5 - bx;
                let dy = y as f32 + 0.5 - by;
                let d = (dx * dx + dy * dy).sqrt();
                if d <= br {
                    // A light rim keeps the dot distinct from the logo behind it.
                    let px = if d > br - 1.4 * SS as f32 {
                        image::Rgba([255, 255, 255, 255])
                    } else {
                        image::Rgba([228, 48, 48, 255])
                    };
                    canvas.put_pixel(x, y, px);
                }
            }
        }
    }

    let scaled = image::imageops::resize(&canvas, SIZE, SIZE, image::imageops::FilterType::Lanczos3);
    tauri::image::Image::new_owned(scaled.into_raw(), SIZE, SIZE)
}

#[tauri::command]
fn next_run(app: AppHandle) -> i64 {
    let cfg = schedule::load(&app);
    if !cfg.enabled {
        return 0;
    }
    let from = if cfg.last_run == 0 { schedule::now_secs() } else { cfg.last_run };
    schedule::next_run_after(from as i64, &cfg)
}

#[tauri::command]
fn get_last_check(app: AppHandle) -> LastCheckView {
    let last = schedule::load_last_check(&app);
    let items = last
        .sections
        .iter()
        .map(|(id, lines)| (id.clone(), schedule::items_for(&app, id, lines)))
        .collect();
    LastCheckView { ts: last.ts, sections: last.sections, section_ts: last.section_ts, items }
}

// The last run as the window preloads it: the output per section plus the
// items already parsed from it.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LastCheckView {
    ts: u64,
    sections: std::collections::BTreeMap<String, Vec<String>>,
    section_ts: std::collections::BTreeMap<String, u64>,
    items: std::collections::BTreeMap<String, Vec<schedule::CheckItem>>,
}

// ---------------------------------------------------------------------------
// The menu-bar dropdown
// ---------------------------------------------------------------------------
//
// Built from what the last check saved and rebuilt whenever that changes, so the
// menu bar is useful on its own: how many updates, which apps per source, and
// Install all / Check now to act on them without opening the window. A native
// menu rather than a popover: it behaves like every other menu-bar item and
// costs nothing to keep current.

/// What the app is busy with, for the dropdown's header. Checks are counted
/// because several can overlap (Check all runs a section at a time while the
/// schedule may fire); installing is one batch at a time.
struct TrayActivity {
    checks: std::sync::atomic::AtomicUsize,
    /// What is being installed, as the header says it: "5 updates", "Brave".
    installing: Mutex<Option<String>>,
}

static TRAY_ACTIVITY: TrayActivity = TrayActivity {
    checks: std::sync::atomic::AtomicUsize::new(0),
    installing: Mutex::new(None),
};

/// The menu as last built, so a tick or a hover that changes nothing leaves the
/// menu alone — replacing it while it is open would make it jump.
static TRAY_PLAN: Mutex<Option<TrayPlan>> = Mutex::new(None);

pub(crate) fn tray_checking(app: &AppHandle, on: bool) {
    use std::sync::atomic::Ordering;
    if on {
        TRAY_ACTIVITY.checks.fetch_add(1, Ordering::SeqCst);
    } else {
        let _ = TRAY_ACTIVITY
            .checks
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)));
    }
    refresh_tray_menu(app);
}

fn tray_installing(app: &AppHandle, what: Option<String>) {
    if let Ok(mut g) = TRAY_ACTIVITY.installing.lock() {
        *g = what;
    }
    refresh_tray_menu(app);
}

fn tray_is_installing() -> bool {
    TRAY_ACTIVITY.installing.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// The sources the dropdown lists, in the order Install all takes them: Homebrew
/// first (that is what the password is for), the App Store next (it only opens
/// the App Store), macOS last because it can restart the Mac.
const TRAY_SOURCES: &[(&str, &str)] = &[
    ("brew_casks", "Homebrew apps"),
    ("app_store", "App Store"),
    ("macos_updates", "macOS updates"),
];

/// Names listed per source before "and N more…" takes over.
const TRAY_MAX_NAMES: usize = 12;

#[derive(Clone, Debug, PartialEq)]
struct TraySource {
    id: String,
    label: String,
    items: Vec<TrayItem>,
    more: usize,
}

/// One app in a source's submenu. Clicking it updates just that app.
#[derive(Clone, Debug, PartialEq)]
struct TrayItem {
    id: String,
    name: String,
    /// "Zoom  6.1.0 → 6.2.1"
    text: String,
}

/// Everything the dropdown says, worked out away from the menu objects so it
/// can be tested.
#[derive(Clone, Debug, PartialEq)]
struct TrayPlan {
    header: String,
    checked: Option<String>,
    sources: Vec<TraySource>,
    install_label: String,
    install_enabled: bool,
    busy: bool,
}

fn tray_plan(
    cfg: &schedule::ScheduleConfig,
    last: &schedule::LastCheck,
    checks_running: usize,
    installing: Option<&str>,
    now: u64,
) -> TrayPlan {
    let total = schedule::total_from_counts(&cfg.last_counts);
    let busy = installing.is_some() || checks_running > 0;
    let header = match (installing, checks_running, total) {
        (Some(what), _, _) => format!("Installing {what}…"),
        (None, c, _) if c > 0 => "Checking for updates…".to_string(),
        (None, _, 0) if cfg.last_run == 0 => "Not checked yet".to_string(),
        (None, _, 0) => "Everything is up to date".to_string(),
        (None, _, 1) => "1 update available".to_string(),
        (None, _, n) => format!("{n} updates available"),
    };
    let checked_at = schedule::BADGE_SECTIONS
        .iter()
        .filter_map(|s| last.section_ts.get(*s))
        .copied()
        .max()
        .unwrap_or(cfg.last_run);
    let checked = (checked_at > 0).then(|| format!("Checked {}", relative_time(checked_at, now)));

    let mut sources = Vec::new();
    for (id, label) in TRAY_SOURCES {
        let n = cfg.last_counts.get(*id).copied().unwrap_or(0);
        if n == 0 {
            continue;
        }
        let lines = last.sections.get(*id).map(|v| v.as_slice()).unwrap_or(&[]);
        let mut items = schedule::parse_items(id, lines);
        schedule::apply_ignores(id, &mut items, &cfg.ignored);
        let mut items: Vec<TrayItem> = items
            .iter()
            .filter(|i| !i.ignored)
            .map(|i| TrayItem { id: i.id.clone(), name: i.name.clone(), text: tray_item_text(i) })
            .collect();
        let more = items.len().saturating_sub(TRAY_MAX_NAMES);
        items.truncate(TRAY_MAX_NAMES);
        sources.push(TraySource { id: id.to_string(), label: format!("{label} ({n})"), items, more });
    }

    let install_label = match total {
        0 => "Install all".to_string(),
        1 => "Install 1 update".to_string(),
        n => format!("Install all {n} updates"),
    };
    TrayPlan { header, checked, sources, install_label, install_enabled: total > 0 && !busy, busy }
}

/// "Zoom  6.1.0 → 6.2.1", or less when a source reports no versions. Homebrew
/// writes a build after a comma ("4.86.0,236216", sometimes a forty-character
/// hash); a glance at the menu does not need it, so it is dropped unless it is
/// the only thing that changed.
fn tray_item_text(item: &schedule::CheckItem) -> String {
    let from = item.installed.as_deref().unwrap_or("").trim();
    let to = item.available.as_deref().unwrap_or("").trim();
    let short = |v: &str| v.split(',').next().unwrap_or(v).trim().to_string();
    let (from_s, to_s) = (short(from), short(to));
    let (from, to) = if from_s == to_s && from != to { (from, to) } else { (from_s.as_str(), to_s.as_str()) };
    match (from, to) {
        (_, "") => item.name.clone(),
        ("", to) => format!("{}  {to}", item.name),
        (from, to) if from == to => item.name.clone(),
        (from, to) => format!("{}  {from} → {to}", item.name),
    }
}

fn relative_time(then: u64, now: u64) -> String {
    let ago = now.saturating_sub(then);
    match ago {
        0..=59 => "just now".to_string(),
        60..=119 => "a minute ago".to_string(),
        120..=3599 => format!("{} minutes ago", ago / 60),
        3600..=7199 => "an hour ago".to_string(),
        7200..=86399 => format!("{} hours ago", ago / 3600),
        86400..=172799 => "yesterday".to_string(),
        _ => format!("{} days ago", ago / 86400),
    }
}

fn refresh_tray_menu(app: &AppHandle) {
    let Some(tray) = app.try_state::<Tray>() else { return };
    let checks = TRAY_ACTIVITY.checks.load(std::sync::atomic::Ordering::SeqCst);
    let installing = TRAY_ACTIVITY.installing.lock().ok().and_then(|g| g.clone());
    let plan = tray_plan(&schedule::load(app), &schedule::load_last_check(app), checks, installing.as_deref(), now_secs());
    if TRAY_PLAN.lock().ok().is_some_and(|g| g.as_ref() == Some(&plan)) {
        return;
    }
    match build_tray_menu(app, &plan) {
        Ok(menu) => {
            if let Err(e) = tray.0.set_menu(Some(menu)) {
                diag(app, &format!("menu bar: could not replace the menu: {e}"));
                return;
            }
            if let Ok(mut g) = TRAY_PLAN.lock() {
                *g = Some(plan);
            }
        }
        Err(e) => diag(app, &format!("menu bar: could not build the menu: {e}")),
    }
}

fn build_tray_menu(app: &AppHandle, plan: &TrayPlan) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
    type Item = Box<dyn IsMenuItem<tauri::Wry>>;
    let text = |id: String, label: &str, enabled: bool| -> tauri::Result<Item> {
        Ok(Box::new(MenuItem::with_id(app, id, label, enabled, None::<&str>)?))
    };

    let mut items: Vec<Item> = vec![text("header".into(), &plan.header, false)?];
    if let Some(checked) = &plan.checked {
        items.push(text("checked".into(), checked, false)?);
    }
    if !plan.sources.is_empty() {
        items.push(Box::new(PredefinedMenuItem::separator(app)?));
    }
    for src in &plan.sources {
        if src.items.is_empty() {
            items.push(text(format!("src:{}", src.id), &src.label, true)?);
            continue;
        }
        let mut sub: Vec<Item> = vec![text(format!("hint:{}", src.id), "Click an app to update it", false)?];
        for it in &src.items {
            // The app's own id travels in the menu id, so a menu rebuilt under an
            // open one cannot send the click to a neighbour.
            sub.push(text(format!("item:{}:{}", src.id, it.id), &it.text, true)?);
        }
        if src.more > 0 {
            sub.push(text(format!("more:{}", src.id), &format!("and {} more…", src.more), true)?);
        }
        let refs: Vec<&dyn IsMenuItem<tauri::Wry>> = sub.iter().map(|b| b.as_ref()).collect();
        items.push(Box::new(Submenu::with_id_and_items(app, format!("src:{}", src.id), &src.label, true, &refs)?));
    }
    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(text("install".into(), &plan.install_label, plan.install_enabled)?);
    items.push(text("check".into(), "Check now", !plan.busy)?);
    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(text("open".into(), "Open PartyMAN", true)?);
    items.push(text("quit".into(), "Quit PartyMAN", true)?);

    let refs: Vec<&dyn IsMenuItem<tauri::Wry>> = items.iter().map(|b| b.as_ref()).collect();
    Menu::with_items(app, &refs)
}

/// Brings the window up on one source — what a name in the dropdown does.
fn open_section(app: &AppHandle, section: &str) {
    show_main_window(app);
    let _ = app.emit("open-section", section.to_string());
}

/// Apps and system updates, then everything that follows a check: the badge, the
/// reminder, the window. Check now in the menu bar and the check at launch.
async fn check_apps_and_report(app: AppHandle) {
    let cfg = schedule::run_checks(&app, schedule::CheckScope::AppsOnly).await;
    set_tray_count(&app, cfg.last_total);
    schedule::notify_result(&app, &cfg);
    let _ = app.emit("schedule-updated", cfg);
}

/// "item:<section>:<id>" as the dropdown labels an app, split back up.
fn tray_item_id(menu_id: &str) -> Option<(&str, &str)> {
    let rest = menu_id.strip_prefix("item:")?;
    let (section, item) = rest.split_once(':')?;
    (!section.is_empty() && !item.is_empty()).then_some((section, item))
}

/// Per source, the apps to update by id and name — or none, meaning the whole
/// source.
type TrayInstallPlan = Vec<(String, Vec<(String, String)>)>;

/// The dropdown's Install all: everything the count reports, the way Install all
/// in the window does it, but with no window needed.
async fn install_outstanding_from_tray(app: AppHandle) {
    if tray_is_installing() {
        return;
    }
    let cfg = schedule::load(&app);
    let last = schedule::load_last_check(&app);
    let mut plan: TrayInstallPlan = Vec::new();
    for (section, _) in TRAY_SOURCES {
        if cfg.last_counts.get(*section).copied().unwrap_or(0) == 0 {
            continue;
        }
        let lines = last.sections.get(*section).cloned().unwrap_or_default();
        let items = schedule::items_for(&app, section, &lines)
            .into_iter()
            .filter(|i| !i.ignored)
            .map(|i| (i.id, i.name))
            .collect();
        plan.push((section.to_string(), items));
    }
    if plan.is_empty() {
        return;
    }
    let what = match cfg.last_total {
        1 => "1 update".to_string(),
        n => format!("{n} updates"),
    };
    diag(&app, &format!("menu bar: Install all ({what})"));
    run_tray_install(app, what, plan).await;
}

/// One name clicked in the dropdown: update just that app.
async fn install_one_from_tray(app: AppHandle, section: String, item_id: String) {
    if tray_is_installing() {
        diag(&app, &format!("menu bar: {item_id} clicked while an install is running; ignored"));
        return;
    }
    // The name as the menu showed it; the id alone if the menu has moved on.
    let name = TRAY_PLAN
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()?
                .sources
                .iter()
                .find(|s| s.id == section)?
                .items
                .iter()
                .find(|i| i.id == item_id)
                .map(|i| i.name.clone())
        })
        .unwrap_or_else(|| item_id.clone());
    diag(&app, &format!("menu bar: update {name} ({section}: {item_id})"));
    run_tray_install(app, name.clone(), vec![(section, vec![(item_id, name)])]).await;
}

/// Works through a plan source by source, in TRAY_SOURCES order: Homebrew first
/// (that is what the password is for), the App Store next (it only opens the App
/// Store), macOS last because it can restart the Mac. Homebrew asks for the
/// password through the usual dialog and macOS through its own. It ends with a
/// notification saying what happened, since nothing else need be on screen to
/// say it.
async fn run_tray_install(app: AppHandle, what: String, plan: TrayInstallPlan) {
    tray_installing(&app, Some(what));

    let mut installed: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut cancelled: Vec<String> = Vec::new();
    let mut app_store = 0usize;
    for (section, items) in plan {
        // The window follows along: whoever clicked in the menu bar gets the
        // log of what they started, not a notification two minutes later.
        open_section(&app, &section);
        let outcomes = if items.is_empty() {
            upgrade_section(&app, &section)
                .await
                .map(|o| vec![(section_label(&section).to_string(), o)])
                .unwrap_or_default()
        } else {
            let (ids, names): (Vec<String>, Vec<String>) = items.into_iter().unzip();
            upgrade_items(&app, &section, &ids, &names).await
        };
        if section == "app_store" {
            // Only opened the App Store; the installs happen there.
            app_store += outcomes.len().max(1);
        } else {
            for (name, outcome) in outcomes {
                match outcome.as_str() {
                    "ok" => installed.push(name),
                    "cancelled" => cancelled.push(name),
                    _ => failed.push(name),
                }
            }
        }
        settle_after_upgrade(&app, &section).await;
    }

    tray_installing(&app, None);
    match install_summary(&installed, &failed, &cancelled, app_store) {
        Some(body) => {
            diag(&app, &format!("menu bar: install finished — {body}"));
            use tauri_plugin_notification::NotificationExt;
            let _ = app.notification().builder().title("PartyMAN Update Manager").body(body).show();
        }
        // Dismissing the password prompt was the user's own doing; no need to
        // tell them about it.
        None => diag(&app, &format!("menu bar: install finished — cancelled: {}", cancelled.join(", "))),
    }
}

/// A sentence or two on how an install from the menu bar went, for the
/// notification. None when the only thing that happened was the user
/// dismissing the password prompt.
fn install_summary(installed: &[String], failed: &[String], cancelled: &[String], app_store: usize) -> Option<String> {
    fn list(names: &[String], what: &str) -> String {
        match names {
            [] => String::new(),
            [a] => a.clone(),
            [a, b] => format!("{a} and {b}"),
            [a, b, c] => format!("{a}, {b} and {c}"),
            _ => format!("{} {what}", names.len()),
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !installed.is_empty() {
        parts.push(format!("Updated {}.", list(installed, "apps")));
    }
    if !failed.is_empty() {
        parts.push(format!("{} failed.", list(failed, "updates")));
    }
    match app_store {
        0 => {}
        1 => parts.push("1 App Store update is waiting in the App Store.".to_string()),
        n => parts.push(format!("{n} App Store updates are waiting in the App Store.")),
    }
    if parts.is_empty() {
        if !cancelled.is_empty() {
            return None;
        }
        return Some("Nothing was installed. Open PartyMAN for details.".to_string());
    }
    if !cancelled.is_empty() {
        parts.push(format!("{} cancelled.", list(cancelled, "updates")));
    }
    if !failed.is_empty() {
        parts.push("Open PartyMAN for details.".to_string());
    }
    Some(parts.join(" "))
}


// Redraws the icon with the count baked into its badge, and the dropdown with
// the count's details. Cheap enough to do on every change: the icon is a 176x176
// compose and downscale, the menu a dozen items.
fn set_tray_count(app: &AppHandle, total: usize) {
    if let Some(tray) = app.try_state::<Tray>() {
        let _ = tray.0.set_icon(Some(tray_icon(total)));
        let tip = match total {
            0 => "PartyMAN — up to date".to_string(),
            1 => "PartyMAN — 1 update available".to_string(),
            n => format!("PartyMAN — {n} updates available"),
        };
        let _ = tray.0.set_tooltip(Some(tip));
    }
    refresh_tray_menu(app);
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn get_schedule(app: AppHandle) -> schedule::ScheduleConfig {
    schedule::load(&app)
}

// The window only owns the user's preferences; the run history belongs to
// whichever run last completed, so it is preserved rather than round-tripped.
#[tauri::command]
fn set_schedule(app: AppHandle, config: schedule::ScheduleConfig) -> Result<schedule::ScheduleConfig, String> {
    let previous = schedule::load(&app);
    let cfg = schedule::ScheduleConfig {
        last_run: previous.last_run,
        last_total: previous.last_total,
        last_counts: previous.last_counts,
        snoozed_until: previous.snoozed_until,
        ..config
    };
    let mut cfg = cfg;
    cfg.last_total = schedule::total_from_counts(&cfg.last_counts);
    schedule::save(&app, &cfg)?;
    schedule::sync_agent(&cfg)?;
    set_tray_count(&app, cfg.last_total);
    Ok(cfg)
}

// Postpones the reminder without touching the count, so the menu bar stays honest
// about what is outstanding.
#[tauri::command]
fn snooze_updates(app: AppHandle, hours: u64) -> Result<schedule::ScheduleConfig, String> {
    let cfg = schedule::snooze(&app, hours)?;
    let _ = app.emit("schedule-updated", cfg.clone());
    Ok(cfg)
}

#[tauri::command]
async fn run_schedule_now(app: AppHandle) -> schedule::ScheduleConfig {
    let cfg = schedule::run_checks(&app, schedule::CheckScope::AppsOnly).await;
    set_tray_count(&app, cfg.last_total);
    schedule::notify_result(&app, &cfg);
    let _ = app.emit("schedule-updated", cfg.clone());
    cfg
}

pub fn run() {
    let headless = std::env::args().any(|a| a == schedule::SCHEDULED_RUN_FLAG);

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(move |app| {
            let handle = app.handle().clone();
            diag(&handle, &format!(
                "launch v{}{}",
                app.package_info().version,
                if headless { " (scheduled run, no window)" } else { "" },
            ));

            // Launched by launchd: no window, no Dock icon — check, report, quit.
            if headless {
                #[cfg(target_os = "macos")]
                let _ = handle.set_activation_policy(tauri::ActivationPolicy::Accessory);

                tauri::async_runtime::spawn(async move {
                    // The open app holds this lock, and it does its own checking on
                    // a timer, so there is nothing for this run to do.
                    let _lock = match schedule::try_acquire_run_lock(&handle) {
                        Some(lock) => lock,
                        None => {
                            diag(&handle, "scheduled run: the app is open and checks on its own timer; nothing to do");
                            handle.exit(0);
                            return;
                        }
                    };
                    let cfg = schedule::run_scheduled(&handle).await;
                    schedule::notify_result(&handle, &cfg);
                    // Delivery is handed to the system asynchronously, so quitting
                    // the instant after posting can lose the notification.
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                    handle.exit(0);
                });
                return Ok(());
            }

            if let Some(window) = app.get_webview_window("main") {
                if let Ok(icon) = app_icon() {
                    let _ = window.set_icon(icon);
                }
                let _ = window.show();
            }

            // Closing the window leaves the app in the menu bar so the schedule
            // keeps running; Quit in the tray menu is what actually exits.
            if let Some(window) = app.get_webview_window("main") {
                let hide_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(w) = hide_handle.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                });
            }

            // The menu itself comes from set_tray_count below, and is rebuilt
            // whenever the count changes.
            let tray = tauri::tray::TrayIconBuilder::new()
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    let id = event.id.as_ref();
                    match id {
                        "open" => show_main_window(app),
                        "check" => {
                            diag(app, "menu bar: Check now");
                            tauri::async_runtime::spawn(check_apps_and_report(app.clone()));
                        }
                        "install" => {
                            diag(app, "menu bar: Install all");
                            tauri::async_runtime::spawn(install_outstanding_from_tray(app.clone()));
                        }
                        "quit" => {
                            diag(app, "menu bar: Quit");
                            app.exit(0)
                        }
                        // "item:<section>:<id>" updates that one app; "src:<section>"
                        // and "more:<section>" bring the window up on the source.
                        _ => {
                            if let Some((section, item_id)) = tray_item_id(id) {
                                tauri::async_runtime::spawn(install_one_from_tray(
                                    app.clone(), section.to_string(), item_id.to_string(),
                                ));
                            } else if let Some(section) = id.split(':').nth(1) {
                                diag(app, &format!("menu bar: open {section}"));
                                open_section(app, section);
                            }
                        }
                    }
                })
                // The pointer arriving on the icon is the moment before the menu
                // opens: the right time to bring "Checked … ago" up to date.
                .on_tray_icon_event(|tray, event| {
                    if matches!(event, tauri::tray::TrayIconEvent::Enter { .. }) {
                        refresh_tray_menu(tray.app_handle());
                    }
                })
                // Not a template: a template icon would discard the logo's colour and
                // the badge along with it.
                .icon(tray_icon(0))
                .icon_as_template(false)
                .build(app)?;
            app.manage(Tray(tray));

            // Whatever the last run found, so the count is right straight away.
            // Recomputed rather than trusted: a total stored before the counting
            // rule changed would otherwise sit in the menu bar until the next run.
            let mut startup_cfg = schedule::load(&handle);
            startup_cfg.last_total = schedule::total_from_counts(&startup_cfg.last_counts);
            let _ = schedule::save(&handle, &startup_cfg);
            set_tray_count(&handle, startup_cfg.last_total);

            if let Err(e) = schedule::ensure_agent_current(&startup_cfg) {
                eprintln!("could not refresh the background schedule agent: {e}");
            }

            if let Some(lock) = schedule::try_acquire_run_lock(&handle) {
                app.manage(RunLock(lock));
            }

            if startup_cfg.check_on_launch {
                let launch_handle = handle.clone();
                diag(&handle, "check on launch");
                tauri::async_runtime::spawn(check_apps_and_report(launch_handle));
            }

            // While the app is open it does the scheduled runs itself.
            let timer_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    refresh_tray_menu(&timer_handle);
                    let cfg = schedule::load(&timer_handle);
                    if !cfg.enabled {
                        continue;
                    }
                    if !schedule::is_due(&cfg) {
                        continue;
                    }
                    let cfg = schedule::run_scheduled(&timer_handle).await;
                    set_tray_count(&timer_handle, cfg.last_total);
                    schedule::notify_result(&timer_handle, &cfg);
                    let _ = timer_handle.emit("schedule-updated", cfg);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            run_check, run_upgrade, run_upgrade_items, get_platform,
            get_upgrade_history, search_cask, track_app, track_apps,
            check_app_update, open_release_url, get_release_notes, resolve_cask_input,
            get_schedule, set_schedule, run_schedule_now, snooze_updates, get_last_check, next_run,
            open_logs_folder, ignore_item, unignore_item, app_management_status, open_app_management_settings,
            tooling_status, setup_homebrew
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tray_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn item(id: &str, name: &str, from: &str, to: &str) -> String {
        format!("{}{id}\t{name}\t{from}\t{to}\t", schedule::ITEM_LINE)
    }

    fn state(casks: &[String], mas: usize, now: u64) -> (schedule::ScheduleConfig, schedule::LastCheck) {
        let mut cfg = schedule::ScheduleConfig::default();
        cfg.last_run = now - 600;
        cfg.last_counts = BTreeMap::from([
            ("brew_casks".to_string(), casks.len()),
            ("app_store".to_string(), mas),
            ("macos_updates".to_string(), 0),
            ("brew_formulae".to_string(), 84),
        ]);
        let mut last = schedule::LastCheck::default();
        last.sections.insert("brew_casks".to_string(), casks.to_vec());
        last.section_ts.insert("brew_casks".to_string(), now - 600);
        last.section_ts.insert("brew_formulae".to_string(), now - 5);
        (cfg, last)
    }

    #[test]
    fn the_dropdown_lists_outstanding_sources_by_name_and_leaves_the_rest_out() {
        let now = 1_700_000_000;
        let casks = vec![item("zoom", "Zoom", "6.1.0", "6.2.1"), item("slack", "Slack", "", "4.45")];
        let (cfg, last) = state(&casks, 1, now);
        let plan = tray_plan(&cfg, &last, 0, None, now);
        assert_eq!(plan.header, "3 updates available");
        // Developer tooling was checked seconds ago, but it is not part of the
        // count, so the time shown is the apps' check.
        assert_eq!(plan.checked.as_deref(), Some("Checked 10 minutes ago"));
        assert_eq!(plan.sources.len(), 2, "{:?}", plan.sources);
        assert_eq!(plan.sources[0].label, "Homebrew apps (2)");
        let texts: Vec<&str> = plan.sources[0].items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["Zoom  6.1.0 → 6.2.1", "Slack  4.45"]);
        assert_eq!(plan.sources[0].items[0].id, "zoom");
        assert_eq!(plan.sources[1].label, "App Store (1)");
        assert!(plan.sources[1].items.is_empty(), "no stored output, so no names");
        assert_eq!(plan.install_label, "Install all 3 updates");
        assert!(plan.install_enabled && !plan.busy);
    }

    #[test]
    fn homebrew_build_suffixes_are_dropped_from_the_menu_unless_they_are_the_change() {
        let it = |from: &str, to: &str| schedule::CheckItem {
            id: "x".into(), name: "Docker".into(), app_dir: None,
            installed: Some(from.into()), available: Some(to.into()), note: None, ignored: false,
        };
        assert_eq!(tray_item_text(&it("4.86.0,236216", "4.93.0,240920")), "Docker  4.86.0 → 4.93.0");
        assert_eq!(tray_item_text(&it("2.9939.2,d3e50475", "2.16120.0,801c07c2")), "Docker  2.9939.2 → 2.16120.0");
        assert_eq!(tray_item_text(&it("4.86.0,236216", "4.86.0,240920")), "Docker  4.86.0,236216 → 4.86.0,240920");
        assert_eq!(tray_item_text(&it("", "15.6")), "Docker  15.6");
        assert_eq!(tray_item_text(&it("", "")), "Docker");
    }

    #[test]
    fn ignored_items_are_neither_counted_nor_listed() {
        let now = 1_700_000_000;
        let casks = vec![item("zoom", "Zoom", "6.1.0", "6.2.1")];
        let (mut cfg, last) = state(&casks, 0, now);
        cfg.ignored.push(schedule::IgnoredItem {
            section: "brew_casks".into(), id: "zoom".into(), name: "Zoom".into(), available: "6.2.1".into(), since: now,
        });
        cfg.last_counts.insert("brew_casks".to_string(), 0);
        let plan = tray_plan(&cfg, &last, 0, None, now);
        assert_eq!(plan.header, "Everything is up to date");
        assert!(plan.sources.is_empty());
        assert_eq!(plan.install_label, "Install all");
        assert!(!plan.install_enabled);
    }

    #[test]
    fn the_header_says_what_is_going_on() {
        let now = 1_700_000_000;
        let (cfg, last) = state(&[item("zoom", "Zoom", "1", "2")], 0, now);
        assert_eq!(tray_plan(&cfg, &last, 1, None, now).header, "Checking for updates…");
        let busy = tray_plan(&cfg, &last, 0, Some("5 updates"), now);
        assert_eq!(busy.header, "Installing 5 updates…");
        assert!(busy.busy && !busy.install_enabled);
        assert_eq!(tray_plan(&cfg, &last, 0, Some("Brave"), now).header, "Installing Brave…");
        let mut fresh = schedule::ScheduleConfig::default();
        fresh.last_run = 0;
        let plan = tray_plan(&fresh, &schedule::LastCheck::default(), 0, None, now);
        assert_eq!(plan.header, "Not checked yet");
        assert_eq!(plan.checked, None);
    }

    #[test]
    fn long_lists_are_cut_with_a_count_of_the_rest() {
        let now = 1_700_000_000;
        let casks: Vec<String> = (0..20).map(|i| item(&format!("app{i}"), &format!("App {i}"), "1", "2")).collect();
        let (cfg, last) = state(&casks, 0, now);
        let plan = tray_plan(&cfg, &last, 0, None, now);
        assert_eq!(plan.sources[0].items.len(), TRAY_MAX_NAMES);
        assert_eq!(plan.sources[0].more, 20 - TRAY_MAX_NAMES);
    }

    #[test]
    fn a_menu_id_names_the_app_to_update() {
        assert_eq!(tray_item_id("item:brew_casks:visual-studio-code"), Some(("brew_casks", "visual-studio-code")));
        assert_eq!(tray_item_id("item:macos_updates:macOS Sequoia 15.6.1-24G90"), Some(("macos_updates", "macOS Sequoia 15.6.1-24G90")));
        assert_eq!(tray_item_id("src:brew_casks"), None);
        assert_eq!(tray_item_id("item:brew_casks:"), None);
        assert_eq!(tray_item_id("hint:brew_casks"), None);
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = 1_000_000;
        assert_eq!(relative_time(now - 10, now), "just now");
        assert_eq!(relative_time(now - 90, now), "a minute ago");
        assert_eq!(relative_time(now - 25 * 60, now), "25 minutes ago");
        assert_eq!(relative_time(now - 3700, now), "an hour ago");
        assert_eq!(relative_time(now - 5 * 3600, now), "5 hours ago");
        assert_eq!(relative_time(now - 30 * 3600, now), "yesterday");
        assert_eq!(relative_time(now - 3 * 86400, now), "3 days ago");
    }

    #[test]
    fn the_summary_names_what_happened() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(install_summary(&s(&["Zoom"]), &[], &[], 0).as_deref(), Some("Updated Zoom."));
        assert_eq!(install_summary(&s(&["Zoom", "Slack", "Firefox"]), &[], &[], 0).as_deref(), Some("Updated Zoom, Slack and Firefox."));
        assert_eq!(install_summary(&s(&["A", "B", "C", "D"]), &[], &[], 0).as_deref(), Some("Updated 4 apps."));
        assert_eq!(
            install_summary(&s(&["Zoom"]), &s(&["Slack"]), &[], 2).as_deref(),
            Some("Updated Zoom. Slack failed. 2 App Store updates are waiting in the App Store. Open PartyMAN for details.")
        );
        assert_eq!(install_summary(&[], &[], &[], 0).as_deref(), Some("Nothing was installed. Open PartyMAN for details."));
        // The user dismissed the prompt themselves: nothing to tell them.
        assert_eq!(install_summary(&[], &[], &s(&["Postman"]), 0), None);
        assert_eq!(install_summary(&s(&["Zoom"]), &[], &s(&["Postman"]), 0).as_deref(), Some("Updated Zoom. Postman cancelled."));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn ctty_of(pid: String) -> String {
        let out = std::process::Command::new("ps")
            .args(["-o", "tty=", "-p", &pid])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn field(text: &str, key: &str) -> String {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| panic!("missing {key} in:\n{text}"))
            .trim()
            .to_string()
    }

    // Adoption must never install something merely similarly named. brew search
    // returns a hit for almost any string, so an app with no cask at all still
    // produces candidates; only an app-artifact match makes one safe to adopt.
    #[tokio::test]
    #[ignore = "queries Homebrew; needs brew and a network"]
    async fn only_real_matches_are_marked_exact() {
        // Apps with no cask whatsoever — brew search still returns noise for them.
        for app in ["FileZilla", "Google Docs", "Google Sheets"] {
            let found = search_cask(app.to_string()).await;
            let exact: Vec<_> = found.iter().filter(|c| c.exact).map(|c| &c.token).collect();
            assert!(
                exact.is_empty(),
                "{app}: nothing installs it, yet these were treated as matches: {exact:?} \
                 (all candidates: {:?})",
                found.iter().map(|c| &c.token).collect::<Vec<_>>()
            );
        }

        // Apps that genuinely have a cask must still be adoptable.
        // zoom.us.app <-> the "zoom" cask share no spelling; only the bundle
        // identifier connects them.
        for (app, want) in [("Parsec", "parsec"), ("Rectangle", "rectangle"), ("zoom.us", "zoom")] {
            let found = search_cask(app.to_string()).await;
            let exact: Vec<_> = found.iter().filter(|c| c.exact).map(|c| c.token.as_str()).collect();
            assert!(
                exact.contains(&want),
                "{app}: expected {want} to be recognised, got {exact:?}"
            );
        }
    }

    // Looking a cask up in a browser and copying the address is the natural way
    // to find one, so the address must be accepted as readily as a bare token.
    #[tokio::test]
    #[ignore = "queries Homebrew; needs brew and a network"]
    async fn accepts_a_pasted_formulae_url_or_a_bare_token() {
        let from_url = resolve_cask_input(
            "https://formulae.brew.sh/cask/dbeaver-enterprise#default".to_string(),
        ).await;
        assert_eq!(from_url.as_ref().map(|c| c.token.as_str()), Some("dbeaver-enterprise"));

        let bare = resolve_cask_input("dbeaver-enterprise".to_string()).await;
        assert_eq!(bare.as_ref().map(|c| c.token.as_str()), Some("dbeaver-enterprise"));

        // Whitespace and a trailing slash are what a real copy-paste looks like.
        let messy = resolve_cask_input("  https://formulae.brew.sh/cask/parsec/  ".to_string()).await;
        assert_eq!(messy.as_ref().map(|c| c.token.as_str()), Some("parsec"));

        // A cask that does not exist must be refused rather than adopted blindly.
        assert!(resolve_cask_input("definitely-not-a-real-cask-xyz".to_string()).await.is_none());
        assert!(resolve_cask_input("../../etc/passwd".to_string()).await.is_none());
    }

    // Writes the menu-bar glyph out so it can be looked at; a mask that reads as a
    // smudge at 22pt is the whole problem being solved here.
    #[test]
    #[ignore = "writes a preview image for eyeballing"]
    fn dump_tray_icon() {
        for count in [0usize, 3, 20, 148] {
            let icon = tray_icon(count);
            let img = image::RgbaImage::from_raw(icon.width(), icon.height(), icon.rgba().to_vec())
                .expect("icon buffer");
            let big = image::imageops::resize(&img, 176, 176, image::imageops::FilterType::Nearest);
            big.save(format!("/private/tmp/claude-503/-Users-paymon-src-partyman-update-manager/a94671f6-9cb0-4eb7-ab04-937d132c5b29/scratchpad/tray-{count}.png"))
                .expect("save");
        }
        let icon = tray_icon(20);
        let img = image::RgbaImage::from_raw(icon.width(), icon.height(), icon.rgba().to_vec())
            .expect("icon buffer");
        // scale up so the shape is legible in a preview
        let big = image::imageops::resize(&img, 176, 176, image::imageops::FilterType::Nearest);
        big.save("/private/tmp/claude-503/-Users-paymon-src-partyman-update-manager/a94671f6-9cb0-4eb7-ab04-937d132c5b29/scratchpad/tray.png")
            .expect("save");
    }

    // The batch shell needs a controlling terminal so that a single sudo
    // authentication covers every sudo call in the run, while stdout/stderr must
    // stay pipes so Homebrew's output keeps the plain form the line parser and
    // cask sentinel splitting expect.
    // A process the batch leaves behind — here one that, like a daemon, shrugs
    // off the hangup sent when the shell exits — holds the output pipe open for
    // far longer than the run. The run must still end when the shell does.
    #[tokio::test]
    async fn run_ends_when_the_shell_exits_not_when_its_pipes_close() {
        let script = r#"
echo before
( trap '' HUP; exec sleep 60 ) &
echo "HOLDER=$!"
echo after >&2
"#;
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let _pty = attach_controlling_pty(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");

        let started = std::time::Instant::now();
        let lines = stream_child(&mut child, |_| {}).await.lines;
        let took = started.elapsed();

        let holder = lines.iter().find_map(|l| l.strip_prefix("HOLDER=")).map(str::to_string);
        if let Some(pid) = &holder {
            let _ = std::process::Command::new("kill").arg(pid).status();
        }
        assert!(took < DRAIN_GRACE + Duration::from_secs(2), "waited on the holder: {took:?}");
        assert!(holder.is_some(), "holder never started: {lines:?}");
        for want in ["before", "after"] {
            assert!(lines.iter().any(|l| l == want), "lost {want:?}: {lines:?}");
        }
    }

    // The same, one level down: a cask that leaves a process holding brew's
    // output used to stall `brew … | tee` — and with it the rest of the batch —
    // though brew itself had finished.
    #[tokio::test]
    async fn brew_logged_returns_with_brew_despite_a_lingering_child() {
        let dir = std::env::temp_dir().join(format!("pm-brew-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fake_brew = dir.join("brew");
        fs::write(
            &fake_brew,
            "#!/bin/bash\necho \"brew $*\"\n( trap '' HUP; exec sleep 60 ) &\necho \"HOLDER=$!\"\necho 'Warning: to stderr' >&2\nexit 3\n",
        )
        .unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&fake_brew).status().unwrap();

        let script = format!(
            "export PATH=\"{dir}:$PATH\"\n{f}\nout=$(mktemp)\npm_brew_logged \"$out\" upgrade --cask demo\necho \"RC=$?\"\necho \"SAVED=$(grep -c . \"$out\")\"\nrm -f \"$out\"\n",
            dir = dir.display(),
            f = brew_logged_fn(),
        );
        // Run it the way the app does: a pty attached, streamed until the shell exits.
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let _pty = attach_controlling_pty(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let started = std::time::Instant::now();
        let lines = tokio::time::timeout(Duration::from_secs(30), stream_child(&mut child, |_| {})).await;
        let took = started.elapsed();

        // The holder is in the output whichever way this went; clean it up first.
        let text = lines.as_ref().map(|r| r.lines.join("\n")).unwrap_or_default();
        if let Some(pid) = text.lines().find_map(|l| l.strip_prefix("HOLDER=")) {
            let _ = std::process::Command::new("kill").arg(pid).status();
        }
        let _ = fs::remove_dir_all(&dir);

        assert!(lines.is_ok(), "the run waited on brew's child");
        assert!(took < Duration::from_secs(10), "took {took:?}:\n{text}");
        assert!(text.contains("brew upgrade --cask demo"), "output not streamed:\n{text}");
        assert!(text.contains("Warning: to stderr"), "stderr not streamed:\n{text}");
        assert_eq!(field(&text, "RC="), "3", "brew's exit status lost:\n{text}");
        assert_eq!(field(&text, "SAVED="), "3", "output not saved for the retry checks:\n{text}");
    }

    #[test]
    fn outcome_reads_the_scripts_own_markers_not_homebrews_first_try() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A first attempt that a retry rescues still ends with Done.
        assert_eq!(outcome_from_lines(&l(&["Error: firefox: Failure while executing", "→  Backup conflict — retrying with --force…", "→  Done."]), Some(0)), "ok");
        assert_eq!(outcome_from_lines(&l(&["✖  Update failed for zoom."]), Some(1)), "failed");
        assert_eq!(outcome_from_lines(&l(&["→  Done! Ghostty is now managed by Homebrew.", "✖  zoom.us couldn't be set up — no such cask."]), Some(0)), "partial");
        // No markers at all: the exit status decides.
        assert_eq!(outcome_from_lines(&l(&["brew: command not found"]), Some(127)), "failed");
        assert_eq!(outcome_from_lines(&l(&["✔  App Store opened"]), None), "ok");
        // A dismissed password prompt is its own outcome, for Homebrew and for macOS.
        assert_eq!(outcome_from_lines(&l(&["sudo: no password was provided", "↩  Cancelled: postman was left as it was."]), Some(1)), "cancelled");
        assert_eq!(outcome_from_lines(&l(&["execution error: User canceled. (-128)", "→  macOS update complete."]), Some(0)), "cancelled");
        // Homebrew 5 ticks off each download; that is not a result.
        assert_eq!(outcome_from_lines(&l(&["\u{2714}\u{FE0E} Cask brave-browser (1.96.60.0)", "✖  Update failed for brave-browser."]), Some(1)), "failed");
    }

    #[test]
    fn versions_come_from_either_of_homebrews_shapes() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let v = versions_from_lines(&l(&["==> Upgrading docker-desktop", "  4.86.0,236216 -> 4.93.0,240920", "🍺  docker-desktop was successfully upgraded!"]), Some("docker-desktop"));
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].item.as_str(), v[0].from.as_str(), v[0].to.as_str()), ("docker-desktop", "4.86.0,236216", "4.93.0,240920"));
        let v = versions_from_lines(&l(&["==> Upgraded 2 outdated packages:", "slack 4.51.191 -> 4.52.162", "postman 12.23.7 -> 12.30.0"]), None);
        assert_eq!(v.iter().map(|x| x.item.as_str()).collect::<Vec<_>>(), ["slack", "postman"]);
        // Prose with an arrow in it is not a version line.
        assert!(versions_from_lines(&l(&["→  Retrying zoom in ~/Applications…", "a -> b"]), None).is_empty());
    }

    #[test]
    fn a_batch_adoption_recorded_as_one_entry_is_read_as_one_per_app() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let e = HistoryEntry {
            ts: 100, section: "untracked_apps".into(), label: "Untracked Apps".into(),
            items: l(&["ghostty", "tailscale-app", "zoom"]),
            item_names: l(&["Ghostty", "Tailscale", "zoom.us"]),
            lines: l(&[
                "→  Done! Ghostty is now managed by Homebrew.",
                "==> Running installer for tailscale-app",
                "→  Done! Tailscale is now managed by Homebrew.",
                "✖  zoom.us couldn't be set up — no such cask.",
                "────", "✖  1 app(s) could not be set up:",
            ]),
            run_id: "100-untracked_apps".into(), kind: "adopt".into(),
            ..Default::default()
        };
        let split = split_legacy_adoption(e.clone());
        assert_eq!(split.len(), 3);
        assert_eq!(split[1].item_names, vec!["Tailscale"]);
        assert_eq!(split[1].lines.len(), 2);
        assert_eq!(split[2].outcome, "failed");
        assert!(split.iter().all(|s| s.run_id == e.run_id));
        // A run written with a proper run id is already split; leave it alone.
        let mut fresh = e.clone(); fresh.run_id = "100-123-4-untracked_apps".into();
        assert_eq!(split_legacy_adoption(fresh).len(), 1);
    }

    #[tokio::test]
    async fn attaches_controlling_pty_but_leaves_stdio_piped() {
        let script = r#"
[ -t 1 ] && echo "STDOUT_TTY=yes" || echo "STDOUT_TTY=no"
[ -t 2 ] && echo "STDERR_TTY=yes" || echo "STDERR_TTY=no"
echo "CTTY=$(ps -o tty= -p $$ | tr -d ' ')"
echo "DESCENDANT_CTTY=$(bash -c 'bash -c "ps -o tty= -p \$\$"' | tr -d ' ')"
"#;
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let pty = attach_controlling_pty(&mut cmd);
        assert!(pty.is_some(), "openpty failed");

        let out = cmd.output().await.expect("spawn");
        let text = String::from_utf8_lossy(&out.stdout).to_string();

        // Homebrew must not see a terminal: a tty turns on spinners, colour and
        // CR line endings, none of which the output parser tolerates.
        assert_eq!(field(&text, "STDOUT_TTY="), "no", "stdout leaked a tty:\n{text}");
        assert_eq!(field(&text, "STDERR_TTY="), "no", "stderr leaked a tty:\n{text}");
        assert!(!text.contains('\r'), "CR line endings appeared:\n{text}");

        // sudo keys its cached credential to this terminal, and every descendant
        // must share it for one authentication to cover the whole batch.
        let ctty = field(&text, "CTTY=");
        assert!(ctty != "??" && !ctty.is_empty(), "no controlling terminal: {ctty:?}");
        assert_eq!(
            ctty,
            field(&text, "DESCENDANT_CTTY="),
            "descendants must share the terminal:\n{text}"
        );

        // It must be the pty we allocated, not one inherited from our own process.
        let ours = ctty_of(std::process::id().to_string());
        assert_ne!(ctty, ours, "child reused the parent's terminal");
    }
}
