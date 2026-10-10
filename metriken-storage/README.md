# metriken-storage

Storage for metriken recordings: the parquet segment format, the long layout
(one row per timestamp and occupant, for groups whose members come and go)
and the occupant stream that names each occupant. `metriken-query` reads
these segments; archive writers produce them.

Until 0.1.0 this crate was `metriken-segment`. The plan for what else it
takes in is `docs/journal/2026-10-09-storage-scan.md`.
