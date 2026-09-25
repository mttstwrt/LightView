# Tasks

Phase 1 — one PR. Each step lands as its own commit, green on `cargo test` and
`cargo clippy --all-targets --all-features`.

- [ ] **Key generator** — `sort/order_key.rs`: `after`, `before`, `spread`,
  `default_key`; property tests; `default_key` pinned against SQLite.
- [ ] **Sidecar** — `Order { key, set, pos }` on `MetaCollection`;
  `honoured_order()`; round trip and older-build `extra` tests.
- [ ] **Index** — `media_order` in `schema_sql()` and `path_keyed_tables()`;
  `reindex_file` writes it and reports a change; `companion_index_version`
  stamp.
- [ ] **Custom sort** — `SortField::Custom`, its statement, `SortedItem.block`,
  no groups; ordering fixtures.
- [ ] **Events and gate** — `OrderChanged` (domain Items) from every
  `reindex_file` caller that sees a change; `Gallery.arrangeable`.
- [ ] **Upkeep** — `edit_companion`; remove/delete dissolve, rename carries,
  merge concatenates; duplicate merge adopts.
- [ ] **Order service and commands** — `place`, `lock_set`, `unlock_set`,
  `reset_order`; pure planners; the gate.
- [ ] **SPA** — Custom in the sort menu; the Arrange submenu and pick mode;
  "Lock as set…"; "Unlock" in the tag manager; block marks; request sequencing;
  viewer re-anchor; notice toast.
- [ ] **Docs** — query, companion, cache, server, frontend, duplicates,
  AGENTS.md.
- [ ] **Verify** — `cargo test`, clippy, `tsc`, `drive.sh` and `grid.mjs`
  extended; measure a 200-member block move and the Custom query.

Phase 2 — mouse drag in `JustifiedGrid`, its own PR, after this one.
