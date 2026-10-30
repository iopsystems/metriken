# metriken-charts

Browser chart modules for metriken recordings, as files a server can embed and
serve. These are the charts the rezolus viewer draws: line, multi-series,
percentile scatter, heatmaps and histogram heatmaps, with zoom, tooltips,
event markers and display-mode (decimated) data.

The crate exports `ASSETS`, `ASSETS_PATH` and the `include_dir` crate.
`ASSETS` embeds `assets/` with `include_dir` 0.7 (re-exported as
`metriken_charts::include_dir`), and `ASSETS_PATH` names the directory on the
machine that built the crate:

- `assets/charts/`: ES modules. They import only from inside this directory.
  `charts/echarts.min.js` is Apache ECharts 5.4.3; see NOTICE.
- `assets/charts.css`: the chart card styles. It also holds rezolus viewer
  styles for the single-chart view, the cgroup and GPU selectors, the CPU
  topology diagram and compare mode, which a page that draws none of them can
  ignore.

## What a page provides

- `echarts` as a global: load `charts/echarts.min.js`, or another ECharts 5,
  before the modules.
- CSS custom properties on `document.documentElement`:
  - every property named in `CSS_MAP` in `charts/util/colormap.js`, with no
    fallback: an unset property reads as an empty string. The modules read
    them when they draw, except the five behind `SCATTER_PALETTE`
    (`--accent`, `--chart-cyan`, `--chart-teal`, `--chart-green`,
    `--chart-purple`), which are read once, when `charts/util/colormap.js` is
    first imported;
  - optionally `--chart-1` to `--chart-10` and `--cgroup-1` to `--cgroup-16`,
    the series palettes, which fall back to built-in colours and are also
    read once, at import;
  - for `charts.css`: `--bg`, `--bg-card`, `--bg-card-gradient`,
    `--bg-card-hover`, `--border-emphasis`, `--card-radius`,
    `--content-padding`, `--danger`, `--danger-fg`, `--fg-on-accent`,
    `--font-mono`, `--font-sans`, `--font-size-base`, `--font-size-sm`,
    `--font-size-xs`, `--shadow-glow`, `--shadow-lg`, `--success`,
    `--transition-base`, and, for the compare and topology styles,
    `--compare-*` and `--topo-*`.

  Set the properties before importing the modules. A later theme change does
  not reach the colours read at import.

  The rezolus viewer's `src/viewer/assets/lib/style.css` defines all of them
  for a light and a dark theme, except `--topo-border` and `--topo-numa-cols`,
  which have defaults in `charts.css`.
- A host, through `charts/host.js`. Every key is optional:

  | key | used for | without it |
  | --- | --- | --- |
  | `executePromQLRangeQuery`, `applyResultToPlot` | the line chart's "total" toggle | no toggle |
  | `fetchQuantileSpectrumForPlot` | the percentile chart's spectrum checkboxes | no checkboxes |
  | `getStepOverride` | gap detection at a coarser step | no step override |
  | `getThemeVersion` | redrawing on a theme change | redrawn on the next data or format change |
  | `redraw` | re-rendering the page after a chart changes shared zoom | nothing |
  | `eventsForChart`, `subscribeEvents` | event markers and tags | no events |
  | `eventEditingAllowed`, `openEventForm` | "+ Add Event" on a frozen tooltip | not offered |
  | `openEventInfo` | an event tag's popover | a click on a tag does nothing |

  `setChartHost({ ... })` merges over what earlier calls set and refuses an
  unknown key or a value that is not a function.

## Drawing a chart

```js
import { Chart, ChartsState } from './charts/chart.js';

const el = document.createElement('div');
el.className = 'chart';
container.appendChild(el);

const chart = new Chart({ spec, chartsState: new ChartsState(), interval: 1 });
chart.mount(el);    // draws when the element scrolls into view
chart.update({ spec: next, interval: 1 });
chart.destroy();    // before removing the element
```

`update` redraws when one of these differs from the last spec:

- `data`, compared by its length and, element by element, its first and last
  entries (for a line chart, the timestamps and the last series);
- `time_data`, `multiSeries` or `familyBand`;
- `opts.format` or `series_names`, by identity;
- the host's theme version.

It does not redraw for a spec changed in place, or for a change only to the
entries between the first and the last.

`spec` is a plot descriptor: `opts` (`id`, `title`, `type`, `format`, ...) and
`data` (`[timestamps_s, values...]`), or display-mode data written by
`applyDisplayToPlot` in `charts/display.js`. `ChartsState` holds zoom shared by
the charts given the same instance.

## Display mode

`charts/display.js` decodes the binary bodies that rezolus's `display_wire`
encoder writes (`decodeDisplayBinary`, `decodeHeatmapBinary`), keeps decoded
responses in a per-query tile cache (`tileLookup`, `tileStore`,
`clearDisplayTiles`), and writes a response into a plot spec
(`applyDisplayToPlot`). It does not fetch; the page does.

## Tests

The chart code has node tests, which the published crate does not include:

```sh
node --test metriken-charts/tests/*.mjs
```

`tests/charts_boundary.test.mjs` fails when a module under `assets/charts/`
names a path outside it or uses a Mithril global.
