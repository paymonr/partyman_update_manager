// Theme selection.
//
// A theme is two attributes on <html>: the `km` class turns the kawaii-meadow
// token mapping on, and `data-theme` picks which of the kit's themes is active
// (absent means its Classic). Default is neither, so the app keeps its own
// palette. See themes.css for how the roles resolve.

export type ThemeId = "default" | "km-classic" | "km-day" | "km-night" | "km-auto";

export const THEMES: { id: ThemeId; label: string; note: string }[] = [
  { id: "default", label: "Default", note: "The original PM Updater look" },
  { id: "km-classic", label: "Kawaii Meadow — Classic", note: "Pastel surfaces on a deep ground" },
  { id: "km-day", label: "Kawaii Meadow — Day", note: "A sunlit meadow" },
  { id: "km-night", label: "Kawaii Meadow — Night", note: "Dark cards on a near-black sky" },
  { id: "km-auto", label: "Kawaii Meadow — Follow system", note: "Day or Night, matching macOS appearance" },
];

const STORAGE_KEY = "theme";
const DARK_QUERY = "(prefers-color-scheme: dark)";

export function loadTheme(): ThemeId {
  const saved = localStorage.getItem(STORAGE_KEY) as ThemeId | null;
  return THEMES.some((t) => t.id === saved) ? (saved as ThemeId) : "default";
}

export function saveTheme(theme: ThemeId) {
  localStorage.setItem(STORAGE_KEY, theme);
}

function systemPrefersDark(): boolean {
  return window.matchMedia?.(DARK_QUERY).matches ?? false;
}

// The kit's accent family. Pink is its default, but that default is expressed
// through [data-accent="pink"] selectors, and the per-theme card and text
// colours live in [data-theme="night"][data-accent="pink"] — both attributes on
// the same element. Leaving the attribute off silently drops those rules, which
// is what left Night showing white cards on a near-black page.
function accentFor(kitTheme: string | null): string {
  return kitTheme ? "purple" : "pink";
}

export function applyTheme(theme: ThemeId) {
  const html = document.documentElement;
  const km = theme !== "default";
  html.classList.toggle("km", km);

  // Classic is the kit's own default, so it carries no data-theme at all.
  const kitTheme =
    theme === "km-day" ? "day"
    : theme === "km-night" ? "night"
    : theme === "km-auto" ? (systemPrefersDark() ? "night" : "day")
    : null;

  if (km) {
    html.setAttribute("data-accent", accentFor(kitTheme));
  } else {
    html.removeAttribute("data-accent");
  }

  if (kitTheme) {
    html.setAttribute("data-theme", kitTheme);
  } else {
    html.removeAttribute("data-theme");
  }
}

// Follow-system has to keep following: macOS can flip appearance while the app is
// open, and on a schedule the user never touches.
let unwatch: (() => void) | null = null;

export function watchSystemTheme(getTheme: () => ThemeId) {
  unwatch?.();
  const mql = window.matchMedia?.(DARK_QUERY);
  if (!mql) return;
  const onChange = () => {
    if (getTheme() === "km-auto") applyTheme("km-auto");
  };
  mql.addEventListener("change", onChange);
  unwatch = () => mql.removeEventListener("change", onChange);
}
