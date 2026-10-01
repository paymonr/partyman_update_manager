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
    "asdf",
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
    /// Run a check as soon as the app opens, rather than waiting for the interval.
    pub check_on_launch: bool,
    /// Show brew formulae, npm, pip, rbenv and rvm in the app and include them
    /// in scheduled runs. Off by default: most people never touch them, and the
    /// formulae check alone can take a minute.
    pub show_dev_tools: bool,
    /// Items set aside until something newer turns up. See apply_ignores.
    pub ignored: Vec<IgnoredItem>,
    /// Homebrew's own version against its latest release, from the last run.
    pub brew_self: BrewSelf,
}

/// Whether Homebrew itself is current. Checked with every run and shown as a
/// banner when a newer release is out; it never counts as an update.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BrewSelf {
    /// "7.0.7" — the release the installed Homebrew is based on. Empty when
    /// Homebrew is not installed.
    pub installed: String,
    /// The latest release on GitHub, or empty when that could not be fetched.
    pub latest: String,
    pub checked: u64,
    pub outdated: bool,
}

/// An item the user has chosen not to count for now: an app they update some
/// other way, or a version they are skipping. Ignoring is per version, not per
/// app — the next version brings the item back.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IgnoredItem {
    pub section: String,
    pub id: String,
    pub name: String,
    /// The version that was available when it was ignored. Empty when the
    /// source reports no version, in which case the ignore holds until lifted.
    pub available: String,
    pub since: u64,
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
            check_on_launch: false,
            show_dev_tools: false,
            ignored: Vec::new(),
            brew_self: BrewSelf::default(),
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

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<String>,
    /// "restart" when installing it restarts the Mac.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The app bundle on disk ("/Applications/Brave Browser.app"), when the
    /// check found it. Used to see whether the app is running before it is
    /// replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_path: Option<String>,
    /// Set aside by the user; listed, but not counted or preselected.
    pub ignored: bool,
}

pub fn apply_ignores(section: &str, items: &mut [CheckItem], ignored: &[IgnoredItem]) {
    for it in items.iter_mut() {
        it.ignored = ignored.iter().any(|g| {
            g.section == section
                && g.id == it.id
                && (g.available.is_empty() || it.available.as_deref().unwrap_or("") == g.available)
        });
    }
}

/// The items a check found, with the user's ignores applied.
pub fn items_for(app: &AppHandle, section: &str, lines: &[String]) -> Vec<CheckItem> {
    let mut items = parse_items(section, lines);
    apply_ignores(section, &mut items, &load(app).ignored);
    items
}

/// count_for, less whatever the user has set aside.
pub fn count_with(section: &str, lines: &[String], ignored: &[IgnoredItem]) -> usize {
    match section {
        "brew_casks" | "app_store" | "macos_updates" | "untracked_apps" => {
            let mut items = parse_items(section, lines);
            apply_ignores(section, &mut items, ignored);
            items.iter().filter(|i| !i.ignored).count()
        }
        _ => count_for(section, lines),
    }
}

/// One item as a check script reports it: a tab-separated line the UI never
/// shows, carrying the id to act on, the name to show, and the versions.
pub const ITEM_LINE: &str = "__PM_ITEM__\t";

fn parse_item_line(line: &str) -> Option<CheckItem> {
    let rest = line.strip_prefix(ITEM_LINE)?;
    let mut f = rest.split('\t');
    let id = f.next()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let name = f.next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&id).to_string();
    let opt = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let installed = opt(f.next());
    let available = opt(f.next());
    let note = opt(f.next());
    let app_path = opt(f.next());
    Some(CheckItem { id, name, app_dir: None, installed, available, note, app_path, ignored: false })
}

// The one place a check's output is turned into items: the menu-bar count, the
// checklist the app shows and the history's names all come from here. Scripts
// report each item on an ITEM_LINE; output written before those existed (an
// older last_check.json) falls back to the human-readable lines.
pub fn parse_items(section: &str, lines: &[String]) -> Vec<CheckItem> {
    let structured: Vec<CheckItem> = lines.iter().filter_map(|l| parse_item_line(l)).collect();
    if !structured.is_empty() {
        return structured;
    }
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
                        ..Default::default()
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
                    ..Default::default()
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
                            ..Default::default()
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
                        ..Default::default()
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

// What the update count reports: apps and system updates, nothing else.
// Developer tooling (brew formulae, npm, pip, gems) never counts. It runs to
// hundreds of packages and would bury the handful of updates worth acting on.
// It is still checked, and still shown on its own tab.
pub const BADGE_SECTIONS: &[&str] = &["macos_updates", "app_store", "brew_casks"];

pub fn total_from_counts(counts: &BTreeMap<String, usize>) -> usize {
    counts
        .iter()
        .filter(|(section, _)| BADGE_SECTIONS.contains(&section.as_str()))
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
    /// When each section was checked. A manual run leaves developer tooling
    /// alone, so those sections can be older than `ts`.
    pub section_ts: BTreeMap<String, u64>,
}

fn last_check_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("last_check.json"))
}

