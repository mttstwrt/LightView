// Entry point for the separate devtools window.

import { render } from "solid-js/web";
import "./styles/global.css";
import { DevtoolsApp } from "./components/debug/DevtoolsApp";

render(() => <DevtoolsApp />, document.getElementById("root")!);
