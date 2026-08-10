# Chrome: commands, settings, and the space they compete for

[← docs index](../README.md) · [frontend](README.md)

The app's non-browsing UI — the settings panel and the buttons floating over the
grid — grew one section and one control at a time, and stopped distinguishing
between things you *do* and things you *set*. This page describes the split that
fixed that, and the constraints that keep it fixed.

**Responsible for:** what is offered outside the grid and the viewer, where it
sits on each surface, and in what order. **Not responsible for:** what any of it
does — the panels own that, and `App` owns their open state.

**Public interface:** `CommandMenu.tsx` exports `CommandHandlers` (the seven
things a command can run), the two renderings `CommandMenu` / `CommandFab`, and
the `commandsOpen` signal that `TopBar` reads to wire the mobile back gesture.

**Invariants callers must uphold** are the two under
"[Two constraints](#two-constraints)" below, plus: nothing that acts on a
selection belongs in the command list.

## What it was

`SettingsMenu.tsx` was 1,705 lines rendering thirteen `Section`s. Three were a
heading plus a single button — Tags → "Manage Tags…", Deduplication → "Find
Duplicates…", Trash → "View Trash…" — each spending a section header on one
verb. On mobile the panel is a full-screen page, so scrolling past configuration
to reach a button pressed daily cost the whole screen.

The mobile chrome had drifted the same way with less room to absorb it. Four
controls floated over an edge-to-edge grid: search (top-left), select and the
gear (top-right), and the web client's upload FAB (bottom-right). The FAB was
the tell — it carried three separate hide conditions (`viewerOpen`, mobile
selection mode, mobile selection non-empty) because it was one button competing
for the bottom edge with the selection bar and the video player's controls.
There was no room for a fifth, so any design that adds chrome per action was
ruled out on the surface that needs it most.

## One command list, two presentations

The commands are declared once as data in `CommandMenu.tsx` — label, icon, an
availability predicate and a handler — and that one list is rendered two ways.
The settings panel is not a third rendering of it; it keeps only configuration,
which is what made it shrink.

| Surface | Control | Opens |
|---|---|---|
| Desktop | one icon at the end of the top-bar row, in the gear's place | a dropdown, the shape `SettingsMenu` already used |
| Mobile | one FAB in the thumb-zone corner, in the upload FAB's place | a sheet, the shape the filter/sort controls already use |

Both are **substitutions, not additions**: the desktop icon replaced the gear,
the mobile FAB replaced the upload button. Neither surface gained a control, and
no new kind of container was introduced — a dropdown and a sheet both already
existed. That was deliberate. A second top-bar row, a rail, or a strip of one
icon per command would each be a new piece of UI to design, lay out and keep
responsive, and the point of this work was to have *less* chrome, not more of it
arranged differently.

The commands, in list order: upload, auto-tagging, manage tags, find
duplicates, trash, open a folder, and settings.

**Settings is the last entry, not a button beside the list.** Opening the
settings panel is a command like any other, so treating it as one keeps the rule
free of exceptions — and on mobile it took the phone from four floating controls
to three. It costs settings a second tap, which is the premise of the whole
split: rare things are allowed to be further away.

**Selection-scoped actions stay out.** `SelectionBar` and `ContextMenu` already
own everything that acts on a selection, and hoisting those into a global list
would break the one thing the mobile chrome already got right. It follows that
the actions button inheriting the FAB's hide-while-selecting rule is correct
rather than a gap.

**Both renderings run the command inside `batch()`, and must.** `TopBar` mirrors
"is a mobile overlay up?" into a history entry so the platform back gesture
closes it. Unbatched, closing the list and opening the settings page are two
reactive passes, and the instant between them reads as "no overlay up" — which
pops the entry, and the popstate then lands after settings opened and closes it
again. The symptom is that tapping Settings appears to do nothing at all.

## On a phone, frequency maps to reachability

The bottom-right FAB position is the thumb zone; the top corners are not. That
hierarchy was already half-built by accident, and this made it the rule:

| Position | Holds | Because |
|---|---|---|
| Bottom-right (thumb) | the command list | used every session |
| Top-right | select, then the view switcher | a mode toggle and a mode selector |
| Top-left | search / filter | frequent, but a deliberate context switch |
| Inside the command list | settings | rare |

This is the same do-versus-set split as the settings panel, expressed as
geometry instead of section order.

The two top-right buttons are laid out by a flex row rather than by absolute
offsets, because the view switcher removes itself entirely on a gallery with
only one view enabled and a hard-coded offset would leave a hole where it was.

## Two constraints

These are what make folding settings into the list safe rather than merely
tidy. Both are easy to violate by copying the old FAB.

