use crate::promql::streaming::histogram::{HistogramOps, Stride};
use std::collections::HashMap;
use std::sync::Arc;

use promql_parser::label::Matcher;
use promql_parser::parser::{self, Expr};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::labels::Labels;
use crate::{DataSource, QueryOptions};

mod columns;
pub use columns::referenced_metrics;
#[cfg(feature = "archive")]
pub(crate) use columns::{repeated_matcher, selectors};
pub(crate) mod streaming;

#[cfg(test)]
mod tests;

#[derive(Error, Debug)]
pub enum QueryError {
    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Evaluation error: {0}")]
    EvaluationError(String),

    #[error("Unsupported operation: {0}")]
    Unsupported(String),

    /// The source holds no counter, gauge or histogram of this name. The
    /// payload is the bare name.
    #[error("Metric not found: {0}")]
    MetricNotFound(String),
}

/// The kind of series a query function reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    GaugeHistogram,
}

impl MetricKind {
    fn noun(self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
            MetricKind::GaugeHistogram => "gauge histogram",
        }
    }
}

/// What to say of a function that cannot read gauge histogram `name`.
fn gauge_histogram_reads(name: &str) -> String {
    format!(
        "{name} is a gauge histogram: each row is its state at that time, not a count since \
         start, so it is read as it is, with histogram_quantile(0.5, {name}), \
         histogram_quantiles([..], {name}), histogram_count({name}), histogram_sum({name}), \
         histogram_mean({name}) or histogram_heatmap({name})"
    )
}

/// The error for a function of a counting histogram's increase given gauge
/// histogram `name`: `what` names the function.
pub(crate) fn gauge_histogram_refused(name: &str, what: &str) -> QueryError {
    QueryError::Unsupported(format!(
        "{what} takes the difference between rows, which a gauge histogram does not have; {}",
        gauge_histogram_reads(name)
    ))
}

/// Whether histogram functions read `name` as a gauge histogram. A name the
/// source holds as both a gauge histogram and a counting histogram (two
/// producers disagreeing) is refused, not read either way. The gauge
/// listing is asked first, and is empty for a source holding none.
pub(crate) fn reads_gauge_histogram(
    source: &dyn DataSource,
    name: &str,
) -> Result<bool, QueryError> {
    if !source.gauge_histogram_names().iter().any(|n| n == name) {
        return Ok(false);
    }
    if source.histogram_names().iter().any(|n| n == name) {
        return Err(QueryError::Unsupported(format!(
            "{name} is a gauge histogram in some series and a counting histogram in others, \
             which are read differently; select one kind with a label matcher"
        )));
    }
    Ok(true)
}

/// Gauge histogram `name`'s rows for evaluation from `start_ns` (snapped as
/// the gauge grid is) to `end_ns` at `step_ns`, with the times to evaluate
/// at and the staleness: the step, or the metric's row interval when that
/// is longer. `None` when nothing is read.
pub(crate) fn read_gauge_histogram(
    source: &dyn DataSource,
    name: &str,
    filter: &Labels,
    (start_ns, end_ns, step_ns): (u64, u64, u64),
    eval: Option<&[u64]>,
) -> Option<(crate::histogram_stream::HistogramStream, Vec<u64>, u64)> {
    let times = streaming::gauge_histogram::times(start_ns, end_ns, step_ns, eval);
    let (first, last) = (*times.first()?, *times.last()?);
    let interval_ns = (source.gauge_histogram_interval(name).max(0.0) * 1e9) as u64;
    let staleness_ns = step_ns.max(interval_ns);
    let stream =
        source.gauge_histogram_stream(name, filter, first.saturating_sub(staleness_ns), last)?;
    Some((stream, times, staleness_ns))
}

/// A range's `(start_ns, end_ns, step_ns)` as the gauge grid lays it: the
/// start snapped down to a step boundary in grid mode, unless the range is
/// one point (an instant), which is evaluated where asked.
pub(crate) fn state_grid(start: f64, end: f64, step: f64, opts: &QueryOptions) -> (u64, u64, u64) {
    let step_ns = (step * 1e9) as u64;
    let raw = range_start_ns(start);
    let start_ns = match opts.rate_mode {
        crate::RateMode::Grid if step_ns > 0 && end > start => (raw / step_ns) * step_ns,
        _ => raw,
    };
    (start_ns, range_end_ns(end), step_ns)
}

/// What a read of `name` as `kind` that found no series means: `Ok` when
/// the source holds `name` as `kind` (nothing matched the range or the
/// labels, so the answer is empty), [`QueryError::Unsupported`] when it
/// holds `name` only as another kind, and [`QueryError::MetricNotFound`]
/// when it holds no metric of that name. Asked only on that path.
pub(crate) fn nothing_read(
    source: &dyn DataSource,
    name: &str,
    kind: MetricKind,
) -> Result<(), QueryError> {
    let holds = |k: MetricKind| {
        let names = match k {
            MetricKind::Counter => source.counter_names(),
            MetricKind::Gauge => source.gauge_names(),
            MetricKind::Histogram => source.histogram_names(),
            MetricKind::GaugeHistogram => source.gauge_histogram_names(),
        };
        names.iter().any(|n| n == name)
    };
    if holds(kind) {
        return Ok(());
    }
    let held = [
        MetricKind::Counter,
        MetricKind::Gauge,
        MetricKind::Histogram,
        MetricKind::GaugeHistogram,
    ]
    .into_iter()
    .find(|k| *k != kind && holds(*k));
    match held {
        Some(MetricKind::GaugeHistogram) => Err(QueryError::Unsupported(format!(
            "this function reads a {}; {}",
            kind.noun(),
            gauge_histogram_reads(name)
        ))),
        Some(MetricKind::Counter) if kind == MetricKind::Gauge => {
            Err(QueryError::Unsupported(format!(
                "{name} is a counter: read it through rate() or irate(), \
                 e.g. rate({name}[1m])"
            )))
        }
        Some(held) => Err(QueryError::Unsupported(format!(
            "{name} is a {}, and this function reads a {}",
            held.noun(),
            kind.noun()
        ))),
        None => Err(QueryError::MetricNotFound(name.to_string())),
    }
}

/// A single sample in the result.
///
/// `#[non_exhaustive]`: build with [`Sample::new`] + [`with_interval`](Sample::with_interval)
/// so future per-sample fields don't break downstream construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Sample {
    pub metric: HashMap<String, String>,
    pub value: (f64, f64), // (timestamp_seconds, value)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<(f64, f64)>,
}

