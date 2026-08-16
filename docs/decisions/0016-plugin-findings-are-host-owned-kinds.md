# 0016 — Plugin findings are host-owned kinds, not host-drawn shapes

[← docs index](../README.md) · [plugins](../plugins/README.md) · [findings and UI](../plugins/findings-and-ui.md) · [duplicates](../duplicates/README.md)

Supersedes [0015](0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md).

## Context

[0015](0015-plugin-ui-is-fixed-shapes-not-a-declared-layout.md) settled plugin
UI as a closed vocabulary of three interactions the host draws — `choice`,
`confirm`, `label` — with the plugin declaring which one a finding uses and
supplying content. A deliberate property of that design was that the host
**never interprets a finding's `kind`**: the plugin's word for what it found is
opaque, and the shape alone selects the interaction.

Reading the design back against the two motivating plugins, before any of it was
built, that property is exactly what caps the result at generic. The bar in this
codebase is `DuplicatesPanel` and `MergeDialog` — roughly 970 lines of native
Solid sitting on a domain the host genuinely understands, which is why the panel
can rank a group by resolution, mark a best copy, and offer a field-by-field
merge. None of that is reachable by a renderer that has been told only "this is
a `choice`". UI quality is a function of domain knowledge, and domain knowledge
can only live where the code is.

Checked case by case, the vocabulary serves neither plugin it was designed for:

- **`label` exists for faces**, but [`findings-and-ui.md`](../plugins/findings-and-ui.md)
  §10 defers faces on the grounds that a cluster's identity cannot live in any
  one companion — it needs a `plugin_groups` table, a merge and rename surface,
  and an answer for a re-run that reshapes a cluster the user already named.
  That is a panel over cross-file state. A text input in a single file's info
  panel is not a smaller version of it; it is a different thing that happens to
  collect a string.
- **`choice` is for source candidates**, but the interesting action is not
  "pick one of these". It is "this is the source, it is four times the
  resolution of my copy, take it and keep my tags". A radio list over a
  key→string detail table cannot express a resolution comparison against the
  local file, let alone a fetch-and-replace flow.

So three shapes were designed to serve two plugins, one of them is not what its
plugin will use, and the other is a thin version of what its plugin wants. That
is an abstraction with zero real implementations, which principle 1 says to
extract on the third case rather than the first.

## Options considered

**Keep the three shapes.** Cheapest, and it retains one real property: an
unanticipated third-party plugin gets *some* surface with no host release.
Rejected because neither known plugin is well served, and because the cost of
the vocabulary is paid now while the benefit accrues to a plugin population that
does not exist.

**A declared layout schema, or plugin-supplied HTML in a sandboxed iframe.**
Both were considered and rejected in 0015 and the reasoning is unchanged — a UI
framework expressed in JSON versioned forever across a repository boundary, or a
`postMessage` protocol plus a second rendering model to keep working on a phone.
Neither motivating case needs one, and the schema would still look worse than
hand-written components for the same reason the shapes do.

**Host-owned kinds.** The host defines a small set of *semantic* finding kinds.
Each kind has a typed payload and a native Solid surface built for it. A plugin
declares in its manifest which kinds it produces; a plugin declaring a kind the
host does not know is refused at install with a message naming it, following the
`api_version` precedent in
[0012](0012-plugins-declare-the-contract-they-were-built-for.md) rather than
being rendered wrongly.

## Decision

Host-owned kinds. The rule of thumb is **generalise the lifecycle, specialise
the presentation**.

Everything in `findings-and-ui.md` that is domain-neutral stays exactly as
designed, because it is right regardless of who draws pixels: `findings`
alongside `tags` in a result line, the `plugin_findings` and
`plugin_finding_verdicts` cache tables following the `not_duplicates`
precedent, `pending::` as one `EXISTS` arm beside `HasNamespace`, `known`
echoed back on the request line, a confirmation writing the option's tags to
`tags.user` and its provenance to `meta.plugins[prefix]` with no companion
schema bump, `settings_schema`, and `api_version: 2`. That half is also the
half that is drivable from `curl` before any UI exists.

