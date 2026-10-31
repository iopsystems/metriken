//! The plot spec. A [`Plot`] describes what one chart shows and how: its
//! query, metric type, unit system and axis format. A `Chart` in
//! `assets/charts/chart.js` renders it. Serialized with serde, a `Plot` is the
//! object a page passes as `Chart`'s `spec`, after the page has run the query
//! and written the result into it.
//!
//! [`Plot::gauge`], [`Plot::counter`], [`Plot::histogram`] and
//! [`Plot::histogram_latency`] return a [`PlotBuilder`]. Its methods set the
//! presentation (`with_description`, `with_log_scale`, `range` and the
//! rest), the total query (`with_total_query`) and the scope (`with_scope`),
//! and [`PlotBuilder::build`] takes the query and returns the `Plot`.

use serde::{Deserialize, Serialize};

/// What one chart shows and how. A `Plot` serializes with `data: []` and none
/// of the result fields (`min_value`, `max_value`, `time_data`,
/// `formatted_time_data`, `series_names`); the page adds them.
#[derive(Serialize, Clone, Debug)]
pub struct Plot {
    data: Vec<Vec<f64>>,
    opts: PlotOpts,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_data: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    formatted_time_data: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_names: Option<Vec<String>>,
    promql_query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    promql_query_total: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<Vec<String>>,
}

impl Plot {
    /// A gauge: a point-in-time value (e.g., memory usage, temperature).
    pub fn gauge<T: Into<String>, U: Into<String>>(title: T, id: U, unit: Unit) -> PlotBuilder {
        PlotBuilder::new(PlotOpts::new(title, id, MetricType::Gauge, None, unit))
    }

    /// A delta counter: the rate of change of a cumulative counter (e.g., CPU
    /// usage rate, packet rate).
    pub fn counter<T: Into<String>, U: Into<String>>(title: T, id: U, unit: Unit) -> PlotBuilder {
        PlotBuilder::new(PlotOpts::new(
            title,
            id,
            MetricType::DeltaCounter,
            None,
            unit,
        ))
    }

    /// A histogram: a distribution (e.g., latency, IO size), drawn as `view`
    /// selects.
    pub fn histogram<T: Into<String>, U: Into<String>>(
        title: T,
        id: U,
        unit: Unit,
        view: HistogramView,
    ) -> PlotBuilder {
        PlotBuilder::new(PlotOpts::new(
            title,
            id,
            MetricType::Histogram,
            Some(view),
            unit,
        ))
    }

    /// A percentile histogram of latency, on a log scale with a 0 to 100s
    /// range.
    pub fn histogram_latency<T: Into<String>, U: Into<String>>(title: T, id: U) -> PlotBuilder {
        Self::histogram(title, id, Unit::Time, HistogramView::Percentiles)
            .with_log_scale(true)
            .range(0.0, 100_000_000_000.0)
    }

    /// The PromQL query the chart draws.
    pub fn query(&self) -> &str {
        &self.promql_query
    }

    /// The query the line chart's Mean/Total toggle draws when ticked.
    pub fn total_query(&self) -> Option<&str> {
        self.promql_query_total.as_deref()
    }

    /// The value passed as `scope` to the page's data functions (see
    /// `charts/host.js`); rezolus sets it to the uuids of the recordings the
    /// queries read. `None` when unset.
    pub fn scope(&self) -> Option<&[String]> {
        self.scope.as_deref()
    }

    /// The query, then the total query if there is one, for a page that
    /// rewrites every query after building its plots.
    pub fn queries_mut(&mut self) -> impl Iterator<Item = &mut String> {
        std::iter::once(&mut self.promql_query).chain(self.promql_query_total.as_mut())
    }

    /// Set the value passed as `scope` to the page's data functions; `None`
    /// leaves `scope` out of the spec.
    pub fn set_scope(&mut self, scope: Option<Vec<String>>) {
        self.scope = scope;
    }
}

/// Builds a [`Plot`]; made by [`Plot::gauge`], [`Plot::counter`],
/// [`Plot::histogram`] and [`Plot::histogram_latency`].
#[derive(Clone, Debug)]
#[must_use]
pub struct PlotBuilder {
    opts: PlotOpts,
    total: Option<String>,
    scope: Option<Vec<String>>,
}

impl PlotBuilder {
    fn new(opts: PlotOpts) -> Self {
        Self {
            opts,
            total: None,
            scope: None,
        }
    }

