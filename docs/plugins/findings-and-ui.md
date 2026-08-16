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

## 2. Three shapes, fixed by the host

A finding declares a **kind** (the plugin's word for what it is, which the host
never interprets) and takes one of three **shapes** (the host's vocabulary,
which selects the interaction):

| Shape | The question | Answer | Motivating case |
|---|---|---|---|
| `choice` | which of these, if any? | one option id, or none | image source candidates |
| `confirm` | is this true? | yes / no | "this is AI-generated" |
| `label` | what is this called? | a string | naming a face cluster |

The host owns every pixel. A plugin supplies content — a title, options, a
detail table of key→string, tags an option implies — and never layout. Adding a
fourth shape is a host release, and that is the trade: see
[decision 0015](../decisions/0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md).

Shapes are declared in the **manifest**, not in results, so the host knows a
plugin's whole UI surface before running it:

```json
"ui": {
  "settings_schema": { … },
  "context_menu_items": [ … ],
  "findings": [
    { "kind": "source", "shape": "choice", "title": "Original source" }
  ]
}
```

A result carrying a `kind` the manifest never declared is dropped with a log
line. That is deliberate: it keeps "what can this plugin ask me?" answerable
from the manifest alone, which is what makes the filter chip and the plugin list
possible before any job has run.

## 3. What a plugin emits

`findings` sits alongside `tags` in an ordinary result line — additive, so a
plugin can tag and find in the same pass, or do only one:

```json
{"path": "/g/a.jpg", "tags": [], "findings": [
  {"kind": "source",
   "id": "src:9f2a",
   "options": [
     {"id": "danbooru:5512",
      "label": "Danbooru #5512",
      "detail": {"Artist": "kantoku", "Posted": "2019-04-02", "Match": "98%"},
      "url": "https://danbooru.donmai.us/posts/5512",
      "thumb": "https://danbooru.donmai.us/preview/5512.jpg",
      "tags": ["source:danbooru", "artist:kantoku"]}
   ]}
]}
```

`id` is the plugin's own stable key for this finding, so a re-run can be matched
against a previous verdict. `confirm` findings carry `detail` and `tags` but no
`options`; `label` findings carry neither.

**Where a confirmed answer goes, and why it is not the plugin's tag
namespace.** `apply_plugin_output` replaces `tags.plugins[prefix].tags`
wholesale on every run — that is what makes re-tagging idempotent, and it means
anything written there is erased by the next job. So a confirmation writes:

- the option's `tags` into **`tags.user`**. The user affirmed them; they are
  indexed and filterable the moment they land, they survive re-tagging, and they
  are removable through the tag UI like any other user tag.
- the chosen option (minus its thumbnail URL) into
  **`meta.plugins[<prefix>].<kind>`**, which is already
  `HashMap<String, Value>` and needs no schema change. That is where provenance
  lives: which plugin found it, which candidate won, what the score was.

For `label`, the typed value becomes the tag `<kind>:<value>` with spaces
underscored — `person:alice` — by host convention rather than a template
language in the manifest.

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

## 6. The screens

### Findings in the info panel

The viewer's info panel — side panel on desktop, bottom sheet on touch — gains a
**Findings** section under Tags. One block per unresolved finding:

```
┌─ Original source ──────────────── source-finder ─┐
│                                                   │
│  ○ ▣  Danbooru #5512                              │
│       Artist  kantoku                             │
│       Posted  2 Apr 2019                          │
│       Match   98%                          ↗ open │
│                                                   │
│  ○ ▣  Pixiv 74123456                              │
│       Artist  かんとく                             │
│       Match   91%                          ↗ open │
│                                                   │
│         [ Confirm ]      [ None of these ]        │
└───────────────────────────────────────────────────┘
```

- **`choice`** is a radio list. Each row: an optional 48 px thumbnail, the
  label, a two-column detail grid, and `url` as an external link. **Confirm**
  enables once a row is selected; **None of these** records a rejection.
- **`confirm`** is the detail grid plus **Yes** / **No**.
- **`label`** is a text input with autocomplete over existing `<kind>:` tags,
  plus **Save** / **Skip**.

Once answered the block collapses to one line — `Original source · Danbooru
#5512` — with a change affordance, so a mistake is one click from being undone
rather than requiring a re-run.

Keyboard, because the whole point is answering many in a row: `1`–`9` select an
option, `Enter` confirms, `N` rejects, and the viewer's existing arrow keys move
to the next file. That combination is the queue.

### Noticing there is work

