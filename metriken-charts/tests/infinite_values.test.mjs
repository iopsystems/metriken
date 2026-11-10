import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

// A histogram quantile past the highest finite bucket is +Inf (metriken-query
// 0.47). A percentile chart draws it in the OOB band above range.max; every
// other chart draws ±Infinity as a gap. ECharts given an Infinity in a series,
// or in a y-axis bound, sets the y extent to [0, 1] and every finite point
// leaves the plot. These tests render through the vendored ECharts and read
// the extent it computes, and check that values print as +Inf / -Inf.
globalThis.document = globalThis.document || {
    documentElement: { getAttribute: () => null },
    createElement: () => fakeElement(),
    addEventListener() {},
    removeEventListener() {},
};
globalThis.getComputedStyle = globalThis.getComputedStyle || (() => ({ getPropertyValue: () => '' }));

const echarts = createRequire(import.meta.url)('../assets/charts/echarts.min.js');
// ECharts measures axis labels on a canvas, and node has none.
echarts.setPlatformAPI({ measureText: (text) => ({ width: text.length * 7 }) });
const { getBaseYAxisOption } = await import('../assets/charts/base.js');
const { Chart, ChartsState } = await import('../assets/charts/chart.js');
const { configureLineChart } = await import('../assets/charts/line.js');
const { configureMultiSeriesChart } = await import('../assets/charts/multi.js');
const { configureScatterChart } = await import('../assets/charts/scatter.js');
const { configureHeatmap } = await import('../assets/charts/heatmap.js');
const { configureQuantileHeatmap } = await import('../assets/charts/quantile_heatmap.js');
const { buildDivergenceBand } = await import('../assets/charts/boxplot.js');
const { createAxisLabelFormatter, formatWithUnit } = await import('../assets/charts/util/units.js');
const { heatmapTriplesMinMax } = await import('../assets/charts/util/heatmap_data.js');

const T0 = 1_700_000_000;
const N = 60;
const times = (n = N) => Array.from({ length: n }, (_, i) => T0 + i);
const flat = (v, n = N) => Array.from({ length: n }, () => v);
// Finite values from lo to hi, with `inf` at the indices in `at`.
const ramp = (lo, hi, at = [], inf = Infinity) => Array.from({ length: N }, (_, i) =>
    (at.includes(i) ? inf : lo + ((hi - lo) * i) / (N - 1)));

const rendered = [];
afterEach(() => {
    while (rendered.length) rendered.pop().dispose();
});

function fakeElement() {
    return {
        classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
        style: {},
        children: [],
        querySelector: () => null,
        querySelectorAll: () => [],
        appendChild: (c) => c,
        insertBefore: (c) => c,
        removeChild() {},
        remove() {},
        addEventListener() {},
        removeEventListener() {},
        setAttribute() {},
        getBoundingClientRect: () => ({ width: 600, height: 300, left: 0, top: 0 }),
        getContext: () => new Proxy({}, { get: () => () => ({ addColorStop() {} }), set: () => true }),
    };
}

// A chart drawn by its real chart module into an SSR ECharts instance.
function configured(configure, style, data, format, spec = {}) {
    const echart = echarts.init(null, null, { renderer: 'svg', ssr: true, width: 600, height: 300 });
    rendered.push(echart);
    const chart = new Chart({
        spec: { opts: { id: 'c', title: 't', type: 'line', style, format }, data, ...spec },
        chartsState: new ChartsState(),
        interval: 1,
    });
    chart.echart = echart;
    chart.domNode = fakeElement();
    configure(chart);
    return chart;
}

const yExtent = (chart) => chart.echart.getModel().getComponent('yAxis').axis.scale.getExtent();

// The y extent is finite, holds [lo, hi], and is not stretched far past it.
function assertCovers(chart, lo, hi, what) {
    const [min, max] = yExtent(chart);
    assert.ok(Number.isFinite(min) && Number.isFinite(max), `${what}: extent ${min},${max}`);
    assert.ok(min <= lo && max >= hi, `${what}: extent ${min},${max} does not hold ${lo}..${hi}`);
    const span = hi - lo;
    assert.ok(min >= lo - span && max <= hi + span, `${what}: extent ${min},${max} too wide for ${lo}..${hi}`);
}

// Zoom the chart to the middle half, as the shared zoom does on a drag.
function zoomMiddle(chart) {
    const zoom = { start: 25, end: 75 };
    chart.chartsState.zoomLevel = zoom;
    chart._applyZoom(zoom);
}

// ── percentile (scatter) chart ──────────────────────────────────────────────

