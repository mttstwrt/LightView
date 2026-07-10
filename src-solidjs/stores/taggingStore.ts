// Remote-tagging state for the web client: which workers are connected (and
// what plugins they offer), plus the server's job list. Fed by the
// `tagging-workers` / `tagging-job` SSE events (App.tsx) with
// `refreshTaggingStatus()` as the on-demand re-sync (menu open, SSE lag).
//
// The active job is bridged into pluginStore's toast so worker jobs get the
// same bottom-right progress UI as desktop plugin runs. Only one job drives
// the toast at a time; others still show in the Settings job list.

import { createSignal } from "solid-js";
import {
  getTaggingStatus,
  type TaggingJob,
  type WorkerStatus,
} from "../lib/ipc";
import type { PluginInfo } from "../lib/types";
import {
  pluginStarted,
  pluginProgress,
  pluginFinished,
  pluginFailed,
  pluginCancelled,
} from "./pluginStore";

const [workers, setWorkers] = createSignal<WorkerStatus[]>([]);
const [jobs, setJobs] = createSignal<TaggingJob[]>([]);

export { workers as taggingWorkers, jobs as taggingJobs };

let activeJobId: string | null = null;

export const activeTaggingJobId = () => activeJobId;

/** Deduped union of every connected worker's plugins — what "tag with a
 * worker" can offer right now. */
export function workerPlugins(): PluginInfo[] {
  const seen = new Map<string, PluginInfo>();
  for (const w of workers()) {
    for (const p of w.plugins) {
      if (!seen.has(p.name)) seen.set(p.name, p);
    }
  }
  return Array.from(seen.values());
}

/** One runnable menu entry per (plugin, place-to-run-it). A plugin offered by
 * a single worker gets one unpinned entry (any future worker with it may
 * claim); a plugin offered by several — e.g. installed both on the server and
 * on a remote worker — gets one entry per worker, pinned, so the user picks
 * where it runs. */
export interface TaggingAction {
  plugin: PluginInfo;
  /** Label suffix, e.g. "on Server (nas)" — empty when there's no choice. */
  where: string;
  /** Pass to enqueueTaggingJob to pin the job; undefined = first claimer. */
  workerId?: string;
}

export function taggingActions(): TaggingAction[] {
  const byPlugin = new Map<string, { plugin: PluginInfo; offeredBy: typeof list }>();
  const list = workers();
  for (const w of list) {
    for (const p of w.plugins) {
      const entry = byPlugin.get(p.name) ?? { plugin: p, offeredBy: [] };
      entry.offeredBy.push(w);
      byPlugin.set(p.name, entry);
    }
  }
  const actions: TaggingAction[] = [];
  for (const { plugin, offeredBy } of byPlugin.values()) {
    if (offeredBy.length === 1) {
      actions.push({ plugin, where: "" });
    } else {
      for (const w of offeredBy) {
        actions.push({ plugin, where: `on ${w.workerName}`, workerId: w.workerId });
      }
    }
  }
  return actions;
}

export function applyWorkersEvent(next: WorkerStatus[]) {
  setWorkers(next);
}

/** Start following a job this client just enqueued, so the toast shows
 * feedback before the first SSE `running` snapshot arrives. */
export function trackQueuedJob(job: TaggingJob) {
  activeJobId = job.id;
  pluginStarted(job.pluginName, job.displayName, job.total);
  applyJobEvent(job, /* fromEnqueue */ true);
}

/** Merge one job snapshot (SSE or local) into the list and drive the toast. */
export function applyJobEvent(job: TaggingJob, fromEnqueue = false) {
  setJobs((prev) => {
    const idx = prev.findIndex((j) => j.id === job.id);
    if (idx === -1) return [...prev, job];
    const next = prev.slice();
    next[idx] = job;
    return next;
  });

  switch (job.state) {
    case "queued":
      // A running job we were following got requeued (worker stalled).
      if (!fromEnqueue && activeJobId === job.id) {
        activeJobId = null;
        pluginFailed("Worker lost — job requeued");
      }
      break;
    case "running":
      // Adopt any running job if nothing else drives the toast, so progress
      // shows even for jobs enqueued by another device.
      if (activeJobId === null) {
        activeJobId = job.id;
        pluginStarted(job.pluginName, job.displayName, job.total);
      }
      if (activeJobId === job.id) {
        pluginProgress(job.completed + job.failed, job.total);
      }
      break;
    case "done":
      if (activeJobId === job.id) {
        activeJobId = null;
        if (job.failed > 0) {
          pluginFailed(`${job.completed} tagged, ${job.failed} failed`);
        } else if (job.total === 0) {
          pluginFinished("Nothing left to tag");
        } else {
          pluginFinished(`Tagged ${job.completed} files`);
        }
      }
      break;
    case "failed":
      if (activeJobId === job.id) {
        activeJobId = null;
        pluginFailed(job.error ?? "Tagging failed");
      }
      break;
    case "cancelled":
      if (activeJobId === job.id) {
        activeJobId = null;
        pluginCancelled();
      }
      break;
  }
}

/** Full re-sync from the server. Cheap; also runs the server's lazy reaper. */
export async function refreshTaggingStatus() {
  try {
    const status = await getTaggingStatus();
    setWorkers(status.workers);
    setJobs(status.jobs);
  } catch {
    // Not fatal — desktop builds and unpaired sessions just have no data.
  }
}
