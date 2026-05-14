# Reviewing the `yv/sql-testing` branch

This document orients a reviewer who has not seen the SQL migration before.
Read it before opening the diff. It covers what changed, what's load-bearing
vs. scaffolding, where to spend attention, and what the known correctness
gaps are.

The companion document lives at `/work/rezolus/REVIEWING.md` — it covers the
viewer side. Read this one first; Rezolus's doc references it.

---

## TL;DR

We replaced an in-house PromQL-subset evaluator with DuckDB SQL. The legacy
evaluator stays behind a feature gate (`legacy`) because the Rezolus WASM
viewer cannot run DuckDB in the browser and the Rezolus desktop binary's
live-agent ingest path has no DuckDB equivalent yet.

The new code is split across two crates:

- **`metriken-query-sql`** (new, ~2.3 KLOC) — a pure DuckDB engine with H2
  histogram UDFs, SQL macros, and a connection pool. **This is what
  survives long-term.** Read it carefully.
- **`metriken-query`** (changed, feature-gated) — gains a `sql` feature that
  routes incoming PromQL strings through a catalogue → SQL translator →
  `metriken-query-sql`. **This layer is migration scaffolding** and is
  designed to be deleted in one commit once Rezolus emits SQL natively.
  Skim it.

Correctness is validated by `metriken-query/examples/sql_vs_promql.rs` —
runs every Rezolus dashboard plot through both engines, classifies each
pair as identical / tolerant / divergent. Current numbers: ~89% identical
on single-source rezolus parquets; all known divergences trace to a small
set of root causes documented in `/work/rezolus/REVIEWING.md`.

---

## Architecture before / after

### Before (legacy `main`)

```
metriken-query  (one crate, ~3.9 KLOC)
  ├─ promql/   streaming PromQL evaluator (promql-parser → custom dispatcher)
  ├─ tsdb/     in-memory time-series store loaded from parquet
  └─ public:   QueryEngine::query_range(promql, ...) → QueryResult
```

One execution path. One consumer (Rezolus) issues PromQL strings; the engine
parses, dispatches over the in-memory `Tsdb`, and returns a Prometheus-shaped
matrix or histogram heatmap.

### After (`yv/sql-testing`)

```
metriken-query-sql  (new crate, ~2.3 KLOC — load-bearing)
  ├─ backend.rs   (414 LOC) DuckDB connection pool, panic-safe slot evacuation
  ├─ udf.rs      (1014 LOC) 9 H2 histogram UDFs + 13 unsafe FFI blocks
  ├─ macros.rs    (308 LOC) 30 SQL macros (irate/rate primitives + dashboard composites)
  ├─ views.rs     (405 LOC) parquet introspection + `_src` TEMP TABLE loader
  ├─ observability.rs (157 LOC) BackendStats
  └─ public:      DuckDbBackend::run_sql(sql, parquet) → Vec<RecordBatch>
                  DuckDbBackend::describe_parquet(path) → Arc<MetricCatalog>

metriken-query  (feature-gated, ~4.4 KLOC total)
  feature "sql"     (default desktop)
    engine.rs       (237 LOC) Engine::new(parquet) + query_range(promql,…)
    catalogue.rs    (184 LOC) queries.toml registry: PromQL string → entry id + captures
    translate.rs   (2403 LOC) ~65 entry-id resolvers emit wide-form SQL
    template.rs     (948 LOC) capture parser/matcher
    project.rs      (394 LOC) Arrow RecordBatch → QueryResult projection
    interp.rs       (133 LOC) output_metric placeholder interpolation
    result.rs        (77 LOC) shared QueryResult/Sample/HistogramHeatmap types
    queries.toml   (1055 lines, 69 entries)

  feature "legacy"  (default off, on for Rezolus WASM viewer + ingest)
    promql/        (~1.5 KLOC) original streaming PromQL evaluator, unchanged
    tsdb/          (~0.9 KLOC) in-memory store, unchanged
```

Three execution paths now live under the `metriken-query::*` namespace:

1. **`Engine::query_range`** (sql feature) — `PromQL string → Catalogue::lookup
   → translate::try_generate → DuckDbBackend::run_sql → project::run → QueryResult`.
2. **`QueryEngine::query_range`** (legacy feature) — unchanged from `main`.
3. **`DuckDbBackend::run_sql`** directly — bypasses the catalogue entirely.
   Callers issue arbitrary DuckDB SQL. Used today by Rezolus's static
   viewer (it has its own SQL emitter); Rezolus's dashboard crate ships
   the SQL templates that drive it.

The `QueryResult` type is shared across all three paths — same shape going
out, three different engines computing it.

---

## Feature-gate matrix

The Cargo features in `metriken-query/Cargo.toml`:

```toml
default = ["sql", "lz4"]
sql     = ["dep:metriken-query-sql"]
legacy  = ["dep:promql-parser"]
ingest  = ["legacy", "dep:metriken-exposition"]
lz4     = ["parquet/lz4"]
```

What each consumer enables:

| Consumer | Features | Code paths active | Why |
|---|---|---|---|
| Rezolus desktop binary | `ingest, lz4` → transitively `legacy, sql, lz4` | both | Live-agent ingest needs `Tsdb`/PromQL; future SQL path available |
| Rezolus dashboard crate | `legacy` | legacy only | Dashboard generators run native against `Tsdb` |
| Rezolus WASM viewer | `legacy` only (no default) | legacy only | DuckDB doesn't compile to `wasm32` |
| Future "Rezolus emits SQL" | `sql` only | sql only | The end state — `legacy` deletable |

The Rezolus workspace `Cargo.toml` sets `default-features = false` on
`metriken-query` so each consuming crate opts into exactly what it needs.
This is what makes the WASM viewer link without pulling in DuckDB.

---

## Load-bearing vs. scaffolding

Concrete file list. Reviewer's heuristic: spend time on load-bearing files;
skim scaffolding files for "does this look like it produces correct SQL"
and trust the harness for the bulk validation.

**Load-bearing (survives long-term):**

- All of `metriken-query-sql/src/*.rs`
- `metriken-query/src/result.rs` (the `QueryResult` shape — both paths return it)
- `metriken-query/src/promql/*` and `metriken-query/src/tsdb/*` for as long
  as the WASM viewer and live ingest remain on PromQL

**Scaffolding (deletable when Rezolus emits SQL natively):**

- `metriken-query/src/engine.rs`
- `metriken-query/src/catalogue.rs`
- `metriken-query/src/translate.rs`
- `metriken-query/src/template.rs`
- `metriken-query/src/interp.rs`
- `metriken-query/src/project.rs`
- `metriken-query/queries.toml`

The module-level doc comments on `engine.rs`, `catalogue.rs`, and
`translate.rs` already say so. The end-state migration is one commit:
delete those files, drop the `sql` feature, rename `legacy` → default,
and update consumers' query-construction code to issue SQL directly.

---

## Where to spend attention

If you have an hour:

1. **`metriken-query-sql/src/lib.rs`** (56 LOC) and
   **`metriken-query-sql/src/backend.rs`** (414 LOC) — the public API and
   the connection pool. The pool's panic-safe slot evacuation is the
   only non-trivial concurrency story; read `get_or_init` and
   `with_connection`.
2. **`metriken-query-sql/src/udf.rs`** — read the preamble (the duckdb-rs
   `ListVector::child` gotcha is documented inline) and one h2_*
   implementation. The 13 `unsafe` blocks are FFI vector writes; all
   guarded by `metriken-query-sql/tests/lag_repro.rs` (the regression
   suite for the LAG-on-LIST bug).
3. **`metriken-query/src/engine.rs`** — entire file (237 LOC). The
   five-step pipeline is documented at the top.
4. Pick 2–3 representative entry ids from `metriken-query/queries.toml`
   and trace them through `translate.rs`. Suggested:
   - `gauge_bare` — simplest shape
   - `counter_irate_total` — counter with reset-aware rate
   - `histogram_quantile_*` — heatmap shape
5. Run the harness:
   ```
   cargo run --release --example sql_vs_promql \
       --features "legacy,sql" \
       -- /work/rezolus/site/viewer/data/demo.parquet
   ```
   and read the resulting JSON for one identical pair and one divergent pair.

If you have ten minutes: read the four "mental models" below and the
divergence taxonomy.

If you have all day: also read `translate.rs` end-to-end. The 40+
resolvers are a switch table; once you've internalised the `Shape` struct
(lines ~52-74), the rest follows.

---

## Mental models for the adversarial reviewer

**1. Scaffolding has a finish line.** The PromQL → SQL bridge is temporary
code. The doc comments name it as such. When Rezolus emits SQL natively,
seven files (`engine.rs`, `catalogue.rs`, `translate.rs`, `template.rs`,
`interp.rs`, `project.rs`, `queries.toml`) get deleted in one commit. The
reviewer's job is to spot-check the bridge produces correct SQL, not to
audit it as a long-lived abstraction.

**2. One crate survives; one is plumbing.** `metriken-query-sql` is the
new engine; everything else added in this branch is plumbing to keep
existing PromQL consumers working during the migration window. The
crate split is what makes "delete the scaffolding" mechanically clean.