impl Sample {
    /// A sample with no uncertainty interval. Add one with [`with_interval`](Self::with_interval).
    pub fn new(metric: HashMap<String, String>, value: (f64, f64)) -> Self {
        Self {
            metric,
            value,
            interval: None,
        }
    }

    /// Attach (or clear) the measurement-uncertainty interval `(lo, hi)`.
    pub fn with_interval(mut self, interval: Option<(f64, f64)>) -> Self {
        self.interval = interval;
        self
    }
}

/// A matrix sample with multiple values over time.
///
/// `#[non_exhaustive]`: build with [`MatrixSample::new`] +
/// [`with_intervals`](MatrixSample::with_intervals).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MatrixSample {
    pub metric: HashMap<String, String>,
    pub values: Vec<(f64, f64)>, // Vec of (timestamp_seconds, value)
    /// Per-value uncertainty band, present only when EVERY value has one.
    ///
    /// Kept at its original type and semantics for consumers that predate
    /// [`bands`](Self::bands); a series where only some points carry a band
    /// reports `None` here and the full picture in `bands`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intervals: Option<Vec<(f64, f64)>>,
    /// Per-value uncertainty band, present when ANY value has one, with `None`
    /// at the values that do not.
    ///
    /// A band is absent at a point whose interval nobody observed — see
    /// [`interpolated`](Self::interpolated). `intervals` cannot express that
    /// (one missing band drops the whole array), which is why this exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bands: Option<Vec<Option<(f64, f64)>>>,
    /// Which values are interpolated across a span the producer did not read,
    /// parallel to `values`. Present when any value is.
    ///
    /// Such a point still carries a value — the total across the hole is known
    /// even though its distribution inside is not — but no band, since the
    /// honest bound on an unobserved interval is not a number. Renderers use
    /// this to distinguish an interpolated point from a measured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpolated: Option<Vec<bool>>,
}

impl MatrixSample {
    /// A matrix sample with no uncertainty band. Add one with
    /// [`with_intervals`](Self::with_intervals).
    pub fn new(metric: HashMap<String, String>, values: Vec<(f64, f64)>) -> Self {
        Self {
            metric,
            values,
            intervals: None,
            bands: None,
            interpolated: None,
        }
    }

    /// Attach (or clear) the per-value uncertainty band, parallel to `values`.
    pub fn with_intervals(mut self, intervals: Option<Vec<(f64, f64)>>) -> Self {
        self.intervals = intervals;
        self
    }

    /// Attach (or clear) the per-value bands, parallel to `values`, where a
    /// `None` entry marks a value with no band.
    pub fn with_bands(mut self, bands: Option<Vec<Option<(f64, f64)>>>) -> Self {
        self.bands = bands;
        self
    }

    /// Attach (or clear) the per-value interpolated markers, parallel to
    /// `values`.
    pub fn with_interpolated(mut self, interpolated: Option<Vec<bool>>) -> Self {
        self.interpolated = interpolated;
        self
    }
}

/// Histogram heatmap data for visualization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramHeatmapResult {
    /// Timestamps in seconds
    pub timestamps: Vec<f64>,
    /// Bucket boundaries (latency values in the histogram's unit, e.g.,
    /// nanoseconds)
    pub bucket_bounds: Vec<u64>,
    /// Heatmap data as [time_index, bucket_index, count]
    pub data: Vec<(usize, usize, f64)>,
    /// Minimum count value (for color scaling)
    pub min_value: f64,
    /// Maximum count value (for color scaling)
    pub max_value: f64,
}

/// Result of a PromQL query
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "camelCase")]
pub enum QueryResult {
    #[serde(rename = "vector")]
    Vector { result: Vec<Sample> },

    #[serde(rename = "matrix")]
    Matrix { result: Vec<MatrixSample> },

    #[serde(rename = "scalar")]
    Scalar { result: (f64, f64) }, // (timestamp, value)

    #[serde(rename = "histogram_heatmap")]
    HistogramHeatmap { result: HistogramHeatmapResult },
}

/// Where to read histogram `name` from for a range starting at `start_ns`:
/// two of its rows earlier, so the range's first row has the row before it
/// to take a delta from. An unstrided call emits points from `start_ns` on; a
/// strided call passes the opening of the window holding its range's start
/// (`window_read_start`).
pub(crate) fn histogram_read_start(
    source: &dyn crate::DataSource,
    name: &str,
    start_ns: u64,
) -> u64 {
    let interval_ns = (source.histogram_interval(name).max(0.0) * 1e9) as u64;
    start_ns.saturating_sub(2 * interval_ns)
}

/// The most windows a strided call shorter than its metric's row interval
/// may lay over its range. A row's delta is shared over every window its
/// interval overlaps, so with such a stride the work grows with the windows
/// rather than the rows.
const MAX_STRIDE_WINDOWS: u64 = 100_000;

/// A strided call's windows: anchored at the range's end for an instant
/// query, so its window is the stride ending there, and at multiples of the
/// stride for a range query. A stride shorter than the metric's row
/// interval (`interval_s`), or with no interval known, laying more than
/// [`MAX_STRIDE_WINDOWS`] windows is refused.
fn stride_windows(
    ns: u64,
    interval_s: f64,
    start_ns: u64,
    end_ns: u64,
    opts: &QueryOptions,
) -> Result<Stride, QueryError> {
    let stride = Stride {
        ns,
        anchor_ns: if opts.instant { end_ns } else { 0 },
        emit_from_ns: if opts.instant { 0 } else { start_ns },
    };
    let windows = end_ns.saturating_sub(window_read_start(Some(stride), start_ns)) / ns + 1;
    // An interval that is not a positive number says nothing about the
    // rows, so the stride is capped as if it were shorter.
    let known = interval_s.is_finite() && interval_s > 0.0;
    let shorter = !known || (ns as f64) < interval_s * 1e9;
    if shorter && windows > MAX_STRIDE_WINDOWS {
        return Err(QueryError::Unsupported(format!(
            "a stride of {} s, shorter than the {interval_s} s row interval, lays {windows} \
             windows over this range, more than {MAX_STRIDE_WINDOWS}; use a longer stride or none",
            ns as f64 / 1e9
        )));
    }
    Ok(stride)
}

