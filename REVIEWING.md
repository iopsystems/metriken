# Reviewing the `yv/sql-testing` branch (metriken side)

This doc orients a reviewer cold. Every concrete claim below is tied to
a file:line in the working tree at commit `82f41f6`; counts come from
`wc -l` and `grep -c` over those files. Companion doc:
`/work/rezolus/REVIEWING.md`.

The previous version of this file made claims that turned out to be
stale; this rewrite cites each claim to a verifiable source. If a
number drifts, fix it here and update the citation.

---

## TL;DR

This branch adds a DuckDB-based query engine alongside the existing
PromQL evaluator. The PromQL evaluator stays in tree because both the
Rezolus binary's `rezolus view` server-side path and the dashboard
crate still link `metriken-query::{promql,tsdb}` (verified below).

The new code is split across two crates:

- **`metriken-query-sql`** (new, **2,368 LOC** of `src/*.rs` — `wc -l
  metriken-query-sql/src/*.rs`) — pure DuckDB engine, H2 histogram
  UDFs, SQL macros, and a connection pool.
- **`metriken-query`** (changed, **4,432 LOC** of `src/*.rs` —
  `wc -l metriken-query/src/*.rs`) — gains a `sql` Cargo feature that
  routes incoming PromQL strings through a catalogue → SQL translator
  → `metriken-query-sql`. The translator is migration scaffolding;
  its module preamble at `metriken-query/src/translate.rs:3` reads:
  > "**This whole module is migration scaffolding.** It exists to
  > bridge Rezolus's still-PromQL dashboard emitter to the new SQL
  > backend."

Branch diff vs `main` (after the review-prep cleanup commit): **+15,045 /
−397 across 59 files**. `git rev-list --count main..yv/sql-testing` →
**60** commits.

The correctness harness lives at
`metriken-query/examples/sql_vs_promql.rs` (1,719 LOC). Numbers in the
Verification section.

---

## Stale claims removed from the prior version of this doc

The previous TL;DR stated:

> "the Rezolus WASM viewer cannot run DuckDB in the browser"

This is **false** as of this branch. The Rezolus static viewer is a
WASM crate at `rezolus/crates/viewer-sql/` that runs queries through
`@duckdb/duckdb-wasm@1.33.1-dev45.0`, imported at
`rezolus/site/viewer-sql/lib/script.js:12`:

```js
import * as duckdb from 'https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm@1.33.1-dev45.0/+esm';
```

The viewer-sql crate's `Cargo.toml` (lines 21-29) has **no
`metriken-query` dependency** at all — only `dashboard`, `wasm-bindgen`,
`serde`, etc. So the `legacy` feature gate is *not* there "for the WASM
viewer"; the WASM viewer doesn't link metriken-query.

Related stale claims also corrected in this rewrite:

- `metriken-query/src/lib.rs:5-13` (module docstring) says `legacy` is
  "enabled by the Rezolus WASM viewer". **Stale** — fix should land
  here.
- `metriken-query/Cargo.toml` comment above `[features]` says
  "The Rezolus WASM viewer crate must opt out (`default-features =
  false`) and opt into `legacy` instead". **Stale** — same fix.
- The prior REVIEWING feature-gate matrix row "Rezolus WASM viewer |
  legacy only | DuckDB doesn't compile to wasm32". **Stale** — the
  WASM viewer is duckdb-wasm-backed; see the rezolus REVIEWING.md
  "Why the JS / Rust split" section for the architecture.

The actual current consumers of `metriken-query` are listed below.

---

## Who actually links metriken-query (verified)

| Consumer | Features enabled | Source |
|---|---|---|
| Rezolus binary (`rezolus`) | `ingest`, `lz4` → transitively `legacy` | `rezolus/Cargo.toml:78`: `metriken-query = { workspace = true, features = ["ingest", "lz4"] }`. Workspace dep at `rezolus/Cargo.toml:22` is `default-features = false`, so `sql` is *not* enabled here. |
| Rezolus dashboard crate | `legacy` | `rezolus/crates/dashboard/Cargo.toml:13`: `metriken-query = { workspace = true, features = ["legacy"] }`. Only uses `metriken_query::Tsdb` (`crates/dashboard/src/data.rs:12`, `crates/dashboard/src/lib.rs:8`). |
| Rezolus viewer-sql crate (WASM viewer) | — | `rezolus/crates/viewer-sql/Cargo.toml` has no `metriken-query` dep. |

The metriken workspace cargo features (`metriken-query/Cargo.toml:5-10`):

```toml
default = ["sql", "lz4"]
sql = ["dep:metriken-query-sql"]
legacy = ["dep:promql-parser"]
ingest = ["legacy", "dep:metriken-exposition"]
lz4 = ["parquet/lz4"]
```

Module gating at `metriken-query/src/lib.rs:20-36`:

- `catalogue`, `engine`, `interp`, `project`, `template`, `translate`
  → `#[cfg(feature = "sql")]`
