# metriken-dashboard and metriken-viewer: one viewer for every metriken producer

**Status:** OPEN. Design 2026-10-12; nothing built. This is path step 7 of
[one recording stack](2026-10-08-one-recording-stack.md) (piece 5). "Step N
below" means this entry's own order of work. The owner decided step 7's
scope on 2026-10-12, before work begins; the questions, their options and
the decisions are under "Scope decisions", and the design below follows
them.

## Goal

The plan's GO criterion 5: `cachecannon view` and `rezolus view` show the same
dashboard for the same archive, both served by `metriken-viewer`. rezolus
mounts the library for `rezolus view` and builds its static site viewer from
it; cachecannon mounts it in place of its own viewer. rezolus 6.0.0 is
released after this step (the rezolus 6.0 entry, rezolus
`docs/journal/2026-10-08-6-0-release-readiness.md`, decision 7).

## Today

Sources: metriken origin/main d2bf74d, rezolus upstream/main 15c77844
(6.0.0-alpha.46), cachecannon main 74056cbb, llm-perf main edf0f20a,
systemslab main 3329ebd1. Line counts are total / production (before the
first `#[cfg(test)]` module).

### rezolus

| Path | Lines | What |
|---|---|---|
| `crates/dashboard/src/` generic parts: `plot.rs` (apart from two functions that name rezolus metrics, see "Crates"), `service_extension.rs`, `metric_catalog.rs`, `display_wire.rs`, `events.rs`, `capture_alias.rs`, `source_kind.rs`, `lib.rs`, `dashboard/{mod,service,category,query_explorer}.rs` | 4,817 / 2,757 | the `View`/`Group`/`Plot` model, `ServiceExtension` and `TemplateRegistry`, KPI checks, the metric catalog, the display wire format, events, capture aliases, source classification, the section context and the template-driven and simple-capture sections |
| `crates/dashboard/src/dashboard/` rezolus sections: `overview`, `exceptions`, `cpu`, `gpu`, `hw_sensors`, `memory`, `network`, `scheduler`, `syscall`, `softirq`, `blockio`, `filesystem`, `ext4`, `xfs`, `cgroups`, `rezolus` | 4,665 / 3,791 | Rust generators for rezolus's own samplers, listed in `SECTION_META` (`dashboard/mod.rs:27`). Several read the recording to decide what to draw (`gpu.rs` enumerates `(vendor, id)` pairs). |
| `crates/dashboard/templates/` | 11 files, 52,964 bytes | `ServiceExtension` JSON for cachecannon, llm-perf, vllm (3), sglang (4), valkey, inference-library (a category), embedded with `include_dir!` |
| `crates/dashboard/src/main.rs` | 88 | dumps every section's JSON |
| `src/viewer/` (not `tui/`) | 8,091 / about 5,320 | the axum server. `mod.rs` (2,042 / 1,343) is the clap command, `Config`, the file, A/B, `.rez` and live startup modes and recording selection; `routes.rs` (1,168 / 704) the router and embedded assets; `actions.rs` (1,039 / 966) upload, URL load, attach and detach, connect, reset, save; `state.rs` (457 / 436); `metadata.rs` (525 / 504) section regeneration and `service_extensions_from_metadata`; `follow.rs` (978 / 494) following an archive whose writer is running; `live.rs` (732 / 377); `capture_registry.rs` (334 / 224); `ab_extract.rs` (250 / 122); `proxy_allow.rs` (176 / 83); `report_save*.rs` (390 / 68) |
| `src/viewer/tui/` | 2,258 / about 1,520 | `view --tui`, ratatui |
| `crates/viewer/` | 1,767 / 1,374, plus `build.sh` | the WASM viewer: `Viewer` and `WasmCaptureRegistry` behind `#[wasm_bindgen]`, its own template detection (`detect_and_validate_service_exts`, `lib.rs:576`) and Save as Report |
| `crates/report-save/` | 1,464 / 748 | Save as Report builders shared by both backends |
| `crates/rez/src/{reader,live}.rs` | 3,965 / about 450 | `RezReader`, a 157-line wrapper over metriken-storage's `ArchiveReader` implementing `MetricsSource`, and `LiveReader`, which reopens it as the writer appends |
| `src/viewer/assets/` | 66 files, 2.1 MB | the frontend: 20,417 lines of JS and 4,377 of CSS, plus `echarts.min.js` (1,024,740 bytes), `mithril.js` and `lit/` vendored. 634 KB as a gzip -9 tarball |
| `site/viewer/` | `index.html` 34, `lib/script.js` 607, `lib/viewer_api.js` 187; 73 symlinks, 62 in `lib/` and 11 in `templates/` (the 11 point at `crates/dashboard/templates/`), and a generated `templates/manifest.json`; `data/` 15 demo files (parquet), 55 MB | the static viewer on GitHub Pages. `script.js` and `viewer_api.js` are hand-kept copies of the server's (the `viewer-parity` skill); everything else in `lib/` links into `src/viewer/assets/lib/` |
| `tests/*.mjs` | 57 files, 5,564 lines | the JS suite, `node --test tests/*.mjs`; four `wasm_viewer_*` files drive the built bundle, one (`wasm_viewer_rez_archive`) runs `cargo` to build a fixture, `display_wire_fixture` reads a fixture written by `cargo test -p dashboard`, `viewer_link_parity` pins `src/mcp/link.rs` to `url_state.js` |
| `tests/viewer_smoke.sh`, `tests/display_synthetic.sh` | | server end to end; the second starts a viewer for the 7 `VIEWER_URL` tests in `display_synthetic.test.mjs` |
| CI | | `cargo.yml` job `wasm-viewer` (wasm32 check of `rez` without `write`, `build.sh`, the JS suite, template symlinks, binaryen structure check); `pages.yml` (builds the bundle, `cp -rL site`, deploys); `release.yml` `build-viewer-static` (the `rezolus-viewer-<tag>.tar.gz` bundle); `viewer-symlinks.yml` |

Coupling from `src/viewer` to the rest of the binary, by reference count:
`crate::recorder::*` 40 (`.rez` writers and seal policy, mostly in test
fixtures; `stream` and two constants in `live.rs`), `crate::mcp::*` 25 (`RecordingSelector` and
its helpers, for `--baseline`/`--experiment`), `crate::parquet_metadata` keys
20 (`KEY_REPORT`, `KEY_SELECTION`, `KEY_EVENTS`, `AbContainers`, ...),
`crate::rez_reader` 11, `crate::dendro_copy` 10, `crate::hindsight` 7
(the buffer writer and its metadata, in `live.rs`, `actions.rs`, `follow.rs`), `crate::parquet_tools::combine` 1.

