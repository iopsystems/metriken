# metriken-dashboard and metriken-viewer: one viewer for every metriken producer

**Status:** OPEN. Design 2026-10-12; nothing built. This is path step 7 of
[one recording stack](2026-10-08-one-recording-stack.md) (piece 5). "Step N
below" means this entry's own order of work. The owner decided step 7's
scope on 2026-10-12, before work begins; the questions, their options and
the decisions are under "Scope decisions", and the design below follows
them. Later decisions the same day are recorded there too: how
a `rezolus-dashboard` crate would be released (Q2), how an archive holding
several recordings from one run opens (Q11), and how templates travel
(Q12). The last of these, after an evaluation the owner accepted, makes
rezolus's sections a template the agent sends in its handshake, with an
expander in metriken-dashboard, and replaces the `rezolus-dashboard` crate
and the section-provider trait that Q2 (b) had decided (Q12, "Decided after
the evaluation").

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
| `crates/dashboard/src/dashboard/` rezolus sections: `overview`, `exceptions`, `cpu`, `gpu`, `hw_sensors`, `memory`, `network`, `scheduler`, `syscall`, `softirq`, `blockio`, `filesystem`, `ext4`, `xfs`, `cgroups`, `rezolus` | 4,665 / 3,791 | Rust generators for rezolus's own samplers, listed in `SECTION_META` (`dashboard/mod.rs:27`). Several read the recording to decide what to draw (`gpu.rs` enumerates `(vendor, id)` pairs); all of them decide from the metric catalog only, never from sample values (Q12, evaluation). |
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
  and `has_host_traffic_counters`, which the evidence here assumed would
  move to `rezolus-dashboard`; after Q12 the rezolus template expresses them
  and the TUI keeps Rust copies ("Crates"). The other 16 changed sampler
  sections only, which stay in rezolus (Q2; as the rezolus template after
  Q12). 6 of the 37 (f34e1e2c, 021cd04e, 4f7646a4, dc14645e,
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

The 15 `SECTION_META` entries other than the Query Explorer, plus Overview (4,665 / 3,791 lines), describe
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
  sections. (Q12 made this a direction to evaluate; the evaluation found a
  new section-template format, separate from `ServiceExtension`, that
  expresses the sections, and Q12 decides it.)

Recommendation: (b), published to crates.io from rezolus's release workflow.
It keeps the coupling that exists today (sampler and section together).
(Corrected 2026-10-12: the draft said (b) "removes none of what cachecannon
can show". It does not by itself keep cachecannon's separate Server and
Client sections; Q11 decides how they are kept.) GO 5 then reads: both
commands mount `metriken-viewer` with the same providers
(`rezolus-dashboard` and the archive's templates).

Decided 2026-10-12: (b). `rezolus-dashboard` is a crate in rezolus,
published to crates.io from rezolus's release workflow. Being published, it
depends only on crates.io crates (`metriken-dashboard`, `metriken-query`),
not on rezolus's unpublished `crates/rez` or on the binary. Q12 records a
direction under evaluation that would replace the crate with templates the
agent sends; until that evaluation reports, the design follows (b).

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

Asked of the owner on 2026-10-12: should `rezolus-dashboard` be released
independently of rezolus, with its own 0.x version, a `rezolus-dashboard-v*`
tag and a workflow that only runs `cargo publish -p rezolus-dashboard`? The
case for it was that `release.yml` on a `v*` tag builds and signs the deb and
rpm packages for every distribution it lists, so a release cut only to move
the crate costs far more than the crate needs.

Decided 2026-10-12: no. `rezolus-dashboard` is released from rezolus's own
release workflow, with rezolus's version, because a section changes with its
sampler (19 of 19 section commits since 2026-07-14 also touch
`src/agent/samplers`, "Today"), and one version line keeps a
section and its sampler released together. The consequences above stand and
are accepted:

- a metriken-query release that changes `MetricsSource` reaches cachecannon
  and systemslab only after metriken-dashboard, metriken-viewer and a rezolus
  release;
- a cachecannon branch before rezolus 6.0.0 pins a prerelease,
  `rezolus-dashboard = "=6.0.0-alpha.N"`, and step 3 of the order of work
  cuts the first 6.0.0-alpha tag.

Superseded 2026-10-12 by Q12. rezolus's sections become a section template
that the agent sends in its handshake, expanded by metriken-dashboard, in
the direction of option (c) above. There is no `rezolus-dashboard` crate
and no section-provider trait. The two decisions above, (b) and releasing
`rezolus-dashboard` from rezolus's release workflow with rezolus's version,
no longer apply, and neither do their consequences: a metriken-query release
that changes `MetricsSource` reaches cachecannon and systemslab after
metriken-dashboard and metriken-viewer, with no rezolus release in between;
cachecannon pins no `=6.0.0-alpha.N`; and step 3 of the order of work cuts
no prerelease tag for a crate publish. The reason given for one version line
still holds in the new form: the template is a file in rezolus's tree that
the agent embeds, so a sampler and its section still land in one commit and
ship in one release.

Decided 2026-10-12 (owner): rezolus's `crates/dashboard` stays as a
transitional crate with its Rust generators until systemslab has moved to
templates. systemslab is its only dependent outside rezolus, by git
dependency (tag v5.22.1 on systemslab main, `Cargo.toml`; its rezolus-6.0 PR
pins rev ab24b072 and moves the pin to the v6.0.0 tag before merging);
cachecannon and llm-perf do not depend on it, and it is never published to
crates.io (`publish = false`). rezolus 6.0.0 ships it as an unpublished
workspace crate with the API systemslab uses unchanged (the dashboard model,
the generators, `TemplateRegistry::embedded()`, `generate_section`,
`build_dashboard_context`), so systemslab's PR can pin the v6.0.0 tag with
no change to its dashboard calls; its `rez` imports move to metriken-storage
(plan, "Library users"), and its metriken-query follows rezolus 6.0.0's.
While the generators and the template both exist, rezolus CI runs the GO 8
comparison against them, so a change to either that makes them differ fails
CI. The generators and the crate's Rust section API are deleted in a 6.x
release after systemslab's own entry lands and systemslab is on
metriken-dashboard plus the rezolus template (order of work, step 6).
rezolus's own viewer stops using the crate at step 3, so with the generators
gone nothing in it is left in use and the crate goes with them.

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

After Q12 the premise above no longer holds: rezolus's sections are a
template, not Rust compiled into the bundle, so a generic bundle from
metriken-viewer (option (b)) would render rezolus's sections for any
recording that carries the template, and rezolus's site could use it. What
rezolus's own `cdylib` still adds is its fallback registry (the rezolus
template and the 11 `ServiceExtension` files, Q7) for recordings that carry
no template. The decision stays (a), and this entry's order of work follows
it; it can be revisited once rezolus's `cdylib` does nothing but register
that registry, since the registry could then be loaded by a generic bundle
at run time instead.

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
  which goes when that crate is deleted (Q2, after systemslab moves), so
  they are re-pointed at rezolus's template
  directory (Q7; this said `crates/rezolus-dashboard/templates/` until Q12
  removed that crate). `site/docs/architecture.svg`,
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

As first decided, rezolus's 11 files were to move to
`crates/rezolus-dashboard/templates/`, exported as
`rezolus_dashboard::templates()` for systemslab. Q12 removed that crate, so
this paragraph is replaced (2026-10-12):

rezolus's fallback registry holds the 11 `ServiceExtension` files and the
rezolus section template (Q12). They live in one directory in rezolus's tree
outside `crates/dashboard`, which is deleted once systemslab has moved
(Q2); step 1 names the directory and decides whether `crates/dashboard`'s
`TemplateRegistry::embedded()` reads the same directory until then, so the
11 files exist once. The agent embeds the rezolus template from it, and `rezolus
view` and rezolus's `cdylib` register all 12 files as the fallback. The
rezolus template's registry entry carries a `match` predicate over the
source's metadata and catalog (`source == "rezolus"`, a `sampler_status`
key, or a metric in `REZOLUS_SELF_ANCHORS`), which replaces the rezolus
fingerprint in `source_kind.rs`; it serves recordings whose rezolus sources
carry no template (parquet, 5.x `.rez`, and `.dendro` recorded before the
agent sent one).

systemslab builds its registry on `TemplateRegistry::embedded()` (systemslab
`crates/dashboard/src/service.rs:113`), which this decision removes from
metriken-dashboard, and gets rezolus's sections today from the built-ins
that `build_dashboard_context(.., &[])` shows ("Crates"). Both go from
metriken-dashboard; rezolus's `crates/dashboard` keeps both until
systemslab's entry (Q2). On metriken-dashboard, systemslab registers the
rezolus template and the `ServiceExtension` files it uses from rezolus, and
keeps inserting its own templates over them, as it does today. How a
consumer outside rezolus obtains the files from a rezolus tag is decided in
step 1 of the order of work, with the template directory, because
cachecannon needs it at step 4, before rezolus 6.0.0; the candidates and a
recommendation are recorded there.

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
systemslab moves its `dashboard` pin to `metriken-dashboard` (with the
rezolus template registered, Q7; this said `rezolus-dashboard` until Q12),
and its chart sync to `metriken-viewer`'s tag, in its own journal entry on
dendro artifacts (2026-10-08, in systemslab, a private repository), after
rezolus 6.0.0, as its reader PR already does. Until then systemslab keeps
its git dependency on rezolus's `crates/dashboard`, which rezolus 6.0.0
ships with the API systemslab uses unchanged, so its rezolus-6.0 PR moves
the pin to the v6.0.0 tag with no change to its dashboard calls; its `rez`
imports move to metriken-storage (plan, "Library users"), and its
metriken-query follows rezolus 6.0.0's; the generators are deleted only
after systemslab has moved (Q2, decided 2026-10-12).

Decided 2026-10-12: step 7 closes when rezolus is on the released crates
and a cachecannon branch passes GO 1 below (the plan's GO 5); systemslab
moves after rezolus 6.0.0.

### Q10. Is a non-root mount in scope?

The page requires `/` (`<base href="/">`, absolute `/api/v1` paths). cachecannon
and rezolus mount at `/`. Recommendation: out of scope; recorded under "Left
alone".

Decided 2026-10-12: out of scope.

### Q11. How does an archive of several recordings from one run open?

One `record` run with several endpoints writes one archive with a recording
per endpoint: `record --endpoint agent --endpoint svc`, rezolus-capture, and
cachecannon and llm-perf once they record the server under test and
themselves in process (plan piece 2). Today, at rezolus upstream/main
50075985:

- `rezolus view` maps a multi-recording archive onto the baseline and
  experiment slots and shows any further recordings as more captures
  (`init_file_mode_rez`, `src/viewer/mod.rs`). The WASM
  viewer's `Viewer::new` refuses one, and its `attach` opens one as a
  comparison (`crates/viewer/src/lib.rs`).
- A `.dendro` series carries no `source` label. A recording's labels
  (`source`, `host`, `record --label`) are in the manifest, and
  `open_recordings` returns them beside each recording's reader. Templates
  select by `source` (`vllm.json` has 35 `source="vllm"` matchers), so they
  match nothing. 5.x `recording combine` wrote one parquet with `source` in
  every column's metadata (`src/parquet_tools/combine.rs`), which the
  templates matched. rezolus `docs/backlog.md` lists this as "Multi-source
  single-run recordings open as A/B; templates don't match `.dendro`
  series", blocking 6.0.0, with the viewer fix left to the owner.
- Opened flattened (`open_with_pool`), a query naming one sampler in two
  recordings is refused by `route()` (metriken-query `src/archive.rs:100`)
  as cross-recording.

Decided 2026-10-12: such an archive opens as one combined view. Recording
labels are usable as series labels, so a template's `{source="vllm"}`
matches that recording's series. A/B (baseline and experiment) is shown only
on request: `--baseline`/`--experiment`, or recordings that carry an `arm`
label. Per-host sections follow from this: with two rezolus sources,
rezolus's sections render once per source (per-source scope; after Q12,
the source's template instantiated once per source that carries it,
"Crates"). This decides the design gap this entry recorded under
"cachecannon" (its option (a)).

What metriken origin/main (ab81e2a) has to build on:

- `ArchiveReader::composition_sources` (metriken-storage `src/reader.rs:948`)
  returns one recording's tables as lazy composition sources, and
  `ParquetBuilder::source_labeled` (metriken-query `src/parquet.rs:556`)
  merges a source into a multi-source reader with extra labels on every
  series. Composing each recording from `open_recordings` under its labels
  gives a combined reader with no new code. rezolus uses this only in tests
  (`crates/rez/src/reader.rs`,
  `composition_sources_merge_recordings_under_distinct_labels`).
- That multi-source reader (`MultiParquetSource`, metriken-storage
  `src/parquet.rs`) strips the added keys from the filter it passes to each
  member and then inserts them into each returned series. A member series
  that already carries one of the keys trips a `debug_assert!`; in a release
  build the added value replaces the series' own.
- The composed reader does not apply two things `route()` applies within one
  recording: the cross-cadence evaluation timestamps (`eval_timestamps_for`
  in `archive.rs`, documented as answering for one recording and passed by
  the caller), and the refusal of a query over a metric name two samplers
  share (`gpu_nvidia` and `gpu_amd_smi` both publish `gpu_utilization`; the
  `composition_sources` docs, "Composition tolerates name-sharing that
  `route` refuses").
- `composition_sources` refuses a flattened multi-recording reader. Its docs
  note that two recordings may carry identical label sets (the recorder only
  warns), and that composing those under their own labels merges their
  series.
- `__run__` (metriken-storage `src/segmented.rs:398`, `:1241`) is a label the
  reader adds, and reserves, to split one table's histogram series by
  configuration run. It is a precedent for a label the reader adds; it does
  not identify a recording.

Design consequences:

- The labels are added in metriken-query or metriken-storage, not in the
  viewer, so `rezolus mcp`, `recording check` and metriken-py see the series
  the dashboard sees. Which of the two is sub-question 1.
- The viewer decides from the recording labels and the command line: A/B
  when `--baseline`/`--experiment` is given or the recordings carry `arm`,
  combined otherwise. One function in metriken-dashboard makes this
  decision for both shells (GO 2). A single recording is the combined case
  with one member, so its series carry its labels too, and a template
  selecting `source="cachecannon"` matches a cachecannon archive of one
  recording.
- `rezolus view`: `init_file_mode_rez` opens the combined source by default
  and calls `resolve_ab_slots` only in the A/B case. Follow mode finds each
  recording again by its label set and already refuses duplicate label sets,
  which sub-question 4 affects.
- The WASM viewer: `Viewer::new` opens a multi-recording archive as one
  combined source instead of refusing it, and `attach` fills two slots only
  under the A/B rule.
- A source's template is instantiated once per source that carries it (or
  that the registry's `match` predicate selects, for a source without one),
  and every query in an instance is restricted to that source's series by
  its recording labels. (Until Q12 this was section providers taking the
  claimed source in `sections` and `generate`.) With one such source the nav
  is as today; with two, each set of entries is named by `SourceEntry.name`
  (host). How a template's queries get the matcher that selects one
  source's series is open (sub-question 8).
- cachecannon's `--server` and `--client` parquets become recordings of one
  archive, and its Server and Client sections are rezolus's sections, once
  per host.
- This lands before step 7 moves code (Q6), in rezolus and in metriken: the
  series labels need a metriken-query or metriken-storage release (wherever
  sub-question 1 puts them), and the combined view and the A/B rule are
  rezolus work on that release. Both land with path step 5, which is where
  templates first have to match. Per-source instantiation of the rezolus
  template needs the converted sections, which exist from step 1 below.

Open sub-questions:

1. Where the labels are added: by composing in metriken-query
   (`composition_sources` and `source_labeled`, which exist, with the
   differences from `route()` above), or by `route()` in metriken-query's
   `archive.rs` adding each recording's labels to its series and joining
   recordings instead of refusing them (keeps per-query table selection and
   both refusals; new code). The cost of either on a large multi-recording
   archive is not measured; the 9.6 h archive has one recording.
   Decided 2026-10-13: `route()`. Built; see "Series labels, as built"
   below.
2. Which recording labels become series labels: all of them, or a fixed set
   (`source`, `host`). Whether `arm`, which selects A/B, is also one.
   Decided 2026-10-13: all of them, `arm` included.
3. A recording label whose key a series already carries (a scraped
   Prometheus series with `host` or `source`): replace, as
   `MultiParquetSource` does in a release build, refuse, or rename one.
   Decided 2026-10-13: the recording's value wins and the series' own moves
   to `exported_<key>`, as Prometheus does with `honor_labels: false`: also
   when the values are equal, and one prefix further when the series has
   `exported_<key>` too.
4. Two recordings with identical label sets (two endpoints that infer the
   same `source` and `host`): refuse, or add a reserved label
   (`__recording__`, under the `__` rule of rezolus `docs/labels.md`) to keep
   their series apart.
   Decided 2026-10-13: `__recording__`, added only to recordings whose
   label set another recording shares, carrying the recording's catalog
   index. Twins are found among every recording of the reader, tables or
   not, so the label neither changes value nor comes and goes on a live
   reopen. A twin group that repeats an index (readers of different archives
   joined by `flatten`) is numbered by position.
5. Recordings of different runs. `recording combine a.rez b.rez -o ab.rez`,
   rezolus's documented A/B form, does not set `arm` (no non-test `"arm"`
   under `src/parquet_tools/`), so under the rule above its output opens
   combined, with two runs at different wall-clock times on one axis. Either
   `combine` stamps `arm`, or the rule also asks whether the recordings'
   time ranges overlap.
6. The cross-cadence evaluation timestamps in a combined view, where each
   recording has its own row times.
   Built 2026-10-13: applied across recordings whose time ranges overlap
   (recordings of one run share a wall clock), each sampler keyed by its
   recording; recordings that do not overlap stay on the uniform grid. In
   `RateMode::Raw`, which reads each series at its own rows by contract, a
   query combining series across recordings is refused. See "Series labels,
   as built".
7. Whether `rezolus mcp`, which refuses a multi-recording archive without
   `--recording` today, reads the combined source by default.
8. (Added 2026-10-12 with Q12.) How a template instance's queries are
   restricted to one source's series: the expander inserting the source's
   recording-label matchers into every selector, or the template naming a
   viewer-supplied selector (as it names `${throughput}` and the cgroup
   placeholder). The rezolus template's queries carry no `source` matcher
   today, because the Rust generators wrote none.
9. (Added 2026-10-12.) Which header a template instance's `View` carries in
   a combined view. `View::new` (rezolus `crates/dashboard/src/plot.rs:103`)
   fills `interval`, `source`, `version` and the start and end time from the
   `MetricsSource` (`time_range()` comes from the rows), so an instance
   needs either its own recording's values or the combined reader's. Which
   one is open.

#### Series labels, as built (2026-10-13)

metriken branch `feat/recording-series-labels` (metriken-storage and
metriken-query, unreleased):

- `SamplerReader` holds its recording's labels in an `Arc` shared by the
  recording's tables; `same_recording` compares the pointers, so recordings
  of readers joined by `ArchiveReader::flatten`, whose `recording` numbers
  repeat, stay distinct. `ArchiveReader::series_labels` gives each
  recording its labels plus `__recording__` when its label set is shared.
- metriken-storage `labeled::LabeledSource` wraps a `DataSource`: every
  returned series (counters, counter streams, the counter scan, gauges,
  histogram streams, label listings, `column_map`) gets the labels, with a
  clashing key's own value under `exported_<key>` (equal values too, so a
  series with the key and one without stay apart; a taken
  `exported_<key>` moves one prefix further). A filter on an added
  key is decided against the added value and stripped; one on
  `exported_<key>` is decided on the relabelled series (a scan declines
  then, and a histogram drops the rows), because whether a series has
  `exported_<key>` depends on both values.