    /// The plot, drawing `query`.
    pub fn build<T: Into<String>>(self, query: T) -> Plot {
        Plot {
            data: Vec::new(),
            opts: self.opts,
            min_value: None,
            max_value: None,
            time_data: None,
            formatted_time_data: None,
            series_names: None,
            promql_query: query.into(),
            promql_query_total: self.total,
            scope: self.scope,
        }
    }

    /// The query the line chart's Mean/Total toggle draws when ticked, such
    /// as `histogram_sum(x)` for a plot of `histogram_mean(x)`. Without one,
    /// the line chart has no toggle.
    pub fn with_total_query<T: Into<String>>(mut self, query: T) -> Self {
        self.total = Some(query.into());
        self
    }

    /// The value passed as `scope` to the page's data functions (see
    /// `charts/host.js`); rezolus sets it to the uuids of the recordings the
    /// queries read. Without it, `scope` is absent from the spec.
    pub fn with_scope(mut self, scope: Vec<String>) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Sets the standard 0..1 range used for percentage metrics.
    pub fn percentage_range(self) -> Self {
        self.range(0.0, 1.0)
    }

    /// Set the unit system the charts scale and label values with.
    pub fn with_unit_system(mut self, unit: Unit) -> Self {
        self.opts.format.unit_system = Some(unit.to_string());
        self
    }

    /// A subtitle for the chart. The chart modules do not draw it; the page
    /// does.
    pub fn with_description<T: Into<String>>(mut self, text: T) -> Self {
        self.opts.description = Some(text.into());
        self
    }

    /// The subtitle set by [`with_description`](Self::with_description).
    pub fn description(&self) -> Option<&str> {
        self.opts.description.as_deref()
    }

    /// Sets the unit system to `unit` when it names a [`Unit`], and otherwise
    /// leaves the plot unchanged. A caller that needs to reject an unknown
    /// name parses it with [`Unit::from_str`](std::str::FromStr::from_str)
    /// first.
    pub fn maybe_unit_system(self, unit: Option<&str>) -> Self {
        match unit.map(str::parse::<Unit>) {
            Some(Ok(u)) => self.with_unit_system(u),
            Some(Err(_)) | None => self,
        }
    }

    /// The percentiles a percentile histogram plot draws, as fractions
    /// (0.5 for p50).
    pub fn with_percentiles(mut self, percentiles: Vec<f64>) -> Self {
        self.opts.percentiles = Some(percentiles);
        self
    }

    /// Set the y-axis label.
    pub fn with_axis_label<T: Into<String>>(mut self, y_label: T) -> Self {
        self.opts.format.y_axis_label = Some(y_label.into());
        self
    }

    /// Set the entity each heatmap row represents in tooltips (e.g. "GPU").
    pub fn with_row_label<T: Into<String>>(mut self, row_label: T) -> Self {
        self.opts.format.row_label = Some(row_label.into());
        self
    }

    /// Set the chart style. For a gauge or counter plot, `resolveStyle` in
    /// `charts/metric_types.js` otherwise picks it from the result: a
    /// per-device query drawn as a heatmap with two devices is drawn as a
    /// line with one. `resolvedStyle` uses this value ahead of that
    /// inference.
    pub fn with_style(mut self, style: ChartStyle) -> Self {
        self.opts.style = Some(style);
        self
    }

    /// Draw the y-axis of a line, multi-series or percentile chart on a log
    /// scale (`true`) or a linear one (`false`, the default). The heatmaps
    /// ignore it.
    pub fn with_log_scale(mut self, log_scale: bool) -> Self {
        self.opts.format.log_scale = Some(log_scale);
        self
    }

    /// Clamp plotted values to `min..=max`. A line or multi-series chart
    /// draws a value outside the range at the bound. A percentile chart caps
    /// its y-axis at `max` and draws values above it in a band marked OOB. A
    /// heatmap clamps cell values, so its colour scale stops at `max`, and a
    /// histogram heatmap leaves out buckets above `max`. A quantile heatmap
    /// ignores the range.
    pub fn range(mut self, min: f64, max: f64) -> Self {
        self.opts.format.range = Some(Range {
            min: Some(min),
            max: Some(max),
        });
        self
    }
}

