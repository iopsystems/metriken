# Metriken — Architecture

Metriken is two systems separated by a Parquet file in the middle. On the left, an application produces metrics; on the right, a dashboard reads them back and asks questions.

```
                    ┌────────────────────────────────────────────────────────┐
                    │                  WRITE SIDE  (app process)             │
                    └────────────────────────────────────────────────────────┘

   Application source                                        At runtime, in RAM
   ──────────────────                                        ──────────────────
   #[metric]                       metriken-derive           ┌──────────────────┐
   static FOO: Counter   ────────► expands to register   ──► │  linkme[METRICS] │
       = Counter::new();           the static into a         │  global slice    │
                                   distributed slice         │  (one entry per  │
                                                             │   #[metric])     │
                                                             └─────────┬────────┘
                                                                       │
                                                                       │ metrics()
                                                                       ▼
   FOO.increment();   ◄─── atomic op, no indirection           ┌──────────────────┐
                                                               │ dyn registry     │ ◄── runtime register/
                                                               │ (RwLock<BTree>)  │     unregister
                                                               └─────────┬────────┘
                                                                         │
                                                          ┌──────────────┘
                                                          ▼
                                              ┌────────────────────┐
                                              │   Snapshotter      │ metriken-exposition
                                              │  (walks registry,  │
                                              │   emits Snapshot   │
                                              │   {counters,       │
                                              │    gauges,         │
                                              │    histograms})    │
                                              └─────────┬──────────┘
                                                        │
                                  ┌─────────────────────┼──────────────────────┐
                                  ▼                     ▼                      ▼
                          Prometheus text       JSON / MsgPack          ┌─────────────┐
                          (scrape endpoint)     (serde)                 │  Parquet    │
                                                                        │ (wide form: │
                                                                        │  1 column / │
                                                                        │  labeled    │
                                                                        │  series)    │
                                                                        └──────┬──────┘
                                                                               │
                    ──────────────────────────────────────────────────────────────────────
                                                                               │
                    ┌──────────────────────────────────────────────────────────▼─────────┐
                    │                   READ SIDE  (query / dashboard)                   │
                    └─────────────────────────────────────────────────────────────────────┘

                                                  Parquet file
                                                       │
                          ┌────────────────────────────┴────────────────────────────┐
                          │                                                         │
                          ▼                                                         ▼
              ┌────────────────────────┐                       ┌───────────────────────────────┐
              │   LEGACY PATH          │                       │   SQL HARNESS PATH            │
              │   (default; live)      │                       │   (feature = "harness";       │
              │                        │                       │    migration scaffolding,     │
              │ Tsdb::load(path)       │                       │    no prod callers today)     │
              │  ├─ CounterCollection  │                       │                               │
              │  ├─ GaugeCollection    │                       │ Engine::new(path)             │
              │  └─ HistogramCollection│                       │  │                            │
              │     (per-series        │                       │  │  query_range(promql,…)     │
              │      Arrow-backed      │                       │  ▼                            │
              │      time series)      │                       │ Catalogue::lookup ──► picks   │
              │        │               │                       │   one of ~69 known PromQL     │
              │        │               │                       │   shapes, extracts captures   │
              │        ▼               │                       │  │                            │
              │ QueryEngine            │                       │  ▼                            │
              │ (promql/streaming/*    │                       │ translate::try_generate       │
              │  iterator pipelines:   │                       │   ── emits wide-form SQL ──►  │
              │  irate, rate, deriv,   │                       │                               │
              │  sum-by, histogram     │                       │  ┌─────────────────────────┐  │
              │  quantile, …)          │                       │  │  metriken-query-sql     │  │
              │        │               │                       │  │  DuckDbBackend          │  │
              │        ▼               │                       │  │   ├─ in-mem conn pool   │  │
              │   QueryResult          │                       │  │   ├─ _src TEMP TABLE    │  │
              │   {Vector|Matrix|      │                       │  │   ├─ H2 histogram UDFs  │  │
              │    Scalar|Heatmap}     │                       │  │   │  (h2_lower/upper/   │  │
              │                        │                       │  │   │   quantile/delta…)  │  │
              │                        │                       │  │   └─ shared_macros.sql  │  │
              │                        │                       │  │      (irate_1s, rate_5m,│  │
              │                        │                       │  │       hist_p99, …)      │  │
              │                        │                       │  └────────────┬────────────┘  │
              │                        │                       │               │ Arrow batches │
              │                        │                       │               ▼               │
              │                        │                       │     harness::project::run     │
              │                        │                       │     (positional → Matrix or   │
              │                        │                       │      HistogramHeatmap)        │
              │                        │                       │               │               │
              │                        │                       │               ▼               │
              │                        │                       │           QueryResult         │
              └────────────────────────┘                       └───────────────────────────────┘
```