- `promql`, `tsdb` → `#[cfg(feature = "legacy")]`
- `result` is always present (shared `QueryResult` type)

So today, the *legacy* feature exists because **both the rezolus binary
and the dashboard crate need `metriken_query::Tsdb` / `promql::*`**, not
because of any WASM viewer.

---

## Architecture (current code)

### Two engines under one crate namespace

```
metriken-query  (re-exports from src/lib.rs)
  ├─ #[cfg(feature = "sql")]
  │   ├─ Engine               metriken-query/src/engine.rs:108
  │   ├─ Engine::new(path)    metriken-query/src/engine.rs:119
  │   ├─ Engine::query_range  metriken-query/src/engine.rs:190
  │   ├─ Catalogue,           metriken-query/src/catalogue.rs:94
  │   │   Catalogue::embedded()   :120
  │   │   CatalogueEntry, OutputShape, ...
  │   ├─ translate::try_generate  metriken-query/src/translate.rs (top)
  │   ├─ project::run         metriken-query/src/project.rs
  │   └─ delegates execution to metriken-query-sql::DuckDbBackend
  │
  └─ #[cfg(feature = "legacy")]
      ├─ QueryEngine          metriken-query/src/promql/mod.rs:29
      ├─ QueryEngine::new(tsdb)
      ├─ QueryEngine::query_range  metriken-query/src/promql/mod.rs:351
      └─ Tsdb                 metriken-query/src/tsdb/mod.rs
```

Both engines return the same `QueryResult` type defined in
`metriken-query/src/result.rs:1`. That's the cross-cut shared between
sql and legacy paths.

### SQL pipeline (the new path)

The 5-step pipeline documented at `metriken-query/src/engine.rs:1-18`:

> 1. `Catalogue::lookup` — match the query against a registered
>    template, extract captures.
> 2. `metriken-query-sql::DuckDbBackend::describe_parquet` — get the
>    parquet's metric metadata (cached after first request).
> 3. `crate::translate::try_generate` — emit the wide-form SQL.
> 4. `metriken-query-sql::DuckDbBackend::run_sql` — execute it.
> 5. `crate::project::run` — turn Arrow batches back into
>    `QueryResult`.

`DuckDbBackend` types:

- `pub struct DuckDbBackend` at `metriken-query-sql/src/backend.rs:87`
- `pub fn run_sql(...)` at `metriken-query-sql/src/backend.rs:192`
- `pub fn describe_parquet(...)` at `metriken-query-sql/src/backend.rs:254`

---

## File-by-file inventory (with LOC)

### `metriken-query-sql/src/` — 2,368 LOC total

| File | LOC | What it owns |
|---|---:|---|
| `backend.rs` | 414 | `DuckDbBackend` struct, connection pool, `run_sql` / `describe_parquet` |
| `udf.rs` | 1,022 | 9 H2 histogram UDFs + 13 `unsafe` FFI blocks (counts below) |
| `views.rs` | 405 | Parquet introspection + `_src` TEMP TABLE loader |
| `macros.rs` | 314 | 19 SQL macros (count below) |
| `observability.rs` | 157 | `BackendStats` |
| `lib.rs` | 56 | Public surface |

Counts that need explicit verification:

