# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### metriken-query 0.42.0

- **Changed:** a query over an archive reads each process group (copies of
  one source, a recording and a hindsight dump of it, two recorders of one
  agent; `ArchiveReader::read_groups`) as one recording, so a process counts
  once, except across a bridge (a continuous recording beside separate
  recordings of attempts), which is a recording of its own and still counts
  twice beside them until its pieces join their attempts:
  `sum(rate(cpu_cycles[1m]))` over a source and its copy equals the
  source alone, where it was twice that, and the group's series carry one
  label set, without `__recording__`. Each source of a group, and each
  producer epoch of a source, is evaluated alone up to the per-series
  function (`rate`, `irate`, a gauge read, `avg_over_time`, `idelta`,
  `deriv`), as one recording is, and the results are combined per series:
  at each point the finest source whose rows cover the function's whole
  window answers (ties to the lowest uuid, then catalog order); else the
  finest holding rows there; and where no source holds rows (a gap longer
  than twice the larger of the median spacings before and after it), there
  is no point. No
  value is a difference between rows of two sources, and no increase is
  taken across an epoch's `from_ts`, so a restart is never read as a spike
  (`rate` across one read 83 million per second on a fixture whose counter
  restarted at half its last value; it now reads the true 1000/s).
  Aggregation and binary operators run on the combined series. Rules S1 to
  S9 of `docs/journal/2026-10-19-recording-identity-and-views.md`.
