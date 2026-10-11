# metriken-storage

Storage for metriken recordings: the parquet segment format (wide and long
tables, occupant streams), the column readers the query engine reads
through, and the archive containers: dendro, and `.rez` behind the `rez`
feature, read and written. `metriken-query` runs queries over these.

Until 0.1.0 this crate was `metriken-segment`; it also took in the column
readers from `metriken-query`, `metriken-archive`, and rezolus's `.rez`
crate. See `docs/journal/2026-10-09-storage-scan.md`.