- **9 H2 UDFs** — `grep '^\s*conn.register_scalar_function::<H2' metriken-query-sql/src/udf.rs` lists: `H2LowerUdf`, `H2UpperUdf`, `H2MidUdf`, `H2TotalUdf`, `H2DeltaUdf`, `H2QuantileUdf`, `H2QuantilesUdf`, `H2CountInRangeUdf`, `H2CombineUdf`.
- **13 unsafe blocks** in `udf.rs` — `grep -c '^\s*unsafe ' metriken-query-sql/src/udf.rs` → 13.
- **19 SQL macros** in `macros.rs` — `grep -c 'CREATE OR REPLACE MACRO' metriken-query-sql/src/macros.rs` → 19.
  Note: the wasm-side `crates/viewer-sql/src/macros.sql` contains 30
  `CREATE MACRO` statements (verified separately in the rezolus
  REVIEWING.md). The two files have different macro inventories;
  treat them as parallel copies, not byte-identical.

`udf.rs` preamble (`metriken-query-sql/src/udf.rs:1-37`) documents two
real DuckDB-rs gotchas inline: the `ListVector::child(capacity)` reserve
that zeros sibling LAG'd LIST inputs past index 2048, and
`ListVector::get_entry` returning uninitialized memory for NULL parent
rows. The fix pattern (`child(0).as_slice_with_len(n)`) is followed
uniformly across the 13 unsafe blocks.

The regression test for the LAG-on-LIST bug is
`metriken-query-sql/tests/lag_repro.rs` (1,021 LOC). Preamble at line 1:

> "Diagnostic test for the LAG-over-LIST UDF bug. Probes (a)
> chunk-boundary effects (>2048 rows) and (b) actual h2_delta+h2_total
> pipeline using metriken-query-sql's real UDFs."

### `metriken-query/src/` — 4,432 LOC total

| File | LOC | Feature | What it owns |
|---|---:|---|---|
| `translate.rs` | 2,403 | `sql` | PromQL→SQL emitter; preamble names it "migration scaffolding" |
| `template.rs` | 948 | `sql` | Capture parser/matcher |
| `project.rs` | 394 | `sql` | Arrow `RecordBatch` → `QueryResult` |
| `engine.rs` | 237 | `sql` | `Engine` + 5-step pipeline |
| `catalogue.rs` | 184 | `sql` | TOML registry loader |
| `interp.rs` | 133 | `sql` | `output_metric` placeholder interpolation |
| `lib.rs` | 56 | always | Re-exports + module gating |
| `result.rs` | 77 | always | `QueryResult`, `Sample`, `MatrixSample`, `HistogramHeatmapResult`, `QueryError` |
| `promql/` (subdir) | — | `legacy` | `QueryEngine`, streaming evaluator |
| `tsdb/` (subdir) | — | `legacy` | In-memory store |

`queries.toml` is **1,055 lines, 69 entries** —
`grep -cE '^\s*id\s*=' metriken-query/queries.toml` → 69.

`translate.rs` preamble (`metriken-query/src/translate.rs:1-50`)
documents the organisation:

> "1. `try_generate` (top of file) — top-level dispatch on entry id.
>  2. `Shape` struct and `PerColExpr` / `Aggregation` enums — the
>     canonical "what to compute" representation. ...
>  3. `resolve_shape` (~1370) and `resolve_binary` (~140) — the big
>     switch tables. **65 entry ids** match here today."

The "65 entry ids" wording supersedes prior claims of "40+ shapes".

---

## Known current build issue (rezolus binary)

`cargo check --bin rezolus` from the rezolus repo currently fails at
compile time before BPF compilation (we couldn't get past the BPF
build on this dev box without `clang`, but the static analysis is
clear). The reason is **type references that no longer exist in
metriken-query**:

`rezolus/src/viewer/mod.rs` references the following:

| Reference site | Type |
|---|---|
| `:1650` | `promql::DispatchConfig` |
| `:1663` | `promql::DispatchConfig` (struct literal) |
| `:1676` | `metriken_query::DispatchObserver` |
| `:1677` | `metriken_query::CatalogueEntry`, `metriken_query::Diff` |
| `:1695` | `metriken_query::Mode` |

