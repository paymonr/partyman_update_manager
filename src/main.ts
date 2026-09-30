import "./themes/themes.css";

import App from "./App.svelte";
import { mount } from "svelte";
import { applyTheme, loadTheme } from "./themes/theme";

// Applied before mount so the first paint is already in the chosen theme.
applyTheme(loadTheme());

const app = mount(App, { target: document.getElementById("app")! });

export default app;