/// Where a call's rows are needed from: `start_ns`, or for a strided call the
/// opening of the window holding it, so that window is whole.
fn window_read_start(stride: Option<Stride>, start_ns: u64) -> u64 {
    match stride {
        Some(s) => start_ns.min(s.opens_ns(s.window(start_ns))),
        None => start_ns,
    }
}

/// The most an unstrided instant query of a histogram function reads back, in
/// seconds, and what it reads back when no metric it names has a positive
/// interval: Prometheus's default lookback for an instant vector.
const INSTANT_LOOKBACK_S: f64 = 300.0;

/// How many of a histogram's row intervals an unstrided instant query reads
/// back. A series whose latest row is older than that has no value at the
/// instant, nor has a series whose row before its latest is more than five
/// row intervals before the instant: that row, which the delta needs, is not
/// read.
const INSTANT_LOOKBACK_ROWS: f64 = 3.0;

/// How far back an instant query of a histogram function reads, in
/// seconds: [`INSTANT_LOOKBACK_ROWS`] times the longest
/// `DataSource::histogram_interval` among the metrics `query` names, at most
/// [`INSTANT_LOOKBACK_S`]. A strided call reads back nothing here: its one
/// window is the stride ending at the instant (`stride_windows`), which its
/// handler reads from that window's opening (`window_read_start`). A source
/// with no per-histogram interval answers its `interval()` (1 s for a
/// parquet file or a `MemoryStore` without `sampling_interval_ms`).
fn instant_histogram_lookback(source: &dyn crate::DataSource, query: &str) -> f64 {
    let strided =
        histogram_calls(query).any(|args| matches!(parse_optional_stride(args), Ok((_, Some(_)))));
    if strided {
        return 0.0;
    }
    // A gauge histogram is read at the instant itself, with a gauge's
    // staleness, and needs no row before it.
    let names = crate::referenced_metrics(query).unwrap_or_default();
    let gauge_histograms = source.gauge_histogram_names();
    if !names.is_empty() && names.iter().all(|n| gauge_histograms.contains(n)) {
        return 0.0;
    }
    let interval = names
        .iter()
        .map(|name| source.histogram_interval(name))
        .filter(|i| i.is_finite() && *i > 0.0)
        .fold(0.0, f64::max);
    if interval > 0.0 {
        (INSTANT_LOOKBACK_ROWS * interval).min(INSTANT_LOOKBACK_S)
    } else {
        INSTANT_LOOKBACK_S
    }
}

/// Whether `query` calls one of the histogram functions that give a value
/// per tick, with or without a `by`/`without` clause after its name.
/// `histogram_heatmap` is left out: an unstrided one's instant is the
/// heatmap of the one-point range, and a strided one's the stride ending at
/// the instant.
fn calls_histogram_function(query: &str) -> bool {
    histogram_calls(query).next().is_some()
}

/// The argument text of each call in `query` to a histogram function that
/// gives a value per tick (see [`calls_histogram_function`]). Text inside a
/// string literal or a `#` comment is neither a call nor a parenthesis.
fn histogram_calls(query: &str) -> impl Iterator<Item = &str> {
    // The offset of the `)` closing a `(` just before `from`, outside string
    // literals.
    fn close(bytes: &[u8], quoted: &[bool], from: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (i, &b) in bytes.iter().enumerate().skip(from) {
            if quoted[i] {
                continue;
            }
            match b {
                b'(' => depth += 1,
                b')' if depth == 0 => return Some(i),
                b')' => depth -= 1,
                _ => {}
            }
        }
        None
    }
    const FUNCTIONS: [&str; 7] = [
        "histogram_quantile",
        "histogram_quantiles",
        "histogram_percentiles",
        "histogram_mean",
        "histogram_count",
        "histogram_sum",
        "histogram_irate",
    ];
    let bytes = query.as_bytes();
    // Byte offsets inside a string literal (`"..."`, `'...'` or `` `...` ``,
    // each taking backslash escapes as promql-parser lexes them) or a `#`
    // comment, which runs to the end of the line: there a function name or a
    // parenthesis is text.
    let mut quoted = vec![false; bytes.len()];
    let mut open: Option<u8> = None;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        match open {
            Some(q) => {
                quoted[i] = true;
                if escaped {
                    escaped = false;
                } else if b == b'\\' && q != b'\n' {
                    escaped = true;
                } else if b == q || (q == b'\n' && b == b'\r') {
                    open = None;
                }
            }
            None if matches!(b, b'"' | b'\'' | b'`' | b'#') => {
                open = Some(if b == b'#' { b'\n' } else { b });
                quoted[i] = true;
            }
            None => {}
        }
    }
    let skip_ws = move |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    query
        .match_indices("histogram_")
        .filter_map(move |(at, _)| {
            if quoted[at]
                || (at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_'))
            {
                return None;
            }
            let name_len = bytes[at..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                .count();
            if !FUNCTIONS.contains(&&query[at..at + name_len]) {
                return None;
            }
            let mut i = skip_ws(at + name_len);
            for keyword in ["by", "without"] {
                if query[i..].starts_with(keyword) {
                    let k = skip_ws(i + keyword.len());
                    if bytes.get(k) == Some(&b'(') {
                        i = skip_ws(close(bytes, &quoted, k + 1)? + 1);
                    }
                }
            }
            if bytes.get(i) != Some(&b'(') {
                return None;
            }
            Some(&query[i + 1..close(bytes, &quoted, i + 1)?])
        })
}

/// How far beyond a query range's bounds samples are still read, in
/// nanoseconds.
///
/// Ranges arrive as `f64` seconds. Converting a sample's own time to seconds
/// and back is off by up to about 130 ns at the current epoch, and stays
/// under 1 µs until about 2115, so a bound computed from a sample's time can
/// leave that sample out. A grid point up to this far past `end` can
/// therefore be emitted, when `end` falls just before a step boundary.
pub(crate) const RANGE_SLACK_NS: u64 = 1_000;

/// A query range's start in nanoseconds, rounded to the nearest. The grid
/// snaps this down to a step boundary, so it takes no slack; a sample at the
/// start is read through the rate's lookback or a gauge's staleness window.
pub(crate) fn range_start_ns(start: f64) -> u64 {
    (start * 1e9).round() as u64
}

/// Where the histogram handlers start emitting: the range's start, rounded,
/// then moved [`RANGE_SLACK_NS`] earlier so a sample at the start is
/// included.
pub(crate) fn range_read_start_ns(start: f64) -> u64 {
    range_start_ns(start).saturating_sub(RANGE_SLACK_NS)
}

/// A query range's end in nanoseconds, rounded and then widened by
/// [`RANGE_SLACK_NS`] so a sample at the end is read.
pub(crate) fn range_end_ns(end: f64) -> u64 {
    ((end * 1e9).round() as u64).saturating_add(RANGE_SLACK_NS)
}

/// The PromQL query engine, backed by any `DataSource`.
pub(crate) struct QueryEngine {
    source: Arc<dyn DataSource>,
    /// The process groups among `source`'s recordings that a leaf reads per
    /// member (`streaming::per_source`); `None` when there are none.
    groups: Option<Arc<streaming::per_source::SourceGroups>>,
}

/// Try to parse an optional stride (in seconds) from the trailing argument.
/// Input like `"metric, 15"` returns `("metric", Some(15_000_000_000))`.
/// Input like `"metric"` returns `("metric", None)`.
fn parse_optional_stride(s: &str) -> Result<(&str, Option<u64>), QueryError> {
    let (before, after) = split_last_top_level_comma(s);
    if let Some(tail) = after {
        if let Ok(secs) = tail.parse::<f64>() {
            let ns = (secs * 1_000_000_000.0) as u64;
            if secs.is_nan() || secs <= 0.0 || ns == 0 {
                return Err(QueryError::ParseError(
                    "stride must be a positive number of seconds, at least 1 ns".to_string(),
                ));
            }
            return Ok((before.trim(), Some(ns)));
        }
    }
    Ok((s, None))
}

/// Split a string at the last comma that is not inside `{}` braces.
/// Returns `(before, Some(after))` if found, or `(whole_string, None)`.
fn split_last_top_level_comma(s: &str) -> (&str, Option<&str>) {
    let mut brace_depth = 0i32;
    let mut last_comma = None;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            ',' if brace_depth == 0 => last_comma = Some(i),
            _ => {}
        }
    }
    match last_comma {
        Some(i) => (&s[..i], Some(s[i + 1..].trim())),
        None => (s, None),
    }
}