// p50 from 1 to 2, p99 from 3 to 4 with +Inf at three points in the middle
// half and one finite 500 (over range.max, so in the OOB band too).
const P50 = ramp(1, 2);
const P99 = ramp(3, 4, [20, 30, 40]);
P99[35] = 500;
const PCT_FORMAT = { unit_system: 'time', range: { max: 5 } };
const percentileChart = (format = PCT_FORMAT) => configured(
    configureScatterChart, 'scatter', [times(), P50, P99], format, { series_names: ['0.5', '0.99'] },
);

test('a percentile chart draws +Inf in its OOB band at the default zoom', () => {
    const chart = percentileChart();
    assert.equal(yExtent(chart)[1], chart._oobAxisMax);
    assertCovers(chart, 1, 5, 'default zoom');
    const dots = chart.echart.getOption().series.find((s) => s.type === 'scatter' && s.name === 'p99');
    const inf = dots.data.filter((d) => d.value && d.value[2] === Infinity);
    assert.equal(inf.length, 3, 'every +Inf point is drawn');
});

test('zooming a percentile chart with +Inf keeps a finite axis over the finite data', () => {
    const chart = percentileChart();
    zoomMiddle(chart);
    const [min, max] = yExtent(chart);
    assert.equal(max, chart._oobAxisMax);
    // Visible finite values run from about 1.25 to 3.75; the axis is padded
    // from the lowest one, not stretched down by the +Inf or the 500.
    assert.ok(min > 1 && min < 1.25, `extent ${min},${max}`);
});

test('pinning a percentile with +Inf keeps a finite axis over the pinned series', () => {
    const chart = percentileChart();
    chart.echart.dispatchAction({ type: 'legendToggleSelect', name: 'p99' });
    assert.deepEqual([...chart.pinnedSet], ['p99']);
    const [min, max] = yExtent(chart);
    assert.equal(max, chart._oobAxisMax);
    assert.ok(min > 2.9 && min < 3, `extent ${min},${max}`);
});

test('a percentile chart with no range.max draws +Inf as a gap', () => {
    const chart = percentileChart({ unit_system: 'time' });
    assert.equal(chart._oobAxisMax, null);
    assertCovers(chart, 1, 500, 'no range');
});

test('a decimated percentile chart draws +Inf in its OOB band, or as a gap with no range', () => {
    const col = (v) => Float64Array.from(v);
    const boxplot = [{ metric: {}, t: times(), min: col(P50), lo: col(P50), median: col(P50), hi: col(P50), max: col(ramp(2, 3, [10, 50])) },
        { metric: {}, t: times(), min: col(ramp(3, 4)), lo: col(P99), median: col(P99), hi: col(P99), max: col(ramp(4, 5, [20])) }];
    const spec = { series_names: ['0.5', '0.99'], boxplot, boxplotDecimated: true };
    const banded = configured(configureScatterChart, 'scatter', [times(), P50, P99], { unit_system: 'time', range: { max: 600 } }, spec);
    assert.equal(yExtent(banded)[1], banded._oobAxisMax);
    const gapped = configured(configureScatterChart, 'scatter', [times(), P50, P99], { unit_system: 'time' }, spec);
    assertCovers(gapped, 1, 500, 'decimated, no range');
});

// ── line and multi-series charts ────────────────────────────────────────────

const LINE = ramp(1, 2, [10, 30]);
LINE[40] = -Infinity;

for (const [name, format] of [['no range', { unit_system: 'time' }], ['a range', { unit_system: 'time', range: { min: 0, max: 5 } }]]) {
    test(`a line chart with ±Infinity and ${name} draws it as a gap`, () => {
        const chart = configured(configureLineChart, 'line', [times(), LINE], format);
        assertCovers(chart, 1, 2, 'default zoom');
        const line = chart.echart.getOption().series.find((s) => s.type === 'line' && !s.stack);
        const at = (i) => line.data.find((d) => d[0] === (T0 + i) * 1000);
        assert.deepEqual(at(10), [(T0 + 10) * 1000, null, Infinity]);
        assert.deepEqual(at(40), [(T0 + 40) * 1000, null, -Infinity]);
        zoomMiddle(chart);
        assertCovers(chart, 1.3, 1.7, 'zoomed');
    });
}

test('a multi-series chart with ±Infinity draws it as a gap', () => {
    const chart = configured(configureMultiSeriesChart, 'multi', [times(), LINE, ramp(3, 4, [5])], { unit_system: 'time' },
        { series_names: ['a', 'b'] });
    assertCovers(chart, 1, 4, 'multi');
});

