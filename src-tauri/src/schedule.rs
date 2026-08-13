// Scheduled update checks.
//
// The schedule runs even when the app has been quit, so it cannot live in the
// webview. A launchd agent re-launches the app with SCHEDULED_RUN_FLAG on the
// chosen interval; that run has no window, records what it found, tells the user
// via a notification, and exits. When the app *is* open it does the same work on
// an in-process timer and the agent run stands down (see run_lock).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager};

pub const AGENT_LABEL: &str = "com.partyman.updater.scheduler";
pub const SCHEDULED_RUN_FLAG: &str = "--scheduled-run";

// Every section a scheduled run checks. Untracked apps are left out: that scan
// runs `codesign` over each app in /Applications, far too slow for a background
// job, and it lists unmanaged apps rather than pending updates.
pub const CHECKED_SECTIONS: &[&str] = &[
    "macos_updates",
    "app_store",
    "brew_casks",
    "brew_formulae",
    "npm_globals",
    "pip_packages",
    "ruby_rvm",
    "ruby_rbenv",
];

// `default` at the container level so a config written by an older version, or
// missing a field added later, still loads with its history intact.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScheduleConfig {
    pub enabled: bool,
    /// "hourly" | "daily" | "weekly". Each reads only the fields it needs.
    pub frequency: String,
    /// Minute past the hour; every frequency uses it.
    pub minute: u32,
    /// Hour of day for daily and weekly.
    pub hour: u32,
    /// 0 = Sunday … 6 = Saturday, for weekly. Matches launchd's own numbering.
    pub weekday: u32,
    pub notify: bool,
    pub last_run: u64,
    pub last_total: usize,
    pub last_counts: BTreeMap<String, usize>,
    /// Reminders stay quiet until this time; the count still updates underneath.
    pub snoozed_until: u64,
    /// Fold brew formulae, npm, pip and gems into the count as well. Off by
    /// default because they run to hundreds and drown out app updates.
    pub count_dev_updates: bool,
    /// Run a check as soon as the app opens, rather than waiting for the interval.
    pub check_on_launch: bool,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            frequency: "daily".to_string(),
            minute: 0,
            hour: 10,
            weekday: 1,
            notify: true,
            last_run: 0,
            last_total: 0,
            last_counts: BTreeMap::new(),
            snoozed_until: 0,
            count_dev_updates: false,
            check_on_launch: false,
        }
    }
}

impl ScheduleConfig {
    pub fn snoozed(&self) -> bool {
        now_secs() < self.snoozed_until
    }

    /// Clamped so a hand-edited config cannot ask launchd for an impossible time.
    pub fn minute(&self) -> u32 {
        self.minute.min(59)
    }

    pub fn hour(&self) -> u32 {
        self.hour.min(23)
    }

    pub fn weekday(&self) -> u32 {
        self.weekday % 7
    }
}

/// The next moment the schedule should fire, strictly after `from`.
///
/// Local time, so "10:00 daily" stays at 10:00 across a daylight-saving change
/// rather than drifting by an hour the way a fixed interval would.
pub fn next_run_after(from: i64, cfg: &ScheduleConfig) -> i64 {
    use chrono::{Datelike, Duration, Local, NaiveTime, TimeZone, Timelike};

    let from_dt = match Local.timestamp_opt(from, 0).single() {
        Some(dt) => dt,
        None => return from,
    };

    match cfg.frequency.as_str() {
        "hourly" => {
            let mut t = from_dt
                .with_minute(cfg.minute())
                .and_then(|t| t.with_second(0))
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(from_dt);
            while t <= from_dt {
                t += Duration::hours(1);
            }
            t.timestamp()
        }
        "weekly" => {
            let at = NaiveTime::from_hms_opt(cfg.hour(), cfg.minute(), 0).unwrap_or_default();
            let mut day = from_dt.date_naive();
            for _ in 0..8 {
                if let Some(t) = Local.from_local_datetime(&day.and_time(at)).single() {
                    if t > from_dt && t.weekday().num_days_from_sunday() == cfg.weekday() {
                        return t.timestamp();
                    }
                }
                day += Duration::days(1);
            }
            from + 7 * 24 * 3600
        }
        _ => {
            let at = NaiveTime::from_hms_opt(cfg.hour(), cfg.minute(), 0).unwrap_or_default();
            let mut day = from_dt.date_naive();
            for _ in 0..3 {
                if let Some(t) = Local.from_local_datetime(&day.and_time(at)).single() {
                    if t > from_dt {
                        return t.timestamp();
                    }
                }
                day += Duration::days(1);
            }
            from + 24 * 3600
        }
    }
}