/// How a histogram plot is drawn. The page wraps the plot's query to match
/// (`buildHistogramQuery` in `charts/metric_types.js`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HistogramView {
    /// A scatter plot of percentiles over time; the query is wrapped with
    /// `histogram_quantiles()`.
    Percentiles,
    /// A heatmap of the histogram's buckets over time; the query is wrapped
    /// with `histogram_heatmap()`.
    Buckets,
    /// A heatmap of the percentiles over time; the query is wrapped with
    /// `histogram_quantiles()` over the 100 quantiles 0.01 to 1.00, ignoring
    /// `percentiles`.
    QuantileHeatmap,
}

impl HistogramView {
    const ALL: &'static [HistogramView] =
        &[Self::Percentiles, Self::Buckets, Self::QuantileHeatmap];

    fn as_str(self) -> &'static str {
        match self {
            Self::Percentiles => "percentiles",
            Self::Buckets => "buckets",
            Self::QuantileHeatmap => "quantile_heatmap",
        }
    }
}

/// The chart a plot is drawn as, by the module `chart.js` dispatches to.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChartStyle {
    /// One series as a line (`line.js`).
    Line,
    /// Several series as lines (`multi.js`).
    Multi,
    /// One row per entity, coloured by value (`heatmap.js`).
    Heatmap,
    /// Percentiles as points (`scatter.js`).
    Scatter,
    /// A histogram's buckets over time (`histogram_heatmap.js`).
    HistogramHeatmap,
    /// Percentiles over time as a heatmap (`quantile_heatmap.js`).
    QuantileHeatmap,
}

impl ChartStyle {
    const ALL: &'static [ChartStyle] = &[
        Self::Line,
        Self::Multi,
        Self::Heatmap,
        Self::Scatter,
        Self::HistogramHeatmap,
        Self::QuantileHeatmap,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Multi => "multi",
            Self::Heatmap => "heatmap",
            Self::Scatter => "scatter",
            Self::HistogramHeatmap => "histogram_heatmap",
            Self::QuantileHeatmap => "quantile_heatmap",
        }
    }
}

macro_rules! names {
    ($ty:ty, $what:literal) => {
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $ty {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::ALL
                    .iter()
                    .copied()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| {
                        let known: Vec<&str> = Self::ALL.iter().map(|v| v.as_str()).collect();
                        format!(
                            concat!("unknown ", $what, " {:?}; expected one of {}"),
                            s,
                            known.join(", ")
                        )
                    })
            }
        }
    };
}

names!(HistogramView, "histogram view");
names!(ChartStyle, "chart style");

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
enum MetricType {
    Gauge,
    DeltaCounter,
    Histogram,
}

/// A chart's title, id, metric type and axis format: the spec's `opts`.
#[derive(Serialize, Clone, Debug)]
struct PlotOpts {
    title: String,
    id: String,
    #[serde(rename = "type")]
    metric_type: MetricType,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtype: Option<HistogramView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    percentiles: Option<Vec<f64>>,
    format: FormatConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<ChartStyle>,
}

impl PlotOpts {
    fn new<T: Into<String>, U: Into<String>>(
        title: T,
        id: U,
        metric_type: MetricType,
        subtype: Option<HistogramView>,
        unit: Unit,
    ) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            metric_type,
            subtype,
            percentiles: None,
            format: FormatConfig::new(unit),
            description: None,
            style: None,
        }
    }
}

#[derive(Default, Clone, Serialize, Debug)]
struct Range {
    #[serde(skip_serializing_if = "Option::is_none")]
    min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<f64>,
}

#[derive(Serialize, Clone, Debug)]
struct FormatConfig {
    x_axis_label: Option<String>,
    y_axis_label: Option<String>,

    unit_system: Option<String>, // e.g., "percentage", "time", "bitrate"
    precision: Option<u8>,       // decimal places

    log_scale: Option<bool>,

    // Values outside are clamped at render time.
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<Range>,

    value_label: Option<String>, // label used in tooltips for the value

    // Entity each heatmap row represents in tooltips (e.g. "CPU", "GPU").
    // Defaults to "CPU" in the charts when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    row_label: Option<String>,
}

impl FormatConfig {
    fn new(unit: Unit) -> Self {
        Self {
            x_axis_label: None,
            y_axis_label: None,
            unit_system: Some(unit.to_string()),
            precision: Some(2),
            log_scale: None,
            range: None,
            value_label: None,
            row_label: None,
        }
    }
}

