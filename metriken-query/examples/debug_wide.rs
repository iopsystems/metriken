//! Print the generated wide-form SQL for one (entry, query) pair.
//! Used to debug divergences from the long-form path.

use std::path::PathBuf;
use std::sync::Arc;

use metriken_query::{Catalogue, QueryEngine, SqlBackend, Tsdb};
use metriken_query_sql::{wide_form, DuckDbBackend};

fn main() {
    let parquet: PathBuf = std::env::args()
        .nth(1)
        .expect("arg1 = parquet path")
        .into();
    let query = std::env::args().nth(2).expect("arg2 = query");

    let tsdb = Arc::new(Tsdb::load(&parquet).unwrap());
    let (start_ns, end_ns) = tsdb.time_range().unwrap();
    let start = start_ns as f64 / 1e9;
    let end = end_ns as f64 / 1e9;
    let step = ((end - start) / 500.0).floor().max(1.0);
    let cat = Catalogue::embedded();
    let (entry, captures) = cat.lookup(&query).expect("no catalogue match");
    eprintln!("entry id: {}", entry.id);

    // Build catalog (mirroring backend's get_or_init)
    let conn = duckdb::Connection::open_in_memory().unwrap();
    metriken_query_sql::register_all(&conn).unwrap();
    let catalog = metriken_query_sql::views::ensure_views(
        &conn,
        parquet.to_str().unwrap(),
    )
    .unwrap();

    let sql = wide_form::try_generate(entry, &captures, &catalog);
    match sql {
        Some(s) => {
            println!("=== generated wide-form SQL ===");
            println!("{}", s);
        }
        None => {
            println!("(wide-form declined — would fall back to long-form)");
        }
    }

    // Also run both engines and compare results
    let engine = QueryEngine::new(tsdb.clone());
    let backend = DuckDbBackend::new();

    let p_res = engine.query_range_promql(&query, start, end, step);
    let s_res = backend.run(entry, &captures, parquet.to_str().unwrap(), start, end, step);

    eprintln!("\n=== PromQL result ===");
    eprintln!("{:#?}", p_res);
    eprintln!("\n=== SQL result ===");
    eprintln!("{:#?}", s_res);
}
