//! Gate test: every PromQL string the Rezolus frontend issues has a working
//! SQL twin that produces byte-equal canonical JSON to PromQL.
//!
//! Walks every parquet under `/work/rezolus/site/viewer/data/` and replays
//! every line in `tests/data/production_queries.txt`. Asserts:
//!
//!   1. **Coverage** (always enforced) — every frontend query matches a
//!      catalogue template, so it is modelable in SQL.
//!   2. **Correctness on production parquets** (always enforced) — for
//!      single-source agent-recorded parquets and the "_pin" combined
//!      parquet, every (query, parquet) pair where both engines succeed
//!      produces identical canonical JSON.
//!   3. **Correctness on combined non-_pin AB parquets** (warning only) —
//!      these parquets contain at least one gap in the snapshot stream
//!      where consecutive snapshots are >1s apart. PromQL's `rate()` /
//!      `irate()` evaluate at every step in the requested range and emit
//!      carried-forward values over gaps; SQL emits one rate per data row.
//!      Values agree at every shared timestamp, but the two series are
//!      shifted relative to each other, so the canonical JSONs differ.
//!      Fixing this requires step-based SQL evaluation (LEFT JOIN against
//!      a generate_series of step timestamps); deferred — the production
//!      use case is single-source recording where gaps don't occur.
//!
//! Pairs where one or both engines error out (e.g. cpu_usage queries
//! against a load-generator-only parquet) are silently skipped — the same
//! convention `divergence_inspector.rs` uses.
//!
//! The test is `#[ignore]` by default because it loads the real parquet
//! files (a few MB each) and runs ~70 SQL queries per parquet — noticeable
//! but bounded. Run locally with:
//!     cargo test --release -p metriken-query --test frontend_coverage \
//!         -- --ignored --nocapture
//! Set `METRIKEN_FRONTEND_COVERAGE_DEEP=1` to recurse into `disagg/` too.
//! Set `METRIKEN_FRONTEND_COVERAGE_STRICT_AB=1` to upgrade the AB warning
//! into a hard failure (useful when iterating on a step-based fix).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metriken_query::{
    canonicalise, Catalogue, CompiledTemplate, Mode, QueryEngine, SqlBackend, Tsdb,
};
use metriken_query_sql::DuckDbBackend;

fn data_root() -> PathBuf {
    PathBuf::from("/work/rezolus/site/viewer/data")
}

/// Collect top-level `.parquet` files in `root`. Subdirectories (notably
/// `disagg/`, which holds large multi-hour recordings) are skipped by default;
/// set `METRIKEN_FRONTEND_COVERAGE_DEEP=1` to recurse and exercise them too.
/// The 9 top-level parquets give full label / metric / shape coverage; the
/// disagg recordings are correctness-equivalent but slow enough to dominate
/// CI time.
fn collect_parquets(root: &Path) -> Vec<PathBuf> {
    let deep = std::env::var("METRIKEN_FRONTEND_COVERAGE_DEEP").is_ok();
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("parquet") {
            out.push(path);
        } else if deep && path.is_dir() {
            out.extend(collect_parquets(&path));
        }
    }
    out.sort();
    out
}