- metriken-query `route()` groups the tables a query names by recording,
  builds each recording's source as before (the table, or a `try_new`
  union that still refuses a name two samplers share), wraps it in
  `LabeledSource`, and joins several recordings through
  `MultiParquetSource`. `columns` resolves through the same wrapper. The
  cross-recording refusal is gone.
- Alignment across recordings (decided 2026-10-13, after a review found
  aggregations across recordings wrong). The engine already aligned two
  ways: the grid evaluates every series at `start + k·step` with windowed
  rates, which handles recordings at one cadence and any phase; and the
  cross-cadence timestamps move a query onto the slow sampler's rows. The
  second was first applied within one recording only, which left
  different cadences across recordings on the grid: with a 1 s and a 10 s
  recording, `sum(g)` read 110 at the slow one's rows and 10 between.
  Applying it to every query over overlapping recordings (the next
  attempt) was wrong too, as the following review showed: a recording
  starting halfway through cut every other series to the slow rows inside
  its span and dropped them outside it (`g` over 40 s kept 2 of 40 points),
  and even a query combining nothing was drawn at the slow resolution. As
  built: across recordings the timestamps apply only to a query that
  combines series of different recordings (`combines_across_recordings`,
  from `promql::combinations`), and only when the slow recording's rows
  cover every other recording's span to within one of its gaps. A query
  combines across recordings when it aggregates series of more than one,
  applies an operator with `on`, `ignoring` or a group modifier across more
  than one, or applies a plain operator whose sides come from different
  sets of recordings; a plain operator whose sides come from the same
  recordings pairs within each (`g / h`), and one whose sides differ is
  paired across them by the engine, which pairs a side of one series with
  every series of the other. What the engine had no answer for across
  recordings, because a metric's series always shared one table's rows:
  combining in `RateMode::Raw` (`sum(g)` alternated 10, 100, 10, 100 for
  recordings half an interval apart), now refused by the same rule;
  histogram functions, refused above; the range edges on the grid, where
  a `sum` reads only the recordings that have a row (the first point
  before the later recording's first row, and the last point when its next
  row lies past the range, read low); and different cadences where the
  slow recording does not cover the others, which stay on the grid and
  read the slow one only at its rows. Those last two are left as they are;
  trimming a combining query's range to the span every recording covers
  would address both. A stricter rule refusing every aggregation across
  recordings was considered and not taken: the grid answers
  `sum by (source)` correctly apart from those cases. `scalar()` and
  `absent()` reduce across series but are not supported by the engine; if
  they become so, `combinations` needs a case for them.