/// Owned counterpart to `streaming::GroupBy<'_>` — held on the
/// handler stack between parse and dispatch, then borrowed.
#[derive(Debug, Default, Clone)]
pub(crate) enum HistogramGroupBy {
    #[default]
    None,
    By(Vec<String>),
    Without(Vec<String>),
}

impl HistogramGroupBy {
    pub(crate) fn as_ref(&self) -> streaming::GroupBy<'_> {
        match self {
            HistogramGroupBy::None => streaming::GroupBy::Include(&[]),
            HistogramGroupBy::By(ls) => streaming::GroupBy::Include(ls),
            HistogramGroupBy::Without(ls) => streaming::GroupBy::Exclude(ls),
        }
    }
}

/// Give each selector of `expr` with no name, and exactly one `__name__="x"`
/// matcher, the name `x`, removing that matcher: `{__name__="x", a="b"}`
/// selects what `x{a="b"}` selects. This is how a query names a metric whose
/// name is not a PromQL identifier (`redis/commands`).
pub(crate) fn name_selectors(expr: &mut Expr) {
    match expr {
        Expr::Aggregate(agg) => {
            name_selectors(&mut agg.expr);
            if let Some(param) = agg.param.as_mut() {
                name_selectors(param);
            }
        }
        Expr::Unary(u) => name_selectors(&mut u.expr),
        Expr::Binary(b) => {
            name_selectors(&mut b.lhs);
            name_selectors(&mut b.rhs);
        }
        Expr::Paren(p) => name_selectors(&mut p.expr),
        Expr::Call(call) => {
            for arg in call.args.args.iter_mut() {
                name_selectors(arg);
            }
        }
        Expr::VectorSelector(sel) => name_selector(sel),
        Expr::MatrixSelector(sel) => name_selector(&mut sel.vs),
        Expr::Subquery(s) => name_selectors(&mut s.expr),
        Expr::NumberLiteral(_) | Expr::StringLiteral(_) | Expr::Extension(_) => {}
    }
}

fn name_selector(sel: &mut parser::VectorSelector) {
    if sel.name.is_some() || !sel.matchers.or_matchers.is_empty() {
        return;
    }
    let mut names = sel
        .matchers
        .matchers
        .iter()
        .enumerate()
        .filter(|(_, m)| m.name == "__name__");
    let (Some((at, m)), None) = (names.next(), names.next()) else {
        return;
    };
    if m.op != promql_parser::label::MatchOp::Equal {
        return;
    }
    let name = m.value.clone();
    sel.matchers.matchers.remove(at);
    sel.name = Some(name);
}

/// A histogram function's selector that carries a `__name__` matcher,
/// parsed as PromQL and named by `name_selector`: its metric name and the
/// filter of its other matchers. `None` when `selector` does not parse as a
/// PromQL vector selector with a `__name__` matcher; the caller parses it by
/// hand. An error for an `offset` or `@`, or for `__name__` matchers that do
/// not name one metric.
fn name_matcher_selector(selector: &str) -> Option<Result<(String, Labels), QueryError>> {
    let Ok(Expr::VectorSelector(mut sel)) = parser::parse(selector) else {
        return None;
    };
    if !sel.matchers.matchers.iter().any(|m| m.name == "__name__") {
        return None;
    }
    let refused = |why: &str| Some(Err(QueryError::ParseError(format!("{selector}: {why}"))));
    if sel.offset.is_some() || sel.at.is_some() {
        return refused("a histogram function's selector takes no `offset` or `@`");
    }
    name_selector(&mut sel);
    match sel.name {
        Some(name) => Some(Ok((name, extract_filter_labels(&sel.matchers.matchers)))),
        None => refused(
            "`__name__` is accepted only as a single `__name__=\"x\"` matcher on a \
             selector with no metric name",
        ),
    }
}

/// Rewrite `sum [by/without (...)] (<histogram_func>(...))` to the
/// native `<histogram_func> [by/without (...)] (...)` form. `None` when
/// `query` is not of that form. A grouping on the inner function
/// (`sum(histogram_irate by (x) (lat))`) is an error: the native form has
/// one grouping.
pub(crate) fn unwrap_sum_around_histogram(query: &str) -> Result<Option<String>, QueryError> {
    match sum_around_histogram(query) {
        None => Ok(None),
        Some(SumAround::Native(rewritten)) => Ok(Some(rewritten)),
        Some(SumAround::InnerGrouping) => Err(QueryError::ParseError(format!(
            "{query}: a histogram function inside `sum` takes no `by`/`without` \
             clause; drop it for one series (`sum(histogram_mean(lat))`), or move \
             it to the `sum` for a series per group (`sum by (x) (histogram_mean(lat))`)"
        ))),
    }
}