pub fn load_last_check(app: &AppHandle) -> LastCheck {
    let mut last: LastCheck = last_check_path(app)
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Files written before per-section times existed checked everything at once.
    for id in last.sections.keys() {
        last.section_ts.entry(id.clone()).or_insert(last.ts);
    }
    last
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

    // Real output from a machine where Homebrew had disabled two outdated casks.
    // `brew upgrade` refuses those, so counting them meant a number that could
    // never reach zero.
    #[test]
    fn only_updatable_casks_are_counted() {
        let lines: Vec<String> = [
            "→  Refreshing Homebrew…",
            "⚠  Outdated apps:",
            "   claude",
            "   claude-code@latest",
            "   visual-studio-code",
            "→  Homebrew has disabled these, so it can no longer update them. They are not counted:",
            "   electron (disabled 2026-09-01)",
            "   flameshot (disabled 2026-09-01)",
            // An app installed twice is a warning, not an extra update.
            "→  Installed twice, from the App Store and by Homebrew. Each copy is checked and counted on its own:",
            "   Microsoft Word: App Store, and Homebrew cask microsoft-word",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let ids: Vec<_> = parse_items("brew_casks", &lines).into_iter().map(|i| i.id).collect();
        assert_eq!(ids, ["claude", "claude-code@latest", "visual-studio-code"]);
        assert_eq!(count_for("brew_casks", &lines), 3);
    }

    #[test]
    fn item_lines_carry_names_and_versions_and_win_over_the_prose() {
        let lines: Vec<String> = [
            "⚠  Outdated apps:",
            "__PM_ITEM__\tdocker-desktop\tDocker Desktop\t4.86.0,236216\t4.93.0,240920\t",
            "   Docker Desktop  4.86.0 → 4.93.0  (docker-desktop)",
            "__PM_ITEM__\tmacOS Sequoia 15.1-24B83\tmacOS Sequoia 15.1\t\t15.1\trestart",
        ].iter().map(|s| s.to_string()).collect();
        let items = parse_items("brew_casks", &lines);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "docker-desktop");
        assert_eq!(items[0].name, "Docker Desktop");
        assert_eq!(items[0].installed.as_deref(), Some("4.86.0,236216"));
        assert_eq!(items[0].available.as_deref(), Some("4.93.0,240920"));
        assert_eq!(items[1].installed, None);
        assert_eq!(items[1].note.as_deref(), Some("restart"));
        assert_eq!(count_for("brew_casks", &lines), 2);
    }

    #[test]
    fn an_ignore_holds_for_one_version_only() {
        let lines: Vec<String> = [
            "__PM_ITEM__\tclaude\tClaude\t2.9939.2\t2.16120.0\t",
            "__PM_ITEM__\tslack\tSlack\t4.51\t4.52\t",
        ].iter().map(|s| s.to_string()).collect();
        let ignore = |available: &str| vec![IgnoredItem {
            section: "brew_casks".into(), id: "claude".into(), name: "Claude".into(),
            available: available.into(), since: 0,
        }];
        // Ignored at this version: not counted.
        assert_eq!(count_with("brew_casks", &lines, &ignore("2.16120.0")), 1);
        // A newer version turned up: it counts again.
        assert_eq!(count_with("brew_casks", &lines, &ignore("2.15000.0")), 2);
        // No version recorded: the ignore holds until lifted.
        assert_eq!(count_with("brew_casks", &lines, &ignore("")), 1);
        // Another section's ignore is not this section's.
        let mut other = ignore("2.16120.0"); other[0].section = "app_store".into();
        assert_eq!(count_with("brew_casks", &lines, &other), 2);
        let mut items = parse_items("brew_casks", &lines);
        apply_ignores("brew_casks", &mut items, &ignore("2.16120.0"));
        assert!(items[0].ignored && !items[1].ignored);
    }

    #[test]
    fn a_manual_check_leaves_developer_tooling_out() {
        let apps: Vec<_> = sections_for(CheckScope::AppsOnly).collect();
        assert_eq!(apps, ["macos_updates", "app_store", "brew_casks"]);
        let all: Vec<_> = sections_for(CheckScope::All).collect();
        assert_eq!(all, CHECKED_SECTIONS);
        // Every checked section is either an app section or developer tooling.
        for s in CHECKED_SECTIONS {
            assert!(BADGE_SECTIONS.contains(s) != DEV_SECTIONS.contains(s), "{s}");
        }
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
        assert_eq!(total_from_counts(&counts), 6);
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

// Developer tooling: brew formulae, npm, pip and gems.
pub const DEV_SECTIONS: &[&str] = &[
    "brew_formulae",
    "npm_globals",
    "pip_packages",
    "ruby_rvm",
    "ruby_rbenv",
    "asdf",
];

/// Which sections a run covers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CheckScope {
    /// Everything in CHECKED_SECTIONS: the scheduled run.
    All,
    /// Apps and system updates only: Run Now, the menu bar's Check Now, and the
    /// check at launch. Developer tooling is slow to check and runs to hundreds
    /// of packages, so it is checked from its own tab or on the schedule.
    AppsOnly,
}

pub fn sections_for(scope: CheckScope) -> impl Iterator<Item = &'static str> {
    CHECKED_SECTIONS
        .iter()
        .copied()
        .filter(move |s| scope == CheckScope::All || !DEV_SECTIONS.contains(s))
}

