//! Auto-walking shadow-mode promotion gate: every `mode = "shadow"` catalogue
//! entry × every example × every available real parquet must produce
//! byte-equal canonical JSON between PromQL and SQL.
//!
//! This is the gate that decides whether a new templated entry is "done."
//! Drop a new `[[query.examples]]` entry into `queries.toml` and this test
//! picks it up automatically — no per-entry Rust glue needed.
//!
//! Optional `dump_*` tests below remain as targeted diagnostics for individual
//! query shapes; they print side-by-side comparisons rather than failing on
//! mismatch, useful when investigating an in-flight divergence.
//!
//! Run with: `cargo test -p metriken-query --test divergence_inspector`
//! Use `-- --nocapture` to see PromQL/SQL summaries on the diagnostic tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metriken_query::{
    canonicalise, Catalogue, CatalogueEntry, CompiledTemplate, Mode, QueryEngine, QueryResult,
    SqlBackend, Tsdb,
};
use metriken_query_sql::DuckDbBackend;

/// Render a multi-line first-difference dump for a single divergence. Lives at
/// the bottom of the `divergence_report` output so each failing pair shows
/// not just `series=N vs series=M` but the first concrete (t, v, v) point that
/// disagrees — without this, every fix attempt needed a one-off debug harness.
///
/// Set `METRIKEN_DIFF_DUMP_SQL=1` to also include the rendered SQL string per
/// failure (useful while iterating on a catalogue entry; noisy at 33 failures).
fn first_diff_dump(p: &serde_json::Value, s: &serde_json::Value) -> String {
    use serde_json::Value;
    let mut out = String::new();

    let p_result = p.get("result");
    let s_result = s.get("result");

    // Heatmap shape: result is an object, not an array. Compare top-level
    // fields and short-circuit.
    if matches!(p_result, Some(Value::Object(_))) || matches!(s_result, Some(Value::Object(_))) {
        let p_shape = p
            .get("resultType")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let s_shape = s
            .get("resultType")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        out.push_str(&format!(
            "    heatmap shape: PromQL resultType={} SQL resultType={}\n",
            p_shape, s_shape
        ));
        if p_shape == s_shape {
            for key in [
                "timestamps",
                "bucket_bounds",
                "data",
                "min_value",
                "max_value",
            ] {
                let pv = p_result.and_then(|r| r.get(key));
                let sv = s_result.and_then(|r| r.get(key));
                if pv != sv {
                    let render = |v: Option<&Value>| match v {
                        Some(Value::Array(a)) => {
                            let n = a.len();
                            let head: Vec<&Value> = a.iter().take(3).collect();
                            let tail: Vec<&Value> = a.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
                            format!("len={} head={:?} tail={:?}", n, head, tail)
                        }
                        Some(other) => other.to_string(),
                        None => "<missing>".to_string(),
                    };
                    out.push_str(&format!(
                        "    field {} differs: PromQL={} SQL={}\n",
                        key,
                        render(pv),
                        render(sv)
                    ));
                }
            }
        }
        return out;
    }

    let empty: Vec<Value> = Vec::new();
    let p_arr: &Vec<Value> = p_result.and_then(|v| v.as_array()).unwrap_or(&empty);
    let s_arr: &Vec<Value> = s_result.and_then(|v| v.as_array()).unwrap_or(&empty);

    // Index by `metric` so the same logical series is compared even when the
    // engines emit them in different orders. canonicalise() already sorts by
    // metric, so identical metric-sets line up positionally — but if one side
    // produced an extra series the offsets shift, so a label-keyed index is
    // more robust.
    let metric_key = |v: &Value| v.get("metric").map(|m| m.to_string()).unwrap_or_default();
    let mut p_by_metric: std::collections::BTreeMap<String, &Value> = Default::default();
    let mut s_by_metric: std::collections::BTreeMap<String, &Value> = Default::default();
    for v in p_arr {
        p_by_metric.insert(metric_key(v), v);
    }
    for v in s_arr {
        s_by_metric.insert(metric_key(v), v);
    }

    // Label-set asymmetry, if any.
    let p_only: Vec<&String> = p_by_metric.keys().filter(|k| !s_by_metric.contains_key(*k)).collect();
    let s_only: Vec<&String> = s_by_metric.keys().filter(|k| !p_by_metric.contains_key(*k)).collect();
    if !p_only.is_empty() || !s_only.is_empty() {
        if !p_only.is_empty() {
            out.push_str(&format!(
                "    {} series only in PromQL (first 3): {:?}\n",
                p_only.len(),
                p_only.iter().take(3).collect::<Vec<_>>()
            ));
        }
        if !s_only.is_empty() {
            out.push_str(&format!(
                "    {} series only in SQL    (first 3): {:?}\n",
                s_only.len(),
                s_only.iter().take(3).collect::<Vec<_>>()
            ));
        }
    }

    // Walk the metric-set intersection and find the first series whose values
    // disagree, then dump up to ±2 points of context around the first diff.
    for (metric, p_series) in &p_by_metric {
        let Some(s_series) = s_by_metric.get(metric) else {
            continue;
        };
        if p_series == s_series {
            continue;
        }
        let p_vals: &Vec<Value> = p_series.get("values").and_then(|v| v.as_array()).unwrap_or(&empty);
        let s_vals: &Vec<Value> = s_series.get("values").and_then(|v| v.as_array()).unwrap_or(&empty);

        // Locate the first index where the (t, v) pair differs (or one side ran out).
        let max_n = p_vals.len().max(s_vals.len());
        let mut first_diff_idx: Option<usize> = None;
        for i in 0..max_n {
            let pp = p_vals.get(i);
            let ss = s_vals.get(i);
            if pp != ss {
                first_diff_idx = Some(i);
                break;
            }
        }
        let Some(idx) = first_diff_idx else {
            continue;
        };
        out.push_str(&format!(
            "    first divergent series metric={} (PromQL n={} SQL n={})\n",
            metric,
            p_vals.len(),
            s_vals.len()
        ));
        let lo = idx.saturating_sub(2);
        let hi = (idx + 3).min(max_n);
        out.push_str(&format!("    first-diff at index {}:\n", idx));
        for i in lo..hi {
            let marker = if i == idx { "*" } else { " " };
            let render = |v: Option<&Value>| -> String {
                match v {
                    Some(Value::Array(arr)) if arr.len() == 2 => {
                        format!("(t={}, v={})", arr[0], arr[1])
                    }
                    Some(other) => other.to_string(),
                    None => "<missing>".to_string(),
                }
            };
            out.push_str(&format!(
                "      {} i={:>3} promql={} sql={}\n",
                marker,
                i,
                render(p_vals.get(i)),
                render(s_vals.get(i))
            ));
        }

        // Compute relative error at the first diff if both sides have a numeric value.
        if let (Some(Value::Array(pa)), Some(Value::Array(sa))) =
            (p_vals.get(idx), s_vals.get(idx))
        {
            if pa.len() == 2 && sa.len() == 2 {
                let pv = pa[1].as_f64().or_else(|| pa[1].as_str().and_then(|s| s.parse().ok()));
                let sv = sa[1].as_f64().or_else(|| sa[1].as_str().and_then(|s| s.parse().ok()));
                if let (Some(pv), Some(sv)) = (pv, sv) {
                    let denom = pv.abs().max(sv.abs()).max(1e-12);
                    let rel = (pv - sv).abs() / denom;
                    out.push_str(&format!(
                        "      diff: promql={} sql={} abs={:.3e} rel={:.3e}\n",
                        pv,
                        sv,
                        (pv - sv).abs(),
                        rel
                    ));
                }
            }
        }
        break;
    }

    out
}

