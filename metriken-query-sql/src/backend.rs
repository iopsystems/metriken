//! Embedded-DuckDB query engine — per-`data_source` connection pool with panic-safe
//! slot eviction.
//!
//! `DuckDbBackend` keeps a small **connection pool** per `data_source`
//! (parquet path or glob). The first request for a given source pays the
//! cold-start cost: for each pool slot, open an in-memory DB, register
//! UDFs + macros, and load the parquet into a `_src` TEMP TABLE via
//! `views::ensure_views`. Subsequent requests check out a slot via
//! atomic round-robin and only pay the SQL `prepare_cached` + `execute`
//! cost.
//!
//! `Connection` is `!Sync` (DuckDB connections are single-threaded), so
//! each pool slot is a `Mutex<Connection>`. With pool size N, up to N
//! requests against the same fixture proceed concurrently before any
//! lock contention. Default N is `available_parallelism().min(8)`,
//! overridable via `METRIKEN_SQL_POOL`.
//!
//! Two output shapes, dispatched on `entry.output_shape`:
//!
//! **Matrix** (default) — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Columns 1..1+`value_columns.len()` — DOUBLE values (column 1 is used
//!   today; multi-value support arrives with multi-quantile queries).
//! - Remaining columns — series-defining label values, one per
//!   `label_columns` entry, in the order declared.
//!
//! **Heatmap** — positional columns:
//! - Column 0 — `t`, DOUBLE seconds.
//! - Column 1 — `bucket_idx`, INTEGER (H2 bucket index, NOT remapped).
//! - Column 2 — `count`, DOUBLE (non-zero count for this `(t, bucket)`).
//! - Column 3 — `p`, INTEGER (grouping_power; expected constant within a query).
//!
//! Positional rather than name-based access avoids duckdb-rs's habit of
//! panicking if column metadata is read before the statement executes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arrow::record_batch::RecordBatch;
use duckdb::Connection;

use crate::live::LiveSource;
use crate::observability::BackendStats;
use crate::views::MetricCatalog;
use crate::SqlError;

/// Per-data-source state: a pool of independent in-memory DuckDB
/// connections plus the shared (Rust-side) metadata catalog produced
/// by `ensure_views`. Each connection has its own UDF registrations,
/// `_src` TEMP TABLE, and prepared-statement cache; checkout is
/// lockless via a round-robin atomic counter, so contention only
/// arises when concurrent requests > pool size.
///
/// Each slot is `Mutex<Option<Connection>>` rather than
/// `Mutex<Connection>`. A slot can be **empty** in two cases:
/// 1. After a panic inside DuckDB query execution — we
///    `catch_unwind` at the `run` boundary and clear the slot so
///    its (possibly inconsistent) connection state isn't reused.
/// 2. Lazy rebuild path — a fresh connection is built on the next
///    checkout when the slot is `None`.
///
/// Crucially, because we catch the panic **before** the lock guard
/// is dropped, the `Mutex` itself is never poisoned: peer slots in
/// the pool keep serving without any cross-slot fallout. With the
/// pool sized to expected concurrency, one bad query loses its own
/// response and (briefly) one slot, not the whole process.
struct ConnState {
    pool: Vec<Mutex<Option<Connection>>>,
    next: AtomicUsize,
    /// Per-metric catalog shared by every query against this data
    /// source. Built once at pool-init time; cloning the `Arc` is
    /// O(1) and lets callers pass `&MetricCatalog` to the translator
    /// without paying the deep-clone cost on every query.
    catalog: Arc<MetricCatalog>,
    /// Sampling interval (ns). Kept for observability — the actual
    /// `_src` setup uses the pre-rendered SQL below.
    #[allow(dead_code)]
    interval_ns: u64,
    /// Pre-rendered `_src` setup SQL. Built once at pool-init time
    /// from the parquet schema; for multi-source captures it carries
    /// canonical-alias projections so dashboard SQL's bare regexes
    /// (`^cpu_usage(/[^:]+)?$` etc.) bind on prefixed parquets.
    src_sql: Arc<str>,
    /// Pre-rendered `_cgroup_index` setup SQL. Built once at
    /// pool-init time so a lazy slot rebuild (post-panic recovery
    /// path) doesn't have to re-walk the parquet schema. Empty
    /// only when the parquet carries no cgroup columns — the
    /// CREATE statement itself is always included so dashboard
    /// SQL JOINing against `_cgroup_index` binds cleanly.
    cgroup_index_sql: Arc<str>,
    /// Pre-rendered `_src_<source>` per-source view DDL — one
    /// `CREATE TEMP VIEW` per distinct `source` label value found in
    /// the parquet's column field metadata. Empty when no column
    /// carries a `source` label. Lets service-extension KPI SQL
    /// target `_src_<service_name>` and bind regardless of how many
    /// instances of the same source the parquet ships.
    per_source_views_sql: Arc<str>,
    per_node_views_sql: Arc<str>,
    /// Captured at backend-construction time; needed when a slot
    /// post-panic rebuilds, since `pool_size` lives on the backend.
    pool_size: usize,
}