/// Whether the open app should run a check now. A schedule that has never run
/// waits for its next proper slot rather than firing the moment it is enabled —
/// "Check when PM Updater opens" and Run Now cover the impatient case.
pub fn is_due(cfg: &ScheduleConfig) -> bool {
    if !cfg.enabled || cfg.last_run == 0 {
        return false;
    }
    now_secs() as i64 >= next_run_after(cfg.last_run as i64, cfg)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn config_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("schedule.json"))
}

pub fn load(app: &AppHandle) -> ScheduleConfig {
    config_path(app)
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, cfg: &ScheduleConfig) -> Result<(), String> {
    let path = config_path(app).ok_or("no app data directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_dir: Option<String>,
}

// Mirrors parseItems() in App.svelte. The four sections below list their items
// individually; the rest report a total instead, which count_for reads.
pub fn parse_items(section: &str, lines: &[String]) -> Vec<CheckItem> {
    let mut items = Vec::new();
    match section {
        "brew_casks" => {
            let mut in_block = false;
            for line in lines {
                if line.contains("Outdated apps:") {
                    in_block = true;
                    continue;
                }
                if !in_block {
                    continue;
                }
                let trimmed = line.trim();
                if trimmed.starts_with('→') {
                    break;
                }
                if let Some(name) = trimmed.split_whitespace().next() {
                    items.push(CheckItem {
                        id: name.to_string(),
                        name: name.to_string(),
                        app_dir: None,
                    });
                }
            }
        }
        "app_store" => {
            let mut in_block = false;
            for line in lines {
                if line.contains("Outdated App Store apps:") {
                    in_block = true;
                    continue;
                }
                if !in_block {
                    continue;
                }
                let trimmed = line.trim();
                if trimmed.starts_with('→') {
                    break;
                }
                let mut parts = trimmed.split_whitespace();
                let id = match parts.next() {
                    Some(id) if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) => id,
                    _ => continue,
                };
                let rest = parts.collect::<Vec<_>>().join(" ");
                let name = strip_trailing_parenthetical(&rest);
                items.push(CheckItem {
                    id: id.to_string(),
                    name: if name.is_empty() { id.to_string() } else { name },
                    app_dir: None,
                });
            }
        }
        "macos_updates" => {
            for line in lines {
                if !line.contains('*') {
                    continue;
                }
                if let Some(idx) = line.find("Label:") {
                    let label = line[idx + "Label:".len()..].trim();
                    if !label.is_empty() {
                        items.push(CheckItem {
                            id: label.to_string(),
                            name: label.to_string(),
                            app_dir: None,
                        });
                    }
                }
            }
        }
        "untracked_apps" => {
            for line in lines {
                let trimmed = line.trim();
                let rest = match trimmed.strip_prefix('⚠') {
                    Some(r) => r.trim(),
                    None => continue,
                };
                let (name, app_dir) = match rest.strip_suffix("[~/Applications]") {
                    Some(n) => (n.trim(), Some("~/Applications".to_string())),
                    None => (rest, None),
                };
                if !name.is_empty() {
                    items.push(CheckItem {
                        id: name.to_string(),
                        name: name.to_string(),
                        app_dir,
                    });
                }
            }
        }
        _ => {}
    }
    items
}

fn strip_trailing_parenthetical(s: &str) -> String {
    let trimmed = s.trim_end();
    if trimmed.ends_with(')') {
        if let Some(open) = trimmed.rfind('(') {
            return trimmed[..open].trim().to_string();
        }
    }
    trimmed.to_string()
}

// How many updates a section is reporting. The itemised sections are counted by
// their items; the others print a "⚠  <n> outdated …" summary line, and a ⚠ line
// with no leading number (rvm's "Current: … | Latest: …") is one update.
pub fn count_for(section: &str, lines: &[String]) -> usize {
    match section {
        "brew_casks" | "app_store" | "macos_updates" | "untracked_apps" => {
            parse_items(section, lines).len()
        }
        _ => lines
            .iter()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix('⚠')?.trim();
                let leading: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                Some(leading.parse::<usize>().unwrap_or(1))
            })
            .sum(),
    }
}

