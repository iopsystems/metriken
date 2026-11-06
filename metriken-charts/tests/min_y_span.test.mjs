import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

// A chart with `min_y_span` draws a y-axis at least that wide, inside its
// range. The tests read the axis extent from the vendored ECharts, which
// computes it from the option the chart modules build. Most use a percentage
// chart, whose values are fractions, so 1.0 is 100%.
globalThis.document = globalThis.document || {
    documentElement: { getAttribute: () => null },
    addEventListener() {},
    removeEventListener() {},
};
globalThis.getComputedStyle = globalThis.getComputedStyle || (() => ({ getPropertyValue: () => '' }));

const echarts = createRequire(import.meta.url)('../assets/charts/echarts.min.js');
// ECharts measures axis labels on a canvas, and node has none.
echarts.setPlatformAPI({ measureText: (text) => ({ width: text.length * 7 }) });
const { getBaseYAxisOption, minSpanBounds } = await import('../assets/charts/base.js');
const { Chart, ChartsState } = await import('../assets/charts/chart.js');
const { configureLineChart } = await import('../assets/charts/line.js');
const { configureMultiSeriesChart } = await import('../assets/charts/multi.js');
const { configureScatterChart } = await import('../assets/charts/scatter.js');

const T0 = 1_700_000_000;
const ONE_POINT = { unit_system: 'percentage', min_y_span: 0.01, range: { min: 0, max: 1 } };

// An undisposed instance keeps its animation loop running, and node with it.
const rendered = [];
afterEach(() => {
    while (rendered.length) rendered.pop().dispose();
});

const timed = (values) => values.map((v, i) => [(T0 + i) * 1000, v]);

function renderedChart(values, { format = ONE_POINT, logScale = false, extraSeries = [], sampling } = {}) {
    const echart = echarts.init(null, null, { renderer: 'svg', ssr: true, width: 600, height: 300 });
    echart.setOption({
        animation: false,
        xAxis: { type: 'time' },
        yAxis: getBaseYAxisOption(logScale, format.unit_system, format),
        series: [{ type: 'line', sampling, data: timed(values) }, ...extraSeries],
    });
    rendered.push(echart);
    return echart;
}

function yExtent(echart) {
    return echart.getModel().getComponent('yAxis').axis.scale.getExtent();
}

function assertExtent(actual, expected) {
    assert.ok(
        Math.abs(actual[0] - expected[0]) < 1e-9 && Math.abs(actual[1] - expected[1]) < 1e-9,
        `extent ${actual}, expected ${expected}`,
    );
}

// A Chart around `echart`, which _rescaleYAxis reads and updates.
function chartOn(echart, values, zoomLevel, format = ONE_POINT) {
    const chartsState = new ChartsState();
    chartsState.zoomLevel = zoomLevel;
    const chart = new Chart({
        spec: {
            opts: { id: 'hit-rate', type: 'line', style: 'line', format },
            data: [values.map((_, i) => T0 + i), values],
        },
        chartsState,
        interval: 1,
    });
    chart.echart = echart;
    return chart;
}

const flat = (v, n = 60) => Array.from({ length: n }, () => v);

test('a flat 100% draws on 99-100%', () => {
    assertExtent(yExtent(renderedChart(flat(1))), [0.99, 1]);
});

test('a flat 0% draws on 0-1%', () => {
    assertExtent(yExtent(renderedChart(flat(0))), [0, 0.01]);
});

test('a flat value inside the range is centred in a one point span', () => {
    assertExtent(yExtent(renderedChart(flat(0.5))), [0.495, 0.505]);
});

test('values that vary by a point or more keep the axis ECharts chooses', () => {
    const values = [0.2, 0.25, 0.3];
    const percent = yExtent(renderedChart(values));
    const unitless = yExtent(renderedChart(values, { format: {} }));
    assert.deepEqual(percent, unitless);
});

const band = (lo, hi, n = 60) => [
    { type: 'line', stack: 'band', data: timed(flat(lo, n)) },
    { type: 'line', stack: 'band', data: timed(flat(hi - lo, n)) },
];

test('a band wider than the line sets the extent the span is measured on', () => {
    const [min, max] = yExtent(renderedChart(flat(0.5), { extraSeries: band(0.4, 0.6) }));
    assert.ok(min <= 0.4 && max >= 0.6, `extent ${min},${max}`);
});

test('an extent with no values has no bounds', () => {
    assert.equal(minSpanBounds(Infinity, -Infinity, ONE_POINT), null);
    assert.equal(minSpanBounds(NaN, NaN, ONE_POINT), null);
});

// A chart drawn by its real chart module, as a page would have it. A span
// that does not reach the module's axis leaves ECharts' usual extent.
function configured(configure, data, format, style, spec = {}) {
    const echart = echarts.init(null, null, { renderer: 'svg', ssr: true, width: 600, height: 300 });
    rendered.push(echart);
    const chart = new Chart({
        spec: { opts: { id: 'c', title: 't', type: 'line', style, format }, data, series_names: data.slice(1).map(() => 'a'), ...spec },
        chartsState: new ChartsState(),
        interval: 1,
    });
    chart.echart = echart;
    chart.domNode = {
        classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
        style: {},
        querySelector: () => null,
        querySelectorAll: () => [],
        appendChild: () => {},
        addEventListener() {},
    };
    configure(chart);
    return chart;
}

