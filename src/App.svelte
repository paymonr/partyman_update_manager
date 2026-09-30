<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getVersion } from "@tauri-apps/api/app";
  import { check as checkUpdate } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
  import { enable as enableAutostart, disable as disableAutostart, isEnabled as isAutostartEnabled } from "@tauri-apps/plugin-autostart";
  import { onMount, afterUpdate } from "svelte";
  import iconUrl from "./assets/icon.png";

  type Status = "idle" | "running" | "done" | "error";

  interface Section {
    id: string;
    label: string;
    description: string;
    upgradeCmd?: string;
    platform: "all" | "mac" | "linux" | "windows";
    dev?: boolean;
  }

  interface CheckItem {
    id: string;
    name: string;
    appDir?: string;
  }

  interface CaskCandidate {
    token: string;
    name: string;
    /** Confirmed to install this exact app, rather than merely a similar name. */
    exact: boolean;
  }

  interface CaskSearchState {
    status: "searching" | "found" | "none";
    candidates: CaskCandidate[];
  }

  interface HistoryEntry {
    ts: number;
    section: string;
    label: string;
    items: string[];
    item_names: string[];
    lines: string[];
  }

  const sections: Section[] = [
    {
      id: "macos_updates",
      label: "macOS updates",
      description: "System updates from Apple",
      upgradeCmd: "sudo softwareupdate -ia",
      platform: "mac",
    },
    {
      id: "app_store",
      label: "App Store",
      description: "Apps installed from the Mac App Store",
      upgradeCmd: "mas upgrade",
      platform: "mac",
    },
    {
      id: "brew_casks",
      label: "Homebrew apps",
      description: "Apps installed with Homebrew",
      upgradeCmd: "brew upgrade --cask --greedy",
      platform: "mac",
    },
    {
      id: "untracked_apps",
      label: "Apps without auto-updates",
      description: "Apps that nothing keeps up to date",
      platform: "mac",
    },
    {
      id: "brew_formulae",
      label: "Homebrew formulae",
      description: "Command-line tools installed with Homebrew",
      upgradeCmd: "brew upgrade",
      platform: "mac",
      dev: true,
    },
    {
      id: "npm_globals",
      label: "npm",
      description: "Global npm packages",
      upgradeCmd: "npm update -g",
      platform: "all",
      dev: true,
    },
    {
      id: "pip_packages",
      label: "pip",
      description: "Python packages",
      upgradeCmd: "pip3 install --upgrade $(pip3 list --outdated --format=freeze | cut -d= -f1 | tr '\\n' ' ')",
      platform: "all",
      dev: true,
    },
    {
      id: "ruby_rbenv",
      label: "rbenv",
      description: "Ruby versions and gems managed by rbenv",
      platform: "all",
      dev: true,
    },
    {
      id: "ruby_rvm",
      label: "rvm",
      description: "Ruby versions and gems managed by rvm",
      platform: "all",
      dev: true,
    },
  ];

  // Sections that support individual item selection
  const itemSections = new Set(["macos_updates", "brew_casks", "app_store", "untracked_apps"]);

  let statuses: Record<string, Status> = {};
  let upgradeStatuses: Record<string, Status> = {};
  let outputs: Record<string, string[]> = {};
  let upgradeLogs: Record<string, string[]> = {};
  let lastChecked: Record<string, Date | null> = {};
  let parsedItems: Record<string, CheckItem[]> = {};
  let selectedItems: Record<string, string[]> = {};
  let viewMode: Record<string, "readonly" | "select" | "upgrade"> = {};
  let currentPlatform = "mac";
  let runningAll = false;
  let activeTab = "";
  let appVersion = "";
  let historyEntries: HistoryEntry[] = [];
  let historySearch = "";
  let expandedHistoryEntry: number | null = null;
  let caskSearch: Record<string, CaskSearchState> = {};

  const HIDDEN_APPS_KEY = "hiddenUntrackedApps";
  let hiddenApps: string[] = [];

  function loadHiddenApps() {
    try {
      hiddenApps = JSON.parse(localStorage.getItem(HIDDEN_APPS_KEY) ?? "[]");
    } catch {
      hiddenApps = [];
    }
  }

  function hideApp(name: string) {
    if (!hiddenApps.includes(name)) hiddenApps = [...hiddenApps, name];
    localStorage.setItem(HIDDEN_APPS_KEY, JSON.stringify(hiddenApps));
  }

  function unhideAll() {
    hiddenApps = [];
    localStorage.setItem(HIDDEN_APPS_KEY, "[]");
  }


  type View = "updates" | "history" | "settings" | "whatsnew";
  let view: View = "updates";
  let snoozeOpen = false;

  function pick(id: string) {
    activeTab = id;
    view = "updates";
    snoozeOpen = false;
  }

  function go(next: View) {
    view = next;
    snoozeOpen = false;
    if (next === "history") loadHistory();
    if (next === "settings") loadSchedule();
  }

  let whatsNewVersion = "";
  let whatsNewNotes = "";

  const SEEN_VERSION_KEY = "lastSeenVersion";
  const PENDING_NOTES_KEY = "pendingUpdateNotes";

  // Turns the release body into something readable without pulling in a markdown
  // library: headings, bullets and blank lines are all these notes use.
  type NoteSegment = { text: string; strong: boolean };
  type NoteLine = { kind: "heading" | "bullet" | "text"; segments: NoteSegment[] };

  // **bold** is the only inline markup these notes use; splitting on the marker
  // alternates plain and bold, so the asterisks never reach the screen.
  function splitBold(text: string): NoteSegment[] {
    return text
      .split("**")
      .map((part, i) => ({ text: part, strong: i % 2 === 1 }))
      .filter((seg) => seg.text !== "");
  }

  function renderNotes(body: string): NoteLine[] {
    return body
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line !== "")
      .map((line) => {
        if (/^#{1,6}\s/.test(line)) {
          return { kind: "heading" as const, segments: splitBold(line.replace(/^#{1,6}\s*/, "")) };
        }
        if (/^[-*]\s/.test(line)) {
          return { kind: "bullet" as const, segments: splitBold(line.replace(/^[-*]\s*/, "")) };
        }
        return { kind: "text" as const, segments: splitBold(line) };
      });
  }

  function openWhatsNew(version: string, notes: string) {
    whatsNewVersion = version;
    whatsNewNotes = notes;
    view = "whatsnew";
  }

  // Shown once, the first time a version runs. A fresh install has nothing to
  // compare against, so it records the version and stays quiet.
  async function checkWhatsNew() {
    const seen = localStorage.getItem(SEEN_VERSION_KEY);
    localStorage.setItem(SEEN_VERSION_KEY, appVersion);
    if (!seen || seen === appVersion) return;

    // Stashed before the app relaunched into this version; falls back to the
    // network for an update applied some other way.
    let notes = "";
    try {
      const stashed = JSON.parse(localStorage.getItem(PENDING_NOTES_KEY) ?? "null");
      if (stashed?.version === appVersion) notes = stashed.notes ?? "";
    } catch {
      notes = "";
    }
    localStorage.removeItem(PENDING_NOTES_KEY);

    if (!notes) {
      try {
        notes = await invoke<string>("get_release_notes", { version: appVersion });
      } catch (e) {
        console.error("Failed to fetch release notes:", e);
      }
    }
    if (notes.trim()) openWhatsNew(appVersion, notes);
  }

  type ScheduleConfig = {
    enabled: boolean;
    frequency: "hourly" | "daily" | "weekly";
    minute: number;
    hour: number;
    weekday: number;
    notify: boolean;
    lastRun: number;
    lastTotal: number;
    lastCounts: Record<string, number>;
    snoozedUntil: number;
    checkOnLaunch: boolean;
    showDevTools: boolean;
  };

  let schedule: ScheduleConfig = {
    enabled: false, frequency: "daily", minute: 0, hour: 10, weekday: 1, notify: true,
    lastRun: 0, lastTotal: 0, lastCounts: {}, snoozedUntil: 0,
    checkOnLaunch: false, showDevTools: false,
  };
  let scheduleSaving = false;
  let scheduleRunning = false;
  let scheduleError = "";

  const FREQUENCIES = [
    { id: "hourly", label: "Hourly" },
    { id: "daily", label: "Daily" },
    { id: "weekly", label: "Weekly" },
  ] as const;

  const WEEKDAYS = [
    { id: 0, label: "Sunday" },
    { id: 1, label: "Monday" },
    { id: 2, label: "Tuesday" },
    { id: 3, label: "Wednesday" },
    { id: 4, label: "Thursday" },
    { id: 5, label: "Friday" },
    { id: 6, label: "Saturday" },
  ];

  const HOURS = Array.from({ length: 24 }, (_, h) => ({
    id: h,
    label: `${h % 12 === 0 ? 12 : h % 12} ${h < 12 ? "AM" : "PM"}`,
  }));

  function hourLabel(h: number): string {
    return `${h % 12 === 0 ? 12 : h % 12}:`;
  }

  let nextRunAt = 0;

  function pad(n: number): string {
    return String(n).padStart(2, "0");
  }

  async function setMinute(e: Event) {
    const m = Number((e.target as HTMLInputElement).value);
    if (!Number.isFinite(m)) return;
    schedule.minute = Math.min(59, Math.max(0, Math.trunc(m)));
    await saveSchedule();
  }

  async function refreshNextRun() {
    try {
      nextRunAt = await invoke<number>("next_run");
    } catch {
      nextRunAt = 0;
    }
  }

  function sectionLabel(id: string): string {
    return sections.find((s) => s.id === id)?.label ?? id;
  }

  async function loadSchedule() {
    try {
      schedule = await invoke<ScheduleConfig>("get_schedule");
      await refreshNextRun();
    } catch (e) {
      console.error("Failed to read schedule:", e);
    }
  }

  async function toggleDevTools() {
    if (!schedule.showDevTools && sections.find((s) => s.id === activeTab)?.dev) {
      activeTab = appSections[0]?.id ?? "";
    }
    await saveSchedule();
  }

  async function saveSchedule() {
    scheduleSaving = true;
    scheduleError = "";
    try {
      schedule = await invoke<ScheduleConfig>("set_schedule", { config: schedule });
      await refreshNextRun();
    } catch (e) {
      // Installing the background agent can fail; re-read so the controls show
      // what is actually in effect rather than what was clicked.
      scheduleError = String(e);
      await loadSchedule();
    } finally {
      scheduleSaving = false;
    }
  }

  async function runScheduleNow() {
    scheduleRunning = true;
    scheduleError = "";
    try {
      schedule = await invoke<ScheduleConfig>("run_schedule_now");
    } catch (e) {
      scheduleError = String(e);
    } finally {
      scheduleRunning = false;
    }
  }

  // The scheduled run already did this work, so the app opens with each section
  // filled in rather than eight empty tabs and a Check button.
  async function hydrateLastCheck() {
    try {
      const last = await invoke<{
        ts: number;
        sections: Record<string, string[]>;
        sectionTs?: Record<string, number>;
      }>("get_last_check");
      if (!last?.ts) return;
      for (const [id, lines] of Object.entries(last.sections ?? {})) {
        if (!sections.some((sec) => sec.id === id)) continue;
        if (outputs[id]?.length) continue; // anything checked live this session wins
        // A manual run skips developer tooling, so those sections keep their own,
        // older time rather than the time of the run.
        const when = new Date((last.sectionTs?.[id] ?? last.ts) * 1000);
        outputs[id] = lines;
        parsedItems[id] = parseItems(id, lines);
        selectedItems[id] = parsedItems[id].map((i) => i.id);
        viewMode[id] = parsedItems[id].length > 0 ? "select" : "readonly";
        statuses[id] = "done";
        if (!lastChecked[id] || lastChecked[id]!.getTime() < when.getTime()) {
          lastChecked[id] = when;
        }
      }
      outputs = outputs;
      parsedItems = parsedItems;
      selectedItems = selectedItems;
      viewMode = viewMode;
      statuses = statuses;
      lastChecked = lastChecked;
    } catch (e) {
      console.error("Failed to preload the last check:", e);
    }
  }

  function isSnoozed(): boolean {
    return schedule.snoozedUntil * 1000 > Date.now();
  }

  async function snoozeUpdates(hours: number) {
    try {
      schedule = await invoke<ScheduleConfig>("snooze_updates", { hours });
    } catch (e) {
      scheduleError = String(e);
    }
  }

  // Installing is always something the user starts: several of these need an
  // administrator password, so they have to be at the machine to answer it.
  // Sections run one after another so the prompts cannot overlap.
  async function installOutstanding() {
    view = "updates";
    snoozeOpen = false;
    // Only what the count reports. Developer tooling is never installed from
    // here: it is not part of the count, and "Install all" must not run
    // `brew upgrade`, `npm update -g` or pip across hundreds of packages.
    const pending = countedEntries(schedule.lastCounts)
      .map(([id]) => id)
      .filter((id) => sections.some((sec) => sec.id === id && platformVisible(sec)));
    for (const id of pending) {
      activeTab = id;
      // The parsed list already leaves out what cannot be updated (casks
      // Homebrew has disabled), so prefer it to a blanket section upgrade.
      const items = (parsedItems[id] ?? []).map((i) => i.id);
      if (items.length > 0) await runUpgradeItems(id, items);
      else await runUpgrade(id);
    }
  }

  // Apps installed both from the App Store and by Homebrew, as the Homebrew
  // check reports them. Each copy is still checked and counted on its own.
  const TWICE_HEADER = "→  Installed twice";

  function parseInstalledTwice(lines: string[]): string[] {
    const found: string[] = [];
    let inBlock = false;
    for (const line of lines) {
      if (line.startsWith(TWICE_HEADER)) { inBlock = true; continue; }
      if (!inBlock) continue;
      if (line.trim().startsWith("→") || !line.startsWith("   ")) break;
      found.push(line.trim());
    }
    return found;
  }

  $: installedTwice = parseInstalledTwice(outputs["brew_casks"] ?? []);
  let installedTwiceDismissed = false;

  // The per-section counts that make up the total: apps and system updates.
  function countedEntries(counts: Record<string, number>): [string, number][] {
    return Object.entries(counts).filter(
      ([id, n]) => n > 0 && sections.some((sec) => sec.id === id && !sec.dev),
    );
  }

  function formatWhen(ts: number): string {
    return ts ? new Date(ts * 1000).toLocaleString() : "Never";
  }

  function nextRunLabel(): string {
    if (!schedule.enabled) return "Off";
    return nextRunAt ? new Date(nextRunAt * 1000).toLocaleString() : "—";
  }

  sections.forEach((s) => {
    statuses[s.id] = "idle";
    upgradeStatuses[s.id] = "idle";
    outputs[s.id] = [];
    upgradeLogs[s.id] = [];
    lastChecked[s.id] = null;
    parsedItems[s.id] = [];
    selectedItems[s.id] = [];
    viewMode[s.id] = "readonly";
  });

  function saveLastChecked() {
    const serialized: Record<string, string> = {};
    for (const [k, v] of Object.entries(lastChecked)) {
      if (v) serialized[k] = v.toISOString();
    }
    localStorage.setItem("lastChecked", JSON.stringify(serialized));
  }

  function loadLastChecked() {
    try {
      const raw = localStorage.getItem("lastChecked");
      if (!raw) return;
      const parsed = JSON.parse(raw) as Record<string, string>;
      for (const [k, v] of Object.entries(parsed)) {
        if (k in lastChecked) lastChecked[k] = new Date(v);
      }
      lastChecked = lastChecked;
    } catch {}
  }

  function parseItems(section: string, lines: string[]): CheckItem[] {
    if (section === "brew_casks") {
      const items: CheckItem[] = [];
      let inBlock = false;
      for (const line of lines) {
        if (line.includes("Outdated apps:")) { inBlock = true; continue; }
        if (inBlock && line.trim().startsWith("→")) break;
        if (inBlock && line.trim()) {
          const name = line.trim().split(/\s+/)[0];
          if (name) items.push({ id: name, name });
        }
      }
      return items;
    }
    if (section === "app_store") {
      const items: CheckItem[] = [];
      let inBlock = false;
      for (const line of lines) {
        if (line.includes("Outdated App Store apps:")) { inBlock = true; continue; }
        if (inBlock && line.trim().startsWith("→")) break;
        if (inBlock && line.trim()) {
          const parts = line.trim().split(/\s+/);
          const id = parts[0];
          const name = parts.slice(1).join(" ").replace(/\s*\([^)]+\)\s*$/, "").trim();
          if (id && /^\d+$/.test(id)) items.push({ id, name: name || id });
        }
      }
      return items;
    }
    if (section === "macos_updates") {
      const items: CheckItem[] = [];
      for (const line of lines) {
        const m = line.match(/\*\s*Label:\s*(.+)/);
        if (m) {
          const label = m[1].trim();
          items.push({ id: label, name: label });
        }
      }
      return items;
    }
    if (section === "untracked_apps") {
      const items: CheckItem[] = [];
      for (const line of lines) {
        const m = line.match(/⚠\s+(.+?)(\s+\[~\/Applications\])?$/);
        if (m) {
          const name = m[1].trim();
          const appDir = m[2] ? "~/Applications" : undefined;
          items.push({ id: name, name, appDir });
        }
      }
      return items;
    }
    return [];
  }

  onMount(async () => {
    currentPlatform = await invoke<string>("get_platform");
    appVersion = await getVersion();

    const savedAutoCheck = localStorage.getItem("autoCheckUpdates");
    autoCheckUpdates = savedAutoCheck === null ? true : savedAutoCheck === "true";

    try { startOnLogin = await isAutostartEnabled(); } catch (e) { console.error("Failed to read start-on-login state:", e); }

    await checkWhatsNew();

    loadHiddenApps();
    loadLastChecked();
    await loadSchedule();
    await hydrateLastCheck();

    if (autoCheckUpdates) {
      const lastCheck = parseInt(localStorage.getItem("lastUpdateCheck") ?? "0", 10);
      if (Date.now() - lastCheck > 24 * 60 * 60 * 1000) {
        checkAppUpdate();
      }
    }

    const visible = sections.filter(platformVisible);
    if (visible.length > 0) activeTab = visible[0].id;

    await listen<ScheduleConfig>("schedule-updated", ({ payload }) => {
      schedule = payload;
    });

    await listen<{ section: string; line: string }>("check-output", ({ payload }) => {
      outputs[payload.section] = [...outputs[payload.section], payload.line];
      outputs = outputs;
    });

    await listen<{ section: string; status: string }>("check-status", ({ payload }) => {
      statuses[payload.section] = payload.status as Status;
      statuses = statuses;
      if (payload.status === "done" || payload.status === "error") {
        lastChecked[payload.section] = new Date();
        lastChecked = lastChecked;
        saveLastChecked();
        if (itemSections.has(payload.section)) {
          const items = parseItems(payload.section, outputs[payload.section]);
          parsedItems[payload.section] = items;
          parsedItems = parsedItems;
          // Everything starts selected: updating what is outdated is the
          // common case, and unticking one is easier than ticking sixteen.
          selectedItems[payload.section] = items.map((i) => i.id);
          selectedItems = selectedItems;
          viewMode[payload.section] = items.length > 0 ? "select" : "readonly";
          viewMode = viewMode;
        }
      }
    });

    await listen<{ section: string; line: string }>("upgrade-output", ({ payload }) => {
      if (itemSections.has(payload.section)) {
        upgradeLogs[payload.section] = appendLine(upgradeLogs[payload.section], payload.line);
        upgradeLogs = upgradeLogs;
      } else {
        outputs[payload.section] = [...outputs[payload.section], payload.line];
        outputs = outputs;
      }
    });

    await listen<{ section: string; status: string }>("upgrade-status", ({ payload }) => {
      upgradeStatuses[payload.section] = payload.status as Status;
      upgradeStatuses = upgradeStatuses;
      // Re-check what is left so the menu-bar count matches what is now installed
      // instead of what was outstanding before the upgrade ran. Not for the App
      // Store: "updating" there only opens the App Store, so a recount now would
      // record the updates as still outstanding before the user has installed
      // anything. Its count moves on its next check.
      if (payload.status === "done" && payload.section !== "app_store") {
        invoke("recount_section", { section: payload.section }).catch((e) =>
          console.error("Failed to recount after upgrade:", e),
        );
      }
    });
  });

  async function findCask(itemId: string) {
    caskSearch[itemId] = { status: "searching", candidates: [] };
    caskSearch = caskSearch;
    try {
      const results = await invoke<CaskCandidate[]>("search_cask", { appName: itemId });
      caskSearch[itemId] = { status: results.length > 0 ? "found" : "none", candidates: results };
    } catch {
      caskSearch[itemId] = { status: "none", candidates: [] };
    }
    caskSearch = caskSearch;
  }

  async function findAllCasks() {
    await Promise.all(activeParsedItems.map(item => findCask(item.id)));
  }

  async function trackApp(caskToken: string, appDir?: string) {
    upgradeLogs["untracked_apps"] = [];
    upgradeLogs = upgradeLogs;
    viewMode["untracked_apps"] = "upgrade";
    viewMode = viewMode;
    upgradeStatuses["untracked_apps"] = "running";
    upgradeStatuses = upgradeStatuses;
    try {
      await invoke("track_app", { caskToken, appdir: appDir ?? null });
    } catch (e) {
      upgradeLogs["untracked_apps"] = [...(upgradeLogs["untracked_apps"] ?? []), `Error: ${e}`];
      upgradeLogs = upgradeLogs;
      upgradeStatuses["untracked_apps"] = "error";
      upgradeStatuses = upgradeStatuses;
    }
  }

  async function trackAllApps() {
    // Only casks confirmed to install this exact app. brew search matches on name
    // similarity, so an app with no cask at all still returns something — FileZilla
    // returns "filefillet", Google Docs returns "google-trends" — and adopting the
    // first result would install unrelated software. Anything unconfirmed is left
    // for the user to choose deliberately.
    const items = activeParsedItems
      .map(it => ({ it, match: caskSearch[it.id]?.candidates?.find(c => c.exact) }))
      .filter(({ it, match }) => caskSearch[it.id]?.status === "found" && !!match)
      .map(({ it, match }) => ({
        token: match!.token,
        name: it.name,
        appdir: it.appDir ?? null,
      }));
    if (items.length === 0) {
      upgradeLogs["untracked_apps"] = [
        "→  Nothing to enable: none of these apps has a Homebrew cask that installs it.",
        "→  Use Find on an individual app if you know which cask it belongs to.",
      ];
      upgradeLogs = upgradeLogs;
      viewMode["untracked_apps"] = "upgrade";
      viewMode = viewMode;
      upgradeStatuses["untracked_apps"] = "done";
      upgradeStatuses = upgradeStatuses;
      return;
    }
    upgradeLogs["untracked_apps"] = [];
    upgradeLogs = upgradeLogs;
    viewMode["untracked_apps"] = "upgrade";
    viewMode = viewMode;
    upgradeStatuses["untracked_apps"] = "running";
    upgradeStatuses = upgradeStatuses;
    try {
      await invoke("track_apps", { items });
    } catch (e) {
      upgradeLogs["untracked_apps"] = [...(upgradeLogs["untracked_apps"] ?? []), `Error: ${e}`];
      upgradeLogs = upgradeLogs;
      upgradeStatuses["untracked_apps"] = "error";
      upgradeStatuses = upgradeStatuses;
    }
  }

  async function runSection(id: string) {
    outputs[id] = [];
    outputs = outputs;
    parsedItems[id] = [];
    parsedItems = parsedItems;
    selectedItems[id] = [];
    selectedItems = selectedItems;
    viewMode[id] = "readonly";
    viewMode = viewMode;
    if (id === "untracked_apps") { caskSearch = {}; }
    statuses[id] = "running";
    statuses = statuses;
    try {
      await invoke("run_check", { section: id });
    } catch (e) {
      outputs[id] = [...outputs[id], `Error: ${e}`];
      outputs = outputs;
      statuses[id] = "error";
      statuses = statuses;
    }
  }

  // Developer tooling is left out: brew formulae, npm, pip and gems are slow to
  // check and run to hundreds of packages, so they are checked from their own tab.
  async function runAll() {
    runningAll = true;
    const visible = sections.filter(s => platformVisible(s) && !s.dev);
    for (const s of visible) {
      await runSection(s.id);
    }
    runningAll = false;
  }

  // Progress ticks replace one another instead of stacking up, so a large
  // download is a single line that counts upwards rather than forty lines.
  const PROGRESS_PREFIX = "⬇";

  function appendLine(existing: string[], line: string): string[] {
    const last = existing[existing.length - 1];
    if (line.startsWith(PROGRESS_PREFIX) && last?.startsWith(PROGRESS_PREFIX)) {
      return [...existing.slice(0, -1), line];
    }
    return [...existing, line];
  }

  async function runUpgrade(id: string) {
    outputs[id] = [];
    outputs = outputs;
    upgradeStatuses[id] = "running";
    upgradeStatuses = upgradeStatuses;
    try {
      await invoke("run_upgrade", { section: id });
    } catch (e) {
      outputs[id] = [...outputs[id], `Error: ${e}`];
      outputs = outputs;
      upgradeStatuses[id] = "error";
      upgradeStatuses = upgradeStatuses;
    }
  }

  async function loadHistory() {
    historyEntries = await invoke<HistoryEntry[]>("get_upgrade_history");
  }

  async function runUpgradeItems(id: string, items: string[]) {
    const itemNames = (parsedItems[id] ?? [])
      .filter(i => items.includes(i.id))
      .map(i => i.name);
    upgradeLogs[id] = [];
    upgradeLogs = upgradeLogs;
    viewMode[id] = "upgrade";
    viewMode = viewMode;
    upgradeStatuses[id] = "running";
    upgradeStatuses = upgradeStatuses;
    try {
      await invoke("run_upgrade_items", { section: id, items, itemNames });
    } catch (e) {
      outputs[id] = [...outputs[id], `Error: ${e}`];
      outputs = outputs;
      upgradeStatuses[id] = "error";
      upgradeStatuses = upgradeStatuses;
    }
  }

  function toggleItem(sectionId: string, itemId: string, checked: boolean) {
    if (checked) {
      selectedItems[sectionId] = [...(selectedItems[sectionId] || []), itemId];
    } else {
      selectedItems[sectionId] = (selectedItems[sectionId] || []).filter(id => id !== itemId);
    }
    selectedItems = selectedItems;
  }

  function selectAll(sectionId: string) {
    selectedItems[sectionId] = parsedItems[sectionId].map(i => i.id);
    selectedItems = selectedItems;
  }

  function selectNone(sectionId: string) {
    selectedItems[sectionId] = [];
    selectedItems = selectedItems;
  }

  function platformVisible(s: Section) {
    return s.platform === "all" || s.platform === currentPlatform;
  }

  $: appSections = sections.filter(s => platformVisible(s) && !s.dev);
  $: devSections = sections.filter(s => platformVisible(s) && !!s.dev);

  // What each sidebar row shows: how many things are outdated there. A checked
  // list is the freshest source; otherwise the count the last run recorded.
  // null means never checked.
  function computeCounts(
    st: Record<string, Status>,
    items: Record<string, CheckItem[]>,
    recorded: Record<string, number>,
    hidden: string[],
  ): Record<string, number | null> {
    const out: Record<string, number | null> = {};
    for (const s of sections) {
      if (s.id === "untracked_apps") {
        out[s.id] = st[s.id] === "done"
          ? (items[s.id] ?? []).filter((i) => !hidden.includes(i.name)).length
          : null;
      } else if (itemSections.has(s.id) && st[s.id] === "done") {
        out[s.id] = (items[s.id] ?? []).length;
      } else if (s.id in recorded) {
        out[s.id] = recorded[s.id];
      } else {
        out[s.id] = null;
      }
    }
    return out;
  }
  $: counts = computeCounts(statuses, parsedItems, schedule.lastCounts, hiddenApps);
  $: newestCheck = newestOf(appSections.map((s) => lastChecked[s.id]), schedule.lastRun);

  function newestOf(dates: (Date | null | undefined)[], fallbackTs: number): Date | null {
    let best: Date | null = fallbackTs ? new Date(fallbackTs * 1000) : null;
    for (const d of dates) {
      if (d && (!best || d.getTime() > best.getTime())) best = d;
    }
    return best;
  }

  function relTime(d: Date | null): string {
    if (!d) return "";
    const mins = Math.round((Date.now() - d.getTime()) / 60000);
    if (mins < 1) return "just now";
    if (mins < 60) return `${mins} min ago`;
    const hours = Math.round(mins / 60);
    if (hours < 24) return `${hours} hour${hours === 1 ? "" : "s"} ago`;
    const days = Math.round(hours / 24);
    if (days < 14) return `${days} day${days === 1 ? "" : "s"} ago`;
    return d.toLocaleDateString([], { month: "short", day: "numeric" });
  }

  // The one line under a source's name: what state it is in, in plain words.
  function statusLine(
    section: Section,
    status: Status,
    count: number | null,
    checked: Date | null,
  ): string {
    if (status === "running") return "Checking…";
    if (status === "error") return "The check failed. See the output below.";
    if (status !== "done" || count === null) return section.description;
    const when = checked ? ` · checked ${relTime(checked)}` : "";
    if (section.id === "untracked_apps") {
      return count === 0 ? `Everything is managed${when}` : `${count} app${count === 1 ? "" : "s"}${when}`;
    }
    return count === 0 ? `Up to date${when}` : `${count} outdated${when}`;
  }

  $: activeSectionId = activeTab;
  $: activeSection = sections.find(s => s.id === activeSectionId);
  $: activeStatus = (activeSectionId ? statuses[activeSectionId] : "idle") as Status;
  $: activeUpgradeStatus = (activeSectionId ? upgradeStatuses[activeSectionId] : "idle") as Status;
  $: activeLines = activeSectionId ? outputs[activeSectionId] : [] as string[];
  $: activeLastChecked = activeSectionId ? lastChecked[activeSectionId] : null;
  $: activeHasOutdated = activeLines.some((l) => l.includes("⚠"));
  $: activeCount = activeSectionId ? counts[activeSectionId] ?? null : null;
  $: activeParsedItems = activeSectionId ? (parsedItems[activeSectionId] ?? []) : [];
  $: activeSelectedItems = activeSectionId ? (selectedItems[activeSectionId] ?? []) : [];
  $: activeHasItemSelection = !!activeSectionId && itemSections.has(activeSectionId);
  $: activeViewMode = activeSectionId ? (viewMode[activeSectionId] ?? "readonly") : "readonly";
  $: activeUpgradeLines = activeSectionId ? (upgradeLogs[activeSectionId] ?? []) : [] as string[];
  $: activeFoundCount = activeSectionId === "untracked_apps"
    ? activeParsedItems.filter(it => caskSearch[it.id]?.status === "found").length
    : 0;
  $: showSelectView = activeHasItemSelection && activeStatus === "done" && activeParsedItems.length > 0 && activeViewMode === "select";

  $: filteredHistory = historySearch.trim()
    ? historyEntries.filter(e => {
        const q = historySearch.toLowerCase();
        return e.label.toLowerCase().includes(q)
          || e.item_names.some(n => n.toLowerCase().includes(q))
          || e.lines.some(l => l.toLowerCase().includes(q));
      })
    : historyEntries;

  type AppUpdateStatus = "idle" | "checking" | "up-to-date" | "available" | "error";
  interface AppUpdateInfo { version: string; url: string; notes: string; }
  let appUpdateStatus: AppUpdateStatus = "idle";
  let appUpdateInfo: AppUpdateInfo | null = null;
  let autoCheckUpdates = true;
  let startOnLogin = false;

  async function toggleStartOnLogin() {
    // bind:checked has already flipped startOnLogin to the desired value.
    try {
      if (startOnLogin) { await enableAutostart(); } else { await disableAutostart(); }
    } catch (e) {
      console.error("Failed to update start-on-login:", e);
      // Revert the toggle to whatever the OS actually reports.
      try { startOnLogin = await isAutostartEnabled(); } catch {}
    }
  }

  let pendingUpdate: Awaited<ReturnType<typeof checkUpdate>> = null;

  let appUpdateError = "";

  async function checkAppUpdate() {
    if (appUpdateStatus === "checking") return;
    appUpdateStatus = "checking";
    appUpdateError = "";
    try {
      const update = await checkUpdate();
      if (update) {
        appUpdateStatus = "available";
        appUpdateInfo = { version: update.version, url: "", notes: update.body ?? "" };
        pendingUpdate = update;
      } else {
        appUpdateStatus = "up-to-date";
      }
      localStorage.setItem("lastUpdateCheck", Date.now().toString());
    } catch (e: unknown) {
      appUpdateStatus = "error";
      appUpdateError = e instanceof Error ? e.message : JSON.stringify(e) ?? String(e) ?? "Unknown error";
      console.error("Update check failed:", e);
    }
  }

  async function installAppUpdate() {
    if (!pendingUpdate) return;
    // Kept for after the relaunch: the pending update object does not survive it.
    if (appUpdateInfo) {
      localStorage.setItem(
        PENDING_NOTES_KEY,
        JSON.stringify({ version: appUpdateInfo.version, notes: appUpdateInfo.notes }),
      );
    }
    appUpdateStatus = "checking";
    await pendingUpdate.downloadAndInstall();
    await relaunch();
  }

  function openReleaseUrl(url: string) {
    invoke("open_release_url", { url });
  }

  let outputEl: HTMLElement | null = null;

  afterUpdate(() => {
    if (outputEl) outputEl.scrollTop = outputEl.scrollHeight;
  });

  const skipPatterns = [
    /taps are not trusted/,
    /HOMEBREW_REQUIRE_TAP_TRUST/,
    /HOMEBREW_NO_REQUIRE_TAP_TRUST/,
    /Homebrew will ignore/,
    /This will become the default/,
    /Enable trust checks now/,
    /Trust specific formulae/,
    /or trust installed formulae/,
    /You can trust all/,
    /brew trust /,
    /brew untap /,
    /Prefer trusting/,
    /Untap them with/,
    /To keep allowing/,
    /trust --formula/,
    /trust --cask/,
    /trust --command/,
    /aws\/tap|hashicorp\/tap|romkatv\/|weaveworks\/tap/,
    /✔︎ JSON API/,
    /✔︎ API Source/,
    /✔︎ Cask .+\(.+\)/,
    /^==> Purging files/,
    /whichever comes first/,
    /not recommended and will be removed/,
  ];

  // Homebrew's untrusted-tap warning is a long block that it rewords between
  // releases, so matching it line by line keeps going stale — it grew back to
  // roughly two thirds of the output. Skip from the warning until Homebrew
  // starts saying something else instead.
  function dropTapTrustBlock(lines: string[]): string[] {
    const out: string[] = [];
    let skipping = false;
    for (const l of lines) {
      if (/taps are not trusted/i.test(l)) { skipping = true; continue; }
      if (skipping) {
        // Anything that opens a new step, or a real result, ends the block.
        if (/^\s*(==>|🍺|✖|→|⚠|Error:)/.test(l) && !/tap/i.test(l)) skipping = false;
        else continue;
      }
      out.push(l);
    }
    return out;
  }

  function simplifyUntrackedLog(lines: string[]): string[] {
    return dropTapTrustBlock(lines)
      .filter(l => !skipPatterns.some(p => p.test(l)))
      .map((l): string | null => {
        if (/^==> Fetching downloads for:/.test(l))
          return `Downloading ${l.replace(/^==> Fetching downloads for:\s*/, "")}…`;
        if (/^==> Downloading /.test(l)) return "Downloading…";
        if (/^Already downloaded:/.test(l) || /^#{3,}/.test(l)) return null;
        if (/^==> Installing Cask /.test(l))
          return `Installing ${l.replace(/^==> Installing Cask /, "")}…`;
        if (/^Error: It seems there is already an App/.test(l))
          return "✖  Setup failed — close the app and try again.";
        if (/^Warning: It seems there is already an App/.test(l))
          return "Replacing existing app…";
        if (/^==> Removing App/.test(l)) return "Removing old version…";
        if (/^==> Moving App/.test(l) || l.startsWith("🍺")) return null;
        if (/^==> Using sudo/.test(l) || /^sudo:/.test(l) || /^Error: Permission denied/.test(l)) return null;
        return l;
      })
      .filter((l): l is string => l !== null && l.trim() !== "");
  }

  // For a list of entries: the day and the minute are enough to tell them apart.
  function formatShort(d: Date): string {
    const sameYear = d.getFullYear() === new Date().getFullYear();
    return d.toLocaleString([], {
      month: "short", day: "numeric", ...(sameYear ? {} : { year: "numeric" }),
      hour: "numeric", minute: "2-digit",
    });
  }

</script>

<main>
  <!-- The toolbar doubles as the title bar: the window has no native one, so it
       is the drag handle. Buttons inside it still click normally. -->
  <div class="toolbar" data-tauri-drag-region>
    <div class="brand" data-tauri-drag-region>
      <img src={iconUrl} alt="" class="brand-icon" />
      <span class="brand-name"><span class="brand-accent">PartyMAN</span> Update Manager</span>
    </div>
    <div class="toolbar-space" data-tauri-drag-region></div>
  </div>

  {#if appUpdateStatus === "available" && appUpdateInfo}
    <div class="strip strip-ok">
      <span>PartyMAN {appUpdateInfo.version} is ready to install.</span>
      {#if appUpdateInfo.notes?.trim()}
        <button class="btn btn-plain" onclick={() => openWhatsNew(appUpdateInfo!.version, appUpdateInfo!.notes)}>What's new</button>
      {/if}
      <button class="btn btn-primary" onclick={installAppUpdate}>Install and relaunch</button>
      <button class="dismiss" aria-label="Dismiss" onclick={() => { appUpdateStatus = "idle"; appUpdateInfo = null; }}>✕</button>
    </div>
  {/if}

  {#if installedTwice.length > 0 && !installedTwiceDismissed}
    <div class="strip">
      <div>
        <strong>Installed twice.</strong>
        {installedTwice.length === 1 ? "This app is" : "These apps are"} installed from the App Store
        and by Homebrew, so each copy is checked and counted on its own.
        <ul>
          {#each installedTwice as entry}<li>{entry}</li>{/each}
        </ul>
      </div>
      <button class="dismiss" aria-label="Dismiss" onclick={() => { installedTwiceDismissed = true; }}>✕</button>
    </div>
  {/if}

  <div class="split">
      <aside class="sidebar">
        <div class="summary">
          {#if schedule.lastTotal > 0}
            <div class="summary-count">
              <span class="num">{schedule.lastTotal}</span>
              <span class="num-label">update{schedule.lastTotal === 1 ? "" : "s"}</span>
            </div>
          {:else if newestCheck}
            <div class="summary-count ok">
              <span class="tick">✓</span>
              <span class="num-label">Up to date</span>
            </div>
          {:else}
            <div class="summary-count">
              <span class="num-label">Not checked yet</span>
            </div>
          {/if}
          {#if newestCheck}
            <p class="summary-note">{runningAll ? "Checking…" : `Checked ${relTime(newestCheck)}`}</p>
          {/if}
          <div class="summary-actions">
            <button class="btn" onclick={runAll} disabled={runningAll}>{runningAll ? "Checking…" : "Check all"}</button>
            {#if schedule.lastTotal > 0}
              <button class="btn btn-primary" onclick={installOutstanding}>Install all</button>
            {/if}
          </div>
          {#if schedule.lastTotal > 0}
            <div class="snooze">
              <button class="btn btn-plain btn-small" onclick={() => { snoozeOpen = !snoozeOpen; }} aria-expanded={snoozeOpen}>
                {isSnoozed() ? "Reminders snoozed" : "Snooze reminders"}
              </button>
              {#if snoozeOpen}
                <div class="snooze-backdrop" onclick={() => { snoozeOpen = false; }} role="presentation"></div>
                <div class="snooze-menu" role="menu">
                  <span class="snooze-title">Remind me in</span>
                  <button role="menuitem" onclick={() => { snoozeUpdates(1); snoozeOpen = false; }}>1 hour</button>
                  <button role="menuitem" onclick={() => { snoozeUpdates(24); snoozeOpen = false; }}>1 day</button>
                  <button role="menuitem" onclick={() => { snoozeUpdates(72); snoozeOpen = false; }}>3 days</button>
                  {#if isSnoozed()}
                    <button role="menuitem" onclick={() => { snoozeUpdates(0); snoozeOpen = false; }}>Resume reminders</button>
                  {/if}
                </div>
              {/if}
            </div>
          {/if}
        </div>

        <ul class="sources">
          {#each appSections as s (s.id)}
            {@const st = statuses[s.id]}
            {@const n = counts[s.id]}
            <li>
              <button class="source" class:active={view === "updates" && activeTab === s.id} onclick={() => pick(s.id)}>
                <span class="source-name">{s.label}</span>
                {#if st === "running"}
                  <span class="spinner" aria-label="Checking"></span>
                {:else if st === "error"}
                  <span class="count err" title="The check failed">!</span>
                {:else if n === null}
                  <span class="count none">–</span>
                {:else if n === 0}
                  <span class="count zero" title="Up to date">✓</span>
                {:else}
                  <span class="count" class:quiet={s.id === "untracked_apps"}>{n}</span>
                {/if}
              </button>
            </li>
          {/each}
        </ul>

        {#if schedule.showDevTools && devSections.length > 0}
          <p class="group-title">Developer tools</p>
          <ul class="sources">
            {#each devSections as s (s.id)}
              {@const st = statuses[s.id]}
              {@const n = counts[s.id]}
              <li>
                <button class="source" class:active={view === "updates" && activeTab === s.id} onclick={() => pick(s.id)}>
                  <span class="source-name">{s.label}</span>
                  {#if st === "running"}
                    <span class="spinner" aria-label="Checking"></span>
                  {:else if st === "error"}
                    <span class="count err" title="The check failed">!</span>
                  {:else if n === null}
                    <span class="count none">–</span>
                  {:else if n === 0}
                    <span class="count zero" title="Up to date">✓</span>
                  {:else}
                    <span class="count quiet">{n}</span>
                  {/if}
                </button>
              </li>
            {/each}
          </ul>
        {/if}

        <div class="sidebar-foot">
          <button class="source" class:active={view === "history"} onclick={() => go("history")}>History</button>
          <button class="source" class:active={view === "settings" || view === "whatsnew"} onclick={() => go("settings")}>Settings</button>
        </div>
      </aside>

  {#if view === "whatsnew"}
    <div class="page">
      <div class="page-head">
        <h2>What's new in {whatsNewVersion}</h2>
        <button class="btn btn-plain" onclick={() => go("updates")}>Done</button>
      </div>
      <div class="page-body prose">
        {#each renderNotes(whatsNewNotes) as line}
          {#if line.kind === "heading"}
            <h3>{#each line.segments as seg}{seg.text}{/each}</h3>
          {:else}
            <p class={line.kind === "bullet" ? "bullet" : ""}>
              {#each line.segments as seg}{#if seg.strong}<strong>{seg.text}</strong>{:else}{seg.text}{/if}{/each}
            </p>
          {/if}
        {/each}
      </div>
    </div>

  {:else if view === "history"}
    <div class="page">
      <div class="page-head">
        <h2>History <span class="page-sub">last 180 days</span></h2>
        <input class="search" type="search" placeholder="Search" bind:value={historySearch} />
      </div>
      <div class="page-body history">
        {#if filteredHistory.length === 0}
          <p class="empty">{historyEntries.length === 0 ? "Nothing has been updated from here yet." : "Nothing matches that search."}</p>
        {:else}
          {#each filteredHistory as entry, i}
            <div class="history-entry" class:open={expandedHistoryEntry === i}>
              <button class="history-row" onclick={() => expandedHistoryEntry = expandedHistoryEntry === i ? null : i}>
                <span class="history-when">{formatShort(new Date(entry.ts * 1000))}</span>
                <span class="history-what">
                  <span class="history-source">{sectionLabel(entry.section)}</span>
                  {#if entry.item_names.length > 0}
                    {#each entry.item_names as name}<span class="chip">{name}</span>{/each}
                  {:else}
                    <span class="chip chip-quiet">everything</span>
                  {/if}
                </span>
                <span class="history-caret" aria-hidden="true">{expandedHistoryEntry === i ? "▾" : "▸"}</span>
              </button>
              {#if expandedHistoryEntry === i}
                <div class="output history-output">
                  {#each entry.lines as line}<div class="line">{line}</div>{/each}
                </div>
              {/if}
            </div>
          {/each}
        {/if}
      </div>
    </div>

  {:else if view === "settings"}
    <div class="page">
      <div class="page-head"><h2>Settings</h2></div>
      <div class="page-body settings">
        {#if scheduleError}<div class="strip strip-err">{scheduleError}</div>{/if}

        <section>
          <h3>Checking for updates</h3>
          <p class="section-hint">
            Checks run in the background, even when PartyMAN is closed, and only ever look.
            Installing is always something you start.
          </p>
          <label class="row">
            <span class="row-text">
              <span class="row-label">Check automatically</span>
              <span class="row-desc">Found updates show in the menu bar</span>
            </span>
            <input type="checkbox" bind:checked={schedule.enabled} onchange={saveSchedule} disabled={scheduleSaving} />
          </label>
          <div class="row">
            <span class="row-text">
              <span class="row-label">How often</span>
              <span class="row-desc">Next check: {nextRunLabel()}</span>
            </span>
            <select class="select" bind:value={schedule.frequency} onchange={saveSchedule} disabled={!schedule.enabled || scheduleSaving}>
              {#each FREQUENCIES as choice}<option value={choice.id}>{choice.label}</option>{/each}
            </select>
          </div>
          {#if schedule.frequency === "weekly"}
            <div class="row">
              <span class="row-text"><span class="row-label">On</span></span>
              <select class="select" bind:value={schedule.weekday} onchange={saveSchedule} disabled={!schedule.enabled || scheduleSaving}>
                {#each WEEKDAYS as day}<option value={day.id}>{day.label}</option>{/each}
              </select>
            </div>
          {/if}
          <div class="row">
            <span class="row-text">
              <span class="row-label">At</span>
              <span class="row-desc">
                {schedule.frequency === "hourly"
                  ? "This many minutes past every hour"
                  : `Local time, ${hourLabel(schedule.hour)}${pad(schedule.minute)}`}
              </span>
            </span>
            <span class="row-controls">
              {#if schedule.frequency !== "hourly"}
                <select class="select" bind:value={schedule.hour} onchange={saveSchedule} disabled={!schedule.enabled || scheduleSaving}>
                  {#each HOURS as h}<option value={h.id}>{h.label}</option>{/each}
                </select>
              {/if}
              <input class="select minutes" type="number" min="0" max="59" value={schedule.minute} onchange={setMinute}
                disabled={!schedule.enabled || scheduleSaving} aria-label="Minutes past the hour" />
            </span>
          </div>
          <label class="row">
            <span class="row-text">
              <span class="row-label">Check when PartyMAN opens</span>
              <span class="row-desc">Apps and system updates only; developer tools keep to the schedule</span>
            </span>
            <input type="checkbox" bind:checked={schedule.checkOnLaunch} onchange={saveSchedule} disabled={scheduleSaving} />
          </label>
          <div class="row">
            <span class="row-text">
              <span class="row-label">Last check</span>
              <span class="row-desc">
                {#if schedule.lastRun}
                  {formatWhen(schedule.lastRun)} · {schedule.lastTotal} update{schedule.lastTotal === 1 ? "" : "s"}
                  {#if countedEntries(schedule.lastCounts).length > 0}
                    ({countedEntries(schedule.lastCounts).map(([sec, n]) => `${sectionLabel(sec)} ${n}`).join(", ")})
                  {/if}
                {:else}
                  Never
                {/if}
              </span>
            </span>
            <button class="btn" onclick={runScheduleNow} disabled={scheduleRunning}>
              {scheduleRunning ? "Checking…" : "Check now"}
            </button>
          </div>
        </section>

        <section>
          <h3>Notifications</h3>
          <label class="row">
            <span class="row-text">
              <span class="row-label">Notify me when updates are found</span>
              <span class="row-desc">The menu bar shows the count either way</span>
            </span>
            <input type="checkbox" bind:checked={schedule.notify} onchange={saveSchedule} disabled={scheduleSaving} />
          </label>
          {#if isSnoozed()}
            <div class="row">
              <span class="row-text">
                <span class="row-label">Reminders paused</span>
                <span class="row-desc">Until {formatWhen(schedule.snoozedUntil)}</span>
              </span>
              <button class="btn" onclick={() => snoozeUpdates(0)}>Resume</button>
            </div>
          {/if}
        </section>

        <section>
          <h3>Developer tools</h3>
          <label class="row">
            <span class="row-text">
              <span class="row-label">Show developer tools</span>
              <span class="row-desc">Homebrew formulae, npm, pip, rbenv and rvm. They never count towards the total, and are only checked from their own page or by the schedule while shown.</span>
            </span>
            <input type="checkbox" bind:checked={schedule.showDevTools} onchange={toggleDevTools} disabled={scheduleSaving} />
          </label>
        </section>

        <section>
          <h3>Start-up</h3>
          <label class="row">
            <span class="row-text">
              <span class="row-label">Open at login</span>
              <span class="row-desc">Keeps the menu bar icon available for scheduled checks</span>
            </span>
            <input type="checkbox" bind:checked={startOnLogin} onchange={toggleStartOnLogin} />
          </label>
        </section>

        <section>
          <h3>PartyMAN itself</h3>
          <div class="row">
            <span class="row-text">
              <span class="row-label">Version {appVersion}</span>
              <span class="row-desc">
                {#if appUpdateStatus === "error" && appUpdateError}Couldn't check: {appUpdateError}
                {:else if appUpdateStatus === "up-to-date"}This is the latest version
                {:else if appUpdateStatus === "available" && appUpdateInfo}{appUpdateInfo.version} is available
                {:else}Checks GitHub for a newer version{/if}
              </span>
            </span>
            <button class="btn" class:btn-primary={appUpdateStatus === "available"}
              onclick={() => { if (appUpdateStatus === "available") { installAppUpdate(); } else { checkAppUpdate(); } }}
              disabled={appUpdateStatus === "checking"}>
              {#if appUpdateStatus === "checking"}Checking…
              {:else if appUpdateStatus === "available"}Install update
              {:else if appUpdateStatus === "error"}Try again
              {:else}Check for updates{/if}
            </button>
          </div>
          <label class="row">
            <span class="row-text">
              <span class="row-label">Check for a new PartyMAN daily</span>
              <span class="row-desc">Once a day, when the app opens</span>
            </span>
            <input type="checkbox" bind:checked={autoCheckUpdates}
              onchange={() => localStorage.setItem("autoCheckUpdates", String(autoCheckUpdates))} />
          </label>
          <div class="about">
            <img src={iconUrl} alt="" class="about-icon" />
            <div>
              <p><strong>PartyMAN Update Manager</strong> keeps one Mac's software current from one place: Apple, the App Store, Homebrew and the usual developer tools.</p>
              <p class="about-links">
                <button class="link" onclick={() => openReleaseUrl("https://github.com/paymonr/partyman_update_manager/releases")}>Release notes</button>
                <button class="link" onclick={() => openReleaseUrl("https://github.com/paymonr/partyman_update_manager")}>Source on GitHub</button>
              </p>
              <p class="about-fine">Apache License 2.0.</p>
            </div>
          </div>
        </section>
      </div>
    </div>

  {:else if activeSection}
        <section class="content">
          <header class="source-head">
            <div class="source-title">
              <h2>{activeSection.label}</h2>
              <p class="source-status" class:err={activeStatus === "error"}>
                {statusLine(activeSection, activeStatus, activeCount, activeLastChecked)}
              </p>
            </div>
            <div class="source-actions">
              {#if activeHasItemSelection && activeStatus === "done" && activeParsedItems.length > 0}
                <div class="segmented" role="tablist" aria-label="View">
                  <button class:active={activeViewMode === "select"} onclick={() => { viewMode[activeSectionId] = "select"; viewMode = viewMode; }}>List</button>
                  <button class:active={activeViewMode === "readonly"} onclick={() => { viewMode[activeSectionId] = "readonly"; viewMode = viewMode; }}>Output</button>
                  <button class:active={activeViewMode === "upgrade"} onclick={() => { viewMode[activeSectionId] = "upgrade"; viewMode = viewMode; }}>Log</button>
                </div>
              {/if}
              <button class="btn" onclick={() => runSection(activeSectionId)}
                disabled={activeStatus === "running" || activeUpgradeStatus === "running"}>
                {activeStatus === "running" ? "Checking…" : activeStatus === "done" ? "Check again" : "Check now"}
              </button>
            </div>
          </header>

          {#if showSelectView}
            {#if activeSectionId === "untracked_apps"}
              {@const shown = activeParsedItems.filter(i => !hiddenApps.includes(i.name))}
              <div class="list-bar">
                <span class="list-count">{shown.length} app{shown.length === 1 ? "" : "s"}</span>
                <button class="btn btn-small" onclick={findAllCasks}>Look up all</button>
                {#if activeFoundCount > 0}
                  <button class="btn btn-small btn-primary" onclick={trackAllApps} disabled={activeUpgradeStatus === "running"}>
                    {activeUpgradeStatus === "running" ? "Enabling…" : `Enable auto-updates for ${activeFoundCount}`}
                  </button>
                {/if}
                {#if hiddenApps.length > 0}
                  <button class="btn btn-small btn-plain" onclick={unhideAll}>Show {hiddenApps.length} hidden</button>
                {/if}
              </div>
              <div class="list">
                {#each shown as item}
                  <div class="item item-untracked">
                    <span class="item-name">{item.name}</span>
                    <span class="item-side">
                      {#if !caskSearch[item.id]}
                        <button class="btn btn-small" onclick={() => findCask(item.id)}>Look up</button>
                      {:else if caskSearch[item.id].status === "searching"}
                        <span class="item-note">Looking up…</span>
                      {:else if caskSearch[item.id].status === "none"}
                        <span class="item-note">Homebrew doesn't carry this</span>
                        <button class="btn btn-small btn-plain" onclick={() => hideApp(item.name)}>Hide</button>
                      {:else if caskSearch[item.id].status === "found"}
                        {@const confirmed = caskSearch[item.id].candidates.find(c => c.exact)}
                        {#if confirmed}
                          <span class="item-note ok">{confirmed.token}</span>
                          <button class="btn btn-small btn-primary" onclick={() => trackApp(confirmed.token, item.appDir)}
                            disabled={activeUpgradeStatus === "running"}>Enable auto-updates</button>
                        {:else}
                          <span class="item-note">No matching cask</span>
                          <button class="btn btn-small btn-plain" onclick={() => hideApp(item.name)}>Hide</button>
                        {/if}
                      {/if}
                    </span>
                  </div>
                {/each}
                {#if shown.length === 0}
                  <p class="empty">Every app here is managed by something.</p>
                {/if}
              </div>
              <footer class="source-foot">
                <span class="foot-note">
                  Look up an app to see whether Homebrew carries it. Hidden apps are kept out of the list and the count.
                </span>
              </footer>
            {:else}
              <div class="list">
                {#each activeParsedItems as item}
                  <label class="item">
                    <input type="checkbox" checked={activeSelectedItems.includes(item.id)}
                      onchange={(e) => toggleItem(activeSectionId, item.id, (e.target as HTMLInputElement).checked)} />
                    <span class="item-name">{item.name}</span>
                  </label>
                {/each}
              </div>
              <footer class="source-foot">
                <button class="btn btn-primary" onclick={() => runUpgradeItems(activeSectionId, activeSelectedItems)}
                  disabled={activeUpgradeStatus === "running" || activeSelectedItems.length === 0}>
                  {#if activeUpgradeStatus === "running"}Updating…
                  {:else if activeSectionId === "app_store"}Open App Store to update
                  {:else}Update {activeSelectedItems.length} selected{/if}
                </button>
                <button class="btn btn-plain" onclick={() => selectAll(activeSectionId)}
                  disabled={activeSelectedItems.length === activeParsedItems.length}>Select all</button>
                <button class="btn btn-plain" onclick={() => selectNone(activeSectionId)}
                  disabled={activeSelectedItems.length === 0}>Deselect all</button>
                {#if activeSectionId !== "app_store"}
                  <span class="foot-note">You'll be asked for your password once, if it's needed.</span>
                {/if}
              </footer>
            {/if}
          {:else if activeViewMode === "upgrade"}
            <div class="output" bind:this={outputEl}>
              {#if activeUpgradeLines.length === 0}
                <p class="empty">Nothing has been updated here yet.</p>
              {:else}
                {@const displayLines = activeSectionId === "untracked_apps" ? simplifyUntrackedLog(activeUpgradeLines) : activeUpgradeLines}
                {#each displayLines as line}<div class="line" class:ok={line.startsWith("✔") || line.startsWith("🍺")} class:warn={line.startsWith("⚠")} class:bad={line.startsWith("✖") || line.startsWith("Error")}>{line}</div>{/each}
              {/if}
            </div>
          {:else}
            <div class="output" bind:this={outputEl}>
              {#if activeLines.length === 0}
                {#if activeStatus === "running"}
                  <p class="empty">Checking…</p>
                {:else}
                  <p class="empty">
                    Not checked yet.
                    {#if activeSection.dev}These aren't part of the update count; check them whenever you like.
                    {:else}Check now to see what's out of date. Nothing is installed until you choose to.{/if}
                  </p>
                {/if}
              {:else}
                {#each activeLines as line}<div class="line" class:ok={line.startsWith("✔")} class:warn={line.startsWith("⚠")} class:bad={line.startsWith("✖")}>{line}</div>{/each}
              {/if}
            </div>
            {#if activeSection.dev && activeSection.upgradeCmd && activeStatus === "done" && activeHasOutdated}
              <footer class="source-foot">
                <button class="btn btn-primary" onclick={() => runUpgrade(activeSectionId)} disabled={activeUpgradeStatus === "running"}>
                  {activeUpgradeStatus === "running" ? "Updating…" : "Update all"}
                </button>
                <code class="cmd">{activeSection.upgradeCmd}</code>
              </footer>
            {:else if !activeSection.dev && activeSection.upgradeCmd && activeStatus === "done" && activeHasOutdated && !activeHasItemSelection}
              <footer class="source-foot">
                <button class="btn btn-primary" onclick={() => runUpgrade(activeSectionId)} disabled={activeUpgradeStatus === "running"}>
                  {activeUpgradeStatus === "running" ? "Updating…" : "Update all"}
                </button>
              </footer>
            {/if}
          {/if}
        </section>
  {/if}
  </div>
</main>

<style>
  :global(*, *::before, *::after) { box-sizing: border-box; margin: 0; padding: 0; }
  :global(html), :global(body) { height: 100%; overflow: hidden; }
  :global(body) {
    font-family: var(--pm-font);
    font-size: 13px;
    line-height: 1.45;
    background: var(--pm-surface);
    color: var(--pm-text-2);
    -webkit-font-smoothing: antialiased;
  }
  :global(button), :global(input), :global(select) { font: inherit; color: inherit; }
  :global(:focus-visible) { outline: 2px solid var(--pm-accent); outline-offset: 2px; }
  :global(button:focus:not(:focus-visible)) { outline: none; }

  main { height: 100vh; display: flex; flex-direction: column; overflow: hidden; }

  /* ── Toolbar ─────────────────────────────────────────────────────────── */
  .toolbar {
    display: flex;
    align-items: center;
    gap: 12px;
    /* The standard unified-toolbar height; the traffic lights are moved to sit
       centred in it (trafficLightPosition in tauri.conf.json). */
    height: 52px;
    flex-shrink: 0;
    /* Room for the traffic lights, which macOS draws over this strip. */
    padding: 0 14px 0 86px;
    border-bottom: 1px solid var(--pm-border);
    background: var(--pm-surface-2);
    user-select: none;
  }
  .toolbar-space { flex: 1; align-self: stretch; }
  .brand { display: flex; align-items: center; gap: 10px; }
  .brand-icon { width: 28px; height: 28px; border-radius: 7px; }
  .brand-name { font-size: 15px; font-weight: 600; color: var(--pm-text-bright); letter-spacing: -0.01em; }
  .brand-accent { color: var(--pm-accent); }

  /* ── Buttons ─────────────────────────────────────────────────────────── */
  .btn {
    display: inline-flex; align-items: center; gap: 6px;
    padding: 5px 12px;
    border-radius: var(--pm-radius);
    border: 1px solid var(--pm-border-strong);
    background: var(--pm-card);
    color: var(--pm-text);
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
    transition: background 0.12s, border-color 0.12s, color 0.12s;
  }
  .btn:hover:not(:disabled) { background: var(--pm-hover); }
  .btn:disabled { opacity: 0.45; cursor: default; }
  .btn-primary { background: var(--pm-accent); border-color: transparent; color: var(--pm-on-accent); font-weight: 600; }
  .btn-primary:hover:not(:disabled) { background: var(--pm-accent-hover); }
  .btn-plain { background: transparent; border-color: transparent; color: var(--pm-muted); }
  .btn-plain:hover:not(:disabled) { color: var(--pm-text); background: var(--pm-hover); }
  .btn-small { padding: 2px 9px; font-size: 12px; }
  .link { background: none; border: none; padding: 0; color: var(--pm-accent); cursor: pointer; text-decoration: underline; text-underline-offset: 2px; }
  .dismiss { background: none; border: none; color: var(--pm-muted); cursor: pointer; margin-left: auto; padding: 2px 6px; border-radius: var(--pm-radius-sm); }
  .dismiss:hover { color: var(--pm-text); background: var(--pm-hover); }

  /* ── Strips (app update, installed twice, errors) ────────────────────── */
  .strip {
    display: flex; align-items: center; gap: 10px; flex-wrap: wrap;
    margin: 10px 14px 0;
    padding: 8px 12px;
    border-radius: var(--pm-radius);
    background: var(--pm-info-tint);
    border: 1px solid var(--pm-border);
  }
  .strip ul { margin: 4px 0 0 18px; }
  .strip-ok { background: var(--pm-ok-bg); border-color: var(--pm-ok-border); }
  .strip-err { background: var(--pm-err-bg); border-color: var(--pm-err-border); }

  /* ── Split: sidebar + content ────────────────────────────────────────── */
  .split { flex: 1; min-height: 0; display: flex; }

  .sidebar {
    width: 246px; flex-shrink: 0;
    display: flex; flex-direction: column;
    overflow-y: auto;
    padding: 12px 8px;
    border-right: 1px solid var(--pm-border);
    background: var(--pm-surface-2);
  }
  .summary { padding: 4px 8px 14px; margin-bottom: 6px; border-bottom: 1px solid var(--pm-border); }
  .summary-count { display: flex; align-items: baseline; gap: 6px; color: var(--pm-text-bright); }
  .num { font-size: 26px; font-weight: 600; letter-spacing: -0.02em; font-variant-numeric: tabular-nums; line-height: 1.1; }
  .num-label { font-size: 14px; font-weight: 500; color: var(--pm-text-2); }
  .summary-count.ok { align-items: center; }
  .tick { color: var(--pm-ok); font-size: 18px; font-weight: 700; }
  .summary-actions { display: flex; align-items: center; gap: 6px; margin-top: 10px; }
  .summary-note { color: var(--pm-muted); margin-top: 2px; font-size: 12px; }

  .snooze { position: relative; margin: 6px 0 0 -9px; }
  .snooze-backdrop { position: fixed; inset: 0; z-index: 10; }
  .snooze-menu {
    position: absolute; top: calc(100% + 4px); left: 0; z-index: 20;
    display: flex; flex-direction: column; min-width: 160px;
    padding: 4px; border-radius: var(--pm-radius);
    background: var(--pm-card); border: 1px solid var(--pm-border-strong);
    box-shadow: 0 8px 24px var(--pm-shadow);
  }
  .snooze-title { padding: 4px 10px 2px; font-size: 11px; color: var(--pm-muted); }
  .snooze-menu button { text-align: left; border: none; background: none; padding: 5px 10px; border-radius: var(--pm-radius-sm); cursor: pointer; }
  .snooze-menu button:hover { background: var(--pm-hover); }

  .group-title { margin: 14px 8px 4px; font-size: 11.5px; font-weight: 600; color: var(--pm-muted); }
  .sidebar-foot {
    margin-top: auto; padding-top: 8px;
    border-top: 1px solid var(--pm-border);
    display: flex; flex-direction: column; gap: 1px;
  }
  .sources { list-style: none; display: flex; flex-direction: column; gap: 1px; }
  .source {
    display: flex; align-items: center; gap: 8px; width: 100%;
    padding: 5px 7px; border: none; border-radius: var(--pm-radius-sm);
    background: transparent; color: var(--pm-text-2); text-align: left; cursor: pointer;
  }
  .source:hover { background: var(--pm-hover); }
  .source.active { background: var(--pm-card-2); color: var(--pm-text-bright); box-shadow: inset 0 0 0 1px var(--pm-border); }
  .source-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .count {
    min-width: 20px; padding: 1px 6px; border-radius: 999px; text-align: center; flex-shrink: 0;
    font-size: 11.5px; font-weight: 600; font-variant-numeric: tabular-nums;
    background: var(--pm-accent); color: var(--pm-on-accent);
  }
  .count.quiet { background: var(--pm-border); color: var(--pm-text-2); }
  .count.zero { background: transparent; color: var(--pm-ok); }
  .count.none { background: transparent; color: var(--pm-faint); font-weight: 400; }
  .count.err { background: var(--pm-err); color: #fff; }
  .spinner {
    width: 12px; height: 12px; border-radius: 50%;
    border: 2px solid var(--pm-border-strong); border-top-color: var(--pm-accent);
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }

  .content { flex: 1; min-width: 0; display: flex; flex-direction: column; background: var(--pm-surface); }
  .source-head {
    display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; flex-wrap: wrap;
    padding: 14px 18px 12px;
    border-bottom: 1px solid var(--pm-border);
  }
  .source-title { flex: 1 1 240px; min-width: 0; }
  h2 { font-size: 16px; font-weight: 600; color: var(--pm-text-bright); letter-spacing: -0.01em; }
  .source-status { margin-top: 2px; color: var(--pm-muted); }
  .source-status.err { color: var(--pm-err); }
  .source-actions { display: flex; align-items: center; gap: 8px; flex-shrink: 0; margin-left: auto; }

  .segmented { display: flex; gap: 2px; padding: 2px; border-radius: var(--pm-radius); background: var(--pm-bg); }
  .segmented button {
    border: none; background: transparent; color: var(--pm-muted);
    padding: 3px 10px; border-radius: calc(var(--pm-radius) - 2px); cursor: pointer; font-size: 12px; font-weight: 500;
  }
  .segmented button:hover { color: var(--pm-text); }
  .segmented button.active { background: var(--pm-card-2); color: var(--pm-text); }

  .list { flex: 1; min-height: 0; overflow-y: auto; padding: 6px 10px; }
  .list-bar { display: flex; align-items: center; gap: 6px; padding: 10px 18px 0; }
  .list-count { color: var(--pm-muted); margin-right: auto; }
  .item {
    display: flex; align-items: center; gap: 10px;
    padding: 6px 8px; border-radius: var(--pm-radius-sm);
    cursor: pointer; user-select: none;
  }
  .item:hover { background: var(--pm-card); }
  .item input[type="checkbox"] { accent-color: var(--pm-accent); width: 14px; height: 14px; flex-shrink: 0; cursor: pointer; }
  .item-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--pm-text); }
  .item-untracked { cursor: default; }
  .item-side { display: flex; align-items: center; gap: 6px; flex-shrink: 0; }
  .item-note { font-size: 12px; color: var(--pm-muted); }
  .item-note.ok { color: var(--pm-ok); font-family: "SF Mono", Menlo, monospace; font-size: 11.5px; }

  .source-foot {
    display: flex; align-items: center; gap: 6px; flex-wrap: wrap;
    padding: 10px 18px;
    border-top: 1px solid var(--pm-border);
    background: var(--pm-surface-2);
  }
  .foot-note { font-size: 12px; color: var(--pm-muted); margin-left: auto; }
  .cmd { font-family: "SF Mono", Menlo, monospace; font-size: 12px; color: var(--pm-muted); margin-left: 4px; }

  .output {
    flex: 1; min-height: 0; overflow-y: auto;
    padding: 12px 18px;
    background: var(--pm-bg);
    font-family: "SF Mono", Menlo, monospace;
    font-size: 12px; line-height: 1.65;
  }
  .line { white-space: pre-wrap; word-break: break-word; color: var(--pm-muted); }
  .line.ok { color: var(--pm-ok); }
  .line.warn { color: var(--pm-text); }
  .line.bad { color: var(--pm-err); }
  .empty { font-family: var(--pm-font); font-size: 13px; color: var(--pm-muted); max-width: 44ch; padding: 6px 8px; }

  /* ── Pages: History, Settings, What's new ────────────────────────────── */
  .page { flex: 1; min-width: 0; min-height: 0; display: flex; flex-direction: column; }
  .page-head {
    display: flex; align-items: center; gap: 12px;
    padding: 14px 18px 12px;
    border-bottom: 1px solid var(--pm-border);
  }
  .page-head h2 { flex: 1; }
  .page-sub { font-size: 12px; font-weight: 400; color: var(--pm-muted); margin-left: 6px; }
  .page-body { flex: 1; min-height: 0; overflow-y: auto; }
  .search {
    width: 220px; padding: 4px 9px;
    border-radius: var(--pm-radius); border: 1px solid var(--pm-border-strong);
    background: var(--pm-bg); color: var(--pm-text);
  }
  .search:focus { outline: none; border-color: var(--pm-accent); }
  .search::placeholder { color: var(--pm-faint); }

  .history { padding: 4px 0; }
  .history-entry { border-bottom: 1px solid var(--pm-border); }
  .history-row {
    display: flex; align-items: flex-start; gap: 14px; width: 100%;
    padding: 9px 18px; border: none; background: transparent; text-align: left; cursor: pointer;
  }
  .history-row:hover { background: var(--pm-card); }
  .history-when { width: 128px; flex-shrink: 0; color: var(--pm-muted); font-size: 12px; padding-top: 2px; font-variant-numeric: tabular-nums; }
  .history-what { flex: 1; display: flex; flex-wrap: wrap; align-items: center; gap: 5px; }
  .history-source { color: var(--pm-text); font-weight: 500; margin-right: 4px; }
  .chip { padding: 1px 7px; border-radius: 999px; background: var(--pm-card); border: 1px solid var(--pm-border); font-size: 12px; color: var(--pm-text-2); }
  .chip-quiet { color: var(--pm-muted); border-style: dashed; }
  .history-caret { color: var(--pm-faint); font-size: 11px; padding-top: 3px; }
  .history-output { flex: none; padding: 8px 18px 12px 160px; border-top: 1px solid var(--pm-border); }

  .settings { padding: 6px 18px 24px; }
  .settings section { padding: 14px 0 6px; }
  .settings section + section { border-top: 1px solid var(--pm-border); }
  .settings h3 { font-size: 13px; font-weight: 600; color: var(--pm-text-bright); margin-bottom: 8px; }
  .section-hint { color: var(--pm-muted); margin: -4px 0 10px; max-width: 60ch; }
  .row {
    display: flex; align-items: center; justify-content: space-between; gap: 16px;
    padding: 9px 0;
  }
  .row + .row { border-top: 1px solid var(--pm-border); }
  label.row { cursor: pointer; }
  .row-text { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
  .row-label { color: var(--pm-text); font-weight: 500; }
  .row-desc { font-size: 12px; color: var(--pm-muted); }
  .row-controls { display: flex; gap: 6px; flex-shrink: 0; }
  .row input[type="checkbox"] { accent-color: var(--pm-accent); width: 16px; height: 16px; flex-shrink: 0; cursor: pointer; }
  .select {
    padding: 4px 8px; border-radius: var(--pm-radius-sm);
    border: 1px solid var(--pm-border-strong); background: var(--pm-card); color: var(--pm-text);
  }
  /* The opened menu is drawn by macOS, not the page, so its rows carry their own colours. */
  .select option { background: var(--pm-card); color: var(--pm-text); }
  .select:disabled { opacity: 0.5; }
  .minutes { width: 4.2rem; }

  .about { display: flex; gap: 14px; align-items: flex-start; padding: 12px 0 0; color: var(--pm-muted); }
  .about-icon { width: 44px; height: 44px; border-radius: 10px; flex-shrink: 0; }
  .about p + p { margin-top: 6px; }
  .about strong { color: var(--pm-text); }
  .about-links { display: flex; gap: 14px; }
  .about-fine { font-size: 12px; color: var(--pm-muted); }

  .prose { padding: 14px 18px 24px; max-width: 60ch; color: var(--pm-text-2); }
  .prose h3 { font-size: 13px; font-weight: 600; color: var(--pm-text-bright); margin: 14px 0 6px; }
  .prose h3:first-child { margin-top: 0; }
  .prose p { margin: 4px 0; line-height: 1.6; }
  .prose .bullet { padding-left: 14px; text-indent: -10px; }
  .prose .bullet::before { content: "• "; color: var(--pm-accent); }
  .prose strong { color: var(--pm-text); }

  @media (prefers-reduced-motion: reduce) {
    .spinner { animation: none; border-top-color: var(--pm-accent); }
    .btn, .segmented button { transition: none; }
  }
</style>