/// Default DuckDB-backed implementation of `SqlBackend`. Lazily builds
/// a connection pool per unique `data_source` on first request. Pool
/// size is fixed at backend construction; tune via
/// `DuckDbBackend::with_pool_size` or the `METRIKEN_SQL_POOL` env var.
///
/// Two kinds of data sources cohabit on the same backend:
///
/// - **Parquet sources** — keyed in `connections` by file path or
///   glob. Lazily warmed on first `run_sql`; each map entry holds the
///   N-slot pool described above.
/// - **Live sources** — keyed in `live_sources` by a caller-supplied
///   string (e.g. `"baseline"`). Created up-front by
///   [`DuckDbBackend::create_live_source`], single-connection
///   single-mutex (see [`LiveSource`] docs). When a caller passes a
///   key that matches a live source to `run_sql` or `describe`, the
///   request routes there instead of through the parquet path.
///
/// The maps are checked separately so the same string can never name
/// both a live source and a parquet path on the same backend — live
/// keys are checked first; if no match, the request is treated as a
/// parquet path. In practice rezolus uses
/// `CaptureBackend::{Sql, Live}` to assign distinct strings, so
/// collisions are not possible by construction.
pub struct DuckDbBackend {
    connections: Mutex<HashMap<String, Arc<ConnState>>>,
    live_sources: Mutex<HashMap<String, Arc<LiveSource>>>,
    pool_size: usize,
    stats: Arc<BackendStats>,
}

