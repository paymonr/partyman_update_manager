// Order matters only for readability: html.km out-specifies the kit's :root, so
// the theme layer wins regardless. Applied before mount so the first paint is
// already in the chosen theme.
import "./themes/kawaii-meadow-tokens.css";
import "./themes/themes.css";

import App from "./App.svelte";
import { mount } from "svelte";
import { applyTheme, loadTheme } from "./themes/theme";

applyTheme(loadTheme());

const app = mount(App, { target: document.getElementById("app")! });

export default app;
