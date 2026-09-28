# metriken-segment

The parquet segment format for metriken metrics: the long layout (one row
per timestamp and occupant, for groups whose members come and go) and the
occupant stream that names each occupant. `metriken-query` reads these
segments; writers such as rezolus's archive writer produce them.

See `docs/journal/2026-09-28-high-cardinality-stack.md` and
`docs/journal/2026-09-28-long-segments.md`.