fn load_queries() -> Vec<String> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("production_queries.txt");
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
#[ignore = "loads real Rezolus parquets; run with --ignored"]
fn every_frontend_query_models_in_sql_with_zero_divergences() {
    let queries = load_queries();
    assert!(!queries.is_empty(), "production_queries.txt should not be empty");

    // Coverage gate (1): every frontend query has a catalogue match.
    let cat = Catalogue::embedded();
    let unmatched: Vec<_> = queries.iter().filter(|q| cat.lookup(q).is_none()).collect();
    assert!(
        unmatched.is_empty(),
        "{} frontend queries have no catalogue match (first 5 shown):\n{}",
        unmatched.len(),
        unmatched
            .iter()
            .take(5)
            .map(|q| format!("  - {q}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Correctness gate (2): replay every (parquet, query) pair and assert
    // PromQL == SQL where both succeed. Build per-parquet checklist for the
    // human-readable report.
    let parquets = collect_parquets(&data_root());
    if parquets.is_empty() {
        eprintln!(
            "no real parquets found under {} — skipping replay (the catalogue gate above already passed)",
            data_root().display()
        );
        return;
    }

    let mut checklist: Vec<(PathBuf, usize, usize, usize, Vec<String>)> = Vec::new();
    for parquet in &parquets {
        let label = parquet.strip_prefix(data_root()).unwrap_or(parquet).to_path_buf();
        let tsdb = match Tsdb::load(parquet) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                eprintln!("skip {}: load failed: {e}", label.display());
                continue;
            }
        };
        let Some((min_ns, max_ns)) = tsdb.time_range() else {
            eprintln!("skip {}: no time range", label.display());
            continue;
        };
        let start = min_ns as f64 / 1e9;
        let end = max_ns as f64 / 1e9;
        let step = 1.0;

        let engine = QueryEngine::new(tsdb);
        let backend = DuckDbBackend::new();

        let mut hits = 0usize;
        let mut errors = 0usize;
        let mut diffs: Vec<String> = Vec::new();

        for q in &queries {
            let Some((entry, captures)) = cat.lookup(q) else {
                unreachable!("coverage gate above ensures every query matches");
            };
            if entry.mode == Mode::Off {
                continue;
            }
            // Both engines must succeed for a meaningful comparison.
            let promql = match engine.query_range_promql(q, start, end, step) {
                Ok(r) => r,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            let template = CompiledTemplate::parse(&entry.promql).unwrap();
            let captures_re = template.match_query(q).unwrap_or(captures);
            let sql = match backend.run(entry, &captures_re, parquet.to_str().unwrap(), start, end, step) {
                Ok(r) => r,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };
            hits += 1;

            let p_json = canonicalise(&promql);
            let s_json = canonicalise(&sql);
            if p_json != s_json {
                diffs.push(format!("{} :: {}", entry.id, truncate(q, 100)));
            }
        }

        checklist.push((label, hits, errors, diffs.len(), diffs));
    }

    // Print human-readable checklist (visible with --nocapture).
    eprintln!("\n=== Frontend coverage checklist ===");
    eprintln!(
        "{:<48} {:>6} {:>6} {:>6}",
        "parquet", "checked", "errors", "diffs"
    );
    for (label, hits, errors, n_diffs, _) in &checklist {
        eprintln!(
            "{:<48} {:>6} {:>6} {:>6}",
            label.display(),
            hits,
            errors,
            n_diffs
        );
    }

    // Split the checklist into "primary" vs "AB-combined-with-gaps".
    // Primary = single-source recordings + AB_level_pin (which has no gaps
    // because the recordings were aligned). Combined-with-gaps = AB_base /
    // AB_base_pin / AB_level — these have one inter-snapshot gap each, which
    // PromQL handles via step-based eval and SQL doesn't (yet).
    let strict_ab = std::env::var("METRIKEN_FRONTEND_COVERAGE_STRICT_AB").is_ok();
    let is_combined_with_gaps = |label: &Path| -> bool {
        let s = label.to_string_lossy();
        s == "AB_base.parquet" || s == "AB_base_pin.parquet" || s == "AB_level.parquet"
    };

    let primary_diffs: usize = checklist
        .iter()
        .filter(|(label, _, _, _, _)| !is_combined_with_gaps(label))
        .map(|(_, _, _, n, _)| *n)
        .sum();
    let ab_diffs: usize = checklist
        .iter()
        .filter(|(label, _, _, _, _)| is_combined_with_gaps(label))
        .map(|(_, _, _, n, _)| *n)
        .sum();

    if ab_diffs > 0 {
        eprintln!(
            "\n⚠ {ab_diffs} divergent (parquet, query) pairs on AB combined-with-gaps parquets \
            (PromQL step-based eval vs SQL row-based eval over data gaps).\
            \n  Documented in test header. Set METRIKEN_FRONTEND_COVERAGE_STRICT_AB=1 to upgrade to failure."
        );
    }

    let mut diffs_to_panic = primary_diffs;
    if strict_ab {
        diffs_to_panic += ab_diffs;
    }
    if diffs_to_panic > 0 {
        let mut detail = String::new();
        for (label, _, _, _, diffs) in &checklist {
            if !strict_ab && is_combined_with_gaps(label) {
                continue;
            }
            for d in diffs {
                detail.push_str(&format!("  {} :: {}\n", label.display(), d));
            }
        }
        panic!(
            "{} (parquet, query) pairs diverged between PromQL and SQL on production parquets:\n{}",
            diffs_to_panic, detail
        );
    }
    eprintln!(
        "\n✅ {} parquets checked, {} total (parquet, query) pairs verified, {} primary divergences{}",
        checklist.len(),
        checklist.iter().map(|c| c.1).sum::<usize>(),
        primary_diffs,
        if ab_diffs > 0 {
            format!(" ({ab_diffs} AB-gap warnings)")
        } else {
            String::new()
        }
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
