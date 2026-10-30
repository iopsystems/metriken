import test from 'node:test';
import assert from 'node:assert/strict';

// charts/host.js is what the chart modules call for what they do not hold in
// their spec. These check its defaults and what setChartHost accepts.
//
// The host is module state. The tests in this file run in order and need a
// module graph in which nothing has set a host yet, which node --test's
// default per-file process gives them.

const host = await import('../assets/charts/host.js');

test('defaults leave out the fetches and the step override', () => {
    const h = host.chartHost();
    assert.equal(h.executePromQLRangeQuery, null);
    assert.equal(h.applyResultToPlot, null);
    assert.equal(h.fetchQuantileSpectrumForPlot, null);
    assert.equal(h.getStepOverride(), null);
});

test('setChartHost refuses a key it does not know', () => {
    assert.throws(() => host.setChartHost({ getStepOverrides: () => 5 }), /unknown key 'getStepOverrides'/);
});

test('setChartHost refuses a value that is not a function', () => {
    assert.throws(() => host.setChartHost({ getStepOverride: 30 }), /'getStepOverride' is not a function/);
});

test('setChartHost merges over what earlier calls set', () => {
    const query = async () => null;
    try {
        host.setChartHost({ executePromQLRangeQuery: query });
        host.setChartHost({ getStepOverride: () => 7 });
        assert.equal(host.chartHost().executePromQLRangeQuery, query);
        assert.equal(host.chartHost().getStepOverride(), 7);
    } finally {
        host.resetChartHost();
    }
    assert.equal(host.chartHost().executePromQLRangeQuery, null);
});