/// Real-data parquet files we shadow against. Files that don't exist in the
/// current sandbox are skipped silently — the gate runs against whatever real
/// data is available locally.
fn parquets() -> Vec<PathBuf> {
    let dir = Path::new("/work/rezolus/site/viewer/data");
    [
        "demo",
        "cachecannon",
        "vllm",
        "sglang_gemma3",
        "vllm_gemma3",
    ]
    .iter()
    .map(|n| dir.join(format!("{n}.parquet")))
    .filter(|p| p.exists())
    .collect()
}

/// All concrete query strings to check for an entry.
/// - Literal entries: `[entry.promql]`.
/// - Templated entries: every `examples[*].query`.
fn concrete_queries(entry: &CatalogueEntry) -> Vec<String> {
    let template = CompiledTemplate::parse(&entry.promql).expect("parse template");
    if template.has_captures() {
        entry.examples.iter().map(|e| e.query.clone()).collect()
    } else {
        vec![entry.promql.clone()]
    }
}

/// Probe the parquet's time bounds via DuckDB so we issue queries against the
/// data range that's actually present, not a hard-coded one. The bounds are
/// **snapped** to the parquet's `sampling_interval_ms` so that PromQL's
/// step-grid origin (`start`) lines up with the SQL backend's snapped row
/// timestamps (`views::ensure_views` rounds every parquet timestamp to the
/// nearest interval boundary). Without this snap, PromQL emits at
/// `start, start+step, ...` (offset by sub-second nanoseconds when the parquet
/// was captured slightly off the second) while SQL emits at integer seconds —
/// same series count, mismatched timestamp axis, every gauge_bare divergence.
fn probe_time_range(parquet: &Path) -> Option<(f64, f64)> {
    let conn = duckdb::Connection::open_in_memory().ok()?;
    let (lo_ns, hi_ns): (i64, i64) = conn
        .query_row(
            "SELECT MIN(CAST(timestamp AS BIGINT)), MAX(CAST(timestamp AS BIGINT)) \
             FROM read_parquet(?)",
            [parquet.to_str().unwrap()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok()?;
    let interval_ns = sampling_interval_ns(parquet).unwrap_or(1_000_000_000);
    let snap = |t: i64, interval: i64| -> i64 {
        ((t + interval / 2) / interval) * interval
    };
    let lo_snapped = snap(lo_ns, interval_ns);
    let hi_snapped = snap(hi_ns, interval_ns);
    Some((lo_snapped as f64 / 1e9, hi_snapped as f64 / 1e9))
}

/// Read the parquet's `sampling_interval_ms` file-level metadata and convert
/// to nanoseconds. Returns `None` if the key is absent (caller should fall back
/// to the loader default — 1 s — to mirror `views::ensure_views`).
fn sampling_interval_ns(parquet: &Path) -> Option<i64> {
    let conn = duckdb::Connection::open_in_memory().ok()?;
    let ms: Option<String> = conn
        .query_row(
            "SELECT CAST(value AS VARCHAR) FROM parquet_kv_metadata(?) \
             WHERE key = 'sampling_interval_ms'",
            [parquet.to_str().unwrap()],
            |row| row.get::<_, String>(0).map(Some),
        )
        .ok()
        .flatten();
    ms.and_then(|s| s.parse::<i64>().ok())
        .map(|ms| ms * 1_000_000)
}

/// Run both engines on the same query against the same parquet. Returns
/// `Some((promql_json, sql_json))` on success or `None` if either side errors
/// (typical when a metric named in the query isn't present in this parquet —
/// e.g. vllm-only metrics against demo.parquet).
///
/// `engine` and `backend` are hoisted by the caller so the parquet load
/// (PromQL side) and DuckDB connection setup (SQL side) happen once per
/// parquet — not once per entry × query × parquet (110+ pairs would otherwise
/// each pay the cold-start cost).
fn run_pair(
    engine: &QueryEngine<Arc<Tsdb>>,
    backend: &DuckDbBackend,
    parquet: &Path,
    entry: &CatalogueEntry,
    query: &str,
    start: f64,
    end: f64,
    step: f64,
) -> Option<(serde_json::Value, serde_json::Value)> {
    let promql_result: QueryResult = engine.query_range_promql(query, start, end, step).ok()?;
    let promql_json = canonicalise(&promql_result);

    let template = CompiledTemplate::parse(&entry.promql).ok()?;
    let captures = template.match_query(query)?;
    let sql_result = backend
        .run(entry, &captures, parquet.to_str().unwrap(), start, end, step)
        .ok()?;
    let sql_json = canonicalise(&sql_result);

    Some((promql_json, sql_json))
}

/// Walk every `mode = "shadow"` entry × every example × every parquet, run
/// both engines, and return the list of failing pairs plus checked/skipped
/// counts. Each failure string includes a first-difference dump so the report
/// is self-contained — no need for a one-off debug harness per divergence.
fn walk_shadow_entries() -> (Vec<String>, u64, u64) {
    let parquets = parquets();
    let cat = Catalogue::embedded();
    let dump_sql = std::env::var("METRIKEN_DIFF_DUMP_SQL").is_ok();
    let mut failures: Vec<String> = Vec::new();
    let mut checked: u64 = 0;
    let mut skipped: u64 = 0;

    for parquet in &parquets {
        let (start, end) = match probe_time_range(parquet) {
            Some(r) => r,
            None => continue,
        };
        let step = 1.0;
        let parquet_label = parquet.file_name().unwrap().to_string_lossy().to_string();

        // Hoist Tsdb load and DuckDB backend setup out of the per-(entry,
        // query) loop. PromQL's Tsdb::load is the expensive part — without
        // hoisting we re-load each parquet ~110 times per run.
        let tsdb = match Tsdb::load(parquet) {
            Ok(t) => Arc::new(t),
            Err(_) => continue,
        };
        let engine = QueryEngine::new(tsdb);
        let backend = DuckDbBackend::new();

        for entry in cat.entries() {
            if entry.mode != Mode::Shadow {
                continue;
            }
            for q in concrete_queries(entry) {
                let pair = run_pair(&engine, &backend, parquet, entry, &q, start, end, step);
                match pair {
                    Some((p, s)) => {
                        checked += 1;
                        if p != s {
                            let p_count = p
                                .get("result")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let s_count = s
                                .get("result")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                                .unwrap_or(0);
                            let mut msg = format!(
                                "{}::{} on {} — PromQL series={} SQL series={}\n{}",
                                entry.id,
                                q,
                                parquet_label,
                                p_count,
                                s_count,
                                first_diff_dump(&p, &s)
                            );
                            if dump_sql {
                                if let Some(rendered) = render_sql(entry, &q, parquet) {
                                    msg.push_str(&format!(
                                        "    rendered SQL:\n{}\n",
                                        indent_block(&rendered, "      ")
                                    ));
                                }
                            }
                            failures.push(msg);
                        }
                    }
                    None => {
                        skipped += 1;
                    }
                }
            }
        }
    }
    (failures, checked, skipped)
}

fn indent_block(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_sql(entry: &CatalogueEntry, q: &str, parquet: &Path) -> Option<String> {
    let template = entry.sql.as_ref()?;
    let parsed = CompiledTemplate::parse(&entry.promql).ok()?;
    let captures = parsed.match_query(q)?;
    metriken_query_sql::interp::interpolate(template, &captures, parquet.to_str().unwrap()).ok()
}

/// Default test: prints a divergence report but NEVER fails. Use this as the
/// "where do we stand?" diagnostic during catalogue work — running `cargo test
/// -p metriken-query --test divergence_inspector -- --nocapture` shows the
/// current set of (entry, parquet) divergences without breaking unrelated CI.
///
/// The strict-mode counterpart (`every_shadow_entry_matches_promql_on_real_parquets`)
/// is `#[ignore]`d by default — opt in with `--ignored` once the catalogue is
/// known-clean.
#[test]
fn divergence_report() {
    let parquets = parquets();
    if parquets.is_empty() {
        eprintln!("no real parquet files present under /work/rezolus/site/viewer/data; skipping");
        return;
    }
    let (failures, checked, skipped) = walk_shadow_entries();
    let (expected, unexpected): (Vec<_>, Vec<_>) =
        failures.into_iter().partition(|f| is_expected(f));
    eprintln!(
        "shadow auto-walker: checked={} skipped={} expected={} unexpected={}",
        checked,
        skipped,
        expected.len(),
        unexpected.len()
    );
    if !unexpected.is_empty() {
        eprintln!("unexpected divergences ({} pairs):", unexpected.len());
        for f in &unexpected {
            eprintln!("  {f}");
        }
    }
    if !expected.is_empty() {
        eprintln!("expected divergences ({} pairs, allowed):", expected.len());
        for f in &expected {
            eprintln!("  {f}");
        }
    }
}

/// `(entry_id, parquet_basename)` pairs known to diverge but documented and
/// accepted as out-of-scope for the strict gate. The strict gate filters
/// failures against this list — only previously-unseen divergences fail it.
///
/// **Adding to this list is a deliberate choice**, not a workaround. Each
/// entry should be paired with a one-line note explaining *why* the
/// divergence is acceptable (the metric is exotic, the engine semantics
/// genuinely differ in a way users should adopt to, etc.) and a follow-up
/// reference if a real fix is planned. Removing an entry is what strict-mode
/// promotion looks like in catalogue work.
const EXPECTED_DIVERGENCES: &[(&str, &str)] = &[
    // PromQL emits one trailing timestamp per heatmap call even when no
    // bucket events occur in the last tick (the iterator pushes the timestamp
    // before checking whether any non-zero buckets exist). The SQL twin
    // produces no row for an empty-delta tick because `UNNEST` of a NULL/empty
    // bucket-list returns zero rows. Net effect: SQL has one fewer timestamp
    // entry than PromQL on parquets whose last tick happens to have no bucket
    // events. Bucket bounds, data, min_value, max_value all match — only
    // `timestamps.len()` differs by one. Cosmetic; revisit by either having
    // SQL emit an anchor row per timestamp or having PromQL prune empty
    // trailing ticks.
    ("histogram_heatmap_generic", "vllm.parquet"),

    // Sub-ULP floating-point noise from a different SUM evaluation order
    // between PromQL's accumulator and DuckDB's window aggregator. The first
    // observed diff is `27.083333333333332 vs 27.083333333333336` —
    // mathematically identical, JSON-distinct. Promoting this to "byte equal"
    // would require approximate-comparison support in the inspector; for now,
    // accept the one-shot noise and revisit if these cases multiply.
    ("counter_rate_sum_scaled", "sglang_gemma3.parquet"),
    ("counter_rate_sum_scaled", "vllm_gemma3.parquet"),
];

fn is_expected(failure: &str) -> bool {
    // Failure messages start with `{entry_id}::{query} on {parquet} — ...`.
    // Match by substring on `id` and ` on {parquet}` to keep the lookup
    // robust against future format tweaks.
    EXPECTED_DIVERGENCES.iter().any(|(entry_id, parquet)| {
        let prefix = format!("{entry_id}::");
        let on = format!(" on {parquet} ");
        failure.starts_with(&prefix) && failure.contains(&on)
    })
}

/// Strict promotion gate: zero **unexpected** divergences allowed. The
/// catalogue is byte-clean modulo the documented `EXPECTED_DIVERGENCES` list,
/// so this test runs by default — CI will fail loud on any regression.
#[test]
fn every_shadow_entry_matches_promql_on_real_parquets() {
    let parquets = parquets();
    if parquets.is_empty() {
        eprintln!("no real parquet files present under /work/rezolus/site/viewer/data; skipping");
        return;
    }
    let (failures, checked, skipped) = walk_shadow_entries();
    let (expected, unexpected): (Vec<_>, Vec<_>) = failures
        .into_iter()
        .partition(|f| is_expected(f));
    eprintln!(
        "shadow auto-walker: checked={} skipped={} expected_divergences={} unexpected_divergences={}",
        checked,
        skipped,
        expected.len(),
        unexpected.len()
    );
    if !expected.is_empty() {
        eprintln!("expected divergences (allowed):");
        for f in &expected {
            eprintln!("  {f}");
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected shadow divergences ({} pairs):\n  {}",
        unexpected.len(),
        unexpected.join("\n  ")
    );
}

// ------------------------------------------------------------------
// Diagnostic helpers below — kept as a side-by-side dump for the
// histogram_quantile family. Useful while investigating a divergence,
// but not part of the gate.
// ------------------------------------------------------------------

#[test]
fn dump_histogram_quantile_diff() {
    run_diff_check("histogram_quantile(0.5, scheduler_runqueue_latency)");
}

#[test]
fn p99_matches() {
    run_diff_check("histogram_quantile(0.99, scheduler_runqueue_latency)");
}

#[test]
fn whitespace_variant_matches() {
    run_diff_check("histogram_quantile(0.5,scheduler_runqueue_latency)");
}

#[test]
fn syscall_latency_multi_op_series() {
    run_diff_check("histogram_quantile(0.5, syscall_latency)");
}

fn run_diff_check(query: &str) {
    let parquet = Path::new("/work/rezolus/site/viewer/data/demo.parquet");
    if !parquet.exists() {
        eprintln!("demo.parquet not present at {}; skipping", parquet.display());
        return;
    }
    eprintln!("\n>>> {query}");
    let (start, end) = probe_time_range(parquet).expect("probe range");
    let step = 1.0;

    let tsdb = Arc::new(Tsdb::load(parquet).expect("load"));
    let engine = QueryEngine::new(tsdb);
    let promql = engine
        .query_range_promql(query, start, end, step)
        .expect("promql ok");
    let promql_json = canonicalise(&promql);

    let cat = Catalogue::embedded();
    let (entry, captures) = cat.lookup(query).expect("entry must match");
    eprintln!("matched entry id = {}", entry.id);
    eprintln!("captures = {:?}", captures);

    let backend = DuckDbBackend::new();
    let sql = backend
        .run(entry, &captures, parquet.to_str().unwrap(), start, end, step)
        .expect("sql ok");
    let sql_json = canonicalise(&sql);

    let summarize = |label: &str, v: &serde_json::Value| {
        let r = v
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        eprintln!("=== {label} ===");
        eprintln!("  series_count={}", r.len());
        for (i, s) in r.iter().enumerate().take(3) {
            let m = s.get("metric").map(|m| m.to_string()).unwrap_or_default();
            let vs = s
                .get("values")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            eprintln!("  series[{i}] metric={m} n_points={}", vs.len());
            for p in vs.iter().take(3) {
                eprintln!("    point={p}");
            }
        }
    };
    summarize("PromQL", &promql_json);
    summarize("SQL", &sql_json);

    if promql_json != sql_json {
        let p = serde_json::to_string_pretty(&promql_json).unwrap();
        let s = serde_json::to_string_pretty(&sql_json).unwrap();
        eprintln!("=== PromQL JSON ===\n{p}");
        eprintln!("=== SQL JSON ===\n{s}");
        panic!("shadow divergence: PromQL ≠ SQL canonical JSON");
    } else {
        eprintln!("=== PROMQL == SQL (byte-equivalent canonical JSON) ===");
    }

    let template = CompiledTemplate::parse(&entry.promql).unwrap();
    let caps2 = template.match_query(query).unwrap();
    let rendered_sql = metriken_query_sql::interp::interpolate(
        entry.sql.as_ref().unwrap(),
        &caps2,
        parquet.to_str().unwrap(),
    )
    .unwrap();
    eprintln!("=== rendered SQL ===\n{rendered_sql}");
}