`grep -rn 'DispatchConfig\|DispatchObserver' /work/metriken/ --include='*.rs'`
returns **no results** — the types are gone.
`pub enum Mode` is not defined in metriken at all
(`grep -rn 'pub enum Mode' /work/metriken/`).

The most likely removal is commit `a25e285` —
"collapse PromQL evaluator to streaming-only (rezolus subset; ~2300
line shrink) (#94)" landed 2026-05-01 from
`yao@iop.systems`. That commit's title matches the shape of the
deletion.

The previous rezolus REVIEWING.md FAQ explained the build failure as
"those types still exist in metriken-query, but they're feature-gated
behind `sql`". That's **wrong**: `CatalogueEntry` is feature-gated;
the rest (`DispatchConfig`, `DispatchObserver`, `Diff`, `Mode`) are
gone entirely.

Fixing the rezolus binary build is either:

1. Remove the `dispatch_for_capture` + `LoggingDispatchObserver` code
   path at `src/viewer/mod.rs:1640-1700` and the calls at `:1727-1731`
   and `:1756-1760`. The server-backed viewer then runs PromQL only,
   which matches the current `BACKEND='promql'` constant at
   `src/viewer/assets/lib/viewer_api.js:7`.
2. Or restore the dispatch types on the metriken side if shadow-mode
   evaluation is still wanted.

This is out of scope for the SQL-migration branch itself but blocks
landing — flag it in the PR description.

---

## Where to spend attention (in 1 hour)

1. **`metriken-query-sql/src/lib.rs`** (56 LOC) + **`backend.rs`** (414).
   Public API and connection pool. The pool's panic-safe slot evacuation
   is the only non-trivial concurrency story.
2. **`metriken-query-sql/src/udf.rs`** (1,022) — read the preamble
   (lines 1-37) and one `h2_*` UDF impl. The gotcha pattern is
   documented; the unsafe blocks are uniform.
3. **`metriken-query/src/engine.rs`** (237) — entire file. The 5-step
   pipeline doc-comment at the top is the spec.
4. Pick 2-3 representative entries from `metriken-query/queries.toml`
   and trace them through `translate.rs`. Suggested: `gauge_bare`,
   `counter_irate_total`, `histogram_quantile_*`.
5. Run the harness (see Verification).

If you have ten minutes: the FAQ below.

---

## FAQ

**Q: Why two crates?**

A: Disjoint dependency sets. `metriken-query-sql` pulls in `duckdb-rs`
and the bundled DuckDB C build; `metriken-query` with `legacy` only
pulls in `promql-parser`. Splitting them out keeps the legacy build
small and lets `metriken-query-sql` survive as the long-term engine
when the catalogue/translate scaffolding gets deleted.

**Q: Why is `translate.rs` 2,403 lines?**

A: Every PromQL shape Rezolus's dashboard emits today needs a matching
SQL translator. The preamble at `metriken-query/src/translate.rs:31`
says **65 entry ids** resolve in `resolve_shape`. The file is a switch
table, not a hard-to-follow algorithm. It is scaffolding — when
Rezolus emits SQL natively, it goes away (the preamble at line 3-7
says so explicitly).

**Q: Why is `udf.rs` so much `unsafe`?**

A: DuckDB-rs's vscalar UDF API is FFI-shaped — output vectors come
back as raw pointers you write into. The 13 unsafe blocks are all
bounds-checked, all follow the documented pattern (preamble lines
1-37), and all are exercised by `tests/lag_repro.rs`. The
`ListVector::child(N)` reserve gotcha is real and was the original
motivation for the regression suite.

**Q: Why retain `legacy`?**

A: Two real consumers link it today:

- The rezolus dashboard crate (`rezolus/crates/dashboard/Cargo.toml:13`
  enables `features = ["legacy"]`) uses `metriken_query::Tsdb`.
- The rezolus binary (`rezolus/Cargo.toml:78` enables `features =
  ["ingest", "lz4"]`; `ingest` brings `legacy` transitively per
  `metriken-query/Cargo.toml:8`) uses `metriken_query::promql::*` and
  `metriken_query::Tsdb` for the live-agent ingest path and the
  server-backed `/api/v1/query_range` endpoint.

The WASM viewer (the static-site browser viewer at
`rezolus/crates/viewer-sql/`) does **not** link `metriken-query`. The
prior doc's reasoning that "WASM viewer needs legacy because DuckDB
doesn't compile to wasm32" no longer matches the code — the static
viewer is duckdb-wasm-backed.

**Q: Why does the harness need `--features "legacy,sql"`?**

A: Because it instantiates **both** engines side-by-side. See
`metriken-query/Cargo.toml:14-16`:

```toml
[[example]]
name = "sql_vs_promql"
required-features = ["legacy", "sql"]
```

---

## Verification

### Cargo builds

The Cargo features at `metriken-query/Cargo.toml:5-10` (quoted above)
admit four sensible configurations. Reviewer can run:

```
cargo build -p metriken-query                                                # default = sql+lz4
cargo build -p metriken-query --no-default-features --features "legacy"      # legacy only
cargo build -p metriken-query --no-default-features --features "ingest,lz4"  # binary's config
cargo build -p metriken-query --all-features                                 # everything
```

Workspace builds: `cargo build --workspace` and `cargo test --workspace
--all-features`.

### PromQL ↔ SQL correctness harness

Located at `metriken-query/examples/sql_vs_promql.rs` (1,719 LOC).
Required features: `legacy,sql` (from `Cargo.toml:14-16` above).

Run:

```
cargo run --release --example sql_vs_promql \
    --features "legacy,sql" \
    -- /work/rezolus/site/viewer/data/demo.parquet
```

Output lands in `/tmp/sql_vs_promql_yv/` per the example source.

**Reviewer note: the numerical results in the prior version of this
doc are not re-verified here.** The prior numbers (~89% identical on
single-source parquets; 849/3/1370 across 11 parquets) were taken from
`/tmp/sql_vs_promql_yv/summary.json` at commit `c9b105b`. The branch
has advanced 4 more commits since then (HEAD is `82f41f6`); a re-run
is needed to refresh the table before the PR is opened. See
`/work/rezolus/REVIEWING.md` "Known divergence taxonomy" section for
the cumulative behaviour story.

### Cross-branch sanity check

`metriken-query/examples/promql_only.rs` (169 LOC) runs the PromQL
evaluator alone and can be invoked on both `main` and `yv/sql-testing`
for direct A/B. Per its top doc-comment:

> "PromQL-only harness for cross-branch correctness validation."

---

## Branch layout

`git rev-list --count main..yv/sql-testing` → **60** commits. Commit
log (`git log --oneline main..yv/sql-testing`) groups roughly into:

- Wide-form scaffolding (cachecannon, irate, hist_irate, fixtures,
  golden snapshots).
- Architectural simplification: PromQL evaluator collapsed to
  streaming-only — commit `a25e285` "collapse PromQL evaluator to
  streaming-only (rezolus subset; ~2300 line shrink) (#94)".
- DuckDB connection pool: panic-safe slot evacuation, h2_combine
  self-join (chunk-boundary panic workaround).
- Wide-form completeness: `counter_rate_bare_generic`, NULL handling,
  per-cpu / per-id resolvers, `rate_5m` range-window fix.
- Harness: `sql_vs_promql.rs`, `promql_only.rs`. Multi-source
  aggregation. Fixture coverage.
- Review prep (latest): bench CSV deletion, planning docs deletion,
  REVIEWING.md, `.gitattributes` — commit `82f41f6`.

`a25e285` is the commit that broke the rezolus binary build by
removing `DispatchConfig`/`DispatchObserver`/`Diff`/`Mode` — see the
"Known current build issue" section.

Don't squash. The stage-by-stage history is reading-order for a
commit-by-commit review.

---

## Pointers

- Companion doc (rezolus side): `/work/rezolus/REVIEWING.md`
- Correctness harness: `metriken-query/examples/sql_vs_promql.rs`
- LAG-on-LIST regression suite: `metriken-query-sql/tests/lag_repro.rs`
- Macros (native): `metriken-query-sql/src/macros.rs` (19 macros)
- Macros (wasm parallel copy): `rezolus/crates/viewer-sql/src/macros.sql` (30 macros)
- Parity test for the macros: `rezolus/crates/viewer-sql/tests/macros.rs`
