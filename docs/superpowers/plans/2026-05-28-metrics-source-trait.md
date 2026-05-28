# MetricsSource Trait Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a public `MetricsSource` trait to `metriken-query` and expose `columns()`, `version()`, and `query()` on `ParquetReader`, enabling Rezolus to treat file-backed and future live-mode sources through one interface.

**Architecture:** Define `MetricsSource` at the crate root in `lib.rs`. Promote `column_map()` from test-only to always-available on `DataSource`, `Memory`, `MultiParquetSource`, and `ParquetSource`. Promote the `columns` module and `query()` method on `QueryEngine` from test-only to production. Add inherent methods `query()`, `columns()`, and `version()` to `ParquetReader`, then implement `MetricsSource` for it.

**Tech Stack:** Rust, metriken-query crate (Arrow/Parquet, promql-parser, thiserror)

---

## File Map

| File | Change |
|---|---|
| `metriken-query/src/lib.rs` | Add `MetricsSource` trait (pub) |
| `metriken-query/src/promql/mod.rs` | Un-gate `mod columns` and `query()` from `#[cfg(test)]` |
| `metriken-query/src/promql/columns.rs` | No change needed (already `pub` on `QueryEngine`) |
| `metriken-query/src/parquet.rs` | Un-gate `column_map()` on `MultiParquetSource`/`ParquetSource`; add inherent `query()`/`columns()`/`version()` to `ParquetReader`; add `impl MetricsSource for ParquetReader` |
| `metriken-query/src/memory.rs` | Un-gate `column_map()` on `Memory` |
| `metriken-query/src/lib.rs` | Un-gate `DataSource::column_map()` from `#[cfg(test)]` |
| `metriken-query/src/promql/tests.rs` | Add compile-time `MetricsSource` bound test |

---

## Task 1: Promote `DataSource::column_map()` and `Memory::column_map()` out of `#[cfg(test)]`

`columns()` on `QueryEngine` calls `self.source.column_map()`. Both the trait method and its implementations are currently `#[cfg(test)]`. They must be available in production builds before anything else.

**Files:**
- Modify: `metriken-query/src/lib.rs`
- Modify: `metriken-query/src/memory.rs`
- Modify: `metriken-query/src/parquet.rs` (two `column_map` impls: `MultiParquetSource` and `ParquetSource`)

- [ ] **Step 1: Remove `#[cfg(test)]` from `DataSource::column_map()` in `lib.rs`**

  In `metriken-query/src/lib.rs`, find:

  ```rust
      /// Parquet column name for every `(metric_name, labels)` pair.
      #[cfg(test)]
      fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>>;
  ```

  Replace with (remove the `#[cfg(test)]` attribute only):

  ```rust
      /// Parquet column name for every `(metric_name, labels)` pair.
      fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>>;
  ```

- [ ] **Step 2: Remove `#[cfg(test)]` from `Memory::column_map()` in `memory.rs`**

  In `metriken-query/src/memory.rs`, find:

  ```rust
      #[cfg(test)]
      fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
  ```

  Replace with:

  ```rust
      fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
  ```

