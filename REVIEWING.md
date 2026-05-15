# Reviewing `yv/sql-testing` — metriken side

Companion: `/work/rezolus/REVIEWING.md` (consumer side).

This branch ships two things on the engine side:

1. **`metriken-query-sql`** — a new DuckDB-backed query engine
   (connection pool, H2 histogram UDFs, shared dashboard macros).
   It's live: the Rezolus static viewer's macro/UDF tests link it,
   and the server-backed viewer routes `/api/v1/query{,_range}`
   through it.
2. **A PromQL → SQL harness** behind a non-default `harness`
   feature. No production caller. Its fate is the merge-blocking
   open question; see *What the harness is for* below.

The pre-existing PromQL evaluator (`metriken-query::promql` +
`tsdb`, gated `feature = "legacy"`) is unchanged in its role on
this branch: it's the live engine for Rezolus MCP, `report-save`,
the dashboard crate's `Tsdb` re-export, and the viewer's live-mode
`validate_service_extensions` KPI check. Removing it entirely is
the topic of the *Removing Tsdb entirely* section in the rezolus
doc.

The evaluator did get one structural change before this branch:
commit `a25e285` collapsed it to a streaming-only dispatcher,
deleting ~2,700 LOC of eager pre-aggregation. Histogram quantiles,
`rate`/`irate`, `avg_over_time`, `idelta`, and the aggregate
matrix grouping body all stream now; a small eager residual
handles binary operators with `group_left`/`group_right` and the
scalar-passthrough backstop. The shadow-mode plumbing is gone.

---

## What the harness is for

`metriken-query/src/harness/translate.rs` opens with:

> **This whole module is migration scaffolding.** It exists to
> bridge Rezolus's still-PromQL dashboard emitter to the new SQL
> backend.

Today no production caller links it — only `tests/`, the
`sql_vs_promql` correctness harness, and the orphan-detector
catalogue health check. Rezolus's dashboard emitter, viewer, and
static viewer all bypass it entirely.

Two clean exits, neither chosen on this branch:

- **Land it.** Wire `harness::Engine` into Rezolus MCP — and
  optionally into `validate_service_extensions` and `report-save`.
  The MCP migration path (see rezolus doc) becomes "translate
  PromQL → SQL via `harness::Engine`, run through `DuckDbBackend`"
  rather than "rewrite MCP queries to SQL by hand". The harness
  becomes a regression suite for live code.
- **Delete it.** The `harness/` subdirectory + `queries.toml` is a
  clean delete: nothing else links it. `metriken-query-sql` stays
  unchanged (the static viewer depends on its UDFs and macros).

The harness sitting behind a non-default feature flag keeps the
question reversible — but only until someone has to take a side
to merge.

---

## `metriken-query-sql` — the new engine

Lives under `metriken-query-sql/src/`. Public surface in `lib.rs`
(small). Concurrency lives in `backend.rs`:

- `DuckDbBackend` is a per-source connection pool. First request
  for a parquet pays cold-start (open + register UDFs + macros +
  materialise `_src` and `_cgroup_index`); subsequent requests
  hit a warm slot.
- `catch_unwind` around the query body recovers from UDF-callback
  panics by evicting the slot, so a poisoned connection doesn't
  take the pool down. The ordering invariant is annotated inline
  — read the comment at the `catch_unwind` block before changing
  it; the `pool_invalidate.rs` stress test pins the contract.

The H2 histogram UDFs and `irate_lag` live in `udf.rs`. The
preamble (lines 1-37) is the spec for the unsafe pattern — it
documents two duckdb-rs gotchas (`ListVector::child(N)` zeroing
sibling LIST inputs past index 2048, `get_entry` returning
uninitialized memory for NULL rows) and the uniform
`child(0).as_slice_with_len(n)` fix. After reading those, one
`h2_*` impl is representative of all of them.

`shared_macros.sql` (re-exported as `SHARED_MACROS`) is the
single source of truth for the 20 SQL macros that both the native
engine and the WASM `viewer-sql` register on their connections.
This is what stops native ↔ WASM emitter drift: the bytes are
literally the same, parsed by both sides.

---

## Cargo features and consumers

| Feature | What it pulls in | Consumers |
|---|---|---|
| `legacy` (default) | The streaming PromQL evaluator + `Tsdb` | The live PromQL surface enumerated in the rezolus doc. |
| `ingest` | `legacy` + `metriken-exposition` (parquet→Tsdb) | The Rezolus binary (live-agent ingest path). |
| `harness` (off) | The PromQL→SQL translator + `harness::Engine` | None today. See above. |
| `lz4` (default) | `parquet/lz4` | — |

`legacy` and `harness` share only `result.rs`. Each combination
builds clean. The Rezolus binary uses `ingest` with
`default-features = false` (so no `lz4` from this dep); the
dashboard crate uses `legacy`; the static viewer doesn't link
this crate at all.

---

## Verification

`metriken-query/examples/sql_vs_promql.rs` runs every catalogue
entry through both the PromQL evaluator and the SQL pipeline
against the same parquet, and diffs canonical-JSON results
plot-by-plot:

```bash
cargo run --release --example sql_vs_promql --features "legacy,harness" \
  -- /work/rezolus/site/viewer/data/demo.parquet
```

Output goes to `/tmp/sql_vs_promql_yv/`; `summary.json` is the
top-level result. Rerun before opening the PR — it's the only
thing that catches semantic drift between the two engines on real
data.

`metriken-query-sql/CAUTION.md` is the running catalog of known
PromQL ↔ SQL semantic divergences this harness has surfaced.
Check it before being surprised by a "PromQL says X, SQL says Y"
finding.

---

## Where to spend attention

1. **The land-or-delete decision** on the harness. Everything else
   is conditional on this.
2. **`metriken-query-sql/src/lib.rs` + `backend.rs`** — public
   surface and the only non-trivial concurrency story
   (panic-safe slot evacuation).
3. **`udf.rs` preamble + one `h2_*` UDF impl.** The unsafe
   pattern is uniform across all blocks.
4. **Run `sql_vs_promql`** against a real parquet; skim
   `summary.json`.
5. **Skip `harness/translate.rs`** (~2,400 lines of switch
   table) unless deciding *land*. Its fate determines whether
   reviewing the table internals matters.
