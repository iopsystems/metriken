# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 10 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 9 measured, 2 failed cold-start

## Fixtures that failed cold-start

| fixture | phase | message |
|---|---|---|
| `/work/rezolus/site/viewer/data/disagg/disagg-sglang.parquet` | `duck_views` | Constraint Error: PRIMARY KEY or UNIQUE constraint violation: duplicate key "0::10" |
| `/work/rezolus/site/viewer/data/disagg/sglang-nixl-16c.parquet` | `duck_views` | Constraint Error: PRIMARY KEY or UNIQUE constraint violation: duplicate key "0::4" |

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 338.6 ms | 16.9 ms | 5.2 ms | 501.3 ms | 0.5 ms | 506.4 ms |
| `AB_base_pin.parquet` | 3.1 | 333.8 ms | 9.5 ms | 2.6 ms | 527.8 ms | 0.1 ms | 535.7 ms |
| `AB_level.parquet` | 3.3 | 352.4 ms | 9.0 ms | 2.8 ms | 524.4 ms | 0.1 ms | 531.6 ms |
| `AB_level_pin.parquet` | 2.8 | 337.5 ms | 9.6 ms | 3.0 ms | 519.9 ms | 0.1 ms | 531.2 ms |
| `cachecannon.parquet` | 3.8 | 362.8 ms | 8.2 ms | 3.0 ms | 1110.7 ms | 0.1 ms | 1111.0 ms |
| `demo.parquet` | 1.2 | 113.5 ms | 9.2 ms | 3.5 ms | 183.1 ms | 0.1 ms | 179.8 ms |
| `sglang_gemma3.parquet` | 2.1 | 286.5 ms | 11.4 ms | 3.6 ms | 1465.4 ms | 0.4 ms | 1395.8 ms |
| `vllm.parquet` | 4.0 | 311.0 ms | 13.2 ms | 2.8 ms | 3731.8 ms | 0.2 ms | 3711.1 ms |
| `vllm_gemma3.parquet` | 2.1 | 285.0 ms | 12.9 ms | 2.9 ms | 1377.2 ms | 0.3 ms | 1224.7 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 137 | 143 | 33 | 0 | 0 | 0.228 | 19.686 | 86.26× |
| `AB_base_pin.parquet` | 135 | 143 | 35 | 0 | 0 | 0.227 | 20.847 | 91.76× |
| `AB_level.parquet` | 135 | 143 | 35 | 0 | 0 | 0.233 | 20.016 | 85.78× |
| `AB_level_pin.parquet` | 138 | 141 | 34 | 0 | 0 | 0.240 | 20.484 | 85.33× |
| `cachecannon.parquet` | 116 | 162 | 17 | 18 | 0 | 0.577 | 70.646 | 122.53× |
| `demo.parquet` | 99 | 188 | 9 | 17 | 0 | 0.126 | 4.535 | 36.13× |
| `sglang_gemma3.parquet` | 175 | 105 | 33 | 0 | 0 | 0.623 | 64.285 | 103.25× |
| `vllm.parquet` | 178 | 103 | 32 | 0 | 0 | 1.299 | 205.935 | 158.56× |
| `vllm_gemma3.parquet` | 179 | 105 | 29 | 0 | 0 | 0.681 | 63.488 | 93.19× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.14 | 15.10 | 105.60× |
| `softirq_irate_total_by_kind` | 7 | 63 | 0.14 | 12.26 | 86.42× |
| `softirq_irate_by_id_by_kind` | 7 | 63 | 0.15 | 12.40 | 83.85× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.15 | 12.52 | 83.50× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.17 | 11.89 | 70.58× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.11 | 6.93 | 65.82× |
| `counter_irate_sum_with_labels` | 54 | 486 | 0.08 | 4.88 | 59.16× |
| `counter_irate_ratio_with_labels` | 2 | 18 | 0.56 | 25.98 | 46.30× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.23 | 10.52 | 45.57× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 0.22 | 9.66 | 43.18× |
| `counter_irate_by_id_scaled` | 2 | 18 | 0.41 | 17.47 | 42.79× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.16 | 6.90 | 42.71× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 0.44 | 18.66 | 42.14× |
| `counter_ratio_scaled` | 1 | 9 | 0.21 | 8.67 | 42.01× |
| `counter_ratio_generic` | 1 | 9 | 0.21 | 8.63 | 40.51× |
| `counter_irate_by_id_with_labels` | 2 | 18 | 0.17 | 6.80 | 39.41× |
| `counter_irate_by_id_generic` | 3 | 27 | 0.14 | 5.30 | 38.49× |
| `counter_ratio_by_id_generic` | 1 | 9 | 0.26 | 9.46 | 36.11× |
| `counter_total_sum_generic` | 9 | 81 | 0.12 | 4.34 | 35.30× |
| `gauge_subtract` | 1 | 9 | 0.06 | 1.69 | 26.70× |
| `memory_util_pct` | 1 | 9 | 0.07 | 1.77 | 25.29× |
| `counter_irate_total_scaled` | 1 | 9 | 0.08 | 1.80 | 23.36× |
| `counter_irate_total_mul` | 2 | 18 | 0.07 | 1.66 | 22.79× |
| `counter_rate_bare_generic` | 2 | 18 | 0.10 | 1.98 | 19.10× |
| `gauge_bare` | 5 | 45 | 0.05 | 0.77 | 15.45× |
| `gauge_bare_with_labels` | 1 | 9 | 0.06 | 0.75 | 12.78× |
| `gauge_sum_bare` | 1 | 9 | 0.06 | 0.75 | 12.09× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.15 | 16.64 | 113.66× |
| `softirq_irate_total_by_kind` | 6 | 54 | 0.13 | 11.65 | 87.25× |
| `softirq_irate_by_id_by_kind` | 6 | 54 | 0.15 | 12.22 | 80.21× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.15 | 11.68 | 77.19× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.16 | 11.23 | 68.60× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.11 | 6.81 | 64.67× |
| `counter_irate_sum_with_labels` | 54 | 486 | 0.09 | 5.08 | 57.78× |
| `counter_irate_ratio_with_labels` | 2 | 18 | 0.57 | 29.17 | 51.53× |
| `counter_ratio_scaled` | 1 | 9 | 0.18 | 8.83 | 47.86× |
| `counter_ratio_generic` | 1 | 9 | 0.19 | 9.17 | 47.21× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 0.22 | 10.27 | 46.94× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.22 | 10.17 | 45.87× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.15 | 6.92 | 44.68× |
| `counter_irate_by_id_scaled` | 2 | 18 | 0.39 | 17.46 | 44.56× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 0.41 | 17.96 | 43.57× |
| `counter_irate_by_id_with_labels` | 2 | 18 | 0.17 | 7.24 | 43.20× |
| `counter_total_sum_generic` | 9 | 81 | 0.11 | 4.21 | 39.10× |
| `counter_ratio_by_id_generic` | 1 | 9 | 0.25 | 9.64 | 39.09× |
| `counter_irate_by_id_generic` | 3 | 27 | 0.15 | 5.13 | 34.54× |
| `gauge_subtract` | 1 | 9 | 0.06 | 1.74 | 31.59× |
| `memory_util_pct` | 1 | 9 | 0.07 | 1.83 | 26.48× |
| `counter_irate_total_scaled` | 1 | 9 | 0.08 | 1.93 | 23.08× |
| `counter_irate_total_mul` | 2 | 18 | 0.08 | 1.65 | 20.85× |
| `counter_rate_bare_generic` | 2 | 18 | 0.10 | 1.88 | 19.27× |
| `gauge_bare` | 5 | 45 | 0.05 | 0.77 | 15.02× |
| `gauge_sum_bare` | 1 | 9 | 0.06 | 0.78 | 12.34× |
| `gauge_bare_with_labels` | 1 | 9 | 0.06 | 0.77 | 12.16× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.15 | 15.48 | 102.60× |
| `softirq_irate_total_by_kind` | 6 | 54 | 0.15 | 12.51 | 82.86× |
| `softirq_irate_by_id_by_kind` | 6 | 54 | 0.16 | 13.14 | 80.13× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.17 | 13.06 | 77.65× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.18 | 11.73 | 63.60× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.11 | 6.38 | 57.51× |
| `counter_irate_sum_with_labels` | 54 | 486 | 0.09 | 5.14 | 55.42× |
| `counter_irate_ratio_with_labels` | 2 | 18 | 0.53 | 28.05 | 52.55× |
| `counter_ratio_scaled` | 1 | 9 | 0.20 | 9.23 | 45.32× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 0.23 | 10.29 | 45.20× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.16 | 7.08 | 44.72× |
| `counter_ratio_generic` | 1 | 9 | 0.22 | 9.12 | 41.97× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 0.46 | 18.72 | 40.95× |
| `counter_irate_by_id_with_labels` | 2 | 18 | 0.18 | 7.31 | 40.89× |
| `counter_irate_by_id_scaled` | 2 | 18 | 0.45 | 18.15 | 40.67× |
| `counter_ratio_by_id_generic` | 1 | 9 | 0.25 | 10.17 | 40.18× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.25 | 9.79 | 38.93× |
| `counter_irate_by_id_generic` | 3 | 27 | 0.14 | 5.43 | 37.75× |
| `counter_total_sum_generic` | 9 | 81 | 0.13 | 4.53 | 36.22× |
| `gauge_subtract` | 1 | 9 | 0.06 | 1.83 | 31.66× |
| `memory_util_pct` | 1 | 9 | 0.07 | 1.94 | 27.98× |
| `counter_irate_total_scaled` | 1 | 9 | 0.09 | 1.99 | 22.62× |
| `counter_rate_bare_generic` | 2 | 18 | 0.10 | 2.05 | 20.47× |
| `counter_irate_total_mul` | 2 | 18 | 0.10 | 1.86 | 18.96× |
| `gauge_bare` | 5 | 45 | 0.05 | 0.77 | 15.46× |
| `gauge_bare_with_labels` | 1 | 9 | 0.06 | 0.81 | 13.31× |
| `gauge_sum_bare` | 1 | 9 | 0.07 | 0.83 | 12.30× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.16 | 15.45 | 93.73× |
| `softirq_irate_by_id_by_kind` | 7 | 63 | 0.15 | 12.87 | 87.00× |
| `softirq_irate_total_by_kind` | 7 | 63 | 0.15 | 12.40 | 85.05× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.15 | 12.55 | 81.85× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.17 | 12.18 | 73.68× |
| `counter_irate_sum_with_labels` | 55 | 495 | 0.10 | 5.14 | 51.05× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.14 | 7.14 | 50.65× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.13 | 6.55 | 50.08× |
| `counter_irate_ratio_with_labels` | 2 | 18 | 0.56 | 26.92 | 47.97× |
| `counter_ratio_scaled` | 1 | 9 | 0.21 | 9.65 | 45.26× |
| `counter_irate_by_id_with_labels` | 2 | 18 | 0.16 | 7.16 | 44.53× |
| `counter_ratio_generic` | 1 | 9 | 0.22 | 9.28 | 42.10× |
| `counter_ratio_by_id_generic` | 1 | 9 | 0.24 | 10.02 | 41.70× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 0.25 | 10.14 | 40.38× |
| `counter_irate_by_id_generic` | 3 | 27 | 0.13 | 5.32 | 40.06× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 0.47 | 18.83 | 39.85× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.25 | 9.77 | 39.80× |
| `counter_irate_by_id_scaled` | 2 | 18 | 0.45 | 17.70 | 39.75× |
| `counter_total_sum_generic` | 9 | 81 | 0.12 | 4.70 | 38.48× |
| `gauge_subtract` | 1 | 9 | 0.06 | 1.83 | 31.90× |
| `memory_util_pct` | 1 | 9 | 0.07 | 1.99 | 28.74× |
| `counter_irate_total_mul` | 2 | 18 | 0.07 | 1.79 | 24.48× |
| `counter_irate_total_scaled` | 1 | 9 | 0.09 | 2.05 | 23.22× |
| `counter_rate_bare_generic` | 2 | 18 | 0.11 | 2.08 | 19.56× |
| `gauge_bare` | 5 | 45 | 0.05 | 0.81 | 15.59× |
| `gauge_bare_with_labels` | 1 | 9 | 0.06 | 0.79 | 12.63× |
| `gauge_sum_bare` | 1 | 9 | 0.07 | 0.85 | 12.38× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.51 | 150.78 | 294.86× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.63 | 156.61 | 250.19× |
| `softirq_irate_total_by_kind` | 6 | 54 | 0.66 | 153.23 | 233.56× |
| `softirq_irate_by_id_by_kind` | 6 | 54 | 0.75 | 156.10 | 207.02× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 1.36 | 219.09 | 161.20× |
| `counter_irate_sum_with_labels` | 35 | 315 | 0.20 | 25.24 | 128.54× |
| `counter_irate_by_id_scaled` | 2 | 18 | 2.01 | 212.46 | 105.79× |
| `counter_total_sum_generic` | 9 | 81 | 0.34 | 34.65 | 100.74× |
| `counter_ratio_generic` | 1 | 9 | 1.09 | 101.92 | 93.92× |
| `counter_ratio_by_id_generic` | 1 | 9 | 1.25 | 91.08 | 73.14× |
| `counter_ratio_scaled` | 1 | 9 | 1.61 | 89.56 | 55.60× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 1.36 | 71.25 | 52.51× |
| `counter_irate_by_id_with_labels` | 2 | 18 | 1.46 | 71.88 | 49.30× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 1.85 | 90.06 | 48.74× |
| `counter_irate_total_scaled` | 1 | 9 | 0.14 | 6.29 | 45.98× |
| `memory_util_pct` | 1 | 9 | 0.14 | 6.14 | 43.07× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.15 | 5.74 | 38.53× |
| `gauge_subtract` | 1 | 9 | 0.16 | 6.15 | 37.83× |
| `counter_irate_ratio_with_labels` | 2 | 18 | 0.44 | 12.42 | 28.44× |
| `counter_irate_by_id_generic` | 3 | 27 | 1.62 | 43.56 | 26.89× |
| `counter_irate_total_mul` | 2 | 18 | 0.23 | 5.81 | 25.61× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.35 | 7.33 | 20.91× |
| `gauge_bare` | 5 | 45 | 0.15 | 2.92 | 19.76× |
| `gauge_sum_bare` | 1 | 9 | 0.18 | 3.26 | 18.44× |
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.53 | 9.09 | 17.11× |
| `counter_rate_bare_generic` | 2 | 18 | 0.28 | 4.64 | 16.44× |
| `gauge_bare_with_labels` | 1 | 9 | 0.17 | 1.65 | 9.90× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `softirq_irate_by_id_by_kind` | 6 | 54 | 0.17 | 9.31 | 54.42× |
| `softirq_irate_total_by_kind` | 6 | 54 | 0.16 | 8.38 | 53.42× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.17 | 9.11 | 53.12× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.18 | 8.76 | 49.16× |
| `counter_irate_sum_with_labels` | 25 | 225 | 0.07 | 3.36 | 46.33× |
| `counter_ratio_generic` | 1 | 9 | 0.17 | 6.56 | 39.33× |
| `counter_ratio_by_id_generic` | 1 | 9 | 0.22 | 7.23 | 32.49× |
| `counter_irate_by_id_generic` | 1 | 9 | 0.40 | 12.74 | 31.60× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.17 | 4.85 | 28.76× |
| `counter_irate_by_id_with_labels` | 2 | 18 | 0.21 | 5.07 | 24.53× |
| `counter_irate_by_id_scaled` | 2 | 18 | 0.57 | 13.49 | 23.57× |
| `counter_total_sum_generic` | 7 | 63 | 0.10 | 2.27 | 23.51× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 0.61 | 13.05 | 21.47× |
| `counter_irate_total_scaled` | 1 | 9 | 0.08 | 1.58 | 20.80× |
| `counter_irate_ratio_with_labels` | 2 | 18 | 0.14 | 2.77 | 20.22× |
| `gauge_subtract` | 1 | 9 | 0.05 | 1.00 | 18.45× |
| `memory_util_pct` | 1 | 9 | 0.07 | 1.20 | 18.30× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.11 | 1.80 | 16.93× |
| `counter_rate_bare_generic` | 2 | 18 | 0.11 | 1.58 | 14.98× |
| `counter_irate_total_mul` | 2 | 18 | 0.12 | 1.49 | 12.41× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.09 | 1.10 | 12.40× |
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.16 | 1.87 | 11.46× |
| `gauge_sum_bare` | 1 | 9 | 0.06 | 0.64 | 11.12× |
| `gauge_bare` | 5 | 45 | 0.05 | 0.40 | 8.38× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.26 | 53.51 | 204.78× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.17 | 24.73 | 142.44× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.41 | 55.29 | 136.26× |
| `counter_irate_ratio_with_labels` | 3 | 27 | 0.42 | 47.97 | 115.11× |
| `counter_irate_by_id_with_labels` | 6 | 54 | 0.65 | 70.43 | 107.81× |
| `rezolus_cpu_ipns` | 1 | 9 | 1.32 | 112.30 | 85.04× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 0.93 | 79.21 | 84.78× |
| `softirq_irate_by_id_by_kind` | 4 | 36 | 0.73 | 60.99 | 83.67× |
| `softirq_irate_total_by_kind` | 4 | 36 | 0.74 | 60.43 | 82.08× |
| `counter_irate_by_id_scaled` | 2 | 18 | 0.91 | 74.61 | 81.57× |
| `rezolus_cpu_aperf_chain_total` | 1 | 9 | 0.86 | 68.12 | 79.51× |
| `rezolus_cpu_ipns_per_id` | 1 | 9 | 1.44 | 114.77 | 79.49× |
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.62 | 49.03 | 79.31× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 0.43 | 31.65 | 72.91× |
| `counter_ratio_scaled` | 1 | 9 | 0.43 | 30.98 | 72.08× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 9 | 0.96 | 67.51 | 70.45× |
| `counter_total_sum_generic` | 11 | 99 | 0.28 | 17.70 | 63.72× |
| `gauge_a_over_a_plus_b` | 1 | 9 | 0.12 | 7.48 | 59.84× |
| `counter_irate_sum_with_labels` | 52 | 468 | 0.23 | 13.27 | 58.22× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.69 | 35.79 | 51.82× |
| `counter_ratio_by_id_generic` | 2 | 18 | 0.92 | 43.70 | 47.68× |
| `counter_ratio_generic` | 2 | 18 | 0.95 | 42.97 | 45.23× |
| `gauge_sum_by_g_with_labels` | 6 | 54 | 0.11 | 4.15 | 37.60× |
| `gauge_ratio_with_labels_ignoring` | 2 | 18 | 0.13 | 4.55 | 35.86× |
| `gauge_subtract` | 1 | 9 | 0.09 | 3.24 | 34.17× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.93 | 30.00 | 32.38× |
| `memory_util_pct` | 1 | 9 | 0.11 | 3.30 | 29.33× |
| `counter_irate_total_scaled` | 1 | 9 | 0.14 | 3.62 | 26.63× |
| `gauge_sum_with_labels` | 4 | 36 | 0.10 | 2.46 | 23.67× |
| `counter_irate_by_id_generic` | 5 | 45 | 0.94 | 21.89 | 23.39× |
| `counter_rate_bare_generic` | 2 | 18 | 0.14 | 2.85 | 20.68× |
| `gauge_sum_by_g_bare` | 1 | 9 | 0.09 | 1.56 | 16.83× |
| `gauge_bare` | 5 | 45 | 0.09 | 1.47 | 16.22× |
| `counter_rate_sum_scaled` | 1 | 9 | 0.16 | 2.63 | 16.15× |
| `gauge_sum_by_g_scaled` | 7 | 63 | 0.10 | 1.60 | 15.79× |
| `gauge_sum_scaled` | 1 | 9 | 0.10 | 1.53 | 15.42× |
| `gauge_avg_scaled` | 6 | 54 | 0.10 | 1.51 | 14.87× |
| `gauge_bare_with_labels` | 1 | 9 | 0.12 | 1.47 | 12.62× |
| `gauge_max_bare` | 1 | 9 | 0.16 | 1.54 | 9.76× |
| `gauge_sum_bare` | 2 | 18 | 0.20 | 1.55 | 7.60× |
| `counter_irate_total_mul` | 2 | 18 | 0.72 | 3.36 | 4.64× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.22 | 115.66 | 518.47× |
| `counter_irate_ratio_with_labels` | 3 | 27 | 0.76 | 146.18 | 192.23× |
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.71 | 134.04 | 188.52× |
| `counter_ratio_by_g_with_labels` | 1 | 9 | 1.20 | 148.67 | 123.64× |
| `softirq_irate_by_id_by_kind` | 7 | 63 | 1.13 | 136.72 | 120.95× |
| `softirq_irate_total_by_kind` | 7 | 63 | 1.19 | 136.41 | 114.86× |
| `counter_irate_by_id_generic` | 4 | 36 | 1.61 | 168.92 | 104.83× |
| `counter_irate_by_id_with_labels` | 6 | 54 | 1.55 | 150.05 | 96.69× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 1.42 | 136.62 | 96.51× |
| `counter_irate_sum_with_labels` | 52 | 468 | 0.67 | 62.64 | 92.96× |
| `counter_irate_by_id_scaled` | 2 | 18 | 1.90 | 172.52 | 91.03× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 2.02 | 179.69 | 88.89× |
| `gauge_a_over_a_plus_b` | 1 | 9 | 0.17 | 14.14 | 83.34× |
| `rezolus_cpu_aperf_chain_total` | 1 | 9 | 1.59 | 125.53 | 78.93× |
| `rezolus_cpu_ipns` | 1 | 9 | 2.76 | 217.31 | 78.74× |
| `rezolus_cpu_ipns_per_id` | 1 | 9 | 2.80 | 210.53 | 75.12× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 9 | 1.75 | 124.93 | 71.36× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 2.20 | 136.63 | 62.18× |
| `counter_irate_total_scaled` | 1 | 9 | 0.10 | 6.41 | 61.68× |
| `counter_ratio_generic` | 2 | 18 | 1.43 | 80.20 | 56.10× |
| `gauge_sum_by_g_with_labels` | 6 | 54 | 0.16 | 8.26 | 51.92× |
| `counter_total_sum_generic` | 10 | 90 | 0.72 | 37.14 | 51.28× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 1.43 | 64.19 | 44.84× |
| `memory_util_pct` | 1 | 9 | 0.17 | 6.06 | 35.61× |
| `gauge_ratio_with_labels_ignoring` | 2 | 18 | 0.24 | 8.27 | 34.85× |
| `counter_ratio_by_id_generic` | 2 | 18 | 2.39 | 79.38 | 33.17× |
| `gauge_subtract` | 1 | 9 | 0.18 | 5.81 | 33.09× |
| `gauge_sum_with_labels` | 4 | 36 | 0.15 | 4.66 | 30.77× |
| `gauge_sum_by_g_bare` | 1 | 9 | 0.15 | 2.92 | 19.85× |
| `gauge_sum_scaled` | 1 | 9 | 0.14 | 2.66 | 19.29× |
| `gauge_sum_by_g_scaled` | 7 | 63 | 0.16 | 2.98 | 19.00× |
| `gauge_avg_scaled` | 6 | 54 | 0.15 | 2.78 | 18.72× |
| `gauge_bare` | 5 | 45 | 0.15 | 2.58 | 17.67× |
| `gauge_bare_with_labels` | 2 | 18 | 0.17 | 3.04 | 17.64× |
| `counter_rate_bare_generic` | 2 | 18 | 0.29 | 4.05 | 13.93× |
| `gauge_max_bare` | 1 | 9 | 0.27 | 2.93 | 10.67× |
| `counter_rate_sum_scaled` | 1 | 9 | 0.51 | 4.21 | 8.23× |
| `gauge_sum_bare` | 2 | 18 | 0.39 | 2.92 | 7.53× |
| `counter_irate_total_mul` | 2 | 18 | 1.15 | 5.81 | 5.06× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `softirq_time_pct_by_id_by_kind` | 10 | 90 | 0.29 | 57.60 | 197.74× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 36 | 0.17 | 20.78 | 120.98× |
| `counter_irate_ratio_with_labels` | 3 | 27 | 0.43 | 45.85 | 105.82× |
| `counter_irate_by_id_with_labels` | 6 | 54 | 0.71 | 69.99 | 98.27× |
| `softirq_irate_by_id_by_kind` | 6 | 54 | 0.71 | 62.38 | 88.16× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 108 | 0.67 | 57.86 | 85.96× |
| `softirq_irate_total_by_kind` | 6 | 54 | 0.74 | 61.25 | 83.04× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 18 | 1.01 | 82.25 | 81.67× |
| `counter_irate_by_id_scaled` | 2 | 18 | 1.00 | 80.67 | 80.58× |
| `rezolus_cpu_ipns_per_id` | 1 | 9 | 1.35 | 108.40 | 80.58× |
| `rezolus_cpu_ipns` | 1 | 9 | 1.38 | 109.61 | 79.71× |
| `rezolus_cpu_user_per_id` | 1 | 9 | 0.48 | 34.86 | 72.97× |
| `rezolus_cpu_aperf_chain_total` | 1 | 9 | 0.90 | 64.12 | 71.12× |
| `counter_ratio_by_id_scaled` | 1 | 9 | 0.44 | 30.65 | 69.62× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 9 | 1.03 | 66.02 | 64.01× |
| `counter_ratio_by_g_with_labels` | 1 | 9 | 0.70 | 44.65 | 63.89× |
| `gauge_a_over_a_plus_b` | 1 | 9 | 0.12 | 7.65 | 62.74× |
| `counter_ratio_scaled` | 1 | 9 | 0.50 | 30.20 | 60.69× |
| `counter_total_sum_generic` | 11 | 99 | 0.30 | 16.44 | 54.76× |
| `counter_irate_sum_with_labels` | 52 | 468 | 0.25 | 12.71 | 50.61× |
| `counter_ratio_generic` | 2 | 18 | 1.00 | 41.57 | 41.58× |
| `counter_ratio_by_id_generic` | 2 | 18 | 1.09 | 43.25 | 39.83× |
| `gauge_sum_by_g_with_labels` | 6 | 54 | 0.11 | 4.05 | 36.25× |
| `gauge_subtract` | 1 | 9 | 0.10 | 3.25 | 33.62× |
| `counter_irate_by_id_generic` | 5 | 45 | 0.68 | 21.34 | 31.31× |
| `counter_irate_with_labels_scaled` | 3 | 27 | 0.83 | 26.00 | 31.22× |
| `gauge_ratio_with_labels_ignoring` | 2 | 18 | 0.14 | 4.40 | 30.71× |
| `memory_util_pct` | 1 | 9 | 0.12 | 3.40 | 28.78× |
| `counter_irate_total_scaled` | 1 | 9 | 0.13 | 3.60 | 27.57× |
| `gauge_sum_with_labels` | 4 | 36 | 0.10 | 2.43 | 25.26× |
| `counter_rate_bare_generic` | 2 | 18 | 0.16 | 2.78 | 17.90× |
| `gauge_sum_by_g_bare` | 1 | 9 | 0.10 | 1.70 | 17.44× |
| `gauge_bare` | 5 | 45 | 0.09 | 1.41 | 15.91× |
| `gauge_avg_scaled` | 6 | 54 | 0.10 | 1.58 | 15.47× |
| `gauge_sum_scaled` | 1 | 9 | 0.10 | 1.53 | 15.04× |
| `gauge_sum_by_g_scaled` | 7 | 63 | 0.11 | 1.63 | 14.59× |
| `counter_rate_sum_scaled` | 1 | 9 | 0.19 | 2.74 | 14.38× |
| `gauge_bare_with_labels` | 1 | 9 | 0.11 | 1.45 | 13.10× |
| `gauge_max_bare` | 1 | 9 | 0.14 | 1.47 | 10.17× |
| `gauge_sum_bare` | 2 | 18 | 0.20 | 1.58 | 7.78× |
| `counter_irate_total_mul` | 2 | 18 | 0.67 | 3.32 | 4.93× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 273.99× | 0.25 | 67.44 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="sleep",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 271.11× | 0.25 | 68.62 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="yield",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 268.15× | 0.25 | 66.84 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 247.91× | 0.27 | 68.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="event",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 238.10× | 0.29 | 69.14 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="timer",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 236.16× | 0.30 | 69.68 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 217.66× | 0.33 | 70.83 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 207.58× | 0.34 | 70.78 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 204.53× | 0.34 | 68.60 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name="/system.slice/rezolus.service"}[5m]))` |
| 196.74× | 0.36 | 71.29 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="memory",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 12.09× | 0.06 | 0.75 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 12.78× | 0.06 | 0.75 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 12.85× | 0.12 | 1.53 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 13.91× | 0.06 | 0.80 | `gauge_bare` | `memory_available` |
| 14.90× | 0.08 | 1.26 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 15.26× | 0.05 | 0.76 | `gauge_bare` | `memory_free` |
| 15.37× | 0.05 | 0.80 | `gauge_bare` | `memory_cached` |
| 15.45× | 0.05 | 0.77 | `gauge_bare` | `memory_buffers` |
| 15.82× | 0.09 | 1.39 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 16.12× | 0.05 | 0.76 | `gauge_bare` | `memory_total` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 315.94× | 0.24 | 75.69 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 298.16× | 0.26 | 76.59 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="yield",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 286.55× | 0.26 | 75.64 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="sleep",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 267.31× | 0.29 | 77.28 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="event",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 267.04× | 0.29 | 77.40 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="timer",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 241.01× | 0.32 | 76.41 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 223.37× | 0.35 | 78.76 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 222.67× | 0.34 | 76.65 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name="/system.slice/rezolus.service"}[5m]))` |
| 210.96× | 0.38 | 79.15 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 210.21× | 0.38 | 80.55 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="memory",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 12.16× | 0.06 | 0.77 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 12.34× | 0.06 | 0.78 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 13.01× | 0.12 | 1.58 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 14.38× | 0.05 | 0.75 | `gauge_bare` | `memory_cached` |
| 14.99× | 0.05 | 0.77 | `gauge_bare` | `memory_buffers` |
| 15.01× | 0.05 | 0.78 | `gauge_bare` | `memory_available` |
| 15.12× | 0.09 | 1.38 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 15.49× | 0.05 | 0.77 | `gauge_bare` | `memory_free` |
| 15.91× | 0.08 | 1.33 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 16.11× | 0.05 | 0.77 | `gauge_bare` | `memory_total` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 273.68× | 0.26 | 69.96 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="sleep",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 269.83× | 0.26 | 70.73 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="yield",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 263.22× | 0.26 | 69.64 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="event",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 261.95× | 0.26 | 68.55 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 245.98× | 0.29 | 70.76 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="timer",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 226.77× | 0.32 | 71.48 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 209.12× | 0.34 | 71.71 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 202.54× | 0.36 | 73.02 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 200.20× | 0.36 | 72.36 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="memory",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 200.16× | 0.37 | 73.86 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="process",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 12.30× | 0.07 | 0.83 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 13.31× | 0.06 | 0.81 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 13.85× | 0.12 | 1.71 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 14.03× | 0.05 | 0.75 | `gauge_bare` | `memory_cached` |
| 14.86× | 0.05 | 0.78 | `gauge_bare` | `memory_available` |
| 15.46× | 0.05 | 0.77 | `gauge_bare` | `memory_free` |
| 15.49× | 0.10 | 1.53 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 15.53× | 0.05 | 0.76 | `gauge_bare` | `memory_buffers` |
| 16.21× | 0.05 | 0.78 | `gauge_bare` | `memory_total` |
| 17.54× | 0.08 | 1.46 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 275.83× | 0.25 | 70.20 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 260.83× | 0.27 | 71.64 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="event",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 253.65× | 0.28 | 71.74 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="sleep",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 247.63× | 0.29 | 71.18 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="yield",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 236.87× | 0.30 | 71.34 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="timer",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 219.16× | 0.33 | 71.42 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 205.05× | 0.36 | 73.17 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 199.18× | 0.38 | 74.87 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 197.99× | 0.37 | 73.58 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="memory",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 187.94× | 0.40 | 74.48 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="query",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 11.60× | 0.13 | 1.55 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 12.38× | 0.07 | 0.85 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 12.63× | 0.06 | 0.79 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 13.18× | 0.13 | 1.73 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 14.25× | 0.11 | 1.50 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 14.30× | 0.06 | 0.81 | `gauge_bare` | `memory_cached` |
| 15.15× | 0.05 | 0.77 | `gauge_bare` | `memory_total` |
| 15.22× | 0.05 | 0.84 | `gauge_bare` | `memory_available` |
| 15.40× | 0.05 | 0.80 | `gauge_bare` | `memory_free` |
| 16.14× | 0.05 | 0.84 | `gauge_bare` | `memory_buffers` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 782.17× | 0.19 | 146.20 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hi"}[5m])) / 1000000000` |
| 782.12× | 0.19 | 145.64 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 765.11× | 0.19 | 145.69 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="irq_poll"}[5m])) / 1000000000` |
| 666.63× | 0.22 | 148.96 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="irq_poll"}[5m])) / cpu_cores / 1000000000` |
| 652.61× | 0.23 | 148.52 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 636.20× | 0.23 | 148.26 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hi"}[5m])) / cpu_cores / 1000000000` |
| 611.74× | 0.27 | 162.14 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 346.29× | 0.46 | 158.95 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 337.28× | 0.49 | 164.28 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 316.74× | 0.52 | 164.10 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 2.09× | 1.35 | 2.83 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 4.35× | 0.62 | 2.71 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 9.90× | 0.17 | 1.65 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 12.34× | 0.25 | 3.10 | `gauge_bare` | `memory_available` |
| 13.38× | 0.18 | 2.41 | `counter_irate_sum_with_labels` | `sum(irate(request_errors{source="cachecannon"}[5s]))` |
| 14.57× | 0.18 | 2.65 | `counter_irate_sum_with_labels` | `sum(irate(bytes_rx{source="cachecannon"}[5s]))` |
| 14.61× | 0.18 | 2.58 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 16.15× | 0.28 | 4.56 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 16.58× | 0.15 | 2.48 | `counter_irate_sum_with_labels` | `sum(irate(cache_misses{source="cachecannon"}[5s]))` |
| 17.11× | 0.53 | 9.09 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 112.94× | 0.06 | 6.78 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="irq_poll"}[5m])) / 1000000000` |
| 103.91× | 0.07 | 6.94 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hi"}[5m])) / 1000000000` |
| 99.11× | 0.07 | 6.80 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 96.72× | 0.08 | 7.50 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 94.19× | 0.08 | 7.63 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="irq_poll"}[5m])) / cpu_cores / 1000000000` |
| 90.83× | 0.08 | 7.50 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hi"}[5m])) / cpu_cores / 1000000000` |
| 67.55× | 0.14 | 9.43 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 62.05× | 0.15 | 9.23 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 59.11× | 0.14 | 8.29 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="tasklet"}[5m]))` |
| 56.28× | 0.17 | 9.46 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 7.73× | 0.06 | 0.44 | `gauge_bare` | `memory_available` |
| 8.23× | 0.05 | 0.40 | `gauge_bare` | `memory_free` |
| 8.32× | 0.05 | 0.39 | `gauge_bare` | `memory_total` |
| 8.38× | 0.05 | 0.40 | `gauge_bare` | `memory_cached` |
| 8.84× | 0.05 | 0.42 | `gauge_bare` | `memory_buffers` |
| 11.12× | 0.06 | 0.64 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 11.46× | 0.16 | 1.87 | `counter_ratio_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_instructions{name=~"__SELECTED_CGROUPS__"}[5m])) / sum by (name) (ir…` |
| 12.40× | 0.09 | 1.10 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{name=~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 12.41× | 0.12 | 1.49 | `counter_irate_total_mul` | `sum(irate(network_bytes{direction="receive"}[5m])) * 8` |
| 14.06× | 0.07 | 1.03 | `counter_irate_by_g_with_labels_scaled` | `sum by (name) (irate(cgroup_cpu_usage{state="system",name=~"__SELECTED_CGROUPS__"}[5m])) / 100000000…` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 444.74× | 0.35 | 154.86 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 405.09× | 0.39 | 158.61 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="other",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 401.88× | 0.39 | 157.23 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 396.26× | 0.41 | 161.08 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="write",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 387.52× | 0.39 | 152.10 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 383.23× | 0.14 | 52.77 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hi"}[5m])) / 1000000000` |
| 375.99× | 0.42 | 158.18 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="query",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 372.13× | 0.42 | 154.59 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="memory",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 367.59× | 0.14 | 52.21 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 363.72× | 0.15 | 52.82 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="irq_poll"}[5m])) / 1000000000` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 4.51× | 0.72 | 3.27 | `counter_irate_total_mul` | `sum(irate(network_bytes{direction="receive"}[5m])) * 8` |
| 7.60× | 0.20 | 1.55 | `gauge_sum_bare` | `sum(gpu_pcie_bandwidth)` |
| 9.76× | 0.16 | 1.54 | `gauge_max_bare` | `max(gpu_temperature)` |
| 10.82× | 0.95 | 10.28 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 11.45× | 0.14 | 1.55 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 12.62× | 0.12 | 1.47 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 13.90× | 0.70 | 9.79 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.93× | 0.12 | 1.70 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_power_usage) / 1000` |
| 13.94× | 0.12 | 1.60 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_memory_utilization) / 100` |
| 14.24× | 0.10 | 1.45 | `gauge_avg_scaled` | `avg(gpu_tensor_utilization) / 100` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 1385.33× | 0.46 | 639.68 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="timer",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 807.12× | 0.79 | 635.84 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="event",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 802.32× | 0.16 | 126.23 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 765.85× | 0.17 | 127.74 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="irq_poll"}[5m])) / 1000000000` |
| 742.49× | 0.86 | 638.23 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="time",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 732.69× | 0.88 | 648.16 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 676.27× | 0.95 | 645.69 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 670.31× | 0.96 | 646.38 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="filesystem",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 657.34× | 0.99 | 647.67 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="write",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 601.44× | 1.05 | 633.16 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 5.06× | 1.15 | 5.81 | `counter_irate_total_mul` | `sum(irate(network_bytes{direction="receive"}[5m])) * 8` |
| 6.95× | 0.39 | 2.70 | `gauge_sum_bare` | `sum(gpu_pcie_bandwidth)` |
| 8.23× | 0.51 | 4.21 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 10.67× | 0.27 | 2.93 | `gauge_max_bare` | `max(gpu_temperature)` |
| 12.12× | 0.24 | 2.93 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 13.93× | 0.29 | 4.05 | `counter_rate_bare_generic` | `rate(memory_numa_foreign[5m])` |
| 15.63× | 0.19 | 2.92 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 16.20× | 0.16 | 2.57 | `gauge_bare` | `memory_total` |
| 16.28× | 0.16 | 2.67 | `gauge_bare` | `memory_buffers` |
| 16.40× | 0.18 | 2.99 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 378.79× | 0.34 | 129.28 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="lock",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 369.48× | 0.15 | 55.19 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hrtimer"}[5m])) / 1000000000` |
| 366.19× | 0.15 | 53.94 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="irq_poll"}[5m])) / 1000000000` |
| 358.52× | 0.37 | 133.31 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="other",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 341.20× | 0.38 | 131.12 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="query",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 331.63× | 0.40 | 133.20 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="socket",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 320.39× | 0.42 | 133.35 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="filesystem",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 312.66× | 0.43 | 133.44 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="memory",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 307.52× | 0.43 | 130.72 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="timer",name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 306.83× | 0.42 | 127.63 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{op="ipc",name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 4.93× | 0.67 | 3.32 | `counter_irate_total_mul` | `sum(irate(network_bytes{direction="receive"}[5m])) * 8` |
| 7.67× | 0.20 | 1.55 | `gauge_sum_bare` | `sum(gpu_pcie_bandwidth)` |
| 10.17× | 0.14 | 1.47 | `gauge_max_bare` | `max(gpu_temperature)` |
| 11.36× | 0.14 | 1.58 | `gauge_avg_scaled` | `avg(gpu_dram_bandwidth_utilization) / 100` |
| 12.18× | 0.13 | 1.58 | `gauge_sum_bare` | `sum(rezolus_memory_usage_resident_set_size)` |
| 12.52× | 0.80 | 10.01 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.10× | 0.11 | 1.45 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 14.24× | 0.12 | 1.66 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_dram_bandwidth_utilization) / 100` |
| 14.24× | 0.11 | 1.63 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 14.38× | 0.19 | 2.74 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |

