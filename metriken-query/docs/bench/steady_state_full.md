# Steady-state bench — PromQL vs DuckDB

**Reps per query**: 5 (first rep dropped from stats)
**Warm-up passes**: 1
**Queries in workload**: 313
**Fixtures**: 11 measured, 0 failed cold-start

## Cold-start (per fixture)

| fixture | size (MB) | Tsdb::load | duck open | duck register | duck views | first PromQL | first SQL |
|---|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 3.6 | 327.1 ms | 16.2 ms | 5.3 ms | 213.7 ms | 0.6 ms | 227.4 ms |
| `AB_base_pin.parquet` | 3.1 | 308.6 ms | 9.0 ms | 2.5 ms | 219.8 ms | 0.1 ms | 222.9 ms |
| `AB_level.parquet` | 3.3 | 349.8 ms | 11.4 ms | 3.3 ms | 251.5 ms | 0.2 ms | 248.2 ms |
| `AB_level_pin.parquet` | 2.8 | 330.4 ms | 9.2 ms | 3.1 ms | 217.8 ms | 0.2 ms | 233.7 ms |
| `cachecannon.parquet` | 3.8 | 357.0 ms | 8.7 ms | 2.9 ms | 272.0 ms | 0.2 ms | 268.0 ms |
| `demo.parquet` | 1.2 | 106.9 ms | 8.6 ms | 2.8 ms | 102.4 ms | 0.2 ms | 110.5 ms |
| `disagg/disagg-sglang.parquet` | 5.7 | 737.5 ms | 10.7 ms | 3.0 ms | 1018.7 ms | 0.3 ms | 1041.6 ms |
| `disagg/sglang-nixl-16c.parquet` | 16.8 | 2866.0 ms | 23.2 ms | 3.6 ms | 3876.4 ms | 0.8 ms | 3650.2 ms |
| `sglang_gemma3.parquet` | 2.1 | 285.4 ms | 24.1 ms | 3.1 ms | 445.8 ms | 0.7 ms | 446.0 ms |
| `vllm.parquet` | 4.0 | 357.0 ms | 19.7 ms | 3.6 ms | 417.5 ms | 0.8 ms | 456.8 ms |
| `vllm_gemma3.parquet` | 2.1 | 319.8 ms | 16.7 ms | 3.2 ms | 437.4 ms | 0.9 ms | 452.5 ms |

_first-query probe used `(memory_total - memory_available) / memory_total` where it ran cleanly._

## Per-fixture aggregate (steady state)

Total wall-clock summed across every measured query × rep, per engine.

| fixture | measured | skipped (both err) | promql-only err | sql-only err | catalogue miss | PromQL total (s) | SQL total (s) | sql/promql |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `AB_base.parquet` | 308 | 0 | 5 | 0 | 0 | 0.124 | 0.705 | 5.70× |
| `AB_base_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.137 | 0.759 | 5.53× |
| `AB_level.parquet` | 308 | 0 | 5 | 0 | 0 | 0.118 | 0.661 | 5.62× |
| `AB_level_pin.parquet` | 308 | 0 | 5 | 0 | 0 | 0.118 | 0.692 | 5.86× |
| `cachecannon.parquet` | 308 | 0 | 5 | 0 | 0 | 0.172 | 1.234 | 7.15× |
| `demo.parquet` | 308 | 0 | 5 | 0 | 0 | 0.087 | 0.472 | 5.39× |
| `disagg/disagg-sglang.parquet` | 308 | 0 | 5 | 0 | 0 | 0.200 | 1.277 | 6.37× |
| `disagg/sglang-nixl-16c.parquet` | 308 | 0 | 5 | 0 | 0 | 0.913 | 3.403 | 3.73× |
| `sglang_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.141 | 1.098 | 7.77× |
| `vllm.parquet` | 308 | 0 | 5 | 0 | 0 | 0.192 | 1.249 | 6.52× |
| `vllm_gemma3.parquet` | 308 | 0 | 5 | 0 | 0 | 0.138 | 1.086 | 7.89× |

_"skipped (both err)" is dominated by queries referencing metrics absent from the fixture (e.g. GPU queries against a CPU-only recording) — expected. "promql-only err" or "sql-only err" are the alarming categories: an engine that errors where the other doesn't is a divergence worth investigating._

## Per-entry-shape rollup

