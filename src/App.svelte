<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getVersion } from "@tauri-apps/api/app";
  import { onMount } from "svelte";

  type Status = "idle" | "running" | "done" | "error";

  interface Section {
    id: string;
    label: string;
    description: string;
    upgradeCmd?: string;
    platform: "all" | "mac" | "linux" | "windows";
  }

  const sections: Section[] = [
    {
      id: "macos_updates",
      label: "macOS System Updates",
      description: "Checks for OS-level updates via softwareupdate",
      upgradeCmd: "sudo softwareupdate -ia",
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
      id: "app_store",
      label: "App Store",
      description: "Apps installed via the Mac App Store",
      upgradeCmd: "mas upgrade",
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
      label: "Homebrew Formulae",
      description: "Command-line tools installed via Homebrew",
      upgradeCmd: "brew upgrade",
      platform: "mac",
    },
    {
      id: "npm_globals",
      label: "npm",
      description: "Globally installed Node packages",
      upgradeCmd: "npm update -g",
      platform: "all",
    },
    {
      id: "pip_packages",
      label: "pip",
      description: "Outdated Python packages",
      upgradeCmd: "pip3 install --upgrade $(pip3 list --outdated --format=freeze | cut -d= -f1 | tr '\\n' ' ')",
      platform: "all",
    },
    {
      id: "ruby_rvm",
      label: "Ruby (rvm)",
      description: "Ruby versions and gems managed by rvm",
      platform: "all",
    },
    {
      id: "ruby_rbenv",
      label: "Ruby (rbenv)",
      description: "Ruby versions and gems managed by rbenv",
      platform: "all",
    },
  ];

  let statuses: Record<string, Status> = {};
  let outputs: Record<string, string[]> = {};
  let lastChecked: Record<string, Date | null> = {};
  let currentPlatform = "mac";
  let runningAll = false;
  let activeTab = "";
  let appVersion = "";

  sections.forEach((s) => {
    statuses[s.id] = "idle";
    outputs[s.id] = [];
    lastChecked[s.id] = null;
  });

  onMount(async () => {
    currentPlatform = await invoke<string>("get_platform");
    appVersion = await getVersion();

    const visible = sections.filter(platformVisible);
    if (visible.length > 0) activeTab = visible[0].id;

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
      }
    });
  });

  async function runSection(id: string) {
    outputs[id] = [];
    outputs = outputs;
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
      activeTab = s.id;
      await runSection(s.id);
    }
    runningAll = false;
  }

  function copyCmd(cmd: string) {
    navigator.clipboard.writeText(cmd);
  }

  function platformVisible(s: Section) {
    return s.platform === "all" || s.platform === currentPlatform;
  }

  const statusColor: Record<Status, string> = {
    idle: "#4a5568",
    running: "#3b82f6",
    done: "#22c55e",
    error: "#ef4444",
  };

  $: activeSection = sections.find((s) => s.id === activeTab);
  $: activeStatus = activeTab ? statuses[activeTab] : "idle";
  $: activeLines = activeTab ? outputs[activeTab] : [];
  $: activeLastChecked = activeTab ? lastChecked[activeTab] : null;

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
      <h1>BedWatch Updater</h1>
      {#if appVersion}
        <span class="version">v{appVersion}</span>
      {/if}
    </div>
    <p class="subtitle">Report-only — nothing is installed automatically</p>
    <button class="run-all" onclick={runAll} disabled={runningAll}>
      {runningAll ? "Checking…" : "Check All"}
    </button>
  </header>

  <div class="tab-bar">
    {#each sections.filter(platformVisible) as section (section.id)}
      {@const st = statuses[section.id]}
      <button
        class="tab"
        class:active={activeTab === section.id}
        class:running={st === "running"}
        class:done={st === "done"}
        class:error={st === "error"}
        onclick={() => (activeTab = section.id)}
      >
        <span class="tab-dot" style="background:{statusColor[st]}"></span>
        {section.label}
      </button>
    {/each}
  </div>

  {#if activeSection}
    <div class="panel" class:running={activeStatus === "running"} class:done={activeStatus === "done"} class:error={activeStatus === "error"}>
      <div class="panel-header">
        <div>
          <h2>{activeSection.label}</h2>
          <p class="desc">{activeSection.description}</p>
          {#if activeLastChecked}
            <p class="last-checked">Last checked at {formatTime(activeLastChecked)}</p>
          {/if}
        </div>
        <div class="panel-actions">
          {#if activeSection.upgradeCmd && activeStatus === "done"}
            <button class="copy-btn" onclick={() => copyCmd(activeSection!.upgradeCmd!)}>
              Copy upgrade command
            </button>
          {/if}
          <button
            class="check-btn"
            onclick={() => runSection(activeTab)}
            disabled={activeStatus === "running"}
          >
            {activeStatus === "running" ? "Running…" : "Run Check"}
          </button>
        </div>
      </div>

      <div class="output">
        {#if activeLines.length === 0}
          <p class="empty">Press "Run Check" to check for updates.</p>
        {:else}
          {#each activeLines as line}
            <div class="line">{line}</div>
          {/each}
        {/if}
      </div>
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
    background: #0f1117;
    color: #e2e8f0;
  }

  main { max-width: 860px; margin: 0 auto; padding: 1.5rem 1rem 1.5rem; display: flex; flex-direction: column; height: 100vh; overflow: hidden; }

  header {
    display: flex;
    align-items: center;
    gap: 1rem;
    margin-bottom: 1.5rem;
    flex-wrap: wrap;
  }

  .title-block { display: flex; align-items: baseline; gap: 0.5rem; flex: 1; }
  h1 { font-size: 1.4rem; font-weight: 700; color: #f8fafc; }
  .version { font-size: 0.75rem; color: #4a5568; font-weight: 500; }
  .subtitle { font-size: 0.78rem; color: #4a5568; width: 100%; margin-top: -1rem; }

  .run-all {
    background: #3b82f6;
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
  .run-all:hover:not(:disabled) { background: #2563eb; }
  .run-all:disabled { opacity: 0.5; cursor: not-allowed; }

  /* ── Tab bar ── */
  .tab-bar {
    display: flex;
    gap: 2px;
    overflow-x: auto;
    border-bottom: 1px solid #2d3748;
    padding-bottom: 0;
    scrollbar-width: none;
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
    color: #64748b;
    font-size: 0.82rem;
    font-weight: 500;
    cursor: pointer;
    white-space: nowrap;
    transition: color 0.15s, border-color 0.15s;
    margin-bottom: -1px;
  }
  .tab:hover { color: #cbd5e1; }
  .tab.active { color: #f1f5f9; border-bottom-color: #3b82f6; }
  .tab.running { color: #3b82f6; }
  .tab.done { color: #94a3b8; }
  .tab.error { color: #ef4444; }

  .tab-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    flex-shrink: 0;
    transition: background 0.2s;
  }

  /* ── Panel ── */
  .panel {
    background: #1e2330;
    border: 1px solid #2d3748;
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
  .panel.running { border-color: #3b82f6; }
  .panel.done { border-color: #22c55e44; }
  .panel.error { border-color: #ef444444; }

  .panel-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 1rem 1.25rem;
    border-bottom: 1px solid #2d3748;
    gap: 1rem;
    flex-wrap: wrap;
  }

  h2 { font-size: 1rem; font-weight: 600; color: #f1f5f9; }
  .desc { font-size: 0.78rem; color: #64748b; margin-top: 0.2rem; }

  .panel-actions { display: flex; gap: 0.5rem; align-items: center; flex-shrink: 0; }

  .check-btn {
    background: #3b82f6;
    color: white;
    border: none;
    border-radius: 7px;
    padding: 0.45rem 1rem;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s;
  }
  .check-btn:hover:not(:disabled) { background: #2563eb; }
  .check-btn:disabled { opacity: 0.5; cursor: not-allowed; }

  .copy-btn {
    background: #14532d33;
    color: #4ade80;
    border: 1px solid #16a34a66;
    border-radius: 7px;
    padding: 0.45rem 1rem;
    font-size: 0.82rem;
    cursor: pointer;
    transition: background 0.15s;
  }
  .copy-btn:hover { background: #14532d66; }

  .output {
    flex: 1;
    background: #0f1117;
    padding: 1rem 1.25rem;
    overflow-y: auto;
    font-family: "SF Mono", "Fira Code", monospace;
    font-size: 0.8rem;
    line-height: 1.7;
  }

  .last-checked { font-size: 0.72rem; color: #4a5568; margin-top: 0.25rem; }
  .empty { color: #4a5568; font-style: italic; font-family: inherit; font-size: 0.85rem; }
  .line { white-space: pre-wrap; word-break: break-all; color: #94a3b8; }
</style>
