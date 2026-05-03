//! Coverage check: for every catalogue entry whose `mode != off`,
//! run `wide_form::try_generate` against a real production parquet
//! and report which entries emit wide-form SQL vs fall through to
//! long-form. Intended as the gate that confirms the long-form
//! fallback path is no longer reachable before deleting it.
//!
//! Usage: `cargo run -p metriken-query --release --example wide_form_coverage`
//!
//! Picks the smallest available production parquet for the catalog,
//! then walks every entry. For templated entries, uses the first
//! `examples[]` query; for literal entries, uses `entry.promql` as-is.

use std::path::PathBuf;

use metriken_query::{Catalogue, CompiledTemplate, Mode};

fn main() {
    let parquet: PathBuf = "/work/rezolus/site/viewer/data/demo.parquet".into();
    if !parquet.exists() {
        eprintln!("parquet not found: {}", parquet.display());
        std::process::exit(2);
    }

    let conn = duckdb::Connection::open_in_memory().unwrap();
    metriken_query_sql::register_all(&conn).unwrap();
    let catalog = metriken_query_sql::views::ensure_views(&conn, parquet.to_str().unwrap()).unwrap();

    let cat = Catalogue::embedded();
    let mut covered = 0usize;
    let mut uncovered: Vec<(String, String)> = Vec::new();
    let mut off = 0usize;

    for entry in cat.entries() {
        if entry.mode == Mode::Off {
            off += 1;
            continue;
        }
        // Use the first example for templated entries; the literal
        // promql for non-templated.
        let query = if entry.examples.is_empty() {
            entry.promql.clone()
        } else {
            entry.examples[0].query.clone()
        };
        let template = CompiledTemplate::parse(&entry.promql).unwrap();
        let captures = match template.match_query(&query) {
            Some(c) => c,
            None => {
                uncovered.push((entry.id.clone(), format!("template did not match its own example: {query}")));
                continue;
            }
        };
        match metriken_query_sql::wide_form::try_generate(entry, &captures, &catalog) {
            Some(_) => covered += 1,
            None => uncovered.push((entry.id.clone(), query)),
        }
    }

    println!("\nWide-form coverage on {}", parquet.display());
    println!("  shadow/primary entries: {}", covered + uncovered.len());
    println!("  off:                    {}", off);
    println!("  wide-form covered:      {}", covered);
    println!("  fall through:           {}", uncovered.len());
    if !uncovered.is_empty() {
        println!("\nEntries falling through to long-form:");
        for (id, q) in &uncovered {
            println!("  {id} :: {q}");
        }
    }
}