For each catalogue entry id, the median of per-query medians (so an outlier query within an entry doesn't dominate). `n_queries` is the number of distinct production queries that matched the entry; `n_runs` is `n_queries × measurement reps`.

### `AB_base.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.34 | 4.69 | 13.80× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.76 | 13.72× |
| `gauge_subtract` | 1 | 4 | 0.05 | 0.75 | 13.71× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.53 | 13.53× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.75 | 12.05× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.47 | 12.03× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.24 | 2.81 | 11.53× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.11 | 1.19 | 10.96× |
| `counter_ratio_generic` | 2 | 8 | 0.17 | 1.85 | 10.68× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 1.28 | 10.38× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.21 | 9.92× |
| `counter_ratio_scaled` | 1 | 4 | 0.23 | 2.23 | 9.84× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.64 | 9.70× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.39 | 9.37× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.77 | 9.13× |
| `counter_total_sum_generic` | 11 | 44 | 0.12 | 1.10 | 9.10× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 1.07 | 8.17× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.29 | 7.49× |
| `counter_rate_bare_generic` | 2 | 8 | 0.14 | 1.02 | 7.22× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 2.90 | 6.31× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.30 | 1.84 | 6.07× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.50 | 6.00× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.20 | 4.30× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.12 | 0.39 | 3.28× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.25 | 3.24× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.10 | 0.22 | 2.24× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.08 | 1.31× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.04 | 0.02 | 0.38× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.06 | 0.01 | 0.26× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.12 | 0.02 | 0.21× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.11 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.03 | 0.00 | 0.04× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |

### `AB_base_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.06 | 0.87 | 14.12× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.12 | 1.74 | 14.08× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 4.58 | 12.29× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.25 | 3.02 | 11.83× |
| `counter_total_sum_generic` | 11 | 44 | 0.10 | 1.18 | 11.54× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.17 | 1.90 | 11.19× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.64 | 10.86× |
| `memory_util_pct` | 1 | 4 | 0.07 | 0.74 | 10.47× |
| `counter_ratio_scaled` | 1 | 4 | 0.23 | 2.32 | 10.05× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.31 | 9.68× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.13 | 1.22 | 9.31× |
| `counter_ratio_generic` | 2 | 8 | 0.22 | 2.00 | 9.13× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 1.11 | 9.08× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 1.40 | 8.45× |
| `counter_irate_total_mul` | 2 | 8 | 0.10 | 0.80 | 8.31× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 1.23 | 7.83× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.13 | 1.00 | 7.61× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.30 | 2.02 | 6.78× |
| `counter_irate_total_scaled` | 1 | 4 | 0.09 | 0.62 | 6.73× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.34 | 6.02× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 2.54 | 5.78× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.52 | 5.61× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.27 | 5.54× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.13 | 0.50 | 3.77× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.22 | 2.47× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.20 | 2.24× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.11 | 0.11 | 1.04× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.02 | 0.37× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.29× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.24× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.11 | 0.03 | 0.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.14 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.11 | 0.01 | 0.05× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.10 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `AB_level.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `gauge_subtract` | 1 | 4 | 0.06 | 0.86 | 15.21× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.54 | 14.23× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 3.03 | 13.41× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.77 | 13.34× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.37 | 4.56 | 12.42× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 1.87 | 11.46× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.24 | 2.56 | 10.70× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.32 | 10.60× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 1.92 | 10.50× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.15 | 1.48 | 9.97× |
| `counter_total_sum_generic` | 11 | 44 | 0.09 | 0.89 | 9.46× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.29 | 9.31× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.10 | 0.93 | 8.93× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.12 | 8.75× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.73 | 8.61× |
| `counter_rate_bare_generic` | 2 | 8 | 0.16 | 1.21 | 7.58× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.14 | 1.02 | 7.40× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 1.55 | 7.10× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.28 | 6.37× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 0.51 | 6.25× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.44 | 2.66 | 6.09× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.42 | 6.02× |
| `gauge_sum_bare` | 2 | 8 | 0.05 | 0.19 | 4.21× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.43 | 3.86× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.22 | 2.02× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.91× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.09 | 1.36× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.28× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.07 | 0.02 | 0.24× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.15× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.15× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |

### `AB_level_pin.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_generic` | 5 | 20 | 0.11 | 1.55 | 13.81× |
| `counter_ratio_scaled` | 1 | 4 | 0.18 | 2.47 | 13.71× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.35 | 4.71 | 13.32× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.74 | 12.46× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.21 | 2.51 | 11.82× |
| `counter_ratio_generic` | 2 | 8 | 0.19 | 2.13 | 11.03× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 1.00 | 10.78× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.26 | 2.71 | 10.57× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.14 | 1.45 | 10.26× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.65 | 10.07× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.15 | 9.92× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.65 | 9.87× |
| `counter_total_sum_generic` | 11 | 44 | 0.11 | 0.94 | 8.84× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.75 | 8.77× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.10 | 8.70× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.58 | 8.54× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.12 | 0.98 | 8.04× |
| `counter_rate_bare_generic` | 2 | 8 | 0.13 | 1.00 | 7.76× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.38 | 2.77 | 7.19× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.47 | 6.41× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.27 | 1.64 | 6.07× |
| `gauge_bare` | 10 | 40 | 0.05 | 0.31 | 6.02× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.21 | 5.02× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.15 | 0.41 | 2.78× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.08 | 0.23 | 2.72× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.19 | 2.24× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.06 | 0.10 | 1.69× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.08 | 0.02 | 0.31× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.27× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.06 | 0.02 | 0.26× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.09 | 0.02 | 0.17× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.05 | 0.00 | 0.06× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.07 | 0.00 | 0.06× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.04× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.01× |

### `cachecannon.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.46 | 10.28 | 22.51× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.27 | 3.89 | 14.17× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.32 | 4.53 | 14.01× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.27 | 3.66 | 13.74× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.85 | 11.21 | 13.21× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.22 | 2.73 | 12.35× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.90 | 11.86× |
| `counter_ratio_generic` | 2 | 8 | 0.61 | 7.15 | 11.65× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.32 | 3.72 | 11.58× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.81 | 7.96 | 9.79× |
| `counter_total_sum_generic` | 11 | 44 | 0.17 | 1.65 | 9.64× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.49 | 4.48 | 9.21× |
| `gauge_subtract` | 1 | 4 | 0.11 | 0.90 | 8.39× |
| `counter_irate_total_mul` | 2 | 8 | 0.13 | 1.09 | 8.33× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.46 | 8.21× |
| `counter_irate_by_id_scaled` | 2 | 8 | 1.45 | 11.75 | 8.13× |
| `counter_rate_bare_generic` | 2 | 8 | 0.18 | 1.38 | 7.75× |
| `counter_ratio_scaled` | 1 | 4 | 0.66 | 5.00 | 7.59× |
| `counter_irate_total_scaled` | 1 | 4 | 0.14 | 0.93 | 6.56× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.12 | 0.62 | 5.22× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.18 | 0.86 | 4.83× |
| `gauge_sum_bare` | 2 | 8 | 0.06 | 0.26 | 4.04× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.24 | 2.81× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.19 | 0.53 | 2.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.12 | 0.23 | 1.96× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.11 | 1.28× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.15 | 0.13 | 0.89× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.09 | 0.02 | 0.21× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.14× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.15 | 0.02 | 0.14× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.22 | 0.02 | 0.09× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.31 | 0.03 | 0.09× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.12 | 0.00 | 0.04× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.05 | 0.00 | 0.02× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `gauge_max_bare` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.04 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.05 | 0.00 | 0.01× |

### `demo.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_by_id_scaled` | 2 | 8 | 0.40 | 6.51 | 16.39× |
| `counter_ratio_generic` | 2 | 8 | 0.16 | 2.08 | 13.05× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.20 | 2.59 | 12.84× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.12 | 1.40 | 11.91× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.16 | 1.70 | 10.96× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.13 | 1.47 | 10.92× |
| `counter_irate_total_scaled` | 1 | 4 | 0.05 | 0.58 | 10.88× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.64 | 10.60× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.49 | 10.51× |
| `counter_total_sum_generic` | 11 | 44 | 0.08 | 0.81 | 10.28× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.59 | 9.22× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.77 | 8.80× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.43 | 3.55 | 8.23× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.08 | 0.64 | 7.83× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 1.10 | 7.51× |
| `counter_rate_bare_generic` | 2 | 8 | 0.15 | 0.90 | 6.03× |
| `gauge_sum_bare` | 2 | 8 | 0.04 | 0.24 | 5.86× |
| `gauge_bare` | 10 | 40 | 0.04 | 0.24 | 5.78× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.11 | 0.40 | 3.49× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.22 | 2.56× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.13 | 0.24 | 1.85× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.10 | 1.28× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.08 | 1.08× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.05 | 0.02 | 0.36× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.10 | 0.03 | 0.29× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.06 | 0.02 | 0.28× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.23× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.02 | 0.16× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.03 | 0.00 | 0.12× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.09 | 0.00 | 0.04× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.04 | 0.00 | 0.03× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.03 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.03 | 0.00 | 0.02× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.04 | 0.00 | 0.02× |
| `gauge_sum_scaled` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.03 | 0.00 | 0.01× |
| `gauge_max_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.05 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.05 | 0.00 | 0.01× |
| `gauge_sum_with_labels` | 5 | 20 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.03 | 0.00 | 0.01× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.03 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `disagg/disagg-sglang.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.23 | 6.02 | 26.21× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.11 | 1.44 | 13.17× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.09 | 1.09 | 12.66× |
| `counter_irate_total_scaled` | 1 | 4 | 0.08 | 1.03 | 12.60× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.42 | 4.99 | 11.78× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.17 | 1.97 | 11.68× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.24 | 2.66 | 11.14× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.22 | 2.43 | 11.02× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.24 | 2.53 | 10.36× |
| `memory_util_pct` | 1 | 4 | 0.08 | 0.80 | 10.10× |
| `counter_total_sum_generic` | 11 | 44 | 0.18 | 1.77 | 10.07× |
| `counter_irate_total_mul` | 2 | 8 | 0.11 | 1.11 | 10.03× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.26 | 2.57 | 9.88× |
| `gauge_subtract` | 1 | 4 | 0.09 | 0.85 | 9.78× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.10 | 0.99 | 9.60× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.27 | 2.41 | 8.85× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.46 | 4.01 | 8.71× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.17 | 1.41 | 8.51× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.29 | 2.44 | 8.38× |
| `counter_ratio_scaled` | 1 | 4 | 0.42 | 3.45 | 8.26× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.48 | 7.37× |
| `counter_rate_bare_generic` | 2 | 8 | 0.17 | 1.24 | 7.20× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.99 | 6.66 | 6.73× |
| `gauge_sum_scaled` | 1 | 4 | 0.09 | 0.55 | 6.26× |
| `gauge_bare` | 10 | 40 | 0.08 | 0.44 | 5.89× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.13 | 0.72 | 5.67× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.26 | 4.57× |
| `gauge_sum_with_labels` | 5 | 20 | 0.08 | 0.22 | 2.90× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.08 | 0.23 | 2.89× |
| `gauge_sum_bare` | 2 | 8 | 0.14 | 0.28 | 1.96× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.13 | 0.24 | 1.83× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.15 | 0.24 | 1.60× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.10 | 0.07 | 0.69× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.09 | 0.02 | 0.25× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.19× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.14 | 0.02 | 0.17× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.19 | 0.02 | 0.13× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.00 | 0.02× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.12 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.06 | 0.00 | 0.01× |
| `counter_ratio_generic` | 2 | 8 | 0.13 | 0.00 | 0.01× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.16 | 0.00 | 0.01× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.17 | 0.00 | 0.01× |

### `disagg/sglang-nixl-16c.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `rezolus_cpu_ipns` | 1 | 4 | 0.24 | 15.59 | 65.46× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.44 | 11.82 | 26.60× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.88 | 17.88 | 20.40× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.27 | 5.00 | 18.19× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.39 | 4.65 | 11.82× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.16 | 1.64 | 10.02× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.14 | 1.37 | 9.84× |
| `gauge_subtract` | 1 | 4 | 0.15 | 1.33 | 8.74× |
| `memory_util_pct` | 1 | 4 | 0.18 | 1.43 | 8.13× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.97 | 7.66 | 7.89× |
| `gauge_bare` | 10 | 40 | 0.12 | 0.91 | 7.83× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.99 | 7.72 | 7.78× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.22 | 1.65 | 7.41× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.31 | 2.08 | 6.81× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.82 | 5.46 | 6.63× |
| `counter_ratio_generic` | 2 | 8 | 0.80 | 5.04 | 6.32× |
| `counter_irate_total_mul` | 2 | 8 | 0.22 | 1.35 | 6.22× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.40 | 2.44 | 6.10× |
| `counter_total_sum_generic` | 11 | 44 | 0.35 | 2.13 | 6.01× |
| `counter_irate_total_scaled` | 1 | 4 | 0.22 | 1.33 | 5.98× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 1.31 | 7.76 | 5.94× |
| `counter_ratio_scaled` | 1 | 4 | 0.81 | 4.69 | 5.79× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.16 | 0.91 | 5.71× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 1.08 | 5.85 | 5.43× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 1.42 | 7.69 | 5.41× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 4.01 | 21.51 | 5.36× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 3.05 | 16.17 | 5.30× |
| `gauge_sum_scaled` | 1 | 4 | 0.11 | 0.57 | 5.27× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 1.02 | 5.30 | 5.21× |
| `counter_irate_by_id_scaled` | 2 | 8 | 5.17 | 22.54 | 4.36× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.09 | 0.35 | 3.80× |
| `gauge_max_bare` | 1 | 4 | 0.14 | 0.53 | 3.67× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.11 | 0.38 | 3.44× |
| `counter_rate_bare_generic` | 2 | 8 | 0.65 | 2.17 | 3.35× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 2.28 | 7.58 | 3.32× |
| `gauge_sum_bare` | 2 | 8 | 0.17 | 0.41 | 2.49× |
| `gauge_sum_with_labels` | 5 | 20 | 0.12 | 0.28 | 2.34× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.27 | 0.56 | 2.07× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.15 | 0.24 | 1.63× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.21 | 0.24 | 1.18× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.10 | 0.02 | 0.18× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.08 | 0.00 | 0.05× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.10 | 0.00 | 0.05× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.08 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.05 | 0.00 | 0.01× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.00 | 0.01× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.09 | 0.00 | 0.01× |

### `sglang_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.29 | 4.74 | 16.27× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.21 | 2.99 | 14.55× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.07 | 1.01 | 14.46× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.16 | 2.33 | 14.26× |
| `counter_ratio_generic` | 2 | 8 | 0.25 | 3.45 | 13.95× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.21 | 2.85 | 13.85× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.08 | 1.03 | 13.67× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.52 | 7.09 | 13.59× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.70 | 13.31× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.44 | 5.69 | 12.87× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 2.65 | 12.67× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.78 | 9.44 | 12.06× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.42 | 4.98 | 11.79× |
| `counter_total_sum_generic` | 11 | 44 | 0.15 | 1.73 | 11.64× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.21 | 2.38 | 11.33× |
| `counter_rate_bare_generic` | 2 | 8 | 0.08 | 0.83 | 10.80× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.46 | 4.83 | 10.55× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.62 | 10.51× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.09 | 0.99 | 10.47× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.23 | 2.40 | 10.30× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.16 | 1.62 | 9.96× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.76 | 9.80× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.45 | 4.02 | 9.03× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.62 | 8.72× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.73 | 8.54× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.08 | 0.46 | 6.05× |
| `gauge_max_bare` | 1 | 4 | 0.07 | 0.38 | 5.87× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.30 | 5.36× |
| `gauge_avg_scaled` | 6 | 24 | 0.08 | 0.40 | 5.13× |
| `gauge_sum_scaled` | 1 | 4 | 0.08 | 0.38 | 4.79× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.25 | 3.98× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.22 | 3.76× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.22 | 3.35× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.22 | 3.30× |
| `gauge_sum_bare` | 2 | 8 | 0.08 | 0.24 | 3.17× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.14 | 0.41 | 3.00× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.96× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.11 | 0.20 | 1.85× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.12 | 0.10 | 0.84× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.15 | 0.12 | 0.80× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 0.12 | 0.77× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.11 | 0.02 | 0.14× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.09 | 0.00 | 0.01× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.01× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

### `vllm.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_by_id_generic` | 2 | 8 | 0.38 | 5.04 | 13.24× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.10 | 1.13 | 11.52× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.24 | 2.71 | 11.42× |
| `counter_ratio_generic` | 2 | 8 | 0.30 | 3.45 | 11.39× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.23 | 2.51 | 11.03× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.26 | 2.73 | 10.45× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.70 | 7.22 | 10.33× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.69 | 10.30× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.64 | 6.35 | 9.98× |
| `counter_irate_total_mul` | 2 | 8 | 0.09 | 0.91 | 9.84× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.95 | 9.21 | 9.74× |
| `memory_util_pct` | 1 | 4 | 0.06 | 0.59 | 9.52× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.69 | 6.18 | 8.98× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.14 | 1.25 | 8.70× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.21 | 1.81 | 8.63× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.49 | 4.11 | 8.35× |
| `counter_total_sum_generic` | 11 | 44 | 0.19 | 1.53 | 7.98× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.66 | 5.19 | 7.92× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.15 | 1.18 | 7.92× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.08 | 0.59 | 7.71× |
| `gauge_subtract` | 1 | 4 | 0.07 | 0.52 | 7.54× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.70 | 5.08 | 7.28× |
| `counter_rate_bare_generic` | 2 | 8 | 0.12 | 0.90 | 7.23× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.15 | 1.04 | 6.83× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.15 | 1.02 | 6.69× |
| `gauge_max_bare` | 1 | 4 | 0.06 | 0.36 | 6.07× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.18 | 1.03 | 5.60× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.09 | 0.49 | 5.50× |
| `gauge_sum_scaled` | 1 | 4 | 0.07 | 0.38 | 5.42× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.36 | 5.01× |
| `gauge_bare` | 10 | 40 | 0.06 | 0.21 | 3.63× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.21 | 3.40× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.22 | 3.35× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.06 | 0.21 | 3.33× |
| `gauge_sum_with_labels` | 5 | 20 | 0.07 | 0.20 | 2.75× |
| `gauge_sum_bare` | 2 | 8 | 0.09 | 0.20 | 2.28× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.16 | 0.33 | 2.11× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.10 | 0.17 | 1.80× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.11 | 0.18 | 1.71× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.08 | 0.02 | 0.24× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.05 | 0.00 | 0.08× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.07 | 0.00 | 0.05× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.03× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.03× |
| `counter_ratio_complement` | 1 | 4 | 0.06 | 0.00 | 0.02× |
| `counter_ratio_scaled` | 1 | 4 | 0.07 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.04 | 0.00 | 0.01× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.11 | 0.00 | 0.01× |

### `vllm_gemma3.parquet`

| entry id | n_queries | n_runs | PromQL median (ms) | SQL median (ms) | sql/promql |
|---|---:|---:|---:|---:|---:|
| `counter_ratio_generic` | 2 | 8 | 0.23 | 3.93 | 17.06× |
| `counter_rate_sum_scaled` | 1 | 4 | 0.06 | 0.97 | 16.87× |
| `counter_ratio_by_id_generic` | 2 | 8 | 0.30 | 4.88 | 16.53× |
| `counter_irate_by_id_with_labels` | 6 | 24 | 0.18 | 2.88 | 15.64× |
| `counter_ratio_scaled` | 1 | 4 | 0.21 | 3.00 | 14.09× |
| `counter_irate_by_id_generic` | 5 | 20 | 0.20 | 2.81 | 13.73× |
| `rezolus_cpu_ipns` | 1 | 4 | 0.54 | 7.30 | 13.46× |
| `rezolus_cpu_aperf_chain_per_id` | 1 | 4 | 0.43 | 5.32 | 12.26× |
| `softirq_irate_by_id_by_kind` | 10 | 40 | 0.11 | 1.36 | 11.82× |
| `memory_util_pct` | 1 | 4 | 0.05 | 0.63 | 11.79× |
| `gauge_a_over_a_plus_b` | 1 | 4 | 0.09 | 0.97 | 11.41× |
| `rezolus_cpu_user_per_id` | 1 | 4 | 0.22 | 2.53 | 11.40× |
| `counter_ratio_by_id_scaled` | 1 | 4 | 0.23 | 2.58 | 11.34× |
| `counter_total_sum_generic` | 11 | 44 | 0.14 | 1.55 | 11.25× |
| `counter_irate_ratio_with_labels` | 3 | 12 | 0.08 | 0.91 | 11.19× |
| `rezolus_cpu_ipns_per_id` | 1 | 4 | 0.70 | 7.77 | 11.14× |
| `gauge_subtract` | 1 | 4 | 0.06 | 0.71 | 11.12× |
| `counter_irate_with_labels_per_cpu_core_pct` | 12 | 48 | 0.15 | 1.67 | 10.96× |
| `counter_rate_bare_generic` | 2 | 8 | 0.09 | 1.02 | 10.89× |
| `rezolus_cpu_aperf_chain_total` | 1 | 4 | 0.43 | 4.61 | 10.68× |
| `counter_irate_by_id_scaled` | 2 | 8 | 0.48 | 5.03 | 10.47× |
| `counter_irate_with_labels_scaled` | 3 | 12 | 0.22 | 2.27 | 10.44× |
| `gauge_ratio_with_labels_ignoring` | 2 | 8 | 0.07 | 0.73 | 9.88× |
| `counter_irate_total_mul` | 2 | 8 | 0.08 | 0.72 | 9.41× |
| `softirq_time_pct_by_id_by_kind` | 10 | 40 | 0.13 | 1.10 | 8.73× |
| `counter_irate_total_per_cpu_core_pct` | 2 | 8 | 0.48 | 3.84 | 7.99× |
| `counter_irate_total_scaled` | 1 | 4 | 0.07 | 0.55 | 7.87× |
| `gauge_sum_scaled` | 1 | 4 | 0.06 | 0.39 | 6.20× |
| `counter_irate_sum_with_labels` | 85 | 340 | 0.07 | 0.45 | 6.11× |
| `softirq_irate_total_by_kind` | 10 | 40 | 0.15 | 0.89 | 5.89× |
| `gauge_avg_scaled` | 6 | 24 | 0.07 | 0.36 | 5.09× |
| `gauge_bare` | 10 | 40 | 0.07 | 0.30 | 4.26× |
| `gauge_sum_by_g_with_labels` | 6 | 24 | 0.06 | 0.22 | 3.77× |
| `gauge_sum_with_labels` | 5 | 20 | 0.06 | 0.21 | 3.44× |
| `gauge_sum_by_g_scaled` | 7 | 28 | 0.07 | 0.24 | 3.35× |
| `gauge_sum_bare` | 2 | 8 | 0.07 | 0.22 | 3.34× |
| `gauge_max_bare` | 1 | 4 | 0.11 | 0.36 | 3.23× |
| `gauge_sum_by_g_bare` | 1 | 4 | 0.09 | 0.28 | 3.14× |
| `counter_ratio_by_g_with_labels` | 1 | 4 | 0.14 | 0.37 | 2.76× |
| `counter_irate_by_g_with_labels_scaled` | 4 | 16 | 0.09 | 0.18 | 1.98× |
| `counter_irate_by_g_with_labels` | 20 | 80 | 0.09 | 0.18 | 1.97× |
| `rezolus_bpf_avg_run_time_per_sampler` | 1 | 4 | 0.07 | 0.02 | 0.22× |
| `rezolus_bpf_run_time_sec` | 1 | 4 | 0.06 | 0.00 | 0.07× |
| `rezolus_bpf_run_time_sec_per_sampler` | 1 | 4 | 0.06 | 0.00 | 0.06× |
| `counter_irate_subtract_with_labels` | 2 | 8 | 0.04 | 0.00 | 0.02× |
| `counter_ratio_by_id_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `counter_ratio_complement` | 1 | 4 | 0.05 | 0.00 | 0.02× |
| `gauge_bare_with_labels` | 56 | 224 | 0.03 | 0.00 | 0.01× |

## Top-10 ratios per fixture

### `AB_base.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.49× | 0.11 | 1.76 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 14.26× | 0.08 | 1.10 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 14.13× | 0.10 | 1.44 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.80× | 0.34 | 4.69 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 13.72× | 0.06 | 0.76 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 13.71× | 0.05 | 0.75 | `gauge_subtract` | `memory_total - memory_available` |
| 12.23× | 0.12 | 1.51 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 12.05× | 0.15 | 1.75 | `rezolus_cpu_user_per_id` | `sum by (id) (irate(cpu_usage{state="user"}[5m])) / 1000000000` |
| 12.03× | 0.21 | 2.47 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 11.84× | 0.11 | 1.35 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |

### `AB_base.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_utilization) / 100` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_router_request_errors_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="decode_forward",source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_request_success_total{source="vllm"}[5s]))` |

### `AB_base_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 17.64× | 0.12 | 2.18 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 15.43× | 0.08 | 1.18 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 15.39× | 0.10 | 1.58 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 14.12× | 0.06 | 0.87 | `gauge_subtract` | `memory_total - memory_available` |
| 12.56× | 0.18 | 2.31 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 12.30× | 0.08 | 0.98 | `counter_total_sum_generic` | `sum(irate(network_drop[5m]))` |
| 12.29× | 0.37 | 4.58 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 11.83× | 0.25 | 3.02 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 11.30× | 0.14 | 1.53 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 11.25× | 0.18 | 2.06 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |

### `AB_base_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_num_running_reqs{source="sglang-decode"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_estimated_write_bytes_per_gpu_total{source="sglang"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_generation_duration_seconds{source="sglang-router"}` |

### `AB_level.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.21× | 0.06 | 0.86 | `gauge_subtract` | `memory_total - memory_available` |
| 14.23× | 0.11 | 1.54 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 13.61× | 0.14 | 1.85 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 13.41× | 0.23 | 3.03 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 13.34× | 0.06 | 0.77 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 12.88× | 0.07 | 0.88 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 12.65× | 0.11 | 1.44 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 12.42× | 0.37 | 4.56 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 12.17× | 0.10 | 1.23 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 11.46× | 0.16 | 1.87 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |

### `AB_level.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="canceled",source="llm-perf"}[5s]))` |
| 0.01× | 0.11 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="ipc"}` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_with_labels` | `sum by (id) (gpu_memory{state="free"})` |
| 0.01× | 0.03 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_utilization) / 100` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_generation_tokens_total{source="sglang"}[5s]))` |