test('a multi-series chart with a range draws an infinite band edge as a gap', () => {
    const col = (v) => Float64Array.from(v);
    const boxplot = [LINE, ramp(3, 4)].map((v) => ({ metric: {}, t: times(), min: col(ramp(1, 2)), lo: col(v), median: col(v), hi: col(v), max: col(v) }));
    const chart = configured(configureMultiSeriesChart, 'multi', [times(), LINE, ramp(3, 4)], { range: { max: 5 } },
        { series_names: ['a', 'b'], boxplot, boxplotDecimated: true });
    assertCovers(chart, 1, 4, 'multi bands');
    const fill = chart.echart.getOption().series.find((s) => s.stack === 'mb0 outer' && s.areaStyle);
    const at10 = fill.data.find((d) => d[0] === (T0 + 10) * 1000);
    assert.equal(at10[1], null, 'the band is not drawn up to range.max');
});

test('a line chart drops an uncertainty band with an infinite end', () => {
    const intervals = ramp(1, 2).map((v, i) => (i === 10 ? [v, Infinity] : [v - 0.1, v + 0.1]));
    const chart = configured(configureLineChart, 'line', [times(), ramp(1, 2)], { unit_system: 'time' }, { intervals });
    assertCovers(chart, 0.9, 2.1, 'band');
});

test('a line chart draws an interpolated ±Infinity as a gap', () => {
    const interpolated = flat(false).map((_, i) => i === 10);
    const chart = configured(configureLineChart, 'line', [times(), LINE], { unit_system: 'time' }, { interpolated });
    assertCovers(chart, 1, 2, 'interpolated');
    // ECharts keeps this extent while the main line holds finite values, so
    // check the dashed overlay's data too.
    const overlay = chart.echart.getOption().series.find((s) => s.lineStyle?.type === 'dashed');
    assert.deepEqual(overlay.data[10], [(T0 + 10) * 1000, null]);
});

test('a decimated line chart draws an infinite median or max as a gap', () => {
    const col = (v) => Float64Array.from(v);
    const boxplot = [{ metric: {}, t: times(), min: col(ramp(1, 2)), lo: col(ramp(1, 2)), median: col(LINE), hi: col(ramp(1, 2)), max: col(ramp(2, 3, [50])) }];
    const chart = configured(configureLineChart, 'line', [times(), LINE], { unit_system: 'time' }, { boxplot });
    assertCovers(chart, 1, 3, 'decimated line');
});

test('a divergence band with an infinite median is a gap', () => {
    const [base, fill] = buildDivergenceBand({ t: [T0, T0 + 1], lower: [1, 1], upper: [2, Infinity] });
    assert.deepEqual(fill.data[1], [(T0 + 1) * 1000, null]);
    assert.deepEqual(base.data[0], [T0 * 1000, 1]);
});

// ── formatters ──────────────────────────────────────────────────────────────

test('the unit formatters print ±Infinity as +Inf and -Inf', () => {
    for (const unit of ['time', 'bytes', 'percentage', 'count', 'rate', 'no-such-unit']) {
        const fmt = createAxisLabelFormatter(unit);
        assert.equal(fmt(Infinity), '+Inf', unit);
        assert.equal(fmt(-Infinity), '-Inf', unit);
    }
    // NaN prints as before: 0 in the unit's base.
    assert.deepEqual(formatWithUnit(NaN, 'time'), { value: '0', unit: 'ns' });
    assert.equal(createAxisLabelFormatter('time')(1500), '1.5 μs');
});

test('the axis label formatter with no unit prints +Inf', () => {
    const fmt = getBaseYAxisOption(false, undefined).axisLabel.formatter;
    assert.equal(fmt(Infinity), '+Inf');
    assert.equal(fmt(-Infinity), '-Inf');
    assert.equal(fmt(5), 5);
});

const tooltipOf = (chart) => chart.echart.getOption().tooltip[0].formatter;

