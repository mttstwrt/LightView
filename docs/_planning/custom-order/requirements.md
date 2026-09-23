# Custom order

## Background

Every sort the grid offers is a column: capture date, name, size, rating, and
the three recency dates. For a library whose file names carry no order —
downloads named by hash, `IMG_` numbers from two cameras, pages scanned out of
sequence — none of them can express the arrangement a person has in mind, and
there is nothing they can do about it short of renaming files in a tree
LightView promises to leave alone.

[`query/`](../../query/README.md#sets) decided against a per-member ordinal on
the grounds that "the filename already answers" the order. The planning doc
behind that decision (commit `0356b69`) named the bar for reopening it: *"If a
set genuinely needs an order its filenames do not carry, that is the moment to
add an ordinal — not before."* This is that case.

The obvious encoding, an ordinal inside the tag (`set::comic:3`), is not
available. Every mechanism that makes a set a set compares the tag string
exactly — the filter's `ti.tag = ?`, the duplicate finder's co-membership check,
autocomplete, rename, merge — so each page would become a set of its own. An
older LightView reading the same gallery would see two hundred one-member sets
and nothing would say so.

The governing sentence: **the person arranges; everything they did not arrange
stays where the date order puts it.**

## Requirements

### R1 — A Custom sort

A sort named Custom, alongside the column sorts. Before anyone arranges
anything it is identical to Date, newest first.

### R2 — A file keeps the place it was given

Moving a file puts it immediately after the file it was dropped behind, and it
stays there across restarts, cache deletion, and other machines opening the same
gallery. Files nobody moved keep their date order, and a new file lands where its
date puts it.

### R3 — A set given an order is one block

A set becomes a **block** when it is given an order. Under Custom its members
are contiguous, in that order, and the block sits where it was put. A set with
no order — a burst, a people cluster such as `set::alice` spanning ten years —
stays loose, and its members sort as individual files.

### R4 — A file is in at most one block

A file appears once in the grid, so it cannot sit contiguously inside two
blocks. Ordering a set that would put a file into a second block is refused, and
the refusal names the file and the block it is already in.

### R5 — Dragging a member: inside reorders, outside moves the block

Dropping a block member into a gap touching its own block reorders the set.
Dropping it anywhere else moves the whole block. A member never leaves its set
by being dragged.

### R6 — A drop means the same thing under a filter

In a filtered view, dropping a file between two visible neighbours places it
immediately after the first of them in the full order. Files the filter hides
are never jumped over or reordered by a drop they could not see.

### R7 — Another machine's arrangement arrives

A reorder made on one machine reaches every open window on every other machine
sharing the gallery, as a refetch, without anyone touching anything.

### R8 — Nothing a person cannot regenerate is spent on it

The feature must not cost a user who never uses it anything but time. In
particular it must not delete `date_added`, `last_viewed` or `date_rated` from a
gallery that has no sidecars — which a cache format bump does.

### R9 — Mouse drag on the desktop; "Move to…" everywhere

Arranging by mouse drag in the grid on the desktop. On a phone, and as a
keyboardless path on the desktop, a "Move to…" command reaches every placement a
drag can.

## Out of scope

- **Touch drag.** Long-press already opens the context menu on touch
  (`ThumbnailCell.tsx`), and a drag in a scrolling virtualized grid fights both
  it and the scroll. Its own plan, later.
- **One file in two ordered sets with different orders.** R4 forbids it; the
  data shape makes it unrepresentable rather than checked.
- **A custom order per filter or per folder.** One arrangement per gallery.
- **Reordering from inside the viewer.** The viewer tracks an index, and the
  commands live in the grid.

## Decisions taken with the user

- Unplaced files follow the date order; new files slot in by date. Accepted weak
  spot: a placed file can drift from a neighbour whose date is later re-read or
  which is renamed, because it was placed relative to that neighbour's key.
- Only ordered sets lock.
- Inside reorders, outside moves the block.
- Desktop mouse drag first.

## Open

**Is "drop at the very top" a pin?** A key below every existing key also sorts
above every file that arrives later, so an image dropped at the top stays there
forever. An image dropped just after today's first image is anchored to that
neighbour instead, and slides down as new files arrive. The two drops look
identical on the day they are made. The design currently pins. The alternative
gives a top drop a key just below the current first file's, so it behaves as the
newest file in the gallery and slides down as later files arrive.
