use std::process::Stdio;
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

async fn emit_line(app: &AppHandle, section: &str, line: &str) {
    let _ = app.emit(
        "check-output",
        OutputPayload {
            section: section.to_string(),
            line: line.to_string(),
        },
    );
}

async fn emit_upgrade_line(app: &AppHandle, section: &str, line: &str) {
    let _ = app.emit(
        "upgrade-output",
        OutputPayload {
            section: section.to_string(),
            line: line.to_string(),
        },
    );
}

async fn emit_upgrade_status(app: &AppHandle, section: &str, status: &str) {
    let _ = app.emit(
        "upgrade-status",
        StatusPayload {
            section: section.to_string(),
            status: status.to_string(),
        },
    );
}

async fn emit_status(app: &AppHandle, section: &str, status: &str) {
    let _ = app.emit(
        "check-status",
        StatusPayload {
            section: section.to_string(),
            status: status.to_string(),
        },
    );
}

async fn run_shell(app: &AppHandle, section: &str, script: &str) {
    let shell = if cfg!(target_os = "windows") {
        "powershell"
    } else {
        "bash"
    };
    let flag = if cfg!(target_os = "windows") {
        "-Command"
    } else {
        "-c"
    };

    // Source rvm/rbenv/nvm so PATH is complete even in a non-login shell
    let preamble = if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
        r#"
export PATH="$HOME/.rvm/bin:$HOME/.rbenv/bin:$HOME/.nvm/versions/node/$(ls $HOME/.nvm/versions/node 2>/dev/null | tail -1)/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
command -v rbenv &>/dev/null && eval "$(rbenv init -)"
"#
    } else {
        ""
    };

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
        _ => "done", // treat non-zero as done-with-warnings, not error
    };
    emit_status(app, section, final_status).await;
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
APP_DIR="/Applications"
cask_tokens=""
cask_apps=""
if command -v brew &>/dev/null; then
  cask_tokens=$(brew list --cask 2>/dev/null)
  if [ -n "$cask_tokens" ] && command -v jq &>/dev/null; then
    cask_apps=$(brew info --cask --json=v2 $cask_tokens 2>/dev/null \
      | jq -r '.casks[].artifacts[]?.app[]? | select(type=="string")' 2>/dev/null)
  fi
fi
untracked=0
for app in "$APP_DIR"/*.app; do
  [ -e "$app" ] || continue
  base=$(basename "$app")
  name=${base%.app}
  codesign -dvv "$app" 2>&1 | grep -q "Authority=Software Signing" && continue
  [ -e "$app/Contents/_MASReceipt/receipt" ] && continue
  [ -n "$cask_apps" ] && echo "$cask_apps" | grep -qxF "$base" && continue
  norm=$(echo "$name" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9')
  hit=0
  while IFS= read -r tok; do
    [ -z "$tok" ] && continue
    [ "$(echo "$tok" | tr -cd 'a-z0-9')" = "$norm" ] && { hit=1; break; }
  done <<< "$cask_tokens"
  [ "$hit" -eq 1 ] && continue
  echo "⚠  $name"
  untracked=1
done
[ "$untracked" -eq 0 ] && echo "✔  No untracked apps found in $APP_DIR."
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

fn upgrade_script(section: &str) -> Option<&'static str> {
    match section {
        "macos_updates" => Some(r#"
osascript -e 'do shell script "softwareupdate -ia --verbose" with administrator privileges' 2>&1
echo "→  macOS update complete."
"#),
        "brew_casks" => Some(r#"
if command -v brew &>/dev/null; then
  brew upgrade --cask --greedy 2>&1
  echo "→  Homebrew cask upgrade complete."
else
  echo "✖  brew not found"
fi
"#),
        "app_store" => Some(r#"
echo "→  Opening App Store Updates…"
open "macappstores://showUpdatesPage"
echo "✔  App Store opened — please click Update next to each app."
"#),
        "brew_formulae" => Some(r#"
if command -v brew &>/dev/null; then
  brew upgrade 2>&1
  echo "→  Homebrew formulae upgrade complete."
else
  echo "✖  brew not found"
fi
"#),
        "npm_globals" => Some(r#"
if command -v npm &>/dev/null; then
  npm update -g 2>&1
  echo "→  npm global packages updated."
else
  echo "✖  npm not found"
fi
"#),
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
"#),
        "ruby_rvm" => Some(r#"
[ -s "$HOME/.rvm/scripts/rvm" ] && source "$HOME/.rvm/scripts/rvm"
if command -v gem &>/dev/null; then
  gem update 2>&1
  echo "→  gems updated."
else
  echo "✖  gem not found"
fi
"#),
        "ruby_rbenv" => Some(r#"
if command -v gem &>/dev/null; then
  gem update 2>&1
  echo "→  gems updated."
else
  echo "✖  gem not found"
fi
"#),
        _ => None,
    }
}

async fn run_upgrade_shell(app: &AppHandle, section: &str, script: &str) {
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
            emit_upgrade_line(&app1, &sec1, &line).await;
        }
    });

    let app2 = app.clone();
    let sec2 = section.to_string();
    let t2 = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            emit_upgrade_line(&app2, &sec2, &line).await;
        }
    });

    let _ = tokio::join!(t1, t2);
    let _ = child.wait().await;
    emit_upgrade_status(app, section, "done").await;
}

#[tauri::command]
async fn run_upgrade(app: AppHandle, section: String) {
    match upgrade_script(&section) {
        Some(script) => run_upgrade_shell(&app, &section, script).await,
        None => {
            emit_upgrade_line(&app, &section, &format!("No upgrade command for: {section}")).await;
            emit_upgrade_status(&app, &section, "error").await;
        }
    }
}


#[tauri::command]
async fn run_upgrade_items(app: AppHandle, section: String, items: Vec<String>) {
    if items.is_empty() {
        emit_upgrade_line(&app, &section, "No items selected.").await;
        emit_upgrade_status(&app, &section, "done").await;
        return;
    }
    let script: String = match section.as_str() {
        "brew_casks" => {
            let names = items.iter()
                .filter(|n| n.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '.'))
                .cloned().collect::<Vec<_>>().join(" ");
            format!(
                "if command -v brew &>/dev/null; then\n  brew upgrade --cask {names} 2>&1\n  echo '→  Done.'\nelse\n  echo '✖  brew not found'\nfi"
            )
        }
        "app_store" => {
            let names: Vec<String> = items.iter().cloned().collect();
            let list = if names.is_empty() {
                "your selected apps".to_string()
            } else {
                names.join(", ")
            };
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
    run_upgrade_shell(&app, &section, &script).await;
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
        .invoke_handler(tauri::generate_handler![run_check, run_upgrade, run_upgrade_items, get_platform])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