/// Default pool size: `available_parallelism().min(8)`, with
/// `METRIKEN_SQL_POOL` as an explicit override. Cap at 8 because
/// cold-start scales linearly with pool size (each slot pays the
/// open + register + parquet-read cost) and 8 is well past the
/// point of diminishing returns on a busy single-fixture viewer.
fn default_pool_size() -> usize {
    if let Ok(v) = std::env::var("METRIKEN_SQL_POOL") {
        if let Ok(n) = v.parse::<usize>() {
            return n.max(1);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4)
}

impl DuckDbBackend {
    pub fn new() -> Self {
        Self::with_pool_size(default_pool_size())
    }

    /// Construct a backend with an explicit pool size. `n` is clamped to
    /// at least 1; passing 1 reproduces the pre-pool single-connection
    /// behavior. Useful in tests where deterministic single-threaded
    /// execution is wanted.
    pub fn with_pool_size(n: usize) -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            live_sources: Mutex::new(HashMap::new()),
            pool_size: n.max(1),
            stats: Arc::new(BackendStats::default()),
        }
    }

    /// Register a live data source under `data_source`. The returned
    /// `Arc<LiveSource>` is the appender handle for the caller's
    /// ingest loop; subsequent `run_sql(_, data_source)` calls route
    /// to it. Errors if a live source is already registered under the
    /// same key — re-registration would silently lose the old
    /// source's accumulated rows.
    pub fn create_live_source(
        &self,
        data_source: &str,
        source_name: &str,
        sampling_interval_ms: u64,
    ) -> Result<Arc<LiveSource>, SqlError> {
        let live = LiveSource::new(source_name, sampling_interval_ms)?;
        let mut map = self.live_sources.lock().expect("poisoned");
        if map.contains_key(data_source) {
            return Err(SqlError::Backend(format!(
                "live source '{data_source}' already registered"
            )));
        }
        map.insert(data_source.to_string(), live.clone());
        Ok(live)
    }

    /// Look up a registered live source by `data_source` key. Returns
    /// `None` for parquet sources. Used by `run_sql` / `describe_parquet`
    /// / `invalidate` to dispatch on source kind.
    fn live_source(&self, data_source: &str) -> Option<Arc<LiveSource>> {
        let map = self.live_sources.lock().expect("poisoned");
        map.get(data_source).cloned()
    }

    /// Per-backend in-process counters. Lock-free; safe to read from
    /// any thread. See `observability::StatsSnapshot::Display` for a
    /// human-readable dump.
    pub fn stats(&self) -> Arc<BackendStats> {
        self.stats.clone()
    }

    /// Look up (or lazily build) the cached connection pool for
    /// `data_source`. Returns the `Arc` and a `cold` flag — `true`
    /// means this call paid the per-slot open + UDF-registration +
    /// view-building cost, `false` means the pool was already warm.
    fn get_or_init(&self, data_source: &str) -> Result<(Arc<ConnState>, bool), SqlError> {
        // Fast path: already cached.
        {
            let map = self.connections.lock().expect("poisoned");
            if let Some(arc) = map.get(data_source) {
                return Ok((arc.clone(), false));
            }
        }
        // Slow path: build under the outer lock.
        let mut map = self.connections.lock().expect("poisoned");
        if let Some(arc) = map.get(data_source) {
            return Ok((arc.clone(), false));
        }
        let timing = std::env::var("METRIKEN_SQL_TIMING").is_ok();
        let t0 = std::time::Instant::now();

        // Read parquet metadata exactly once. Pre-Phase-A this happened
        // per-slot inside `views::ensure_views`; with `pool_size = 8`
        // that meant 8 metadata reads at cold-start. Hoist it here so
        // the pool slots only do the per-connection setup.
        let (columns, interval_ns) = crate::views::read_introspection(data_source)
            .map_err(|e| SqlError::Backend(format!("read_introspection {data_source}: {e}")))?;
        let catalog = Arc::new(crate::views::build_catalog(&columns));
        // Pre-render the `_src` and `_cgroup_index` setup SQL once;
        // pool slots and lazy rebuilds reuse these strings instead
        // of re-walking the parquet metadata.
        let src_sql: Arc<str> = Arc::from(crate::views::render_src_sql(
            data_source,
            interval_ns,
            &columns,
        ));
        let cgroup_index_sql: Arc<str> = Arc::from(crate::views::render_cgroup_index_sql(&columns));
        let per_source_views_sql: Arc<str> = Arc::from(crate::views::render_per_source_views_sql(
            data_source,
            interval_ns,
            &columns,
        ));
        let per_node_views_sql: Arc<str> = Arc::from(crate::views::render_per_node_views_sql(
            data_source,
            interval_ns,
            &columns,
        ));

        let mut pool: Vec<Mutex<Option<Connection>>> = Vec::with_capacity(self.pool_size);
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;

        for _ in 0..self.pool_size {
            let conn = build_slot_connection(
                self.pool_size,
                &src_sql,
                &cgroup_index_sql,
                &per_source_views_sql,
                &per_node_views_sql,
            )?;
            pool.push(Mutex::new(Some(conn)));
        }

        if timing {
            eprintln!(
                "duckdb cold-start data_source={} pool_size={} wall={:.1}ms",
                data_source,
                self.pool_size,
                ms(t0.elapsed()),
            );
        }

        let state = Arc::new(ConnState {
            pool,
            next: AtomicUsize::new(0),
            catalog,
            interval_ns,
            src_sql,
            cgroup_index_sql,
            per_source_views_sql,
            per_node_views_sql,
            pool_size: self.pool_size,
        });
        map.insert(data_source.to_string(), state.clone());
        Ok((state, true))
    }

    /// Execute `sql` against `data_source` and return the raw Arrow
    /// `RecordBatch`es DuckDB produces. The caller is responsible for
    /// any projection (e.g. into `QueryResult` shapes).
    ///
    /// Routing: a live source registered under `data_source` (see
    /// [`Self::create_live_source`]) handles the request; otherwise
    /// `data_source` is treated as a parquet path and routed through
    /// the pooled in-memory DBs.
    pub fn run_sql(&self, sql: &str, data_source: &str) -> Result<Vec<RecordBatch>, SqlError> {
        let t_total = std::time::Instant::now();

        // Live sources short-circuit the parquet pool. Their single
        // shared Connection serializes reads against writes via its
        // own Mutex — see `live.rs` for the rationale.
        if let Some(live) = self.live_source(data_source) {
            let result = live.run_sql(sql);
            self.stats.total.record(t_total.elapsed().as_nanos() as u64);
            return result;
        }

        let (state, _cold) = self.get_or_init(data_source)?;

        // Acquire a slot. Round-robin via `next` gives a starting point;
        // we then scan all slots non-blockingly and take the first one
        // that's free. Falls back to blocking on the round-robin pick
        // only when every slot is busy. This eliminates the "queued
        // behind a slow slot while peers idle" pathology — a slow query
        // in slot 3 no longer holds back every 8th incoming task on a
        // pool-size-8 backend.
        let start = state.next.fetch_add(1, Ordering::Relaxed) % state.pool.len();
        let (idx, mut slot) = {
            let mut acquired = None;
            for offset in 0..state.pool.len() {
                let candidate = (start + offset) % state.pool.len();
                if let Ok(guard) = state.pool[candidate].try_lock() {
                    acquired = Some((candidate, guard));
                    break;
                }
            }
            acquired.unwrap_or_else(|| {
                (
                    start,
                    state.pool[start].lock().expect("slot mutex poisoned"),
                )
            })
        };

        if slot.is_none() {
            // Lazy rebuild after a panic. Reuses the pre-rendered
            // `_src` + `_cgroup_index` + per-source view setup SQL so
            // this path doesn't pay another parquet introspection.
            let conn = build_slot_connection(
                state.pool_size,
                &state.src_sql,
                &state.cgroup_index_sql,
                &state.per_source_views_sql,
                &state.per_node_views_sql,
            )?;
            *slot = Some(conn);
        }
        let conn_ref = slot.as_ref().expect("just-initialised slot");

        // Catch panics raised inside the Rust code DuckDB calls back
        // into during query execution (e.g. arrow-conversion bugs).
        // Note: panics that originate inside a UDF callback cross the
        // duckdb-rs C++ FFI as non-unwinding and abort the process
        // before this boundary sees them — see the `h2_combine`
        // LIST<LIST> note in `udf.rs` for the canonical example.
        //
        // **Ordering is load-bearing.** The `catch_unwind` runs inside
        // the `MutexGuard` (`slot`) scope. If it were moved outside —
        // letting the panic unwind across the guard's drop — the
        // slot's `Mutex` would be **poisoned**, every subsequent
        // checkout of this slot would error, and the only recovery
        // path would be evicting the entire pool. By catching inside
        // we keep the `Mutex` clean, clear the slot to `None`, and
        // peer slots remain unaffected. The post-panic recovery shape
        // (slot=None → lazy rebuild on next checkout, peer slots
        // untouched) is pinned by
        // `tests::empty_slot_is_lazily_rebuilt_without_mutex_poisoning`.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut stmt = conn_ref
                .prepare_cached(sql)
                .map_err(|e| SqlError::Backend(format!("prepare: {e}")))?;
            let arrow = stmt
                .query_arrow([])
                .map_err(|e| SqlError::Backend(format!("query_arrow: {e}")))?;
            let batches: Vec<RecordBatch> = arrow.collect();
            Ok::<_, SqlError>(batches)
        }));

        let result = match outcome {
            Ok(r) => r,
            Err(payload) => {
                *slot = None;
                let msg = panic_message(&payload);
                Err(SqlError::Backend(format!(
                    "internal SQL panic (slot {idx}): {msg}"
                )))
            }
        };

        // Record total wall-clock for this call. Per-phase counters
        // (slot_lock / prepare / execute / extract) are kept in
        // `BackendStats` for future instrumentation; for now we ship
        // top-line latency only. See `observability.rs`.
        self.stats.total.record(t_total.elapsed().as_nanos() as u64);
        result
    }

    /// Return the per-metric catalog (parquet metadata: physical
    /// columns, label maps, histogram `grouping_power`) for
    /// `data_source`. Returns an `Arc` so the caller can hand a
    /// borrow to the translator without paying per-query clone cost.
    /// Warm path is a hashmap lookup + Arc clone; cold path pays
    /// one parquet metadata read and does **not** warm the connection
    /// pool.
    ///
    /// For live sources, returns a catalog built from the current
    /// schema state. The catalog changes over time as new metrics
    /// appear; callers that cache it should refresh after schema
    /// growth (or just call this each time — it's a hashmap read +
    /// per-column descriptor clone).
    pub fn describe_parquet(&self, data_source: &str) -> Result<Arc<MetricCatalog>, SqlError> {
        // Live source short-circuit.
        if let Some(live) = self.live_source(data_source) {
            return Ok(Arc::new(live.catalog()));
        }
        // Warm path: pool already initialised for this source.
        {
            let map = self.connections.lock().expect("poisoned");
            if let Some(state) = map.get(data_source) {
                return Ok(state.catalog.clone());
            }
        }
        // Cold path: pure parquet introspection, no pool warm-up.
        crate::views::describe_parquet(data_source)
            .map(Arc::new)
            .map_err(|e| SqlError::Backend(format!("describe_parquet {data_source}: {e}")))
    }

    /// Evict the cached connection pool for `data_source`. Subsequent
    /// `run_sql` / `describe_parquet` calls against that source pay
    /// the full cold-start cost again. Returns `true` if a pool was
    /// actually present (the source was warm).
    ///
    /// Callers (e.g. the rezolus viewer's experiment-detach handler)
    /// invoke this after the underlying parquet file is removed or
    /// replaced — the pool's `_src` TEMP TABLE was loaded from that
    /// file at pool-init time and is stale by definition. In-flight
    /// queries holding an `Arc<ConnState>` keep their pool slots
    /// alive until they finish; only the cache entry is dropped.
    ///
    /// Live sources are also evicted here — the registered key is
    /// removed from the live-sources map and any `Arc<LiveSource>`
    /// the caller still holds keeps working in isolation. Use this
    /// when shutting down the live ingest loop.
    pub fn invalidate(&self, data_source: &str) -> bool {
        let live_removed = {
            let mut map = self.live_sources.lock().expect("poisoned");
            map.remove(data_source).is_some()
        };
        let parquet_removed = {
            let mut map = self.connections.lock().expect("poisoned");
            map.remove(data_source).is_some()
        };
        live_removed || parquet_removed
    }
}