const times = (n = 60) => Array.from({ length: n }, (_, i) => T0 + i);

for (const [name, configure, style] of [
    ['line', configureLineChart, 'line'],
    ['multi-series', configureMultiSeriesChart, 'multi'],
    ['percentile', configureScatterChart, 'scatter'],
]) {
    test(`a ${name} chart draws its span at the default zoom`, () => {
        const chart = configured(configure, [times(), flat(1)], ONE_POINT, style);
        assertExtent(yExtent(chart.echart), [0.99, 1]);
    });
}

function decimated(median, max) {
    return [{
        metric: {},
        t: times(),
        min: Float64Array.from(median),
        lo: Float64Array.from(median),
        median: Float64Array.from(median),
        hi: Float64Array.from(median),
        max: Float64Array.from(max),
    }];
}

test('a decimated percentile chart draws its span', () => {
    const spec = { boxplot: decimated(flat(1), flat(1)), boxplotDecimated: true };
    const chart = configured(configureScatterChart, [times(), flat(1)], ONE_POINT, 'scatter', spec);
    assertExtent(yExtent(chart.echart), [0.99, 1]);
});

test('a decimated percentile chart with an OOB band keeps its span function', () => {
    const format = { ...ONE_POINT, range: { min: 0, max: 0.1 } };
    const spec = { boxplot: decimated(flat(0.1), [...flat(0.1, 59), 0.5]), boxplotDecimated: true };
    const chart = configured(configureScatterChart, [times(), flat(0.1)], format, 'scatter', spec);
    assert.equal(typeof chart.echart.getOption().yAxis[0].min, 'function');
});

for (const [name, configure, style] of [
    ['line', configureLineChart, 'line'],
    ['multi-series', configureMultiSeriesChart, 'multi'],
]) {
    test(`a ${name} chart clamps to the range before it measures the span`, () => {
        const chart = configured(configure, [times(), flat(1.5)], ONE_POINT, style);
        assertExtent(yExtent(chart.echart), [0.99, 1]);
    });
}

test('a percentile chart with values in its OOB band keeps the usual axis', () => {
    const values = [...flat(0.1, 59), 2];
    const range = { min: 0, max: 0.1 };
    const extent = (format) =>
        yExtent(configured(configureScatterChart, [times(), values], format, 'scatter').echart);
    assert.deepEqual(extent({ ...ONE_POINT, range }), extent({ unit_system: 'percentage', range }));
});

test('a range narrower than the span sets the axis at every zoom', () => {
    const format = { ...ONE_POINT, range: { min: 0, max: 0.005 } };
    const values = flat(0.002);
    const echart = renderedChart(values, { format });
    assertExtent(yExtent(echart), [0, 0.005]);
    chartOn(echart, values, { start: 25, end: 75 }, format)._rescaleYAxis();
    assertExtent(yExtent(echart), [0, 0.005]);
});

test('resetting the zoom on a log axis leaves ECharts to choose its bounds', () => {
    const format = { ...ONE_POINT, log_scale: true };
    const values = flat(0.5);
    const echart = renderedChart(values, { format, logScale: true });
    const chart = chartOn(echart, values, { start: 0, end: 100 }, format);
    chart._rescaleYAxis();
    assert.equal(echart.getOption().yAxis[0].min, null);
});

test('a chart with no span is left alone', () => {
    const [min, max] = yExtent(renderedChart(flat(0.5), { format: {} }));
    assert.ok(max - min > 0.1, `extent ${min},${max}`);
});

test('values just over a point apart keep the axis ECharts chooses', () => {
    const values = [0.5, 0.515];
    assert.deepEqual(yExtent(renderedChart(values)), yExtent(renderedChart(values, { format: {} })));
});

test('unclamped values outside the range keep the axis ECharts chooses', () => {
    for (const v of [1.5, -0.5]) {
        assert.deepEqual(yExtent(renderedChart(flat(v))), yExtent(renderedChart(flat(v), { format: {} })));
    }
});

test('the span is set per chart', () => {
    const format = { ...ONE_POINT, min_y_span: 0.001 };
    assertExtent(yExtent(renderedChart(flat(0), { format })), [0, 0.001]);
    assertExtent(yExtent(renderedChart([0, 0.0005], { format })), [0, 0.001]);
});

test('a span works in any unit, and needs no range', () => {
    const format = { unit_system: 'count', min_y_span: 100 };
    assertExtent(yExtent(renderedChart(flat(1000), { format })), [950, 1050]);
});

test('a log axis and a chart with no data are left alone', () => {
    const log = renderedChart(flat(0.5), { logScale: true }).getOption().yAxis[0];
    assert.equal(log.min, null);
    assert.equal(log.max, null);
    assert.doesNotThrow(() => renderedChart([]));
});

