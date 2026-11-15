import "@fontsource-variable/inter";
import "@fontsource/ibm-plex-mono/400.css";
import { mount } from "svelte";
import App from "./App.svelte";
import "./style.css";

const target = document.getElementById("app");
if (!target) throw new Error("Missing application root");
mount(App, { target });
