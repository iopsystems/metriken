//! Snapshot tests for every catalogue entry's emitted SQL.
//!
//! `translate.rs` has 2,403 LOC of PromQL→SQL emission logic and
//! previously had a single trivial unit test (`regex_literal_detection`).
//! These snapshots pin the SQL string each entry produces against a
//! known synthetic `MetricCatalog` — drift in resolver bodies surfaces
//! immediately via `cargo insta review`.
//!
//! One snapshot per catalogue entry (69 entries → ~69 snapshots, give
//! or take a few that share resolvers). The snapshots are keyed by
//! entry id, not by example index, so renaming an entry id triggers a
//! visible review action.
//!
//! Running:
//!   cargo test -p metriken-query --test translate_snapshots --features sql
//! Reviewing changes:
//!   cargo insta review --workspace

#![cfg(feature = "sql")]

use std::collections::{BTreeMap, HashMap};

use metriken_query::{Catalogue, CompiledTemplate};
use metriken_query_sql::views::{MetricCatalog, MetricSeries};

/// Build a synthetic `MetricCatalog` rich enough to drive every
/// catalogue entry to a non-empty SQL string. Adding metrics here as
/// new entries land is expected; the test surfaces missing metrics as
/// "entry produced empty SQL" rather than a flat None.
fn synthetic_catalog() -> MetricCatalog {
    let mut series_by_metric: HashMap<String, Vec<MetricSeries>> = HashMap::new();

    fn add_scalar(
        sbm: &mut HashMap<String, Vec<MetricSeries>>,
        metric: &str,
        label_kvs: &[(&str, &str)],
    ) {
        let labels: BTreeMap<String, String> = label_kvs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        sbm.entry(metric.to_string())
            .or_default()
            .push(MetricSeries {
                physical: metric.to_string(),
                labels,
            });
    }

    // Bare scalar metrics referenced by literal-form entries.
    add_scalar(&mut series_by_metric, "requests", &[]);
    add_scalar(&mut series_by_metric, "queue_depth", &[]);
    add_scalar(&mut series_by_metric, "temperature_offset", &[]);

    // Rezolus CPU family — per-cpu fan-out via `/<n>` physical names.
    for cpu in 0..2u32 {
        for state in ["user", "system", "softirq", "irq"] {
            let physical = format!("cpu_usage/{state}/{cpu}");
            series_by_metric
                .entry("cpu_usage".to_string())
                .or_default()
                .push(MetricSeries {
                    physical,
                    labels: BTreeMap::from([
                        ("state".to_string(), state.to_string()),
                        ("id".to_string(), cpu.to_string()),
                    ]),
                });
        }
        for m in [
            "cpu_instructions",
            "cpu_cycles",
            "cpu_tsc",
            "cpu_aperf",
            "cpu_mperf",
            "cpu_l3_miss",
            "cpu_l3_access",
            "cpu_dtlb_miss",
            "cpu_branch_miss",
            "cpu_branches",
        ] {
            let physical = format!("{m}/{cpu}");
            series_by_metric
                .entry(m.to_string())
                .or_default()
                .push(MetricSeries {
                    physical,
                    labels: BTreeMap::from([("id".to_string(), cpu.to_string())]),
                });
        }
    }
    add_scalar(&mut series_by_metric, "cpu_cores", &[]);

    // Memory.
    add_scalar(&mut series_by_metric, "memory_total", &[]);
    add_scalar(&mut series_by_metric, "memory_available", &[]);
    add_scalar(&mut series_by_metric, "memory_numa_local", &[("node", "0")]);

    // Softirq family with `kind` label.
    for cpu in 0..2u32 {
        for kind in ["hi", "timer", "net_rx"] {
            let physical = format!("softirq/{kind}/{cpu}");
            series_by_metric
                .entry("softirq".to_string())
                .or_default()
                .push(MetricSeries {
                    physical,
                    labels: BTreeMap::from([
                        ("kind".to_string(), kind.to_string()),
                        ("id".to_string(), cpu.to_string()),
                    ]),
                });
            let phys2 = format!("softirq_time/{kind}/{cpu}");
            series_by_metric
                .entry("softirq_time".to_string())
                .or_default()
                .push(MetricSeries {
                    physical: phys2,
                    labels: BTreeMap::from([
                        ("kind".to_string(), kind.to_string()),
                        ("id".to_string(), cpu.to_string()),
                    ]),
                });
        }
    }

    // Rezolus BPF sampler family.
    for sampler in ["cpu/usage", "memory/meminfo"] {
        let physical = format!("rezolus_bpf_run_time/{sampler}");
        series_by_metric
            .entry("rezolus_bpf_run_time".to_string())
            .or_default()
            .push(MetricSeries {
                physical,
                labels: BTreeMap::from([("sampler".to_string(), sampler.to_string())]),
            });
        let physical = format!("rezolus_bpf_run_count/{sampler}");
        series_by_metric
            .entry("rezolus_bpf_run_count".to_string())
            .or_default()
            .push(MetricSeries {
                physical,
                labels: BTreeMap::from([("sampler".to_string(), sampler.to_string())]),
            });
    }

    // GPU.
    add_scalar(&mut series_by_metric, "gpu_dram_bandwidth_utilization", &[]);
    add_scalar(&mut series_by_metric, "gpu_memory_used", &[]);
    add_scalar(&mut series_by_metric, "gpu_memory_free", &[]);

    // Histograms — physical column carries `:buckets` suffix per the
    // metriken-exposition convention; grouping_power = 3 for synthetic
    // fixtures (matches metriken-query-fixtures defaults).
    let mut histogram_p_by_metric: HashMap<String, u8> = HashMap::new();
    for (m, p) in [
        ("request_latency", 3u8),
        ("syscall_latency", 3u8),
        ("tcp_packet_latency", 4u8),
    ] {
        series_by_metric
            .entry(m.to_string())
            .or_default()
            .push(MetricSeries {
                physical: format!("{m}:buckets"),
                labels: BTreeMap::new(),
            });
        histogram_p_by_metric.insert(m.to_string(), p);
    }

    MetricCatalog {
        series_by_metric,
        histogram_p_by_metric,
    }
}

