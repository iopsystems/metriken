# A high-cardinality metrics stack: what belongs in metriken, dendro and rezolus

**Status:** OPEN — intent-first. Boundaries agreed 2026-09-28; nothing moved
yet. Every phase lands before rezolus 6.0 (iopsystems/rezolus#1224), which is
then built on this stack rather than moved onto it afterwards (decided
2026-09-28).

## Goal

Make the storage and query of high-cardinality metrics, meaning groups
whose members come and go, reusable by any metriken-instrumented service,
not only by rezolus.

## Why

rezolus's move to dendro archives produced a design for metrics with many
short-lived members (rezolus `docs/journal/2026-09-25-dendro-archive-layout.md`):

- groups with slots are stored long, one row per tick and occupant;
- each occupant's labels live in a parquet stream beside its table;
- metriken-query 0.31.0 reads long segments (`long.rs`, #165).

Nothing in that design is about hosts. A cache server has the same shape
with its own dimensions: request counters per tenant or per client
connection, hit and miss counts per key prefix, latency per worker thread.
Those members are unbounded in number and come and go, and each needs an
identity. rezolus is one producer, whose members happen to be threads,
cgroups and CPUs.

The code is spread in a way that hides this. The segment format's reader
is here, in metriken-query, but its writer is in rezolus
(`crates/rez/src/rez.rs`, `TableBuilder`/`GroupTableBuilder`). The long
layout's markers and `__occupant__` are here, but the occupant stream that
names occupants is in rezolus (`crates/rez/src/occupants.rs`, rezolus#1315).
The row form of a group snapshot (`WalGroupRow`, rezolus
`crates/rez/src/wal.rs`) is pinned byte for byte against this repo's
`GroupSchema` (`metriken-exposition/src/snapshot.rs:176`) but lives in
rezolus. Slot identity and `__uid__` minting (`SlotIdentity`, rezolus
`src/agent/identity.rs:277`) are rezolus-only. Another service writing
segments that rezolus's tools could read would need code from both repos.

## The boundaries

The test for where code belongs: a program other than rezolus that wants to
write or read these archives should need dendro and metriken, and nothing
from rezolus.

| layer | owns | knows nothing about |
|---|---|---|
| **dendro** | Storage and transport: sources, streams, segments as opaque bytes, WAL rows, caller rows, retention, compaction, replication. | What a column means. |
| **metriken** | Groups with slots and occupant identity; the row wire form of a group snapshot and the stream protocol; the parquet segment format (wide, long, occupant stream), writing and reading; an archive writer and reader over dendro; the query engine. | Hosts, samplers, and any one application's defaults. |
| **rezolus** | Host telemetry (the BPF samplers and what they measure), the agent binary, recorder and hindsight commands and their defaults (which groups are long, seal sizes, restatement cadence), the viewer, MCP and dashboards, and the legacy `.rez` container and its converters. | How segment bytes are laid out. |

### Crates within metriken

- **`metriken-segment`** (new): the segment format and nothing else.
  - Column conventions: `timestamp`, the window sidecars, metric field
    metadata, histogram lists.
  - The long layout: its markers, the footer occupant list, `occupant`
    and `__occupant__`.
  - The occupant stream: its rows, its label-column encoding, its WAL row.
  - Encode and decode for all three shapes.
  - No storage and no registry dependency, so it builds for wasm32.
- **`metriken-query`**: the query engine and the parquet readers, which
  read through `metriken-segment`. `long.rs` moves out and is re-exported
  for a release.
- **`metriken-archive`** (new): the writer and reader over dendro, and the
  only metriken crate that depends on dendro.
  - Writer: WAL rows become segments; groups with slots are written long,
    with their occupant stream; seal policy; restatements.
  - Reader: a `MetricsSource` over an archive, meaning the catalog, the
    union of a source's streams, and evaluation across cadences. It reads
    through a catalog trait, so rezolus can keep reading `.rez` by
    implementing it.
  - The writer depends on `metriken-exposition`, which depends on the
    registry, which does not build for wasm32 (linkme). So the writer sits
    behind a `write` feature, the pattern rezolus's `crates/rez` uses.
- **`metriken-exposition`**: gains the row form of a group snapshot
  (`WalGroupRow`) and the `/metrics/stream` protocol, so any metriken
  service can be streamed by a recorder.
- **`metriken`**: gains groups with slots and occupant identity (see
  "Dynamic slots", below).

## Phases

All before 6.0, in an order that keeps each step building on code that has
already moved, so nothing new is written against a rezolus type and then
moved:

1. **`metriken-segment`.** It starts with the long layout from
   metriken-query and the occupant stream from rezolus#1315, which is held
   so its format code lands here directly. The wide segment writer
   (rezolus `TableBuilder`/`GroupTableBuilder`) moves here too, so every
   shape is written and read in one crate. The check is that rezolus's
   long-table and table-builder tests pass unchanged against the moved
   code.
2. **The wire form and stream protocol in `metriken-exposition`.** This
   covers `WalGroupRow` and the `/metrics/stream` endpoint (rezolus
   `src/agent/exposition/http/`). It comes before the writer because the
   writer's input is these rows.
3. **The archive reader in `metriken-archive`.** The generic parts of
   rezolus's `RezReader` move here: the `Catalog` trait
   (`crates/rez/src/catalog.rs`), the stream union, and cross-cadence
   evaluation. What stays in rezolus is choosing between recordings (the
   A/B slots, `--recording` selectors) and the `.rez` catalog. It comes
   before the writer so the writer's tests read what they write through
   the real reader.
4. **The archive writer in `metriken-archive`.** This is rezolus's 6.0
   step 3: a dendro-backed writer that writes groups with slots long, with
   their occupant stream, and materializes a long table's WAL tail as long.
   rezolus's recorder and hindsight use it with rezolus's defaults.
5. **Groups with slots and identity in `metriken`.** This covers slot
   identity, `__uid__` minting and dynamic slots. It needs its own design
   entry (see below). Phases 1–4 don't depend on it: they take occupants
   from any source.
6. **rezolus 6.0** builds on the result: the agent uses metriken's groups
   and identity and its stream endpoint, and the recorder, hindsight and
   viewer use `metriken-archive`.

Each phase lands as its own PR and release. Where a public path changes,
the old one is re-exported for one release.

## Dynamic slots

metriken's groups have a fixed capacity: `CounterGroup::new(entries)`
(`metriken/src/group/counter.rs:84`). rezolus's per-thread group is sized at
`MAX_PID` (4,194,304), which suits a key space the kernel assigns and
bounds. A cache server's tenants or connections are not bounded that way.
Its slots have to be assigned at runtime, given an identity when assigned,
and freed when the member leaves, so the recorder sees a new occupant when
a slot is reused. This is new design, and phase 5 gets its own entry
before it is built. The storage side does not depend on it, since the long
layout and the occupant stream take occupants from any source. Only
phase 5 waits on that design.

## Open questions

- Names: `metriken-segment` and `metriken-archive` are working names.
- Versioning: the WAL row and the segment format become public wire
  formats with more than one producer. Each needs a version that a reader
  checks.
- Whether `metriken-archive` depends on dendro unconditionally, or behind
  a feature.

## Not in this effort

- Changing dendro. It already knows nothing about metrics.
- Changing the query engine's semantics.