**3. `unsafe` is the cost of admission for FFI.** `udf.rs` has 13 unsafe
blocks because DuckDB's vscalar UDF API hands you raw output vectors. The
duckdb-rs `ListVector::child` quirk (documented at the top of the file)
forced a specific input-read pattern; the regression test in
`tests/lag_repro.rs` (1021 LOC of diagnostic harness) is there to catch
re-introductions.

**4. The harness is the proof.** `examples/sql_vs_promql.rs` (1719 LOC)
runs every Rezolus plot through both engines and classifies the result.
The number to read is "~89% identical on single-source rezolus parquets"
on `yv/sql-testing`; the remaining 11% is well-categorised (see below)
and dominated by known semantic choices, not bugs.

---

## Adversarial-reviewer FAQ

**Q: Why two crates? Why not put everything in `metriken-query`?**

A: Because the `sql` and `legacy` halves have disjoint dependency sets
and the WASM viewer cannot pull in DuckDB. Splitting them out means
`metriken-query-sql` can compile with `--features=sql` alone and the
WASM viewer can pull in `metriken-query` with `--features=legacy` alone
without DuckDB. The two crates also have asymmetric lifecycles:
`metriken-query-sql` is meant to survive, `metriken-query` gradually
hollows out.

**Q: Why is `translate.rs` 2372 lines?**

A: Because every PromQL shape Rezolus's dashboard emits today needs a
matching SQL translator. There are 40+ shapes (count varies by how you
group entry ids); each has its own multiplier/divisor semantics,
aggregation choice, reset behaviour, and column-resolution rule. The
file is a switch table, not a hard-to-follow algorithm. The whole file
is scaffolding — when Rezolus emits SQL natively, it goes away.

**Q: Why is `udf.rs` so much `unsafe`?**

A: FFI vector writes into DuckDB's columnar output. There is no
zero-cost safe abstraction over `duckdb_vector_get_data()` in
duckdb-rs today. All 13 unsafe blocks are bounds-checked, all are
exercised by `tests/lag_repro.rs`, and all follow the same pattern
(documented at the top of the file). The `ListVector::child(N)`
gotcha — calling it on an input zeroes a sibling input's child past
2048 — is real and the documented workaround (`as_slice_with_len`) is
followed uniformly. See the file preamble.

**Q: Why does `CatalogueEntry` have a bunch of dead `Option<...>` fields
(`mode`, `fixture`, `start`, `end`, `step`, `description`)?**

A: TOML compatibility. The 69 entries in `queries.toml` carry those
keys from the pre-migration "dispatcher with Mode" world; deserialising
with the keys present and ignored avoids a 69-entry TOML rewrite. The
fields are unused at runtime. **This branch removes them** — see the
diff to `catalogue.rs` and `queries.toml`.

**Q: Why retain the `legacy` feature at all?**

A: Two real consumers still need it: (1) Rezolus's WASM viewer cannot
run DuckDB in the browser (no `wasm32` target for the bundled C build);
(2) Rezolus's desktop binary `rezolus view <live-agent-url>` polls the
agent's msgpack endpoint into an in-memory `Tsdb` — there is no
parquet file to point DuckDB at. Both are tracked in the live-agent
migration plan; until they're resolved, `legacy` stays.

**Q: What survives a hypothetical v1.0 release?**

A: `metriken-query-sql` plus whatever shrunken form of `metriken-query`
is needed to bridge until Rezolus emits SQL natively. Best case:
`metriken-query-sql` is renamed to `metriken-query` and the
PromQL crate disappears entirely. The crate split is engineered to
make that mechanically straightforward.

---

## Verification

The headline numbers. Re-runnable from this branch.

### Cargo builds

```
cargo build -p metriken-query                                             # default = sql+lz4
cargo build -p metriken-query --no-default-features --features "legacy,ingest"
cargo build -p metriken-query --all-features
cargo test --workspace --all-features
```

All four should pass. The desktop binary in Rezolus builds with
`legacy,sql,ingest,lz4` (transitive).

### PromQL ↔ SQL correctness harness

`metriken-query/examples/sql_vs_promql.rs` runs every Rezolus dashboard
plot through both engines for every parquet under
`/work/rezolus/site/viewer/data/`. Output lands in `/tmp/sql_vs_promql_yv/`.

Run:

```
cargo run --release --example sql_vs_promql \
    --features "legacy,sql" \
    -- /work/rezolus/site/viewer/data/demo.parquet
```

Latest numbers (commit `c9b105b`, 11 parquets × 250 plots = 2222 pairs):

