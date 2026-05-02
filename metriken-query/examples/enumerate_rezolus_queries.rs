//! Enumerate every PromQL query the Rezolus viewer issues, classify each
//! against the embedded catalogue, and report the gap.
//!
//! Two extractors:
//!   1. Dashboard plot specs — pre-rendered JSON dumps from the Rezolus
//!      `dashboard` crate. Run `cargo run -p dashboard -- /tmp/dashboard_json`
//!      first; the resulting per-section JSON files contain a `promql_query`
//!      field on every plot.
//!   2. Service-extension KPI templates — `/work/rezolus/config/templates/*.json`,
//!      with `kpis[].query` fields.
//!
//! Both are pure JSON walks — this binary does not link the rezolus dashboard
//! crate (which depends on metriken-query). It just reads the artefacts.
//!
//! Output:
//!   - `production_queries.txt`: one query per line, sorted, deduped (stable)
//!   - When `--classify` is set: per-query catalogue match status, plus a
//!     summary of hit/miss rates by inferred shape.
//!
//! Usage:
//!   cargo run --example enumerate_rezolus_queries
//!   cargo run --example enumerate_rezolus_queries -- --classify
//!   cargo run --example enumerate_rezolus_queries -- --out tests/data/production_queries.txt

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use metriken_query::Catalogue;
use serde_json::Value;

const DEFAULT_DASHBOARD_DIR: &str = "/tmp/dashboard_json";
const DEFAULT_TEMPLATES_DIR: &str = "/work/rezolus/config/templates";
const DEFAULT_OUT: &str = "metriken-query/tests/data/production_queries.txt";

#[derive(Default)]
struct Args {
    dashboard_dir: Option<PathBuf>,
    templates_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    classify: bool,
    miss_only: bool,
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--dashboard-dir" => {
                args.dashboard_dir = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--templates-dir" => {
                args.templates_dir = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--out" => {
                args.out = Some(PathBuf::from(&raw[i + 1]));
                i += 2;
            }
            "--classify" => {
                args.classify = true;
                i += 1;
            }
            "--miss-only" => {
                args.miss_only = true;
                args.classify = true;
                i += 1;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: enumerate_rezolus_queries \
[--dashboard-dir DIR] [--templates-dir DIR] [--out FILE] [--classify] [--miss-only]\n\
Defaults: dashboard-dir={DEFAULT_DASHBOARD_DIR}, templates-dir={DEFAULT_TEMPLATES_DIR}, out={DEFAULT_OUT}"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn collect_promql_query_fields(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "promql_query" {
                    if let Value::String(s) = v {
                        if !s.is_empty() {
                            out.insert(s.clone());
                        }
                    }
                } else {
                    collect_promql_query_fields(v, out);
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_promql_query_fields(v, out);
            }
        }
        _ => {}
    }
}

fn collect_kpi_queries(value: &Value, out: &mut BTreeSet<String>) {
    if let Value::Object(map) = value {
        if let Some(Value::Array(kpis)) = map.get("kpis") {
            for k in kpis {
                if let Some(Value::String(q)) = k.get("query") {
                    if !q.is_empty() {
                        out.insert(q.clone());
                    }
                }
            }
        }
    }
}

fn read_dir_jsons(dir: &PathBuf) -> Vec<(String, Value)> {
    let mut out = vec![];
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            eprintln!("note: cannot read {}: {e}", dir.display());
            return out;
        }
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for ent in entries {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("note: cannot read {}: {e}", path.display());
                continue;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(v) => {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                out.push((name, v));
            }
            Err(e) => {
                eprintln!("note: cannot parse {}: {e}", path.display());
            }
        }
    }
    out
}

/// Group a query by its broad shape so the gap report is readable.
fn shape_of(q: &str) -> &'static str {
    let q = q.trim();
    if q.starts_with("histogram_heatmap(") {
        "histogram_heatmap"
    } else if q.starts_with("histogram_quantiles(") {
        "histogram_quantiles"
    } else if q.starts_with("histogram_quantile(") {
        "histogram_quantile"
    } else if q.starts_with("avg_over_time(") {
        "avg_over_time"
    } else if q.starts_with("rate(") {
        "rate_bare"
    } else if q.starts_with("irate(") {
        "irate_bare"
    } else if q.contains("histogram_quantile(") {
        "histogram_quantile_arith"
    } else if q.contains("irate(") || q.contains("rate(") {
        if q.contains(" / ") {
            "rate_arith"
        } else if q.contains("sum by") {
            "rate_sum_by"
        } else {
            "rate_sum"
        }
    } else if q.contains(" / ") || q.contains(" * ") || q.contains(" - ") || q.contains(" + ") {
        "gauge_arith"
    } else {
        "bare_selector"
    }
}