// What the menu-bar count reports. Developer tooling — brew formulae, npm, pip,
// gems — is deliberately excluded: those run to hundreds of packages and would
// bury the handful of app and system updates actually worth acting on. They are
// still checked, and still shown on the page.
pub const BADGE_SECTIONS: &[&str] = &["macos_updates", "app_store", "brew_casks"];

pub fn total_from_counts(counts: &BTreeMap<String, usize>, include_dev: bool) -> usize {
    counts
        .iter()
        .filter(|(section, _)| {
            BADGE_SECTIONS.contains(&section.as_str())
                || (include_dev && section.as_str() != "untracked_apps")
        })
        .map(|(_, n)| n)
        .sum()
}

// The output of the last run, kept so the app can show each section already
// filled in instead of making the user re-run checks it just did. Separate from
// the config so a large result never risks the settings file.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LastCheck {
    pub ts: u64,
    pub sections: BTreeMap<String, Vec<String>>,
}

fn last_check_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("last_check.json"))
}

pub fn load_last_check(app: &AppHandle) -> LastCheck {
    last_check_path(app)
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_last_check(app: &AppHandle, last: &LastCheck) {
    let path = match last_check_path(app) {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(last) {
        let _ = fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn counts_itemised_sections_by_item() {
        let out = lines(
            "→  Refreshing Homebrew…\n\
             ⚠  Outdated apps:\n   \
             alt-tab (6.46.1) != 11.4.4\n   \
             brave-browser (1.91.172.0) != 1.93.136.0\n\
             →  To upgrade all: brew upgrade --cask",
        );
        let items = parse_items("brew_casks", &out);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "alt-tab");
        // the trailing "→" line must not be counted as an app
        assert_eq!(count_for("brew_casks", &out), 2);
    }

    #[test]
    fn reads_the_summary_count_for_non_itemised_sections() {
        let out = lines(
            "⚠  7 outdated formula(e):\n   \
             foo 1.0 -> 2.0\n\
             →  To upgrade all: brew upgrade",
        );
        assert_eq!(count_for("brew_formulae", &out), 7);
    }

    #[test]
    fn a_warning_without_a_number_counts_as_one() {
        let out = lines("⚠  Current: ruby-3.1.0  |  Latest: ruby-3.3.0");
        assert_eq!(count_for("ruby_rvm", &out), 1);
    }

    #[test]
    fn up_to_date_sections_count_zero() {
        assert_eq!(count_for("brew_formulae", &lines("✔  All up to date.")), 0);
        assert_eq!(
            count_for("brew_casks", &lines("✔  All Homebrew cask apps are up to date.")),
            0
        );
    }

    #[test]
    fn parses_app_store_ids_and_names() {
        let out = lines(
            "⚠  Outdated App Store apps:\n   \
             497799835 Xcode (15.0 -> 15.1)\n   \
             garbage line without an id",
        );
        let items = parse_items("app_store", &out);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "497799835");
        assert_eq!(items[0].name, "Xcode");
    }

    #[test]
    fn parses_macos_labels_and_untracked_app_dirs() {
        let macos = lines("   * Label: macOS Sequoia 15.2-24C101\n   Title: macOS Sequoia");
        let items = parse_items("macos_updates", &macos);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "macOS Sequoia 15.2-24C101");

        let untracked = lines("⚠  Slack\n⚠  Figma [~/Applications]\n✔  No untracked apps found.");
        let items = parse_items("untracked_apps", &untracked);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].name, "Figma");
        assert_eq!(items[1].app_dir.as_deref(), Some("~/Applications"));
    }

    #[test]
    fn badge_total_counts_only_apps_and_system_updates() {
        let mut counts = BTreeMap::new();
        counts.insert("brew_casks".to_string(), 3);
        counts.insert("app_store".to_string(), 2);
        counts.insert("macos_updates".to_string(), 1);
        counts.insert("untracked_apps".to_string(), 9);
        // developer tooling is checked and shown, but never inflates the badge
        counts.insert("brew_formulae".to_string(), 137);
        counts.insert("ruby_rvm".to_string(), 265);
        counts.insert("npm_globals".to_string(), 2);
        counts.insert("pip_packages".to_string(), 7);
        assert_eq!(total_from_counts(&counts, false), 6);
        // opting in adds the package managers but never untracked apps
        assert_eq!(total_from_counts(&counts, true), 6 + 137 + 265 + 2 + 7);
    }

    #[test]
    fn weekly_lands_on_the_chosen_day_and_time() {
        use chrono::{Datelike, Local, TimeZone, Timelike};
        let mut cfg = ScheduleConfig::default();
        cfg.frequency = "weekly".into();
        cfg.weekday = 3; // Wednesday
        cfg.hour = 9;
        cfg.minute = 30;

        let from = Local.with_ymd_and_hms(2026, 8, 13, 12, 0, 0).unwrap(); // a Thursday
        let next = Local.timestamp_opt(next_run_after(from.timestamp(), &cfg), 0).unwrap();

        assert_eq!(next.weekday().num_days_from_sunday(), 3);
        assert_eq!((next.hour(), next.minute()), (9, 30));
        assert!(next > from, "must be in the future");
        // the very next Wednesday, not the one after
        assert!((next - from).num_days() < 7);
    }

    #[test]
    fn daily_rolls_to_tomorrow_once_todays_slot_has_passed() {
        use chrono::{Local, TimeZone, Timelike};
        let mut cfg = ScheduleConfig::default();
        cfg.frequency = "daily".into();
        cfg.hour = 10;
        cfg.minute = 0;

        let before = Local.with_ymd_and_hms(2026, 8, 13, 9, 0, 0).unwrap();
        let next = Local.timestamp_opt(next_run_after(before.timestamp(), &cfg), 0).unwrap();
        assert_eq!((next.hour(), next.minute()), (10, 0));
        assert_eq!((next - before).num_hours(), 1);

        let after = Local.with_ymd_and_hms(2026, 8, 13, 11, 0, 0).unwrap();
        let next = Local.timestamp_opt(next_run_after(after.timestamp(), &cfg), 0).unwrap();
        assert_eq!((next - after).num_hours(), 23);
    }

    #[test]
    fn hourly_uses_the_minute_and_ignores_the_hour() {
        use chrono::{Local, TimeZone, Timelike};
        let mut cfg = ScheduleConfig::default();
        cfg.frequency = "hourly".into();
        cfg.minute = 15;

        let from = Local.with_ymd_and_hms(2026, 8, 13, 9, 20, 0).unwrap();
        let next = Local.timestamp_opt(next_run_after(from.timestamp(), &cfg), 0).unwrap();
        assert_eq!((next.hour(), next.minute()), (10, 15));
    }

    #[test]
    fn a_schedule_that_never_ran_waits_for_its_slot() {
        let mut cfg = ScheduleConfig::default();
        cfg.enabled = true;
        cfg.last_run = 0;
        assert!(!is_due(&cfg));
    }

    #[test]
    fn plist_calendar_keys_match_the_frequency() {
        let mut cfg = ScheduleConfig::default();
        cfg.frequency = "hourly".into();
        cfg.minute = 5;
        let hourly = calendar_entries(&cfg);
        assert!(hourly.contains("<key>Minute</key><integer>5</integer>"));
        assert!(!hourly.contains("Hour"), "hourly must not pin an hour: {hourly}");

        cfg.frequency = "weekly".into();
        cfg.weekday = 6;
        let weekly = calendar_entries(&cfg);
        assert!(weekly.contains("<key>Weekday</key><integer>6</integer>"), "{weekly}");
        assert!(weekly.contains("<key>Hour</key>"), "{weekly}");
    }
}