for (const unit_system of ['time', undefined]) {
    test(`a percentile chart's tooltip prints +Inf (unit ${unit_system})`, () => {
        const chart = percentileChart({ ...PCT_FORMAT, unit_system });
        const html = tooltipOf(chart)([{ seriesName: 'p99', marker: '', value: [T0 * 1000, 5.1, Infinity] }]);
        assert.match(html, /\+Inf/);
        assert.doesNotMatch(html, /Infinity/);
    });

    test(`a decimated percentile chart's tooltip prints +Inf (unit ${unit_system})`, () => {
        const col = (v) => Float64Array.from(v);
        const boxplot = [{ metric: {}, t: times(), min: col(P50), lo: col(P50), median: col(P50), hi: col(P50), max: col(P50) }];
        const chart = configured(configureScatterChart, 'scatter', [times(), P50], { unit_system },
            { series_names: ['0.5'], boxplot, boxplotDecimated: true });
        const html = tooltipOf(chart)([{ seriesName: 'p50', marker: '', value: [T0 * 1000, null, Infinity] }]);
        assert.match(html, /\+Inf/);
        assert.doesNotMatch(html, /Infinity/);
    });

    test(`a line chart's tooltip prints an infinite gap as +Inf (unit ${unit_system})`, () => {
        const chart = configured(configureLineChart, 'line', [times(), LINE], { unit_system });
        const html = tooltipOf(chart)([{ seriesName: 't', marker: '', value: [T0 * 1000, null, Infinity] }]);
        assert.match(html, /\+Inf/);
        assert.doesNotMatch(html, /Infinity|raw/);
    });

    test(`a multi-series chart's tooltip prints -Inf (unit ${unit_system})`, () => {
        const chart = configured(configureMultiSeriesChart, 'multi', [times(), LINE], { unit_system }, { series_names: ['a'] });
        const html = tooltipOf(chart)([{ seriesName: 'a', marker: '', value: [T0 * 1000, null, -Infinity] }]);
        assert.match(html, /-Inf/);
        assert.doesNotMatch(html, /Infinity/);
    });
}

// The heatmaps' configure functions set the option on a recording stub.
function stubChart(spec) {
    let option = null;
    return {
        spec,
        chartId: 'c',
        domNode: fakeElement(),
        chartsState: {},
        interval: 1,
        echart: {
            setOption(o) { option = option ? { ...option, ...o } : o; },
            getOption: () => option,
            getModel: () => null,
            dispatchAction() {},
            on() {},
            off() {},
        },
    };
}

test('a heatmap tooltip with no unit prints +Inf', () => {
    const chart = stubChart({ opts: { title: 't', format: {} }, time_data: [1, 2], data: [[0, 0, 1]], min_value: 1, max_value: 2 });
    configureHeatmap(chart);
    const html = chart.echart.getOption().tooltip.formatter({ data: [1000, 0, 0, null, Infinity] });
    assert.match(html, /\+Inf/);
    assert.doesNotMatch(html, /Infinity/);
});

function quantileChart(extra = {}) {
    const chart = stubChart({
        opts: { title: 't', format: {} },
        data: [[1, 2, 3, 4], [1, 10, 100, Infinity]],
        series_names: ['0.99'],
        ...extra,
    });
    configureQuantileHeatmap(chart);
    return chart;
}

for (const format of [{}, { unit_system: 'time' }]) {
    test(`a quantile heatmap tooltip prints +Inf (format ${JSON.stringify(format)})`, () => {
        const opts = { title: 't', format };
        const plain = quantileChart({ opts }).echart.getOption().tooltip.formatter({ data: [4000, 0, Infinity] });
        assert.match(plain, /\+Inf/);
        assert.doesNotMatch(plain, /Infinity/);
        const pair = quantileChart({
            opts,
            compareCounterpartData: { data: [[1, 2, 3, 4], [1, 10, 100, 100]] },
            compareCaptureLabels: { baseline: 'b', experiment: 'e' },
            compareSelfRole: 'experiment',
        }).echart.getOption().tooltip.formatter({ data: [4000, 0, Infinity] });
        assert.match(pair, /Δ<\/span>\s*<span[^>]*>\+Inf<\/span>/);
        assert.doesNotMatch(pair, /Infinity|\+\+/);
    });
}

test('a quantile heatmap colors its finite cells from the finite values', () => {
    // A stub ECharts api hands renderItem one cell's [time, quantile, value].
    const chart = quantileChart();
    const { renderItem } = chart.echart.getOption().series[0];
    const colorOf = (v) => renderItem(
        { coordSys: { x: 0, y: 0, width: 600, height: 300 } },
        { value: (i) => [1000, 0, v][i], coord: ([t, q]) => [t / 10, 150 - q * 100] },
    ).style.fill;
    // 1 to 100 spans the scale: 10 sits in the middle, not at the top.
    assert.notEqual(colorOf(10), colorOf(100));
    assert.equal(colorOf(Infinity), colorOf(100));
});

test('heatmap value bounds skip ±Infinity', () => {
    assert.deepEqual(heatmapTriplesMinMax([[0, 0, 1], [1, 0, Infinity], [2, 0, 3], [3, 0, -Infinity]]), { min: 1, max: 3 });
});