**The button and the settings entry must render from local state alone.** The
upload FAB was `<Show when={config()?.enabled}>` — a server-fetched gate that
removed the whole control when the fetch failed. Inheriting that would delete
the only route to Settings whenever the server is unreachable, and Settings →
Connection is exactly where "Install server certificate" and "Reset connection"
live: the recovery actions for an unreachable server. Individual entries may be
gated on fetched config — Upload still is, via `uploadStore` — but the button
and the settings entry may not.

`ConnectionBanner` offers "Reset connection" independently once
`serverUnreachable` is set, so this is not a single point of failure. But the
certificate link has no second home, and the case that needs it is not the
outage — it is a *working* client whose click-through exception is about to
lapse.

**The icon has to mean "more", not "add".** An overflow glyph reads as
"everything else" and plausibly contains Settings; a `+` reads as create, which
over-promises upload and under-promises the rest. The cost is that upload —
probably the most-used command on a phone — moves to two taps. That is a real
cost, paid to get one extensible control instead of four fixed ones.

## Ordering, and why the previous scheme failed

`Section` took an `order` prop so visual position could be set independently of
source order, and it was used deliberately to float the most-used sections to
the top. It stopped being respected as sections were added: four values
collided (three at `1`, two each at `2`, `3` and `4`), ties fell back to source
order, and the decoupling the prop existed for was silently gone.

The intent was right and the mechanism was wrong. A magic number per call site,
spread across 1,705 lines, cannot stay consistent because nobody ever sees the
thirteen numbers together — only the one they are typing. The replacement is the
same for both halves of this work: **one ordered list in one place, where
position is the order.** Adding an entry now means looking at the others, which
is the property the numbers never had. `Section` no longer takes an `order`
prop, and the settings panel is read top to bottom in source order: Display,
Views, Thumbnails, Remote Access, Connection, Default Filter, Storage.

## Where the two stateful sections went

Plugins (desktop) and Remote Tagging (web) never collapsed to
`{label, icon, availability, handler}`. Plugins is a list over `plugins()` with
a per-plugin run button gated on whether one is already running; Remote Tagging
is a live worker roster plus a row per plugin×worker plus job progress. Both are
panels wearing a section's clothes.

They took the route tags, duplicates and trash had already taken: **one command
that opens a dedicated panel**, `AutoTagPanel`. One panel and one command rather
than two of each, because the two are the same user intent — run a tagger over
this gallery — differing only in *where* the tagger runs, and the two bodies are
never both reachable: the desktop spawns its own plugins, the web client
enqueues for a worker. So the command list gains one entry on both surfaces and
the panel branches once on `isWeb()`.

"No new container" therefore holds for the trigger and not for the body, which
was always going to be true of these two.

## The view switcher

`Section label="View"` was mobile-only and existed because the phone's top bar
cannot fit the switcher the desktop top bar carries. It is neither a command nor
configuration — it is a mode selector used constantly, so burying it two taps
deep inside a command list would have been a regression, and leaving it as the
first section of a settings page is what this whole split exists to stop.

It took the top-right corner instead, which is exactly the slot the gear vacated
by folding into the command list — so the phone still carries three floating
controls. The button's glyph tracks the current mode, and tapping it opens a
small anchored menu of the enabled views. Two taps to switch, same as before,
but from a control that says what the current view is instead of from a page
that has to be scrolled.

Rejected: putting it in the filter/sort sheet (it would make "search" mean
"search, sort and switch view", and the sheet is a deliberate context switch),
and an always-visible segmented control (permanent chrome on the surface with
the least room, which is the thing being reduced).

Note that the surviving `Section label="Views"` — the per-gallery
enable/disable list — is genuinely configuration and genuinely a different
thing. With the mobile "View" section gone, the two names no longer collide.

## Loose ends this closed

- **"Gallery" was not a `Section`** but a bare `<div style={{ order: 8 }}>` with
  a border — the one pattern the panel already had an exception for. It is the
  "open a folder" command now, and the div is gone.
- **The FAB's three hide conditions** went with the FAB. Upload is a list entry;
  the list's own visibility rules replace them, and all of those rules read
  local signals.
- **`UploadButton` is `UploadSheet`.** It no longer owns a trigger or fetches
  its own config; `uploadStore` holds the config because two unrelated things
  need it — the sheet, and the command list's availability predicate.

## Non-issues, checked

- **The viewer.** Folding the gear into the FAB slot does not make settings
  unreachable while the viewer is open — it already was. `MediaViewer` is `z-50`
  and the mobile chrome is `z-40`, so the gear was covered before too.
- **Selection mode.** See above: the actions button hiding while the selection
  bar is up follows from selection actions living elsewhere.

## What is still worth doing

`SettingsMenu` is smaller but still owns around twenty signals covering remote
access, pairing and QR, password, upload config, remote delete, render config,
and rebuild/precache progress. The plugin signals left with `AutoTagPanel`; the
rest are genuinely configuration state and have nowhere better to be until the
Remote Access section itself is worth splitting up.
