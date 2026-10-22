//! Prometheus text as the snapshots a recording stages.

use crate::exposition::{HistogramCount, Scrape, Value};
use metriken_model::{
    Counter, Gauge, GroupSchema, GroupSnapshot, Histogram as SnapshotHistogram, MetricDesc,
    Snapshot, SnapshotV3,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

/// Converts Prometheus text format responses into snapshots the archive
/// writer stages.
///
/// Maintains a stable mapping from (metric_name, labels) to numeric IDs across
/// scrapes within a recording session, ensuring consistent column identity.
pub struct PrometheusConverter {
    metric_ids: HashMap<MetricKey, usize>,
    next_id: usize,
    /// The scrapes' `# HELP` text by metric name, first seen wins. Written
    /// into the recording's `descriptions` metadata, the key an agent's
    /// `/metrics/descriptions` fills for an agent recording.
    descriptions: BTreeMap<String, String>,
    /// A name was added to `descriptions` since the recording last stored
    /// them: see [`pending_descriptions`](Self::pending_descriptions).
    descriptions_changed: bool,
    /// A scrape with skipped lines has been logged at `warn`. Later ones are
    /// logged at `debug`, so an exporter that always serves a bad line warns
    /// once per converter (one endpoint's recording) rather than every tick.
    skips_warned: bool,
}

impl Default for PrometheusConverter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct MetricKey {
    name: String,
    labels: Vec<(String, String)>,
}

impl PrometheusConverter {
    /// A converter for one endpoint's scrapes. The endpoint's `source` and
    /// URL describe the recording, not a metric, so neither is written into
    /// the metrics' metadata.
    pub fn new() -> Self {
        Self {
            metric_ids: HashMap::new(),
            next_id: 0,
            descriptions: BTreeMap::new(),
            descriptions_changed: false,
            skips_warned: false,
        }
    }

    /// The `descriptions` metadata value (a JSON object of metric name to
    /// HELP text) when a scrape since the last
    /// [`descriptions_stored`](Self::descriptions_stored) added a name, else
    /// `None`. An exporter's HELP lines rarely change, so this is normally
    /// `Some` after the first scrape and `None` from then on.
    pub fn pending_descriptions(&self) -> Option<String> {
        if !self.descriptions_changed {
            return None;
        }
        serde_json::to_string(&self.descriptions).ok()
    }

    /// The value [`pending_descriptions`](Self::pending_descriptions) last
    /// returned was handed to the archive writer. Called only once the
    /// hand-off succeeded, so a failed hand-off is retried after the next
    /// scrape. The writer applies the patch later, and one it cannot apply is
    /// logged there and not retried.
    pub fn descriptions_stored(&mut self) {
        self.descriptions_changed = false;
    }

    fn get_or_assign_id(&mut self, name: &str, labels: &[(String, String)]) -> String {
        let key = MetricKey {
            name: name.to_string(),
            labels: labels.to_vec(),
        };
        if let Some(id) = self.metric_ids.get(&key) {
            return id.to_string();
        }
        let id = self.next_id;
        self.next_id += 1;
        self.metric_ids.insert(key, id);
        id.to_string()
    }

    fn build_metadata(&self, name: &str, labels: &[(String, String)]) -> HashMap<String, String> {
        let mut metadata = HashMap::new();
        metadata.insert("metric".to_string(), name.to_string());
        for (k, v) in labels {
            metadata.insert(k.clone(), v.clone());
        }
        // No `source` or `endpoint`: both describe the recording (one
        // converter is one endpoint), and an exporter's own `source` label
        // is kept.
        metadata
    }

    fn log_skipped(&mut self, count: usize, first: &str) {
        let (cut, more) = shown_line(first);
        let more = if more { " (cut)" } else { "" };
        if self.skips_warned {
            debug!("skipped {count} unparseable exposition line(s); the first: {cut:?}{more}");
        } else {
            self.skips_warned = true;
            warn!(
                "skipped {count} unparseable exposition line(s) (later scrapes log at debug); \
                 the first: {cut:?}{more}"
            );
        }
    }

    /// Convert one scrape, bracketed by the round trip that produced it.
    ///
    /// `request_ns`/`response_ns` are when the request went out and when the
    /// response finished arriving, and every value carries that interval as
    /// its acquisition window. Exposition carries no acquisition instant; the
    /// optional trailing timestamp on a sample is a staleness marker and is
    /// not used.
    pub fn convert(&mut self, text: &str, request_ns: u64, response_ns: u64) -> Snapshot {
        let scrape = Scrape::parse(&sanitize_metric_names(text));
        if let Some(first) = &scrape.first_skipped {
            self.log_skipped(scrape.skipped, first);
        }

        for (name, doc) in &scrape.docs {
            if !self.descriptions.contains_key(name) {
                self.descriptions.insert(name.clone(), doc.clone());
                self.descriptions_changed = true;
            }
        }

        let mut counters = Vec::new();
        let mut gauges = Vec::new();
        let mut histograms = Vec::new();

        // One window for the whole scrape: every sample in it was read by the
        // same request/response, so they share an acquisition instant.
        let window = sample_window(request_ns, response_ns);

        for sample in scrape.samples {
            // Sorted by name, which `MetricKey` relies on.
            let labels = sample.labels;

            match sample.value {
                Value::Counter(v) => {
                    if !v.is_finite() {
                        continue;
                    }
                    let id = self.get_or_assign_id(&sample.metric, &labels);
                    counters.push(
                        Counter::new(id, v as u64, self.build_metadata(&sample.metric, &labels))
                            .with_window(window),
                    );
                }
                Value::Gauge(v) => {
                    if !v.is_finite() {
                        continue;
                    }
                    let id = self.get_or_assign_id(&sample.metric, &labels);
                    gauges.push(
                        Gauge::new(id, v as i64, self.build_metadata(&sample.metric, &labels))
                            .with_window(window),
                    );
                }
                Value::Untyped(v) => {
                    if !v.is_finite() {
                        continue;
                    }
                    // _total, _sum, and _count are monotonically increasing
                    // by Prometheus convention, so store them as counters.
                    // _sum is particularly useful: rate(_sum) / rate(_count)
                    // gives the true mean for comparison against approximated
                    // histogram percentiles.
                    let id = self.get_or_assign_id(&sample.metric, &labels);
                    let metadata = self.build_metadata(&sample.metric, &labels);
                    if sample.metric.ends_with("_total")
                        || sample.metric.ends_with("_sum")
                        || sample.metric.ends_with("_count")
                    {
                        counters.push(Counter::new(id, v as u64, metadata).with_window(window));
                    } else {
                        gauges.push(Gauge::new(id, v as i64, metadata).with_window(window));
                    }
                }
                Value::Histogram(ref buckets) => {
                    if let Some((h, metadata)) = convert_histogram(buckets, &sample.metric, &labels)
                    {
                        let id = self.get_or_assign_id(&sample.metric, &labels);
                        histograms
                            .push(SnapshotHistogram::new(id, h, metadata).with_window(window));
                    }
                }
                Value::Summary(ref quantiles) => {
                    for quantile in quantiles {
                        if !quantile.count.is_finite() {
                            continue;
                        }
                        let q = quantile.quantile.to_string();
                        let mut q_labels = labels.clone();
                        q_labels.push(("quantile".to_string(), q));
                        q_labels.sort();
                        let id = self.get_or_assign_id(&sample.metric, &q_labels);
                        gauges.push(
                            Gauge::new(
                                id,
                                quantile.count as i64,
                                self.build_metadata(&sample.metric, &q_labels),
                            )
                            .with_window(window),
                        );
                    }
                }
            }
        }

        // One scrape is one acquisition — one request, one response, one
        // window — which is exactly what a V3 acquisition group models. See
        // `PROMETHEUS_GROUP` for why this is a group rather than a flat V2
        // snapshot.
        //
        // Sorted by assigned id, not by encounter order. Ids are stable across
        // scrapes for a given (name, labels), so the schema then changes only
        // when the metric SET does, rather than whenever an exporter reorders
        // its output. A churning schema is not incorrect — every row carries
        // its own hash and re-anchors when it changes — but it defeats the
        // receiver's schema cache and pads every WAL row with a fresh copy.
        by_id(&mut counters, |c| &c.name);
        by_id(&mut gauges, |g| &g.name);
        by_id(&mut histograms, |h| &h.name);

        let schema = GroupSchema {
            counters: counters.iter().map(desc_of).collect(),
            gauges: gauges.iter().map(desc_of).collect(),
            histograms: histograms.iter().map(desc_of).collect(),
            gauge_histograms: Vec::new(),
        };
        let group = GroupSnapshot {
            name: PROMETHEUS_GROUP.to_string(),
            schema_hash: schema.hash(),
            // Always sent. The producer contract allows omitting it on a cache
            // hit, but a scrape's schema is rebuilt from scratch each tick
            // here — there is no cheaper path to take, and a stateless payload
            // is one less thing that can go stale.
            schema: Some(Arc::new(schema)),
            window,
            counters: counters.into_iter().map(|c| Some(c.value)).collect(),
            gauges: gauges.into_iter().map(|g| Some(g.value)).collect(),
            histograms: histograms.into_iter().map(|h| Some(h.value)).collect(),
            gauge_histograms: Vec::new(),
        };

        Snapshot::V3(SnapshotV3 {
            systemtime: SystemTime::now(),
            // The round trip. `duration` is documented as the snapshot's
            // collection duration, and for a scrape that is exactly the
            // interval the window brackets.
            duration: Duration::from_nanos(response_ns.saturating_sub(request_ns)),
            metadata: HashMap::new(),
            groups: vec![group],
        })
    }
}

/// The characters of a skipped line that are logged.
const SHOWN_CHARS: usize = 200;

/// The first [`SHOWN_CHARS`] characters of `line`, cut on a character
/// boundary, and whether anything was cut.
fn shown_line(line: &str) -> (&str, bool) {
    match line.char_indices().nth(SHOWN_CHARS) {
        Some((end, _)) => (&line[..end], true),
        None => (line, false),
    }
}

/// The table an endpoint's scrape lands in: `<sampler>/<group>`.
///
/// A `.rez` reader dispatches a table's WAL rows between the V3 group-row
/// decoder and the V1/V2 sampler-cell decoder on whether its key contains
/// `/`, so a group name without one would send group rows down the sampler
/// path.
///
/// `prometheus` as the sampler rather than the service's own name: each
/// target is its own recording tagged `source=<service>`, so repeating the
/// service here would say nothing new, while "these readings came from a
/// Prometheus scrape" is not recorded anywhere else. One group, because one
/// scrape is one acquisition.
pub const PROMETHEUS_GROUP: &str = "prometheus/scrape";

/// Sort by the numeric id assigned to each metric.
///
/// By VALUE, not lexically: ids are decimal strings, so a lexical sort puts
/// `"10"` before `"2"` and the order changes shape as a target crosses a power
/// of ten — which would rewrite the schema for no reason.
fn by_id<T>(items: &mut [T], name: impl Fn(&T) -> &String) {
    items.sort_by_key(|i| name(i).parse::<u64>().unwrap_or(u64::MAX));
}

/// One schema member from a snapshot value: the id it is keyed by, and the
/// metadata that says what it actually is.
fn desc_of<T: HasNameAndMetadata>(item: &T) -> MetricDesc {
    MetricDesc {
        name: item.name().to_string(),
        metadata: item
            .metadata()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

/// Read a name and metadata off any of the three snapshot value shapes, so
/// [`desc_of`] is written once rather than three times.
trait HasNameAndMetadata {
    fn name(&self) -> &str;
    fn metadata(&self) -> &HashMap<String, String>;
}

macro_rules! has_name_and_metadata {
    ($($t:ty),*) => {$(
        impl HasNameAndMetadata for $t {
            fn name(&self) -> &str {
                &self.name
            }
            fn metadata(&self) -> &HashMap<String, String> {
                &self.metadata
            }
        }
    )*};
}
has_name_and_metadata!(Counter, Gauge, SnapshotHistogram);

/// Convert Prometheus cumulative histogram buckets into a histogram::Histogram.
///
/// Uses the upper bound (le) of each bucket as the representative value and
/// computes per-bucket delta counts from the cumulative Prometheus counts.
///
/// For `_seconds` metrics, le values are multiplied by 1e9 to convert to
/// nanoseconds, the unit metriken's latency histograms use. Other metrics use a
/// generic power-of-10 scale that makes the smallest le boundary >= 1.
fn convert_histogram(
    buckets: &[HistogramCount],
    metric_name: &str,
    labels: &[(String, String)],
) -> Option<(histogram::Histogram, HashMap<String, String>)> {
    // Filter to finite boundaries only (+Inf cannot be represented)
    let finite_buckets: Vec<_> = buckets
        .iter()
        .filter(|b| b.less_than.is_finite() && b.count.is_finite())
        .collect();

    if finite_buckets.is_empty() {
        return None;
    }

    // For _seconds histograms, convert to nanoseconds.
    // Otherwise, use a generic scale that preserves precision.
    let scale = if metric_name.ends_with("_seconds") {
        1e9
    } else {
        compute_generic_scale(&finite_buckets)
    };

    // max_value_power must cover the largest scaled value
    let max_scaled = finite_buckets
        .iter()
        .map(|b| (b.less_than * scale) as u64)
        .max()
        .unwrap_or(1);
    let max_value_power = if max_scaled == 0 {
        8
    } else {
        ((max_scaled as f64).log2().ceil() as u8 + 1).clamp(8, 64)
    };

    let grouping_power: u8 = 7;

    let mut h = histogram::Histogram::new(grouping_power, max_value_power).ok()?;

    // Convert cumulative counts to deltas and add to histogram
    let mut prev_count = 0u64;
    for bucket in &finite_buckets {
        let cum_count = bucket.count as u64;
        let delta = cum_count.saturating_sub(prev_count);
        if delta > 0 {
            let value = (bucket.less_than * scale) as u64;
            let _ = h.add(value, delta);
        }
        prev_count = cum_count;
    }

    let mut metadata = HashMap::new();
    metadata.insert("metric".to_string(), metric_name.to_string());
    for (k, v) in labels {
        metadata.insert(k.clone(), v.clone());
    }
    // See `build_metadata` for why neither is written here.
    metadata.insert("grouping_power".to_string(), grouping_power.to_string());
    metadata.insert("max_value_power".to_string(), max_value_power.to_string());

    Some((h, metadata))
}

/// Compute a power-of-10 scale factor that makes the smallest positive le
/// boundary >= 1, preserving precision when converting float boundaries to u64.
fn compute_generic_scale(buckets: &[&HistogramCount]) -> f64 {
    let min_le = buckets
        .iter()
        .map(|b| b.less_than)
        .filter(|v| *v > 0.0)
        .fold(f64::INFINITY, f64::min);

    if min_le >= 1.0 || min_le == f64::INFINITY {
        return 1.0;
    }

    let mut scale = 1.0;
    while min_le * scale < 1.0 {
        scale *= 10.0;
    }
    scale
}

/// Replace colons with underscores in metric names.
///
/// The exposition parser reads a metric name as letters, digits and `_`
/// (`char::is_alphanumeric`), which does not include colons. Prometheus
/// allows colons in metric names (commonly used by recording rules and
/// namespaced exporters like vLLM), so we normalize them to underscores
/// before parsing.
///
/// Only the metric name portion of each line is modified - label values and
/// HELP descriptions are left untouched.
fn sanitize_metric_names(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            // For comment lines (HELP/TYPE), replace colons in the metric
            // name token only (the word after HELP/TYPE keyword)
            if let Some(rest) = trimmed
                .strip_prefix("# HELP ")
                .or(trimmed.strip_prefix("# TYPE "))
            {
                let prefix = &trimmed[..trimmed.len() - rest.len()];
                // The metric name is the first token
                let name_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
                let name = &rest[..name_end];
                let after = &rest[name_end..];
                result.push_str(prefix);
                result.push_str(&name.replace(':', "_"));
                result.push_str(after);
            } else {
                result.push_str(trimmed);
            }
        } else {
            // Sample line: metric_name{labels} value [timestamp]
            // Replace colons only in the metric name (before '{' or whitespace)
            let name_end = trimmed
                .find(|c: char| c == '{' || c.is_whitespace())
                .unwrap_or(trimmed.len());
            let name = &trimmed[..name_end];
            let after = &trimmed[name_end..];
            result.push_str(&name.replace(':', "_"));
            result.push_str(after);
        }
        result.push('\n');
    }
    result
}

/// The acquisition window for every sample in one scrape: the round trip,
/// `[request_sent, response_received]`, on the recorder's clock.
///
/// Not the sample's own timestamp. Prometheus exposition allows an optional trailing
/// timestamp in milliseconds since the epoch, a federation or pushgateway
/// staleness marker, which the parser skips.
/// Taken as the window, `m_total 3 1000` would begin one second after the
/// Unix epoch, decades before the row holding it, and a recording would mix
/// two meanings of the window.
///
/// Every value in a response was read by the exporter somewhere inside the
/// round trip, and nothing here can say where, so the round trip is the
/// bound `rate()` prices its uncertainty from. An exporter that serves
/// values it computed before the request arrived read them earlier than
/// `request_sent`; nothing the client sees distinguishes that case, so for
/// such an exporter the recorded window is narrower than the real
/// uncertainty.
fn sample_window(request_ns: u64, response_ns: u64) -> Option<metriken_types::Window> {
    // Defensive: a non-monotonic wall clock (NTP step, VM migration) can make
    // the response read EARLIER than the request. A window whose end precedes
    // its begin would give `rate()` a saturating-to-zero width, so collapse it
    // to the instant we are surest of instead.
    if response_ns < request_ns {
        return Some(metriken_types::Window::new(response_ns, response_ns));
    }
    Some(metriken_types::Window::new(request_ns, response_ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HELP text accumulates across scrapes, first seen wins, and
    /// `pending_descriptions` reports a value only when a scrape added a name
    /// since the last `descriptions_stored`.
    #[test]
    fn help_lines_accumulate_and_are_pending_only_when_a_name_is_new() {
        let mut conv = PrometheusConverter::new();
        assert_eq!(conv.pending_descriptions(), None);

        conv.convert("# HELP a_total Apples.\na_total 1\n", 0, 1);
        let json = conv.pending_descriptions().expect("a new name is pending");
        let map: BTreeMap<String, String> = serde_json::from_str(&json).unwrap();
        assert_eq!(map, BTreeMap::from([("a_total".into(), "Apples.".into())]));

        // Not stored yet, so still pending after a scrape that adds nothing.
        conv.convert("# HELP a_total Apples.\na_total 2\n", 0, 1);
        assert!(conv.pending_descriptions().is_some());
        conv.descriptions_stored();
        assert_eq!(conv.pending_descriptions(), None);

        // The same name with different text keeps the first; nothing pending.
        conv.convert("# HELP a_total Changed.\na_total 3\n", 0, 1);
        assert_eq!(conv.pending_descriptions(), None);

        // A new name makes the whole map pending again.
        conv.convert("# HELP b Bananas.\nb 4\n", 0, 1);
        let map: BTreeMap<String, String> =
            serde_json::from_str(&conv.pending_descriptions().unwrap()).unwrap();
        assert_eq!(
            map,
            BTreeMap::from([
                ("a_total".into(), "Apples.".into()),
                ("b".into(), "Bananas.".into()),
            ])
        );
    }

    /// An exporter's trailing timestamp does not become the acquisition
    /// window: the window is the recorder's round trip (see
    /// `sample_window`).
    #[test]
    fn an_embedded_timestamp_is_not_the_window() {
        let mut conv = PrometheusConverter::new();
        let (request_ns, response_ns) = (5_000_000_000u64, 5_002_000_000u64);
        let text = "m_total 3 1000\n";
        // Read through the version-agnostic accessor: these assert a property
        // of the READINGS, not of the wire shape, and it is the same accessor
        // the parquet path uses.
        let mut snap = conv.convert(text, request_ns, response_ns);
        let w = snap.counters()[0].window.expect("window set");
        assert_eq!(
            (w.begin_ns, w.end_ns),
            (request_ns, response_ns),
            "the window is our round trip, not the exporter's 1000 ms epoch stamp"
        );
    }

    /// The offset is stored relative to the row timestamp, so an epoch-anchored
    /// window is not merely wide — it is a ~56-year error in the operand
    /// `rate()` prices its uncertainty from.
    #[test]
    fn an_embedded_timestamp_cannot_predate_the_row_it_describes() {
        let mut conv = PrometheusConverter::new();
        // A realistic recording clock: 2026-ish, not 1970.
        let fetch_ns = 1_780_000_000_000_000_000u64;
        let mut snap = conv.convert("m_total 3 1000\n", fetch_ns, fetch_ns + 2_000_000);
        let w = snap.counters()[0].window.expect("window set");
        assert!(
            w.begin_ns >= fetch_ns.saturating_sub(1),
            "a window beginning {} against a row at {fetch_ns} would be decades of \
             fabricated uncertainty",
            w.begin_ns
        );
    }

    #[test]
    fn absent_timestamp_falls_back_to_fetch_time() {
        let mut conv = PrometheusConverter::new();
        let (request_ns, response_ns) = (5_000_000_000u64, 5_002_000_000u64);
        let text = "m_total 3\n";
        let mut snap = conv.convert(text, request_ns, response_ns);
        let w = snap.counters()[0].window.expect("window set");
        assert_eq!((w.begin_ns, w.end_ns), (request_ns, response_ns));
    }

    /// A scrape is one acquisition, and every value in it carries the round
    /// trip as its window, not a zero-width window at one instant.
    #[test]
    fn every_value_in_a_scrape_carries_the_round_trip_as_its_window() {
        let mut conv = PrometheusConverter::new();
        let (request_ns, response_ns) = (10_000_000_000u64, 10_045_000_000u64);
        let text = "\
# TYPE a_total counter
a_total 1
# TYPE b gauge
b 2
";
        let mut snap = conv.convert(text, request_ns, response_ns);
        let (cs, gs) = (snap.counters(), snap.gauges());
        assert!(!cs.is_empty() && !gs.is_empty(), "fixture");
        for w in cs
            .iter()
            .map(|c| c.window)
            .chain(gs.iter().map(|g| g.window))
        {
            let w = w.expect("every value carries a window");
            assert_eq!((w.begin_ns, w.end_ns), (request_ns, response_ns));
            assert!(w.width_ns() > 0, "a scrape is not an instant");
        }
    }

    /// The flat view of a converted scrape keeps each value's name, metadata
    /// and window.
    ///
    /// `metriken-exposition` contracts that a V3 snapshot and the V2 describing
    /// the same readings produce the same `HashedSnapshot`, but that says
    /// nothing about whether this converter puts the same names, values,
    /// metadata and windows into it. Pinned here, on our side of the boundary.
    #[test]
    fn the_flat_view_of_a_scrape_is_unchanged_by_the_group_shape() {
        let mut conv = PrometheusConverter::new();
        let text = "\
# TYPE a_total counter
a_total 7
# TYPE b gauge
b{k=\"v\"} -3
";
        let mut snap = conv.convert(text, 1_000, 2_000);

        let counters = snap.counters();
        assert_eq!(counters.len(), 1);
        assert_eq!(counters[0].value, 7);
        assert_eq!(
            counters[0].metadata.get("metric").map(String::as_str),
            Some("a_total")
        );
        // Neither describes a metric, and neither is written here any more —
        // one converter is one endpoint, so both are properties of the
        // recording. `endpoint` in particular had no reader anywhere.
        for key in ["source", "endpoint"] {
            assert!(
                !counters[0].metadata.contains_key(key),
                "`{key}` belongs to the recording, not the metric: {:?}",
                counters[0].metadata
            );
        }
        let w = counters[0].window.expect("window survives the group");
        assert_eq!((w.begin_ns, w.end_ns), (1_000, 2_000));

        let gauges = snap.gauges();
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].value, -3);
        assert_eq!(gauges[0].metadata.get("k").map(String::as_str), Some("v"));
    }

    /// A long skipped line is logged cut at 200 characters, on a character
    /// boundary.
    #[test]
    fn a_long_skipped_line_is_cut_at_200_characters() {
        let line = format!("m{{a=1}} {}", "€".repeat(300));
        let (cut, more) = shown_line(&line);
        assert!(more);
        assert_eq!(cut.chars().count(), 200);
        assert!(line.starts_with(cut));
        assert!(
            cut.ends_with('€'),
            "the cut falls after a multibyte character"
        );
        let short = "m{a=1} 1";
        assert_eq!(shown_line(short), (short, false));
        let exact = "x".repeat(200);
        assert_eq!(shown_line(&exact), (exact.as_str(), false));
    }

    /// A skipped line is logged at `warn` once per converter; a clean scrape
    /// does not count.
    #[test]
    fn a_skipped_line_warns_once_per_converter() {
        let mut conv = PrometheusConverter::new();
        conv.convert("ok 1\n", 1_000, 2_000);
        assert!(!conv.skips_warned);
        let mut snap = conv.convert("ok 1\nm{a=1} 2\n", 1_000, 2_000);
        assert!(conv.skips_warned);
        assert_eq!(snap.gauges().len(), 1, "the good line is still converted");
    }

    /// A label value is stored as the exposition text spells it, `=`, `,`,
    /// `}` and escapes included. prometheus-parse split the label block on
    /// every `,` and `=`, so `op="a=b"` reached the archive with no `op`,
    /// `op="a,b"` as `op="a"`, and the `a}b` line not at all.
    #[test]
    fn a_label_value_with_format_punctuation_reaches_the_metadata() {
        let mut conv = PrometheusConverter::new();
        let text = "\
# TYPE xss_total counter
xss_total{case=\"eq\",op=\"a=b\"} 1
xss_total{case=\"comma\",op=\"x=1,y=2\"} 2
xss_total{case=\"brace\",op=\"a}b\"} 3
xss_total{case=\"escapes\",op=\"q\\\"b\\\\s\\nn\"} 4
";
        let mut snap = conv.convert(text, 1_000, 2_000);
        let mut got: Vec<(String, Option<String>, u64)> = snap
            .counters()
            .iter()
            .map(|c| {
                (
                    c.metadata["case"].clone(),
                    c.metadata.get("op").cloned(),
                    c.value,
                )
            })
            .collect();
        got.sort();
        let want = |case: &str, op: &str, v| (case.to_string(), Some(op.to_string()), v);
        assert_eq!(
            got,
            vec![
                want("brace", "a}b", 3),
                want("comma", "x=1,y=2", 2),
                want("eq", "a=b", 1),
                want("escapes", "q\"b\\s\nn", 4),
            ]
        );
        assert!(
            snap.counters()
                .iter()
                .all(|c| !c.metadata.contains_key("y")),
            "no label is invented from a value"
        );
    }

    /// A scrape is ONE acquisition, so it is one group with one window — not a
    /// group per metric, and not a flat V2 snapshot where every value column
    /// drags its own `<name>:window_begin`/`:window_width` pair. That per-value
    /// pair is what would triple a Prometheus table's schema width in a `.rez`,
    /// which is exactly the cost acquisition groups removed.
    #[test]
    fn a_scrape_is_one_acquisition_group() {
        let mut conv = PrometheusConverter::new();
        let text = "\
# TYPE a_total counter
a_total 1
# TYPE c_total counter
c_total 2
# TYPE b gauge
b 3
";
        let Snapshot::V3(v3) = conv.convert(text, 1_000, 2_000) else {
            panic!("a scrape must convert to V3 — the flat V2 shape is what this replaced")
        };
        assert_eq!(v3.groups.len(), 1);
        let g = &v3.groups[0];
        assert_eq!(g.counters.len(), 2);
        assert_eq!(g.gauges.len(), 1);
        assert!(g.window.is_some(), "the group carries the scrape's window");
        assert_eq!(v3.duration.as_nanos(), 1_000, "the round trip");
    }

    /// The table key contains a `/` (see [`PROMETHEUS_GROUP`]).
    #[test]
    fn the_group_name_is_a_sampler_slash_group_key() {
        let (sampler, group) = PROMETHEUS_GROUP
            .split_once('/')
            .expect("a V3 group name is <sampler>/<group>");
        assert!(!sampler.is_empty() && !group.is_empty());
        assert_eq!(sampler, "prometheus");
    }

    /// The schema changes only when the metric SET does — not when an exporter
    /// reorders its output, and not as a target's ids cross a power of ten.
    ///
    /// A churning schema is not incorrect (each row carries its own hash and
    /// re-anchors), but it defeats the receiver's schema cache and pads every
    /// WAL row with a fresh copy of the membership.
    #[test]
    fn the_schema_hash_is_stable_across_scrapes_of_the_same_metrics() {
        let mut conv = PrometheusConverter::new();
        // Enough metrics that ids run past 9, where a lexical sort would put
        // "10" before "2" and reshuffle the schema.
        let many = |vals: &[u64]| {
            let mut t = String::new();
            for (i, v) in vals.iter().enumerate() {
                t.push_str(&format!("# TYPE m{i}_total counter\nm{i}_total {v}\n"));
            }
            t
        };
        let hash_of = |snap: Snapshot| match snap {
            Snapshot::V3(v3) => v3.groups[0].schema_hash,
            _ => panic!("V3"),
        };

        let first = hash_of(conv.convert(&many(&[0; 12]), 1_000, 2_000));
        let second = hash_of(conv.convert(&many(&[1; 12]), 3_000, 4_000));
        assert_eq!(first, second, "same metrics, same schema");

        // A metric appearing IS a membership change, and must re-anchor.
        let grown = hash_of(conv.convert(&many(&[1; 13]), 5_000, 6_000));
        assert_ne!(first, grown, "a new metric changes the membership");
    }

    /// A wall clock that steps backwards mid-scrape (NTP, a VM migration) must
    /// not produce a window whose end precedes its begin: the width saturates
    /// to zero, which would silently re-assert the very claim this replaced.
    #[test]
    fn a_backwards_clock_collapses_rather_than_inverting() {
        let w = sample_window(9_000, 8_000).unwrap();
        assert_eq!((w.begin_ns, w.end_ns), (8_000, 8_000));
    }
}