/// What `sum_around_histogram` found.
enum SumAround {
    /// The native form.
    Native(String),
    /// A grouping clause on the function inside the `sum`.
    InnerGrouping,
}

fn sum_around_histogram(query: &str) -> Option<SumAround> {
    let query = query.trim();
    let after_sum = query.strip_prefix("sum")?;
    if !matches!(after_sum.chars().next(), Some('(' | ' ' | '\t')) {
        return None;
    }
    let rest = after_sum.trim_start();

    let (grouping_clause, after_modifier) = if let Some(r) = rest.strip_prefix("by") {
        let (after, clause) = take_grouping_clause(r.trim_start(), "by")?;
        (Some(clause), after)
    } else if let Some(r) = rest.strip_prefix("without") {
        let (after, clause) = take_grouping_clause(r.trim_start(), "without")?;
        (Some(clause), after)
    } else {
        (None, rest)
    };

    let after_open = after_modifier.strip_prefix('(')?;
    let close = find_close_paren(after_open)?;
    if after_open[close + 1..]
        .trim_start()
        .chars()
        .next()
        .is_some()
    {
        return None;
    }
    let inner = after_open[..close].trim();

    let inner_func = [
        "histogram_irate",
        "histogram_mean",
        "histogram_count",
        "histogram_sum",
    ]
    .iter()
    .find(|f| inner.starts_with(*f))?;
    let after_name = inner.strip_prefix(*inner_func)?;
    if !matches!(after_name.chars().next(), Some('(' | ' ' | '\t')) {
        return None;
    }

    let inner_after_name = after_name.trim_start();
    for keyword in ["by", "without"] {
        if let Some(rest) = inner_after_name.strip_prefix(keyword) {
            if rest.trim_start().starts_with('(') {
                return Some(SumAround::InnerGrouping);
            }
        }
    }

    Some(SumAround::Native(match grouping_clause {
        Some(clause) => format!("{inner_func} {clause} {inner_after_name}"),
        None => inner.to_string(),
    }))
}

fn take_grouping_clause<'a>(s: &'a str, keyword: &str) -> Option<(&'a str, String)> {
    let inside = s.strip_prefix('(')?;
    let close = find_close_paren(inside)?;
    let labels = &inside[..close];
    let after = inside[close + 1..].trim_start();
    Some((after, format!("{keyword} ({labels})")))
}

/// Parse `<func> [by (labels) | without (labels)] (body)`.
pub(crate) fn parse_histogram_call<'a>(
    func: &str,
    query: &'a str,
) -> Result<Option<(&'a str, HistogramGroupBy)>, QueryError> {
    let Some(after_name) = query.strip_prefix(func) else {
        return Ok(None);
    };
    if !matches!(after_name.chars().next(), Some('(' | ' ' | '\t')) {
        return Ok(None);
    }
    let rest = after_name.trim_start();

    let (group_by, after_modifier) = if let Some(r) = rest.strip_prefix("by") {
        let (labels, after) = parse_grouping_clause(func, r.trim_start(), "by")?;
        (HistogramGroupBy::By(labels), after)
    } else if let Some(r) = rest.strip_prefix("without") {
        let (labels, after) = parse_grouping_clause(func, r.trim_start(), "without")?;
        (HistogramGroupBy::Without(labels), after)
    } else {
        (HistogramGroupBy::None, rest)
    };

    let body = after_modifier.strip_prefix('(').ok_or_else(|| {
        QueryError::ParseError(format!("{func}: expected '(' after function name"))
    })?;
    if !body.ends_with(')') {
        return Err(QueryError::ParseError(format!(
            "{func}: missing closing ')'"
        )));
    }
    Ok(Some((&body[..body.len() - 1], group_by)))
}

fn parse_grouping_clause<'a>(
    func: &str,
    s: &'a str,
    keyword: &str,
) -> Result<(Vec<String>, &'a str), QueryError> {
    let inside = s
        .strip_prefix('(')
        .ok_or_else(|| QueryError::ParseError(format!("{func}: expected '(' after '{keyword}'")))?;
    let close = find_close_paren(inside).ok_or_else(|| {
        QueryError::ParseError(format!("{func}: unbalanced parens in '{keyword}' clause"))
    })?;
    let labels = parse_label_list(&inside[..close])?;
    Ok((labels, inside[close + 1..].trim_start()))
}

fn parse_label_list(s: &str) -> Result<Vec<String>, QueryError> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(QueryError::ParseError(format!(
                "invalid label name in grouping clause: {p}"
            )));
        }
        out.push(p.to_string());
    }
    Ok(out)
}