Change rate since 2026-07-14, out of 331 rezolus commits: 80 touch the
viewer (`src/viewer`, `crates/viewer`, `crates/dashboard`, assets); 35 touch
`src/viewer/assets`, 17 of them also touching Rust in the viewer or dashboard.
19 touch the rezolus sections, and all 19 also touch `src/agent/samplers` (a
sampler and its section land in one commit, for example d610218d, 696aa241,
d18caaee).

**Templates.** The server resolves a recording's dashboard by
`service_extensions_from_metadata` (`src/viewer/metadata.rs:258`):
`service_queries` in the metadata first, then `per_source_metadata`, then the
built-in registry by source name. The WASM viewer's
`detect_and_validate_service_exts` (`crates/viewer/src/lib.rs:576`) consults
the registry by source name only, so a template embedded by
`recording annotate --queries` is shown by `rezolus view` and not by the
static site. That is the parity gap path step 5 has to close in both shells.

**Assets.** Release builds embed `src/viewer/assets` with `include_dir!`
(`routes.rs:39`) and serve them with an ETag and `no-cache`
(`asset_response`). `developer-mode` serves them with `ServeDir` from
`src/viewer/assets` relative to the working directory, reads templates from
disk, and reloads the page on change (`notify` plus `tower-livereload`,
`mod.rs:1313`). The page assumes it is mounted at `/`: `index.html` sets
`<base href="/">` and the JS requests `/api/v1/...` and `/data/...` by
absolute path.

**Live mode.** `live.rs` runs its own loop over `recorder::stream`
(`Subscription::connect`, `pump`) into a temporary dendro archive through
hindsight's buffer writer, and reads it through `rez::live::LiveReader`.
`actions::fetch_agent_info` parses the agent's `/` banner and `/systeminfo`.
Path step 4 rewrites this onto `metriken-recorder` (recorder entry, "What
stays in rezolus"; GO 1 there).

### Consumers

**cachecannon** (`src/viewer/`, 1,098 lines of Rust in 11 files; 5,902 lines
of JS and CSS, 5,922 with `index.html`).
Its own axum server on one tokio worker, routes `/`, `/about`, `/data/{path}`,
`/lib/{*path}`, and five `/api/v1` routes in `api.rs` (162), written there because
metriken-query 0.36 has no HTTP layer;
`labels` and `label/{name}/values` return fixed lists. Its own `View`/`Plot`
types (`plot.rs`, 197) and Rust sections (452): Overview, Throughput,
Latency, Cache, Connections, and Server Metrics / Client Metrics (CPU,
memory, network, runqueue latency) from rezolus parquets given with
`--server`/`--client`. Parquet only (metriken-query 0.36 without `archive`),
no live mode, no compare, no upload, no report. The frontend is a subset of
an early-2026 copy of rezolus's (15 files against 66; `mithril.js` and
`echarts.min.js` identical, the rest 300 to 500 changed lines per file from
the nearest rezolus revision). From reading the code, not run: every
`/api/v1` query is bound to the benchmark parquet (`api::routes(state.source
.clone())`), so the Server and Client sections query a file that does not
hold their metrics. Its admin server, which serves `/metrics/stream`, is
hand-written on tokio, not axum. Its plan (cachecannon
`docs/journal/2026-10-08-dendro-recording.md`) has `view` read `.dendro`
(its step 5), move `cachecannon.json` into cachecannon (its step 6), and mount
`metriken-viewer` "after rezolus 6.0.0".

**llm-perf** has no viewer. Its dashboard is rezolus's `llm-perf.json`
template, matched on `source=llm-perf`; its plan's step 6 moves the template
into llm-perf and the archive.

**systemslab** takes `dashboard` and `rez` (without `write`) from rezolus git
tag v5.22.1, wraps `dashboard::dashboard::{build_dashboard_context,
generate_section}` in `systemslab-dashboard` (1,760 lines, with its own
`rpc-perf.json`), and has its own Svelte UI. It vendors 25 of rezolus's chart
modules (6,224 lines, `svelte-ui/src/lib/charts/rezolus-next/upstream/`)
with `pnpm sync:charts`, which runs `git show <rev>:<path>` against a rezolus
checkout at the pinned tag, rewrites `../data.js` imports to a 194-line shim,
and excludes the compare code. An older hand-kept copy (11 files, 2,357
lines) is still imported by some components.

### Measured on 15c77844

On an Apple M-series laptop (macOS), one build each.

- **WASM bundle** (`./crates/viewer/build.sh`, wasm-pack 0.14, opt-level `s`):
  `wasm_viewer_bg.wasm` 6,557,109 bytes, 2,214,679 gzip -9, 1,569,364
  brotli; `wasm_viewer.js` 42,354 bytes. The build took 1 min 38 s from a
  cold target directory. The bundle from 2026-09-22 in the main checkout was
  6,112,775 bytes.
- **JS suite** (`node --test tests/*.mjs`, node 22.18, bundle built): 397
  tests, 389 pass, 0 fail, 8 skipped (7 need `VIEWER_URL`, 1 is
  compare-mode only), 23.5 s.
- **Dashboard load on the 9.6 h recording** (`metrics-9.6h.dendro`,
  1.28 GB, the 9.6-hour recording the storage scan measured on),
  `rezolus view` release build, the storage scan's Playwright harness
  (`dash.js`, kept on the author's host with the archive) loading 12 sections and
  scrolling each until 5 s pass with no request, three runs:

  | run | first `/api/v1/sections` | peak footprint | summed section span | summed `query_range` wall | requests |
  |---|---|---|---|---|---|
  | 1 | 0.6 s | 2,288 MB | 14.4 s | 136.6 s over 344 | 352, one 404 (`/selection`, no saved selection) |
  | 2 | 0.1 s | 2,518 MB | 15.0 s | 145.9 s over 344 | same |
  | 3 | 0.1 s | 2,383 MB | 18.5 s | 203.7 s over 344 | same |

  Run 3's span is 28% above run 1's and its summed `query_range` wall 49%
  above, with the same build, so a single run cannot show a 5% change. The
  storage scan's gate runs on the same file (its "Gate result", five
  alternating runs per build, 9 to 10 October) gave spans of 13.4 to 13.8 s
  and peaks of 2,285 to 2,523 MB; adding the runs of its earlier
  scan-branch comparison on the same host widens these to 13.4 to 14.5 s and
  2,238 to 2,523 MB. The first column is launch to the first answer
  from `/api/v1/sections`, polled every 0.1 s; run 1 includes a cold page
  cache. The script that produced the table wraps `dash.js` as the storage
  scan's dashboard runner does and adds that column. It is not committed
  yet; step 1 commits it with the GO harness.

## Scope decisions

Each question has the options and evidence put to the owner, kept as
history, and the decision taken on 2026-10-12. The design below follows the
decisions.

### Q1. Which repository holds the two crates?

The plan left this open ("Structure", last paragraph).

- **(a) This repository.** A query-engine change and the viewer change that
  needs it can be one PR. Cost: this repository's CI (`cargo.yml`: nextest
  and doctests on Ubuntu, macOS and Windows; `cargo hack` powerset, rustfmt
  and clippy on Ubuntu) gains wasm-pack, a wasm32 clang, binaryen, node and 57 JS test
  files, and its history gains the viewer's change rate (80 of 331 rezolus
  commits since 2026-07-14).
- **(b) A new repository, `iopsystems/metriken-viewer`**, holding both crates
  and the assets, depending on metriken crates by version. Cost: a change that
  spans the engine and the viewer is two PRs and a release in between, as it
  is today between metriken and rezolus.
- **(c) metriken-dashboard here, metriken-viewer in its own repository.**
  `display_wire.rs` (dashboard) is pinned to the JS decoder by a shared
  fixture (`tests/fixtures/display_wire_mixed.json`, written by
  `cargo test -p dashboard`, read by `display_wire_fixture.test.mjs`), so
  this split puts one format's two halves in two repositories.

Decided 2026-10-12: (c). The draft recommended (b); the owner chose (c) on
this evidence:

- metriken-dashboard has consumers that never load the viewer. systemslab
  depends on rezolus's `dashboard` crate and calls
  `dashboard::generate_section` over a `&dyn MetricsSource` with its own
  frontend (systemslab `Cargo.toml:93-100`); rezolus reads its KPI types in
  `src/parquet_metadata.rs`, `src/parquet_tools/check.rs` and
  `src/parquet_tools/combine.rs`, outside `src/viewer`.
- `generate_section` takes `&dyn MetricsSource`, so dashboard must resolve the
  same metriken-query as its caller or the trait object does not type-check
  (systemslab `Cargo.toml:93-98`). In this workspace the two are released
  together; in another repository every metriken-query release needs a
  dashboard release before a consumer can move.
- Since 2026-07-14, 13 of the 37 commits to `crates/dashboard` also touched
  `src/viewer/assets` or `crates/viewer`. Under (c) each such change is two
  PRs: a dashboard release, then the viewer. 21 of the 37 touched
  `src/agent/samplers`; 3 of those (dbe7f2ac, d7e3973c, 21839c61) also
  changed the model or the wire and the assets, and would have been PRs in
  metriken, metriken-viewer and rezolus. Two more of the 21 (75ed9cfa,
  1c8eed86) changed `plot.rs` only to add `blockio_device_latency_metric`
  and `has_host_traffic_counters`, which move to `rezolus-dashboard` (see
  "Crates"). The other 16 changed sampler sections only, which stay in
  rezolus (Q2). 6 of the 37 (f34e1e2c, 021cd04e, 4f7646a4, dc14645e,
  e6f18c14, 655777e5) touched only generic dashboard code, with no sampler,
  asset or WASM change (f34e1e2c also edited the templates); under (c)
  each is a metriken PR and a release before rezolus sees it.

The display wire fixture keeps the format's two halves checked across the
repositories. Today it is a golden file at rezolus's repository root,
`tests/fixtures/display_wire_mixed.json`, which `display_wire.rs:407` reads
as `../../tests/fixtures/display_wire_mixed.json` and writes only under
`UPDATE_FIXTURES=1`. A file outside the crate directory is not in the
package, and metriken-storage's manifest already excludes `tests/`
(`metriken-storage/Cargo.toml:14`), so the fixture moves inside the crate:

- `metriken-dashboard/fixtures/display_wire_mixed.json`, in a directory the
  manifest does not exclude. metriken-dashboard's test compares the encoder
  against it, as `display_wire.rs` does today, and a CI step checks that
  `cargo package --list -p metriken-dashboard` names it.
- metriken-viewer's `display_wire_fixture.test.mjs` reads it from the
  resolved metriken-dashboard source directory, which `cargo metadata`
  reports, so a dashboard release that changes the format fails the
  viewer's JS tests until the decoder follows.
- A wire format change is a breaking metriken-dashboard release (a 0.x
  minor), so a consumer does not take it by `cargo update`.
- metriken-viewer's release notes name the metriken-dashboard version its
  tests ran against. systemslab requires its chart JS and its Rust dashboard
  to come from the same revision, so its chart sync reads the fixture and
  the encoder at that metriken-dashboard version and the assets at the
  matching metriken-viewer tag.

### Q2. Where do rezolus's sampler sections live?

The 15 sampler sections in `SECTION_META` plus Overview (4,665 / 3,791 lines) describe
rezolus's samplers. cachecannon wants them too: its Server and Client
sections redraw CPU, memory, network and runqueue from rezolus recordings.

- **(a) In metriken-dashboard** (a `rezolus` module or feature). GO 5 is
  then direct, since every mount has the same sections. Cost: a sampler and
  its section land in one rezolus commit today (19 of 19 section commits
  since 2026-07-14 also touch `src/agent/samplers`); under (a) each new
  sampler waits for a metriken-dashboard release, and metriken carries
  knowledge of rezolus's metric names.
- **(b) In rezolus, as a crate `rezolus-dashboard`** that implements a
  section-provider trait from metriken-dashboard. A sampler and its section
  stay one commit. cachecannon and systemslab, to show system sections, depend
  on `rezolus-dashboard`. rezolus publishes nothing to crates.io today
  (`publish = false` in every manifest); systemslab already takes rezolus
  crates by git tag. A crates.io-published consumer cannot take a git
  dependency, so (b) means publishing `rezolus-dashboard` if cachecannon
  publishes.
- **(c) As JSON templates** carried in the archive like any producer's. The
  `ServiceExtension` format cannot express what the generators do from the
  data (`gpu.rs` enumerates devices, `cpu.rs`, `blockio.rs` and others test
  for metrics before drawing), so (c) is a rewrite of the format and the
  sections.

Recommendation: (b), published to crates.io from rezolus's release workflow.
It keeps the coupling that exists today (sampler and section together).
(Corrected 2026-10-12: the draft said (b) "removes none of what cachecannon
can show". It does not by itself keep cachecannon's separate Server and
Client sections; see "Design gap: two rezolus sources in one archive" under
"cachecannon".) GO 5 then reads: both commands
mount `metriken-viewer` with the same providers (`rezolus-dashboard` and the
archive's templates).

Decided 2026-10-12: (b). `rezolus-dashboard` is a crate in rezolus,
published to crates.io from rezolus's release workflow. Being published, it
depends only on crates.io crates (`metriken-dashboard`, `metriken-query`),
not on rezolus's unpublished `crates/rez` or on the binary.

Consequence of Q1 (c) with Q2 (b): `rezolus-dashboard` is published only
when rezolus's release workflow runs, which is on a `v*` tag
(`release.yml`), and that workflow publishes nothing to crates.io today. A
metriken-query release that changes `MetricsSource` therefore reaches
cachecannon and systemslab only after metriken-dashboard, metriken-viewer
and a rezolus release, since each resolves one metriken-query for all of
them. Before rezolus 6.0.0 the cachecannon branch (Order of work, step 4)
depends on a `rezolus-dashboard` 6.0.0-alpha prerelease, with an `=`
requirement (`rezolus-dashboard = "=6.0.0-alpha.N"`), because a caret
requirement on a prerelease also accepts later alphas, which may break, and
6.0.0. `iopsystems/rezolus` has no 6.0.0-alpha tag today (its newest tag
is v5.27.0), so step 3 cuts one.

**For the owner: should `rezolus-dashboard` be released independently of
rezolus, with its own version, tag and workflow?** Recommendation: yes, with
a 0.x version of its own, a `rezolus-dashboard-v*` tag, and a workflow that
only runs `cargo publish -p rezolus-dashboard`. Evidence: `release.yml` on a
`v*` tag builds and signs the deb and rpm packages for every distribution
it lists, so a rezolus release costs far more than the crate needs; without
an independent release, every metriken-query change to `MetricsSource`
leaves cachecannon and systemslab waiting on a rezolus release cut for
that reason only; and before 6.0.0 it forces the prerelease pin above. The
cost is one more version line in rezolus and a rule for when it is bumped
(any change under `crates/rezolus-dashboard`). A sampler and its section
still land in one rezolus commit; the section reaches other applications
at the next `rezolus-dashboard` tag instead of the next rezolus tag. Not
decided; until it is, the design assumes the release workflow.

### Q3. Does metriken-viewer publish a WASM bundle, or does each application build one?

Under Q2 (b) the static site needs rezolus's sections compiled into the
bundle, so the bundle rezolus serves cannot be built by the library alone.

- **(a) The library ships the WASM side as a Rust module** (feature `wasm`)
  whose `#[wasm_bindgen]` exports reach the final `cdylib`, plus a build
  script. Each application has a small `cdylib` crate that registers its
  providers and templates in its start function (rezolus's `crates/viewer`
  shrinks to that). rezolus builds its own bundle for `site/` and the release
  tarball, as today.
- **(b) The library also publishes a generic bundle** (template and
  simple-capture sections only) as a release asset, for a consumer with no
  Rust section provider. No consumer needs one today: cachecannon has no
  static site and systemslab has its own UI.

Recommendation: (a) only. (b) can be added when a consumer asks for it.
`#[wasm_bindgen(start)]` may appear once per final binary, so the library
must not declare one (rezolus's `crates/viewer/src/lib.rs:23` does today).

Decided 2026-10-12: (a). Each application builds its own WASM bundle;
`metriken-viewer` publishes none.

### Q4. Does live mode move into the library?

- **(a) Yes, behind a feature `live`**, over `metriken-recorder` (one streamed
  source, no retention, a reader refresh on each `Committed` event), with the
  application's `Probe` from the recorder entry supplying identity. rezolus's
  banner parsing (`actions::fetch_agent_info`) becomes its probe's `enrich`.
  `follow.rs` (494 production lines) shares the reader refresh and moves too.
- **(b) No.** The library exposes a hook to install a live source; rezolus
  keeps `connect`, `reset` and the session.

Recommendation: (a). After path step 4 nothing in live mode is specific to a
rezolus agent except what the probe already isolates, and the plan's crate
graph already lists `metriken-recorder` under `metriken-viewer` for this.
cachecannon gains a live view of its own stream at no cost.

Decided 2026-10-12: (a). Live mode is in `metriken-viewer`, behind `live`,
over `metriken-recorder`.

### Q5. Does the static site stay in rezolus?

`site/` is rezolus.com: the landing page (919 lines), `docs/` (7 pages) and
`/viewer/`.

- **(a) Stays.** `site/viewer/` is built from the library's assets and
  rezolus's bundle (Q3 a). The 62 symlinks in `site/viewer/lib/`,
  `sync-viewer-symlinks` and `viewer-symlinks.yml` go: `build.sh` writes the
  assets out of the crate (`metriken_viewer::assets::write_to(dir)`, run by
  a small binary or xtask) into an ignored `site/viewer/lib/`. The 11
  symlinks in `site/viewer/templates/` point at `crates/dashboard/templates/`,
  which is deleted, so they are re-pointed at
  `crates/rezolus-dashboard/templates/` (Q7). `site/docs/architecture.svg`,
  the one other symlink under `site/`, is unaffected. The two hand-kept copies,
  `site/viewer/lib/script.js` and `viewer_api.js`, move into the library as
  the WASM shell's assets.
- **(b) The viewer page moves to the library's repository**, published to its
  own GitHub Pages. It would then need rezolus's sections (Q2), which the
  library's repository does not have.

Recommendation: (a).

Decided 2026-10-12: (a). `site/` stays in rezolus; the viewer page's assets
are copied out of `metriken-viewer` at build time.

### Q6. Does step 7 wait for the Save as Report and template work, or move code that is about to change?

In 6.x Save as Report writes `.dendro` only (rezolus 6.0 entry, decision 5;
"Removing the `.rez`, parquet and raw writers"). Of `crates/report-save`'s 748
production lines, `save_single_parquet`, `save_combined_ab_tarball`,
`build_rez_report_from_rez`, `build_rez_report_from_parquets`,
`parquet_rows_in` and `rewrite_parquet_bytes` are to be removed or replaced;
`build_dendro_report` stays. Two paths the replacement needs do not exist:
parquet to `.dendro`, and `.rez` to `.dendro` in a wasm32 build (whether
metriken-storage's conversion builds without `write` is not checked). Path
step 4 rewrites `live.rs`; path step 5 changes how both shells find templates.

- **(a) Move after** path step 4, path step 5 and the rezolus 6.0 entry's Save as Report
  change have landed in rezolus, so the library starts with the 6.x shape.
- **(b) Move first**, and make those changes in the library.

Recommendation: (a). Under (b) each of those changes is written once in
rezolus's tree (where its tests run) and again in the library, or written in
the library before rezolus can use it.

Decided 2026-10-12: (a). The code moves after path step 4, path step 5 and
the rezolus 6.0 entry's Save as Report change have landed.

### Q7. Where do the built-in templates live after path step 5?

After path step 5 a new archive carries its producer's template. Older
recordings (parquet, 5.x `.rez`, archives recorded before a producer sent
its template) only match the built-in registry by source name. vllm, sglang
and valkey are scraped Prometheus servers with no metriken producer to own
their templates; the recorder takes them per endpoint
(`--endpoint url,template=<file>`).

- **(a) The library has no built-in templates.** An application passes a
  `TemplateRegistry` as the fallback. rezolus keeps its 11 files (for old
  recordings and for `record --endpoint url,template=vllm`); cachecannon and
  llm-perf keep theirs in their repositories (their step 6).
- **(b) metriken-dashboard embeds all 11.** Each template edit then needs a
  metriken-dashboard release, which piece 3 exists to remove.

Recommendation: (a).

Decided 2026-10-12: (a). Neither library has built-in templates; each
application passes its fallback registry.

rezolus's 11 files move to `crates/rezolus-dashboard/templates/`, and
`rezolus-dashboard` exports them as a `TemplateRegistry`
(`rezolus_dashboard::templates()`), because systemslab builds its registry
on `TemplateRegistry::embedded()` (systemslab
`crates/dashboard/src/service.rs:113`), which this decision removes from the
library. systemslab replaces that call with `rezolus_dashboard::templates()`
and keeps inserting its own templates over it, as it does today.

### Q8. Do the TUI and MCP stay in rezolus?

`view --tui` (2,258 lines, ratatui) reads the section model; MCP
(`src/mcp`) reads archives through metriken-query and formats viewer links
(`src/mcp/link.rs`, pinned to `url_state.js` by
`tests/viewer_link_parity.test.mjs`).

Recommendation: both stay. The TUI becomes a consumer of metriken-dashboard's
model; it is not part of GO 5. The viewer link format moves into the library
as a Rust function (`metriken_viewer::link`) beside `url_state.js`, with the
parity test, so MCP calls it instead of keeping a copy.

Decided 2026-10-12: the TUI and MCP stay in rezolus; the viewer link moves
to `metriken-viewer` with its parity test. `metriken_viewer::link` is
outside every feature, so it builds with default features off; rezolus
calls it from `src/mcp`. (Corrected 2026-10-12: this said `rezolus mcp`
would then not build the server. rezolus is one binary crate, and features
unify across it, so `rezolus view` enabling `server` builds it for every
subcommand.)

### Q9. Which consumers move inside step 7?

- rezolus: required (it is the source of the code, and 6.0.0 waits for it).
- cachecannon: the plan's GO 5 names `cachecannon view`; cachecannon's own
  plan says "after rezolus 6.0.0".
- systemslab: piece 5 says it mounts the same library. Its UI is Svelte; it
  uses the dashboard model and vendored chart modules, not the server or the
  page.

Recommendation: step 7 closes when rezolus is on the released crates and a
cachecannon branch passes GO 5; cachecannon merges and releases on its own
schedule, so rezolus 6.0.0 does not wait for a cachecannon release.
systemslab moves its `dashboard` pin to `metriken-dashboard` and
`rezolus-dashboard`, and its chart sync to `metriken-viewer`'s tag, in its
own journal entry on dendro artifacts (2026-10-08, in systemslab, a private
repository), after rezolus 6.0.0, as its reader PR already does.

Decided 2026-10-12: step 7 closes when rezolus is on the released crates
and a cachecannon branch passes GO 1 below (the plan's GO 5); systemslab
moves after rezolus 6.0.0.

### Q10. Is a non-root mount in scope?

The page requires `/` (`<base href="/">`, absolute `/api/v1` paths). cachecannon
and rezolus mount at `/`. Recommendation: out of scope; recorded under "Left
alone".

Decided 2026-10-12: out of scope.

## Design

### Crates

Following Q1 (c): `metriken-dashboard` in this repository, released with
metriken-query; `metriken-viewer` in `iopsystems/metriken-viewer`, depending
on the metriken crates by crates.io version; `rezolus-dashboard` in rezolus
(Q2). All 0.x. An application resolves one metriken-query for all three, so
a metriken-query release that changes `MetricsSource` reaches an application
only after `metriken-viewer` and `rezolus-dashboard` have released against
it, and `rezolus-dashboard` releases only with rezolus (Q2, the consequence
and the question for the owner there).

**`metriken-dashboard`.** Depends on metriken-query and metriken-storage
(reader features only), serde, serde_json. No `metriken`, no threads; builds
for wasm32. Holds:

- the model (`plot.rs`: `View`, `Group`, `Plot`, `PlotOpts`, `Unit`, and
  the generic `has_metric`), the display wire format, events, the metric
  catalog, capture aliases;
- `ServiceExtension`, `CategoryExtension`, `Kpi`, `Check`, and
  `TemplateRegistry` without an embedded set (`embedded()` and
  `from_embedded` go; `load(dir)`, `from_templates` and `insert_template`
  stay);
- `service_extensions_from_metadata`, moved from rezolus's
  `src/viewer/metadata.rs` (path step 5 moves it into the dashboard crate first,
  so both shells call it);
- source classification (`source_kind.rs`), with the rezolus fingerprint
  (`REZOLUS_SELF_ANCHORS`, `source == "rezolus"`) moved behind the provider
  trait below, so the library does not name rezolus. Two functions in
  `plot.rs` name rezolus metrics and move to `rezolus-dashboard` beside the
  fingerprint: `blockio_device_latency_metric` (`plot.rs:53`, picks
  `blockio_device_latency` or the pre-rename `blockio_latency`) and
  `has_host_traffic_counters` (`plot.rs:78`, tests for
  `network_host_bytes`). Their callers are the rezolus sections (`blockio`,
  `network`, `overview`) and the TUI's overview;
- the section context and the template, category, simple-capture and query
  explorer sections;
- a provider trait for sections an application generates in Rust:

```rust
pub trait SectionProvider: Send + Sync {
    /// Whether this provider's sections apply to a source, from its name,
    /// metadata and metric names (rezolus: `source == "rezolus"`,
    /// `sampler_status`, or a self-sampler metric).
    fn claims(&self, source: &SourceInfo<'_>) -> bool;
    /// Navigation entries, in order, for a recording one of whose sources
    /// this provider claims.
    fn sections(&self) -> Vec<Section>;
    fn generate(&self, data: &dyn MetricsSource, route: &str, nav: Vec<Section>)
        -> Option<View>;
    /// The overview, when this provider supplies one.
    fn overview(&self, data: &dyn MetricsSource, nav: Vec<Section>,
        throughput_query: Option<&str>) -> Option<View> { None }
}
pub struct Dashboards { providers: Vec<Arc<dyn SectionProvider>>, templates: TemplateRegistry }
```

`build_dashboard_context` and `generate_section` take a `&Dashboards`
instead of reading `SECTION_META`. Today's `SourceEntry { is_rezolus }`
becomes the provider that claimed the source. rezolus's sections become
`rezolus-dashboard`'s implementation (Q2). Names and shapes are settled in
step 1 below, against rezolus's and cachecannon's callers.

An empty source list keeps its meaning. Today `build_dashboard_context`
shows the built-in sections when `sources` is empty or any entry
`is_rezolus` (`dashboard/mod.rs:106`, "legacy caller"), and systemslab
relies on the empty case: it calls `build_dashboard_context(filesize,
&service_exts, None, &[])` (systemslab `crates/dashboard/src/rezolus.rs:35`).
With providers, an empty list means every registered provider's sections,
so systemslab gets the same sections by registering
`rezolus-dashboard`'s provider in its `Dashboards`, with
`rezolus_dashboard::templates()` plus its own templates (Q7), and passing
`&[]` as before.

The query explorer is a library section, but today it is a rezolus nav
entry: it sits in `SECTION_META` between Exceptions and CPU, so it appears
only when a rezolus source is present. `sections()` therefore needs a way to
place library sections inside a provider's order, for example a
`Section::library("query")` entry the context expands, rather than the
library appending them after every provider. Step 1 settles which.

**`metriken-viewer`.** Depends, by crates.io version, on
metriken-dashboard, metriken-query, metriken-storage (reader, and `write`
for report saving off wasm32), and behind features:

- `server` (default): axum, tower-http, tokio, include_dir. The router, the
  state, file / A/B / upload / URL load / follow modes, Save as Report, the
  embedded assets.
- `live`: metriken-recorder with `http` (Q4).
- `dev`: serving assets from a directory and `tower-livereload` plus
  `notify` for page reload.
- `wasm`: wasm-bindgen; the `Viewer` and `WasmCaptureRegistry` exports, and
  no `server`. Builds for wasm32 with no `metriken`, `metriken-core` or
  `linkme`, and no `std::thread::spawn` on any reachable path (the reader's
  sequential wasm arm stays).

It uses `RezReader` and its `MetricsSource` impl from metriken-storage and
metriken-query (storage scan, "What callers change"): the storage scan
makes `RezReader` a metriken-storage type behind `rez` (its step 5), so only
metriken-storage or metriken-query may implement `MetricsSource` for it
(E0117), and the impl goes into metriken-query behind `rez`. It takes
`LiveReader` (`crates/rez/src/live.rs`, 293 lines with no test module, which
reopens a `RezReader` as its writer appends) into its `server` feature,
which `live` requires. Both follow mode (`follow.rs`, under `server`) and
live mode (`live.rs`, under `live`, Q4) read through it, and nothing
outside `src/viewer` does; it reads archives only through metriken-storage
and implements `MetricsSource` for its own type. This supersedes the
storage scan's destination for `LiveReader`, the rezolus binary, which it
chose before Q4 put live mode in the library. It also absorbs the
`parquet_metadata` keys the viewer reads (report, selection, events, A/B
markers), and `crates/report-save` reduced to the `.dendro` builders (Q6).

### Server API

```rust
pub struct ViewerConfig {
    pub dashboards: Dashboards,
    pub assets: Assets,               // Assets::Embedded, or Assets::Dir(path) with `dev`
    pub brand: Brand,                 // page title, /about text, version string
    pub proxy_allow: Allowlist,       // `--proxy-allow`
    pub query_concurrency: usize,
    pub cache_bytes: usize,
    pub upload_limit: usize,          // today 50 MiB
    #[cfg(feature = "live")]
    pub live: Option<LiveConfig>,     // client, Probe, interval; None hides /connect
}
pub enum Input {
    Empty,                                  // upload-only
    File { path: PathBuf, follow: bool },   // parquet, .rez, .dendro; follows a running writer
    Recordings { baseline: Recording, experiment: Option<Recording> }, // after the caller's selection
    #[cfg(feature = "live")]
    Live(Url),
}
pub struct Viewer { .. }
impl Viewer {
    pub async fn open(cfg: ViewerConfig, input: Input) -> Result<Self, OpenError>;
    pub fn recordings(path: &Path) -> Result<Vec<(Labels, Recording)>, OpenError>;
    pub fn router(&self) -> axum::Router;  // mount at "/"
    pub fn notices(&self) -> &[String];    // what the server logs today
    pub async fn shutdown(self);           // finish live sessions, remove live directories
}
```

`rezolus view` keeps its clap command, `--baseline`/`--experiment` selection
(over `RecordingSelector` from `src/mcp`), `--templates`, the browser open
and `REZOLUS_NO_OPEN`, `--tui`, signal handling, and its probe; it calls
`Viewer::recordings`, selects, and passes `Input::Recordings`. The global
state that `live.rs` keeps for a `process::exit` in the Ctrl-C handler
(`LIVE_DIRS`, `LIVE_THREADS`) becomes `Viewer::shutdown`; the library calls
no `process::exit`.

rezolus's `developer-mode` feature becomes `Assets::Dir` pointing at the
library checkout named by an environment variable, with `dev` enabled, so
editing a JS file in the library checkout reloads `rezolus view` without a
rebuild, as editing `src/viewer/assets` does today.

### Assets

The frontend lives in the `metriken-viewer` crate's `assets/` directory in
`iopsystems/metriken-viewer` and is embedded with `include_dir!`. The
package stays under crates.io's 10 MB limit (2.1 MB of assets today).
`index.html` takes the title from `Brand`. Branding in the JS (about 60 occurrences of "rezolus": the `rezolus-theme` localStorage key,
`rezolus_report_` file name prefixes, `RezolusChart`) is renamed where a user
sees it and kept where renaming would lose saved state (the theme key) or
change file names users script against; each kept name is listed in step 1's
commit.

`metriken_viewer::assets::write_to(dir)` writes the embedded assets out, for
static builds (Q5): rezolus's site build copies them into an ignored
`site/viewer/lib/`. systemslab's `sync:charts` reads the same files from an
`iopsystems/metriken-viewer` tag instead of a rezolus revision, and the
display wire fixture and encoder from the metriken-dashboard version that
tag's release notes name (Q1), since its chart JS and its Rust dashboard
must come from the same revision (systemslab
`svelte-ui/scripts/sync-rezolus-charts.mjs:5`).

### WASM bundle

The library's `wasm` feature holds the exports; it declares no
`#[wasm_bindgen(start)]`. An application's `cdylib` (rezolus's
`crates/viewer`, about 30 lines) calls `metriken_viewer::wasm::install(
Dashboards)` from its own start function. Each application builds its
own bundle (Q3); metriken-viewer publishes none. The `metriken-viewer` crate
ships `build.sh` in its package (wasm-pack, opt-level `s`, the Homebrew LLVM
lookup, the binaryen structure check from rezolus's `cargo.yml`), taking the
application crate's path and an output directory. That directory is in
cargo's registry source cache, shared by every project on the host, so
`build.sh` writes nothing next to itself (output, temporaries and wasm-pack's
`pkg/` all go under the output directory or the application's target
directory), and it is run as `bash <path>/build.sh` rather than executed,
so it does not depend on the packaged file keeping its mode bits. The
application finds the path from `cargo metadata`: the `manifest_path` of
the resolved `metriken-viewer` package, whose directory holds `build.sh`.
The templates manifest that `build.sh` writes today comes from the
application's template directory.

### Report saving

In 6.x Save as Report writes `.dendro` only. The library keeps
`build_dendro_report` (a dendro source sealed into a copy) and whichever
conversions the rezolus 6.0 entry builds (parquet to `.dendro`; `.rez` to
`.dendro`, on wasm32 only if it builds there). A source it cannot convert in
the browser offers no save, with a message pointing at `rezolus view`.

### Templates

A recording's dashboard comes from, in order: `service_queries` in the
source's metadata (archive) or file metadata (parquet), then
`per_source_metadata`, then the application's fallback registry by source
name, then the section providers that claim the source, then a
simple-capture section. One function in metriken-dashboard implements this
order for both shells. After path step 5 a cachecannon or llm-perf archive shows
its dashboard in `rezolus view` with no rezolus template, and the fallback
only serves recordings older than that.

### What stays in rezolus

`rezolus-dashboard` (the sampler sections, the rezolus fingerprint with
`blockio_device_latency_metric` and `has_host_traffic_counters`, and the 11
fallback templates as `rezolus_dashboard::templates()`, published to
crates.io from the release workflow), the `view` CLI (selection, `--templates`, browser open,
`--tui`), the probe for live mode, the TUI, MCP, `site/` (landing, docs, the
viewer page built from the library), `rezolus-viewer-<tag>.tar.gz` in the
release workflow, the JS tests that drive rezolus's built bundle
(`wasm_viewer_*`), and the `viewer-smoke` and `viewer-render` skills pointed
at `rezolus view`. `crates/rez`, `crates/report-save` and `crates/dashboard` are
deleted (`crates/rez`'s re-exports are replaced by imports from
metriken-storage). The `viewer-parity` skill moves to
`iopsystems/metriken-viewer`, rewritten for one crate.

### cachecannon

`cachecannon view` builds a `Viewer` with `Dashboards { providers:
[rezolus-dashboard], templates: [cachecannon.json] }` and serves its
`router()` on the axum server it already runs. Its `src/viewer/` (1,098
lines of Rust in 11 files) and its assets (5,902 lines of JS and CSS, 5,922
with `index.html`) are deleted. Its Rust sections are
replaced by its template; a plot the template cannot express is a
cachecannon `SectionProvider`. `--server` and `--client` parquets become the
experiment slot or a combined archive; that choice is cachecannon's.

**Design gap, for the owner: two rezolus sources in one archive.** Today
`cachecannon view --server s.parquet --client c.parquet` draws a Server
Metrics and a Client Metrics section (CPU, memory, network, runqueue
latency), one per host. A combined archive holding cachecannon and two
rezolus agents gets neither under this design. `build_dashboard_context`
already lists each source as a `SourceEntry` with a resolved name
(`source_kind.rs:140`), but it shows the built-in sections once if any
source is rezolus (`dashboard/mod.rs:106`), and their queries carry no
source selector, so each chart combines both hosts' series. The provider
trait above keeps that: `claims` is per source, `sections` and `generate`
are not. Using the experiment slot for the second host (option below)
renders the client as an A/B overlay of the server, which is a different
comparison. Options:

- **(a) Per-source provider scope.** `generate` and `sections` take the
  claimed source, and a provider that claims two sources yields one set of
  nav entries per source, named by `SourceEntry.name` (host), with every
  query restricted to that source's series. This needs a label or matcher
  that selects one source's series in a multi-source `.dendro`; I do not
  know whether metriken-storage exposes one to PromQL (the `__run__` label
  it adds, `segmented.rs:398`, separates histogram configuration runs, not
  sources), and step 1 checks. It also means every rezolus section's
  queries take a selector, which touches the 15 sampler sections.
- **(b) A cachecannon `SectionProvider`** that redraws Server and Client
  sections with host-scoped queries, as cachecannon's Rust does today.
  It duplicates rezolus sections in cachecannon and needs the same
  per-source matcher as (a).
- **(c) Accept one set of rezolus sections over both hosts**, each chart
  combining both hosts' series as it does today. Smallest change; loses
  the per-host sections cachecannon has today.

Recommendation: (a), decided in step 1 because it changes the provider
trait's signatures, and GO 1's fixture gains a second rezolus agent so the
two-host case is tested. It is the only option that gives every
application with several hosts in one archive (rezolus `record --endpoint a
--endpoint b`, a fleet recording) per-host sections, not just cachecannon.
If the per-source matcher does not exist and cannot be added to
metriken-storage without a format change, (c) for 6.0.0 and (a) after.

## GO / NO-GO

GO when all hold:

1. **One dashboard.** For one `.dendro` holding a rezolus agent and a
   cachecannon run, `/api/v1/sections` and every `/data/<section>.json`
   from `cachecannon view` and `rezolus view` are byte-identical, with
   cachecannon's dashboard from the archive's template (plan GO 5).
2. **Shell parity.** A native test builds the server state and the WASM
   `Viewer` (the crate is `rlib` as well as `cdylib`, as rezolus's
   `crates/viewer` is today) from the same fixtures and asserts `sections`,
   every section, `metrics`, `systeminfo`, `file_metadata` and `metadata`
   are byte-identical. The test is a function in metriken-viewer (feature
   `test-support`) that takes a `Dashboards`, because the rezolus sections
   are not in that repository: metriken-viewer's CI runs it with templates
   only (a recording with a template, a simple capture, a two-recording
   archive), and rezolus's CI runs it with `rezolus-dashboard` on a rezolus
   recording. No such test exists today; the skill describes it and the
   suite has one-sided tests.
3. **Tests carried over.** rezolus's viewer tests (Rust, `node --test`,
   `viewer_smoke.sh`) pass after rezolus moves onto the crates, and the JS
   suite, split between `iopsystems/metriken-viewer` and the tests that stay
   in rezolus, runs at least as many tests in total as at the commit before
   step 1, re-measured there (397 at 15c77844), with none skipped other than
   those skipped there (8 at 15c77844). A test whose expectation changes is
   named in its commit.
4. **Bundle size.** rezolus's `wasm_viewer_bg.wasm`, built by the library's
   `build.sh`, is no more than 5% above the size at the commit before step 1
   (6,557,109 bytes at 15c77844, to be re-measured there).
5. **Page load.** On `metrics-9.6h.dendro`, five alternating runs of
   `rezolus view` before and after, with the harness above: the build fails
   when the median summed section span, summed `query_range` wall or peak
   footprint is more than 5% worse and the ranges do not overlap (the
   storage scan's rule). The spread between runs above (28% in span, 49% in
   summed `query_range` wall) is why one run is not enough. With this spread
   a regression smaller than about 25% (span) or 50% (`query_range` wall)
   passes; the rule detects only larger ones. The storage scan records the
   same limit for its own measures (its "GO / NO-GO", the paragraph on
   overlap). If the five baseline runs at step 1 are as tight as the storage
   scan's gate runs (13.4 to 13.8 s), the detectable size is smaller; the
   step 1 commit records the measured ranges and the size they allow.
6. **No registry in the browser.** For wasm32-unknown-unknown, `cargo tree
   -e normal` lists no `metriken`, `metriken-core` or `linkme`, and `cargo
   check` of the same configuration is a CI step, in each of three
   repositories: this one for `-p metriken-dashboard`,
   `iopsystems/metriken-viewer` for `-p metriken-viewer --no-default-features
   --features wasm`, and rezolus for its bundle's `cdylib` (`crates/viewer`,
   which pulls in `rezolus-dashboard`).
7. **Developer mode.** With `Assets::Dir`, an edit to a file under a
   `metriken-viewer` checkout's `assets/` is served by `rezolus view` on the
   next reload without a rebuild.

NO-GO if the section-provider trait cannot carry rezolus's sections without
metriken-dashboard naming rezolus metrics, or if the WASM exports cannot be
registered from an application crate. Then this entry records why, and the
fallback is Q2 (a) for the sections or a bundle built in
`iopsystems/metriken-viewer`.

## Order of work

0. **Prerequisites, in rezolus and metriken.** Path step 4 (live viewer on
   `metriken-recorder`) and path step 5 (templates from the archive, with
   `service_extensions_from_metadata` in the dashboard crate and called by
   both shells, closing the WASM gap above) are released and rezolus is on
   them. The rezolus 6.0 entry's Save as Report change has landed (Q6).
1. **Split inside rezolus.** With every test still in one tree: the provider
   trait in `crates/dashboard`, rezolus's sections moved to a new
   `crates/rezolus-dashboard`, `src/viewer` (minus the CLI and TUI) moved to
   a library crate `crates/viewer-server` with the API above, the assets
   moved into that crate, the WASM exports moved behind a feature with
   `crates/viewer` reduced to the registering `cdylib`, and `LiveReader`
   moved into `crates/viewer-server`. `RezReader` and its `MetricsSource`
   impl are taken from metriken-storage and metriken-query once the storage
   scan's move of `crates/rez` has released; until then they stay in
   `crates/rez`. `blockio_device_latency_metric` and
   `has_host_traffic_counters` move from `plot.rs` to
   `crates/rezolus-dashboard`. This step also settles:
   - how a provider places library sections in its nav order, since the
     query explorer sits between Exceptions and CPU in `SECTION_META` and
     appears only for rezolus sources ("Crates");
   - whether providers are scoped per source, which decides the
     cachecannon Server/Client gap ("cachecannon"), with a second rezolus
     agent added to GO 1's fixture;
   - `build.sh` writing nothing next to itself and being run as
     `bash <path>/build.sh`, the path taken from the `manifest_path` that
     `cargo metadata` reports for the crate ("WASM bundle"), so it already
     works when the crate is a registry dependency.

   The shell-parity test (GO 2) and the harness for GO 5 are written here.
   At this step GO 2, GO 4, GO 5 and GO 6 (for `crates/dashboard` and
   rezolus's `cdylib`) pass in rezolus, and GO 3 and GO 7 pass in their
   one-tree form: every test still in rezolus, and `Assets::Dir` pointed at
   `crates/viewer-server/assets`. This is a rezolus PR series and changes no
   behaviour.
2. **New crates.** Copy `crates/dashboard` into this repository as
   `metriken-dashboard` (released with metriken-query, with the display wire
   fixture in `metriken-dashboard/fixtures/` and a CI check that `cargo
   package --list` names it, Q1), and `crates/viewer-server` (with assets, JS tests,
   `build.sh`, the parity skill) to `iopsystems/metriken-viewer` as
   `metriken-viewer`; whether to carry history with `git filter-repo` is
   decided then. CI in the viewer repository: nextest, clippy, rustfmt, the
   wasm32 checks, `node --test`; this repository's CI gains the
   `metriken-dashboard` wasm32 check and the fixture package check. rezolus
   builds against both branches (this repository's and metriken-viewer's)
   through `[patch.crates-io]` and passes GO 3 in its split form, and GO 7
   with `Assets::Dir` pointed at a metriken-viewer checkout, before the
   0.1.0 releases. GO 6 is then a CI step in all three repositories.
3. **rezolus on the released crates.** The in-tree copies are deleted, the
   site build uses `assets::write_to`, the 62 `site/viewer/lib/` symlinks
   and their CI check go, the 11 `site/viewer/templates/` symlinks are
   re-pointed at `crates/rezolus-dashboard/templates/`, and
   `rezolus-dashboard` is published from the release workflow (Q2), or from
   its own if the owner decides so. GO 2 to GO 6 are re-run on the released
   crates. Before 6.0.0 the publish needs a rezolus prerelease tag
   (6.0.0-alpha.N), which this step cuts.
4. **cachecannon branch** on the released crates (GO 1). It reads `.dendro`
   by then (its step 5). Before rezolus 6.0.0 it depends on
   `rezolus-dashboard = "=6.0.0-alpha.N"` (Q2). Merged on cachecannon's
   schedule (Q9); a later metriken-query release reaches it only after
   metriken-viewer and a `rezolus-dashboard` release follow.
5. **systemslab** in its own entry, after rezolus 6.0.0 (Q9).

## Left alone

- Mounting the page under a path other than `/` (Q10).
- A generic WASM bundle published by the library (Q3 b).
- Moving the TUI into a library.
- The cachecannon defect noted under "Consumers" (`--server` and `--client`
  sections queried against the benchmark parquet) is not reproduced; it goes
  away when cachecannon mounts the library, and is to be reported to
  cachecannon if it is confirmed before then.
- The dashboard load numbers are from a laptop; the GO 5 runs use one host
  for both builds, which is all the rule needs.
