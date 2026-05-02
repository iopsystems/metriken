# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 4 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 1.2 | 121.2 ms | 18.5 ms | 7.3 ms | 149.3 ms | 0.7 ms | 158.9 ms |
| `cachecannon.parquet` | 3.8 | 356.5 ms | 6.1 ms | 2.5 ms | 1044.9 ms | 0.1 ms | 1060.9 ms |
| `AB_base.parquet` | 3.6 | 344.0 ms | 10.9 ms | 3.0 ms | 501.1 ms | 0.1 ms | 504.4 ms |
| `vllm.parquet` | 4.0 | 298.5 ms | 10.6 ms | 2.9 ms | 3459.3 ms | 0.2 ms | 3446.5 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.081 | 4.919 | 61.08× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.261 | 69.870 | 267.41× |
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.135 | 33.656 | 248.68× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.533 | 341.851 | 641.46× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 20.91 | 135.93× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 7.60 | 110.83× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 8.58 | 67.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 8.22 | 65.27× |
| `counter_ratio_generic` | 2 | 8 | 0.14 | 8.83 | 64.54× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 8.65 | 63.52× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.39 | 24.48 | 63.09× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.40 | 25.02 | 62.55× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.16 | 9.90 | 60.16× |
| `gauge_subtract` | 1 | 4 | 0.05 | 2.35 | 49.90× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 2.57 | 44.58× |
| `memory_util_pct` | 1 | 4 | 0.05 | 2.13 | 41.78× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 2.39 | 41.16× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 3.07 | 40.80× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 1.63 | 40.12× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 2.60 | 37.99× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.07 | 2.47 | 36.97× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.09 | 3.11 | 36.04× |
| `gauge_bare` | 10 | 40 | 0.04 | 1.32 | 35.14× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 2.33 | 34.99× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 4.73 | 31.74× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 3.37 | 27.95× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 2.53 | 25.92× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.37 | 14.03× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.46 | 12.57× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.03 | 0.36 | 10.38× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.12 | 1.18 | 9.43× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.15 | 1.29 | 8.34× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.36 | 6.41× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.43 | 6.29× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.11 | 3.54× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.09 | 3.52× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.02 | 0.08 | 3.40× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.02 | 0.08 | 3.37× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.09 | 3.36× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.09 | 3.20× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.09 | 3.14× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.07 | 2.89× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.09 | 2.89× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.09 | 2.83× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.09 | 2.75× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.09 | 2.68× |
| `counter_ratio_complement` | 1 | 4 | 0.03 | 0.08 | 2.42× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.09 | 2.33× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.08 | 2.33× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.08 | 2.19× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.11 | 2.17× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.04 | 0.07 | 1.62× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.42 | 392.95 | 930.16× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 64.99 | 557.30× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.85 | 442.89 | 518.15× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.29 | 93.64 | 325.56× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.49 | 140.36 | 283.88× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.51 | 141.36 | 275.20× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.54 | 144.19 | 268.03× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.69 | 436.31 | 258.91× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.34 | 81.14 | 236.07× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 7.32 | 157.32× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.01 | 138.09 | 136.52× |
| `counter_ratio_generic` | 2 | 8 | 1.00 | 136.05 | 136.36× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.16 | 19.85 | 124.04× |
| `memory_util_pct` | 1 | 4 | 0.07 | 8.02 | 121.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 15.47 | 120.35× |
| `counter_irate_total_scaled` | 1 | 4 | 0.11 | 12.48 | 115.29× |
| `counter_total_sum_generic` | 11 | 44 | 0.21 | 22.12 | 105.96× |
| `counter_ratio_scaled` | 1 | 4 | 1.42 | 137.07 | 96.76× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 1.55 | 137.88 | 88.67× |
| `gauge_bare` | 10 | 40 | 0.09 | 7.05 | 76.96× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 16.93 | 76.43× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.20 | 15.14 | 75.23× |
| `gauge_subtract` | 1 | 4 | 0.15 | 10.80 | 73.19× |
| `counter_irate_total_mul` | 2 | 8 | 0.18 | 12.64 | 69.66× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.11 | 68.27 | 61.34× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.42 | 16.17 | 38.23× |
| `counter_rate_bare_generic` | 2 | 8 | 0.25 | 8.32 | 33.84× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 1.01 | 10.94× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.10 | 1.14 | 10.88× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.10 | 1.04 | 10.58× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.12 | 1.06 | 8.85× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.17 | 1.03 | 6.20× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.54 | 3.07 | 5.74× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.60 | 2.89 | 4.80× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.10 | 3.75× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.10 | 3.69× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.12 | 3.63× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.13 | 3.60× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.10 | 3.37× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.10 | 3.17× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.09 | 3.15× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.11 | 3.10× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.11 | 2.74× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.11 | 2.73× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.06 | 0.15 | 2.55× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.11 | 2.47× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.09 | 2.26× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.12 | 1.77× |

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.18 | 183.54 | 1018.74× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 20.16 | 224.60× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 31.50 | 220.89× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 28.88 | 215.69× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 31.45 | 192.50× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 9.57 | 91.48× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 12.32 | 91.21× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.26 | 24.06 | 90.97× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 6.93 | 89.84× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.14 | 12.62 | 89.82× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.42 | 36.43 | 87.71× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 37.06 | 85.52× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 12.72 | 82.47× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 15.35 | 73.23× |
| `gauge_subtract` | 1 | 4 | 0.05 | 3.79 | 70.20× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 5.28 | 68.83× |
| `counter_ratio_generic` | 2 | 8 | 0.21 | 13.86 | 67.11× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 6.04 | 63.50× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 14.40 | 62.50× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 14.46 | 59.93× |
| `memory_util_pct` | 1 | 4 | 0.05 | 3.07 | 58.90× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 2.15 | 53.00× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 3.73 | 50.42× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 3.90 | 49.60× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 7.53 | 48.75× |
| `gauge_bare` | 10 | 40 | 0.05 | 2.15 | 43.74× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 3.37 | 32.08× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.12 | 1.94 | 16.17× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.12 | 1.83 | 15.25× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.65 | 11.00× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.61 | 10.53× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.69 | 9.71× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.56 | 8.78× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.11 | 0.66 | 6.20× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.13 | 4.21× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.11 | 3.93× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.10 | 3.78× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.10 | 3.76× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.12 | 3.65× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.10 | 3.54× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.10 | 3.52× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.11 | 3.27× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.09 | 3.13× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.11 | 2.92× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.13 | 2.75× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.10 | 2.45× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.09 | 2.38× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.11 | 1.57× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.23 | 1778.08 | 7701.50× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.20 | 317.28 | 1573.62× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.70 | 263.66 | 377.92× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 1.04 | 376.62 | 362.77× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 1.33 | 381.60 | 287.18× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.95 | 270.77 | 284.21× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 1.37 | 360.10 | 262.60× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.48 | 382.47 | 258.26× |
| `counter_total_sum_generic` | 11 | 44 | 0.35 | 87.61 | 249.89× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 1.79 | 405.25 | 226.38× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.57 | 126.28 | 220.04× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.65 | 133.87 | 204.50× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 16.10 | 184.88× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.17 | 30.48 | 182.45× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.71 | 123.57 | 173.70× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.15 | 22.64 | 149.70× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 7.97 | 134.75× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.19 | 25.94 | 133.32× |
| `counter_ratio_generic` | 2 | 8 | 1.26 | 133.23 | 105.44× |
| `gauge_sum_with_labels` | 5 | 20 | 0.13 | 13.53 | 105.36× |
| `memory_util_pct` | 1 | 4 | 0.09 | 8.57 | 96.79× |
| `counter_ratio_by_id_generic` | 2 | 8 | 1.31 | 124.92 | 95.61× |
| `gauge_subtract` | 1 | 4 | 0.13 | 12.36 | 92.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 1.49 | 135.81 | 91.41× |
| `rezolus_cpu_ipns` | 1 | 4 | 2.77 | 218.54 | 79.00× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 1.57 | 121.41 | 77.55× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 2.67 | 205.60 | 77.01× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.22 | 17.08 | 76.36× |
| `counter_irate_total_mul` | 2 | 8 | 0.24 | 16.75 | 70.49× |
| `gauge_bare` | 10 | 40 | 0.12 | 7.75 | 67.34× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.12 | 7.98 | 66.90× |
| `gauge_avg_scaled` | 6 | 24 | 0.13 | 8.80 | 66.21× |
| `gauge_sum_scaled` | 1 | 4 | 0.13 | 8.43 | 63.74× |
| `counter_irate_by_id_generic` | 5 | 20 | 1.43 | 85.75 | 59.97× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.15 | 8.12 | 54.50× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.48 | 62.66 | 42.40× |
| `counter_rate_bare_generic` | 2 | 8 | 0.24 | 9.64 | 40.29× |
| `gauge_bare_with_labels` | 56 | 224 | 0.06 | 2.20 | 36.91× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 1.66 | 33.43× |
| `gauge_sum_bare` | 2 | 8 | 0.27 | 8.53 | 31.16× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.41 | 9.94 | 24.47× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.14 | 2.06 | 15.01× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.18 | 1.67 | 9.38× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.11 | 2.93× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.16 | 2.40× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.10 | 2.17× |
| `counter_ratio_scaled` | 1 | 4 | 0.08 | 0.12 | 1.55× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.09 | 0.10 | 1.07× |

