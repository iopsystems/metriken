//! Print PromQL vs SQL values for a single query, finding the first
//! diverging point. Used to debug shadow-mode divergences.

use std::path::PathBuf;
use std::sync::Arc;

use metriken_query::{canonicalise, Catalogue, QueryEngine, SqlBackend, Tsdb};
use metriken_query_sql::DuckDbBackend;

fn main() {
    let parquet = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: probe_rate_diff <parquet> <query>"),
    );
    let query = std::env::args().nth(2).expect("query");

    let tsdb = Arc::new(Tsdb::load(&parquet).unwrap());
    let (s, e) = tsdb.time_range().unwrap();
    let start = s as f64 / 1e9;
    let end = e as f64 / 1e9;
    println!("time range: {} → {} ({}s)", start, end, end - start);

    let engine = QueryEngine::new(tsdb);
    let backend = DuckDbBackend::new();
    let cat = Catalogue::embedded();
    let (entry, caps) = cat.lookup(&query).expect("catalogue match");
    println!("matched entry: {}", entry.id);

    let p = engine.query_range_promql(&query, start, end, 1.0).unwrap();
    let s = backend
        .run(entry, &caps, parquet.to_str().unwrap(), start, end, 1.0)
        .unwrap();
    let pj = canonicalise(&p);
    let sj = canonicalise(&s);
    let p_arr = pj
        .get("result")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let s_arr = sj
        .get("result")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    println!("PromQL: {} series, SQL: {} series", p_arr.len(), s_arr.len());
    for (i, ps) in p_arr.iter().enumerate() {
        let p_metric = ps.get("metric").map(|m| m.to_string()).unwrap_or_default();
        let p_vals: Vec<&serde_json::Value> = ps
            .get("values")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let s_series = s_arr
            .iter()
            .find(|s| s.get("metric").map(|m| m.to_string()).unwrap_or_default() == p_metric);
        let s_vals: Vec<&serde_json::Value> = s_series
            .and_then(|s| s.get("values"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        println!(
            "series {} metric={} (PromQL n={} SQL n={})",
            i,
            p_metric,
            p_vals.len(),
            s_vals.len()
        );
        let n = p_vals.len().max(s_vals.len());
        let mut diffs = 0;
        for j in 0..n {
            let pv = p_vals.get(j);
            let sv = s_vals.get(j);
            if pv != sv {
                if diffs < 6 {
                    println!("  [{:>3}] PromQL: {:?}    SQL: {:?}", j, pv, sv);
                }
                diffs += 1;
            }
        }
        if diffs > 0 {
            println!("  total diffs in this series: {}", diffs);
        }
    }
}
