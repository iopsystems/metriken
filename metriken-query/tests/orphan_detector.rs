//! Catalogue health tests.
//!
//! Two checks, separately reportable:
//!
//! 1. **Resolver reachability** — every entry in `queries.toml` must
//!    produce SQL via `translate::try_generate` when paired with its
//!    own example. Catches: adding a TOML entry without wiring it
//!    into `translate.rs`; renaming an id in one place only.
//!
//! 2. **Lookup ordering** (informational) — `Catalogue::lookup(query)`
//!    is first-match-wins, so an entry whose example matches an
//!    earlier (more-general) entry's template is *unreachable from
//!    user queries* even though `try_generate` works for it
//!    directly. Logged, not asserted, because shadowed entries can
//!    still be valid documentation/test fixtures.

#![cfg(feature = "harness")]

use std::collections::{BTreeMap, HashMap};

use metriken_query::harness::{self, Catalogue, CompiledTemplate};
use metriken_query_sql::views::{MetricCatalog, MetricSeries};

/// Build a `MetricCatalog` that contains every metric referenced by an
/// embedded catalogue entry, with a single synthetic series per metric.
/// Metrics with histogram shape get their `grouping_power` registered too.
fn synthetic_catalog() -> MetricCatalog {
    // The set of metrics various entries hard-code internally (the
    // try_avg_over_time / try_histogram dispatch paths inspect
    // `catalog.series_by_metric` for these by name).
    let scalar_metrics = [
        // counter_irate_basic / counter_rate_basic / counter_irate_reset
        // / counter_rate_reset — all key off "requests".
        "requests",
        // gauge_avg_over_time keys off "queue_depth".
        "queue_depth",
        // Multi-source / multi-id rezolus metrics referenced by various
        // resolve_shape arms.
        "cpu_usage",
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
        "cpu_cores",
        "memory_total",
        "memory_available",
        "softirq_time",
        "softirq_count",
        "rezolus_bpf_run_time",
        "rezolus_bpf_run_count",
        "gpu_dram_bandwidth_utilization",
        "gpu_memory_used",
        "gpu_memory_free",
        // Any synthetic capture name `${m}` could land here — covering the
        // common shapes the templates actually use.
    ];

    let histogram_metrics = [
        ("request_latency", 3u8),
        ("syscall_latency", 3u8),
        ("tcp_packet_latency", 4u8),
    ];

    let mut series_by_metric: HashMap<String, Vec<MetricSeries>> = HashMap::new();
    for m in scalar_metrics {
        series_by_metric.insert(
            m.to_string(),
            vec![MetricSeries {
                physical: m.to_string(),
                labels: BTreeMap::new(),
            }],
        );
    }
    let mut histogram_p_by_metric: HashMap<String, u8> = HashMap::new();
    for (m, p) in histogram_metrics {
        series_by_metric.insert(
            m.to_string(),
            vec![MetricSeries {
                physical: format!("{m}:buckets"),
                labels: BTreeMap::new(),
            }],
        );
        histogram_p_by_metric.insert(m.to_string(), p);
    }

    MetricCatalog {
        series_by_metric,
        histogram_p_by_metric,
    }
}

/// Pick a query string for an entry: first golden example for
/// templated entries, the literal `promql` field for literal entries.
fn example_for(entry: &harness::CatalogueEntry) -> String {
    if entry.examples.is_empty() {
        entry.promql.clone()
    } else {
        entry.examples[0].query.clone()
    }
}

#[test]
fn every_catalogue_entry_has_a_resolver() {
    let cat = Catalogue::embedded();
    let metric_catalog = synthetic_catalog();

    let mut orphans: Vec<String> = Vec::new();
    let mut self_mismatch: Vec<(String, String)> = Vec::new();

    for entry in cat.entries() {
        let query = example_for(entry);

        // Compile the entry's *own* template (bypassing lookup ordering)
        // and match it against the example. Decouples this test from
        // catalogue ordering — that concern is the next test.
        let template = match CompiledTemplate::parse(&entry.promql) {
            Ok(t) => t,
            Err(e) => {
                self_mismatch.push((entry.id.clone(), format!("template parse error: {e}")));
                continue;
            }
        };
        let captures = match template.match_query(&query) {
            Some(c) => c,
            None => {
                self_mismatch.push((entry.id.clone(), format!("own template does not match own example `{query}`")));
                continue;
            }
        };

        if harness::translate::try_generate(entry, &captures, &metric_catalog).is_none() {
            orphans.push(entry.id.clone());
        }
    }

    if !self_mismatch.is_empty() || !orphans.is_empty() {
        let mut msg = String::from("\nCatalogue health check failed:\n");
        if !self_mismatch.is_empty() {
            msg.push_str("\n  Entries whose own template doesn't match their own example:\n");
            for (id, q) in &self_mismatch {
                msg.push_str(&format!("    {id}: {q}\n"));
            }
        }
        if !orphans.is_empty() {
            msg.push_str(
                "\n  Entries with no `translate::try_generate` resolver \
                 (orphan in queries.toml):\n",
            );
            for id in &orphans {
                msg.push_str(&format!("    {id}\n"));
            }
        }
        panic!("{msg}");
    }
}

/// Informational: report (do not fail) when an entry's example would
/// be intercepted by an earlier, more-general entry in
/// `Catalogue::lookup(query)`. Shadowed entries are unreachable from
/// user queries; they may still be valid as test fixtures or
/// documentation. Flip `#[ignore]` to wake this up as a hard check
/// once the catalogue is rationalized.
#[test]
#[ignore = "informational — shadowed entries are an open audit item"]
fn entries_are_not_shadowed_by_earlier_entries() {
    let cat = Catalogue::embedded();
    let mut shadowed: Vec<(String, String, String)> = Vec::new();

    for entry in cat.entries() {
        let query = example_for(entry);
        if let Some((matched, _)) = cat.lookup(&query) {
            if matched.id != entry.id {
                shadowed.push((entry.id.clone(), matched.id.clone(), query));
            }
        }
    }

    if !shadowed.is_empty() {
        let mut msg = String::from(
            "\n  Entries whose example is intercepted by an earlier entry:\n",
        );
        for (id, by, q) in &shadowed {
            msg.push_str(&format!("    {id} ← shadowed by `{by}` on query `{q}`\n"));
        }
        panic!("{msg}");
    }
}