fn find_close_paren(s: &str) -> Option<usize> {
    let mut depth: usize = 1;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract label filter from parsed PromQL matchers, skipping `__name__`.
pub(crate) fn extract_filter_labels(matchers: &[Matcher]) -> Labels {
    let mut filter_labels = Labels::default();
    for matcher in matchers {
        if matcher.name == "__name__" {
            continue;
        }
        let op = matcher.op.to_string();
        if op == "=" {
            filter_labels
                .inner
                .insert(matcher.name.clone(), matcher.value.clone());
        } else if op == "=~" {
            filter_labels
                .inner
                .insert(matcher.name.clone(), format!("~{}", matcher.value));
        } else if op == "!=" || op == "!~" {
            filter_labels
                .inner
                .insert(matcher.name.clone(), format!("!{}", matcher.value));
        }
    }
    filter_labels
}

impl QueryEngine {
    pub(crate) fn new(source: Arc<dyn DataSource>) -> Self {
        Self {
            source,
            groups: None,
        }
    }

    /// An engine over `source`, some of whose recordings form the process
    /// groups `groups` lists.
    #[cfg_attr(not(feature = "archive"), allow(dead_code))]
    pub(crate) fn with_groups(
        source: Arc<dyn DataSource>,
        groups: streaming::per_source::SourceGroups,
    ) -> Self {
        Self {
            source,
            groups: Some(Arc::new(groups)),
        }
    }

    /// The underlying `DataSource` this engine queries against — an `Arc`
    /// clone (refcount bump), not a copy. Lets a reader that wraps a
    /// `QueryEngine` (`ParquetReader`, `SegmentedParquetReader`) hand its raw
    /// source to a composing reader (`SegmentedParquetReader` over several
    /// `ParquetReader`s; `UnionMetricsSource` over several disjoint-identity
    /// readers) without re-deriving it.
    pub(crate) fn data_source(&self) -> Arc<dyn DataSource> {
        Arc::clone(&self.source)
    }

    /// Time range of the underlying source in nanoseconds, or `None` if empty.
    pub(crate) fn time_range(&self) -> Option<(u64, u64)> {
        self.source.time_range()
    }

    pub(crate) fn counter_names(&self) -> Vec<String> {
        self.source.counter_names()
    }
    pub(crate) fn gauge_names(&self) -> Vec<String> {
        self.source.gauge_names()
    }
    pub(crate) fn histogram_names(&self) -> Vec<String> {
        self.source.histogram_names()
    }
    pub(crate) fn gauge_histogram_names(&self) -> Vec<String> {
        self.source.gauge_histogram_names()
    }
    pub(crate) fn gauge_histogram_labels(
        &self,
        name: &str,
    ) -> Vec<std::collections::BTreeMap<String, String>> {
        self.source.gauge_histogram_labels(name)
    }
    pub(crate) fn counter_labels(
        &self,
        name: &str,
    ) -> Vec<std::collections::BTreeMap<String, String>> {
        self.source.counter_labels(name)
    }
    pub(crate) fn gauge_labels(
        &self,
        name: &str,
    ) -> Vec<std::collections::BTreeMap<String, String>> {
        self.source.gauge_labels(name)
    }
    pub(crate) fn histogram_labels(
        &self,
        name: &str,
    ) -> Vec<std::collections::BTreeMap<String, String>> {
        self.source.histogram_labels(name)
    }
    pub(crate) fn interval(&self) -> f64 {
        self.source.interval()
    }
    pub(crate) fn file_metadata(&self) -> std::collections::HashMap<String, String> {
        self.source.file_metadata()
    }

    /// Look up a single metadata value by key without cloning the full map.
    pub(crate) fn metadata_get(&self, key: &str) -> Option<String> {
        self.source.metadata_get(key)
    }

    /// Execute an instant query at a single timestamp.
    /// Uses the latest available timestamp when `time` is `None`.
    pub(crate) fn query(
        &self,
        query_str: &str,
        time: Option<f64>,
    ) -> Result<QueryResult, QueryError> {
        let target = time.unwrap_or_else(|| {
            self.source
                .time_range()
                .map(|(_, max_ns)| max_ns as f64 / 1e9)
                .unwrap_or(0.0)
        });
        let step = self.source.interval().max(1.0);
        // A histogram function emits at the rows it reads, which an instant
        // rarely falls on, and needs the row before for a delta: it reads
        // back a few rows, and each series' latest point is kept below.
        let start = if calls_histogram_function(query_str) {
            target - instant_histogram_lookback(&*self.source, query_str)
        } else {
            target
        };
        let opts = QueryOptions {
            instant: true,
            ..QueryOptions::default()
        };
        let result = self.query_range_opts(query_str, start, target, step, &opts)?;
        let QueryResult::Matrix { result: samples } = result else {
            return Ok(result);
        };
        let vector: Vec<Sample> = samples
            .into_iter()
            .filter_map(|s| {
                s.values.last().copied().map(|value| Sample {
                    metric: s.metric.clone(),
                    value,
                    interval: s.intervals.as_ref().and_then(|iv| iv.last().copied()),
                })
            })
            .collect();
        Ok(QueryResult::Vector { result: vector })
    }

    fn parse_metric_selector(&self, selector: &str) -> Result<(String, Labels), QueryError> {
        if let Some(named) = name_matcher_selector(selector) {
            return named;
        }
        if let Some(brace_pos) = selector.find('{') {
            let metric_name = selector[..brace_pos].trim().to_string();
            let labels_part = &selector[brace_pos + 1..selector.len() - 1];

            let mut labels = Labels::default();
            for part in labels_part.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }

                let (key, value, negate) = if let Some(pos) = part.find("!=") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), true)
                } else if let Some(pos) = part.find("!~") {
                    (&part[..pos], part[pos + 2..].trim().trim_matches('"'), true)
                } else if let Some(pos) = part.find("=~") {
                    (
                        &part[..pos],
                        part[pos + 2..].trim().trim_matches('"'),
                        false,
                    )
                } else if let Some(pos) = part.find('=') {
                    (
                        &part[..pos],
                        part[pos + 1..].trim().trim_matches('"'),
                        false,
                    )
                } else {
                    continue;
                };

                let key = key.trim().to_string();
                if negate {
                    labels.inner.insert(key, format!("!{value}"));
                } else {
                    labels.inner.insert(key, value.to_string());
                }
            }
            Ok((metric_name, labels))
        } else {
            Ok((selector.to_string(), Labels::default()))
        }
    }

    fn evaluate_expr(
        &self,
        expr: &Expr,
        start: f64,
        end: f64,
        step: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        streaming::dispatch::try_streaming(
            &*self.source,
            self.groups.as_deref(),
            expr,
            start,
            end,
            step,
            opts,
        )
    }

    fn handle_histogram_quantiles(
        &self,
        query_str: &str,
        (start, end, step): (f64, f64, f64),
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        // Accept both `histogram_quantiles(` and `histogram_percentiles(` as
        // synonyms — quantile and percentile are interchangeable terms for
        // the [0.0, 1.0] array form used here.
        let prefix_len = if query_str.starts_with("histogram_percentiles(") {
            "histogram_percentiles(".len()
        } else {
            "histogram_quantiles(".len()
        };
        let inner = &query_str[prefix_len..query_str.len() - 1];

        let array_start = inner.find('[').ok_or_else(|| {
            QueryError::ParseError(
                "histogram_quantiles first argument must be an array of quantiles".to_string(),
            )
        })?;
        let array_end = inner.find(']').ok_or_else(|| {
            QueryError::ParseError("Missing closing bracket in quantiles array".to_string())
        })?;

        let array_str = &inner[array_start + 1..array_end];
        let quantiles: Vec<f64> = array_str
            .split(',')
            .map(|s| s.trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                QueryError::ParseError(format!("Failed to parse quantile value: {}", e))
            })?;

        if quantiles.is_empty() {
            return Err(QueryError::ParseError(
                "Quantiles array cannot be empty".to_string(),
            ));
        }

        for &q in &quantiles {
            if !(0.0..=1.0).contains(&q) {
                return Err(QueryError::ParseError(format!(
                    "histogram_quantiles values must be between 0.0 and 1.0, got {}",
                    q
                )));
            }
        }

        let after_array = &inner[array_end + 1..].trim();
        let remaining = after_array
            .strip_prefix(',')
            .map(|s| s.trim())
            .ok_or_else(|| {
                QueryError::ParseError(
                    "histogram_quantiles requires a metric name as second argument".to_string(),
                )
            })?;

        let (metric_selector, stride_ns) = parse_optional_stride(remaining)?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        if reads_gauge_histogram(self.source.as_ref(), &metric_name)? {
            if stride_ns.is_some() {
                return Err(gauge_histogram_refused(
                    &metric_name,
                    "a stride, which pools the observations between rows,",
                ));
            }
            let grid = state_grid(start, end, step, opts);
            let read = read_gauge_histogram(
                self.source.as_ref(),
                &metric_name,
                &labels,
                grid,
                opts.eval_timestamps.as_deref(),
            );
            let Some((stream, times, staleness_ns)) = read else {
                nothing_read(
                    self.source.as_ref(),
                    &metric_name,
                    MetricKind::GaugeHistogram,
                )?;
                return Ok(QueryResult::Matrix { result: vec![] });
            };
            let result = streaming::gauge_histogram::quantiles(
                stream,
                &quantiles,
                &times,
                staleness_ns,
                &metric_name,
            );
            return Ok(QueryResult::Matrix { result });
        }

        let start_ns = range_read_start_ns(start);
        let end_ns = range_end_ns(end);
        let stride = stride_ns
            .map(|ns| {
                let interval = self.source.histogram_interval(&metric_name);
                stride_windows(ns, interval, start_ns, end_ns, opts)
            })
            .transpose()?;
        let stream = self.source.histogram_stream(
            &metric_name,
            &labels,
            histogram_read_start(
                self.source.as_ref(),
                &metric_name,
                window_read_start(stride, start_ns),
            ),
            end_ns,
        );
        let Some(stream) = stream else {
            nothing_read(self.source.as_ref(), &metric_name, MetricKind::Histogram)?;
            return Ok(QueryResult::Matrix { result: vec![] });
        };
        let result = stream.quantiles(&quantiles, start_ns, end_ns, stride, &metric_name);
        if result.is_empty() {
            return Ok(QueryResult::Matrix { result: vec![] });
        }
        Ok(QueryResult::Matrix { result })
    }

    fn handle_histogram_heatmap(
        &self,
        query_str: &str,
        (start, end, step): (f64, f64, f64),
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        let inner = &query_str["histogram_heatmap(".len()..query_str.len() - 1];
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        if reads_gauge_histogram(self.source.as_ref(), &metric_name)? {
            if stride_ns.is_some() {
                return Err(gauge_histogram_refused(
                    &metric_name,
                    "a stride, which pools the observations between rows,",
                ));
            }
            let grid = state_grid(start, end, step, opts);
            let read = read_gauge_histogram(
                self.source.as_ref(),
                &metric_name,
                &labels,
                grid,
                opts.eval_timestamps.as_deref(),
            );
            let Some((stream, times, staleness_ns)) = read else {
                nothing_read(
                    self.source.as_ref(),
                    &metric_name,
                    MetricKind::GaugeHistogram,
                )?;
                return Ok(QueryResult::Matrix { result: vec![] });
            };
            return Ok(
                match streaming::gauge_histogram::heatmap(stream, &times, staleness_ns) {
                    Some(result) => QueryResult::HistogramHeatmap { result },
                    None => QueryResult::Matrix { result: vec![] },
                },
            );
        }

        let start_ns = range_read_start_ns(start);
        let end_ns = range_end_ns(end);
        let stride = stride_ns
            .map(|ns| {
                let interval = self.source.histogram_interval(&metric_name);
                stride_windows(ns, interval, start_ns, end_ns, opts)
            })
            .transpose()?;
        let stream = self.source.histogram_stream(
            &metric_name,
            &labels,
            histogram_read_start(
                self.source.as_ref(),
                &metric_name,
                window_read_start(stride, start_ns),
            ),
            end_ns,
        );
        let Some(stream) = stream else {
            nothing_read(self.source.as_ref(), &metric_name, MetricKind::Histogram)?;
            return Ok(QueryResult::Matrix { result: vec![] });
        };
        let result = stream.heatmap(start_ns, end_ns, stride);
        match result {
            Some(result) => Ok(QueryResult::HistogramHeatmap { result }),
            None => Ok(QueryResult::Matrix { result: vec![] }),
        }
    }

    fn handle_histogram_irate(
        &self,
        inner: &str,
        group_by: HistogramGroupBy,
        start: f64,
        end: f64,
    ) -> Result<QueryResult, QueryError> {
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        if stride_ns.is_some() {
            return Err(QueryError::ParseError(
                "histogram_irate does not accept a window/stride argument".to_string(),
            ));
        }
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;
        if reads_gauge_histogram(self.source.as_ref(), &metric_name)? {
            return Err(gauge_histogram_refused(&metric_name, "histogram_irate"));
        }

        let start_ns = range_read_start_ns(start);
        let end_ns = range_end_ns(end);
        let stream = self.source.histogram_stream(
            &metric_name,
            &labels,
            histogram_read_start(self.source.as_ref(), &metric_name, start_ns),
            end_ns,
        );
        let Some(stream) = stream else {
            nothing_read(self.source.as_ref(), &metric_name, MetricKind::Histogram)?;
            return Ok(QueryResult::Matrix { result: vec![] });
        };
        let result = stream.irate(group_by.as_ref(), start_ns, end_ns, &metric_name);
        if result.is_empty() {
            return Ok(QueryResult::Matrix { result: vec![] });
        }
        Ok(QueryResult::Matrix { result })
    }

    fn handle_histogram_scalar(
        &self,
        func: &str,
        inner: &str,
        group_by: HistogramGroupBy,
        (start, end, step): (f64, f64, f64),
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        let (metric_selector, stride_ns) = parse_optional_stride(inner.trim())?;
        let (metric_name, labels) = self.parse_metric_selector(metric_selector)?;

        if reads_gauge_histogram(self.source.as_ref(), &metric_name)? {
            if stride_ns.is_some() {
                return Err(gauge_histogram_refused(
                    &metric_name,
                    "a stride, which pools the observations between rows,",
                ));
            }
            let grid = state_grid(start, end, step, opts);
            let read = read_gauge_histogram(
                self.source.as_ref(),
                &metric_name,
                &labels,
                grid,
                opts.eval_timestamps.as_deref(),
            );
            let Some((stream, times, staleness_ns)) = read else {
                nothing_read(
                    self.source.as_ref(),
                    &metric_name,
                    MetricKind::GaugeHistogram,
                )?;
                return Ok(QueryResult::Matrix { result: vec![] });
            };
            let reduce = match func {
                "histogram_mean" => streaming::gauge_histogram::Reduce::Mean,
                "histogram_sum" => streaming::gauge_histogram::Reduce::Sum,
                _ => streaming::gauge_histogram::Reduce::Count,
            };
            let result = streaming::gauge_histogram::reduce(
                stream,
                reduce,
                group_by.as_ref(),
                &times,
                staleness_ns,
                &metric_name,
            );
            return Ok(QueryResult::Matrix { result });
        }

        let start_ns = range_read_start_ns(start);
        let end_ns = range_end_ns(end);
        let stride = stride_ns
            .map(|ns| {
                let interval = self.source.histogram_interval(&metric_name);
                stride_windows(ns, interval, start_ns, end_ns, opts)
            })
            .transpose()?;
        let stream = self.source.histogram_stream(
            &metric_name,
            &labels,
            histogram_read_start(
                self.source.as_ref(),
                &metric_name,
                window_read_start(stride, start_ns),
            ),
            end_ns,
        );
        let Some(stream) = stream else {
            nothing_read(self.source.as_ref(), &metric_name, MetricKind::Histogram)?;
            return Ok(QueryResult::Matrix { result: vec![] });
        };
        let group = group_by.as_ref();
        let result = match func {
            "histogram_mean" => stream.mean(group, start_ns, end_ns, stride, &metric_name),
            "histogram_sum" => stream.sum(group, start_ns, end_ns, stride, &metric_name),
            _ => stream.count(group, start_ns, end_ns, stride, &metric_name),
        };
        if result.is_empty() {
            return Ok(QueryResult::Matrix { result: vec![] });
        }
        Ok(QueryResult::Matrix { result })
    }

    pub fn query_range(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
        step: f64,
    ) -> Result<QueryResult, QueryError> {
        self.query_range_opts(query_str, start, end, step, &QueryOptions::default())
    }

    /// Evaluates `query_str` over `[start, end]`.
    ///
    /// A selector naming a metric of the kind its function reads, with no
    /// samples in the range or none matching its labels, gives an empty
    /// matrix; `query` gives an empty vector, and `histogram_heatmap` an
    /// empty matrix rather than a heatmap. A name the source holds only as
    /// another kind is [`QueryError::Unsupported`]. [`QueryError::MetricNotFound`]
    /// means the source holds no metric of that name.
    pub fn query_range_opts(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
        step: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        self.evaluate_range(query_str, start, end, step, opts)
    }

    /// [`query_range_opts`](Self::query_range_opts) in display form; see
    /// [`crate::MetricsSource::query_range_display_opts`]. A series from the
    /// streaming engine is reduced as it is collected.
    pub(crate) fn query_range_display_opts(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
        step: f64,
        display: &crate::DisplayOptions,
        opts: &QueryOptions,
    ) -> Result<crate::DisplayResult, QueryError> {
        self.evaluate_range_as(
            query_str,
            (start, end, step),
            opts,
            |r| crate::display::display_from_result(r, start, end, step, display),
            |expr| {
                streaming::dispatch::try_streaming_display(
                    &*self.source,
                    self.groups.as_deref(),
                    expr,
                    start,
                    end,
                    step,
                    opts,
                    display,
                )
            },
        )
    }

    fn evaluate_range(
        &self,
        query_str: &str,
        start: f64,
        end: f64,
        step: f64,
        opts: &QueryOptions,
    ) -> Result<QueryResult, QueryError> {
        self.evaluate_range_as(
            query_str,
            (start, end, step),
            opts,
            |r| r,
            |expr| self.evaluate_expr(expr, start, end, step, opts),
        )
    }

    /// Evaluate `query_str`, converting the result of a histogram function
    /// with `from_result` and evaluating any other expression with `expr`.
    fn evaluate_range_as<T>(
        &self,
        query_str: &str,
        (start, end, step): (f64, f64, f64),
        opts: &QueryOptions,
        from_result: impl Fn(QueryResult) -> T,
        expr: impl FnOnce(&Expr) -> Result<T, QueryError>,
    ) -> Result<T, QueryError> {
        if (query_str.starts_with("histogram_quantiles(")
            || query_str.starts_with("histogram_percentiles("))
            && query_str.ends_with(")")
        {
            return self
                .handle_histogram_quantiles(query_str, (start, end, step), opts)
                .map(from_result);
        }

        if query_str.starts_with("histogram_heatmap(") && query_str.ends_with(")") {
            return self
                .handle_histogram_heatmap(query_str, (start, end, step), opts)
                .map(from_result);
        }

        let rewritten = unwrap_sum_around_histogram(query_str)?;
        let query_str: &str = rewritten.as_deref().unwrap_or(query_str);

        for func in ["histogram_mean", "histogram_count", "histogram_sum"] {
            if let Some((inner, group_by)) = parse_histogram_call(func, query_str)? {
                return self
                    .handle_histogram_scalar(func, inner, group_by, (start, end, step), opts)
                    .map(from_result);
            }
        }
        if let Some((inner, group_by)) = parse_histogram_call("histogram_irate", query_str)? {
            return self
                .handle_histogram_irate(inner, group_by, start, end)
                .map(from_result);
        }

        match parser::parse(query_str) {
            Ok(mut parsed) => {
                name_selectors(&mut parsed);
                expr(&parsed)
            }
            Err(err) => {
                let error_msg = format!("{:?}", err);
                if error_msg.contains("invalid promql query") && query_str.contains(" by ") {
                    Err(QueryError::ParseError(
                        "Invalid query syntax. Aggregation operators require parentheses around the expression, e.g., 'sum by (id) (irate(metric[5m]))' not 'sum by (id) irate(metric[5m])'".to_string()
                    ))
                } else {
                    Err(QueryError::ParseError(format!(
                        "Failed to parse query: {}",
                        error_msg
                    )))
                }
            }
        }
    }
}