- Cost: `rate` and `irate` on the grid over process groups are computed in
  one batched scan of every recording the query reaches, each group's members
  combined point by point as the scan reads (a point is answered once every
  member has passed it and the rows nine past it decide its coverage, so a
  group holds no more than a few rows' worth of points per series), and
  aggregated or reduced for display as the scan reads, as one recording is.
  A member clipped to a producer epoch, raw placement, explicit evaluation
  timestamps, gauges and the per-series rate path read each member alone.
  On the 9.6 h recording with a copy of itself (one uuid), medians of three
  interleaved rounds, against the same archive read twice by 0.41.0: a
  display query of `sum(irate(cgroup_cpu_usage[5s]))` over 9.6 h at a 1 s
  step 86 ms against 56 ms (peak RSS 279 MB against 191 MB; one source 30
  ms and 128 MB), of `sum(irate(cpu_usage[5s]))` 27 ms against 21 ms (150
  MB against 119 MB), `rate(cgroup_cpu_usage[1m])` over 1 h at 1 s 13.8 ms
  against 11.9 ms, `sum(irate(cgroup_cpu_usage[5s]))` over 1 h 10.0 ms
  against 5.8 ms, and `sum(rate(cgroup_cpu_usage[1m]))` over 9.6 h at 60 s
  39 ms against 33 ms. Single-source archives take the path they took
  before (six interleaved rounds: 30.9 against 29.8 ms for the 9.6 h display
  query, 6.04 against 5.93 ms for the 1 h `rate`, spreads overlapping).
- Histogram functions over a group read one of its sources alone, until
  they are combined per source: the one covering the most of the query's
  range, ties to the finest. They count a process once and leave the other
  sources' spans out.
- A recording that does not hold the metric a function reads (a filtered
  copy, another host) adds no series to it, beside a group as beside a
  single source; series come in catalog order.
- Archives whose sources form no group of more than one member, each of
  one producer epoch, answer as before (a differential harness,
  `metriken-storage/tests/answers_dump.rs`): byte for byte on six fixtures
  and one long-table recording, and on a wide-table recording and a 9.6 h
  recording identical apart from last-place differences in summed bands,
  which two runs of 0.41.0 also show. A source holding several producer
  epochs is now read per epoch, which changes its answers only across each
  restart.

### metriken-recorder 0.5.0

- **Changed:** takes metriken-storage 0.5, whose `WriterConfig` it
  re-exports.

### metriken-storage 0.5.0

- **Added:** `scan::interleave`, several counter scans read as one in time
  order, their series concatenated (what `RecordingsSource` joins its
  recordings' scans with).

- **Changed:** `ArchiveReader::series_labels` labels each source with its
  read group's labels (`read_groups`), so the sources of one process group
  carry one label set and `__recording__` tells apart only read groups whose
  label sets are equal (two processes recorded with the same labels), with
  the catalog index of the group's first source. `counter_labels`,
  `gauge_labels` and `histogram_labels` list a group's series once.
- **Added:** `scan::CounterScan::into_parts`, the matched series and the
  chunk reader, for a caller that wraps the reader.
- **Added:** `ArchiveReader::read_groups`, `read_group_of` and
  `source_position`, and `process::{read_groups, ReadGroup}`: what a query
  reads as one recording, a process group or the part of one whose sources
  agree on every label. A key some sources carry takes the carried value; a
  key carried with two values splits the group by value (a source lacking
  it takes its uuid group's value when that is one value).

- **Added:** process groups. `ArchiveReader::process_groups()` groups an
  archive's sources by producer process, by the rules of
  `docs/journal/2026-10-19-recording-identity-and-views.md`. Sources that
  share a uuid (dendro copies, hindsight dumps of one buffer) are taken as
  one uuid group. A bridge, a uuid group that overlaps two others which do
  not overlap each other, is set aside: an armed group bridges groups of its
  own `arm`, an unarmed one (shared context) bridges armed groups, and a
  group carrying two arms none. The remaining uuid groups join where they
  share a producer epoch and the intersection of their spans in it holds a
  row of each, closed transitively; groups that share no epoch overlap by
  span, from the first row of any member to the last. Each
  `producer_epochs` entry covers its source from its `from_ts` to the next
  entry's, in `from_ts` order. The result names sources by their position
  in `ArchiveReader::sources()`, which gives each one's labels, catalog
  index, uuid, span and epoch spans, says which joins connected each group,
  and carries notices: copies whose `host` differs, two sources of one
  epoch whose spans intersect without a row of each, and pairs whose rows
  could not be read (a pair is known to share no row when either source is
  known to have none in the intersection). The pure rules are
  `process::form`, over `SourceInfo`.
  metriken-query 0.42.0 reads each process group once.
- **Added:** `Catalog::sources_with_uuids`, defaulting to `sources` with no
  uuid; `DendroCatalog` reads the uuid in the same query. A `Catalog`
  wrapper that forwards `sources` and not this method loses the uuids, so
  its copies are not grouped.
- **Added:** `SamplerReader::has_row_in(lo, hi)`, `Some(found)` or `None`
  when it cannot tell (a failed read, a segment gone since open, WAL rows
  sealed or evicted since open). It answers from the catalog's segment
  spans listed at open, reading a segment's timestamp column only when that
  segment starts before the range and ends after it.
- Cost: `process::form` visits only the pairs of uuid groups whose spans
  intersect, plus up to (uuid groups × pairs of one epoch without a shared
  row) for the bridge search those pairs need, and keeps memory linear in
  the uuid groups plus those pairs (1000 sources: 4 ms for a fleet of one
  span and distinct epochs, 14 ms for one epoch recorded 1000 times; 3000
  sources: 32 ms and 120 ms; peak RSS 14 MB for all of them,
  `tests/group_cost.rs` `form_scale`). The worst case is many sources of one
  epoch that intersect pairwise and share no row: nested spans with rows only
  at their ends take 42 ms at 200 sources, 332 ms at 400 and 2.94 s at 800
  with a row check that reads nothing (425 M checks at 800),
  with a notice per pair (`form_nested_apart`). Opening the 9.6 h
  single-source archive is unchanged (median 11.8 to 12.4 ms before, 12.0 to
  12.1 ms after, three sets of 11 opens each), and grouping 64 sources that
  share one epoch takes 0.25 to 0.6 ms beside an open of 335 ms or more.
- **Fixed:** `HistogramStream::merge` and `merge_on_clock` overcounted when
  they rebucketed a stream to a coarser configuration: buckets that land in
  one coarser bucket had their cumulative counts summed, so 3 observations
  of 5 and 1 of 6 at grouping power 2 read as 7 observations at grouping
  power 0. The merged bucket now takes the last cumulative count. Only
  merges of streams of different configurations (several files or
  recordings of one metric) were affected. In queries,
  `histogram_count` and `histogram_quantile` over two recordings or files
  of one metric with different grouping powers counted the finer one's
  observations more than once; present since metriken-query 0.37.0.

### metriken-storage 0.4.0

- **Breaking:** dendro is 0.6 (was 0.5). Archives this release creates are
  dendro schema version 5, which releases built on dendro 0.5.1 or earlier
  (metriken-storage 0.3.x, metriken-recorder 0.3.x) refuse with `unsupported
  archive schema version 5`. That covers `ArchiveWriter`, which always
  creates a new file, and the `.rez` to dendro conversion
  (`rez::to_dendro`). Schema 4 archives still read, sealed segments and a
  live WAL tail alike. On schema 5 a segment's index (the metric-name
  fingerprint `Encoder` writes) is a row of the `segment_indexes` table
  rather than the `segments.caller_index` column; `Catalog::segment_indexes`
  reads either.
- **Breaking:** `Encoder`'s `SegmentEncoder` implementation follows dendro
  0.6: `encode` and `encode_after` take `&dendro::segment::EncodeContext` in
  place of the stream name. A caller driving the encoder itself builds one
  with `EncodeContext::new(source_id, stream)`. `DendroCatalog` wraps a
  dendro 0.6 `Archive`.
- **Breaking:** `Encoder::for_streams` is replaced by
  `Encoder::for_sources`, which takes each source's id with that source's
  stream list (`Archive::all_streams(id)`), and `Encoder::for_archive`,
  which builds the same from every source of an `Archive`. The ids are those
  of the archive the rows are read from, a copy's input included, since
  dendro passes that archive's id to the encoder. A caller that built one
  stream list from every source passes `Encoder::for_archive(&archive)?`
  instead. For a copy of an archive still being written, build it inside
  the read snapshot the copy reads, as `for_archive`'s documentation shows;
  a long stream that appears between the two is otherwise encoded wide and
  the copy fails.
- **Fixed:** whether a stream is long is decided per source. The writer and
  `for_streams` kept one set of long stream names for every source, so when
  one source recorded a group long and another recorded a group of the same
  name wide, the wide source's rows were sealed as long, the seal failed
  with `failed to decode a WAL long row: array had incorrect length,
  expected 4`, and that source's `sync` returned the error, which stopped
  the recording. A copy or dump of such an archive with `for_streams` failed
  the same way on the wide source's unsealed rows. Present since
  metriken-storage 0.1.0, and in metriken-archive's writer before it moved
  (0.3.5 has it).

### metriken-exposition 0.24.0

- **Breaking:** with `stream`, dendro is 0.6 (was 0.5).
  `FrameProducer::handshake`, `interval` and `empty_interval` return dendro
  0.6's `replicate::Frame`, and `FrameProducer::opening` its `Result`, so a
  caller naming those types must use dendro 0.6. The frames on the wire are
  unchanged, so 0.23 and 0.24 producers serve the same bytes to any
  subscriber.

### metriken-recorder 0.4.0

- **Breaking:** dendro is 0.6 (was 0.5), and the re-exported `WalRow` and
  `Frame` are dendro 0.6's. The archive a `Recorder` writes is dendro schema
  version 5, which metriken-recorder 0.3.x and other readers on dendro 0.5.1
  or earlier refuse. Takes metriken-storage 0.4 and metriken-exposition
  0.24.

### metriken-query 0.41.0

- **Breaking:** takes metriken-storage 0.4. Its `BufferPool`,
  `CompositionSource`, `UnionChild` and `ArchiveReader` appear in this
  crate's API. With `archive`, it reads dendro archives of schema versions 4
  and 5.

### metriken-storage 0.3.2

- `RezDb::update_recording_labels`, which replaces one recording's labels in
  place, as `update_recording_metadata` replaces its metadata. An assembly of
  several `.rez` archives uses it to tag each copied recording (an A/B arm)
  without rewriting segments.

### metriken-recorder 0.3.1

- **Fixed:** a scraped Prometheus label value is stored as the exposition
  text spells it. The `prometheus-parse` crate split each label block on
  every `,` and each pair on every `=`, so `op="a=b"` and `op="a="` were
  stored with no `op` label, `op="a,b"` as `op="a"`, `op="x=1,y=2"` as a
  label `y="2"` with no `op`, a line whose value held `}` was dropped, and
  `\"`, `\\` and `\n` were stored as written instead of as `"`, `\` and a
  newline. The converter now reads exposition text with its own parser
  (`exposition.rs`), and `prometheus-parse` and `chrono` are no longer
  dependencies. Series whose labels became identical after the loss (two
  `op` values that each contain `=`) are now told apart, and a value with
  an escape is stored unescaped, so it no longer matches a recording made
  before this release. Archives already written are read unchanged.
- A sample line that does not parse is skipped. `prometheus-parse` kept
  some of these under whatever pairs it could split: a label name with
  `.`, `-` or `:` or in quotes, an unquoted or single-quoted value, an
  unescaped `"` in a value, an empty pair. Each scrape with skipped lines
  is logged with the count and the first line (up to 200 characters,
  quoted) inside a `scrape` span naming the endpoint: at `warn` the first
  time for each endpoint in a run, at `debug` after that.

### metriken-exposition 0.23.2

- `RegistryPasses` reuses a pass for three quarters of its floor, where it
  reused it for the whole floor. A subscriber served at an interval equal
  to the floor (as `StreamSubscription::negotiated` serves an offer below
  it) wakes about one floor after its last pass, a little early or late,
  and an early wake was handed that pass again and sent an empty frame:
  cachecannon measured 35% of frames empty at its 10 ms floor. Per layout,
  a pass is now built at most once per three quarters of the floor, on the
  clock of the callers' `now`.

### metriken-storage 0.3.1

- **Fixed:** a table first queried while a writer sealed it could lose the
  sealed rows. `DbSegmentStore::build` read the table's segment list and
  its live WAL as separate statements, so a seal committing between them
  put its segment after the list and hid its rows from the WAL read by the
  watermark it installed. Both reads now run in one read snapshot, through
  the new `Catalog::read_snapshot`, which `DendroCatalog` and `RezDb`
  implement. Reproduced on a dendro archive: a seal of ticks 5 to 9
  between the two reads left the view with ticks 0 to 4 of 0 to 8.
- **Fixed:** a table's view could return rows twice when a sequence number
  it listed named another segment by the time it fetched it. A fetch now
  goes through the new `Catalog::segment_bytes_matching`, which returns the
  bytes only while the number still names the listed row count and span,
  and otherwise reports the segment as gone. Reproduced with a dendro
  writer reopened on the archive (`Writer::open`, `resume_source`) after
  retention evicted every segment of a stream: its next seal is segment 0
  again, and the view read ticks 5 to 8 from it and from its own live tail.
  Compaction, which keeps the first replaced segment's number, cannot run
  while a view holds the file (`ArchiveMut::open` is refused). A
  `Catalog` implemented outside this crate gets defaults: no snapshot, and
  a matching fetch that reads every segment's catalog row of the table.
- **Fixed:** a table whose first seal committed while
  `ArchiveReader::from_catalog` opened the archive could be missing from
  the reader for as long as it was open. The open read the table's segment
  list (still empty) and its live WAL (emptied by the seal) as separate
  statements, so the table had no rows to probe and was skipped. Each
  source's catalog reads at open now run in one read snapshot.
- **Fixed:** a long table could open without the labels of occupants whose
  rows a seal of its occupant stream moved while the stream was read: the
  stream's segment list and live WAL were separate statements. They are
  read in one snapshot, and its segments are fetched by their listed entry.
- **Fixed:** `RezDb::read_snapshot` left its transaction open when the
  closure panicked: later reads on that connection saw the old snapshot,
  later `read_snapshot` calls failed, and a live `.rez` could not
  checkpoint past it. The transaction is now ended on unwind as well.

### metriken-exposition 0.23.1

- `StreamSubscription::negotiated` negotiates the stream interval. A
  request's interval is an offer; a negotiated subscription serves the
  larger of it and the producer's update floor, names the served interval
  in the frame-interval header, and carries it and the floor in the
  handshake's metadata under `metriken.frame_interval` and
  `metriken.update_floor`. The keys are per connection: a subscriber that
  keeps a source's handshake metadata drops them. Without `negotiated` the
  offer is served as given, as in 0.23.0, so the served interval, the
  headers and the handshake bytes of an existing producer do not change.
  `StreamSubscription::offered_interval` is the offer. A producer that
  refuses an offer below its floor instead checks it before building the
  subscription. When metriken-recorder older than 0.3 offers a negotiating
  producer less than its floor, it records the offer as
  `sampling_interval_ms`, keeps the two `metriken.*` keys in the recording's
  metadata, and drops the connection if the floor is longer than its
  interval plus its timeout. rezolus's default floor (its 10 ms ttl) and the
  recorder's 2 s minimum timeout keep the drop from happening at default
  settings.

### metriken-recorder 0.3.0

- A streamed source runs at the interval its producer serves: the
  frame-interval header, or the handshake's `metriken.frame_interval` when a
  proxy dropped the header. Its recording's `sampling_interval_ms` is that
  interval, a stream is dead after that interval plus the timeout, and the
  wait for a frame after a wrapped command exits is at least as long. A
  reconnect offers at least the update floor the producer named on the
  last connection, and the served interval it answers with replaces the
  old one. The handshake's `metriken.*` stream keys are kept out of the
  recording's metadata. `Subscription::served_interval` is the interval
  served, and `Handshake::served_interval` carries it with each
  connection's handshake. `sampling_interval_ms` is written when the
  recording is added, so a producer whose served interval changes at a
  reconnect keeps the first.

### metriken-model 0.1.3

- `wire::HANDSHAKE_FRAME_INTERVAL` and `wire::HANDSHAKE_UPDATE_FLOOR`, the
  handshake metadata keys for the negotiated interval and the update floor.

### metriken-query 0.40.0

- A strided histogram call (`histogram_count(lat, 15)`,
  `histogram_quantiles([..], lat, 15)`, `histogram_heatmap(lat, 15)`)
  shares each row's delta among the windows its interval overlaps, in
  proportion to the overlap, where it counted the whole delta in the window
  holding the row. A window can hold part of an observation (to 1/65536),
  so a strided count, sum or heatmap cell can be fractional; quantiles and
  means are taken from the shares. The shares sum to the rows' deltas. A
  stride that is not a multiple of the row interval gives windows of equal
  count: 15 s windows over rows every 10 s hold one and a half rows each,
  where they held one and two in turn. Across a gap in a series, the delta
  of the row after it is shared over the gap's windows.
- A strided window is emitted once every series still reporting has a row
  after its close, so a series read at another phase or cadence adds
  its share first. A series that misses rows stops holding windows open;
  when it reports again, its share for windows already emitted counts in
  the oldest window still open.
- A closed strided window's point is stamped at the window's close, a
  multiple of the stride for a range query, where it was stamped at the
  window's last row; the last window, still open, is stamped at its last
  row. Two strided calls over series read at different times therefore
  share their timestamps, so a binary operation between them matches. A
  window is emitted when that stamp is in the range, so a window closing in
  the range whose rows all fall before its start now gives a point.
- A strided instant query counts the share of the row that straddles the
  window's opening: `histogram_count(lat, 60)` at 1399.5 over rows every
  second counts 59.5 where it counted 60.
- A strided call whose stride is shorter than its metric's row interval
  and lays more than 100 000 windows over its range is refused
  (`histogram_count(lat, 0.001)` over 300 s of 1 s rows): each row's delta
  is shared over every window its interval overlaps, so with such a stride
  the work grows with the windows. At most 256 windows are held open at
  once.
- An outlier observation in a row's delta is shared, in part, into each
  window the row's interval overlaps, so a high quantile or a heatmap's
  tail can show it in two adjacent windows where it showed in one.

### metriken-query 0.39.1

- Histogram functions sum each row's delta into a dense per-bucket
  accumulator, without building maps per row. Results are unchanged; on a
  9.6 h, 1 s recording (release build, warm) quantiles over one series
  run 11-16x faster and heatmaps 3-14x.

### metriken-query 0.39.0

- A strided histogram call (`histogram_count(lat, 60)`,
  `histogram_quantiles([..], lat, 60)`, `histogram_heatmap(lat, 60)`) in a
  range query lays its windows at multiples of the stride, `(k·60 s,
  (k+1)·60 s]`, where it laid them from the first row read. The range read
  no longer moves them, the first window holding the range's start is read
  whole, and the last window is the open one holding the range's end. Each
  point is stamped at the last row of its window, as before.
  A window is emitted only when it holds a row at or after the range's
  start, so no point falls before the range. The read begins up to one
  stride and two row intervals before the range. A stride that is not a multiple of the row
  interval gives windows whose row counts alternate (2 and 3 rows for a
  5 s stride over 2 s rows), where the windows used to stretch to whole
  rows; a caller decimating a chart should round its stride to the row
  interval.
- A strided call as an instant query counts the one stride ending at the
  instant and reads that window and the two rows before it, where
  `histogram_count`, `_sum`, `_mean` and `_quantiles` read back 300 s and
  their window fell wherever the read began, and `histogram_heatmap`
  counted one row.
- A stride under a nanosecond, or `NaN`, is a parse error; it gave a point
  per row.
- Rows of several series at one timestamp fall in one strided window. A
  window closed on the first of them, so the rest opened the next window,
  and a boundary gave a second point holding one row (#279).

### metriken-query 0.38.0

- An unstrided instant query of a histogram function reads back three
  times the histogram's row interval (`DataSource::histogram_interval`, at
  most 300 s), where it read back 300 s. A series whose latest row is older
  than that has no value at the instant: in a multi-recording archive
  queried at its end, a recording whose last row is more than three times
  the slowest recording's row interval before the end is left out. Nor has
  a series whose row before its latest is more than five row intervals
  before the instant: that row, which the delta needs, is not read. A strided call
  (`histogram_count(lat, 60)`) still reads back 300 s.
- A source that reports no interval for a histogram answers its
  `interval()`, 1 s for a parquet file or a `MemoryStore` without
  `sampling_interval_ms`; on such a source whose rows are further apart
  than 1 s, an instant histogram query has no value. Set the key.
- On a 9.6 h recording at 1 s (debug build, warm), an instant
  `histogram_mean(syscall_latency)` took 1.0 to 1.1 s and takes 44 to
  52 ms.
- An instant query of a histogram function with a `by`/`without` clause
  after its name (`histogram_count by (x) (lat)`) reads back as above. It
  read only the instant itself, so it had a value only at an instant that
  fell on a row.
- An instant query whose label value contains a histogram function call
  (`g{x="histogram_mean(y)"}`) is evaluated as the query it is. It was read
  back as a histogram query: the answer was stamped at the step grid point
  at or before the requested time, on a sub-second recording with a value
  up to a second old, and a series answered up to 300 s after its last row.

### metriken-query 0.37.1

- A selector with no name and one `__name__="x"` matcher selects the metric
  `x`, as in Prometheus, in PromQL expressions and in the histogram
  functions. `irate({__name__="redis/ops", source="a"}[5s])` reads the
  counter `redis/ops`, whose name is not a PromQL identifier. A regex or
  negated `__name__` matcher, or more than one, is still refused.
- `sum(...)` or `sum by|without (..) (...)` around `histogram_irate` or
  `histogram_mean` answers on an archive (`ArchiveReader`
  as a `MetricsSource`) as it does on every other source. Since 0.35.0 the
  archive's routing parsed the query before the `sum(...)` was unwrapped and
  failed with `unknown function with name 'histogram_irate'` (or
  `'histogram_mean'`).
- `columns` (`QueryEngine` and every `MetricsSource`) and
  `referenced_metrics` accept the same `sum(...)` form. They returned a
  parse error for it on every source, so a caller trimming columns by a
  query's references dropped a histogram charted as
  `sum(histogram_irate(..))`.
- `sum(histogram_irate by (x) (..))`, a grouping on the function inside a
  `sum`, is refused on every source with an error naming the cause. Outside
  an archive, an inner `by` clause could be rewritten with the outer `sum`
  dropped, depending on its spelling, and return a series per group.

### metriken-storage 0.3.0, metriken-query 0.37.0, metriken-recorder 0.2.0, metriken-exposition 0.23.0

- **Breaking:** arrow and parquet are 60 (were 58) in metriken-storage and
  metriken-query (and in metriken-recorder's tests), and dendro is 0.5 (was
  0.4) in metriken-storage, metriken-recorder and metriken-exposition.
  Parquet 59 and later no longer depend on `thrift`, which clears
  GHSA-2f9f-gq7v-9h6m (thrift 0.17). Each of the four needs a minor
  release: arrow, parquet and dendro types are public in metriken-storage
  (see its CHANGELOG), dendro types in metriken-recorder (`Frame`,
  `WalRow`) and in metriken-exposition's `stream` feature
  (`dendro::Result`), and metriken-storage types in metriken-query
  (`BufferPool` in `ParquetBuilder::pool` and `open_file_with_pool`). Arrow
  and parquet 60 require Rust 1.88.
- `lru` is 0.18 (was 0.12) in metriken-storage, which clears
  GHSA-rhfx-m35p-ff5j (RUSTSEC-2026-0002, fixed in 0.16.3) and
  RUSTSEC-2026-0253 (fixed in 0.18.2). metriken-query no longer depends on
  `lru`; it did not use it.

### metriken-storage 0.3.0, metriken-query 0.37.0

- **Breaking:** an archive's series carry their recording's labels, and a
  query naming a metric several recordings hold is answered from all of
  them, by one engine over every recording it reaches, instead of being
  refused: `sum(x)` sums every recording's series and `sum by (source)`
  gives one per `source`. A series' own value of a recording key moves to
  `exported_<key>`; recordings with identical label sets are told apart by
  `__recording__`. See the crates' CHANGELOGs.
- **Breaking:** in metriken-query, `rate(x[range])` is the rate over
  `[t - range, t]` (it was the per-step rate, the same as `irate`); a binary
  operator between two series sets refuses many-to-many and many-to-one
  matching; `deriv` is the slope over its `[range]`; and raw-mode joins hold
  a point for the gap before it, up to a bound. See its CHANGELOG.

### metriken-storage 0.2.3

- **Fixed:** `SourceRecorder::maybe_seal` wrote a segment past the seal
  policy's `max_rows` when several passes were committed between two seal
  checks, because dendro's seal took every live row of a stream into one
  segment. It now seals through dendro 0.4.2's `seal_capped` with the
  policy's `max_rows` (so does `finalize`), which splits a backlog into
  segments of at most that many rows, and the encoder implements
  `encode_after`, so a part that starts after the row carrying a group's
  schema, a long table's columns or a cell's metadata is anchored from the
  rows before it. dendro 0.4.2 is required. A split costs one encode per
  `max_rows` rows, so with a small cap under sustained CPU overload a
  recording falls seconds behind wall clock (3.4 to 4.5 s in rezolus's
  2-row hindsight fixture, against 0.1 to 0.4 s uncapped); at production
  caps a split is rare. See the crate's CHANGELOG.
- **Fixed:** rows were lost when a group's schema returned to an earlier
  one within a segment (A, B, A), as a scraped group whose series come and
  go does. The writer stated each schema once per segment and the WAL
  decoder uses the last schema stated; the writer now states a schema
  whenever it changes from the previous row's. Predates the seal cap.
  A group whose schema changes every tick then carries its schema on
  every row: its WAL grows roughly 30 times (50 series: 21.5 KB to
  600.8 KB over 60 ticks), and `max_bytes` does not see it, since the
  byte estimate excludes schema bytes. The WAL decoders now resolve a row
  by its schema hash, so tails a 0.2.2 writer left read in full.
- **Fixed:** `evict_before` deleted the first row of an open segment, the
  one carrying the schema or labels, when the retention window was
  shorter than the seal policy's `max_age`, and every later row of that
  segment was lost. Streams whose open segment started before the cutoff
  are now sealed first, and segments are dropped whole, so `retain` now
  keeps up to about twice its window. Predates the seal cap.

### metriken-recorder 0.1.1

- **Fixed:** `Stop::AfterExit` skipped the extra pass of local and scraped
  sources when the exit landed while a tick's pass was running. That pass is
  stamped at its start, before the exit, so the recording could end before
  the exit. The extra pass is now skipped only when the last pass started at
  or after the exit, the same rule as a streamed source's frame stamped at or
  after the exit.
- **Fixed:** streamed passes that queued while a tick or a commit was
  running were sealed as one segment past the seal policy's `max_rows`.
  A commit takes every queued pass and runs one seal check after it, and
  dendro's seal put every live row of a stream into one segment: with
  2-row segments at 10 Hz, a commit that waited ~350 ms on a seal let three
  or four frames queue, and they were sealed as one 3- or 4-row segment.
  dendro 0.4.2 splits such a backlog into segments of at most `max_rows`
  rows (`seal_capped`), and metriken-storage 0.2.3 seals through it; this
  release requires metriken-storage 0.2.3, which requires dendro 0.4.2.
  The recorder still commits queued passes together.

### metriken-recorder 0.1.0

- **Added:** a new crate that records metriken producers into one `.dendro`
  archive through metriken-storage's `ArchiveWriter`: a producer's
  `/metrics/stream`, Prometheus text scraped as one acquisition group per
  scrape, and local sources the caller takes passes of
  (`RegistrySource`, feature `registry`). `detect` decides how each
  endpoint is recorded (its stream first, then the caller's `Probe` hook,
  then Prometheus); `Recorder` runs the aligned tick, reconnects, refuses an
  endpoint mid-run, writes producer restarts into the metadata, applies
  retention, and reports `Event`s and a `Summary`; it never exits the
  process. Feature `http` (default) brings reqwest over a caller-built
  `Client`; without `registry` the crate does not depend on `metriken`.
  Design: `docs/journal/2026-10-12-metriken-recorder.md`.

### metriken-storage 0.2.2

- **Added:** `long::SegmentMetadata`, `long::SegmentMetadataMerge` and
  `long::merge_segment_metadata`,
  the merged file metadata of concatenated segments, and
  `long::OccupantUnion` (feature `archive`), the same merge as dendro's
  `rewrite::MetadataMerge`, so `compact_stream_with` compacts a long stream
  across occupant-list changes. dendro 0.4.1 is required. See the crate's
  CHANGELOG.

### metriken-query 0.36.1

- **Fixed:** the histogram functions (`histogram_quantile(s)`,
  `histogram_count`, `histogram_sum`, `histogram_mean`,
  `histogram_heatmap`) clipped a tick's or stride's cumulative count at
  `u32::MAX`, so above 4,294,967,295 observations quantiles came back low
  and count, sum, mean and heatmap cells were wrong. See the crate's
  CHANGELOG.

### metriken-model 0.1.2

- **Added:** `wire`, the names `/metrics/stream` is served under and its
  layouts: `WireNames`, `WIRE_NAMES` and `Layout`, from
  `metriken-exposition`, so a consumer that decodes the stream shares the
  producer's names without depending on the registry. Plan:
  `docs/journal/2026-10-12-metriken-recorder.md`, order of work step 1.

### metriken-exposition 0.22.1

- **Changed:** `stream::{WireNames, WIRE_NAMES, Layout}` are
  `metriken_model::wire`'s, re-exported at the same paths with the same
  derives and methods. `metriken-model` 0.1.2 is required.

### metriken-storage 0.2.1

- See the crate's CHANGELOG: a long segment lists each metric column only
  for the occupants it has values for.

### metriken-exposition 0.22.0

- **Added:** `stream` (feature `stream`): the frame producer and frame types
  from `metriken-storage` (`FrameProducer`, `SchemaCache`, `StreamRow`,
  `EncodedGroup`, `EncodedLongGroup`, `EncodedStreamGroup`,
  `encode_groups`, `KeyedLabels`, `SOURCE`), `StreamRow::window`, and the
  `/metrics/stream` route helper with no HTTP framework or runtime:
  `StreamRequest`, `Refusal`, `Layout`, `StreamSubscription`, `Pass`,
  `PassStamp`, `RegistryPasses` and the wire names `WIRE_NAMES`
  (`application/vnd.metriken.replication.v1+dendro`, `x-metriken-*`).
  `stream` builds dendro without its archive, so it compiles no SQLite,
  arrow or parquet. Feature `stream-tokio` adds
  `StreamSubscription::into_stream`. Design:
  `docs/journal/2026-10-11-stream-route.md`.
- **Changed (breaking):** `GroupBuilder` exposes `HistogramGroup`s: one
  member per index the group's metadata lists and whose histogram loads, as
  `Snapshotter` does. `build_groups` now returns those members for any
  registry with a `HistogramGroup`, which changes the group's schema for an
  existing producer. `build_stream` sends a group of histogram groups long.
  `LongMember` has a `histograms` field, which breaks code that constructs
  one.
- **Removed:** `MsgpackToParquet`, `ParquetOptions`, `ParquetCompression`,
  `ParquetHistogramType`, `ParquetSchema` and `ParquetWriter`, and the
  `parquet` and `parquet-conversion` features. They are
  `metriken_storage::convert` (feature `convert`). The crate no longer
  depends on arrow or parquet.
- **Changed:** the default features are `msgpack` and `segment` (they were
  `parquet-conversion` and `segment`; `parquet-conversion` enabled
  `msgpack`).

### metriken-storage 0.2.0, metriken-query 0.36.0

- See each crate's CHANGELOG. metriken-archive and metriken-segment leave
  the workspace; their last releases (0.4.0, 0.2.0) stay on
  metriken-storage 0.1.

### metriken-model 0.1.1

- **Added:** `cost`, a row's cost against a writer's segment byte budget:
  `group_approx_bytes`, `wal_group_row_approx_bytes`,
  `wal_long_row_approx_bytes` and the slot sizes, from `metriken-exposition`
  and `metriken-segment`, which re-export them.

### metriken-exposition 0.21.6

- **Changed:** `group_approx_bytes` is `metriken_model::cost`'s, re-exported
  at the same path. The crate no longer depends on `metriken-segment`; its
  `segment` feature only enables `serde`.

### metriken-segment 0.2.0

- **Changed:** renamed `metriken-storage`. This release re-exports it; its
  history continues in `metriken-storage/CHANGELOG.md`.

### metriken-storage 0.1.0, metriken-query 0.35.0, metriken-archive 0.4.0

- See each crate's CHANGELOG. Plan and gate:
  `docs/journal/2026-10-09-storage-scan.md`.

### metriken-model 0.1.0

- **Added:** the observation model as plain types with no `metriken`
  dependency: the snapshot types (`Snapshot`, `SnapshotV1`/`V2`/`V3`,
  `Counter`, `Gauge`, `Histogram`, `GroupSnapshot`, `GroupValidationError`)
  and their msgpack and JSON (feature `json`) encodings, the group schema
  (`GroupSchema`, `MetricDesc`), the write-ahead-log rows (`WalCell`,
  `WalValue`, `WalGroupRow`, `WalLongRow`, `LongOccupant`) and their
  encodings, the occupant stream's rows (`Occupant`) and stream naming, and
  `convert::wal_group_row`. Re-exports `metriken-types`' `Window` and
  `UID_LABEL`. Builds for wasm32. Plan:
  `docs/journal/2026-10-08-one-recording-stack.md`, path step 1.

### metriken-exposition 0.21.5

- **Changed:** the snapshot types and `GroupSchema`/`MetricDesc` are
  `metriken-model`'s, re-exported at the same paths, and `wal_group_row` is
  `metriken_model::convert::wal_group_row`. Serde, msgpack encoding and
  `GroupSnapshot::validate` no longer depend on this crate's `serde` and
  `msgpack` features. `metriken-model` is an unconditional dependency, so
  serde and rmp-serde are built even with no features; `metriken` 0.11.3 is
  required. The `From<&GroupSchema>` conversions into
  `metriken-segment`'s schema are gone: the two are one type, and
  `metriken-model` provides `From<&GroupSchema> for GroupSchema`, so
  `(&schema).into()` still compiles. Because the two schema types are now
  one, a downstream crate that implemented one trait for both gets
  conflicting implementations (E0119).

### metriken-segment 0.1.7

- **Changed:** `schema::{GroupSchema, MetricDesc, fnv1a_128}`, the WAL row
  types and their `encode_*`/`decode_*` functions, and the occupant stream's
  `Occupant`, `encode_wal_row`, `decode_wal_row`, `stream_of`, `table_of` and
  `SUFFIX` are `metriken-model`'s, re-exported at the same paths.
  `window::Window` is `metriken_types::Window`, the same type as
  `metriken::Window` from `metriken` 0.11.3, which the `metriken` feature now
  requires, so the `From` conversions between them are gone and `.into()`
  between them is the identity conversion. A downstream crate that
  implemented one trait for both `Window` types gets conflicting
  implementations (E0119).

### metriken-types 0.1.0

- **Added:** a crate below `metriken-core` for the types the registry writes
  and readers interpret: `Window`, the acquisition window, and `UID_LABEL`
  (`__uid__`), the slot-identity label. It does not depend on the registry, so
  it builds for wasm32 and carries no `links` key. `Window` derives what
  `metriken-core`'s and `metriken-segment`'s copies derived between them:
  `Clone`, `Copy`, `Debug`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`, and
  serde behind the `serde` feature. Plan:
  `docs/journal/2026-10-08-one-recording-stack.md`, path step 1.

### metriken-core 0.3.3

- **Changed:** `Window` is `metriken_types::Window`, re-exported at the same
  path. It gains `PartialOrd`, `Ord` and `Hash`. The `serde` feature turns on
  `metriken-types/serde`; `metriken-core` no longer depends on serde
  directly.

### metriken 0.11.3

- **Changed:** `group::UID_LABEL` is `metriken_types::UID_LABEL`, re-exported
  at the same path.

### metriken-archive 0.3.2

- **Added:** the long form on the replication stream. `EncodedLongGroup`
  (and `EncodedStreamGroup` over `GroupBuilder::build_stream`'s groups)
  carries a group of counter groups and gauge groups as a `WalLongRow`
  keyed by the producer's occupant keys, whose schema is the group's
  columns. `FrameProducer::interval` sends, before each long row, an
  `Occupant` row on `<group>/occupants` for every occupant the subscription
  was not sent in that group's previous row. `StreamRow::occupants`
  (defaulted to `None`) is what marks a row as long. A group can change
  form between rows: the change sends the row's schema, and for a long row
  every occupant, again.
- **Added:** `StreamDecoder` turns a stream's rows back into
  `StreamedGroup`s, reading each row's form from the row, and
  `SourceRecorder::stage_streamed` writes them. A long row's columns are
  mapped onto the group's long columns once per columns hash. Each
  described key is mapped to an occupant number by the occupant's identity
  (its `__uid__`, or else its labels), as the wide path numbers occupants,
  so a reconnect, a restarted producer whose keys start again from 0, and a
  group that changes form all keep each occupant's series. Recorded over
  the long stream, with a value-derived group and a restarted producer
  among the cases, an archive answers queries as one recorded from wide
  snapshots of the same ticks (`tests/stream_long.rs`).

### metriken-exposition 0.21.4

- **Added:** `GroupBuilder::build_stream`, which emits each group whose
  metrics are all counter groups or gauge groups in the long form
  (`LongGroupSnapshot`): the metrics as columns, one `LongMember` per
  member slot, with the slot's labels and an occupant key. Keys are
  assigned per group from 0 as occupants appear, so they encode in a few
  bytes. A slot gets a new key when its occupant changes (a new `__uid__`,
  or new labels on a slot without one) and, in a group with a
  `Membership::Slots` metric, when it returns after a build in which it
  carried no slot metadata. Any other group is a `GroupSnapshot`, as `build_groups` builds it
  (`StreamGroup`). A long group's columns are rebuilt when its metrics
  change, and a slot's labels when its occupant changes. With 2,500
  occupants over five metrics and 16 leaving and 16 arriving, a change tick
  took 0.57-0.70 ms against 0.36-0.39 ms for an unchanged one; the wide
  build's change tick took 8.7-32 ms (`tests/stream_cost.rs`, release, an
  Apple M-series laptop).

### metriken-query 0.34.0

- **Changed:** a query whose selector names a metric of the kind its
  function reads, with no samples in the range or none matching its labels,
  returns an empty matrix (an empty vector from `query`; `histogram_heatmap`
  returns an empty matrix, not a heatmap). A name the source holds only as
  another kind is `QueryError::Unsupported`, naming the kind it is: `rate()`
  of a gauge, `histogram_mean()` of a counter, a bare histogram.
  `QueryError::MetricNotFound` now means the source holds no metric of that
  name, and its payload is always the bare name. A misspelled name is
  reported from either operand of a binary expression. Callers that matched
  `MetricNotFound` to detect an empty result must check for an empty result
  instead.

### metriken-query 0.33.6

- **Fixed:** the histogram functions read from a range's start with no
  lookback, so a sample at the start could be left out by the same `f64`
  round trip 0.33.5 fixed at the end. They now read from 1 µs before the
  start. The slack constant is `RANGE_SLACK_NS`; a grid point up to 1 µs past
  `end` can be emitted when `end` falls just before a step boundary.

### metriken-query 0.33.5

- **Fixed:** a query range's end, given in `f64` seconds, could convert to a
  few hundred nanoseconds before a sample at that time, which was then left
  out. With grid rates that dropped the last grid point, and on a short
  recording `rate()` returned nothing and reported the metric as not found.
  The end is now rounded and widened by 1 µs (`RANGE_END_SLACK_NS`); the start
  is rounded.

### metriken-query 0.33.4

- **Changed:** a segment the segmented reader opens to read rows is cached
  under an id derived from its bytes (XXH3-128 with a per-process seed,
  `twox-hash`), not a fresh id per open. A reader reopened over the same
  segments on the same `BufferPool` finds the blocks the previous reader
  decoded; identical bytes decode to identical blocks, so sharing them is
  correct. rezolus's live viewer reopens its archive every interval; on a
  60-minute live recording, heavier queries' medians fell 9–27%.

### metriken-archive 0.3.1

- **Added:** `SchemaCache::retain`, `len` and `is_empty`. A producer whose
  group names are unbounded drops the groups it no longer has; the cache
  otherwise keeps one entry per group name it has seen.

### metriken-archive 0.3.0

- **Added:** `stream::SchemaCache`, which converts each group's schema to the
  segment format once per schema hash and hands out an `Arc` on later passes.
  A snapshot carries every group's schema on every pass, and converting all of
  them each pass (then freeing them) was about 95% of the cost of encoding a
  pass for the stream: on rezolus's `v3_build_cost` registry (a 2,500-task
  group), 20.4 ms per unchanged pass before, 0.29 ms after.
- **Changed (breaking):** `EncodedGroup::schema` is `Option<Arc<GroupSchema>>`,
  and `EncodedGroup::encode` and `encode_groups` take a `&mut SchemaCache`.
- **Changed:** `FrameProducer` puts a changed schema into an already-encoded
  row with `encode_wal_group_row_with_schema` instead of cloning the schema
  and decoding and re-encoding the row: 3.3 ms → 0.86 ms per frame on the same
  registry when the task group's membership changed.

### metriken-segment 0.1.6

- **Added:** `wal::encode_wal_group_row_with_schema`, which puts a schema into
  an encoded `WalGroupRow` whose `schema` is `None` without decoding its values
  or cloning the schema. The output is byte for byte what
  `encode_wal_group_row` gives for the anchored row; a payload laid out any
  other way is decoded and re-encoded.

### metriken-exposition 0.21.3

- **Changed:** `GroupBuilder`'s maps hash with foldhash instead of SipHash,
  and a cache hit takes its value-vector sizes from the first pass's decision
  instead of formatting the group's wire name to look the cache up again.
  Measured on rezolus's `v3_build_cost` registry (795 entries, a 4096-slot
  per-task group), with rezolus's route cache: cache-hit ticks 1,499–1,588 µs
  → 1,323–1,479 µs at the median, membership-change ticks 7,193–7,405 µs →
  6,511–6,770 µs.

### metriken 0.11.2

- **Added:** `epoch`, the process's producer epoch (a v4 UUID minted once per
  process) and the clock anchor its timestamps are relative to, moved from
  rezolus's agent. The key name and semantics follow dendro's
  `keys::PRODUCER_EPOCH`.
- **Added:** `group::SlotIdentity`, the occupants of a slot space shared by
  one or more groups, moved from rezolus without its change broadcast.
  `assign` writes an occupant's labels and a minted `__uid__`
  (`group::UID_LABEL`) to every group's slot metadata, and keeps the uid when
  a live slot is assigned the labels it has; `release` clears the slot;
  `retain` releases the slots a producer's own liveness check reports gone.
  `group::SlotMetadata` is the trait it writes through, implemented for
  metriken's group types. `SlotIdentity::grouped` takes the groups as a list
  of lists, for a producer that shares per-group metric lists between
  statics. Phase 5a of
  `docs/journal/2026-09-29-members-that-come-and-go.md`.
- **Added:** `CounterFamily` and `GaugeFamily`, one registered metric whose
  members are created at runtime (`member(labels)`) and removed when their
  handle drops. A family answers as a counter or gauge group: a member is a
  slot, its labels are the slot's metadata, and each member carries a
  `__uid__` minted at creation, so a freed slot's next member is a new
  occupant. A family keeps its own member table rather than a registry
  entry per member: at 1M members, 401 B per member against 651 B, a
  per-tick value read of 2.2 ms, and a member created during reads waits at
  most 1.4 ms where a registry entry waited 121 ms behind a snapshot. Phase
  5c.

### metriken-exposition 0.21.2

- **Added:** `group_builder`, which walks the registry into `SnapshotV3`
  groups, moved from rezolus's agent (`create_v3`). A caller-supplied
  `Router` says which group each metric belongs to (`GroupId`), how its
  members are chosen (`Membership`: value-derived, all slots, a prefix, an
  explicit set, or the slots that carry metadata), where each group's window
  comes from (`Acquisition`, with a `ReadGuard` for a group whose read is the
  acquisition), and which keys to add to member metadata. `GroupBuilder`
  builds the member descriptors, names (`{metric_id}` / `{metric_id}x{idx}`
  by default, `ByName` as an alternative), schema and hash, keeps rezolus's
  two-pass skeleton cache, appends caller groups (`ExtraGroup`), and stamps
  the snapshot with `producer_epoch`, `clock_anchor_wall_ns`, `ts` and
  `wall_offset` from `metriken::epoch`. Families snapshot as one group of
  their live members. `DefaultRouter` routes by `acq_group` metadata for a
  producer with no group registry. Behind `msgpack`, which the schema hash
  needs. Phase 5b of `docs/journal/2026-09-29-members-that-come-and-go.md`.

### metriken-archive 0.1.0

- **Added:** the crate, `ArchiveReader` and the `Catalog` trait with dendro's
  implementation, moved from rezolus's `RezReader`. It is the only metriken
  crate that depends on dendro.

### metriken-segment 0.1.2

- **Added:** `wal`, the write-ahead log's row format (`WalGroupRow`,
  `WalCell`, `WalValue`, their msgpack codec), materializing a WAL tail into
  a segment (`materialize_wal_tail`, over any row type implementing
  `WalRowSource`), and `wal_group_row_approx_bytes`. Moved from rezolus.

### metriken-exposition 0.21.1

- **Added:** `wal_group_row` (a `GroupSnapshot` as a WAL row) and
  `group_approx_bytes`, moved from rezolus, with the test pinning
  `group_approx_bytes` to `wal_group_row_approx_bytes`.

### metriken-query 0.33.0

- **Changed:** depends on metriken-exposition 0.21.0.

### metriken-segment 0.1.1

- **Added:** the wide layout, moved from rezolus: `table` (the table model,
  its parquet encoding and decoding, the sidecar column names and writer
  settings), `builder` (`TableBuilder`, `GroupTableBuilder`, and their
  `Cell` input), `schema` (a group's membership, the wasm-safe mirror of
  `metriken_exposition::GroupSchema`) and `window`. A `metriken` feature
  adds `From` conversions to and from `metriken::Window`.

### metriken-exposition 0.21.0

- **Added:** a `segment` feature (on by default) converting `GroupSchema`
  and `MetricDesc` into `metriken-segment`'s mirror of them, with the tests
  that pin the two to the same msgpack bytes and hash.

### metriken-segment 0.1.0

- **Added:** the crate, the parquet segment format shared by writers and
  `metriken-query`. It has the long layout's format (moved from
  `metriken-query`) and the occupant stream (moved from rezolus).

### metriken-query 0.32.0

- **Added:** `long::OccupantLabels`, which puts occupant-stream labels on a
  long table's series.
- **Changed:** `long` re-exports `metriken_segment::long`.

### metriken-query 0.31.0

- **Added:** long segments. A file marked `metriken.layout = "long"` holds
  one row per (timestamp, occupant) and one column per metric; each metric
  column and occupant reads as one series labelled `__occupant__`.
- **Changed:** `ColumnPosition` has an `occupant` field.

### metriken-query 0.30.0

- **Added:** `CompositionSource::lazy` and `CompositionCatalog`: a
  composition child that answers names, time range and metadata from its
  catalog and loads its source at most once, the first time a query or
  label lookup names one of its metrics. `DataSource::series_count` lets a
  composed reader's `total_series_count` ask each child, so a lazy child
  with a catalog count answers without loading. Composed readers and lazy
  children hand out `counter_streams`, so a composed `rate()` streams.

### metriken-query 0.29.0

- **Changed:** `rate()`/`irate()` read each series as a sample stream and
  hold one interval's worth of it. `DataSource::counter_streams` hands out
  a `CounterStream` per series (`CounterSample`s produced as pulled; the
  default materializes through `counters`); the segmented reader implements
  it natively, reading one column of one segment at a time from positions
  it indexed at open (`CounterColumnRef`, `ColumnPosition`,
  `DataSource::counter_column`, `ColumnChunk`). The grid and pairwise rate
  producers consume a stream, keeping only the samples that bracket the
  current interval; the gauge producers own their samples; and the
  dispatcher hands each series its producer as the series' iterator rather
  than collecting its points first. Results are unchanged. Measured on a
  ten-hour archive's 6,644-task table, `sum(rate())` over every series:
  10.3 GB before, 1.4 GB after.
- **Fixed:** two per-call costs the profile found on the way — the
  `timestamp`/`duration` column positions are resolved once per source
  (`Schema::index_of` formats every field name into its error when the
  name is absent, and a `.rez` segment has no `duration` column), and a
  rate's typical sample spacing is computed once per series rather than
  per point.

### metriken-query 0.28.0

- **Added:** `ColumnRelabel`, identity that varies with time, and
  `SegmentedParquetReader::open_relabeled_with_pool` to open a table with
  one. At open a column contributes every label set it can present as; at
  query time its samples are cut into runs by occupant (`split`), histogram
  rows relabelled by timestamp (`at`), and a filter on a key the relabelling
  supplies is turned into one the columns can answer (`segment_filter`) and
  applied to the relabelled runs afterwards. For a reader whose archive says
  who held each slot and when, without decoding the table to split it.
  `Labels` is public, since the hook takes it.

### metriken-query 0.27.0

- **Added:** `SegmentedParquetReader::open_with_pool` takes a `SegmentStore`,
  a trait that supplies a table's segment bytes on demand
  (`InMemorySegments` is the store over bytes already in hand, and what
  `open_bytes_with_pool` wraps). Open reads each segment's footer once to
  build the identity indexes, a per-segment time span and the column map,
  and keeps neither the bytes nor the parsed footer; a query fetches only
  the segments whose span it touches, through a cache bounded by the pool's
  byte budget. A segment the store no longer has is skipped. Measured on a
  1.3 GB, ten-hour archive whose task table had 159 segments of up to 2,851
  columns: building the dashboard held 4.1 GB resident, of which the
  segment bytes were 1.27 GB and parsed footers most of the rest.
- **Fixed:** a `MultiParquetSource` reached through `dyn DataSource` reported
  no sample timestamps; the inherent method had them and the trait method
  returned its empty default.

### metriken-query 0.26.0

- **Added:** `MemoryStore` takes whole series: `insert_counter_series`,
  `insert_gauge_series` and `insert_histogram_series` add a series with its
  timestamps, values and — for counters and gauges — the per-sample
  acquisition windows that `rate()`/`irate()` turn into uncertainty bounds,
  which the ingest path could not carry. `set_sample_timestamps` declares
  the rows a store assembled from a table stands for; without it the store
  reports the union of its series' timestamps. `UnionChild` composes a
  `MemoryStore` beside parquet readers, `HistogramSnapshot` is public, and
  `Labels` converts from a `BTreeMap<String, String>`. For a reader that
  builds series itself — one that splits a table's columns by occupant
  through an identity index — and hands them to the engine.
- **Fixed:** a `MemoryStore` counter or gauge series that carried windows had
  them dropped on read; they are sliced with the samples now.

### metriken-core 0.3.2

- **Added:** `metadata_version()` on `CounterGroupMetric`, `GaugeGroupMetric`
  and `HistogramGroupMetric`: a value that changes whenever any entry's
  metadata is set, added to or removed, and never otherwise. It lets a
  reader that caches something derived from the metadata (a schema, a hash
  of every entry's labels) decide in O(1) whether the cache is current,
  instead of re-reading every entry every tick. Defaulted to a hash of the
  metadata snapshot — correct for any implementor, but a full read per call,
  so a type that can count its own mutations should override it. The reader
  must take the version before reading the metadata it validates; the doc
  says why.

### metriken 0.11.1

- **Added:** `metadata_version()` on `CounterGroup`, `GaugeGroup`,
  `HistogramGroup` and `ShardedCounterGroup`, backed by a counter in the
  shared metadata store that every mutation path bumps under the write lock.
  Overrides the `metriken-core` default with an O(1) load.

### metriken-query 0.25.0

- **Added:** `is_internal_label(name)`, `is_storage_key(key)` and
  `STORAGE_KEYS` are public. A label whose name begins with `__` is internal,
  following Prometheus: part of a series' identity and matchable in a
  selector, and dropped by `without` and by default binary-op matching
  alongside `__name__`. This crate emits internal labels on every result;
  hiding them from listings and legends is the consumer's contract, which is
  why the predicate is public. The engine's own internal labels are
  `__name__` and `__run__`; a reader above this crate may add its own under
  the same rule. Consumers that check for `__name__` by name should use the
  predicate.
- **Changed (breaking):** `without (...)` and `ignoring (...)` drop every
  label whose name begins with `__`; `__name__` was the only one dropped
  before. Default binary-op matching ignores the same set. The observable
  difference today is on a histogram whose bucket configuration changed
  mid-recording: `histogram_mean without (cpu) (latency{__run__="1"})`
  returned series labelled `{__name__, __run__="1"}` and now returns
  `{__name__}`. The values are the same, because a run is selected before
  any aggregation and runs are never merged; a consumer keying on the full
  result label set sees the change. It is what lets a reader split a series
  by incarnation without breaking `a / sum(a)`.
- **Fixed (breaking):** the `ingest` loader and the parquet loader derive
  labels from one function (`Labels::from_metadata`) and one storage-key
  list. They had drifted: the live path kept `grouping_power` and
  `max_value_power` as labels where the parquet path stripped them, so a
  recording viewed live carried two extra labels per histogram series that
  the same recording read from disk did not. Those two labels are gone from
  the live path. `STORAGE_KEYS` is pinned by a test, and each loader has a
  test that a histogram column carrying every storage key yields no label
  from them.

### metriken-query 0.24.0

- **Fixed (breaking):** the parquet read path no longer rounds timestamps to a
  nominal sampling grid. Every timestamp it decoded used to be replaced with
  `round(ts / sampling_interval_ms)`, which discarded the one thing the file
  states exactly — when each row was read — in favour of a value it merely
  declares. A file whose declaration was wrong, or absent (the reader assumed
  1000 ms), therefore lost data rather than being described oddly: at a 100 ms
  cadence all ten rows of a second landed on one instant and nine of the ten
  values were dropped, so a sub-second query returned held-forward copies of
  each second's survivor. The declared interval is still read, and is still the
  staleness hint, where being approximate costs nothing. `MemoryStore` likewise
  ingests a snapshot at the timestamp it carries.
- **Changed (breaking):** `MetricsSource::snapped_sample_timestamps` and
  `ParquetReader::snapped_sample_timestamps` are removed. They existed to
  expose what the rounding did to a caller deciding where a series has data;
  `sample_timestamps` is now that answer for every source. The internal
  `DataSource::counters`/`gauges` lost their `raw` parameter for the same
  reason — it selected between the two forms, and there is one. `RateMode::Raw`
  is unaffected: it still selects point placement in the streaming layer.

### metriken-core 0.3.1

- **Added:** `with_metadata`/`for_each_metadata` default methods on
  `CounterGroupMetric`, `GaugeGroupMetric`, and `HistogramGroupMetric` —
  object-safe metadata visiting reachable through `&dyn` (unlike the
  generic `with_metadata<R>` inherent methods below, a generic method can't
  live on a trait used as a trait object; these use `&mut dyn FnMut(..)`
  instead so they can). `with_metadata(idx, f)` visits one entry's metadata;
  `for_each_metadata(f)` visits every populated entry's metadata in one
  pass, in unspecified order. Both default to the allocating
  `load_metadata`/`metadata_snapshot` so any existing implementor keeps
  compiling; `metriken`'s `CounterGroup`/`GaugeGroup`/`HistogramGroup`
  override both to route through their internal metadata store without
  cloning. Implementations may hold an internal read lock for the callback's
  duration — callers must not block, await, or re-enter the group inside it.
  Additive: existing implementors keep compiling on the defaults.

### metriken 0.11.0

- **Changed (breaking):** an owned `CounterGroup` entry that has never been
  written now reads back as `None` rather than `Some(0)`, matching what
  `GaugeGroup` has always done with its `i64::MIN` sentinel. The value array is
  allocated whole on first touch, so previously writing *any* index made *every*
  index report an honest-looking zero — a sampler populating part of its group
  (one GPU of two, the CPUs it was allowed) published a phantom zero series for
  the rest, and a consumer could not tell those from real measurements. Owned
  backing is now filled with a `u64::MAX` sentinel, `add` replaces it rather
  than wrapping onto it (a compare-exchange loop, as `GaugeGroup::add` already
  uses), and `value`/`load_with_window` report it as absent. An honest measured
  zero is still `Some(0)`.
  **Externally-backed groups are deliberately unaffected**: that memory belongs
  to the caller and a BPF mmap is kernel zero-filled, so it cannot carry a
  sentinel — and a zero there is a real starting value, with membership derived
  from the map's registered entries rather than from value presence. `load()`
  still returns values raw, sentinel included, to stay index-aligned; use
  `value()` for a per-entry `Option`.
### metriken 0.10.1

- **Added:** `with_metadata` on `CounterGroup`, `GaugeGroup`, `HistogramGroup`,
  `WindowedCounterGroup`, and `WindowedGaugeGroup` — runs a closure against
  `Option<&HashMap<String, String>>` while holding the group's metadata read
  lock for the closure's duration, instead of cloning the whole per-index map
  the way `load_metadata` does. Intended for hot paths (e.g. per-member,
  per-tick identity checks) that only need to inspect metadata, not own a
  copy of it. Callers must not block, await, or re-enter the group's methods
  inside the closure. Additive — `load_metadata` is unchanged and still
  available. `CounterGroup`/`GaugeGroup`/`HistogramGroup` also override the
  new object-safe `with_metadata`/`for_each_metadata` from
  `metriken-core`'s `*GroupMetric` traits (see that entry) so allocation-free
  access works both from concrete callers and through `&dyn`.
  `WindowedCounterGroup`/`WindowedGaugeGroup` gain a matching inherent
  `for_each_metadata` that forwards to the inner group. Additive throughout —
  `load_metadata` and every existing signature are unchanged. Requires
  metriken-core 0.3.1 for the trait defaults it overrides.

### metriken-exposition 0.20.0

- **Changed (breaking):** requires `metriken` 0.11. No API change of its own —
  but `metriken` 0.11 changes `CounterGroup::value()` for unwritten entries, and
  a consumer cannot hold both 0.10 and 0.11 in one tree and still have the group
  types unify. Consumers move in lockstep.

### metriken-exposition 0.19.0

- **BREAKING:** `GroupSnapshot::schema` is now `Option<Arc<GroupSchema>>` (was
  `Option<GroupSchema>`), with serde's `rc` feature enabled so `Arc<T>`
  serializes exactly as a bare `T` — wire bytes are unchanged (proved by the
  `arc_schema_wire_compat` test, which compares the encoding byte-for-byte
  against a mirror struct with a bare `GroupSchema` field). Lets a producer
  that caches schemas by `(name, schema_hash)` hand out another reference to
  the same allocation on a cache hit (an `Arc` clone, i.e. a refcount bump)
  instead of deep-cloning every `MetricDesc` on every tick — measured 54% of
  V3-builder allocations at 2k members before this change.
  `GroupSnapshot::validate()` and `GroupSchema::hash()` are unaffected (the
  hash is computed over the schema's content, not its storage). The wire
  format is untouched, but the field's Rust type changes, so anything that
  constructs or destructures `GroupSnapshot::schema` must adapt — hence a
  breaking bump rather than a patch.

### metriken-exposition 0.18.0

- **Added:** `SnapshotV3` — the acquisition-group snapshot format. Metric
  readings are organized into `GroupSnapshot`s (e.g. `cpu_usage/percpu`) that
  share one acquisition `Window` per group instead of one per metric, with
  membership driven by producer registration rather than value sentinels.
  Group membership is described by a `GroupSchema` (`MetricDesc` entries for
  counters/gauges/histograms) that is content-hashed (`schema_hash`, FNV-1a-128
  as `(hi, lo)`) so receivers can cache parsed schemas across restarts instead
  of re-parsing every tick. `Snapshot::V1`/`Snapshot::V2` still decode exactly
  as before.
- **BREAKING:** `Snapshot` gained a new `V3` variant, which breaks any
  exhaustive `match` on `Snapshot` in downstream consumers. This is
  deliberate — `Snapshot` stays intentionally *not* `#[non_exhaustive]` so
  that a new wire version is a compile-time event for every consumer rather
  than something a wildcard arm could silently swallow (see the `Snapshot`
  rustdoc).
- **Added:** `GroupSnapshot::validate()` — checks the cross-field invariants
  a decoded `GroupSnapshot` cannot express on the wire (per-kind schema/value
  arity, and `schema_hash` agreement with the transmitted `GroupSchema`).
  Receivers that cache parsed schemas by `(name, schema_hash)` must call this
  before inserting into the cache. The `Snapshot::counters()`/`gauges()`/
  `histograms()` accessors now silently skip a group that fails these checks
  instead of asserting on it — a malformed but structurally valid V3 payload
  could previously trigger a `debug_assert_eq!` panic on decoded wire data in
  debug builds.
- **Added:** `Snapshot::from_msgpack()` — decodes with a nesting-depth cap
  and rejects trailing bytes, unlike a bare `rmp_serde::from_slice`, which
  silently ignores trailing bytes and has no depth limit.
- **Fixed:** histograms decoded through `Snapshot::histograms()` (all of
  V1/V2/V3) are now rebuilt through `histogram::Histogram::from_buckets`,
  the validating constructor, and dropped if that fails. Raw
  `Deserialize` on `histogram::Histogram` cannot enforce its invariants
  (e.g. `grouping_power < max_value_power`), so a malformed decoded
  histogram could previously panic downstream in `iter()`/`quantiles()`.
  For V3, the expanded histogram's `grouping_power`/`max_value_power`
  metadata is also overwritten from the canonicalized config, restoring
  the V2 invariant that the metadata copy cannot disagree with the
  embedded config.

### metriken-query 0.23.0

- **Changed (breaking):** `EnvPoint` is `#[non_exhaustive]`, constructed with
  `EnvPoint::new` plus `with_band`/`with_interpolated` (the shape `MatrixSample`
  already uses). Literal construction from another crate no longer compiles.
  Taken in the same release as the field addition below, since that addition is
  breaking only because the struct could be built by literal — paying it once
  makes the next field additive.
- **Changed (breaking):** display-mode decimation carries the interpolated
  flag and the per-point bands. `EnvPoint` gains `interpolated: bool`, and
  `Reducer::reduce` takes `(points, bands, interpolated, budget, band)` where it
  took `(points, intervals, budget, band)`. The added field breaks literal
  construction, which is what the entry above makes a one-time cost.

  The reducer read `MatrixSample::intervals`, the all-or-nothing band field,
  which goes absent for a whole series as soon as one point lacks a band — and a
  hole does exactly that. So a decimated series with one unobserved stretch
  showed NO uncertainty band anywhere, despite having one almost everywhere. It
  now reduces from `bands`, the lossless per-point form added in 0.22.

  A decimated bucket is interpolated if ANY of its samples was — it is only as
  observed as its least observed member, so this ORs rather than votes — and a
  bucket's band now skips the samples that have none rather than counting them
  as zero-width.

### metriken-query 0.22.0

- **Changed (breaking):** requires `metriken-exposition` 0.20 (see that entry).
- **Fixed (performance):** a parquet source parses its schema once instead of
  per lookup. `parse_schema` walks every field in the file and eight call sites
  re-ran it, so any "for each metric name" loop was O(names x columns) with
  nothing memoised — `MetricsSource::total_series_count` is exactly that loop
  and nothing overrides the default, so every consumer paid it. Measured on a
  950-column, 14,210-series recording: `total_series_count()` 542 ms -> 12.9 ms
  (5.6 ms warm), and a dashboard section render 598 ms -> 5.3 ms. The parse
  depends only on `meta`, which never changes after construction, so the cache
  needs no invalidation. Costs one `Vec<ColDesc>` per open source for its
  lifetime.

- **Fixed:** `rate`/`irate` no longer fabricate an acquisition window at a
  timestamp the producer never read. `interp_window` interpolates a window
  between the two bracketing samples, which is the right reading when a grid
  edge merely falls between adjacent reads — but across a HOLE (a counter that
  was null for a stretch: a device that appears at runtime, a partially
  populated group, a failed read) it invented a read that never happened, and
  the band then claimed a precision nobody measured. Such a point now carries
  its value and no band, flagged by the new `Point::interpolated`. The
  hole-spanning value itself is unchanged — the total across the hole is known
  even though its distribution inside it is not.
- **Added:** `MatrixSample::bands` — the per-value uncertainty band, present
  when ANY value has one, with `None` at the values that do not.
  `MatrixSample::intervals` is all-or-nothing by construction and cannot carry
  a partial set, so on a series with a hole it reports `None`; `bands` is the
  lossless view. Additive: `intervals` keeps its type and its documented
  behaviour, and `MatrixSample` is `#[non_exhaustive]` with builder
  construction, so nothing downstream needs to change to keep compiling.
- **Added:** `MatrixSample::interpolated` — which values span a stretch the
  producer did not read, parallel to `values`. This is what a renderer needs to
  distinguish an interpolated point from a measured one (a desaturated
  connector, a dashed segment); an uncertainty band cannot express it, because
  the honest bound on an unobserved interval is not a number. Propagates
  through scalar ops, aggregation, and series-op-series the way bands do: any
  operand being interpolated makes the result interpolated.

### metriken-query 0.18.0

- **Changed:** built against `metriken-exposition` 0.18.0. `Snapshot::V3`
  payloads flow through the ingest path via the exposition accessors
  (expansion), so live-agent ingest of a V3 producer works without engine
  changes. No API changes in metriken-query itself.
- **Added:** table-level acquisition-window columns. A bare
  `:window_begin`/`:window_width` pair (no metric prefix) is read as one
  acquisition window shared by every metric in the table. Resolved as an
  atomic pair — never a begin from one source mixed with a width from
  another: a metric's own `<m>:window_begin`/`<m>:window_width` sidecar
  takes precedence where BOTH are present; the table-level pair is the
  fallback where BOTH are present; otherwise no window (unchanged). Both
  bare names are reserved and never surface as metrics.
  `SegmentedParquetReader` splices table-level windows across segments the
  same way it splices per-metric sidecars.
- **Added:** `UnionMetricsSource` — presents several `ParquetReader`/
  `SegmentedParquetReader` readers with DISJOINT metric-name sets as one
  logical `MetricsSource`, for a caller that has split one logical table
  into several physical ones (e.g. rezolus's `.rez` V3 container, which
  tables a sampler's acquisition groups separately). A per-name accessor
  call dispatches to whichever single child owns that name; catalog methods
  (`counter_names()`/etc.) and `time_range()` are the union across
  children; `interval()` is the FINEST (minimum) across children, since a
  child that skipped ticks (window-advance dedup) has a coarser apparent
  cadence than the sampler's true poll rate. No timestamp splicing or join:
  each child keeps its own samples and acquisition windows exactly as
  before, so a query combining two children's metrics (`a / b`) resolves
  through the same grid-alignment the PromQL engine already does for any
  two independently-sampled series, and a `rate()` band still comes from
  whichever child's own table-level/per-metric window the metric belongs
  to — no fan-out or reconstruction step to lose precision in. Identity
  must be disjoint across children by construction (a caller decision, not
  something derived from untrusted archive bytes); a name seen in more than
  one child deterministically keeps its first owner rather than panicking.
  Built via the new `UnionChild` (`From<&ParquetReader>`/
  `From<&SegmentedParquetReader>`, borrowing — the original reader stays
  usable standalone after contributing to a union). Consumed by rev, not
  published — no version bump.

### metriken-query 0.17.0

- **Added:** `SegmentedParquetReader` — presents an ordered list of parquet
  byte blobs (segments of one logical table) as a single `MetricsSource`.
  Splicing happens at the `DataSource` level, *below* PromQL evaluation, so a
  `rate()` window spanning a segment boundary computes on complete data (unlike
  `MultiParquetSource`, which duplicates same-identity series across files
  rather than splicing one timeline). Open is footer-only — no row-group decode
  — and an identity index built once at open keeps splice cost linear.
  Cross-segment identity conflicts (an agent restart remapping a column id, or
  histogram bucket-power drift) split into distinct `__run__`-labelled series
  with a warning rather than erroring or silently coercing.
- **Behavior change:** `:wall_offset` is now a reserved column name, skipped by
  `parse_schema` alongside `:window_begin` / `:window_width`. A parquet file
  carrying a column literally named `:wall_offset` no longer surfaces it as a
  metric.

### metriken-query 0.14.1

- Fleet fallback acquisition window: a windowless file with a `duration` column
  now gets a coarse per-snapshot window `[timestamp, timestamp + duration]`, so
  `rate()`/`irate()` carry uncertainty bands on older/plain-parquet recordings.
  Per-observation `:window_*` sidecars still take precedence.

### Measurement uncertainty: acquisition windows + rate() error bars (#117)

A cross-crate, breaking release. Metrics can now carry a per-observation
**acquisition window** `[begin_ns, end_ns]`, which the query engine turns into
honest measurement-uncertainty bands on `rate()`/`irate()` and histogram
queries. Pre-1.0, so each breaking crate takes a minor bump.

#### metriken-core 0.3.0
- **Added:** `Window` acquisition-window type (opt-in serde); default
  `Metric::load_window` / `value_with_window` accessors; `MetricEntry::module()`.
- **BREAKING:** `MetricEntry` gained a `module` field, and its constructor takes
  a `module` argument (the `#[metric]` definition's `module_path!()`).

#### metriken 0.10.0
- **Added:** torn-safe windowed wrappers — `WindowCell`, `WindowedLazyCounter` /
  `WindowedLazyGauge`, `WindowedCounterGroup` / `WindowedGaugeGroup`, a per-index
  window API on `CounterGroup`/`GaugeGroup`, and `set_with_window` /
  `load_with_window`.
- **BREAKING:** re-exports metriken-core 0.3.0 (the changed `MetricEntry`);
  `#[metric]` now records the defining module path.

#### metriken-derive 0.6.0
- **BREAKING:** `#[metric]` emits the definition's `module_path!()` into the
  `MetricEntry` (requires metriken 0.10.0 / metriken-core 0.3.0).

#### metriken-exposition 0.17.0
- **Added:** optional per-observation `window` on `Counter`/`Gauge`/`Histogram`
  (serde `default` + `skip_serializing_if`, so it is wire-compatible with older
  snapshots); `new()` + `with_window()` constructors.
- **BREAKING:** `Counter`/`Gauge`/`Histogram` are now `#[non_exhaustive]` — build
  them with `new()` / `with_window()`, not struct literals.

#### metriken-query 0.14.0
- **Added:** reads per-metric `:window_begin` / `:window_width` sidecar columns;
  `rate()`/`irate()` derive interval bounds from acquisition windows (widened to
  contain the nominal), propagated through scalar ops, sum/avg aggregation, and
  series-op-series binary ops; histogram value bands from bucket resolution
  (`histogram_quantile` / `histogram_sum` / `histogram_mean`); `QueryResult`
  carries optional `intervals`; `new()` / `with_interval(s)` constructors.
- **BREAKING:** `Sample` / `MatrixSample` are now `#[non_exhaustive]` (use the
  constructors); `Point` is a struct (was a `(u64, f64)` tuple).

### metriken-query 0.12.0

- Add `MetricsSource::query_range_display` and a `display` module: a
  range query evaluated at native resolution and then decimated to a
  bounded point budget for *display*, without losing spikes. Each point
  is a per-bucket boxplot `EnvPoint { t, min, lo, median, hi, max }` —
  a robust median line, a hard min/max envelope so a 1-in-N spike
  survives the downsample, and a configurable inner band (`lo`/`hi` at
  the `DisplayOptions.band` quantiles, default IQR; the min/max outer
  band is invariant). Returns a `DisplayResult` (a richer, non-PromQL
  shape); only `Matrix` results are decimated, heatmap/scalar/vector
  pass through. The default trait method post-processes `query_range`,
  so every backend gets it with no per-impl code. Analysis consumers
  that recompute on the data should keep using `query_range`.

### metriken-query 0.10.6

- Add `histogram_irate(m)` — per-step rate of a histogram's
  cumulative sample count, returned as an instant vector. Lets
  dashboards derive a fallback event-rate line for histograms
  that have no standalone counter (`scheduler_runqueue_latency`,
  `scheduler_offcpu`, `scheduler_running`, `tcp_packet_latency`).
  Replaces the `sum(irate(histogram_count(m)[5m]))` idiom
  suggested for `histogram_count` in 0.10.5, which never parsed —
  PromQL disallows range vectors on function-call results.
- Add an optional `by (..)` / `without (..)` aggregation modifier
  to `histogram_irate`, `histogram_count`, and `histogram_mean`,
  matching standard PromQL aggregation-operator syntax. With no
  modifier, every matching series collapses into one
  `{__name__: metric_name}` output (today's behaviour); with
  `by`/`without`, one series per distinct projected-label tuple.
  Lets `histogram_mean by (source) (m)` return one mean per
  source in a single query — previously required N filtered
  queries.

### metriken-query 0.10.2

- Restore the matcher-less single-right binary broadcast. Queries
  shaped like `sum(rate(x[..])) / y` (where the aggregate strips
  labels and `y` carries some) were silently empty in 0.10.0/0.10.1
  on single-host parquets — the rezolus viewer's CPU-utilization
  tiles relied on this fallback. Now `matrix_matrix_op` materialises
  the lone unmatched right series into a shared timestamp lookup
  and broadcasts it across every unmatched left series, mirroring
  the eager engine's per-left fallback.

### metriken-query 0.10.1

- Cache the parquet footer once per load and decode columns one at a
  time within each row group. Restores load performance on wide files
  that regressed in 0.9.6's per-column projection rewrite — 5–28×
  faster than 0.10.0 across the rezolus dashboard fixtures
  (vllm.parquet 21.0s → 0.74s; sglang-nixl-16c 130s → 6.0s).

### metriken-query 0.10.0

Breaking — collapses the PromQL evaluator to streaming-only and
narrows the supported surface to the subset rezolus actually uses.

- All eager evaluation removed. `evaluate_expr` now forwards every
  expression to the streaming dispatcher; any AST shape the
  dispatcher doesn't recognise becomes `QueryError::Unsupported`.
- `histogram_heatmap` now streams its input — peak transient heap
  drops ~54% per query versus the eager merge-then-walk path.
- `histogram_quantile`, `histogram_quantiles`, counter `deriv`
  (the 2nd-derivative case), gauge `deriv`, and the binary
  operators (`+`, `-`, `*`, `/`) all flow through the streaming
  pipeline.
- The instant `query()` entry point now routes through
  `query_range` with `start = end = time` and collapses the
  resulting matrix to a vector by taking each series's latest
  point. Inherits the full streaming PromQL surface.
- Removed PromQL features (none used by the only known consumer):
  `scalar(...)`, `vector(...)`, `group_left` / `group_right`
  one-to-many binary matching, the matcher-less single-right
  binary broadcast, and the eager `sum(scalar(x))` passthrough.
- Removed crate features: `http` (along with the `axum` dep and
  the `promql::routes` axum router that lived behind it).
- Removed Tsdb / Collection / Series API surface that had no
  remaining callers: `Tsdb::counters` / `gauges` / `histograms`
  (cloning variants — use `*_ref` instead),
  `CounterCollection::filter` / `rate` / `filtered_rate`,
  `GaugeCollection::filter` / `filtered_sum`,
  `HistogramCollection::filter` / `sum`,
  `CounterSeries::rate` / `windowed_rate` / `windowed_irate`,
  `GaugeSeries::untyped`,
  `HistogramSeries::heatmap` / `percentiles` (the eager
  multi-quantile walker; streaming pipeline replaces it),
  `UntypedCollection`.
- Cumulative cachecannon-bench peak transient heap across 43
  representative queries: 12.82 MiB → 7.53 MiB (−41%).

### metriken-query 0.9.5

- Store histograms in the TSDB as `CumulativeROHistogram`, which only retains
  non-zero buckets in columnar form. This substantially reduces memory usage
  for sparse distributions and lets quantile queries run as a binary search on
  the cumulative counts. Delta and sum between two `CumulativeROHistogram`s
  are computed via a shared `combine()` helper.

### metriken-query 0.9.4

- Support PromQL `on(...)` and `ignoring(...)` label-matching modifiers on
  binary operators, allowing expressions whose operands carry mismatched label
  sets (e.g. `tx_bytes / ignoring(direction) link_bandwidth`) to combine
  correctly.

### 0.5.1
Metriken versions older than 0.5.1 did not have changelogs.
