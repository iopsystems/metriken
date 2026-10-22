# metriken-storage

Storage for metriken recordings: the parquet segment format (wide and long
tables, occupant streams), the column readers the query engine reads
through, and the archive containers: dendro, and `.rez` behind the `rez`
feature, read and written. `metriken-query` runs queries over these.

Features:

- `archive` (default): the dendro archive (catalog, `ArchiveReader`, the
  segment encoder and column filter). Without it nothing here compiles
  SQLite.
- `rez`: the `.rez` format; implies `archive`.
- `write`: the archive writers; implies `archive`.
- `convert`: `MsgpackToArchive`, a raw msgpack recording into a new
  `.dendro` archive; implies `write`. This crate writes no standalone
  parquet file, and reads those written before.
- `stream`: `metriken_exposition::stream` re-exported at
  `metriken_storage::stream` until the next breaking release, and the `StreamRow` and `Pass`
  impls for `rez`'s `AgentRow` and `AgentRows`, which stay.
- `lz4` (default): reading segments written with LZ4_RAW.
- `ingest`, `metriken`, `test-support`: see `Cargo.toml`.

Until 0.1.0 this crate was `metriken-segment`; it also took in the column
readers from `metriken-query`, `metriken-archive`, and rezolus's `.rez`
crate. See `docs/journal/2026-10-09-storage-scan.md`.
