import { render } from "solid-js/web";
import "./styles/global.css";
import { DevtoolsApp } from "./components/debug/DevtoolsApp";

render(() => <DevtoolsApp />, document.getElementById("root")!);