fn main() -> std::io::Result<()> {
    let args = parse_args();
    let dashboard_dir = args
        .dashboard_dir
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DASHBOARD_DIR));
    let templates_dir = args
        .templates_dir
        .unwrap_or_else(|| PathBuf::from(DEFAULT_TEMPLATES_DIR));
    let out_path = args.out.unwrap_or_else(|| PathBuf::from(DEFAULT_OUT));

    let mut all = BTreeSet::new();

    let dashboard_files = read_dir_jsons(&dashboard_dir);
    if dashboard_files.is_empty() {
        eprintln!(
            "warning: no dashboard JSON found under {}.\n  Run `cargo run -p dashboard -- {}` first.",
            dashboard_dir.display(),
            dashboard_dir.display()
        );
    }
    let mut dashboard_count = 0;
    for (_name, v) in &dashboard_files {
        let before = all.len();
        collect_promql_query_fields(v, &mut all);
        dashboard_count += all.len() - before;
    }

    let template_files = read_dir_jsons(&templates_dir);
    let mut template_count = 0;
    for (_name, v) in &template_files {
        let before = all.len();
        collect_kpi_queries(v, &mut all);
        template_count += all.len() - before;
    }

    eprintln!(
        "found {} unique queries ({} dashboard + {} service templates; overlap {})",
        all.len(),
        dashboard_count,
        template_count,
        dashboard_count + template_count - all.len()
    );

    if !args.classify {
        // Plain dump: write to file or stdout.
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let body = all.iter().cloned().collect::<Vec<_>>().join("\n") + "\n";
        std::fs::write(&out_path, body)?;
        eprintln!("wrote {} queries to {}", all.len(), out_path.display());
        return Ok(());
    }

    // Classify against the embedded catalogue.
    let cat = Catalogue::embedded();

    let mut hit_by_shape: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut miss_by_shape: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut hits_by_id: BTreeMap<String, usize> = BTreeMap::new();
    let mut misses: Vec<String> = vec![];

    for q in &all {
        let shape = shape_of(q);
        match cat.lookup(q) {
            Some((entry, _)) => {
                *hit_by_shape.entry(shape).or_default() += 1;
                *hits_by_id.entry(entry.id.clone()).or_default() += 1;
            }
            None => {
                *miss_by_shape.entry(shape).or_default() += 1;
                misses.push(q.clone());
            }
        }
    }

    println!("# catalogue gap report\n");
    println!("Total queries: {}", all.len());
    println!(
        "Hits: {}  Misses: {}\n",
        all.len() - misses.len(),
        misses.len()
    );

    println!("## Hits by catalogue entry id");
    let mut hits_sorted: Vec<_> = hits_by_id.iter().collect();
    hits_sorted.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (id, n) in &hits_sorted {
        println!("  {n:>4}  {id}");
    }
    println!();

    println!("## Hits/misses by shape");
    let all_shapes: BTreeSet<_> = hit_by_shape.keys().chain(miss_by_shape.keys()).collect();
    let mut shape_rows: Vec<_> = all_shapes
        .iter()
        .map(|s| {
            let h = *hit_by_shape.get(*s).unwrap_or(&0);
            let m = *miss_by_shape.get(*s).unwrap_or(&0);
            (h + m, *s, h, m)
        })
        .collect();
    shape_rows.sort_by(|a, b| b.0.cmp(&a.0));
    let _ = all_shapes;
    println!("  {:>5} / {:>5}  {}", "hit", "miss", "shape");
    for (_, shape, h, m) in shape_rows {
        println!("  {h:>5} / {m:>5}  {shape}");
    }
    println!();

    if !args.miss_only {
        println!("## Sample misses (first 30)");
        for q in misses.iter().take(30) {
            println!("  [{}] {}", shape_of(q), q);
        }
    } else {
        println!("## All misses ({})", misses.len());
        for q in &misses {
            println!("  [{}] {}", shape_of(q), q);
        }
    }

    Ok(())
}