/// Runs the counted sections in `scope`, records what it found, and returns the
/// new config. Sections outside the scope keep what the last run found for them.
/// Untracked apps are never included: that scan runs `codesign` over every app
/// in /Applications, which is far too slow for a background job, and it is not
/// an update count anyway.
pub async fn run_checks(app: &AppHandle, scope: CheckScope) -> ScheduleConfig {
    let now = now_secs();
    let started = std::time::Instant::now();
    crate::diag(app, &format!(
        "check run: {}",
        if scope == CheckScope::All { "everything (scheduled)" } else { "apps and system updates" },
    ));
    // Developer tools hidden in Settings are not worth a scheduled check either:
    // nothing shows their result, and the formulae check is the slowest of all.
    let cfg0 = load(app);
    let show_dev = cfg0.show_dev_tools;
    let (mut counts, mut last) = match scope {
        CheckScope::All if show_dev => (BTreeMap::new(), LastCheck::default()),
        _ => (load(app).last_counts, load_last_check(app)),
    };
    last.ts = now;
    crate::tray_checking(app, true);
    for section in sections_for(scope).filter(|s| show_dev || !DEV_SECTIONS.contains(s)) {
        let section_started = std::time::Instant::now();
        let lines = crate::run_check_collect(section).await;
        let n = count_with(section, &lines, &cfg0.ignored);
        crate::diag(app, &format!(
            "check {section}: {n} outdated, {} lines, {}s",
            lines.len(),
            section_started.elapsed().as_secs(),
        ));
        counts.insert(section.to_string(), n);
        crate::emit_section_result(app, section, &lines);
        last.sections.insert(section.to_string(), lines);
        last.section_ts.insert(section.to_string(), now);
    }
    save_last_check(app, &last);
    let mut cfg = load(app);
    // Homebrew itself, last: the checks above let it bring itself up to date,
    // so this records where that left it.
    cfg.brew_self = crate::check_brew_self(app, &cfg.brew_self).await;
    crate::tray_checking(app, false);
    cfg.last_run = now;
    cfg.last_total = total_from_counts(&counts);
    cfg.last_counts = counts;
    let _ = save(app, &cfg);
    crate::diag(app, &format!("check run: {} updates, {}s", cfg.last_total, started.elapsed().as_secs()));
    cfg
}

pub fn notify_result(app: &AppHandle, cfg: &ScheduleConfig) {
    if !cfg.notify || cfg.last_total == 0 || cfg.snoozed() {
        return;
    }
    crate::diag(app, &format!("notification: {} updates", cfg.last_total));
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
    run_checks(app, CheckScope::All).await
}

/// Re-checks a single section after it has been upgraded, so the count reflects
/// what is now installed rather than what was outstanding before. Cheaper than a
/// full run, which is why it can happen after every install.
/// Re-reads Homebrew's own version, after Update Homebrew has run.
pub async fn refresh_brew_self(app: &AppHandle) -> ScheduleConfig {
    let mut cfg = load(app);
    cfg.brew_self = crate::check_brew_self(app, &cfg.brew_self).await;
    let _ = save(app, &cfg);
    cfg
}

/// Records what one section's check found: its count, the total, and the output
/// the app preloads next time. Used after an upgrade and whenever the user runs a
/// check, so Check All and Run Check move the count as well as a scheduled run.
pub fn record_section(app: &AppHandle, section: &str, lines: Vec<String>) -> ScheduleConfig {
    let mut cfg = load(app);
    if !CHECKED_SECTIONS.contains(&section) {
        return cfg;
    }
    cfg.last_counts
        .insert(section.to_string(), count_with(section, &lines, &cfg.ignored));
    cfg.last_total = total_from_counts(&cfg.last_counts);
    let _ = save(app, &cfg);

    // Keep the preloaded view in step, or reopening the app would show the
    // section's earlier contents.
    let mut last = load_last_check(app);
    if last.ts == 0 {
        last.ts = now_secs();
    }
    last.section_ts.insert(section.to_string(), now_secs());
    last.sections.insert(section.to_string(), lines);
    save_last_check(app, &last);

    cfg
}

/// Re-derives one section's count from the output the last check stored, for
/// when what counts has changed (an ignore) but nothing has been re-checked.
pub fn recount_stored(app: &AppHandle, section: &str) -> ScheduleConfig {
    let mut cfg = load(app);
    if let Some(lines) = load_last_check(app).sections.get(section) {
        cfg.last_counts.insert(section.to_string(), count_with(section, lines, &cfg.ignored));
        cfg.last_total = total_from_counts(&cfg.last_counts);
        let _ = save(app, &cfg);
    }
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