`AutoTagPanel` gains one line per plugin that declares findings —
*"source-finder · 12 files awaiting review"* — where the count is a button that
applies `pending::plugin.source-finder` and closes the panel. No badge system,
no new notification surface; it reuses the filter bar as the navigation.

### Plugin settings

A per-plugin section in `SettingsMenu`, rendered from `settings_schema` — the
field that has parsed since the beginning and never rendered. The supported
types are deliberately few, and expanding them is a host change like a new
shape: `string`, `number` (with `min`/`max`), `boolean`, `enum` (select), and
`string[]` (tag-style list).

Values are stored per gallery in `gallery_meta` under
`plugin.<name>.settings`, mirrored to `settings.toml` like every other
per-gallery setting, and handed to the plugin as `LIGHTVIEW_PLUGIN_SETTINGS`
(one JSON object) in its environment. That deliberately needs **no protocol
change** — it is the same mechanism as `LIGHTVIEW_JOB_TOTAL`. A remote worker
gets them in the `claim_tagging_job` response and passes them through.

This is also the concrete form of the plugins page's Track A: settings are
per-gallery, plugin *code* stays global.

## 7. Closing the loop

A request line gains an optional `known` object carrying prior verdicts for that
file, keyed by finding kind:

```json
{"action": "tag", "path": "/tmp/lv/000007.webp",
 "known": {"source": {"status": "confirmed", "option": "danbooru:5512"}}}
```

Without this a re-run re-asks everything the user already answered, which makes
the feature actively annoying on its second use. The plugin decides what to do
with it — the source finder skips the search entirely; a face plugin assigns new
faces to a person it now knows by name.

`status` is `confirmed` or `rejected`. A plugin that ignores the field is
unaffected, which is why this is additive.

## 8. Protocol version

This is **`api_version: 2`**. Every individual addition is additive and
ignorable — `findings` in a result, `known` in a request, settings in the
environment — so nothing here breaks a version 1 plugin. The bump exists for the
other direction: a plugin that declares `ui.findings` needs a host that renders
them, and *silently doing nothing* is the failure
[decision 0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md)
introduced the version to prevent. A version 2 plugin on a version 1 host is
refused with a message instead.

## 9. Build order

1. **Settings form.** No findings involved: `settings_schema` rendering,
   per-gallery storage, the env var, and the worker passing it through. Small,
   independently useful, and it exercises manifest-declared UI with none of the
   state questions.
2. **Findings backend.** Manifest declaration, the `findings` result field, the
   two cache tables, `pending::`, and the confirm/reject commands. All of it
   drivable from `curl` against `lightview-headless`, so it is testable before
   any UI exists.
3. **The info-panel section.** Three shapes, keyboard handling, the collapsed
   summary. `choice` is the only one the source plugin exercises; the other two
   are a few dozen lines each and skipping them would invite a redesign.
4. **`known` on the request line**, and the source plugin itself.

## 10. Deliberately not in this

- **A dedicated review panel.** The filter plus the viewer covers it. Build the
  panel when answering findings in bulk is a real chore and it is clear what the
  chore actually is — an unused `DuplicatesPanel` clone would be worse than
  nothing.
- **Cross-file groups (faces).** A cluster's identity cannot live in any one
  companion, so it is genuinely new state: a `plugin_groups` table, a merge and
  rename surface, and an answer to what happens when a re-run reshapes a cluster
  the user already named. The source case has no such problem, which is why it
  goes first. The `label` shape exists so the face case is additive when it
  comes, not a redesign.
- **A declared layout schema** — [decision 0015](../decisions/0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md).
- **The sandboxed iframe** ([`README.md`](README.md) Track C). Still the
  fallback for a display a schema genuinely cannot describe. Neither motivating
  case needs one.
- **Plugin-added views, filters or sort keys.** Core stays native
  ([decision 0008](../decisions/0008-no-view-module-api.md)).

## 11. Open questions worth resolving during the build

- **Do rejections expire?** Remembering every "no" forever is what stops a
  plugin re-offering the same wrong candidate; it is also unbounded growth
  keyed on absolute paths (see F1 in [`todo.md`](../todo.md)). A per-plugin cap
  with oldest-out is probably right, but it wants a real corpus first.
- **What happens to a confirmation when the file is edited?** A confirmed source
  survives in `tags.user`, which is correct for a crop and wrong for a replaced
  file. The duplicate merge path has the same question and answers it by hand.
- **Whether `choice` needs multi-select.** "Which of these people are in this
  photo?" is a plausible fourth question and is `choice` with a checkbox. Not
  speculating on it until something asks.
