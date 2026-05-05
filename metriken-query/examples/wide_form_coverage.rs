//! Coverage check: for every catalogue entry, run `translate::try_generate`
//! against a real production parquet and report which entries emit
//! wide-form SQL vs fall through. Intended as a sanity gate that the
//! catalogue stays fully covered by the wide-form generator.
//!
//! Usage: `cargo run -p metriken-query --release --example wide_form_coverage`

use std::path::PathBuf;

use metriken_query::{Catalogue, CompiledTemplate};

fn main() {
    let parquet: PathBuf = "/work/rezolus/site/viewer/data/demo.parquet".into();
    if !parquet.exists() {
        eprintln!("parquet not found: {}", parquet.display());
        std::process::exit(2);
    }

    let backend = metriken_query_sql::DuckDbBackend::with_pool_size(1);
    let catalog = backend.describe_parquet(parquet.to_str().unwrap()).unwrap();

    let cat = Catalogue::embedded();
    let mut covered = 0usize;
    let mut uncovered: Vec<(String, String)> = Vec::new();

    for entry in cat.entries() {
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
                uncovered.push((
                    entry.id.clone(),
                    format!("template did not match its own example: {query}"),
                ));
                continue;
            }
        };
        match metriken_query::translate::try_generate(entry, &captures, &catalog) {
            Some(_) => covered += 1,
            None => uncovered.push((entry.id.clone(), query)),
        }
    }

    println!("\nWide-form coverage on {}", parquet.display());
    println!("  total entries:          {}", covered + uncovered.len());
    println!("  wide-form covered:      {}", covered);
    println!("  fall through:           {}", uncovered.len());
    if !uncovered.is_empty() {
        println!("\nEntries with no wide-form generator:");
        for (id, q) in &uncovered {
            println!("  {id} :: {q}");
        }
    }
}
