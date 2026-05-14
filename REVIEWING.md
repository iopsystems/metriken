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
It moved to streaming-only in commit `a25e285` ("collapse PromQL
evaluator to streaming-only"), losing ~2,300 lines of
pre-aggregation machinery.

Branch shape: **66** commits, **+16,210 / −396** across **59**
files (`git diff --shortstat main...yv/sql-testing`,
`git rev-list --count main..yv/sql-testing`).

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

### `metriken-query-sql/src/` — DuckDB engine (2,470 LOC, live)

| File | LOC | Owns |
|---|---:|---|
| `udf.rs` | 1,123 | 9 H2 UDFs + `irate_lag` (10 `register_scalar_function`, 13 `unsafe` blocks). Preamble lines 1–37 document two duckdb-rs gotchas — `ListVector::child(N)` zeroes sibling LIST inputs past index 2048; `get_entry` returns uninitialized memory for NULL rows — and the uniform `child(0).as_slice_with_len(n)` fix pattern. Regression suite at `tests/lag_repro.rs` (1,021 LOC). |
| `backend.rs` | 414 | `DuckDbBackend` (line 87), connection pool with panic-safe slot evacuation, `run_sql` (:192), `describe_parquet` (:254). |
| `views.rs` | 405 | Parquet metric metadata + `_src` TEMP TABLE loader. |
| `macros.rs` | 315 | **19** `CREATE OR REPLACE MACRO` strings registered via `register_all`. |
| `observability.rs` | 157 | `BackendStats`. |
| `lib.rs` | 56 | Public surface. |

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

---

## Test coverage

**205** `#[test]` items across the workspace, exercising the SQL
path at three layers:

| Layer | Where | Coverage |
|---|---|---|
| UDFs | `metriken-query-sql/src/udf.rs` (inline) + `tests/lag_repro.rs` | 21 + 12 + 6 = **39** tests. Per-UDF happy-path/boundary/NULL for every public UDF; LAG-on-LIST regression suite; `irate_lag` (every counter-rate routes through it) with 6 tests: monotonic/sub-second/reset/NULL/dt=0/zero-rate. |
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
2. **Macro library hazard.** 19 macros here as Rust strings; 30
   parallel macros in `/work/rezolus/crates/viewer-sql/src/macros.sql`,
   of which 19 are nominally the same. The parity test
   (`/work/rezolus/crates/viewer-sql/tests/macros.rs`) catches
   behavioural drift but not signature drift —
   `hist_irate_quantile` / `hist_rate5m_quantile` already differ
   (native `(buckets, q, ts, p)` vs wasm `(buckets, q, ts)`).
   A shared `.sql` file `include_str!`'d from both sides closes
   it; ~1 day.
3. **Refresh harness numbers** before the PR opens.
