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
    local APP_PATH
    APP_PATH=$(grep "Permission denied @ apply2files" "$TMPOUT" | head -1 \
      | sed 's/.*@ apply2files - //' | sed 's|/Contents/.*||')
    rm -f "$TMPOUT"
    if [ -n "$APP_PATH" ]; then
      echo "→  $(basename "$APP_PATH") is protected by macOS. Enter your password to allow the update."
      echo "→  Requesting administrator access…"
      osascript -e "do shell script \"chown -R $CURRENT_USER '$APP_PATH'\" with administrator privileges" 2>&1
      brew upgrade --cask $APPDIR_FLAG "$token" 2>&1
      BREW_EXIT=$?
    else
      BREW_EXIT=1
      echo "✖  Could not determine app path."
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
    // Only allow the known user-Applications path; reject anything else
    let appdir_flag = match appdir.as_deref() {
        Some("~/Applications") => "--appdir \"$HOME/Applications\"",
        _ => "",
    };
    let section = "untracked_apps";
    let script = format!(
        r#"CURRENT_USER=$(whoami)
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
TMPOUT=$(mktemp)
brew install --cask --force {appdir_flag} {cask_token} 2>&1 | tee "$TMPOUT"
BREW_EXIT=${{PIPESTATUS[0]}}
if grep -q "Permission denied @ apply2files" "$TMPOUT"; then
  APP_PATH=$(grep "Permission denied @ apply2files" "$TMPOUT" | head -1 | sed 's/.*@ apply2files - //' | sed 's|/Contents/.*||')
  rm -f "$TMPOUT"
  if [ -n "$APP_PATH" ]; then
    echo "→  $(basename "$APP_PATH") is protected by macOS. Enter your password to let the update manager take over."
    echo "→  Requesting administrator access…"
    osascript -e "do shell script \"chown -R $CURRENT_USER '$APP_PATH'\" with administrator privileges" 2>&1
    brew install --cask --force {appdir_flag} {cask_token} 2>&1
    if [ $? -eq 0 ]; then
      echo "→  Done! Run a check to see this app move to Homebrew Apps."
    else
      echo "✖  Setup failed."
    fi
  else
    echo "✖  Setup failed."
  fi
elif [ "$BREW_EXIT" -eq 0 ]; then
  rm -f "$TMPOUT"
  echo "→  Done! Run a check to see this app move to Homebrew Apps."
else
  rm -f "$TMPOUT"
  echo "✖  Setup failed."
fi"#
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

// Returns collected output lines for logging.
async fn run_upgrade_shell(app: &AppHandle, section: &str, script: &str) -> Vec<String> {
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
            emit_upgrade_line(&app1, &sec1, &line).await;
            if let Ok(mut v) = coll1.lock() { v.push(line); }
        }
    });

    let app2 = app.clone();
    let sec2 = section.to_string();
    let coll2 = Arc::clone(&collected);
    let t2 = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            emit_upgrade_line(&app2, &sec2, &line).await;
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

fn upgrade_script(section: &str) -> Option<String> {
    match section {
        "macos_updates" => Some(r#"
osascript -e 'do shell script "softwareupdate -ia --verbose" with administrator privileges' 2>&1
echo "→  macOS update complete."
"#.to_string()),
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
"#, fn_def = brew_cask_upgrade_fn())),
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
    let script: String = match section.as_str() {
        "brew_casks" => {
            let safe_tokens: Vec<String> = items.iter()
                .filter(|n| n.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.'))
                .cloned().collect();
            let calls = safe_tokens.iter()
                .map(|t| format!("brew_upgrade_cask '{t}'"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\nif command -v brew &>/dev/null; then\n{fn_def}\n{calls}\nelse\n  echo '✖  brew not found'\nfi",
                fn_def = brew_cask_upgrade_fn(),
                calls = calls
            )
        }
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
            format!(
                "osascript -e 'do shell script \"softwareupdate -i {labels}\" with administrator privileges' 2>&1\necho '→  macOS update complete.'"
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
            get_upgrade_history, search_cask, track_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
