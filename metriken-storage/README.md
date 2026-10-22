# metriken-storage

Storage for metriken recordings: the parquet segment format (wide and long
tables, occupant streams), the column readers the query engine reads
through, and the archive containers: dendro, read and written, and `.rez`
behind the `rez` feature, read only. `metriken-query` runs queries over
these.

Features:

- `archive` (default): the dendro archive (catalog, `ArchiveReader`, the
  segment encoder and column filter). Without it nothing here compiles
  SQLite.
- `rez`: reading the `.rez` format (v1/v2 tar, v3 SQLite), and with
  `write` converting a v3 `.rez` to a dendro archive; implies `archive`.
  Nothing here writes a `.rez`; since 0.5.0 recordings are written as
  dendro archives.
- `write`: the archive writers; implies `archive`.
- `convert`: `MsgpackToArchive`, a raw msgpack recording into a new
  `.dendro` archive; implies `write`. This crate writes no standalone
  parquet file, and reads those written before.
- `stream`: `metriken_exposition::stream` re-exported at
  `metriken_storage::stream` until the next breaking release, and the `StreamRow` and `Pass`
  impls for `rez`'s `AgentRow` and `AgentRows`, which stay.
- `lz4` (default): reading segments written with LZ4_RAW.
- `ingest`, `metriken`: see `Cargo.toml`.

Until 0.1.0 this crate was `metriken-segment`; it also took in the column
readers from `metriken-query`, `metriken-archive`, and rezolus's `.rez`
crate. See `docs/journal/2026-10-09-storage-scan.md`.