### `AB_level_pin.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 15.66× | 0.10 | 1.57 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.49× | 0.09 | 1.32 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 13.81× | 0.11 | 1.55 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 13.71× | 0.18 | 2.47 | `counter_ratio_scaled` | `sum(irate(cpu_dtlb_miss[5m])) / sum(irate(cpu_instructions[5m])) * 1000` |
| 13.49× | 0.12 | 1.60 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_tlb_flush[5m]))` |
| 13.32× | 0.35 | 4.71 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 12.90× | 0.13 | 1.69 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 12.46× | 0.06 | 0.74 | `memory_util_pct` | `(memory_total - memory_available) / memory_total` |
| 11.82× | 0.21 | 2.51 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 11.51× | 0.08 | 0.94 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |

### `AB_level_pin.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.09 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang-prefill"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(sglang_prompt_tokens_total{source="sglang"}[5s]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_http_request_duration_seconds{source="sglang-router"}` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |

### `cachecannon.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 29.65× | 0.17 | 5.05 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 22.51× | 0.46 | 10.28 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 18.78× | 0.15 | 2.79 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 18.42× | 0.18 | 3.24 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 17.35× | 0.38 | 6.57 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 17.06× | 0.37 | 6.39 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 15.80× | 0.29 | 4.59 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="net_rx"}[5m]))` |
| 15.76× | 0.35 | 5.51 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 15.67× | 0.09 | 1.45 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 15.60× | 0.23 | 3.57 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |

### `cachecannon.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `tpot{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `requests_inflight{source="llm-perf"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_request_prefill_time_seconds{source="vllm"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_running{source="vllm"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `ttft{source="llm-perf"}` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="error",source="llm-perf"}[5s]))` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |

### `demo.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.51× | 0.06 | 1.18 | `counter_total_sum_generic` | `sum(irate(blockio_bytes[5m]))` |
| 16.39× | 0.40 | 6.51 | `counter_irate_by_id_scaled` | `sum by (id) (irate(softirq_time[5m])) / 1000000000` |
| 14.72× | 0.11 | 1.68 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 13.58× | 0.11 | 1.47 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 13.25× | 0.17 | 2.25 | `counter_irate_by_id_scaled` | `sum by (id) (irate(cpu_usage[5m])) / 1000000000` |
| 13.13× | 0.13 | 1.68 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 13.05× | 0.16 | 2.08 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 12.84× | 0.20 | 2.59 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 12.52× | 0.15 | 1.84 | `counter_irate_by_g_with_labels_scaled` | `sum by (id) (irate(cpu_usage{state="system"}[5m])) / 1000000000` |
| 12.07× | 0.14 | 1.68 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |

### `demo.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.13 | 0.00 | `counter_irate_by_g_with_labels` | `sum by (name) (irate(cgroup_cpu_throttled_time{name=~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="output",source="llm-perf"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests{status="timeout",source="llm-perf"}[5s]))` |
| 0.01× | 0.09 | 0.00 | `gauge_bare_with_labels` | `request_latency{source="llm-perf"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="memory"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="socket"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 0.01× | 0.04 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="time"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(tokens{direction="input",source="llm-perf"}[5s]))` |

