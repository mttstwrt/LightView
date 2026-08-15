# 0010 — Re-arm the thumbnail drain on completion; do not widen its concurrency

[← docs index](../README.md) · [frontend](../frontend/README.md) · [grid loading](../frontend/grid-loading.md)

## Context

The open-work item that became this decision was written as "a worker pool for
image decode on the client — a single decode worker is fine in practice, but a
small pool would remove the JS orchestration bottleneck under burst load". Its
premise did not survive contact with the code: there is no decode worker to
pool. Nothing in `src-solidjs/` constructs a `Worker`, the only two
`createImageBitmap` calls are the clipboard copy and the GIF atlas, and
`thumbSwap.ts` uses `img.decode()` — a browser facility, not a worker of ours.

Restating it split it in two, and this decision is about the half that is
orchestration rather than decode: is there a real bottleneck in how the grids
issue thumbnail work, and if so, is it concurrency?

There is a real bottleneck, and it is not concurrency. `drainQueued()` issued
one batch and returned; the single-flight slot was cleared in a `finally` that
called nothing further. The only things that started another pass were
`scrollend`, the settle effect, a generation bump, and a 500 ms interval. So
against a still viewport with a deep queue the pipeline ran batch, up to half a
second of idle backend, batch — the backend finishing early bought nothing at
all.

Enqueueing had the mirror image, with a structural cause worth recording:
`handleThumbError` lives at component scope and `scheduleFetch` was a closure
inside `onMount`, so a 404 could not reach the schedule even to ask for a pass.

**How much of a gallery reaches that queue is the part that surprises.** Both
`lightview://thumb` and `GET /thumb` resolve through
`thumb_serve::get_or_generate`, which generates a missing thumbnail inside the
request and returns 404 only once generation has actually failed. A cold
gallery therefore fills from the inline generate-on-miss and from the
frontend's speculative batch warms; what reaches `needsGeneration` is the
residue of sources that could not be decoded at all. Measured over a cold
1200-image gallery in Chromium: 880 thumbnail responses, zero 404s, and the
same figure on the pre-change frontend.

That does not make the gap harmless — a queue that only fills on failure is
exactly the one you want draining briskly, and half a second of dead time per
batch is a poor way to recover — but it does bound the claim. This decision
buys recovery latency, not browsing speed, and anything measured as "cold fill
got faster" is measuring the inline path instead.

## Options considered

**More single-flight slots (the item as written).** Everything the grids issue
lands on one bounded rayon pool in the backend, and nothing can preempt a batch
once issued — that is the premise of the "speculation is never free" invariant
in [`frontend/`](../frontend/README.md) and of the deliberately small
speculative batch size. Adding slots puts more un-preemptable work in flight
against a pool that is already saturated, which raises the worst-case delay
before a scroll onto cold cells gets CPU back. It optimizes the wrong number.

**A shorter poll interval.** Cuts the idle gap without touching the design, and
costs an eviction pass plus a queue scan on every tick whether or not there is
anything to do. It also does not fix enqueue latency, only its magnitude, and
the correct interval depends on how long a batch takes — which varies by tier,
by gallery, and by machine.

**Re-arm the loop from the events that actually mean "there is more to do".** A
batch settling and a fresh miss arriving are exactly those events, and both are
already observable at the point where the work happens.

## Decision

`lib/fetchLoop.ts` owns the two slots and the schedule. Its `fetch` slot pokes
the loop when a batch settles, so a non-empty queue drains back-to-back instead
of one batch per poll; `poke()` is also what a genuinely fresh miss calls, which
is possible because the loop is constructed at component scope where
`handleThumbError` can see it. The 500 ms poll stays, now covering only the
states no event reports.

Pokes coalesce through a zero-delay timer rather than a microtask. Each `<img>`
error handler is its own task, and a microtask runs at the end of the task that
scheduled it — so a microtask would merge nothing, and a screenful of misses
would still be a screenful of passes.

The **warm slot is deliberately not re-armed.** Re-arming it would turn the
background crawl from one batch per poll into a continuous one, on the same
bounded pool as the cells the user is looking at. The visible drain running
back-to-back while speculation stays on the leash is the intended priority, and
it is the same reasoning that gave speculation its own slot in the first place.

## Consequences

- Draining is bounded by backend throughput rather than by the poll interval.
  The gap that used to sit between batches is gone. Measured against the loop
  in isolation, with five synthetic 20 ms batches queued: 106 ms end to end,
  where the poll alone would have needed more than 2 s. A hundred pokes issued
  across two separate tasks collapsed to a single pass, and a `work` function
  throwing synchronously released the slot and re-armed rather than wedging it.
- The end-to-end effect is on recovery from failed generation, and is not
  visible in ordinary browsing — see the note on `get_or_generate` above, and
  the section it points at in
  [`grid-loading.md`](../frontend/grid-loading.md). Driving the built SPA
  against `lightview-headless` over a cold 1200-image gallery shows both grids
  painting, streaming on scroll, evicting to a bounded DOM and returning to the
  top intact, identically before and after — which is the correct result for a
  refactor, and is *not* evidence that the re-arm helps browsing.
- The loop can now spin harder than before by construction: every completed
  batch schedules another pass. It terminates because `take()` removes what it
  issues, so the queue shrinks monotonically and a pass that drains nothing
  falls through to speculation, which is not re-armed.
- Anything added to the drain path is now on a hot loop. A new per-pass cost
  that was invisible at two passes a second will not be.
- `evictFaraway` runs on every pass, so it runs more often than it used to.
  It walks the assigned-rung map, which is bounded by the rendered window; this
  was already JustifiedGrid's behaviour, where the poll was unconditional.
- The decode half of the original item is untouched and still open — see D2 in
  [`todo.md`](../todo.md). Its case rests on WebKitGTK's main-thread decoding
  and on `ImageBitmap.close()` giving back memory the browser's image cache
  will not, and it needs a measurement on the web client before any code.
- If a future measurement does show the backend pool going idle mid-drain, the
  answer is a smaller batch, not another slot: the batch size is what bounds
  re-prioritization, and this decision does not stand in the way of changing it.
