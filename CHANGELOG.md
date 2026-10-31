# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### metriken-charts 0.1.0

- **Added:** the crate. It holds the rezolus viewer's chart modules
  (`assets/charts/`) and `assets/charts.css`, copied from rezolus at
  3ca68c43. ECharts 6.1.0 is bundled; NOTICE and LICENSE-d3 carry its
  notices. `ASSETS` embeds `assets/` as an `include_dir` 0.7 `Dir`, and
  `ASSETS_PATH` is the directory on the build machine. The modules import
  nothing outside `assets/charts/`; a page supplies its data layer, theme
  version and events through `charts/host.js`. 26 of rezolus's node test
  files came with them (`node --test metriken-charts/tests/*.mjs`).
- **Added:** `metriken_charts::plot`, which builds the plot spec a chart
  renders, from the types in rezolus's `dashboard` crate. `Plot::gauge`,
  `Plot::counter`, `Plot::histogram` and `Plot::histogram_latency` return
  a `PlotBuilder`, whose `build(query)` returns the `Plot`. `query`,
  `total_query` and `scope` read a `Plot`, and `queries_mut` and
  `set_scope` are for a page that rewrites queries after building. `Unit`,
  `HistogramView` (the `Plot::histogram` view) and `ChartStyle` (for
  `with_style`) name what the chart modules accept; each is
  `#[non_exhaustive]`, so a value added on the JS side is not a breaking
  change. The spec's JSON is the same as rezolus's at 3ca68c43 for the
  fields `Plot` holds; a plot's width in the page grid and the A/B
  experiment query are not part of `Plot`.

### metriken 0.11.5

- **Changed:** requires `histogram` 1.6.3 (was 1.4.1). 1.6.2 inlines the
  recording path into callers in other crates: measured with 1.6.1 plus the
  same `#[inline]` attributes on that path, from a downstream crate without
  LTO, a per-thread sharded histogram (the layout of
  `ShardedCounterHistogram`) recorded 1.37x faster on one thread and 1.41x
  on eight.
- **Added:** `ShardedCounterHistogram` and `ShardedCounterHistogramGroup`, the
  histogram counterparts to `ShardedCounterGroup`. Each thread records into
  its own shard, and reads merge the shards. A shard's buckets are allocated
  on the first in-range write to it and kept. They are exposed as
  `Value::Histogram` and `Value::HistogramGroup`, which metriken-exposition
  reads with no change. `ShardedCounterHistogramGroup` implements
  `SlotMetadata`.
- **Changed:** `ShardedCounterGroup` holds 256 shards, up from 64, each
  padded to the target's cache line size instead of 64 bytes: 128 on x86_64,
  aarch64 and powerpc64; 32, 16 or 256 on the architectures whose lines are
  that size; 64 otherwise (the values crossbeam-utils' `CachePadded` uses).
  A group of up to 16 counters takes 32 KiB where the padding is 128 bytes
  (4 KiB for up to 8 counters in 0.11.4), and a group of up to 8 takes 16
  KiB where it is 64 bytes. `value()` and `load()` sum 256 shards. On an
  Apple M4 (128-byte lines) two threads writing adjacent shards packed at 64
  bytes shared a line and ran at 0.71x the per-thread rate of 128-byte
  padding, 0.53x at eight threads (5 unpinned runs on a loaded machine, so
  indicative only); on a Ryzen 7 7840HS (64-byte lines) the two paddings
  were within 0.3% at 1, 2 and 8 threads.
- **Changed:** `AtomicCounterHistogram::increment` and `ShardedCounterGroup`'s
  recording path are inlined into callers in other crates. Measured from a
  downstream crate without LTO on a Ryzen 7 7840HS, one thread, median of 5
  interleaved runs: `AtomicCounterHistogram::increment` 425 to 511 million
  values/s, `ShardedCounterGroup::increment` 611 to 636 million/s.
- **Fixed:** a thread that records without calling `set_thread_shard` takes a
  shard from a pool on its first write and returns it when it exits. Threads
  alive at the same time get distinct shards while no more than 256 of them
  have held one at once; a thread that starts while all 256 are leased shares
  a shard for its whole life. Pooled shards are numbered down from 255, away
  from the low ids callers usually pass to `set_thread_shard`. The shard was
  the address of a thread-local modulo the shard count. Measured on Linux
  with glibc 2.41, that address was the same modulo 4096 in every thread, so
  64 threads all wrote one shard; on macOS the addresses were equal modulo
  16, which left 4 of 64 shards in use.

### metriken-core 0.3.5

Requires histogram 1.6.3 (was 1.4.1).

### metriken-model 0.2.2

Requires histogram 1.6.3 (was 1.4.1).

- **Changed:** a snapshot fails to decode as a whole if any histogram in it
  has a config that `histogram::Config::new` rejects, derived config fields
  that differ from what `Config::new` computes, or a bucket count that does
  not match its config. `histogram`'s `Deserialize` validates since 1.6.0.
  With 1.4.1 such a snapshot decoded; `Snapshot::histograms` dropped a
  histogram whose config `Config::new` rejects or whose bucket count is
  wrong, and rebuilt one whose only fault was a derived field. Builds that
  already resolved `histogram` 1.6 behaved this way. Histograms written by `histogram` 0.8.2 through 0.11.3, whose `max`
  differs from the current encoding, decode (histogram 1.6.3).

### metriken-exposition 0.25.2

Requires histogram 1.6.3 (was 1.4.1).

### metriken-storage 0.6.1

Requires histogram 1.6.3 (was 1.4.1).

- **Fixed:** `UnionSource::try_new` accepts a metric name present in several
  children when their series of it carry different label sets, and a read
  of that name returns every such child's series. Before, any name in two
  children was `UnionError::NonDisjoint`, so a metric family read in two
  acquisition groups of one sampler, each over its own population and told
  apart by a label, could not be queried. `NonDisjoint` now means one label
  set of a name is in more than one child, or a child holding the name lists
  no label sets for it. `UnionSource::new` is unchanged: the first child's
  series of a name present in several.

### metriken-query 0.43.1

Requires histogram 1.6.3 (was 1.4.1).

- **Fixed:** an archive query naming a metric that two group tables of one
  sampler hold under different labels returns both tables' series instead of
  failing with "present in more than one acquisition-group table" (through
  metriken-storage 0.6.1).
- **Changed:** a query whose tables include tables of two samplers holding one
  metric name reads every table's series of it when their labels differ, which
  they do in recordings that carry the `sampler` label on every series (current
  rezolus agents). Before, any such query was refused as ambiguous, including
  `sum by (sampler) (rezolus_bpf_run_count)` and any query reading two BPF
  samplers' tables. A name read from samplers of materially different cadence
  is evaluated at the slower sampler's rows, as a query combining two such
  samplers' metrics already is.
  `sum(gpu_utilization)` on a host running both `gpu_nvidia` and
  `gpu_amd_smi` now sums both vendors' series; `by (vendor)` keeps them apart.
  One series (the same labels) in two tables is still refused. The errors say
  the clash is a series with the same labels; they still list metric names.

### metriken-recorder 0.6.1

Requires histogram 1.6.3 (was 1.4.1).

### metriken-recorder 0.6.0

Requires metriken-storage 0.6.0, metriken-model 0.2.1 and dendro 0.6.1.

- **Changed:** a recording's labels and metadata are written as its
  identity layers (metriken-storage `layers`), each write through
  `layers::append` and dendro's `SourceWriter::set_identity`:
  - the recorder layer: at the add, `instance` (an endpoint's URL, as
    parsed, without a user name or password) as a label, `transport` (`stream`, `prometheus`, `local`),
    `sampling_interval_ms`, and the probe hook's labels and metadata; one
    entry per producer epoch, the opening one with no `from_ts` and each
    restart's from its first row, the newest restart's with the hook's
    reply about its process; a scrape's HELP text as `descriptions`, each
    entry holding the names not described yet; a late hook reply's
    metadata. The hook's labels and metadata now sit
    below the handshake's, where they were above;
  - the producer layer: a stream handshake's labels and metadata, or a
    local source's `SourceIdentity` metadata, at the add and for each
    restart's own handshake, without dendro's keys (`events`, `encoder`,
    `writer_sessions`). A producer identity (`metriken.producer.*`
    handshake keys, or `SourceIdentity::producer`) is written as
    `producer_name`, `version`, `build` and the `host` label; the
    stream's own `metriken.` keys are not stored;
  - the user layer: an endpoint's labels and metadata, a local source's
    labels, and each `Recorder::patch_metadata`. A value set to `""`
    removes the key from the recording, where it was stored as `""`.

  A caller's `events` is stored as given, outside the layers, as before. A
  handshake's or a local source's `instance` label and `transport` and
  `sampling_interval_ms` metadata are left out of its entry, so they do not
  replace the recorder's; a local source's own `sampling_interval_ms`, which
  was kept before, gives way to the recorder's tick. A restart's producer
  entry can change the recording's labels mid-recording (a new handshake's
  `source`, say), as can a late hook reply's `producer_name` or a
  caller's patch, and the identical-labels warning is checked then as at
  the add.