## Crate map

```
metriken-core      ── traits + linkme slice + dyn registry + metadata.
                     Deliberately small and stable so multiple major
                     versions of metriken can link in the same binary
                     without two slices fighting over one symbol.
                     ("links = metriken-core" in Cargo.toml turns a
                     duplicate into a link error, not a runtime panic.)

metriken-derive    ── #[metric] proc-macro. Lowers to a
                     declare_metric_v1! invocation that drops a
                     #[linkme::distributed_slice] static into METRICS.

metriken           ── user-facing API. Counter, Gauge, AtomicHistogram,
                     RwLockHistogram, CounterGroup, ShardedCounterGroup,
                     Lazy<T>. Re-exports the macro and the registry
                     accessors from -core.

metriken-exposition── reads the registry; produces Snapshot{V1,V2}.
                     Output drivers (feature-gated):
                       prometheus  – text format
                       json/msgpack – serde
                       parquet     – wide-form, one column per series,
                                     label k/v pairs in field metadata.
                     MsgpackToParquet converts streamed snapshots into
                     a single Parquet file after the fact.

metriken-query     ── consumer side. Two backends behind one
                     QueryResult type:
                       tsdb/ + promql/   – the live PromQL evaluator.
                       harness/          – PromQL→SQL translator that
                                            delegates execution to…

metriken-query-sql ── …DuckDbBackend. Owns a per-data-source pool of
                     in-memory DuckDB connections, registers the H2
                     histogram UDFs and the shared SQL macro library,
                     and builds a MetricCatalog from the parquet's
                     Arrow field metadata so wide-form SQL can select
                     by canonical metric name + labels.

metriken-query-
   -fixtures       ── series-shaped builder on top of
                     metriken-exposition for deterministic Parquet
                     test fixtures.
```

## The key ideas, in plain English

**Producer side.** A `#[metric] static FOO: Counter` is just a static. Mutating it is one atomic op — no map lookup, no lock, no allocation. The "registry" is built by the linker: `linkme` collects every `#[metric]` static across the whole binary into one contiguous array (`METRICS`) at link time. Calling `metriken::metrics()` is just handing you a slice into that array, concatenated with whatever was registered at runtime via the dyn registry. This is why metriken-core has the `links = "metriken-core"` directive in its `Cargo.toml`: two copies of the slice in the same binary would be a silent disaster, so Cargo is forced to refuse the build.

**The wire format is wide.** Each labeled time series is its own Parquet column. The metric's canonical name and label pairs live in Arrow field metadata. Histograms are stored as `List<UInt64>` (the bucket counts), with `grouping_power` / `max_value_power` in the column metadata. This shape is what lets the SQL side address series via `_src."col_name"` and project rates with windowed `LAG` instead of doing a self-join on a long-form `(t, metric, labels..., value)` table.

**Two query backends, one result type.** The legacy PromQL evaluator (in `promql/streaming/*`) builds iterator pipelines over an in-memory `Tsdb`. It's the live path today — Rezolus and its viewers depend on it. The SQL harness (in `harness/*`) is migration scaffolding: a registry of ~69 known PromQL shapes (`queries.toml`), each with a template-matcher + a wide-form SQL emitter + a positional projector that turns DuckDB's Arrow output back into the same `QueryResult`. The harness exists so it can be exercised side-by-side via `examples/sql_vs_promql.rs` until the dashboards emit SQL natively; the whole directory is a clean delete once that happens.

**H2 histograms cross the boundary intact.** Rezolus's H2 (base-2) histogram layout is the one representation that survives the trip from producer to query. On the producer side, `metriken::AtomicHistogram` wraps the canonical `histogram` crate's layout. On the SQL side, `metriken-query-sql/src/udf.rs` reimplements that same bucket math as DuckDB scalar UDFs (`h2_lower`, `h2_upper`, `h2_quantile`, `h2_delta`, …) so SQL queries can do quantiles and per-period deltas directly over the `List<UInt64>` bucket columns without unpacking. The bucket-math is the contract; both sides verify against it.
