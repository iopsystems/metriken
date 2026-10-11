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
- `convert`: `MsgpackToParquet`, msgpack snapshots into one parquet file.
  Needs neither dendro nor the archive.
- `stream`: `metriken_exposition::stream` at `metriken_storage::stream`, for
  one release.
- `lz4` (default): reading segments written with LZ4_RAW.
- `ingest`, `metriken`, `test-support`: see `Cargo.toml`.

Until 0.1.0 this crate was `metriken-segment`; it also took in the column
readers from `metriken-query`, `metriken-archive`, and rezolus's `.rez`
crate. See `docs/journal/2026-10-09-storage-scan.md`.
