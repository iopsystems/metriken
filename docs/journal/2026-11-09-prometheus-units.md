# Prometheus units: detect a unit in the name, normalize, and keep the original name

**Status:** SUPERSEDED (see "Reverted", at the end). Design 2026-11-09,
owner decisions the same day (D1a, D2a, D3a, below). Released in
metriken-recorder 0.12.0, metriken-storage 0.11.0 and metriken-query
0.48.0; removed in the releases after metriken-recorder 0.14.0.
Issue #379. Builds on #384 (float scalars,
`2026-11-06-float-scalars.md`) and #388 (histogram overflow,
`2026-11-09-histogram-overflow.md`).

## Goal

A Prometheus series whose name ends in a unit token is stored in the
normalized unit (nanoseconds or bytes), with the `unit` storage key set,
under a name whose token is the normalized one; the exporter's name is
kept in the internal label `__prometheus_name__`. A histogram's buckets,
`_sum` and overflow bound, a summary's quantiles and `_sum`, and plain
scalars of one metric are scaled by one factor. A query naming a series by
its exporter name still finds it.

Out of scope: native metriken producers (they set `unit` themselves),
exporter label values, and the `histogram_count`/`histogram_sum`
treatment of the overflow (#388 D3).

## Today

- **Histogram scale** (`metriken-recorder/src/prometheus.rs`,
  `convert_histogram`, line 449): ×1e9 when the name ends in `_seconds`,
  else `compute_generic_scale`, the smallest power of 10 that makes the
  smallest positive `le` at least 1. The factor is not written anywhere.
  `OVERFLOW_BOUND_KEY` is the top finite `le` times the same factor
  (line 504).
- **Scalars are not scaled.** A histogram's `_sum` and `_count` reach the
  converter as separate `Value::Untyped` samples (the parser's module doc,
  `exposition.rs`: "`# TYPE x histogram` makes `x_bucket` lines the
  buckets of histogram `x` (its `_sum` and `_count` are untyped)"), and the
  converter stores an untyped sample ending in `_total`, `_sum` or
  `_count` as a float counter, any other as a float gauge
  (`prometheus.rs:215-234`). So a `_seconds` histogram has buckets in
  nanoseconds and `_sum` in seconds. A summary's quantile values are
  float gauges under the summary's name with a `quantile` label
  (`prometheus.rs:251-270`); its `_sum`/`_count` are untyped like a
  histogram's.
- **`# UNIT` is dropped.** `parse_comment` (`exposition.rs:236`) returns
  `None` for any comment other than `# HELP` and `# TYPE`, and `Scrape`
  has no field for units.
- **Native histograms never arrive.** The recorder requests the text
  format (no protobuf `Accept` header is set anywhere in
  `metriken-recorder/src`), and native histograms are protobuf-only.
- **`unit` storage key.** It is in `STORAGE_KEYS`
  (`metriken-storage/src/labels.rs:30`) and in the writer's
  column-describing keys (`writer.rs:442`), so it is never a label. No
  code in this workspace reads it; native producers write it from static
  metadata (rezolus samplers: `nanoseconds` 128 times, `bytes` 75,
  `percentage` 13, by `grep unit = "..."` over `src/agent/samplers`), and
  rezolus's `.rez` reader returns it per metric
  (`crates/rez/src/reader.rs`,
  `metric_metadata_carries_unit_and_description_per_metric`). The
  Prometheus converter never writes it.
- **Names index.** `names.rs:39-45`: a storage key added after the
  metriken-storage release that first writes `MNI2` (0.10.0) needs
  `MNI3`. 0.10.0 was in release PR #389, open when this was designed and
  merged (7b99961) before this change was rebased onto main; this change
  targets 0.11.0 and takes `MNI3`. A reader that does not know
  an `MNI` version reads the segment's footer (`names.rs:36-38`).
- **Internal labels.** `Labels::from_metadata` (`labels.rs:106`) turns
  every non-storage key into a label, in both loaders (parquet
  `parquet.rs:1974`, live `memory.rs:416`), and identity is equality of
  the whole map. A `__` label is matchable in a selector, dropped by
  `without` and by default binary-op matching (`is_matched_label`,
  `labels.rs:91`; only `__recording__` is kept), and returned by
  metriken-query's listings; hiding it is the consumer's job
  (`is_internal_label` doc, `labels.rs:58-80`, and rezolus
  `docs/labels.md`, "Where the rule is enforced"). The same doc says "No
  writer in this workspace emits a `__` key into column metadata"
  (`labels.rs:71`), which this change makes false.
- **Where a selector's name is resolved.** Three places, all by name
  catalog, none by time range:
  1. `ArchiveReader`'s `Plan` (`metriken-query/src/archive.rs:72-117`)
     routes a query to the tables whose name catalogs hold a name the
     query references (`referenced_metrics`, parse only), then prunes
     recordings by scope and by recording labels. A name no table holds is
     "query references no metric present"; a name held only by pruned
     recordings answers empty (`missing`).
  2. The engine (`QueryEngine`, used directly by `MemoryStore`, `Tsdb`,
     the union and segmented sources) evaluates against one
     `&dyn DataSource`; `held_somewhere`
     (`promql/streaming/dispatch.rs:728`) decides `MetricNotFound` from
     `counter_names`/`gauge_names`/`histogram_names`/`gauge_histogram_names`.
  3. The histogram functions metriken parses by hand
     (`histogram_quantiles`, `histogram_heatmap`, `histogram_irate`,
     `histogram_mean/count/sum`) take their selector through
     `parse_metric_selector` (`promql/mod.rs:1183`).
- **Name matchers.** A selector names one metric: `foo` or
  `{__name__="foo"}` (`name_selector`, `promql/mod.rs:771`). A regex on
  `__name__` does not name a metric and is not evaluated, so it has no
  fallback to interact with.

## Settled from the code

1. **Hook.** In `PrometheusConverter::convert`, between `Scrape::parse`
   and the per-sample loop: a pass over every sample computes its stored
   name, factor and unit (`Normalized`), then the collision rule (item 7)
   runs over the whole scrape, then the existing loop emits with the
   stored name, `__prometheus_name__` added to the labels when the name
   changed, values multiplied by the factor, and `unit` in the metadata.
   `get_or_assign_id` keys on the stored name and labels.
   `convert_histogram` takes the factor instead of testing `_seconds`.
2. **Token table**, matched as the last `_`-separated word of the name
   after removing one of `_bucket`, `_sum`, `_count`, `_total`, as approved
   in the issue:

   | token | stored token, `unit` | factor |
   |---|---|---|
   | `seconds` / `milliseconds`, `ms`, `msec` / `microseconds`, `us` / `nanoseconds`, `ns` | `nanoseconds` | 1e9 / 1e6 / 1e3 / 1 |
   | `bytes` / `kilobytes`, `kb` / `kibibytes`, `kib` / `megabytes`, `mb` / `mebibytes`, `mib` / `gigabytes`, `gb` / `gibibytes`, `gib` | `bytes` | 1 / 1e3 / 2^10 / 1e6 / 2^20 / 1e9 / 2^30 |
   | `bits` | `bytes` | 1/8 |
   | `percent` | `percent` (name), `unit=percentage` | 1 |
   | `ratio`, `celsius`, `hertz`, `cores` | unchanged | 1 |

   `percentage` is the value rezolus samplers already use. `percent` and
   `ratio` stay distinct: a ratio is not multiplied by 100, because the
   exporter's 0–1 range is what a query against it expects, and nothing
   in the name says the two are the same quantity. The table lives in
   metriken-storage (a new public module next to `STORAGE_KEYS`), since
   the recorder writes by it and the query's fallback (item 9) derives
   candidate names from it.
3. **`# UNIT`.** The parser gains `Comment::Unit` and `Scrape::units`,
   keyed by family name; a sample's family is its name with one of
   `_total`, `_bucket`, `_sum`, `_count` removed, or its name. A UNIT
   value found in the token table decides the factor and the stored
   token, over the name's own token. The rule that keeps a series'
   meaning readable from its name: **a scaled series is always renamed**.
   When the name ends in a token, that token is replaced; when it does
   not (non-conforming OpenMetrics), the stored token is appended before
   the suffix (`foo_total` with `# UNIT foo seconds` is
   `foo_nanoseconds_total`). With factor 1 and no token in the name, the
   name is kept and only `unit` is written. A UNIT value not in the table
   is ignored and the name decides.
4. **Suffixes.** The stored name is the original with its token replaced
   and the suffix kept: `x_ms_sum` → `x_nanoseconds_sum` (×1e6),
   `x_ms_count` → `x_nanoseconds_count` (×1, no `unit`: it is a count),
   `x_seconds_total` → `x_nanoseconds_total` (×1e9),
   `x_bits_total` → `x_bytes_total` (÷8). The histogram is stored under
   its base name (`x_ms` → `x_nanoseconds`, as the parser already strips
   `_bucket`), and its `__prometheus_name__` is the base name `x_ms`,
   which is what a metriken selector names a histogram by (today a
   recording holds `x_ms`). An untyped `_bucket` sample (no `# TYPE`
   histogram line, so not grouped into a histogram) is kept as served:
   its `le` label is in the exporter's unit, and renaming it would put a
   seconds `le` under a nanoseconds name (corrected after review; the
   first build renamed it). `_count` is renamed so that
   `rate(x_nanoseconds_sum) / rate(x_nanoseconds_count)` pairs; binary
   matching ignores the differing `__prometheus_name__` values
   (`is_matched_label`).
5. **Histogram scale and the overflow.** A histogram's stored bounds are
   `le × unit factor × g`, where `g` is `compute_generic_scale` over the
   bounds already multiplied by the unit factor (1 for a `_seconds`
   histogram, so its stored buckets do not change; 10 for an unknown-unit
   histogram whose smallest bound is 0.5, as today; it also covers a
   `_ratio` histogram, whose bounds below 1 would otherwise truncate to 0,
   and `_bits` bounds below 8). `g` is written as the new storage key
   `scale` when it is not 1 (item 6). `overflow_bound` is the top finite
   `le` times the same total factor, as today with the new factor. The
   overflow counter is a count and is not scaled; it is named
   `<stored histogram name>_overflow` and carries the histogram's labels,
   `__prometheus_name__` included, because the reader links it by name
   and the histogram's labels (`OVERFLOW_KEY`, #388 "As built").
6. **The `scale` key.** `SCALE_KEY = "scale"`, an `f64` written as text,
   on a counting histogram column only, meaning stored value = value in
   `unit` (or in the exporter's unit when there is no `unit`) × scale.
   Added to `STORAGE_KEYS` and the writer's list, names index `MNI3`; an
   `MNI2` or `MNI1` index is still read with its label sets stripped of
   today's storage keys, as `MNI1` is now. Older readers: metriken-storage
   0.10.0 and earlier show `scale` as a label on such a histogram (they
   do not take `MNI3` and read the footer), and an exporter label named
   `scale` stops being a label on every series, in old recordings too,
   the same cost #388 stated for `overflow`. `unit` is already a storage
   key, so old readers already hide it.
7. **Collisions**, decided per converter (one endpoint for the life of
   the recorder process), sticky. A stored name collides when two
   different original names in a scrape map to it (`foo_seconds` and
   `foo_ms`), or when it equals a name the scrape serves unrenamed
   (`foo_seconds` and `foo_nanoseconds`). Storage would not merge them
   (`__prometheus_name__` differs, so they are different series), but a
   query of the stored name would return both and `sum` would count one
   quantity twice. The family of each original name in a collision (the
   name without `_bucket`, `_sum`, `_count` or `_total`, from
   `split_suffix`) is added to the converter's `keep_original` set, and
   every member of it is stored under its original name, unscaled, with
   no `unit` and no label, in that scrape and every later one. (Corrected
   after review: the first build kept only the colliding name, so a
   histogram `lat_seconds` beside a gauge `lat_ms` kept its buckets as
   seconds × 10 while its `_sum` and `_count` were renamed and its `_sum`
   scaled ×1e9.) A warning is logged once per converter, then at
   `debug`. A collision that first appears mid-run therefore renames the
   already-renamed series back to its original name from that scrape on:
   one series under two names in one recording, once. Per-scrape
   detection was rejected: a sibling that comes and goes would flip the
   name every time. The set is not persisted, so a recorder restarted
   before the sibling reappears renames again until it does.
8. **Untyped samples** follow the name and the UNIT line like typed ones;
   the `_total`/`_sum`/`_count` counter rule is unchanged and reads the
   stored name (the suffix is kept). **Summaries:** quantile values and
   `_sum` scaled, `_count` renamed only. **Native histograms:** none
   arrive (Today).
9. **The fallback's mechanics.** For a name `N` that the reachable
   sources hold as no kind, the candidates are the names the token table
   can produce from `N` (its own token replaced; each stored token
   appended, item 3); a candidate `C` that is held and has series with
   `__prometheus_name__ = N` replaces `N` in the selector, with the
   matcher `__prometheus_name__="N"` added. The candidates are a handful
   per name, so the fallback reads the label sets of those names only,
   not of every name in the archive. The rewrite is done once per query
   on the parsed expression after `name_selectors`, in `Plan` for
   routing, and in `parse_metric_selector` for the hand-parsed histogram
   functions. "Matches no series" is by name catalog, not by time range:
   a range-dependent rule would change which series a dashboard panel
   shows as it is zoomed. A regex on `__name__` is not evaluated today,
   so it gets no fallback. Two candidates both matching `N` is an error
   naming both. It offers the form that selects one on any source, the
   stored name with `__prometheus_name__="N"`, and over an archive also
   a recording-label matcher, which selects nothing on the live store or
   another non-archive source. (Corrected
   after review: the design said this needed two recorder versions with
   different tables; one version is enough, when two endpoints send
   different `# UNIT` lines for a name with no unit token, `x_total` with
   `# UNIT x seconds` in one and `# UNIT x kilobytes` in another. The
   first build read the first candidate instead of erroring; the
   reviewer's probe showed `rate(x_total[3s])` returning only one
   endpoint, in nanoseconds, and `x_total{job="b"}` returning nothing.
   The error is now built, in `Plan` and in the engine. A second review
   found its first wording offered only the recording-label matcher;
   it now offers the `__prometheus_name__` form everywhere.)
10. **Bits to bytes for counters.** ÷8 is a power of two, so it is exact
    in f64 (outside subnormals); `rate(x_bytes_total)` is bytes/s.
11. **Recorder upgrade.** Recordings made before keep their names and
    values; nothing rewrites them. A recording made after has the new
    names. How a query sees an archive holding both depends on D1 and D2.

## Decisions (owner, 2026-11-09)

D1a, D2a and D3a, as recommended. Release #389 (storage 0.10.0 with
`MNI2`, query 0.47.0, recorder 0.11.0) lands before this, so `scale` takes
`MNI3` and the CHANGELOG blocks are storage 0.11.0, query 0.48.0 and
recorder 0.12.0. The options as they were put:

### D1. What a fallback query returns

A query by an exporter name (`histogram_quantile(0.99, x_seconds)`,
`rate(x_seconds_sum[1m])`) is answered from series stored in nanoseconds.

- **D1a (recommended, chosen): the stored series as stored.** Values in the
  normalized unit; the result's `__name__` is the stored name
  (`x_nanoseconds`), with `__prometheus_name__="x_seconds"` among its
  labels, so the unit is visible in the result. Cost: a dashboard or KPI
  written for the exporter finds its data but reads ×1e9 (seconds →
  nanoseconds), ×1e3 for `_ms`, ÷8 for bits; thresholds and unit
  annotations written for the exporter are wrong for new recordings. In an
  archive with an older recording of the same endpoint, `x_seconds` holds
  exactly in the older one, so (D2) the query returns the older recording
  only, in seconds, and `x_nanoseconds` returns the newer only; nothing
  joins the two names (a query can: `x_nanoseconds or x_seconds * 1e9`).
- **D1b: converted back to the exporter's unit.** Values divided by the
  factor, `__name__` = `N`. A query written for the exporter reads as it
  did, for old and new recordings alike, and old and new recordings of
  one endpoint could then be unioned under `N` safely. Costs: one stored
  series reads in two units depending on the name typed; the factor must
  be stored per renamed column (a second key, since a UNIT-line rename is
  not derivable from the name), every scalar read path multiplies, and
  histogram quantile, band, heatmap, mean and sum outputs divide by a
  factor carried on the histogram stream; and it departs from the
  decided "exact match preferred" if unioning is wanted.

### D2. Over which recordings "matches no series" is decided

- **D2a (recommended, chosen): per selector, over the recordings that selector
  reaches** after the scope and the recording-label routing `Plan`
  already does per selector. `x_seconds{source="new"}` falls back when
  the new recording lacks the name even if an older recording in the
  archive has it. With D1a, two selectors of one query can then resolve
  differently (`x_seconds{source="old"} + x_seconds{source="new"}` adds
  seconds to nanoseconds); the result's `__name__` shows it only for
  unaggregated series.
- **D2b: per query, over every recording the query's scope reaches**,
  ignoring label matchers. One resolution per name per query, so units
  never mix within a query; but `x_seconds{source="new"}` answers empty
  whenever any reachable recording holds `x_seconds`.
- Not proposed: per series label matchers (`x_seconds{op="a"}` falling
  back while `x_seconds` does not), which mixes units within one name;
  and per recording inside one selector, which mixes units within one
  result under D1a.

### D3. The generic scale factor at query time

A histogram with no unit token whose smallest bound is below 1 is stored
×10^k today, unrecorded. This change records `scale`.

- **D3a (recommended, chosen): record only.** Quantiles read as today (×10^k), a
  reader that wants exporter units divides by `scale`. Old and new
  recordings of one endpoint pool consistently.
- **D3b: metriken-query divides histogram function outputs by `scale`.**
  New recordings read in exporter units; old recordings (no key, same
  ×10^k) read ×10^k, so pooling an old and a new recording of one
  endpoint mixes the two in one population. Also needs a per-series
  factor through pooling and the process-group combine, as the overflow
  bound needed in #388.

## Versions

- metriken-storage 0.11.0, breaking: `STORAGE_KEYS` gains `scale`, which
  changes which keys are labels in every recording (as `overflow` did in
  0.10.0); names index `MNI3`. Additive: the token table module,
  `SCALE_KEY`, `PROMETHEUS_NAME_LABEL`.
- metriken-query 0.48.0, breaking: requires metriken-storage 0.11.0,
  whose types (`DataSource`, `Labels`) are in its public API. The
  fallback itself only answers queries that were `MetricNotFound` or
  "references no metric" before (D3a: no quantile changes).
- metriken-recorder 0.12.0, breaking: series names, values (`_sum`,
  scalars in non-normalized units, bits) and labels change for a
  Prometheus endpoint.

metriken-charts 0.1.4 (fix; 0.1.3 is #390): its `STORAGE_KEYS` copy
(`assets/charts/util/labels.js`) had not taken `overflow` and
`overflow_bound` from #388; it takes them and `scale`.

## As built

- **Storage** (`metriken-storage`): `prometheus_units.rs` holds the token
  table, `normalize` and `fallback_candidates`, and
  `PROMETHEUS_NAME_LABEL`; `SCALE_KEY` is in `STORAGE_KEYS` and the
  writer's column keys; the names index is `MNI3`, and `MNI1` and `MNI2`
  are read with their label sets stripped of today's storage keys
  (`Frame::older`). `SamplerReader::holds_labeled` answers "does this
  table hold `C` with a series whose `__prometheus_name__` is `N`" from
  the names index where it can.
- **Recorder** (`metriken-recorder`): `exposition.rs` keeps `# UNIT`
  (`Scrape::units`); `PrometheusConverter::plan` normalizes each exporter
  name of a scrape, finds collisions and adds their families to
  `keep_original`;
  `convert` writes the stored name, the label, `unit`, and values ×
  factor; `convert_histogram` takes the factor, applies the generic scale
  to the scaled bounds and writes `scale` when it is not 1. A kept
  original is stored with factor 1, so a kept `_seconds` histogram's
  bounds are seconds × the generic scale (recorded), not ×1e9 as before
  this change. HELP text is kept under the stored name too.
- **Query** (`metriken-query`): `promql/prometheus_names.rs`. Over an
  archive, `Plan::new` resolves each selector (`resolve_names` in
  `archive.rs`) over the tables in its scope whose recording labels its
  filter can match, routes on the resolved names, and hands the
  resolution to the engine (`QueryEngine::with_names`), which rewrites
  the selectors it evaluates by (name, filter). Any other source resolves
  over the whole source when the engine evaluates. The hand-parsed
  histogram functions resolve in `parse_metric_selector`; `columns`
  resolves too, after `name_selectors`, so `{__name__="x"}` resolves as
  `x` does. Off an archive, the engine lists the source's names once
  (`QueryEngine::held`) and reads candidates' label sets only for a name
  it does not hold; before review it listed them for every named
  selector. The list is kept for the engine's life: one query for
  `MemoryStore` and archives; the reader's life for `ParquetReader`,
  `SegmentedParquetReader` and `UnionMetricsSource`, whose names are
  fixed at open. A binary operation's result keeps the left operand's
  `__prometheus_name__` (Prometheus keeps every label but `__name__`);
  matching and `without` ignore it.
- **Tests:** `metriken-storage/src/prometheus_units.rs` (each token, the
  suffixes, `# UNIT` precedence, candidates); `names.rs` and
  `tests/writer.rs` (`MNI1` and `MNI2` read as `MNI3`);
  `metriken-recorder/tests/prometheus_units.rs` through exposition text →
  `PrometheusConverter` → `ArchiveWriter` → archive → PromQL and the live
  `MemoryStore`: every token class, `# UNIT` over the name, `_sum`,
  `_count`, `_total`, a histogram's buckets, `_sum`, overflow and bound in
  nanoseconds, a summary, bits as a byte rate, the generic scale on a
  `_ratio`, `_bits` and unit-less histogram, a collision in one scrape and
  one mid-run (sticky), a collision that keeps a histogram's whole family,
  an untyped `_bucket` kept as served, an exporter name stored under two
  names (an error, recorded and live; over the archive a
  recording-label matcher picks one, and on any source the stored name
  with `__prometheus_name__="x_total"` does, which the error names),
  `columns` of `{__name__="x"}`, the fallback in PromQL and the
  hand-parsed histogram functions, recorded and live, an older recording
  beside a newer one (exact preferred, per selector, under a scope, with
  finite values that tell seconds from nanoseconds), and
  `__prometheus_name__` ignored by matching and `without`;
  `promql/prometheus_names.rs` (a query of six selectors lists the
  source's names once); `metriken-charts/tests/labels.test.mjs` (hidden
  from legends and from the labels that name series).
  `tests/float_scalars.rs` fixtures were renamed to names without a unit
  token (`cpu_time_total`), since they test that values are stored as
  served.

## rezolus (other repository; not edited here)

- `docs/labels.md`: the internal-labels table gains
  `__prometheus_name__` (written by metriken-recorder's Prometheus
  converter on a renamed series); rule 1's "the agent writes exactly one
  such label, `__uid__`; no writer emits another" becomes false; the
  storage-keys table gains `scale`, and `overflow`/`overflow_bound` from
  #388; the `unit` row's "written by" gains the Prometheus converter.
- KPI lists and dashboards over Prometheus endpoints that name `_seconds`,
  `_ms` or `_bits` series: under D1a they need the stored names to read
  in normalized units; under either option a dashboard that formats by
  the `unit` key now gets one for Prometheus series.
- Legends and the metric catalog hide `__prometheus_name__` already
  through `is_internal_label`; worth one test with a renamed series.

## Reverted

The owner reversed the design: every Prometheus series is stored under the
exporter's name in the exporter's units, and the `unit` key names that unit
(`metriken_storage::prometheus_units::UNITS`). The owner's reason: store
series as the exporter serves them, without renaming or rescaling, so a
metric's name is the one its exporter documents and no reader has to know
a rename to find it. Removed with it:
`__prometheus_name__`, metriken-query's exporter-name fallback and its
ambiguity error, and the collision path. No compatibility path reads
recordings made with the rewrite; only unreleased rezolus wrote them.

A histogram's buckets hold integers, so its bounds are still stored
multiplied by `scale` and rounded. A time unit has a fixed scale, its
nanoseconds per unit, so one metric's histograms from exporters with
different smallest buckets pool; any other histogram takes the smallest
power of 10 that makes every bound an integer. Every reader passes the
column's `scale` on each histogram row, and metriken-query divides each
value it takes from a bucket by it, so the histogram reads in its `unit`.
A histogram function whose output pools rows of two scales is refused:
buckets at two scales cannot be added.
