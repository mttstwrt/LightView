// What the gallery is busy with: installed plugins, the current plugin run, and
// thumbnail work the user asked for.
//
// Three stores became one. They were separate because the old tagging model had
// a distributed job queue with a worker roster and a job list, and thumbnail
// progress was a third thing reported over a third channel. There is one
// in-process executor and one event stream now, so a run is a plugin name and a
// pair of counters, and keeping three stores in step for that would be three
// places to forget.

import { createSignal } from "solid-js";

import { api } from "../lib/ipc";
import type { PluginInfo, ServerEvent } from "../lib/types";

const [plugins, setPlugins] = createSignal<PluginInfo[]>([]);

/** The run in progress, if any. There is at most one: the executor is
 *  in-process, and a second run would contend for the same bounded pool. */
export interface Run {
  plugin: string;
  done: number;
  total: number;
}

const [run, setRun] = createSignal<Run | null>(null);
const [lastError, setLastError] = createSignal<string | null>(null);

/** Thumbnail generation the grid is waiting on, or null when it has gone
 *  quiet. The grid is the only thing that knows — a cell 404s, the path is
 *  queued, a batch lands — so it reports and this only holds the counters for
 *  the indicator to read. */
export interface ThumbWork {
  done: number;
  total: number;
}

const [thumbWork, setThumbWork] = createSignal<ThumbWork | null>(null);

export { plugins, run, lastError, thumbWork, setThumbWork };

/** Load the installed plugins.
 *
 *  Under `--serve` this is empty and the panel says so: plugins are not
 *  installed on the server, and the models could not run there anyway. That is
 *  an honest absence rather than a disabled control. */
export async function loadPlugins() {
  try {
    setPlugins(await api.listPlugins());
  } catch {
    setPlugins([]);
  }
}

/** Apply one server event. Progress is throttled server-side to at most one a
 *  second, so this can be as naive as it looks. */
export function applyEvent(event: ServerEvent) {
  switch (event.kind) {
    case "job-progress":
      setRun({ plugin: event.plugin, done: event.done, total: event.total });
      break;
    case "job-finished":
      setRun(null);
      setLastError(event.error);
      break;
    case "resync":
      if (event.domains.includes("jobs")) void loadPlugins();
      break;
    default:
      break;
  }
}
