// Shared plot-spec construction for foreign (non-Rezolus) source metrics.
//
// Two consumers must agree byte-for-byte on the spec — the MetricBrowser, which
// renders a selected metric inline, and the `/source/:sourceName/chart/:chartId`
// single-chart route, which reconstructs one chart from the URL. The chart id
// is the shared handle between them: the expand link builds
// `#/source/<name>/chart/<opts.id>`, and the route resolves `<opts.id>` back to
// the same metric via `specForChartId`. Keep them in one place so the id and
// the type→query mapping can't drift.

import { buildDefaultQuery } from './metric_types.js';
import { STORED_UNITS } from './util/units.js';

/** Deterministic chart id for a source metric — the round-trip handle. */
export const sourceMetricChartId = (name) => `source-metric-${name}`;

/**
 * Build a section-style plot spec for a foreign-source metric.
 *
 * Histograms carry the RAW metric name as `promql_query` (with
 * `opts.subtype = 'percentiles'`) because the section pipeline
 * (buildEffectiveQuery) wraps histograms itself; passing an already-wrapped
 * `histogram_quantiles(...)` would double-wrap and return no data.
 * Counters/gauges are not re-wrapped, so they carry their buildDefaultQuery
 * form (irate(...) / raw).
 *
 * `info.unit`, the unit the metric is stored in, becomes `opts.format.unit`,
 * so the chart scales and labels the values from it (`resolveUnit` in
 * `util/units.js`), and a histogram's `info.scale` (its `scale` storage key)
 * becomes `opts.format.unit_scale`. A counter's query is a per-second rate,
 * so a counter in a unit of bytes is drawn as `datarate`, and one in a unit
 * of time as `percentage` (seconds per second is a fraction). A `bits` gauge
 * is drawn as `count`, since the only bits system, `bitrate`, is a rate.
 * Without `info.unit` the spec has no `format`.
 */
export const specForSourceMetric = (info) => {
    const opts = {
        id: sourceMetricChartId(info.name),
        title: info.name,
        description: info.description || '',
        type: info.metric_type,
    };
    if (info.unit) {
        opts.format = { unit: info.unit };
        const dimension = Object.hasOwn(STORED_UNITS, info.unit)
            ? STORED_UNITS[info.unit].dimension
            : null;
        if (info.metric_type === 'counter' && dimension === 'bytes') {
            opts.format.unit_system = 'datarate';
        } else if (info.metric_type === 'counter' && dimension === 'time') {
            opts.format.unit_system = 'percentage';
        } else if (info.metric_type === 'gauge' && dimension === 'bits') {
            opts.format.unit_system = 'count';
        }
        if (info.metric_type === 'histogram' && info.scale != null) {
            opts.format.unit_scale = info.scale;
        }
    }
    let promql_query;
    if (info.metric_type === 'histogram') {
        opts.subtype = 'percentiles';
        promql_query = info.name;
    } else {
        promql_query = buildDefaultQuery(info);
    }
    // Simple-capture charts render full-width (renderChart keys off spec.width),
    // matching the jitter chart and giving the wide x-axis room to read.
    return { promql_query, opts, width: 'full' };
};

/**
 * Resolve the plot spec for a chart id against a metric catalog (as returned by
 * `ViewerApi.getMetrics`). Returns null when no metric matches — an unknown or
 * stale id, or an empty/absent catalog.
 */
export const specForChartId = (chartId, metrics) => {
    const info = (metrics || []).find((mi) => sourceMetricChartId(mi.name) === chartId);
    return info ? specForSourceMetric(info) : null;
};
