# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 4 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 1.2 | 107.1 ms | 13.1 ms | 5.5 ms | 153.8 ms | 0.4 ms | 152.6 ms |
| `cachecannon.parquet` | 3.8 | 347.8 ms | 7.5 ms | 2.4 ms | 1114.6 ms | 0.1 ms | 1060.2 ms |
| `AB_base.parquet` | 3.6 | 342.3 ms | 10.2 ms | 2.9 ms | 533.5 ms | 0.2 ms | 500.4 ms |
| `vllm.parquet` | 4.0 | 288.4 ms | 10.7 ms | 2.2 ms | 3630.7 ms | 0.2 ms | 3394.4 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.068 | 0.381 | 5.60× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.144 | 0.934 | 6.49× |
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.104 | 0.589 | 5.64× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.174 | 1.031 | 5.91× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.13 | 1.64 | 12.27× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.10 | 1.21 | 12.13× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.10 | 1.19 | 11.95× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.19 | 2.11 | 11.35× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.26 | 11.30× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.59 | 9.95× |
| `counter_total_sum_generic` | 11 | 44 | 0.06 | 0.56 | 9.41× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.11 | 0.99 | 9.39× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.09 | 0.85 | 9.31× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.42 | 9.07× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.56 | 8.54× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 2.78 | 7.94× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.61 | 7.68× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.40 | 7.27× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.28 | 7.17× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.68 | 6.78× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.36 | 2.37 | 6.53× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.04 | 0.25 | 5.82× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.05 | 0.22 | 4.42× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.13 | 3.56× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.09 | 0.29 | 3.22× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.06 | 0.16 | 2.54× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.07 | 0.16 | 2.38× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.01 | 0.36× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.01 | 0.32× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.06 | 0.02 | 0.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.21× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.08 | 0.01 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.30 | 7.26 | 24.04× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 2.12 | 14.90× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.29 | 3.46 | 11.77× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.23 | 2.47 | 10.68× |
| `counter_total_sum_generic` | 11 | 44 | 0.11 | 1.07 | 10.04× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.25 | 2.43 | 9.68× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.68 | 9.44× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.39 | 3.61 | 9.24× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.26 | 2.41 | 9.11× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.64 | 8.64× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.67 | 5.66 | 8.44× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.73 | 8.35× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.97 | 8.19× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.67 | 8.16× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.69 | 5.39 | 7.84× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.75 | 7.77× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.29 | 9.72 | 7.52× |
| `counter_ratio_scaled` | 1 | 4 | 0.51 | 3.74 | 7.33× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.11 | 0.81 | 7.31× |
| `counter_ratio_generic` | 2 | 8 | 0.51 | 3.73 | 7.29× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.32 | 5.59× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.34 | 4.96× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.30 | 3.88× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.19 | 2.07× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.32 | 1.99× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.17 | 1.99× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.18 | 1.79× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.19× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.19× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.19× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.19 | 0.01 | 0.07× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.23 | 0.02 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.10 | 1.30 | 12.55× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.62 | 11.00× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.24 | 10.99× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 1.82 | 10.89× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 2.14 | 10.58× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.69 | 10.49× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.22 | 2.27 | 10.47× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.37 | 10.35× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.22 | 9.89× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.61 | 9.62× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.92 | 9.57× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 0.88 | 9.54× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 1.59 | 9.50× |
| `counter_irate_total_mul` | 2 | 8 | 0.07 | 0.65 | 9.48× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.53 | 9.16× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.76 | 8.84× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.86 | 8.59× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.05 | 8.38× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.16 | 1.34 | 8.33× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.33 | 8.08× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.33 | 2.51 | 7.72× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 2.21 | 5.86× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.27 | 5.36× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.18 | 3.11× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.29 | 2.55× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.18 | 2.18× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.16 | 2.00× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.07 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.01 | 0.23× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.19× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.01 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.01 | 0.15× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.20 | 2.28 | 11.68× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.70 | 10.77× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.37 | 3.88 | 10.56× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.22 | 2.22 | 10.16× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.22 | 2.17 | 9.89× |
| `counter_ratio_generic` | 2 | 8 | 0.28 | 2.76 | 9.87× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 0.91 | 9.80× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.16 | 9.44× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.89 | 8.04 | 9.02× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.55 | 4.91 | 8.99× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.71 | 6.11 | 8.58× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.42 | 3.53 | 8.35× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 0.92 | 8.25× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.63 | 7.90× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.69 | 7.85× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.97 | 7.84× |
| `counter_total_sum_generic` | 11 | 44 | 0.17 | 1.34 | 7.81× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 1.16 | 7.78× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.48 | 7.71× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.12 | 0.89 | 7.38× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.56 | 7.37× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.46 | 7.12× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.20 | 1.42 | 7.09× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.81 | 7.08× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.67 | 4.54 | 6.75× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.34 | 2.21 | 6.42× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.07 | 0.41 | 5.77× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.40 | 5.56× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.68 | 3.54 | 5.20× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.39 | 5.19× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.35 | 4.69× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.32 | 4.62× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.33 | 4.54× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.33 | 4.47× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.34 | 4.45× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.19 | 3.15× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.34 | 2.09× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.20 | 1.93× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.17 | 1.71× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.18× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.05× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.08 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.09× | 0.10 | 1.34 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.59× | 0.09 | 1.19 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="tasklet"}[5m]))` |
| 12.67× | 0.10 | 1.27 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 12.54× | 0.10 | 1.27 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 12.29× | 0.11 | 1.37 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 12.27× | 0.13 | 1.64 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 12.26× | 0.10 | 1.25 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 12.13× | 0.10 | 1.21 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 11.98× | 0.09 | 1.11 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="tasklet"}[5m])) / 1000000000` |
| 11.96× | 0.11 | 1.34 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_http_request_duration_seconds{source="sglang-router"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 25.64× | 0.13 | 3.35 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 24.04× | 0.30 | 7.26 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 21.27× | 0.17 | 3.59 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="tasklet"}[5m]))` |
| 17.37× | 0.12 | 2.16 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 15.82× | 0.14 | 2.25 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 15.17× | 0.29 | 4.33 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="rcu"}[5m]))` |
| 14.83× | 0.15 | 2.23 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="system"}[5m])) / cpu_cores / 1000000000` |
| 14.50× | 0.15 | 2.24 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 13.09× | 0.16 | 2.12 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 12.90× | 0.13 | 1.69 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_tx"}[5m])) / cpu_cores / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `gauge_sum_with_labels` | `sum(gpu_memory{state="used"})` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_num_aborted_requests_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.82× | 0.05 | 0.67 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 13.28× | 0.10 | 1.37 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 12.57× | 0.11 | 1.34 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.04× | 0.11 | 1.36 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 11.60× | 0.13 | 1.45 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="tasklet"}[5m]))` |
| 11.48× | 0.07 | 0.76 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 11.29× | 0.12 | 1.30 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 11.08× | 0.11 | 1.26 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 11.00× | 0.06 | 0.62 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 10.98× | 0.06 | 0.69 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_tx"}[5m])) / 1000000000` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_memory_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_avg_scaled` | `avg(gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 11.82× | 0.19 | 2.28 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 11.77× | 0.33 | 3.88 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 11.49× | 0.20 | 2.25 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 11.48× | 0.21 | 2.37 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 11.42× | 0.20 | 2.23 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 11.36× | 0.20 | 2.24 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 11.20× | 0.19 | 2.10 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 11.16× | 0.20 | 2.19 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 10.77× | 0.06 | 0.70 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |
| 10.53× | 0.21 | 2.17 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `response_latency{source="cachecannon"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-prefill"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `blockio_size{op="write"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang"}` |