- [ ] **Step 3: Remove `#[cfg(test)]` from both `column_map` impls in `parquet.rs`**

  In `metriken-query/src/parquet.rs`, find the `MultiParquetSource` impl block. Remove the `#[cfg(test)]` gate on `column_map`:

  ```rust
      #[cfg(test)]
      fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>> {
          let mut out = std::collections::HashMap::new();
          for (pf, _) in &self.files {
              for (metric, cols) in pf.column_map() {
                  out.entry(metric)
                      .or_insert_with(std::collections::HashMap::new)
                      .extend(cols);
              }
          }
          out
      }
  ```

  Replace with (remove `#[cfg(test)]` line only):

  ```rust
      fn column_map(&self) -> std::collections::HashMap<String, std::collections::HashMap<Labels, String>> {
          let mut out = std::collections::HashMap::new();
          for (pf, _) in &self.files {
              for (metric, cols) in pf.column_map() {
                  out.entry(metric)
                      .or_insert_with(std::collections::HashMap::new)
                      .extend(cols);
              }
          }
          out
      }
  ```

  Then in `parquet.rs`, find the `ParquetSource` impl block. Remove the `#[cfg(test)]` gate on `column_map`. Also remove `#[cfg(test)]` from the `column_name` field in `ColDesc` and from `ColDesc` construction since `column_map` on `ParquetSource` references it:

  The `ColDesc` struct has:
  ```rust
  struct ColDesc {
      col_idx: usize,
      name: String,
      labels: Labels,
      #[cfg(test)]
      column_name: String,
      kind: ColKind,
  }
  ```

  And in `parse_schema`, the construction is:
  ```rust
      Some(ColDesc { col_idx, name, labels, #[cfg(test)] column_name, kind })
  ```

  Change `ColDesc` to always include `column_name`:
  ```rust
  struct ColDesc {
      col_idx: usize,
      name: String,
      labels: Labels,
      column_name: String,
      kind: ColKind,
  }
  ```

  Change `parse_schema` construction to always include `column_name`:
  ```rust
      Some(ColDesc { col_idx, name, labels, column_name, kind })
  ```

  Change `ParquetSource::column_map` to remove `#[cfg(test)]`:
  ```rust
      fn column_map(&self) -> HashMap<String, HashMap<Labels, String>> {
          let ts_col_idx = self.meta.schema().index_of("timestamp").unwrap_or(usize::MAX);
          let mut out: HashMap<String, HashMap<Labels, String>> = HashMap::new();
          for c in parse_schema(self, ts_col_idx) {
              out.entry(c.name).or_default().insert(c.labels, c.column_name);
          }
          out
      }
  ```

- [ ] **Step 4: Verify it compiles**

  ```bash
  cargo build -p metriken-query 2>&1 | grep "^error"
  ```

  Expected: no output (no errors).

- [ ] **Step 5: Verify tests still pass**

  ```bash
  cargo test -p metriken-query 2>&1 | grep "test result"
  ```

  Expected: `test result: ok. 95 passed; 0 failed`.

- [ ] **Step 6: Commit**

  ```bash
  git add metriken-query/src/lib.rs metriken-query/src/memory.rs metriken-query/src/parquet.rs
  git commit -m "refactor(query): promote column_map from test-only to production on DataSource"
  ```

---

## Task 2: Promote `QueryEngine::query()` and `mod columns` out of `#[cfg(test)]`

`QueryEngine::query()` is the instant-query method; `mod columns` is the column-resolution module. Both are gated `#[cfg(test)]` and need to be available in production.

**Files:**
- Modify: `metriken-query/src/promql/mod.rs`

- [ ] **Step 1: Remove `#[cfg(test)]` from `mod columns` in `promql/mod.rs`**

  In `metriken-query/src/promql/mod.rs`, find:

  ```rust
  #[cfg(test)]
  mod columns;
  ```

  Replace with:

  ```rust
  pub(crate) mod columns;
  ```

- [ ] **Step 2: Remove `#[cfg(test)]` from `QueryEngine::query()` in `promql/mod.rs`**

  In `metriken-query/src/promql/mod.rs`, find:

  ```rust
      /// Execute an instant query at a single timestamp (test helper).
      #[cfg(test)]
      pub(crate) fn query(&self, query_str: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
  ```

  Replace with (remove `#[cfg(test)]` and update doc comment):

  ```rust
      /// Execute an instant query at a single timestamp.
      pub(crate) fn query(&self, query_str: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
  ```

- [ ] **Step 3: Verify it compiles**

  ```bash
  cargo build -p metriken-query 2>&1 | grep "^error"
  ```

  Expected: no output (no errors).

- [ ] **Step 4: Verify tests still pass**

  ```bash
  cargo test -p metriken-query 2>&1 | grep "test result"
  ```

  Expected: `test result: ok. 95 passed; 0 failed`.

- [ ] **Step 5: Commit**

  ```bash
  git add metriken-query/src/promql/mod.rs
  git commit -m "refactor(query): promote query() and mod columns out of cfg(test) on QueryEngine"
  ```

---

## Task 3: Add `MetricsSource` trait to `lib.rs`

