# Reviewing the `yv/sql-testing` branch (metriken side)

Companion doc: `/work/rezolus/REVIEWING.md`.

This branch ships **`metriken-query-sql`** — a standalone DuckDB
query backend (connection pool, H2 histogram UDFs, dashboard SQL
macros). It's a real engine the rezolus static viewer's macros
and UDFs are tested against (see
`/work/rezolus/crates/viewer-sql/tests/macros.rs`).

It also adds a PromQL→SQL **harness** behind a new
`harness` feature on `metriken-query`. The harness has no
production caller and is off by default — see "What the harness
is for" below.

The pre-existing PromQL evaluator (`metriken-query::promql` +
`tsdb`, gated `feature = "legacy"`) stays as the live engine for
the rezolus binary's server-backed viewer + live-agent ingest.

The evaluator now has two modes: a **streaming dispatcher**
(`promql/streaming/`) that walks expressions window-by-window
without materialising whole matrices — used for rate, irate,
avg_over_time, idelta, the aggregate matrix grouping body, and
both `histogram_quantile{,s}` — and a small **eager** residual
that handles cases streaming can't subsume: binary operators
with `group_left`/`group_right` and the scalar-passthrough
backstop. The dispatcher is always-on (no toggle) and runs at
every recursion level. Commit `a25e285` ("collapse PromQL
evaluator to streaming-only") removed ~2,300 lines of eager
pre-aggregation that streaming now covers, along with the
shadow-mode plumbing (no `Dispatch*` types, `with_dispatch`, or
observer interfaces survive; `grep -ri shadow` finds only stale
comments and the unrelated `orphan_detector` "shadowed entry"
concept). Other changes on this branch are ~270 LOC of small
edits across `promql/{mod.rs, streaming/*.rs, tests.rs}`.

Branch shape: **74** commits, **+14,858 / −397** across **58**
files (`git diff --shortstat origin/main...HEAD`,
`git rev-list --count origin/main..HEAD`). Includes a merge from
`origin/main` that brought `QueryEngine::columns()`, trimmed
parquet codecs to zstd-only, and bumped arrow/parquet/chrono.

---

## What the harness is for

`metriken-query/src/harness/translate.rs` opens with:

> **This whole module is migration scaffolding.** It exists to
> bridge Rezolus's still-PromQL dashboard emitter to the new SQL
> backend.

The harness module (`metriken-query::harness::*` — catalogue,
template, translate, interp, project, engine, plus
`queries.toml`) compiles a PromQL string into wide-form SQL,
runs it through `DuckDbBackend`, and projects the Arrow result
back into `QueryResult`. The only consumers are:

- `tests/engine_pipeline.rs` (5 end-to-end pipeline tests)
- `tests/translate_snapshots.rs` (per-entry SQL snapshots)
- `tests/orphan_detector.rs` (catalogue health)
- `examples/sql_vs_promql.rs` (the correctness harness — runs
  PromQL and SQL side-by-side against real Rezolus parquets and
  reports divergence)

The rezolus binary, the rezolus dashboard crate, and the rezolus
static viewer all bypass this layer. Two ways forward; this
branch defers the choice:

1. **Land a consumer.** Wire the rezolus server-backed viewer's
   `/api/v1/query_range` and MCP to `harness::Engine`. The
   harness becomes a regression suite for live code.
2. **Delete.** The `harness/` subdirectory + `queries.toml` is
   a clean delete if no consumer materializes.
   `metriken-query-sql` stays — the static viewer depends on it
   transitively via the parity test.

Either resolves cleanly; the harness's existence behind a
non-default feature flag keeps the question reversible.

---

## Crate layout

### `metriken-query-sql/src/` — DuckDB engine (2,380 LOC, live)

| File | LOC | Owns |
|---|---:|---|
| `udf.rs` | 1,123 | 9 H2 UDFs + `irate_lag` (10 `register_scalar_function`, 13 `unsafe` blocks). Preamble lines 1–37 document two duckdb-rs gotchas — `ListVector::child(N)` zeroes sibling LIST inputs past index 2048; `get_entry` returns uninitialized memory for NULL rows — and the uniform `child(0).as_slice_with_len(n)` fix pattern. Regression suite at `tests/lag_repro.rs` (1,021 LOC). |
| `backend.rs` | 414 | `DuckDbBackend` (line 87), connection pool with panic-safe slot evacuation, `run_sql` (:192), `describe_parquet` (:254). |
| `views.rs` | 405 | Parquet metric metadata + `_src` TEMP TABLE loader. |
| `macros.rs` | 224 | Loads `shared_macros.sql` via `include_str!`, splits on `;`, registers each `CREATE MACRO` on the connection. Re-exports the file as `SHARED_MACROS` so the wasm side consumes the same bytes. |
| `shared_macros.sql` | 145 | **19** canonical macros (3 rate/delta primitives + 7 histogram primitives + 9 dashboard helpers). Single source of truth for both native and wasm. |
| `observability.rs` | 157 | `BackendStats`. |
| `lib.rs` | 57 | Public surface. |

### `metriken-query/src/` — legacy PromQL + harness (4,447 LOC)

```
src/
├── lib.rs          (38 LOC)   Public surface; feature gates.
├── result.rs       (77)        QueryResult / Sample / MatrixSample / HistogramHeatmapResult — shared by both paths.
├── promql/         (legacy)    Streaming PromQL evaluator. Live.
├── tsdb/           (legacy)    In-memory parquet → Tsdb store. Live.
└── harness/        (harness)   PromQL → wide-form SQL (scaffolding).
    ├── mod.rs      (34)
    ├── engine.rs   (237)       The 5-step pipeline; doc-comment lines 1–18 is the spec.
    ├── translate.rs (2,402)    PromQL → SQL emitter. resolve_shape (~:1370) is the big switch — 65 entry ids resolve here.
    ├── template.rs (948)       Capture parser/matcher (29 inline tests).
    ├── project.rs  (395)       Arrow RecordBatch → QueryResult.
    ├── catalogue.rs (183)      TOML registry loader.
    ├── interp.rs   (133)       output_metric placeholder interpolation.
    └── (parent dir) queries.toml — 1,055 lines, 69 entries.
```

### Cargo features

```toml
default = ["legacy", "lz4"]
legacy  = ["dep:promql-parser"]
ingest  = ["legacy", "dep:metriken-exposition"]
harness = ["dep:metriken-query-sql"]
lz4     = ["parquet/lz4"]
```

`legacy` and `harness` are independent paths sharing only
`result.rs`. All four pass on this branch:

```bash
cargo build -p metriken-query                                   # default = legacy + lz4
cargo build -p metriken-query --no-default-features -F legacy   # legacy only
cargo build -p metriken-query --no-default-features -F ingest,lz4   # what the rezolus binary uses
cargo build -p metriken-query --all-features                    # everything (harness too)
```

### Consumers

| Consumer | Features | Uses |
|---|---|---|
| Rezolus binary | `ingest, lz4` → `legacy` transitively (`/work/rezolus/Cargo.toml:76`, workspace dep at `:22` is `default-features = false`) | `Tsdb`, `promql::QueryEngine` |
| Rezolus dashboard crate (`/work/rezolus/crates/dashboard/Cargo.toml:13`) | `legacy` | `Tsdb` only (`data.rs:12`, `lib.rs:8`) |
| Rezolus static viewer (`/work/rezolus/crates/viewer-sql/Cargo.toml`) | — | does not link `metriken-query` |

### Test infrastructure

- **`metriken-query-fixtures`** (new crate, 297 + 547 LOC): 13
  baked-in parquets covering counter / gauge / histogram /
  multi-source / reset / overflow / GPU / softirq / 500 ms
  sampling shapes. `src/lib.rs` exposes path lookups; the
  `generate` binary rebuilds them. Used by every integration
  test in `metriken-query` (and by `tests/lag_repro.rs` in
  `metriken-query-sql`).
- **`metriken-query-sql/CAUTION.md`** (176 LOC): the running
  catalog of known PromQL ↔ SQL semantic divergences. When a
  harness run produces a divergence, check this file first.

---

## Test coverage

**231** test items pass across the workspace (`cargo test
--workspace --all-features`), exercising the SQL
path at three layers:

| Layer | Where | Coverage |
|---|---|---|
| UDFs | `metriken-query-sql/src/udf.rs` (inline) + `tests/lag_repro.rs` | **27 + 12 = 39** tests. Per-UDF happy-path/boundary/NULL for every public UDF; LAG-on-LIST regression suite; the inline file includes 6 `irate_lag` cases (monotonic/sub-second/reset/NULL/dt=0/zero-rate) since every counter-rate routes through it. |
| Macros | `metriken-query-sql/src/macros.rs` (inline) | 11 tests — `irate_1s` ×2, `rate_5m` ×2, `cpu_busy_pct`, `ipc`, `ipns`, `hist_p99`, `bps_from_bytes`, `delta_1s`, registration smoke. |
| Harness translator | `metriken-query/tests/translate_snapshots.rs` | One combined `insta` snapshot covers every catalogue id (all 69) — `entry_id → SQL`. Resolver drift surfaces as a single intra-file diff via `cargo insta review`. |
| Harness catalogue health | `tests/orphan_detector.rs` | Strict: every entry produces non-empty SQL. `#[ignore]`'d informational: 10 entries shadowed by an earlier, more-general entry in `Catalogue::lookup` — a real audit item. |
| Harness pipeline | `tests/engine_pipeline.rs` | 5 end-to-end tests against `metriken-query-fixtures` parquets. Tests **the harness**, not a production code path. |
| Template parser | `harness/template.rs` (inline) | 29 tests covering every capture kind. |
| Catalogue loader | `harness/catalogue.rs` (inline) | 4 TOML-deserialization tests. |

Not directly tested: per-`OutputShape` variants in `project.rs`
(covered transitively); `backend.rs`'s panic-safe slot
evacuation (4 backend tests exercise the type-system contract,
not an induced panic).

---

## Verification: PromQL ↔ SQL correctness harness

`metriken-query/examples/sql_vs_promql.rs` (1,719 LOC) compares
legacy and SQL evaluators plot-by-plot against real Rezolus
parquets. Requires both features (`Cargo.toml:45-47`).

```bash
cargo run --release --example sql_vs_promql --features "legacy,harness" \
  -- /work/rezolus/site/viewer/data/demo.parquet
```

Output: `/tmp/sql_vs_promql_yv/`; `summary.json` is the
top-level result. **Stale by ~50 commits; rerun before opening
the PR.** `examples/promql_only.rs` (169 LOC) is the
cross-branch A/B for the PromQL evaluator alone against `main`.

---

## Where to spend attention

1. Read the **"What the harness is for"** section above and
   decide whether `harness::Engine` lands a consumer or the
   `harness/` subdirectory gets deleted. Everything else is a
   sub-decision.
2. **`metriken-query-sql/src/lib.rs`** (56) + **`backend.rs`**
   (414) — public surface and the only non-trivial concurrency
   story (panic-safe slot evacuation).
3. **`udf.rs`** preamble (lines 1–37) + one `h2_*` UDF impl.
   The unsafe pattern is uniform across all 13 blocks.
4. Run the harness and skim `summary.json`.
5. Skip-or-skim **`harness/translate.rs`** unless deciding for
   option (1) — 2,400 lines of switch table whose ultimate fate
   depends on the design question.

---

## Open questions

1. **Land or delete `harness::Engine`.** The only merge-blocker.
2. **Refresh harness numbers** before the PR opens.
