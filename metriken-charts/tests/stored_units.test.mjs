import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

// A series stored in seconds, milliseconds, KiB or percent is formatted from
// `format.unit`: each value is multiplied into the base of its unit system
// (nanoseconds, bytes, a 0..1 fraction) before it is scaled and suffixed. A
// spec with no `unit` formats from `unit_system` alone. Every chart type is
// drawn by its real module and its axis, tooltip and legend labels are read
// back.

// Every element the modules create, so the heatmaps' DOM legend can be read.
const created = [];
globalThis.document = {
    documentElement: { getAttribute: () => null },
    createElement: () => {
        const el = fakeElement();
        created.push(el);
        return el;
    },
    addEventListener() {},
    removeEventListener() {},
};
globalThis.getComputedStyle = () => ({ getPropertyValue: () => '' });

const echarts = createRequire(import.meta.url)('../assets/charts/echarts.min.js');
echarts.setPlatformAPI({ measureText: (text) => ({ width: text.length * 7 }) });
const { Chart, ChartsState } = await import('../assets/charts/chart.js');
const { configureLineChart } = await import('../assets/charts/line.js');
const { configureMultiSeriesChart } = await import('../assets/charts/multi.js');
const { configureScatterChart } = await import('../assets/charts/scatter.js');
const { configureHeatmap } = await import('../assets/charts/heatmap.js');
const { configureQuantileHeatmap } = await import('../assets/charts/quantile_heatmap.js');
const { configureHistogramHeatmap } = await import('../assets/charts/histogram_heatmap.js');
const { specForSourceMetric } = await import('../assets/charts/source_metric.js');
const { createAxisLabelFormatter, resolveUnit } = await import('../assets/charts/util/units.js');

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

const rendered = [];
afterEach(() => {
    while (rendered.length) rendered.pop().dispose();
    created.length = 0;
});

// The formatter a chart with this format would use.
const fmt = (format) => {
    const { unitSystem, factor } = resolveUnit(format);
    return createAxisLabelFormatter(unitSystem, 2, factor);
};

// ── resolveUnit and the formatter ───────────────────────────────────────────

test('a seconds series formats as time', () => {
    assert.deepEqual(resolveUnit({ unit: 'seconds' }), { unitSystem: 'time', factor: 1e9 });
    const f = fmt({ unit: 'seconds' });
    assert.equal(f(0.25), '250 ms');
    assert.equal(f(1.5), '1.5 s');
    assert.equal(f(0.000004), '4 μs');
    assert.equal(f(0.0000005), '500 ns');
    // The same with the system named.
    assert.equal(fmt({ unit: 'seconds', unit_system: 'time' })(0.25), '250 ms');
});

test('a milliseconds and a microseconds series format as time', () => {
    const ms = fmt({ unit: 'milliseconds' });
    assert.equal(ms(1.5), '1.5 ms');
    assert.equal(ms(2500), '2.5 s');
    assert.equal(ms(0.25), '250 μs');
    assert.equal(fmt({ unit: 'microseconds' })(250), '250 μs');
    assert.equal(fmt({ unit: 'nanoseconds' })(250), '250 ns');
});

test('a KiB series formats as bytes, and as a data rate when charted as one', () => {
    assert.deepEqual(resolveUnit({ unit: 'kibibytes' }), { unitSystem: 'bytes', factor: 1024 });
    const kib = fmt({ unit: 'kibibytes' });
    assert.equal(kib(4), '4 KB');
    assert.equal(kib(1024), '1 MB');
    assert.equal(kib(0.5), '512 B');
    assert.equal(fmt({ unit: 'kilobytes' })(1), '1000 B');
    assert.equal(fmt({ unit: 'mebibytes' })(3), '3 MB');
    assert.equal(fmt({ unit: 'gibibytes' })(2), '2 GB');
    assert.equal(fmt({ unit: 'megabytes' })(1), '976.56 KB');
    assert.equal(fmt({ unit: 'gigabytes' })(1), '953.67 MB');
    // datarate's scales are decimal.
    assert.equal(fmt({ unit: 'kibibytes', unit_system: 'datarate' })(1000), '1.02 MB/s');
});

test('a percent series formats as a percentage without the x100', () => {
    assert.deepEqual(resolveUnit({ unit: 'percent' }), { unitSystem: 'percentage', factor: 0.01 });
    const pct = fmt({ unit: 'percent' });
    assert.equal(pct(45), '45 %');
    assert.equal(pct(99.5), '99.5 %');
    assert.equal(fmt({ unit: 'percent', unit_system: 'percentage' })(45), '45 %');
    // `percentage` is read as `percent` (0 to 100): rezolus samplers and
    // metriken-recorder 0.12–0.14 write that name.
    assert.equal(fmt({ unit: 'percentage' })(45), '45 %');
    assert.equal(fmt({ unit: 'ratio' })(0.45), '45 %');
});

test('bits, hertz, celsius and cores have a system each', () => {
    assert.equal(fmt({ unit: 'bits' })(2500), '2.5 Kbps');
    assert.equal(fmt({ unit: 'hertz' })(2.4e9), '2.4 GHz');
    assert.equal(fmt({ unit: 'celsius' })(61.5), '61.5 °C');
    assert.equal(fmt({ unit: 'cores' })(3.25), '3.25 cores');
});

test('an unset or unknown unit formats from unit_system alone', () => {
    for (const format of [{ unit_system: 'time' }, { unit_system: 'time', unit: 'fortnights' },
        { unit_system: 'time', unit: 'constructor' }, { unit_system: 'time', unit: null }]) {
        assert.deepEqual(resolveUnit(format), { unitSystem: 'time', factor: 1 }, JSON.stringify(format));
        assert.equal(fmt(format)(250000000), '250 ms');
    }
    assert.deepEqual(resolveUnit({}), { unitSystem: undefined, factor: 1 });
    assert.deepEqual(resolveUnit(), { unitSystem: undefined, factor: 1 });
    assert.equal(fmt({ unit_system: 'percentage' })(0.45), '45 %');
    assert.equal(fmt({ unit_system: 'bytes' })(4096), '4 KB');
});

test('a unit_system of another dimension ignores the unit', () => {
    // rezolus charts CPU as `irate(cpu_usage[5m]) / 1e9`: the query has
    // already put the nanoseconds counter's rate in the percentage base.
    assert.deepEqual(resolveUnit({ unit: 'nanoseconds', unit_system: 'percentage' }),
        { unitSystem: 'percentage', factor: 1 });
    assert.equal(fmt({ unit: 'nanoseconds', unit_system: 'percentage' })(0.45), '45 %');
    for (const [unit, unit_system] of [['seconds', 'count'], ['bytes', 'time'], ['bits', 'count'],
        ['percent', 'time'], ['celsius', 'percentage'], ['kibibytes', 'rate'], ['bytes', 'bitrate'],
        ['bits', 'bytes'], ['bits', 'datarate'], ['seconds', 'percentage']]) {
        assert.deepEqual(resolveUnit({ unit, unit_system }), { unitSystem: unit_system, factor: 1 },
            `${unit} as ${unit_system}`);
    }
});

// ── every chart type ────────────────────────────────────────────────────────

const T0 = 1_700_000_000;
const N = 20;
const times = () => Array.from({ length: N }, (_, i) => T0 + i);
const flat = (v) => Array.from({ length: N }, () => v);

