# Findings: plugin output that is not tags, and the UI that resolves it

[← docs index](../README.md) · [plugins](README.md) · [companion](../companion/README.md) · [worker tagging](../remote/worker-tagging.md)

**Status:** design. Nothing here is built.
**Audience:** whoever picks up A7 in [`todo.md`](../todo.md).

The protocol has one verb and one output shape: an image goes in, tags come
out. That is enough for a tagger and for nothing else. This page is about the
class of plugin it cannot express, what those plugins actually need, and the
smallest contract that serves them.

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

1. **A result that is structured data, not a tag list.** Fields with types, and
   more than one candidate per file.
2. **A place for a person to see it and decide**, in bulk, without opening each
   file.
3. **The decision going somewhere durable, and back to the plugin** — so a
   second run does not re-ask, and so a confirmed answer can become a tag,
   because *after* confirmation "this is Alice" is exactly a tag.

Call the middle thing a **finding**: something a plugin noticed that is not yet
a fact.

---

## 2. Findings as a result kind

The NDJSON protocol already carries an `action` field that never varies and a
`meta` passthrough that is written to the companion file and never read. The
shape of the extension is therefore additive: a result may carry `findings`
alongside — or instead of — `tags`.

A finding needs, at minimum: a stable **id** the plugin can recognise on a later
run; a **kind** the plugin names and the host does not interpret; **fields** to
display; and an optional **group** key, which is what makes clusters and
candidate sets the same mechanism. Whether a finding also carries suggested
**actions**, or whether the manifest declares once what a verdict on this kind
means, is open — see §5.

The host does not interpret a finding's kind, exactly as it does not interpret a
tag. What it interprets is the *envelope*: enough to render a row, group rows,
and record a verdict against one.

**Findings must not go through `tags.plugins[prefix]`.** That field is a list of
strings the tag index reads and the filter language queries; a finding is
neither, and forcing it through would make every candidate URL an
autocomplete-able tag. Companion files are a wire format other LightView
installations read ([`companion/`](../companion/README.md)), so this is a schema
addition with a migration, not a field to improvise.

---

## 3. The review surface belongs to the host

Both cases need a screen that shows what a plugin found and takes an answer.
Neither needs that screen to be drawn *by* the plugin:

- Naming a face cluster is a grid of thumbnails, a text field, and a merge
  action.
- Confirming an image source is a list of candidates with a thumbnail, some
  text, and pick-one-or-none.

Both are "here is a group of items and some metadata; give me a verdict". That
is one host screen with two configurations, not two plugin-authored UIs — and it
is a screen that stays consistent, keyboard-navigable and fast over ten thousand
items, which plugin-authored UI in a sandboxed iframe would not be.

This is the concrete form of the direction already recorded in
[`README.md`](README.md) §6: **plugin UI is host-rendered and declarative.** The
manifest's `PluginUiConfig.settings_schema` — parsed since the beginning, never
rendered — becomes the plugin's *configuration* form. The review surface is not
declared at all; it is a host feature that any plugin emitting findings feeds.

Track C's sandboxed iframe stays the fallback for a display a schema genuinely
cannot describe. It is a versioned `postMessage` protocol and a plugin lifecycle
to maintain forever, and neither motivating case needs one.

---

## 4. The verdict has to reach the plugin

This is the part that distinguishes findings from every existing plugin output,
and the part most likely to be got wrong by building only the display half.

A confirmed source, or a named cluster, is an input to the *next* run: the
source-finder should not re-search a file whose source is confirmed, and the
face plugin should assign new faces to a person it already knows by name. So a
verdict is durable per-file state that the host must hand back on the next
request — the plugin cannot keep it, because a plugin is a subprocess with no
storage and may be running on a different machine than last time.

Which suggests the request line grows an optional field carrying the prior
verdicts for that file, and the host gains a way to answer "which files have an
unresolved finding of kind X" — the filter language is the obvious place, and
`has::plugin.<prefix>` is the obvious precedent.

Two consequences worth stating before anyone builds this:

- **A verdict is a user edit.** It belongs in the companion file with ratings
  and user tags, not in the cache, which is derived and rebuildable.
- **Confirmation may produce a tag**, and that tag should look like any other:
  `person:alice` is a tag, and once confirmed there is no reason for it to be
  anything else. The finding is scaffolding; the tag is the durable result.

---

## 5. Open questions

- **How much of the review UI is declared and how much is fixed.** A fixed
  layout per finding *kind* is simpler and covers both cases; a declared layout
  is more general and is a schema to version forever. The declared version is
  the more expensive bet and should not be taken first.
- **Where verdicts live in the companion schema**, and whether a rejected
  candidate is remembered or merely not confirmed. Remembering rejections is
  what stops a plugin re-offering the same wrong answer every run, and it is
  also unbounded growth.
- **Whether findings need their own verb** or ride on `tag`. A separate verb
  makes a plugin's capability explicit — and a plugin that only finds sources
  has no business being offered under "tag untagged".
- **Grouping across files.** A face cluster spans many files, so its identity
  cannot live in any one companion. That is a genuinely new kind of state, and
  the reason the face case is harder than the source case despite looking
  similar. The source case has no such problem and is therefore the better one
  to build first.
- **Whether a finding can be produced by a remote worker at all.** Nothing
  stops it — the results channel is the same — but `apply_plugin_tags` is the
  allowlisted write path, and a findings write is a different shape with
  different validation.

---

## 6. Why this waits on the versioned protocol

A UI contract that can drift silently is worse than no UI contract: a plugin
whose findings the host renders wrongly is a plugin producing confidently wrong
information for a person to confirm. `api_version`
([0012](../decisions/0012-plugins-declare-the-contract-they-were-built-for.md))
is the mechanism that makes this addable — a host can tell a plugin that emits
findings from one that does not, and refuse what it cannot render, instead of
guessing from the shape of a JSON object.

That is also why the sequencing in [`todo.md`](../todo.md) puts this last. It is
not that it is less valuable; it is that it is the item that most needs
everything under it to have stopped moving.