### `disagg/disagg-sglang.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 33.58× | 0.10 | 3.49 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 26.21× | 0.23 | 6.02 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 20.10× | 0.17 | 3.45 | `counter_total_sum_generic` | `sum(irate(cpu_dtlb_miss[5m]))` |
| 17.53× | 0.16 | 2.86 | `counter_irate_sum_with_labels` | `sum(irate(cpu_migrations{direction="from"}[5m]))` |
| 15.81× | 0.13 | 2.03 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 14.84× | 0.17 | 2.54 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="tasklet"}[5m])) / cpu_cores / 1000000000` |
| 14.21× | 0.22 | 3.10 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 14.21× | 0.20 | 2.82 | `softirq_irate_total_by_kind` | `sum(irate(softirq{kind="hrtimer"}[5m]))` |
| 14.06× | 0.14 | 1.97 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="hrtimer"}[5m])) / cpu_cores / 1000000000` |
| 13.96× | 0.07 | 0.99 | `gauge_ratio_with_labels_ignoring` | `gpu_pcie_throughput{direction="receive"} / ignoring(direction) gpu_pcie_bandwidth` |

### `disagg/disagg-sglang.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(set_count{source="cachecannon"}[5s]))` |
| 0.01× | 0.18 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.08 | 0.00 | `gauge_avg_scaled` | `avg(gpu_sm_utilization) / 100` |
| 0.01× | 0.08 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.10 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.06 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.15 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(get_count{source="cachecannon"}[5s]))` |

### `disagg/sglang-nixl-16c.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 65.46× | 0.24 | 15.59 | `rezolus_cpu_ipns` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m])) * sum(irate(cpu_tsc[5m])) * sum(irate(…` |
| 31.20× | 0.19 | 5.89 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="net_rx"}[5m])) / cpu_cores / 1000000000` |
| 26.60× | 0.44 | 11.82 | `rezolus_cpu_aperf_chain_total` | `sum(irate(cpu_tsc[5m])) * sum(irate(cpu_aperf[5m])) / sum(irate(cpu_mperf[5m])) / cpu_cores` |
| 22.08× | 0.42 | 9.18 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(cpu_usage[5m])) / cpu_cores / 1000000000` |
| 21.12× | 0.24 | 5.00 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="timer"}[5m])) / cpu_cores / 1000000000` |
| 20.40× | 0.88 | 17.88 | `counter_irate_total_per_cpu_core_pct` | `sum(irate(softirq_time[5m])) / cpu_cores / 1000000000` |
| 19.22× | 0.27 | 5.16 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="rcu"}[5m])) / cpu_cores / 1000000000` |
| 15.71× | 0.34 | 5.30 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(softirq_time{kind="sched"}[5m])) / cpu_cores / 1000000000` |
| 15.09× | 0.35 | 5.29 | `counter_irate_with_labels_per_cpu_core_pct` | `sum(irate(cpu_usage{state="user"}[5m])) / cpu_cores / 1000000000` |
| 14.67× | 0.11 | 1.65 | `gauge_ratio_with_labels_ignoring` | `gpu_pcie_throughput{direction="receive"} / ignoring(direction) gpu_pcie_bandwidth` |

