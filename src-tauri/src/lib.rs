mod schedule;

use std::fs;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use image::GenericImageView;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

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

#[derive(Clone, serde::Serialize)]
struct CaskCandidate {
    token: String,
    name: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct HistoryEntry {
    ts: u64,
    section: String,
    label: String,
    items: Vec<String>,
    item_names: Vec<String>,
    lines: Vec<String>,
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

// The cask helper bundle: the protected-bundle recovery followed by the
// single-cask upgrade function that depends on it.
fn cask_fns() -> String {
    format!("{}\n{}", protected_bundle_fn(), brew_cask_upgrade_fn())
}

fn brew_cask_upgrade_fn() -> &'static str {
    r#"brew_upgrade_cask() {
  local token="$1"
  local CURRENT_USER
  CURRENT_USER=$(whoami)
  local TMPOUT
  TMPOUT=$(mktemp)
  local APPDIR_FLAG=""

  brew upgrade --cask "$token" 2>&1 | tee "$TMPOUT"
  local BREW_EXIT="${PIPESTATUS[0]}"

  if grep -q "It seems there is already an App at" "$TMPOUT"; then
    echo "→  Backup conflict (app may have self-updated) — retrying with --force…"
    rm -f "$TMPOUT"; TMPOUT=$(mktemp)
    brew upgrade --cask --force $APPDIR_FLAG "$token" 2>&1 | tee "$TMPOUT"
    BREW_EXIT="${PIPESTATUS[0]}"
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
    brew install --cask --force $APPDIR_FLAG "$token" 2>&1 | tee "$TMPOUT"
    BREW_EXIT="${PIPESTATUS[0]}"
  fi

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

  if [ "$BREW_EXIT" -eq 0 ]; then
    echo "→  Done."
  else
    echo "✖  Update failed for $token."
  fi
}
"#
}

// Bash function that adopts one app into Homebrew management. $1 is "user" to
// install into ~/Applications (otherwise /Applications), $2 is the cask token.
// Depends on pm_fix_protected_bundle (protected_bundle_fn) being defined too.
fn adopt_cask_fn() -> &'static str {
    r#"adopt_cask() {
  local use_userdir="$1" token="$2"
  local flag=""
  [ "$use_userdir" = "user" ] && flag="--appdir $HOME/Applications"
  local TMPOUT; TMPOUT=$(mktemp)
  brew install --cask --force $flag "$token" 2>&1 | tee "$TMPOUT"
  local BREW_EXIT=${PIPESTATUS[0]}
  if grep -q "Permission denied @ apply2files" "$TMPOUT"; then
    if pm_fix_protected_bundle "$TMPOUT"; then
      rm -f "$TMPOUT"
      if brew install --cask --force $flag "$token" 2>&1; then
        echo "→  Done! $token is now managed by Homebrew."
      else
        echo "✖  Setup failed for $token."
      fi
    else
      rm -f "$TMPOUT"
    fi
  elif [ "$BREW_EXIT" -eq 0 ]; then
    rm -f "$TMPOUT"
    echo "→  Done! $token is now managed by Homebrew."
  elif grep -qE "Running installer for|/usr/sbin/installer|installer -pkg" "$TMPOUT"; then
    rm -f "$TMPOUT"
    echo "✖  $token couldn't be set up — its installer failed."
    echo "→  This usually means the app is still running, or it is managed by your organization (MDM)."
    echo "→  Quit the app completely (check the menu bar) and try again."
    echo "→  If it keeps failing, it is likely managed by IT and can't be adopted by Homebrew — you can leave it to update on its own."
  else
    rm -f "$TMPOUT"
    echo "✖  Setup failed for $token."
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
                        Some(CaskCandidate { token, name })
                    })
                    .collect();
                if !candidates.is_empty() {
                    return candidates;
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
            return text.lines()
                .filter(|l| !l.is_empty() && !l.starts_with("==>") && !l.contains("No formulae or casks"))
                .take(5)
                .map(|t| CaskCandidate { token: t.trim().to_string(), name: t.trim().to_string() })
                .collect();
        }
    }

    vec![]
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
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\n{helper}\n{adopt}\nadopt_cask '{ud}' '{token}'\necho '→  Run a check to see this app move to Homebrew Apps.'\nelse\n  echo '✖  brew not found'\nfi",
        helper = protected_bundle_fn(),
        adopt = adopt_cask_fn(),
        ud = use_userdir,
        token = cask_token,
    );
    let lines = run_upgrade_shell(&app, section, &script).await;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    append_upgrade_log(&app, HistoryEntry {
        ts,
        label: section_label(section).to_string(),
        section: section.to_string(),
        items: vec![cask_token.clone()],
        item_names: vec![cask_token],
        lines,
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
        calls.push_str(&format!("adopt_cask '{ud}' '{token}'\n", ud = ud, token = it.token));
        tokens.push(it.token.clone());
        names.push(it.name.clone());
    }
    if tokens.is_empty() {
        emit_upgrade_line(&app, section, "No apps to enable.").await;
        emit_upgrade_status(&app, section, "done").await;
        return;
    }
    let script = format!(
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\nexport PM_ASKPASS_APP='{scope}'\n{helper}\n{adopt}\n{calls}echo '→  All set! Run a check to move these apps to Homebrew Apps.'\nelse\n  echo '✖  brew not found'\nfi",
        scope = askpass_scope(&names),
        helper = protected_bundle_fn(),
        adopt = adopt_cask_fn(),
        calls = calls,
    );
    let lines = run_upgrade_shell(&app, section, &script).await;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    append_upgrade_log(&app, HistoryEntry {
        ts,
        label: section_label(section).to_string(),
        section: section.to_string(),
        items: tokens,
        item_names: names,
        lines,
    });
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

async fn run_shell(app: &AppHandle, section: &str, script: &str) {
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
            emit_line(app, section, &format!("Failed to spawn: {e}")).await;
            emit_status(app, section, "error").await;
            return;
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let app1 = app.clone();
    let sec1 = section.to_string();
    let t1 = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            emit_line(&app1, &sec1, &line).await;
        }
    });

    let app2 = app.clone();
    let sec2 = section.to_string();
    let t2 = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            emit_line(&app2, &sec2, &line).await;
        }
    });

    let _ = tokio::join!(t1, t2);
    let status = child.wait().await;
    let final_status = match status {
        Ok(s) if s.success() => "done",
        _ => "done",
    };
    emit_status(app, section, final_status).await;
}