/// Build one pool slot's connection: open the in-memory DB, set the
/// statement-cache capacity, register UDFs/macros, build the `_src`
/// TEMP TABLE, and seed the `_cgroup_index` TEMP TABLE. The caller
/// passes `interval_ns` and the pre-rendered cgroup-index SQL from
/// a single hoisted introspection so this function does no parquet
/// metadata I/O of its own.
fn build_slot_connection(
    pool_size: usize,
    src_sql: &str,
    cgroup_index_sql: &str,
    per_source_views_sql: &str,
    per_node_views_sql: &str,
) -> Result<Connection, SqlError> {
    let conn =
        Connection::open_in_memory().map_err(|e| SqlError::Backend(format!("open duckdb: {e}")))?;
    // Default prepared-statement cache is 16; bump it so the catalogue
    // (~70 distinct shapes) plus per-label-value variants all fit
    // and stay parsed/planned across repeat queries from the viewer.
    // Each cached Statement is small (planning artifacts only).
    conn.set_prepared_statement_cache_capacity(1024);
    // When pooling, cap DuckDB's intra-query worker threads to 1: we
    // are already achieving parallelism at the pool level. pool_size
    // connections × DuckDB's default of N internal workers per query
    // oversubscribes the box (e.g. 4 slots × 4 internal workers = 16
    // threads on 4 cores), causing context-switch storms that show up
    // as a *regression* at N ≈ pool_size in the concurrent bench.
    // With pool_size=1 (single-connection mode) we leave DuckDB's
    // intra-query parallelism alone — that's the only way a single
    // request gets multi-core.
    if pool_size > 1 {
        conn.execute("PRAGMA threads=1", [])
            .map_err(|e| SqlError::Backend(format!("pragma threads: {e}")))?;
    }
    crate::register_all(&conn)
        .map_err(|e| SqlError::Backend(format!("register UDFs/macros: {e}")))?;
    conn.execute(src_sql, [])
        .map_err(|e| SqlError::Backend(format!("create _src table: {e}")))?;
    // `_cgroup_index` is always created (cgroups dashboard SQL JOINs
    // against it unconditionally); the INSERT body is empty when the
    // parquet has no cgroup columns.
    conn.execute_batch(cgroup_index_sql)
        .map_err(|e| SqlError::Backend(format!("create _cgroup_index: {e}")))?;
    // Per-source views (`_src_<prefix>` for each `<prefix>::`-tagged
    // application source) are created on multi-source captures so
    // service-extension KPI SQL binds. Skipped when the parquet is
    // single-source — the string is empty in that case.
    if !per_source_views_sql.is_empty() {
        conn.execute_batch(per_source_views_sql)
            .map_err(|e| SqlError::Backend(format!("create per-source views: {e}")))?;
    }
    // Per-node views (`_src_node_<X>` for each `node`-labelled column
    // set) are created on multi-node captures so SQL can target a
    // single node. Skipped when no column carries a `node` label.
    if !per_node_views_sql.is_empty() {
        conn.execute_batch(per_node_views_sql)
            .map_err(|e| SqlError::Backend(format!("create per-node views: {e}")))?;
    }
    Ok(conn)
}

