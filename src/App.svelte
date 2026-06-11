<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getVersion } from "@tauri-apps/api/app";
  import { check as checkUpdate } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
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
      label: "OS System Updates",
      description: "Checks for OS-level updates via softwareupdate",
      upgradeCmd: "sudo softwareupdate -ia",
      platform: "mac",
    },
    {
      id: "app_store",
      label: "App Store",
      description: "Apps installed via the Mac App Store",
      upgradeCmd: "mas upgrade",
      platform: "mac",
    },
    {
      id: "brew_casks",
      label: "Homebrew Apps",
      description: "GUI apps managed by Homebrew",
      upgradeCmd: "brew upgrade --cask --greedy",
      platform: "mac",
    },
    {
      id: "untracked_apps",
      label: "Apps Without Auto-Updates",
      description: "Apps on your Mac that aren't connected to an update manager yet. Enable auto-updates to keep them current.",
      platform: "mac",
    },
    {
      id: "brew_formulae",
      label: "brew",
      description: "Command-line tools installed via Homebrew",
      upgradeCmd: "brew upgrade",
      platform: "mac",
      dev: true,
    },
    {
      id: "npm_globals",
      label: "npm",
      description: "Globally installed Node packages",
      upgradeCmd: "npm update -g",
      platform: "all",
      dev: true,
    },
    {
      id: "pip_packages",
      label: "pip",
      description: "Outdated Python packages",
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
  let activeDevTab = "";
  let appVersion = "";
  let showHistory = false;
  let historyEntries: HistoryEntry[] = [];
  let historySearch = "";
  let expandedHistoryEntry: number | null = null;
  let caskSearch: Record<string, CaskSearchState> = {};
  let showMenu = false;
  let showSettings = false;
  let showAbout = false;

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

    loadLastChecked();

    if (autoCheckUpdates) {
      const lastCheck = parseInt(localStorage.getItem("lastUpdateCheck") ?? "0", 10);
      if (Date.now() - lastCheck > 24 * 60 * 60 * 1000) {
        checkAppUpdate();
      }
    }

    const visible = sections.filter(platformVisible);
    if (visible.length > 0) activeTab = visible[0].id;

    const firstDev = sections.find(s => platformVisible(s) && !!s.dev);
    if (firstDev) activeDevTab = firstDev.id;

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
          selectedItems[payload.section] = [];
          selectedItems = selectedItems;
          viewMode[payload.section] = "readonly";
          viewMode = viewMode;
        }
      }
    });

    await listen<{ section: string; line: string }>("upgrade-output", ({ payload }) => {
      if (itemSections.has(payload.section)) {
        upgradeLogs[payload.section] = [...upgradeLogs[payload.section], payload.line];
        upgradeLogs = upgradeLogs;
      } else {
        outputs[payload.section] = [...outputs[payload.section], payload.line];
        outputs = outputs;
      }
    });

    await listen<{ section: string; status: string }>("upgrade-status", ({ payload }) => {
      upgradeStatuses[payload.section] = payload.status as Status;
      upgradeStatuses = upgradeStatuses;
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

  async function runAll() {
    runningAll = true;
    const visible = sections.filter(platformVisible);
    for (const s of visible) {
      if (s.dev) {
        activeTab = "dev";
        activeDevTab = s.id;
      } else {
        activeTab = s.id;
      }
      await runSection(s.id);
    }
    runningAll = false;
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

  function copyCmd(cmd: string) {
    navigator.clipboard.writeText(cmd);
  }

  function platformVisible(s: Section) {
    return s.platform === "all" || s.platform === currentPlatform;
  }

  const statusColor: Record<Status, string> = {
    idle: "#3d5166",
    running: "#00659A",
    done: "#22c55e",
    error: "#ef4444",
  };

  // Build main tab list, collapsing dev sections into a single "Dev" entry
  $: devSections = sections.filter(s => platformVisible(s) && !!s.dev);

  $: tabItems = (() => {
    const items: Array<{ id: string; label: string; virtual?: true } | Section> = [];
    let devInserted = false;
    for (const s of sections) {
      if (!platformVisible(s)) continue;
      if (s.dev) {
        if (!devInserted) { items.push({ id: "dev", label: "Dev", virtual: true }); devInserted = true; }
      } else {
        items.push(s);
      }
    }
    return items;
  })();

  // Aggregate status for the Dev tab dot
  $: devStatus = ((): Status => {
    const vals = devSections.map(s => statuses[s.id]);
    if (vals.some(v => v === "error")) return "error";
    if (vals.some(v => v === "running")) return "running";
    if (vals.length > 0 && vals.every(v => v === "done")) return "done";
    return "idle";
  })();

  $: activeSectionId = activeTab === "dev" ? activeDevTab : activeTab;
  $: activeSection = sections.find(s => s.id === activeSectionId);
  $: activeStatus = (activeSectionId ? statuses[activeSectionId] : "idle") as Status;
  $: activeUpgradeStatus = (activeSectionId ? upgradeStatuses[activeSectionId] : "idle") as Status;
  $: activeLines = activeSectionId ? outputs[activeSectionId] : [] as string[];
  $: activeLastChecked = activeSectionId ? lastChecked[activeSectionId] : null;
  $: activeHasOutdated = activeLines.some((l) => l.includes("⚠"));
  $: activeParsedItems = activeSectionId ? (parsedItems[activeSectionId] ?? []) : [];
  $: activeSelectedItems = activeSectionId ? (selectedItems[activeSectionId] ?? []) : [];
  $: activeHasItemSelection = !!activeSectionId && itemSections.has(activeSectionId);
  $: activeViewMode = activeSectionId ? (viewMode[activeSectionId] ?? "readonly") : "readonly";
  $: activeUpgradeLines = activeSectionId ? (upgradeLogs[activeSectionId] ?? []) : [] as string[];
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

  let pendingUpdate: Awaited<ReturnType<typeof checkUpdate>> = null;

  async function checkAppUpdate() {
    if (appUpdateStatus === "checking") return;
    appUpdateStatus = "checking";
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
    } catch {
      appUpdateStatus = "error";
    }
  }

  async function installAppUpdate() {
    if (!pendingUpdate) return;
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

  function simplifyUntrackedLog(lines: string[]): string[] {
    return lines
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

  function formatTime(d: Date): string {
    return d.toLocaleString([], {
      year: "numeric", month: "short", day: "numeric",
      hour: "2-digit", minute: "2-digit", second: "2-digit",
    });
  }
</script>

<main>
  <header>
    <div class="title-block">
      <img src={iconUrl} alt="" class="app-icon" />
      <h1><span class="brand-bed">PartyMAN</span> Update Manager</h1>
      {#if appVersion}
        <span class="version">v{appVersion}</span>
      {/if}
    </div>
    <p class="subtitle">Check for updates and apply them with one click</p>
    <div class="view-switcher">
      <button class:active={!showHistory && !showSettings && !showAbout} onclick={() => { showHistory = false; showSettings = false; showAbout = false; }}>Updates</button>
      <button class:active={showHistory} onclick={() => { showHistory = true; showSettings = false; showAbout = false; loadHistory(); }}>History</button>
    </div>
    <div class="menu-wrap">
      <button class="hamburger" onclick={() => { showMenu = !showMenu; }}
        class:active={showMenu} aria-label="Menu">
        <svg width="16" height="14" viewBox="0 0 16 14" fill="none" xmlns="http://www.w3.org/2000/svg">
          <path d="M1 1h14M1 7h14M1 13h14" stroke="currentColor" stroke-width="1.75" stroke-linecap="round"/>
        </svg>
      </button>
      {#if showMenu}
        <div class="menu-backdrop" onclick={() => { showMenu = false; }}></div>
        <div class="dropdown">

          <button class="menu-item menu-item-primary" onclick={() => { runAll(); showMenu = false; }} disabled={runningAll}>
            <span class="menu-item-label">{runningAll ? "Checking…" : "Check All"}</span>
          </button>

          <div class="menu-sep"></div>

          <button class="menu-item" onclick={() => { showSettings = true; showHistory = false; showAbout = false; showMenu = false; }}>
            <span class="menu-item-label">Settings</span>
          </button>

          <button class="menu-item" onclick={() => { showHistory = true; showSettings = false; showAbout = false; showMenu = false; loadHistory(); }}>
            <span class="menu-item-label">History</span>
          </button>

          <div class="menu-sep"></div>

          <button class="menu-item" onclick={() => { showAbout = true; showHistory = false; showSettings = false; showMenu = false; }}>
            <span class="menu-item-label">About</span>
          </button>

        </div>
      {/if}
    </div>
  </header>

  {#if appUpdateStatus === "available" && appUpdateInfo}
    <div class="app-update-banner">
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg" style="flex-shrink:0">
        <circle cx="8" cy="8" r="6.25" stroke="currentColor" stroke-width="1.5"/>
        <path d="M8 5v3.5M8 11v.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
      </svg>
      <span>PartyMAN v{appUpdateInfo.version} is available</span>
      <button class="banner-dl-btn" onclick={installAppUpdate}>Install Update</button>
      <button class="banner-dismiss" onclick={() => { appUpdateStatus = "idle"; appUpdateInfo = null; }}>✕</button>
    </div>
  {/if}

  {#if showSettings}
    <div class="settings-panel">
      <div class="page-header">
        <h2>Settings</h2>
        <button class="page-close" onclick={() => { showSettings = false; }}>✕</button>
      </div>
      <div class="settings-body">
        <div class="settings-section">
          <h3 class="settings-section-title">App Updates</h3>
          <div class="settings-row">
            <div class="settings-row-info">
              <span class="settings-row-label">Check for Updates</span>
              <span class="settings-row-desc">Check GitHub for a newer version of PartyMAN Update Manager</span>
            </div>
            <button class="settings-check-btn"
              onclick={() => { if (appUpdateStatus === "available") { installAppUpdate(); } else { checkAppUpdate(); } }}
              disabled={appUpdateStatus === "checking"}>
              {#if appUpdateStatus === "checking"}Checking…
              {:else if appUpdateStatus === "up-to-date"}✔ Up to date
              {:else if appUpdateStatus === "available" && appUpdateInfo}v{appUpdateInfo.version} — Install
              {:else}Check Now{/if}
            </button>
          </div>
          <label class="settings-row">
            <div class="settings-row-info">
              <span class="settings-row-label">Auto-check on startup</span>
              <span class="settings-row-desc">Automatically check once per day when the app opens</span>
            </div>
            <input type="checkbox" bind:checked={autoCheckUpdates}
              onchange={() => localStorage.setItem("autoCheckUpdates", String(autoCheckUpdates))} />
          </label>
        </div>
      </div>
    </div>

  {:else if showAbout}
    <div class="about-panel">
      <div class="page-header">
        <h2>About</h2>
        <button class="page-close" onclick={() => { showAbout = false; }}>✕</button>
      </div>
      <div class="about-body">
        <img src={iconUrl} alt="" class="about-icon" />
        <h2 class="about-app-name"><span class="brand-bed">PartyMAN</span> Update Manager</h2>
        <p class="about-app-version">Version {appVersion}</p>
        <p class="about-app-desc">A macOS update manager that checks for and applies updates across system tools, App Store apps, Homebrew, and more — all in one place.</p>
        <div class="about-actions">
          <button class="about-action-btn" onclick={() => openReleaseUrl("https://github.com/paymonr/partyman_update_manager/releases")}>
            View Releases
          </button>
          <button class="about-action-btn" onclick={() => openReleaseUrl("https://github.com/paymonr/partyman_update_manager")}>
            GitHub
          </button>
        </div>
        <p class="about-license">Licensed under the Apache License 2.0</p>
      </div>
    </div>

  {:else if showHistory}
    <div class="history-panel">
      <div class="history-header">
        <h2>Update History <span class="history-sub">last 180 days</span></h2>
        <input
          class="history-search"
          type="search"
          placeholder="Search by app, type, or keyword…"
          bind:value={historySearch}
        />
      </div>
      <div class="history-list">
        {#if filteredHistory.length === 0}
          <p class="empty">{historyEntries.length === 0 ? "No update history yet." : "No results for that search."}</p>
        {:else}
          {#each filteredHistory as entry, i}
            <div class="history-entry">
              <button class="history-entry-header" onclick={() => expandedHistoryEntry = expandedHistoryEntry === i ? null : i}>
                <div class="history-meta">
                  <span class="history-label">{entry.label}</span>
                  <span class="history-time">{formatTime(new Date(entry.ts * 1000))}</span>
                </div>
                <div class="history-items">
                  {#if entry.item_names.length > 0}
                    {#each entry.item_names as name}
                      <span class="history-item-chip">{name}</span>
                    {/each}
                  {:else}
                    <span class="history-item-chip history-item-bulk">bulk upgrade</span>
                  {/if}
                </div>
                <span class="history-expand">{expandedHistoryEntry === i ? "▲" : "▼"}</span>
              </button>
              {#if expandedHistoryEntry === i}
                <div class="history-output">
                  {#each entry.lines as line}
                    <div class="line">{line}</div>
                  {/each}
                </div>
              {/if}
            </div>
          {/each}
        {/if}
      </div>
    </div>
  {:else}

  <div class="tab-bar">
    {#each tabItems as item (item.id)}
      {#if item.id === "dev"}
        <button
          class="tab"
          class:active={activeTab === "dev"}
          class:running={devStatus === "running"}
          class:done={devStatus === "done"}
          class:error={devStatus === "error"}
          onclick={() => (activeTab = "dev")}
        >
          <span class="tab-dot" style="background:{statusColor[devStatus]}"></span>
          Dev
        </button>
      {:else}
        {@const st = statuses[item.id]}
        <button
          class="tab"
          class:active={activeTab === item.id}
          class:running={st === "running"}
          class:done={st === "done"}
          class:error={st === "error"}
          onclick={() => (activeTab = item.id)}
        >
          <span class="tab-dot" style="background:{statusColor[st]}"></span>
          {item.label}
        </button>
      {/if}
    {/each}
  </div>

  {#if activeTab === "dev"}
    <div class="sub-tab-bar">
      {#each devSections as section (section.id)}
        {@const st = statuses[section.id]}
        <button
          class="sub-tab"
          class:active={activeDevTab === section.id}
          class:running={st === "running"}
          class:done={st === "done"}
          class:error={st === "error"}
          onclick={() => (activeDevTab = section.id)}
        >
          <span class="tab-dot" style="background:{statusColor[st]}"></span>
          {section.label}
        </button>
      {/each}
    </div>
  {/if}

  {#if activeSection}
    <div class="panel" class:running={activeStatus === "running"} class:done={activeStatus === "done"} class:error={activeStatus === "error"}>
      <div class="panel-header">
        <div class="panel-info">
          <h2>{activeSection.label}</h2>
          <p class="desc">{activeSection.description}</p>
          {#if activeLastChecked}
            <p class="last-checked">Last checked at {formatTime(activeLastChecked)}</p>
          {/if}
          {#if activeSection.upgradeCmd}
            <div class="cmd-bar">
              <code class="cmd-text">{activeSection.upgradeCmd}</code>
              <button
                class="cmd-copy"
                onclick={() => copyCmd(activeSection!.upgradeCmd!)}
                aria-label="Copy upgrade command"
              >
                <svg width="13" height="13" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg">
                  <rect x="5" y="5" width="9" height="9" rx="1.5" stroke="currentColor" stroke-width="1.5"/>
                  <path d="M11 5V3.5A1.5 1.5 0 0 0 9.5 2H3.5A1.5 1.5 0 0 0 2 3.5V9.5A1.5 1.5 0 0 0 3.5 11H5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>
                </svg>
              </button>
            </div>
          {/if}
        </div>
        <div class="panel-actions">
          {#if activeHasItemSelection && activeSectionId !== "untracked_apps" && activeStatus === "done" && activeParsedItems.length > 0}
            <button
              class="update-btn"
              onclick={() => runUpgradeItems(activeSectionId, activeSelectedItems)}
              disabled={activeUpgradeStatus === "running" || activeSelectedItems.length === 0}
            >
              {activeUpgradeStatus === "running" ? "Updating…" : `Update Selected (${activeSelectedItems.length})`}
            </button>
          {:else if activeSection.upgradeCmd && activeStatus === "done" && activeHasOutdated && !activeHasItemSelection && !activeSection.dev}
            <button
              class="update-btn"
              onclick={() => runUpgrade(activeSectionId)}
              disabled={activeUpgradeStatus === "running"}
            >
              {activeUpgradeStatus === "running" ? "Updating…" : "Run Update"}
            </button>
          {/if}
          {#if activeHasItemSelection && activeStatus === "done" && activeParsedItems.length > 0}
            <div class="view-toggle">
              <button class:active={activeViewMode === "readonly"} onclick={() => { viewMode[activeSectionId] = "readonly"; viewMode = viewMode; }}>Read-Only</button>
              <button class:active={activeViewMode === "select"} onclick={() => { viewMode[activeSectionId] = "select"; viewMode = viewMode; }}>Updates</button>
              <button class:active={activeViewMode === "upgrade"} onclick={() => { viewMode[activeSectionId] = "upgrade"; viewMode = viewMode; }}>Log</button>
            </div>
          {/if}
          <button
            class="check-btn"
            onclick={() => runSection(activeSectionId)}
            disabled={activeStatus === "running" || activeUpgradeStatus === "running"}
          >
            {activeStatus === "running" ? "Running…" : "Run Check"}
          </button>
        </div>
      </div>

      {#if showSelectView}
        <div class="items-list">
          {#if activeSectionId === "untracked_apps"}
            <div class="items-bar">
              <span class="items-count">{activeParsedItems.length} app{activeParsedItems.length === 1 ? "" : "s"} found</span>
              <button class="items-sel-btn" onclick={findAllCasks}>Check All</button>
            </div>
            {#each activeParsedItems as item}
              <div class="item-row untracked-row">
                <span class="item-name">{item.name}</span>
                <div class="cask-search-state">
                  {#if !caskSearch[item.id]}
                    <button class="find-cask-btn" onclick={() => findCask(item.id)}>Check</button>
                  {:else if caskSearch[item.id].status === "searching"}
                    <span class="cask-status-text">Checking…</span>
                  {:else if caskSearch[item.id].status === "none"}
                    <span class="cask-status-text cask-none">Can't be auto-updated</span>
                  {:else if caskSearch[item.id].status === "found"}
                    <span class="cask-match">✓ Match found</span>
                    <button
                      class="track-btn"
                      onclick={() => trackApp(caskSearch[item.id].candidates[0].token, item.appDir)}
                      disabled={activeUpgradeStatus === "running"}
                    >Enable Auto-Updates</button>
                  {/if}
                </div>
              </div>
            {/each}
          {:else}
            <div class="items-bar">
              <span class="items-count">{activeParsedItems.length} outdated</span>
              <button class="items-sel-btn" onclick={() => selectAll(activeSectionId)}>Select All</button>
              <button class="items-sel-btn" onclick={() => selectNone(activeSectionId)}>Clear</button>
            </div>
            {#each activeParsedItems as item}
              <label class="item-row">
                <input
                  type="checkbox"
                  checked={activeSelectedItems.includes(item.id)}
                  onchange={(e) => toggleItem(activeSectionId, item.id, (e.target as HTMLInputElement).checked)}
                />
                <span class="item-name">{item.name}</span>
              </label>
            {/each}
          {/if}
        </div>
      {:else if activeViewMode === "upgrade"}
        <div class="output" bind:this={outputEl}>
          {#if activeUpgradeLines.length === 0}
            <p class="empty">No updates have been run yet.</p>
          {:else}
            {@const displayLines = activeSectionId === "untracked_apps" ? simplifyUntrackedLog(activeUpgradeLines) : activeUpgradeLines}
            {#each displayLines as line}
              <div class="line">{line}</div>
            {/each}
          {/if}
        </div>
      {:else}
        {#if activeHasItemSelection && activeStatus === "done" && activeParsedItems.length > 0}
          <div class="updates-hint">
            Updates available — switch to <button class="hint-link" onclick={() => { viewMode[activeSectionId] = "select"; viewMode = viewMode; }}>Updates</button> to choose which ones to install.
          </div>
        {/if}
        <div class="output" bind:this={outputEl}>
          {#if activeLines.length === 0}
            <p class="empty">Press "Run Check" to check for updates.</p>
          {:else}
            {#each activeLines as line}
              <div class="line">{line}</div>
            {/each}
          {/if}
        </div>
      {/if}
    </div>
  {/if}
  {/if}
</main>

<style>
  :global(*, *::before, *::after) { box-sizing: border-box; margin: 0; padding: 0; }
  :global(html), :global(body) {
    height: 100%;
    overflow: hidden;
  }
  :global(body) {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: #111822;
    color: #e2e8f0;
  }

  main { max-width: 860px; margin: 0 auto; padding: 1.5rem 1rem 1.5rem; display: flex; flex-direction: column; height: 100vh; overflow: hidden; }

  header {
    display: flex;
    align-items: center;
    column-gap: 0.75rem;
    row-gap: 0.35rem;
    margin-bottom: 1rem;
    flex-wrap: wrap;
    padding-bottom: 0.9rem;
    border-bottom: 1px solid #2a3848;
    position: relative;
  }
  header::after {
    content: "";
    position: absolute;
    bottom: -1px;
    left: 0;
    width: 3rem;
    height: 2px;
    background: #F58026;
    border-radius: 1px;
  }

  .title-block { display: flex; align-items: center; gap: 0.65rem; flex: 1; }
  .app-icon { width: 36px; height: 36px; border-radius: 8px; flex-shrink: 0; }
  h1 { font-size: 1.4rem; font-weight: 700; color: #f8fafc; letter-spacing: -0.01em; }
  .brand-bed { color: #F58026; }
  .version { font-size: 0.75rem; color: #3d5166; font-weight: 500; }
  .subtitle { font-size: 0.78rem; color: #3d5166; width: 100%; margin-top: -0.5rem; }

  .run-all {
    background: #00659A;
    color: white;
    border: none;
    border-radius: 8px;
    padding: 0.45rem 1.1rem;
    font-size: 0.88rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
  }
  .run-all:hover:not(:disabled) { background: #0076b3; }
  .run-all:disabled { opacity: 0.5; cursor: not-allowed; }

  /* ── Main tab bar ── */
  .tab-bar {
    display: flex;
    gap: 2px;
    overflow-x: auto;
    border-bottom: 1px solid #2a3848;
    padding-bottom: 0;
    scrollbar-width: none;
    margin-top: -0.25rem;
  }
  .tab-bar::-webkit-scrollbar { display: none; }

  .tab {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.55rem 0.9rem;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: #4a6070;
    font-size: 0.82rem;
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
    transition: color 0.15s, border-color 0.15s;
    margin-bottom: -1px;
  }
  .tab:hover { color: #a8bfcc; }
  .tab.active { color: #f1f5f9; border-bottom-color: #F58026; }
  .tab.running { color: #00659A; }
  .tab.done { color: #6a8899; }
  .tab.error { color: #ef4444; }

  .tab-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
    transition: background 0.2s;
  }

  /* ── Dev sub-tab bar ── */
  .sub-tab-bar {
    display: flex;
    gap: 2px;
    overflow-x: auto;
    background: #161e29;
    border-bottom: 1px solid #2a3848;
    padding: 0 0.5rem;
    scrollbar-width: none;
  }
  .sub-tab-bar::-webkit-scrollbar { display: none; }

  .sub-tab {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.4rem 0.8rem;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: #3d5166;
    font-size: 0.78rem;
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
    transition: color 0.15s, border-color 0.15s;
    margin-bottom: -1px;
  }
  .sub-tab:hover { color: #8aa4b8; }
  .sub-tab.active { color: #e2e8f0; border-bottom-color: #F58026; }
  .sub-tab.running { color: #00659A; }
  .sub-tab.done { color: #4a6070; }
  .sub-tab.error { color: #ef4444; }

  /* ── Panel ── */
  .panel {
    background: #1E2733;
    border: 1px solid #2a3848;
    border-top: none;
    border-radius: 0 0 12px 12px;
    overflow: hidden;
    transition: border-color 0.2s;
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    margin-bottom: 0;
  }
  .panel.running { border-color: #00659A; }
  .panel.done { border-color: #22c55e44; }
  .panel.error { border-color: #ef444444; }

  .panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 1rem 1.25rem;
    border-bottom: 1px solid #2a3848;
    gap: 1rem;
    flex-wrap: wrap;
    background: #1a2230;
  }

  h2 { font-size: 1rem; font-weight: 600; color: #f1f5f9; }
  .desc { font-size: 0.78rem; color: #4a6070; margin-top: 0.2rem; }

  .panel-actions { display: flex; gap: 0.5rem; align-items: center; flex-shrink: 0; }

  .check-btn {
    background: #00659A;
    color: white;
    border: none;
    border-radius: 7px;
    padding: 0.45rem 1rem;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
  }
  .check-btn:hover:not(:disabled) { background: #0076b3; }
  .check-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  .update-btn {
    background: #F58026;
    color: #111822;
    border: none;
    border-radius: 7px;
    padding: 0.45rem 1rem;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
  }
  .update-btn:hover:not(:disabled) { background: #d96e1a; }
  .update-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  /* ── View toggle ── */
  .view-toggle {
    display: flex;
    border: 1px solid #2a3848;
    border-radius: 6px;
    overflow: hidden;
  }
  .view-toggle button {
    background: transparent;
    border: none;
    color: #4a6070;
    font-size: 0.78rem;
    font-weight: 500;
    padding: 0.3rem 0.65rem;
    cursor: pointer;
    transition: background 0.12s, color 0.12s;
  }
  .view-toggle button:hover { color: #8aa4b8; }
  .view-toggle button.active { background: #2a3848; color: #f1f5f9; }

  /* ── Items checklist ── */
  .items-list {
    background: #0d1219;
    padding: 0.75rem 1.25rem 1rem;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }

  .items-bar {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-bottom: 0.45rem;
  }

  .items-count {
    font-size: 0.72rem;
    color: #3d5166;
    flex: 1;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .items-sel-btn {
    background: transparent;
    border: 1px solid #2a3848;
    border-radius: 4px;
    color: #4a6070;
    font-size: 0.68rem;
    padding: 0.1rem 0.45rem;
    cursor: pointer;
    transition: color 0.1s, border-color 0.1s;
  }
  .items-sel-btn:hover { color: #8aa4b8; border-color: #3a4858; }

  .item-row {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.2rem 0;
    cursor: pointer;
    user-select: none;
  }
  .item-row input[type="checkbox"] {
    accent-color: #F58026;
    width: 13px;
    height: 13px;
    cursor: pointer;
    flex-shrink: 0;
  }
  .item-name {
    font-size: 0.8rem;
    color: #8aa4b8;
    font-family: "SF Mono", "Fira Code", monospace;
  }
  .item-row:hover .item-name { color: #c8dae6; }

  /* ── Updates hint bar ── */
  .updates-hint {
    padding: 0.45rem 1.25rem;
    background: #0f1a26;
    border-bottom: 1px solid #1a2d40;
    font-size: 0.78rem;
    color: #4a6070;
  }
  .hint-link {
    background: none;
    border: none;
    color: #00659A;
    font-size: inherit;
    font-weight: 600;
    cursor: pointer;
    padding: 0;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .hint-link:hover { color: #5ab8e8; }

  /* ── Output ── */
  .output {
    flex: 1;
    background: #0d1219;
    padding: 1rem 1.25rem;
    overflow-y: auto;
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.8rem;
    line-height: 1.7;
  }

  .last-checked { font-size: 0.72rem; color: #3d5166; margin-top: 0.25rem; }
  .empty { color: #3d5166; font-style: italic; font-family: inherit; font-size: 0.85rem; }
  .line { white-space: pre-wrap; word-break: break-all; color: #8aa4b8; }

  /* ── App update banner ── */
  .app-update-banner {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.5rem 1rem;
    background: #1a2d1a;
    border: 1px solid #22c55e44;
    border-radius: 8px;
    font-size: 0.82rem;
    color: #22c55e;
    margin-bottom: 0.5rem;
  }
  .banner-dl-btn {
    background: #22c55e;
    color: #0d1a0d;
    border: none;
    border-radius: 5px;
    padding: 0.22rem 0.7rem;
    font-size: 0.78rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.12s;
  }
  .banner-dl-btn:hover { background: #16a34a; }
  .banner-dismiss {
    background: transparent;
    border: none;
    color: #22c55e88;
    font-size: 0.75rem;
    cursor: pointer;
    margin-left: auto;
    padding: 0.1rem 0.25rem;
    transition: color 0.1s;
  }
  .banner-dismiss:hover { color: #22c55e; }

  /* ── Hamburger + dropdown ── */
  .menu-wrap {
    position: absolute;
    top: 0;
    right: 0;
    z-index: 30;
  }

  .hamburger {
    display: flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: 1px solid #2a3848;
    border-radius: 8px;
    color: #4a6070;
    width: 36px;
    height: 36px;
    cursor: pointer;
    transition: color 0.12s, border-color 0.12s, background 0.12s;
  }
  .hamburger:hover, .hamburger.active { color: #f1f5f9; border-color: #3a4858; background: #1a2230; }

  .menu-backdrop { position: fixed; inset: 0; z-index: 10; }

  .dropdown {
    position: absolute;
    top: calc(100% + 2px);
    right: 0;
    z-index: 20;
    background: #1a2230;
    border: 1px solid #2a3848;
    border-radius: 10px;
    min-width: 210px;
    padding: 0.35rem 0;
    box-shadow: 0 8px 24px #00000055;
  }

  .menu-item {
    display: flex;
    align-items: center;
    width: 100%;
    padding: 0.5rem 1rem;
    background: transparent;
    border: none;
    color: #c8dae6;
    font-size: 0.84rem;
    font-weight: 500;
    cursor: pointer;
    text-align: left;
    gap: 0.5rem;
    transition: background 0.1s;
  }
  .menu-item:hover:not(:disabled) { background: #212e3f; }
  .menu-item:disabled { opacity: 0.5; cursor: not-allowed; }
  .menu-item-primary { color: #f1f5f9; font-weight: 600; }
  .menu-item-primary:hover:not(:disabled) { background: #00659A22; }
  .menu-item-label { flex: 1; }
  .menu-item-meta { font-size: 0.72rem; color: #3d5166; }
  .menu-item-meta.ok { color: #22c55e; }
  .menu-item-meta.new { color: #F58026; font-weight: 600; }
  .menu-sep { height: 1px; background: #2a3848; margin: 0.25rem 0; }

  /* ── Shared page header (Settings, About) ── */
  .page-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.9rem 1.25rem;
    border-bottom: 1px solid #2a3848;
    background: #1a2230;
  }
  .page-header h2 { font-size: 0.95rem; font-weight: 600; color: #f1f5f9; }
  .page-close {
    background: transparent;
    border: none;
    color: #3d5166;
    font-size: 0.85rem;
    cursor: pointer;
    padding: 0.2rem 0.4rem;
    border-radius: 4px;
    transition: color 0.1s, background 0.1s;
  }
  .page-close:hover { color: #f1f5f9; background: #2a3848; }

  /* ── Settings page ── */
  .settings-panel {
    background: #1E2733;
    border: 1px solid #2a3848;
    border-top: none;
    border-radius: 0 0 12px 12px;
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }
  .settings-body { padding: 1.25rem; overflow-y: auto; flex: 1; }
  .settings-section { margin-bottom: 1.5rem; }
  .settings-section-title {
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.07em;
    color: #3d5166;
    margin-bottom: 0.75rem;
  }
  .settings-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1.5rem;
    padding: 0.75rem 1rem;
    background: #1a2230;
    border: 1px solid #2a3848;
    border-radius: 8px;
    cursor: pointer;
  }
  .settings-row-info { display: flex; flex-direction: column; gap: 0.15rem; }
  .settings-row-label { font-size: 0.84rem; color: #e2e8f0; font-weight: 500; }
  .settings-row-desc { font-size: 0.72rem; color: #3d5166; }
  .settings-row input[type="checkbox"] { accent-color: #F58026; cursor: pointer; width: 16px; height: 16px; flex-shrink: 0; }

  .settings-check-btn {
    background: #00659A;
    color: white;
    border: none;
    border-radius: 6px;
    padding: 0.35rem 0.85rem;
    font-size: 0.78rem;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
    flex-shrink: 0;
    transition: background 0.15s;
  }
  .settings-check-btn:hover:not(:disabled) { background: #0076b3; }
  .settings-check-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  /* ── About page ── */
  .about-panel {
    background: #1E2733;
    border: 1px solid #2a3848;
    border-top: none;
    border-radius: 0 0 12px 12px;
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }
  .about-body {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    flex: 1;
    padding: 2rem 1.5rem;
    text-align: center;
  }
  .about-icon { width: 72px; height: 72px; border-radius: 16px; margin-bottom: 0.5rem; }
  .about-app-name { font-size: 1.25rem; font-weight: 700; color: #f8fafc; }
  .about-app-version { font-size: 0.78rem; color: #3d5166; }
  .about-app-desc { font-size: 0.82rem; color: #4a6070; max-width: 360px; line-height: 1.6; margin: 0.5rem 0; }
  .about-actions { display: flex; gap: 0.5rem; margin-top: 0.5rem; }
  .about-action-btn {
    background: transparent;
    border: 1px solid #2a3848;
    border-radius: 7px;
    color: #8aa4b8;
    font-size: 0.82rem;
    font-weight: 500;
    padding: 0.4rem 1rem;
    cursor: pointer;
    transition: color 0.12s, border-color 0.12s, background 0.12s;
  }
  .about-action-btn:hover { color: #f1f5f9; border-color: #3a4858; background: #1a2230; }
  .about-license { font-size: 0.68rem; color: #2a3848; margin-top: 1rem; }

  /* ── View switcher (Updates / History toggle) ── */
  .view-switcher {
    display: flex;
    border: 1px solid #2a3848;
    border-radius: 8px;
    overflow: hidden;
    flex-shrink: 0;
  }
  .view-switcher button {
    background: transparent;
    border: none;
    color: #4a6070;
    font-size: 0.82rem;
    font-weight: 500;
    padding: 0.4rem 1rem;
    cursor: pointer;
    transition: background 0.12s, color 0.12s;
  }
  .view-switcher button:hover { color: #a8bfcc; }
  .view-switcher button.active { background: #2a3848; color: #f1f5f9; }


  /* ── History panel ── */
  .history-panel {
    background: #1E2733;
    border: 1px solid #2a3848;
    border-radius: 0 0 12px 12px;
    display: flex;
    flex-direction: column;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  .history-header {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: 0.9rem 1.25rem;
    border-bottom: 1px solid #2a3848;
    background: #1a2230;
    flex-wrap: wrap;
  }
  .history-header h2 { font-size: 0.95rem; font-weight: 600; color: #f1f5f9; flex-shrink: 0; }
  .history-sub { font-size: 0.72rem; color: #3d5166; font-weight: 400; margin-left: 0.4rem; }

  .history-search {
    flex: 1;
    min-width: 180px;
    background: #0d1219;
    border: 1px solid #2a3848;
    border-radius: 6px;
    color: #e2e8f0;
    font-size: 0.82rem;
    padding: 0.3rem 0.65rem;
    outline: none;
    transition: border-color 0.15s;
  }
  .history-search:focus { border-color: #00659A; }
  .history-search::placeholder { color: #3d5166; }

  .history-list {
    flex: 1;
    overflow-y: auto;
    padding: 0.5rem 0;
  }

  .history-entry {
    border-bottom: 1px solid #1a2230;
  }

  .history-entry-header {
    display: flex;
    align-items: flex-start;
    gap: 0.75rem;
    padding: 0.65rem 1.25rem;
    cursor: pointer;
    transition: background 0.1s;
    background: transparent;
    border: none;
    width: 100%;
    text-align: left;
    color: inherit;
  }
  .history-entry-header:hover { background: #1a2230; }

  .history-meta {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 130px;
    flex-shrink: 0;
  }
  .history-label {
    font-size: 0.78rem;
    font-weight: 600;
    color: #F58026;
  }
  .history-time {
    font-size: 0.68rem;
    color: #3d5166;
  }

  .history-items {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    flex: 1;
    align-items: center;
  }

  .history-item-chip {
    background: #0d1219;
    border: 1px solid #2a3848;
    border-radius: 4px;
    padding: 0.1rem 0.45rem;
    font-size: 0.72rem;
    font-family: "SF Mono", "Fira Code", monospace;
    color: #8aa4b8;
  }
  .history-item-bulk {
    color: #3d5166;
    font-style: italic;
    font-family: inherit;
  }

  .history-expand {
    font-size: 0.6rem;
    color: #3d5166;
    flex-shrink: 0;
    align-self: center;
  }

  .history-output {
    padding: 0.5rem 1.25rem 0.75rem 2.5rem;
    background: #0d1219;
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.75rem;
    line-height: 1.7;
    border-top: 1px solid #1a2230;
  }

  /* ── Untracked apps find/track row ── */
  .untracked-row { justify-content: space-between; }

  .cask-search-state {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex-shrink: 0;
  }

  .find-cask-btn {
    background: transparent;
    border: 1px solid #2a3848;
    border-radius: 4px;
    color: #4a6070;
    font-size: 0.68rem;
    padding: 0.1rem 0.5rem;
    cursor: pointer;
    transition: color 0.1s, border-color 0.1s;
  }
  .find-cask-btn:hover { color: #8aa4b8; border-color: #3a4858; }

  .cask-status-text {
    font-size: 0.7rem;
    color: #3d5166;
    font-style: italic;
  }
  .cask-none { color: #4a3030; }

  .cask-match {
    font-size: 0.7rem;
    color: #22c55e;
    font-weight: 500;
  }

  .track-btn {
    background: #F58026;
    color: #111822;
    border: none;
    border-radius: 4px;
    padding: 0.12rem 0.55rem;
    font-size: 0.68rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.12s;
  }
  .track-btn:hover:not(:disabled) { background: #d96e1a; }
  .track-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  /* ── Panel info column ── */
  .panel-info { display: flex; flex-direction: column; flex: 1; min-width: 0; }

  /* ── Command bar ── */
  .cmd-bar {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.55rem;
    background: #0d1219;
    border: 1px solid #2a3848;
    border-radius: 6px;
    padding: 0.28rem 0.28rem 0.28rem 0.6rem;
    align-self: flex-start;
    max-width: 100%;
  }

  .cmd-text {
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.71rem;
    color: #5ab8e8;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .cmd-copy {
    background: transparent;
    border: none;
    color: #3d5166;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0.22rem;
    border-radius: 4px;
    transition: color 0.12s, background 0.12s;
    flex-shrink: 0;
  }
  .cmd-copy:hover { color: #5ab8e8; background: #00659A22; }

</style>
