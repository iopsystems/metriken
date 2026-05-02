# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 4 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 1.2 | 106.0 ms | 13.2 ms | 5.2 ms | 148.1 ms | 0.4 ms | 154.8 ms |
| `cachecannon.parquet` | 3.8 | 334.0 ms | 7.2 ms | 2.6 ms | 1013.5 ms | 0.1 ms | 1021.1 ms |
| `AB_base.parquet` | 3.6 | 325.9 ms | 10.2 ms | 2.7 ms | 490.6 ms | 0.2 ms | 488.2 ms |
| `vllm.parquet` | 4.0 | 286.2 ms | 10.6 ms | 2.9 ms | 3312.2 ms | 0.2 ms | 3300.9 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.068 | 0.428 | 6.27× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.137 | 1.052 | 7.69× |
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.104 | 0.670 | 6.47× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.179 | 1.370 | 7.64× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.10 | 1.57 | 16.43× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.16 | 2.64 | 16.41× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.10 | 1.66 | 16.21× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.10 | 1.53 | 15.23× |
| `counter_irate_total_scaled` | 1 | 4 | 0.05 | 0.67 | 12.48× |
| `counter_ratio_generic` | 2 | 8 | 0.14 | 1.61 | 11.82× |
| `counter_irate_total_mul` | 2 | 8 | 0.06 | 0.62 | 11.07× |
| `counter_total_sum_generic` | 11 | 44 | 0.06 | 0.60 | 10.70× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.33 | 3.39 | 10.31× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.46 | 9.87× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.90 | 9.46× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.11 | 0.99 | 9.42× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.46 | 8.80× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.07 | 0.64 | 8.69× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.32 | 7.95× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.73 | 7.49× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.35 | 2.48 | 7.07× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.05 | 0.26 | 5.58× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.15 | 4.13× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.09 | 0.27 | 3.08× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.05 | 0.15 | 2.85× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.06 | 0.15 | 2.35× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.07 | 0.17 | 2.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.01 | 0.36× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.01 | 0.33× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.06 | 0.02 | 0.29× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.20× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.30 | 9.10 | 30.79× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.30 | 5.22 | 17.18× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.33 | 5.11 | 15.47× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.22 | 3.31 | 15.20× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.22 | 3.36 | 15.12× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 2.10 | 13.81× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.61 | 7.48 | 12.24× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.87 | 12.16× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.61 | 7.01 | 11.54× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.80 | 11.32× |
| `counter_total_sum_generic` | 11 | 44 | 0.11 | 1.11 | 10.29× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.20 | 10.12× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.14 | 11.22 | 9.81× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.79 | 8.94× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.69 | 8.49× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.79 | 8.48× |
| `counter_ratio_generic` | 2 | 8 | 0.52 | 4.31 | 8.31× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.51 | 8.31× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.23 | 1.86 | 8.19× |
| `counter_ratio_scaled` | 1 | 4 | 0.50 | 3.58 | 7.14× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.11 | 0.78 | 7.14× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.33 | 5.62× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.28 | 3.75× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.36 | 2.94× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.22 | 2.79× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.18 | 2.01× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.16 | 1.96× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.19× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.18 | 0.02 | 0.10× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.18 | 0.01 | 0.08× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.75 | 17.43× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.11 | 1.74 | 15.14× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.58 | 14.75× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.19 | 2.70 | 14.40× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.69 | 14.39× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.66 | 14.11× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.20 | 2.83 | 14.01× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.71 | 13.92× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 0.99 | 12.29× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.41 | 11.18× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.69 | 11.11× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 1.82 | 10.97× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.69 | 10.64× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.93 | 10.60× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.69 | 10.43× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.64 | 9.95× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.81 | 9.68× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.93 | 9.22× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.12 | 1.05 | 8.42× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 2.88 | 8.32× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.16 | 1.30 | 8.04× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.33 | 7.39× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.35 | 2.33 | 6.63× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.19 | 2.80× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.28 | 2.59× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.16 | 1.99× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.16 | 1.84× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.04 | 0.01 | 0.32× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.01 | 0.21× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.01 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 3.93 | 19.20× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 4.15 | 19.14× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.21 | 3.74 | 18.03× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.34 | 5.73 | 16.61× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.88 | 15.18× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.66 | 15.02× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.51 | 7.18 | 14.09× |
| `counter_ratio_generic` | 2 | 8 | 0.30 | 3.98 | 13.45× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.83 | 10.60 | 12.72× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.86 | 11.66× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.78 | 11.66× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 1.06 | 11.25× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 1.32 | 10.90× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.72 | 7.82 | 10.84× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.47 | 4.86 | 10.36× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.89 | 10.17× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.63 | 6.37 | 10.15× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.68 | 9.65× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.20 | 1.85 | 9.15× |
| `counter_total_sum_generic` | 11 | 44 | 0.18 | 1.64 | 9.08× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 1.00 | 8.88× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.13 | 1.17 | 8.69× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.12 | 1.02 | 8.25× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.76 | 8.14× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.33 | 2.65 | 7.94× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.64 | 4.65 | 7.31× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.51 | 7.05× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.08 | 0.53 | 7.00× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.53 | 6.97× |
| `counter_irate_total_mul` | 2 | 8 | 0.12 | 0.81 | 6.52× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.40 | 5.85× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.35 | 4.89× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.38 | 4.82× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.29 | 4.79× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.36 | 4.26× |
| `gauge_avg_scaled` | 6 | 24 | 0.09 | 0.38 | 4.21× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.48 | 3.58× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.22 | 2.31× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.19 | 1.93× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.08 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.10 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.62× | 0.09 | 1.62 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.47× | 0.10 | 1.68 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 16.88× | 0.10 | 1.69 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |
| 16.62× | 0.10 | 1.73 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 16.55× | 0.10 | 1.59 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 16.53× | 0.11 | 1.90 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 16.43× | 0.10 | 1.57 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 16.41× | 0.16 | 2.64 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 16.21× | 0.10 | 1.66 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 16.03× | 0.10 | 1.67 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.00× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="read"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_power_usage) / 1000` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 30.79× | 0.30 | 9.10 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 25.00× | 0.13 | 3.27 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 18.61× | 0.12 | 2.27 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 17.94× | 0.30 | 5.43 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 17.52× | 0.29 | 5.06 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.24× | 0.32 | 5.50 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 17.18× | 0.30 | 5.22 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.95× | 0.31 | 5.25 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 16.55× | 0.31 | 5.14 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 16.07× | 0.33 | 5.30 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.06 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.04 | 0.00 | `gauge_avg_scaled` | `avg(gpu_memory_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-prefill"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_memory_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang-prefill"}` |

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 19.18× | 0.10 | 1.92 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 16.93× | 0.10 | 1.76 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 16.26× | 0.11 | 1.73 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 15.91× | 0.11 | 1.76 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="tasklet"}[5m]))` |
| 15.87× | 0.11 | 1.75 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 15.48× | 0.11 | 1.66 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 15.14× | 0.11 | 1.74 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 15.07× | 0.12 | 1.81 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 14.82× | 0.12 | 1.74 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 14.64× | 0.12 | 1.75 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="success",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="free"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 21.76× | 0.18 | 3.88 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 20.69× | 0.20 | 4.04 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 20.05× | 0.21 | 4.15 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 19.61× | 0.19 | 3.68 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 19.46× | 0.19 | 3.72 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 19.20× | 0.20 | 3.93 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 19.01× | 0.20 | 3.72 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 18.29× | 0.20 | 3.63 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 17.91× | 0.21 | 3.68 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 17.81× | 0.21 | 3.75 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `blockio_size{op="write"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_flops_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_time_to_first_token_seconds{source="sglang"}` |
| 0.01× | 0.11 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_tpot_seconds{source="sglang-router"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="decode_forward",source="sglang-decode"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang-decode"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang-decode"}` |

