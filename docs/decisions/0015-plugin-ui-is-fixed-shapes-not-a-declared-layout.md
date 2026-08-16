# 0015 — Plugin UI is a fixed set of host-drawn shapes, not a declared layout

[← docs index](../README.md) · [plugins](../plugins/README.md) · [findings and UI](../plugins/findings-and-ui.md)

## Context

Two planned plugins produce output the tag protocol cannot carry: a face
recogniser that emits clusters nobody can name, and a reverse-search plugin that
returns ranked source candidates of which one or none is correct. Both need a
person to decide, which means both need UI.

[`plugins/README.md`](../plugins/README.md) §6 had already settled the shape of
the answer at one level — host-rendered and declarative, not a sandboxed iframe
— but "declarative" spans a wide range. It can mean a plugin picking from a
small menu the host draws, or a plugin shipping a description of a form the host
renders generically. Those are very different commitments, and the second is
what most people mean by "plugin-driven UI".

The manifest has carried a `PluginUiConfig.settings_schema` field since the
beginning. It has always parsed and never rendered, so the question was live
either way.

## Options considered

**A declared layout schema.** The plugin ships JSON describing fields, controls
and arrangement; the host renders whatever it is given. Genuinely extensible: a
new plugin UI needs no host release, which is the property that makes a plugin
system feel like one.

It is also a UI framework expressed in JSON, and the cost is not the first
version. It is that conditionals follow (show this field only when that one is
set), then validation, then layout hints, then escape hatches for the thing the
schema cannot say — and every one of them is a public contract to version
forever, across a repository boundary
([0012](0012-plugins-declare-the-contract-they-were-built-for.md)), for a host
whose entire plugin population is currently four taggers. The output would still
look worse than three hand-written Solid components, because a generic renderer
cannot know that a source candidate wants a thumbnail and a similarity
percentage side by side.

**Plugin-supplied HTML in a sandboxed iframe** (Track C in the plugins page).
The legitimate way to get genuinely arbitrary displays, and correspondingly
expensive: a versioned `postMessage` protocol, a plugin lifecycle, resource
limits, and a second rendering model to keep working on a phone. Neither
motivating case needs it.

**Fixed shapes the host draws.** The host defines a closed vocabulary of
interactions; the plugin declares which one a finding uses and supplies content.
A fourth shape is a host release.

## Decision

Fixed shapes, three of them: `choice` (pick one of N, or none), `confirm`
(yes/no), and `label` (name this). A plugin declares its findings in the
manifest — kind, shape, title — and supplies content per result: options, a
detail table of key→string, a URL, a thumbnail, and the tags an option implies.
The host owns all layout.

`settings_schema` is the one place a real schema *is* rendered generically, and
it is a deliberate exception rather than an inconsistency: a configuration form
is a bounded, well-understood artefact with a finite type list, and the
supported types are correspondingly few — `string`, `number`, `boolean`, `enum`,
`string[]`. A review UI is not bounded in that way, which is exactly why it gets
a vocabulary instead of a language.

The general renderer is the thing to extract on the *third* real case, not the
first hypothetical one, and by then there will be three concrete UIs to
generalise from rather than a guess.

## Consequences

- Both motivating cases fit today, and the second one (faces) is additive rather
  than a redesign: `label` exists for it before anything needs it.
- The whole review surface is three components. They are keyboard-navigable,
  identical on desktop and phone, and consistent with the rest of the app,
  because they *are* the rest of the app.
- A plugin author wanting a fourth interaction is blocked on a LightView
  release. That is the real cost, and it is acceptable while the plugin
  population is small and known. It stops being acceptable if third-party
  plugins become common, which is the signal to revisit.
- The host knows a plugin's entire UI surface from its manifest, before running
  anything. That is what lets the filter offer `pending::plugin.<name>` and the
  plugin list say what a plugin will ask for — neither of which a
  results-only declaration could support.
- Shapes are a versioned vocabulary: adding one is backwards-compatible, but
  *changing* one is not, and a plugin naming a shape the host does not know must
  be refused rather than rendered wrongly. That is what `api_version: 2` is for.
- Nothing here forecloses Track C. A sandboxed iframe remains available for a
  display a vocabulary genuinely cannot express, and it would sit beside these
  shapes rather than replace them.