What changes is the manifest declaration and what sits above it. `shape` is
replaced by the host's registry of kinds, of which there are two:

| Kind | Payload | Surface |
|---|---|---|
| `source` | ranked external candidates for one file, each with dimensions, bytes, URL, thumbnail, and implied tags | candidate list in the viewer's info panel, opening a dedicated source dialog |
| `face-cluster` | a set of files the plugin believes are one person | a `FacesPanel` beside `DuplicatesPanel`: contact sheet per cluster, name, merge, split |

`source` gets **its own dialog rather than reusing `MergeDialog`**. The two
problems rhyme — a better copy exists, keep the metadata, discard the loser —
but a merge participant is a local file with a companion, a mtime and an entry
in `media_meta`, and a remote candidate is a URL with dimensions. Making one
masquerade as the other costs more than the duplicated layout it saves, and it
would constrain both dialogs to whichever intersection stayed true. The reuse
that actually pays is one layer down and is unaffected by this: `modify_companion`,
`reindex_tags_for_file` and `trash_files_impl` are the same primitives
`merge_duplicates` composes, so a source replacement and a duplicate merge still
write a companion identically.

The three shapes are **kept as a documented fallback tier and not built**. When
a plugin wants a surface that does not merit its own kind, `confirm` and `label`
are a few dozen lines each on top of a lifecycle that will already exist. The
trigger for building them is the one 0015 already named for revisiting itself: a
third-party plugin population.

## Consequences

- A new plugin *class* costs a host release. This is less of a change than it
  looks: under 0015 a fourth interaction was already a host release. The genuine
  loss is that an unanticipated plugin now gets no surface at all rather than a
  mediocre one, and the fallback tier above is the answer when that starts to
  bite.
- Faces get the cross-file panel the problem always needed, instead of a text
  input that would have been replaced by one. The work deferred in §10 of
  `findings-and-ui.md` — `plugin_groups`, merge and rename, re-run reshaping — is
  now the plan for the kind rather than a reason the shape was insufficient.
- `source` can reach a fetch-and-replace flow, which is the action the whole
  feature exists for and which no shape could have expressed.
- **The host acquires an outbound HTTP client in its default build.** `reqwest`
  is currently behind the `worker` feature, so nothing in the desktop app or the
  headless server makes an outbound request today; geocoding is deliberately
  offline. Fetching a candidate changes that, and it is the one new cost this
  decision carries that 0015 did not. The alternative — having the plugin
  download it, since it already declares `NetworkAccess` and already made the
  search request — was rejected because a remote worker would land the bytes on
  the wrong machine, and returning them through an NDJSON pipe means base64 in a
  line-oriented protocol.
- **A fetch is addressed by finding and option id, never by a URL from the
  client.** The URL is read from `plugin_findings` host-side. A paired browser
  that could name the URL would make the host an open proxy for anything on its
  network, which is the shape [0005](0005-remote-invoke-is-an-allowlist.md)
  exists to prevent. Replacing a file also puts the flow behind the same
  `delete` capability as `merge_duplicates`, for the same reason: it ends in a
  trash call.
- The host still knows a plugin's whole UI surface from its manifest before
  running anything, which is what lets the filter offer `pending::plugin.<name>`
  and the plugin list say what a plugin will ask for. Declaring a kind carries
  that as well as declaring a shape did.
- This is consistent with [0008](0008-no-view-module-api.md) rather than an
  exception to it: presentation is native, and the plugin boundary is a
  *compute* boundary. Fixed shapes were the one place trying to make
  presentation semi-pluggable.
- `api_version: 2` is unaffected. It was never about the shapes specifically —
  it exists so a plugin that declares findings is refused by a host that cannot
  render them, rather than silently doing nothing.