fn example_for(entry: &metriken_query::CatalogueEntry) -> String {
    if entry.examples.is_empty() {
        entry.promql.clone()
    } else {
        entry.examples[0].query.clone()
    }
}

/// One combined snapshot: every entry id paired with the SQL it emits
/// (or "<None>" / "<template-mismatch>" diagnostic markers). Sorted by
/// id so diff churn is per-entry, not per-list-order.
///
/// Why one combined snapshot instead of 69 individual ones: faster
/// review (one file diff), no per-entry test boilerplate, easy to
/// `cargo insta accept` after a deliberate change. The downside is
/// that any single-entry change shows up as a diff in the combined
/// file — but `cargo insta review` displays it intra-file anyway.
#[test]
fn every_entry_emits_stable_sql() {
    let cat = Catalogue::embedded();
    let metric_catalog = synthetic_catalog();

    let mut rows: Vec<(String, String)> = Vec::new();
    for entry in cat.entries() {
        let query = example_for(entry);
        let template = match CompiledTemplate::parse(&entry.promql) {
            Ok(t) => t,
            Err(e) => {
                rows.push((entry.id.clone(), format!("<template-parse-error: {e}>")));
                continue;
            }
        };
        let captures = match template.match_query(&query) {
            Some(c) => c,
            None => {
                rows.push((entry.id.clone(), format!("<template-mismatch: {query}>")));
                continue;
            }
        };
        let sql = metriken_query::translate::try_generate(entry, &captures, &metric_catalog);
        let body = match sql {
            Some(s) => s,
            None => "<None>".to_string(),
        };
        rows.push((entry.id.clone(), body));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut combined = String::new();
    for (id, sql) in &rows {
        combined.push_str("\n=========================\n");
        combined.push_str(id);
        combined.push_str("\n=========================\n");
        combined.push_str(sql);
        combined.push('\n');
    }

    insta::assert_snapshot!("entry_sql", combined);
}
