# Chrome: commands, settings, and the space they compete for

[← docs index](../README.md) · [frontend](README.md)

The app's non-browsing UI — the settings panel and the buttons floating over the
grid — grew one section and one control at a time, and stopped distinguishing
between things you *do* and things you *set*. This page is the plan for pulling
those apart. It is not built yet; the open work entry is
[`todo.md` D3](../todo.md#d3-commands-and-settings-are-the-same-drawer-on-both-surfaces).

## What is there now

`SettingsMenu.tsx` is 1,705 lines rendering thirteen `Section`s. Three are a
heading plus a single button — Tags → "Manage Tags…", Deduplication → "Find
Duplicates…", Trash → "View Trash…" — each spending a section header on one
verb. On mobile the panel is a full-screen page, so scrolling past configuration
to reach a button pressed daily costs the whole screen.

The mobile chrome drifted the same way with less room to absorb it. Four
controls float over an edge-to-edge grid: search (top-left), select and the gear
(top-right), and the web client's upload FAB (bottom-right). The FAB is the
tell — it carries three separate hide conditions (`viewerOpen`, mobile selection
mode, mobile selection non-empty) because it is one button competing for the
bottom edge with the selection bar and the video player's controls. There is no
room for a fifth, so any design that adds chrome per action is ruled out on the
surface that needs it most.

## One command list, two presentations

Declare the commands once as data — label, icon, an availability predicate
(`capabilities()`, `isWeb()`, desktop-only) and a handler — and render that one
list two ways. The settings panel is not a third rendering of it; it keeps only
configuration, which is what makes it shrink.

| Surface | Control | Opens |
|---|---|---|
| Desktop | one icon appended to the existing top-bar row, in the gear's place | a dropdown, the shape `SettingsMenu` already uses |
| Mobile | one FAB in the thumb-zone corner, in the upload FAB's place | a sheet, the shape the filter/sort controls already use |

Both are **substitutions, not additions**: the desktop icon replaces the gear,
the mobile FAB replaces the upload button. Neither surface gains a control, and
no new kind of container is introduced — a dropdown and a sheet both already
exist. That is deliberate. A second top-bar row, a rail, or a strip of one icon
per command would each be a new piece of UI to design, lay out and keep
responsive, and the point of this work is to have *less* chrome, not more of it
arranged differently.

The commands: manage tags, find duplicates, view trash, upload, run a plugin,
open a folder, and settings.

**Settings is the last entry, not a button beside the list.** Opening the
settings panel is a command like any other, so treating it as one keeps the rule
free of exceptions — and on mobile it takes the phone from four floating
controls to three. It costs settings a second tap, which is the premise of the
whole split: rare things are allowed to be further away.

**Selection-scoped actions stay out.** `SelectionBar` and `ContextMenu` already
own everything that acts on a selection, and hoisting those into a global list
would break the one thing the mobile chrome currently gets right. It follows
that the actions button inheriting the FAB's hide-while-selecting rule is
correct rather than a gap.

## On a phone, frequency maps to reachability

The bottom-right FAB position is the thumb zone; the top corners are not. That
hierarchy is already half-built by accident, and this makes it the rule:

| Position | Holds | Because |
|---|---|---|
| Bottom-right (thumb) | the command list | used every session |
| Top-right | select | a mode toggle, touch's only route into multi-select |
| Top-left | search / filter | frequent, but a deliberate context switch |
| Inside the command list | settings | rare |

This is the same do-versus-set split as the settings panel, expressed as
geometry instead of section order.

## Two constraints

These are what make folding settings into the list safe rather than merely
tidy. Both are easy to violate by copying the existing FAB.

**The button and the settings entry must render from local state alone.** The
upload FAB today is `<Show when={config()?.enabled}>` — a server-fetched gate
that removes the whole control when the fetch fails. Inheriting that would
delete the only route to Settings whenever the server is unreachable, and
Settings → Connection is exactly where "Install server certificate" and "Reset
connection" live: the recovery actions for an unreachable server. Individual
entries may be gated on fetched config; the button and the settings entry may
not.

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

## Ordering, and why the current scheme failed

`Section` takes an `order` prop so visual position can be set independently of
source order, and it was used deliberately to float the most-used sections to
the top. It stopped being respected as sections were added: four values now
collide (three at `1`, two each at `2`, `3` and `4`), ties fall back to source
order, and the decoupling the prop exists for is silently gone.

The intent was right and the mechanism was wrong. A magic number per call site,
spread across 1,705 lines, cannot stay consistent because nobody ever sees the
thirteen numbers together — only the one they are typing. The replacement is the
same for both halves of this work: **one ordered list in one place, where
position is the order.** Adding a fourteenth entry then means looking at the
other thirteen, which is the property the numbers never had.

The command list is that structure for the commands. The settings panel wants
the same treatment for what remains, at which point `Section` loses its `order`
prop entirely.

## Loose ends this also closes

- **"Gallery" is not a `Section`** but a bare `<div style={{ order: 8 }}>` with
  a border — the one pattern the panel has already has an exception. It becomes
  the "open a folder" command and the div goes.
- **The FAB's three hide conditions** disappear with the FAB: upload becomes a
  list entry, and the list's own visibility rules replace them.

## Non-issues, checked

- **The viewer.** Folding the gear into the FAB slot does not make settings
  unreachable while the viewer is open — it already is. `MediaViewer` is `z-50`
  and the mobile chrome is `z-40`, so the gear is covered today.
- **Selection mode.** See above: the actions button hiding while the selection
  bar is up follows from selection actions living elsewhere.
