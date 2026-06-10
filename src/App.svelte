<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getVersion } from "@tauri-apps/api/app";
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
      description: "GUI apps managed by Homebrew (Chrome, Slack, Docker…)",
      upgradeCmd: "brew upgrade --cask --greedy",
      platform: "mac",
    },
    {
      id: "untracked_apps",
      label: "Untracked Apps",
      description: "Apps in /Applications not managed by Homebrew or the App Store",
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
  const itemSections = new Set(["macos_updates", "brew_casks", "app_store"]);

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
    return [];
  }

  onMount(async () => {
    currentPlatform = await invoke<string>("get_platform");
    appVersion = await getVersion();

    loadLastChecked();

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

  async function runSection(id: string) {
    outputs[id] = [];
    outputs = outputs;
    parsedItems[id] = [];
    parsedItems = parsedItems;
    selectedItems[id] = [];
    selectedItems = selectedItems;
    viewMode[id] = "readonly";
    viewMode = viewMode;
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

  async function runUpgradeItems(id: string, items: string[]) {
    upgradeLogs[id] = [];
    upgradeLogs = upgradeLogs;
    viewMode[id] = "upgrade";
    viewMode = viewMode;
    upgradeStatuses[id] = "running";
    upgradeStatuses = upgradeStatuses;
    try {
      await invoke("run_upgrade_items", { section: id, items });
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

  let outputEl: HTMLElement | null = null;

  afterUpdate(() => {
    if (outputEl) outputEl.scrollTop = outputEl.scrollHeight;
  });

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
    <button class="run-all" onclick={runAll} disabled={runningAll}>
      {runningAll ? "Checking…" : "Check All"}
    </button>
  </header>

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
          {#if activeHasItemSelection && activeStatus === "done" && activeParsedItems.length > 0}
            <button
              class="update-btn"
              onclick={() => runUpgradeItems(activeSectionId, activeSelectedItems)}
              disabled={activeUpgradeStatus === "running" || activeSelectedItems.length === 0}
            >
              {activeUpgradeStatus === "running" ? "Updating…" : `Update Selected (${activeSelectedItems.length})`}
            </button>
          {:else if activeSection.upgradeCmd && activeStatus === "done" && activeHasOutdated && !activeHasItemSelection}
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
        </div>
      {:else if activeViewMode === "upgrade"}
        <div class="output" bind:this={outputEl}>
          {#if activeUpgradeLines.length === 0}
            <p class="empty">No updates have been run yet.</p>
          {:else}
            {#each activeUpgradeLines as line}
              <div class="line">{line}</div>
            {/each}
          {/if}
        </div>
      {:else}
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
    gap: 1rem;
    margin-bottom: 1.5rem;
    flex-wrap: wrap;
    padding-bottom: 1.25rem;
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
  .subtitle { font-size: 0.78rem; color: #3d5166; width: 100%; margin-top: -0.85rem; }

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
    flex-shrink: 0;
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
