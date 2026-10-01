// Theme selection. A theme is one `data-theme` attribute on <html>; themes.css
// redefines the --pm-* colour roles under it. Slate is the default and carries
// no attribute, so the app looks as it always has unless a theme is picked.

export type ThemeId = "slate" | "light" | "dark" | "warm-dark";

export const THEMES: { id: ThemeId; label: string; note: string }[] = [
  { id: "slate", label: "Slate", note: "Cool blue-grey, the original look" },
  { id: "light", label: "Light", note: "Neutral greys, light" },
  { id: "dark", label: "Dark", note: "Neutral greys, dark" },
  { id: "warm-dark", label: "Warm Dark", note: "Dark greys with a little warmth" },
];

const STORAGE_KEY = "theme";

export function loadTheme(): ThemeId {
  const saved = localStorage.getItem(STORAGE_KEY);
  return THEMES.some((t) => t.id === saved) ? (saved as ThemeId) : "slate";
}

export function saveTheme(theme: ThemeId) {
  localStorage.setItem(STORAGE_KEY, theme);
}

export function applyTheme(theme: ThemeId) {
  const html = document.documentElement;
  if (theme === "slate") html.removeAttribute("data-theme");
  else html.setAttribute("data-theme", theme);
}