### `disagg/sglang-nixl-16c.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.00× | 0.11 | 0.00 | `counter_total_sum_generic` | `sum(irate(cpu_branch_misses[5m]))` |
| 0.00× | 0.28 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.07 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_tensor_utilization) / 100` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.08 | 0.00 | `gauge_sum_by_g_scaled` | `sum by (id) (gpu_sm_occupancy) / 100` |
| 0.01× | 0.08 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.13 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(smg_http_requests_total{source="sglang-router"}[5s]))` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.14 | 0.00 | `gauge_sum_with_labels` | `sum(smg_worker_requests_active{source="sglang-router"})` |
| 0.01× | 0.07 | 0.00 | `gauge_bare_with_labels` | `sglang_per_stage_req_latency_seconds{stage="prefill_forward",source="sglang-prefill"}` |

### `sglang_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.38× | 0.19 | 3.54 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 17.30× | 0.27 | 4.60 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 17.30× | 0.35 | 6.06 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 16.97× | 0.17 | 2.81 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_send_ipi"}[5m]))` |
| 16.37× | 0.12 | 1.90 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |
| 16.27× | 0.29 | 4.74 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 16.16× | 0.20 | 3.16 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |
| 15.77× | 0.17 | 2.73 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="timer"}[5m]))` |
| 15.30× | 0.19 | 2.91 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="timer"}[5m])) / 1000000000` |
| 15.28× | 0.15 | 2.33 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |

### `sglang_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.21 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="yield"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_num_preemptions_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `think_duration{source="llm-perf"}` |
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(requests_sent{source="cachecannon"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |

### `vllm.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 14.49× | 0.19 | 2.71 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 13.58× | 0.37 | 5.04 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 12.93× | 0.21 | 2.67 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 12.14× | 0.22 | 2.61 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="net_rx"}[5m])) / 1000000000` |
| 12.02× | 0.22 | 2.63 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="sched"}[5m]))` |
| 12.01× | 0.21 | 2.56 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 11.85× | 0.19 | 2.29 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="rcu"}[5m]))` |
| 11.78× | 0.38 | 4.49 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_instructions[5m])) / sum by (id) (irate(cpu_cycles[5m]))` |
| 11.67× | 0.30 | 3.45 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 11.53× | 0.24 | 2.73 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="to"}[5m]))` |

### `vllm.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.07 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(bytes_tx{source="cachecannon"}[5s]))` |
| 0.01× | 0.14 | 0.00 | `counter_irate_by_id_generic` | `sum by (id) (irate(cpu_dtlb_miss[5m]))` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="poll"}` |
| 0.01× | 0.05 | 0.00 | `gauge_bare_with_labels` | `blockio_latency{op="write"}` |
| 0.01× | 0.12 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(cgroup_cpu_throttled_time{name!~"__SELECTED_CGROUPS__"}[5m]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `target_rate{source="cachecannon"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `smg_router_ttft_seconds{source="sglang-router"}` |
| 0.01× | 0.06 | 0.00 | `gauge_bare_with_labels` | `vllm_time_to_first_token_seconds{source="vllm"}` |
| 0.01× | 0.11 | 0.00 | `counter_ratio_by_id_scaled` | `sum by (id) (irate(cpu_dtlb_miss[5m])) / sum by (id) (irate(cpu_instructions[5m])) * 1000` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `sglang_e2e_request_latency_seconds{source="sglang-prefill"}` |

### `vllm_gemma3.parquet` — worst SQL/PromQL

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 18.61× | 0.17 | 3.09 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="task_switch"}[5m]))` |
| 17.21× | 0.19 | 3.19 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="sched"}[5m])) / 1000000000` |
| 17.06× | 0.23 | 3.93 | `counter_ratio_generic` | `sum(irate(cpu_instructions[5m])) / sum(irate(cpu_cycles[5m]))` |
| 16.99× | 0.29 | 4.88 | `counter_ratio_by_id_generic` | `sum by (id) (irate(cpu_branch_misses[5m])) / sum by (id) (irate(cpu_branch_instructions[5m]))` |
| 16.94× | 0.17 | 2.95 | `softirq_irate_by_id_by_kind` | `sum by (id) (irate(softirq{kind="net_rx"}[5m]))` |
| 16.87× | 0.06 | 0.97 | `counter_rate_sum_scaled` | `sum(rate(gpu_energy_consumption[5m])) / 1000` |
| 16.55× | 0.33 | 5.40 | `counter_irate_ratio_with_labels` | `sum(irate(cgroup_cpu_instructions{name!~"__SELECTED_CGROUPS__"}[5m])) / sum(irate(cgroup_cpu_cycles{…` |
| 16.37× | 0.18 | 2.92 | `softirq_time_pct_by_id_by_kind` | `sum by (id) (irate(softirq_time{kind="rcu"}[5m])) / 1000000000` |
| 16.27× | 0.18 | 3.01 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_migrations{direction="from"}[5m]))` |
| 16.16× | 0.18 | 2.88 | `counter_irate_by_id_with_labels` | `sum by (id) (irate(cpu_tlb_flush{reason="remote_shootdown"}[5m]))` |

### `vllm_gemma3.parquet` — best SQL/PromQL (SQL wins)

| ratio | PromQL ms | SQL ms | entry | query |
|---:|---:|---:|---|---|
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="lock"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="other"}` |
| 0.01× | 0.07 | 0.00 | `gauge_bare` | `scheduler_running` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_inter_token_latency_seconds{source="vllm"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="process"}` |
| 0.01× | 0.05 | 0.00 | `counter_irate_sum_with_labels` | `sum(irate(vllm_prompt_tokens_total{source="vllm"}[5s]))` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="timer"}` |
| 0.01× | 0.04 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="filesystem"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `syscall_latency{op="sleep"}` |
| 0.01× | 0.03 | 0.00 | `gauge_bare_with_labels` | `vllm_num_requests_waiting{source="vllm"}` |

