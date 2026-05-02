# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 4 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 1.2 | 113.7 ms | 22.6 ms | 6.6 ms | 103.5 ms | 1.7 ms | 110.8 ms |
| `cachecannon.parquet` | 3.8 | 337.7 ms | 7.1 ms | 2.9 ms | 276.6 ms | 0.1 ms | 287.2 ms |
| `AB_base.parquet` | 3.6 | 332.6 ms | 12.8 ms | 2.9 ms | 223.7 ms | 0.2 ms | 236.1 ms |
| `vllm.parquet` | 4.0 | 293.4 ms | 12.3 ms | 3.3 ms | 415.8 ms | 0.3 ms | 425.7 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.081 | 0.491 | 6.03× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.154 | 1.440 | 9.38× |
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.122 | 0.994 | 8.12× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.207 | 1.866 | 9.02× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 7.60 | 20.57× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.11 | 1.56 | 13.75× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.51 | 13.14× |
| `counter_ratio_generic` | 2 | 8 | 0.14 | 1.85 | 12.79× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.18 | 2.32 | 12.60× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 1.21 | 11.77× |
| `counter_irate_total_scaled` | 1 | 4 | 0.06 | 0.76 | 11.73× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.57 | 11.16× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.55 | 10.79× |
| `counter_total_sum_generic` | 11 | 44 | 0.07 | 0.71 | 9.75× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.31 | 9.75× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.37 | 9.55× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.17 | 1.46 | 8.79× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.10 | 0.82 | 7.93× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.77 | 7.83× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 3.05 | 7.00× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 0.82 | 6.01× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.16 | 4.36× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.10 | 0.34 | 3.22× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.07 | 0.21 | 3.11× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.25 | 2.09× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.06 | 0.08 | 1.29× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.09 | 1.02× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.04 | 0.03 | 0.85× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.03 | 0.56× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.01 | 0.15× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.01 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.13× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 1.38 | 44.27 | 32.09× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 9.49 | 21.54× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.25 | 4.24 | 16.73× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.19 | 2.64 | 14.25× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.28 | 3.95 | 14.24× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.25 | 3.39 | 13.68× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.71 | 9.56 | 13.54× |
| `counter_ratio_generic` | 2 | 8 | 0.57 | 7.69 | 13.39× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.33 | 4.12 | 12.42× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 1.12 | 11.78× |
| `counter_total_sum_generic` | 11 | 44 | 0.11 | 1.27 | 11.62× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.39 | 4.19 | 10.62× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.81 | 10.50× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 1.28 | 10.32× |
| `gauge_subtract` | 1 | 4 | 0.08 | 0.74 | 9.63× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.74 | 6.69 | 9.06× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.73 | 8.55× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.87 | 8.50× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.42 | 8.28× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.12 | 0.96 | 7.97× |
| `counter_ratio_scaled` | 1 | 4 | 0.69 | 4.66 | 6.74× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.38 | 5.86× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.20 | 2.35× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.35 | 2.27× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.08 | 0.17 | 2.13× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.07 | 0.09 | 1.38× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.10 | 0.09 | 0.94× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.09 | 0.03 | 0.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.21× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.01 | 0.19× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.25 | 0.02 | 0.09× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.01 | 0.08× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.31 | 0.02 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.36 | 5.71 | 15.83× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.12 | 1.54 | 13.01× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.55 | 12.11× |
| `counter_ratio_scaled` | 1 | 4 | 0.17 | 2.08 | 11.87× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.49 | 11.37× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.11 | 1.27 | 11.20× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 2.49 | 11.15× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.27 | 2.93 | 10.90× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.65 | 10.50× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 1.79 | 10.26× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.92 | 9.89× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.55 | 9.84× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.60 | 9.82× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 1.02 | 9.51× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.71 | 9.39× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.27 | 2.46 | 9.25× |
| `counter_rate_bare_generic` | 2 | 8 | 0.11 | 0.92 | 8.67× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.36 | 7.91× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.16 | 7.77× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.58 | 7.69× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.63 | 7.54× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.30 | 6.26× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.45 | 2.52 | 5.60× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.33 | 2.50× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.20 | 2.13× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.17 | 1.79× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.07 | 0.10 | 1.32× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.26× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.08 | 0.02 | 0.23× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.07 | 0.02 | 0.23× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.14× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.11× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.76 | 12.01 | 15.71× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.28 | 3.45 | 12.51× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.90 | 11.49× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.79 | 8.84 | 11.12× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.64 | 7.02 | 10.92× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.44 | 4.84 | 10.89× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 1.45 | 10.86× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.25 | 2.71 | 10.66× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.26 | 2.74 | 10.54× |
| `counter_ratio_generic` | 2 | 8 | 0.38 | 3.98 | 10.40× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.09 | 10.26× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.45 | 4.59 | 10.11× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.15 | 1.49 | 10.04× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 1.02 | 9.94 | 9.76× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.18 | 1.72 | 9.72× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.53 | 5.05 | 9.45× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.67 | 9.01× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.13 | 1.13 | 8.73× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.65 | 8.66× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.23 | 1.91 | 8.45× |
| `counter_total_sum_generic` | 11 | 44 | 0.20 | 1.67 | 8.39× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.16 | 1.28 | 8.20× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 0.87 | 7.71× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.00 | 7.03× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.10 | 0.70 | 6.95× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.12 | 0.74 | 6.32× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.75 | 4.51 | 6.03× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.45 | 5.87× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.09 | 0.45 | 4.89× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.39 | 4.60× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.41 | 4.53× |
| `gauge_max_bare` | 1 | 4 | 0.10 | 0.46 | 4.47× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.10 | 0.44 | 4.17× |
| `gauge_sum_bare` | 2 | 8 | 0.10 | 0.41 | 4.15× |
| `gauge_avg_scaled` | 6 | 24 | 0.11 | 0.43 | 3.84× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.22 | 3.25× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.47 | 2.88× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.12 | 0.22 | 1.82× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.25 | 1.65× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.21× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.12 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.16 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 20.57× | 0.37 | 7.60 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 18.02× | 0.15 | 2.72 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 16.32× | 0.06 | 1.02 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 15.70× | 0.10 | 1.62 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.48× | 0.11 | 1.64 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 14.00× | 0.11 | 1.60 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.81× | 0.12 | 1.71 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 13.41× | 0.12 | 1.54 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 13.16× | 0.12 | 1.57 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 12.92× | 0.12 | 1.56 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.00× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_clock{clock="graphics"})` |
| 0.01× | 0.07 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_tlb_flush{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.12 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_rx{source="cachecannon"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="write"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 33.62× | 0.16 | 5.53 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 32.09× | 1.38 | 44.27 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 22.01× | 0.60 | 13.13 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 21.54× | 0.44 | 9.49 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.17× | 0.33 | 6.67 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 20.10× | 0.38 | 7.69 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 20.06× | 0.36 | 7.14 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 18.00× | 0.30 | 5.49 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 17.92× | 0.40 | 7.11 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 17.90× | 0.38 | 6.85 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.06 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `vllm_e2e_request_latency_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 21.16× | 3.47 | 73.40 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 16.16× | 0.11 | 1.77 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.83× | 0.36 | 5.71 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 15.14× | 0.12 | 1.79 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.07× | 0.07 | 0.92 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 13.42× | 0.26 | 3.52 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_migrations{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 13.20× | 0.27 | 3.53 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 13.11× | 0.04 | 0.59 | `counter_irate_sum_with_labels` | `sum(irate(responses_received{source="cachecannon"}[5s]))` |
| 13.08× | 0.13 | 1.67 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 12.56× | 0.13 | 1.65 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.10 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_read_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="query"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 21.25× | 4.36 | 92.57 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_syscall{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 17.81× | 0.83 | 14.76 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_tlb_flush{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 17.52× | 0.25 | 4.31 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 17.38× | 0.20 | 3.51 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 16.03× | 0.22 | 3.55 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 15.71× | 0.76 | 12.01 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 15.48× | 0.62 | 9.54 | `counter_irate_with_labels_scaled` | `sum(irate(cgroup_cpu_usage{name!~"__SELECTED_CGROUPS__"}[5m])) / 1000000000` |
| 14.67× | 0.24 | 3.51 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 14.63× | 0.23 | 3.31 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 13.93× | 0.19 | 2.63 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="rcu"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.08 | 0.00 | `gauge_bare_with_labels` | `get_latency{source="cachecannon"}` |
| 0.01× | 0.16 | 0.00 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.15 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_inter_token_latency_seconds{source="sglang-decode"}` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `itl{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |

