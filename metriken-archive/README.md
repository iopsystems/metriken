# metriken-archive

Archives of metriken metrics (a catalog of sources and tables, sealed parquet
segments, a write-ahead log and caller rows, as dendro stores them) read as
one PromQL `MetricsSource` per source. `ArchiveReader` opens tables lazily,
unions the tables of one sampler, and evaluates queries across tables of
different cadence on the slow table's own rows.

`DendroCatalog` reads a dendro archive; another container implements the
`Catalog` trait. See `docs/journal/2026-09-28-high-cardinality-stack.md`.
