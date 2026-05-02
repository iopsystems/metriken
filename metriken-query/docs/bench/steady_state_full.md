# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 4 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 1.2 | 108.0 ms | 14.6 ms | 5.6 ms | 108.8 ms | 0.5 ms | 113.7 ms |
| `cachecannon.parquet` | 3.8 | 338.1 ms | 8.8 ms | 3.5 ms | 282.5 ms | 0.1 ms | 277.6 ms |
| `AB_base.parquet` | 3.6 | 322.5 ms | 12.2 ms | 3.1 ms | 241.0 ms | 0.2 ms | 235.9 ms |
| `vllm.parquet` | 4.0 | 311.3 ms | 12.0 ms | 3.4 ms | 461.8 ms | 0.3 ms | 458.8 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.084 | 0.498 | 5.95× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.166 | 1.504 | 9.08× |
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.145 | 1.169 | 8.05× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.188 | 1.818 | 9.65× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.39 | 6.95 | 17.93× |
| `counter_ratio_generic` | 2 | 8 | 0.15 | 2.18 | 14.14× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.80 | 13.61× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.19 | 2.49 | 13.04× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.52 | 12.90× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.54 | 11.90× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.35 | 11.79× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.13 | 1.55 | 11.57× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.38 | 10.40× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.57 | 10.17× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.72 | 9.11× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.49 | 8.41× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.35 | 8.36× |
| `counter_rate_bare_generic` | 2 | 8 | 0.10 | 0.84 | 8.02× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.80 | 7.24× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.45 | 3.20 | 7.08× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.12 | 0.76 | 6.10× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.40 | 4.40× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.45 | 4.22× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.15 | 3.83× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.07 | 0.22 | 3.29× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.22 | 2.32× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.08 | 1.24× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.02 | 0.50× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.42× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.03 | 0.29× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.10× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 1.49 | 47.75 | 31.99× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.39 | 9.31 | 23.87× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.32 | 5.09 | 16.08× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.32 | 4.76 | 14.75× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.20 | 2.79 | 13.90× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.34 | 4.51 | 13.45× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 1.14 | 13.05× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.28 | 3.54 | 12.77× |
| `counter_total_sum_generic` | 11 | 44 | 0.12 | 1.41 | 11.71× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.69 | 7.33 | 10.68× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.83 | 10.36× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.79 | 10.06× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.77 | 7.44 | 9.65× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.43 | 4.09 | 9.57× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.12 | 1.14 | 9.54× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.16 | 9.20× |
| `counter_ratio_scaled` | 1 | 4 | 0.56 | 4.99 | 8.85× |
| `counter_ratio_generic` | 2 | 8 | 0.62 | 5.00 | 8.13× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.81 | 8.09× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.49 | 7.93× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 0.95 | 7.26× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.43 | 6.27× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.10 | 0.38 | 4.02× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.51 | 2.90× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.20 | 1.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.14 | 0.23 | 1.61× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.14 | 0.10 | 0.72× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.24× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.20× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.11 | 0.02 | 0.18× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.23 | 0.02 | 0.09× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.30 | 0.02 | 0.07× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.27 | 7.10 | 26.54× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.12 | 1.88 | 16.32× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.44 | 6.57 | 15.06× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.82 | 12.65× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.16 | 1.96 | 12.17× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.16 | 1.92 | 12.00× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.27 | 3.24 | 11.91× |
| `counter_ratio_generic` | 2 | 8 | 0.20 | 2.39 | 11.69× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.14 | 1.61 | 11.19× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.80 | 11.12× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.72 | 10.39× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.32 | 9.72× |
| `counter_ratio_scaled` | 1 | 4 | 0.25 | 2.41 | 9.53× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.29 | 2.75 | 9.47× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.20 | 1.89 | 9.42× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.44 | 8.92× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.18 | 1.55 | 8.83× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 1.29 | 8.35× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.75 | 7.46× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.17 | 1.23 | 7.18× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.45 | 2.79 | 6.20× |
| `counter_irate_total_mul` | 2 | 8 | 0.14 | 0.83 | 6.05× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.33 | 5.69× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.43 | 2.29× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.21 | 1.93× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.16 | 0.24 | 1.52× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.08 | 0.11 | 1.35× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.10 | 0.03 | 0.26× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.08 | 0.02 | 0.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.09 | 0.02 | 0.21× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.12 | 0.03 | 0.21× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.15 | 0.02 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.01 | 0.07× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.70 | 13.90 | 19.96× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.23 | 3.46 | 14.78× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.39 | 4.94 | 12.77× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.20 | 2.49 | 12.42× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.49 | 11.30× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.41 | 4.38 | 10.72× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.81 | 10.52× |
| `counter_ratio_generic` | 2 | 8 | 0.31 | 3.18 | 10.26× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 1.25 | 10.09× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.77 | 7.71 | 10.04× |
| `counter_total_sum_generic` | 11 | 44 | 0.17 | 1.67 | 9.89× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.04 | 9.73× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.26 | 2.47 | 9.63× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.97 | 9.16 | 9.44× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.58 | 5.41 | 9.26× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 1.15 | 8.41× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.12 | 0.97 | 8.37× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.62 | 8.20× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.51 | 4.15 | 8.09× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.91 | 7.99× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.54 | 7.96× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.20 | 1.59 | 7.93× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.19 | 1.46 | 7.55× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.62 | 7.51× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.78 | 7.33× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.55 | 7.07× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.68 | 4.26 | 6.31× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.36 | 5.69× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.08 | 0.41 | 5.42× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.07 | 0.39 | 5.38× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.41 | 5.19× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.35 | 4.86× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.35 | 4.72× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.33 | 4.25× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.35 | 4.21× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.21 | 3.50× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.36 | 2.07× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.20 | 1.83× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.18 | 1.77× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.23× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.12 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_scaled` | 1 | 4 | 0.11 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.93× | 0.39 | 6.95 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 15.65× | 0.17 | 2.60 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 15.08× | 0.07 | 1.03 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.14× | 0.15 | 2.18 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 14.02× | 0.12 | 1.66 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.89× | 0.12 | 1.64 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_tx"}[5m]))` |
| 13.87× | 0.12 | 1.63 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.81× | 0.11 | 1.53 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 13.62× | 0.12 | 1.67 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 13.61× | 0.06 | 0.80 | `counter_irate_total_scaled` | `sum(irate(rezolus_cpu_usage[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_queue_reqs{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 31.99× | 1.49 | 47.75 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 26.09× | 0.17 | 4.34 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 23.87× | 0.39 | 9.31 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 23.62× | 0.58 | 13.77 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 21.79× | 0.21 | 4.57 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 19.31× | 0.35 | 6.84 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 19.20× | 0.40 | 7.76 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 18.96× | 0.37 | 7.03 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 18.75× | 0.37 | 7.02 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.81× | 0.30 | 5.41 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 26.54× | 0.27 | 7.10 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 18.78× | 0.07 | 1.32 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 18.29× | 0.12 | 2.11 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 18.16× | 3.71 | 67.47 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 17.79× | 0.13 | 2.30 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.06× | 0.44 | 6.57 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.71× | 0.22 | 3.22 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 14.56× | 0.16 | 2.34 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 13.51× | 0.15 | 1.99 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.29× | 0.25 | 3.27 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_generation_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.04 | 0.00 | `gauge_avg_scaled` | `avg(gpu_tensor_utilization) / 100` |
| 0.01× | 0.10 | 0.00 | `gauge_bare` | `tcp_packet_latency` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.04 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="free"})` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 25.21× | 4.28 | 107.81 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 19.96× | 0.70 | 13.90 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 19.60× | 0.80 | 15.59 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.31× | 0.24 | 3.85 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 15.39× | 0.22 | 3.39 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 15.32× | 0.39 | 5.96 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_migrations{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 15.15× | 0.23 | 3.46 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 14.94× | 0.66 | 9.85 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 14.79× | 0.23 | 3.46 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="local_mm_shootdown"}[5m]))` |
| 14.67× | 0.21 | 3.13 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |

