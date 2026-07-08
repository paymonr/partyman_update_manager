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
  local current_user app_path app_basename chownout
  current_user=$(whoami)
  app_path=$(grep "Permission denied @ apply2files" "$tmpout" | head -1 \
    | sed 's/.*@ apply2files - //' | sed 's|/Contents/.*||')
  if [ -z "$app_path" ]; then
    echo "✖  Could not determine app path."
    return 1
  fi
  app_basename=$(basename "$app_path")
  echo "→  $app_basename is protected by macOS. Enter your password to allow the change."
  echo "→  Requesting administrator access…"
  # '2>&1; true' keeps chown errors in the result and stops `do shell script`
  # from raising, so we can inspect what actually happened.
  chownout=$(mktemp)
  osascript -e "do shell script \"chown -R $current_user '$app_path' 2>&1; true\" with administrator privileges" > "$chownout" 2>&1
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
        "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\n{helper}\n{adopt}\n{calls}echo '→  All set! Run a check to move these apps to Homebrew Apps.'\nelse\n  echo '✖  brew not found'\nfi",
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

async fn run_shell(app: &AppHandle, section: &str, script: &str) {
    let shell = if cfg!(target_os = "windows") { "powershell" } else { "bash" };
    let flag  = if cfg!(target_os = "windows") { "-Command" } else { "-c" };

    let preamble = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        r#"
export PATH="$HOME/.rvm/bin:$HOME/.rbenv/bin:$HOME/.nvm/versions/node/$(ls $HOME/.nvm/versions/node 2>/dev/null | tail -1)/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
command -v rbenv &>/dev/null && eval "$(rbenv init -)"
"#
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
//   - Names the specific app via $PM_ASKPASS_APP when set.
//   - Uses a plain `osascript` dialog (no "System Events") to avoid the TCC
//     automation permission prompt.
//   - Exits non-zero on Cancel or timeout so sudo aborts cleanly instead of
//     receiving an error string as the password.
const ASKPASS_PREAMBLE: &str = r#"
export PATH="$HOME/.rvm/bin:$HOME/.rbenv/bin:$HOME/.nvm/versions/node/$(ls $HOME/.nvm/versions/node 2>/dev/null | tail -1)/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
command -v rbenv &>/dev/null && eval "$(rbenv init -)"
_pm_askpass=$(mktemp /tmp/partyman-askpass-XXXX)
cat > "$_pm_askpass" <<'PARTYMAN_ASKPASS'
#!/bin/bash
_pm_app="${PM_ASKPASS_APP:-}"
if [ -n "$_pm_app" ]; then
  _pm_msg="PartyMAN Update Manager needs your administrator password to update ${_pm_app}."
else
  _pm_msg="PartyMAN Update Manager needs your administrator password to complete this update."
fi
_pm_pw=$(osascript -e "display dialog \"${_pm_msg}\" default answer \"\" with hidden answer with title \"PartyMAN Update Manager\" with icon caution giving up after 120" -e 'text returned of result' 2>/dev/null) || exit 1
[ -z "$_pm_pw" ] && exit 1
printf '%s' "$_pm_pw"
PARTYMAN_ASKPASS
chmod 700 "$_pm_askpass"
export SUDO_ASKPASS="$_pm_askpass"
trap 'rm -f "$_pm_askpass"' EXIT
"#;

// Returns collected output lines for logging (including any cask sentinels).
async fn run_upgrade_shell(app: &AppHandle, section: &str, script: &str) -> Vec<String> {
    let shell = if cfg!(target_os = "windows") { "powershell" } else { "bash" };
    let flag  = if cfg!(target_os = "windows") { "-Command" } else { "-c" };

    let preamble = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        ASKPASS_PREAMBLE
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
        for (token, name) in &pairs {
            // Sanitize for the AppleScript dialog (strip quotes/backslashes) and the
            // single-quoted shell export.
            let name_esc = name.replace(['\\', '"'], "").replace('\'', "'\\''");
            body.push_str(&format!("echo '{CASK_START}{token}'\n"));
            body.push_str(&format!("export PM_ASKPASS_APP='{name_esc}'\n"));
            body.push_str(&format!("brew_upgrade_cask '{token}'\n"));
            body.push_str(&format!("echo '{CASK_END}{token}'\n"));
        }
        let script = format!(
            "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\n{fn_def}\n{body}else\n  echo '✖  brew not found'\nfi",
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

#[tauri::command]
fn open_release_url(url: String) {
    if url.starts_with("https://github.com/paymonr/partyman_update_manager") {
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
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                let png = include_bytes!("../icons/128x128@2x.png");
                let img = image::load_from_memory(png)?;
                let (w, h) = img.dimensions();
                let icon = tauri::image::Image::new_owned(img.to_rgba8().into_raw(), w, h);
                let _ = window.set_icon(icon);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            run_check, run_upgrade, run_upgrade_items, get_platform,
            get_upgrade_history, search_cask, track_app, track_apps,
            check_app_update, open_release_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