// ---------------------------------------------------------------------------
// Running a scheduled check
// ---------------------------------------------------------------------------

use tauri_plugin_notification::NotificationExt;

fn run_lock_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("run.lock"))
}

// One scheduled run at a time. The launchd agent fires whether or not the app is
// open, so the open app holds this lock for its whole life and the agent's run
// stands down rather than checking twice over.
#[cfg(unix)]
pub fn try_acquire_run_lock(app: &AppHandle) -> Option<fs::File> {
    use std::os::unix::io::AsRawFd;
    let path = run_lock_path(app)?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    held.then_some(file)
}

#[cfg(not(unix))]
pub fn try_acquire_run_lock(_app: &AppHandle) -> Option<fs::File> {
    None
}

/// Runs every counted section, records what it found, and returns the new config.
/// Untracked apps are skipped: that scan runs `codesign` over every app in
/// /Applications, which is far too slow for a background job, and it is not an
/// update count anyway.
pub async fn run_checks(app: &AppHandle) -> ScheduleConfig {
    let mut counts = BTreeMap::new();
    let mut last = LastCheck { ts: now_secs(), sections: BTreeMap::new() };
    for section in CHECKED_SECTIONS {
        let lines = crate::run_check_collect(section).await;
        counts.insert(section.to_string(), count_for(section, &lines));
        last.sections.insert(section.to_string(), lines);
    }
    save_last_check(app, &last);
    let mut cfg = load(app);
    cfg.last_run = now_secs();
    cfg.last_total = total_from_counts(&counts, cfg.count_dev_updates);
    cfg.last_counts = counts;
    let _ = save(app, &cfg);
    cfg
}