impl Default for DuckDbBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort extraction of a printable message from a panic payload.
/// `Box<dyn Any + Send>` carries the `panic!()` argument, which is
/// usually a `&'static str` or a `String`; a few panics carry other
/// types and we fall back to a generic label rather than guessing.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// `run_matrix`, `downcast_f64`, and `run_heatmap` moved to
// `metriken-query/src/project.rs` as part of the Phase A boundary
// flip. The DuckDB engine now exposes `run_sql` returning raw
// `Vec<RecordBatch>`; projection into the `metriken-query`-side
// `QueryResult::{Matrix, HistogramHeatmap}` shapes is a translator
// concern. The H2 bucket-bound helpers (`udf::h2_lower`, `udf::h2_upper`)
// remain `pub` and are called from `project.rs` across the crate
// boundary — pure functions over `(idx, p)`, no leak.

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("metriken-query-fixtures")
            .join("fixtures")
            .join(format!("{name}.parquet"))
    }

    /// Pin the panic-recovery contract that `catch_unwind` exists to
    /// support: when a slot's `Option<Connection>` is observed as
    /// `None` (the state `catch_unwind` leaves a panicked slot in),
    /// the next checkout transparently rebuilds it.
    ///
    /// Why test the recovery shape rather than driving a real panic:
    /// the only known sources of intra-query panics are UDF
    /// callbacks, whose panics cross the duckdb-rs C++ FFI as
    /// non-unwinding and abort the process before `catch_unwind` can
    /// see them (the `h2_combine` LIST<LIST> mode at `udf.rs:525`
    /// documents this). Panics that `catch_unwind` *can* catch
    /// originate inside Rust code DuckDB calls back into (e.g. arrow
    /// conversion bugs); reproducing those deterministically would
    /// pin us to a specific duckdb-rs version's misbehaviour. So we
    /// stub the post-panic state and verify the contract instead.
    #[test]
    fn empty_slot_is_lazily_rebuilt_without_mutex_poisoning() {
        let backend = DuckDbBackend::with_pool_size(2);
        let path = fixture_path("counter_basic");
        let path = path.to_str().unwrap();

        // Warm the pool and grab the next-counter so we can target
        // the slot the next round-robin pick will land on.
        backend.run_sql("SELECT 1", path).expect("warm pool");
        let state = backend
            .connections
            .lock()
            .expect("poisoned")
            .get(path)
            .expect("pool warm")
            .clone();
        let next_idx = state.next.load(Ordering::Relaxed) % state.pool.len();

        // Drop the connection in that slot. This is the state
        // `catch_unwind` leaves behind after a panicking query
        // (backend.rs `*slot = None`). The mutex itself stays alive
        // and unpoisoned.
        *state.pool[next_idx].lock().expect("not poisoned") = None;

        // The very next call must (a) hit that emptied slot, (b)
        // rebuild it (re-registering UDFs + recreating `_src`), and
        // (c) return the expected result.
        let batches = backend
            .run_sql("SELECT COUNT(*) AS n FROM _src", path)
            .expect("rebuilt slot serves query");
        let n = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(n, 11);
        assert!(
            state.pool[next_idx].lock().expect("not poisoned").is_some(),
            "slot must be populated post-rebuild",
        );

        // Peer slot still serves too — proving no cross-slot fallout.
        let peer_idx = (next_idx + 1) % state.pool.len();
        assert!(state.pool[peer_idx].lock().expect("not poisoned").is_some());
        backend
            .run_sql("SELECT COUNT(*) FROM _src", path)
            .expect("peer slot still healthy");
    }

    #[test]
    fn run_sql_returns_arrow_record_batches() {
        let backend = DuckDbBackend::with_pool_size(1);
        let path = fixture_path("counter_basic");
        let path = path.to_str().unwrap();
        let batches = backend
            .run_sql("SELECT COUNT(*) AS n FROM _src", path)
            .expect("run_sql ok");
        // Single batch with one row, one column "n".
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);
        let n_col = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .expect("n is i64");
        assert_eq!(n_col.value(0), 11); // counter_basic has 11 timestamps (0..=10)
    }

    #[test]
    fn run_sql_surfaces_bad_sql_as_error() {
        let backend = DuckDbBackend::with_pool_size(1);
        let path = fixture_path("counter_basic");
        let path = path.to_str().unwrap();
        let err = backend
            .run_sql("SELECT * FROM nonexistent_table", path)
            .expect_err("should error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("nonexistent_table") || msg.contains("Catalog"),
            "expected DuckDB binder error referencing the missing table, got: {msg}"
        );
    }

    #[test]
    fn describe_parquet_warm_cache_avoids_reread() {
        // Warm path: pool already initialised → describe_parquet returns
        // the cached catalog without re-reading the parquet file.
        let backend = DuckDbBackend::with_pool_size(1);
        let path = fixture_path("counter_multi_label");
        let path = path.to_str().unwrap();
        // Warm by running any query.
        backend.run_sql("SELECT 1", path).expect("warm pool");
        let cat = backend.describe_parquet(path).expect("describe ok");
        assert_eq!(cat.series_by_metric.get("cpu_usage").unwrap().len(), 4);
    }

    #[test]
    fn describe_parquet_cold_path_does_not_warm_pool() {
        // Cold path: describe without warming the pool. Subsequent
        // queries should still work (pool init is independent).
        let backend = DuckDbBackend::with_pool_size(1);
        let path = fixture_path("counter_multi_label");
        let path = path.to_str().unwrap();
        let cat = backend.describe_parquet(path).expect("describe ok");
        assert_eq!(cat.series_by_metric.get("cpu_usage").unwrap().len(), 4);
        // Confirm nothing was inserted into the pool map yet.
        let pool_warm = backend.connections.lock().unwrap().contains_key(path);
        assert!(
            !pool_warm,
            "describe_parquet on a cold source must not warm the pool"
        );
    }
}