Define the public trait at crate root. Each method maps to an existing `ParquetReader` inherent method (or one we'll add in Task 4).

**Files:**
- Modify: `metriken-query/src/lib.rs`

- [ ] **Step 1: Add `MetricsSource` trait to `lib.rs`**

  In `metriken-query/src/lib.rs`, add the following after the existing `use` declarations (after the `DataSource` trait block, near end of file):

  ```rust
  use std::collections::{BTreeMap, HashMap, HashSet};

  /// A queryable source of metric data. Implemented by `ParquetReader`
  /// (file-backed, streaming) and `MemoryStore` (in-memory, append-only).
  pub trait MetricsSource: Send + Sync {
      /// Execute a PromQL range query.
      fn query_range(
          &self,
          expr: &str,
          start: f64,
          end: f64,
          step: f64,
      ) -> Result<QueryResult, QueryError>;

      /// Execute a PromQL instant query.
      fn query(
          &self,
          expr: &str,
          time: Option<f64>,
      ) -> Result<QueryResult, QueryError>;

      /// Resolve which parquet columns are referenced by a PromQL query.
      /// Used for query-aware trimming (e.g. "save report" workflows).
      fn columns(&self, query: &str) -> Result<HashSet<String>, QueryError>;

      /// Time range of the data in seconds, or `None` if empty.
      fn time_range(&self) -> Option<(f64, f64)>;

      /// Sampling interval in seconds.
      fn interval(&self) -> f64;

      /// Source application that produced the data (e.g. `"rezolus"`).
      fn source(&self) -> String;

      /// Source application version.
      fn version(&self) -> String;

      /// All key-value metadata from the file footer or memory store.
      fn file_metadata(&self) -> HashMap<String, String>;

      fn counter_names(&self) -> Vec<String>;
      fn gauge_names(&self) -> Vec<String>;
      fn histogram_names(&self) -> Vec<String>;

      fn counter_labels(&self, name: &str) -> Vec<BTreeMap<String, String>>;
      fn gauge_labels(&self, name: &str) -> Vec<BTreeMap<String, String>>;
      fn histogram_labels(&self, name: &str) -> Vec<BTreeMap<String, String>>;
  }
  ```

  Note: `QueryResult` and `QueryError` are re-exported via `pub use promql::{..., QueryError, QueryResult, ...}` which is already in `lib.rs`. The trait body uses the short names since they're in scope.

  The `use std::collections::{BTreeMap, HashMap, HashSet};` line should be added at the top of `lib.rs` alongside or replacing existing `use` statements. Check first whether those types are already imported; if they are, skip the redundant imports.

  The current `lib.rs` has no top-level `use` for collections — the existing `DataSource` trait uses fully-qualified paths (`std::collections::HashMap`). For the new trait, use fully-qualified paths too to stay consistent:

  ```rust
  /// A queryable source of metric data. Implemented by `ParquetReader`
  /// (file-backed, streaming) and `MemoryStore` (in-memory, append-only).
  pub trait MetricsSource: Send + Sync {
      /// Execute a PromQL range query.
      fn query_range(
          &self,
          expr: &str,
          start: f64,
          end: f64,
          step: f64,
      ) -> Result<QueryResult, QueryError>;

      /// Execute a PromQL instant query.
      fn query(
          &self,
          expr: &str,
          time: Option<f64>,
      ) -> Result<QueryResult, QueryError>;

      /// Resolve which parquet columns are referenced by a PromQL query.
      /// Used for query-aware trimming (e.g. "save report" workflows).
      fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError>;

      /// Time range of the data in seconds, or `None` if empty.
      fn time_range(&self) -> Option<(f64, f64)>;

      /// Sampling interval in seconds.
      fn interval(&self) -> f64;

      /// Source application that produced the data (e.g. `"rezolus"`).
      fn source(&self) -> String;

      /// Source application version.
      fn version(&self) -> String;

      /// All key-value metadata from the file footer or memory store.
      fn file_metadata(&self) -> std::collections::HashMap<String, String>;

      fn counter_names(&self) -> Vec<String>;
      fn gauge_names(&self) -> Vec<String>;
      fn histogram_names(&self) -> Vec<String>;

      fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
      fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
      fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>>;
  }
  ```

  Also add `MetricsSource` to the public re-exports. In `lib.rs`, find:

  ```rust
  pub use parquet::{ParquetBuilder, ParquetReader};
  pub use promql::{HistogramHeatmapResult, QueryError, QueryResult, Sample};
  ```

  No change needed — `MetricsSource` is defined directly in `lib.rs` at crate root so it's already `pub`.

- [ ] **Step 2: Verify it compiles**

  ```bash
  cargo build -p metriken-query 2>&1 | grep "^error"
  ```

  Expected: no output (no errors).

- [ ] **Step 3: Commit**

  ```bash
  git add metriken-query/src/lib.rs
  git commit -m "feat(query): add public MetricsSource trait to crate root"
  ```

---

## Task 4: Add `query()`, `columns()`, `version()` inherent methods + `impl MetricsSource for ParquetReader`

Add three new inherent methods to `ParquetReader` and implement the full `MetricsSource` trait.

**Files:**
- Modify: `metriken-query/src/parquet.rs`

- [ ] **Step 1: Add inherent methods `query()`, `columns()`, and `version()` to `ParquetReader`**

  In `metriken-query/src/parquet.rs`, find the end of the `ParquetReader` impl block (just before `// ─── Builder ─...`). Add these three methods before the closing `}`:

  ```rust
      /// Execute a PromQL instant query.
      pub fn query(&self, expr: &str, time: Option<f64>) -> Result<QueryResult, QueryError> {
          self.engine.query(expr, time)
      }

      /// Resolve which parquet columns are referenced by a PromQL query.
      /// Used for query-aware trimming (e.g. "save report" workflows).
      pub fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, QueryError> {
          self.engine.columns(query)
      }

      /// Source application version from file metadata (e.g. file footer's "version" key).
      /// Returns an empty string if absent.
      pub fn version(&self) -> String {
          self.engine.file_metadata().remove("version").unwrap_or_default()
      }
  ```

- [ ] **Step 2: Add `impl MetricsSource for ParquetReader`**

  After the closing `}` of the `ParquetReader` impl block (and before the `// ─── Builder ─` comment), add:

  ```rust
  impl crate::MetricsSource for ParquetReader {
      fn query_range(&self, expr: &str, start: f64, end: f64, step: f64) -> Result<crate::QueryResult, crate::QueryError> {
          ParquetReader::query_range(self, expr, start, end, step)
      }

      fn query(&self, expr: &str, time: Option<f64>) -> Result<crate::QueryResult, crate::QueryError> {
          ParquetReader::query(self, expr, time)
      }

      fn columns(&self, query: &str) -> Result<std::collections::HashSet<String>, crate::QueryError> {
          ParquetReader::columns(self, query)
      }

      fn time_range(&self) -> Option<(f64, f64)> {
          ParquetReader::time_range(self)
      }

      fn interval(&self) -> f64 {
          ParquetReader::interval(self)
      }

      fn source(&self) -> String {
          ParquetReader::source(self)
      }

      fn version(&self) -> String {
          ParquetReader::version(self)
      }

      fn file_metadata(&self) -> std::collections::HashMap<String, String> {
          ParquetReader::file_metadata(self)
      }

      fn counter_names(&self) -> Vec<String> {
          ParquetReader::counter_names(self)
      }

      fn gauge_names(&self) -> Vec<String> {
          ParquetReader::gauge_names(self)
      }

      fn histogram_names(&self) -> Vec<String> {
          ParquetReader::histogram_names(self)
      }

      fn counter_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
          ParquetReader::counter_labels(self, name)
      }

      fn gauge_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
          ParquetReader::gauge_labels(self, name)
      }

      fn histogram_labels(&self, name: &str) -> Vec<std::collections::BTreeMap<String, String>> {
          ParquetReader::histogram_labels(self, name)
      }
  }
  ```

- [ ] **Step 3: Verify it compiles**

  ```bash
  cargo build -p metriken-query 2>&1 | grep "^error"
  ```

  Expected: no output (no errors).

- [ ] **Step 4: Verify tests still pass**

  ```bash
  cargo test -p metriken-query 2>&1 | grep "test result"
  ```

  Expected: `test result: ok. 95 passed; 0 failed`.

- [ ] **Step 5: Commit**

  ```bash
  git add metriken-query/src/parquet.rs
  git commit -m "feat(query): add query/columns/version to ParquetReader; impl MetricsSource"
  ```

---

## Task 5: Add compile-time `MetricsSource` trait test

Add a zero-runtime test that asserts `ParquetReader: MetricsSource` at compile time. This test lives in the existing test module.

**Files:**
- Modify: `metriken-query/src/promql/tests.rs`

- [ ] **Step 1: Add compile-time trait bound test**

  In `metriken-query/src/promql/tests.rs`, append at the end of the file:

  ```rust
  // ─── MetricsSource trait bound check ─────────────────────────────────────────

  #[test]
  fn test_parquet_reader_implements_metrics_source() {
      fn assert_metrics_source<T: crate::MetricsSource>(_: &T) {}
      // Compile-time check that ParquetReader: MetricsSource.
      // No fixture needed — the bound is verified by the compiler.
      fn _check(r: &crate::ParquetReader) {
          assert_metrics_source(r);
      }
  }
  ```

- [ ] **Step 2: Verify all tests pass (now 96)**

  ```bash
  cargo test -p metriken-query 2>&1 | grep "test result"
  ```

  Expected: `test result: ok. 96 passed; 0 failed`.

- [ ] **Step 3: Clippy clean**

  ```bash
  cargo clippy -p metriken-query 2>&1 | grep "^error\|^warning" | head -10
  ```

  Expected: no errors; any warnings should be pre-existing (not introduced by this change).

- [ ] **Step 4: Commit**

  ```bash
  git add metriken-query/src/promql/tests.rs
  git commit -m "test(query): add compile-time MetricsSource bound check for ParquetReader"
  ```

---

## Self-Review Checklist

- [x] **Spec coverage:** `MetricsSource` trait — covered Task 3. `columns()` on `ParquetReader` — covered Tasks 1+2+4. `version()` on `ParquetReader` — covered Task 4. `query()` on `ParquetReader` — covered Tasks 2+4. `impl MetricsSource for ParquetReader` — covered Task 4. Compile test — covered Task 5.

- [x] **Placeholder scan:** All steps contain exact Rust code. No "TBD" or "add appropriate handling."

- [x] **Type consistency:** `QueryResult`, `QueryError` — used by the trait and impl block, both from `crate::promql` re-exported in `lib.rs`. `HashSet<String>` — used in trait and inherent method, spelled `std::collections::HashSet<String>` consistently. `HashMap<String, String>` — used in `file_metadata()` consistently. `BTreeMap<String, String>` — used in `*_labels()` consistently.

- [x] **`column_map` chain:** Task 1 promotes `DataSource::column_map()`, `Memory::column_map()`, `MultiParquetSource::column_map()`, and `ParquetSource::column_map()` — required for Task 2 to compile (the `columns()` method on `QueryEngine` calls `self.source.column_map()`).

- [x] **`ColDesc::column_name`:** The `#[cfg(test)]` gate on the `column_name` field of `ColDesc` and its construction in `parse_schema` must be removed as part of Task 1 Step 3, because `ParquetSource::column_map` (now non-test) references `c.column_name`.

- [x] **`mod columns` visibility:** Changed to `pub(crate)` in Task 2 so the `columns()` method defined in that file is accessible to `parquet.rs` via `self.engine.columns(query)`.

- [x] **`Memory` module:** `memory.rs` is gated `#[cfg(test)]` at the module level in `lib.rs`. This is fine — `Memory` is used by the test harness only. `Memory::column_map()` is promoted from `#[cfg(test)]` to always-compiled within the `#[cfg(test)]` module boundary. Since `DataSource::column_map()` is no longer `#[cfg(test)]`, `Memory` (a `DataSource` implementor) must implement it unconditionally — which Task 1 achieves by removing the `#[cfg(test)]` guard on `Memory::column_map()`. The method body is present regardless of whether the test cfg is active; the module is only available in test builds, but the method is not conditionally compiled within it.