// A chart drawn by its real chart module into an SSR ECharts instance.
function configured(configure, style, data, format, spec = {}) {
    const echart = echarts.init(null, null, { renderer: 'svg', ssr: true, width: 600, height: 300 });
    rendered.push(echart);
    const chart = new Chart({
        spec: { opts: { id: 'c', title: 't', type: 'gauge', style, format }, data, ...spec },
        chartsState: new ChartsState(),
        interval: 1,
    });
    chart.echart = echart;
    chart.domNode = fakeElement();
    configure(chart);
    return chart;
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

const yLabel = (chart) => {
    const y = chart.echart.getOption().yAxis;
    return (Array.isArray(y) ? y[0] : y).axisLabel.formatter;
};
const tooltip = (chart) => {
    const t = chart.echart.getOption().tooltip;
    return (Array.isArray(t) ? t[0] : t).formatter;
};
const legendLabels = () => created
    .filter((el) => el.className === 'heatmap-legend-tick-label')
    .map((el) => el.textContent);

// The same quantity three ways: nanoseconds with only `unit_system: 'time'`,
// and seconds and milliseconds with the unit named. Each reads the same.
const SAME_QUANTITY = [
    { format: { unit_system: 'time' }, scale: 1e9 },
    { format: { unit: 'seconds' }, scale: 1 },
    { format: { unit: 'milliseconds', unit_system: 'time' }, scale: 1e3 },
];
const label = (format) => JSON.stringify(format);

for (const { format, scale } of SAME_QUANTITY) {
    test(`a line chart labels its axis and tooltip from ${label(format)}`, () => {
        const chart = configured(configureLineChart, 'line', [times(), flat(0.25 * scale)], format);
        assert.equal(yLabel(chart)(0.25 * scale), '250 ms');
        const html = tooltip(chart)([{ seriesName: 't', marker: '', value: [T0 * 1000, 0.25 * scale] }]);
        assert.match(html, /250 ms/);
    });

    test(`a multi-series chart labels its axis and tooltip from ${label(format)}`, () => {
        const chart = configured(configureMultiSeriesChart, 'multi',
            [times(), flat(0.25 * scale), flat(2 * scale)], format, { series_names: ['a', 'b'] });
        assert.equal(yLabel(chart)(2 * scale), '2 s');
        const html = tooltip(chart)([
            { seriesName: 'a', marker: '', value: [T0 * 1000, 0.25 * scale] },
            { seriesName: 'b', marker: '', value: [T0 * 1000, 2 * scale] },
        ]);
        assert.match(html, /250 ms/);
        assert.match(html, /2 s/);
    });

    test(`a percentile chart labels its axis, OOB axis and tooltip from ${label(format)}`, () => {
        const data = [times(), flat(0.004 * scale), flat(0.25 * scale)];
        const plain = configured(configureScatterChart, 'scatter', data, format, { series_names: ['0.5', '0.99'] });
        assert.equal(yLabel(plain)(0.004 * scale), '4 ms');
        const html = tooltip(plain)([{ seriesName: 'p99', marker: '', value: [T0 * 1000, 0.25 * scale] }]);
        assert.match(html, /250 ms/);
        // With a range.max the axis is capped and values over it are parked
        // in the OOB band; the labels below it keep the unit.
        const capped = configured(configureScatterChart, 'scatter', data,
            { ...format, range: { max: 0.1 * scale } }, { series_names: ['0.5', '0.99'] });
        assert.equal(yLabel(capped)(0.004 * scale), '4 ms');
    });

    test(`a decimated percentile chart labels its axis and tooltip from ${label(format)}`, () => {
        const col = () => Float64Array.from(flat(0.004 * scale));
        const boxplot = [{ metric: {}, t: times(), min: col(), lo: col(), median: col(), hi: col(), max: col() }];
        const chart = configured(configureScatterChart, 'scatter', [times(), flat(0.004 * scale)], format,
            { series_names: ['0.5'], boxplot, boxplotDecimated: true });
        const types = chart.echart.getOption().series.map((s) => s.type);
        assert.ok(types.length > 0 && !types.includes('scatter'), `drawn as bands, not dots: ${types}`);
        assert.equal(yLabel(chart)(0.004 * scale), '4 ms');
        const html = tooltip(chart)([{ seriesName: 'p50', marker: '', value: [T0 * 1000, 0.004 * scale] }]);
        assert.match(html, /4 ms/);
    });

    test(`a heatmap labels its tooltip and legend from ${label(format)}`, () => {
        const chart = stubChart({
            opts: { title: 't', format },
            time_data: [T0, T0 + 1],
            data: [[0, 0, 0.001 * scale], [1, 0, 0.003 * scale]],
            min_value: 0.001 * scale,
            max_value: 0.003 * scale,
        });
        configureHeatmap(chart);
        const html = tooltip(chart)({ data: [T0 * 1000, 0, 0, null, 0.003 * scale] });
        assert.match(html, /3 ms/);
        const ticks = legendLabels();
        assert.ok(ticks.includes('1 ms') && ticks.includes('3 ms'), JSON.stringify(ticks));
    });

    test(`a quantile heatmap labels its tooltip and legend from ${label(format)}`, () => {
        const chart = stubChart({
            opts: { title: 't', format },
            data: [[T0, T0 + 1], [0.001 * scale, 0.1 * scale]],
            series_names: ['0.99'],
        });
        configureQuantileHeatmap(chart);
        const html = tooltip(chart)({ data: [(T0 + 1) * 1000, 0, 0.1 * scale] });
        assert.match(html, /100 ms/);
        const ticks = legendLabels();
        assert.ok(ticks.includes('1 ms') && ticks.includes('100 ms'), JSON.stringify(ticks));
    });
}

// Bucket bounds 1..1e9 in nanoseconds, with or without `unit_system: 'time'`.
for (const format of [{ unit_system: 'time' }, {}, { unit: 'nanoseconds' }]) {
    test(`a histogram heatmap labels its buckets from ${label(format)}`, () => {
        const bucketBounds = Array.from({ length: 10 }, (_, i) => 10 ** i);
        const chart = stubChart({
            opts: { title: 't', format },
            time_data: [T0, T0 + 1],
            bucket_bounds: bucketBounds,
            data: [[0, 3, 5], [1, 6, 7]],
        });
        configureHistogramHeatmap(chart);
        const axis = chart.echart.getOption().yAxis.axisLabel.formatter;
        assert.equal(axis(3), '1 μs');
        assert.equal(axis(6), '1 ms');
        // A cell from 1 μs (log 3) to 10 μs.
        const html = chart.echart.getOption().tooltip.formatter({ data: [T0 * 1000, 3, 4, 5, 0] });
        assert.match(html, /1 μs/);
    });
}

test('a histogram heatmap reads its bucket unit from `unit`, not `unit_system`', () => {
    const chart = stubChart({
        opts: { title: 't', format: { unit: 'milliseconds', unit_system: 'count' } },
        time_data: [T0, T0 + 1],
        bucket_bounds: [1, 10, 100, 1000],
        data: [[0, 1, 5], [1, 2, 7]],
    });
    configureHistogramHeatmap(chart);
    assert.equal(chart.echart.getOption().yAxis.axisLabel.formatter(1), '10 ms');
});

test('a percent series charted as a line reads in percent', () => {
    const chart = configured(configureLineChart, 'line', [times(), flat(45)], { unit: 'percent' });
    assert.equal(yLabel(chart)(45), '45 %');
    const kib = configured(configureLineChart, 'line', [times(), flat(4)], { unit: 'kibibytes' });
    assert.equal(yLabel(kib)(4), '4 KB');
});

test('with no unit or unit system, the axis and tooltip are unchanged', () => {
    const chart = configured(configureLineChart, 'line', [times(), flat(0.25)], {});
    assert.equal(yLabel(chart)(0.25), 0.25);
    assert.equal(yLabel(chart)(250000), '2.5e+5');
    const html = tooltip(chart)([{ seriesName: 't', marker: '', value: [T0 * 1000, 0.25] }]);
    assert.match(html, /0\.25/);
});

// ── source metrics ──────────────────────────────────────────────────────────

test('a source metric spec carries the catalog unit', () => {
    assert.deepEqual(
        specForSourceMetric({ name: 'req_seconds', metric_type: 'gauge', unit: 'seconds' }).opts.format,
        { unit: 'seconds' });
    assert.deepEqual(
        specForSourceMetric({ name: 'rx_bytes_total', metric_type: 'counter', unit: 'bytes' }).opts.format,
        { unit: 'bytes', unit_system: 'datarate' });
    assert.deepEqual(
        specForSourceMetric({ name: 'lat', metric_type: 'histogram', unit: 'seconds' }).opts.format,
        { unit: 'seconds' });
    assert.equal(specForSourceMetric({ name: 'up', metric_type: 'gauge' }).opts.format, undefined);
});

test('a source time counter divides its rate into a fraction and is drawn as a percentage', () => {
    for (const [unit, query] of [
        ['seconds', 'irate(t[5m])'],
        ['milliseconds', 'irate(t[5m]) / 1000'],
        ['microseconds', 'irate(t[5m]) / 1000000'],
        ['nanoseconds', 'irate(t[5m]) / 1000000000'],
    ]) {
        const spec = specForSourceMetric({ name: 't', metric_type: 'counter', unit });
        assert.equal(spec.promql_query, query, unit);
        assert.deepEqual(spec.opts.format, { unit_system: 'percentage' }, unit);
        assert.equal(fmt(spec.opts.format)(0.5), '50 %');
    }
});

test('a source bytes or bits counter keeps its unit in a rate system', () => {
    const kib = specForSourceMetric({ name: 'rx_kibibytes_total', metric_type: 'counter', unit: 'kibibytes' });
    assert.equal(kib.promql_query, 'irate(rx_kibibytes_total[5m])');
    assert.deepEqual(kib.opts.format, { unit: 'kibibytes', unit_system: 'datarate' });
    assert.equal(fmt(kib.opts.format)(4), '4.1 KB/s');
    const bits = specForSourceMetric({ name: 'rx_bits_total', metric_type: 'counter', unit: 'bits' });
    assert.equal(bits.promql_query, 'irate(rx_bits_total[5m])');
    assert.deepEqual(bits.opts.format, { unit: 'bits', unit_system: 'bitrate' });
    assert.equal(fmt(bits.opts.format)(2500), '2.5 Kbps');
});

test('a source bits gauge is drawn as a count of bits, not a rate', () => {
    const spec = specForSourceMetric({ name: 'link_bits', metric_type: 'gauge', unit: 'bits' });
    assert.equal(spec.promql_query, 'link_bits');
    assert.deepEqual(spec.opts.format, { unit_system: 'count' });
    assert.equal(fmt(spec.opts.format)(2500), '2.5 K');
});

test('a source gauge or histogram carries its unit, and ignores a scale', () => {
    const g = specForSourceMetric({ name: 'temp_celsius', metric_type: 'gauge', unit: 'celsius' });
    assert.equal(g.promql_query, 'temp_celsius');
    assert.deepEqual(g.opts.format, { unit: 'celsius' });
    const h = specForSourceMetric({ name: 'hit_ratio', metric_type: 'histogram', unit: 'ratio', scale: '10' });
    assert.deepEqual(h.opts.format, { unit: 'ratio' });
});
