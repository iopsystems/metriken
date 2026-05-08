//! PromQL-only harness for cross-branch correctness validation.
//!
//! For each plot in the dashboard JSON dump and each demo parquet, run
//! the plot's `promql_query` through `metriken_query::QueryEngine`
//! (legacy in-memory PromQL evaluator) and dump the result as JSON.
//!
//! This file is intentionally PromQL-only so it compiles against both
//! main's `metriken-query` (default features) and yv/sql-testing's
//! `metriken-query --features legacy` — letting us run the same plots
//! on both branches and diff the outputs.
//!
//! Drop into `metriken-query/examples/` on each branch.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use metriken_query::{QueryEngine, Tsdb};

const USAGE: &str = "Usage: promql_only --dashboard-dir DIR --parquets P1 [P2 ...] --out DIR";

#[derive(Default, Debug)]
struct Args {
    dashboard_dir: PathBuf,
    parquets: Vec<PathBuf>,
    out: PathBuf,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args::default();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--dashboard-dir" => { a.dashboard_dir = PathBuf::from(&raw[i + 1]); i += 2; }
            "--parquets" => {
                i += 1;
                while i < raw.len() && !raw[i].starts_with("--") {
                    a.parquets.push(PathBuf::from(&raw[i])); i += 1;
                }
                continue;
            }
            "--out" => { a.out = PathBuf::from(&raw[i + 1]); i += 2; }
            _ => { eprintln!("unknown arg: {}\n{USAGE}", raw[i]); std::process::exit(2); }
        }
    }
    if a.dashboard_dir.as_os_str().is_empty() || a.parquets.is_empty()
        || a.out.as_os_str().is_empty() {
        eprintln!("{USAGE}"); std::process::exit(2);
    }
    a
}

#[derive(Debug)]
struct PlotSpec {
    id: String,
    section: String,
    plot_type: String,
    subtype: Option<String>,
    percentiles: Option<Vec<f64>>,
    promql: String,
}

const DEFAULT_PERCENTILES: &[f64] = &[0.5, 0.9, 0.99, 0.999, 0.9999];

fn effective_promql(plot: &PlotSpec) -> String {
    if plot.plot_type != "histogram" { return plot.promql.clone(); }
    if plot.subtype.as_deref() == Some("buckets") {
        return format!("histogram_heatmap({})", plot.promql);
    }
    let qs: Vec<String> = plot.percentiles.as_deref().unwrap_or(DEFAULT_PERCENTILES)
        .iter().map(|p| format!("{p}")).collect();
    format!("histogram_quantiles([{}], {})", qs.join(", "), plot.promql)
}

fn load_plots(dir: &Path) -> Vec<PlotSpec> {
    let mut out = vec![];
    let mut entries: Vec<_> = fs::read_dir(dir).expect("read dir")
        .filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for ent in entries {
        let path = ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        if name == "sections" { continue; }
        let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for g in v.get("groups").and_then(Value::as_array).iter().flat_map(|a| a.iter()) {
            collect_plots(g, &name, &mut out);
            for sg in g.get("subgroups").and_then(Value::as_array).iter().flat_map(|a| a.iter()) {
                collect_plots(sg, &name, &mut out);
            }
        }
    }
    out
}

fn collect_plots(node: &Value, section: &str, out: &mut Vec<PlotSpec>) {
    let plots = match node.get("plots").and_then(Value::as_array) { Some(p) => p, None => return };
    for p in plots {
        let promql = p.get("promql_query").and_then(Value::as_str).unwrap_or("").to_string();
        let id = p.get("opts").and_then(|o| o.get("id")).and_then(Value::as_str).unwrap_or("").to_string();
        let plot_type = p.get("opts").and_then(|o| o.get("type")).and_then(Value::as_str).unwrap_or("").to_string();
        let subtype = p.get("opts").and_then(|o| o.get("subtype")).and_then(Value::as_str).map(String::from);
        let percentiles = p.get("opts").and_then(|o| o.get("percentiles")).and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>());
        if id.is_empty() || promql.is_empty() { continue; }
        out.push(PlotSpec { id, section: section.to_string(), plot_type, subtype, percentiles, promql });
    }
}

fn main() {
    let args = parse_args();
    fs::create_dir_all(&args.out).expect("mkdir out");
    let started = Instant::now();
    let plots = load_plots(&args.dashboard_dir);
    eprintln!("loaded {} plots", plots.len());

    let mut all = BTreeMap::<String, BTreeMap<String, Value>>::new();
    for parquet in &args.parquets {
        let stem = parquet.file_stem().unwrap().to_string_lossy().into_owned();
        eprintln!("=== {stem} ===");
        let t0 = Instant::now();
        let tsdb = Arc::new(Tsdb::load(parquet).expect("Tsdb::load"));
        eprintln!("  loaded in {:.2}s", t0.elapsed().as_secs_f64());
        let engine = QueryEngine::new(tsdb.clone());
        let (start, end) = engine.get_time_range();
        let dur = (end - start).min(3600.0).max(0.0);
        let step = (dur / 500.0).floor().max(1.0);
        let win_start = (end - 3600.0).max(start);
        eprintln!("  win [{win_start:.0}, {end:.0}] step={step}");
        let mut per_parquet = BTreeMap::<String, Value>::new();
        let mut ok = 0; let mut errs = 0; let mut skipped = 0;
        for plot in &plots {
            if plot.promql.contains("__SELECTED_CGROUPS__") {
                skipped += 1;
                continue;
            }
            let q = effective_promql(plot);
            let result_or_err = engine.query_range(&q, win_start, end, step);
            let entry = match result_or_err {
                Ok(r) => { ok += 1; serde_json::json!({"result": serde_json::to_value(&r).unwrap()}) }
                Err(e) => { errs += 1; serde_json::json!({"error": e.to_string()}) }
            };
            per_parquet.insert(plot.id.clone(), entry);
        }
        eprintln!("  ok={ok} err={errs} skipped={skipped}");
        all.insert(stem, per_parquet);
    }
    let path = args.out.join("promql_results.json");
    fs::write(&path, serde_json::to_vec_pretty(&all).unwrap()).expect("write");
    eprintln!("wrote {} ({:.1}s)", path.display(), started.elapsed().as_secs_f64());

    // Also a per-(parquet, plot) file dump for easy diff'ing.
    for (stem, plots_map) in &all {
        let dir = args.out.join(stem);
        fs::create_dir_all(&dir).unwrap();
        for (pid, val) in plots_map {
            let safe: String = pid.chars().map(|c|
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }
            ).collect();
            fs::write(dir.join(format!("{safe}.json")), serde_json::to_vec_pretty(val).unwrap()).ok();
        }
    }
}
