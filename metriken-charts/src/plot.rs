//! The plot spec: the JSON a page hands a chart in `assets/charts/` to say
//! what it draws. A [`Plot`] is one chart: its query and its [`PlotOpts`]
//! (title, metric type, units, axis format). Serializing a `Plot` with serde
//! gives the object `Chart`'s `spec` takes.

use serde::{Deserialize, Serialize};

/// One chart: its options and its queries. A `Plot` built here serializes
/// with `data: []` and none of the result fields (`min_value`, `max_value`,
/// `time_data`, `formatted_time_data`, `series_names`); the page adds them.
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
    /// The PromQL query the chart draws.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promql_query: Option<String>,
    /// A second query for the same chart, for a page that compares two
    /// recordings. The chart modules do not read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promql_query_experiment: Option<String>,
    /// The query the line chart's Mean/Total toggle draws when ticked, such
    /// as `histogram_sum` of the plot's histogram.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promql_query_total: Option<String>,
    /// The uuids of the sources every query of the plot reads. Absent reads
    /// every source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    /// The chart's width in the page's grid.
    #[serde(skip_serializing_if = "plot_width_is_half", default)]
    pub width: PlotWidth,
}

impl Plot {
    /// A half-width plot of `promql_query`.
    pub fn new<T: Into<String>>(opts: PlotOpts, promql_query: T) -> Self {
        Self {
            data: Vec::new(),
            opts,
            min_value: None,
            max_value: None,
            time_data: None,
            formatted_time_data: None,
            series_names: None,
            promql_query: Some(promql_query.into()),
            promql_query_experiment: None,
            promql_query_total: None,
            scope: None,
            width: PlotWidth::default(),
        }
    }