/// The unit systems `UNIT_SYSTEMS` in `assets/charts/util/units.js` defines.
/// A chart shows a system it does not define as its name, unscaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Unit {
    Count,
    Rate,
    Time,
    Bytes,
    Datarate,
    Bitrate,
    Percentage,
    Frequency,
    Power,
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let s = match self {
            Self::Count => "count",
            Self::Rate => "rate",
            Self::Time => "time",
            Self::Bytes => "bytes",
            Self::Datarate => "datarate",
            Self::Bitrate => "bitrate",
            Self::Percentage => "percentage",
            Self::Frequency => "frequency",
            Self::Power => "power",
        };

        write!(f, "{s}")
    }
}

impl Unit {
    /// Every variant, in the order `units.js` lists them.
    pub const ALL: &'static [Unit] = &[
        Self::Time,
        Self::Bytes,
        Self::Bitrate,
        Self::Datarate,
        Self::Percentage,
        Self::Frequency,
        Self::Power,
        Self::Count,
        Self::Rate,
    ];
}

impl std::str::FromStr for Unit {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|u| u.to_string() == s)
            .ok_or_else(|| {
                let known: Vec<String> = Self::ALL.iter().map(Unit::to_string).collect();
                format!(
                    "unknown unit_system {s:?}; expected one of {}",
                    known.join(", ")
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Unit` names exactly the systems `units.js` scales; a system added to
    /// one and not the other would be refused, or shown verbatim, by the charts.
    #[test]
    fn units_match_the_charts() {
        let js = include_str!("../assets/charts/util/units.js").replace("\r\n", "\n");
        let block = &js[js.find("const UNIT_SYSTEMS = {").unwrap()..];
        let block = &block[..block.find("\n};").unwrap()];
        let charts: Vec<&str> = block
            .lines()
            .filter_map(|l| l.strip_prefix("    ")?.strip_suffix(": {"))
            .collect();
        let ours: Vec<String> = Unit::ALL.iter().map(Unit::to_string).collect();
        assert_eq!(charts, ours);
        for name in &ours {
            assert_eq!(name.parse::<Unit>().unwrap().to_string(), *name);
        }
    }

    #[test]
    fn maybe_unit_system_drops_an_unknown_system() {
        let unit = |b: PlotBuilder| {
            serde_json::to_value(b.build("q")).unwrap()["opts"]["format"]["unit_system"].clone()
        };
        let known = Plot::counter("t", "id", Unit::Count).maybe_unit_system(Some("bytes"));
        assert_eq!(unit(known), "bytes");
        let unknown = Plot::counter("t", "id", Unit::Count)
            .maybe_unit_system(Some("<img src=x onerror=alert(1)>"));
        assert_eq!(unit(unknown), "count", "the unknown system is not applied");
        let err = "watts".parse::<Unit>().unwrap_err();
        assert!(
            err.contains("\"watts\"") && err.contains("percentage"),
            "{err}"
        );
    }

    /// `ChartStyle` names exactly the styles `chart.js` dispatches on; it
    /// throws on any other.
    #[test]
    fn chart_styles_match_the_dispatch() {
        let js = include_str!("../assets/charts/chart.js").replace("\r\n", "\n");
        let block = &js[js.find("// Handle different chart types").unwrap()..];
        let block = &block[..block.find("Unknown chart style").unwrap()];
        let mut charts: Vec<&str> = block
            .split("style === '")
            .skip(1)
            .map(|s| &s[..s.find('\'').unwrap()])
            .collect();
        let mut ours: Vec<String> = ChartStyle::ALL.iter().map(|s| s.to_string()).collect();
        charts.sort_unstable();
        ours.sort_unstable();
        assert_eq!(charts, ours);
        for name in &ours {
            assert_eq!(name.parse::<ChartStyle>().unwrap().to_string(), *name);
        }
        assert!("Heatmap".parse::<ChartStyle>().is_err());
        for s in ChartStyle::ALL {
            let j = serde_json::to_value(s).unwrap();
            assert_eq!(j, s.to_string());
            assert_eq!(serde_json::from_value::<ChartStyle>(j).unwrap(), *s);
        }
    }

    /// `resolveStyle` and `buildHistogramQuery` in `metric_types.js` each
    /// match on every view other than the default, `percentiles`; the serde
    /// name, `Display` and `FromStr` agree.
    #[test]
    fn histogram_views_match_metric_types() {
        // A Windows checkout has CRLF line endings.
        let js = include_str!("../assets/charts/metric_types.js").replace("\r\n", "\n");
        for f in [
            "export function resolveStyle",
            "export function buildHistogramQuery",
        ] {
            let body = &js[js.find(f).unwrap()..];
            let body = &body[..body.find("\n}\n").unwrap()];
            for view in HistogramView::ALL {
                if *view != HistogramView::Percentiles {
                    assert!(
                        body.contains(&format!("subtype === '{view}'")),
                        "{f}: {view}"
                    );
                }
            }
        }
        for view in HistogramView::ALL {
            let name = view.to_string();
            assert_eq!(name.parse::<HistogramView>().unwrap(), *view);
            let j = serde_json::to_value(view).unwrap();
            assert_eq!(j, name.as_str());
            assert_eq!(serde_json::from_value::<HistogramView>(j).unwrap(), *view);
            let json =
                serde_json::to_value(Plot::histogram("t", "i", Unit::Time, *view).build("q"))
                    .unwrap();
            assert_eq!(json["opts"]["subtype"], name.as_str());
        }
    }

    /// `metric_types.js` matches on these strings.
    #[test]
    fn metric_type_strings() {
        let ty =
            |b: PlotBuilder| serde_json::to_value(b.build("q")).unwrap()["opts"]["type"].clone();
        assert_eq!(ty(Plot::gauge("t", "i", Unit::Count)), "gauge");
        assert_eq!(ty(Plot::counter("t", "i", Unit::Count)), "delta_counter");
        assert_eq!(
            ty(Plot::histogram(
                "t",
                "i",
                Unit::Time,
                HistogramView::Buckets
            )),
            "histogram"
        );
    }

    #[test]
    fn unset_total_and_scope_are_elided_from_json() {
        let json = serde_json::to_value(Plot::counter("t", "id", Unit::Count).build("up")).unwrap();
        assert!(json.get("promql_query_total").is_none(), "{json}");
        assert!(json.get("scope").is_none(), "{json}");
        let opts: Vec<&String> = json["opts"].as_object().unwrap().keys().collect();
        assert_eq!(opts, ["format", "id", "title", "type"], "{json}");
        assert!(json["opts"]["format"].get("row_label").is_none(), "{json}");
        assert!(json["opts"]["format"].get("range").is_none(), "{json}");
    }

    #[test]
    fn queries_mut_visits_the_query_then_the_total() {
        let mut plot = Plot::counter("t", "id", Unit::Count)
            .with_total_query("b")
            .build("a");
        let seen: Vec<String> = plot.queries_mut().map(|q| q.clone()).collect();
        assert_eq!(seen, ["a", "b"]);
        for q in plot.queries_mut() {
            q.push('!');
        }
        assert_eq!(plot.query(), "a!");
        assert_eq!(plot.total_query(), Some("b!"));
        let mut plot = Plot::counter("t", "id", Unit::Count).build("a");
        assert_eq!(plot.queries_mut().count(), 1);
        plot.set_scope(Some(vec!["u".into()]));
        assert_eq!(plot.scope(), Some(&["u".to_string()][..]));
    }

    /// The JSON the chart modules read. A change here is a change to what
    /// every page that builds specs with this crate sends to the charts.
    #[test]
    fn plot_json_shape() {
        let plot = Plot::histogram_latency("Latency", "lat")
            .with_description("p50 to p99.9")
            .with_percentiles(vec![0.5, 0.999])
            .with_axis_label("Latency")
            .with_row_label("GPU")
            .with_style(ChartStyle::Scatter)
            .with_total_query("histogram_sum(lat)")
            .with_scope(vec!["a".into()])
            .build("lat");
        assert_eq!(
            serde_json::to_value(&plot).unwrap(),
            serde_json::json!({
                "data": [],
                "opts": {
                    "title": "Latency",
                    "id": "lat",
                    "type": "histogram",
                    "subtype": "percentiles",
                    "percentiles": [0.5, 0.999],
                    "format": {
                        "x_axis_label": null,
                        "y_axis_label": "Latency",
                        "unit_system": "time",
                        "precision": 2,
                        "log_scale": true,
                        "range": { "min": 0.0, "max": 100_000_000_000.0 },
                        "value_label": null,
                        "row_label": "GPU"
                    },
                    "description": "p50 to p99.9",
                    "style": "scatter"
                },
                "promql_query": "lat",
                "promql_query_total": "histogram_sum(lat)",
                "scope": ["a"]
            })
        );
    }
}
