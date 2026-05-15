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
`tsdb`, gated `feature = "legacy"`) stays in the binary as the
live engine for the rezolus MCP module (`src/mcp/`), the
`crates/report-save` rendering path, the dashboard crate (which
re-exports `Tsdb` as a schema source), and one viewer side-effect
— `validate_service_extensions` in `src/viewer/metadata.rs`,
which runs each KPI's PromQL on the live-agent `Tsdb` to hide
empty plots. The server-backed viewer's `/api/v1/query{,_range}`
handlers flipped to SQL/DuckDB in `f9b392b` (Stage 3); the
live-agent ingest loop still populates a `Tsdb`, but those
handlers return `capture_not_found` for live mode pending stages
3-9.

The evaluator now has two modes: a **streaming dispatcher**
(`promql/streaming/`) that walks expressions window-by-window
without materialising whole matrices — used for rate, irate,
avg_over_time, idelta, the aggregate matrix grouping body, and
both `histogram_quantile{,s}` — and a small **eager** residual
that handles cases streaming can't subsume: binary operators
with `group_left`/`group_right` and the scalar-passthrough
backstop. The dispatcher is always-on (no toggle) and runs at
every recursion level. Commit `a25e285` ("collapse PromQL
evaluator to streaming-only") deleted ~2,700 lines of eager
pre-aggregation that streaming now covers, along with the
shadow-mode plumbing (no `Dispatch*` types, `with_dispatch`, or
observer interfaces survive; `grep -ri shadow` finds only stale
comments and the unrelated `orphan_detector` "shadowed entry"
concept). Post-collapse, those paths have seen +744 / −375 LOC
across 10 follow-up commits (`git log --shortstat a25e285..HEAD --
metriken-query/src/promql/{mod.rs,streaming,tests.rs}`). The +270
spike is `1ead064` (`QueryEngine::columns()`, merged in from
origin/main); per-commit churn elsewhere ranges from a few lines
to ~135.

Branch shape: **79** commits, **+16,095 / −397** across **59**
files (`git diff --shortstat origin/main...HEAD`,
`git rev-list --count origin/main..HEAD`). Includes a merge from
`origin/main` that brought `QueryEngine::columns()`, trimmed
parquet codecs to zstd-only, and bumped arrow/parquet/chrono.
The most recent code-touching commit (`17895d6` multi-source
`_src` + `_cgroup_index` + `h2_combine_lol`) ships the test
coverage and engine extensions described under "Crate layout"
and "Test coverage" below. Two follow-up doc refreshes
(`5bdef1f`, `fe3414d`) updated this file.

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

### `metriken-query-sql/src/` — DuckDB engine (~3,620 LOC, live)

| File | LOC | Owns |
|---|---:|---|
| `udf.rs` | 1,187 | 9 H2 UDFs + `irate_lag` (10 `register_scalar_function`, 13 `unsafe` items — 5 `unsafe {}` blocks + 8 `unsafe fn invoke` declarations). Preamble lines 1–37 document two duckdb-rs gotchas — `ListVector::child(N)` zeroes sibling LIST inputs past index 2048; `get_entry` returns uninitialized memory for NULL rows — and the uniform `child(0).as_slice_with_len(n)` fix pattern. Regression suite at `tests/lag_repro.rs` (1,021 LOC). |
| `backend.rs` | 544 | `DuckDbBackend` connection pool (struct at :98), `run_sql` (:216), `describe_parquet` (:297), `invalidate` (:327). `catch_unwind` boundary (:258) is annotated with the load-bearing ordering invariant; recovery-shape test pins it. `ConnState` carries precomputed `src_sql` + `cgroup_index_sql` (both `Arc<str>`) so panic-rebuild paths re-execute the same setup without re-reading the parquet schema. |
| `views.rs` | 1,122 | Parquet metric metadata + `_src` and `_cgroup_index` TEMP TABLE builders. `read_introspection` (line 120) is shared by pool init + `describe_parquet`; tolerates missing `sampling_interval_ms` (1 s fallback), `metric_type=histogram` without `grouping_power` (column dropped), and duplicate physical names (first wins). `render_src_sql` handles both single-source (`* EXCLUDE` shortcut) and multi-source captures — for the latter, `canonical_alias` mirrors the wasm viewer's `canonicalAlias` to project rezolus-tagged prefixed columns under canonical dashboard names. `render_cgroup_index_sql` + `create_cgroup_index` build the cgroup-index table the cgroup dashboard SQL JOINs against. |
| `macros.rs` | 389 | Loads `shared_macros.sql` via `include_str!`, parses it with a quote-aware splitter (semicolons inside `'..'` / `"..."` literals stay in the statement), and registers each `CREATE MACRO` on the connection. Re-exports the file as `SHARED_MACROS` so the wasm side consumes the same bytes. |
| `shared_macros.sql` | 167 | **20** canonical macros (3 rate/delta primitives + 7 histogram primitives + 9 dashboard helpers + the new `h2_combine_lol` shared cross-backend list-of-lists folder). Single source of truth for both native and wasm. |
| `observability.rs` | 157 | `BackendStats`. |
| `lib.rs` | 57 | Public surface. |

### `metriken-query/src/` — legacy PromQL + harness (11,026 LOC total: promql 4,716 + tsdb 1,863 + harness 4,332 + lib/result 115)

```
src/
├── lib.rs          (38 LOC)   Public surface; feature gates.
├── result.rs       (77)        QueryResult / Sample / MatrixSample / HistogramHeatmapResult — shared by both paths.
├── promql/         (legacy)    Streaming PromQL evaluator. Live.
├── tsdb/           (legacy)    In-memory parquet → Tsdb store. Live.
└── harness/        (harness)   PromQL → wide-form SQL (scaffolding).
    ├── mod.rs      (34)
    ├── engine.rs   (237)       The 5-step pipeline; doc-comment lines 1–18 is the spec.
    ├── translate.rs (2,402)    PromQL → SQL emitter. `try_generate` (`:112`) dispatches all 69 entry IDs across 6 resolvers: `resolve_shape` (`:1400`, 43), `resolve_binary` (`:175`, 11), `try_chain` (`:1005`, 5), `try_histogram` (`:702`, 5), `try_pair_match` (`:874`, 4), `try_avg_over_time` (`:678`, 1).
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
| Rezolus binary | `ingest` → `legacy` transitively (`/work/rezolus/Cargo.toml:77`, workspace dep at `:19` is `default-features = false`) | `Tsdb`, `promql::QueryEngine` |
| Rezolus dashboard crate (gate at `/work/rezolus/crates/dashboard/Cargo.toml:16`, dep at `:22`) | `legacy` | `Tsdb` only (`data.rs:13`, `lib.rs:11`) |
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

**258** test items pass across the workspace (`cargo test
--workspace --all-features`), exercising the SQL path at every
layer that has a public surface:

| Layer | Where | Coverage |
|---|---|---|
| UDFs | `metriken-query-sql/src/udf.rs` (inline) + `tests/lag_repro.rs` | **32 + 12 = 44** tests. Per-UDF happy-path/boundary/NULL; LAG-on-LIST regression suite; 6 `irate_lag` cases (monotonic / sub-second / reset / NULL / dt=0 / zero-rate). Recent additions cover `h2_quantiles` with an empty `qs` list, `h2_quantiles` on an all-zero histogram, `h2_quantile` on a single-element histogram, `h2_total` on a NULL list, and `h2_combine` with a NULL argument. |
| Macros | `metriken-query-sql/src/macros.rs` (inline) | **16** tests — the 11 macro semantics tests (`irate_1s` ×2, `rate_5m` ×2, `cpu_busy_pct`, `ipc`, `ipns`, `hist_p99`, `bps_from_bytes`, `delta_1s`, registration smoke), 3 covering the quote-aware splitter (semicolons inside `'..'` and `"..."`; `--` inside literals; real `--` comments), and 2 cross-backend parity tests for the new `h2_combine_lol` shared macro (matches variadic UDF on two-list input; empty outer list returns empty result). |
| Pool / panic safety | `metriken-query-sql/src/backend.rs` (inline) + `tests/pool_invalidate.rs` | **5 + 4 = 9** tests. Backend tests: `run_sql` returning Arrow, bad-SQL surfaces as `Err`, `describe_parquet` warm vs. cold path, plus `empty_slot_is_lazily_rebuilt_without_mutex_poisoning` — pins the post-`catch_unwind` recovery shape (slot=`None` → rebuild on next checkout, peer slots untouched). Pool-invalidate tests: unknown source, warm eviction, cold-re-init after invalidate, plus a 4-worker concurrent stress test that runs queries while a peer thread spams `invalidate` (regression for the `Arc<ConnState>` keeps-alive contract on line 281). |
| Parquet metadata | `metriken-query-sql/src/views.rs` (inline) | **18** tests. The 6 existing snap / catalog / describe tests, 3 malformed-metadata pins (missing `sampling_interval_ms` → 1 s fallback; `metric_type=histogram` without `grouping_power` → column dropped; duplicate physical names → first wins), 3 `_cgroup_index` builder tests (empty parquet → empty index, cgroup column with split name/id/labels MAP, apostrophe-bearing cgroup names handled), and 6 `canonical_alias` / `render_src_sql` tests for the multi-source projection path (named-column pass-through, numeric-encoded rebuild, non-numeric-before-numeric label sort, histogram `:buckets` suffix, single-source `* EXCLUDE` shortcut, multi-source canonical projection drops non-rezolus columns). |
| Harness translator | `metriken-query/tests/translate_snapshots.rs` | One combined `insta` snapshot covers every catalogue id (all 69) — `entry_id → SQL`. Resolver drift surfaces as a single intra-file diff via `cargo insta review`. |
| Harness catalogue health | `tests/orphan_detector.rs` | Strict: every entry produces non-empty SQL. `#[ignore]`'d informational: 10 entries shadowed by an earlier, more-general entry in `Catalogue::lookup` — a real audit item. |
| Harness pipeline | `tests/engine_pipeline.rs` | 5 end-to-end tests against `metriken-query-fixtures` parquets. Tests **the harness**, not a production code path. |
| Template parser | `harness/template.rs` (inline) | 29 tests covering every capture kind. |
| Catalogue loader | `harness/catalogue.rs` (inline) | 4 TOML-deserialization tests. |

The panic-safety story is now pinned at the recovery shape
rather than by inducing a real panic: UDF-callback panics are
non-unwinding (the `h2_combine` LIST<LIST> note in `udf.rs` is
the canonical example) and abort the process before
`catch_unwind` can see them, so reproducing one in a test would
require a Rust-side bug in duckdb-rs we don't want to encode.
The recovery test instead stubs the post-panic state (`slot =
None`) and asserts the next `run_sql` rebuilds it without
touching peer slots. `project.rs` `OutputShape` variants stay
covered only transitively.

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
2. **`metriken-query-sql/src/lib.rs`** (57) + **`backend.rs`**
   (544) — public surface and the only non-trivial concurrency
   story (panic-safe slot evacuation; see the `catch_unwind`
   block at :258 for the ordering invariant).
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
