# metriken-recorder

Records metriken producers into one `.dendro` archive, through
`metriken-storage`'s `ArchiveWriter`. A source is a producer's
`/metrics/stream` (read on its own timeline), a Prometheus endpoint (one
`prometheus/scrape` acquisition group per scrape, windowed by the round
trip), or a local source the caller takes passes of in process.

`detect` decides how each endpoint is recorded before the archive opens: by
its stream first, then by the caller's `Probe` hook with the route's answer,
then as Prometheus. `Recorder` runs an aligned tick with one commit per tick,
detects pending endpoints off the tick, reconnects dropped streams, refuses
an endpoint mid-run, writes producer restarts into the recording's metadata,
applies retention, and reports `Event`s and a `Summary`. It never exits the
process, never waits for the caller to read its events, and installs no
signal handler.

Features:

- `http` (default): `detect`, `Subscription`, `pump`, scraping and
  `Recorder::start`, over a `reqwest::Client` the caller builds, so TLS,
  headers and the crypto provider are the caller's. Without it the frame
  decoder and the Prometheus converter take bytes, and
  `Recorder::start_local` records local sources.
- `registry`: `RegistrySource`, the process's own metriken registry as a
  local source. It is the only feature that depends on `metriken` (and so
  on `metriken-core` and `linkme`).

A `Recorder` needs a tokio runtime with its time driver, and with `http` its
IO driver.

The design and its status are in
`docs/journal/2026-10-12-metriken-recorder.md`.
