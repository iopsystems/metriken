# Metriken — Architecture

Metriken is two systems separated by a Parquet file in the middle — or, in the live-agent case, by a `LiveSource` ingest: same wide-column shape, no parquet on disk. On the left, an application produces metrics; on the right, a dashboard reads them back and asks questions.

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

              Parquet file on disk                       Live agent snapshots
                       │                                           │
                       │ read_parquet(...)                          │ append per snapshot
                       │ (one slot per pool slot)                   │ ALTER TABLE _src ADD COLUMN
                       ▼                                            ▼
              ┌────────────────────────────────────────────────────────────────┐
              │           metriken-query-sql ── DuckDbBackend                  │
              │                                                                │
              │  parquet path:                       live path:                │
              │  ├─ ConnState pool                   ├─ LiveSource             │
              │  │  (N independent in-mem DBs)       │  (one shared            │
              │  ├─ _src TEMP TABLE per slot         │   Mutex<Connection>)    │
              │  ├─ _cgroup_index TEMP TABLE         ├─ _src TABLE (grows)     │
              │  ├─ _src_<source> TEMP VIEWs         ├─ _cgroup_index          │
              │  └─ MetricCatalog from               │  (rebuilt on cgroup     │
              │     parquet field metadata           │   column add)           │
              │                                      ├─ _src_<source> as       │
              │                                      │   SELECT * (passthrough)│
              │                                      └─ MetricCatalog          │
              │                                                                │
              │  Shared on every connection:                                   │
              │  ├─ H2 histogram UDFs (h2_lower/upper/quantile/delta/...)      │
              │  └─ shared_macros.sql (irate_1s, rate_5m, hist_p99, ...)       │
              │                                                                │
              │            run_sql(sql, data_source) ──► Arrow batches         │
              └────────────────────────────────────────────────────────────────┘
                                          │
                                          ▼
                            crates/prom-matrix or harness::project
                            (Arrow → Matrix/Heatmap shape consumers
                             on the rezolus + WASM viewer side)

(The metriken-query crate — legacy PromQL evaluator + harness — is
deleted in C5 of this branch. The diagram above is the post-deletion
end state.)
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
                     GaugeGroup, HistogramGroup, Lazy<T>. Re-exports
                     the macro and the registry accessors from -core.

metriken-exposition── reads the registry; produces Snapshot{V1,V2}.
                     Output drivers (feature-gated):
                       prometheus  – text format
                       json/msgpack – serde
                       parquet     – wide-form, one column per series,
                                     label k/v pairs in field metadata.
                     MsgpackToParquet converts streamed snapshots into
                     a single Parquet file after the fact.

metriken-query     ── DELETED IN C5. Was the consumer-side PromQL
                     evaluator (tsdb/ + promql/) plus a PromQL→SQL
                     translator harness (harness/) that never gained
                     a production caller. Its last consumer
                     (rezolus's validate_service_extensions)
                     migrated to SQL in C3.

metriken-query-sql ── The query engine.
                       parquet path: DuckDbBackend owns a per-data-
                       source pool of in-memory DuckDB connections,
                       each materialising _src + _cgroup_index +
                       _src_<source> per-source views from the
                       parquet's Arrow field metadata.
                       live path: LiveSource owns a single shared
                       Mutex<Connection> whose _src grows via
                       ALTER + INSERT per agent snapshot.
                       Registers H2 histogram UDFs + shared SQL macro
                       library on every connection. Exposes
                       MetricCatalog via both source kinds so wide-
                       form SQL selects by canonical metric name +
                       labels regardless of where the data came from.

metriken-query-
   -fixtures       ── series-shaped builder on top of
                     metriken-exposition for deterministic Parquet
                     test fixtures.
```

## The key ideas, in plain English

**Producer side.** A `#[metric] static FOO: Counter` is just a static. Mutating it is one atomic op — no map lookup, no lock, no allocation. The "registry" is built by the linker: `linkme` collects every `#[metric]` static across the whole binary into one contiguous array (`METRICS`) at link time. Calling `metriken::metrics()` is just handing you a slice into that array, concatenated with whatever was registered at runtime via the dyn registry. This is why metriken-core has the `links = "metriken-core"` directive in its `Cargo.toml`: two copies of the slice in the same binary would be a silent disaster, so Cargo is forced to refuse the build.

**The wire format is wide.** Each labeled time series is its own Parquet column. The metric's canonical name and label pairs live in Arrow field metadata. Histograms are stored as `List<UInt64>` (the bucket counts), with `grouping_power` / `max_value_power` in the column metadata. This shape is what lets the SQL side address series via `_src."col_name"` and project rates with windowed `LAG` instead of doing a self-join on a long-form `(t, metric, labels..., value)` table.

**One query backend, two ingest paths.** Post-C5 `metriken-query` is gone — the legacy PromQL evaluator (`promql/streaming/*`) and the SQL harness (`harness/*`) have been deleted. Rezolus and the static viewer drive SQL through `metriken-query-sql::DuckDbBackend` directly, with the parquet path materializing `_src` from `read_parquet(...)` and the live path appending to a `LiveSource` that owns a single mutable `_src` table. The SQL macros (`shared_macros.sql`, re-exported as `SHARED_MACROS`) are byte-identical across native and WASM consumers because `include_str!` pulls the same file into both. The PromQL evaluator's role lived only long enough to backstop `validate_service_extensions` while service-extension KPI templates accumulated SQL coverage; once 128/218 templates carried SQL and LiveSource bridged the live-agent path, the deletion plan executed.

**H2 histograms cross the boundary intact.** Rezolus's H2 (base-2) histogram layout is the one representation that survives the trip from producer to query. On the producer side, `metriken::AtomicHistogram` wraps the canonical `histogram` crate's layout. On the SQL side, `metriken-query-sql/src/udf.rs` reimplements that same bucket math as DuckDB scalar UDFs (`h2_lower`, `h2_upper`, `h2_quantile`, `h2_delta`, …) so SQL queries can do quantiles and per-period deltas directly over the `List<UInt64>` bucket columns without unpacking. The bucket-math is the contract; both sides verify against it.
