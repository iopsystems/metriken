# Recording identity, layered templates, and views

**Status:** OPEN. Design 2026-10-19, reframed by the owner on 2026-10-20:
storage keeps everything and records provenance, and the viewer decides
presentation. dendro's documents table, rezolus's combined view (#1504),
process groups in metriken-storage (#304) and the S9 per-source union for
counters and gauges in metriken-query (branch `feat/per-source-union`; "S1 to
S9 in metriken" below) are done; the producer identity keys and
`producer_identity!`, the identity layers in metriken-storage (#326), and
metriken-recorder writing them (step 2) are built and unreleased; nothing
else is built. The owner decided Q1, Q2, Q7 and Q8 on 2026-10-26, and amended A8 and
decided Q3, the layer entries and Q40 on 2026-10-27; Q1 was amended with the
producer identity keys on 2026-10-27. "Step N" means this entry's order of work.
Follows the metriken-viewer entry's Q11 (how an archive of several
recordings from one run opens) and Q12 (templates travel in the producer's
handshake) ([metriken-viewer](2026-10-12-metriken-viewer.md)), and rezolus's
combined view (#1504, merged as ddd8619b: one view per archive, A/B opt-in
through an `arm` label, recordings without an arm shown beside the
comparison). The owner set the principles on 2026-10-19, and on 2026-10-20
the framing, the label name `service_kind`, the behavioural cases, and
decisions O1 to O5 and P1 to P9 (under "Owner decisions" in the
specification). Earlier revisions of this entry stored runs and sessions as
identity layers, joined runs in `combine`, and refused duplicate sources;
the reframing drops all of that (listed under "Dropped by the reframing" at
the end of the specification).

Sources: rezolus upstream/main 9acb5a25, metriken upstream/main 451ef1f,
dendro origin/main 66a2390 (v0.6.0 and a version bump), llm-perf
upstream/main 5704c35, cachecannon upstream/main dbd17a5; systemslab's
behaviour as of 2026-10-20. Line numbers are at those commits, except that
citations added with the 2026-10-26 and 2026-10-27 decisions are at rezolus
upstream/main 5a53b921 and metriken upstream/main 8f1e17f. Facts about
other software: prometheus-operator's ServiceMonitor adds a `service` target
label to scraped Kubernetes data; OpenTelemetry's Prometheus mapping sets
`job` to `service.namespace/service.name`, or to `service.name` alone when
the namespace is empty; Prometheus's scrape moves a scraped label that
clashes with a target label to `exported_<name>` when `honor_labels` is
false; and Prometheus removes target labels beginning with `__` once target
relabeling finishes, while scraped series labels and the output of metric
relabeling are not stripped, and `__name__` is a series label. These were
checked in a review against those projects' sources, not in this entry's
repositories.

## Motivation

People record metrics from several producers at once and look at the result
in many ways, and the same data is often looked at more than once. The
design has to let them:

- **M1. Open one file and see its producer's dashboard.** A rezolus
  recording shows rezolus's sections; an llm-perf, cachecannon or vLLM
  recording shows that producer's KPIs, with nothing to configure.
- **M2. See several producers recorded together.** A benchmark records the
  load generator, the server under test and the rezolus agent on each host.
  Each producer's dashboard is shown, and a producer's template can use
  another producer's metrics when that producer is present (llm-perf showing
  the server host's CPU beside its request latency).
- **M3. See one producer process once, however many times it was
  recorded.** Copies, trimmed or filtered copies, hindsight dumps of one
  buffer, a colleague's copy, and two recorders of one agent running at the
  same time hold overlapping rows from the same process. Shown together
  they are one recording, and no query counts that process twice.
- **M4. Compare.** A/B, with arms recorded on different days, concurrently
  in one record invocation, or back to back on one host; any number of arms
  drawn against a reference.
- **M5. See tiers and fleets.** Several agents of one software in one
  recording (client and server hosts, a fleet) are grouped by tier or host
  on the same chart, each group aggregated, with each member one toggle
  away.
- **M6. Add without losing.** Combining files, adding labels, templates or
  views adds a layer or an entry in one. No edit overwrites or removes what the
  producer, the recorder or an earlier edit wrote: a later write adds an entry,
  and the earlier entries stay readable (Q2). Trimming or retention removes
  only the entries of epochs it removes every row of. `combine` never refuses
  or drops a source.
- **M7. Reuse a view.** A view saved today applies to next week's
  recordings of the same setup without editing.
- **M8. Carry the view in the file.** A `.dendro` holds what is needed to
  reproduce a view: the data, its provenance, and the user's saved
  documents. A shared report opens the same for its recipient.
- **M9. Know why it looks this way.** Each consumer says which rule picked
  the presentation, offers the alternatives, and saves the user's choice.
- **M10. Get the same answer everywhere.** The server viewer, the static
  site, MCP, `recording check` and systemslab present and query an archive
  the same way.
- **M11. Keep opening old inputs.** `.rez`, parquet, v4 `.dendro`, and
  recordings from before producer epochs open, with defaults computed from
  what they carry.

Scope: multi-arm statistics, high-dimensional parameter sweeps,
cross-experiment analysis and cross-producer anomaly detection are
systemslab features, built on top of this; dendro, metriken and rezolus
provide one archive's presentation and queries (P5, P7).

Storage and presentation are split because the decisions above depend on
intent. Whether two agents are tiers of one benchmark or the two arms of an
A/B, whether a second copy of a recording is a duplicate or a finer-grained
view of the same process, and whether two runs a week apart are a
comparison: the data alone does not determine it. A storage operation that
refuses a duplicate or joins two recordings into a run makes that decision
once, for every later reader, without the user. The viewer can make it from
evidence, say what it chose, and let the user change it; the change is
saved as a document beside the data and does not alter it.

### Workflows

Each workflow below came from the scenario reviews of 2026-10-19 and
2026-10-20. The specification's cases are checked against them.

- **W1.** A tiered benchmark: llm-perf's own `.dendro`, which llm-perf
  records in process with the vLLM server's metrics (llm-perf
  `src/recording.rs:1-4`), combined with a `rezolus record` of the agent on
  each host.
- **W2.** A/B across days: the same benchmark on Monday and on Tuesday,
  combined afterwards.
- **W3.** A concurrent A/B in one record invocation: two servers recorded at
  once, `--endpoint a,arm=a --endpoint b,arm=b`.
- **W4.** systemslab experiments with retried attempts, exported from one
  systemslab and imported into another.
- **W5.** Hindsight dumps: several dumps of one buffer around an incident,
  which overlap when the buffer has long tables.
- **W6.** Agent restarts within one recording (several producer epochs in
  one source).
- **W7.** Copies: a trimmed, filtered or annotated copy beside its original,
  or a colleague's copy.
- **W8.** Old inputs: `.rez`, parquet, v4 `.dendro`, pre-v5.21 recordings
  with no producer epoch.
- **W9.** Prometheus-only recordings, including federated Kubernetes data
  that carries its own `job`, `instance` and `service` labels.
- **W10.** Sharing a report: Save-as-Report, sent to someone who opens it in
  the static site.
- **W11.** Reapplying a view next week to new recordings of the same setup.
- **W12.** MCP queries and `recording check` over a multi-source archive.
- **W13.** The static-site viewer opening any of the above in a browser.

## Principles

1. **Flexible ways of looking at the data.** No assumption about A/B versus
   grouped versus ungrouped, how many levels a run has (server; client and
   server; client, server and database; …), or how many arms a comparison
   has.
2. **Additive operations.** A user-supplied template, label or view is a
   layer in P5's order; it never overwrites the producer's copy.
3. **Every key is open to redesign.** The current labels, keys and values
   grew organically and are open to reconsidering.
4. **Views are reusable.** A user can reshape a template or a view, save it
   in the archive, eject it to a file, and apply it to future sets of
   recordings.

## Today

- **The combined view assumes two arms.** rezolus's comparison takes one
  recording per arm and at most two arms (`combined::arm_slots`,
  `crates/dashboard/src/combined.rs:296`), and templates are found by
  `source`. It refuses three shapes: two recordings sharing an arm ("two
  recordings carry the same `arm`", `:315`), more than two armed recordings
  (`:318-323`), and one armed recording among several ("one recording
  carries an `arm`; a comparison takes two arms", `:328`). Without
  a comparison, every recording's sections are listed together: the first
  copy of a template has its routes, each further copy the routes followed
  by `@<key>` (`/cpu@web-02`, `/service/<name>@<key>`), and every entry names
  its recording (`build_combined_context`'s doc comment,
  `crates/dashboard/src/dashboard/mod.rs:310-345`; read from the comment,
  not rendered).
- **One process recorded twice is counted twice.** An archive reader adds
  `__recording__`, with the catalog index, to recordings whose label sets
  are equal (metriken-storage `ArchiveReader::series_labels`,
  `reader.rs:1714-1741`), so two copies of one recording read as two series
  and a `sum` adds both. rezolus `combine` refuses two inputs that share a
  source uuid ("combining them would count it twice", `combine.rs:784`,
  through dendro's `shared_sources`), which catches a copy made by dendro
  and nothing else: `to_dendro` mints a new uuid for each `.rez` recording
  (metriken-storage `src/rez/to_dendro.rs:74`), and two recorders of one
  agent write two uuids.
- **dendro copies keep a source's uuid, and dendro does not require uuids
  to be unique.** `copy_sources_into` inserts each copied source with its
  original uuid (dendro `src/rewrite.rs:828`, `insert_source_with_uuid`);
  the `sources` table has no unique constraint on `uuid`
  (`insert_source_sql`, `src/archive/sqlite.rs:3304-3321`; dendro's writer
  documents that nothing checks it, `src/writer.rs:450`). A fresh source gets
  a random uuid (`mint_uuid`, `randomblob(16)`). Hindsight dumps go through
  dendro's copy, so dumps of one buffer share the buffer's uuid. A dump is
  trimmed at segment granularity, so it reaches at least as far as asked
  (`src/hindsight/buffer.rs:325-327`), and when the buffer has long tables
  it starts one restatement period early (`:328-331`, `:361`); nearby dumps
  overlap either way.
- **`source` does at least seven jobs:** the user's name for an endpoint
  (`--endpoint url,source=web-01`), the software identity (comparisons with
  `"rezolus"` in `combine.rs`, `filter.rs:145`, `source_kind.rs:45`, the
  dashboard builder (`dashboard/mod.rs:775`), the viewer's `metadata.rs`,
  the analysis extractor and the WASM viewer
  (`crates/viewer/src/lib.rs:1663`), among others), the template registry
  key, the category membership key, the series label that 215 matchers in
  ten of the eleven templates under `crates/dashboard/templates/` select
  (occurrences of `source=\"`; `inference-library.json` has none), a
  per-series provenance value (`external`, `hwmon`, `thermal`, …), and the
  agent socket address in hindsight's config. It is both a recording label
  and a metadata copy, and the two can disagree (`--metadata source=` or
  `--label source=` under `--endpoint`; metriken-recorder refuses only
  `producer_epoch` and `producer_epochs` as caller metadata,
  `recorder.rs:660-672`).
- **Values are overwritten and lost**, among them: `--label source=`
  replaces an endpoint's `source=`; `template=FILE` on an endpoint (#1501)
  replaces the producer's handshake template; a restarted producer's
  metadata replaces the old (metriken-recorder's default merge takes the new
  value, `probe.rs:71-76`, called from `restart.rs:120-128`; rezolus's probe
  hook overrides it, `src/recorder/agent_probe.rs:211-213`, with a `merge`
  that unions `descriptions`, `:222-239`; caller-pinned keys are kept, and
  the old `version` survives only in its `producer_epochs` entry,
  `restart.rs:50-57`, `:87-92`); `combine` to parquet drops
  `producer_epoch`, `producer_epochs` and `dashboard`, keeps each input's
  `version` only under `per_source_metadata`, and takes the top-level
  `systeminfo` from the first rezolus input; metriken-storage's
  `ArchiveReader::metadata()` (`reader.rs:1777-1780`, and so `source()`,
  `version()`) reports only the first recording; `annotate --source
  --overwrite` loses the old name.
- **Precedence disagrees:** the handshake beats the probe hook for `source`
  and loses to it for every other label and for metadata (metriken-recorder
  `identity.rs`); an invocation-wide `--label` beats the endpoint's
  `source=` while invocation-wide `--metadata` loses to the endpoint's
  `role=`/`template=` (`role=` only since #1501). A label clash is resolved
  five different ways by path: archive, `exported_<key>` (metriken-storage
  `labeled.rs`, test `a_clashing_series_label_moves_to_exported` in
  metriken-query `src/archive.rs:898`); parquet multi-source, overwrite
  (metriken-storage `MultiParquetSource`, `parquet.rs`, which inserts the
  injected value over the series' own behind a `debug_assert!`); occupant
  versus column, column; rezolus `combine`, `source` only if absent but
  `node`/`instance` always (`combine.rs:1486-1489`); the agent's external
  metrics, the pushed label wins (`src/agent/exposition/http/snapshot.rs`).
- **Names collide:** `kind` (static series label, `Event.kind`,
  `SourceKind`); `instance` (the Prometheus convention for a target address,
  which nothing sets to a target address; a series label in a combined
  parquet for its non-rezolus inputs, which get `instance` while rezolus
  inputs get `node` (`combine.rs:1469-1471`), from the input's `instance`
  metadata, else `"0"` or a sequence number, `combine.rs:1046`, `:1070`,
  `:1473`; `Event.instance`; a template copy in the combined view); `role`
  (endpoint metadata shown in the viewer's Sources table,
  `src/viewer/assets/lib/sections/section_views.js:242`, llm-perf's
  `loadgen`/`service` source label, `src/recording.rs:39-43` and `:128`, and
  `Kpi.role`); `id`; `baseline`/`experiment` (capture ids, selector flags,
  `combine --ab` keys, and positional display aliases reserved from label
  values, `capture_alias.rs`); `rezolus` (a source, a `protocol=` value, the
  binary name). One concept has several names: the machine is `host`, `node`
  and `hostname`; a recording is a recording, capture, source, side, member,
  slot or instance; a quantile is `quantile` in the engine and `percentile`
  from the exporter; the producer's version is `version` in metriken and
  `producer_version` in dendro.
- **`service` and `job` are taken in other systems.** prometheus-operator's
  ServiceMonitor adds `service` to scraped Kubernetes data, and
  OpenTelemetry sets `job` to `service.namespace/service.name` (or
  `service.name` when the namespace is empty); federated Prometheus data
  recorded by rezolus keeps those labels on its series. systemslab adds
  `job` and `experiment_id` at query time. Nothing in rezolus, metriken,
  dendro, llm-perf, cachecannon or systemslab writes or reads `service_kind`
  (`git grep` on each).
- **The query restriction is in the viewer only.** `scope_matchers` and
  `restrict_query` (`combined.rs:433`, `:611`) rewrite each query of a
  template copy to its recording's label matchers; the only non-test caller
  is `crates/dashboard/src/dashboard/mod.rs`. `restrict_query` leaves a
  selector's own `k="v"` in place of the scope's matcher on `k`
  (`pinned_labels`, `:786`), so a selector written for one source keeps its
  own `source` matcher and the scope's is not added.
- **Per-endpoint options are a fixed list.** `--endpoint` accepts `source`,
  `role`, `template`, `arm`, `host` (#1538) and `protocol` and refuses
  anything else ("unknown endpoint option", `src/recorder/config.rs:430-456`);
  `--label k=v` applies to every recording of the invocation.
- **Archive-level storage exists but rezolus does not use it.** dendro 0.6.0
  has an archive-level `documents` table (dendro
  `src/archive/sqlite/documents.rs:24-44`; design in dendro's
  `docs/journal/2026-10-19-archive-documents.md`). rezolus writes a report's
  selection (with its pins) and archive-wide events into one recording's
  metadata, under `selection` and `events`, plus `report` for a trimmed save
  (`crates/report-save/src/lib.rs:509-523`, `:735-748`; `descriptions` is
  never written to the anchor): the anchor, which is the recording
  whose labels equal the baseline recording's, else the first in catalog
  order (`crates/report-save/src/lib.rs:552`, `anchored`). The anchor is
  matched by full label set, and `anchored` falls back to the first
  recording without a message when no label set matches.

## Behavioural specification

Each rule below is written as inputs and the expected result, so a reviewer
can turn it into a test. "The viewer" means both rezolus viewers (server
and WASM), which share the `dashboard` crate; MCP and `recording check` call
the same `dashboard` crate function (Every consumer), and systemslab is a
consumer of that crate (Consumers: systemslab).

### Owner decisions (2026-10-20)

Recorded in three rounds: O1 to O5, then P1 to P9, then R1 to R4 and the
adopted items A1 to A13. A later item replaces the earlier text it
contradicts; the list below is the merged result.

- **O1. Grouping draws groups on one chart.** A grouping label (`arm`,
  `tier`, `host`, a user label) by default shows one series per group on
  the same chart. The members of a group are aggregated (P3), with a toggle
  that splits the group into one line per member. `arm` and time clusters
  compare (P4): a reference group, the diff views, and elapsed alignment
  when the groups do not overlap. Tiers and other labels group without a
  comparison. Different producers (case 2) are nested navigation groups.
- **O2. Same epoch, disjoint spans: separate recordings.** Only sources of
  one epoch that overlap (R3), or sources with one uuid, are unioned.
  Back-to-back experiments on one host with a daemon agent are then case
  4, an A/B with elapsed alignment. The notice offers "treat as one
  recording" (one timeline with a gap).
- **O3. Three concepts.** The kind is the software, from `__producer__`; it
  drives cases 2 and 4, template lookup, and cross-producer requirements.
  `service_kind` refines a producer-given kind (vLLM prefill against decode
  is case 4 with navigation sub-groups); for a source with no producer
  layer it supplies the kind through a registry match (Q35). The tier is a
  user label, `tier`, not unique by design: three server hosts share
  `tier=server`, and grouping by tier aggregates them; it works for host
  metrics when `__producer__` is unknown. `host` names the machine;
  `instance` is the URL the recorder used (Q1). A legacy `--endpoint
  url,source=X`, and an old `source` that matches no known producer, map
  to a plain naming label `alias=X` (P2), not to a kind.
- **O4. systemslab tiers** (planned). The experiment author gives a job a
  tier in its spec, and the runner passes `--label tier=<value>` to the
  recorder it starts for that job. The tier is a label the author chose, not
  a systemslab id, so systemslab still adds none of its ids to the file. No
  tier is derived from job names, which are unique; a job without a tier
  gets no `tier` label, and its recording is drawn as its own line (A8).
  Inside systemslab its query-time `job` label may also be used as a
  grouping label.
- **O5. Open-time label overlay.** metriken-query accepts a label overlay when
  an archive is opened: selectors (a source uuid, or a kind and labels) mapped
  to labels to add or change, applied in memory for the caller's session and
  never written to the file by rezolus. It is a generic caller API; a viewer
  relabel uses the same mechanism and, saved, becomes user-layer entries on the
  sources it names, or, for a rule meant for other archives, a label layer
  document (Q40). Its place in the layer order is P5's.
- **P1. Two grouping levels.** Level 1 is `arm`, else time clusters when there
  are two or more (R2). Level 2 is `tier`, else a single differing asserted
  label (the rules of 4d), else `host`. Charts draw one line per level-1 ×
  level-2 pair: two days × {client, server} is four lines, with elapsed
  alignment and a reference per tier. Level 2 is chosen over the whole archive.
  A8 (amended 2026-10-27): within each level-1 group, each presented recording
  that lacks the level-2 key is drawn as its own line, named by `host`, else
  `instance` (4e), and the notice names them; no group of untiered recordings
  is formed. Across level-1 groups such a line is compared with the untiered
  line of the same `host` in the reference group. A line without `host`, a line
  whose host has no untiered line in the reference group (it may be a tier
  member there), and the lines of a host with more than one untiered line in
  its own group or the reference group are drawn without a reference, and the
  notice names them. A combined archive whose sources carry no `tier` and no
  qualifying asserted label is drawn one line per host (4e). The user assigns
  tiers at record time or by grouping in the viewer; saved from the viewer, the
  assignment is written as `tier` labels in the user layer of those sources
  (`metriken.layer.user`, Q2), through the metriken-storage layer function. The
  save is a new entry in that layer (Q2); being later in the fold, it wins over
  a `tier` asserted at record time, which stays readable in its own entry.
  Tiers stored this way stay with the sources and do not travel with a view to
  another archive. Other labels saved from the viewer (`arm`) go to the user
  layer the same way (Q40). When level 1 is `arm` and one arm's members fall in
  two or more time clusters, time clusters become level 2 (A9). A saved
  grouping with more levels draws one line per tuple of its aggregated levels'
  values; an expanded level draws its members; the reference is chosen per
  tuple of the values other than level 1 (A10). A line keyed by `instance` (a
  recording without `host`, 4e) has no reference across level-1 groups, as
  under A8.
- **P2. `alias` is the naming label.** The legacy `source=X` and an old
  unmatched `source` become `alias=X`. `name` is not used, because rezolus's
  cgroup series carry a `name` label (for example
  `cgroup_cpu_instructions{name="/system.slice/rezolus.service"}`,
  `crates/dashboard/src/dashboard/rezolus.rs:24`). `alias` is never a
  grouping key.
- **P3. Member aggregation per plot type.**
  - Counters and rates sum.
  - Histograms merge buckets and then derive, so a tier's p99 is a real
    p99. "Heatmap" here means a histogram heatmap. Members with different
    bucket configurations are downsampled to the coarsest common
    configuration, with a notice; with no common configuration each member
    is drawn on its own (A7).
  - Ratios are recomputed from the aggregated numerator and denominator
    when the template declares both, else meaned with a notice.
  - A gauge takes the template's declaration. An undeclared gauge, in any
    template, defaults by unit: percent → mean, bytes → sum, anything else
    one line per member with the notice "no aggregation declared" (A6).
  - Per-device charts (rows keyed by `id`, for example per-GPU heatmaps, or
    a `sum by (id)` query) are not aggregated across members: they are
    drawn per member, as small multiples or as rows keyed by (member,
    `id`). `topk` and `bottomk` are computed over the aggregated series
    (A5).
  - A member missing the metric is left out, and the legend says "2 of 3
    members", computed per step; a mean is over the members present at
    each step.
- **P4. Time clusters compare like `arm`:** a reference group, the diff
  views, elapsed alignment. `arm` and time clusters compare; tiers and other
  labels group without comparison.
- **P5. One layer order,** from least to most specific; the last wins:
  1. the viewer's registry fallback;
  2. the recorder layer: what the recorder writes from observation
     (`instance`, the producer epoch, and `host` from rezolus's
     `/systeminfo` until the producer's handshake carries it, Q1), and the
     recorder default template, which the recorder binary chooses to embed
     (`record --endpoint url,template=FILE` writes the KPI template,
     `service_queries`; the planned `dashboard=FILE` would write the
     sections key, `dashboard`);
  3. the producer layer, which wins over the recorder default template
     because it carries version-specific queries;
  4. the user layers: labels asserted at record time (`--label`, per-endpoint
     labels, `service_kind=`, `alias=`; a per-endpoint value beats an
     invocation-wide one), then later user-layer entries (`recording annotate`,
     labels saved from the viewer, P1), the newest winning on a key (Q2); then
     the archive's saved user layers (views, template layers, relabels);
  5. live unsaved edits.

  The asymmetry is deliberate (R1): a record-time label sits above the
  producer layer, because the user is asserting a fact; a record-time
  template sits below it, because the producer's carries version-specific
  queries. `record` warns when `template=FILE` is given and the producer
  sends a KPI template (A12). An open-time overlay (O5) is applied by its
  caller above the archive's saved layers for that caller's session. To beat
  a broken producer template, a user saves a user layer that hides
  everything beneath and adds their own (D4); a record-time flag that writes
  a user layer may be added later (Q38). systemslab is not in this order: it
  is a consumer that renders each archive with the archive's own layers and
  keeps its own views at its own levels in its own storage (Consumers:
  systemslab).
- **P6, as amended by R2 and R3. Overlap and clusters.**
  - Two sources of one epoch overlap when the intersection of their spans
    is non-empty and holds at least one row of each (R3).
    - Sources sharing a uuid are first taken as one (copies): a uuid group.
  - A bridge is a uuid group whose span overlaps two other uuid groups that
    do not overlap each other and covers the gap between them (R3 overlap
    for one epoch, plain span overlap otherwise); for example a continuous
    daemon recording beside one recording per day. By this rule the middle
    source of a chain (09:00–11:00, 10:00–12:00, 11:30–13:00) is a bridge.
  - Bridges are found and sliced over the whole archive when no source
    carries `arm`, and among each arm's members when some do; the clusters
    that decide level 1 and A9 are formed after this. A source is never a
    bridge across arms, and a source without `arm` (shared context) is
    never a bridge.
  - Order: uuid groups first; then bridges; then process groups from the
    other uuid groups by the union rule; then clusters, across all kinds
    (A1), from the presented recordings those give, by transitively
    overlapping spans. Each bridge is then sliced at the clusters' spans.
  - Pieces: within a cluster, pieces and process groups join by the union
    rule. A piece overlapping two or more process groups of its epoch is cut
    at their spans first, and each part joins the one it overlaps. A piece
    that joins nothing is a presented recording of its own; the pieces of
    two bridges recording one agent union with each other. A bridge does not
    join process groups in different clusters, so a continuous recording
    does not merge two attempts (case 3). The pieces of a bridge between
    clusters are not drawn, and the notice names the bridge and the time it
    leaves out.
  - The exact algorithm is settled with a prototype against real archives;
    the scenarios it must pass are Q39.
  - Cross-producer sections bind within their level-1 group (A1).
- **P7, as corrected by A4. MCP scope.**
  - `describe-recording`, `describe-metrics`, `detect-anomalies` and
    `extract-features` work per presented recording. With no selector they
    iterate the presented recordings in catalog order and report each one
    separately, never pooled: a ranked top K per recording, under a total
    cap on the result size. Today `extract-features` refuses a
    several-recording archive without a selector (it passes `combined_ok`
    false, `src/mcp/mod.rs:152-156`), and `detect-anomalies` runs pooled
    over the combined reader; P7 changes both.
  - `query` and `analyze-correlation` span recordings only where the PromQL
    or the selectors say so. With no selector, an archive with arms or time
    clusters is refused with a listing, as main refuses one with arms
    (`src/mcp/mod.rs:152-156`); an unarmed, single-cluster archive reads
    every recording, copies once after S1. Today main reads an unarmed
    archive combined (`:295-310`), so copies count twice until S1.
  - MCP does not evaluate per group. Cross-producer anomaly detection,
    multi-arm statistics and sweeps are out of scope (Motivation, Scope).
  - Notices go in the tool result on the MCP server (the stdio server's
    stderr does not reach the client) and on stderr for the CLI.
- **P8. Save-as-Report records the archive's own layers only.** With an
  overlay active it warns, names the overlay's labels, and offers to save
  them into the report as a label layer document; it saves them only with
  the user's consent (A11).
- **P9. Unidentified sources.** Sources with no identifiable kind
  (unmatched Prometheus scrapes) form one navigation group, "unidentified",
  presented by P1 inside it: level 1 as everywhere; level 2 `tier`, else
  4d's single label, else `host`, else `instance`. They count as one kind
  for presentation step 3 (A3).
- **R4. systemslab is described generically.** This entry names
  systemslab's query-time `job` and `experiment_id` labels, says that it
  keeps its own views at its own levels, and that its import does not
  preserve its ids.
- **A2. A split of same-uuid copies** names each copy by uuid and time span.
- **A13. Default views on combine.** Combining into an archive that has a
  saved view keeps the first input's default view as the default; when the
  first input has none and a later one has one, the later one's becomes the
  default; the other inputs' defaults become named views. A recording is
  unplaced when no selector or hand-picked set of a view that names its
  members matches it; unplaced recordings are shown with the notice "N
  recordings not placed by the saved view".
- A cross-producer section declares its binding (case 2).

### Data model

- **Producer process.** One agent or service process, identified by its
  `producer_epoch`, a v4 UUID minted per process (#1231, first released in
  v5.21.0). In a `.dendro` source written by 6.x, each `producer_epochs`
  entry runs from its `from_ts` to the next entry's; entries are written
  only when the producer restarts (metriken-recorder `restart.rs:54`), and
  otherwise the source's `producer_epoch` covers its whole range. A
  recording written by v5.21.0 to v5.28.0 that spans an agent restart has
  no epoch history: a `.rez` carries the epoch it opened with (the manifest
  is seeded at open, v5.28.0 `src/recorder/mod.rs:1425`), and a parquet
  carries the last one (the footer is written at the end of the run from
  `ep.agent.producer_epoch`, `:887-892`, which `note_epoch` overwrites at
  each restart, `:576`, after warning, `:558-578`, called from
  `note_epoch_change`, `:552-556`). #1415, which writes the history, is in
  no tag yet. An in-process `RegistrySource` takes the process-wide epoch
  (metriken-recorder `src/local.rs:41-48`), so two in-process sources of
  one process share one epoch (Q9). With the layers, the epoch history
  moves into the recorder layer's entries (Q3).
- **Source.** One endpoint's rows in one archive, across the producer epochs
  that served it: a dendro `sources` row with labels, metadata and a `uuid`.
  The uuid is minted at random when a recorder creates the source and kept
  by every dendro copy, so two sources with one uuid are copies of one
  source; the `sources` schema has no `UNIQUE` on it (dendro
  `src/archive/sqlite.rs:3525-3535`). A `.rez` recording converted by
  `to_dendro` gets a new uuid. A combined parquet's sources are its `source`
  values.
- **Process group.** Sources sharing a uuid are first taken as one (copies,
  P6). With bridges set aside (P6), these uuid groups join one process group
  when they share a producer epoch and overlap within it (R3: a non-empty
  intersection holding at least one row of each); the relation is closed
  transitively. A bridge's pieces join afterwards, within each cluster (P6).
  A source that joins no other is a process group of one. Process groups are
  formed regardless of kind. Sources sharing a uuid whose `host` values
  differ get a notice. The process group is computed at open time and never
  stored.
- **Labels of a process group.** A key some of its sources carry and others
  lack takes the carried value. The group is split into presented recordings
  only when two of its sources carry different values of an asserted label
  (a user-layer label) other than `alias`; an annotated copy keeps its
  original's `arm`, and differing `alias` values are shown in the
  information panel, as for Q10. A recorder- or producer-layer key with
  different values (two recorders' `instance`) is shown in the information
  panel with each value (Q10).
- **Presented recording.** What the viewer shows as one recording: a
  process group, or one part of a split group.
- **Recording.** The CLI's name for a presented recording (`--recording`,
  `describe-recording`). Elsewhere this entry says source for a stored row
  and presented recording for what is shown.
- **Kind** (O3). The software a source comes from: `__producer__`, from the
  producer layer, else the recorder layer's `producer_name`, which the probe
  hook takes from `/about`, `/status` or the `/` banner, or which D2's read
  derives (Q3), else the D2 read of a legacy `source`, else, for a source with
  no producer layer, the producer of the registry entry its `service_kind`, then
  its `alias`, matches (Q35). A source with none is unidentified (P9). Sources
  of one process group whose kinds differ (possible through D2's read, a
  registry match differing between sources of one process group, or two
  in-process sources of one process under Q9 (a); Q8 keeps one `__producer__`
  per source) take the kind of the source with the newest first timestamp, with
  a notice.
- **Labels.** Each source carries labels from the layers below. Grouping is by
  labels: asserted at record time (`--label arm=…`, `--label tier=…`,
  per-endpoint labels after step 3.4), in the viewer (saved into the sources'
  user layer, Q40, or as a label layer document that a view or report carries
  when it is a rule for other archives), or supplied as an open-time overlay
  (O5).
- **Process group and group.** This entry says "process group" for the
  sources S1 unions, and "group" alone for a group of a grouping (an arm, a
  time cluster, a tier).

### Identity in three layers

A source's identity is three layers, each kept whole and stored apart. What
a reader sees is the layers merged in P5's order, the last winning, with
the lower values readable.

1. **Producer layer:** what the software declared in its handshake, including a
   producer identity: name, version, build, and the hostname of the machine it
   runs on, which gives `host` (Q1). Rust producers will fill name and version
   without input through metriken-exposition's `producer_identity!`, expanding in
   the producer's crate (`env!("CARGO_PKG_NAME")`,
   `env!("CARGO_PKG_VERSION")`), and read the hostname at run time; metriken-py
   and others will set it explicitly; a recording made in process (llm-perf's
   own source through metriken-recorder's `RegistrySource`, llm-perf
   `src/recording.rs:169`) will take it as a field of its `SourceIdentity`, as
   it takes `epoch`. A Prometheus endpoint has no handshake; a `*_build_info`
   series, where present, may fill it (deferred). The producer layer holds a
   list of handshakes, at least one per epoch of a producer that sends a
   handshake, each stored as sent (its entries, Q2); the merged view folds them
   per key, newest last, the earlier entries stay readable by epoch, and
   retention keeps the handshake of every epoch it still holds data for. A
   handshake that changes identity is Q8: a new version or build is an event on
   the source, and a different producer name starts a new source.
2. **Recorder layer** (below the producer in P5): what the recorder observed:
   the endpoint (`instance`: the URL it recorded from, as given; Q1), the
   transport, the host from `/systeminfo` (rezolus's way of reporting the
   producer's host, until the handshake carries it), the served interval, and
   the producer epoch and its history (its entries, in place of the flat
   `producer_epoch` and `producer_epochs` keys, Q3); what the probe hook read
   from the endpoint's `/about`, `/status` or `/` banner, or what D2's read
   derives (Q3), as metadata (`producer_name`, `producer_component`, `version`
   or `exporter_version`, `agent_version`; none of them is a series label, since
   rezolus's cgroup and hw_sensors series carry their own `name`, Q3); and the
   recorder default template (`template=FILE`). The clock anchor a `.rez`
   recording stores is the agent process's (from `/status`, since v5.21.0,
   #1270); a streamed source takes the handshake's, which is the same process
   anchor.
3. **User layers** (above the producer in P5): labels asserted at record time
   (`service_kind`, `alias`, `tier`, `arm` and any other label, invocation-wide
   with `--label` or per endpoint, the per-endpoint value winning), labels
   saved from the viewer (P1, Q40) and labels written by `recording annotate`,
   stored in the source's metadata as one entry per write, the newest winning
   on a key (M6, Q2); then relabels and template layers saved as documents in
   the archive; then live edits. An open-time overlay sits above the saved
   documents for its caller (P5).

A consequence of P5 (R1): an asserted label beats the producer's declared
label on the same key, while a `template=FILE` given at record time sits
below the producer's template. A label a tool supplies when it runs the
recorder is a user-layer label, as `--label` is, including one it puts on
its own in-process source (Q7). llm-perf's `source` and `role=loadgen` on
its own source and `role=service` on the server's endpoint (llm-perf
`src/recording.rs:128`, `:171-174`) are therefore user-layer labels; the
two `role` values become `tier=client` and `tier=server`, and `source` is
read by D2.

Names:

- **`service_kind`** refines a kind (O3; chosen over `metriken_service` and
  `__service__`). Neither `job` nor `service` is used, because federated
  data and systemslab already carry them (Today).
- **`alias`** is the user's name for an endpoint, where the legacy
  `source=` goes (P2). No rezolus series carries it. The word is already
  used for three other things, which Q4's renames cover: a capture's
  display alias (`CaptureIdentity.alias`, `capture_alias.rs:65`; the
  viewer's capture list, `crates/viewer/src/lib.rs:1112-1120`), an A/B
  tarball side's alias (`AbSide.alias`, `src/parquet_metadata.rs:186`), and
  a KPI template's `aliases`, other names the registry finds it by
  (`ServiceExtension.aliases`, `crates/dashboard/src/service_extension.rs:13`;
  the registry is indexed by `service_name` and each alias, `:568`;
  `llm-perf.json` lists `llm-bench`, `valkey.json` lists `redis`).
- **`tier`** is the user label for a role in a benchmark (client, server,
  database), shared by every endpoint in that role (O3, O4).
- **`instance`** is the URL the recorder recorded from, as given. It tells
  two endpoints of one archive apart and is shown to the user; nothing
  binds or unions on it across presented recordings, because a port
  forward or a proxy makes the address the recorder's view rather than the
  producer's (Q1). 4e and P9 use it as a last-resort series key.
- **`host`** is the producer's hostname from its handshake (for rezolus,
  `/systeminfo` until then), or a user-layer `host` asserted at record time
  for a producer that declares none, such as a Prometheus endpoint.
- **`__producer__`** is the producer's declared name, set from the producer
  layer, else from the recorder layer's `producer_name` (Q3). Prometheus
  removes target labels beginning with `__` after target
  relabeling; rezolus keeps `__` labels by its own rule (`docs/labels.md`):
  stored, part of series identity, matchable in a selector, hidden from
  listings, legends and MCP output, and dropped by aggregation. Because it
  is hidden, a legend shows the kind from the navigation group, not from
  the series.
- **`role`** keeps its current meanings, listed with the renames (Q4);
  llm-perf's label moves to `tier` (Q7).
- **Series labels** come from the identity keys of every layer, as
  recording labels do today: `__producer__`, `instance`, `host`,
  `service_kind`, `alias`, `tier` and every other user label. The
  transport, the interval, the epoch history and the source `uuid` stay
  source metadata. On one key the last layer in P5's order gives the series
  label; the lower values are readable per layer in the source's metadata
  and information panel (Q2 for the storage).
- **One clash rule.** A series that carries a key the recording adds keeps
  its own value under `exported_<key>`, equal values included, with a
  further `exported_` prefix when the series also carries
  `exported_<key>` (metriken-storage `labeled.rs:1-17`), as Prometheus's
  scrape does with `honor_labels` false. This replaces the five rules in
  Today on every path (archive, parquet, combined parquet, occupants,
  rezolus `combine`, the agent's external metrics). This work depends on it
  (owner decision): federated Prometheus data carries its targets'
  `instance` on every series, and the recorder-layer `instance` has to move
  it to `exported_instance` on the parquet multi-source path as well, where
  today the added value overwrites it. `alias` was chosen so that it does
  not clash with a rezolus series label (P2).

### D2: how `source` is written and read

- **Write rule for new recordings.** Today `source` is written as the user's
  `source=` when given, else the producer's name, else the name inferred from
  the URL (metriken-recorder `identity.rs:75`, `infer_source_name`, for example
  `svc-9090-federate`), or `local` for an in-process source with none
  (`recorder.rs:1541-1550`). From step 2, when the layers land, the recorder
  writes no `source` of its own: the producer's `source`, where its handshake
  declares one, stays in the producer layer, and a `source=X` the user gives is
  a user-layer label, which wins over the producer's (P5) as it does today,
  until step 3.4 writes it as `alias=X` (Compatibility); a user-layer `source`
  (a rezolus endpoint's between steps 2 and 3.4; llm-perf's `server_source`,
  and the `source=llm-perf` on its own source, until step 6) is read as the
  read rule below reads a legacy `source` (a registry match, else `alias`). The
  probe hook's `source=rezolus` is the recorder's own and is not written; the
  agent's handshake declares the same value in the producer layer. A registry
  template's `source=` matchers keep matching a user-given name through step
  3.3. A 5.x reader's template lookup by `source` is not kept working; this
  work does not provide forward compatibility (Q3).
- **Read rule for recordings made before this.** An old recording does not say
  who set its `source`: metriken-recorder reports `inferred` in `Event::Active`
  only (`recorder.rs:201-207`), and nothing in the archive records it. So
  `__producer__` is taken from evidence other than `source`. The evidence,
  first match wins: 1. The stream handshake's `dashboard` template, whose
  `revision` the agent sets to `rezolus <version>`
  (`src/agent/exposition/http/mod.rs:283-301`): `__producer__` is `rezolus`. 2.
  A `version` the probe hook wrote, or that a msgpack snapshot's metadata
  carries (Q3), together with one of the self-metric names in
  `REZOLUS_SELF_ANCHORS` (`crates/dashboard/src/source_kind.rs:32`): `rezolus`.
  For a legacy recording of a Prometheus scrape (a `prometheus/scrape` group, or
  a parquet or combined-parquet input with no `version` and no
  `sampler_status`), which carries no `version` (Q3), the self-metric names
  alone suffice when the `REZOLUS_SELF_ANCHORS` series' own labels, before the
  recording's labels are added, carry no `job` label, with or without
  `exported_` prefixes: the agent's own series carry none, and a Prometheus
  server's `/federate` re-emits them with each target's `job`, so a federated
  scrape stays unidentified. `instance` is not tested, since `parquet combine`
  stamps it on every input whose `source` is not `rezolus` (v5.28.0
  `combine.rs:1096-1115`). The viewer shows a notice that the source was
  identified as rezolus from its metric names. 3. `sampler_status` under
  `per_source_metadata`, which v5.15.0 to v5.28.0 recorders wrote one level deep
  in the parquet of an agent that answered `/samplers`
  (`build_per_source_metadata`, v5.28.0
  `src/recorder/mod.rs:1796-1840`) and a combined parquet carries two levels
  deep (`source_kind.rs:116-121` reads only the latter): `rezolus` for that
  source. 4. A carried `service_queries`: its `service_name`. This is weak:
  `template=` and `annotate --queries` write the same key, and an old recording
  cannot say whether the user replaced the producer's. 5. Otherwise none.

  Then `source`, when it differs from `__producer__`: a value that names a
  known producer (a registry entry) reads as that producer, refined by the
  entry's `service_kind` (Q35); any other value reads as `alias`. The probe
  hook and the agent's stream handshake both set `source=rezolus`
  (`src/recorder/agent_probe.rs:191`; `src/agent/exposition/http/mod.rs:247`,
  `CARGO_BIN_NAME`), so on a recording that 1, 3, or 2 with a `version`
  identify, `source` other than `rezolus` was set by the user; on a scrape
  identified by its metric names alone it may be the inferred name. A
  Prometheus scrape (a `prometheus/scrape` group) has no producer layer, so
  its `source` is read by the registry match alone, and an inferred name that
  matches no registry entry reads as `alias`.

  Identity is read only from evidence that names one producer: a declaration (a
  handshake, an answer from `/about`, `/status` or the `/` banner, the agent's
  `sampler_status`), a name the user gave that matches a registry entry, or, for
  a recording made before the layers only, a set of metric names that only one
  producer emits (evidence 2). A new source whose probe hook recorded no
  `producer_name` is not identified by its metric names. When evidence 1, 2 or 3
  identifies a legacy source, a further at-open recorder entry D2's read derives
  (`by: upgrade`, or `by: convert` when `recording convert` writes it, Q2)
  carries that `producer_name`, so the identification survives the layers being
  written and retention, which keeps entries without `epoch`; one identified by
  its metric names without a `version` keeps its notice, which a reader
  re-derives from the source's metadata (no `version`, no `sampler_status`). A
  registry match on `source` is not written: the derived user-layer `source` is
  read by the registry rule as before, so the match follows the reader's
  registry. A name the recorder derived from a URL is not evidence, but a legacy
  `source` cannot be told from an inferred one (`infer_source_name` gives `vllm`
  for `http://vllm/metrics`, whose port is the default), so a legacy `source`
  that matches a registry entry is read as that producer with a notice that the
  name may have been inferred. Evidence 4 is still accepted, with its caveat
  above; it is not written either, and the carried `service_queries` stays in
  the derived metadata, read as before.


  Reading `source` alone does not identify the producer: metriken-recorder's
  `Identity::metadata()` writes the resolved `source` over the handshake's,
  so the producer's own value is gone; registry names include role names a
  user sets (`vllm-prefill`, `sglang-router`); and a `.rez` or `.dendro`
  stores no URL to recognise an inferred name by. A parquet's file-level
  `endpoint` (v5.21.0 on) does, but it is not used: a `source` equal to the
  name inferred from it may still have been typed by the user.

### D1: the per-query engine scope

- The restriction of a query to some sources is a separate input to
  metriken-query (a scope in `QueryOptions`, or an equivalent argument),
  applied by the engine to every selector's series set.
- The scope is per query, defaulting to its template copy's: a query placed
  from another copy carries that copy's scope. The case today is an
  Overview's throughput query, which `build_combined_context` takes from a
  service's copy (`crates/dashboard/src/dashboard/mod.rs:425-440`,
  restricted again at 644-650; test
  `each_instance_has_its_own_throughput_and_systeminfo`,
  `src/viewer/combined_test.rs:397`). A cross-producer section (case 2) is
  the second case.
- A scope names a set of sources. It resolves to the process groups of the
  presented recordings it names, restricted to that presented recording's
  sources when the group is split, and for a bridge piece to the piece's
  span. Within each process group the engine
  unions only the sources the scope names (S1).
- Both viewers' query APIs take the scope as a request parameter,
  replacing the section's `metadata.scope` and the frontend's `applyScope`
  (`src/viewer/assets/lib/data.js:786`, called at `app.js:1153`; its use of
  `injectLabel`, which also adds the vendor and GPU id selections,
  `data.js:680-700`, and the node, `app.js:1149`).
- A stored query (a report selection entry, whose `promql_query` text
  `selection/selection.js` keeps at lines 147-149, 277, 529, 700 and
  931-946; a view document) carries its scope. Within an archive the scope
  names sources by uuid; across archives by kind and labels (Views,
  "References to sources"). A stored entry without a scope is read as
  written, with the matchers spliced into its text.
- `restrict_query`'s rule that a selector's own `k="v"` replaces the scope's
  matcher on `k` is removed. Under the template evaluation rule (Templates),
  the pin rule fails: a federated Prometheus template selecting
  `{instance="api:9090"}` keeps its own `instance` matcher in place of the
  scope's, so when `instance` tells the recordings apart, the query sums
  every recording's `instance="api:9090"` series.
- MCP's `recording` selector opens one presented recording, so it needs no
  scope. Without a selector the per-recording tools iterate the presented
  recordings, and `query` refuses an archive with arms or time clusters,
  else reads each process group once (P7). `recording check` needs a binding
  rather than a scope (Templates).

### Presentation: how the default is chosen

The `dashboard` crate computes the presentation in this order, for every
consumer. A saved view document replaces steps 3 to 6 with what it records;
the steps still run so the consumer can say where the saved view and the
evidence disagree.

1. Find each source's kind and `service_kind` (data model; unidentified
   sources count as one kind, P9).
2. Take sources sharing a uuid as one, set bridges aside (over the whole
   archive when no source carries `arm`, else within each arm; never across
   arms), form process groups and
   presented recordings, then slice the bridges (data model, P6; Q39).
3. Navigation: one top-level group per kind when there are two or more
   kinds (case 2); within a kind, one sub-group per `service_kind` value
   when it has two or more; a flat list otherwise.
4. Grouping inside charts, in two levels (P1, case 4), each chosen over the
   whole archive, every kind included. Level 1: `arm` when any source
   carries it; else the time clusters (P6: bridges removed, clusters formed,
   bridges sliced) when there are two or more. Level 2: time clusters, when
   level 1 is `arm` and one arm's members fall in two or more clusters; else
   `tier` when any source carries it; else exactly one asserted label, other
   than `alias`, `instance`, `host`, `service_kind` and the level-1 label,
   whose values differ among the presented recordings, that each of them
   carries, and that has fewer values than there are presented recordings;
   else `host`, a recording without `host` taking `instance` (as P9). Within
   each level-1 group, each presented recording that lacks the level-2 key
   is drawn as its own line, named as in 4e (A8).
5. Alignment, chosen after grouping: elapsed when the groups of the
   innermost comparing level (arm or time cluster) do not overlap, whichever
   rule chose them; absolute otherwise. Under A9 the time clusters are
   formed within each arm, and each level-2 cluster is mapped from its own
   start.
6. Members within a group are aggregated by plot type (P3).
7. One notice names the rules that picked the default (for example
   "compared by time, then grouped by `tier`") and the alternatives, each
   one action away. A flat presentation of one presented recording gets no
   notice.

Edge cases of the steps: an archive with zero sources opens with a notice
and no sections, and MCP's `describe-recording` returns an empty list. A
saved view whose grouping label no source carries is replaced by the
evidence default, with a notice naming the label.

### Case 1: one producer

- **Input.** An archive or file holding one presented recording: one
  rezolus recording, one llm-perf recording, or any one producer's,
  including overlapping copies of one source (case 3 reduces them to one)
  and one source that spans agent restarts (W6).
- **Default.** The template stack for its kind (P5, Templates): the
  registry fallback, the recorder default, the producer's layer carried in
  the file (rezolus's `dashboard` handshake template, a producer's
  `service_queries`), and any user layers the archive holds, the last
  winning. With no template at all, the generic catalog view (today's
  `/source/<name>` entry). Flat navigation.
- **Alternatives.** The registry's template in place of the producer's (a
  user layer hiding both template parts beneath, D4; today's
  `--registry-templates`); any saved view.
- **Saved.** A template layer document when the user edits the template; a
  view document when the user changes layout or alignment.
- **Edge cases.** A carried `service_queries` that does not parse: the
  registry's template with a notice, as today (`source_templates.rs`). A
  Prometheus source with no `alias` (a legacy one's inferred `source` reads as
  `alias`, D2): the registry lookup usually finds nothing, and the generic view
  is shown. A source with several epochs: one presented recording, with a
  counter reset at each epoch's `from_ts` (S9).
- **Test.** Fixture: one `.dendro` from `rezolus record` of one agent that
  restarted once. Expected: flat navigation holding rezolus's sections; no
  groups; one series per chart as today; absolute alignment; no
  presentation notice; `rate(cpu_cycles[1m])` has no spike at the restart.
  Fixture: one llm-perf `.dendro` holding only llm-perf's own source.
  Expected: flat navigation holding llm-perf's KPI sections, and its
  cross-producer section hidden with the notice "section X needs rezolus:
  no rezolus recording" (case 2).

### Case 2: different producers recorded together

- **Input.** Presented recordings of two or more kinds, for example llm-perf
  and a vLLM server (in llm-perf's own `.dendro`) combined with a `rezolus
  record` of the agents (W1).
- **Default.** The left navigation has one group per kind, named by the
  kind. Each group holds that kind's sections from its template stack. The
  query explorer renders once, outside the groups. The groups are entries
  in the navigation tree; the kinds do not get panes of their own side by
  side. A kind with two or more `service_kind` values (vLLM prefill and
  decode) gets one sub-group per value. Unidentified sources form one
  group (P9). Presented recordings of one kind are grouped inside the
  charts by case 4.
- **Cross-producer sections.** A template may declare sections that need
  another kind present: llm-perf can ship a section that plots the server
  host's CPU from rezolus beside its own request latency.
  - The section names the other producer by kind (`__producer__`, refined
    by `service_kind` where it says so), never by source uuid, so the
    template applies to any recording (principle 4).
  - The section declares its binding, for example "kind `rezolus`, on the
    host of the recording with `tier=server`". Today `role=` on a rezolus
    `--endpoint` is metadata (`src/recorder/mod.rs:1189-1194`) and only
    llm-perf writes `role` as a label (llm-perf `src/recording.rs:128`).
    rezolus writes `source`, `arm` (#1504) and `host` (#1538) per endpoint
    as labels; a binding on another label set per endpoint at record time
    needs step 3.4's per-endpoint labels for any key.
  - Host matching: the declaring recording's host is its `host` label
    (producer-declared, or asserted for a producer that declares none). A
    name matches a rezolus source whose `host`, lowercased with the domain
    stripped, equals it lowercased with the domain stripped. An IP
    literal is compared whole, so it matches only a rezolus source whose
    `host` was asserted as the same IP; rezolus reports a hostname, and the
    notice says to assert one. `instance` takes no part (Q1): through a
    port forward or a proxy, the address is the recorder's view and names
    no machine.
  - Under a grouping the section binds within its level-1 group, then
    within shared context (A1).
  - No match: the section is hidden, with the notice "section X needs
    rezolus on <host>: no match among <list>". The binding recording
    carries no `host`: the section is hidden, with the notice "section X
    needs the host of the recording with `tier=server`; set `host=` on its
    endpoint". No recording carries the binding label: the section is
    hidden, with the notice "section X needs a recording with
    `tier=server`". Two recordings carry it: one copy of
    the section per such recording, each titled by its host, with a notice.
  - Each query in the section carries an engine scope (D1) naming the
    presented recording it binds to.
  - The section appears in the declaring producer's navigation group.
  - The declaration's format is part of the template schema (Q13).
- **Alternatives.** A flat list (today's layout); host as the top level,
  with kinds under each host.
- **Saved.** The navigation arrangement, as part of the view's layout.
- **Edge cases.** The required kind absent, the binding label absent, no
  host match, and two declaring recordings are covered above. A
  cross-producer section under time clusters binds per cluster.
- **Test.** Fixture: llm-perf's own `.dendro`, holding llm-perf's source and a
  vLLM source from `http://s1.lab.example:8000` with `tier=server`,
  `host=s1.lab.example` asserted at record time, and `alias=vllm` (llm-perf's
  `[metrics] server_source = "vllm"`, written as a `source` label today,
  llm-perf `src/recording.rs:128-131`, and as `alias` after the change; its
  kind comes from the registry match of `alias`, Q35), and llm-perf's own
  source with `tier=client` (Q7), combined with one `rezolus record` of the
  agents on `c1` and `s1.lab.example`. Expected: navigation groups `rezolus`,
  `llm-perf`, `vllm`; in the `rezolus` group one line per host on each chart,
  since the agents carry no `tier` (A8); in the `llm-perf` group the server-CPU
  section reading only the `s1` agent's rows; absolute alignment; notices "3
  producers: rezolus, llm-perf, vllm" and "grouped by `tier`; c1 and
  s1.lab.example carry no `tier`". Variants of the same fixture, each with its
  expected result:
  - The vLLM `instance` is `http://localhost:8000` through a port forward,
    with `host=s1.lab.example` asserted: the section binds to `s1`.
  - The vLLM source carries no `host`: the section is hidden with "section
    server CPU needs the host of the recording with `tier=server`; set
    `host=` on its endpoint", even when its `instance` names `s1`.
  - No agent on the server host: the section is hidden with "section
    server CPU needs rezolus on s1: no match among c1".
  - Two vLLM sources with `tier=server`, asserted as `host=s1` and
    `host=s2`, and agents on both: two copies of the section, titled `s1`
    and `s2`, and a notice; in the `vllm` group one line, `server`, whose
    split toggle draws two, by `host`; in the `rezolus` group still one
    series per host, since the agents carry no `tier` (A8).
  - The whole fixture recorded on two days and combined: two level-1
    groups (time clusters), each holding all three kinds; each day's
    server-CPU section reads only that day's `s1` agent.

### Case 3: the same producer process twice

- **Input.** Two or more sources in one process group: overlapping copies
  (W7), trimmed or filtered copies, hindsight dumps of one buffer (W5;
  these share a uuid, so disjoint dumps join too), a colleague's copy, two
  recorders of one agent running at the same time at different intervals,
  and a systemslab job's automatic recording beside a recording started by
  a separate record invocation against the same agent.
- **Default.** One presented recording: the group's sources, each
  evaluated alone and combined per span, the finest source's results in each
  span (S1 to S9), with the group's labels (data model). The notice names
  the sources unioned and why.
- **Alternatives.** Split by source: each source becomes its own presented
  recording, compared with absolute or elapsed alignment. A query whose
  scope names several of the split sources still reads their process group
  once (S1), so splitting changes what is drawn per group and never doubles
  an aggregate.
- **Saved.** The split, as a view document naming the sources by uuid;
  copies that share a uuid (hindsight dumps) are named by uuid and time span
  (A2). A split names this archive's sources, so it does not transfer to
  another archive, and the export says so (Views).
- **Edge cases.**
  - Different time resolutions: in each span the finest source's results,
    and a coarser source's where the finest has none or its rows do not
    cover the whole window (S3, S9).
  - Both sources hold a read at one timestamp with one acquisition window
    and the values differ: a conflict (S4).
  - A filtered copy holds fewer series: the union is per series, so the
    series it lacks come from the other source and nothing conflicts.
  - Epoch chains: source A holds epochs e1 then e2 (a restart, W6), source
    B holds e2 only, overlapping A in e2 (a dump taken after the restart).
    They form one process group through e2; the e1 span comes from A alone,
    and the union has a counter reset at e2's `from_ts` (S9).
  - Same epoch, spans that do not overlap, different uuids (a daemon agent
    that outlived two `record` invocations): two presented recordings, case
    4 (O2). The notice offers "treat as one recording".
  - Same epoch, spans that touch or overlap without an intersection holding
    a row of each (R3): not unioned, and warned about: "2 recordings share
    producer epoch … but do not overlap by a row; shown separately, an
    aggregate over both may count one process twice", with "treat as one
    recording" offered.
  - A continuous recording of the agent (one epoch) beside separate
    recordings of two disjoint attempts (W4): the union relation alone would
    join all three through the continuous one. Bridges are set aside first
    (P6), so the continuous recording is a bridge, the attempts form two
    process groups and two clusters, and the bridge is sliced: in each
    cluster its piece joins that attempt's process group by the union rule,
    and the piece between attempts is not drawn. The attempts stay separate,
    compared by time cluster (4c); "treat as one recording" is offered.
  - Two sources of one process group carry different values of an asserted
    label (a user-layer label; one recorder run with `--label arm=a` and one
    with `arm=b`): the group is split, because the user asserted the
    difference, and each part is a presented recording in its own group. The
    notice says that one process appears in two groups. Within each part S1
    still holds. A copy that lacks the label (an annotated or filtered copy)
    takes its original's value and is not split off.
- **Test.** Fixture: a `.dendro` recorded from agent A, and a hindsight dump
  of A's buffer covering part of it (different uuids, one epoch), combined.
  Expected: flat navigation; one presented recording;
  `sum(rate(cpu_cycles[1m]))` equals the record's alone; notice "2 sources
  unioned: they share producer epoch … over overlapping spans". Fixture: two
  dumps of one hindsight buffer covering 10:00–10:05 and 10:20–10:25.
  Expected: one presented recording with a gap from 10:05 to 10:20; notice
  "2 sources unioned: they share uuid …". Fixture: two `rezolus record`
  invocations against one agent at the same time, at 1 s and 100 ms.
  Expected: one presented recording; `irate(cpu_cycles[5s])` at step 0.1
  returns 100 ms points over the 100 ms recording's span. Fixture: one agent
  (epoch e) recorded continuously 09:00–18:00 and by two separate
  invocations 09:00–10:00 and 17:00–18:00. Expected: two presented
  recordings, {09:00–10:00 attempt with the continuous recording's
  09:00–10:00}, {17:00–18:00 attempt with its 17:00–18:00}, compared with
  elapsed alignment; `sum(rate(cpu_cycles[1m]))` at 09:30 equals the
  attempt's alone; the notice names the 10:00–17:00 left out. Fixture: dumps
  D1 10:00–10:05 and D2 10:20–10:25 of one hindsight buffer, and a copy L of
  the live buffer 09:50–10:30, all one uuid. Expected: one uuid group, which
  is not a bridge (no other source exists for it to bridge), so one
  presented recording with all of L's rows from 09:50 to 10:30, nothing
  truncated. Variant: add a `record` R of the same agent 09:00–11:00
  (another uuid, one epoch). Expected: the uuid group {D1, D2, L} and R are
  two uuid groups; neither is a bridge (each overlaps only the other); they
  share the epoch and overlap, so they form one process group and one
  presented recording from 09:00 to 11:00, with the finest source's
  results per span (S3, S9).

### Case 4: different processes of the same software

- **Input.** Two or more presented recordings of one kind, from different
  producer processes, or one process seen over disjoint spans (O2): two
  rezolus agents (W1's hosts, W2, W3, W4), a fleet.
- **Default.** Two grouping levels (P1), the first rule that applies at
  each:
  - **4a.** A saved view applies: use it.
  - **4b (level 1).** Some sources carry `arm`: compare by `arm`, one group
    per value, with a reference group and the diff views. Sources without
    `arm` are shared context, shown once (today's convention from #1504).
  - **4c (level 1).** No `arm`: time clusters over every kind (P6, A1).
    Bridges are set aside; the remaining presented recordings are clustered
    by transitively overlapping spans; each bridge is sliced at the
    clusters' spans, and its pieces join within each cluster (P6). The
    pieces between clusters are not drawn, and the notice names the bridge
    and the time left out. Two or more clusters are compared like `arm`
    (P4), with elapsed alignment between them (W2, back-to-back
    experiments).
  - **4d (level 2),** chosen over the whole archive and applied within each
    level-1 group: time clusters, when level 1 is `arm` and one arm's
    members fall in two or more clusters (A9); else `tier` when some sources
    carry it, one series per tier on each chart, members aggregated (P3), no
    comparison, and each presented recording without `tier` drawn as its
    own line, named as in 4e (A8); else exactly one asserted label, other than
    `alias`, `instance`, `host`, `service_kind` and the level-1 label, whose
    values differ among the presented recordings, that each of them carries,
    and that has fewer values than there are presented recordings: group by
    it the same way. When two or more labels qualify, none is chosen and
    each is offered.
  - **4e (level 2).** Else one series per `host`, a recording without `host`
    taking `instance` (as P9). A host holding several presented recordings
    gets 4e again within it, with `instance` in place of `host`; when
    `instance` is equal or missing too, one series per presented recording
    in catalog order, with the notice "could not tell them apart".
  - Charts draw one line per level-1 × level-2 pair.
- **Alignment.** Chosen after grouping: elapsed when the groups of the
  innermost comparing level (arm or time cluster) do not overlap, whichever
  rule chose them; absolute otherwise. Under A9 the clusters are formed
  within each arm and each is mapped from its own start. With three or more
  level-1 groups whose spans overlap pairwise in part, Q26 decides; until
  then the proposal is absolute time with a notice naming the pairs that do
  not overlap. Test: three arms at 09:00–11:00, 10:00–12:00 and 11:30–13:00
  open at absolute time with "arms 1 and 3 do not overlap".
- **Alternatives.** Every rule that did not apply is offered: compare by
  `arm` or by time cluster, group by `tier`, by each qualifying label, by
  host, or by presented recording, split a group into its members, and
  "treat as one recording" for presented recordings of one epoch. The
  choice is one action away.
- **Saved.** The grouping rule and alignment as a view document; a tier the
  user assigned to express it as user-layer labels on the sources (P1), as is
  any other label the user assigned to those recordings (for example `arm`,
  Q40); a rule meant for other archives as a label layer document.
- **Edge cases.**
  - More than two level-1 groups: every group is drawn against the
    reference (Q24 for which). The views that compare two (heatmap diff,
    side-by-side heatmaps, a percentile chart's Full/Tail spectrum) are
    offered only when there are exactly two. Statistics across many arms
    are systemslab's (Scope).
  - One group only (one armed recording among unarmed ones, or every
    source on one arm): the archive opens with that group and shared
    context, and a notice. Today's `arm_slots` refusals of these shapes are
    removed.
  - Kinds mixed with `arm`: `arm` applies across every kind, so arm `a`'s
    group holds arm `a`'s rezolus and llm-perf recordings; kinds whose
    sources do not carry it are shared context. Time clusters form across
    every kind (A1).
  - Selectors: `--recording tier=server` matching three presented
    recordings is refused with a listing, as `--recording` refuses several
    today; `--recording` selects one presented recording.
- **Test.** Each fixture uses synthetic agents whose counter
  `test_requests` grows at a constant rate and whose gauge `test_util` is
  constant, so every expected value is exact.
  - W3, `--endpoint a,arm=a --endpoint b,arm=b`, two agents at 10/s and
    20/s. Expected: flat navigation; on the `test_requests` chart two
    lines, `a` at 10/s the reference and `b` at 20/s; diff views offered;
    absolute alignment; notice "compared by `arm`: the sources carry 2
    values".
  - Three server agents at 10/s, 20/s and 30/s and one client at 5/s,
    `tier` given per endpoint (`--endpoint url,tier=server`, step 3.4) or
    by separate invocations (`--label` is invocation-wide). Expected: on
    `test_requests` two lines, `server` at 60/s and `client` at 5/s; the
    split toggle draws four; no diff view; notice "grouped by `tier`".
  - The same servers with `test_util` at 40%, 50% and 60%, the template
    declaring a mean: `server` at 50%. Undeclared with unit percent: 50%
    (A6). Undeclared with no unit: three lines and the notice "no
    aggregation declared".
  - The same servers with one histogram each, one of them at a single
    bucket: the `server` percentile chart is computed from the merged
    buckets, and its p99 equals the p99 of the merged distribution. Two
    members with grouping powers 7 and 4: merged at grouping power 4, with
    a notice (A7).
  - The 30/s server without `test_requests`: `server` at 30/s from the
    other two, legend "2 of 3 members" at every step where it is missing.
  - A per-GPU heatmap (rows by `id`) over the three servers: rows keyed by
    (member, `id`), nothing summed across members. `topk(1, …)` on the
    `server` line picks from the aggregated series (A5).
  - Three agents with `tier=server` and one with no `tier`: two lines,
    `server` and the fourth agent's host, and a notice naming that
    recording (A8). Variant: two of the four agents without `tier`: three
    lines, `server` and each one's host. The same archive after the fourth
    agent is given `tier=server` in the viewer and saved: one line, and
    the fourth source's `metriken.layer.user` holds `tier: server` in its
    `labels`.
  - A bridge: agent A recording 09:00–18:00, agent B 09:00–10:00 and agent
    C 17:00–18:00, on one kind, no labels. Expected: two clusters,
    {B, A 09:00–10:00} and {C, A 17:00–18:00}, compared with elapsed
    alignment; A between 10:00 and 17:00 is not drawn, and the notice says
    "A spans both clusters; 10:00–17:00 not shown".
  - Two bridges: agents D1 and D2 recording 09:00–18:00, B 09:00–10:00 and C
    17:00–18:00. Expected: D1 and D2 are both bridges (each contains B and
    C, which do not overlap; D1 and D2 overlap each other); clusters {B, D1
    and D2 09:00–10:00} and {C, D1 and D2 17:00–18:00}; elapsed alignment;
    the notice names both bridges; D1's and D2's pieces are two lines in
    each cluster. Variant: D1 and D2 record one agent (one epoch): in each
    cluster their pieces union, so the agent is one line.
  - A chain: X 09:00–11:00, Y 10:00–12:00, Z 11:30–13:00, one kind, no
    labels. Expected: Y is a bridge; clusters {X, Y 10:00–11:00} and {Z, Y
    11:30–12:00}; Y between 11:00 and 11:30 is not drawn.
  - W2 with arms on both days: `arm=a` and `arm=b` agents on Monday and
    again on Tuesday, combined. Expected: level 1 `arm`; each arm's members
    fall in two clusters, so time clusters are level 2 within each arm (A9);
    four lines (arm × day), each day mapped from its own start; notice
    "compared by `arm`, then by time".
  - Arms × a parameter: `arm=a` and `arm=b`, each run at `concurrency=1` and
    `concurrency=2`, overlapping. Expected: level 1 `arm`, level 2
    `concurrency` (`arm` is excluded as the level-1 label); four lines;
    absolute alignment.
  - One daemon agent recorded by two `rezolus record` invocations on two
    days, no labels, combined. Expected: two lines compared, elapsed
    alignment; notice "compared by time: the recordings do not overlap",
    with "treat as one recording" offered.
  - Two days, each with a client and a server agent carrying `tier`.
    Expected: four lines on each chart (day × tier), elapsed alignment, a
    reference per tier; notice "compared by time, then grouped by `tier`".
  - Four overlapping agents on hosts `h1` to `h4`, no labels. Expected:
    four lines, absolute alignment, notice "grouped by host".

### Case 5: sources without a producer epoch

- **Input.** Prometheus scrapes, recordings from before v5.21.0, a parquet
  without the key. Each such source is its own process group unless it
  shares a uuid with another (a dendro copy).
- **Default.** Presented by cases 1, 2 and 4 as separate process groups. Two
  epochless sources of one kind, with overlapping spans, that agree on every
  recorder-layer key both carry (at least one) get a warning that they may
  be one target recorded twice and that an aggregate over both counts it
  twice. For a Prometheus scrape the keys compared are the kind and
  `instance`. A `.rez` written by v5.18.0 or later carries `host`
  (`build_rez_labels`, v5.18.0 `src/recorder/mod.rs:565`). A parquet written
  by v5.21.0 to v5.28.0 carries the scraped URL as `endpoint` (v5.28.0
  `src/recorder/mod.rs:878`), read as the recorder-layer `instance`; one
  written before v5.21.0 carries neither (its hostname is inside
  `systeminfo`; `node` is written by `annotate`, v5.28.0
  `src/parquet_tools/annotate.rs:767-769`), so two copies of such a parquet
  get no warning (Q15).
- **Alternatives.** Treat the warned sources as one process (case 3
  semantics); hide one.
- **Saved.** "Treat as one" or "hide", as a view document naming the
  sources by uuid; it does not transfer.
- **Edge cases.** Never refused. A federated scrape whose series carry
  their own `instance`: the warning compares the recorder-layer `instance`
  only.
- **Test.** Fixture: two `.dendro` files from two `rezolus record` invocations
  scraping one Prometheus endpoint at the same time, combined. Expected: one
  navigation group; one line per presented recording with 4e's notice "could
  not tell them apart"; and the notice "2 recordings may be one target recorded
  twice (`instance=…`); an aggregate over both counts it twice", with "treat as
  one" offered. Fixture: two scrapes of one unmatched Prometheus endpoint, both
  legacy recordings whose inferred `source` values are equal and read as
  `alias` (D2). Expected: the "unidentified" group with two lines (`alias` is
  never a grouping key; level 2 falls to `host`, then `instance`) and the same
  warning (A3).

### Case 6: inputs without documents

- **Input.** A `.rez`, a parquet, a v4 `.dendro`, or a v5 `.dendro` with no
  view document.
- **Default.** Cases 1 to 5 from what the input carries: epochs where
  present (v5.21.0 and later), `arm` and other labels, the D2 read of
  `source`. No separate mode exists for old inputs.
- **Upgrade.** `recording upgrade --to dendro` writes a v5 archive and may
  write a view document into it (from flags or a view file), so the
  upgraded archive opens with that view. Without one, the default applies.
- **Saved.** A view of an input that cannot hold documents is exported to a
  file, or saved by converting the input to `.dendro` (Q27).
- **Edge cases.** Every archive that opens today still opens. This accepts
  archives main refuses since #1504 (in no tag): the three `arm_slots`
  shapes (case 4, one group or more than two arms).
- **Test.** Fixture: a v5.28.0 `.rez` with two recordings, `arm=a` and
  `arm=b`. Expected: today's comparison. Fixture: a v5.28.0 `.rez` with
  three recordings on `arm=a`, from three agents (three epochs), covering
  10:00–10:10, 10:00–10:10 and 11:00–11:10. Expected: one arm whose members
  fall in two time clusters, so time clusters become level 2 (A9): two lines
  under `arm=a`, {the two 10:00 recordings, aggregated} and {the 11:00
  recording}, each mapped from its own start; the notices "one arm: every
  recording carries `arm=a`" and "arm `a` spans 2 time clusters; compared by
  time within it" (refused today).

### Query rules

Each rule holds for every consumer (viewer, static site, MCP, `recording
check`, systemslab through the `dashboard` crate).

- **S1. One process counts once.** A query reads each process group's rows
  at most once, however many of its sources the scope names. Test: an
  archive holding a source and its dendro copy; `sum(rate(cpu_cycles[1m]))`
  equals the value for the source alone. Today it is twice that (Today).
- **S2. Union per series.** Within a process group, a series is identified
  by its own labels and the group's labels; members of one group are not
  told apart by `__recording__`. Test: two copies of one source with equal
  label sets; `count(cpu_cycles)` equals the count for one copy.
- **S3. Finest source per span.** For each series, the time axis is cut
  into spans in which the set of sources holding that series is constant. A
  source has no row at `t` when its rows on either side of `t` are further
  apart than twice the larger of the median of its gaps before that pair and
  the median of its gaps after it, each over up to eight gaps; with none on
  one side, the other side's median is used. Beyond a read, a source's rows
  count only as far as the nearest nine on each side within 64 of its
  intervals: a series with fewer there is taken as having no more rows on
  that side (implemented 2026-10-21; reading until nine rows, however far,
  cost a 1 h query four times as much on a recording with many ended
  series). In each span the results come
  from the source with the finest spacing there (S9), ties going to the
  lowest uuid, then to catalog order; where it has no result, or its rows
  do not cover the whole window (S9), the next finest's is used. Test: a
  1 s source covering 10:00 to 10:10 and a 100 ms source covering 10:04 to
  10:05; `rate(c[10s])` at a 100 ms step returns the 100 ms source's results
  from 10:04:10 (the first point whose window starts at or after its first
  row) to 10:05, and the 1 s source's elsewhere. Test: a 1 s source with
  one 5 s gap; the gap is a hole (5 s against a 2 s threshold).
- **S4. Conflicts are reported.** Two sources of one group holding a read
  of one series at one timestamp with one acquisition window must hold one
  value. When they differ, the query returns the finer source's value; if
  both have the same interval, the value of the source with the earlier
  first timestamp; further ties by S3's rule (Q16, decided 2026-10-21). The
  consumer reports a conflict naming both sources, the series and the
  first timestamp. Two stream recorders of one
  rezolus agent share reads: ticks of two subscriptions that fall within
  three quarters of one pass's TTL are answered from that pass
  (`src/agent/exposition/http/mod.rs:194-197` at rezolus upstream/main
  7add8f06, read from the comment), so
  equal stamps carry equal values. The skew case is scraped
  sources, whose rows interleave. Test: a copy with one value edited; the
  chart renders, and one conflict notice names the edited timestamp.
- **S5. Gaps stay gaps.** A span no member covers, by S3's threshold, has
  no rows; nothing is interpolated across it.
- **S6. Counter decreases.** A decrease within one source's own rows is a
  counter reset, as today: the epoch says nothing about a counter that
  wrapped or that a sampler zeroes on read (rezolus `CLAUDE.md`,
  `producer_epoch`). A decrease between two sources' values at a junction
  never enters a value (S9); it is compared only as a diagnostic, and a
  notice is given when the epoch history does not explain it.
- **S7. Windows travel with rows.** Each row keeps its own acquisition
  window, and a rate's band uses the windows of the pair of rows that
  supplied the increase.
- **S8. Groups of several process groups.** A group of several process
  groups is aggregated across them by plot type (P3), each process group
  counted once by S1.

#### Query engine support (measured)

Measured on 2026-10-20 at metriken upstream/main 451ef1f (metriken-query
0.41.0, metriken-storage 0.4.0) with an uncommitted test-only probe in
metriken-query, one run per case. The fixture is one series with rows every
1 s from 1000 to 1059 s, every 100 ms from 1060.0 to 1119.9 s, and every
1 s from 1120 to 1180 s; counter `c` grows at 10/s, counter `c2` at 10/s
then 30/s inside the fine span then 10/s, and gauge `g` records which row
was read. Windows are 1 ms either side of each stamp; queries cover 1010 to
1170 s at steps of 1 s and 0.1 s; the reference for `rate(x[r])` is
computed from the true counter, with a tolerance of 1e-6.

How the engine reads cadence today, from the code:

- A series holds rows at any spacing, and nothing rounds them.
- Each table gets one interval hint (a `MemoryStore`, one for the store):
  the builder's value for a `MemoryStore`; at catalog open, the span
  divided by (rows − 1), rounded to whole ms (metriken-storage
  `reader.rs:1394-1402`, the division at `:1396`); from segment bytes, the
  segment's declared interval, else the median gap of the first segments
  holding two rows between them (`reader.rs:1535-1561`), applied by `timed`
  (metriken-query `archive.rs:463`).
- Three bounds derive from that hint: the rate reach of twice the interval
  (`interval_reach`, `batch_rate.rs:414`; `range_start`,
  `promql/streaming/rate.rs:149`), gauge staleness of `max(step,
  interval)` (`promql/streaming/dispatch.rs:1177`), and raw irate's maximum
  gap (`dispatch.rs:929`, four intervals per the comment at `:847`). The
  rate band's hole test uses the median of the first eight gaps (nine
  samples, `SPACING_PROBE`, `rate.rs:243`; `sampled_window`, `rate.rs:420`;
  the test itself at `batch_rate.rs:355` on the batch path and `rate.rs:435`
  on the streaming path).
- Cross-cadence evaluation (`cross_cadence`, `archive.rs:210-273`) takes
  each table's median gap, keeps the densest table per sampler, and
  compares samplers (`:238-252`); it has no notion of a table whose cadence
  changes.

Single series, one source, no overlap:

- With the hint at the coarse 1 s, `irate`, `rate[10s]`, `rate[60s]`, the
  gauge and raw mode are exact at both steps, every point present, with
  bands on every rate point. A constant-rate counter is exact for any hint.
  Equal duplicate rows are not counted twice.
- With a fine hint (100 ms or 250 ms) rate values are wrong and gauge points
  are dropped. At a 100 ms hint, `rate(c2[10s])` at step 0.1 is wrong at 70
  of 140 points near the 1 s to 100 ms transition (30.0 against 28.6, 4.9%,
  and the band excludes the true value); `rate(c2[60s])` is wrong at 322 of
  460 points in the fine span (29.33 against 29.10 at most). At a 250 ms
  hint (what the catalog-open measurement gives for this fixture, 180 s over
  720 gaps, 721 rows; declared in the probe, the catalog-open path not run),
  `rate(c2[10s])` is wrong at 40 of 140 points (30.0 against 29.2).
- The gauge drops points in the coarse spans at a fine hint: 96 of 480 and
  77 of 381 kept at 100 ms, 144 of 480 and 115 of 381 at 250 ms.
- Raw `irate` at a 100 ms hint gives no value for any coarse pair (0 of 48
  and 0 of 39).
- A series whose first nine samples are fine and whose later span is
  coarse: `irate` has no band at 460 of 460 points of the later coarse
  span, and `rate` at 414 of 460, at hints of 1 s and 100 ms.
- `avg_over_time(g[5s])` is the mean over samples, as in Prometheus, so
  fine rows weigh ten times as much as coarse ones; against a time-weighted
  mean it is off by up to 18.5 at 1061.1 s.
- Unstrided histogram functions emit one point per row and ignore the step:
  `histogram_quantile` gives correct values at 701 points; unstrided
  `histogram_count` reads 110 per coarse row and 11 per fine row, so the
  series drops tenfold across the fine span. The strided forms
  (`histogram_count(h, 1)`, `histogram_quantiles([0.95], h, 1)`) give 161
  points, all correct.
- `increase`, `delta` and `max_over_time` are unsupported, and
  `histogram_quantile(q, rate(h[..]))` does not parse.

Cross-cadence evaluation: a table that is fine in the middle (median gap
100 ms) beside a steady 1 s table is evaluated at the 1 s table's 181 rows
at step 0.1, so the fine span is read at 1 s only (60 points). The values
are correct and the resolution is lost.

A union of two sources of one counter growing 1000/s:

- Equal values at equal timestamps, and interleaved rows (offset 50 ms)
  with exact values, are exact for every query.
- Two recorders read one counter at slightly different moments, so their
  values differ by acquisition skew, and the engine takes a decrease between
  rows of the two sources as a counter reset. With equal stamps, the coarse
  source's value 20 ms stale and the fine row first, `rate(c[10s])` reads
  per-region maxima from 67,500 to 115,500 at step 0.1 and from 60,402 to
  104,500 at step 1. In raw mode the second row at one timestamp replaces
  the previous row before the equal-timestamp skip
  (`CounterPairwiseRate::next`, `rate.rs:84-105`: the replace at `:91`, the
  skip at `:95`). With rows interleaved 50 ms apart and the coarse source 80
  ms stale, `irate` at step 0.1 reads up to 1,199,700 and raw `irate` up to
  2.4e6.
- Histograms clamp a per-bucket decrease to zero (`accumulate_delta_into`,
  `promql/streaming/histogram.rs:544-546`, through `delta_walk`, `:550-576`,
  the clamp at `:571`), which overcounts by about 1.4% in the interleaved
  case (`histogram_count` sum 17,950 against 17,710, 161 strided points of
  110) rather than spiking. Its comment says this mirrors counter
  semantics; counters treat the decrease as a reset instead.
- Keeping only the fine rows inside the fine span makes the interior exact,
  and both junctions stay wrong: near 1060 s `irate` reads 1080 (at 1061.0
  s, step 1) and 1800 (at 1060.1 s, step 0.1), and at 1120 s, where the
  first coarse row is below the last fine row, `irate` reads 120,870 at
  step 1 and 1,199,700 at step 0.1 (`rate[10s]` 12,987). Choosing the
  finest rows per span is not enough alone, because each junction still
  compares values from two recorders; S9 evaluates each source alone for
  this reason.

The viewer's charts: a percentile or heatmap chart builds the unstrided
`histogram_quantiles(…)` or `histogram_heatmap(…)` unless a Granularity
override coarser than the native interval is set (rezolus
`src/viewer/assets/lib/data.js:1257-1268`; `buildHistogramQuery`,
`charts/metric_types.js:85`). No rezolus template uses `avg_over_time`.
The templates' `histogram_mean`, `histogram_sum` and `histogram_irate` were
not measured.

The rules that follow: S9 is a prerequisite for case 3; S10 and S11 are
not needed while each source keeps one cadence; S12 is done.

- **S9. Per-source evaluation, combined per span.** Decided 2026-10-21. A
  union of sources of one process group is not one series to the engine.
  Each source is evaluated alone, with its own cadence (its own rows,
  interval and acquisition windows, as one recording is evaluated today), up
  to the per-series function: `rate`, `irate`, a histogram's per-row or
  per-stride delta. The results are combined per series per span (S3's
  spans), and aggregation across series and across process groups (S8)
  applies after the combination: `sum(rate(x[1m]))` sums the combined
  series, and `histogram_quantiles` across hosts merges the combined deltas.
  Where only one source covers a span, its results are used. In a span both
  cover, the finer source's result at `t` is used where its rows cover the
  whole window, its start at or after the source's first row and `t` within
  one interval of its last row, and the other source's result elsewhere, so
  a point is never answered from part of a window. The window is the
  function's: `[t − r, t]` for `rate(x[r])`, `[t − step, t]` for `irate` on
  the grid, the staleness window for a gauge (`[t − s, t]`, `s` the step or
  the interval when that is longer), and for an unstrided histogram function
  the pair of rows its per-row delta is taken between. No value is a
  difference between a row of one source and a row of another; at a junction
  the incoming source's own pair of rows supplies the increase, which its
  own evaluation already holds. No pair of rows spans two sources, so every
  counter reset is within one source's rows. At each epoch's `from_ts` a
  series has a counter reset, whether or not its values decrease there. This
  keeps the engine's assumption of one cadence per series: three bounds (the
  rate reach, gauge staleness and raw `irate`'s maximum gap) come from the
  table's interval and the band's hole test from the series' first nine
  samples (`SPACING_PROBE`, `promql/streaming/rate.rs`); with one cadence
  per source both are that cadence. Test: the interleaved fixture (a coarse
  source with rows every 1 s from 1000 to 1180 s, read 80 ms before its
  stamps; a fine source with rows every 100 ms from 1060.05 to 1119.95 s,
  read at its stamps); `irate(c[5s])` at step 0.1 and `rate(c[10s])` read
  1000/s within 1e-3 everywhere, junctions included. Test: the same fixture
  with a counter `c2` growing 1000/s, then 3000/s from 1060 to 1120 s, then
  1000/s, and `rate(c2[10s])` from 1010 to 1170 s at step 0.1. The fine
  source's result is used from 1070.1 s (the first point whose window starts
  at or after its first row, 1060.05 s) to 1119.9 s (its last point: a
  source is held past its last row only at the query's end), and the coarse
  source's elsewhere. The reference at each point is computed from the rows
  of the source used, each row's value the true counter at its stamp less
  that source's staleness (80 ms for the coarse source, none for the fine),
  interpolated at `t − 10 s` and `t`; tolerance 1e-6. At 1065 s the coarse
  rows give `(C(1064.92) − C(1054.92)) / 10` = (14,760 + 5,080) / 10 =
  1984/s, where the true counter's rate is 2000/s; from 1070.1 to 1119.9 s
  the fine rows give 3000/s.
- **S10. Bounds from local spacing.** Not needed while each source keeps one
  cadence. Three bounds come from the table's interval and the band's hole
  test from the series' first nine samples, which is wrong only for a series
  whose rows change spacing (measured above): with a 100 ms interval over a
  table whose rows are also 1 s apart, `rate(c2[10s])` was wrong at 70 of
  140 points near the change, the gauge kept 96 of 480 points in a coarse
  span, and raw `irate` gave no value for any coarse pair; with fine first
  rows, `irate` had no band on any point of the coarse span. Under S9 no
  series the engine evaluates mixes cadences. The branch `fix/local-spacing`
  on brayniac/metriken (44a5b0c, unmerged) took each bound from the spacing
  of the rows near the point, capped by the longest row gap the segments
  record in their footers. It was shelved for its cost (on a 9.6 h archive,
  the batched `rate(x[1m])` path slower and a gauge over 600 s at a 0.1 s
  step slower: the branch's CHANGELOG states 9% and 29% (40 µs); two later
  runs at 44a5b0c measured 6 to 9% and 25 to 27% (30 to 37 µs)) and because
  reviews found that it changed answers: for instant queries (a gauge held
  into a pause and past its own last row, no instant `irate` at the end of a
  table with a pause), for cross-cadence evaluation past a slow table's last
  row, and for a range that crosses between segments, all fixed in 44a5b0c;
  and, found in review of 44a5b0c and not fixed, for a series with too few
  samples to have a spacing of its own (a gauge with one sample in a table
  that paused for 600 s was held for up to 600 s). Reopens if a single
  source changes cadence partway: a recorder whose interval changes during a
  recording, or, plausibly and not verified, a `.dendro` source spanning an
  agent restart after which the agent's snapshot TTL changed (a subscription
  is served at the larger of the requested interval and the snapshot TTL,
  rezolus `src/agent/exposition/http/mod.rs:188-197` at upstream/main
  7add8f06, so a TTL raised above the requested interval changes the rows'
  spacing; whether a restarted agent stays in one source was not checked).
  The tests that belong to it then: one source whose rows go from 100 ms to
  1 s apart at 10:05, with no hole reported after 10:05 (the reason S3 takes
  the larger median), and the coarse-fine-coarse and fine-first fixtures
  with each interval above.
- **S11. Cross-cadence evaluation per span.** Not needed while each source
  keeps one cadence. `cross_cadence` (`archive.rs:210-273`) compares one
  median gap per table, which is wrong only for a table whose cadence
  changes partway, and under S9 there is none. The test first written here
  described the shelved design and had the result the wrong way round: per
  span, the fine-in-the-middle table beside a steady 1 s table at step 0.1
  is evaluated at the steady table's rows (1 s points) in the fine span,
  where the cadences differ tenfold, and on the step's grid (100 ms points)
  where both tables read every second. 44a5b0c on `fix/local-spacing`
  implements and tests that. Reopens with S10.
- **S12. Per-sample and per-row functions are documented.** Done: the
  metriken-query crate docs ("Functions whose values depend on row
  spacing") state that `avg_over_time` is a mean over samples (Prometheus
  semantics, so fine rows weigh more), that an unstrided histogram function
  gives one point per row that holds observations and a strided form one
  point per stride window that holds observations, and that
  a chart uses the strided form with the stride at its step (step 3.5), with
  a doctest of both histogram forms over rows 1 s and then 100 ms apart.

#### S1 to S9 in metriken (counters and gauges, built)

Built on branch `feat/per-source-union` of metriken (metriken-query 0.42.0,
metriken-storage 0.5.0, unreleased), slice 2a of three:

- 2a: S1, S2, S3, S5 to S9 for counters and gauges (`rate`, `irate`, a
  gauge read, `avg_over_time`, `idelta`, `deriv`), with each producer
  epoch's `from_ts` as the start of a new run of rows. Histograms read one
  source of a group (the one covering the most of the query's range, ties
  to the finest), which counts a process once and leaves the other sources'
  spans out.
- 2b: histograms per source (the per-row and per-stride deltas combined per
  series per span).
- 2c: S4: conflict detection, the warnings channel on query results (an
  API change; `QueryResult` has none today), and Q16's value at a conflict.
  Until then the value at a conflict is whichever source S3's order
  chooses, which is the finer source's and, at equal intervals, the lowest
  uuid's rather than the earlier-starting source's.

The leaf design: each leaf (`rate(x[r])`, `irate`, a bare gauge selector,
the windowed gauge functions) over a process group is evaluated by the same
code that evaluates one recording, once per member, through a wrapper that
clips the member to its epoch and records the timestamps of every row it
hands out, and the members' series are combined per label set before the
rest of the expression runs (metriken-query
`promql/streaming/per_source.rs`, called from `dispatch.rs`). The
alternative, a combinator inside each producer (the batched
`batch_rate.rs`, the streaming `rate.rs`, the gauge producers), needs a
source identity on every sample and a separate implementation on each of
three paths; evaluating the leaf per member reuses the one-recording code,
so "evaluated alone, as one recording is evaluated today" holds by
construction. A member that is a whole source reads the batched counter
scan, the wrapper recording each row the scan decodes; a member clipped to
an epoch takes the per-series path, since a scan cannot be clipped. Every
member is read over the whole query range. Reading a later member only over
the grid times the first does not cover was tried and changed answers: a
grid rate interpolates between the rows on either side of its window's
edges, which can be any distance away across a hole, and a gauge read starts
at its range's start, so a narrower read moves points (a 250 ms source with
a hole over [200, 210) s beside a 2 s source, `rate(c[1s])` at a 0.25 s
step: 9360 at 210.25, 210.5 and 210.75 s where the 2 s source alone reads
9380, 9400 and 9420; 134 of 4360 random grid queries differed, and padding
the ranges never brought that to 0). The wrapper also reads, for each
series, the nine rows nearest the read on each side (before its start and
after its end; ten of the metric's own intervals, then 64 when a series
has fewer than nine rows there, a series with fewer than nine rows in 64
intervals being taken as having no more), keeping
their timestamps; the read decodes values too, since the sources have no
timestamp-only read per series. S3's hole test takes the medians of up to
eight gaps on each side of a gap, so without these rows a series looks as
if its rows start at the read's start or end at its end, and the first or
last points of a range depend on where it starts or ends (a 1 s source with
a gap to 203 s and then 5 s rows, beside a 2 s source: `g` at 204 s read
478 to 204 s and 486 to 260 s). `metriken-storage/tests/union_oracle.rs` checks the
union against each source read alone over the whole range and combined
point by point (S3 and S9 applied in the test; random fixtures with holes,
cadence changes, cadences from 250 ms to 15 s and a faster second table).
Measured on the 9.6 h recording with a copy of itself, against the same
archive read twice by metriken-query 0.41.0, medians of four interleaved
rounds: a display query of `sum(irate(cgroup_cpu_usage[5s]))` over 9.6 h at
1 s 194 ms against 60 ms, `rate(cgroup_cpu_usage[1m])` over 1 h 22 ms
against 12 ms, `sum(irate(...))` over 1 h 21 ms against 6.0 ms,
`sum(rate(...[1m]))` over 9.6 h at 60 s 56 ms against 36 ms. (Reading the
context rows until each series had nine, however far, measured as one
ten-interval read and then a read to the member's end, took the 1 h `rate`
and `sum(irate)` queries to 92.7 and 91.6 ms on that recording, whose long
cgroup table holds many series that ended, against 22.3 and 20.5 ms with
the bound.) Peak memory on the display query was 1.08 GB against 193 MB,
because each member's series were materialized before they were combined.
Since `perf/stream-union`, `rate` and `irate` on the grid over groups read
every recording in one batched scan (metriken-storage `scan::interleave`),
each member a series of the scan; a stage per partition holds each group
series' points until every member has passed the point and its rows nine
past it decide the coverage (`RowTrack`, which gives the runs `runs` gives
for the same rows and keeps only the rows and runs a later answer needs),
then passes the chosen point to the aggregation or display sink, so an
aggregate or display query materializes nothing per series (a plain `rate`
returns each series' points, as for one recording). A member whose recording
ends inside the range is finished when its scan ends (`ScanChunk::ended`),
and in a display aggregation a member series silent for ten times the
larger of its spacing and the step is ended as one recording's is
(`UnionStage::end_member`), so neither holds its group's points to the
end; a member series that pauses outside that rule still holds them until
its rows resume. `metriken-storage/tests/union_memory.rs` pins both: with
a member that ends at a quarter of the range, or whose series fall silent
there, the stages hold about 340 points per group series at most (a chunk
or two of rows), against about 3000 when the member is waited on to the
end. A display query of `sum(irate(cpu_usage[5s]))` over 9.6 h at 1 s,
read in one batched scan end to end, takes 32 ms against 59 ms with each
member read alone and combined after (#306), and peaks at 157 MB RSS
against 473 MB (one source: 13 ms, 80 MB). On this recording a cgroup
series that falls silent for ten spacings returns, so grouped display
queries of `cgroup_cpu_usage` end the batched read after about 9 ms and
are evaluated without the flush (on one source too); the figures that
follow include that. Against the double read by 0.41.0, the
`cgroup_cpu_usage` display query is 86 ms against 56 ms, 295 MB against
191 MB; the 1 h `rate` 13.8 against 11.9 ms; `sum(rate(...[1m]))` over
9.6 h 39 against 33 ms. Gauges, raw placement,
explicit evaluation timestamps, the per-series rate path and members clipped
to an epoch read each member alone as before. Two
pre-existing behaviours were found on the way and are not changed here:
`offset` in a selector is ignored (on upstream/main too), and a single
source's last points can move with the range's end (a rate at the range's
end, held past the source's last row). Queries
reaching no multi-member group take the code path they took before, and
cost the same within noise.

Readings of the rules where the text left a choice:

- The coverage rule needs "has rows at `t`" as well as "covers the
  window". A member holds `t` when one of its runs (S3's hole rule) runs from
  at or before `t` to within one interval after; it covers the window when
  that run also starts at or before `t − w`. At each point the finest member
  that covers the window and has a result answers; else the finest that
  holds `t` and has a result (the start of a recording, where no member
  covers a whole window); else nothing (S5). In raw placement the member is
  chosen the same way and only its own points are kept.
- "At each epoch's `from_ts` a series has a counter reset" is implemented as
  a member boundary: no increase is taken across `from_ts`, so the counter's
  first value after a restart never enters a rate. The Prometheus reading
  (the first value after the reset counted as the increase) would read a
  counter that a restarted producer does not start at zero as a spike.
- A group's labels: a key some sources carry takes the carried value; a key
  carried with two values splits the group into one read group per value
  (`ArchiveReader::read_groups`), a source lacking the key taking its uuid
  group's value when that is one value. This is the presented-recording
  split by any label, since the user layer is not yet stored apart (Q2
  decides `metriken.layer.user`; until it is written, any label can split a
  group).
- A group's metadata, where a tool shows one recording for it (rezolus's
  `RezReader::open_recordings` before metriken-storage 0.5.3, now
  `process::merge_metadata` behind `ArchiveReader::open_read_groups`): `events`
  is the union of the sources' events, each `id` once (the first source's
  copy), an event with no id once per distinct event, sorted by timestamp
  then id, except that a lone parseable payload wins over an earlier one
  that does not parse; every other key is the first source's, in archive
  order, that holds it. The sources are copies or recordings of one process,
  so they agree on a key both hold unless one was edited alone, and a key one
  holds (KPIs annotated onto a snapshot) is the group's whichever order they
  were combined in. A tool that edits a group writes each of its sources, found
  by `catalog_index` in the group reader's `sources()`.
- Bridges are read groups of their own until slice 3 joins their pieces, so
  case 3's continuous-recording-plus-attempts test (`sum` at 09:30 equals
  the attempt's alone) waits for slice 3.
- S5 applies to every series of a group, including one that a single source
  of the group holds (a filtered copy lacks it): a hole in it is a gap, as
  for a series every source holds, where the source alone interpolates a
  rate across the hole as before.
- A recording that does not hold the metric a leaf reads, as the kind it
  reads, is not read for that leaf (a filtered copy, another host), so it
  adds neither series nor a "not found".
- A gauge's staleness window and S9's "within one interval of its last row"
  use the member's interval for that metric; the rank uses the same
  interval.
- A group is one recording to cross-cadence evaluation only when its
  members are one source; a group of several sources is evaluated on the
  uniform grid, as several recordings are.

### What is stored where

| Item | Where | Written by |
| --- | --- | --- |
| Rows, source labels, source metadata (producer handshakes, recorder layer) | the source row and its segments and WAL | the recorder |
| Labels asserted at record time (`--label`, per-endpoint labels, `service_kind=`, `alias=`) and labels saved from the viewer (P1, Q40); the user layer | source metadata, apart from the recorder layer (Q2) | the recorder, on the user's behalf; `recording annotate` and the viewer, through the metriken-storage layer function |
| Source uuid | `sources.uuid`, random at creation, kept by dendro copies | dendro |
| Producer epoch and history | the recorder layer's entries (Q3); before the layers, the flat `producer_epoch` and `producer_epochs` keys | the recorder |
| View (default and named), template layer, label layer (relabel), report, archive-level events | v5 `.dendro` `documents` table, one kind each (Q24) | the viewer, MCP, `recording annotate`, `recording upgrade`, `recording views import` |
| Open-time label overlay (O5) | the caller's memory, for its session | the caller that opens the archive |
| Process groups, presented recordings, the default presentation | nowhere; computed at open | the `dashboard` crate |
| systemslab's own views | systemslab's storage | systemslab |

A `.rez`, a parquet or a v4 `.dendro` holds no documents: dendro's
`put_document` (`documents.rs:729`, `:773`) refuses a v4 archive as
`Error::SchemaTooOld` through `check_writable` (defined at `:207`, the
refusal at `:212`, called at `:362`). Each opens with the default (case 6).

### Every consumer

- **`combine`** is a plain union of its inputs' sources. Overlap is allowed
  and nothing is refused as a duplicate: two inputs sharing a source uuid
  produce an archive holding both rows, which the reader unions (S1). Each
  copied source keeps its uuid, labels, metadata and epochs (dendro
  `copy_sources_into`). It calls `copy_documents_into` first, in the same
  `read_snapshot` as its `copy_sources_into` (`combine.rs:808`, which today
  returns only a source count, so two inputs that each hold a `default` view
  resolve to the one copied last without a message), and lists each fork.
  Default views follow A13. Removing today's `shared_sources` refusal
  (`combine.rs:784`) waits until the readers implement S1, since before that
  a union is counted twice. Combining into a parquet loses epochs today
  (Q17).
- **`recording upgrade --to dendro`** refuses nothing for overlap, mints a new
  uuid per recording of a `.rez` or parquet as today, keeps each source's uuid
  when the input is a `.dendro` (new: today a `.dendro` input is refused,
  rezolus `src/parquet_tools/mod.rs:68-74`; as a dendro copy does, so the copy
  still unions with its original), writes the layers (Q3), and may write a view
  document (case 6).
- **`record`**: `--label k=v` on every recording of the invocation (today);
  per-endpoint labels, `--endpoint url,tier=client`, for any key (new:
  today the option list is fixed, and `source`, `arm` and `host` are its
  only labels); `--endpoint url,service_kind=X`; `source=X` written as
  `alias=X` (Compatibility). No run, session or join
  options.
- **The viewer** applies the presentation steps, shows the notice and the
  alternatives, and saves the user's choice as a document in a v5 `.dendro`,
  except labels the user assigns to presented recordings (a tier by grouping,
  P1, or any other key), which it writes to their sources' user layer (Q40). A
  relabel the user saves as a rule for other archives is a label layer document
  holding a selector on kind and labels. Save-as-Report saves the current
  presentation, saved or not, as the report's default view, with the archive's
  own layers only (P8).
- **MCP and `recording check`** call the same `dashboard` crate function as
  the viewer, with the archive's saved default view, else the evidence
  default. Notices go in the MCP tool result and on stderr for the CLI (P7).
  `--recording` and the tools' `recording` argument select a presented
  recording; a selector that matches several sources of one process group
  selects that group, and one that matches more than one presented recording
  is refused with a listing, as today. The per-recording tools iterate
  presented recordings when no selector is given (P7). `recording check`
  gives one verdict per presented recording, so a source and its copy give
  one verdict, and binds templates by kind (Templates). MCP `add_event` and
  `recording check --annotate` write per presented recording (an
  archive-level document naming its sources' uuids when it has several) or
  archive-level; they cannot address a group of a grouping.
- **The static site** applies the same rules through the shared `dashboard`
  crate, reads and writes documents (Views), and saves reports as `.dendro`
  (Views, "Save-as-Report").

### Consumers: systemslab

systemslab is a consumer, and none of this is in metriken, dendro or
rezolus's layer order (P5). The items are planned systemslab work.

- It renders each archive through the `dashboard` crate, with the
  archive's own layers, so the presentation rules and query rules apply.
- It keeps its own views at its own levels in its own storage, layered over
  the archive's rendering.
- Its runner passes `--label tier=<value>` for a job whose spec gives a
  tier (O4); otherwise it writes nothing into the file.
- It may pass an open-time label overlay (O5); rezolus never saves it
  (P8).
- Its query-time `job` and `experiment_id` labels group inside systemslab
  only, so a view built on them is not reproducible from the file (M8).
- Multi-arm statistics, high-dimensional sweeps and cross-experiment
  analysis are its features (Motivation, Scope).

| Here | systemslab | Relation |
| --- | --- | --- |
| `tier` | a tier the experiment author gives a job in its spec | Passed by the runner as `--label tier=<value>`; a job without one gets none. |
| none | job: one host's set of steps (such as server or client), labelled `job` at query time | rezolus has no `job` of its own. One job can hold several rezolus sources (an agent and a service on one host). |
| none | a retried attempt | A retry is a new set of recordings. A host agent that outlives several attempts gives them one producer epoch over disjoint spans, so they are separate presented recordings (O2), compared by time cluster (4c). |
| a view document | systemslab's own views | Kept in systemslab's storage, not in the file. |
| source uuid | none | systemslab's import does not preserve its own ids; the file's uuids are unchanged. |

### Dropped by the reframing

From the earlier revisions: runs as a stored identity layer (run id, name,
labels, the given-name flag, run records); sessions as a layer and the
`session_id` key; `combine` joining runs (cliques, `--no-join`,
`--run-name` permitting a join); `record --run-name`, `--run-label`,
`--endpoint url,run_name=`; `combine --allow-overlap`; the duplicate
refusals in `combine` and `to_dendro`; same-uuid merging into one source
row, and the dendro API it needed; the edit tools (rename, relabel as a run
operation, join, split, move, re-id) and their redirect records; the
derived UUIDv5 runs for old archives; the derived default view written on
first write; D3 (views name runs) and D5 (default grouping by run, with
today's refusals kept for compatibility); `service` as the endpoint-kind
label, then `service_kind` as the kind itself (O3 makes it a refinement);
`name` as the naming label (P2); systemslab layers inside the archive's
template stack (P5); MCP evaluating per group (P7).
Relabel stays, as user entries on the sources it names (Q40), a label layer
document and an open-time overlay. The `role` rename is dropped: the earlier
revision renamed the `role` value `service` (`per_source_metadata` `role:
"service"`/`"loadgen"`, `src/parquet_metadata.rs:106-109`; the recorder's
endpoint `role=`; llm-perf's `ROLE_SERVICE`, `src/recording.rs:43`) to `server`
so that `service` had one meaning, and with no label named `service` the rename
fixes nothing.

### Traceability

Motivation to specification:

- M1 → case 1; Templates; P5.
- M2 → case 2 (navigation groups, cross-producer sections and their
  binding); D1; P9.
- M3 → case 3; S1 to S12; P6/R3; case 5 (warning where no epoch exists,
  or where one epoch does not union).
- M4 → case 4 (4b, 4c, more than two groups); O1, P1, P4; alignment.
- M5 → case 4 (4d, 4e); O1, O3, O4, P1, P3.
- M6 → `combine` (plain union, default views on combine); identity layers;
  P5; the clash rule and the series-label rule; label layers and overlays
  (O5). Interim gap: `combine` keeps its shared-uuid refusal until S1
  lands, and combine-to-parquet drops epochs (Q17). Test: `recording
  annotate --alias web` on a rezolus recording leaves the producer's
  `source=rezolus` readable in the labels of `metriken.layer.producer`.
- M7 → References to sources (rules transfer); Views (export and apply);
  combine with a saved view (A13). Test: a view "group by `tier`" exported
  from one archive and applied to another with three `tier=server` and one
  `tier=client` agents draws two lines. Test: combining an archive with no
  saved view (first) and one with a default view (second) opens with the
  second's view, and recordings it does not place get "N recordings not
  placed by the saved view".
- M8 → What is stored where; Views (documents, Save-as-Report); P8.
  Caveat: a systemslab view that groups by query-time `job` or
  `experiment_id` is not reproducible from the file; the `tier` label (O4)
  is the reproducible path. Test: Save-as-Report from a presentation
  grouped by `tier` (unsaved), reopened in the static site, shows the same
  two lines and no notice. Test: Save-as-Report with an overlay adding
  `tier` warns, names `tier`, and the report carries the label layer only
  when the user accepts; declined, the reopened report has no `tier`.
- M9 → presentation step 7; the Alternatives and Saved entries of every
  case; MCP and `recording check` notices (P7). Test: `rezolus mcp query`
  with no selector on the two-day fixture of case 4 is refused, with a
  listing of both time clusters on stderr; through the MCP server the
  listing is in the tool result.
- M10 → Every consumer; the query rules hold for every consumer; P7.
  Test: the case 4 `tier` fixture gives the same values for the `server`
  line in the viewer, through MCP `query` with `sum by (tier)
  (rate(test_requests[10s]))`, and in the static site. Test: `rezolus mcp
  detect-anomalies` on an archive of 20 presented recordings returns one
  ranked top K per recording, in catalog order, every recording named,
  within the total size cap.
- M11 → case 6; case 5; D2; Compatibility.

Tests for the layer order (P5, R1), under M6:

- A producer that sends a KPI template, recorded with `--endpoint
  url,template=FILE`: the producer's KPIs render, `record` warns, and the
  file's template is readable as the recorder default.
- A producer whose handshake declares `source=rezolus`, recorded with
  `--endpoint url,alias=web`: the series carry `alias=web`, and the
  handshake's value is readable in the labels of
  `metriken.layer.producer`.
- A producer whose handshake declares `tier=a`, recorded with `--label
  tier=b`: the series carry `tier=b`, and `tier=a` is readable in the
  labels of `metriken.layer.producer`.

Specification to motivation:

- Case 1 → M1. Case 2 → M2, M5. Case 3 → M3, M6. Case 4 → M4, M5, M9.
  Case 5 → M3, M11. Case 6 → M11, M8.
- O1, P1, P3, P4, A5 to A10 → M4, M5. O2, P6, R2, R3, A2 → M3, M4.
  O3, P2, P9, A3 → M1, M2, M11. O4, R4 → M5, M8. O5, P5, R1, P8, A11, A12
  → M6, M8. P7, A4 → M9, M10. A1 → M2, M4. A13 → M6, M7.
- Cross-producer binding → M2. Combine with a saved view → M6, M7.
- S1 to S12 → M3, M10. D1 → M2, M10. D2 → M11. Presentation steps → M9,
  M10.
- The clash rule and the series-label rule → M6, W9.
- `combine` → M6. MCP, `recording check`, static site, systemslab → M10.

Workflows to cases: W1 → cases 2, 4d or 4e; W2 → 4b or 4c; W3 → 4b;
W4 → 4c, O4, Consumers: systemslab; W5, W7 → case 3; W6 → case 1 (one
restarted source), case 3 (epoch chains); W8 → cases 5, 6; W9 → case 5, the
clash rule; W10 → Save-as-Report, P8; W11 → References to sources, view
export; W12 → MCP, `recording check`, P7; W13 → static site.

## Templates in layers

### Decided

- **A template is a stack of layers in P5's order,** from least to most
  specific, the last winning: the registry fallback (built in); the recorder
  default (`template=FILE`, written by the recorder); the producer's layer
  (carried in the recording); the archive's user layers (documents written
  by `recording annotate` or the viewer); live unsaved edits. The producer's
  layer wins over the recorder default because it carries version-specific
  queries (#1501 calls the producer's copy the maintained one; #1493 reads
  the recording's template first). systemslab is not in this stack
  (Consumers: systemslab). A user layer adds sections or KPIs, replaces one
  by its id (a section's route, a KPI's id), hides one, or hides everything
  beneath it (D4); it never deletes the layers beneath it. The rendered
  template is the stack merged.
- **Hide everything beneath, of one template part (D4),** is a layer
  operation: a user layer can hide every KPI, every section, or both, of the
  layers below it. It is still additive: the producer's copy is kept, and a
  later layer, or removing this one, shows it again. The viewer reports what
  such a layer hid. It covers two cases today's code handles by replacing: a
  broken producer KPI template, which a user now beats by saving a user
  layer that hides everything beneath and adds their own (a record-time flag
  that writes such a layer is Q38), and `rezolus view --registry-templates`
  (`TemplatePreference::Registry`, `source_templates.rs:72`), which becomes
  a user layer that hides both parts (KPIs and sections) beneath and adds
  the registry's template.
- **The registry layer today is a whole-template fallback, and per-id
  merging with it is new.** `source_templates.rs` renders the template the
  metadata carries when there is one; `--registry-templates` inverts that
  for sources the registry has a template for; a carried `service_queries`
  that does not parse falls back to the registry's with a notice. Nothing
  merges the two by id. Which of the two the stack does is Q20.
- **Template lookup by kind, refined by `service_kind`,** applies to the
  registry layer only; a legacy `source` reaches it through the D2 read
  rule, and a source with no producer layer through the registry match of
  its `service_kind`, then its `alias` (Q35). The producer's layer and user
  layers are carried with the recording and need no lookup.
- **Cross-producer sections** (case 2) name the other producer by kind,
  declare their binding, and are bound per query through the engine scope
  (D1).
- **A chart declares how a group's members aggregate** (P3): counters and
  rates sum; histograms merge buckets and then derive; a ratio is recomputed
  from a declared numerator and denominator, else meaned with a notice; an
  undeclared gauge defaults by unit in any template (A6): percent mean,
  bytes sum, else one line per member with a notice. This holds in producer,
  registry and fallback templates alike (Q36 for the format).
- **Templates stop naming their source, after every reader restricts.** Each
  query of a template copy is restricted to its presented recording (today
  by `scope_matchers` and `restrict_query`, to be replaced by the engine
  scope of D1). MCP evaluates templates only in `run_checks`
  (`src/mcp/server.rs`), which goes through `recording check`'s
  per-recording path. Unrestricted today: a combined parquet
  (`label_parquet_by_source`, `combined.rs:1310`, returns the reader
  unchanged when `source` is a JSON array), A/B tarballs, and every released
  rezolus (5.x, through v5.28.0; no 6.0 prerelease is tagged). Any of those
  evaluates a template without `source=` against series it was not written
  for, and one reading several recordings sums them. So templates keep their
  `source=` matchers until a recording declares a template schema that only
  restricting readers accept (Q19). The order is: the restriction
  everywhere, released; then templates without `source=`, dropping the 215
  matchers and the cachecannon and llm-perf tests that require them
  (cachecannon's `every_metric_selects_the_handshakes_source`,
  `src/admin/template.rs:125`, is one).
- **Producer-layer and registry-layer templates are evaluated against the
  series' own labels, falling back to the recording's labels for a key the
  series does not carry; on a key both carry, the template matches the
  series' value and the scope the recording's.** Such a template is never
  evaluated against `exported_<key>`. The registry layer needs the rule
  because a Prometheus producer has no producer layer. Without this, a
  recorder or user `instance` renames a producer's own `instance` series
  labels and the producer's template selectors on them stop matching, which
  a federated Prometheus producer makes likely. The rule is engine
  behaviour, built in metriken-query after the scope input, and needs the
  scope (D1): one PromQL selector cannot match the series' value and the
  recording's on the same key.
- **`recording check` binds templates to recordings, not scopes.**
  `open_targets` (`src/parquet_tools/check.rs:551`, `open_recordings` at
  `:582`) opens one reader per recording, so a query cannot read another
  recording's series; what a template without `source=` loses there is the
  choice of which recordings it applies to, which becomes a binding by
  kind. A combined parquet is one `ParquetReader` over every source
  (`check.rs:568-579`), so binding there needs a per-source scope as well.
  Under S1 the targets are presented recordings, so a process group opens
  as one target.
- **The recorder default:** `record --endpoint url,template=FILE` and the
  config file's `[[endpoints]] template =` (`EndpointConfig::template`,
  `src/recorder/endpoint.rs:51-56`, #1501) today write the file as the
  recording's `service_queries`, which replaces the producer's handshake KPI
  template and leaves its `dashboard` alone. Under P5 the file is the recorder
  default, below the producer's layer: when the producer sent a KPI template,
  the producer's is rendered, and `record` warns (A12). #1501 is in no tag
  (v5.28.0 is the latest), so this changes behaviour on main only. Each layer
  is stored under its own key (Q2), and `service_queries` holds the KPI
  template P5 renders. New: `service_kind=` and `dashboard=FILE` (sections) on
  an endpoint, and `recording annotate --template` / `--dashboard` (annotate
  has `--queries`, `src/parquet_tools/mod.rs:86-93`, and `--templates DIR`,
  `:105-115`). The viewer adds a layer for the open archive and can save it.
- The registry gains section templates (today it holds KPI templates and
  categories; the rezolus section template is embedded separately).
- **systemslab renders the file's dashboards** (planned work) through the
  shared `dashboard` crate, the expander both rezolus viewers use, so a
  file's layers render there as in rezolus. Its own dashboards live at its
  own levels in its own storage, over the archive's rendering (Consumers:
  systemslab).

## Views

A view is how an archive is looked at: which presented recordings, how they
are grouped, how time is aligned, which template layers apply, and how
charts and navigation are laid out. It is data, saved in the archive as a
document, versioned and with history.

### References to sources

A view stores rules where it can, so it reapplies to future recordings
(principle 4):

- **Grouping rules** name label keys: "group by `arm`", "group by `arm`,
  then by `concurrency`, expanded". They transfer to any archive whose
  sources carry those labels.
- **Selectors** name a kind and labels: "the rezolus recording with
  `host=web-01`", "every `service_kind=vllm` recording". They transfer.
  A selector may also name a producer epoch, which matches only that
  process's data, so it transfers only to copies of the same recording.
- **Hand-picked sets** name sources by uuid: a split of one process group
  (case 3), "treat as one" (case 5), a group the user built by picking
  recordings. Within an archive a uuid also names its copies, which share
  it. A hand-picked set does not transfer: applied to another archive its
  uuids match nothing, and both the export and the apply say so.
- **Labels on specific sources** are user-layer entries (Q40). A **label
  layer** maps selectors (a uuid, or a kind and labels) to labels; its
  kind-and-labels rules transfer, and its uuid selectors match only within
  the archive.

### Decided

- **Grouping is an ordered list of grouping labels** (for example `arm`,
  then a parameter label), each level aggregated or expanded into its
  members, with groups the user can split or merge. The owner's example:
  three control and three experiment nodes shown as two aggregated groups,
  or as six individual series, or experiment nodes split by parameter.
  "Compare by none" is today's combined view. Each group is one series on
  each chart, its members aggregated as the chart declares, with a toggle
  that splits the group into one line per member (O1, P3). A grouping by
  `arm` or by time cluster is a comparison (a reference group and the diff
  views, P4); a grouping by `tier` or another label is not. The default has
  two levels (P1). Recordings without the level-1 label are shared context,
  shown once; within a level-1 group, each recording without the level-2
  key is drawn as its own line (A8). A grouping renders whatever groups it
  yields, one group included, with a notice.
- **A group can be an explicit set** (a hand-picked set, above). This keeps
  today's `?compare=1` without arms expressible: `arm_slots` then takes the
  first two recordings by position, and a view records them as two groups
  of one recording each.
- **The default view** is the archive's saved default view document;
  without one, the presentation steps of the specification.
- **Alignment is a setting of the view:** absolute time (groups that
  overlap: a concurrent A/B, a fleet), trim to the window every group
  covers, elapsed time (each group mapped onto a common timeline from its
  own start), or alignment on an event kind (#1330). A saved view records
  its alignment. Without one, alignment is chosen after grouping: elapsed
  when the groups' spans do not overlap, absolute otherwise (presentation
  step 5).
- **Layout:** the navigation arrangement (kinds, sub-groups, flat; cases 2
  and 4), and overlay or side-by-side views (small multiples) per group.
  The views that compare two recordings (heatmap diff, side-by-side
  heatmaps, a percentile chart's Full/Tail spectrum) draw a reference and
  one other group only; they are offered when there are two groups.
- **Views are saved in the archive** as versioned JSON documents with
  history, in dendro's archive-level `documents` table (dendro 0.6.0,
  schema 5): append-only revisions with `uuid`, `kind`, `name`, `schema`,
  `body`, `supersedes` and `created_wall_ns` (`documents.rs:24-44`). A
  report's pins, archive-wide events, template layers and label layers are
  documents too, so no recording is the anchor. Reports saved before this
  anchor by label set and are read by label set.
- **Save-as-Report writes a `.dendro`** from any archive source, with the
  current presentation, saved or not, as the report's default view. That
  is a change: today a `.rez` source or a parquet comparison saves a `.rez`
  (`crates/report-save/src/lib.rs:571`, `build_rez_report_from_rez`, and
  `:781`, `build_rez_report_from_parquets`) and a single parquet a
  `.parquet` (`save_single_parquet`, called from
  `crates/viewer/src/lib.rs:1460-1468`); a dendro source already saves a
  `.dendro` (`build_dendro_report`, `:650`).
- **The static-site viewer reads and writes documents.** The WASM build
  reads them through dendro's read path, without `crates/rez`'s `write`
  feature, and Save-as-Report writes them in the browser through
  `ArchiveMut::create_in_memory` (dendro `sqlite.rs:2998`; dendro takes a
  revision's timestamp from SQLite's clock, `unixepoch('subsec')`,
  `documents.rs:267-270`, for this reason). A `.rez` or parquet source's
  report needs an ingest path into `.dendro` that builds without `write`:
  `to_dendro` is gated on metriken-storage's `write` feature
  (`metriken-storage/src/rez/mod.rs`), which the WASM build cannot enable,
  and `report-save` takes `rez` without default features
  (`crates/report-save/Cargo.toml`).
- **Views are reusable:** a view, a template layer or a label layer can be
  exported to a file and applied to another archive, or supplied as an
  open-time overlay (O5). Rules and selectors apply; hand-picked sets are
  reported as not applicable (Q25 for what does not match).
- **`recording views import <from> <into>`** copies view, template layer,
  label layer and report documents, reports forks (`copy_documents_into`)
  and references that match nothing, and needs a dendro kind filter on the
  copy, or goes through `put_document`.
- **`combine` reports document forks** (Every consumer).
- **systemslab's views** are systemslab's, in its own storage (Consumers:
  systemslab).

### What exists to build on

- Per-recording template copies, query restriction by recording labels,
  and a scoped catalog per copy (`crates/dashboard/src/combined.rs`, on
  rezolus main since #1504). The scoped name and label listings go through
  `owners_of` and `holds_*` (`scoped_names`, `scoped_labels`).
- One engine across recordings (metriken #270).
- The frontend overlays N recordings on line charts (rezolus #1152), lists
  them in the compare badge (#1172), and draws every recording but the
  experiment as one family band, mean ± kσ or min..max (#1331), which is the
  aggregated form of one group. #1488 pairs strided diff columns to the
  nearest window and floors each recording's stride at its own interval.
  From an archive, a comparison feeds two recordings to the frontend and
  shows the others beside it in their own sections (`comparison_order`,
  `crates/viewer/src/lib.rs:138`), so #1331's family band is not reached
  from an archive (read from the code, not run).

## Compatibility

Lands with step 3.4 unless stated. Each user-facing spelling of `source`
gets an alias, a deprecation warning, or a refusal:

- `--endpoint url,source=X` once `alias=` exists: read as `alias=X` with a
  deprecation warning (P2); `source=` and `alias=` with different values on
  one endpoint is refused. An invocation that passes `--endpoint
  url,source=X,protocol=msgpack` keeps working with the warning, and X
  still selects a registry template when it names a registry entry (as
  `alias`, Q35).
  Test: `--endpoint url,source=X,alias=Y` is refused; `--endpoint
  url,source=X` warns and the recording carries `alias=X`.
- `record --label source=X`, and `record --url … --metadata source=X`
  (the forms the help and the "no source name specified" warning name,
  `src/recorder/mod.rs:107`, `:133`, `:652`): read as `alias=X` with the
  same warning.
- `recording annotate --source X [--overwrite]`: an alias for `--alias X`,
  with a warning. `--alias` adds a user-layer entry setting `alias`, which
  wins over a record-time `alias`; the earlier entry and the producer's
  `source` stay (M6). Since no write overwrites, `--overwrite` has no
  effect; it is accepted and ignored with a warning (Q40).
- `recording metadata --field source`: prints the value the D2 read rule
  gives, with a warning naming `--field alias`.
- `recording combine --ab baseline=… experiment=…`: into a `.rez` or
  `.dendro` each value names an input (the path as typed, or its file
  name), which this work does not change. Only into a `.parquet.ab.tar`
  does a value name an input's source (`src/parquet_tools/mod.rs:411-426`);
  there it is matched against `alias`, then the legacy `source`, with a
  warning pointing at a multi-recording archive with `arm` labels (or the
  A/B tarballs are retired, step 3.2).
- llm-perf `[metrics] server_source` (`src/config.rs:504`), which names the
  server under test: an alias for a new key naming the server's `alias`,
  changed in llm-perf in step 6, with a warning; until then llm-perf writes it
  as a `source` label, which D2 reads as a user-given `source`.
- cachecannon writes `source` in three places: its stream handshake label
  (`src/admin/stream.rs:54`), its snapshot metadata
  (`src/admin/mod.rs:569-570`), and its parquet metadata (`:663-664`, with
  `version`). All three are replaced by the producer identity macro; a
  recorder reads a `source` from an older producer as `__producer__`.
- Report anchors (lands with step 3.5): a report written before this is
  read by its label-set anchor; a label set that matches no recording is
  reported rather than falling back to the first recording silently. New
  reports are documents and have no anchor.
- `arm_slots`'s three refusals (a shared arm, more than two armed
  recordings, one armed recording among several) are removed with step
  3.5; those archives open with a notice (case 4). These refusals are in no
  release (#1504 is untagged).
- `template=FILE` becomes the recorder default below the producer's KPI
  template (P5), and `record` warns when both are present (A12); a change
  on main only (#1501 is untagged).
- MCP (P7, lands with step 3.5): `detect-anomalies`, `describe-recording`,
  `describe-metrics` and `extract-features` with no selector iterate the
  presented recordings and report each, where today `detect-anomalies`
  runs pooled over the combined reader and `extract-features` refuses;
  `query` and `analyze-correlation` with no selector keep refusing an
  archive with arms and also refuse one with time clusters; an unarmed
  archive's copies count once after S1, where today they count twice;
  notices go in the tool result.
- `combine`'s shared-uuid refusal is removed once readers implement S1
  (step 3.5). An archive combined with a copy then opens with the copy
  unioned.
- systemslab picks service templates by `source` today; once it renders
  through the shared `dashboard` crate it follows that crate's lookup (the
  kind refined by `service_kind`; a legacy `source` through D2). Its
  query-time `job` and `experiment_id` labels need no rename.
- hindsight's config key `source` (the agent's address): renamed (for
  example `endpoint`), with `source` accepted and warned about.

## Open questions

Identity and naming:

- **Q1.** The exact set and spelling of recorder-layer keys. Decided
  2026-10-26:
  - `instance` is the URL the recorder recorded from, as given, path
    included. It tells two endpoints of one archive apart and is displayed;
    nothing binds or unions on it across presented recordings. 4e and P9 use
    it as a last-resort series key within a host or among sources without
    one, where a port forward can merge or split those series. Through a
    port forward or a proxy the address is the recorder's view:
    `localhost:9000` can name a remote process, one service reached through
    two forwards gets two values, and one address on two days can be two
    programs. Case 5's duplicate warning still compares it, and a port
    forward can make that warning wrong in either direction; it is a warning
    only. An earlier revision resolved a `localhost` endpoint by the
    recorder's hostname, which a port forward makes wrong; that key is
    dropped.
  - `host` (not `node`) is the producer's hostname, declared in its
    handshake as part of the producer identity (the producer layer). rezolus
    reports it through `/systeminfo` until its handshake carries it. A
    producer that declares none (a Prometheus endpoint) has no `host`
    unless one is asserted at record time (`--endpoint url,host=NAME`,
    rezolus #1538); a cross-producer binding (case 2) then needs that
    assertion. This gives up a case the earlier rule
    covered: an endpoint recorded by its DNS name or IP no longer binds
    without `host=`, so llm-perf's server-CPU section (W1) is hidden until
    the server's host is asserted (llm-perf's `server_host`, order of work
    step 6).
  - The producer identity's keys (amended 2026-10-27): a producer declares
    it in its handshake's metadata, and a msgpack snapshot's, as four
    strings, `metriken.producer.name`, `metriken.producer.version`,
    `metriken.producer.build` and `metriken.producer.host`, the last three
    optional (metriken-model `identity`). metriken-exposition's
    `producer_identity!` fills the name and version from the producer's
    crate and the host from the machine's hostname. They are metadata
    rather than handshake labels so that a recorder that predates the
    layers stores them on the source without adding them to its series.
    The layered recorder will read them into the producer entry (`host` as
    a label, step 2) and will not store the `metriken.` keys themselves.
    `Layers::derive_legacy` reads them into a producer entry (#328), so an
    upgraded source recorded with them before the layered recorder has its
    producer identity and `host`.
  - The keys that can clash with a producer's own series labels, and so go
    through the clash rule: `instance` (federation, a pushgateway), `source`
    (rezolus's own provenance values), `alias`, `tier`, `host`, `arm`,
    `role`.

  The candidates were `instance` as a URL or as `host:port`, `host` or
  `node`, and a recorder-hostname key such as `recorder_host`.
- **Q2.** How each layer is stored in dendro. Decided 2026-10-26: in source
  metadata, one key per layer holding JSON, under the `metriken.` prefix
  metriken uses for its handshake keys (`metriken.frame_interval`) and segment
  keys (`metriken.layout`). Decided 2026-10-27: each layer is a list of entries:
  `metriken.layer.user = {"labels": {…}, "metadata": {…}, "entries": [{"id": …,
  "seq": …, "ts": …, "by": …, "labels": {…}, "metadata": {…}, "unset_labels":
  […], "unset_metadata": […]}, …]}`, and likewise `metriken.layer.producer` and
  `metriken.layer.recorder`, whose entries also carry `from_ts`. `id` is random
  per entry, except a derived entry's (below) and an `external` entry's,
  `external-<seq>-<id of the entry being appended>`. `seq` is one more than the
  highest `seq` in the layer when the entry is written (the first entry in an
  empty layer takes 0), so it gives append order without a clock. `ts` is the
  wall time of the write in nanoseconds since the Unix epoch, shown to the user
  and not used for ordering. `from_ts` is the timestamp of the first row the
  entry covers, as dendro defines it for `producer_epochs`
  (`src/lib.rs:223-227`); an entry written before the source's first row (at
  open, since metriken-recorder adds the source before its pump starts,
  `recorder.rs:2520-2527`) carries none, folds before every entry that has one,
  and covers from the source's first row. `by` names the writer: `producer` (a
  handshake, or an in-process `SourceIdentity`), `recorder` (including what
  rezolus's probe hook reads from `/about`, `/status`, the `/` banner and
  `/systeminfo`: `producer_name`, `producer_component`, `version` or
  `exporter_version`, `agent_version`, `host`), `record` (labels the user gave
  at record time), `annotate`, `viewer`, `upgrade` and `convert` (layers derived
  by D2's read rule, Q3; a msgpack snapshot's `version` goes into its epoch's
  producer entry), `combine` (its label stamping, step 3.4), or `external` (a
  change the layer function found made outside it). `unset_labels` and
  `unset_metadata` list the labels and metadata keys the entry removes, so an
  undo is one more entry: unsetting removes the key from the entry's layer, so
  the merged result takes the lower layers' value, and undoing one edit of
  several writes the key's earlier value. They are separate because the recorder
  writes `source` as both a label and a metadata key. A key set to the empty
  string is left out of the merged result, as Prometheus treats an empty label
  value as no label, so a user entry removes a label the producer declared by
  setting it to `""`; in the producer and recorder layers an empty value removes
  the key from that layer, as an unset does, and does not remove a lower layer's
  value (amended 2026-10-27). A producer entry carries the handshake's epoch
  (its `uuid`, metriken-recorder `frames.rs:144-146`) when the handshake names
  one; the recorder drops the `producer_epoch` copy a producer puts in its
  handshake metadata, so the fold writes no flat `producer_epoch`. A reconnect
  that resends a handshake identical to the producer entry that wins the fold,
  or to the producer entry appended last when appending it would change neither
  the producer fold nor its epoch's, appends nothing; `from_ts` is not compared
  (amended 2026-10-27: the highest-`seq` entry need not win the fold when
  restart stamps disagree, so a resend whose `from_ts` sorts before the winner
  is compared with the producer entry appended last, and one that changes the
  layer's fold or its epoch's fold is appended). A
  recorder entry is an observation: the entry at open carries the endpoint
  (`instance`) and the transport, and each epoch, the opening one included, is a
  recorder entry carrying that `epoch`. Only recorder entries carrying `epoch`
  delimit spans; the spans are taken as `epoch_spans` takes them from
  `producer_epochs` today: in `from_ts` order, not list order, since restart
  stamps come from clocks that need not agree, with the first also covering
  earlier rows (metriken-storage `src/process.rs:121-127`). A span's handshake
  is the fold of the producer entries with that `epoch`; a source with no epoch
  has one span, whose handshake is the fold of all its producer entries. Every
  layer folds per key, so a key a later entry omits keeps its earlier value; a
  restarted producer that drops a label such as `zone` keeps it until an entry
  unsets it. rezolus's `descriptions` is unioned across entries and layers
  rather than replaced, as a restart unions it today
  (`src/recorder/agent_probe.rs:222-239`); for a layered source the layer
  function's fold does this, and the recorder no longer calls the probe hook's
  `merge` (#328). The producer and recorder layers fold their entries in
  (`from_ts`, `seq`, `id`) order, `id` ordering derived entries that share the
  other two. The user layer folds its entries in (`seq`, `id`) order: within one
  copy that is append order, so a later edit wins whatever the writer's clock
  says. When copies of one source (one uuid) meet in a process group, their user
  entries are merged as a union by `id` and folded the same way; the notice
  names any key that entries present in only one copy set to different values,
  since `seq` does not order edits made apart in different copies. The top-level
  `labels` and `metadata` are a cache of the fold: a reader folds the entries,
  and when the cache disagrees the fold wins and a notice names the source. The
  layer function itself does not discard such a change: a label or layered
  metadata key changed outside it since its last write is appended first, as a
  user entry by `external`, a removal as the key set to `""`; the write that
  derives a legacy source's layers checks nothing, since the derivation is the
  source's own state (amended 2026-10-27). Labels are source-wide: the merged
  `labels` carry the newest value for every row of the source, and only the
  epoch spans and the per-epoch templates (Q8) apply per span. No edit rewrites
  or removes an entry (M6). Trimming and retention are the one removal: dropping
  every row of an epoch drops the producer and recorder entries carrying that
  `epoch`, as the producer layer specifies (Identity in three layers); entries
  without `epoch` (the endpoint at open) and user entries are kept. Today
  nothing prunes `producer_epochs`, and `epoch_spans` skips an entry with no
  rows in the span. The first layer write to a source without `metriken.layer.*`
  (an `annotate`, a viewer save, `combine`'s stamping) first writes the layers
  D2's read rule derives (`by: upgrade`), so the record-time labels and metadata
  of a legacy source enter the fold. The recorder writes through the layer
  function before any other tool does (step 2 before step 3.4): a recorder that
  writes the flat keys or `update_metadata` to a layered source has its epochs
  ignored (only the recorder layer is read) and its writes to keys the layers
  set captured as `external` user entries. Amended 2026-10-27, the derivation
  is: the labels as one user entry, leaving out `metriken.` labels; one at-open
  recorder entry holding the legacy metadata other than the keys dendro
  maintains (`writer_sessions`, `encoder`, `events`), the flat epoch keys and
  `metriken.` keys, so a legacy value a producer or user edit overrides and a
  later edit in that layer unsets falls back to the source's own (a recorder
  unset removes the source's own value too, since an unset removes the key from
  its whole layer); `producer_name`, when D2's evidence 1 to 3 names one, in a
  further at-open recorder entry with `seq` 1 that folds over the at-open entry
  and whose id includes the name, so readers holding different evidence derive
  the same entries apart from that one; each epoch as a recorder entry, read as
  `epoch_spans` reads the flat keys (the `producer_epochs` list
  when it parses and is not empty, each listing kept, else `producer_epoch` with
  no `from_ts`). A listed epoch's `version` is not derived: the source's
  `version` is its newest, which the at-open entry holds, and the flat
  `producer_epochs`, which no write removes, keeps the history. Entries derived
  by D2's read rule take `seq` 0 (1 for the evidence entry, and an epoch
  entry its position in the legacy list, so epochs listed at one `from_ts`
  keep their list order), the one exception to the `seq` rule, and an
  `id` derived from the source uuid, the layer name, the entry's epoch and its
  `from_ts` (none for the at-open entry; `from_ts` because a legacy list can
  repeat an epoch); the derived user layer is one entry holding every derived
  user-layer key. Derived entries carry no `ts`, and two derived entries with
  one `id` are compared on `labels`, `metadata`, the unset lists and `epoch`
  only, so a derivation read at open, one written by `upgrade` and one written
  by `convert` are the same entry. So the derivation read from a pre-layer copy
  and the one written into a layered copy are one entry in the union; when two
  entries share an `id` with different contents, the layered copy's entry is
  kept (which one is kept when both copies are layered and their derivations
  differ, for example derived by an older and a newer `upgrade`, is open) and
  the notice names the source; a pre-layer copy meeting a layered one is read
  through the same derived layers for the merge. A label the user assigns to
  specific sources, from the viewer (any key, `tier` and `arm` alike) or with
  `recording annotate`, is a user entry; a rule meant for other archives (a
  selector on kind and labels) is a label layer document saved with a view and
  applied as an open-time overlay (O5). Exports to formats without layers
  (parquet, Prometheus) carry the merged `labels`; whether a parquet keeps the
  epoch history is Q17. The layers are the record; the source's `labels` and its
  merged metadata keys (`service_queries` and the others P5 renders) are their
  result, written by one metriken-storage function. Every write of a labelled or
  layered key goes through it rather than through dendro's
  `update_source_labels` or `update_source_metadata`: today rezolus `combine`'s
  label stamping (`src/parquet_tools/combine.rs:442`) and `annotate`'s
  `service_queries` and `systeminfo` writes (`annotate.rs:454`) bypass it, and
  move onto it in step 3.4. The recorder drops every `metriken.`-prefixed key
  from a producer's handshake metadata, as it drops `metriken.frame_interval`
  today (metriken-recorder `recorder.rs:996-998`), so only the layer function
  writes the layer keys. The layer function (metriken-storage `layers::append`)
  edits a source's two maps and does no I/O. It builds without
  metriken-storage's `write` feature, so the WASM viewer can call it on an
  in-memory archive (The static-site viewer reads and writes documents), and the
  recorder applies its result to a live source with dendro's
  `SourceWriter::set_identity` (dendro 0.6.1), since `update_metadata` can
  neither set labels nor remove a key. An entry that sets or unsets a
  `metriken.` label or metadata key, a flat epoch key, or a key dendro maintains
  (`writer_sessions`, `encoder`, `events`) is refused. The flat `producer_epoch`
  and `producer_epochs` keys move into `metriken.layer.recorder` (Q3).

  Separate columns were the alternative. dendro's columns are what the
  container acts on (`uuid`, `clock_anchor_wall_ns`, `complete`, the
  heartbeat); it never interprets identity layers, which exist for
  metriken's merge order (P5) and clash rule. Every dendro path that copies
  a source carries its free-form identity only as `labels` and `metadata`
  (the replication `Handshake` frame, `copy_sources_into`), and
  metriken-storage's `to_dendro` writes the same two, so columns would
  need a dendro schema bump, a wire change and a change to each path,
  where metadata keys travel through all of them unchanged. A reader that
  does not know the keys shows them as ordinary metadata.
- **Q3.** Migration: reading recordings made before the layers (`source`
  label and metadata) without rewriting them; whether `recording upgrade`
  rewrites them; until when the recorder writes `source` (D2); and how to
  read a recording that carries none of D2's evidence (a streamed producer
  with no template, a parquet from a producer that is not the agent). Two
  cases the evidence list does not settle: a producer scraped as msgpack (V2
  snapshots, whose snapshot metadata carries `source` and `version`, as
  cachecannon's does at `src/admin/mod.rs:569-570`), where the candidates are
  (a) the snapshot metadata's `source` as `__producer__`, since the producer
  wrote it, or (b) none, as for a Prometheus scrape; and a Prometheus scrape
  of `rezolus exporter`, which carries no `version`, so evidence 2 does not
  match: metriken-recorder calls the probe hook's `enrich` only for a streamed
  endpoint (`first_enrich`, `recorder.rs:1885`; `Verdict::Scrape` carries
  nothing), and the exporter serves no `/status` (`src/exporter/mod.rs:192-195`)
  and a banner, `Rezolus <version> Exporter` (`:204-206`), that
  `parse_root_version` rejects, since it accepts only a banner ending in
  ` Agent` (`src/recorder/agent_probe.rs:128-136`).
  Candidates: (a) keep the version requirement, so the scrape has no
  producer; (b) the self-metric names alone suffice, as `source_kind.rs`'s
  self-metric check does (`:51-54`), and `__producer__` is `rezolus`.
  Decided 2026-10-27:
  - Recordings made before the layers are read without rewriting them,
    with D2's read rule applied at open.
  - `recording upgrade` writes the layers when it converts a legacy file,
    and also takes a `.dendro` written before the layers and writes a new
    one with them, derived by the same read rule and keeping each source's
    uuid. `recording convert` (legacy `.raw` msgpack snapshots, through
    metriken-storage's `MsgpackToArchive`) writes the layers the same way.
  - Forward compatibility is out of scope. From step 2 the flat
    `producer_epoch` and `producer_epochs` keys move into the recorder
    layer's entries (Q2), with no copy kept for older readers, and the
    recorder writes no `source` of its own (D2).
  - The recorder records streaming producers, Prometheus endpoints and
    in-process sources (Q7) only. `protocol=msgpack` is already a spelling of
    `stream` (rezolus `src/recorder/config.rs:451`); the recorder scrapes no
    msgpack snapshot, so the msgpack case does not arise for a new recording;
    candidate (b) applies to a legacy one. A legacy recording of a msgpack
    scrape gets no `__producer__` from its snapshot metadata's `source`, which
    is read by D2's rule like any legacy `source`; a `version` its snapshot
    metadata carries (a 5.x agent's does) counts as D2's evidence 2.
  - The `rezolus exporter` scrape, amended 2026-10-27: for a new recording the
    exporter declares its name and component. From step 3.4 the agent and the
    exporter both serve `GET /about`, a JSON object `{"name": "rezolus",
    "version": …, "component": "agent" | "exporter"}`; the exporter adds
    `agent_version`, the `version` in the newest snapshot it decoded from its
    agent (the one at its configured `mpk_url`), absent when it has decoded
    none. The key is `component`, since `role` already has three meanings (Q4).
    Step 2 has metriken-recorder call the probe hook's `enrich`, with no epoch,
    for every endpoint detection classifies as scraped (`Outcome::Scraped`,
    built at `detect.rs:252-254`, whichever path reached `check_scrape`,
    including `--endpoint http://h:4242/metrics`, which never tries the stream
    route), and widens `enrich`'s documentation (`probe.rs:66-69`) to scraped
    endpoints. A `Retry` or a timeout for a scraped endpoint arms the same late
    retry as for a streamed one (`adopt_enrich`, `recorder.rs:2615-2633`), and a
    late answer is a further recorder entry. On late activation a scraped
    endpoint enters `Activating` with its `enrich` on its own task, as a
    streamed one does (`recorder.rs:2483-2502`), since no tick waits on the
    probe hook (`:2402-2406`); today a late `Outcome::Scraped` is added with no
    `enrich` (`:2504-2513`). The probe hook fetches `/about` first and accepts
    only that object with `name` `rezolus` and `component` `agent` or
    `exporter`; any other answer is ignored. It writes recorder-layer metadata:
    `producer_name`, `producer_component`, and `version` for an agent or
    `exporter_version` for an exporter, since the exporter's version is not the
    agent's. An exporter's `/about` without `agent_version` is recorded as it
    stands. `Retry` is not used for it: `accept` drops a `Retry` reply whole
    (`probe.rs:113-122`), and after `ENRICH_RETRIES` (5, `recorder.rs:898`)
    failed retries the recording keeps none of the hook's metadata. Filling
    `agent_version` in later needs a further metriken-recorder change, listed as
    open in step 2. For an endpoint that predates `/about` it falls back to
    `/status` and the `/` banner, and records `producer_name` `rezolus` with,
    for a `/status` answer or a banner `Rezolus <version> Agent`,
    `producer_component` `agent` and `version` (on a streamed agent this repeats
    the handshake's `source` `rezolus`); for a banner `Rezolus <version>
    Exporter`, `producer_component` `exporter` and `exporter_version`.
    Detection's `not_streamable`, which calls the same `identify` and refuses
    anything identified as an agent ("a Rezolus agent is never scraped by 6.x",
    `src/recorder/agent_probe.rs:144-162`), keeps returning `Verdict::Scrape`
    for an exporter, as it does for `Other` today; only `enrich` records it. The
    hook writes no `source` label (D2). The agent's version is `version` on an
    agent recording and `agent_version` on an exporter's; rezolus analysis's
    `Context.agent_version` (filled from `data.version()`,
    `src/analysis/extract/mod.rs:556`) reads `version` only and must read both
    (step 3.4), and the names are part of Q4's `version`/`producer_version`
    question. A scrape has no epochs, so an agent or exporter upgraded during
    the recording is not reflected. For a legacy recording of an exporter
    scrape, which has no `/about` answer: (b), with a notice (D2's evidence 2).
    Open: whether `producer_component` `exporter` selects a different template,
    since the exporter writes histograms as `{name}_distribution_*`
    (`src/exporter/prometheus.rs:113-143`) and percentiles as gauges with a
    `percentile` label (`src/exporter/snapshot.rs:101-105`).
- **Q4.** The renames the inventory lists (`node`→`host`, `percentile`→
  `quantile` from the exporter, `role`'s three meanings (llm-perf's moves to
  `tier`, Q7), `kind` collisions, `version`/`producer_version`, and the probe
  hook's `agent_version`, `exporter_version`, `producer_name` and
  `producer_component` (Q3)): which change, in which release, with which
  read-time aliases. `instance` is the exception: a combined parquet already
  carries it as a series label (`"0"`, a sequence number, or the input's
  `instance` metadata, for example `"primary"` in rezolus
  `docs/parquet_metadata.md`), and the recorder-layer `instance` lands with step
  2, when the recorder stops writing an inferred `source` (D2). Candidates for
  reading a combined parquet's `instance`: (a) keep it as `instance` for parquet
  only, which leaves two meanings; (b) apply the clash rule, so it reads as
  `exported_instance`; (c) rename it on read to a user label. A selector
  `instance="primary"` written for a combined parquet changes meaning under (b)
  and (c).
- **Q5.** What categories become. A category (`members` in
  `inference-library.json` naming `vllm` and `sglang`) lists sources.
  Candidates: (a) `members` names kinds, refined by `service_kind`; (b) a
  category becomes a saved view, shipped by the registry as a view document;
  (c) both, with (a) as the read rule for existing category templates.
- **Q6.** Selectors. `--recording`, `--baseline` and `--experiment` match
  recording labels only, not metadata. What PromQL and the selectors see for
  `source` after the change: for a recording made from step 2 on, a `source`
  the user or a tool gave (rezolus `record`'s until step 3.4, llm-perf's until
  step 6), else the producer's own where its handshake declares one, else
  nothing; for a legacy recording, the legacy value as a label or nothing. Two
  agents that both alias to `source=rezolus` differ only by `host` (and
  `instance` after the change), so a selector `source=rezolus` names two
  presented recordings and is refused.
- **Q7.** The recorder layer of an in-process source. llm-perf's
  `RegistrySource` has no endpoint, no `instance` and no `/systeminfo`. Its
  `role=loadgen` label, and the `role=service` it puts on the server's
  endpoint (`src/recording.rs:128`), are asserted by llm-perf as the
  caller. Decided 2026-10-26: a tool that runs the recorder writes what a
  user would, so every label it supplies is in the user layer, including
  one on its own in-process source; there is no fourth layer. The producer
  layer of an in-process source holds what any producer declares (name,
  version, hostname), passed in `SourceIdentity`. llm-perf writes
  `tier=client` on its own source and `tier=server` on the server's
  endpoint in place of `role` (`role` has three meanings, Q4; `tier` is
  O3's label for a role in a benchmark), and the user's configuration can
  override both. llm-perf's KPI template on its own source
  (`service_queries` in its `RegistrySource` metadata) is producer-layer
  metadata, as in a handshake. The alternative of recording the load
  generator and the server under test into separate files and combining
  them later was rejected: llm-perf's one archive is written live on one
  clock, and two files need a second step and can cover different spans.
- **Q8.** Handshakes that change identity across epochs. A producer restart
  can bring a different producer on the same port, and an upgrade can
  rename a metric. Decided 2026-10-26:
  - A new version or build of the same producer is an event on one source:
    the recorder layer's entry for the new epoch (Q3) records it, and the
    viewer marks it. `__producer__` keeps one value per source. Each
    epoch's rows are evaluated with that epoch's handshake template, which
    covers a metric that keeps its name and changes meaning. The newest
    epoch's template gives the sections; how a chart is matched to an
    older epoch's query is settled with the template evaluation rule.
    Counters need nothing more, since no increase is taken across an epoch
    boundary (metriken-query `archive.rs:369-379`, one member per epoch); a
    renamed metric's old series ends and the new one starts.
  - A reconnect whose handshake names a different producer starts a new
    source, since none of its series continue the old one's.
  - The change reaches a subscriber as a dendro replication frame,
    `SourceUpdate { source, ts, labels, metadata }`, sent by dendro's
    `ArchivePublisher`. Today the publisher reads the sources once, when it
    opens (dendro `src/replicate/publisher.rs:214`), and sends each
    source's metadata only in its `Handshake`: a subscriber tailing an
    archive keeps the identity it read at connect after the recorder
    patches it (metriken-recorder `restart.rs`, `metadata_patch`), and
    receives no `Handshake` for a source created after it connected. The
    archive records no time for a metadata change
    (`update_source_metadata` and `update_source_labels` are plain
    updates, `src/archive/sqlite.rs:2890`, `:2918`), so `SourceUpdate.ts`
    is the poll time; a subscriber that needs the edit time reads the
    entry's `ts` from the layer keys the frame carries (Q2), since dendro
    does not interpret the layers. `Frame` is `#[non_exhaustive]` and a
    subscriber on dendro 0.3.0 or later skips a kind it does not know
    (WIRE.md §7). Nothing ships today that follows a recorder over
    replication; this is for the subscriber that will.

  Splitting at every version change was rejected: it makes a run one
  line in two pieces for every consumer, for a case per-epoch template
  evaluation already handles. The candidates were (a) the newest epoch's
  `__producer__` and template for the whole recording, (b) both per epoch,
  and (c) a new source when the producer's name changes, with (a)
  within one producer's epochs.

Presentation and query rules:

- **Q9.** Two in-process sources of one process share an epoch. Candidates:
  (a) the process group key is the epoch alone, so they union, which is
  correct when they are one process's metrics split in two and conflicts
  under S4 when both carry one series; (b) the key is the epoch and the
  producer name, so a process exposing two producers' sources keeps them
  apart.
- **Q10.** A recorder- or producer-layer key whose values differ among the
  sources of one process group (two recorders' `instance`, a `host` that
  changed between copies). Candidates: (a) no series label for the key, each
  value shown in the information panel, and a selector on the key matching
  the sources that carry the value; (b) the value of the source with the
  newest first timestamp. Asserted labels (user-layer labels) are decided
  (data model: a carried value fills a gap, two different values split).
- **Q13.** The format of a cross-producer requirement in the template
  schema (a section-level `requires` list of kinds, or a per-query kind),
  decided with Q21.
- **Q15.** An extra check for copies without an epoch, a shared uuid or a
  recorder-layer key (two copies of one parquet written before v5.21.0,
  which carries no `endpoint`). Candidates: (a)
  none, as written; (b) a warning when two sources' first segments or row
  groups are byte-equal.
- **Q16.** The value S4 returns at a conflict. Decided 2026-10-21: the
  finer source's value; if both have the same interval, the value of the
  source with the earlier first timestamp; further ties by S3's rule (the
  lowest uuid, then catalog order). The consumer reports a conflict naming
  both sources, the series and the first timestamp. The candidates were (a)
  the finer source's, (b) none (a gap at that timestamp), so a conflict is
  visible in the chart, and (c) the value of the source with the earlier
  first timestamp; the decision is (a) with (c) as its tie rule.
- **Q17.** `combine` into a parquet output drops `producer_epoch` and
  `producer_epochs`, so the result cannot be unioned. Candidates: (a)
  carry them under `per_source_metadata`, which is keyed by `source` and so
  needs a key per source when copies share a `source`; (b) warn that the
  output loses process identity; (c) retire combine-to-parquet in favour
  of `.dendro`.
- **Q18.** The same host, overlapping spans, different epochs of one kind
  (4e). Whether one series per `instance` is right, or the default should
  be a comparison by presented recording at absolute time.

Templates:

- **Q19.** The template schema that marks a template as needing
  restriction, and what a released reader does with it. Candidates: (a) a
  new top-level field in the KPI template, which released readers ignore
  (`ServiceExtension` has no `deny_unknown_fields` at v5.28.0) and so
  evaluate unrestricted, which is the failure this exists to prevent; (b) a
  new metadata key in place of `service_queries`, which released readers do
  not read, so they render the registry's template for the `source` or
  none; (c) a change that a v5.28.0 parse rejects, such as a new key inside
  a KPI's `check` object, which v5.28.0 refuses (`CheckRaw` is
  `deny_unknown_fields`, `crates/dashboard/src/service_extension.rs:163-164`).
  v5.28.0's `src/viewer/metadata.rs:269-276` drops a `service_queries` it
  cannot parse without a notice, and its fallback then renders the
  registry's template for the source, with its `source=` matchers (for a
  combined parquet by each `per_source_metadata` source, `:302-311`; for a
  single file by the top-level `source`, `:313-320`), so (c) degrades to
  (b) without a notice.
- **Q20.** Where the registry layer sits in the stack. Candidates: (a) the
  registry layer is present only when the producer layer is absent or does
  not parse, which is today's whole-template fallback and keeps an old
  registry KPI out of a newer producer's template; (b) a per-id merge, the
  registry's entries filling ids the producer's layer lacks. (a) is the
  proposed answer: it is additive (the registry layer stays readable, and
  `--registry-templates` becomes a user layer that hides both parts beneath
  and adds the registry's template, D4), and (b) mixes templates written
  against different metric names. The same question applies to the two
  other boundaries P5 creates: the recorder default against the registry
  (the recorder default replaces the registry's whole template, or fills
  ids it lacks), and the producer's layer against the recorder default
  (whole or per id), each decided per template part (KPIs, sections).
- **Q21.** The merge rules. Candidates for each:
  - Ids: a section's route and a KPI's id, with a KPI without an id keyed
    by its title within its section.
  - Ordering of added sections: (a) appended after the layer beneath, in
    layer order; (b) placed by an explicit `after: <id>` in the layer,
    falling back to (a).
  - Hiding versus replacing: replacing keeps the position of the replaced
    entry; hiding removes it from the render; hide-everything-beneath (D4)
    is a flag on the layer, per part. A hide-all layer records the ids it
    hid when it was written; on reapply (another producer version, another
    archive) an id beneath that is not on that list is reported, so a KPI
    the producer added later is not hidden without a message.
  - A layer written for one producer version applied to another: (a) the
    layer records the producer name and version it was written against, and
    an id it names that the stack lacks is reported, not an error; (b) the
    layer is applied only to the version it names.
  - The evaluation rule for a user KPI that replaces a producer KPI: (a)
    the replaced KPI's rule (series' own labels first), since it is
    written against the same series; (b) recording labels, since the user
    wrote it against the archive.
- **Q22.** The layer format: a schema-1 section template plus operations,
  or a new layered schema.
- **Q23.** How `service_queries` (KPI templates) and `dashboard` (section
  templates) converge: one template model, or two with the same layering.

Views:

- **Q24.** The document kinds (`view`, `template-layer`, `labels`,
  `report`) and the view document's schema (grouping list, alignment,
  layout, navigation arrangement, template layer references, label layer
  references, hand-picked sets, time range) and its versioning rules.
  Whether a metadata key (`version`) can be a grouping key: metadata is not
  a series label. What `--baseline`/`--experiment`, the viewer's
  `set_compare_requested` and `?compare=1` become; the proposed answer is
  that they select a two-group view. Which group is the reference of a diff
  view: today `arm_slots` picks by value (`arm=baseline`/`experiment`,
  matched ignoring case), else by archive order with a note naming the
  choice. Candidates: (a) the same rule over group values, with the view
  able to name the reference; (b) the first group in the grouping order.
  With three or more level-1 groups (arms or time clusters): (a) one
  reference, the `baseline` arm, else the earliest-starting group, every
  other group drawn against it; (b) the user picks, defaulting to (a); (c)
  diff views are offered for a chosen pair only.
- **Q25.** Applying a view to a new archive. For a selector that matches no
  recording: (a) the group is shown empty with a notice; (b) the view is
  refused with a listing. A recording no selector names is shown as
  unplaced, with A13's notice (decided). For a selector that matches several
  presented recordings: (a) they form the group, aggregated or expanded as
  the view says; (b) refused, as `--recording` refuses today. The alignment
  conflicts of Q26 apply when the new archive's recordings carry different
  event kinds.
- **Q26.** Alignment details. The partial-overlap threshold for offering
  elapsed alignment, and how trim is computed across tables of different
  cadence. Conflicts:
  - Elapsed alignment and shared context shown once: shared context has no
    start of its own. Candidates: shown at absolute time beside the aligned
    groups; aligned to the first group's start; hidden under elapsed
    alignment.
  - A group of several presented recordings has several starts.
    Candidates: the earliest start in the group; each recording aligned to
    its own start before aggregation.
  - Trim with no common window renders nothing. Candidates: refuse trim
    with a message naming the groups that do not overlap; fall back to
    elapsed alignment with a notice.
  - Three or more level-1 groups whose spans overlap in part pairwise (A
    overlaps B, B overlaps C, A does not overlap C). Candidates: absolute
    time, since some pairs overlap; elapsed, since not every pair does;
    elapsed with the overlapping pairs noted.
  - Event alignment when only some recordings carry the event kind.
    Candidates: groups without the event are left out with a notice; shown
    at absolute time; the view is refused.
- **Q27.** Saving a view of an input without documents. A v4 archive is
  never given documents (dendro refuses `put_document` as `SchemaTooOld`; a
  copy made through `dendro::rewrite` is a v5 archive and takes them).
  Candidates for a `.rez`, parquet or v4 `.dendro`: (a) the view can only be
  exported to a file; (b) saving converts the input to a v5 `.dendro`
  beside it; (c) both, the user choosing.
- **Q28.** Template copies under grouping: one copy per group (queries
  scoped to the group's presented recordings) or one copy per presented
  recording with groups overlaid; how side-by-side views are arranged; and
  how the WASM viewer's comparison, which feeds two recordings
  (`comparison_order`), shows groups of several recordings each.
- **Q29.** Cost with many sources. Measured on 30-minute slices of the
  9.6-hour bench archive copied N times (labels `host=h1..hN`):
  - `ArchiveReader::from_catalog` probes every source at open. For each
    table it fetches the first sealed segment, one more segment for each
    names fingerprint not seen before, and a materialized WAL tail
    (`table_probes`, metriken-storage `reader.rs:149-185`). An earlier
    per-source cost figure is dropped here because its run is not recorded.
  - dendro's `docs/journal/2026-10-19-segment-index-table.md` measured a
    metriken prototype (`proto/segment-index`) that stores a three-tier
    catalog index (metric names and kinds, label keys per metric, values per
    key up to 64; about 332 bytes a segment) and opens from it without
    fetching a segment. On an Apple M-series laptop, cold, each "before"
    figure from an earlier session than its "after" (as that journal
    states): reading every index of the N=64 archive with the `a-64` index
    fell from 2798 ms (column) to 227 ms (table); a cold open with the index
    on fell from 2239 ms to 1538 ms median at N=64, and from 176.9 ms to
    95.9 ms at N=4; with no catalog index, a cold N=64 open went from 2977
    ms to 2476 ms. On the one-source 9.6-hour archive the catalog index
    opened cold no faster than the fingerprint: `b-64` in 304 ms against 306
    to 309 ms for the fingerprint, and `a-64` in 351 ms. About 1.4 s of a
    cold N=64 open is not the index read and was not broken down.
  - Routing prunes recordings that cannot match before reading
    (metriken-query `archive.rs` `Plan::new`, through `owners_of` and
    `translate_filter`). A host-restricted query stayed at about 4 ms and
    14 fetches from one source for N=1 to 64.
  - Label listings take no selector and read every recording. The combined
    view's scoped name listing took 1.27 s cold at N=4, 6.0 s at N=16 and
    43.5 s at N=64 before rezolus listed through `owners_of` and `holds_*`.
  - Decided direction: metriken-storage stores each segment's catalog
    index (at least the metric-name set) in `segment_indexes` under a new
    magic, so open reads the catalog only; a column projection drops the
    index; scope-aware listings (`*_labels_matching(name, filter)` in
    metriken-storage, and a `MetricsSource` method in metriken-query whose
    default filters after listing).
  - Open: per-recording template copies multiply each section's queries by
    the recording count. How a group's members aggregate is decided (O1, the
    chart's declaration); whether an aggregated group costs one query per
    group or a reduction over per-recording results is not. Copies unioned
    by S1 add sources to read without adding presented recordings; their
    cost is not measured.
- **Q30.** Report migration. A report's selection, pins and events live in
  the anchor recording's metadata in a `.dendro` or `.rez`, and in the
  footer of a parquet report. Whether readers lift them into documents at
  read time, whether `recording upgrade --to dendro` writes them as
  documents, and how long the metadata form is read. Candidates: (a)
  readers lift the metadata form into documents at read time, and
  `recording upgrade --to dendro` writes them, the metadata form read for
  two releases; (b) only `upgrade` converts, and readers keep reading the
  metadata form indefinitely; (c) both (a) and (b)'s indefinite read.
- **Q31.** Events. MCP `add_event` writes one recording and refuses a
  multi-recording archive without a selector (`add_events_selected` in
  `src/parquet_tools/events.rs`); `recording annotate --event` writes a copy
  into every recording (`src/parquet_tools/annotate.rs`), and so do
  `record`'s run markers (`src/recorder/mod.rs`, through `patch_metadata`).
  Which events become archive-level documents (run markers, a user's
  annotation of the whole recording) and which stay per source (a producer
  restart), whether per-source copies are merged into one on read, and how
  events from two sources of one process group are deduplicated. An event
  written to a presented recording of several sources is an archive-level
  document naming its sources' uuids; no tool addresses a group of a
  grouping (Every consumer). Candidates for the per-source copies: (a)
  merged on read when kind, time and description are equal; (b) kept, each
  drawn once per presented recording, so a process group draws one copy; (c)
  `upgrade` folds them into one document and readers do nothing.
- **Q32.** The explorer and a chart's scope (D1). A chart's query as served
  today already contains its scope's matchers (`restrict_query` runs in the
  backend), so its text pasted into the Query Explorer reads one recording.
  The Expand link (`SingleChartView`, `app.js:1475-1497`,
  `features/explorers.js`) loads a chart's query into an editable input and
  re-runs it; under D1 the scope must travel with the plot into that view.
  Candidates: (a) the query carries its scope, shown beside the text and
  editable or removable; (b) the scope is written into the text as
  matchers on copy, which brings back the splicing D1 removes for this one
  path; (c) the scope is dropped and the query reads every recording. (a)
  is the proposed answer.
- **Q34.** Retried attempts. Each attempt of a retried systemslab job is a
  separate set of recordings. Candidates for marking a superseded attempt:
  (a) a user label (`superseded=true`) as a user entry on the superseded
  attempt's sources (Q40); (b) nothing in the file, left to systemslab's own
  views.
- **Q35.** How the registry maps names to kinds. Registry entries are named
  by producer and role (`vllm`, `vllm-prefill`, `sglang-router`), and D2
  and the kind of a source without a producer layer read through them.
  Candidates: (a) each entry declares its producer and an optional
  `service_kind` (`vllm-prefill` is producer `vllm`, `service_kind`
  `prefill`), and lookup is by producer and `service_kind`, then producer;
  (b) entry names stay flat, and a legacy name reads as a kind whole. An
  entry's `aliases` (`ServiceExtension.aliases`, indexed by the registry
  beside `service_name`, `service_extension.rs:13`, `:568`; `llm-bench` for
  llm-perf, `redis` for valkey) are other producer names under (a), and are
  unrelated to the `alias` label (P2).
- **Q36.** The format of a chart's aggregation declaration (P3): a field
  per plot (`aggregate: sum | mean | max | per_member`). The defaults are
  decided (A6: counters and rates sum, histograms merge, an undeclared
  gauge goes by unit, percent mean, bytes sum, else per member with a
  notice); only the field's spelling and where it sits in each template
  schema remain.
- **Q37.** The open-time overlay's interface (O5): an argument to
  metriken-query's archive open, a file format shared with exported label
  layers, or both; and how a caller (systemslab, a script) passes it to
  the `dashboard` crate.
- **Q38.** A record-time flag that writes a user layer (for example to beat
  a broken producer template without opening the viewer), since
  `template=FILE` is now the recorder default below the producer (P5).
  Candidates: (a) none until needed; (b) `--endpoint url,user_template=FILE`
  writing a user layer document; (c) `recording annotate --template` only,
  after recording.
- **Q39.** Bridge and cluster rules. The rule in P6 is the starting point;
  the exact algorithm is settled with a prototype against real archives.
  Required test scenarios, each with its expected presentation written
  before the prototype runs:
  - Cross-arm cluster identity under A9: arm `a` in two clusters, arm `b`
    in three; and concurrent arms started 30 s apart. Candidate: clusters
    formed over the whole archive, with the same start for every arm (this
    would amend step 5 and case 4's Alignment).
  - Two process groups of one epoch in one cluster through another kind's
    overlap (P1 09:00–10:00, an llm-perf run 09:50–11:00, P2 10:30–11:00)
    with a bridge C of that epoch: whether C's 10:00–10:30 part, which joins
    nothing, is drawn; and the cut when the two groups' spans overlap
    without an R3 overlap.
  - The three-arm chain (arms at 09:00–11:00, 10:00–12:00, 11:30–13:00):
    no bridge across arms (P6); check it against the expected absolute
    alignment.
  - An unarmed continuous recording of one epoch beside armed per-day
    recordings of the same agent: it is shared context and not a bridge, so
    the union rule would join the days through it; decide whether it is
    sliced at the arms' clusters or kept out of the union. Also which part
    of an `arm`-split group such a recording joins.
  - A presented recording whose span has internal gaps. Candidate: its span
    runs from its first row to its last.
  - Detection in one pass against every source, bridges included, with
    plain overlap across epochs and kinds.
  - Nested bridges: 09:00–18:00 containing 09:00–13:00 containing two
    shorter recordings that do not overlap.
  - Epochless bridges (plain span overlap).
  - A piece crossing an epoch boundary. Candidate: split at the `from_ts`.
  - The chain: 09:00–11:00, 10:00–12:00, 11:30–13:00 (the middle is a bridge
    under P6).
  - A sweep under one long agent recording: llm-perf run back to back at
    concurrency 1, 2 and 4 with one continuous server agent. Candidates:
    three clusters with the agent sliced; one cluster grouped by
    `concurrency`.
  - The case 6 fixture of three `arm=a` recordings (10:00 ×2, 11:00) with
    its epochs stated: three agents, three epochs, so no two union; and the
    variant where the two 10:00 recordings share an epoch, which unions
    them under R3 and leaves two presented recordings.
- **Q40.** User-layer writes after record time (raised and decided 2026-10-27).
  A write after record time adds an entry to the user layer (Q2), so nothing is
  overwritten (M6), and `recording annotate` needs no `--overwrite`. A label
  the user assigns to specific sources, from the viewer (any key) or with
  `recording annotate`, is a user entry; a rule meant for other archives is a
  label layer document saved with a view. Copies of one source merge their user
  entries by `id` (Q2).


Closed by the owner on 2026-10-20: Q11 (one epoch over disjoint spans gives
separate presented recordings, O2), Q12 (a cross-producer section declares
its binding; accepted recommendation), Q14 (`arm` and time clusters
compare; other labels group on one chart, O1, P4), Q33 (systemslab's views
stay in its own storage and are not pushed into files, P5). Q29's
aggregation semantics are decided by P3, and S8 follows it.

Closed (was V9): released copiers dropping documents.
dendro 0.6.0 (#42) creates every archive at schema 5, and 0.5.1 and earlier
refuse it ("unsupported archive schema version 5") rather than copying it
without its documents. rezolus moved to dendro 0.6 in #1506. The cost is
that released rezolus (5.x, on dendro 0.3.3 in v5.28.0) opens no archive
written by the new dendro.

## Order of work

1. dendro:
   - ~~The `documents` table, released with its schema version bumped at
     create.~~ Done in dendro 0.6.0; rezolus on it since #1506.
   - A kind filter on `copy_documents_into`, for `recording views import`.
     Needs: Q24.
   - Nothing for the union: dendro already keeps a copy's uuid and allows
     two sources with one uuid.
   - The `SourceUpdate` replication frame (Q8), and `ArchivePublisher`
     changed to detect, on each `next`, a source whose labels or metadata
     changed and a source created since it opened, sending `SourceUpdate`
     or `Handshake`. `SourceUpdate.ts` is the poll time (Q8).
2. metriken, released:
   - The producer identity (metriken-exposition macro and handshake keys, with
     the producer's hostname; the keys in metriken-model `identity`),
     `SourceIdentity` carrying it for an in-process source (Q7), and the three
     layers in metriken-recorder and metriken-storage, stored as Q2 decides,
     with the one function that writes the layers and the merged labels. The
     recorder starts a new source when a reconnect's handshake names a
     different producer, and writes a patched identity through the layer
     function; dendro's publisher carries it to subscribers (step 1). The
     recorder writes no `source` of its own: no inferred name
     (`infer_source_name`), no `local` default for an in-process source
     (`recorder.rs:1541-1550`), and rezolus's probe hook stops writing
     `source=rezolus` (`src/recorder/agent_probe.rs:191`; D2). The at-open
     recorder entry carries `instance` (Q1, Q4). The recorder layer's entries
     replace the flat epoch keys for new writes; readers take the entries, and
     the flat keys only on a source without `metriken.layer.recorder` (Q3 reads
     old recordings unrewritten): metriken-recorder `restart.rs` and
     `add_source` (`recorder.rs:1535`, `:1568`), the producer's own handshake
     metadata (metriken-exposition `stream/frames.rs:352-359`, moved into the
     entry's `epoch`, Q2), metriken-storage `to_dendro.rs:61-64`, and
     metriken-storage `convert/mod.rs` (`epoch_patch`, `:229`) write them,
     `epoch_spans` (`process.rs:125`) reads them, dendro names them
     (`keys::PRODUCER_EPOCHS`, `src/lib.rs:228`), metriken-storage
     `convert/mod.rs:170` reads them, and rezolus refuses them as `--metadata`
     (`src/recorder/config.rs:212-222`, test `:755`, through the
     `parquet_metadata.rs` constants) and names them in help and messages
     (`src/recorder/mod.rs:289`, `:667`, `src/hindsight/mod.rs:372`). The
     refusal of those keys as caller metadata, there and in metriken-recorder
     (`reserved`, `recorder.rs:671-676`), extends to every `metriken.`-prefixed
     key, and keeps refusing the two flat names. Needs, for subscribers: the
     dendro frame above. metriken-recorder also calls the probe hook's
     `enrich` for every endpoint detection classifies as scraped, with the
     late retry on `Retry` and on late activation (today `first_enrich` asks
     streamed endpoints only, `recorder.rs:1885`), writing its metadata into
     the recorder layer (Q3); the warnings and docs that say the hook serves
     a streamed endpoint (`recorder.rs:2292`, `:2339`, `:2623`,
     `ENRICH_RETRIES`, `late_enrich`) change with it. Open: a way to fill
     in a value the first answer lacked (an exporter's `agent_version`), since
     `Retry` discards the whole answer (Q3).

     Built in parts (2026-10-27): the producer identity keys and macro (#327),
     the layers and `layers::append` (#326), then metriken-recorder writing
     every label and metadata write as a layer entry (#328). #328's calls,
     open to reversal: `transport` (`stream`, `prometheus`, `local`) names the
     transport in the at-open recorder entry, beside `instance` as a label and
     `sampling_interval_ms`; the probe hook's labels and metadata go in the
     recorder layer at open, its `source` label dropped, and a late reply's
     metadata in a recorder entry with no epoch, so it delimits no span; a
     caller's `events` is stored as given outside the layers, since `rezolus
     record` writes its run markers there; a local source's `SourceIdentity`
     labels are user-layer labels and its metadata producer-layer metadata,
     with `SourceIdentity::producer` for its producer identity; a scrape's
     HELP text is a recorder entry, still skipped when the caller set
     `descriptions`; a restart writes, per new epoch, a recorder entry and a
     producer entry (that process's handshake) from its first row, so an epoch
     replaced before it was written keeps its handshake; the hook's labels and
     metadata sit below the handshake's, where the flat write put them above;
     dendro's keys are left out of a handshake's entry and its `encoder` and
     `writer_sessions` refused from a caller; `Probe::merge` is not called,
     since the fold takes the newest value and unions `descriptions`; the
     hook's late labels are still not applied. Left for later PRs: a new
     source when a reconnect's handshake names a different producer, the probe
     hook for scraped endpoints and late activation, and `__producer__` with
     the clash rule. The plan above is as written before #328, and its
     `recorder.rs` line references are to the code before it. #328 also leaves
     a URL's user name and password out of `instance` and writes only newly
     described names of a scrape's HELP text.
   - `__producer__`, and the one clash rule on every path, including
     `MultiParquetSource`.
   - In metriken-query, each producer epoch's rows evaluated with that
     epoch's handshake template (Q8), as part of the template evaluation rule
     (the D1 item below).
   - ~~In metriken-query, the per-sample `avg_over_time` and per-row
     unstrided histogram behaviour documented (S12).~~ Done in the
     metriken-query crate docs.
   - In metriken-storage and metriken-query, process groups (S1, S2) and
     conflicts (S4). Needs: Q9 and Q16 for the grouping key and the
     conflict value; prerequisite for case 3.
   - In metriken-query, the S9 per-source union, with S3's span selection
     (which source's results are used in each span, which needs S9's
     coverage rule) and S5 to S8: each source evaluated alone and the
     results combined per series per span, never a delta across sources.
     Needs: the process groups above; prerequisite for case 3.
   - In metriken-query the open-time label overlay (O5), a generic caller
     API. Needs: Q37.
   - In metriken-query the scope input (D1), resolving to process groups,
     then the template evaluation rule, for the producer and registry
     layers.
   - The catalog index and scope-aware listings. Needs: Q29.
3. rezolus, in this order:
   1. ~~Open and merge the combined view.~~ Done: #1504.
   2. The restriction as the engine scope (D1) in both viewers' query APIs
      and the frontend (replacing `applyScope`'s use of `injectLabel` and
      `metadata.scope`), stored queries carrying their scope, combined
      parquets and A/B tarballs (or retire `--ab` tarballs), with
      `restrict_query`'s pin rule removed; released. Needs: step 2's scope,
      Q30, Q32.
   3. Templates without `source=` (cachecannon and llm-perf in the same
      step), behind the template schema of Q19. Needs: Q19.
   4. `alias`, `tier`, `service_kind` and per-endpoint labels in `record`, and
      the layers; the template-to-recording binding in `recording check` by
      kind; template lookup by kind for the registry layer; template and label
      layers in P5's order, including `template=` becoming the recorder default
      below the producer's layer; the compatibility list; `instance` in
      combined parquets; the two clash rules that are rezolus code
      (`combine`'s, and the agent's external metrics) moved to the one rule;
      the internal comparisons with `"rezolus"` (`filter.rs:145`,
      `dashboard/mod.rs:775`, `crates/viewer/src/lib.rs:1663`,
      `source_kind.rs:45`, and the others in Today) reading the kind through
      D2; `dashboard=FILE`; the per-endpoint `source=` written as `alias`
      (Compatibility); `recording upgrade` and `recording convert` writing the
      layers, and `upgrade` accepting a pre-layer `.dendro` and keeping its
      uuids (Q3); `arm=` (#1504) and `host=` (#1538) as user-layer entries;
      `combine`'s label stamping and `annotate`'s metadata writes through the
      metriken-storage layer function (Q2); `/about` on the agent and the
      exporter, read by the probe hook (Q3), which needs step 2's probe call
      for a scraped endpoint, and rezolus analysis reading `agent_version`.
      Needs: step 2, Q4, Q5, Q6, Q20, Q21, Q22, Q23, Q35, Q38.
   5. The presentation (cases 1 to 6) as one `dashboard` crate function
      called by the viewer, MCP and `recording check`, with notices in MCP's
      tool results and on the CLI's stderr, and the per-recording MCP tools
      iterating presented recordings (P7); two-level grouping (P1),
      aggregation by plot type and the split toggle (P3), time clusters
      compared like `arm` (P4, P6), and the unidentified group (P9);
      cross-producer sections with their binding and host matching; views,
      template layers and label layers as documents, interactive
      relabels saved as rules, and labels saved from the viewer into the
      user layer (P1, Q40); alignment; reports, pins and
      group events as documents; `combine` as a plain union reporting
      document forks and placing new recordings beside a saved view, with
      the shared-uuid refusal removed; `recording upgrade` writing a view
      document; `recording views import` and view export; Save-as-Report
      writing `.dendro` with the current presentation as its default view
      and the archive's own layers only (P8), and the read-build ingest path
      for the static site; histogram charts passing a stride at the step
      (S12); the `arm_slots` and report-anchor compatibility items. Needs:
      step 2's process groups and layer function, Q10, Q13, Q15 to Q18,
      Q24 to Q28, Q30, Q31, Q36, Q39.
4. The renames (Q4, except `instance`), each with read-time aliases.
5. systemslab, as a consumer (planned): render each archive through the
   `dashboard` crate with the archive's own layers; keep its own views at
   its own levels in its own storage; pass `--label
   tier=<value>` for a job whose spec gives a tier (O4); optionally pass an
   open-time overlay. Needs: step 3.5, Q34, Q37.
6. llm-perf (Q7): `tier=client` and `tier=server` in place of `role`, the
   producer name and hostname in `SourceIdentity`, a `[metrics] server_host`
   asserted as `host` on the server endpoint (Q1), a `[metrics]` key that
   overrides each `tier`, and a new key naming the server's `alias`, with
   `server_source` accepted as an alias for it with a warning and written as
   `alias` rather than as a `source` label (`src/recording.rs:128-131`,
   Compatibility), and no `source` label on its own source (`:172`), whose
   producer name `SourceIdentity` carries. Needs: step 2, and step 3.4 for
   registry lookup by `alias`.