## Top-10 ratios per fixture

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 330.27× | 0.06 | 19.68 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hi"}[5m])) / cpu_cores / 1000000000` |
| 291.25× | 0.07 | 20.34 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 265.47× | 0.08 | 20.13 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="irq_poll"}[5m])) / cpu_cores / 1000000000` |
| 222.27× | 0.09 | 19.90 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="block"}[5m])) / cpu_cores / 1000000000` |
| 151.48× | 0.14 | 20.91 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 141.68× | 0.15 | 21.21 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 138.22× | 0.15 | 21.26 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 133.86× | 0.16 | 21.13 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 133.08× | 0.06 | 8.15 | `counter_irate_sum_with_labels` | `sum(irate(syscall{op="yield"}[5m]))` |
| 131.84× | 0.05 | 6.92 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="hi"}[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 1.35× | 0.09 | 0.12 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 1.46× | 0.07 | 0.11 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 1.56× | 0.09 | 0.14 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 1.62× | 0.04 | 0.07 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 1.72× | 0.07 | 0.13 | `counter_total_sum_generic` | `sum(irate(cpu_tlb_flush[5m]))` |
| 1.85× | 0.05 | 0.10 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 2.03× | 0.06 | 0.13 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 2.12× | 0.04 | 0.08 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 2.17× | 0.05 | 0.11 | `counter_ratio_by_id_complement` | `1 - sum by (id) (irate(cpu_l3_miss[5m])) / sum by (id) (irate(cpu_l3_access[5m]))` |
| 2.18× | 0.04 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 1958.87× | 0.20 | 387.37 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 1943.99× | 0.20 | 390.18 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="irq_poll"}[5m])) / cpu_cores / 1000000000` |
| 1893.64× | 0.20 | 388.12 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hi"}[5m])) / cpu_cores / 1000000000` |
| 1825.89× | 0.22 | 397.89 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 1626.73× | 0.11 | 184.63 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 978.21× | 0.41 | 401.43 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 968.24× | 0.41 | 399.68 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 930.16× | 0.42 | 392.95 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 920.13× | 0.43 | 397.42 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |
| 885.93× | 0.15 | 131.34 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="irq_poll"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.87× | 0.22 | 0.19 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.07× | 0.15 | 0.16 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 1.19× | 0.13 | 0.15 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 1.24× | 0.13 | 0.16 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 1.68× | 0.10 | 0.16 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 1.77× | 0.07 | 0.12 | `counter_ratio_by_id_complement` | `1 - sum by (id) (irate(cpu_l3_miss[5m])) / sum by (id) (irate(cpu_l3_access[5m]))` |
| 1.87× | 0.05 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 1.99× | 0.10 | 0.19 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 2.06× | 0.11 | 0.22 | `counter_total_sum_generic` | `sum(irate(cpu_branch_instructions[5m]))` |
| 2.10× | 0.05 | 0.10 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 1453.88× | 0.13 | 189.43 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1118.72× | 0.16 | 183.24 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="filesystem",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1064.53× | 0.17 | 184.47 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="event",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1048.63× | 0.18 | 183.95 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="yield",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1028.14× | 0.18 | 185.24 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="write",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1026.87× | 0.18 | 183.04 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="memory",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1024.43× | 0.18 | 182.82 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="timer",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1017.56× | 0.18 | 183.16 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="lock",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1014.13× | 0.18 | 183.90 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="time",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 1006.16× | 0.18 | 182.95 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="other",name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 1.41× | 0.14 | 0.19 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 1.57× | 0.07 | 0.11 | `counter_ratio_by_id_complement` | `1 - sum by (id) (irate(cpu_l3_miss[5m])) / sum by (id) (irate(cpu_l3_access[5m]))` |
| 1.82× | 0.05 | 0.10 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 2.18× | 0.04 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 2.21× | 0.04 | 0.09 | `counter_irate_ratio_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s])) / sum(irate(requests{status="sent",source…` |
| 2.38× | 0.04 | 0.09 | `counter_ratio_complement` | `1 - sum(irate(cpu_l3_miss[5m])) / sum(irate(cpu_l3_access[5m]))` |
| 2.40× | 0.04 | 0.09 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="success",source="llm-perf"}[5s]))` |
| 2.41× | 0.06 | 0.13 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 2.45× | 0.04 | 0.10 | `gauge_a_over_a_plus_b` | `sum(gpu_memory{state="used"}) / (sum(gpu_memory{state="used"}) + sum(gpu_memory{state="free"}))` |
| 2.47× | 0.08 | 0.19 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 8844.82× | 0.20 | 1780.39 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 8408.79× | 0.22 | 1817.35 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="ipc",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 8336.65× | 0.21 | 1755.56 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="yield",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 8243.07× | 0.22 | 1808.67 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="lock",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 7802.45× | 0.23 | 1814.40 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="filesystem",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 7791.86× | 0.23 | 1766.48 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="time",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 7747.67× | 0.23 | 1767.76 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="read",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 7684.33× | 0.23 | 1783.41 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="socket",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 7644.29× | 0.23 | 1774.43 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="timer",name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 7633.66× | 0.23 | 1762.42 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_syscall{op="query",name=~"__SELECTED_CGROUPS__"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.11× | 2.17 | 0.23 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 0.11× | 1.93 | 0.21 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.11× | 2.55 | 0.28 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.17× | 1.11 | 0.19 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 0.27× | 0.90 | 0.24 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |
| 0.49× | 0.52 | 0.26 | `counter_irate_sum_with_labels` | `sum(irate(connections_failed{source="cachecannon"}[5s]))` |
| 0.68× | 0.21 | 0.14 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |
| 1.07× | 0.09 | 0.10 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 1.07× | 0.18 | 0.19 | `counter_irate_sum_with_labels` | `sum(irate(bytes_rx{source="cachecannon"}[5s]))` |
| 1.28× | 0.15 | 0.19 | `counter_irate_sum_with_labels` | `sum(irate(request_errors{source="cachecannon"}[5s]))` |

