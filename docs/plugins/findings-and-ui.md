# Findings: plugin output that is not tags, and the UI that resolves it

[← docs index](../README.md) · [plugins](README.md) · [companion](../companion/README.md) · [query](../query/README.md) · [chrome](../frontend/chrome.md)

**Status:** designed, not built. This is the plan for A7 in [`todo.md`](../todo.md).
**Audience:** whoever implements it.

The protocol has one verb and one output shape: an image goes in, tags come
out. That is enough for a tagger and for nothing else. This page is the class of
plugin it cannot express, the contract those plugins need, and what the screens
actually look like.

---

## 1. Two plugins the current protocol cannot express

**Recognising faces.** A plugin can cluster faces and emit a cluster id as a
tag. It cannot be told that cluster #7 is a particular person, because there is
nowhere for a plugin to put UI and no channel for an answer to come back. The
tags it can emit are therefore permanently anonymous, which is most of the value
gone.

**Finding an image's original source.** A reverse-search plugin returns
*candidates*: a handful of URLs, each with a site, a title, an artist, a
similarity score, maybe a thumbnail. The right answer is one of them, or none of
them, and only a person can say which. Squeezing that into tags loses everything
that matters — a URL is not a tag, five ranked candidates are not five tags, and
"the user confirmed this one" has nowhere to be recorded.

The two look different and want the same three things:

1. **A result that is structured data, not a tag list.**
2. **A place for a person to see it and decide**, without opening each file one
   at a time by hand.
3. **The decision going somewhere durable, and back to the plugin** — so a
   second run does not re-ask, and so a confirmed answer becomes an ordinary
   tag, because *after* confirmation "this is Alice" is exactly a tag.

Call the middle thing a **finding**: something a plugin noticed that is not yet
a fact.

---

## 2. Kinds the host understands

A finding declares a **kind**, and the host *interprets* it. A kind is a
semantic contract — a typed payload, and a surface built for that payload. The
host knows two:

| Kind | What the plugin found | What a person decides | Surface |
|---|---|---|---|
| `source` | ranked external candidates for one file | which one is it, if any — and whether to take a better copy | candidate list in the info panel → [source dialog](#7-the-source-dialog) |
| `face-cluster` | a set of files it believes are one person | who this is; whether the cluster is right | `FacesPanel`, beside `DuplicatesPanel` |

This is the reverse of the earlier design, where the host never interpreted a
kind and a generic *shape* selected the interaction. That guaranteed generic
output: a renderer told only "this is a pick-one" cannot know that a source
candidate wants its resolution compared against the local file. Adding a kind is
a host release, and that is the trade — see
[decision 0016](../decisions/0016-plugin-findings-are-host-owned-kinds.md),
which supersedes
[0015](../decisions/0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md).

Kinds are declared in the **manifest**, not in results, so the host knows a
plugin's whole UI surface before running it:

```json
"ui": {
  "settings_schema": { … },
  "context_menu_items": [ … ],
  "findings": [
    { "kind": "source", "title": "Original source" }
  ]
}
```

A plugin declaring a kind the host does not know is **refused at install**, with
a message naming the kind — the same treatment as an unsupported `api_version`
([0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md)),
and for the same reason: a plugin whose output nothing can render must say so at
install time rather than run to completion and produce silence. A result
carrying a kind the *manifest* never declared is dropped with a log line, which
keeps "what can this plugin ask me?" answerable from the manifest alone.

## 3. What a plugin emits

`findings` sits alongside `tags` in an ordinary result line — additive, so a
plugin can tag and find in the same pass, or do only one. The `data` object is
typed by the kind:

```json
{"path": "/g/a.jpg", "tags": [], "findings": [
  {"kind": "source",
   "id": "src:9f2a",
   "data": {"candidates": [
     {"id": "danbooru:5512",
      "label": "Danbooru #5512",
      "site": "Danbooru",
      "url": "https://danbooru.donmai.us/posts/5512",
      "file_url": "https://danbooru.donmai.us/data/5512.png",
      "thumb": "https://danbooru.donmai.us/preview/5512.jpg",
      "width": 4096, "height": 4096, "bytes": 8241664,
      "similarity": 0.98,
      "detail": {"Artist": "kantoku", "Posted": "2019-04-02"},
      "tags": ["source:danbooru", "artist:kantoku"]}
   ]}}
]}
```

`id` is the plugin's own stable key for this finding, so a re-run can be matched
against a previous verdict.

The typed fields are the point of the exercise, and each one buys a specific
piece of the UI: `width`/`height`/`bytes` are what let the dialog say *"4096²,
7.9 MB — yours is 1024², 412 KB"* and enable the replace action only when the
candidate is genuinely better; `similarity` is what sorts and what greys out a
weak match; `file_url` is the image itself as distinct from the page a person
would visit, and its absence is what makes a candidate confirm-only. `detail`
survives from the old design as a free-form key→string table, because the
per-site trivia a plugin wants to show — circle, character, upload date — is
genuinely open-ended and is display-only.

A `face-cluster` finding is keyed to a cluster rather than a file, and its
payload is the member list plus per-face crop rectangles. Its storage is the
`plugin_groups` question in [§8](#8-faces-a-cluster-is-not-a-file), and it is
not in the first build.

**Where a confirmed answer goes, and why it is not the plugin's tag
namespace.** `apply_plugin_output` replaces `tags.plugins[prefix].tags`
wholesale on every run — that is what makes re-tagging idempotent, and it means
anything written there is erased by the next job. So a confirmation writes:

- the chosen candidate's `tags` into **`tags.user`**. The user affirmed them;
  they are indexed and filterable the moment they land, they survive
  re-tagging, and they are removable through the tag UI like any other user tag.
- the candidate itself (minus its thumbnail and file URLs) into
  **`meta.plugins[<prefix>].<kind>`**, which is already
  `HashMap<String, Value>` and needs no schema change. That is where provenance
  lives: which plugin found it, which candidate won, what the score was.

A name given to a `face-cluster` follows the same rule from the other end: it
becomes the tag `person:alice` on every member's companion, with spaces
underscored, by host convention rather than a template language in the manifest.

**No companion schema bump.** Both destinations already exist. That is a
deliberate constraint on the design, not a lucky accident: the companion is a
wire format other LightView installations read, and a migration is a much
larger commitment than a feature at this stage deserves.

## 4. Where the state lives

| What | Where | Why |
|---|---|---|
| Pending findings | cache DB, `plugin_findings` | Regenerable by re-running the plugin. Scaffolding, not a user edit. |
| Rejections ("none of these") | cache DB, `plugin_finding_verdicts` | Follows the `not_duplicates` precedent exactly — a remembered "no" that stops the same wrong answer being re-offered. |
| Confirmations | companion (`tags.user` + `meta.plugins`) | The actual answer. Portable, survives a cache wipe, visible to another installation. |

Confirmations are *also* mirrored into the verdicts table, because that is the
index the request path reads — but the companion is the record. A deleted
cache.db costs the pending set and the rejections, and the indexer recovers the
confirmations from companions as it walks them.

## 5. Finding the work: `pending::`

One new filter term, mirroring `has::` in both spelling and implementation:

```
pending::plugin.source-finder        # unresolved findings from this plugin
pending::any                         # anything awaiting an answer
```

In the evaluator this is one `EXISTS (SELECT 1 FROM plugin_findings …)` arm next
to the `HasNamespace` one it copies. In the parser it is one `strip_prefix`
alongside `has::`.

This is what removes the need for a dedicated review queue in the first
version. Filtering the grid to `pending::plugin.source-finder` and arrow-keying
through the viewer *is* a review queue, built out of parts that already exist,
already virtualize, and already work on a phone.

## 6. Where findings appear

### The info panel, for per-file kinds

The viewer's info panel — side panel on desktop, bottom sheet on touch — gains a
**Findings** section under Tags. One block per unresolved finding on this file:

```
┌─ Original source ──────────── source-finder ─┐
│                                              │
│ ○ ▣  Danbooru #5512         4096²   98%   ↗  │
│ ○ ▣  Pixiv 74123456         2048²   91%   ↗  │
│                                              │
│     [ Review… ]         [ None of these ]    │
└──────────────────────────────────────────────┘
```

The block is deliberately a *summary and an entry point*, not the whole
interaction. Confirming the obvious case without leaving the viewer is what
makes a long queue bearable, so the rows are selectable and `Enter` confirms the
selected one outright; **Review…** opens the [source dialog](#7-the-source-dialog)
for everything else — comparing the actual pixels, and taking a better copy.

Once answered the block collapses to one line — `Original source · Danbooru
#5512` — with a change affordance, so a mistake is one click from being undone
rather than requiring a re-run.

Keyboard, because the whole point is answering many in a row: `1`–`9` select a
candidate, `Enter` confirms, `N` rejects, `R` opens the dialog, and the viewer's
existing arrow keys move to the next file. That combination is the queue, and it
is why [§5](#5-finding-the-work-pending)'s filter needs no dedicated panel to go
with it.

Kinds that are not per-file get no info-panel block; they get their own panel,
because there is no single file to hang them on. That is the whole of the faces
problem — see [§8](#8-faces-a-cluster-is-not-a-file).

### Noticing there is work

`AutoTagPanel` gains one line per plugin that declares findings —
*"source-finder · 12 files awaiting review"* — where the count is a button that
applies `pending::plugin.source-finder` and closes the panel. No badge system,
no new notification surface; it reuses the filter bar as the navigation.

### Plugin settings

A per-plugin section in `SettingsMenu`, rendered from `settings_schema` — the
field that has parsed since the beginning and never rendered. The supported
types are deliberately few, and expanding them is a host change like a new
kind: `string`, `number` (with `min`/`max`), `boolean`, `enum` (select), and
`string[]` (tag-style list).

Values are stored per gallery in `gallery_meta` under
`plugin.<name>.settings`, mirrored to `settings.toml` like every other
per-gallery setting, and handed to the plugin as `LIGHTVIEW_PLUGIN_SETTINGS`
(one JSON object) in its environment. That deliberately needs **no protocol
change** — it is the same mechanism as `LIGHTVIEW_JOB_TOTAL`. A remote worker
gets them in the `claim_tagging_job` response and passes them through.

This is also the concrete form of the plugins page's Track A: settings are
per-gallery, plugin *code* stays global.

## 7. The source dialog

Its own dialog, not a `MergeDialog` variant. The two problems rhyme — a better
copy exists, keep the metadata, discard the loser — but a merge participant is a
local file with a companion, an mtime and a `media_meta` row, while a candidate
is a URL with dimensions attached. Making one impersonate the other constrains
both to whichever intersection stays true, for a saving that is layout only. The
reuse that pays sits a layer below and is untouched: `modify_companion`,
`reindex_tags_for_file` and `trash_files_impl` are the same primitives
`merge_duplicates` composes, so a source replacement and a duplicate merge write
a companion identically.

```
┌─ Original source ──────────────────────────────────────────── source-finder ─┐
│                                                                              │
│  YOURS                              CANDIDATE  ‹ 1 of 3 ›                    │
│  ┌────────────┐                     ┌────────────┐                           │
│  │            │                     │            │   Danbooru #5512     98%  │
│  │            │                     │            │   Artist   kantoku        │
│  └────────────┘                     └────────────┘   Posted   2 Apr 2019     │
│  1024 × 1024                        4096 × 4096                    ↗ open    │
│  412 KB · JPEG                      7.9 MB · PNG                             │
│                                     ▲ 4× resolution                          │
│                                                                              │
│  Tags this adds:   source:danbooru   artist:kantoku                          │
│                                                                              │
│  [ Confirm only ]  [ Download & keep both ]  [ Download & replace ]  [ ✕ ]   │
└──────────────────────────────────────────────────────────────────────────────┘
```

Three outcomes, and they are separated because they carry very different risk:

- **Confirm only** records the verdict. The candidate's `tags` go to
  `tags.user`, the chosen candidate (minus thumbnail and file URLs) to
  `meta.plugins[<prefix>].source`. No bytes move. This is the common case and
  the only one the first build strictly needs.
- **Download & keep both** records the same verdict, then fetches `file_url`
  next to the original, names it from the original plus a suffix, and copies the
  companion across so the new file arrives with the same tags and rating. The
  fs-watcher indexes it like any other new file. The pair will usually then show
  up in the duplicate finder, which is correct — the user asked for two copies.
- **Download & replace** does the same, then trashes the original through
  `trash_files_impl`. Behind the `delete` capability, exactly like
  `merge_duplicates`, because it ends in a trash call.

The comparison is the reason the dialog exists, so it has to be honest about a
few things a naive version gets wrong. A candidate is only *better* if it is
larger in both dimensions — a re-crop at higher resolution is not an upgrade of
this file, and the replace action is disabled with the reason shown when the
aspect ratio differs by more than a rounding error. Dimensions and byte count
come from the plugin and are therefore claims, not facts; they are checked
against the fetched bytes before anything is trashed, and a mismatch aborts with
the original untouched. The fetch is bounded in size and decoded before it is
kept, so a URL that turns out to serve an HTML error page or a 900 MB file fails
cleanly.

**Fetching is the host's, and it is addressed by id.** The command takes a
finding id and a candidate id and reads the URL from `plugin_findings`
host-side; it never accepts a URL from the caller, because a paired browser that
could name one would make the host an open proxy for its network — the shape
[decision 0005](../decisions/0005-remote-invoke-is-an-allowlist.md) exists to
prevent. This is also the host's first outbound HTTP request: `reqwest` is
behind the `worker` feature today and geocoding is deliberately offline, so
enabling it in the default build is a real cost, recorded in
[decision 0016](../decisions/0016-plugin-findings-are-host-owned-kinds.md)
along with why the plugin does not do the download instead.

## 8. Faces: a cluster is not a file

Everything above assumes a finding belongs to one file. A face cluster does not:
its identity is the thing being named, and it spans files. That is why it needs
its own panel rather than a text input in the info panel, and it is why it is
scheduled after `source` rather than beside it.

The state is genuinely new. A `plugin_groups` table holds the cluster, its
members, and the name once given; the name is *also* written to each member's
companion as an ordinary `person:alice` user tag, so the answer survives a cache
wipe and travels with the files — the same division of labour as
[§4](#4-where-the-state-lives), where the cache holds scaffolding and the
companion holds the record.

`FacesPanel` sits beside `DuplicatesPanel` and borrows its structure, because
the interaction is the same one: a list of machine-proposed groups, a contact
sheet per group, and per-group actions. Name a cluster; merge two clusters that
are the same person; split out a face that does not belong; reject a cluster
entirely, which is `not_duplicates` again under another name.

The question to answer before building it is what a re-run does to a cluster the
user already named. Re-clustering from scratch discards names; never
re-clustering means new photos of Alice are never recognised. The likely answer
is that a named cluster becomes a *labelled anchor* the next run assigns against
rather than a group it may reshape, which turns naming into training data and is
the behaviour the `known` field in [§9](#9-closing-the-loop) exists to carry.
That wants a real corpus before it is settled.

## 9. Closing the loop

A request line gains an optional `known` object carrying prior verdicts for that
file, keyed by finding kind:

```json
{"action": "tag", "path": "/tmp/lv/000007.webp",
 "known": {"source": {"status": "confirmed", "candidate": "danbooru:5512"}}}
```

Without this a re-run re-asks everything the user already answered, which makes
the feature actively annoying on its second use. The plugin decides what to do
with it — the source finder skips the search entirely; a face plugin assigns new
faces to a person it now knows by name.

`status` is `confirmed` or `rejected`. A plugin that ignores the field is
unaffected, which is why this is additive.

## 10. Protocol version

This is **`api_version: 2`**. Every individual addition is additive and
ignorable — `findings` in a result, `known` in a request, settings in the
environment — so nothing here breaks a version 1 plugin. The bump exists for the
other direction: a plugin that declares `ui.findings` needs a host that renders
them, and *silently doing nothing* is the failure
[decision 0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md)
introduced the version to prevent. A version 2 plugin on a version 1 host is
refused with a message instead.

The same argument covers kinds within version 2: a host that does not know a
declared kind refuses the plugin by name at install rather than accepting it and
dropping every finding it produces.

## 11. Build order

The first three steps are independent of any particular kind, which is the point
of the split in [decision 0016](../decisions/0016-plugin-findings-are-host-owned-kinds.md)
— the lifecycle is built once and every kind lands on top of it.

1. **Settings form.** No findings involved: `settings_schema` rendering,
   per-gallery storage, the env var, and the worker passing it through. Small,
   independently useful, and it exercises manifest-declared UI with none of the
   state questions.
2. **Findings backend.** Manifest declaration and the kind registry, the
   `findings` result field, the two cache tables, `pending::`, and the
   confirm/reject commands. All of it drivable from `curl` against
   `lightview-headless`, so it is testable before any UI exists.
3. **The info-panel section**, confirm and reject only, plus keyboard handling
   and the collapsed summary. At this point the loop closes end to end without
   any bytes moving.
4. **The source dialog**, and with it the host's HTTP fetch, the size and decode
   bounds, and the two download actions. This is the step that adds `reqwest` to
   the default build, and keeping it separate means steps 1–3 can land without
   it.
5. **`known` on the request line**, and the source plugin itself.

Faces follow as their own project: `plugin_groups`, `FacesPanel`, and the
re-run-reshaping answer in [§8](#8-faces-a-cluster-is-not-a-file).

## 12. Deliberately not in this

- **A generic review panel.** The filter plus the viewer covers the per-file
  kinds. `FacesPanel` is not this — it is a surface for one kind that has no
  per-file home.
- **The three generic shapes** — `choice`, `confirm`, `label` — as a fallback
  for plugins that do not merit their own kind. Designed, deliberately unbuilt,
  and the trigger is a third-party plugin population; see
  [decision 0016](../decisions/0016-plugin-findings-are-host-owned-kinds.md).
- **A declared layout schema** — [decision 0015](../decisions/0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md),
  whose rejection of it survives being superseded on everything else.
- **The sandboxed iframe** ([`README.md`](README.md) Track C). Still the
  fallback for a display a native surface genuinely should not be written for.
  Neither motivating case needs one.
- **Plugin-added views, filters or sort keys.** Core stays native
  ([decision 0008](../decisions/0008-no-view-module-api.md)).
- **EXIF rewriting on a downloaded source.** The candidate arrives with whatever
  metadata the remote site embedded, and the
  [duplicates EXIF boundary](../duplicates/README.md#the-exif-boundary) applies
  unchanged: anything expressible in the companion transfers, anything requiring
  a re-encode does not.

## 13. Open questions worth resolving during the build

- **Do rejections expire?** Remembering every "no" forever is what stops a
  plugin re-offering the same wrong candidate; it is also unbounded growth
  keyed on absolute paths (see F1 in [`todo.md`](../todo.md)). A per-plugin cap
  with oldest-out is probably right, but it wants a real corpus first.
- **What happens to a confirmation when the file is edited?** A confirmed source
  survives in `tags.user`, which is correct for a crop and wrong for a replaced
  file. The duplicate merge path has the same question and answers it by hand.
- **What a downloaded source is named, and whether the original's name is the
  one worth keeping.** "Keep both" needs a suffix; "replace" has a choice
  between inheriting the original's filename — which keeps every path-keyed
  cache row and companion valid — and taking the remote one, which is more
  honest about provenance and invalidates all of it. Inheriting is almost
  certainly right, and it is worth confirming against F1 in
  [`todo.md`](../todo.md) before it is written.
- **Whether a confirmed source should be re-checked.** A site can gain a higher
  resolution version years later. Re-asking is cheap for the plugin and annoying
  for the person, so any re-check wants to be explicit rather than automatic.