- **Changed:** the recorder writes no `source` of its own: no name
  inferred from the URL, no `local` default, and not the probe hook's
  `source`. A `source` label the caller or the producer gives is kept in
  its layer. The recorder writes no `source` metadata key, which it wrote
  beside the label before; a recording has one only when the caller's or
  the producer's metadata sets it, so metriken-query's
  `MetricsSource::source()` returns `""` otherwise. `Event::Active`'s
  `name` is the `source` label the caller, the producer or the hook gave,
  else the name inferred from an endpoint's URL, else, for a local source,
  `source <id>`, where it was `local`.
- **Changed:** new recordings carry no flat `producer_epoch` or
  `producer_epochs`; their epochs are the recorder layer's entries, which
  metriken-storage's `epoch_spans` reads. `Event::Restarted`'s `version`
  is the new process's, from the probe hook or else its handshake.
- **Changed:** a caller's label or metadata key under `metriken.`, and
  dendro's `encoder` and `writer_sessions`, are refused, at start
  (`StartError::Reserved`) and by `patch_metadata` (`Error::Reserved`), as
  `producer_epoch` and `producer_epochs` are.
- **Added:** a streamed source that reconnects to a different producer (a
  handshake naming another producer identity, else another `source` label)
  gets a new recording. The old producer's restarts that have a row are
  written without the probe hook's reply (the new producer is on the
  port), and its recording is finalized at its last row. A new source is
  added for the endpoint, anchored and timed by the new handshake, with
  the caller's labels, metadata and patches written again, and the probe
  hook is asked about the new producer on the following ticks; the new
  recording carries no label from the hook, and a label it gives arrives
  as `LabelsNotApplied` (a `producer_name` in the reply still adds
  `__producer__` when neither the producer's handshake nor the caller
  sets `producer_name` or `__producer__`; a handshake that names its
  producer only by its `source` label sets neither).
  `Event::ProducerChanged { source, from, to, name }` reports it. A
  handshake that names no producer does not split, and leaves the producer
  on record; a reconnect while no handshake has named a producer does not
  split either. While the recorder stops, a producer change does not
  split: the last rows go into the old recording, as a restart when the
  epoch differs. `Committed`'s `passes_total` and
  `first_ts`, and the `SourceSummary`, count the endpoint across its
  recordings. A recording is not evicted by `retain` after it is finalized. Two
  recordings of one endpoint carry identical labels when no producer name,
  or the same one, is on record (a producer named only by its `source`
  label, which the caller's `source` outranks); readers tell them apart by
  `__recording__`.
- **Changed:** an endpoint URL's user name and password are left out of
  the recorder's own log lines and of `SourceSummary::endpoint`, as #328
  leaves them out of the `instance` label. Messages from detection and
  subscription still name the URL as given.
- **Added:** the probe hook's `enrich` is called for a scraped endpoint
  too, with no epoch: at the start beside the streamed endpoints, and on
  late activation before the endpoint is added (it enters `Activating`, as
  a streamed one does, and is added on the tick after its `enrich`
  returns, one interval later than before, even with `NoProbe`). Its reply is written into the recorder layer; a
  `Retry` or a timeout is asked again on later ticks, and a reply that
  arrives then is a further recorder entry. `Probe::enrich`'s doc covers
  scraped endpoints. A run that ends with a reply still owed, and one whose
  retries ran out, say the recording keeps the labels and metadata it was
  added with.
- **Changed:** the identical-labels warning keeps the set of endpoints
  recording under each label set, so a producer change on one of two
  endpoints sharing labels keeps the warning for the other, and a
  restart, a late hook reply or a caller's patch that changes an
  endpoint's labels moves it, and warns when its new labels equal another
  recording's. Two recordings warned about at the add that both move to
  one new label set are warned about again when the second moves. `finish`
  returns the cause `Event::Failed` reports.
- **Changed:** a recording whose merged layers name a producer (its
  identity, the probe hook's `producer_name`, or a caller's) carries the
  `__producer__` label (metriken-storage's layer merge). A hook reply or
  epoch entry that names it after the recording was added adds the label
  then, so the label set changes mid-recording, as a restart's producer
  entry can change it.
- **Added:** `SourceIdentity::producer` and `with_producer`,
  `RegistrySource::with_producer`, and `ProducerIdentity` re-exported.
- **Removed:** `restart::metadata_patch`, `restart::Restart` and
  `restart::FETCH_ATTEMPTS` (the `restart` module is private).
  `Probe::merge` is no longer called: in each layer the fold takes a key's
  newest value, the higher layer winning, and merges `descriptions` key by
  key.

### metriken-model 0.2.1

- **Added:** `identity::ProducerIdentity` (re-exported at the crate root),
  a producer's name, version, build and hostname, and the metadata keys a
  producer declares it under in its handshake or snapshot:
  `metriken.producer.name`, `metriken.producer.version`,
  `metriken.producer.build` and `metriken.producer.host` (the constants
  `PRODUCER_NAME`, `PRODUCER_VERSION`, `PRODUCER_BUILD`, `PRODUCER_HOST`).
  `to_metadata` writes the name, and each other key that is set and not
  empty; `from_metadata` reads them back, and gives `None` without a name.

### metriken-exposition 0.25.1

Requires metriken-model 0.2.1.

- **Added:** `producer_identity!()`, the calling crate's
  `ProducerIdentity`: `CARGO_PKG_NAME` and `CARGO_PKG_VERSION` where the
  macro expands, and the machine's hostname. A producer adds
  `identity.to_metadata()` to the metadata it passes to
  `FrameProducer::new` or `for_source`, or to a snapshot's metadata.
  Nothing adds it by default. `hostname()` reads the hostname as the OS
  reports it, which may be fully qualified; new dependency `hostname` 0.4
  on unix and windows targets. `ProducerIdentity` is re-exported at the
  crate root.

### metriken-storage 0.6.0

Requires metriken-model 0.2.1. A minor version because metriken-storage
0.5.4 and earlier do not read the names index below, and `HistogramRow` has
a new public field.

- **Changed:** `HistogramRow` has a public field `resumes`: a row whose
  change from the series' previous row is no reading, which only sets the
  counts and time the next change is taken from. A reader of one source's
  rows sets it false; metriken-query's process-group combine marks the row
  before each change it takes from another source's rows, and
  `histogram_irate` gives no point at such a row. A struct literal of
  `HistogramRow` must set it.
- **Added:** `labeled::Precedence`, which value a key gets when a series
  and its recording both carry it: `Recording` (the default and the only
  behaviour before), where the series' own value moves to
  `exported_<key>`, or `Series`, where the series keeps its own value and
  the recording's is not added. `LabeledSource::with_precedence` sets it.
  `LabeledSource::marking` sets `Series` and a label on every series
  (metriken-query uses `__recording__`), which is then the only key
  `recording_label_keys` reports, so operators pair a recording's series
  by it. Under `Series` a filter's matchers on the added keys and the
  mark's are decided on each relabelled series. `add_labels_with`,
  `translate_filter_with` and `keeps_with` are the free functions with a
  precedence.
- **Added:** `overlay`, an open-time label overlay: entries, each a
  selector (a source's uuid, or matchers on its stored labels) and labels to
  set or, with `""`, remove, applied in order where the catalog reports a
  source's labels and never written to the archive.
  `ArchiveReader::open_read_groups_overlaid` and
  `open_read_groups_from_bytes_overlaid` open with one, and
  `LabelOverlay::catalog` wraps a `Catalog` for `from_catalog`; read groups,
  `__recording__`, query routing and listings see the overlaid labels.
  `LabelOverlay::new` refuses an entry that sets or removes `__name__`,
  `__run__`, `__occupant__`, `__uid__`, `__recording__`, an `exported_` key
  or a `metriken.` key.
- **Added:** `ArchiveReader::counter_labels_matching`,
  `gauge_labels_matching`, `histogram_labels_matching` and
  `gauge_histogram_labels_matching`: the label sets of a metric that a
  filter (a selector's matchers, as `Labels::matches` reads them) selects,
  as a query's selector selects them. A recording whose labels no series of
  it can carry under the filter is not read.
- **Added:** `layers`, a source's identity in three layers (recorder,
  producer, user) stored in its dendro metadata under
  `metriken.layer.recorder`, `metriken.layer.producer` and
  `metriken.layer.user`. Each layer is a list of entries (`Entry`: `id`,
  `seq`, `ts`, `by`, `from_ts`, `epoch`, `labels`, `metadata`,
  `unset_labels`, `unset_metadata`) and a cache of their fold.
  `layers::append` writes them: it appends an entry, assigns its `seq`, and
  rewrites the layers' caches, the source's labels (the layers merged
  recorder, producer, user, the last winning) and the metadata keys the
  layers set, removing one they no longer set. It edits the two maps and
  does no I/O, and builds without `write`.
  - A source without layers is first given the layers
    `Layers::derive_legacy` reads from it: its labels as one user entry; its
    metadata, other than `writer_sessions`, `encoder`, `events`, the flat
    epoch keys and `metriken.` keys, in an at-open recorder entry; the
    `producer_name` the caller's `LegacyEvidence` names, in an entry of its
    own, over the at-open entry; and each epoch as a recorder entry, read
    as `epoch_spans` reads the flat keys, in list order. Derived ids are
    built from the source's uuid, the layer, the epoch and its `from_ts`.
  - A label or layered metadata key changed outside `append` since its last
    write is appended as a user entry by `external`, a removal as the key
    set to `""`, by the next `append` that appends an entry. It wins over
    later producer and recorder values of the key until a user entry
    unsets it. A label under `metriken.` written outside `append`, or
    carried by a legacy source, is dropped.
  - A user entry removes a lower layer's key by setting it to `""`, a
    metadata key written outside `append` included; a key whose merged
    value is empty is left out. In the producer and recorder layers an
    empty value removes the key from that layer only. Unsetting a key
    removes it from the entry's layer.
  - A producer entry with the same contents as the one that wins the fold
    appends nothing, as does one with the same contents as the producer
    entry appended last when appending it would change neither the fold
    nor the fold of its epoch's entries. An entry that writes a
    `metriken.` label or metadata key, a flat epoch key, or
    `writer_sessions`, `encoder` or `events` is refused. `descriptions` is
    merged key by key, and a `descriptions` value written outside `append`
    whose keys all hold their merged values is not recorded as a change.
    Fields this build does not know are kept.
- **Changed:** `process::epoch_spans` reads a source's epochs from its
  recorder layer's entries that carry an `epoch`; a damaged user or producer
  layer does not affect it. A source without a recorder layer is read from
  `producer_epochs` and `producer_epoch` as before, as is one whose
  recorder layer does not parse. A legacy source gains a
  recorder layer at its first `append` to any layer, derived from those
  keys.
- **Added:** `layers::PRODUCER_LABEL`, `__producer__`: the merged layers
  give a source this label from its merged `producer_name` (the producer's
  declared name, else the recorder layer's), unless a layer sets the label
  itself; a user entry setting it to `""` removes it. A source whose
  stored labels lack it (written before this) gains it at its next
  `append`. Internal by the `__` rule: listings and legends hide it, and
  default binary matching and `without` drop it.
- **Changed:** `ArchiveReader` finds twin recordings (which get
  `__recording__`) on the labels a query matches on, leaving out internal
  labels other than `__recording__`. Two recordings that differ only in
  `__producer__`, as an endpoint's recordings before and after a producer
  change do, are twins, so `without` and binary operators keep them apart.
  Recordings that differ only in another internal label (a caller's
  `__x`) are twins too, where `without` merged them.
- **Changed:** `MultiParquetSource` (the parquet multi-source path) applies
  the archive's clash rule on every read and listing path, `column_map`
  included: a series that carries a key its file's injected labels add
  keeps its own value under `exported_<key>`, and a matcher on
  `exported_<key>` is decided on the relabelled series. Before, the
  injected value overwrote the series' own, a debug build panicked, and
  `column_map` carried no injected labels.
- **Added:** `labeled::relabel_stream`, a histogram stream relabelled
  under the clash rule, for a source that injects labels.
- **Added:** `SourceRecorder::set_identity(labels, metadata)` (feature
  `write`), dendro 0.6.1's `SourceWriter::set_identity` for a recording
  source. Requires dendro 0.6.1.
- **Changed:** a `descriptions` change made outside `append` is recorded
  in its `external` entry as the names it adds or gives another value,
  where it was the whole value; a later producer or recorder value of
  another name is no longer hidden under it.
- **Added:** `layers::forbidden_metadata`, whether an entry may not carry
  a metadata key, for a writer filtering input it did not choose.
- **Added:** `layers::identity_keys` and the constants `PRODUCER_NAME`,
  `VERSION`, `BUILD` and `HOST`: a producer identity as producer-layer
  keys. `derive_legacy` derives a producer entry from a legacy source's
  `metriken.producer.*` metadata.
- **Changed:** a sealed segment's caller index (dendro's
  `segment_indexes`) is a names index (`MNI1`): the metric names its footer
  holds by kind, and its `sampling_interval_ms`, `metriken.format` and
  `metriken.layout`, msgpack with named fields compressed with zstd. The
  writer stores it at each seal, and the parquet and `.rez` conversions with
  each segment they insert; a segment whose `sampling_interval_ms` is not a
  non-negative integer, or whose footer repeats one of those three keys,
  gets none.
  `ArchiveReader` takes a table's names, and its first segment's interval,
  from these indexes, so opening an archive whose segments all carry one
  fetches no sealed segment. A segment with a names fingerprint (`MNS1`,
  written by 0.5.4 and earlier) is read as before. A later segment with no
  index is fetched when another segment of its table has a names index or
  fingerprint (dendro's compaction drops a merged segment's index), so a
  compacted table costs a
  fetch per merged segment at open. A table with no index on any segment
  (copied with a projection, or compacted so that every segment was
  merged) still lists only its first segment's names and its live tail's.
  An index that does not decode, of an `MNI` version this reader does not
  know, or that decompresses past 4 MiB is fetched. A segment whose bytes
  are damaged now fails the first query that reads it rather than the
  open, when its index lets the open skip it. A reader before this one does not
  read `MNI1` and takes each table's names from its first segment and live
  tail only, so it neither lists nor returns data for a metric that first
  appears in a later segment of an archive written by this one.
- **Changed:** a names index (`MNI1`) holds, after its names frame, a
  second zstd frame with the segment's columns: each column's metric name
  and labels as its footer lists them, and each histogram's config. The
  open decodes only the names frame. For a recording complete at open,
  `ArchiveReader`'s label listings (`counter_labels` and the others) take
  a table's label sets from these frames, relabelled through its occupant
  stream for a long table, without building the table's reader, so without
  fetching its sealed segments; such a table is listed even when its
  reader would fail to build on a damaged segment. A table lists through
  its reader as before when its recording was not complete at open (still
  being written, or stopped before it was finalized), when it is long and
  has a live tail, or when a segment's columns are not known (an index
  without the frame, a segment the open did not fetch, a frame that does
  not decode or decompresses past 64 MiB). A complete recording evicted or
  compacted after the open lists as it was at the open until a query
  builds the table's reader. On a 9.6-hour archive, listing every metric's
  label sets went from about 5.1 s to 0.34 s.

### metriken-query 0.43.0

Requires metriken-storage 0.6.0. A minor version because metriken-query's
public API uses metriken-storage types.

- **Changed:** a query over a process group reports conflicts (S4): where
  two of its sources hold a row of one series at one time, with one
  acquisition window for a counter or gauge, and different values, the
  result's `notices` (range, instant and display queries) hold one
  `Notice::Conflict` per series and pair of sources, with the first time
  and the count. The rows compared are those the query reads, which
  include rows before its range (a range function's window,
  a selector's staleness window, a histogram's previous rows), so a notice
  can name a time no point uses. Gauge histograms are not compared.
  Detection always runs; there is no option to turn it off.
  - The batched `rate`/`irate` scan of a group compares rows as it reads
    them and holds a row until no unread segment starts before it: at most
    a chunk (up to eight segments) of each member. Where an unread segment
    has no timestamp statistics, the scan does not know where the rest
    starts, and a row is held until every other source's member has read
    past its time, ended or starts after it.
  - Other paths (sources with several producer epochs, scopes, `deriv`,
    `idelta`, offsets, instant queries, gauges) keep the counter and gauge
    rows they read, 40 bytes each per read of the metric, from the first
    row of any other source of the group on, until the query ends. A 24 h,
    1 s group of two copies with 768 series read once this way keeps about
    5 GB.
  - On a group of two 40-minute sources overlapping by 20 minutes, 64
    counter and 64 gauge series at 1 s, a 1 h query at a 15 s step,
    `rate(cpu_cycles[1m])` took 5.4 to 5.5 ms against 4.8 to 5.0 ms
    without detection, and `g` 5.9 to 6.0 ms against 4.1 to 4.2 ms
    (medians of 21 runs, two rounds, on a loaded machine).
- **Changed:** where two sources of a process group have equal spacing,
  the one with the earlier first row now answers (Q16), before the lowest
  uuid. At a conflict the value is the earlier-starting source's, and where
  the two sources' rows interleave without conflicting, the rows used can
  differ from before, so a result over them can change.
- **Breaking:** every `QueryResult` and `DisplayResult` variant has a
  field `notices: Vec<Notice>`, what the caller should be told about the
  result, absent from the JSON when empty; `notices()` and `notices_mut()`
  read and extend it, and the display form and an instant query carry the
  result's notices. `Notice::Conflict` (two sources of one process group
  holding a row of one series at one time with different values, S4) is
  the first kind. It is non-exhaustive:
  outside metriken-query build one with `Notice::conflict` and match it
  with `..`. A notice of a kind this version does not know deserializes as
  `Notice::Unknown`. A pattern matching a result variant needs `..`, and a
  construction sets `notices`.
- **Changed:** a histogram over a process group (`histogram_count`,
  `histogram_quantiles` and the others, `by` aggregation and strides
  included) combines the group's sources per span (S9). Each member's
  per-row deltas are taken per series where its own rows cover them and
  the pair is not inside a run of a member ranked above it (finer, then
  the lowest uuid). Members are offered coarsest first and a pair
  overlapping one already taken is not taken, so a coarse pair straddling
  a finer run's edge is taken; a pair set aside for lying inside a finer
  run is taken at the end where nothing taken overlaps it. The deltas
  taken are summed into one cumulative stream. A junction between two
  members loses less than one interval of the coarser one, and with three
  or more up to two intervals of a member. Members are read three coarsest
  intervals past the read's end; where a read starts or ends can still
  move a junction near it with three or more members or with holes within
  a member. Before, a group's histogram read the one source covering the
  most of the range, leaving the others' spans out. Where a finer run sits
  inside a coarse one, the group can count less than the coarse source
  alone. Members of different bucket configurations still read one source.
  The group's histogram interval is its coarsest member's.
- **Added:** `QueryOptions::series_labels` and
  `QueryOptions::with_series_labels`, the template evaluation rule for a
  producer's or the registry's template: an `ArchiveReader` query matches a
  key that a series and its recording both carry against the series' own
  value and leaves the recording's value off the series; a key only the
  recording carries is matched against the recording's value. A matcher on
  a recording label then leaves out no recording, so the scope chooses the
  recordings. The recording's keys are then not what tells recordings
  apart: where the scope (else the archive) holds more than one read
  group, every series carries `__recording__`, numbered over the reader's
  read groups (the catalog index of each group's first source, or its
  position when readers joined by `flatten` share catalog indices), and
  operators pair series by it; with one read group no series carries it,
  a twin's own included, and a source and its copies are one read group.
  Without a scope, a query that selected one recording by a label matcher
  reads every recording: it is refused where a metric is a different kind
  in two of them (the refusal names the scope) and gets no single
  recording's cross-cadence timestamps. Any source other than an
  `ArchiveReader` refuses a query that sets it, as it refuses a scope, and
  an instant query (`query`, which takes no options) cannot set it.
- **Added:** `Scope` and `QueryOptions::with_scope`: the sources a query
  reads, each named by uuid (a source and its copies) or by position among
  the reader's `ArchiveReader::sources`, with an optional span. An
  `ArchiveReader` query reads only the named sources' tables, within each
  process group unions only the named sources, and reads a source's rows
  within its span; a query referencing a metric that only unnamed sources
  hold gives an empty result. A source read over a span follows the
  process-group rules even alone: its points end one interval after its
  last row in the span, and a gap of more than twice its median row
  spacing has no points (S5), where an unscoped read interpolates across
  it. A source named by two entries is refused
  (`QueryError::Unsupported`), and so is a query carrying a scope on any
  source other than an `ArchiveReader`.
- **Added:** `MetricsSource::counter_labels_matching` and its gauge,
  histogram and gauge histogram siblings, and `label_values_matching`: a
  metric's label sets, or one key's values, among the series a filter
  selects. The default filters the full listing; `ArchiveReader` leaves out
  the recordings the filter cannot match without reading them.
  `selector_filter` builds the filter from one vector selector
  (`{source="a"}`, `metric{cpu=~"0|1"}`). metriken-storage's `Labels`, the
  filter type and what the `*_labeled` builders take, is re-exported.
- **Changed:** `ParquetBuilder::file_labeled` and the other labelled
  builders: a series whose own labels carry an injected key keeps its own
  value under `exported_<key>`, where the injected value overwrote it. A
  selector on an injected key now resolves in `columns()`, where it found
  no column.

### metriken-query 0.42.2

Requires metriken-storage 0.5.4. With 0.5.3, `UnionMetricsSource` lists
no sample timestamps and `ArchiveReader`'s list fills every table's
segment cache.

- **Added:** `MetricsSource::sample_timestamps` for `ArchiveReader`
  (feature `archive`), which had the trait's empty default. It lists the
  instants at which the first source with rows, or a copy of it (same
  uuid, same read group), holds a row in any of its tables, ascending,
  each instant once, WAL tail included. Tables of different
  cadence contribute the union of their rows. Other sources are not
  listed, even one in the same read group: a second recorder of one agent
  samples on its own schedule. A reader of several read groups lists the
  group of its first source with rows; open each with
  `ArchiveReader::open_read_groups` for its list. Built from each table's `row_timestamps`, which the reader keeps:
  on a 9.6 h, 49-table recording the first call took 6.6 s and a second
  3.4 ms.
- **Added:** `MetricsSource::sample_timestamps` for `UnionMetricsSource`,
  which had the trait's empty default: the union of the children's,
  ascending, each instant once.

### metriken-storage 0.5.4

- **Added:** `DataSource::sample_timestamps` for `UnionSource`, which had
  the trait's empty default: the union of the children's, ascending, each
  instant once.
- **Changed:** `SegmentedSource`'s `DataSource::sample_timestamps` no
  longer adds the segments it opens to the segment cache; a segment
  already cached is read from it. Each table's cache is charged against
  the whole pool budget, so reading the timestamps of all 49 tables of a
  9.6 h recording peaked at 2.25 GB resident with a 512 MiB pool when the
  scan filled the caches, and peaks at 0.37 GB with pools of 16 and
  512 MiB. The cost: `eval_timestamps_for` reads timestamps through this
  method and no longer leaves the tables it read cached, so the query
  that follows fetches their segments.

### metriken-storage 0.5.3

- **Added:** `ArchiveReader::open_read_groups(path, pool)` and
  `open_read_groups_from_bytes(bytes, pool)` (feature `archive`): a dendro
  archive opened as one `ArchiveReader` per read group, the sources a query
  reads as one recording (a source and its copies, and sources that share a
  producer epoch and overlap in it by a row of each; a group splits where
  its sources carry different values of a label), where `from_catalog`
  gives one per source. Each entry is the group's labels and a reader whose
  `sources()` are the group's sources (`catalog_index`, `uuid`) for a tool
  that edits them, whose `metadata()` is the group's, and whose series
  carry the group's labels. Entries are in order of each group's first
  source within one open. Only a dendro archive opens: a standalone parquet
  file, a v3 `.rez` (SQLite without dendro's stamp) and a v1/v2 tar `.rez`
  are refused by their first bytes, with a message naming `rezolus
  recording upgrade` and `to_dendro::convert_path`. Builds for
  `wasm32-unknown-unknown`.
- **Added:** `ArchiveReader::flatten_read_groups(LabeledRecordings)`
  (feature `archive`), the grouping the opens apply: readers of one source
  joined into one per read group, found as `ArchiveReader::read_groups`
  finds them over the same sources joined. A reader of several sources, or
  of none, is kept as it is.
- **Added:** `process::merge_metadata(&[&BTreeMap<String, String>])` and
  `process::EVENTS` (feature `archive`): a read group's metadata from its
  sources', the rule rezolus applied in its own reader, except that a lone
  parseable payload wins over an earlier one that does not parse. `events`
  is the union of the sources' events, each `id` once (the first source's
  copy), an event without an id once per distinct event, sorted by
  `timestamp` then `id`; a payload that is not a JSON object is skipped,
  and a single payload is kept as written. Every other key is the first
  source's that holds it.
- **Changed:** `file_id` (which file is at a path) is a crate-root module
  under feature `archive`; `rez::file_id` re-exports it.

### metriken-storage 0.5.2

- **Added:** `rez::to_dendro::convert_v3_into(db: &RezDb, out: &mut
  ArchiveMut) -> Result<Converted, to_dendro::Error>` (features `rez` and
  `write`), beside `convert_tar_into`: a v3 `.rez` into an archive the
  caller made. With `RezDb::open_bytes` and `ArchiveMut::create_in_memory`
  it converts with no filesystem and no threads, and runs on
  `wasm32-unknown-unknown`. On an error, what was written before it stays
  in `out`. `convert_v3_to_dendro` and `to_dendro::convert_path`'s v3 arm
  call it; what they write is unchanged. `convert_path` refuses an existing
  output before it reads a v3 input, as it does the other inputs.

### metriken-query 0.42.1

- **Fixed:** `MetricsSource`'s default `all_names`, `total_series_count`,
  `all_label_keys` and `label_values_by_key` include gauge histograms,
  which were queryable and listed by `gauge_histogram_names` but missing
  from these. Every reader here uses the defaults except
  `ParquetReader::total_series_count`, which already counted them.

### metriken-recorder 0.5.1

- **Fixed:** a streamed producer whose probe hook reply (`Probe::enrich`)
  did not arrive within the timeout before its recording was added is asked
  again on later ticks, one ask at a time, up to 5 more times, retry `k`
  bounded by the timeout times `2^k` (at most 60 s); no tick waits on one.
  The metadata of a reply that arrives is written into the recording,
  never over the caller's keys, `source`, `sampling_interval_ms` or
  `producer_epoch`. Its labels are not applied, since a recording's labels
  are fixed when it is added; a warning names them and says to give them
  as the endpoint's labels, and `Event::LabelsNotApplied { source, labels }`
  carries them. A run that ends with a reply still owed warns that the
  recording keeps its handshake's labels and metadata, and a retry about a
  producer that has since restarted is dropped. Before, one ask was
  made, and a reply slowed by load (two recorders reading one agent) left
  the recording without the hook's metadata.

### metriken-storage 0.5.1

- **Added:** converters from the legacy recording formats to `.dendro`,
  so an edit of an old recording can start from an archive.
  `to_dendro::ParquetToArchive` (feature `write`; `label`, `metadata`,
  `writer_config`, `convert_file_path`, and `convert_into` over a `File`
  or `Bytes` into a caller's `ArchiveMut`, which may be
  `ArchiveMut::create_in_memory`) converts a standalone parquet
  recording, combined files included, into one source.
  `rez::to_dendro::convert_tar_to_dendro` and `convert_tar_into`
  (features `rez` and `write`) convert a v1 or v2 tar `.rez`, a source
  per recording with its manifest labels, metadata, clock anchor (else its
  first row), clock offsets and `complete` flag; a tar cut short converts
  what its reader reads. Both return `to_dendro::Written` (sources,
  tables, segments, rows, clock offsets), and every function here errs
  with `to_dendro::Error` (`Box<dyn Error + Send + Sync>`).
  `to_dendro::convert_path(input, output)` picks the converter by content
  (parquet, v3 `.rez`, tar `.rez`; a dendro archive is refused as one
  already) and returns `to_dendro::ConvertedFrom`, which carries what the
  converter wrote. Its errors name the input. Every path form writes to a
  staging path beside the output, refuses to publish an archive whose
  `-wal` still holds frames, and hard-links it into place (or, where
  hard links are not supported, copies it into a file created with
  `create_new`), so a file at the output is never removed or replaced;
  the staging files are removed however the conversion ends, a panic
  included, and errors name the output, not the staging path.
- Tables are re-encoded column for column (labels, per-metric and
  table-level windows, `timestamp`, `duration`, `:wall_offset`) and split
  at the `WriterConfig` seal caps: `max_rows`, and `max_bytes` metered as
  the input's average encoded bytes per row, so a run of rows denser than
  the average overshoots it in proportion. The recorder's writer meters
  resident bytes instead, which bounds a live segment. A copy has nothing
  resident to bound, and each segment carries its own footer (about 3.8
  MB at 5000 columns, about 3 MB of it the arrow schema), so cutting where
  the writer would made wide recordings 10 to 48 times larger. The
  recordings rezolus publishes with its viewer convert to one segment
  each (two for the 17.6 MB one).
- Rows are decoded in batches sized from the schema to about 1 MiB: 8
  bytes a column, and a histogram column all its buckets,
  `((max_value_power - grouping_power + 1) << grouping_power) * 8`
  bytes, since they decode whether or not they hold counts; a list column
  without that configuration is read a row at a time. Rows are encoded into the open
  segment as they are read, and each segment is inserted when it is
  sealed. Peak RSS: 54 MB for 900 rows of two 262,144-bucket histograms
  (a 73 KB file, which 1024-row batches took to 10.4 GB), and 1.46 to
  1.62 GB (two runs) for the 17.6 MB recording of 5028 columns (4.79 GB
  before).
- Counting histograms are written as `counter_histogram`. A parquet
  file's `duration` is also written as the table-level window pair when
  it has none, which keeps the bands the parquet reader derives from it.
  Every parquet segment declares `sampling_interval_ms`: the file's, or
  1000 when it has none, which is what `ParquetReader` takes then. A tar
  table's segments declare the interval the tar's reader reads it at (the
  first segment's declared one, else the median gap of its first rows); a
  tar table of fewer than two rows declares none, and both readers take
  1 s for it. A parquet file's key-value metadata (without `ARROW:schema`
  and `metriken.format`) and a tar recording's metadata are the source's
  metadata, and `version` is also written as `producer_version`.
- The output has no WAL rows. An output that exists, or beside which a
  `-wal`, `-shm` or `-journal` file exists, is refused; on an error the
  archive and its sidecars are removed, and the `ArchiveMut` forms leave
  what they wrote in the caller's archive. A tar table with no rows is not
  written, so its names are not listed; a long segment in a tar is
  refused. Nothing spawns a thread, and the crate still checks for wasm32
  with `rez` and `write`.
- **Changed:** `ArchiveReader` over a catalog takes the interval a table's
  first segment declares as `sampling_interval_ms` over the one it
  measures from the rows, as it already did for segments held in memory.
  Segments the writer seals declare none, so archives written by
  metriken read as before. On a recording with dropped or jittered rows
  the measured interval is off the nominal one (0.994 s, 1.003 s, 1.004 s
  and 1.013 s against 1 s on six of the recordings rezolus publishes with
  its viewer), which moved gauge staleness and instant `irate` for a
  converted parquet file. Answers that differed from `ParquetReader` on
  those six, before and after: 6 and 0 of 238; 648 and 340 of 9132 (two
  recordings); 656 and 340 of 9132; 495 and 229 of 18181; 820 and 260 of
  19479. What remains is the readers' differences below.
- Differences between `ParquetReader` and `ArchiveReader` over the same
  file, which a converted parquet file shows too: columns sharing one
  label set (a producer that dropped a distinguishing label) read as one
  series, not one each, and once the table is split into segments, which
  column that series reads can change at a segment boundary; a metric name
  holding `/` fails with a different error; `rate` at a range's first
  points differs by up to 2.4e-8 relative; and other values by up to
  4.0e-15 relative (summation order).

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
  combined point by point as the scan reads (a point is held until every
  member has passed it and the rows nine past it decide its coverage; a
  member whose recording ends inside the range is finished when its scan
  ends, and in a display aggregation a member series silent for ten times
  the larger of its spacing and the step is ended as one recording's is
  when no unread segment holds a row of it (otherwise it holds its unit
  until it returns; see below), so neither holds the group's points to the
  end), and aggregated or reduced
  for display as the scan reads, as one recording is.
  A member clipped to a producer epoch, raw placement, explicit evaluation
  timestamps, gauges and the per-series rate path read each member alone.
  Measured on the 9.6 h recording with a copy of itself (one uuid), medians
  of three interleaved rounds. A display query of `sum(irate(cpu_usage[5s]))`
  over 9.6 h at a 1 s step, read in one batched scan end to end, takes 32 ms
  against 59 ms with each member read alone and combined after (#306), and
  peaks at 157 MB RSS against 473 MB (one source: 13 ms, 80 MB). On this
  recording a cgroup series that falls silent for ten spacings returns, so
  grouped display queries of `cgroup_cpu_usage` ended the batched read after
  about 9 ms and were evaluated without the flush (before the change below);
  the figures below include that. Against the same archive read twice by
  0.41.0: a display query of
  `sum(irate(cgroup_cpu_usage[5s]))` over 9.6 h at a 1 s step 86 ms against
  56 ms (peak RSS 295 MB against 191 MB; one source 30 ms and 125 MB),
  `rate(cgroup_cpu_usage[1m])` over 1 h at 1 s 13.8 ms against 11.9 ms,
  `sum(irate(cgroup_cpu_usage[5s]))` over 1 h 10.0 ms against 5.8 ms, and
  `sum(rate(cgroup_cpu_usage[1m]))` over 9.6 h at 60 s 39 ms against 33 ms.
  With the copy holding only the first hour, or hours 3 to 4 (a dump's
  shape), the cgroup display query takes 49 to 50 ms and 231 to 239 MB. A
  group of 8000 series in two members: `sum(rate(w[10s]))` 318 to 349 ms
  against 415 to 458 ms read per member. Single-source archives take the
  path they took before (30.9 against 33.2 ms for the 9.6 h cgroup display
  query, 6.05 against 6.28 ms for the 1 h `rate`, spreads overlapping).
  `metriken-storage/tests/union_memory.rs` pins the release, through the
  hidden `fixtures::take_union_held_peak` (`fixtures` feature): with a
  member that ends at a quarter of the range, or whose series fall silent
  there, the stages hold about 340 points per group series at most, against
  about 3000 when the member is waited on to the end.
- Cost: a grouped display read (`sum`, `avg`, `min`, `max` or `count` of
  `rate`/`irate`, reduced for display as the scan reads) no longer gives up
  when a series it ended as silent comes back, which sent the query to the
  path without the flush. A series silent for ten times the larger of its
  spacing and the step is ended only when no unread segment of the scan
  holds a row of it (`scan::ChunkReader::later_rows`, answered from the
  scan's plan and the segment footers' null counts, since a wide table
  keeps a stopped series' column with every cell null). One that can still
  come back stays open and holds back only its own group's flush (over a
  process group, its unit's), because the per-series path interpolates its
  points across the silence to the sample it comes back with, so its
  group's aggregates there are not final until that sample is read. Answers
  are unchanged: on 200 random churn fixtures (243,200 answers, 2046 display
  reads that the flush gave up on before) all but 76 are byte-identical, and
  those 76 differ by at most 3.2e-16 relative (summation order). On the
  9.6 h recording, alone and with a copy, five display queries of
  `cgroup_cpu_usage` (the three below, `sum by (name) (rate(...[1m]))` and
  `count by (name) (irate(...[5s]))`) at full resolution, and the three
  below at a 500-point budget, are identical apart from 98 values that
  differ by at most 3.2e-16 relative, and the batched read gives up on
  none of them. On that recording 10 of 106 `cgroup_cpu_usage` series fall
  silent and come back, 52 times, for 100 s to 8415 s. Display queries over
  9.6 h at a 1 s step, medians of three interleaved rounds against the same
  build without this change (two runs of that build differed by up to 7%):
  `sum by (name) (irate(cgroup_cpu_usage[5s]))` 93 ms against 59 ms, peak
  RSS 379 MB against 86 MB; `sum by (state) (...)` 44 against 36 ms, 151
  against 73 MB; `sum(irate(cgroup_cpu_usage[5s]))` 41 against 35 ms, 110
  against 69 MB. With a copy of the recording (one process group): 165
  against 117 ms and 591 against 231 MB, 110 against 93 ms and 280 against
  212 MB, 106 against 91 ms and 247 against 227 MB. A scan that does not
  say (`LaterRows::Unknown`) keeps the old rule.
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
- **Added:** gauge histograms (`docs/journal/2026-10-21-gauge-histogram.md`).
  `histogram_quantile`, `histogram_quantiles`/`histogram_percentiles`,
  `histogram_count`, `histogram_sum`, `histogram_mean` (with `by`,
  `without` and `sum by (..)`) and `histogram_heatmap` read a gauge
  histogram as a gauge is read: a point per step at `start + k·step`, from
  each series' latest row within the step or its row interval, summed
  bucket by bucket across series (rebucketed to the coarsest
  configuration), and never a difference between rows. An empty state
  counts 0 and has no quantile or mean. `histogram_irate`, the strided
  forms, and `rate`, `irate` or a bare selector given a gauge histogram
  return `QueryError::Unsupported` naming the metric as a gauge histogram
  and the functions that read it; a name held as both kinds is refused.
  `MetricsSource` gains `gauge_histogram_names` and `gauge_histogram_labels`
  (defaulted to empty; `label_values` includes them), implemented by every
  reader here, and `MemoryStore` gains `insert_gauge_histogram_series`,
  with `ingest_snapshot` storing a snapshot's `gauge_histograms` as gauge
  histograms. Within a process group (and across recordings), each
  source's rows are held for that source's own interval and a series
  several sources hold is one series, answered by the latest of its rows
  still held, so a source and its copy count once.
- **Changed:** takes metriken-model 0.2 (`gauge_histograms` fields on the
  snapshot types `MemoryStore::ingest_snapshot` takes). Parquet files and
  archives written by earlier releases, whose counting histograms say
  `metric_type = histogram`, read as before.

### metriken-recorder 0.5.0

- **Changed:** takes metriken-storage 0.5, whose `WriterConfig` it
  re-exports.
- **Changed:** takes metriken-model 0.2 and metriken-exposition 0.25, whose
  snapshot and WAL row types gained `gauge_histograms` fields.

### metriken-core 0.3.4

- **Added:** `Value::GaugeHistogram`, a histogram whose buckets are current
  state rather than counts since start. `Value` is `#[non_exhaustive]`, so a
  consumer built before the variant reaches its wildcard arm and skips the
  metric instead of exposing it as a counting histogram
  (`docs/journal/2026-10-21-gauge-histogram.md`).

### metriken 0.11.4

- **Added:** `GaugeHistogram`: `set` and `set_with_window` replace every
  bucket, there is no `increment`, and it reports
  `Value::GaugeHistogram`. For distributions of state (free extents per
  size, queue lengths across queues), which readers must not difference.
- **Deprecated:** the counting histograms are renamed for their kind,
  beside `GaugeHistogram`: `AtomicHistogram` is `AtomicCounterHistogram`,
  `RwLockHistogram` is `RwLockCounterHistogram`, `HistogramGroup` is
  `CounterHistogramGroup`. The old names stay as `#[deprecated]` type
  aliases, so existing code builds with a warning. `metriken_core::Value`
  keeps `Histogram` and `HistogramGroup` for the counting kind.

### metriken-model 0.2.0

- **Breaking:** a gauge histogram's kind is on the wire. `GroupSchema`,
  `GroupSnapshot`, `WalGroupRow`, `LongOccupant`, `SnapshotV1` and
  `SnapshotV2` gain a last field, `gauge_histograms`, and `WalValue` a last
  variant, `GaugeHistogram`. A struct literal of these types needs the new
  field. Each field is left out of the encoding when empty, so a row,
  schema or snapshot without a gauge histogram encodes (and a schema
  hashes) exactly as in 0.1.x and 0.1.x decoders read it; one with a gauge
  histogram is one element longer, which 0.1.x decoders refuse
  (rmp-serde's `LengthMismatch`, or no variant of the untagged `Snapshot`
  matching) instead of reading the state as counts.
  `Snapshot::histograms` returns the counting histograms only;
  `Snapshot::gauge_histograms` returns the others.
  This break is deliberate: recording is mostly local, so a producer and
  its recorder are upgraded together. An older recorder (metriken-recorder
  0.4.0, and `rezolus record` and hindsight, which record through it)
  connected to a producer that emits a gauge histogram ends the whole
  recording on the first frame holding one: `StreamDecoder::decode` refuses
  the row, and the recorder fails, aborting every endpoint's task and
  dropping the archive. Reconnecting does not help. Upgrade producers and
  recorders together.
- **Added:** `METRIC_TYPE` (`"metric_type"`), `COUNTER_HISTOGRAM`
  (`"counter_histogram"`), `GAUGE_HISTOGRAM` (`"gauge_histogram"`),
  `LEGACY_HISTOGRAM` (`"histogram"`, the counting kind's string before
  this release), and `is_counter_histogram` (either spelling) and
  `is_gauge_histogram`.

### metriken-exposition 0.25.0

- **Breaking:** takes metriken-model 0.2, whose snapshot types it
  re-exports (new `gauge_histograms` fields).
- **Added:** gauge histograms. `Snapshotter` writes a `GaugeHistogram` to
  the snapshot's `gauge_histograms` list and `GroupBuilder` to the group's
  `gauge_histograms` slots (the schema's `gauge_histograms` descriptors), so
  a consumer built on metriken-model 0.1 refuses the snapshot, frame or row
  rather than reading the state as counts; producers without gauge
  histograms send what they sent before.
  `prometheus_text` writes one as the series Prometheus stores for an
  OpenMetrics `gaugehistogram`, each a `gauge` family: `<name>_bucket{le}`,
  `<name>_gcount` and `<name>_gsum` (with `percentiles`, a `gauge` family
  `<name>{quantile}` in place of the buckets). The 0.0.4 text format has no
  gauge histogram type, and a scrape fails on an unknown `# TYPE`.

### metriken-storage 0.5.0

- **Added:** `scan::LaterRows` (non-exhaustive) and
  `scan::ChunkReader::later_rows` (by default `LaterRows::Unknown`):
  whether a scan's unread segments can hold a row of a series. The
  segmented scan answers from its plan and the footers' null counts, and
  `scan::interleave` from the scan holding the series.
- **Changed:** `ColumnPosition` has a field `holds_values`: false when the
  segment footer's statistics say every cell of the column is null.
- **Added:** `scan::interleave`, several counter scans read as one in time
  order, their series concatenated (what `RecordingsSource` joins its
  recordings' scans with).
- **Changed:** `scan::ScanChunk` has a field `ended`: the series of the
  scans `interleave` joined that have ended since the last chunk (empty for
  a single scan).

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
- **Added:** gauge histograms. A histogram column whose field metadata has
  `metric_type = gauge_histogram` is listed by
  `DataSource::gauge_histogram_names` and read by `gauge_histogram_stream`
  (with `gauge_histogram_labels` and `gauge_histogram_interval`), and never
  by the histogram methods, so a caller that differences histogram rows does
  not see one. The new methods default to empty and `None`; every source in
  the crate forwards them, and `CompositionCatalog::gauge_histograms` names
  a lazy child's. `ArchiveReader` gains `gauge_histogram_names`,
  `gauge_histogram_labels` and `SamplerReader::holds_gauge_histogram`;
  `Memory` gains `push_gauge_histogram_series` and, with `ingest`,
  `upsert_gauge_histogram_sample`. `DataSource::gauge_histogram_parts`
  gives a read as parts of one row interval each, defaulting to one part;
  `MultiParquetSource` gives each file's parts under its injected labels.
  `StreamDecoder::decode` keeps a group row's gauge histogram slots. A long
  table has no gauge histogram columns: a long row carrying gauge
  histograms, or a group written long that gains one, is written without
  their values, with a warning once per group.
- **Breaking:** `.rez` writing is removed; reading every `.rez` (v1/v2
  tar, v3 SQLite, live or finalized) is unchanged, as is `rez::to_dendro`.
  Recordings are written as dendro archives, which hold gauge histograms
  (a `.rez` cannot). Removed: the
  modules `rez::rez_v3_writer` (`RezArchive`, `RecordingWriter`,
  `StreamRecorderV3`, `CHECKPOINT_INTERVAL`), `rez::rez_stream` (the tar
  stream writer), `rez::rez_v3_rewrite` (`copy_recordings_into` and the
  `combine`/`filter`/`upgrade`/report rewrites), `rez::parquet_ingest` and
  `rez::seal_policy` (use `dendro::seal::SealPolicy::default()` or
  `metriken_storage::WriterConfig::default().seal`, whose defaults are the
  same: 8 MiB, 900 rows, 300 s); from `rez::rez`, `RezRecorder`, `write_archive`,
  `write_archive_bytes`, `append_tar_entry`, `RecordingData`,
  `RecordingSegments`, `RezArchive`, `read_archive`, `Entry`,
  `PushEntries`, `entries_approx_bytes`, `group_by_sampler`, `dedup_key`,
  `build_labels`, the `recorder_tests_support`
  fixtures, and the re-exports of the segment writer (`TableBuilder`,
  `GroupTableBuilder`, `Cell`, `CellValue`, `cells_approx_bytes`, the cost
  constants, `wal_group_row_approx_bytes`, `group_approx_bytes`,
  `write_table_parquet`, `segment_writer_props`, `table_to_batch`; use
  `builder`, `table` and `wal`); `rez::wal::wal_value`; from
  `rez::rez_sqlite::RezDb`, `create`, `create_in_memory`, `serialize`,
  `remove_archive`, `checkpoint_passive`, `transaction` (and `RezTx`),
  `insert_recording`, `insert_segment`, `insert_wal_rows`,
  `insert_wal_rows_batch` (and `TickBatch`), `prune_wal`, `evict_before`
  (and `Evicted`), `evict_caller_rows_before`, `incremental_vacuum`,
  `vacuum_into`, `mark_complete`, `update_recording_metadata`,
  `update_recording_labels` and `commits`; `rez_sqlite::PAGE_SIZE` (now
  private, test-only); `table::cadence_hint`; and the
  `test-support` feature. `RezDb::open` and `open_bytes` set only the
  reader's page cache (the writer's `synchronous` and
  `wal_autocheckpoint` are gone). `rez::wire` (the row wire,
  `AgentRow`/`AgentRows`, `encode_group`/`encode_snapshot`) stays: it is
  the stream path's, not a `.rez` writer.
- **Added:** segment format 2. A segment holding a gauge histogram column is
  written with `metriken.format = 2`, which metriken-storage 0.4.0 refuses
  (earlier releases refuse any archive dendro 0.6 writes when opening it,
  on its schema version): opening such an archive with 0.4.0 fails with
  "probing table fs/ext4: segment format 2 is newer than this reader, which
  reads up to 1". Segments without one are still format 1.
  `format::FORMAT_VERSION` is now 2 and means the newest format this crate
  reads; a writer stamps `format::stamped_for(props, &schema)` (or
  `stamped_if_newer`), not `FORMAT_VERSION`. `format::format_of` gives a
  schema's format. A live WAL tail holding a gauge histogram is refused by
  0.4.0 too, through metriken-model 0.2's longer rows; the published
  releases' answers are in `docs/journal/2026-10-21-gauge-histogram.md`.
- **Breaking:** a column's histogram kind comes from the list its value
  arrived in: `CellValue::GaugeHistogram` (V1/V2 snapshots'
  `gauge_histograms`, `WalValue::GaugeHistogram` cells) and
  `GroupTableBuilder::push_row_with_gauge_histograms` (a group's
  `gauge_histograms` slots) write a `gauge_histogram` column; the counting
  lists write `counter_histogram`, replacing any `metric_type` the
  producer's metadata carried. A group holding a gauge histogram is written
  wide.
- **Changed:** new archive segments name the counting histogram
  `metric_type = counter_histogram` (was `histogram`); `.rez` segments, all
  written before, say `histogram`. Readers take
  `histogram`, `counter_histogram` and an absent `metric_type` as counting
  (`metriken_model::is_counter_histogram`), so every archive and file
  written before reads as it did; 0.4.0 also reads a new `counter_histogram`
  column as counting, since it classifies list columns by Arrow type. A list
  column whose `metric_type` names another kind (the sparse layout's
  `sparse_histogram` halves, which were read as garbage buckets, or a kind
  from a later release) is no longer read.
- **Breaking:** metriken writes no standalone parquet. `convert` is
  `MsgpackToArchive`, which writes a raw msgpack recording into a new
  `.dendro` archive (one source; `label`, `metadata`, `writer_config`,
  `convert_file_path`, `convert_reader`) through `ArchiveWriter`, and the
  `convert` feature now turns on `write`. `MsgpackToParquet`,
  `ParquetOptions`, `ParquetCompression`, `ParquetHistogramType` (with the
  sparse layout), `ParquetSchema` and `ParquetWriter` are removed. A
  standalone parquet file is opened by readers that do not check
  `metriken.format` (pyarrow, DuckDB, scripts), which nothing stops reading
  a new column kind as one they know. Older metriken readers refuse an
  archive: every reader before metriken-storage 0.4.0 (metriken-storage
  0.1.0 to 0.3.2, and rezolus 5.21 to 5.28: dendro 0.2.1 or 0.3.3, 5.23 onward on metriken-archive 0.2.7) runs
  on a dendro up to 0.5.1, which opens only schema 4, and dendro 0.6
  writes schema 5, so they refuse it when opening it; metriken-storage
  0.4.0 opens it and refuses a format 2 segment by its `metriken.format`
  key.
  `MsgpackToArchive` carries the first non-empty snapshot metadata into the
  source's metadata (the caller's entries first), writes `producer_epochs`
  when the snapshots' `producer_epoch` changes (a raw recording that spans
  a producer restart) and then takes the newest process's `version`
  (unless the caller set `version`, in which case no entry carries one).
  When the conversion fails it removes the files it created (the archive,
  and sidecars that did not exist before), never a file that was already
  there. Parquet files written before are read as
  they were.
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
- **Changed:** a histogram column's rows are converted to snapshots from the
  list's value offsets into its bucket buffer, instead of through
  `ListArray::iter`, which built an array per row. The snapshots are
  unchanged. On a 9.6 h recording, `histogram_quantile`, `histogram_count`
  and `histogram_heatmap` at a 60 s step take 88 to 90% of the time they
  did (five interleaved runs each, against a 99 to 101% run-to-run spread),
  and reading every histogram stream of the archive 87%.

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