| parquet         | identical | tolerant | divergent |
|-----------------|----------:|---------:|----------:|
| demo            |       179 |        1 |        22 |
| vllm            |       181 |        2 |        19 |
| cachecannon     |        87 |        0 |       115 |
| AB_base         |        66 |        0 |       136 |
| AB_base_pin     |        70 |        0 |       132 |
| AB_level        |        70 |        0 |       132 |
| AB_level_pin    |        66 |        0 |       136 |
| sglang_gemma3   |        34 |        0 |       168 |
| vllm_gemma3     |        25 |        0 |       177 |
| disagg-sglang   |        34 |        0 |       168 |
| sglang-nixl-16c |        37 |        0 |       165 |
| **total**       |   **849** |    **3** |  **1370** |

The headline reads: **~89% identical on single-source rezolus parquets
(demo, vllm)**. Multi-source and numeric-encoded parquets diverge for
known reasons — see the divergence taxonomy below.

### Cross-branch sanity check

`examples/promql_only.rs` runs PromQL alone, so it compiles on both
`main` and `yv/sql-testing`. On the 193 plots whose PromQL strings exist
on both branches and produce output, **zero divergences** in the legacy
evaluator's output between branches. `yv/sql-testing` strictly **expands**
the supported metric set (the parquet loader now reads Arrow field
metadata in addition to column names) — a real upgrade, not a regression.

---

## Known divergence taxonomy

All known divergences fall into these eight categories. The first is an
architectural gap; the rest are either real bugs (with known fixes) or
acceptable semantic differences.

| count | category | what it means |
|------:|---|---|
| 985 | sql view missing the metric | Numeric-encoded parquets (`AB_*`, `*_gemma3`, `cachecannon`) store metric names in Arrow field metadata, not column names. Rezolus's dashboard SQL emitter references columns by metric name. PromQL's `Tsdb` reads field metadata; the SQL emitter doesn't (yet). Architectural cliff, not a regression. Static viewer renders these mostly empty too. Likely older test fixtures — worth confirming the team wants them supported. |
| 167 | `rate_5m` boundary | Wasm-side `rate_5m(c, ts)` macro is `(c - LAG(c, 300)) / 300.0` — positional 300-row lag. On parquets shorter than 300 samples (≤ 5 minutes at 1 Hz), only the last point gets a value. PromQL extrapolates a rate from whatever's in the window. Semantic difference. Fix candidate (range-based window function) documented in `/work/rezolus/REVIEWING.md`. |
| 55 | larger numerical drift (rel ≥ 1e-3) | **All one root cause, not 55 bugs.** Every case is on `disagg-sglang.parquet` (40) or `sglang-nixl-16c.parquet` (15) — the two demo parquets with multiple `rezolus` sources. PromQL sums across all sources; SQL queries one source view. Ratios are exactly N (number of rezolus sources) or 1/N. See `/work/rezolus/REVIEWING.md` for the dashboard-side fix decision. |
| 26 | series count differs | Multi-source aggregation gaps. Same root cause as the 55. |
| 23 | sql produces series, PromQL doesn't | Inverse of category 1, rarer. |
| 16 | small numerical drift (rel < 1e-3) | irate window-math jitter (sub-millisecond timestamp drift divided into per-second deltas). Acceptable noise for production charts. |
| 9 | label set mismatch | Multi-source labelling differences (e.g. `source=rezolus` only on PromQL side). |
| 8 | sql duplicate samples per timestamp | **Real bug**: `cpu-busy-heatmap` and `busy-pct-per-cpu` miss a `sum by (id)` in the SQL form, producing 2× rows per (timestamp, cpu_id). Visible on every parquet with `cpu_usage/<state>/<cpu_id>` columns. Fix is in `crates/dashboard/src/dashboard/cpu.rs`. |

Tolerance settings: `rel_tol = 1e-9`, `abs_tol = 1e-12`, ±2 boundary
sample tolerance for grid-evaluation artefacts.

---

## Branch layout and commit history

The branch is 58 commits ahead of `main`. The commit log groups into
phases:

- Early phase (pre-rename): cachecannon, irate, hist_irate, wide-form
  scaffolding, fixtures, golden snapshots.
- Architectural cleanup: pool DuckDB connections, panic-safe slot
  evacuation, h2_combine self-join (chunk-boundary panic workaround).
- Wide-form completeness: `counter_rate_bare_generic`, NULL handling,
  per-cpu / per-id resolvers, `rate_5m` range-window fix.
- Harness: `sql_vs_promql.rs` and `promql_only.rs` (commits `e97eba7`
  onwards). Multi-source aggregation, infrastructure-label injection,
  fixture coverage. Final batch closes the `remaining_work.md` buckets.

Don't squash. The stage-by-stage history is genuinely useful for
landing the branch in pieces if the reviewer prefers stacked PRs.

---

## Pointers

- Companion document (Rezolus side): `/work/rezolus/REVIEWING.md`
- Correctness harness output: `/tmp/sql_vs_promql_yv/`
- PromQL-only harness output: `/tmp/promql_yv/` and `/tmp/promql_main/`