pub fn notify_result(app: &AppHandle, cfg: &ScheduleConfig) {
    if !cfg.notify || cfg.last_total == 0 || cfg.snoozed() {
        return;
    }
    let n = cfg.last_total;
    // macOS notifications carry no buttons here, so the choice of installing or
    // postponing is offered in the app; this just says where to find it.
    let body = if n == 1 {
        "1 update is available. Open PartyMAN to install or postpone.".to_string()
    } else {
        format!("{n} updates are available. Open PartyMAN to install or postpone.")
    };
    let _ = app
        .notification()
        .builder()
        .title("PartyMAN Update Manager")
        .body(body)
        .show();
}

// ---------------------------------------------------------------------------
// The launchd agent that runs us when the app is closed
// ---------------------------------------------------------------------------

pub fn agent_plist_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist"))
    })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// launchd fires whenever every key present matches, so omitting a key means
// "any". Hourly gives only Minute, daily adds Hour, weekly adds Weekday.
fn calendar_entries(cfg: &ScheduleConfig) -> String {
    let mut out = format!("        <key>Minute</key><integer>{}</integer>\n", cfg.minute());
    if cfg.frequency != "hourly" {
        out.push_str(&format!("        <key>Hour</key><integer>{}</integer>\n", cfg.hour()));
    }
    if cfg.frequency == "weekly" {
        out.push_str(&format!("        <key>Weekday</key><integer>{}</integer>\n", cfg.weekday()));
    }
    out
}

fn gui_domain() -> String {
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    format!("gui/{uid}")
}

/// (Re)writes and loads the agent. Called whenever the schedule is turned on or
/// its interval changes — launchd only picks up a new interval on reload.
pub fn install_agent(cfg: &ScheduleConfig) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy().to_string();
    let path = agent_plist_path().ok_or("no HOME directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>{flag}</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
{calendar}    </dict>
    <key>RunAtLoad</key>
    <false/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
</dict>
</plist>
"#,
        label = AGENT_LABEL,
        exe = xml_escape(&exe),
        flag = SCHEDULED_RUN_FLAG,
        calendar = calendar_entries(cfg),
    );
    fs::write(&path, plist).map_err(|e| e.to_string())?;

    let domain = gui_domain();
    // Unload first: bootstrap is a no-op (error 37) if the label is already loaded,
    // so without this a changed interval would silently keep the old one.
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{AGENT_LABEL}")])
        .output();

    let out = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain, &path.to_string_lossy()])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn remove_agent() -> Result<(), String> {
    let domain = gui_domain();
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{AGENT_LABEL}")])
        .output();
    if let Some(path) = agent_plist_path() {
        if path.exists() {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// True when a loaded agent exists that points at the binary running right now.
/// Moving or reinstalling the app changes that path, and launchd would go on
/// launching the old one — or nothing at all — without saying so.
pub fn agent_is_current() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => return false,
    };
    agent_plist_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|plist| plist.contains(&xml_escape(&exe)))
        .unwrap_or(false)
}

/// Re-registers the agent if it is missing or stale. Cheap enough to call at
/// every launch, and it is what repairs the schedule after the app moves.
pub fn ensure_agent_current(cfg: &ScheduleConfig) -> Result<(), String> {
    if cfg.enabled && !agent_is_current() {
        install_agent(cfg)
    } else {
        Ok(())
    }
}