    /// The options the plot was built with.
    pub fn opts(&self) -> &PlotOpts {
        &self.opts
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MetricType {
    Gauge,
    DeltaCounter,
    Histogram,
}

/// A plot's width: half of the page's grid, or all of it.
#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlotWidth {
    #[default]
    Half,
    Full,
}

fn plot_width_is_half(w: &PlotWidth) -> bool {
    matches!(w, PlotWidth::Half)
}

/// A chart's title, id, metric type and axis format.
#[derive(Serialize, Clone, Debug)]
pub struct PlotOpts {
    title: String,
    id: String,
    #[serde(rename = "type")]
    metric_type: MetricType,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    percentiles: Option<Vec<f64>>,
    format: FormatConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Set by [`PlotOpts::with_style`].
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<String>,
}

#[derive(Default, Clone, Serialize, Debug)]
pub(crate) struct Range {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Serialize, Clone, Debug)]
pub(crate) struct FormatConfig {
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

impl PlotOpts {
    /// A gauge metric represents a point-in-time value (e.g., memory usage, temperature).
    pub fn gauge<T: Into<String>, U: Into<String>>(title: T, id: U, unit: Unit) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            metric_type: MetricType::Gauge,
            subtype: None,
            percentiles: None,
            format: FormatConfig::new(unit),
            description: None,
            style: None,
        }
    }

    /// A delta counter metric represents the rate of change of a cumulative counter
    /// (e.g., CPU usage rate, packet rate).
    pub fn counter<T: Into<String>, U: Into<String>>(title: T, id: U, unit: Unit) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            metric_type: MetricType::DeltaCounter,
            subtype: None,
            percentiles: None,
            format: FormatConfig::new(unit),
            description: None,
            style: None,
        }
    }

    /// A histogram metric represents a distribution (e.g., latency, IO size).
    /// The subtype determines the visualization and query wrapping:
    /// - "percentiles": shows percentile scatter plot, wraps query with histogram_quantiles()
    /// - "buckets": shows bucket heatmap, wraps query with histogram_heatmap()
    /// - "quantile_heatmap": shows a heatmap of the percentiles over time
    pub fn histogram<T: Into<String>, U: Into<String>>(
        title: T,
        id: U,
        unit: Unit,
        subtype: &str,
    ) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            metric_type: MetricType::Histogram,
            subtype: Some(subtype.to_string()),
            percentiles: None,
            format: FormatConfig::new(unit),
            description: None,
            style: None,
        }
    }

    /// Convenience: a histogram metric for latency distributions with standard
    /// defaults (log scale, 100s range).
    pub fn histogram_latency<T: Into<String>, U: Into<String>>(title: T, id: U) -> Self {
        Self::histogram(title, id, Unit::Time, "percentiles")
            .with_log_scale(true)
            .range(0.0, 100_000_000_000.0)
    }

    /// Convenience: sets the standard 0..1 range used for percentage metrics.
    pub fn percentage_range(self) -> Self {
        self.range(0.0, 1.0)
    }

    /// Set the unit system by name. See [`Unit`] for the names the charts
    /// scale.
    pub fn with_unit_system<T: Into<String>>(mut self, unit_system: T) -> Self {
        self.format.unit_system = Some(unit_system.into());
        self
    }

    /// A subtitle for the chart. The chart modules do not draw it; the page
    /// does.
    pub fn with_description<T: Into<String>>(mut self, text: T) -> Self {
        self.description = Some(text.into());
        self
    }

    /// The subtitle set by [`with_description`](Self::with_description).
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Sets the unit system to `unit` when it names a [`Unit`], and otherwise
    /// leaves the plot unchanged. A caller that needs to reject an unknown
    /// name parses it with [`Unit::from_str`](std::str::FromStr::from_str)
    /// first.
    pub fn maybe_unit_system(self, unit: Option<&str>) -> Self {
        match unit.map(str::parse::<Unit>) {
            Some(Ok(u)) => self.with_unit_system(u.to_string()),
            Some(Err(_)) | None => self,
        }
    }

    /// The percentiles a percentile histogram plot draws, as fractions
    /// (0.5 for p50).
    pub fn with_percentiles(mut self, percentiles: Vec<f64>) -> Self {
        self.percentiles = Some(percentiles);
        self
    }

    /// Set the y-axis label.
    pub fn with_axis_label<T: Into<String>>(mut self, y_label: T) -> Self {
        self.format.y_axis_label = Some(y_label.into());
        self
    }

    /// Set the entity each heatmap row represents in tooltips (e.g. "GPU").
    pub fn with_row_label<T: Into<String>>(mut self, row_label: T) -> Self {
        self.format.row_label = Some(row_label.into());
        self
    }

    /// Set the chart style. For a gauge or counter plot, `resolveStyle` in
    /// `charts/metric_types.js` otherwise picks it from the result: a
    /// per-device query drawn as a heatmap with two devices is drawn as a
    /// line with one. `resolvedStyle` uses this value ahead of that
    /// inference.
    pub fn with_style<T: Into<String>>(mut self, style: T) -> Self {
        self.style = Some(style.into());
        self
    }

    /// Draw the y-axis of a line, multi-series or percentile chart on a log
    /// scale (`true`) or a linear one (`false`, the default). The heatmaps
    /// ignore it.
    pub fn with_log_scale(mut self, log_scale: bool) -> Self {
        self.format.log_scale = Some(log_scale);
        self
    }

    /// Clamp plotted values to `min..=max`. A line or multi-series chart
    /// draws a value outside the range at the bound. A percentile chart caps
    /// its y-axis at `max` and draws values above it in a band marked OOB. A
    /// heatmap clamps cell values, so its colour scale stops at `max`, and a
    /// histogram heatmap leaves out buckets above `max`. A quantile heatmap
    /// ignores the range.
    pub fn range(mut self, min: f64, max: f64) -> Self {
        self.format.range = Some(Range {
            min: Some(min),
            max: Some(max),
        });
        self
    }
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
        let js = include_str!("../assets/charts/util/units.js");
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
        let unit =
            |opts: PlotOpts| serde_json::to_value(&opts).unwrap()["format"]["unit_system"].clone();
        let known = PlotOpts::counter("t", "id", Unit::Count).maybe_unit_system(Some("bytes"));
        assert_eq!(unit(known), "bytes");
        let unknown = PlotOpts::counter("t", "id", Unit::Count)
            .maybe_unit_system(Some("<img src=x onerror=alert(1)>"));
        assert_eq!(unit(unknown), "count", "the unknown system is not applied");
        let err = "watts".parse::<Unit>().unwrap_err();
        assert!(
            err.contains("\"watts\"") && err.contains("percentage"),
            "{err}"
        );
    }

    /// `metric_types.js` matches on these strings.
    #[test]
    fn metric_type_strings() {
        let ty = |opts: PlotOpts| serde_json::to_value(&opts).unwrap()["type"].clone();
        assert_eq!(ty(PlotOpts::gauge("t", "i", Unit::Count)), "gauge");
        assert_eq!(
            ty(PlotOpts::counter("t", "i", Unit::Count)),
            "delta_counter"
        );
        assert_eq!(
            ty(PlotOpts::histogram("t", "i", Unit::Time, "buckets")),
            "histogram"
        );
    }

    #[test]
    fn plot_width_half_is_elided_from_json() {
        let plot = Plot::new(PlotOpts::counter("t", "id", Unit::Count), "up");
        let json = serde_json::to_value(&plot).unwrap();
        assert!(
            json.get("width").is_none(),
            "expected `width` to be omitted when Half, got {json}"
        );
    }

    #[test]
    fn plot_width_full_is_serialized() {
        let mut plot = Plot::new(PlotOpts::counter("t", "id", Unit::Count), "up");
        plot.width = PlotWidth::Full;
        let json = serde_json::to_value(&plot).unwrap();
        assert_eq!(json["width"], serde_json::json!("full"));
    }

    #[test]
    fn plot_promql_query_experiment_round_trips() {
        let mut plot = Plot::new(
            PlotOpts::counter("X", "kpi-x", Unit::Count),
            "metric_a".to_string(),
        );
        plot.promql_query_experiment = Some("metric_b".to_string());
        let json = serde_json::to_string(&plot).unwrap();
        assert!(json.contains("\"promql_query_experiment\":\"metric_b\""));

        plot.promql_query_experiment = None;
        let json = serde_json::to_string(&plot).unwrap();
        assert!(!json.contains("promql_query_experiment"), "got {json}");
    }

    /// The JSON the chart modules read. A change here is a change to what
    /// every page that builds specs with this crate sends to the charts.
    #[test]
    fn plot_json_shape() {
        let mut plot = Plot::new(
            PlotOpts::histogram_latency("Latency", "lat")
                .with_description("p50 to p99.9")
                .with_percentiles(vec![0.5, 0.999])
                .with_axis_label("Latency")
                .with_row_label("GPU")
                .with_style("scatter"),
            "lat",
        );
        plot.promql_query_total = Some("histogram_sum(lat)".into());
        plot.scope = Some(vec!["a".into()]);
        plot.width = PlotWidth::Full;
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
                "scope": ["a"],
                "width": "full"
            })
        );
    }
}
