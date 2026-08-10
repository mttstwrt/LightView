# 0009 — Three kinds of chrome: commands, panels, and configuration

[← docs index](../README.md) · [frontend](../frontend/README.md) · [chrome](../frontend/chrome.md)

## Context

Splitting the settings drawer into things you *do* and things you *set*
([`frontend/chrome.md`](../frontend/chrome.md)) needed every one of its
thirteen sections to land on one side of that line. Eleven did. Two did not,
and they did not fail in the same way:

**Plugins (desktop) and Remote Tagging (web)** are actions, but not single
ones. Plugins is a list over the installed plugins with a run button each,
gated on whether one is already running and trailed by a status line; Remote
Tagging is a live worker roster, a row per plugin×worker, and job progress with
per-job cancel. Neither reduces to `{label, icon, availability, handler}`, so
neither could become a command — but both are unambiguously things you do, so
neither could stay in a panel that is meant to hold only configuration.

**The mobile view switcher** is neither. Grid / Justified / Map is a mode
selector, used many times a session, and it exists as a settings section only
because the phone's top bar cannot fit the segmented control the desktop top bar
carries. Put it in the command list and a constantly-used control is two taps
and a scan deep; leave it in settings and the first thing the settings page
shows is not a setting.

## Options considered

**For the two stateful sections.**

*Leave them in the settings panel as documented exceptions.* No new code, and
the panel is still much smaller than it was. Rejected: the exception sits on the
surface where the split matters most — on a phone the settings page is
full-screen, so "scroll past configuration to reach a button you press weekly"
is exactly the cost this work exists to remove, and an exception that big erodes
the rule for the next section anyone adds.

*A panel each, behind a command each.* Follows the pattern tags, duplicates and
trash already established, and keeps the two bodies entirely independent.
Rejected as more machinery than the situation earns: the two are never both
reachable — the desktop spawns plugins in-process, the web client enqueues for a
worker — so two panels would mean two components, two commands, and two
availability predicates to express "one entry, always".

*One panel behind one command.* Chosen. The user's intent is the same on both
surfaces — run a tagger over this gallery — and only *where* it runs differs, so
`AutoTagPanel` branches once on `isWeb()` and the command list gains exactly one
entry.

**For the view switcher.**

*Into the filter/sort sheet.* Zero new chrome. Rejected: it would make the
search button mean "search, sort, and switch view", and that sheet is
deliberately a context switch — the wrong gesture for something used between
every other action.

*An always-visible segmented control floating over the grid.* One tap to switch,
the best possible reachability. Rejected: it is permanent chrome on the surface
with the least room, which is the thing this work is trying to reduce, and three
labels do not fit beside the existing buttons at phone widths.

*The top-right corner the gear vacates.* Chosen. Folding settings into the
command list frees exactly one slot in the top row, and the switcher is the one
control that needs it. The button's glyph shows the current mode; tapping opens
a small anchored menu of the enabled views.

## Consequences

The taxonomy is now explicit, and it is what a new piece of chrome gets sorted
into: **a command** if it fits `{label, icon, availability, handler}`, **a
panel behind a command** if it is an action with state of its own, or
**configuration** if it is a setting. Anything that is none of the three — so
far, only the view switcher — is a mode selector, and mode selectors are placed
by reachability rather than filed into a list.

The phone keeps three floating controls, not four, because each addition here
was paid for by a removal: the command list took the upload FAB's slot, and the
view switcher took the gear's.

`AutoTagPanel` is one component with two bodies that share nothing but a header.
That is accepted: they are the same command, and a split would have to be
undone the moment the desktop can also enqueue to a remote worker.

Because the switcher hides itself entirely when a gallery has fewer than two
views enabled, the top-right buttons are laid out by a flex row rather than by
absolute offsets — a hard-coded offset would leave a hole where it used to be.