/// Brings the agent in line with the config: loaded at the right interval when
/// enabled, gone when not.
pub fn sync_agent(cfg: &ScheduleConfig) -> Result<(), String> {
    if cfg.enabled {
        install_agent(cfg)
    } else {
        remove_agent()
    }
}

/// A whole scheduled run. Checking only: installing needs the user present, both
/// for the administrator password and to decide what actually gets upgraded, so a
/// run reports what it found and the app offers to install it.
pub async fn run_scheduled(app: &AppHandle) -> ScheduleConfig {
    run_checks(app).await
}

/// Re-checks a single section after it has been upgraded, so the count reflects
/// what is now installed rather than what was outstanding before. Cheaper than a
/// full run, which is why it can happen after every install.
pub async fn recount(app: &AppHandle, section: &str) -> ScheduleConfig {
    let mut cfg = load(app);
    if !CHECKED_SECTIONS.contains(&section) {
        return cfg;
    }
    let lines = crate::run_check_collect(section).await;
    cfg.last_counts
        .insert(section.to_string(), count_for(section, &lines));
    cfg.last_total = total_from_counts(&cfg.last_counts, cfg.count_dev_updates);
    let _ = save(app, &cfg);

    // Keep the preloaded view in step, or reopening the app would show the
    // section's pre-upgrade contents.
    let mut last = load_last_check(app);
    if last.ts == 0 {
        last.ts = now_secs();
    }
    last.sections.insert(section.to_string(), lines);
    save_last_check(app, &last);

    cfg
}

/// Silences reminders for a while. The badge keeps showing the real count.
pub fn snooze(app: &AppHandle, hours: u64) -> Result<ScheduleConfig, String> {
    let mut cfg = load(app);
    cfg.snoozed_until = now_secs() + hours * 3600;
    save(app, &cfg)?;
    Ok(cfg)
}

#[cfg(test)]
mod agent_tests {
    use super::*;

    // Touches the real launchd session, so it is opt-in: `cargo test -- --ignored`.
    // CI has no GUI login session for `launchctl bootstrap gui/<uid>` to attach to.
    #[test]
    #[ignore = "registers a real LaunchAgent; needs a GUI login session"]
    fn installs_updates_and_removes_the_agent() {
        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = remove_agent();
            }
        }
        let _cleanup = Cleanup;

        let path = agent_plist_path().expect("HOME set");

        let mut cfg = ScheduleConfig::default();
        cfg.frequency = "daily".into();
        cfg.hour = 4;
        cfg.minute = 20;

        install_agent(&cfg).expect("install");
        assert!(path.exists(), "plist was not written");
        let plist = fs::read_to_string(&path).unwrap();
        assert!(plist.contains("<key>Hour</key><integer>4</integer>"), "hour missing:\n{plist}");
        assert!(plist.contains("<key>Minute</key><integer>20</integer>"), "minute missing:\n{plist}");
        assert!(plist.contains(SCHEDULED_RUN_FLAG), "flag missing:\n{plist}");
        assert!(agent_is_current(), "agent should point at this binary");

        let loaded = std::process::Command::new("launchctl")
            .args(["print", &format!("{}/{}", gui_domain(), AGENT_LABEL)])
            .output()
            .unwrap();
        assert!(
            loaded.status.success(),
            "launchd does not know about the job: {}",
            String::from_utf8_lossy(&loaded.stderr).trim()
        );

        // A changed schedule must actually reach launchd, not be swallowed
        // because the label was already loaded.
        cfg.frequency = "weekly".into();
        cfg.weekday = 5;
        cfg.hour = 6;
        install_agent(&cfg).expect("reinstall with a new schedule");
        let plist = fs::read_to_string(&path).unwrap();
        assert!(plist.contains("<key>Weekday</key><integer>5</integer>"), "{plist}");
        let reloaded = std::process::Command::new("launchctl")
            .args(["print", &format!("{}/{}", gui_domain(), AGENT_LABEL)])
            .output()
            .unwrap();
        assert!(reloaded.status.success(), "job vanished after reinstall");

        remove_agent().expect("remove");
        assert!(!path.exists(), "plist survived removal");
        assert!(!agent_is_current());
    }
}
