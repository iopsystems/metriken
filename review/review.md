# Reviewing `yv/sql-testing` — metriken side

Companion: `/work/rezolus/review/review.md` (consumer side).

This branch ships two engine-side deliverables and one big
deletion:

1. **`metriken-query-sql`** — the DuckDB-backed query engine
   (connection pool, H2 histogram UDFs, shared dashboard macros).
   The Rezolus static viewer's macro/UDF tests link it, and the
   server-backed viewer routes `/api/v1/query{,_range}` through
   it for *both* file mode and live mode.
2. **`LiveSource`** — a single-`Connection` in-memory DuckDB
   ingest that grows `_src` snapshot-by-snapshot via `ALTER +
   INSERT`. Lets rezolus's live-agent path share the SQL pipeline
   with the parquet path. See "LiveSource" below.
3. **`metriken-query` deleted.** The legacy PromQL evaluator
   (`tsdb/` + `promql/`) + migration-scaffolding harness
   (`harness/`) — ~13K LOC across the three subdirs, plus the
   `queries.toml` shape catalogue and three feature-gated examples
   — were removed in C5 of this branch along with the crate itself
   (~16.7K LOC total). The deletion was unblocked by C2-C4 on the
   rezolus side (see the rezolus doc's `Tsdb removed — historical
   roadmap` for the sequence).

The evaluator did get one structural change before this branch:
commit `a25e285` collapsed it to a streaming-only dispatcher,
deleting ~2,700 LOC of eager pre-aggregation. Histogram quantiles,
`rate`/`irate`, `avg_over_time`, `idelta`, and the aggregate
matrix grouping body all stream now; a small eager residual
handles binary operators with `group_left`/`group_right` and the
scalar-passthrough backstop. The shadow-mode plumbing is gone.

---

## Harness fate — deleted in C5

`metriken-query/src/harness/translate.rs` opens with:

> **This whole module is migration scaffolding.** It exists to
> bridge Rezolus's still-PromQL dashboard emitter to the new SQL
> backend.

No production caller has ever linked it — only `tests/`, the
`sql_vs_promql` correctness harness, and the orphan-detector
catalogue health check. Rezolus's dashboard emitter, viewer, static
viewer, MCP, report-save, `parquet annotate`, and (post-LiveSource)
the live-agent query path all bypass it entirely.

The two clean exits remained open until LiveSource shipped:

- **Land it.** Wire `harness::Engine` into the remaining PromQL
  holdouts on the rezolus side — `validate_service_extensions` and
  `report-save`'s live-mode query embed.
- **Delete it.** The `harness/` subdirectory + `queries.toml` is a
  clean delete.

LiveSource closed the question by routing the live-agent query
path through `DuckDbBackend::run_sql` directly, with no PromQL
involved at the query layer. The two remaining harness-candidate
consumers (`validate_service_extensions`, report-save's live-mode
query embed) are the same two the rezolus doc lists as "Removing
Tsdb entirely" items B and A respectively — both are SQL-rewrite
work, not harness wiring. So the harness has no remaining
plausible consumer.

**Outcome: deleted.** C5 of this branch removed
`metriken-query/src/harness/`, `queries.toml`, and the three
examples (`sql_vs_promql.rs`, `wide_form_coverage.rs`,
`enumerate_rezolus_queries.rs`) together with the rest of the
legacy modules and the entire `metriken-query` crate.

---

## `metriken-query-sql` — the engine

Lives under `metriken-query-sql/src/`. Public surface in `lib.rs`
(small). Modules: `backend.rs` (pool + dispatch), `live.rs`
(LiveSource), `views.rs` (parquet introspection + `MetricCatalog`
+ `render_*_sql`), `udf.rs` (H2 UDFs), `macros.rs` (loader;
re-exports `shared_macros.sql` as `SHARED_MACROS`),
`observability.rs` (`BackendStats`).

Concurrency lives in `backend.rs`:

- `DuckDbBackend` is a per-source connection pool. First request
  for a parquet pays cold-start (open + register UDFs + macros +
  materialise `_src`, `_cgroup_index`, and `_src_<source>`
  per-source views (`0d68861`)); subsequent requests hit a warm
  slot.
- **Slot acquisition.** Round-robin picks a starting slot; the
  dispatcher scans all slots non-blockingly via `try_lock`
  (`1e5a2a2`) and takes the first idle one. Falls back to blocking
  on the round-robin pick only when every slot is busy. Eliminates
  the "queued behind a slow slot while peers idle" pathology
  (`backend.rs:311-327`).
- `catch_unwind` around the query body recovers from panics in
  Rust code DuckDB calls back into (e.g. Arrow conversion bugs) by
  evicting the slot. UDF-callback panics cross the duckdb-rs FFI
  as non-unwinding and abort the process before `catch_unwind`
  sees them — what this boundary actually catches is the in-Rust
  callback layer above the UDFs. The ordering invariant is
  annotated inline; the contract test
  `backend.rs::tests::empty_slot_is_lazily_rebuilt_without_mutex_poisoning`
  pins it.

The H2 histogram UDFs and `irate_lag` live in `udf.rs`. The
preamble (lines 1-37) is the spec for the unsafe pattern — it
documents two duckdb-rs gotchas (`ListVector::child(N)` zeroing
sibling LIST inputs past index 2048, `get_entry` returning
uninitialized memory for NULL rows) and the uniform
`child(0).as_slice_with_len(n)` fix. After reading those, one
`h2_*` impl is representative of all of them.

`shared_macros.sql` (re-exported as `SHARED_MACROS`) is the
single source of truth for the 20 SQL macros that both the native
engine and the rezolus WASM viewer (`/work/rezolus/crates/viewer-sql/`)
register on their connections. This is what stops native ↔ WASM
emitter drift: the bytes are literally the same, parsed by both
sides.

## LiveSource (`live.rs`)

In-memory DuckDB ingest for sub-second snapshots. ~800 LOC. New on
this branch (`17f1107`); the rezolus side consumes it via
`src/viewer/live_ingest.rs`.

- **Single shared `Mutex<Connection>`, not a pool.** `Connection`
  is `!Sync`; pool model can't apply because each parquet-pool slot
  is an independent in-memory DB. Live mode needs one *shared*
  mutable `_src` table across all reads and writes; one connection
  + Mutex serializes them.
- **Schema growth.** `LiveSource::append(timestamp, duration,
  columns)` runs three phases under one mutex acquisition: (1)
  diff against the known schema, emit `ALTER TABLE _src ADD COLUMN
  <physical> <type_sql>` for each new column; (2) if any added
  column is `cgroup_*`, rebuild `_cgroup_index` via the same
  `render_cgroup_index_sql` the parquet path uses; (3) INSERT one
  row, with NULL for columns absent from this snapshot. `_src_<source>`
  is a pass-through `SELECT *` view that auto-picks up new columns.
- **`DuckDbBackend` integration.** `live_sources:
  Mutex<HashMap<String, Arc<LiveSource>>>` parallel to the parquet
  `connections` map. `run_sql` / `describe_parquet` / `invalidate`
  all check live sources ahead of the parquet pool. Same API; the
  backend dispatches on the data-source string.
- **`canonical_column_name` public free fn** (`494b4fc`). Mirrors
  the parquet path's `canonical_alias` rule (strip `<src>::`
  prefix, build `metric/v1/v2/...:buckets?` from sorted value
  labels, infrastructure keys excluded) so `_src` column names
  built by external bridges are byte-identical to what the parquet
  path produces. Without this, rezolus agents emitting numeric
  label values ended up with `_src` columns named `49, 50, 51, ...`
  and dashboard SQL targeting `^cpu_usage(/[^:]+)?$` matched nothing.
- **Test coverage.** L1: 10 tests in `tests/live.rs` (round-trip,
  time-range bounds, schema growth across kinds, NULL semantics,
  cgroup_index rebuild, per-source view, timestamp snap-to-interval,
  concurrent read+write, bad-SQL surfacing). L2: 5 tests in
  `live.rs::tests` (cross-engine parity — replay parquet
  rows into a LiveSource and assert byte-identical Arrow output for
  SELECT/COUNT/MIN/MAX/SUM/irate_1s/h2_*). The L2 parity tests are
  the load-bearing regression catch: if live and parquet ever
  diverge on identical input data, this fails first.

---

## Cargo features and consumers

Post-C5: the `metriken-query` crate is deleted. `metriken-query-sql`
has no feature flags — single configuration, single build. The
rezolus WASM viewer (`/work/rezolus/crates/viewer-sql/`) and
rezolus itself both link `metriken-query-sql` directly.

## metriken-exposition: deterministic parquet footer

`metriken-exposition/src/parquet.rs:319` sorts file-level
KV metadata by key before writing the Parquet footer. Output is
semantically identical to before — Parquet readers don't depend
on KV order — but the byte layout is now stable run-to-run. Flagged
here because downstream byte-fingerprint comparisons of recorder
output will see a one-time shift.

---

## Rezolus-side follow-up: full PromQL purge (engine side: no-op)

The rezolus-side PromQL purge (P1-P6) has landed: no
`plot_promql*` family, no `Plot.promql_query`, no `Kpi.query`, no
template `"query":` strings. See
`/work/rezolus/review/review.md::PromQL purge — completed (P1-P6)`.

The engine side had no code to delete — `metriken-query-sql`
already speaks DuckDB SQL exclusively. The follow-up KPI
transcription work (compound expressions + histogram-percentile
fan-outs) landed in rezolus commits `9b9165f` / `9daefc6` /
`cd92f18`; 209/218 KPIs ship SQL today. The remaining engine
plumbing — `MetricCatalog::nodes()` for per-node `_src_node_<X>`
views — landed in commit `33f5fe2`.

---

## Verification

```bash
# Engine-side workspace, all tests
cargo test --workspace --all-features --all-targets

# Just the engine
cargo test -p metriken-query-sql
```

The load-bearing checks are the L1 + L2 tests inside the engine:
**L1** (`metriken-query-sql/tests/live.rs`) covers the LiveSource
externally — round-trip, time-range bounds, schema growth across kinds,
NULL semantics, cgroup-index rebuild, per-source view, timestamp
snap-to-interval, concurrent read+write, bad-SQL surfacing. **L2** (`metriken-query-sql/src/live.rs::tests`) is the
cross-engine parity gate: it replays parquet rows into a `LiveSource`
and asserts byte-identical Arrow output for SELECT / COUNT / MIN /
MAX / SUM / `irate_1s` / `h2_*`. If live and parquet ever diverge
on identical input, this fails first.

### Historical: `sql_vs_promql`

Pre-C5, the `metriken-query/examples/sql_vs_promql.rs` harness ran
every dashboard plot through both the PromQL evaluator (via the
harness translator) and the SQL pipeline against the same parquets,
diffing canonical-JSON results plot-by-plot. Final run before
deletion across `demo.parquet`, `AB_level_pin.parquet`, and
`AB_base.parquet`: **698 identical / 1 divergent / 0 errors**. The
only divergence was `numa-local-rate` on `AB_base.parquet`,
`rel ≈ 2.7e-5` at the last eval point — floating-point residual from
the 300-second RANGE arithmetic; sub-tolerance under any sane
`--rel-tol`. The harness, its translator, and the example were
deleted in C5 along with the rest of `metriken-query`; the L2 parity
tests are now the regression catch.

No PromQL ↔ SQL divergences other than the sub-tolerance one above
were known on the dashboard path at the moment of deletion.

---

## Where to spend attention

1. **`metriken-query-sql/src/lib.rs` + `backend.rs`** — public
   surface and the non-trivial concurrency story (panic-safe slot
   evacuation, `try_lock` fast-path slot acquisition).
2. **`live.rs`** — the largest new module on this branch; the
   `Mutex<Connection>` + `ALTER + INSERT` model is the new ingest
   shape. Read alongside the L2 parity tests at the bottom of the
   file.
3. **`udf.rs` preamble + one `h2_*` UDF impl.** The unsafe
   pattern is uniform across all blocks.
4. **`views.rs::render_per_source_views_sql`** — the multi-source
   projection that makes service-extension `{{view}}` templates
   bind across `vllm`, `sglang`, `valkey`, etc.
5. **L2 parity tests** in `live.rs::tests` — the cross-engine
   regression catch now that `sql_vs_promql` is gone. Run alongside
   the L1 external tests in `tests/live.rs`; any drift between
   parquet-pool and live-source semantics fails here first.
