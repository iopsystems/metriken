# Reviewing `yv/sql-testing` — metriken side

Companion: `/work/rezolus/review/review.md` (consumer side).

This branch ships three things on the engine side:

1. **`metriken-query-sql`** — a DuckDB-backed query engine
   (connection pool, H2 histogram UDFs, shared dashboard macros).
   It's live: the Rezolus static viewer's macro/UDF tests link it,
   and the server-backed viewer routes `/api/v1/query{,_range}`
   through it for *both* file mode and live mode.
2. **`LiveSource`** — a single-`Connection` in-memory DuckDB ingest
   that grows `_src` snapshot-by-snapshot via `ALTER + INSERT`.
   Lets rezolus's live-agent path share the SQL pipeline with the
   parquet path. See "LiveSource" below.
3. **A PromQL → SQL harness** behind a non-default `harness`
   feature. No production caller. The land-or-delete question has
   converged on **delete** now that LiveSource shipped — see
   *Harness fate* below.

The pre-existing PromQL evaluator (`metriken-query::promql` +
`tsdb`, gated `feature = "legacy"`) is unchanged in its role on
this branch. Its surviving consumers, post the MCP and
report-save-column-trim migrations on the rezolus side, are:
the dashboard crate's `Tsdb` re-export, the viewer's live-mode
`validate_service_extensions` KPI check, and `report-save`'s
live-mode query embed (the column-trim half moved to
`metriken-query-sql::MetricCatalog`). Removing it entirely is the
topic of the *Removing Tsdb entirely* section in the rezolus doc
— now mid-execution in this same branch's C2-C5 commits; the
engine-side fallout (delete `metriken-query/src/{promql, tsdb,
harness}`) is C5.

The evaluator did get one structural change before this branch:
commit `a25e285` collapsed it to a streaming-only dispatcher,
deleting ~2,700 LOC of eager pre-aggregation. Histogram quantiles,
`rate`/`irate`, `avg_over_time`, `idelta`, and the aggregate
matrix grouping body all stream now; a small eager residual
handles binary operators with `group_left`/`group_right` and the
scalar-passthrough backstop. The shadow-mode plumbing is gone.

---

## Harness fate — deleting in C5

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

**Decision: delete.** C5 of this branch removes
`metriken-query/src/harness/`, `queries.toml`, and the three
examples (`sql_vs_promql.rs`, `wide_form_coverage.rs`,
`enumerate_rezolus_queries.rs`) together with the rest of the
legacy modules.

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
engine and the WASM `viewer-sql` register on their connections.
This is what stops native ↔ WASM emitter drift: the bytes are
literally the same, parsed by both sides.

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
  schema growth across kinds, NULL semantics, cgroup_index rebuild,
  per-source view, concurrent read+write, bad-SQL surfacing). L2:
  5 tests in `live.rs::tests` (cross-engine parity — replay parquet
  rows into a LiveSource and assert byte-identical Arrow output for
  SELECT/COUNT/MIN/MAX/SUM/irate_1s/h2_*). The L2 parity tests are
  the load-bearing regression catch: if live and parquet ever
  diverge on identical input data, this fails first.

---

## Cargo features and consumers

| Feature | What it pulls in | Consumers | Fate |
|---|---|---|---|
| `legacy` (default) | The streaming PromQL evaluator + `Tsdb` | The live PromQL surface enumerated in the rezolus doc. | Deleted in C5. |
| `ingest` | `legacy` + `metriken-exposition` (parquet→Tsdb) | The Rezolus binary (live-agent ingest path). | Deleted in C5. |
| `harness` (off) | The PromQL→SQL translator + `harness::Engine` | None today. | Deleted in C5. |
| `lz4` (default) | `parquet/lz4` | — | Survives. |

`legacy` and `harness` share only `result.rs`. Each combination
builds clean. The Rezolus binary uses `ingest` with
`default-features = false` (so no `lz4` from this dep); the
dashboard crate uses `legacy`; the static viewer doesn't link
this crate at all. After C5 the only surviving consumers of
anything in `metriken-query` will be tests of the `metriken-query-sql`
crate (which doesn't depend on `metriken-query`).

---

## Verification

`metriken-query/examples/sql_vs_promql.rs` runs every dashboard
plot through both the PromQL evaluator (via the harness) and the
SQL pipeline against the same parquet(s), and diffs canonical-JSON
results plot-by-plot:

```bash
cargo run --release --example sql_vs_promql --features "legacy,harness" -- \
  --dashboard-dir /tmp/dashboard_json \
  --parquets /work/rezolus/site/viewer/data/demo.parquet \
  --out /tmp/sql_vs_promql
```

(Generate the dashboard-dir input with
`cargo run -p dashboard -- /tmp/dashboard_json` from the rezolus
tree — it dumps one JSON per section, which the harness walks for
plot specs.) `summary.json` in the chosen `--out` directory is the
top-level result; `--max-plots N` caps the run for a quick smoke,
`--rel-tol` / `--abs-tol` tune the diff. Rerun before opening the
PR — it's the only thing that catches semantic drift between the
two engines on real data.

### Known divergences

Across the three real fixtures (`demo.parquet`, `AB_level_pin.parquet`,
`AB_base.parquet`) the harness reports **698 identical / 1 divergent**
on the live dashboard SQL path. The remaining divergence and two
gaps in the scaffolding translator:

- **Sub-tolerance: `numa-local-rate` on `AB_base.parquet`.** `rel ≈
  2.7e-5` at the last eval point — floating-point residual from the
  300-second RANGE arithmetic. Loosening `--rel-tol` from 1e-9 to
  1e-4 moves it into the `within_tolerance` bucket. Not actionable.

- **Wide-form translator** (`metriken-query/src/harness/translate.rs`,
  behind the off-by-default `harness` feature). Emits SQL that
  ignores the PromQL `[range]` argument (uses immediate `LAG`
  regardless of how far back the prior sample is) and doesn't honor
  the `query_range` step parameter (one output row per raw `_src`
  timestamp). The translator's only consumers today are the
  `wide_form_coverage` example and the harness-feature test suite —
  neither gap surfaces in the dashboard SQL path. Both either land
  (translator becomes a production caller; fixes follow) or die with
  the translator under the land-or-delete decision above.

No other PromQL ↔ SQL divergences are known on the dashboard path.

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
5. **Run `sql_vs_promql` once before C5** against demo /
   AB_level_pin / AB_base; record the final divergence count for
   posterity, then watch the harness + evaluator + diff tool all
   delete together. Skip `harness/translate.rs` internals — they're
   leaving.
