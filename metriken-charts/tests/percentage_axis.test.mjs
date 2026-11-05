import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

// A percentage chart whose values stay within one percentage point draws a
// y-axis at least one point wide, inside 0..100%. The tests read the axis
// extent from the vendored ECharts, which computes it from the option the
// chart modules build. Values are fractions, so 1.0 is 100%.
globalThis.document = globalThis.document || { documentElement: { getAttribute: () => null } };
globalThis.getComputedStyle = globalThis.getComputedStyle || (() => ({ getPropertyValue: () => '' }));

const echarts = createRequire(import.meta.url)('../assets/charts/echarts.min.js');
// ECharts measures axis labels on a canvas, and node has none.
echarts.setPlatformAPI({ measureText: (text) => ({ width: text.length * 7 }) });
const { getBaseYAxisOption } = await import('../assets/charts/base.js');
const { Chart, ChartsState } = await import('../assets/charts/chart.js');

const T0 = 1_700_000_000;

// An undisposed instance keeps its animation loop running, and node with it.
const rendered = [];
afterEach(() => {
    while (rendered.length) rendered.pop().dispose();
});

const timed = (values) => values.map((v, i) => [(T0 + i) * 1000, v]);

function renderedChart(values, { unit = 'percentage', logScale = false, extraSeries = [], sampling } = {}) {
    const echart = echarts.init(null, null, { renderer: 'svg', ssr: true, width: 600, height: 300 });
    echart.setOption({
        animation: false,
        xAxis: { type: 'time' },
        yAxis: getBaseYAxisOption(logScale, unit),
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
function chartOn(echart, values, zoomLevel, format = { unit_system: 'percentage' }) {
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
    const unitless = yExtent(renderedChart(values, { unit: null }));
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

test('a flat value on a chart with no unit is left alone', () => {
    const [min, max] = yExtent(renderedChart(flat(0.5), { unit: null }));
    assert.ok(max - min > 0.1, `extent ${min},${max}`);
});

test('values just over a point apart keep the axis ECharts chooses', () => {
    const values = [0.5, 0.515];
    assert.deepEqual(yExtent(renderedChart(values)), yExtent(renderedChart(values, { unit: null })));
});

test('a percentage above 100% keeps the axis ECharts chooses', () => {
    const percent = yExtent(renderedChart(flat(1.5)));
    const unitless = yExtent(renderedChart(flat(1.5), { unit: null }));
    assert.deepEqual(percent, unitless);
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
    assertExtent(yExtent(echart), [-1.1, 1.1]);
});

test('a window ECharts thinned to one point is measured on all of its points', () => {
    const values = Array.from({ length: 7200 }, (_, i) => 0.5 + 0.1 * Math.sin(i * 1.7));
    const zoom = { start: 50, end: 50.1 };
    const extentWith = (format) => {
        const echart = renderedChart(values, { sampling: 'lttb' });
        chartOn(echart, values, zoom, format)._rescaleYAxis();
        return yExtent(echart);
    };
    assert.deepEqual(extentWith({ unit_system: 'percentage' }), extentWith({}));
});

test('a span above the range ceiling keeps the padded axis', () => {
    const values = flat(0.52);
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 25, end: 75 }, {
        unit_system: 'percentage',
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
        unit_system: 'percentage',
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
        unit_system: 'percentage',
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

test('a line that varies by more than a point rescales on zoom as before', () => {
    const values = [...flat(0.2, 30), ...flat(0.8, 30)];
    const echart = renderedChart(values);
    chartOn(echart, values, { start: 25, end: 75 })._rescaleYAxis();
    const [min, max] = yExtent(echart);
    assert.ok(min < 0.2 && max > 0.8, `extent ${min},${max}`);
});