// Sentinel lines emitted around each cask so the single-shell batch output can be
// split back into per-cask history entries. Filtered out of the live UI stream.
const CASK_MARKER_PREFIX: &str = "__PM_CASK_";
const CASK_START: &str = "__PM_CASK_START__:";
const CASK_END: &str = "__PM_CASK_END__:";

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
cat > "$_pm_askpass" <<'PARTYMAN_ASKPASS'
#!/bin/bash
_pm_stamp="${PM_ASK_STAMP:-}"
_pm_retry=""
if [ -n "$_pm_stamp" ] && [ -s "$_pm_stamp" ]; then
  _pm_last=$(cat "$_pm_stamp" 2>/dev/null)
  if [ "${_pm_last%% *}" = "$PPID" ] && [ $(( $(date +%s) - ${_pm_last##* } )) -lt 20 ]; then
    _pm_retry=1
  fi
fi
_pm_app="${PM_ASKPASS_APP:-}"
if [ -n "$_pm_app" ]; then
  _pm_msg="PartyMAN Update Manager needs your administrator password to update ${_pm_app}."
else
  _pm_msg="PartyMAN Update Manager needs your administrator password to complete this update."
fi
[ -n "$_pm_retry" ] && _pm_msg="That password didn't work. $_pm_msg"
_pm_pw=$(osascript -e "display dialog \"${_pm_msg}\" default answer \"\" with hidden answer with title \"PartyMAN Update Manager\" with icon caution giving up after 120" -e 'text returned of result' 2>/dev/null) || exit 1
[ -z "$_pm_pw" ] && exit 1
[ -n "$_pm_stamp" ] && printf '%s %s' "$PPID" "$(date +%s)" > "$_pm_stamp"
printf '%s' "$_pm_pw"
PARTYMAN_ASKPASS
chmod 700 "$_pm_askpass"
export SUDO_ASKPASS="$_pm_askpass"
export PM_ASK_STAMP="$_pm_dir/asked"
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
# Refresh sudo's cached credential every minute so it cannot lapse (default 5
# min) part-way through a long batch and ask a second time. Fully detached from
# our pipes so it never emits anything into the update log.
( while :; do sleep 60; sudo -n -v; done ) >/dev/null 2>&1 &
_pm_keepalive=$!
# Off the job table, or bash announces "Terminated" on stderr when the trap kills
# it and that lands in the update log.
disown "$_pm_keepalive" 2>/dev/null
trap '[ -n "$_pm_keepalive" ] && kill "$_pm_keepalive" 2>/dev/null; [ -n "$_pm_dir" ] && rm -rf "$_pm_dir"' EXIT
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
async fn run_upgrade_shell(app: &AppHandle, section: &str, script: &str) -> Vec<String> {
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
            emit_upgrade_line(app, section, &format!("Failed to spawn: {e}")).await;
            emit_upgrade_status(app, section, "error").await;
            return vec![];
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let collected = Arc::new(Mutex::new(Vec::<String>::new()));

    let app1 = app.clone();
    let sec1 = section.to_string();
    let coll1 = Arc::clone(&collected);
    let t1 = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if !line.starts_with(CASK_MARKER_PREFIX) {
                emit_upgrade_line(&app1, &sec1, &line).await;
            }
            if let Ok(mut v) = coll1.lock() { v.push(line); }
        }
    });

    let app2 = app.clone();
    let sec2 = section.to_string();
    let coll2 = Arc::clone(&collected);
    let t2 = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if !line.starts_with(CASK_MARKER_PREFIX) {
                emit_upgrade_line(&app2, &sec2, &line).await;
            }
            if let Ok(mut v) = coll2.lock() { v.push(line); }
        }
    });

    let _ = tokio::join!(t1, t2);
    let _ = child.wait().await;
    emit_upgrade_status(app, section, "done").await;

    Arc::try_unwrap(collected)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default()
}