test('zooming into a flat stretch keeps the one point span', () => {
    const values = [...flat(0.5, 30), ...flat(1, 30)];
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 60, end: 90 })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.99, 1]);
});

test('resetting the zoom restores the span of the whole chart', () => {
    const values = [...flat(0.3, 50), ...flat(0.305, 10)];
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 10, end: 50 })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.295, 0.305]);
    chartOn(echart, values, { start: 0, end: 100 })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.2975, 0.3075]);
});

test('resetting the zoom keeps the top of an OOB band', () => {
    const values = flat(0.5);
    const echart = renderedChart(values);
    const chart = chartOn(echart, values, { start: 0, end: 100 });
    chart._oobAxisMax = 1.05;
    chart._rescaleYAxis();
    const { min, max } = echart.getOption().yAxis[0];
    assert.equal(typeof min, 'function');
    assert.equal(max, 1.05);
});

test('zooming a flat line with a band wider than the span keeps the padded axis', () => {
    const values = flat(0.5);
    const echart = renderedChart(values, { extraSeries: band(0.45, 0.55) });
    chartOn(echart, values, { start: 25, end: 75 })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.445, 0.555]);
});

test('zooming a flat median at 0 with a wider max band keeps the padded axis', () => {
    const values = flat(0);
    const echart = renderedChart(values, { extraSeries: band(0, 0.05) });
    chartOn(echart, values, { start: 25, end: 75 })._rescaleYAxis();
    assertExtent(yExtent(echart), [-1.1, 1]);
});

test('a window ECharts thinned to one point is measured on all of its points', () => {
    const values = Array.from({ length: 7200 }, (_, i) => 0.5 + 0.1 * Math.sin(i * 1.7));
    const zoom = { start: 50, end: 50.1 };
    const extentWith = (format) => {
        const echart = renderedChart(values, { format, sampling: 'lttb' });
        chartOn(echart, values, zoom, format)._rescaleYAxis();
        return yExtent(echart);
    };
    assert.deepEqual(extentWith(ONE_POINT), extentWith({}));
});

test('a span above the range ceiling keeps the padded axis', () => {
    const values = flat(0.52);
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 25, end: 75 }, {
        ...ONE_POINT,
        range: { min: 0, max: 0.5 },
    })._rescaleYAxis();
    const { min, max } = echart.getOption().yAxis[0];
    assert.ok(min < max, `min ${min}, max ${max}`);
});

test('zooming measures a stacked band at its stacked height', () => {
    const values = flat(0.501);
    const echart = renderedChart(values, { extraSeries: band(0.5, 0.502) });
    chartOn(echart, values, { start: 25, end: 75 })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.496, 0.506]);
});

test('a range with no ceiling keeps the span when zoomed', () => {
    const values = flat(0.5);
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 25, end: 75 }, {
        ...ONE_POINT,
        range: { min: 0, max: null },
    })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.495, 0.505]);
});

test('a band outside the zoomed window does not widen the span', () => {
    const values = flat(1);
    const dipsEarly = [
        { type: 'line', stack: 'band', data: timed([...flat(0.5, 10), ...flat(1, 50)]) },
        { type: 'line', stack: 'band', data: timed(flat(0, 60)) },
    ];
    const echart = renderedChart(values, { extraSeries: dipsEarly });
    chartOn(echart, values, { start: 50, end: 90 })._rescaleYAxis();
    assertExtent(yExtent(echart), [0.99, 1]);
});

test('zooming measures the clamped values a chart draws, not the raw ones', () => {
    const echart = renderedChart(flat(0.5));
    chartOn(echart, flat(0.52), { start: 25, end: 75 }, {
        ...ONE_POINT,
        range: { min: 0, max: 0.5 },
    })._rescaleYAxis();
    const [min, max] = yExtent(echart);
    assert.ok(min < 0.5 && max === 0.5, `extent ${min},${max}`);
    assert.ok(!echart.getModel().getComponent('yAxis').axis.inverse);
});

test('zooming a log axis keeps its padded bounds', () => {
    const values = flat(0.5);
    const echart = renderedChart(values, { logScale: true });
    chartOn(echart, values, { start: 25, end: 75 })._rescaleYAxis();
    const { min, max } = echart.getOption().yAxis[0];
    assert.ok(Math.abs(min - 0.45 / 1.2) < 1e-9, `min ${min}`);
    assert.ok(Math.abs(max - 0.55 * 1.2) < 1e-9, `max ${max}`);
});

test('zooming a flat line with no span keeps the padded axis', () => {
    const values = flat(0.5);
    const echart = renderedChart(values, { format: {} });
    chartOn(echart, values, { start: 25, end: 75 }, {})._rescaleYAxis();
    assertExtent(yExtent(echart), [0.445, 0.555]);
});

test('a line that varies by more than a point rescales on zoom as before', () => {
    const values = [...flat(0.2, 30), ...flat(0.8, 30)];
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 25, end: 75 })._rescaleYAxis();
    const [min, max] = yExtent(echart);
    assert.ok(min < 0.2 && max > 0.8, `extent ${min},${max}`);
});