- `owners()` leaves out a table when no selector naming one of its
  metrics can match its recording's labels (`translate_filter` against
  `series_labels`); when that leaves none, the first recording's tables
  answer, empty, so
  `x{source="a"}` neither opens recording b's tables nor loses recording
  a's cross-cadence alignment.
- `HistogramStream::merge`, which `MultiParquetSource` uses to join
  recordings, decoded every stream under the first stream's bucket
  configuration (a `debug_assert!` only). Two recordings of `lat` at
  grouping power 3 and 7 panicked a debug build and, in release, read the
  second recording's buckets as other buckets. It now leaves out streams of
  another configuration with a warning, as a table's `__run__`s are split.
  Found in review; for an archive, the refusal below now decides first.
- A query that reads one histogram from several recordings is refused,
  naming the recordings and a matcher to use (`lat{source="a"}`). The
  histogram functions pool each row of the series they select
  (`quantiles_impl` sums every series' deltas per row timestamp), and two
  recordings' rows fall at different times: a reviewer joined two
  recordings of `lat` whose rows were half a second apart and got
  `histogram_quantiles([0.5], lat)` alternating 2, 959, 2, 959 between
  them on the union of their row times. A combined view therefore draws a
  histogram per source, which per-source template instances (sub-question
  8) do anyway. Pooling across recordings, onto a common grid, is left
  until something needs it.
- The filter's regex form is literals and alternations
  (`labels::match_pattern`), so `source=~"a|b"` works and `source=~"a.*"`
  matches nothing, as for any label.
- Tests: metriken-query `archive::tests` (one recording carries its
  labels; two recordings queried together, by `source=`, `!=`, `=~`,
  `sum`, `sum by`; the `exported_` rename and its matchers; identical label
  sets; the shared-name refusal; `columns`; cross-cadence across
  recordings of one run and not of two; `sum` across a 1 s and a 10 s
  recording; a partly overlapping recording; only combining queries moving
  onto slow rows; which operators Raw refuses) and metriken-storage
  `labeled::tests`. Seen red: no
  wrapper in `route`, no `__recording__`, `exported_` matchers unchecked,
  the clashing value dropped, cross-cadence across recordings, the merge
  mixing configurations, no owner pruning, `__recording__` by position, no
  chained prefix, no histogram refusal, catalog indices kept when
  flattened twins repeat one, twins only among recordings with tables, no
  cross-cadence across recordings, no overlap check, no Raw refusal, no
  coverage check, every operator across recordings counted as combining,
  non-combining queries moved onto slow rows.
- Cost, measured on `metrics-9.6h.dendro` (one recording), display queries
  at a 500-point budget, five runs per query, three alternating rounds of
  upstream main (95d0d52) and this branch: medians 39.7 to 42.1 ms and 39.9
  to 41.2 ms for `sum by (id) (irate(cpu_usage[5m])) / 1e9`, 24.6 to 24.7
  and 24.5 to 26.6 ms for `irate(cpu_usage[5m])`, 391.8 to 396.1 and 400.3
  to 417.7 ms for `histogram_quantiles([0.5, 0.99],
  scheduler_runqueue_latency)` (one base round, run while the host was
  loaded, is left out: 2 to 5 times slower on every query). Results are
  identical between the builds for three queries; `sum(irate(cpu_usage[5m]))`
  differs from run to run within each build (its last digits), so it was not
  compared. No cost above the run-to-run spread was seen; a multi-recording
  archive was not measured. Rerun after the review fixes (owner pruning
  parses the query's selectors once more per query): in the rounds the
  host left quiet (two of three per build), medians 39.4 and 40.4 ms
  against 40.6 and 44.4 ms, 24.2 and 24.4 ms against 24.3 and 24.9 ms,
  396.6 and 399.8 ms against 400.1 and 401.9 ms for the same three
  queries.
- Joined recordings take the per-series rate path: `MultiParquetSource`
  does not implement `counter_scan`, so a query reading two recordings
  skips `batch_rates`. Not measured.
- The engine's binary operators pair series across recordings now that
  such a query is answered: `rate(x[3s]) / rate(y[3s])` with `x` only in
  `source=a` and `y` only in `source=b` returns one series labelled
  `source=a` (a reviewer ran it), where Prometheus returns nothing because
  the label sets differ. This is the engine's vector matching, not this
  change; it was unreachable while the query was refused.
- What this changes for rezolus, on the release: series of every archive
  carry `source` and `host` (and `__recording__`, `exported_<key>` where
  they apply), and the cross-recording refusal its tests assert is gone.
  The viewer names a series by its first visible label in sorted order
  (`src/viewer/assets/lib/labels.js`, `firstVisibleLabelValue`) and keys
  the `multi` chart style by it, so with `host` added a per-CPU chart's
  series would all be named, and keyed, by the host. The viewer has to
  treat recording labels as it treats a constant label (leave them out of
  a series' name when every series of the chart shares them) before it
  moves to this release.
  The combined view (`init_file_mode_rez`, the WASM `Viewer::new`) and
  `rezolus mcp`'s default (sub-question 7) are rezolus work on this release.

### Q12. How do templates travel, and do rezolus's sections become templates?

Decided 2026-10-12: a producer's template travels in its stream, in the
handshake's metadata or in a frame sent after the handshake (path step 5),
and the viewer reads it from the source's metadata in the archive. dendro
0.4.0's `Frame` has `Handshake`, `Index`, `Rows`, `Segment` and
`ClockOffset`, and no variant that changes a source's metadata after the
handshake, so the second form needs a dendro change; the handshake form
needs none (plan piece 3). Which form step 5 uses was open; the evaluation
below settles it as the handshake.

Direction to evaluate (decided 2026-10-12; kept as history, the evaluation
and the decisions after it follow): express rezolus's sampler sections as templates too, sent by the
agent in its stream like any producer's. `rezolus-dashboard` would then be
data in the archive, and no viewer would depend on a rezolus crate. If the
evaluation succeeds, it changes these decisions:

- Q2: no `rezolus-dashboard` crate, so the Q2 consequence goes (a
  metriken-query change would no longer wait for a rezolus release), and
  cachecannon pins no rezolus prerelease.
- Q3: the bundle no longer needs rezolus's sections compiled in, so a generic
  bundle (Q3 (b)) would serve rezolus's static site.
- Q7: the fallback for recordings older than the agent's templates still
  needs rezolus's sections from somewhere; the evaluation says where.

The known obstacle is that some sections contain logic the `ServiceExtension`
format cannot express (Q2 (c)). rezolus `crates/dashboard/src/plot.rs:53`,
`blockio_device_latency_metric`, picks `blockio_device_latency` or the
pre-rename `blockio_latency` by which exists; `plot.rs:78`,
`has_host_traffic_counters`, decides whether a plot appears; `gpu.rs`
enumerates `(vendor, id)` pairs. The template format may need conditionals
(a plot or query chosen by which metric exists) and repetition over label
values. (Until the evaluation reported, the design below followed Q2 (b).)

#### Evaluation (2026-10-12; the owner accepted it, GO)

The evidence is rezolus upstream/main 50075985's sections and a prototype in
a scratch directory, which is not committed. Each figure is marked as read
from the code, measured, or estimated.

- **What the sections decide from** (read from the code). The 17 generators
  (the 16 entries of `SECTION_META`, the query explorer among them, and
  Overview) decide what to draw from the metric catalog only (the query
  explorer draws nothing; it is an 8-line empty `View`): `has_counter`,
  `counter_labels` and `gauge_labels`, `label_values`, `has_metric`,
  `metric_unique_label_count`. None reads a sample value, so a template
  expanded against the catalog can make the same choices.
- **Prototype** (measured). CPU, GPU and BlockIO were converted, 1,446 of the
  3,799 production lines of the 17 generators (the 3,791 of the 16 sections
  counted under Q2, plus the query explorer's 8). A 200-line Python expander
  reproduced the Rust generators' JSON byte-identical in all 12 comparisons:
  3 sections, each against four catalogs (all metrics, one of each, a
  pre-rename legacy catalog, empty). Three deliberate template mutations
  each produced a mismatch, so the comparison detects a wrong template.
- **Format additions** over `ServiceExtension` that the evaluation found
  necessary:
  - `when` predicates: `present`, `has_label {metric, label, value?}`,
    `distinct_gt {metrics, by, require?, n, each?}`, `any`/`all`/`not`,
    `var`, `param`;
  - `let`: a named predicate, or `first_present {list, default}` (which
    expresses `blockio_device_latency_metric`);
  - viewer parameters, such as `${throughput}`;
  - metadata pass-through and computed `selectors` (`gpu_selector`);
  - `{"builtin": "query"}` to place the query explorer, a library section,
    in the template's order between Exceptions and CPU. The evaluation also
    listed `"overview"`; it is dropped, because Overview is a rezolus
    section to convert (`overview.rs` queries `cpu_usage` and
    `network_bytes`, calls `has_host_traffic_counters`, and is what
    `${throughput}` exists for);
  - `split` (an aggregate and its companion, `first`/`alone`);
  - `repeat {var, in}` (over label values, as `gpu.rs` iterates devices);
  - plot macros: `histogram_summary`, and presets `latency` and `pct`;
  - one rule: a group or subgroup left empty after expansion is omitted.
- **Not expressible directly.** The selector output shapes and the cgroup
  placeholder are viewer features, which the template names rather than
  defines. `distinct_gt` needs an `int` flag to parse GPU ids as integers;
  that is untested. Source classification for a recording with no template
  becomes a `match` predicate over source metadata in the registry entry
  (Q7).
- **Size.** The three converted sections are 24,867 bytes minified and
  5,565 bytes at gzip -9 (measured). The full set is estimated at 60 to 95
  KB minified, about 17 KB gzipped. It is sent once per subscription in
  `Frame::Handshake`'s metadata, a `BTreeMap<String, String>`, well under
  dendro's `MAX_FRAME_BYTES` of 64 MiB. No dendro frame updates a source's
  metadata after the handshake, so the template goes in the handshake.
  dendro stores `sources.metadata` as uncompressed JSON; a rezolus source's
  metadata is about 37 KB today (33 KB of it metric descriptions) and
  would be about 110 KB with the template (estimated).
- **Versioning.** `schema` versions the template language. A viewer expands
  every schema up to its maximum, and keeps doing so. A template with an
  unknown schema, predicate or node is rejected whole, and the viewer uses
  the registry entry for that source instead, with a notice. The stream
  has no negotiation, so a viewer that supports schema N is released before
  any agent sends N. `revision` is informational.
- **Optional.** rezolus's recorder hook could write the built-in template
  into a source whose handshake lacks one, never replacing one that is
  present. Hook metadata currently outranks handshake metadata, so the hook
  must only fill a missing `dashboard` key.

#### Decided after the evaluation (2026-10-12)

1. rezolus's sections become a template the agent sends in its handshake.
   This replaces the planned `rezolus-dashboard` crate (Q2 (b)) and the
   section-provider trait ("Crates", as it stood before this decision).
   - A new section-template document, schema 1, separate from
     `ServiceExtension`. It mirrors the `View`/`Group`/`SubGroup`/`Plot`
     model and adds the conditions and other additions listed above.
   - An expander in metriken-dashboard, built first inside rezolus's
     `crates/dashboard` (order of work, step 1).
   - The agent embeds the template and sends it under a `dashboard` key in
     the handshake's metadata.
   - The 16 sections (Overview and the 15 other `SECTION_META` entries; the
     query explorer is placed by `{"builtin": "query"}`, not converted) are
     converted, and each is checked byte-identical against the Rust
     generator's output on fixture catalogs (GO 8). The generators stay in
     rezolus's `crates/dashboard`, an unpublished transitional crate that
     systemslab depends on by git, and GO 8 runs in rezolus CI while both
     exist. They are deleted in a 6.x release after systemslab is on
     metriken-dashboard plus the rezolus template (Q2, decided 2026-10-12).
   - A recording without a template falls back to the rezolus template in
     the viewer's registry, matched by `source == "rezolus"`, a
     `sampler_status` key, or a metric in `REZOLUS_SELF_ANCHORS` (Q7).
   - The TUI keeps its Rust copies of `blockio_device_latency_metric` and
     `has_host_traffic_counters`.
2. Precedence: the archive's template renders by default. A viewer flag,
   like `rezolus view --templates`, renders with the registry instead.

What this changes in the decisions above:

- Q2: (c), in the form of the new document, supersedes (b) and the decision
  to release `rezolus-dashboard` with rezolus (Q2, "Superseded").
- Q3: a generic bundle would now serve rezolus's site; the decision stays
  (a) and can be revisited (Q3).
- Q7: the rezolus template joins rezolus's fallback registry, and
  systemslab registers it instead of relying on the built-ins that
  `build_dashboard_context(.., &[])` shows.
- Q11: per-source scope means instantiating a source's template once per
  source that carries it; restricting each instance's queries to one
  source's series is open (Q11, sub-question 8).

## Design

### Crates

Following Q1 (c): `metriken-dashboard` in this repository, released with
metriken-query; `metriken-viewer` in `iopsystems/metriken-viewer`, depending
on the metriken crates by crates.io version. Both are 0.x. rezolus's
sections are a template in rezolus's tree that the agent sends (Q12), not a
crate, so a metriken-query release that changes `MetricsSource` reaches an
application after metriken-dashboard and `metriken-viewer` have released
against it, with no rezolus release in between. (Until Q12 this paragraph
described a third crate, `rezolus-dashboard`, released with rezolus.)

**`metriken-dashboard`.** Depends on metriken-query and metriken-storage
(reader features only), serde, serde_json. No `metriken`, no threads; builds
for wasm32. Holds:

- the model (`plot.rs`: `View`, `Group`, `Plot`, `PlotOpts`, `Unit`, and
  the generic `has_metric`), the display wire format, events, the metric
  catalog, capture aliases;
- `ServiceExtension`, `CategoryExtension`, `Kpi`, `Check`, and
  `TemplateRegistry` without an embedded set (`embedded()` and
  `from_embedded` go; `load(dir)`, `from_templates` and `insert_template`
  stay). The registry holds section templates as well as `ServiceExtension`
  files, each section template with its `match` predicate (Q7);
- the section-template document (schema 1) and its expander (Q12): it
  takes a template, the metric catalog of one source, the viewer parameters,
  and the source itself (or a header taken from it), because `View::new`
  (`plot.rs:103`) fills `interval`, `source`, `version` and the time range
  from the `MetricsSource`; which header an instance gets in a combined view
  is open (Q11, sub-question 9). It evaluates `when` and `let` against the catalog, expands
  `repeat`, `split` and the plot macros, places `builtin` sections, omits
  groups and subgroups left empty, and returns the same `View` the Rust
  generators return today. It rejects a template with an unknown schema,
  predicate or node whole;
- `service_extensions_from_metadata`, moved from rezolus's
  `src/viewer/metadata.rs` (path step 5 moves it into the dashboard crate first,
  so both shells call it), extended to the lookup order under "Templates";
- source classification (`source_kind.rs`) without the rezolus fingerprint
  (`REZOLUS_SELF_ANCHORS`, `source == "rezolus"`), which becomes the rezolus
  template's `match` predicate in rezolus's registry, so the library does
  not name rezolus. The fingerprint is removed in metriken-dashboard at step
  2; rezolus's `crates/dashboard` keeps it until it is deleted. Two
  functions in `plot.rs` name rezolus metrics:
  `blockio_device_latency_metric` (`plot.rs:53`, picks
  `blockio_device_latency` or the pre-rename `blockio_latency`) and
  `has_host_traffic_counters` (`plot.rs:78`, tests for
  `network_host_bytes`). Their callers are the rezolus sections (`blockio`,
  `network`, `overview`) and the TUI's overview. The template expresses them
  with `let` (`first_present`) and `when`; the TUI keeps Rust copies of both
  in rezolus (Q12, decision 1). Neither is in the library;
- the section context and the template, category, simple-capture and query
  explorer sections.

There is no section-provider trait (Q12 removed the `SectionProvider` trait
this entry proposed). An application passes its fallback registry and the
precedence flag:

```rust
pub struct Dashboards {
    registry: TemplateRegistry, // ServiceExtension files and section templates with `match`
    prefer_registry: bool,      // true: registry over the archive's template (`--templates`)
}
```

In metriken-dashboard, `build_dashboard_context` and `generate_section` take
a `&Dashboards` instead of reading `SECTION_META`; rezolus's
`crates/dashboard` keeps the old signatures until it is deleted (Q2).
Today's `SourceEntry { is_rezolus }` becomes the section template chosen for
the source ("Templates"). Scope is per source (decided, Q11): a template is
instantiated once per source that carries it, or that the registry's `match`
selects; two such sources yield one set of nav entries each, named by
`SourceEntry.name` (host), and with one the nav is as today. How an
instance's queries are restricted to its source's series is open (Q11,
sub-question 8). Names and shapes are settled in step 1 below, against
rezolus's and cachecannon's callers, and made in metriken-dashboard at step
2.

In metriken-dashboard the empty source list loses its special case
(`crates/dashboard` keeps it for systemslab). Today
`build_dashboard_context` shows the built-in sections when `sources` is
empty or any entry `is_rezolus` (`dashboard/mod.rs:106`, "legacy caller"),
and systemslab relies on the empty case: it calls
`build_dashboard_context(filesize, &service_exts, None, &[])` (systemslab
`crates/dashboard/src/rezolus.rs:35`). With no Rust sections there are no
built-ins to show, so systemslab, in its own entry, stops relying on it: it
registers the rezolus template (Q7) and passes its recording's sources, and
the rezolus template is chosen by the source's `dashboard` key or by the
registry's `match`.

The query explorer is a library section, but today it is a rezolus nav
entry: it sits in `SECTION_META` between Exceptions and CPU, so it appears
only when a rezolus source is present. The template places it with
`{"builtin": "query"}` between its Exceptions and CPU entries (Q12), so
the library appends nothing after a template's sections. Overview is not a
builtin; it is one of the converted rezolus sections. (Before Q12 this was an open choice
for step 1, for example a `Section::library("query")` entry.)

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

A producer's template reaches the archive through its stream, in the
handshake's metadata (Q12), and the viewer reads it from the source's
metadata. A `ServiceExtension` travels under `service_queries`; a section
template travels under `dashboard`, which the rezolus agent sends from step
1 below (path step 5 reserves the key). The agent sends no
`service_queries` today (no occurrence under rezolus `src/agent` at
50075985); a producer such as cachecannon sends its `ServiceExtension`
there. A multi-recording archive from one run
opens as one combined view whose series carry their recording's labels, so
a template's `source` matchers select its producer's series (Q11).

`dashboard` and `service_queries` are independent: a source's section
template and its KPI sections both render; the fallback registry is
consulted for each only when the source lacks it. This keeps today's
behaviour, where a rezolus recording annotated with `recording annotate
--queries` shows the built-in sections and its service sections
(`build_dashboard_context`, `dashboard/mod.rs`: `show_builtins` and the
service sections are decided separately). For each source:

- section template: the source's `dashboard` key, else the fallback
  registry's section template whose `match` predicate selects the source
  (the rezolus template's predicate, Q7);
- KPI sections: `service_queries` in the source's metadata (archive) or file
  metadata (parquet), else `per_source_metadata`, else the registry's
  `ServiceExtension` by source name;
- a source with neither gets a simple-capture section.

One function in metriken-dashboard implements this for both shells. With
the precedence flag set (`Dashboards::prefer_registry`, from a viewer flag
like `rezolus view --templates`), the registry is consulted before the
archive's keys for each part (Q12, decision 2). A template whose schema, predicate or node
the viewer does not know is rejected whole, the registry entry for that
source is used, and a notice says so. After path step 5 a cachecannon or
llm-perf archive shows its dashboard in `rezolus view` with no rezolus
template, and a rezolus agent's archive shows rezolus's sections from its
own template; the fallback only serves recordings older than that.

A viewer that supports schema N is released before any agent sends schema
N, since the stream has no negotiation; the first agent that sends a
template is not released before a viewer that expands it. rezolus's
recorder may also write the built-in template into a source whose handshake
lacks a `dashboard` key, never replacing one that is present (Q12,
evaluation, "Optional"); whether it does is not decided.

### What stays in rezolus

The rezolus section template, which the agent embeds and sends under
`dashboard`, and the fallback registry of 12 files (the rezolus template
with its `match` predicate, and the 11 `ServiceExtension` files) that
`rezolus view` and rezolus's `cdylib` register (Q7, Q12); the `view` CLI
(selection, `--templates`, browser open, `--tui`), the probe for live mode,
the TUI with its Rust copies of `blockio_device_latency_metric` and
`has_host_traffic_counters`, MCP, `site/` (landing, docs, the
viewer page built from the library), `rezolus-viewer-<tag>.tar.gz` in the
release workflow, the JS tests that drive rezolus's built bundle
(`wasm_viewer_*`), and the `viewer-smoke` and `viewer-render` skills pointed
at `rezolus view`. `crates/rez` and `crates/report-save` are deleted
(`crates/rez`'s re-exports are replaced by imports from metriken-storage).
`crates/dashboard` stays, unpublished, as a transitional crate with its
Rust generators and the API systemslab uses, so systemslab can pin rezolus
6.0.0 by git tag; rezolus's own viewer does not use it, and its CI runs GO 8
against it. Its generators and Rust section API, and with them the crate,
are deleted in a 6.x release after systemslab has moved (Q2, decided
2026-10-12). The `viewer-parity` skill moves to
`iopsystems/metriken-viewer`, rewritten for one crate.

### cachecannon

`cachecannon view` builds a `Viewer` with a `Dashboards` whose registry
holds `cachecannon.json` and the rezolus template, and serves its
`router()` on the axum server it already runs. A rezolus recording made by
an agent that sends its template needs no registry entry; the rezolus
template in cachecannon's registry serves older rezolus recordings, and it
is obtained from rezolus by the mechanism step 1 of the order of work
decides. Its
`src/viewer/` (1,098 lines of Rust in 11 files) and its assets (5,902 lines
of JS and CSS, 5,922 with `index.html`) are deleted. Its Rust sections are
replaced by its template. There is no Rust provider to fall back on (Q12),
so a plot the template cannot express needs an addition to the template
language, which is a new schema. `--server` and `--client` parquets become
recordings of one archive with the benchmark, which opens as one combined
view (Q11).

Two rezolus sources in one archive (decided 2026-10-12, Q11). Today
`cachecannon view --server s.parquet --client c.parquet` draws a Server
Metrics and a Client Metrics section (CPU, memory, network, runqueue
latency), one per host. Before the decision, the design would have shown
the rezolus sections once over both hosts, as today's code does: `build_dashboard_context` lists each
source as a `SourceEntry` with a resolved name (`source_kind.rs:140`) but
shows the built-in sections once if any source is rezolus
(`dashboard/mod.rs:106`), and their queries carry no source matcher. The
options were per-source provider scope, a cachecannon section provider
redrawing Server and Client sections, or one set of sections over both
hosts. The experiment slot was not an option for the client, because it
renders the client as an A/B overlay of the server. Per-source scope was
chosen: rezolus's sections render once per rezolus source, restricted by
that recording's labels (after Q12, the rezolus template instantiated once
per rezolus source; the restriction is Q11's sub-question 8), which also gives `record --endpoint a --endpoint b`
and fleet recordings per-host sections. GO 1's fixture holds two rezolus
agents so this is tested.

## GO / NO-GO

GO when all hold:

1. **One dashboard.** For one `.dendro` from one run holding two rezolus
   agents (server and client hosts) and a cachecannon run,
   `/api/v1/sections` and every `/data/<section>.json` from `cachecannon
   view` and `rezolus view` are byte-identical (plan GO 5). Both open it as
   one combined view, not A/B; cachecannon's dashboard comes from the
   archive's template, its `source` matchers select the cachecannon
   recording's series, and rezolus's sections appear once per agent, each
   querying only that agent's series (Q11).
2. **Shell parity.** A native test builds the server state and the WASM
   `Viewer` (the crate is `rlib` as well as `cdylib`, as rezolus's
   `crates/viewer` is today) from the same fixtures and asserts `sections`,
   every section, `metrics`, `systeminfo`, `file_metadata` and `metadata`
   are byte-identical. The test is a function in metriken-viewer (feature
   `test-support`) that takes a `Dashboards`, because the rezolus sections
   are not in that repository: metriken-viewer's CI runs it with templates
   only (a recording with a template, a simple capture, a two-recording
   archive from one run opened combined, and a two-recording archive whose
   recordings carry `arm`, opened A/B), and rezolus's CI runs it with
   rezolus's fallback registry on a rezolus recording, one with the
   `dashboard` key and one without. No such test exists today; the skill describes it and the
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
   Peak footprint is not a measure of the data the viewer holds on this
   host: the storage scan's gate failed it at +8.7% with no overlap while
   `heap -s` showed the same live heap in both builds (its "Gate result";
   the owner granted an exception without changing the gate). So each run
   also records live heap after the load (`heap -s`), and a footprint
   failure with live heap equal to within 1 MB is reported with both
   numbers and put to the owner rather than read as a regression; a live
   heap more than 5% larger fails on its own.
6. **No registry in the browser.** For wasm32-unknown-unknown, `cargo tree
   -e normal` lists no `metriken`, `metriken-core` or `linkme`, and `cargo
   check` of the same configuration is a CI step, in each of two
   repositories: this one for `-p metriken-dashboard`, and
   `iopsystems/metriken-viewer` for `-p metriken-viewer --no-default-features
   --features wasm`. (Until Q12 a third check covered rezolus's `cdylib`,
   which pulled in `rezolus-dashboard`; with templates it adds no Rust
   dependency beyond `metriken-viewer`.)
7. **Developer mode.** With `Assets::Dir`, an edit to a file under a
   `metriken-viewer` checkout's `assets/` is served by `rezolus view` on the
   next reload without a rebuild.
8. **Sections as templates** (Q12). Each of the 16 converted rezolus
   sections (Overview and the 15 other `SECTION_META` entries), expanded
   from the rezolus template by metriken-dashboard's expander, is
   byte-identical to the Rust generator's JSON on every fixture catalog (at
   least all metrics, one of each, a pre-rename legacy catalog, and empty,
   as in the evaluation), and a mutated template fails the comparison. The
   generators' JSON is committed as golden fixtures, and the comparison is a
   rezolus CI check for as long as the generators exist in
   `crates/dashboard`, so a change to either that makes them differ fails
   CI. After step 3 the comparison runs metriken-dashboard's released
   expander against `crates/dashboard`'s generators (a test crate or
   dev-dependency taking both); the expander copy built in
   `crates/dashboard` at step 1 is deleted at step 3. Deleting the
   generators is gated on systemslab having moved to metriken-dashboard plus
   the rezolus template (order of work, step 6); after that the golden
   fixtures keep the template's output pinned. GO 8 compares the expansion
   before source restriction is applied. Restriction is checked separately,
   by GO 1 and a test that every selector in an instance carries the
   source's matchers.

NO-GO if any section cannot be made identical to its Rust output on the
fixture catalogs, or if the WASM exports cannot be registered from an
application crate. Then this entry records why, and the fallback is Q2 (b),
the decision Q12 superseded, for the sections, or a bundle built in
`iopsystems/metriken-viewer`. (Until Q12 the first condition was a
section-provider trait that could not carry rezolus's sections without
metriken-dashboard naming rezolus metrics, with Q2 (a) as the fallback.)

## Order of work

0. **Prerequisites, in rezolus and metriken.** Path step 4 (live viewer on
   `metriken-recorder`) and path step 5 (templates from the archive, with
   `service_extensions_from_metadata` in the dashboard crate and called by
   both shells, closing the WASM gap above) are released and rezolus is on
   them. Path step 5 includes Q11's combined view: a metriken-query or
   metriken-storage release that adds recording labels to series (Q11,
   sub-question 1), and on it, in rezolus's tree, a one-run multi-recording
   archive opening combined in both shells, with A/B shown only on request.
   Path step 5 reserves the `dashboard` key and adds it to the lookup order.
   Step 1 below defines schema 1, builds the expander,
   converts the sections and makes the agent send them. The rezolus 6.0
   entry's Save as Report change has landed (Q6).
1. **Split inside rezolus.** With every test still in one tree: the
   section-template document (schema 1) defined and the expander built in
   `crates/dashboard`; the 16 sections converted into the rezolus template
   with the comparison harness (GO 8), which runs each section's Rust
   generator and the expander on the fixture catalogs and compares the
   JSON; the generators' JSON committed as golden fixtures, and the Rust
   generators kept, with GO 8 a rezolus CI check while both exist (Q2,
   decided 2026-10-12); the agent embedding the template
   and sending it under `dashboard`, in a release that also carries a
   viewer that expands it; the
   fallback registry with the rezolus template's `match` predicate (Q7);
   `src/viewer` (minus the CLI and TUI) moved to
   a library crate `crates/viewer-server` with the API above, the assets
   moved into that crate, the WASM exports moved behind a feature with
   `crates/viewer` reduced to the registering `cdylib`, and `LiveReader`
   moved into `crates/viewer-server`. `RezReader` and its `MetricsSource`
   impl are taken from metriken-storage and metriken-query once the storage
   scan's move of `crates/rez` has released; until then they stay in
   `crates/rez`. `blockio_device_latency_metric` and
   `has_host_traffic_counters` leave `plot.rs`: the template expresses them,
   and the TUI keeps Rust copies (Q12). This step also settles:
   - per-source instantiation of a template (decided, Q11) and how an
     instance's queries are restricted to one source's series (Q11,
     sub-question 8), with a second rezolus agent in GO 1's fixture;
   - the directory in rezolus's tree that holds the template and the 11
     `ServiceExtension` files (Q7), and how a consumer outside rezolus
     (cachecannon at step 4, systemslab after 6.0.0) obtains the rezolus
     template for its fallback registry. Candidates: the template file
     published as an asset of each rezolus release, fetched by the
     consumer; or the file vendored by each consumer at a pinned rezolus
     tag, with a CI check that the copy matches the tag, as systemslab's
     `sync:charts` does for chart modules. Recommended, for the owner to
     decide: vendoring at a pinned tag with the sync check, because a
     consumer published to crates.io (cachecannon, if it publishes) must
     carry the file in its package, a build that fetches a release asset
     needs the network, and systemslab already syncs chart modules this
     way;
   - `build.sh` writing nothing next to itself and being run as
     `bash <path>/build.sh`, the path taken from the `manifest_path` that
     `cargo metadata` reports for the crate ("WASM bundle"), so it already
     works when the crate is a registry dependency.

   Everything step 1 adds to `crates/dashboard` (the expander, `Dashboards`,
   the registry's section templates) is added alongside the existing API.
   `generate_section`, `build_dashboard_context` with its empty-source
   case, and `TemplateRegistry::embedded()` keep their signatures and
   behaviour. The reshaped signatures and the removals are made in
   metriken-dashboard at step 2.

   The shell-parity test (GO 2) and the harness for GO 5 are written here.
   At this step GO 2, GO 4, GO 5, GO 6 (for `crates/dashboard`) and GO 8
   pass in rezolus, and GO 3 and GO 7 pass in their
   one-tree form: every test still in rezolus, and `Assets::Dir` pointed at
   `crates/viewer-server/assets`. This is a rezolus PR series. It changes
   no rendered section for one rezolus source (GO 8). Two things do change:
   an archive with two rezolus sources shows rezolus's sections once per
   source (Q11), and the agent's handshake carries the `dashboard` key.
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
   0.1.0 releases. GO 6 is then a CI step in both repositories it names.
3. **rezolus on the released crates.** The in-tree copies of the viewer
   crates are deleted; `crates/dashboard` stays, unpublished, for systemslab
   and GO 8, and rezolus 6.0.0 ships it with the API systemslab uses
   unchanged (Q2); the expander copy built in it at step 1 is deleted, and
   GO 8 runs against metriken-dashboard's released expander. The binary's
   other `dashboard::` users (`src/parquet_metadata.rs`,
   `src/parquet_tools/{check,combine}.rs`, the TUI) import from
   metriken-dashboard. The site build uses `assets::write_to`, the 62
   `site/viewer/lib/` symlinks and their CI check go, the 11
   `site/viewer/templates/` symlinks are re-pointed at rezolus's template
   directory (Q7). GO 2 to GO 6 are re-run on the released crates. rezolus
   publishes nothing to crates.io and cuts no prerelease for this step.
   (Until Q12 this step published `rezolus-dashboard` from rezolus's release
   workflow and cut a 6.0.0-alpha tag for it.)
4. **cachecannon branch** on the released crates (GO 1). It reads `.dendro`
   and sends its template in its handshake by then (its steps 5 and 6). It
   depends on no rezolus crate; rezolus's sections
   come from the template in the archive, or from the rezolus template in
   cachecannon's registry for older recordings, obtained as step 1 decides
   ("cachecannon"). Merged on
   cachecannon's schedule (Q9); a later metriken-query release reaches it
   after metriken-dashboard and metriken-viewer release against it.
5. **systemslab** in its own entry, after rezolus 6.0.0 (Q9). Its
   rezolus-6.0 PR first moves its git pin on `crates/dashboard` to the
   v6.0.0 tag with no change to its dashboard calls; its `rez` imports move
   to metriken-storage (plan, "Library users"), and its metriken-query
   follows rezolus 6.0.0's. Its entry then moves it to metriken-dashboard
   plus the rezolus template.
6. **Delete the generators**, in a rezolus 6.x release after step 5 lands:
   `crates/dashboard`, with the Rust generators, is deleted; rezolus has
   not used it since step 3. The golden fixtures from step 1 keep the
   template's output pinned (GO 8).

## Left alone

- Mounting the page under a path other than `/` (Q10).
- A generic WASM bundle published by the library (Q3 b). After Q12 it
  would serve rezolus's site; Q3 records when to revisit it.
- Moving the TUI into a library.
- The cachecannon defect noted under "Consumers" (`--server` and `--client`
  sections queried against the benchmark parquet) is not reproduced; it goes
  away when cachecannon mounts the library, and is to be reported to
  cachecannon if it is confirmed before then.
- The dashboard load numbers are from a laptop; the GO 5 runs use one host
  for both builds, which is all the rule needs.