fn check_script(section: &str) -> Option<&'static str> {
    match section {
        "macos_updates" => Some(r#"
if command -v softwareupdate &>/dev/null; then
  updates=$(softwareupdate -l 2>&1)
  if echo "$updates" | grep -q "No new software available"; then
    echo "✔  macOS is up to date."
  else
    echo "$updates" | grep -E "^\s*\*|\bLabel\b|Title:" || echo "$updates" | grep -v "^Software Update Tool" | grep -v "^$"
  fi
else
  echo "✖  softwareupdate not found — not running on macOS"
fi
"#),
        "brew_casks" => Some(r#"
if command -v brew &>/dev/null; then
  echo "→  Refreshing Homebrew…"
  brew update --quiet 2>/dev/null
  outdated=$(brew outdated --cask --greedy 2>/dev/null)
  if [ -z "$outdated" ]; then
    echo "✔  All Homebrew cask apps are up to date."
  else
    echo "⚠  Outdated apps:"
    echo "$outdated" | while read -r line; do echo "   $line"; done
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
    echo "$outdated" | while read -r line; do echo "   $line"; done
  fi
else
  echo "✖  mas not installed."
  echo "→  Install with: brew install mas"
fi
"#),
        "untracked_apps" => Some(r#"
cask_tokens=""
cask_apps=""
if command -v brew &>/dev/null; then
  cask_tokens=$(brew list --cask 2>/dev/null)
  if [ -n "$cask_tokens" ] && command -v jq &>/dev/null; then
    cask_apps=$(brew info --cask --json=v2 $cask_tokens 2>/dev/null \
      | jq -r '.casks[].artifacts[]?.app[]? | select(type=="string")' 2>/dev/null)
  fi
fi

is_tracked() {
  local app="$1"
  local base
  base=$(basename "$app")
  codesign -dvv "$app" 2>&1 | grep -q "Authority=Software Signing" && return 0
  [ -e "$app/Contents/_MASReceipt/receipt" ] && return 0
  [ -n "$cask_apps" ] && echo "$cask_apps" | grep -qxF "$base" && return 0
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
  outdated=$(brew outdated --verbose 2>/dev/null)
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
        Some(script) => run_shell(&app, &section, script).await,
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
  brew upgrade --cask --greedy 2>&1 | tee "$TMPOUT"
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
    match upgrade_script(&section) {
        Some(script) => {
            let lines = run_upgrade_shell(&app, &section, &script).await;
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            append_upgrade_log(&app, HistoryEntry {
                ts,
                label: section_label(&section).to_string(),
                section: section.clone(),
                items: vec![],
                item_names: vec![],
                lines,
            });
        }
        None => {
            emit_upgrade_line(&app, &section, &format!("No upgrade command for: {section}")).await;
            emit_upgrade_status(&app, &section, "error").await;
        }
    }
}

#[tauri::command]
async fn run_upgrade_items(app: AppHandle, section: String, items: Vec<String>, item_names: Vec<String>) {
    if items.is_empty() {
        emit_upgrade_line(&app, &section, "No items selected.").await;
        emit_upgrade_status(&app, &section, "done").await;
        return;
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
            emit_upgrade_line(&app, &section, "No items selected.").await;
            emit_upgrade_status(&app, &section, "done").await;
            return;
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
            "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\nexport PM_ASKPASS_APP='{scope}'\n{fn_def}\n{body}else\n  echo '✖  brew not found'\nfi",
            scope = scope,
            fn_def = cask_fns(),
            body = body,
        );
        let lines = run_upgrade_shell(&app, &section, &script).await;

        // Split the combined output into per-cask groups on the sentinels.
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        let mut cur: Option<(String, Vec<String>)> = None;
        for line in &lines {
            if let Some(token) = line.strip_prefix(CASK_START) {
                if let Some(g) = cur.take() { groups.push(g); }
                cur = Some((token.to_string(), Vec::new()));
            } else if line.strip_prefix(CASK_END).is_some() {
                if let Some(g) = cur.take() { groups.push(g); }
            } else if let Some((_, body_lines)) = cur.as_mut() {
                body_lines.push(line.clone());
            }
        }
        if let Some(g) = cur.take() { groups.push(g); }

        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        for (token, body_lines) in groups {
            let display_name = pairs.iter()
                .find(|(t, _)| *t == token)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| token.clone());
            append_upgrade_log(&app, HistoryEntry {
                ts,
                label: section_label(&section).to_string(),
                section: section.clone(),
                items: vec![token.clone()],
                item_names: vec![display_name],
                lines: body_lines,
            });
        }
        return;
    }

    let script: String = match section.as_str() {
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
            let names_src: Vec<String> = if item_names.is_empty() { items.clone() } else { item_names.clone() };
            let list = names_src.iter()
                .map(|n| n.replace(['\\', '"'], ""))
                .collect::<Vec<_>>().join(", ");
            let heads_up = macos_heads_up(&list);
            format!(
                "{heads_up}\n{MACOS_INSTALLING_NOTE}\nosascript -e 'do shell script \"softwareupdate -i {labels}\" with administrator privileges' 2>&1\necho '→  macOS update complete.'"
            )
        }
        _ => {
            emit_upgrade_line(&app, &section, "Individual upgrades not supported for this section.").await;
            emit_upgrade_status(&app, &section, "error").await;
            return;
        }
    };

    let lines = run_upgrade_shell(&app, &section, &script).await;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let display_names = if item_names.is_empty() { items.clone() } else { item_names };
    append_upgrade_log(&app, HistoryEntry {
        ts,
        label: section_label(&section).to_string(),
        section: section.clone(),
        items: items.clone(),
        item_names: display_names,
        lines,
    });
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
    use ab_glyph::{Font, FontRef, ScaleFont};

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

    if count == 0 {
        let scaled = image::imageops::resize(&canvas, SIZE, SIZE, image::imageops::FilterType::Lanczos3);
        return tauri::image::Image::new_owned(scaled.into_raw(), SIZE, SIZE);
    }

    // Three digits is the most that stays legible at 22pt; beyond that it is a
    // "lots" indicator rather than a number anyone reads.
    let label = if count > 99 { "99+".to_string() } else { count.to_string() };

    // Badge geometry in 44x44 space. The circle grows only slightly with longer
    // labels — past about half the icon's width it stops reading as a badge and
    // starts hiding the logo — so the digits shrink to fit instead.
    let (r, text_scale) = match label.chars().count() {
        1 => (8.5_f32, 1.30_f32),
        2 => (10.0, 1.05),
        _ => (11.0, 0.78),
    };
    let cx = 44.0 - r - 1.0;
    let cy = r + 1.0;

    let (bx, by, br) = (cx * SS as f32, cy * SS as f32, r * SS as f32);
    for y in 0..big {
        for x in 0..big {
            let dx = x as f32 + 0.5 - bx;
            let dy = y as f32 + 0.5 - by;
            let d = (dx * dx + dy * dy).sqrt();
            if d <= br {
                // A ring of the menu-bar background lifts the badge off the logo.
                let px = if d > br - 1.6 * SS as f32 {
                    image::Rgba([255, 255, 255, 255])
                } else {
                    image::Rgba([228, 48, 48, 255])
                };
                canvas.put_pixel(x, y, px);
            }
        }
    }

    if let Ok(font) = FontRef::try_from_slice(include_bytes!("../assets/Nunito.ttf")) {
        let scaled = font.as_scaled(r * text_scale * SS as f32);

        let glyphs: Vec<_> = label
            .chars()
            .map(|c| scaled.scaled_glyph(c))
            .collect();
        let width: f32 = label.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum();

        let mut pen_x = bx - width / 2.0;
        // Centre on the digits' own height rather than the line box, or they sit low.
        let baseline = by + scaled.ascent() * 0.36;

        for mut glyph in glyphs {
            glyph.position = ab_glyph::point(pen_x, baseline);
            pen_x += scaled.h_advance(glyph.id);
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= big as i32 || py >= big as i32 {
                        return;
                    }
                    let dst = canvas.get_pixel_mut(px as u32, py as u32);
                    let a = coverage.clamp(0.0, 1.0);
                    for i in 0..3 {
                        dst.0[i] = (dst.0[i] as f32 * (1.0 - a) + 255.0 * a) as u8;
                    }
                    dst.0[3] = dst.0[3].max((a * 255.0) as u8);
                });
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
fn get_last_check(app: AppHandle) -> schedule::LastCheck {
    schedule::load_last_check(&app)
}

// Redraws the icon with the count baked into its badge. Cheap enough to do on
// every change: it is a 176x176 compose and downscale.
fn set_tray_count(app: &AppHandle, total: usize) {
    if let Some(tray) = app.try_state::<Tray>() {
        let _ = tray.0.set_icon(Some(tray_icon(total)));
        let tip = match total {
            0 => "PM Updater — up to date".to_string(),
            1 => "PM Updater — 1 update available".to_string(),
            n => format!("PM Updater — {n} updates available"),
        };
        let _ = tray.0.set_tooltip(Some(tip));
    }
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
    cfg.last_total = schedule::total_from_counts(&cfg.last_counts, cfg.count_dev_updates);
    schedule::save(&app, &cfg)?;
    schedule::sync_agent(&cfg)?;
    set_tray_count(&app, cfg.last_total);
    Ok(cfg)
}

// Re-checks one section, normally straight after installing its updates, so the
// menu-bar count matches what is really installed.
#[tauri::command]
async fn recount_section(app: AppHandle, section: String) -> schedule::ScheduleConfig {
    let cfg = schedule::recount(&app, &section).await;
    set_tray_count(&app, cfg.last_total);
    let _ = app.emit("schedule-updated", cfg.clone());
    cfg
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
    let cfg = schedule::run_scheduled(&app).await;
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

            let open = tauri::menu::MenuItem::with_id(app, "open", "Open PM Updater", true, None::<&str>)?;
            let check = tauri::menu::MenuItem::with_id(app, "check", "Check Now", true, None::<&str>)?;
            let quit = tauri::menu::MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &check, &quit])?;

            let mut tray = tauri::tray::TrayIconBuilder::new()
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main_window(app),
                    "check" => {
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let cfg = schedule::run_scheduled(&handle).await;
                            set_tray_count(&handle, cfg.last_total);
                            schedule::notify_result(&handle, &cfg);
                            let _ = handle.emit("schedule-updated", cfg);
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                });
            // Not a template: a template icon would discard the logo's colour and
            // the badge along with it.
            tray = tray.icon(tray_icon(0)).icon_as_template(false);
            let tray = tray.build(app)?;
            app.manage(Tray(tray));

            // Whatever the last run found, so the count is right straight away.
            // Recomputed rather than trusted: a total stored before the counting
            // rule changed would otherwise sit in the menu bar until the next run.
            let mut startup_cfg = schedule::load(&handle);
            startup_cfg.last_total = schedule::total_from_counts(
                &startup_cfg.last_counts,
                startup_cfg.count_dev_updates,
            );
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
                tauri::async_runtime::spawn(async move {
                    let cfg = schedule::run_scheduled(&launch_handle).await;
                    set_tray_count(&launch_handle, cfg.last_total);
                    schedule::notify_result(&launch_handle, &cfg);
                    let _ = launch_handle.emit("schedule-updated", cfg);
                });
            }

            // While the app is open it does the scheduled runs itself.
            let timer_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
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
            check_app_update, open_release_url, get_release_notes,
            get_schedule, set_schedule, run_schedule_now, snooze_updates, get_last_check, recount_section, next_run
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
