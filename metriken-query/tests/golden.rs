//! Snapshot-based regression harness for the metriken-query → DuckDB migration.
//!
//! For each `(fixture, query)` pair declared in `metriken-query/queries.toml`,
//! this test loads the fixture, runs the PromQL query, and snapshots the JSON
//! `QueryResult` via `insta`. Update goldens with `cargo insta accept` after
//! a deliberate change. The same goldens become the contract for the future
//! DuckDB-backed engine.
//!
//! The catalogue file is shared with the runtime dispatcher (Phase 2.3): at
//! runtime it routes incoming PromQL to the SQL twin per its `mode`; at test
//! time it iterates the same entries and snapshots the PromQL output.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metriken_query::{
    Catalogue, CatalogueEntry, CompiledTemplate, Mode, QueryEngine, QueryResult, SqlBackend, Tsdb,
};
use metriken_query_sql::DuckDbBackend;
use serde_json::{Map, Value};

/// Engine output isn't guaranteed-stable across runs because internal
/// `HashMap`s leak their randomized iteration order both into the outer
/// series Vec ordering (`sum by (id) (...)`) and into individual label
/// maps. Canonicalize before snapshotting:
///
///   1. Re-serialize through `serde_json::Value` and recursively rebuild
///      every `Object` with keys in sorted order (so `__name__` always
///      precedes `quantile` etc.).
///   2. Sort any array of objects by the JSON-serialized form of each
///      element's `metric` field, so series come out in a stable order.
fn canonicalize(result: &QueryResult) -> Value {
    let value = serde_json::to_value(result).expect("serialize QueryResult");
    let mut canon = sort_keys(value);
    sort_series_arrays(&mut canon);
    canon
}

fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut out: Map<String, Value> = Map::new();
            let mut entries: Vec<_> = obj.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, v) in entries {
                out.insert(k, sort_keys(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Sort the `result` array (Vector / Matrix outputs) by the canonical JSON
/// form of each element's `metric` field. Arrays nested elsewhere in the
/// QueryResult shape (e.g. heatmap data triples) are left alone.
fn sort_series_arrays(value: &mut Value) {
    if let Value::Object(obj) = value {
        if let Some(Value::Array(arr)) = obj.get_mut("result") {
            if arr.iter().all(|e| {
                e.as_object()
                    .map(|o| o.contains_key("metric"))
                    .unwrap_or(false)
            }) {
                arr.sort_by(|a, b| {
                    let ka = a.get("metric").map(|m| m.to_string()).unwrap_or_default();
                    let kb = b.get("metric").map(|m| m.to_string()).unwrap_or_default();
                    ka.cmp(&kb)
                });
            }
        }
    }
}


fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("metriken-query-fixtures")
        .join("fixtures")
}

fn fixture_path(entry: &CatalogueEntry) -> PathBuf {
    let name = entry
        .fixture
        .as_deref()
        .unwrap_or_else(|| panic!("entry {} has no fixture", entry.id));
    let p = fixtures_dir().join(format!("{name}.parquet"));
    assert!(
        p.exists(),
        "fixture {name} not found at {} — run `cargo run -p metriken-query-fixtures --bin generate`",
        p.display()
    );
    p
}

fn run_promql(entry: &CatalogueEntry, query: &str) -> QueryResult {
    let parquet = fixture_path(entry);
    let tsdb = Arc::new(Tsdb::load(&parquet).expect("load fixture"));
    let engine = QueryEngine::new(tsdb);
    let start = entry.start.expect("entry start");
    let end = entry.end.expect("entry end");
    let step = entry.step.expect("entry step");
    engine
        // `query_range_promql` bypasses the dispatcher — even if mode = primary
        // we still want the PromQL output for snapshot anchoring.
        .query_range_promql(query, start, end, step)
        .unwrap_or_else(|e| panic!("PromQL query `{query}` ({}) failed: {e}", entry.id))
}

/// Run the SQL twin via the production `DuckDbBackend` — same code path the
/// runtime dispatcher uses. Returns `None` when the entry has no SQL.
/// `query` is the concrete PromQL (template literal for literal entries, or
/// the example query for templated entries) used to extract captures.
fn run_sql(entry: &CatalogueEntry, query: &str) -> Option<QueryResult> {
    entry.sql.as_ref()?;
    let parquet = fixture_path(entry);
    let backend = DuckDbBackend::new();
    let start = entry.start.expect("entry start");
    let end = entry.end.expect("entry end");
    let step = entry.step.expect("entry step");
    let template = CompiledTemplate::parse(&entry.promql).expect("compile template");
    let captures = template
        .match_query(query)
        .unwrap_or_else(|| panic!("entry {} template did not match `{query}`", entry.id));
    let result = backend
        .run(entry, &captures, parquet.to_str().unwrap(), start, end, step)
        .unwrap_or_else(|e| panic!("SQL twin for {} (`{query}`) failed: {e}", entry.id));
    Some(result)
}

/// Concrete (snapshot_name, query) pairs to exercise for a catalogue entry.
/// Literal-only entries produce one pair: (entry.id, entry.promql). Templated
/// entries with `examples = [...]` produce one pair per example, named
/// `{entry.id}_{example.id_suffix}`.
fn snapshot_targets(entry: &CatalogueEntry) -> Vec<(String, String)> {
    if entry.examples.is_empty() {
        vec![(entry.id.clone(), entry.promql.clone())]
    } else {
        entry
            .examples
            .iter()
            .map(|ex| (format!("{}_{}", entry.id, ex.id_suffix), ex.query.clone()))
            .collect()
    }
}

#[test]
fn golden_canonical_queries() {
    let catalogue = Catalogue::embedded();
    let mut settings = insta::Settings::clone_current();
    settings.set_snapshot_path("golden");
    settings.set_prepend_module_to_snapshot(false);
    settings.set_omit_expression(true);

    for entry in catalogue.entries() {
        for (snapshot_name, query) in snapshot_targets(entry) {
            // PromQL pass — always runs, anchors the golden.
            let promql = canonicalize(&run_promql(entry, &query));
            settings.bind(|| {
                insta::assert_json_snapshot!(snapshot_name.clone(), promql);
            });

            // SQL pass — when the entry has a SQL twin, project via the
            // production backend and snapshot against the SAME golden file.
            // If the canonical forms diverge, this assertion fails with a diff.
            // Skip when mode = off (the catalogue's "explicitly disabled" state).
            if entry.mode == Mode::Off {
                continue;
            }
            if let Some(sql_result) = run_sql(entry, &query) {
                let sql_canon = canonicalize(&sql_result);
                settings.bind(|| {
                    insta::assert_json_snapshot!(snapshot_name.clone(), sql_canon);
                });
            }
        }
    }
}

